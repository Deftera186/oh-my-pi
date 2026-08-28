//! One-way legacy settings import into native TOML.

use std::{
	fs, io,
	path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
	SettingsCatalog, ValidationError, deep_merge,
	io::{SettingsIoError, atomic_replace},
};

const MARKER: &str = ".settings-migration-v2";
const RECORD: &str = "settings-migration.toml";

/// Stable migration action vocabulary.
#[derive(
	Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum MigrationAction {
	/// A representable value was converted.
	Converted,
	/// An obsolete/unsupported value was removed.
	Dropped,
	/// Credential bytes were refused because no combined import API existed.
	CredentialRejected,
}

/// One secret-free migration decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationEntry {
	/// Legacy dotted path; never the value.
	pub path:   String,
	/// Stable action.
	pub action: MigrationAction,
	/// Human-readable, value-free rationale.
	pub reason: String,
}

/// Durable record of unsupported, dropped, and converted legacy keys.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationRecord {
	/// Migration format revision.
	pub revision: u32,
	/// Source labels that participated in the import.
	pub sources:  Vec<String>,
	/// Value-free decisions.
	pub entries:  Vec<MigrationEntry>,
}

/// Result of attempting the one-time migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationOutcome {
	/// The durable marker already existed.
	AlreadyCompleted,
	/// Migration completed and wrote the marker.
	Completed(MigrationRecord),
}

/// Imports `settings.json`/JSONC and the legacy `agent.db` settings table.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(path = %data_dir.display())
)]
pub fn migrate_legacy_settings(
	data_dir: &Path,
	catalog: SettingsCatalog,
) -> Result<MigrationOutcome, MigrationError> {
	let result = migrate_legacy_settings_inner(data_dir, catalog);
	match &result {
		Ok(MigrationOutcome::AlreadyCompleted) => {
			tracing::debug!(
				path = %data_dir.display(),
				"legacy settings migration already completed",
			);
		},
		Ok(MigrationOutcome::Completed(record)) => {
			let mut converted_count = 0_usize;
			let mut dropped_count = 0_usize;
			let mut credential_rejected_count = 0_usize;
			for entry in &record.entries {
				match entry.action {
					MigrationAction::Converted => converted_count += 1,
					MigrationAction::Dropped => dropped_count += 1,
					MigrationAction::CredentialRejected => credential_rejected_count += 1,
				}
			}
			if dropped_count > 0 || credential_rejected_count > 0 {
				tracing::warn!(
					path = %data_dir.display(),
					dropped_count,
					credential_rejected_count,
					"legacy settings entries rejected during migration",
				);
			}
			tracing::info!(
				path = %data_dir.display(),
				source_count = record.sources.len(),
				entry_count = record.entries.len(),
				converted_count,
				dropped_count,
				credential_rejected_count,
				"legacy settings migration applied",
			);
		},
		Err(error) => {
			tracing::warn!(
				path = %data_dir.display(),
				error = %error,
				"legacy settings migration failed",
			);
		},
	}
	result
}

fn migrate_legacy_settings_inner(
	data_dir: &Path,
	catalog: SettingsCatalog,
) -> Result<MigrationOutcome, MigrationError> {
	fs::create_dir_all(data_dir)
		.map_err(|source| MigrationError::CreateDirectory { path: data_dir.to_owned(), source })?;
	let marker = data_dir.join(MARKER);
	if marker.exists() {
		return Ok(MigrationOutcome::AlreadyCompleted);
	}

	let mut record = MigrationRecord { revision: 2, ..MigrationRecord::default() };
	let mut document = toml::Table::new();
	let mut backups = Vec::new();
	let settings_json = data_dir.join("settings.json");
	if settings_json.exists() {
		let source = fs::read_to_string(&settings_json)
			.map_err(|source| MigrationError::Read { path: settings_json.clone(), source })?;
		let value: serde_json::Value = omp_slopjson::from_str(&source)?;
		let table = json_table(value)?;
		deep_merge(&mut document, table);
		record.sources.push("settings.json".to_owned());
		backups.push(settings_json.clone());
	}

	let database = data_dir.join("agent.db");
	if database.exists() {
		if let Some(table) = read_database_settings(&database)? {
			deep_merge(&mut document, table);
			record.sources.push("agent.db:settings".to_owned());
			backups.push(database.clone());
		}
	}

	let changelog_version = convert_legacy(&mut document, &mut record);
	remove_unsupported(&mut document, &mut record);
	reject_credentials(&mut document, &mut record, "");

	let config = data_dir.join("config.toml");
	if !document.is_empty() || config.is_file() {
		let mut current = match fs::read_to_string(&config) {
			Ok(source) => toml::from_str::<toml::Table>(&source)
				.map_err(|source| MigrationError::ExistingConfig { path: config.clone(), source })?,
			Err(error) if error.kind() == io::ErrorKind::NotFound => toml::Table::new(),
			Err(source) => return Err(MigrationError::Read { path: config.clone(), source }),
		};
		// Native values already chosen by the user win over imported legacy data.
		let mut imported = document;
		deep_merge(&mut imported, current);
		current = imported;
		normalize_legacy_layer(&mut current);
		validate_migration_candidate(&current, catalog)?;
		for source in &backups {
			backup_file(source)?;
		}
		atomic_replace(&config, &toml::to_string_pretty(&current)?)?;
	} else {
		for source in &backups {
			backup_file(source)?;
		}
	}
	if let Some(version) = changelog_version {
		atomic_replace(&data_dir.join("last-changelog-version"), &version)?;
	}
	atomic_replace(&data_dir.join(RECORD), &toml::to_string_pretty(&record)?)?;
	atomic_replace(&marker, "revision = 2\n")?;
	Ok(MigrationOutcome::Completed(record))
}

fn json_table(value: serde_json::Value) -> Result<toml::Table, MigrationError> {
	let serde_json::Value::Object(object) = value else {
		return Err(MigrationError::JsonRootNotObject);
	};
	Ok(object
		.into_iter()
		.filter_map(|(key, value)| json_value(value).map(|value| (key, value)))
		.collect())
}

fn json_value(value: serde_json::Value) -> Option<toml::Value> {
	match value {
		serde_json::Value::Null => None,
		serde_json::Value::Bool(value) => Some(toml::Value::Boolean(value)),
		serde_json::Value::Number(value) => value
			.as_i64()
			.map(toml::Value::Integer)
			.or_else(|| {
				value
					.as_u64()
					.and_then(|value| i64::try_from(value).ok())
					.map(toml::Value::Integer)
			})
			.or_else(|| value.as_f64().map(toml::Value::Float)),
		serde_json::Value::String(value) => Some(toml::Value::String(value)),
		serde_json::Value::Array(values) => {
			Some(toml::Value::Array(values.into_iter().filter_map(json_value).collect()))
		},
		serde_json::Value::Object(values) => Some(toml::Value::Table(
			values
				.into_iter()
				.filter_map(|(key, value)| json_value(value).map(|value| (key, value)))
				.collect(),
		)),
	}
}

fn read_database_settings(path: &Path) -> Result<Option<toml::Table>, MigrationError> {
	let connection =
		rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
	let exists: bool = connection.query_row(
		"SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='settings')",
		[],
		|row| row.get(0),
	)?;
	if !exists {
		return Ok(None);
	}
	let columns = connection
		.prepare("PRAGMA table_info(settings)")?
		.query_map([], |row| row.get::<_, String>(1))?
		.collect::<Result<Vec<_>, _>>()?;
	if columns.iter().any(|column| column == "key") && columns.iter().any(|column| column == "value")
	{
		let mut statement = connection.prepare("SELECT key, value FROM settings ORDER BY key")?;
		let rows =
			statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
		let mut table = toml::Table::new();
		for row in rows {
			let (key, encoded) = row?;
			let value: serde_json::Value = serde_json::from_str(&encoded)
				.map_err(|source| MigrationError::DatabaseValue { key: key.clone(), source })?;
			if let Some(value) = json_value(value) {
				set_dotted(&mut table, &key, value);
			}
		}
		return Ok(Some(table));
	}
	if columns.iter().any(|column| column == "data") {
		let encoded = connection
			.query_row("SELECT data FROM settings WHERE id = 1", [], |row| row.get::<_, String>(0))
			.optional()?;
		return encoded
			.map(|encoded| {
				let value = serde_json::from_str(&encoded).map_err(|source| {
					MigrationError::DatabaseValue { key: "data".to_owned(), source }
				})?;
				json_table(value)
			})
			.transpose();
	}
	Ok(None)
}

fn backup_file(path: &Path) -> Result<(), MigrationError> {
	let backup = path.with_file_name(format!(
		"{}.pre-omp-migration.bak",
		path
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("legacy")
	));
	if !backup.exists() {
		fs::copy(path, &backup).map_err(|source| MigrationError::Backup {
			path: path.to_owned(),
			backup,
			source,
		})?;
	}
	Ok(())
}

fn convert_legacy(document: &mut toml::Table, record: &mut MigrationRecord) -> Option<String> {
	for (old, new) in [
		("queueMode", "interaction.steeringMode"),
		("includeModelInPrompt", "prompt.includeModelInPrompt"),
		("includeWorkspaceTree", "prompt.includeWorkspaceTree"),
		("steeringMode", "interaction.steeringMode"),
		("defaultModel", "model.roles.default"),
		("default_model", "model.roles.default"),
		("worktreeDir", "worktree.base"),
		("modelProviderOrder", "model.provider_order"),
		("defaultThinkingLevel", "model.default_thinking"),
		("thinkingBudgets", "model.thinking_budgets"),
		("modelRoles", "model.roles"),
		("modelRoleStorage", "model.role_storage"),
		("modelTags", "model.tags"),
		("cycleOrder", "model.cycle_order"),
		("enabledModels", "model.enabled_models"),
		("disabledProviders", "model.disabled_providers"),
		("tier.openai", "model.tier_openai"),
		("tier.anthropic", "model.tier_anthropic"),
		("tier.google", "model.tier_google"),
		("tier.fireworks", "model.tier_fireworks"),
		("tier.subagent", "model.tier_subagent"),
		("tier.advisor", "model.tier_advisor"),
		("providers.openaiWebsockets", "model.openai_websockets"),
		("providers.openrouterVariant", "model.openrouter_variant"),
		("providers.kimiApiFormat", "model.kimi_api_format"),
		("providers.cacheRetention", "model.cache_retention"),
		("tools.approvalMode", "tools.approval_mode"),
		("compaction.methodOrder", "compaction.method_order"),
		("compaction.keepRecentTokens", "compaction.keep_recent_tokens"),
		("compaction.asyncEnabled", "compaction.async_enabled"),
		("compaction.midTurnEnabled", "compaction.mid_turn_enabled"),
		("contextPromotion.enabled", "context_promotion.enabled"),
		("statusLine.preset", "appearance.statusPreset"),
		("marketplace.autoUpdate", "lifecycle.marketplaceAutoUpdate"),
	] {
		move_key(document, old, new, record);
	}
	if let Some(legacy) = take_path(document, "features.unexpectedStopDetection") {
		let mode = match legacy {
			toml::Value::Boolean(true) => Some("smart"),
			toml::Value::Boolean(false) => Some("none"),
			toml::Value::String(mode) if matches!(mode.as_str(), "none" | "mechanical" | "smart") => {
				set_dotted(document, "interaction.unexpectedStopDetection", toml::Value::String(mode));
				converted(
					record,
					"features.unexpectedStopDetection",
					"interaction.unexpectedStopDetection",
				);
				None
			},
			_ => {
				dropped(record, "features.unexpectedStopDetection", "unexpected-stop mode was invalid");
				None
			},
		};
		if let Some(mode) = mode {
			set_dotted(
				document,
				"interaction.unexpectedStopDetection",
				toml::Value::String(mode.to_owned()),
			);
			converted(
				record,
				"features.unexpectedStopDetection",
				"interaction.unexpectedStopDetection",
			);
		}
	}
	for (old, new) in [
		("async.maxJobs", "async.max_jobs"),
		("async.pollWaitDuration", "async.poll_wait_duration"),
		("bash.enabled", "shell.enabled"),
		("bash.profile", "shell.profile"),
		("bash.args", "shell.args"),
		("bash.login", "shell.login"),
		("bash.commandPrefix", "shell.command_prefix"),
		("bash.embeddedBuiltins", "shell.embedded_builtins"),
		("bash.autoBackground.enabled", "shell.auto_background.enabled"),
		("bash.autoBackground.thresholdMs", "shell.auto_background.threshold_ms"),
		("bash.direnv", "shell.direnv"),
		("bash.direnvLoadTimeoutMs", "shell.direnv_load_timeout_ms"),
		("bash.patterns", "shell.interceptor.patterns"),
		("bashInterceptor.enabled", "shell.interceptor.enabled"),
		("bashInterceptor.patterns", "shell.interceptor.patterns"),
		("shellMinimizer.enabled", "shell.minimizer.enabled"),
		("shellMinimizer.settingsPath", "shell.minimizer.settings_path"),
		("shellMinimizer.only", "shell.minimizer.only"),
		("shellMinimizer.except", "shell.minimizer.except"),
		("shellMinimizer.maxCaptureBytes", "shell.minimizer.max_capture_bytes"),
		("shellMinimizer.sourceOutlineLevel", "shell.minimizer.source_outline_level"),
		("shellMinimizer.legacyFilters", "shell.minimizer.legacy_filters"),
		("shellPath", "shell.executable"),
	] {
		move_key(document, old, new, record);
	}

	if let Some(value) = take_path(document, "collapseChangelog") {
		if let toml::Value::Boolean(collapsed) = value {
			set_dotted(
				document,
				"startup.changelog_mode",
				toml::Value::String(if collapsed { "summary" } else { "expanded" }.to_owned()),
			);
			converted(record, "collapseChangelog", "startup.changelog_mode");
		}
	}
	let changelog_version = take_path(document, "lastChangelogVersion")
		.and_then(|value| value.as_str().map(str::to_owned));
	if changelog_version.is_some() {
		converted(record, "lastChangelogVersion", "last-changelog-version state marker");
	}
	if let Some(theme) = take_path(document, "theme") {
		match theme {
			toml::Value::String(theme) => {
				set_dotted(document, "appearance.theme", toml::Value::String(theme));
				converted(record, "theme", "appearance.theme");
			},
			toml::Value::Table(mut themes) => {
				let selected = themes
					.remove("dark")
					.or_else(|| themes.remove("light"))
					.and_then(|value| value.as_str().map(str::to_owned));
				if let Some(theme) = selected {
					set_dotted(document, "appearance.theme", toml::Value::String(theme));
					converted(record, "theme.dark/theme.light", "appearance.theme");
				} else {
					dropped(record, "theme", "theme table did not contain a string variant");
				}
			},
			_ => dropped(record, "theme", "theme value was not a string or variant table"),
		}
	}
	if value_at_mut(document, "appearance.theme").is_some_and(|value| value.is_table())
		&& let Some(toml::Value::Table(mut themes)) = take_path(document, "appearance.theme")
	{
		let selected = themes
			.remove("dark")
			.or_else(|| themes.remove("light"))
			.and_then(|value| value.as_str().map(str::to_owned));
		if let Some(theme) = selected {
			set_dotted(document, "appearance.theme", toml::Value::String(theme));
			converted(record, "appearance.theme.dark", "appearance.theme");
		} else {
			dropped(record, "appearance.theme", "theme table did not contain a string variant");
		}
	}
	if let Some(toml::Value::Boolean(enabled)) =
		value_at_mut(document, "lifecycle.marketplaceAutoUpdate").map(|value| value.clone())
	{
		set_dotted(
			document,
			"lifecycle.marketplaceAutoUpdate",
			toml::Value::String(if enabled { "auto" } else { "off" }.to_owned()),
		);
		converted(record, "lifecycle.marketplaceAutoUpdate", "lifecycle.marketplaceAutoUpdate enum");
	}
	for (old, new, on, off) in [
		("inspect_image.enabled", "inspect_image.mode", "on", "off"),
		("task.eager", "task.eager", "always", "default"),
		("todo.eager", "todo.eager", "always", "default"),
		("snapcompact.systemPrompt", "snapcompact.system_prompt", "all", "none"),
		("inlineToolDescriptors", "inline_tool_descriptors", "on", "off"),
		("codexResets.autoRedeem", "codex_resets.auto_redeem", "yes", "no"),
	] {
		if let Some(toml::Value::Boolean(enabled)) = take_path(document, old) {
			set_dotted(document, new, toml::Value::String(if enabled { on } else { off }.to_owned()));
			converted(record, old, new);
		}
	}
	normalize_power_sleep_prevention(document, record);
	if let Some(toml::Value::Boolean(enabled)) = take_path(document, "task.isolation.enabled") {
		set_dotted(
			document,
			"task.isolation.mode",
			toml::Value::String(if enabled { "auto" } else { "none" }.to_owned()),
		);
		converted(record, "task.isolation.enabled", "task.isolation.mode");
	}
	if let Some(toml::Value::String(mode)) =
		value_at_mut(document, "task.isolation.mode").map(|value| value.clone())
	{
		let replacement = match mode.as_str() {
			"worktree" => Some("rcopy"),
			"fuse-overlay" => Some("overlayfs"),
			"fuse-projfs" => Some("projfs"),
			_ => None,
		};
		if let Some(replacement) = replacement {
			set_dotted(document, "task.isolation.mode", toml::Value::String(replacement.to_owned()));
			converted(record, "task.isolation.mode", "task.isolation.mode");
		}
	}
	if let Some(toml::Value::String(mode)) = take_path(document, "edit.mode")
		&& (mode == "atom" || mode == "vim")
	{
		set_dotted(document, "tools.edit_dialect", toml::Value::String("hashline".to_owned()));
		converted(record, "edit.mode", "tools.edit_dialect");
	}
	if take_path(document, "edit.modelVariants").is_some() {
		dropped(record, "edit.modelVariants", "model-specific edit variants are unsupported");
	}
	if let Some(toml::Value::String(strategy)) = take_path(document, "compaction.strategy") {
		let order = match strategy.as_str() {
			"off" => Vec::new(),
			"context-full" => vec!["remote", "soft"],
			"handoff" => vec!["handoff", "remote", "soft"],
			"shake" | "shake-summary" => vec!["shake", "remote", "soft"],
			"snapcompact" => vec!["snapcompact", "remote", "soft"],
			_ => Vec::new(),
		};
		if !order.is_empty() || strategy == "off" {
			set_dotted(
				document,
				"compaction.method_order",
				toml::Value::Array(
					order
						.into_iter()
						.map(|item| toml::Value::String(item.to_owned()))
						.collect(),
				),
			);
			converted(record, "compaction.strategy", "compaction.method_order");
		}
	}
	if let Some(toml::Value::Integer(timeout)) = take_path(document, "ask.timeout") {
		let seconds = if timeout > 1_000 {
			(timeout + 500) / 1_000
		} else {
			timeout
		};
		set_dotted(document, "interaction.askTimeoutSeconds", toml::Value::Integer(seconds.max(0)));
		converted(record, "ask.timeout", "interaction.askTimeoutSeconds");
	}
	for (old, new) in [
		("providers.webSearch", "providers.web_search_order"),
		("providers.image", "providers.image_order"),
	] {
		if let Some(toml::Value::String(provider)) = take_path(document, old)
			&& provider != "auto"
		{
			let order: &[&str] = if old == "providers.webSearch" {
				&[
					"perplexity",
					"gemini",
					"anthropic",
					"codex",
					"xai",
					"zai",
					"exa",
					"tinyfish",
					"jina",
					"kagi",
					"tavily",
					"firecrawl",
					"brave",
					"kimi",
					"parallel",
					"synthetic",
					"searxng",
					"startpage",
					"duckduckgo",
					"ecosia",
					"google",
					"mojeek",
					"public",
				]
			} else {
				&["openai", "openai-codex", "antigravity", "xai", "openrouter", "gemini"]
			};
			if order.contains(&provider.as_str()) {
				let values = iter::once(provider.as_str())
					.chain(
						order
							.iter()
							.copied()
							.filter(|candidate| *candidate != provider.as_str()),
					)
					.map(|value| toml::Value::String(value.to_owned()))
					.collect();
				set_dotted(document, new, toml::Value::Array(values));
				converted(record, old, new);
			}
		}
	}
	for (old, new) in [("find", "glob"), ("search", "grep"), ("mnemosyne", "mnemopi")] {
		if value_at_mut(document, new).is_none()
			&& let Some(value) = take_path(document, old)
		{
			set_dotted(document, new, value);
			converted(record, old, new);
		}
	}
	if matches!(
		value_at_mut(document, "memory.backend").and_then(|value| value.as_str()),
		Some("mnemosyne")
	) {
		set_dotted(document, "memory.backend", toml::Value::String("mnemopi".to_owned()));
		converted(record, "memory.backend=mnemosyne", "memory.backend=mnemopi");
	}
	if let Some(enabled) = take_path(document, "memories.enabled") {
		match enabled.as_bool() {
			Some(false) if value_at_mut(document, "memory.backend").is_none() => {
				set_dotted(document, "memory.backend", toml::Value::String("off".to_owned()));
				converted(record, "memories.enabled=false", "memory.backend=off");
			},
			Some(false) => {
				dropped(record, "memories.enabled", "native memory.backend takes precedence");
			},
			Some(true) => {
				dropped(
					record,
					"memories.enabled",
					"legacy local memory is unsupported; memory remains off unless mnemopi is explicit",
				);
			},
			None => {
				dropped(record, "memories.enabled", "invalid legacy memory toggle");
			},
		}
	}
	if let Some(toml::Value::Table(mut exa)) = take_path(document, "exa") {
		let flags = [exa.remove("enabled"), exa.remove("enableSearch")]
			.into_iter()
			.flatten()
			.filter_map(|value| value.as_bool())
			.collect::<Vec<_>>();
		exa.remove("enableResearcher");
		exa.remove("enableWebsets");
		if !flags.is_empty() {
			exa.insert("enabled".to_owned(), toml::Value::Boolean(flags.into_iter().all(|flag| flag)));
		}
		if !exa.is_empty() {
			document.insert("exa".to_owned(), toml::Value::Table(exa));
		}
		converted(record, "exa legacy toggles", "exa.enabled");
	}
	changelog_version
}

fn normalize_power_sleep_prevention(document: &mut toml::Table, record: &mut MigrationRecord) {
	let destination = "power.sleep_prevention";
	let native_explicit = value_at_mut(document, destination).is_some();
	if let Some(value) = take_compat_value(document, "power.sleepPrevention", record) {
		if native_explicit {
			dropped(record, "power.sleepPrevention", "native destination already has precedence");
		} else {
			set_dotted(document, destination, value);
			converted(record, "power.sleepPrevention", destination);
		}
	}
	let destination_explicit = value_at_mut(document, destination).is_some();
	let flags = [
		"power.preventIdleSleep",
		"power.preventSystemSleep",
		"power.declareUserActive",
		"power.preventDisplaySleep",
	]
	.map(|path| (path, take_compat_value(document, path, record)));
	if destination_explicit {
		for (path, value) in flags {
			if value.is_some() {
				dropped(record, path, "explicit sleep-prevention mode already has precedence");
			}
		}
		return;
	}
	let flag = |path: &str| {
		flags
			.iter()
			.find(|(candidate, _)| *candidate == path)
			.and_then(|(_, value)| value.as_ref())
			.and_then(toml::Value::as_bool)
	};
	let any_valid = flags
		.iter()
		.any(|(_, value)| value.as_ref().is_some_and(toml::Value::is_bool));
	if any_valid {
		let mode = if flag("power.preventSystemSleep") == Some(true)
			|| flag("power.declareUserActive") == Some(true)
		{
			"system"
		} else if flag("power.preventDisplaySleep") == Some(true) {
			"display"
		} else if flag("power.preventIdleSleep") != Some(false) {
			"idle"
		} else {
			"off"
		};
		set_dotted(document, destination, toml::Value::String(mode.to_owned()));
	}
	for (path, value) in flags {
		match value {
			Some(toml::Value::Boolean(_)) => converted(record, path, destination),
			Some(_) => dropped(record, path, "legacy sleep-prevention flag was not boolean"),
			None => {},
		}
	}
}

fn take_compat_value(
	document: &mut toml::Table,
	path: &str,
	record: &mut MigrationRecord,
) -> Option<toml::Value> {
	let nested = take_path(document, path);
	let flat = document.remove(path);
	if nested.is_some() && flat.is_some() {
		dropped(record, path, "duplicate flat spelling was ignored");
	}
	nested.or(flat)
}

pub(crate) fn normalize_legacy_layer(document: &mut toml::Table) {
	let mut ignored = MigrationRecord::default();
	let _ = convert_legacy(document, &mut ignored);
	remove_unsupported(document, &mut ignored);
	reject_credentials(document, &mut ignored, "");
}

fn validate_migration_candidate(
	document: &toml::Table,
	catalog: SettingsCatalog,
) -> Result<(), MigrationError> {
	for domain in catalog.descriptors() {
		(domain.validate)(document, catalog)?;
	}
	Ok(())
}

fn remove_unsupported(document: &mut toml::Table, record: &mut MigrationRecord) {
	for path in [
		"bm25",
		"task.simple",
		"computer.backend",
		"read.model",
		"readHashLines",
		"read.hashLines",
		"providers.parallelFetch",
		"providers.parallel_fetch",
		"lsp.shared",
		"bash",
		"bashInterceptor",
		"shellMinimizer",
	] {
		if take_path(document, path).is_some() {
			dropped(record, path, "retired setting");
		}
	}
	for path in [
		"memories",
		"hindsight",
		"localMemory",
		"local_memory",
		"mentalModel",
		"mental_model",
		"commit",
		"claude",
		"codex",
		"gemini",
		"foreignSource",
		"foreign_source",
	] {
		if take_path(document, path).is_some() {
			dropped(record, path, "unsupported or dropped OMP scope");
		}
	}
	if matches!(
		value_at_mut(document, "memory.backend").and_then(|value| value.as_str()),
		Some("local" | "local-lite" | "hindsight")
	) {
		set_dotted(document, "memory.backend", toml::Value::String("off".to_owned()));
		dropped(record, "memory.backend", "unsupported memory backend; reset to off");
	}
}

fn reject_credentials(table: &mut toml::Table, record: &mut MigrationRecord, prefix: &str) {
	let keys = table.keys().cloned().collect::<Vec<_>>();
	for key in keys {
		let path = if prefix.is_empty() {
			key.clone()
		} else {
			format!("{prefix}.{key}")
		};
		let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
		if normalized.contains("apikey")
			|| normalized.contains("accesstoken")
			|| normalized.contains("refreshtoken")
			|| normalized == "token"
			|| normalized == "secret"
		{
			table.remove(&key);
			record.entries.push(MigrationEntry {
				path,
				action: MigrationAction::CredentialRejected,
				reason: "combined provider/MCP token import API unavailable".to_owned(),
			});
		} else if let Some(child) = table.get_mut(&key).and_then(toml::Value::as_table_mut) {
			reject_credentials(child, record, &path);
		}
	}
}

fn converted(record: &mut MigrationRecord, old: &str, new: &str) {
	record.entries.push(MigrationEntry {
		path:   old.to_owned(),
		action: MigrationAction::Converted,
		reason: format!("moved to {new}"),
	});
}

fn dropped(record: &mut MigrationRecord, path: &str, reason: &str) {
	record.entries.push(MigrationEntry {
		path:   path.to_owned(),
		action: MigrationAction::Dropped,
		reason: reason.to_owned(),
	});
}

fn move_key(document: &mut toml::Table, old: &str, new: &str, record: &mut MigrationRecord) {
	let destination_exists = value_at_mut(document, new).is_some();
	if let Some(value) = take_path(document, old) {
		if destination_exists {
			dropped(record, old, "native destination already has precedence");
		} else {
			set_dotted(document, new, value);
			converted(record, old, new);
		}
	}
}

fn set_dotted(document: &mut toml::Table, path: &str, value: toml::Value) {
	let mut segments = path.split('.').peekable();
	let mut table = document;
	while let Some(segment) = segments.next() {
		if segments.peek().is_none() {
			table.insert(segment.to_owned(), value);
			return;
		}
		let entry = table
			.entry(segment.to_owned())
			.or_insert_with(|| toml::Value::Table(toml::Table::new()));
		if !entry.is_table() {
			*entry = toml::Value::Table(toml::Table::new());
		}
		table = entry.as_table_mut().expect("table established above");
	}
}

fn take_path(document: &mut toml::Table, path: &str) -> Option<toml::Value> {
	let mut segments = path.split('.').peekable();
	let mut table = document;
	while let Some(segment) = segments.next() {
		if segments.peek().is_none() {
			return table.remove(segment);
		}
		table = table.get_mut(segment)?.as_table_mut()?;
	}
	None
}

fn value_at_mut<'a>(document: &'a mut toml::Table, path: &str) -> Option<&'a mut toml::Value> {
	let mut segments = path.split('.').peekable();
	let mut table = document;
	while let Some(segment) = segments.next() {
		if segments.peek().is_none() {
			return table.get_mut(segment);
		}
		table = table.get_mut(segment)?.as_table_mut()?;
	}
	None
}

use std::iter;

use rusqlite::OptionalExtension as _;
use toml::{de, ser};

/// Legacy settings migration failure.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
	/// Migration directory creation failed.
	#[error("failed to create migration directory {path}")]
	CreateDirectory {
		/// Data directory that stores migration outputs and markers.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while creating the data directory.
		source: io::Error,
	},
	/// A legacy source could not be read.
	#[error("failed to read legacy settings source {path}")]
	Read {
		/// Legacy JSON source or existing native TOML file being loaded.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while reading the source file.
		source: io::Error,
	},
	/// A source backup could not be created.
	#[error("failed to back up legacy settings source {path} to {backup}")]
	Backup {
		/// Legacy settings source preserved before migration.
		path:   PathBuf,
		/// Migration-specific backup receiving a copy of the source.
		backup: PathBuf,
		#[source]
		/// I/O failure returned while copying the source to its backup.
		source: io::Error,
	},
	/// JSONC parsing failed.
	#[error(transparent)]
	Jsonc(#[from] omp_slopjson::ParseError),
	/// A legacy JSON root was not an object.
	#[error("legacy settings JSON root must be an object")]
	JsonRootNotObject,
	/// Existing native configuration was invalid and cannot safely be merged.
	#[error("existing native settings file {path} is invalid")]
	ExistingConfig {
		/// Existing native settings file that must be merged with imported
		/// values.
		path:   PathBuf,
		#[source]
		/// TOML failure returned while parsing the existing settings.
		source: de::Error,
	},
	/// Legacy database access failed.
	#[error(transparent)]
	Database(#[from] rusqlite::Error),
	/// One database setting did not contain valid JSON.
	#[error("legacy database setting {key} is invalid JSON")]
	DatabaseValue {
		/// Legacy database setting whose stored value is malformed JSON.
		key:    String,
		#[source]
		/// JSON failure returned while decoding the stored setting.
		source: serde_json::Error,
	},
	/// The normalized migration candidate violated a linked runtime schema.
	#[error(transparent)]
	Validation(#[from] ValidationError),
	/// Native TOML encoding failed.
	#[error(transparent)]
	Encode(#[from] ser::Error),
	/// Atomic persistence failed.
	#[error(transparent)]
	Io(#[from] SettingsIoError),
}

#[cfg(test)]
mod tests {
	use super::*;

	const CATALOG: SettingsCatalog = SettingsCatalog::new(&[&crate::SETTINGS_CONTRIBUTION]);

	#[test]
	fn migration_is_recorded_secret_free_and_idempotent() {
		let directory = tempfile::tempdir().expect("directory");
		fs::write(
			directory.path().join("settings.json"),
			"{ // legacy\n defaultModel: 'demo/model', collapseChangelog: false, apiKey: \
			 'never-report', hindsight: { enabled: true }, }",
		)
		.expect("legacy");
		let first = migrate_legacy_settings(directory.path(), CATALOG).expect("migrate");
		let MigrationOutcome::Completed(record) = first else {
			panic!("first migration")
		};
		assert!(
			record
				.entries
				.iter()
				.any(|entry| entry.action == MigrationAction::CredentialRejected)
		);
		assert!(record.entries.iter().any(|entry| entry.path == "hindsight"));
		let report = fs::read_to_string(directory.path().join(RECORD)).expect("record");
		assert!(!report.contains("never-report"));
		let config = fs::read_to_string(directory.path().join("config.toml")).expect("config");
		assert!(config.contains("default = \"demo/model\""));
		assert!(!config.contains("apiKey"));
		assert_eq!(
			migrate_legacy_settings(directory.path(), CATALOG).expect("idempotent"),
			MigrationOutcome::AlreadyCompleted,
		);
	}
	#[test]
	fn prompt_compatibility_migration_records_conversion_and_preserves_native_values() {
		let directory = tempfile::tempdir().expect("directory");
		fs::write(
			directory.path().join("settings.json"),
			"{ includeModelInPrompt: false, includeWorkspaceTree: true }",
		)
		.expect("legacy");
		fs::write(directory.path().join("config.toml"), "[prompt]\nincludeModelInPrompt = true\n")
			.expect("native");
		let MigrationOutcome::Completed(record) =
			migrate_legacy_settings(directory.path(), CATALOG).expect("migrate")
		else {
			panic!("migration completed")
		};
		assert!(record.entries.iter().any(|entry| {
			entry.path == "includeModelInPrompt"
				&& entry.action == MigrationAction::Converted
				&& entry.reason == "moved to prompt.includeModelInPrompt"
		}));
		assert!(record.entries.iter().any(|entry| {
			entry.path == "includeWorkspaceTree"
				&& entry.action == MigrationAction::Converted
				&& entry.reason == "moved to prompt.includeWorkspaceTree"
		}));
		let document: toml::Table =
			toml::from_str(&fs::read_to_string(directory.path().join("config.toml")).expect("config"))
				.expect("native document");
		let prompt = document
			.get("prompt")
			.and_then(toml::Value::as_table)
			.expect("prompt");
		assert_eq!(
			prompt
				.get("includeModelInPrompt")
				.and_then(toml::Value::as_bool),
			Some(true),
		);
		assert_eq!(
			prompt
				.get("includeWorkspaceTree")
				.and_then(toml::Value::as_bool),
			Some(true),
		);
	}
	fn normalized_power_mode(document: &mut toml::Table) -> Option<&str> {
		value_at_mut(document, "power.sleep_prevention").and_then(|value| value.as_str())
	}

	#[test]
	fn power_sleep_prevention_precedence_removes_legacy_keys_and_records_decisions() {
		let mut document: toml::Table = toml::from_str(
			r#"
"power.preventDisplaySleep" = true

[power]
sleep_prevention = "off"
sleepPrevention = "system"
preventIdleSleep = true
preventSystemSleep = true
declareUserActive = true
"#,
		)
		.expect("power settings");
		let mut record = MigrationRecord::default();
		normalize_power_sleep_prevention(&mut document, &mut record);
		assert_eq!(normalized_power_mode(&mut document), Some("off"));
		for path in [
			"power.sleepPrevention",
			"power.preventIdleSleep",
			"power.preventSystemSleep",
			"power.declareUserActive",
			"power.preventDisplaySleep",
		] {
			assert!(value_at_mut(&mut document, path).is_none(), "{path}");
			assert!(!document.contains_key(path), "{path}");
			assert!(
				record
					.entries
					.iter()
					.any(|entry| { entry.path == path && entry.action == MigrationAction::Dropped })
			);
		}
	}

	#[test]
	fn power_sleep_prevention_maps_pi_enum_and_boolean_defaults() {
		for (source, expected) in [
			("[power]\nsleepPrevention = \"display\"\npreventSystemSleep = true\n", "display"),
			("\"power.sleepPrevention\" = \"system\"\n", "system"),
			("[power]\npreventSystemSleep = true\n", "system"),
			("[power]\ndeclareUserActive = true\n", "system"),
			("[power]\npreventDisplaySleep = true\n", "display"),
			("[power]\npreventIdleSleep = true\n", "idle"),
			("[power]\npreventIdleSleep = false\n", "off"),
			("[power]\npreventSystemSleep = false\n", "idle"),
			("\"power.preventIdleSleep\" = false\n", "off"),
		] {
			let mut document: toml::Table = toml::from_str(source).expect("power case");
			let mut record = MigrationRecord::default();
			normalize_power_sleep_prevention(&mut document, &mut record);
			assert_eq!(normalized_power_mode(&mut document), Some(expected), "{source}");
			assert!(!record.entries.is_empty(), "{source}");
		}
		let mut defaults = toml::Table::new();
		normalize_power_sleep_prevention(&mut defaults, &mut MigrationRecord::default());
		assert_eq!(normalized_power_mode(&mut defaults), None);
	}

	#[test]
	fn legacy_key_normalizer_maps_every_supported_runtime_owner() {
		let mut document: toml::Table = toml::from_str(
			r#"
queueMode = "all"
includeModelInPrompt = false
includeWorkspaceTree = true
modelProviderOrder = ["anthropic", "openai"]
defaultThinkingLevel = "high"
theme = "solarized"
modelRoleStorage = "project"
modelRoles = { smol = "openai/gpt-5-mini" }
modelTags = { smol = { name = "Small", hidden = false } }
cycleOrder = ["smol", "default", "slow"]
enabledModels = ["openai/gpt-5-mini"]
disabledProviders = ["anthropic"]

[thinkingBudgets]
low = 1024
medium = 2048

[tier]
openai = "priority"

[providers]
openaiWebsockets = "on"
cacheRetention = "long"

[tools]
approvalMode = "write"

[compaction]
methodOrder = ["remote", "soft"]
keepRecentTokens = 1234
asyncEnabled = true
midTurnEnabled = false

[contextPromotion]
enabled = true

[ask]
timeout = 2500

[bash]
profile = "interactive"
patterns = [{ pattern = "sudo", action = "ask" }]

[statusLine]
preset = "compact"

[marketplace]
autoUpdate = false
"#,
		)
		.expect("legacy document");
		normalize_legacy_layer(&mut document);
		assert_eq!(
			value_at_mut(&mut document, "interaction.steeringMode").and_then(|value| value.as_str()),
			Some("all"),
		);
		assert!(value_at_mut(&mut document, "enabledModels").is_none());
		assert!(value_at_mut(&mut document, "includeModelInPrompt").is_none());
		assert!(value_at_mut(&mut document, "includeWorkspaceTree").is_none());
		assert_eq!(
			value_at_mut(&mut document, "prompt.includeModelInPrompt")
				.and_then(|value| value.as_bool()),
			Some(false),
		);
		assert_eq!(
			value_at_mut(&mut document, "prompt.includeWorkspaceTree")
				.and_then(|value| value.as_bool()),
			Some(true),
		);
		assert_eq!(
			value_at_mut(&mut document, "model.role_storage").and_then(|value| value.as_str()),
			Some("project"),
		);
		assert_eq!(
			value_at_mut(&mut document, "model.roles.smol").and_then(|value| value.as_str()),
			Some("openai/gpt-5-mini"),
		);
		assert_eq!(
			value_at_mut(&mut document, "model.tags.smol.name").and_then(|value| value.as_str()),
			Some("Small"),
		);
		assert_eq!(
			value_at_mut(&mut document, "model.cycle_order")
				.and_then(|value| value.as_array())
				.map(Vec::len),
			Some(3),
		);
		assert_eq!(
			value_at_mut(&mut document, "model.enabled_models")
				.and_then(|value| value.as_array())
				.map(Vec::len),
			Some(1),
		);
		assert_eq!(
			value_at_mut(&mut document, "model.disabled_providers")
				.and_then(|value| value.as_array())
				.map(Vec::len),
			Some(1),
		);
		assert_eq!(
			value_at_mut(&mut document, "model.provider_order")
				.and_then(|value| value.as_array())
				.map(Vec::len),
			Some(2),
		);
		assert_eq!(
			value_at_mut(&mut document, "model.default_thinking").and_then(|value| value.as_str()),
			Some("high"),
		);
		assert!(value_at_mut(&mut document, "model.thinking_budgets").is_some());
		assert_eq!(
			value_at_mut(&mut document, "model.tier_openai").and_then(|value| value.as_str()),
			Some("priority"),
		);
		assert_eq!(
			value_at_mut(&mut document, "model.openai_websockets").and_then(|value| value.as_str()),
			Some("on"),
		);
		assert_eq!(
			value_at_mut(&mut document, "model.cache_retention").and_then(|value| value.as_str()),
			Some("long"),
		);
		assert_eq!(
			value_at_mut(&mut document, "appearance.theme").and_then(|value| value.as_str()),
			Some("solarized"),
		);
		assert_eq!(
			value_at_mut(&mut document, "appearance.statusPreset").and_then(|value| value.as_str()),
			Some("compact"),
		);
		assert_eq!(
			value_at_mut(&mut document, "lifecycle.marketplaceAutoUpdate")
				.and_then(|value| value.as_str()),
			Some("off"),
		);
		assert_eq!(
			value_at_mut(&mut document, "interaction.askTimeoutSeconds")
				.and_then(|value| value.as_integer()),
			Some(3),
		);
		assert_eq!(
			value_at_mut(&mut document, "context_promotion.enabled").and_then(|value| value.as_bool()),
			Some(true),
		);
		assert!(value_at_mut(&mut document, "shell.interceptor.patterns").is_some());
		assert_eq!(
			value_at_mut(&mut document, "shell.profile").and_then(|value| value.as_str()),
			Some("interactive"),
		);
		assert_eq!(
			value_at_mut(&mut document, "tools.approval_mode").and_then(|value| value.as_str()),
			Some("write"),
		);
		assert_eq!(
			value_at_mut(&mut document, "compaction.keep_recent_tokens")
				.and_then(|value| value.as_integer()),
			Some(1234),
		);
		assert_eq!(
			value_at_mut(&mut document, "compaction.async_enabled").and_then(|value| value.as_bool()),
			Some(true),
		);
		assert_eq!(
			value_at_mut(&mut document, "compaction.mid_turn_enabled")
				.and_then(|value| value.as_bool()),
			Some(false),
		);
	}
}
