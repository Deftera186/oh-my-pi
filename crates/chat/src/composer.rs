//! Observer-local composer: a retained editor tree whose draft never enters
//! the session DOM until submission.

use std::{cell::Cell, path::Path, rc::Rc, time::Duration};

use omp_core::Str;
use omp_tui::{
	Command, Frame, Key, Ui, UiContext, UiEvent,
	components::{ComposerStyle, EditorPane},
};

use crate::{
	autocomplete::{PromptAction, PromptActions, composer_chain},
	chrome::{COMPOSER_ID, GAP_ID, STATUS_ID, StatusBand, StatusFacts, composer_ui, top_gap_shown},
};

/// Editor row budget for a terminal of `rows` rows (pi
/// `computeEditorMaxHeight`): roomy terminals get the comfortable `[6, 18]`
/// band below twelve reserved rows; small terminals shrink the cap so the
/// editor leaves at least four rows for the transcript and status, never
/// dropping under the three-row bordered floor.
#[must_use]
pub fn editor_max_rows(rows: u16) -> u16 {
	const MIN: u16 = 6;
	const MAX: u16 = 18;
	const RESERVED: u16 = 12;
	const FALLBACK_ROWS: u16 = 24;
	const MIN_CHROME_ROWS: u16 = 4;
	const MIN_RENDERED_ROWS: u16 = 3;
	let rows = if rows == 0 { FALLBACK_ROWS } else { rows };
	let comfortable = rows.saturating_sub(RESERVED).clamp(MIN, MAX);
	comfortable
		.min(rows.saturating_sub(MIN_CHROME_ROWS))
		.max(MIN_RENDERED_ROWS)
}

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

/// Composer prefix mode: the leading sigil recolors the chrome and Esc
/// clears the draft instead of interrupting (pi rung 8).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrefixMode {
	/// `!` — shell command.
	Bash,
	/// `$` — eval expression.
	Eval,
}

/// Max gap between two spaces for the later one to count as OS auto-repeat
/// (pi `SPACE_REPEAT_MAX_GAP_MS`).
pub const SPACE_REPEAT_MAX_GAP: Duration = Duration::from_millis(120);
/// Absolute jitter floor between two mechanical gaps (pi
/// `SPACE_REPEAT_JITTER_MS`).
pub const SPACE_REPEAT_JITTER: Duration = Duration::from_millis(18);
/// Proportional jitter tolerance for slower repeat rates (pi
/// `SPACE_REPEAT_JITTER_RATIO`).
pub const SPACE_REPEAT_JITTER_RATIO: f64 = 0.35;
/// Consecutive mechanical gaps that confirm a held bar (pi
/// `SPACE_HOLD_MECHANICAL_RUN`).
pub const SPACE_HOLD_MECHANICAL_RUN: u8 = 2;
/// Idle gap after the last repeated space that counts as release (pi
/// `SPACE_HOLD_RELEASE_MS`).
pub const SPACE_HOLD_RELEASE: Duration = Duration::from_millis(250);

/// What the space-hold detector decided about one key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpaceHoldEvent {
	/// Not part of a hold: the key reaches the editor as usual.
	Pass,
	/// A repeat inside a recognized hold (or a pre-burst space already
	/// typed): swallowed.
	Swallow,
	/// The bar is held: delete `track_back` pre-burst spaces and start
	/// recording.
	Begin {
		/// Spaces already inserted before the cadence was recognized.
		track_back: usize,
	},
	/// A non-space key arrived during a hold: stop recording, then let the
	/// key through.
	EndThenPass,
}

/// pi `#handleSpaceHold`: recognizes a held space bar from the metronomic
/// OS auto-repeat cadence, never from taps or smashing.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpaceHold {
	active:         bool,
	last_space:     Option<Duration>,
	prev_gap:       Option<Duration>,
	mechanical_run: u8,
	inserted:       usize,
}

/// Whether two consecutive inter-space gaps look machine-driven.
fn gaps_are_mechanical(gap: Duration, prev: Duration) -> bool {
	if gap > SPACE_REPEAT_MAX_GAP || prev > SPACE_REPEAT_MAX_GAP {
		return false;
	}
	let smaller = gap.min(prev).as_secs_f64() * SPACE_REPEAT_JITTER_RATIO;
	let tolerance = SPACE_REPEAT_JITTER.as_secs_f64().max(smaller);
	(gap.as_secs_f64() - prev.as_secs_f64()).abs() <= tolerance
}

impl SpaceHold {
	/// Whether a recording is in progress.
	#[must_use]
	pub const fn active(&self) -> bool {
		self.active
	}

	/// Observes one key at `now`. `enabled` gates recognition (the setting
	/// and the autocomplete popup); a hold already in progress still ends.
	pub fn observe(&mut self, key: Key, now: Duration, enabled: bool) -> SpaceHoldEvent {
		let is_space = matches!(key, Key::Space | Key::Char(' '));
		if self.active {
			if is_space {
				self.last_space = Some(now);
				return SpaceHoldEvent::Swallow;
			}
			self.end();
			return SpaceHoldEvent::EndThenPass;
		}
		if !is_space {
			self.reset_run();
			return SpaceHoldEvent::Pass;
		}
		if !enabled {
			return SpaceHoldEvent::Pass;
		}
		let gap = self
			.last_space
			.map(|last| now.saturating_sub(last));
		let prev = self.prev_gap;
		self.last_space = Some(now);
		self.prev_gap = gap;
		let mechanical = match (gap, prev) {
			(Some(gap), Some(prev)) => gaps_are_mechanical(gap, prev),
			_ => false,
		};
		if !mechanical {
			// First space, a deliberate tap, or jittery smashing: a real space.
			self.mechanical_run = 0;
			self.inserted += 1;
			return SpaceHoldEvent::Pass;
		}
		self.mechanical_run += 1;
		if self.mechanical_run >= SPACE_HOLD_MECHANICAL_RUN {
			let track_back = self.inserted;
			self.reset_run();
			self.active = true;
			self.last_space = Some(now);
			return SpaceHoldEvent::Begin { track_back };
		}
		SpaceHoldEvent::Swallow
	}

	/// Whether the release idle gap elapsed at `now`; ends the hold when so.
	pub fn release_due(&mut self, now: Duration) -> bool {
		let due = self.active
			&& self
				.last_space
				.is_some_and(|last| now.saturating_sub(last) >= SPACE_HOLD_RELEASE);
		if due {
			self.end();
		}
		due
	}

	/// Host-clock deadline for the release check.
	#[must_use]
	pub fn next_wake(&self) -> Option<Duration> {
		self
			.active
			.then_some(self.last_space? + SPACE_HOLD_RELEASE)
	}

	/// Ends a hold unconditionally (toggle, interrupt).
	pub fn end(&mut self) {
		self.active = false;
		self.reset_run();
	}

	fn reset_run(&mut self) {
		self.inserted = 0;
		self.mechanical_run = 0;
		self.prev_gap = None;
		self.last_space = None;
	}
}

/// Retained composer chrome: status band plus the borderless editor.
///
/// The hardware caret is the editor's insertion point; the host places the
/// terminal cursor from [`Composer::frame`].
pub struct Composer {
	ui:       Ui,
	width:    u16,
	/// Active chrome shape: the band at rest, the rail while the plan
	/// Director is engaged.
	shape:    ComposerStyle,
	/// Whether the host paints a status/notice row directly above the
	/// composer (drives pi's `EditorTopGap`).
	occupied: bool,
	/// IME-safe caret-row layout (`cl_ime_safe_cursor`).
	ime_safe: bool,
	/// Prompt action accepted from the `#` menu, applied after the key.
	pending:  Rc<Cell<Option<PromptAction>>>,
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
		let shape = ComposerStyle::Borderless;
		let mut ui = composer_ui(facts, shape, width, ctx);
		ui.with_component_mut::<EditorPane, _>(COMPOSER_ID, |pane| {
			pane.set_completion(Box::new(chain));
		});
		ui.focus_first();
		Self { ui, width, shape, occupied: false, ime_safe: false, pending }
	}

	/// Tells the composer whether a status/notice row is painted directly
	/// above it (pi `statusRowOccupied`); the band then sits flush under it.
	/// Returns whether the top gap changed.
	pub fn set_status_row_occupied(&mut self, occupied: bool) -> bool {
		if self.occupied == occupied {
			return false;
		}
		self.occupied = occupied;
		self.sync_gap()
	}

	/// Toggles pi's IME-safe cursor layout (`cl_ime_safe_cursor`); returns
	/// whether it changed.
	pub fn set_ime_safe_cursor(&mut self, enabled: bool) -> bool {
		if self.ime_safe == enabled {
			return false;
		}
		self.ime_safe = enabled;
		self
			.ui
			.update_component::<EditorPane>(COMPOSER_ID, |pane| {
				pane.set_ime_safe_cursor(enabled);
				true
			});
		true
	}

	/// Active chrome shape.
	#[must_use]
	pub const fn shape(&self) -> ComposerStyle {
		self.shape
	}

	/// Switches the chrome shape, re-evaluating the top gap (pi
	/// `syncComposerShape` + `EditorTopGap`); returns whether it changed.
	pub fn set_shape(&mut self, shape: ComposerStyle) -> bool {
		if self.shape == shape {
			return false;
		}
		self.shape = shape;
		self
			.ui
			.update_component::<EditorPane>(COMPOSER_ID, |pane| {
				pane.set_composer_style(shape);
				true
			});
		self.sync_gap();
		true
	}

	/// The plan Director engaged (or exited): the composer wears the rail
	/// shape while planning and the band otherwise.
	pub fn set_plan_mode(&mut self, engaged: bool) -> bool {
		self.set_shape(if engaged {
			ComposerStyle::Rail
		} else {
			ComposerStyle::Borderless
		})
	}

	fn sync_gap(&mut self) -> bool {
		let shown = top_gap_shown(self.shape, self.occupied);
		let before = self.ui.height();
		self.ui.set_visible(GAP_ID, shown);
		self.ui.height() != before
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

	/// Current unsent draft in its submitted form: every collapsed paste or
	/// attachment chip is expanded to its full text (pi expands `[Paste #N]`
	/// markers before handing the draft to `$EDITOR` or the model).
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

	/// Draft as displayed: chips stay collapsed to their `<icon> #N` markers.
	#[must_use]
	pub fn text_displayed(&self) -> String {
		self
			.ui
			.with_component::<EditorPane, _>(COMPOSER_ID, |pane| pane.displayed_text().to_owned())
			.unwrap_or_default()
	}

	/// Replaces the draft with text edited outside the composer (pi
	/// `handleExternalEditor`): the chips were expanded into `text`, so the
	/// staged attachment cards are dropped rather than re-collapsed, and the
	/// edited text lands verbatim with the caret at its end.
	pub fn replace_edited(&mut self, text: &str) {
		self
			.ui
			.with_component_mut::<EditorPane, _>(COMPOSER_ID, |pane| pane.attachments().take());
		self.set_text(text);
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

	/// Inserts pasted text verbatim (pi `app.clipboard.pasteTextRaw`): no
	/// chip collapse, no drop classification, newlines kept.
	pub fn paste_raw(&mut self, text: &str) {
		let _ = self.ui.handle_paste_raw(text);
	}

	/// Whether the draft is in a `!` (bash) or `$` (eval) prefix mode (pi
	/// `isBashMode` / `isPythonMode`).
	#[must_use]
	pub fn prefix_mode(&self) -> Option<PrefixMode> {
		match self.text().trim_start().as_bytes().first() {
			Some(b'!') => Some(PrefixMode::Bash),
			Some(b'$') => Some(PrefixMode::Eval),
			_ => None,
		}
	}

	/// Current composer line, for `cl_copy_line`.
	#[must_use]
	pub fn current_line(&self) -> Str {
		self
			.ui
			.with_component::<EditorPane, _>(COMPOSER_ID, |pane| Str::new(pane.current_line()))
			.unwrap_or_default()
	}

	/// Deletes `count` graphemes before the caret (space-hold track-back).
	pub fn delete_before_caret(&mut self, count: usize) {
		for _ in 0..count {
			let _ = self.ui.handle_key(Key::Backspace);
		}
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
				self
					.ui
					.with_component_mut::<EditorPane, _>(COMPOSER_ID, |pane| {
						pane.undo_past_transient(&transient);
					});
				ComposerAction::Changed
			},
			PromptAction::MessageEnd | PromptAction::MessageStart => {
				let end = action == PromptAction::MessageEnd;
				self
					.ui
					.with_component_mut::<EditorPane, _>(COMPOSER_ID, |pane| {
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

	/// Reflows the chrome for a new terminal size: the editor grows with its
	/// content up to [`editor_max_rows`] of `height`.
	pub fn resize(&mut self, width: u16, height: u16) {
		self.width = width;
		let rows = editor_max_rows(height);
		self
			.ui
			.update_component::<EditorPane>(COMPOSER_ID, |pane| {
				pane.set_max_rows(rows);
				true
			});
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
			model: Str::new_static("Sonnet 4.5"),
			thinking: None,
			cwd: Str::new_static("~/proj"),
			scratch: false,
			branch: None,
			tokens: 0,
			context_window: Some(200_000),
			compact_percent: 80,
			working: None,
			..StatusFacts::default()
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
		let prompt = rows
			.iter()
			.position(|row| row.starts_with("╰─ /"))
			.expect("prompt row");
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

	/// Plan mode swaps the band for the rail and back; pi `EditorTopGap`
	/// keeps the one-row gap for the rail and collapses it for the band
	/// only while a status row is painted directly above.
	#[test]
	fn plan_mode_switches_the_shape_and_the_top_gap() {
		let mut composer = composer();
		for character in "hi".chars() {
			composer.key(Key::Char(character));
		}
		assert_eq!(composer.shape(), ComposerStyle::Borderless);
		assert_eq!(rows(&composer)[0], "", "band at rest keeps the gap row");
		assert_eq!(rows(&composer)[2], "╰─ hi");
		assert!(composer.set_status_row_occupied(true), "band under a notice: gap collapses");
		assert_eq!(rows(&composer)[1], "╰─ hi");
		assert!(!composer.set_status_row_occupied(true));

		assert!(composer.set_plan_mode(true));
		assert!(!composer.set_plan_mode(true), "already engaged");
		assert_eq!(composer.shape(), ComposerStyle::Rail);
		let railed = rows(&composer);
		assert_eq!(railed[0], "", "rail keeps the top gap even under a notice");
		assert!(railed.iter().any(|row| row.starts_with('▎') && row.contains("hi")), "{railed:?}");
		assert_eq!(composer.text(), "hi", "the draft survives the reshape");

		assert!(composer.set_plan_mode(false));
		assert_eq!(rows(&composer)[1], "╰─ hi", "band again, notice still up: flush");
		assert!(composer.set_status_row_occupied(false));
		assert_eq!(rows(&composer)[2], "╰─ hi", "notice gone: the gap returns");
	}

	/// pi `computeEditorMaxHeight`, then the composer grows with content up
	/// to that budget.
	#[test]
	fn editor_height_budget_follows_pi_and_caps_growth() {
		assert_eq!(editor_max_rows(40), 18);
		assert_eq!(editor_max_rows(24), 12);
		assert_eq!(editor_max_rows(10), 6);
		assert_eq!(editor_max_rows(6), 3);
		assert_eq!(editor_max_rows(0), 12, "unknown size falls back to 24 rows");

		let mut composer = composer();
		let base = composer.height();
		composer.paste_raw("a\nb\nc\nd");
		assert_eq!(composer.height(), base + 3, "four lines grow the editor by three rows");
		composer.resize(60, 10);
		// Budget 6 rows: the status band, the four content rows, and picker
		// room all fit; a 20-line draft is clamped instead.
		composer.paste_raw(&"\nx".repeat(20));
		assert!(composer.height() <= base + 6, "height {} exceeds the small-terminal budget", composer.height());
		composer.resize(60, 40);
		assert!(composer.height() > base + 6, "a roomy terminal lets the draft grow again");
	}

	/// pi `handleExternalEditor`: the draft handed to `$EDITOR` expands every
	/// chip; the edited text comes back verbatim with the cards dropped.
	#[test]
	fn external_editor_round_trip_expands_chips_and_lands_verbatim() {
		let mut composer = composer();
		let paste = (0..12)
			.map(|n| format!("line{n}"))
			.collect::<Vec<_>>()
			.join("\n");
		composer.paste(&paste);
		for character in "tail".chars() {
			composer.key(Key::Char(character));
		}
		let displayed = composer.text_displayed();
		assert!(displayed.contains("#1 tail"), "{displayed}");
		assert!(!displayed.contains("line0"), "the chip stays collapsed on screen");
		let expanded = composer.text();
		assert_eq!(expanded, format!("{paste} tail"), "the editor draft carries the paste");
		let tall = composer.height();

		let edited = format!("{expanded}\nedited");
		composer.replace_edited(&edited);
		assert_eq!(composer.text(), edited, "verbatim replacement, nothing re-collapsed");
		assert_eq!(composer.text_displayed(), edited);
		assert!(composer.height() < tall, "the attachment card band is gone");
	}
}
#[cfg(test)]
mod dbg_tests {
	use super::*;
	#[test]
	fn dbg_gap() {
		let mut composer = Composer::new(60, UiContext::default(), StatusFacts::default(), vec![], None);
		for c in "hi".chars() { composer.key(Key::Char(c)); }
		eprintln!("A {:?} h={}", omp_tui::frame_text(composer.frame()).lines().collect::<Vec<_>>(), composer.height());
		composer.set_status_row_occupied(true);
		eprintln!("B {:?} h={}", omp_tui::frame_text(composer.frame()).lines().collect::<Vec<_>>(), composer.height());
		composer.set_status_row_occupied(false);
		eprintln!("C {:?} h={}", omp_tui::frame_text(composer.frame()).lines().collect::<Vec<_>>(), composer.height());
		composer.set_plan_mode(true);
		eprintln!("D {:?} h={}", omp_tui::frame_text(composer.frame()).lines().collect::<Vec<_>>(), composer.height());
	}
}
