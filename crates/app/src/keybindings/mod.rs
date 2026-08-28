//! Native keybinding configuration.

pub mod config;

use std::collections::BTreeMap;

use omp_chat_ui::host::InputAction;
use omp_core::Str;
use omp_envd::exthost::{UiCallbackOwner, UiShortcutRosterEntry};
use omp_proto::ui::v1::ShortcutDecl;
use omp_tui::{Chord, Key, Keymap, Mods};

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

struct PlatformChords {
	default: &'static [&'static str],
	windows: &'static [&'static str],
}

impl PlatformChords {
	const fn all(chords: &'static [&'static str]) -> Self {
		Self { default: chords, windows: chords }
	}

	const fn windows(default: &'static [&'static str], windows: &'static [&'static str]) -> Self {
		Self { default, windows }
	}

	const fn for_platform(&self, platform: KeyPlatform) -> &'static [&'static str] {
		match platform {
			KeyPlatform::MacOs => self.default,
			KeyPlatform::Windows => self.windows,
			KeyPlatform::Unix => self.default,
		}
	}
}

/// One configurable application action and the fallback chords that dispatch
/// it.
pub(crate) struct ActionHotkey {
	/// Canonical keybinding action id.
	pub action_id: &'static str,
	/// Semantic chat action dispatched by every effective chord.
	pub action:    InputAction,
	defaults:      PlatformChords,
}

impl ActionHotkey {
	const fn new(action_id: &'static str, action: InputAction, defaults: PlatformChords) -> Self {
		Self { action_id, action, defaults }
	}
}

const ACTION_HOTKEYS: &[ActionHotkey] = &[
	ActionHotkey::new("app.interrupt", InputAction::Interrupt, PlatformChords::all(&["escape"])),
	ActionHotkey::new("app.clear", InputAction::Clear, PlatformChords::all(&["ctrl+c"])),
	ActionHotkey::new("app.exit", InputAction::Exit, PlatformChords::all(&["ctrl+d"])),
	ActionHotkey::new(
		"app.thinking.cycle",
		InputAction::CycleThinking,
		PlatformChords::all(&["shift+tab"]),
	),
	ActionHotkey::new(
		"app.thinking.toggle",
		InputAction::ToggleThinking,
		PlatformChords::all(&["ctrl+t"]),
	),
	ActionHotkey::new(
		"app.model.cycle_forward",
		InputAction::CycleModelForward,
		PlatformChords::all(&["ctrl+p"]),
	),
	ActionHotkey::new(
		"app.model.cycle_backward",
		InputAction::CycleModelBackward,
		PlatformChords::all(&["ctrl+shift+p"]),
	),
	ActionHotkey::new("app.model.select", InputAction::SelectModel, PlatformChords::all(&["alt+p"])),
	ActionHotkey::new("app.model.hub", InputAction::OpenModelHub, PlatformChords::all(&["alt+m"])),
	ActionHotkey::new(
		"app.tools.toggle_tree",
		InputAction::ToggleToolTree,
		PlatformChords::all(&["ctrl+o"]),
	),
	ActionHotkey::new(
		"app.tools.toggle_visibility",
		InputAction::ToggleToolVisibility,
		PlatformChords::all(&["ctrl+shift+o"]),
	),
	ActionHotkey::new(
		"app.editor.external",
		InputAction::ExternalEditor,
		PlatformChords::all(&["ctrl+g"]),
	),
	ActionHotkey::new(
		"app.message.follow_up",
		InputAction::FollowUp,
		PlatformChords::windows(&["alt+enter"], &["alt+enter", "ctrl+q"]),
	),
	ActionHotkey::new("app.retry", InputAction::Retry, PlatformChords::all(&["alt+r"])),
	ActionHotkey::new(
		"app.message.dequeue",
		InputAction::Dequeue,
		PlatformChords::all(&["alt+up", "shift+up"]),
	),
	ActionHotkey::new(
		"app.plan.toggle",
		InputAction::TogglePlan,
		PlatformChords::all(&["alt+shift+p"]),
	),
	ActionHotkey::new(
		"app.history.search",
		InputAction::HistorySearch,
		PlatformChords::all(&["ctrl+r"]),
	),
	ActionHotkey::new(
		"app.debug.menu",
		InputAction::DebugMenu,
		PlatformChords::all(&["ctrl+shift+d"]),
	),
	ActionHotkey::new(
		"app.clipboard.copy_prompt",
		InputAction::CopyPrompt,
		PlatformChords::all(&["alt+shift+c"]),
	),
	ActionHotkey::new(
		"app.clipboard.copy_line",
		InputAction::CopyLine,
		PlatformChords::all(&["alt+shift+l"]),
	),
	ActionHotkey::new(
		"app.voice.toggle",
		InputAction::ToggleVoice,
		PlatformChords::all(&["ctrl+alt+s"]),
	),
	ActionHotkey::new(
		"app.voice.live_toggle",
		InputAction::ToggleLiveVoice,
		PlatformChords::all(&["ctrl+alt+l"]),
	),
	ActionHotkey::new("app.agent_hub", InputAction::AgentHub, PlatformChords::all(&["alt+a"])),
];

/// Iterates configurable application hotkeys used by runtime dispatch.
pub(crate) fn action_hotkeys() -> impl Iterator<Item = &'static ActionHotkey> + Clone {
	ACTION_HOTKEYS.iter()
}

#[derive(Clone, Copy)]
struct FixedBinding {
	chord: &'static str,
	key:   Key,
}

impl FixedBinding {
	const fn new(chord: &'static str, key: Key) -> Self {
		Self { chord, key }
	}
}

enum HotkeyKeys {
	Action { action_id: &'static str, repeat: usize },
	Fixed(&'static [FixedBinding]),
	Approval,
}

struct Hotkey {
	context: &'static str,
	keys:    HotkeyKeys,
	action:  Option<&'static str>,
}

impl Hotkey {
	const fn action(
		context: &'static str,
		action_id: &'static str,
		repeat: usize,
		action: &'static str,
	) -> Self {
		Self { context, keys: HotkeyKeys::Action { action_id, repeat }, action: Some(action) }
	}

	const fn fixed(
		context: &'static str,
		keys: &'static [FixedBinding],
		action: &'static str,
	) -> Self {
		Self { context, keys: HotkeyKeys::Fixed(keys), action: Some(action) }
	}

	const fn approval() -> Self {
		Self { context: "Approval", keys: HotkeyKeys::Approval, action: None }
	}
}

const HOTKEYS: &[Hotkey] = &[
	Hotkey::fixed(
		"Composer",
		&[FixedBinding::new("enter", Key::Enter)],
		"Steer active turn or submit",
	),
	Hotkey::action("Composer", "app.message.follow_up", 1, "Queue follow-up"),
	Hotkey::action("Composer", "app.debug.menu", 1, "Open debug tools"),
	Hotkey::action("Composer", "app.interrupt", 1, "Interrupt active work"),
	Hotkey::action("Composer", "app.interrupt", 2, "Open rewind history"),
	Hotkey::action("Composer", "app.tools.toggle_tree", 1, "Expand exact tool card"),
	Hotkey::action("Composer", "app.tools.toggle_visibility", 1, "Toggle tool activity"),
	Hotkey::action("Composer", "app.clipboard.copy_prompt", 1, "Copy last prompt"),
	Hotkey::action("Composer", "app.clipboard.copy_line", 1, "Copy composer line"),
	Hotkey::action("Composer", "app.thinking.toggle", 1, "Toggle thinking visibility"),
	Hotkey::action("Composer", "app.model.select", 1, "Switch model for this session"),
	Hotkey::action("Composer", "app.history.search", 1, "Search prompt history"),
	Hotkey::action("Composer", "app.message.dequeue", 1, "Restore newest queued item"),
	Hotkey::fixed("Modal", &[FixedBinding::new("enter", Key::Enter)], "Commit highlighted action"),
	Hotkey::fixed(
		"Modal",
		&[FixedBinding::new("escape", Key::Esc)],
		"Cancel modal; never trigger composer shortcuts",
	),
	Hotkey::fixed(
		"Modal",
		&[FixedBinding::new("tab", Key::Tab), FixedBinding::new("shift+tab", Key::BackTab)],
		"Move focus",
	),
	Hotkey::approval(),
];

/// Builds the fixed contextual mappings shared by terminal and native GUI
/// input.
pub(crate) fn keymap() -> Result<Keymap, config::KeybindingsConfigError> {
	let mut keymap = Keymap::default();
	for hotkey in HOTKEYS {
		match &hotkey.keys {
			HotkeyKeys::Fixed(bindings) => {
				for binding in *bindings {
					keymap.bind(parse_chord(binding.chord)?, binding.key);
				}
			},
			HotkeyKeys::Approval => {
				for (key, _) in omp_chat_ui::approval_hotkeys() {
					keymap.bind(Chord::new(Key::Char(key), Mods::default()), Key::Char(key));
				}
			},
			HotkeyKeys::Action { .. } => {},
		}
	}
	Ok(keymap)
}

/// Appends hotkey help from the same action and fixed mappings used at runtime.
pub(crate) fn append_hotkey_help(
	help: &mut String,
	resolved: &config::ResolvedKeybindings,
	platform: KeyPlatform,
) {
	help.push_str("\n**Hotkeys**\n\n| Context | Key | Action |\n|---|---|---|\n");
	for hotkey in HOTKEYS {
		help.push_str("| ");
		help.push_str(hotkey.context);
		help.push_str(" | ");
		match &hotkey.keys {
			HotkeyKeys::Action { action_id, repeat } => {
				let mut first = true;
				for chord in resolved.chords_for(action_id, platform) {
					if !first {
						help.push_str(" / ");
					}
					first = false;
					let label = format_chord_label(chord, platform)
						.expect("resolved action chord must remain normalized");
					help.push('`');
					for index in 0..*repeat {
						if index > 0 {
							help.push(' ');
						}
						help.push_str(label.as_str());
					}
					help.push('`');
				}
			},
			HotkeyKeys::Fixed(bindings) => {
				for (index, binding) in bindings.iter().enumerate() {
					if index > 0 {
						help.push_str(" / ");
					}
					let label = format_chord_label(binding.chord, platform)
						.expect("fixed hotkey chord must remain normalized");
					help.push('`');
					help.push_str(label.as_str());
					help.push('`');
				}
			},
			HotkeyKeys::Approval => {
				for (index, (key, _)) in omp_chat_ui::approval_hotkeys().enumerate() {
					if index > 0 {
						help.push_str(" / ");
					}
					help.push('`');
					help.push(key);
					help.push('`');
				}
			},
		}
		help.push_str(" | ");
		if let Some(action) = hotkey.action {
			help.push_str(action);
		} else {
			for (index, (_, action)) in omp_chat_ui::approval_hotkeys().enumerate() {
				if index > 0 {
					help.push_str(" / ");
				}
				help.push_str(action);
			}
		}
		help.push_str(" |\n");
	}
}

fn parse_chord(chord: &str) -> Result<Chord, config::KeybindingsConfigError> {
	let chord = config::normalize_chord(chord)?;
	Chord::parse(chord.as_str())
		.map_err(|_| config::KeybindingsConfigError::InvalidChord { chord: chord.to_string() })
}

const NO_FALLBACK: &[&str] = &[];

/// Resolves platform-specific fallback chords for an unconfigured action.
pub fn fallback_chords(action: &str, platform: KeyPlatform) -> &'static [&'static str] {
	ACTION_HOTKEYS
		.iter()
		.find(|hotkey| hotkey.action_id == action)
		.map_or(NO_FALLBACK, |hotkey| hotkey.defaults.for_platform(platform))
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
		if label.len() == 1 && label.as_bytes()[0].is_ascii_lowercase() {
			output.push(char::from(label.as_bytes()[0].to_ascii_uppercase()));
		} else {
			output.push_str(label);
		}
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
		assert_eq!(fallback_chords("app.message.dequeue", KeyPlatform::MacOs), [
			"alt+up", "shift+up",
		]);
		assert_eq!(
			format_chord_label("cmd+option+p", KeyPlatform::MacOs).expect("label"),
			"Option+Cmd+P"
		);
		assert_eq!(
			format_chord_label("super+alt+p", KeyPlatform::Unix).expect("label"),
			"Alt+Super+P"
		);
	}
	#[test]
	fn effective_action_chord_drives_runtime_and_help() {
		let resolved = config::ResolvedKeybindings {
			bindings:  BTreeMap::from([(Str::new_static("app.debug.menu"), vec![Str::new_static(
				"ctrl+j",
			)])]),
			conflicts: Vec::new(),
		};
		let hotkey = action_hotkeys()
			.find(|hotkey| hotkey.action_id == "app.debug.menu")
			.expect("debug hotkey");
		let chord = resolved
			.chords_for(hotkey.action_id, KeyPlatform::Unix)
			.next()
			.expect("effective debug chord");
		let active_keymap = keymap().expect("shared keymap");
		let binding =
			omp_chat_ui::host::InputBinding::parse_with(&active_keymap, chord, hotkey.action.clone())
				.expect("runtime binding");

		assert_eq!(
			Some(binding.key),
			active_keymap.resolve(Chord::parse(chord).expect("effective chord")),
		);
		assert_eq!(binding.action, InputAction::DebugMenu);
		let mut help = String::new();
		append_hotkey_help(&mut help, &resolved, KeyPlatform::Unix);
		assert!(help.contains("| Composer | `Ctrl+J` | Open debug tools |"));
		assert!(!help.contains("Ctrl+Shift+D"));
	}

	#[test]
	fn fixed_help_chords_are_installed_in_the_shared_keymap() {
		let keymap = keymap().expect("fixed hotkey registry");
		for (chord, key) in [
			("enter", Key::Enter),
			("escape", Key::Esc),
			("shift+tab", Key::BackTab),
			("1", Key::Char('1')),
			("4", Key::Char('4')),
		] {
			assert_eq!(keymap.resolve(Chord::parse(chord).expect("fixed chord")), Some(key));
		}
		let mut help = String::new();
		append_hotkey_help(&mut help, &config::ResolvedKeybindings::default(), KeyPlatform::Unix);
		assert!(help.contains("| Modal | `Tab` / `Shift+Tab` | Move focus |"));
		assert!(
			help.contains("| Approval | `1` / `2` / `3` / `4` | Once / always / amend / reject |")
		);
	}

	#[test]
	fn every_runtime_hotkey_has_a_canonical_action_id() {
		for hotkey in action_hotkeys() {
			assert_eq!(
				config::canonical_action_id(hotkey.action_id),
				Some(hotkey.action_id),
				"{} is missing from the config registry",
				hotkey.action_id,
			);
		}
	}
	#[test]
	fn every_help_action_references_a_runtime_hotkey() {
		for hotkey in HOTKEYS {
			let HotkeyKeys::Action { action_id, .. } = &hotkey.keys else {
				continue;
			};
			assert!(
				action_hotkeys().any(|runtime| runtime.action_id == *action_id),
				"{action_id} has help metadata but no runtime binding",
			);
		}
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
