//! Reflected, typed settings command handlers.

use std::{
	env,
	fs::{self, OpenOptions},
	io,
	path::{Path, PathBuf},
};

use miette::IntoDiagnostic as _;
use omp_con::{Ctx, Origin, RegItem, Source, Span, TypeSpec, Value, ValueKind, VarFlags};
use omp_core::Str;
use omp_envd::mcp::{
	config::McpServerConfig,
	config_store::{McpConfigStore, set_server_enabled},
	json_rpc,
};
use serde::Serialize;

use crate::cli::{ConfigCommand, ConfigScope, McpConfigCommand, McpConfigScope};

/// Runs a typed command-stream configuration operation.
pub fn run(data_dir: &Path, command: &ConfigCommand) -> miette::Result<()> {
	let project = env::current_dir().into_diagnostic()?;
	if let ConfigCommand::InitXdg { json } = command {
		return init_xdg(data_dir, *json);
	}
	if let ConfigCommand::Mcp { command } = command {
		return run_mcp(data_dir, &project, command);
	}
	match command {
		ConfigCommand::Migrate => {
			let destination = migrate_settings(data_dir, &project)?;
			println!("{}", destination.display());
			Ok(())
		},
		ConfigCommand::Dump => {
			print!("{}", crate::process_ctx(&project)?.dump());
			Ok(())
		},
		ConfigCommand::List { json } => list(&crate::process_ctx(&project)?, *json),
		ConfigCommand::Get { key } => get(&crate::process_ctx(&project)?, key),
		ConfigCommand::Set { key, value, scope } => set_persisted(&project, *scope, key, value),
		ConfigCommand::Unset { key, scope } => {
			let destination = path(&project, *scope)?;
			let ctx = load_cfg(&destination)?;
			let RegItem::Var(spec) = ctx
				.find(key)
				.ok_or_else(|| miette::miette!("unknown convar `{key}`; run `omp config list`"))?
			else {
				return Err(miette::miette!("`{key}` is not a convar"));
			};
			ctx.set(spec.name, (spec.default)(), Origin::Archive)
				.into_diagnostic()?;
			persist_cfg(&destination, &ctx)
		},
		ConfigCommand::Path { scope } => {
			println!("{}", path(&project, *scope)?.display());
			Ok(())
		},
		ConfigCommand::InitXdg { .. } => unreachable!("XDG initialization returns before config"),
		ConfigCommand::Mcp { .. } => unreachable!("MCP commands return before config composition"),
	}
}

#[derive(Serialize)]
struct XdgMigrationReport {
	data:    PathBuf,
	state:   PathBuf,
	cache:   PathBuf,
	moved:   Vec<PathBuf>,
	skipped: Vec<PathBuf>,
}

fn init_xdg(data_dir: &Path, json: bool) -> miette::Result<()> {
	let home = env::var_os("HOME")
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
		.ok_or_else(|| miette::miette!("HOME must be set for config init-xdg"))?;
	let mut roots = omp_core::dirs::native_directories(&home);
	roots.data = data_dir.to_path_buf();
	for root in [&roots.data, &roots.state, &roots.cache] {
		fs::create_dir_all(root).into_diagnostic()?;
	}
	let legacy = home.join(".omp");
	let mut report = XdgMigrationReport {
		data:    roots.data.clone(),
		state:   roots.state.clone(),
		cache:   roots.cache.clone(),
		moved:   Vec::new(),
		skipped: Vec::new(),
	};
	for (source, destination) in [
		(legacy.join("data"), roots.data.clone()),
		(legacy.join("state"), roots.state.clone()),
		(legacy.join("cache"), roots.cache.clone()),
		(legacy.join("sessions"), roots.state.join("sessions")),
		(legacy.join("projects"), roots.state.join("projects")),
	] {
		if source.exists() {
			migrate_without_overwrite(&source, &destination, &mut report)?;
		}
	}
	if json {
		println!("{}", serde_json::to_string_pretty(&report).into_diagnostic()?);
	} else {
		println!("data\t{}", report.data.display());
		println!("state\t{}", report.state.display());
		println!("cache\t{}", report.cache.display());
		println!("migrated\t{}", report.moved.len());
		println!("preserved-conflicts\t{}", report.skipped.len());
	}
	Ok(())
}

fn migrate_without_overwrite(
	source: &Path,
	destination: &Path,
	report: &mut XdgMigrationReport,
) -> miette::Result<()> {
	let metadata = fs::symlink_metadata(source).into_diagnostic()?;
	if metadata.file_type().is_symlink() {
		report.skipped.push(source.to_path_buf());
		return Ok(());
	}
	if metadata.is_file() {
		if destination.exists() {
			report.skipped.push(source.to_path_buf());
			return Ok(());
		}
		if let Some(parent) = destination.parent() {
			fs::create_dir_all(parent).into_diagnostic()?;
		}
		let mut input = fs::File::open(source).into_diagnostic()?;
		let mut output = OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(destination)
			.into_diagnostic()?;
		if let Err(error) = io::copy(&mut input, &mut output)
			.and_then(|_| output.sync_all())
			.and_then(|()| fs::set_permissions(destination, metadata.permissions()))
		{
			drop(output);
			let _ = fs::remove_file(destination);
			return Err(error).into_diagnostic();
		}
		fs::remove_file(source).into_diagnostic()?;
		report.moved.push(source.to_path_buf());
		return Ok(());
	}
	if !metadata.is_dir() {
		report.skipped.push(source.to_path_buf());
		return Ok(());
	}
	if destination.exists() && !destination.is_dir() {
		report.skipped.push(source.to_path_buf());
		return Ok(());
	}
	fs::create_dir_all(destination).into_diagnostic()?;
	let mut entries = fs::read_dir(source)
		.into_diagnostic()?
		.collect::<Result<Vec<_>, _>>()
		.into_diagnostic()?;
	entries.sort_by_key(fs::DirEntry::file_name);
	for entry in entries {
		migrate_without_overwrite(&entry.path(), &destination.join(entry.file_name()), report)?;
	}
	if fs::read_dir(source).into_diagnostic()?.next().is_none() {
		fs::remove_dir(source).into_diagnostic()?;
	}
	Ok(())
}

fn run_mcp(data_dir: &Path, project: &Path, command: &McpConfigCommand) -> miette::Result<()> {
	let user = McpConfigStore::new(mcp_path(data_dir, project, McpConfigScope::Global));
	let project_store = McpConfigStore::new(mcp_path(data_dir, project, McpConfigScope::Project));
	let root = McpConfigStore::new(mcp_path(data_dir, project, McpConfigScope::Root));
	match command {
		McpConfigCommand::List { scope, json } => {
			let stores: Vec<(McpConfigScope, &McpConfigStore)> = match scope {
				Some(McpConfigScope::Global) => vec![(McpConfigScope::Global, &user)],
				Some(McpConfigScope::Project) => vec![(McpConfigScope::Project, &project_store)],
				Some(McpConfigScope::Root) => vec![(McpConfigScope::Root, &root)],
				None => vec![
					(McpConfigScope::Project, &project_store),
					(McpConfigScope::Global, &user),
					(McpConfigScope::Root, &root),
				],
			};
			if *json {
				let mut output = serde_json::Map::new();
				for (scope, store) in stores {
					for name in store.list().into_diagnostic()? {
						output
							.insert(name.to_string(), serde_json::json!({"scope": mcp_scope_name(scope)}));
					}
				}
				println!("{}", serde_json::to_string_pretty(&output).into_diagnostic()?);
			} else {
				for (scope, store) in stores {
					for name in store.list().into_diagnostic()? {
						println!("{}\t{name}", mcp_scope_name(scope));
					}
				}
			}
			Ok(())
		},
		McpConfigCommand::Get { name } => {
			for (scope, store) in [
				(McpConfigScope::Project, &project_store),
				(McpConfigScope::Global, &user),
				(McpConfigScope::Root, &root),
			] {
				if let Some(server) = store.get(name).into_diagnostic()? {
					println!(
						"{}",
						serde_json::to_string_pretty(&serde_json::json!({
							"name": name,
							"scope": mcp_scope_name(scope),
							"config": redacted_server(&server),
						}))
						.into_diagnostic()?
					);
					return Ok(());
				}
			}
			Err(miette::miette!("MCP server `{name}` was not found in native configuration"))
		},
		McpConfigCommand::Add { name, config, scope } => {
			let server: McpServerConfig = serde_json::from_str(config).into_diagnostic()?;
			mcp_store(*scope, &user, &project_store, &root)
				.add(name, server)
				.into_diagnostic()
		},
		McpConfigCommand::Update { name, config, scope } => {
			let server: McpServerConfig = serde_json::from_str(config).into_diagnostic()?;
			mcp_store(*scope, &user, &project_store, &root)
				.update(name, server)
				.into_diagnostic()
		},
		McpConfigCommand::Remove { name, scope } => mcp_store(*scope, &user, &project_store, &root)
			.remove(name)
			.into_diagnostic(),
		McpConfigCommand::Enable { name } | McpConfigCommand::Disable { name } => set_server_enabled(
			&user,
			&project_store,
			Some((&root, true)),
			name,
			matches!(command, McpConfigCommand::Enable { .. }),
		)
		.into_diagnostic(),
	}
}

fn mcp_store<'a>(
	scope: McpConfigScope,
	user: &'a McpConfigStore,
	project: &'a McpConfigStore,
	root: &'a McpConfigStore,
) -> &'a McpConfigStore {
	match scope {
		McpConfigScope::Global => user,
		McpConfigScope::Project => project,
		McpConfigScope::Root => root,
	}
}

fn mcp_path(data_dir: &Path, project: &Path, scope: McpConfigScope) -> PathBuf {
	match scope {
		McpConfigScope::Global => data_dir.join("mcp.json"),
		McpConfigScope::Project => project.join(".omp/mcp.json"),
		McpConfigScope::Root => project.join(".mcp.json"),
	}
}

fn mcp_scope_name(scope: McpConfigScope) -> &'static str {
	scope.into()
}

fn redacted_server(server: &McpServerConfig) -> serde_json::Value {
	let mut value = serde_json::to_value(server).unwrap_or(serde_json::Value::Null);
	if let Some(url) = value.get_mut("url")
		&& let Some(raw) = url.as_str()
	{
		*url = serde_json::Value::String(json_rpc::redact_url_for_log(raw).to_string());
	}
	for map_name in ["env", "headers"] {
		if let Some(values) = value
			.get_mut(map_name)
			.and_then(serde_json::Value::as_object_mut)
		{
			for (name, value) in values {
				let name = name.to_ascii_lowercase();
				if ["key", "token", "secret", "authorization", "cookie"]
					.iter()
					.any(|needle| name.contains(needle))
				{
					*value = serde_json::Value::String("[REDACTED]".to_owned());
				}
			}
		}
	}
	value
}

/// Returns the selected command-stream configuration path.
pub fn path(project: &Path, scope: ConfigScope) -> miette::Result<PathBuf> {
	Ok(match scope {
		ConfigScope::Global => crate::config_path().into_diagnostic()?,
		ConfigScope::Project => project.join(".omp/config.cfg"),
	})
}

/// Loads an existing cfg leniently: lines this build no longer understands are
/// reported and dropped, so an edit never fails on a stale variable and the
/// re-dumped file no longer carries it.
fn load_cfg(path: &Path) -> miette::Result<Ctx> {
	let ctx = Ctx::new();
	// The default bind cfg is the baseline the persisted script diffs
	// against; without it a dump would `unbindall` the defaults away.
	ctx.exec(
		crate::keybindings::DEFAULT_BINDS,
		Source::Config(Str::new_static(crate::keybindings::DEFAULT_BINDS_NAME)),
	)
	.into_diagnostic()?;
	ctx.seal_bind_defaults();
	if path.is_file() {
		let script = crate::keybindings::migrate_generated_preamble(
			&fs::read_to_string(path).into_diagnostic()?,
		);
		let outcome =
			ctx.exec_configs(&|name: &str| (name == "config.cfg").then(|| Str::new(&script)), None);
		if outcome.failed > 0 {
			eprintln!(
				"warning: {} skipped {} statement(s) this build does not understand",
				path.display(),
				outcome.failed
			);
		}
	}
	Ok(ctx)
}

fn persist_cfg(path: &Path, ctx: &Ctx) -> miette::Result<()> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).into_diagnostic()?;
	}
	let temporary = path.with_extension(format!("cfg.tmp.{}", std::process::id()));
	fs::write(&temporary, ctx.dump().as_bytes()).into_diagnostic()?;
	if let Err(source) = fs::rename(&temporary, path) {
		let _ = fs::remove_file(&temporary);
		return Err(source).into_diagnostic();
	}
	Ok(())
}

fn assignment(ctx: &Ctx, name: &str, input: &str) -> miette::Result<String> {
	let RegItem::Var(spec) = ctx
		.find(name)
		.ok_or_else(|| miette::miette!("unknown convar `{name}`; run `omp config list`"))?
	else {
		return Err(miette::miette!("`{name}` is not a convar"));
	};
	let value = if spec.ty.kind == ValueKind::Str {
		serde_json::to_string(input).into_diagnostic()?
	} else {
		input.to_owned()
	};
	Ok(format!("{name} {value}"))
}

fn list(ctx: &Ctx, json: bool) -> miette::Result<()> {
	let mut vars = ctx
		.items()
		.filter_map(|item| match item {
			RegItem::Var(spec) => Some(spec),
			_ => None,
		})
		.collect::<Vec<_>>();
	vars.sort_unstable_by_key(|spec| spec.name);
	if json {
		let mut output = serde_json::Map::new();
		for spec in vars {
			output.insert(
				spec.name.to_owned(),
				serde_json::json!({
					"value": ctx.value(spec.name).into_diagnostic()?.to_string(),
					"default": (spec.default)().to_string(),
					"flags": flag_names(spec.flags),
				}),
			);
		}
		println!("{}", serde_json::to_string_pretty(&output).into_diagnostic()?);
		return Ok(());
	}
	for spec in vars {
		println!(
			"{}\t{}\t{}\t{}",
			spec.name,
			ctx.value(spec.name).into_diagnostic()?,
			(spec.default)(),
			flag_names(spec.flags).join("|"),
		);
	}
	Ok(())
}

fn get(ctx: &Ctx, name: &str) -> miette::Result<()> {
	let value = ctx
		.value(name)
		.map_err(|_| miette::miette!("unknown convar `{name}`; run `omp config list`"))?;
	println!("{value}");
	Ok(())
}

fn flag_names(flags: VarFlags) -> Vec<&'static str> {
	[
		(VarFlags::ARCHIVE, "ARCHIVE"),
		(VarFlags::SESSION, "SESSION"),
		(VarFlags::INHERIT, "INHERIT"),
		(VarFlags::REPLICATED, "REPLICATED"),
		(VarFlags::READONLY, "READONLY"),
		(VarFlags::NOTIFY, "NOTIFY"),
		(VarFlags::UNSAFE, "UNSAFE"),
	]
	.into_iter()
	.filter_map(|(flag, name)| flags.contains(flag).then_some(name))
	.collect()
}

fn value_at<'a>(document: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
	let mut segments = path.split('.');
	let mut value = document.get(segments.next()?)?;
	for segment in segments {
		value = value.as_table()?.get(segment)?;
	}
	Some(value)
}

/// Migrates legacy TOML settings and keybindings to the archived command
/// stream.
///
/// Re-running migration over unchanged inputs writes identical bytes.
pub fn migrate_settings(data_dir: &Path, project: &Path) -> miette::Result<PathBuf> {
	let mut document = toml::Table::new();
	let mut sources = vec![data_dir.join("config.toml"), project.join(".omp/config.toml")];
	if let Some(overlays) = env::var_os("OMP_CONFIG_FILES") {
		sources.extend(env::split_paths(&overlays));
	}
	for source in sources {
		if !source.is_file() {
			continue;
		}
		let text = fs::read_to_string(&source).into_diagnostic()?;
		let incoming = text.parse::<toml::Table>().into_diagnostic()?;
		merge_toml(&mut document, incoming);
	}
	let ctx = Ctx::new();
	for mappings in [
		omp_catalog::settings::LEGACY_CONVAR_MAPPINGS,
		omp_inference::settings::LEGACY_CONVAR_MAPPINGS,
		omp_tools::settings::LEGACY_CONVAR_MAPPINGS,
		omp_envd::LEGACY_CONVAR_MAPPINGS,
		omp_driver::settings::LEGACY_CONVAR_MAPPINGS,
		crate::voice::settings::LEGACY_CONVAR_MAPPINGS,
	] {
		for &(legacy_path, name) in mappings {
			let Some(value) = value_at(&document, legacy_path) else {
				continue;
			};
			let RegItem::Var(spec) = ctx
				.find(name)
				.ok_or_else(|| miette::miette!("migration target `{name}` is not registered"))?
			else {
				return Err(miette::miette!("migration target `{name}` is not a convar"));
			};
			ctx.set(spec.name, toml_to_value(value, spec.ty)?, Origin::Archive)
				.into_diagnostic()?;
		}
	}
	migrate_keybindings(data_dir, &ctx)?;
	let destination = crate::config_path().into_diagnostic()?;
	persist_cfg(&destination, &ctx)?;
	Ok(destination)
}

fn merge_toml(target: &mut toml::Table, incoming: toml::Table) {
	for (key, value) in incoming {
		match (target.get_mut(&key), value) {
			(Some(toml::Value::Table(target)), toml::Value::Table(incoming)) => {
				merge_toml(target, incoming);
			},
			(_, value) => {
				target.insert(key, value);
			},
		}
	}
}

/// Sets and persists one convar in the selected cfg scope.
pub fn set_persisted(
	project: &Path,
	scope: ConfigScope,
	name: &str,
	value: &str,
) -> miette::Result<()> {
	let destination = path(project, scope)?;
	let ctx = load_cfg(&destination)?;
	let assignment = assignment(&ctx, name, value)?;
	ctx.exec(&assignment, Source::Config(Str::new_static("config.cfg")))
		.into_diagnostic()?;
	persist_cfg(&destination, &ctx)
}

fn migrate_keybindings(data_dir: &Path, ctx: &Ctx) -> miette::Result<()> {
	let path = data_dir.join("keybindings.toml");
	if !path.is_file() {
		return Ok(());
	}
	let text = fs::read_to_string(path).into_diagnostic()?;
	let document = text.parse::<toml::Table>().into_diagnostic()?;
	let active = document
		.get("active")
		.and_then(toml::Value::as_str)
		.unwrap_or("default");
	let Some(bindings) = document
		.get("profiles")
		.and_then(toml::Value::as_table)
		.and_then(|profiles| profiles.get(active))
		.and_then(toml::Value::as_table)
		.and_then(|profile| profile.get("bindings"))
		.and_then(toml::Value::as_table)
	else {
		return Ok(());
	};
	for (action, chords) in bindings {
		let Some(command) = legacy_action_command(action) else {
			continue;
		};
		let Some(chords) = chords.as_array() else {
			continue;
		};
		for chord in chords.iter().filter_map(toml::Value::as_str) {
			ctx.bind(Str::new(chord), Str::new_static(command))
				.into_diagnostic()?;
		}
	}
	Ok(())
}

fn legacy_action_command(action: &str) -> Option<&'static str> {
	crate::keybindings::pi_action_command(action)
}

fn toml_to_value(value: &toml::Value, ty: &TypeSpec) -> miette::Result<Value> {
	match ty.kind {
		ValueKind::Bool => value
			.as_bool()
			.map(Value::Bool)
			.ok_or_else(|| miette::miette!("expected boolean migration value")),
		ValueKind::Int => value
			.as_integer()
			.map(Value::Int)
			.ok_or_else(|| miette::miette!("expected integer migration value")),
		ValueKind::Float => value
			.as_float()
			.or_else(|| value.as_integer().map(|value| value as f64))
			.map(Value::Float)
			.ok_or_else(|| miette::miette!("expected numeric migration value")),
		ValueKind::Str => Ok(Value::Str(Str::new(
			value
				.as_str()
				.map_or_else(|| value.to_string(), str::to_owned),
		))),
		ValueKind::Enum => value
			.as_str()
			.map(|value| Value::Enum(Str::new(value)))
			.ok_or_else(|| miette::miette!("expected enum migration value")),
		ValueKind::Duration => {
			let span = if let Some(value) = value.as_str() {
				value.parse::<Span>().into_diagnostic()?
			} else {
				let millis = value
					.as_integer()
					.and_then(|value| u64::try_from(value).ok())
					.ok_or_else(|| miette::miette!("expected duration migration value"))?;
				Span::millis(millis)
			};
			Ok(Value::Duration(span))
		},
		ValueKind::List => {
			let values = value
				.as_array()
				.ok_or_else(|| miette::miette!("expected list migration value"))?;
			let elem = ty.elem.unwrap_or(TypeSpec::STR);
			values
				.iter()
				.map(|value| {
					if elem.kind == ValueKind::Kv
						&& let Some(value) = value.as_str()
					{
						return Ok(Value::Kv(omp_con::Kv(vec![(
							Str::new_static("value"),
							Value::Str(Str::new(value)),
						)])));
					}
					toml_to_value(value, elem)
				})
				.collect::<miette::Result<Vec<_>>>()
				.map(Value::List)
		},
		ValueKind::Kv => value
			.as_table()
			.ok_or_else(|| miette::miette!("expected table migration value"))
			.and_then(|table| {
				table
					.iter()
					.map(|(key, value)| Ok((Str::new(key), toml_to_untyped_value(value)?)))
					.collect::<miette::Result<Vec<_>>>()
			})
			.map(|entries| Value::Kv(omp_con::Kv(entries))),
	}
}

fn toml_to_untyped_value(value: &toml::Value) -> miette::Result<Value> {
	match value {
		toml::Value::String(value) => Ok(Value::Str(Str::new(value))),
		toml::Value::Integer(value) => Ok(Value::Int(*value)),
		toml::Value::Float(value) => Ok(Value::Float(*value)),
		toml::Value::Boolean(value) => Ok(Value::Bool(*value)),
		toml::Value::Datetime(value) => Ok(Value::Str(Str::new(value.to_string()))),
		toml::Value::Array(values) => values
			.iter()
			.map(toml_to_untyped_value)
			.collect::<miette::Result<Vec<_>>>()
			.map(Value::List),
		toml::Value::Table(table) => table
			.iter()
			.map(|(key, value)| Ok((Str::new(key), toml_to_untyped_value(value)?)))
			.collect::<miette::Result<Vec<_>>>()
			.map(omp_con::Kv)
			.map(Value::Kv),
	}
}
