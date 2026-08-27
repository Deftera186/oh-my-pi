//! Native keybinding configuration.

pub mod config;

use std::collections::BTreeMap;

use omp_core::Str;
use omp_envd::exthost::{UiCallbackOwner, UiShortcutRosterEntry};
use omp_proto::ui::v1::ShortcutDecl;

/// Static extension shortcut metadata matched locally before CONTROL dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionShortcutBinding {
	/// Normalized key chord.
	pub chord:          Str,
	/// Declared action identity.
	pub action_id:      Str,
	/// Stable manifest declaration id.
	pub declaration_id: Str,
	/// Exact owning worker generation.
	pub generation:     u64,
	/// Optional phase filter.
	pub when:           Box<[Str]>,
	/// Exact callback owner when installed from a sealed production roster.
	pub owner:          Option<UiCallbackOwner>,
}

/// Immutable extension shortcut table. Core bindings are never shadowed.
#[derive(Clone, Debug, Default)]
pub struct ExtensionShortcutRoster {
	bindings: BTreeMap<Str, ExtensionShortcutBinding>,
}

/// Static shortcut installation failure.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionShortcutError {
	/// The chord was invalid.
	#[error(transparent)]
	InvalidChord(#[from] config::KeybindingsConfigError),
	/// A core action already owns this chord.
	#[error("extension shortcut {chord} conflicts with core action {action}")]
	CoreConflict {
		/// Normalized conflicting chord.
		chord:  Str,
		/// Incumbent core action.
		action: Str,
	},
	/// Two extension declarations in one atomic generation claimed a chord.
	#[error("duplicate extension shortcut {chord}")]
	Duplicate {
		/// Normalized duplicate chord.
		chord: Str,
	},
}

impl ExtensionShortcutRoster {
	/// Builds one atomic generation after rejecting every core conflict.
	pub fn install(
		declarations: &[ShortcutDecl],
		generation: u64,
		core: &config::ResolvedKeybindings,
		platform: KeyPlatform,
	) -> Result<Self, ExtensionShortcutError> {
		let core_chords = config::action_ids()
			.flat_map(|action| {
				core
					.chords_for(action, platform)
					.map(move |chord| (chord, action))
			})
			.collect::<BTreeMap<_, _>>();
		let mut bindings = BTreeMap::new();
		for declaration in declarations {
			let chord = config::normalize_chord(declaration.chord.as_str())?;
			if let Some(action) = core_chords.get(chord.as_str()) {
				return Err(ExtensionShortcutError::CoreConflict { chord, action: Str::new(*action) });
			}
			let binding = ExtensionShortcutBinding {
				chord: chord.clone(),
				action_id: Str::from(declaration.action_id.as_str()),
				declaration_id: Str::from(declaration.declaration_id.as_str()),
				generation,
				when: declaration.when.iter().map(Str::from).collect(),
				owner: None,
			};
			if bindings.insert(chord.clone(), binding).is_some() {
				return Err(ExtensionShortcutError::Duplicate { chord });
			}
		}
		Ok(Self { bindings })
	}

	/// Builds one atomic exact-generation roster from app-published entries.
	pub fn install_verified(
		entries: &[UiShortcutRosterEntry],
		core: &config::ResolvedKeybindings,
		platform: KeyPlatform,
	) -> Result<Self, ExtensionShortcutError> {
		let declarations = entries
			.iter()
			.map(|entry| entry.declaration.clone())
			.collect::<Vec<_>>();
		let generation = entries.first().map_or(0, |entry| entry.owner.generation);
		let mut roster = Self::install(&declarations, generation, core, platform)?;
		for entry in entries {
			let chord = config::normalize_chord(entry.declaration.chord.as_str())?;
			if let Some(binding) = roster.bindings.get_mut(chord.as_str()) {
				binding.owner = Some(entry.owner.clone());
			}
		}
		Ok(roster)
	}

	/// Matches one keystroke locally and applies its static phase filter.
	pub fn bindings(&self) -> impl Iterator<Item = &ExtensionShortcutBinding> {
		self.bindings.values()
	}

	/// Matches one keystroke locally and applies its static phase filter.
	pub fn match_chord(
		&self,
		chord: &str,
		phase: &str,
	) -> Result<Option<&ExtensionShortcutBinding>, config::KeybindingsConfigError> {
		let chord = config::normalize_chord(chord)?;
		Ok(self.bindings.get(chord.as_str()).filter(|binding| {
			binding.when.is_empty() || binding.when.iter().any(|allowed| allowed.as_str() == phase)
		}))
	}
}

/// Host platform used for fallback chords and user-facing modifier labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyPlatform {
	/// Apple terminals.
	MacOs,
	/// Windows terminals.
	Windows,
	/// Linux and other Unix terminals.
	Unix,
}

impl KeyPlatform {
	/// Returns the platform of the current application build.
	pub const fn current() -> Self {
		#[cfg(target_os = "macos")]
		{
			Self::MacOs
		}
		#[cfg(target_os = "windows")]
		{
			Self::Windows
		}
		#[cfg(not(any(target_os = "macos", target_os = "windows")))]
		{
			Self::Unix
		}
	}
}

const INTERRUPT: &[&str] = &["escape"];
const CLEAR: &[&str] = &["ctrl+c"];
const EXIT: &[&str] = &["ctrl+d"];
const CYCLE_THINKING: &[&str] = &["shift+tab"];
const TOGGLE_THINKING: &[&str] = &["ctrl+t"];
const CYCLE_MODEL_FORWARD: &[&str] = &["ctrl+p"];
const CYCLE_MODEL_BACKWARD: &[&str] = &["ctrl+shift+p"];
const SELECT_MODEL: &[&str] = &["alt+p"];
const TOGGLE_TOOL_TREE: &[&str] = &["ctrl+o"];
const EXTERNAL_EDITOR: &[&str] = &["ctrl+g"];
const FOLLOW_UP_DEFAULT: &[&str] = &["alt+enter"];
const FOLLOW_UP_WINDOWS: &[&str] = &["alt+enter", "ctrl+q"];
const RETRY: &[&str] = &["alt+r"];
const DEQUEUE_MACOS: &[&str] = &["shift+up"];
const DEQUEUE_DEFAULT: &[&str] = &["ctrl+up"];
const TOGGLE_PLAN: &[&str] = &["alt+shift+p"];
const TOGGLE_VOICE: &[&str] = &["ctrl+alt+s"];
const TOGGLE_LIVE_VOICE: &[&str] = &["ctrl+alt+l"];
const AGENT_HUB: &[&str] = &["alt+a"];
const NO_FALLBACK: &[&str] = &[];

/// Resolves platform-specific fallback chords for an unconfigured action.
pub fn fallback_chords(action: &str, platform: KeyPlatform) -> &'static [&'static str] {
	match (action, platform) {
		("app.interrupt", _) => INTERRUPT,
		("app.clear", _) => CLEAR,
		("app.exit", _) => EXIT,
		("app.thinking.cycle", _) => CYCLE_THINKING,
		("app.thinking.toggle", _) => TOGGLE_THINKING,
		("app.model.cycle_forward", _) => CYCLE_MODEL_FORWARD,
		("app.model.cycle_backward", _) => CYCLE_MODEL_BACKWARD,
		("app.model.select", _) => SELECT_MODEL,
		("app.tools.toggle_tree", _) => TOGGLE_TOOL_TREE,
		("app.editor.external", _) => EXTERNAL_EDITOR,
		("app.message.follow_up", KeyPlatform::Windows) => FOLLOW_UP_WINDOWS,
		("app.message.follow_up", _) => FOLLOW_UP_DEFAULT,
		("app.message.dequeue", KeyPlatform::MacOs) => DEQUEUE_MACOS,
		("app.message.dequeue", _) => DEQUEUE_DEFAULT,
		("app.retry", _) => RETRY,
		("app.plan.toggle", _) => TOGGLE_PLAN,
		("app.voice.toggle", _) => TOGGLE_VOICE,
		("app.voice.live_toggle", _) => TOGGLE_LIVE_VOICE,
		("app.agent_hub", _) => AGENT_HUB,
		_ => NO_FALLBACK,
	}
}

/// Formats a canonical chord with platform-native modifier names.
pub fn format_chord_label(
	chord: &str,
	platform: KeyPlatform,
) -> Result<Str, config::KeybindingsConfigError> {
	let chord = config::normalize_chord(chord)?;
	let mut output = String::with_capacity(chord.len() + 8);
	for (index, part) in chord.as_str().split('+').enumerate() {
		if index > 0 {
			output.push('+');
		}
		let label = match (part, platform) {
			("alt", KeyPlatform::MacOs) => "Option",
			("alt", _) => "Alt",
			("super", KeyPlatform::MacOs) => "Cmd",
			("super", KeyPlatform::Windows | KeyPlatform::Unix) => "Super",
			("ctrl", _) => "Ctrl",
			("shift", _) => "Shift",
			("enter", _) => "Enter",
			("escape", _) => "Esc",
			("pageup", _) => "PageUp",
			("pagedown", _) => "PageDown",
			("up", _) => "Up",
			("down", _) => "Down",
			("left", _) => "Left",
			("right", _) => "Right",
			("tab", _) => "Tab",
			("space", _) => "Space",
			("backspace", _) => "Backspace",
			("delete", _) => "Delete",
			("home", _) => "Home",
			("end", _) => "End",
			(key, _) => key,
		};
		output.push_str(label);
	}
	Ok(Str::from(output))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn platform_fallbacks_and_labels_are_native() {
		assert_eq!(fallback_chords("app.message.follow_up", KeyPlatform::Windows), [
			"alt+enter",
			"ctrl+q"
		]);
		assert_eq!(fallback_chords("app.message.dequeue", KeyPlatform::MacOs), ["shift+up"]);
		assert_eq!(
			format_chord_label("cmd+option+p", KeyPlatform::MacOs).expect("label"),
			"Option+Cmd+p"
		);
		assert_eq!(
			format_chord_label("super+alt+p", KeyPlatform::Unix).expect("label"),
			"Alt+Super+p"
		);
	}
	#[test]
	fn extension_shortcuts_match_locally_and_core_always_wins() {
		let core = config::ResolvedKeybindings::default();
		let reserved = ShortcutDecl {
			chord: "ctrl+c".to_owned(),
			action_id: "steal_interrupt".to_owned(),
			declaration_id: "reserved".to_owned(),
			..Default::default()
		};
		assert!(matches!(
			ExtensionShortcutRoster::install(&[reserved], 1, &core, KeyPlatform::Unix),
			Err(ExtensionShortcutError::CoreConflict { .. })
		));

		let first = ShortcutDecl {
			chord: "CTRL+ALT+H".to_owned(),
			action_id: "history".to_owned(),
			declaration_id: "history".to_owned(),
			when: vec!["open".to_owned()],
			..Default::default()
		};
		let roster = ExtensionShortcutRoster::install(&[first], 4, &core, KeyPlatform::Unix)
			.expect("extension shortcut");
		let matched = roster
			.match_chord("alt+ctrl+h", "open")
			.expect("normalized chord")
			.expect("local match");
		assert_eq!(matched.action_id, "history");
		assert_eq!(matched.generation, 4);
		assert!(
			roster
				.match_chord("ctrl+alt+h", "settled")
				.unwrap()
				.is_none()
		);

		let replacement = ShortcutDecl {
			chord: "f5".to_owned(),
			action_id: "refresh".to_owned(),
			declaration_id: "refresh".to_owned(),
			..Default::default()
		};
		let roster = ExtensionShortcutRoster::install(&[replacement], 5, &core, KeyPlatform::Unix)
			.expect("replacement generation");
		assert!(roster.match_chord("ctrl+alt+h", "open").unwrap().is_none());
		assert_eq!(
			roster
				.match_chord("f5", "open")
				.unwrap()
				.unwrap()
				.generation,
			5
		);
	}
}
