//! Observer-local composer: a retained editor tree whose draft never enters
//! the session DOM until submission.

use std::{cell::Cell, path::Path, rc::Rc, time::Duration};

use omp_core::Str;
use omp_tui::{Command, Frame, Key, Ui, UiContext, UiEvent, components::EditorPane};

use crate::{
	autocomplete::{PromptAction, PromptActions, composer_chain},
	chrome::{COMPOSER_ID, STATUS_ID, StatusBand, StatusFacts, composer_root},
};

/// Result of applying a composer key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComposerAction {
	/// Composer changed and needs repainting.
	Changed,
	/// Submit the current draft as a prompt.
	Submit(Str),
	/// Run a submitted `/…` line as the console statement after the slash.
	Command(Str),
	/// Write text to the clipboard (the host owns OSC 52 / native access).
	Copy(Str),
	/// No composer action.
	Ignored,
}

/// Retained composer chrome: status band plus the borderless editor.
///
/// The hardware caret is the editor's insertion point; the host places the
/// terminal cursor from [`Composer::frame`].
pub struct Composer {
	ui:      Ui,
	width:   u16,
	/// Prompt action accepted from the `#` menu, applied after the key.
	pending: Rc<Cell<Option<PromptAction>>>,
}

impl Composer {
	/// Creates a focused composer at `width` for the launch facts, with the
	/// slash `roster` and `@` file completion under `project_root`.
	#[must_use]
	pub fn new(
		width: u16,
		ctx: UiContext,
		facts: StatusFacts,
		roster: Vec<Command>,
		project_root: Option<&Path>,
	) -> Self {
		let actions = PromptActions::new();
		let pending = actions.slot();
		let chain = composer_chain(roster, actions, project_root);
		let mut ui = Ui::from_root(composer_root(facts), width, ctx);
		ui.with_component_mut::<EditorPane, _>(COMPOSER_ID, |pane| {
			pane.set_completion(Box::new(chain));
		});
		ui.focus_first();
		Self { ui, width, pending }
	}

	/// Whether the completion dropdown is open (pi routes `Esc` to it before
	/// any global interrupt).
	#[must_use]
	pub fn popup_open(&self) -> bool {
		self
			.ui
			.with_component::<EditorPane, _>(COMPOSER_ID, EditorPane::popup_open)
			.unwrap_or(false)
	}

	/// Replaces the draft, leaving the caret at its end.
	pub fn set_text(&mut self, text: &str) {
		self.ui.set_text(COMPOSER_ID, text);
		self.ui.resize(self.width);
	}

	/// Clears the draft.
	pub fn clear(&mut self) {
		self.set_text("");
	}

	/// Current unsent draft.
	#[must_use]
	pub fn text(&self) -> String {
		self
			.ui
			.values()
			.get(COMPOSER_ID)
			.and_then(serde_json::Value::as_str)
			.map(str::to_owned)
			.unwrap_or_default()
	}

	/// Rendered chrome, including the caret.
	#[must_use]
	pub const fn frame(&self) -> &Frame {
		self.ui.frame()
	}

	/// Chrome height in rows at the current width.
	#[must_use]
	pub const fn height(&self) -> u16 {
		self.ui.height()
	}

	/// Inserts sanitized pasted text at the caret.
	pub fn paste(&mut self, text: &str) {
		let _ = self.ui.handle_paste(text);
	}

	/// Applies one terminal key.
	pub fn key(&mut self, key: Key) -> ComposerAction {
		let (event, claimed) = self.ui.handle_key_claimed(key);
		if let Some(action) = self.pending.take() {
			return self.apply_prompt_action(action);
		}
		match event {
			UiEvent::Submit => {
				let text = self.text();
				if text.trim().is_empty() {
					return ComposerAction::Ignored;
				}
				self.ui.set_text(COMPOSER_ID, "");
				// Large pastes collapse into attachment chips; the submitted text
				// already carries their expansion, so drop the preview band.
				let staged = self
					.ui
					.with_component_mut::<EditorPane, _>(COMPOSER_ID, |pane| pane.attachments().take())
					.unwrap_or_default();
				if !staged.is_empty() {
					self.ui.resize(self.width);
				}
				// pi: a leading `/` line is a command, never a prompt.
				match text.trim_start().strip_prefix('/') {
					Some(command) if !command.starts_with('/') => {
						ComposerAction::Command(Str::new(command.trim()))
					},
					_ => ComposerAction::Submit(Str::new(text)),
				}
			},
			UiEvent::Copied(text) => ComposerAction::Copy(text),
			_ if claimed => ComposerAction::Changed,
			_ => ComposerAction::Ignored,
		}
	}

	/// Runs an accepted `#` prompt action against the editor.
	fn apply_prompt_action(&mut self, action: PromptAction) -> ComposerAction {
		match action {
			PromptAction::CopyLine => {
				let line = self
					.ui
					.with_component::<EditorPane, _>(COMPOSER_ID, |pane| Str::new(pane.current_line()))
					.unwrap_or_default();
				ComposerAction::Copy(line)
			},
			PromptAction::CopyPrompt => ComposerAction::Copy(Str::new(self.text())),
			PromptAction::Undo { transient } => {
				self.ui.with_component_mut::<EditorPane, _>(COMPOSER_ID, |pane| {
					pane.undo_past_transient(&transient);
				});
				ComposerAction::Changed
			},
			PromptAction::MessageEnd | PromptAction::MessageStart => {
				let end = action == PromptAction::MessageEnd;
				self.ui.with_component_mut::<EditorPane, _>(COMPOSER_ID, |pane| {
					pane.move_to_message_edge(end);
				});
				ComposerAction::Changed
			},
			PromptAction::LineStart => {
				self.ui.handle_key(Key::Home);
				ComposerAction::Changed
			},
			PromptAction::LineEnd => {
				self.ui.handle_key(Key::End);
				ComposerAction::Changed
			},
		}
	}

	/// Reflows the chrome for a new terminal width.
	pub fn resize(&mut self, width: u16) {
		self.width = width;
		self.ui.resize(width);
	}

	/// Replaces the presentation context (theme, charset, terminal caps).
	pub fn set_context(&mut self, ctx: UiContext) {
		self.ui.set_context(ctx);
	}

	/// Updates the status band; returns whether it repainted.
	pub fn set_status(&mut self, facts: StatusFacts) -> bool {
		self
			.ui
			.update_component::<StatusBand>(STATUS_ID, |band| band.set_facts(facts))
	}

	/// Advances chrome animations (the working spinner).
	pub fn tick(&mut self, now: Duration) -> bool {
		self.ui.tick(now)
	}

	/// Next animation deadline, if any component asked to be woken.
	#[must_use]
	pub fn next_wake(&self) -> Option<Duration> {
		self.ui.next_wake()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn facts() -> StatusFacts {
		StatusFacts {
			model:          Str::new_static("Sonnet 4.5"),
			thinking:       None,
			cwd:            Str::new_static("~/proj"),
			scratch:        false,
			branch:         None,
			tokens:         0,
			context_window: Some(200_000),
			compact_percent: 80,
			working:        None,
		}
	}

	fn composer() -> Composer {
		Composer::new(
			60,
			UiContext::default(),
			facts(),
			vec![Command::new("help", "Shows a name's description", &[])],
			None,
		)
	}

	fn rows(composer: &Composer) -> Vec<String> {
		omp_tui::frame_text(composer.frame())
			.lines()
			.map(|line| line.trim_end().to_owned())
			.collect()
	}

	#[test]
	fn typing_moves_the_caret_and_enter_submits_then_clears() {
		let mut composer = composer();
		let (column, row) = composer.frame().cursor().expect("caret placed at boot");
		assert_eq!((column, row), (3, 2));
		for character in "hi".chars() {
			assert_eq!(composer.key(Key::Char(character)), ComposerAction::Changed);
		}
		assert_eq!(composer.text(), "hi");
		assert_eq!(composer.frame().cursor(), Some((5, 2)));
		// pi `band` shape: `╰─ ` gutter at column 0, paddingX 0, no frame.
		assert_eq!(rows(&composer)[2], "╰─ hi");
		assert_eq!(composer.key(Key::Enter), ComposerAction::Submit(Str::new_static("hi")));
		assert_eq!(composer.text(), "");
		assert_eq!(composer.frame().cursor(), Some((3, 2)));
	}

	/// pi `useTerminalCursor`: the caret cell is never painted as a block;
	/// only the frame's hardware cursor moves.
	#[test]
	fn caret_cell_stays_unstyled_while_typing() {
		let mut composer = composer();
		for character in "hi".chars() {
			composer.key(Key::Char(character));
		}
		let frame = composer.frame();
		let (column, row) = frame.cursor().expect("caret placed");
		let theme = UiContext::default().theme;
		for x in 0..frame.size().width {
			assert_ne!(
				frame.cell(x, row).style().background_color(),
				theme.accent,
				"column {x} paints a software caret; hardware caret is at {column}"
			);
		}
	}

	#[test]
	fn slash_opens_the_command_popup_below_the_prompt_and_enter_runs_it() {
		let mut composer = composer();
		assert!(!composer.popup_open());
		assert_eq!(composer.key(Key::Char('/')), ComposerAction::Changed);
		assert!(composer.popup_open(), "slash opens the roster");
		let rows = rows(&composer);
		let prompt = rows.iter().position(|row| row.starts_with("╰─ /")).expect("prompt row");
		assert!(rows[prompt + 1].contains("help"), "{rows:?}");
		assert!(rows[prompt + 1].contains("Shows a name's description"), "{rows:?}");
		assert_eq!(composer.key(Key::Esc), ComposerAction::Changed);
		assert!(!composer.popup_open(), "esc closes the popup");
		for character in "help".chars() {
			composer.key(Key::Char(character));
		}
		assert_eq!(composer.key(Key::Enter), ComposerAction::Command(Str::new_static("help")));
		assert_eq!(composer.text(), "");
	}

	#[test]
	fn hash_menu_runs_prompt_actions_and_removes_the_trigger() {
		let mut composer = composer();
		for character in "hello world".chars() {
			composer.key(Key::Char(character));
		}
		composer.key(Key::Home);
		composer.key(Key::Char('#'));
		assert!(composer.popup_open(), "# opens prompt actions");
		let rows = rows(&composer);
		assert!(rows.iter().any(|row| row.contains("Copy current line")), "{rows:?}");
		// pi: a space ends the `#query` token, so the query is one word.
		for character in "msgend".chars() {
			composer.key(Key::Char(character));
		}
		assert_eq!(composer.key(Key::Tab), ComposerAction::Changed);
		assert_eq!(composer.text(), "hello world", "the #query token is removed");
		assert_eq!(composer.frame().cursor(), Some((3 + 11, 2)), "caret moved to the message end");
		for character in " #copywhole".chars() {
			composer.key(Key::Char(character));
		}
		assert_eq!(
			composer.key(Key::Tab),
			ComposerAction::Copy(Str::new_static("hello world ")),
			"copy prompt reports the draft without the trigger"
		);
		assert_eq!(composer.text(), "hello world ");
	}

	#[test]
	fn at_lists_project_files_and_accepts_with_a_trailing_space() {
		let root = tempfile::tempdir().expect("scratch project");
		std::fs::write(root.path().join("note.txt"), "hi").expect("fixture");
		std::fs::create_dir(root.path().join("src")).expect("fixture dir");
		let mut composer =
			Composer::new(60, UiContext::default(), facts(), Vec::new(), Some(root.path()));
		composer.key(Key::Char('@'));
		let deadline = std::time::Instant::now() + Duration::from_secs(5);
		while !composer.popup_open() && std::time::Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(10));
			// The index lands asynchronously; a caret motion re-queries it.
			composer.key(Key::Left);
			composer.key(Key::Right);
		}
		assert!(composer.popup_open(), "@ lists the indexed project");
		for character in "no".chars() {
			composer.key(Key::Char(character));
		}
		assert_eq!(composer.key(Key::Tab), ComposerAction::Changed);
		assert_eq!(composer.text(), "@note.txt ");
	}

	#[test]
	fn colon_opens_the_builtin_emoji_popup() {
		let mut composer = composer();
		for character in ":joy".chars() {
			composer.key(Key::Char(character));
		}
		assert!(composer.popup_open(), "emoji dropdown");
		assert!(rows(&composer).iter().any(|row| row.contains("joy")));
	}

	#[test]
	fn set_text_and_clear_replace_the_draft_with_the_caret_at_the_end() {
		let mut composer = composer();
		composer.set_text("draft");
		assert_eq!(composer.text(), "draft");
		assert_eq!(composer.frame().cursor(), Some((8, 2)));
		composer.clear();
		assert_eq!(composer.text(), "");
		assert_eq!(composer.frame().cursor(), Some((3, 2)));
	}

	#[test]
	fn empty_enter_is_ignored_and_status_updates_repaint() {
		let mut composer = composer();
		assert!(!matches!(composer.key(Key::Enter), ComposerAction::Submit(_)));
		let working = Some(Duration::ZERO);
		assert!(composer.set_status(StatusFacts { working, ..facts() }));
		assert!(!composer.set_status(StatusFacts { working, ..facts() }));
		assert!(composer.next_wake().is_some(), "spinner schedules a wake");
	}
}
