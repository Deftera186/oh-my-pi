//! Retained presentation for the fullscreen Git workbench.

mod commit_view;
mod diff;
mod sidebar;

use std::{
	collections::BTreeSet,
	time::{Duration, Instant},
};

use diff::{DIFF_ID, VIEW_ID, strip_carriage_returns};
use omp_core::{IntoStr, Str, sf};
use omp_tui::{
	DiffActionKind, DiffBuildOptions, DiffDocument, DiffPane, DiffPaneState, DiffPatchTarget,
	DiffTarget, DiffWhitespaceMode, Dim, Key, Layer, Mouse, OverlayOptions, Prop, Size, Ui,
	UiContext, UiEvent, ViewMode,
	components::{Col, EditorPane, Tree},
};
use sidebar::{
	AI_STAGE_BUTTON_ID, AI_STAGE_ID, AMEND_ID, COMMIT_ID, DESCRIPTION_ID, DESCRIPTION_PANE_ID,
	SIDEBAR_ID, SUMMARY_ID, SidebarGroup, SidebarRow, SidebarTarget, VIEW_STYLE_ID, sidebar_rows,
};
use strum::EnumProperty as _;

/// Kind of change reported for one Git path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
pub enum GitChangeKind {
	/// Existing file contents changed.
	#[strum(to_string = "M")]
	Modified,
	/// New tracked file.
	#[strum(to_string = "A")]
	Added,
	/// Removed tracked file.
	#[strum(to_string = "D")]
	Deleted,
	/// Path renamed from [`GitFileRow::orig_path`].
	#[strum(to_string = "R")]
	Renamed,
	/// New untracked file.
	#[strum(to_string = "?")]
	Untracked,
	/// File with unresolved conflicts.
	#[strum(to_string = "U")]
	Conflicted,
}

/// Repository area containing a Git file row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitArea {
	/// Working-tree changes not present in the index.
	Unstaged,
	/// Changes present in the index.
	Staged,
	/// Changes belonging to the pinned commit.
	Commit,
}

/// One changed file shown by the Git workbench.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitFileRow {
	/// Current repository-relative path.
	pub path:      Str,
	/// Previous path for a rename.
	pub orig_path: Option<Str>,
	/// Kind of file change.
	pub kind:      GitChangeKind,
	/// Repository area containing the change.
	pub area:      GitArea,
	/// Added line count, when available.
	pub additions: Option<u64>,
	/// Deleted line count, when available.
	pub deletions: Option<u64>,
}

/// Metadata and file changes for one pinned commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommitInfo {
	/// Full commit object id.
	pub sha:          Str,
	/// First line of the commit message.
	pub subject:      Str,
	/// Remaining commit message body.
	pub body:         Str,
	/// Commit author's display name.
	pub author_name:  Str,
	/// Commit author's email address.
	pub author_email: Str,
	/// Commit author's strict ISO-8601 date.
	pub author_date:  Str,
	/// Full parent commit object ids.
	pub parents:      Vec<Str>,
	/// Files changed by this commit.
	pub files:        Vec<GitFileRow>,
}

/// Complete backend-owned repository snapshot for the workbench.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSnapshot {
	/// Current branch name, or `None` for detached/unborn HEAD.
	pub branch:   Option<Str>,
	/// Working-tree changes not present in the index.
	pub unstaged: Vec<GitFileRow>,
	/// Changes present in the index.
	pub staged:   Vec<GitFileRow>,
	/// Current or pinned commit metadata, when available.
	pub head:     Option<GitCommitInfo>,
	/// Whether the workbench is pinned to a revision.
	pub pinned:   bool,
}

/// Old and new file content loaded for the diff pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitFileContents {
	/// File text on the old side.
	pub old_text:        Str,
	/// File text on the new side.
	pub new_text:        Str,
	/// Whether Git reported binary contents.
	pub binary:          bool,
	/// Whether the file exceeded the presentation size limit.
	pub too_large:       bool,
	/// Raw old-side bytes when media preview applies.
	pub old_bytes:       Option<bytes::Bytes>,
	/// Raw new-side bytes when media preview applies.
	pub new_bytes:       Option<bytes::Bytes>,
	/// Lowercase media format token when the file is a previewable image.
	pub media:           Option<Str>,
	/// Old-side reason an expected media object cannot be previewed.
	pub old_placeholder: Option<Str>,
	/// New-side reason an expected media object cannot be previewed.
	pub new_placeholder: Option<Str>,
}

/// Patch mutation requested from an interactive diff selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitPatchOp {
	/// Add selected changes to the index.
	Stage,
	/// Remove selected changes from the index.
	Unstage,
	/// Discard selected working-tree changes.
	Discard,
}

/// Outbound workbench request for the Git backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitIntent {
	/// Refresh the repository snapshot.
	Refresh,
	/// Load both sides of one file into the diff pane.
	Load {
		/// Repository area containing the file.
		area:      GitArea,
		/// Current repository-relative path.
		path:      Str,
		/// Previous path for a rename.
		orig_path: Option<Str>,
		/// Monotonic request sequence used to reject stale contents.
		seq:       u64,
	},
	/// Stage the exact paths, or every unstaged path when absent.
	StageFiles(Option<Vec<Str>>),
	/// Selectively stage changes matching one natural-language instruction.
	AiStage {
		/// User description of the changes to stage.
		instruction: Str,
	},
	/// Unstage the exact paths, or every staged path when absent.
	UnstageFiles(Option<Vec<Str>>),
	/// Apply an operation to inclusive one-based line ranges.
	ApplyLines {
		/// Requested patch operation.
		op:   GitPatchOp,
		/// Current repository-relative path.
		path: Str,
		/// Inclusive old-side range, or `(0, 0)` when absent.
		old:  (u32, u32),
		/// Inclusive new-side range, or `(0, 0)` when absent.
		new:  (u32, u32),
	},
	/// Create a commit from the composer.
	Commit {
		/// Subject and optional body entered by the user.
		message:   Str,
		/// Whether to amend HEAD.
		amend:     bool,
		/// Whether to stage all working-tree changes first.
		stage_all: bool,
	},
	/// Generate a conventional commit message from the staged diff.
	GenerateCommit {
		/// Whether the generated message replaces the current HEAD message.
		amend: bool,
	},
	/// Resolve an avatar image for an author email.
	Avatar {
		/// Lower- or mixed-case author email.
		email: Str,
	},
	/// Close the workbench and release backend refresh state.
	Close,
}

/// Inbound workbench mutation emitted by the Git backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitUpdate {
	/// Replace repository and commit state.
	Snapshot(GitSnapshot),
	/// Progressive line delivery for one GitIntent::Load while sides stream in.
	ContentsChunk {
		/// Sequence from the corresponding [`GitIntent::Load`].
		seq:       u64,
		/// Complete old-side lines appended since the previous chunk.
		old_lines: Vec<Str>,
		/// Complete new-side lines appended since the previous chunk.
		new_lines: Vec<Str>,
	},
	/// Supply loaded file contents for one request sequence.
	Contents {
		/// Sequence from the corresponding [`GitIntent::Load`].
		seq:      u64,
		/// Loaded old and new file contents.
		contents: GitFileContents,
	},
	/// Report a successful mutation.
	ActionDone {
		/// Human-readable success message.
		message: Str,
	},
	/// Populate the commit composer with an inferred conventional commit.
	CommitGenerated {
		/// Generated conventional-commit subject.
		summary: Str,
		/// Generated body text.
		body:    Str,
	},
	/// Report a failed mutation.
	ActionFailed {
		/// Human-readable failure message.
		message: Str,
	},
	/// Supply an optional author avatar PNG.
	Avatar {
		/// Author email associated with the result.
		email: Str,
		/// Normalized PNG bytes, or `None` when unavailable.
		png:   Option<bytes::Bytes>,
	},
}

const STATUS_TTL: Duration = Duration::from_secs(6);
const SIDEBAR_MIN: u16 = 30;
const SIDEBAR_MAX: u16 = 48;
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::EnumProperty)]
pub(super) enum Focus {
	#[strum(props(Hint = "j/k move · h/l scroll · g/G ends · alt+↓/↑ hunk · ]/[ file · 1–4 view \
	                      · w wrap · b whitespace · s/u stage · x discard · c commit · r \
	                      refresh · q quit"))]
	Diff,
	#[default]
	#[strum(props(Hint = "j/k move · h/l fold/open · g/G ends · space/s/u stage · enter open · \
	                      ]/[ file · alt+↓/↑ hunk · c commit · r refresh · t tree · q quit"))]
	Sidebar,
}

impl Focus {
	fn hint(self) -> &'static str {
		self.get_str("Hint").expect("every focus has a hint")
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingDiscard {
	path:   Str,
	target: DiffTarget,
}

/// Result of routing one interaction through a [`GitWorkbench`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitWorkbenchEvent {
	/// Input was consumed without a backend request.
	Consumed,
	/// Forward one request to the Git backend.
	Intent(GitIntent),
	/// Close the workbench and stop backend refresh.
	Close,
}

/// Retained fullscreen Git workbench presentation.
pub struct GitWorkbench {
	pub(super) ui: Ui,
	pub(super) ctx: UiContext,
	options: OverlayOptions,
	pub(super) snapshot: GitSnapshot,
	pub(super) selected: Option<(GitArea, Str)>,
	pub(in crate::git) sidebar_rows: Vec<SidebarRow>,
	pub(super) sidebar_selected: usize,
	pub(super) focus: Focus,
	pub(super) tree: bool,
	pub(super) collapsed: BTreeSet<Str>,
	pub(super) contents: Option<GitFileContents>,
	load_seq: u64,
	streaming: bool,
	load_finished: bool,
	pub(super) whitespace: DiffWhitespaceMode,
	pub(super) view_mode: ViewMode,
	pub(super) wrap: bool,
	pub(super) amend: bool,
	pub(super) status: Option<(Str, omp_tui::Color, Instant, bool)>,
	pending_discard: Option<PendingDiscard>,
	commit_pending: bool,
	generation_pending: bool,
	sidebar_follow_selection: bool,
	pub(super) avatar: Option<(Str, bytes::Bytes)>,
	avatar_requested: Option<Str>,
	pending_last_hunk: bool,
	width: u16,
	height: u16,
}

impl GitWorkbench {
	/// Opens a workbench over a backend-owned repository snapshot.
	pub fn open(snapshot: GitSnapshot, ctx: &UiContext) -> Self {
		let selected = first_file(&snapshot).map(|file| (file.area, file.path.clone()));
		let mut workbench = Self {
			ui: Ui::from_root(Col::new(), 1, ctx.clone()),
			ctx: ctx.clone(),
			options: OverlayOptions::default().width(Dim::Pct(100)).z(40),
			snapshot,
			selected,
			sidebar_rows: Vec::new(),
			sidebar_selected: 0,
			focus: Focus::Sidebar,
			tree: true,
			collapsed: BTreeSet::new(),
			contents: None,
			load_seq: 0,
			streaming: false,
			load_finished: false,
			whitespace: DiffWhitespaceMode::Off,
			view_mode: ViewMode::Split,
			wrap: false,
			amend: false,
			status: None,
			pending_discard: None,
			commit_pending: false,
			generation_pending: false,
			sidebar_follow_selection: true,
			avatar: None,
			avatar_requested: None,
			pending_last_hunk: false,
			width: 100,
			height: 30,
		};
		workbench.rebuild();
		workbench
	}

	/// Returns the load request for the initially selected file, when present.
	pub fn initial_intent(&mut self) -> Option<GitIntent> {
		self
			.request_selected_load()
			.or_else(|| self.request_avatar())
	}

	/// Applies one backend update and returns a load needed by changed
	/// selection.
	pub fn apply(&mut self, update: GitUpdate) -> Option<GitIntent> {
		match update {
			GitUpdate::Snapshot(snapshot) => self.apply_snapshot(snapshot),
			GitUpdate::ContentsChunk { seq, mut old_lines, mut new_lines } => {
				if seq != self.load_seq || self.load_finished {
					return None;
				}
				if !self.streaming {
					self.streaming = true;
					self.begin_stream();
				}
				sanitize_stream_lines(&mut old_lines);
				sanitize_stream_lines(&mut new_lines);
				self.with_pane(|pane| pane.push_stream(&old_lines, &new_lines));
				None
			},
			GitUpdate::Contents { seq, contents } => {
				if seq != self.load_seq {
					return None;
				}
				self.contents = Some(contents);
				self.finish_document();
				self.streaming = false;
				self.load_finished = true;
				self.sync_patch_target();
				self.request_avatar()
			},
			GitUpdate::ActionDone { message } => {
				let clear_form = self.commit_pending;
				if clear_form {
					self.amend = false;
					self.commit_pending = false;
					self.sidebar_selected = self.first_file_target().unwrap_or(0);
				}
				self.status = Some((message, self.ctx.theme.ok, Instant::now(), false));
				self.pending_discard = None;
				if clear_form {
					self.rebuild_with_form("", "", "");
				} else {
					self.rebuild();
				}
				None
			},
			GitUpdate::CommitGenerated { summary, body } => {
				self.generation_pending = false;
				let (_, _, ai_instruction) = self.form_values();
				self.status = Some((
					Str::new_static("Generated commit message"),
					self.ctx.theme.ok,
					Instant::now(),
					false,
				));
				self.rebuild_with_form(summary.as_str(), body.as_str(), ai_instruction.as_str());
				None
			},
			GitUpdate::ActionFailed { message } => {
				self.commit_pending = false;
				self.generation_pending = false;
				self.status =
					Some((single_line(message.as_str()), self.ctx.theme.err, Instant::now(), true));
				self.pending_discard = None;
				self.rebuild();
				None
			},
			GitUpdate::Avatar { email, png } => {
				self.avatar_requested = Some(email.clone());
				if let Some(png) = png {
					self.avatar = Some((email, png));
				}
				self.rebuild();
				None
			},
		}
	}

	/// Routes one keyboard event.
	pub fn handle_key(&mut self, key: Key) -> GitWorkbenchEvent {
		if key != Key::Char('x') {
			self.pending_discard = None;
		}
		if matches!(key, Key::Tab | Key::BackTab) {
			self.focus = if self.focus == Focus::Diff {
				Focus::Sidebar
			} else {
				Focus::Diff
			};
			self.focus_current();
			return GitWorkbenchEvent::Consumed;
		}
		if key == Key::Esc {
			if self.focus == Focus::Sidebar && self.editing() {
				if matches!(self.current_sidebar_target(), Some(SidebarTarget::AiStage)) {
					self.select_target_kind(SidebarTarget::Section { area: GitArea::Unstaged });
				} else {
					self.select_target_kind(SidebarTarget::Commit);
				}
				return GitWorkbenchEvent::Consumed;
			}
			if self.focus == Focus::Diff && self.clear_diff_selection() {
				return GitWorkbenchEvent::Consumed;
			}
			return GitWorkbenchEvent::Close;
		}
		if !self.editing() {
			match key {
				Key::Char('q') => return GitWorkbenchEvent::Close,
				Key::JumpPrevious => return self.jump_hunk_or_file(-1),
				Key::JumpNext => return self.jump_hunk_or_file(1),
				Key::Char('[') => return self.select_adjacent_file(-1, false),
				Key::Char(']') => return self.select_adjacent_file(1, false),
				Key::Char('v') => {
					self.with_pane(|pane| pane.cycle_mode());
					self.view_mode = match self.view_mode {
						ViewMode::File => ViewMode::Split,
						ViewMode::Split => ViewMode::Inline,
						ViewMode::Inline => ViewMode::Hunk,
						ViewMode::Hunk => ViewMode::File,
					};
					self.sync_view_value();
					return GitWorkbenchEvent::Consumed;
				},
				Key::Char('1') => return self.set_mode(ViewMode::File),
				Key::Char('2') => return self.set_mode(ViewMode::Split),
				Key::Char('3') => return self.set_mode(ViewMode::Inline),
				Key::Char('4') => return self.set_mode(ViewMode::Hunk),
				Key::Char('w') => {
					self.with_pane(|pane| pane.toggle_wrap());
					self.wrap = !self.wrap;
					self.sync_toggle_props();
					return GitWorkbenchEvent::Consumed;
				},
				Key::Char('b') => return self.cycle_whitespace(),
				Key::Char('r') => return GitWorkbenchEvent::Intent(GitIntent::Refresh),
				Key::Char('c') if !self.is_commit_view() => {
					self.focus = Focus::Sidebar;
					self.select_target_kind(SidebarTarget::Summary);
					return GitWorkbenchEvent::Consumed;
				},
				_ => {},
			}
		}
		match self.focus {
			Focus::Diff => self.handle_diff_key(key),
			Focus::Sidebar => self.handle_sidebar_key(key),
		}
	}

	/// Routes pasted text into the active commit text field.
	pub fn handle_paste(&mut self, text: &str) -> GitWorkbenchEvent {
		if !self.editing() {
			return GitWorkbenchEvent::Consumed;
		}
		let _ = self.ui.handle_paste(text);
		self.sync_commit_button();
		GitWorkbenchEvent::Consumed
	}

	/// Routes a viewport-space mouse gesture through the fullscreen retained UI.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> GitWorkbenchEvent {
		let sidebar_width = (viewport.width * 3 / 10).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
		let in_content = row >= 2;
		let in_sidebar = in_content && col >= viewport.width.saturating_sub(sidebar_width);
		if in_content && matches!(kind, Mouse::Click | Mouse::RightClick) {
			self.focus = if in_sidebar {
				Focus::Sidebar
			} else {
				Focus::Diff
			};
			if self.focus == Focus::Sidebar && kind == Mouse::Click {
				self.select_sidebar_form_at(row.saturating_sub(2), viewport.height.saturating_sub(2));
			}
			let color = if self.focus == Focus::Sidebar {
				self.ctx.theme.accent
			} else {
				self.ctx.theme.border
			};
			let _ = self.ui.set_prop("git-separator", Prop::Fg, color);
		}
		let routed = self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
			.unwrap_or(UiEvent::None);
		self.sync_control_values();
		if !matches!(routed, UiEvent::DiffAction { action: DiffActionKind::Discard, .. }) {
			self.pending_discard = None;
		}
		self.route_ui(routed)
	}

	/// Returns the full-viewport active layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		if self
			.status
			.as_ref()
			.is_some_and(|(_, _, at, sticky)| !sticky && at.elapsed() >= STATUS_TTL)
		{
			self.status = None;
			let hint = self.focus.hint();
			let _ = self.ui.set_text("git-status", hint);
			let _ = self
				.ui
				.set_prop("git-status", Prop::Fg, self.ctx.theme.muted);
		}
		if viewport.width != self.width || viewport.height != self.height {
			self.width = viewport.width;
			self.height = viewport.height;
			self.rebuild();
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn apply_snapshot(&mut self, snapshot: GitSnapshot) -> Option<GitIntent> {
		self.pending_discard = None;
		let previous_rows = self.sidebar_rows.clone();
		let previous_target = self.current_sidebar_target().cloned();
		let previous_selected = self.selected.clone();
		self.snapshot = snapshot;
		self.sidebar_rows = sidebar_rows(&self.snapshot, self.tree, &self.ctx);
		if let Some(target) = previous_target {
			let key = target.key();
			if let Some(index) = self
				.sidebar_rows
				.iter()
				.position(|row| row.target.key() == key)
			{
				self.sidebar_selected = index;
			} else if let Some(survivor) = nearest_survivor(&previous_rows, &self.sidebar_rows, &key) {
				self.set_sidebar_index_for_key(survivor.as_str());
			}
		}
		self.selected = previous_selected
			.clone()
			.filter(|(area, path)| find_file(&self.snapshot, *area, path.as_str()).is_some());
		if self.selected.is_none() {
			self.selected = self
				.current_sidebar_target()
				.and_then(|target| match target {
					SidebarTarget::File { area, path, .. } => Some((*area, path.clone())),
					_ => None,
				})
				.or_else(|| first_file(&self.snapshot).map(|file| (file.area, file.path.clone())));
		}
		let changed = self.selected != previous_selected;
		if changed {
			self.contents = None;
			self.streaming = false;
			self.load_finished = true;
			self.install_document();
		}
		self.rebuild();
		if changed {
			self
				.request_selected_load()
				.or_else(|| self.request_avatar())
		} else {
			self.request_avatar()
		}
	}

	fn handle_diff_key(&mut self, key: Key) -> GitWorkbenchEvent {
		match key {
			Key::Char('s') => self.request_diff_action(DiffActionKind::Stage),
			Key::Char('u') => self.request_diff_action(DiffActionKind::Unstage),
			Key::Char('x') => self.request_discard(),
			Key::Enter => self.jump_hunk_or_file(1),
			Key::Char('n') => self.jump_hunk_or_file(1),
			Key::Char('p') => self.jump_hunk_or_file(-1),
			Key::Char('j') => self.route_diff_navigation(Key::Down),
			Key::Char('k') => self.route_diff_navigation(Key::Up),
			Key::Char('h') => self.route_diff_navigation(Key::Left),
			Key::Char('l') => self.route_diff_navigation(Key::Right),
			Key::Char('g') => self.route_diff_navigation(Key::Home),
			Key::Char('G') => self.route_diff_navigation(Key::End),
			Key::Space => self.route_diff_navigation(Key::PageDown),
			_ => self.route_diff_navigation(key),
		}
	}

	fn route_diff_navigation(&mut self, key: Key) -> GitWorkbenchEvent {
		self.focus_current();
		let event = self.ui.handle_key(key);
		self.sync_control_values();
		self.route_ui(event)
	}

	fn handle_sidebar_key(&mut self, key: Key) -> GitWorkbenchEvent {
		if self.editing() {
			return self.handle_editor_key(key);
		}
		if matches!(key, Key::Space | Key::Char('s') | Key::Char('u'))
			&& self
				.current_sidebar_target()
				.is_some_and(SidebarTarget::is_tree_node)
			&& let Some(selected) = self.tree_selected_key()
		{
			self.set_sidebar_index_for_key(selected.as_str());
		}
		let target = self.current_sidebar_target().cloned();
		match (target, key) {
			(_, Key::Char('t')) => {
				self.tree = !self.tree;
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			(Some(SidebarTarget::Amend), Key::Up | Key::Char('k')) => {
				self.select_target_kind(SidebarTarget::AiStage);
				GitWorkbenchEvent::Consumed
			},
			(Some(SidebarTarget::Amend), Key::Down | Key::Char('j')) => {
				self.select_target_kind(SidebarTarget::Summary);
				GitWorkbenchEvent::Consumed
			},
			(Some(SidebarTarget::Amend), Key::Enter | Key::Space) => self.toggle_amend(),
			(Some(SidebarTarget::Commit), Key::Up | Key::Char('k')) => {
				self.select_target_kind(SidebarTarget::Description);
				GitWorkbenchEvent::Consumed
			},
			(Some(SidebarTarget::Commit), Key::Enter | Key::Space) => self.submit_commit(),
			(Some(SidebarTarget::ViewStyle), _) => self.route_sidebar_tree_key(key),
			(Some(target), Key::Space)
				if matches!(target, SidebarTarget::File { .. } | SidebarTarget::Directory { .. }) =>
			{
				self.activate_sidebar(true)
			},
			(Some(target), Key::Char('s')) if target.is_tree_node() => {
				self.explicit_sidebar_stage(true)
			},
			(Some(target), Key::Char('u')) if target.is_tree_node() => {
				self.explicit_sidebar_stage(false)
			},
			(Some(target), _) if target.is_tree_node() => self.route_sidebar_tree_key(key),
			_ => {
				let event = self.ui.handle_key(key);
				self.sync_control_values();
				self.route_ui(event)
			},
		}
	}

	fn handle_editor_key(&mut self, key: Key) -> GitWorkbenchEvent {
		match (self.current_sidebar_target().cloned(), key) {
			(Some(SidebarTarget::AiStage), Key::Enter) => return self.submit_ai_stage(),
			(Some(SidebarTarget::AiStage), Key::Up) => return self.focus_tree(),
			(Some(SidebarTarget::AiStage), Key::Down) => {
				self.select_target_kind(SidebarTarget::Amend);
				return GitWorkbenchEvent::Consumed;
			},
			(Some(SidebarTarget::Summary), Key::Up) => {
				self.select_target_kind(SidebarTarget::Amend);
				return GitWorkbenchEvent::Consumed;
			},
			(Some(SidebarTarget::Summary), Key::Down | Key::Enter) => {
				self.select_target_kind(SidebarTarget::Description);
				return GitWorkbenchEvent::Consumed;
			},
			(Some(SidebarTarget::Description), Key::Up) if self.editor_on_first_line() => {
				self.select_target_kind(SidebarTarget::Summary);
				return GitWorkbenchEvent::Consumed;
			},
			(Some(SidebarTarget::Description), Key::Down) if self.editor_on_last_line() => {
				self.select_target_kind(SidebarTarget::Commit);
				return GitWorkbenchEvent::Consumed;
			},
			_ => {},
		}
		let event = self.ui.handle_key(key);
		self.sync_commit_button();
		self.route_ui(event)
	}

	fn activate_sidebar(&mut self, stage: bool) -> GitWorkbenchEvent {
		let Some(target) = self.current_sidebar_target().cloned() else {
			return GitWorkbenchEvent::Consumed;
		};
		match target {
			SidebarTarget::Directory { area, path, group, .. } if stage => {
				self.stage_directory(area, path.as_str(), group)
			},
			SidebarTarget::Directory { .. } => GitWorkbenchEvent::Consumed,
			SidebarTarget::File { area, path, .. } if stage => self.stage_paths(area, vec![path]),
			SidebarTarget::File { .. } => {
				self.focus = Focus::Diff;
				self.focus_current();
				GitWorkbenchEvent::Consumed
			},
			SidebarTarget::Section { area: GitArea::Unstaged } => {
				GitWorkbenchEvent::Intent(GitIntent::StageFiles(None))
			},
			SidebarTarget::Section { area: GitArea::Staged } => {
				GitWorkbenchEvent::Intent(GitIntent::UnstageFiles(None))
			},
			SidebarTarget::Section { area: GitArea::Commit } | SidebarTarget::ViewStyle => {
				GitWorkbenchEvent::Consumed
			},
			SidebarTarget::AiStage => self.submit_ai_stage(),
			SidebarTarget::Amend => self.toggle_amend(),
			SidebarTarget::Summary | SidebarTarget::Description => {
				self.focus_current();
				GitWorkbenchEvent::Consumed
			},
			SidebarTarget::Commit => self.submit_commit(),
		}
	}

	fn explicit_sidebar_stage(&mut self, stage: bool) -> GitWorkbenchEvent {
		let Some(target) = self.current_sidebar_target().cloned() else {
			return GitWorkbenchEvent::Consumed;
		};
		match target {
			SidebarTarget::File { area, path, .. }
				if (stage && area == GitArea::Unstaged) || (!stage && area == GitArea::Staged) =>
			{
				self.stage_paths(area, vec![path])
			},
			SidebarTarget::Directory { area, path, group, .. }
				if (stage && area == GitArea::Unstaged) || (!stage && area == GitArea::Staged) =>
			{
				self.stage_directory(area, path.as_str(), group)
			},
			SidebarTarget::Section { area: GitArea::Unstaged } if stage => {
				GitWorkbenchEvent::Intent(GitIntent::StageFiles(None))
			},
			SidebarTarget::Section { area: GitArea::Staged } if !stage => {
				GitWorkbenchEvent::Intent(GitIntent::UnstageFiles(None))
			},
			_ => GitWorkbenchEvent::Consumed,
		}
	}

	fn stage_directory(
		&self,
		area: GitArea,
		directory: &str,
		group: SidebarGroup,
	) -> GitWorkbenchEvent {
		let files = match area {
			GitArea::Unstaged => &self.snapshot.unstaged,
			GitArea::Staged => &self.snapshot.staged,
			GitArea::Commit => return GitWorkbenchEvent::Consumed,
		};
		let paths = files
			.iter()
			.filter(|file| {
				let in_directory = file
					.path
					.strip_prefix(directory)
					.is_some_and(|suffix| suffix.starts_with('/'));
				let in_group = matches!(file.kind, GitChangeKind::Added | GitChangeKind::Untracked)
					== (group == SidebarGroup::Additions);
				in_directory && (area != GitArea::Unstaged || in_group)
			})
			.map(|file| file.path.clone())
			.collect();
		self.stage_paths(area, paths)
	}

	fn stage_paths(&self, area: GitArea, paths: Vec<Str>) -> GitWorkbenchEvent {
		if paths.is_empty() {
			return GitWorkbenchEvent::Consumed;
		}
		match area {
			GitArea::Unstaged => GitWorkbenchEvent::Intent(GitIntent::StageFiles(Some(paths))),
			GitArea::Staged => GitWorkbenchEvent::Intent(GitIntent::UnstageFiles(Some(paths))),
			GitArea::Commit => GitWorkbenchEvent::Consumed,
		}
	}

	fn toggle_amend(&mut self) -> GitWorkbenchEvent {
		self.amend = !self.amend;
		let (summary, description, ai_instruction) = self.form_values();
		if self.amend && summary.is_empty() && description.is_empty() {
			if let Some(head) = &self.snapshot.head {
				let subject = head.subject.clone();
				let body = head.body.clone();
				self.rebuild_with_form(subject.as_str(), body.as_str(), ai_instruction.as_str());
				return GitWorkbenchEvent::Consumed;
			}
		}
		let _ = self.ui.set_prop(AMEND_ID, Prop::Checked, self.amend);
		self.sync_commit_button();
		GitWorkbenchEvent::Consumed
	}

	fn submit_ai_stage(&mut self) -> GitWorkbenchEvent {
		let (summary, description, instruction) = self.form_values();
		let instruction = instruction.as_str().trim();
		if instruction.is_empty() || self.snapshot.unstaged.is_empty() {
			return GitWorkbenchEvent::Consumed;
		}
		let instruction = instruction.to_str();
		self.status = Some((
			sf!("Selecting changes for: {instruction}"),
			self.ctx.theme.accent,
			Instant::now(),
			false,
		));
		self.rebuild_with_form(summary.as_str(), description.as_str(), "");
		GitWorkbenchEvent::Intent(GitIntent::AiStage { instruction })
	}

	fn submit_commit(&mut self) -> GitWorkbenchEvent {
		let (summary, description, ai_instruction) = self.form_values();
		if !self.commit_enabled_with(summary.as_str(), description.as_str()) {
			return GitWorkbenchEvent::Consumed;
		}
		let summary = summary.as_str().trim();
		let body = description.as_str().trim();
		if summary.is_empty() {
			self.generation_pending = true;
			self.status = Some((
				Str::new_static("Generating commit message"),
				self.ctx.theme.accent,
				Instant::now(),
				false,
			));
			self.rebuild_with_form("", "", ai_instruction.as_str());
			return GitWorkbenchEvent::Intent(GitIntent::GenerateCommit { amend: self.amend });
		}
		let message = if body.is_empty() {
			summary.to_str()
		} else {
			sf!("{summary}\n\n{body}")
		};
		let stage_all = self.snapshot.staged.is_empty();
		self.commit_pending = true;
		GitWorkbenchEvent::Intent(GitIntent::Commit { message, amend: self.amend, stage_all })
	}

	fn request_diff_action(&mut self, action: DiffActionKind) -> GitWorkbenchEvent {
		let event = self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| pane.request_action(action))
			.flatten();
		event.map_or(GitWorkbenchEvent::Consumed, |event| self.route_ui(event))
	}

	fn request_discard(&mut self) -> GitWorkbenchEvent {
		let event = self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| {
				pane.request_action(DiffActionKind::Discard)
			})
			.flatten();
		let Some(UiEvent::DiffAction { target, .. }) = event else {
			return GitWorkbenchEvent::Consumed;
		};
		self.confirm_discard(target)
	}

	fn confirm_discard(&mut self, target: DiffTarget) -> GitWorkbenchEvent {
		if self.streaming {
			return GitWorkbenchEvent::Consumed;
		}
		if target == DiffTarget::File {
			return GitWorkbenchEvent::Consumed;
		}
		let Some((GitArea::Unstaged, path)) = self.selected.clone() else {
			return GitWorkbenchEvent::Consumed;
		};
		let identity = PendingDiscard { path: path.clone(), target: target.clone() };
		if self.pending_discard.as_ref() != Some(&identity) {
			let label = if matches!(target, DiffTarget::Lines { .. }) {
				"Discard selected lines? Press x again to confirm"
			} else {
				"Discard hunk? Press x (or click) again to confirm"
			};
			self.pending_discard = Some(identity);
			self.status = Some((Str::new_static(label), self.ctx.theme.warn, Instant::now(), false));
			let _ = self.ui.set_text("git-status", label);
			let _ = self
				.ui
				.set_prop("git-status", Prop::Fg, self.ctx.theme.warn);
			return GitWorkbenchEvent::Consumed;
		}
		self.pending_discard = None;
		self.map_diff_action(DiffActionKind::Discard, target)
	}

	fn route_ui(&mut self, event: UiEvent) -> GitWorkbenchEvent {
		match event {
			UiEvent::TreeActivated { id, key } if id.as_str() == SIDEBAR_ID => {
				let selected = self.select_tree_key(key.as_str());
				if matches!(self.current_sidebar_target(), Some(SidebarTarget::Section { .. })) {
					if !self.collapsed.remove(&key) {
						self.collapsed.insert(key);
					}
					self.rebuild();
					return GitWorkbenchEvent::Consumed;
				}
				if matches!(self.current_sidebar_target(), Some(SidebarTarget::File { .. })) {
					self.focus = Focus::Diff;
					self.focus_current();
				}
				selected
			},
			UiEvent::TreeToggled { id, key, expanded } if id.as_str() == SIDEBAR_ID => {
				self.set_sidebar_index_for_key(key.as_str());
				if let Some(expanded) = expanded {
					if expanded {
						self.collapsed.remove(&key);
					} else {
						self.collapsed.insert(key);
					}
					GitWorkbenchEvent::Consumed
				} else {
					self.activate_sidebar(true)
				}
			},
			UiEvent::TreeAction { id, key, action } if id.as_str() == SIDEBAR_ID => {
				self.set_sidebar_index_for_key(key.as_str());
				match action.as_str() {
					"Stage All" => GitWorkbenchEvent::Intent(GitIntent::StageFiles(None)),
					"Unstage All" => GitWorkbenchEvent::Intent(GitIntent::UnstageFiles(None)),
					_ => GitWorkbenchEvent::Consumed,
				}
			},
			UiEvent::DiffAction { action: DiffActionKind::Discard, target, .. } => {
				self.confirm_discard(target)
			},
			UiEvent::DiffAction { action, target, .. } => self.map_diff_action(action, target),
			UiEvent::Pressed(id) => self.activate_chrome(id.as_str()),
			UiEvent::Changed { id, value } if id.as_str() == VIEW_STYLE_ID => {
				self.tree = value.as_str() == "tree";
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			UiEvent::Changed { id, value } if id.as_str() == VIEW_ID => {
				let Ok(mode) = value.as_str().parse::<ViewMode>() else {
					return GitWorkbenchEvent::Consumed;
				};
				self.set_mode(mode)
			},
			UiEvent::Changed { id, value } if id.as_str() == AMEND_ID => {
				let checked = value.as_str() == "true";
				if checked != self.amend {
					self.toggle_amend()
				} else {
					GitWorkbenchEvent::Consumed
				}
			},
			UiEvent::Cancel => GitWorkbenchEvent::Close,
			_ => GitWorkbenchEvent::Consumed,
		}
	}

	fn map_diff_action(&mut self, action: DiffActionKind, target: DiffTarget) -> GitWorkbenchEvent {
		if self.streaming && target != DiffTarget::File {
			return GitWorkbenchEvent::Consumed;
		}
		let Some((area, path)) = self.selected.clone() else {
			return GitWorkbenchEvent::Consumed;
		};
		let valid = matches!(
			(action, area),
			(DiffActionKind::Stage | DiffActionKind::Discard, GitArea::Unstaged)
				| (DiffActionKind::Unstage, GitArea::Staged)
		);
		if !valid || (action == DiffActionKind::Discard && target == DiffTarget::File) {
			return GitWorkbenchEvent::Consumed;
		}
		let op = match action {
			DiffActionKind::Stage => GitPatchOp::Stage,
			DiffActionKind::Unstage => GitPatchOp::Unstage,
			DiffActionKind::Discard => GitPatchOp::Discard,
		};
		match target {
			DiffTarget::File => match op {
				GitPatchOp::Stage => GitWorkbenchEvent::Intent(GitIntent::StageFiles(Some(vec![path]))),
				GitPatchOp::Unstage => {
					GitWorkbenchEvent::Intent(GitIntent::UnstageFiles(Some(vec![path])))
				},
				GitPatchOp::Discard => GitWorkbenchEvent::Consumed,
			},
			DiffTarget::Lines { old, new } => {
				GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op, path, old, new })
			},
			DiffTarget::Hunk(index) => {
				let ranges = self
					.ui
					.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| {
						pane.document().and_then(|document| {
							document.hunks.get(index).map(|hunk| {
								(inclusive_range(hunk.old_range), inclusive_range(hunk.new_range))
							})
						})
					})
					.flatten();
				let Some((old, new)) = ranges else {
					return GitWorkbenchEvent::Consumed;
				};
				GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op, path, old, new })
			},
		}
	}

	fn activate_chrome(&mut self, id: &str) -> GitWorkbenchEvent {
		match id {
			"git-close" => GitWorkbenchEvent::Close,
			"git-stage-file" => self
				.selected
				.as_ref()
				.map_or(GitWorkbenchEvent::Consumed, |(_, path)| {
					GitWorkbenchEvent::Intent(GitIntent::StageFiles(Some(vec![path.clone()])))
				}),
			"git-unstage-file" => {
				self
					.selected
					.as_ref()
					.map_or(GitWorkbenchEvent::Consumed, |(_, path)| {
						GitWorkbenchEvent::Intent(GitIntent::UnstageFiles(Some(vec![path.clone()])))
					})
			},
			"git-up" => self.jump_hunk_or_file(-1),
			"git-down" => self.jump_hunk_or_file(1),
			"git-ws" => self.cycle_whitespace(),
			"git-wrap" => {
				self.with_pane(|pane| pane.toggle_wrap());
				self.wrap = !self.wrap;
				self.sync_toggle_props();
				GitWorkbenchEvent::Consumed
			},
			AI_STAGE_BUTTON_ID => self.submit_ai_stage(),
			COMMIT_ID => self.submit_commit(),
			_ => GitWorkbenchEvent::Consumed,
		}
	}

	fn set_mode(&mut self, mode: ViewMode) -> GitWorkbenchEvent {
		self.with_pane(|pane| pane.set_mode(mode));
		self.view_mode = mode;
		self.sync_view_value();
		GitWorkbenchEvent::Consumed
	}

	fn cycle_whitespace(&mut self) -> GitWorkbenchEvent {
		self.whitespace = match self.whitespace {
			DiffWhitespaceMode::Off => DiffWhitespaceMode::Whitespace,
			DiffWhitespaceMode::Whitespace => DiffWhitespaceMode::Formatting,
			DiffWhitespaceMode::Formatting => DiffWhitespaceMode::Off,
		};
		self.status = Some((
			Str::new_static("Whitespace mode changed"),
			self.ctx.theme.muted,
			Instant::now(),
			false,
		));
		if !self.streaming {
			self.install_document();
		}
		self.rebuild();
		GitWorkbenchEvent::Consumed
	}

	fn jump_hunk_or_file(&mut self, direction: i8) -> GitWorkbenchEvent {
		let moved = self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| pane.jump_hunk(direction))
			.unwrap_or(false);
		if moved {
			return GitWorkbenchEvent::Consumed;
		}
		self.select_adjacent_file(if direction < 0 { -1 } else { 1 }, direction < 0)
	}

	fn select_adjacent_file(&mut self, direction: isize, land_last: bool) -> GitWorkbenchEvent {
		let Some((area, path)) = self.selected.as_ref() else {
			return GitWorkbenchEvent::Consumed;
		};
		let start = self.sidebar_rows.iter().position(|row| matches!(&row.target, SidebarTarget::File { area: row_area, path: row_path, .. } if row_area == area && row_path == path)).unwrap_or(self.sidebar_selected);
		let Some(mut index) = start.checked_add_signed(direction) else {
			return GitWorkbenchEvent::Consumed;
		};
		while index < self.sidebar_rows.len() {
			if matches!(self.sidebar_rows[index].target, SidebarTarget::File { .. }) {
				self.pending_last_hunk = land_last;
				return self
					.select_sidebar(index)
					.map_or(GitWorkbenchEvent::Consumed, GitWorkbenchEvent::Intent);
			}
			let Some(next) = index.checked_add_signed(direction) else {
				break;
			};
			index = next;
		}
		GitWorkbenchEvent::Consumed
	}

	fn select_sidebar(&mut self, index: usize) -> Option<GitIntent> {
		let index = index.min(self.sidebar_rows.len().saturating_sub(1));
		if self.sidebar_selected != index {
			self.sidebar_follow_selection = true;
		}
		self.sidebar_selected = index;
		if let Some(key) = self
			.current_sidebar_target()
			.filter(|target| target.is_tree_node())
			.map(SidebarTarget::key)
		{
			let _ = self
				.ui
				.with_component_mut::<Tree, _>(SIDEBAR_ID, |tree| tree.select_key(key.as_str()));
		}
		let next = self
			.current_sidebar_target()
			.and_then(|target| match target {
				SidebarTarget::File { area, path, .. } => Some((*area, path.clone())),
				_ => None,
			});
		self.focus_current();
		if let Some(next) = next
			&& self.selected.as_ref() != Some(&next)
		{
			self.selected = Some(next);
			self.contents = None;
			self.install_document();
			self.rebuild();
			return self.request_selected_load();
		}
		None
	}

	fn set_sidebar_index_for_key(&mut self, key: &str) -> bool {
		let Some(index) = self
			.sidebar_rows
			.iter()
			.position(|row| row.target.key().as_str() == key)
		else {
			return false;
		};
		if self.sidebar_selected != index {
			self.sidebar_follow_selection = true;
		}
		self.sidebar_selected = index;
		true
	}

	fn select_tree_key(&mut self, key: &str) -> GitWorkbenchEvent {
		if !self.set_sidebar_index_for_key(key) {
			return GitWorkbenchEvent::Consumed;
		}
		self
			.select_sidebar(self.sidebar_selected)
			.map_or(GitWorkbenchEvent::Consumed, GitWorkbenchEvent::Intent)
	}

	fn tree_selected_key(&self) -> Option<Str> {
		self
			.ui
			.values()
			.get(SIDEBAR_ID)
			.and_then(serde_json::Value::as_str)
			.map(Str::new)
	}

	fn route_sidebar_tree_key(&mut self, key: Key) -> GitWorkbenchEvent {
		let before = self.tree_selected_key();
		let previous = self.current_sidebar_target().cloned();
		let event = self.ui.handle_key(key);
		let tree_event = matches!(
			event,
			UiEvent::TreeActivated { .. } | UiEvent::TreeToggled { .. } | UiEvent::TreeAction { .. }
		);
		self.sync_control_values();
		let routed = self.route_ui(event);
		if tree_event || !matches!(routed, GitWorkbenchEvent::Consumed) {
			return routed;
		}
		let after = self.tree_selected_key();
		if after != before {
			return after
				.as_deref()
				.map_or(GitWorkbenchEvent::Consumed, |selected| self.select_tree_key(selected));
		}
		match (previous, key) {
			(Some(SidebarTarget::ViewStyle), Key::Down) => self.focus_tree(),
			(Some(target), Key::Up) if target.is_tree_node() => {
				self.select_target_kind(SidebarTarget::ViewStyle);
				GitWorkbenchEvent::Consumed
			},
			(Some(target), Key::Down) if target.is_tree_node() && !self.is_commit_view() => {
				self.select_target_kind(SidebarTarget::AiStage);
				GitWorkbenchEvent::Consumed
			},
			_ => routed,
		}
	}

	fn focus_tree(&mut self) -> GitWorkbenchEvent {
		let selected = self.tree_selected_key();
		if let Some(key) = &selected {
			self.set_sidebar_index_for_key(key.as_str());
		}
		let _ = self.ui.focus_id(SIDEBAR_ID);
		if let Some(key) = selected {
			let _ = self
				.ui
				.with_component_mut::<Tree, _>(SIDEBAR_ID, |tree| tree.select_key(key.as_str()));
		}
		GitWorkbenchEvent::Consumed
	}

	fn current_sidebar_target(&self) -> Option<&SidebarTarget> {
		self
			.sidebar_rows
			.get(self.sidebar_selected)
			.map(|row| &row.target)
	}

	fn select_target_kind(&mut self, desired: SidebarTarget) {
		let desired_key = desired.key();
		if let Some(index) = self
			.sidebar_rows
			.iter()
			.position(|row| row.target.key() == desired_key)
		{
			if self.sidebar_selected != index {
				self.sidebar_follow_selection = true;
			}
			self.sidebar_selected = index;
		}
		self.focus_current();
	}

	fn first_file_target(&self) -> Option<usize> {
		self
			.sidebar_rows
			.iter()
			.position(|row| matches!(row.target, SidebarTarget::File { .. }))
	}

	fn focus_current(&mut self) {
		let selected_tree_key = self
			.current_sidebar_target()
			.filter(|target| target.is_tree_node())
			.map(SidebarTarget::key);
		let id = match self.focus {
			Focus::Diff => DIFF_ID.to_str(),
			Focus::Sidebar => match self.current_sidebar_target() {
				Some(SidebarTarget::ViewStyle) => VIEW_STYLE_ID.to_str(),
				Some(SidebarTarget::Section { .. }) => SIDEBAR_ID.to_str(),
				Some(SidebarTarget::AiStage) => AI_STAGE_ID.to_str(),
				Some(SidebarTarget::Amend) => AMEND_ID.to_str(),
				Some(SidebarTarget::Summary) => SUMMARY_ID.to_str(),
				Some(SidebarTarget::Description) => DESCRIPTION_ID.to_str(),
				Some(SidebarTarget::Commit) => COMMIT_ID.to_str(),
				_ => SIDEBAR_ID.to_str(),
			},
		};
		let _ = self.ui.focus_id(id.as_str());
		if self.focus == Focus::Sidebar
			&& let Some(key) = selected_tree_key
		{
			let _ = self
				.ui
				.with_component_mut::<Tree, _>(SIDEBAR_ID, |tree| tree.select_key(key.as_str()));
		}
		let color = if self.focus == Focus::Sidebar {
			self.ctx.theme.accent
		} else {
			self.ctx.theme.border
		};
		let _ = self.ui.set_prop("git-separator", Prop::Fg, color);
		if self.status.is_none() {
			let hint = self.focus.hint();
			let _ = self.ui.set_text("git-status", hint);
		}
	}

	fn editing(&self) -> bool {
		self.focus == Focus::Sidebar
			&& matches!(
				self.current_sidebar_target(),
				Some(SidebarTarget::Summary | SidebarTarget::Description | SidebarTarget::AiStage)
			)
	}

	fn editor_on_first_line(&mut self) -> bool {
		self
			.ui
			.with_component_mut::<EditorPane, _>(DESCRIPTION_PANE_ID, |editor| {
				editor.cursor_on_first_line()
			})
			.unwrap_or(true)
	}

	fn editor_on_last_line(&mut self) -> bool {
		self
			.ui
			.with_component_mut::<EditorPane, _>(DESCRIPTION_PANE_ID, |editor| {
				editor.cursor_on_last_line()
			})
			.unwrap_or(true)
	}

	fn select_sidebar_form_at(&mut self, row: u16, content_height: u16) {
		if self.is_commit_view() || content_height == 0 {
			return;
		}
		let (_, description, _) = self.form_values();
		let description_rows = description.lines().count().clamp(1, 5) as u16;
		let commit_row = content_height.saturating_sub(1);
		let description_start = commit_row.saturating_sub(description_rows);
		let summary_row = description_start.saturating_sub(1);
		let amend_row = summary_row.saturating_sub(1);
		let target = if row == commit_row {
			Some(SidebarTarget::Commit)
		} else if row >= description_start && row < commit_row {
			Some(SidebarTarget::Description)
		} else if row == summary_row {
			Some(SidebarTarget::Summary)
		} else if row == amend_row {
			Some(SidebarTarget::Amend)
		} else if row == amend_row.saturating_sub(2) {
			Some(SidebarTarget::AiStage)
		} else {
			None
		};
		if let Some(target) = target {
			self.select_target_kind(target);
		}
	}

	fn clear_diff_selection(&mut self) -> bool {
		self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| pane.clear_selection())
			.unwrap_or(false)
	}

	fn form_values(&self) -> (Str, Str, Str) {
		let values = self.ui.values();
		let summary = values
			.get(SUMMARY_ID)
			.and_then(serde_json::Value::as_str)
			.map_or_else(Str::default, Str::new);
		let description = values
			.get(DESCRIPTION_ID)
			.and_then(serde_json::Value::as_str)
			.map_or_else(Str::default, Str::new);
		let ai_instruction = values
			.get(AI_STAGE_ID)
			.and_then(serde_json::Value::as_str)
			.map_or_else(Str::default, Str::new);
		(summary, description, ai_instruction)
	}

	pub(super) fn commit_enabled_with(&self, summary: &str, description: &str) -> bool {
		!self.generation_pending
			&& (!summary.trim().is_empty() || description.trim().is_empty())
			&& (!self.snapshot.staged.is_empty()
				|| !self.snapshot.unstaged.is_empty()
				|| (self.amend && self.snapshot.head.is_some()))
	}

	pub(super) fn commit_button_label(&self) -> &'static str {
		if self.generation_pending {
			"Generating commit message"
		} else if self.snapshot.staged.is_empty() {
			"Stage all & commit"
		} else {
			"Commit staged changes"
		}
	}

	fn sync_commit_button(&mut self) {
		let (summary, description, _) = self.form_values();
		let disabled = !self.commit_enabled_with(summary.as_str(), description.as_str());
		let _ = self.ui.set_prop(COMMIT_ID, Prop::Dim, disabled);
	}

	fn sync_control_values(&mut self) {
		let values = self.ui.values();
		let view_style = values
			.get(VIEW_STYLE_ID)
			.and_then(serde_json::Value::as_str)
			.map(str::to_owned);
		let diff_view = values
			.get(VIEW_ID)
			.and_then(serde_json::Value::as_str)
			.map(str::to_owned);
		let amend = values.get(AMEND_ID).and_then(serde_json::Value::as_bool);
		if let Some(style) = view_style {
			let tree = style == "tree";
			if tree != self.tree {
				self.tree = tree;
				self.rebuild();
				return;
			}
		}
		if let Some(value) = diff_view
			&& let Ok(mode) = value.parse::<ViewMode>()
			&& mode != self.view_mode
		{
			self.view_mode = mode;
			self.with_pane(|pane| pane.set_mode(mode));
		}
		if let Some(checked) = amend
			&& checked != self.amend
		{
			let _ = self.toggle_amend();
		}
	}

	fn sync_view_value(&mut self) {
		self.rebuild();
	}

	fn sync_toggle_props(&mut self) {
		let active = self.wrap;
		let _ = self.ui.set_prop("git-wrap", Prop::Active, active);
	}

	pub(super) const fn pane_mode(&self) -> ViewMode {
		self.view_mode
	}

	pub(super) const fn pane_wraps(&self) -> bool {
		self.wrap
	}

	fn with_pane(&mut self, action: impl FnOnce(&mut DiffPane)) {
		let _ = self.ui.with_component_mut::<DiffPane, _>(DIFF_ID, action);
	}

	fn begin_stream(&mut self) {
		let Some((_, path)) = self.selected.clone() else {
			return;
		};
		let options = DiffBuildOptions { whitespace: self.whitespace, language: None };
		self.with_pane(|pane| {
			pane.set_patch_target(None);
			pane.begin_stream(path, &options);
		});
	}

	fn request_selected_load(&mut self) -> Option<GitIntent> {
		let (area, path) = self.selected.clone()?;
		let orig_path = find_file(&self.snapshot, area, path.as_str())?
			.orig_path
			.clone();
		self.load_seq = self.load_seq.wrapping_add(1);
		self.streaming = false;
		self.load_finished = false;
		self.contents = None;
		self.install_document();
		Some(GitIntent::Load { area, path, orig_path, seq: self.load_seq })
	}

	fn request_avatar(&mut self) -> Option<GitIntent> {
		if !self.is_commit_view() {
			return None;
		}
		let email = self.snapshot.head.as_ref()?.author_email.clone();
		if self.avatar_requested.as_ref() == Some(&email)
			|| self
				.avatar
				.as_ref()
				.is_some_and(|(cached, _)| cached == &email)
		{
			return None;
		}
		self.avatar_requested = Some(email.clone());
		Some(GitIntent::Avatar { email })
	}

	fn install_document(&mut self) {
		self.install_document_inner(false);
	}

	fn finish_document(&mut self) {
		self.install_document_inner(true);
	}

	fn install_document_inner(&mut self, finish_stream: bool) {
		let contents = self.contents.clone();
		let loaded = contents.is_some();
		let selected = self.selected.clone();
		let whitespace = self.whitespace;
		let empty = if self.snapshot.pinned && self.snapshot.head.is_none() {
			"No commits yet"
		} else {
			"No changes"
		};
		let pending_last = self.pending_last_hunk;
		let _ = self.ui.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| {
			pane.set_empty_message(empty);
			match (selected, contents) {
				(None, _) => pane.set_document(None, DiffPaneState::Empty),
				(_, None) => pane.set_document(None, DiffPaneState::Loading),
				(Some(_), Some(contents)) if contents.too_large => {
					pane.set_document(None, DiffPaneState::TooLarge)
				},
				(Some(_), Some(contents)) if contents.media.is_some() => pane.set_asset(
					contents.old_bytes,
					contents.new_bytes,
					contents.media.unwrap_or_default(),
					contents.old_placeholder,
					contents.new_placeholder,
				),
				(Some(_), Some(contents)) if contents.binary => {
					pane.set_document(None, DiffPaneState::Binary)
				},
				(Some((_, path)), Some(contents)) => {
					let options = DiffBuildOptions { whitespace, language: None };
					let old_text = strip_carriage_returns(contents.old_text.as_str());
					let new_text = strip_carriage_returns(contents.new_text.as_str());
					let document = DiffDocument::build(
						old_text.as_ref(),
						new_text.as_ref(),
						path.as_str(),
						&options,
					);
					if finish_stream {
						pane.finish_stream(document);
					} else {
						pane.set_document(Some(document), DiffPaneState::Ready);
					}
					if pending_last {
						while pane.jump_hunk(1) {}
					}
				},
			}
		});
		if loaded {
			self.pending_last_hunk = false;
		}
	}

	fn rebuild(&mut self) {
		let (summary, description, ai_instruction) = self.form_values();
		self.rebuild_with_form(summary.as_str(), description.as_str(), ai_instruction.as_str());
	}

	fn rebuild_with_form(&mut self, summary: &str, description: &str, ai_instruction: &str) {
		let previous_rows = self.sidebar_rows.clone();
		let previous_target = self.current_sidebar_target().cloned();
		self.rebuild_sidebar_rows();
		if let Some(target) = previous_target {
			let key = target.key();
			if let Some(index) = self
				.sidebar_rows
				.iter()
				.position(|row| row.target.key() == key)
			{
				self.sidebar_selected = index;
			} else if let Some(survivor) = nearest_survivor(&previous_rows, &self.sidebar_rows, &key) {
				self.set_sidebar_index_for_key(survivor.as_str());
			}
		}
		let reconciled = self
			.sidebar_selected
			.min(self.sidebar_rows.len().saturating_sub(1));
		if reconciled != self.sidebar_selected {
			self.sidebar_follow_selection = true;
			self.sidebar_selected = reconciled;
		}
		self.rebuild_retained(summary, description, ai_instruction);
	}

	fn rebuild_retained(&mut self, summary: &str, description: &str, ai_instruction: &str) {
		let old_mode = self.view_mode;
		let follow_selection = self.sidebar_follow_selection;
		let old_wrap = self.wrap;
		let (tree_selected, tree_scroll_top) = self
			.ui
			.with_component_mut::<Tree, _>(SIDEBAR_ID, |tree| {
				(tree.selected_key().map(Str::new), tree.scroll_top())
			})
			.unwrap_or_default();
		let fallback = self
			.current_sidebar_target()
			.filter(|target| target.is_tree_node())
			.map(SidebarTarget::key);
		let tree_selected = fallback.or_else(|| {
			tree_selected.filter(|key| self.sidebar_rows.iter().any(|row| row.target.key() == *key))
		});
		let content_rows = self.height.saturating_sub(2).max(1);
		let sidebar_width = (self.width * 3 / 10).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
		let retained = self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, std::mem::take);
		let fresh = retained.is_none();
		let mut pane = retained
			.unwrap_or_default()
			.with(Prop::Id, DIFF_ID)
			.with(Prop::H, content_rows)
			.with(Prop::Minimap, true);
		pane.set_mode(old_mode);
		if fresh && old_wrap {
			pane.toggle_wrap();
		}
		pane.set_patch_target(self.patch_target());
		let sidebar = self.sidebar_component(
			sidebar_width,
			summary,
			description,
			ai_instruction,
			tree_selected.as_deref(),
			tree_scroll_top,
		);
		let root = self.root_component(pane, sidebar, sidebar_width, content_rows);
		self.ui = Ui::from_root(root, self.width.max(1), self.ctx.clone());
		self.focus_current();
		if !follow_selection {
			let _ = self
				.ui
				.with_component_mut::<Tree, _>(SIDEBAR_ID, |tree| tree.set_scroll_top(tree_scroll_top));
		}
		self.sidebar_follow_selection = false;
		if fresh {
			self.install_document();
		}
	}

	fn patch_target(&self) -> Option<DiffPatchTarget> {
		if self.streaming {
			return None;
		}
		let (area, path) = self.selected.as_ref()?;
		let file = find_file(&self.snapshot, *area, path.as_str())?;
		match area {
			GitArea::Unstaged
				if !matches!(file.kind, GitChangeKind::Untracked | GitChangeKind::Conflicted) =>
			{
				Some(DiffPatchTarget::Stage)
			},
			GitArea::Staged => Some(DiffPatchTarget::Unstage),
			GitArea::Unstaged | GitArea::Commit => None,
		}
	}

	fn sync_patch_target(&mut self) {
		let target = self.patch_target();
		self.with_pane(|pane| pane.set_patch_target(target));
	}

	pub(super) fn current_counts(&self) -> (u64, u64) {
		self
			.selected
			.as_ref()
			.and_then(|(area, path)| find_file(&self.snapshot, *area, path.as_str()))
			.map_or((0, 0), |file| (file.additions.unwrap_or(0), file.deletions.unwrap_or(0)))
	}

	pub(super) fn scope_label(&self) -> Str {
		match self.selected.as_ref().map(|(area, _)| area) {
			Some(GitArea::Unstaged)
				if self.selected.as_ref().is_some_and(|(_, path)| {
					find_file(&self.snapshot, GitArea::Unstaged, path.as_str())
						.is_some_and(|file| file.kind == GitChangeKind::Untracked)
				}) =>
			{
				Str::new_static("Untracked")
			},
			Some(GitArea::Unstaged) => Str::new_static("Unstaged"),
			Some(GitArea::Staged) => Str::new_static("Staged"),
			Some(GitArea::Commit) => self
				.snapshot
				.head
				.as_ref()
				.map_or_else(|| Str::new_static("Commit"), |head| short_sha(&head.sha)),
			None => self
				.snapshot
				.branch
				.clone()
				.unwrap_or_else(|| Str::new_static("HEAD")),
		}
	}

	pub(super) fn is_commit_view(&self) -> bool {
		self.snapshot.pinned || (self.snapshot.unstaged.is_empty() && self.snapshot.staged.is_empty())
	}
}

fn nearest_survivor(previous: &[SidebarRow], current: &[SidebarRow], missing: &Str) -> Option<Str> {
	let index = previous
		.iter()
		.position(|row| row.target.key() == *missing)?;
	let current_key = |target: &SidebarTarget| {
		if !target.is_file_or_directory() {
			return None;
		}
		let key = target.key();
		current
			.iter()
			.any(|row| row.target.key() == key)
			.then_some(key)
	};
	for row in &previous[index + 1..] {
		if let Some(key) = current_key(&row.target) {
			return Some(key);
		}
	}
	for row in previous[..index].iter().rev() {
		if let Some(key) = current_key(&row.target) {
			return Some(key);
		}
	}
	current
		.iter()
		.find(|row| matches!(row.target, SidebarTarget::File { .. }))
		.map(|row| row.target.key())
}

fn first_file(snapshot: &GitSnapshot) -> Option<&GitFileRow> {
	if snapshot.pinned || (snapshot.unstaged.is_empty() && snapshot.staged.is_empty()) {
		snapshot.head.as_ref()?.files.first()
	} else {
		snapshot
			.unstaged
			.first()
			.or_else(|| snapshot.staged.first())
	}
}

fn find_file<'a>(snapshot: &'a GitSnapshot, area: GitArea, path: &str) -> Option<&'a GitFileRow> {
	let files: &[GitFileRow] = match area {
		GitArea::Unstaged => &snapshot.unstaged,
		GitArea::Staged => &snapshot.staged,
		GitArea::Commit => snapshot
			.head
			.as_ref()
			.map_or(&[], |head| head.files.as_slice()),
	};
	files.iter().find(|file| file.path.as_str() == path)
}

const fn inclusive_range((start, count): (u32, u32)) -> (u32, u32) {
	if count == 0 {
		(0, 0)
	} else {
		(start, start.saturating_add(count).saturating_sub(1))
	}
}

pub(super) fn split_path(path: &str) -> (&str, &str) {
	path
		.rsplit_once('/')
		.map_or(("", path), |(directory, basename)| (&path[..directory.len() + 1], basename))
}

fn single_line(text: &str) -> Str {
	let mut words = text.split_whitespace();
	let Some(first) = words.next() else {
		return Str::default();
	};
	let mut line = String::with_capacity(text.len());
	line.push_str(first);
	for word in words {
		line.push(' ');
		line.push_str(word);
	}
	line.into()
}

fn sanitize_stream_lines(lines: &mut [Str]) {
	for line in lines {
		if line.contains('\r') {
			*line = Str::new(line.replace('\r', ""));
		}
	}
}

pub(super) fn short_sha(sha: &Str) -> Str {
	sha.slice(..sha.len().min(8))
}

#[cfg(test)]
mod tests {
	use omp_core::{Str, sf};
	use omp_tui::{
		Component as _, DiffActionKind, DiffTarget, Key, Mouse, Size, UiContext, ViewMode,
		components::Tree, test_support::frame_row_text,
	};

	use super::{
		Focus, GitArea, GitChangeKind, GitCommitInfo, GitFileContents, GitFileRow, GitIntent,
		GitPatchOp, GitSnapshot, GitUpdate, GitWorkbench, GitWorkbenchEvent, SidebarGroup,
		SidebarTarget, commit_view::identicon_lines,
	};

	fn file(path: &'static str, area: GitArea) -> GitFileRow {
		GitFileRow {
			path: Str::new_static(path),
			orig_path: None,
			kind: GitChangeKind::Modified,
			area,
			additions: Some(2),
			deletions: Some(1),
		}
	}

	fn head() -> GitCommitInfo {
		GitCommitInfo {
			sha:          Str::new_static("1234567890abcdef"),
			subject:      Str::new_static("existing subject"),
			body:         Str::new_static("existing body"),
			author_name:  Str::new_static("Ada"),
			author_email: Str::new_static("ada@example.com"),
			author_date:  Str::new_static("2026-08-20T00:00:00Z"),
			parents:      vec![Str::new_static("parent")],
			files:        vec![file("src/old.rs", GitArea::Commit)],
		}
	}

	fn dirty() -> GitSnapshot {
		GitSnapshot {
			branch:   Some(Str::new_static("main")),
			unstaged: vec![
				file("a/one.rs", GitArea::Unstaged),
				file("a/two.rs", GitArea::Unstaged),
				file("b/three.rs", GitArea::Unstaged),
			],
			staged:   vec![file("tests/a.rs", GitArea::Staged)],
			head:     Some(head()),
			pinned:   false,
		}
	}

	fn contents(old: &'static str, new: &'static str) -> GitFileContents {
		GitFileContents {
			old_text:        Str::new_static(old),
			new_text:        Str::new_static(new),
			binary:          false,
			too_large:       false,
			old_bytes:       None,
			new_bytes:       None,
			media:           None,
			old_placeholder: None,
			new_placeholder: None,
		}
	}

	fn pane_document(workbench: &mut GitWorkbench) -> omp_tui::DiffDocument {
		workbench
			.ui
			.with_component_mut::<omp_tui::DiffPane, _>(super::diff::DIFF_ID, |pane| {
				pane.document().cloned()
			})
			.flatten()
			.expect("diff document")
	}

	#[test]
	fn streamed_chunks_finish_as_the_one_shot_document() {
		let final_contents = contents("same\nold\n", "same\nnew\n");
		let mut streamed = GitWorkbench::open(dirty(), &UiContext::default());
		let GitIntent::Load { seq, .. } = streamed.initial_intent().expect("load") else {
			panic!("load")
		};
		assert_eq!(
			streamed.apply(GitUpdate::ContentsChunk {
				seq,
				old_lines: vec![Str::new_static("same"), Str::new_static("old")],
				new_lines: vec![Str::new_static("same")],
			}),
			None
		);
		assert!(streamed.streaming);
		assert_eq!(
			streamed.apply(GitUpdate::ContentsChunk {
				seq,
				old_lines: Vec::new(),
				new_lines: vec![Str::new_static("new")],
			}),
			None
		);
		let _ = streamed.apply(GitUpdate::Contents { seq, contents: final_contents.clone() });
		assert!(!streamed.streaming);
		let streamed_document = pane_document(&mut streamed);

		let _ = streamed.apply(GitUpdate::ContentsChunk {
			seq,
			old_lines: vec![Str::new_static("late")],
			new_lines: vec![Str::new_static("late")],
		});
		assert_eq!(streamed_document, pane_document(&mut streamed));

		let mut one_shot = GitWorkbench::open(dirty(), &UiContext::default());
		let GitIntent::Load { seq, .. } = one_shot.initial_intent().expect("load") else {
			panic!("load")
		};
		let _ = one_shot.apply(GitUpdate::Contents { seq, contents: final_contents });
		assert_eq!(streamed_document, pane_document(&mut one_shot));
	}

	#[test]
	fn crlf_contents_and_stream_chunks_match_lf_documents() {
		let mut crlf = GitWorkbench::open(dirty(), &UiContext::default());
		let GitIntent::Load { seq, .. } = crlf.initial_intent().expect("load") else {
			panic!("load")
		};
		let _ = crlf.apply(GitUpdate::ContentsChunk {
			seq,
			old_lines: vec![Str::new_static("old\r")],
			new_lines: vec![Str::new_static("new\r")],
		});
		let _ = crlf.apply(GitUpdate::Contents { seq, contents: contents("old\r\n", "new\r\n") });
		let crlf_document = pane_document(&mut crlf);

		let mut lf = GitWorkbench::open(dirty(), &UiContext::default());
		let GitIntent::Load { seq, .. } = lf.initial_intent().expect("load") else {
			panic!("load")
		};
		let _ = lf.apply(GitUpdate::Contents { seq, contents: contents("old\n", "new\n") });
		assert_eq!(crlf_document, pane_document(&mut lf));
	}

	#[test]
	fn stale_stream_chunk_is_ignored() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let GitIntent::Load { seq, .. } = workbench.initial_intent().expect("load") else {
			panic!("load")
		};
		assert_eq!(
			workbench.apply(GitUpdate::ContentsChunk {
				seq:       seq.wrapping_add(1),
				old_lines: vec![Str::new_static("stale")],
				new_lines: vec![Str::new_static("stale")],
			}),
			None
		);
		assert!(!workbench.streaming);
		assert!(workbench.contents.is_none());
	}

	#[test]
	fn streamed_document_gates_line_patch_actions_until_finish() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let GitIntent::Load { seq, .. } = workbench.initial_intent().expect("load") else {
			panic!("load")
		};
		let _ = workbench.apply(GitUpdate::ContentsChunk {
			seq,
			old_lines: vec![Str::new_static("old")],
			new_lines: vec![Str::new_static("new")],
		});
		workbench.focus = Focus::Diff;
		let _ = workbench.route_diff_navigation(Key::SelectDown);
		assert_eq!(workbench.handle_key(Key::Char('s')), GitWorkbenchEvent::Consumed);
		let _ = workbench.apply(GitUpdate::Contents { seq, contents: contents("old\n", "new\n") });
		let _ = workbench.route_diff_navigation(Key::SelectDown);
		assert!(matches!(
			workbench.handle_key(Key::Char('s')),
			GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op: GitPatchOp::Stage, .. })
		));
	}

	#[test]
	fn vim_sidebar_keys_fold_and_expand_directories() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let directory = workbench
			.sidebar_rows
			.iter()
			.position(
				|row| matches!(&row.target, SidebarTarget::Directory { path, .. } if path.as_str() == "a"),
			)
			.expect("directory");
		let _ = workbench.select_sidebar(directory);
		let key = workbench.current_sidebar_target().expect("target").key();
		assert_eq!(workbench.handle_key(Key::Char('h')), GitWorkbenchEvent::Consumed);
		assert!(workbench.collapsed.contains(&key));
		assert_eq!(workbench.handle_key(Key::Char('l')), GitWorkbenchEvent::Consumed);
		assert!(!workbench.collapsed.contains(&key));
		assert!(matches!(
			workbench.handle_key(Key::Char('G')),
			GitWorkbenchEvent::Intent(GitIntent::Load { path, .. })
				if path.as_str() == "tests/a.rs"
		));
		assert!(matches!(workbench.current_sidebar_target(), Some(SidebarTarget::File { .. })));
		assert_eq!(workbench.handle_key(Key::Char('g')), GitWorkbenchEvent::Consumed);
	}

	#[test]
	fn section_headers_fold_on_enter_and_arrows_but_space_stages() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.select_target_kind(SidebarTarget::Section { area: GitArea::Unstaged });
		let key = workbench.current_sidebar_target().expect("section").key();
		assert_eq!(workbench.handle_key(Key::Enter), GitWorkbenchEvent::Consumed);
		assert!(workbench.collapsed.contains(&key));
		assert_eq!(workbench.handle_key(Key::Right), GitWorkbenchEvent::Consumed);
		assert!(!workbench.collapsed.contains(&key));
		assert!(matches!(
			workbench.handle_key(Key::Space),
			GitWorkbenchEvent::Intent(GitIntent::StageFiles(None))
		));
		workbench.select_target_kind(SidebarTarget::Section { area: GitArea::Staged });
		assert!(matches!(
			workbench.handle_key(Key::Space),
			GitWorkbenchEvent::Intent(GitIntent::UnstageFiles(None))
		));
	}

	#[test]
	fn hunk_navigation_rolls_into_files_and_brackets_switch_directly() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let GitIntent::Load { seq, .. } = workbench.initial_intent().expect("initial load") else {
			panic!("load")
		};
		let old = "old-1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nold-12\n";
		let new = "new-1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\nnew-12\n";
		let _ = workbench.apply(GitUpdate::Contents { seq, contents: contents(old, new) });
		let _ = workbench.set_mode(ViewMode::Hunk);
		workbench.focus = Focus::Diff;
		assert_eq!(workbench.handle_key(Key::JumpNext), GitWorkbenchEvent::Consumed);
		assert_eq!(workbench.selected, Some((GitArea::Unstaged, Str::new_static("a/one.rs"))));
		assert!(matches!(
			workbench.handle_key(Key::JumpNext),
			GitWorkbenchEvent::Intent(GitIntent::Load { path, .. })
				if path.as_str() == "a/two.rs"
		));
		assert!(matches!(
			workbench.handle_key(Key::Char(']')),
			GitWorkbenchEvent::Intent(GitIntent::Load { path, .. })
				if path.as_str() == "b/three.rs"
		));
		assert!(matches!(
			workbench.handle_key(Key::Char('[')),
			GitWorkbenchEvent::Intent(GitIntent::Load { path, .. })
				if path.as_str() == "a/two.rs"
		));
	}

	#[test]
	fn starts_in_sidebar_and_enter_opens_while_space_stages() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		assert_eq!(workbench.focus, Focus::Sidebar);
		let file = workbench.first_file_target().unwrap();
		let _ = workbench.select_sidebar(file);
		assert_eq!(workbench.handle_key(Key::Enter), GitWorkbenchEvent::Consumed);
		assert_eq!(workbench.focus, Focus::Diff);
		workbench.focus = Focus::Sidebar;
		assert!(matches!(
			workbench.handle_key(Key::Space),
			GitWorkbenchEvent::Intent(GitIntent::StageFiles(Some(paths)))
				if paths.as_slice() == [Str::new_static("a/one.rs")]
		));
	}

	#[test]
	fn escape_ladders_from_editor_then_diff_selection_then_close() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.select_target_kind(SidebarTarget::Summary);
		assert_eq!(workbench.handle_key(Key::Esc), GitWorkbenchEvent::Consumed);
		assert!(matches!(workbench.current_sidebar_target(), Some(SidebarTarget::Commit)));
		workbench.focus = Focus::Diff;
		let GitIntent::Load { seq, .. } = workbench.initial_intent().unwrap() else {
			panic!("load")
		};
		workbench.apply(GitUpdate::Contents { seq, contents: contents("old\n", "new\n") });
		workbench.route_diff_navigation(Key::SelectDown);
		assert_eq!(workbench.handle_key(Key::Esc), GitWorkbenchEvent::Consumed);
		assert_eq!(workbench.handle_key(Key::Esc), GitWorkbenchEvent::Close);
	}

	#[test]
	fn discard_is_scoped_and_identity_confirmed() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let file = workbench.first_file_target().unwrap();
		let _ = workbench.select_sidebar(file);
		assert_eq!(workbench.handle_key(Key::Enter), GitWorkbenchEvent::Consumed);
		let GitIntent::Load { seq, .. } = workbench.initial_intent().unwrap() else {
			panic!("load")
		};
		workbench.apply(GitUpdate::Contents { seq, contents: contents("old\n", "new\n") });
		assert_eq!(
			workbench.handle_key(Key::Char('x')),
			GitWorkbenchEvent::Consumed,
			"file-wide discard is forbidden"
		);
		workbench.set_mode(ViewMode::Hunk);
		assert_eq!(workbench.handle_key(Key::Char('x')), GitWorkbenchEvent::Consumed);
		workbench.handle_key(Key::Char('j'));
		assert_eq!(
			workbench.handle_key(Key::Char('x')),
			GitWorkbenchEvent::Consumed,
			"other action invalidates exact identity"
		);
		assert!(matches!(
			workbench.handle_key(Key::Char('x')),
			GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op: GitPatchOp::Discard, .. })
		));
	}

	#[test]
	fn nearest_survivor_prefers_next_then_previous() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let first = workbench
			.sidebar_rows
			.iter()
			.position(
				|row| matches!(&row.target, SidebarTarget::File { path, .. } if path.as_str() == "a/one.rs"),
			)
			.unwrap();
		let _ = workbench.select_sidebar(first);
		workbench.selected = Some((GitArea::Unstaged, Str::new_static("a/one.rs")));
		let mut next = dirty();
		next.unstaged.remove(0);
		let _ = workbench.apply(GitUpdate::Snapshot(next));
		assert!(
			matches!(workbench.current_sidebar_target(), Some(SidebarTarget::File { path, .. }) if path.as_str() == "a/two.rs")
		);
		assert!(
			workbench
				.tree_selected_key()
				.is_some_and(|key| key.as_str() == "file:Unstaged:a/two.rs")
		);
	}

	#[test]
	fn sidebar_tree_chases_selection_inside_its_visible_window() {
		let mut snapshot = dirty();
		snapshot.unstaged.extend((0..40).map(|index| GitFileRow {
			path:      sf!("bulk/file-{index:02}.rs"),
			orig_path: None,
			kind:      GitChangeKind::Modified,
			area:      GitArea::Unstaged,
			additions: Some(2),
			deletions: Some(1),
		}));
		let mut workbench = GitWorkbench::open(snapshot, &UiContext::default());
		let _ = workbench.layer(Size::new(80, 12));
		assert!(matches!(
			workbench.handle_key(Key::End),
			GitWorkbenchEvent::Intent(GitIntent::Load { .. })
		));
		let (selected, scroll_top) = workbench
			.ui
			.with_component_mut::<Tree, _>(super::sidebar::SIDEBAR_ID, |tree| {
				(tree.selected_key().map(str::to_owned), tree.scroll_top())
			})
			.expect("tree");
		assert!(selected.is_some());
		assert!(scroll_top > 0);
		assert_eq!(workbench.handle_key(Key::Down), GitWorkbenchEvent::Consumed);
		assert!(matches!(workbench.current_sidebar_target(), Some(SidebarTarget::Amend)));
		assert_eq!(workbench.handle_key(Key::Up), GitWorkbenchEvent::Consumed);
		assert!(
			workbench
				.current_sidebar_target()
				.is_some_and(SidebarTarget::is_tree_node)
		);
	}

	#[test]
	fn sidebar_wheel_scroll_does_not_flip_diff_focus() {
		let mut snapshot = dirty();
		snapshot.unstaged.extend((0..40).map(|index| GitFileRow {
			path:      sf!("bulk/file-{index:02}.rs"),
			orig_path: None,
			kind:      GitChangeKind::Modified,
			area:      GitArea::Unstaged,
			additions: Some(2),
			deletions: Some(1),
		}));
		let mut workbench = GitWorkbench::open(snapshot, &UiContext::default());
		let viewport = Size::new(80, 12);
		let _ = workbench.layer(viewport);
		let slot = workbench
			.ui
			.with_component_mut::<Tree, _>(super::sidebar::SIDEBAR_ID, |tree| tree.slot())
			.expect("tree");
		workbench.focus = Focus::Diff;
		workbench.focus_current();
		for _ in 0..30 {
			assert_eq!(
				workbench.handle_mouse(79, 6, Mouse::WheelDown, viewport),
				GitWorkbenchEvent::Consumed
			);
		}
		assert_eq!(workbench.focus, Focus::Diff);
		let (after_slot, scroll_top) = workbench
			.ui
			.with_component_mut::<Tree, _>(super::sidebar::SIDEBAR_ID, |tree| {
				(tree.slot(), tree.scroll_top())
			})
			.expect("tree");
		assert_eq!(after_slot, slot, "wheel input must not rebuild the workbench");
		assert!(scroll_top > 0);
		let idle_snapshot = workbench.snapshot.clone();
		let _ = workbench.apply(GitUpdate::Snapshot(idle_snapshot));
		let after_refresh = workbench
			.ui
			.with_component_mut::<Tree, _>(super::sidebar::SIDEBAR_ID, |tree| tree.scroll_top())
			.expect("tree");
		assert_eq!(after_refresh, scroll_top, "idle refresh must preserve wheel scroll");
	}

	#[test]
	fn sidebar_path_truncation_keeps_counts_and_directory_tail_inside_width() {
		let ctx = UiContext::default();
		let snapshot = GitSnapshot {
			branch:   Some(Str::new_static("main")),
			unstaged: vec![file("very/long/directory/prefix/important.rs", GitArea::Unstaged)],
			staged:   Vec::new(),
			head:     Some(head()),
			pinned:   false,
		};
		let mut workbench = GitWorkbench::open(snapshot, &ctx);
		workbench.tree = false;
		workbench.rebuild();
		let viewport = Size::new(80, 18);
		let frame = workbench.layer(viewport).frame;
		let rendered = (0..frame.size().height)
			.map(|row| frame_row_text(frame, row))
			.collect::<Vec<_>>()
			.join("\n");
		assert!(rendered.contains('…'), "{rendered}");
		assert!(rendered.contains("important.rs"), "{rendered}");
		assert!(rendered.contains("+2 −1"), "{rendered}");
	}

	#[test]
	fn commit_submission_includes_editor_description() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		assert_eq!(workbench.handle_key(Key::Char('c')), GitWorkbenchEvent::Consumed);
		for ch in "subject".chars() {
			assert_eq!(workbench.handle_key(Key::Char(ch)), GitWorkbenchEvent::Consumed);
		}
		assert_eq!(workbench.handle_key(Key::Enter), GitWorkbenchEvent::Consumed);
		for ch in "body".chars() {
			assert_eq!(workbench.handle_key(Key::Char(ch)), GitWorkbenchEvent::Consumed);
		}
		assert_eq!(workbench.handle_key(Key::Down), GitWorkbenchEvent::Consumed);
		assert_eq!(
			workbench.handle_key(Key::Enter),
			GitWorkbenchEvent::Intent(GitIntent::Commit {
				message:   Str::new_static("subject\n\nbody"),
				amend:     false,
				stage_all: false,
			})
		);
	}

	#[test]
	fn empty_commit_submission_generates_and_populates_the_form() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.select_target_kind(SidebarTarget::Commit);
		assert_eq!(
			workbench.handle_key(Key::Enter),
			GitWorkbenchEvent::Intent(GitIntent::GenerateCommit { amend: false })
		);
		assert!(workbench.generation_pending);
		assert_eq!(workbench.commit_button_label(), "Generating commit message");
		let _ = workbench.apply(GitUpdate::CommitGenerated {
			summary: Str::new_static("fix(git): corrected staging"),
			body:    Str::new_static("- Preserved selected hunks."),
		});
		let (summary, description, _) = workbench.form_values();
		assert_eq!(summary.as_str(), "fix(git): corrected staging");
		assert_eq!(description.as_str(), "- Preserved selected hunks.");
		assert!(!workbench.generation_pending);
	}

	#[test]
	fn ai_stage_input_emits_one_natural_language_instruction() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.select_target_kind(SidebarTarget::AiStage);
		for character in "comment-only changes".chars() {
			assert_eq!(
				workbench.handle_key(if character == ' ' {
					Key::Space
				} else {
					Key::Char(character)
				}),
				GitWorkbenchEvent::Consumed
			);
		}
		assert_eq!(
			workbench.handle_key(Key::Enter),
			GitWorkbenchEvent::Intent(GitIntent::AiStage {
				instruction: Str::new_static("comment-only changes"),
			})
		);
		assert!(workbench.form_values().2.is_empty());
	}

	#[test]
	fn failed_actions_keep_a_single_line_sticky_footer() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let _ = workbench.apply(GitUpdate::ActionFailed {
			message: Str::new_static("provider failed\nwith details"),
		});
		let (message, _, _, sticky) = workbench.status.as_ref().expect("status");
		assert_eq!(message.as_str(), "provider failed with details");
		assert!(*sticky);
	}

	#[test]
	fn commit_composer_keys_do_not_leak_staging_actions() {
		let mut snapshot = dirty();
		snapshot
			.unstaged
			.insert(0, file("logo.png", GitArea::Unstaged));
		let mut workbench = GitWorkbench::open(snapshot.clone(), &UiContext::default());
		let mut events = Vec::new();
		for key in [Key::Down, Key::Down, Key::Enter, Key::Tab, Key::Space] {
			events.push(workbench.handle_key(key));
		}
		let staged_path = events
			.iter()
			.find_map(|event| match event {
				GitWorkbenchEvent::Intent(GitIntent::StageFiles(Some(paths))) => paths.first().cloned(),
				_ => None,
			})
			.expect("the deliberate Space should stage one file");
		let staged_index = snapshot
			.unstaged
			.iter()
			.position(|file| file.path == staged_path)
			.expect("staged path should be unstaged");
		let mut staged = snapshot.unstaged.remove(staged_index);
		staged.area = GitArea::Staged;
		snapshot.staged.push(staged);
		let _ = workbench.apply(GitUpdate::Snapshot(snapshot));
		let viewport = Size::new(120, 34);
		let _ = workbench.layer(viewport);
		events.push(workbench.handle_mouse(119, 4, Mouse::WheelDown, viewport));
		events.push(workbench.handle_key(Key::Char('c')));
		for ch in "added smoke assets".chars() {
			events.push(workbench.handle_key(if ch == ' ' { Key::Space } else { Key::Char(ch) }));
		}
		events.push(workbench.handle_key(Key::Enter));
		for ch in "body text here".chars() {
			events.push(workbench.handle_key(if ch == ' ' { Key::Space } else { Key::Char(ch) }));
		}
		events.push(workbench.handle_key(Key::Down));
		events.push(workbench.handle_key(Key::Enter));

		let staging = events
			.iter()
			.filter(|event| {
				matches!(
					event,
					GitWorkbenchEvent::Intent(GitIntent::StageFiles(_) | GitIntent::ApplyLines { .. })
				)
			})
			.count();
		assert_eq!(staging, 1, "only the deliberate Space may stage");
		assert!(matches!(
			events.last(),
			Some(GitWorkbenchEvent::Intent(GitIntent::Commit { message, stage_all: false, .. }))
				if message.as_str() == "added smoke assets\n\nbody text here"
		));
	}

	#[test]
	fn amend_prefills_and_success_clears_every_field() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.select_target_kind(SidebarTarget::Amend);
		assert_eq!(workbench.handle_key(Key::Enter), GitWorkbenchEvent::Consumed);
		let (summary, description, _) = workbench.form_values();
		assert_eq!(summary.as_str(), "existing subject");
		assert_eq!(description.as_str(), "existing body");
		workbench.select_target_kind(SidebarTarget::Commit);
		assert!(matches!(
			workbench.handle_key(Key::Enter),
			GitWorkbenchEvent::Intent(GitIntent::Commit { amend: true, .. })
		));
		workbench.apply(GitUpdate::ActionDone { message: Str::new_static("committed") });
		let (summary, description, _) = workbench.form_values();
		assert!(summary.is_empty() && description.is_empty());
		assert!(!workbench.amend);
	}

	#[test]
	fn commit_button_label_and_enabled_state_follow_staging() {
		let mut snapshot = dirty();
		let mut workbench = GitWorkbench::open(snapshot.clone(), &UiContext::default());
		assert_eq!(workbench.commit_button_label(), "Commit staged changes");
		assert!(workbench.commit_enabled_with("   ", ""));
		assert!(workbench.commit_enabled_with("subject", ""));
		assert!(!workbench.commit_enabled_with("", "body only"));
		snapshot.staged.clear();
		let _ = workbench.apply(GitUpdate::Snapshot(snapshot));
		assert_eq!(workbench.commit_button_label(), "Stage all & commit");
	}

	#[test]
	fn pure_additions_are_partitioned_and_batch_independently() {
		let mut snapshot = dirty();
		snapshot.unstaged.push(GitFileRow {
			path:      Str::new_static("a/new.rs"),
			orig_path: None,
			kind:      GitChangeKind::Untracked,
			area:      GitArea::Unstaged,
			additions: Some(3),
			deletions: None,
		});
		snapshot.unstaged.push(GitFileRow {
			path:      Str::new_static("deleted.rs"),
			orig_path: None,
			kind:      GitChangeKind::Deleted,
			area:      GitArea::Unstaged,
			additions: None,
			deletions: Some(3),
		});
		let mut workbench = GitWorkbench::open(snapshot, &UiContext::default());
		let tracked_directory = workbench
			.sidebar_rows
			.iter()
			.position(|row| {
				matches!(&row.target, SidebarTarget::Directory {
					path,
					group: SidebarGroup::Changes,
					..
				} if path.as_str() == "a")
			})
			.expect("tracked a directory");
		let additions_directory = workbench
			.sidebar_rows
			.iter()
			.position(|row| {
				matches!(&row.target, SidebarTarget::Directory {
					path,
					group: SidebarGroup::Additions,
					..
				} if path.as_str() == "a")
			})
			.expect("additions a directory");
		assert!(tracked_directory < additions_directory);
		let added = workbench
			.sidebar_rows
			.iter()
			.find(
				|row| matches!(&row.target, SidebarTarget::File { path, .. } if path.as_str() == "a/new.rs"),
			)
			.expect("addition row");
		assert!(added.status.is_none());
		let deleted = workbench
			.sidebar_rows
			.iter()
			.find(
				|row| matches!(&row.target, SidebarTarget::File { path, .. } if path.as_str() == "deleted.rs"),
			)
			.expect("deleted row");
		assert!(deleted.strike);
		let _ = workbench.select_sidebar(additions_directory);
		assert!(matches!(
			workbench.handle_key(Key::Space),
			GitWorkbenchEvent::Intent(GitIntent::StageFiles(Some(paths)))
				if paths.as_slice() == [Str::new_static("a/new.rs")]
		));
		let mut refreshed = workbench.snapshot.clone();
		let index = refreshed
			.unstaged
			.iter()
			.position(|file| file.path.as_str() == "a/new.rs")
			.expect("addition");
		let mut staged = refreshed.unstaged.remove(index);
		staged.area = GitArea::Staged;
		refreshed.staged.push(staged);
		let _ = workbench.apply(GitUpdate::Snapshot(refreshed));
		assert!(
			workbench
				.current_sidebar_target()
				.is_some_and(SidebarTarget::is_file_or_directory)
		);
		assert!(!matches!(workbench.current_sidebar_target(), Some(SidebarTarget::Directory {
				path,
				group: SidebarGroup::Additions,
				..
			}) if path.as_str() == "a"));
	}

	#[test]
	fn directory_space_batches_exact_subtree_and_explicit_wrong_area_is_noop() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let directory = workbench
			.sidebar_rows
			.iter()
			.position(|row| {
				matches!(&row.target, SidebarTarget::Directory {
					area: GitArea::Unstaged,
					path,
					..
				} if path.as_str() == "a")
			})
			.unwrap();
		let _ = workbench.select_sidebar(directory);
		assert!(matches!(
			workbench.handle_key(Key::Space),
			GitWorkbenchEvent::Intent(GitIntent::StageFiles(Some(paths)))
				if paths.as_slice()
					== [Str::new_static("a/one.rs"), Str::new_static("a/two.rs")]
		));
		assert_eq!(workbench.handle_key(Key::Char('u')), GitWorkbenchEvent::Consumed);
	}

	#[test]
	fn mapped_diff_actions_keep_file_and_line_contracts() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		assert_eq!(
			workbench.map_diff_action(DiffActionKind::Stage, DiffTarget::File),
			GitWorkbenchEvent::Intent(GitIntent::StageFiles(Some(vec![Str::new_static("a/one.rs",)])))
		);
		assert_eq!(
			workbench
				.map_diff_action(DiffActionKind::Stage, DiffTarget::Lines { old: (1, 1), new: (1, 1) }),
			GitWorkbenchEvent::Intent(GitIntent::ApplyLines {
				op:   GitPatchOp::Stage,
				path: Str::new_static("a/one.rs"),
				old:  (1, 1),
				new:  (1, 1),
			})
		);
	}

	#[test]
	fn staged_and_committed_additions_use_consolidated_lists() {
		let mut snapshot = dirty();
		snapshot.staged.push(GitFileRow {
			path:      Str::new_static("tests/new.rs"),
			orig_path: None,
			kind:      GitChangeKind::Added,
			area:      GitArea::Staged,
			additions: Some(3),
			deletions: None,
		});
		let workbench = GitWorkbench::open(snapshot, &UiContext::default());
		let staged = workbench
			.sidebar_rows
			.iter()
			.find(|row| {
				matches!(&row.target, SidebarTarget::File {
					area: GitArea::Staged,
					path,
					..
				} if path.as_str() == "tests/new.rs")
			})
			.expect("staged addition");
		assert_eq!(staged.status.as_deref(), Some("A"));
		assert!(workbench.sidebar_rows.iter().all(|row| {
			!matches!(&row.target, SidebarTarget::Directory {
				area: GitArea::Staged,
				group: SidebarGroup::Additions,
				..
			})
		}));

		let mut commit = head();
		commit.files.push(GitFileRow {
			path:      Str::new_static("src/new.rs"),
			orig_path: None,
			kind:      GitChangeKind::Added,
			area:      GitArea::Commit,
			additions: Some(2),
			deletions: None,
		});
		let committed = GitWorkbench::open(
			GitSnapshot {
				branch:   None,
				unstaged: Vec::new(),
				staged:   Vec::new(),
				head:     Some(commit),
				pinned:   true,
			},
			&UiContext::default(),
		);
		let added = committed
			.sidebar_rows
			.iter()
			.find(|row| {
				matches!(&row.target, SidebarTarget::File {
					area: GitArea::Commit,
					path,
					..
				} if path.as_str() == "src/new.rs")
			})
			.expect("committed addition");
		assert_eq!(added.status.as_deref(), Some("A"));
	}

	#[test]
	fn identicon_is_case_folded_mirrored_and_ten_cells_wide() {
		let ctx = UiContext::default();
		let first = identicon_lines("Ada@Example.COM", &ctx);
		assert_eq!(first, identicon_lines("ada@example.com", &ctx));
		for line in first {
			let cells = line.chars().collect::<Vec<_>>();
			assert_eq!(cells.len(), 10);
			assert_eq!(&cells[0..2], &cells[8..10]);
			assert_eq!(&cells[2..4], &cells[6..8]);
		}
	}
}
