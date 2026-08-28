//! Canonical TOML keybinding decoding and one-way legacy import.

use std::{
	collections::{BTreeMap, BTreeSet},
	fs, io,
	path::{Path, PathBuf},
};

use omp_core::Str;
use omp_settings::io::atomic_replace;
use serde::{Deserialize, Serialize};
use toml::{de, ser};

use super::{KeyPlatform, fallback_chords};

/// A named keybinding profile with action-to-chord mappings.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct KeybindingProfile {
	/// `profiles.<name>.extends` accepts a profile name; omission means no
	/// inheritance.
	pub extends:  Option<Str>,
	/// `profiles.<name>.bindings` maps canonical action ids to ordered
	/// `modifier+key` arrays; omission uses platform defaults.
	#[serde(default)]
	pub bindings: BTreeMap<Str, Vec<Str>>,
}

/// Canonical `keybindings.toml` document.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct KeybindingsConfig {
	/// `active` accepts a profile name; omission leaves profile selection to the
	/// caller.
	pub active:   Option<Str>,
	/// `profiles` maps profile names to binding tables and defaults to an empty
	/// map.
	#[serde(default)]
	pub profiles: BTreeMap<Str, KeybindingProfile>,
}

/// Origin of a decoded keybinding document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeybindingsSource {
	/// Canonical native TOML.
	NativeToml(PathBuf),
	/// One-time imported JSON source.
	LegacyJson(PathBuf),
	/// One-time imported YAML source.
	LegacyYaml(PathBuf),
}

/// Decoded config and its explicit source label.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedKeybindings {
	/// Typed config.
	pub config: KeybindingsConfig,
	/// Source used for this load/import.
	pub source: KeybindingsSource,
}
/// A duplicate chord in the effective profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeybindingConflict {
	/// Normalized chord claimed by multiple actions.
	pub chord:   Str,
	/// Canonical action ids claiming the chord.
	pub actions: Vec<Str>,
}

/// Fully inherited and validated bindings for one profile.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResolvedKeybindings {
	/// Canonical action ids and normalized chords.
	pub bindings:  BTreeMap<Str, Vec<Str>>,
	/// Ambiguous chords in the effective profile.
	pub conflicts: Vec<KeybindingConflict>,
}

impl ResolvedKeybindings {
	/// Iterates configured chords, or platform fallbacks when the action has no
	/// explicit effective binding.
	pub fn chords_for<'a>(
		&'a self,
		action: &'a str,
		platform: KeyPlatform,
	) -> impl Iterator<Item = &'a str> + Clone {
		let configured = self
			.bindings
			.get(action)
			.map(Vec::as_slice)
			.unwrap_or_default();
		let fallback = if configured.is_empty() {
			fallback_chords(action, platform)
		} else {
			&[]
		};
		configured
			.iter()
			.map(Str::as_str)
			.chain(fallback.iter().copied())
	}
}

const LEGACY_ACTION_IDS: &[(&str, &str)] = &[
	("interrupt", "app.interrupt"),
	("app.copy", "app.clipboard.copy_prompt"),
	("cycleThinkingLevel", "app.thinking.cycle"),
	("toggleThinking", "app.thinking.toggle"),
	("cycleModelForward", "app.model.cycle_forward"),
	("cycleModelBackward", "app.model.cycle_backward"),
	("selectModel", "app.model.select"),
	("externalEditor", "app.editor.external"),
	("followUp", "app.message.follow_up"),
	("retry", "app.retry"),
	("dequeue", "app.message.dequeue"),
	("pasteImage", "app.clipboard.paste_image"),
	("newSession", "app.session.new"),
	("fork", "app.session.fork"),
	("resume", "app.session.resume"),
	("observeSessions", "app.session.observe"),
	("togglePlanMode", "app.plan.toggle"),
	("cursorUp", "tui.editor.cursor_up"),
	("cursorDown", "tui.editor.cursor_down"),
	("submit", "tui.input.submit"),
	("selectUp", "tui.select.up"),
	("selectDown", "tui.select.down"),
	("selectConfirm", "tui.select.confirm"),
	("selectCancel", "tui.select.cancel"),
];

/// TUI primitives available to every keybinding profile.
pub const TUI_ACTION_IDS: &[&str] = &[
	"tui.editor.cursor_up",
	"tui.editor.cursor_down",
	"tui.editor.copy",
	"tui.editor.cut",
	"tui.input.submit",
	"tui.input.paste",
	"tui.input.paste_raw",
	"tui.select.up",
	"tui.select.down",
	"tui.select.confirm",
	"tui.select.cancel",
];

/// Application actions merged with the TUI primitive registry.
pub const APP_ACTION_IDS: &[&str] = &[
	"app.interrupt",
	"app.clear",
	"app.exit",
	"app.thinking.cycle",
	"app.thinking.toggle",
	"app.model.cycle_forward",
	"app.model.cycle_backward",
	"app.model.select",
	"app.model.hub",
	"app.tools.toggle_tree",
	"app.tools.toggle_visibility",
	"app.editor.external",
	"app.message.follow_up",
	"app.retry",
	"app.message.dequeue",
	"app.history.search",
	"app.debug.menu",
	"app.clipboard.paste_image",
	"app.clipboard.paste_raw",
	"app.clipboard.copy_prompt",
	"app.clipboard.copy_line",
	"app.plan.toggle",
	"app.voice.toggle",
	"app.voice.live_toggle",
	"app.session.new",
	"app.session.fork",
	"app.session.resume",
	"app.session.observe",
	"app.session.rename",
	"app.session.delete",
	"app.session.fold",
	"app.session.unfold",
	"app.session.toggle_path",
	"app.session.toggle_sort",
	"app.agent_hub",
];

/// Iterates the single merged application/TUI action authority.
pub fn action_ids() -> impl Iterator<Item = &'static str> + Clone {
	TUI_ACTION_IDS.iter().chain(APP_ACTION_IDS).copied()
}

/// Migrates a legacy action id and verifies it against the merged registry.
pub fn canonical_action_id(action: &str) -> Option<&str> {
	let canonical = LEGACY_ACTION_IDS
		.iter()
		.find_map(|(legacy, canonical)| (*legacy == action).then_some(*canonical))
		.unwrap_or(action);
	action_ids()
		.any(|known| known == canonical)
		.then_some(canonical)
}

impl KeybindingsConfig {
	/// Resolves profile inheritance, migrates legacy action names, validates
	/// chords, and reports ambiguous effective bindings.
	pub fn resolve(
		&self,
		profile: Option<&str>,
	) -> Result<ResolvedKeybindings, KeybindingsConfigError> {
		let profile = profile
			.or(self.active.as_deref())
			.ok_or(KeybindingsConfigError::NoActiveProfile)?;
		let mut visiting = BTreeSet::new();
		let mut bindings = BTreeMap::new();
		self.resolve_into(profile, &mut visiting, &mut bindings)?;
		let mut claims = BTreeMap::<Str, Vec<Str>>::new();
		for (action, chords) in &bindings {
			for chord in chords {
				claims
					.entry(chord.clone())
					.or_default()
					.push(action.clone());
			}
		}
		let conflicts = claims
			.into_iter()
			.filter_map(|(chord, actions)| {
				(actions.len() > 1).then_some(KeybindingConflict { chord, actions })
			})
			.collect();
		Ok(ResolvedKeybindings { bindings, conflicts })
	}

	fn resolve_into(
		&self,
		name: &str,
		visiting: &mut BTreeSet<Str>,
		output: &mut BTreeMap<Str, Vec<Str>>,
	) -> Result<(), KeybindingsConfigError> {
		if !visiting.insert(Str::new(name)) {
			return Err(KeybindingsConfigError::ProfileCycle { profile: name.to_owned() });
		}
		let profile = self
			.profiles
			.get(name)
			.ok_or_else(|| KeybindingsConfigError::UnknownProfile { profile: name.to_owned() })?;
		if let Some(parent) = profile.extends.as_deref() {
			self.resolve_into(parent, visiting, output)?;
		}
		for (raw_action, raw_chords) in &profile.bindings {
			let action = canonical_action_id(raw_action).ok_or_else(|| {
				KeybindingsConfigError::UnknownAction { action: raw_action.to_string() }
			})?;
			let chords = raw_chords
				.iter()
				.map(|chord| normalize_chord(chord))
				.collect::<Result<Vec<_>, _>>()?;
			output.insert(Str::new(action), chords);
		}
		visiting.remove(name);
		Ok(())
	}
}

/// Validates and canonicalizes a `modifier+key` chord.
pub fn normalize_chord(chord: &str) -> Result<Str, KeybindingsConfigError> {
	let parts = chord.split('+').map(str::trim).collect::<Vec<_>>();
	if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
		return Err(KeybindingsConfigError::InvalidChord { chord: chord.to_owned() });
	}
	let key = parts[parts.len() - 1].to_ascii_lowercase();
	if key.len() > 1
		&& !matches!(
			key.as_str(),
			"enter"
				| "escape"
				| "tab" | "space"
				| "backspace"
				| "delete"
				| "up" | "down"
				| "left" | "right"
				| "home" | "end"
				| "pageup"
				| "pagedown"
		) && !key.strip_prefix('f').is_some_and(|number| {
		number
			.parse::<u8>()
			.is_ok_and(|number| (1..=24).contains(&number))
	}) {
		return Err(KeybindingsConfigError::InvalidChord { chord: chord.to_owned() });
	}
	let mut modifiers = BTreeSet::new();
	for modifier in &parts[..parts.len() - 1] {
		let normalized = match modifier.to_ascii_lowercase().as_str() {
			"control" => "ctrl",
			"command" | "cmd" | "meta" => "super",
			"option" => "alt",
			"ctrl" => "ctrl",
			"alt" => "alt",
			"shift" => "shift",
			"super" => "super",
			_ => return Err(KeybindingsConfigError::InvalidChord { chord: chord.to_owned() }),
		};
		if !modifiers.insert(normalized) {
			return Err(KeybindingsConfigError::InvalidChord { chord: chord.to_owned() });
		}
	}
	let mut normalized = modifiers.into_iter().collect::<Vec<_>>();
	normalized.push(&key);
	Ok(Str::new(normalized.join("+")))
}

/// Decodes the only live format, native TOML.
pub fn load(path: &Path) -> Result<LoadedKeybindings, KeybindingsConfigError> {
	let source = fs::read_to_string(path)
		.map_err(|source| KeybindingsConfigError::Read { path: path.to_owned(), source })?;
	let config = toml::from_str(&source)
		.map_err(|source| KeybindingsConfigError::Toml { path: path.to_owned(), source })?;
	Ok(LoadedKeybindings { config, source: KeybindingsSource::NativeToml(path.to_owned()) })
}

/// Imports the first existing legacy JSON/YAML source exactly once. This is not
/// a fallback decoder: after import, only `keybindings.toml` is read.
pub fn import_legacy(
	directory: &Path,
) -> Result<Option<LoadedKeybindings>, KeybindingsConfigError> {
	let native = directory.join("keybindings.toml");
	let marker = directory.join(".keybindings-migration-v1");
	if native.exists() || marker.exists() {
		return Ok(None);
	}
	let candidates = [
		("keybindings.json", LegacyKind::Json),
		("keybindings.yml", LegacyKind::Yaml),
		("keybindings.yaml", LegacyKind::Yaml),
	];
	let Some((path, kind)) = candidates
		.into_iter()
		.map(|(name, kind)| (directory.join(name), kind))
		.find(|(path, _)| path.exists())
	else {
		atomic_replace(&marker, "revision = 1\n")?;
		return Ok(None);
	};
	let source = fs::read_to_string(&path)
		.map_err(|source| KeybindingsConfigError::Read { path: path.clone(), source })?;
	let config = match kind {
		LegacyKind::Json => omp_slopjson::from_str::<KeybindingsConfig>(&source)?,
		LegacyKind::Yaml => serde_yaml::from_str::<KeybindingsConfig>(&source)
			.map_err(|source| KeybindingsConfigError::Yaml { path: path.clone(), source })?,
	};
	atomic_replace(&native, &toml::to_string_pretty(&config)?)?;
	let backup = path.with_file_name(format!(
		"{}.pre-omp-migration.bak",
		path
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("keybindings")
	));
	fs::copy(&path, &backup).map_err(|source| KeybindingsConfigError::Backup {
		path: path.clone(),
		backup,
		source,
	})?;
	let label = match kind {
		LegacyKind::Json => "legacy-json",
		LegacyKind::Yaml => "legacy-yaml",
	};
	atomic_replace(&marker, &format!("revision = 1\nsource = {label:?}\n"))?;
	Ok(Some(LoadedKeybindings {
		config,
		source: match kind {
			LegacyKind::Json => KeybindingsSource::LegacyJson(path),
			LegacyKind::Yaml => KeybindingsSource::LegacyYaml(path),
		},
	}))
}

#[derive(Clone, Copy)]
enum LegacyKind {
	Json,
	Yaml,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn legacy_json_import_has_source_backup_and_native_cutover() {
		let directory = tempfile::tempdir().expect("directory");
		let legacy = directory.path().join("keybindings.json");
		fs::write(
			&legacy,
			"{ active: 'default', profiles: { default: { bindings: { submit: ['enter'], }, }, }, }",
		)
		.expect("legacy");
		let imported = import_legacy(directory.path())
			.expect("import")
			.expect("config");
		assert!(matches!(imported.source, KeybindingsSource::LegacyJson(_)));
		assert_eq!(imported.config.active.as_deref(), Some("default"));
		assert!(
			directory
				.path()
				.join("keybindings.json.pre-omp-migration.bak")
				.exists()
		);
		assert!(import_legacy(directory.path()).expect("second").is_none());
		let native = load(&directory.path().join("keybindings.toml")).expect("native");
		assert!(matches!(native.source, KeybindingsSource::NativeToml(_)));
	}
	#[test]
	fn profile_inheritance_normalizes_chords_and_reports_conflicts() {
		let config = KeybindingsConfig {
			active:   Some(Str::new_static("work")),
			profiles: BTreeMap::from([
				(Str::new_static("base"), KeybindingProfile {
					extends:  None,
					bindings: BTreeMap::from([(Str::new_static("submit"), vec![Str::new_static(
						"Control+Enter",
					)])]),
				}),
				(Str::new_static("work"), KeybindingProfile {
					extends:  Some(Str::new_static("base")),
					bindings: BTreeMap::from([(Str::new_static("retry"), vec![Str::new_static(
						"ctrl+enter",
					)])]),
				}),
			]),
		};
		let resolved = config.resolve(None).expect("resolve");
		assert_eq!(resolved.bindings["tui.input.submit"], vec![Str::new_static("ctrl+enter")]);
		assert_eq!(resolved.conflicts.len(), 1);
	}

	#[test]
	fn profile_cycles_and_invalid_chords_are_rejected() {
		let config = KeybindingsConfig {
			active:   Some(Str::new_static("loop")),
			profiles: BTreeMap::from([(Str::new_static("loop"), KeybindingProfile {
				extends:  Some(Str::new_static("loop")),
				bindings: BTreeMap::new(),
			})]),
		};
		assert!(matches!(config.resolve(None), Err(KeybindingsConfigError::ProfileCycle { .. })));
		assert!(normalize_chord("ctrl+ctrl+x").is_err());
	}
}

/// Native keybinding configuration failure.
#[derive(Debug, thiserror::Error)]
pub enum KeybindingsConfigError {
	/// Reading a source failed.
	#[error("failed to read keybindings source {path}")]
	Read {
		/// Source path whose bytes could not be read.
		path:   PathBuf,
		#[source]
		/// Filesystem error returned while reading the source.
		source: io::Error,
	},
	/// Canonical TOML was malformed.
	#[error("failed to parse native keybindings TOML {path}")]
	Toml {
		/// Native `keybindings.toml` path containing invalid TOML.
		path:   PathBuf,
		#[source]
		/// TOML decoder error identifying the malformed input.
		source: de::Error,
	},
	/// Legacy YAML was malformed.
	#[error("failed to parse legacy keybindings YAML {path}")]
	Yaml {
		/// Legacy YAML path rejected during one-time import.
		path:   PathBuf,
		#[source]
		/// YAML decoder error identifying the malformed input.
		source: serde_yaml::Error,
	},
	/// Legacy JSON/JSONC was malformed.
	#[error(transparent)]
	Json(#[from] omp_slopjson::ParseError),
	/// Native TOML encoding failed.
	#[error(transparent)]
	Encode(#[from] ser::Error),
	/// Atomic persistence failed.
	#[error(transparent)]
	Persist(#[from] omp_settings::io::SettingsIoError),
	/// A legacy source backup failed.
	#[error("failed to back up keybindings source {path} to {backup}")]
	Backup {
		/// Legacy source retained after successful native conversion.
		path:   PathBuf,
		/// `.pre-omp-migration.bak` destination that could not be written.
		backup: PathBuf,
		#[source]
		/// Filesystem error returned while copying the source.
		source: io::Error,
	},
	/// No profile was explicitly requested or selected.
	#[error("no active keybinding profile is selected")]
	NoActiveProfile,
	/// A selected or inherited profile does not exist.
	#[error("unknown keybinding profile {profile}")]
	UnknownProfile {
		/// Name from the `active` or `profiles.<name>.extends` config key.
		profile: String,
	},
	/// Profile inheritance contains a cycle.
	#[error("keybinding profile inheritance cycle at {profile}")]
	ProfileCycle {
		/// Profile name revisited while following `profiles.<name>.extends`.
		profile: String,
	},
	/// An action id is not present in either the application or TUI registry.
	#[error("unknown keybinding action {action}")]
	UnknownAction {
		/// Unsupported `profiles.<name>.bindings` key; accepted keys are
		/// canonical action ids.
		action: String,
	},
	/// A chord has invalid modifiers, key spelling, or duplicate modifiers.
	#[error("invalid keybinding chord {chord}")]
	InvalidChord {
		/// Rejected `modifier+key` value; modifiers and key names are
		/// case-insensitive.
		chord: String,
	},
}
