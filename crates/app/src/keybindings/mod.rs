//! Native keybinding configuration.

pub mod config;

use omp_core::Str;

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
}
