//! Observer-local overlays: approval prompts projected from controller-owned
//! DOM state, plus the model picker, prompt-history picker, and transient
//! notices that live only in the actor (ADR 0005).
//!
//! Every other overlay (session picker, tree, rewind, dashboards, side
//! panels) is a [`Panel`]: a retained component the host stacks, routes keys
//! to, and composites by its [`PanelAnchor`]. Panels ask for effects through
//! [`PanelEvent`] — a console line (ADR 0014), a composer recall, a notice,
//! a clipboard write — and never touch the session DOM.

use std::{
	fmt::{self, Write as _},
	sync::Arc,
	time::Duration,
};

use omp_agent::{ApprovalDecision, ApprovalScope, ApprovalSource};
use omp_con::Ctx;
use omp_core::{Str, StrMut, sf};
use omp_dom::{Dom, KnownTag, PropId, PropKey, Tag, Value};
use omp_tui::{Frame, Key, Prop, Size, Ui, UiContext, UiEvent, assets::provider_logo, dom};

/// Codex quota-reset fireworks celebration.
pub mod fireworks;
/// `/pause` full-screen hold screen.
pub mod pause;
/// `/plan-review` plan review dialog.
pub mod plan_review;
/// Centered scrollable markdown report.
pub mod report;
/// `/git` fullscreen Git workbench.
pub mod git;
/// `/copy` transcript picker.
pub mod copy;
/// `/agents` agent-definition browser.
pub mod agents;
/// `/hub` live agent supervisor and its transcript viewer.
pub mod hub;
/// `/extensions` Extension Control Center dashboard.
pub mod extensions;
/// `/plugins`, `/marketplace` plugin selector.
pub mod plugins;
/// Login dialog, logout account selector, and provider picker.
pub mod login;
/// Debug tools selector plus the context, hotkeys, and changelog reports.
pub mod info;
/// Full-screen `/usage` dashboard.
pub mod usage;
/// Loader-then-result panel over one asynchronous service request.
pub mod tasks;
/// `/branch` rewind selector.
pub mod rewind;
/// `/resume` session picker.
pub mod sessions;
/// Side-channel panels above the editor (`/btw`).
pub mod side;
/// `/tree` branch explorer.
pub mod tree;
/// Application-supplied data feeds for dashboards and account commands.
pub mod services;

pub use services::{NoServices, Services};

/// Where the host composites a [`Panel`] frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelAnchor {
	/// Replaces the composer band (pi swaps the editor slot for pickers).
	Bottom,
	/// Centered modal dialog at 80% width.
	Center,
	/// Full-screen dashboard.
	Full,
	/// Side-channel panel above the editor (`/btw`, `/omfg`, `/cleanse`);
	/// the composer stays live and Esc closes it at rung 2 of the ladder.
	Side,
}

/// What a routed panel key or call asked the host to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PanelEvent {
	/// The panel did not consume the input.
	Ignored,
	/// The panel changed; repaint.
	Consumed,
	/// Close the panel.
	Close,
	/// Run a console line; the panel stays open.
	Run(Str),
	/// Close the panel, then run a console line.
	Finish(Str),
	/// Close the panel and place text in the composer.
	Recall(Str),
	/// Show a transient status notice; the panel stays open.
	Notice(Str),
	/// Write text to the clipboard; the panel stays open.
	Copy(Str),
}

/// pi panel chords the host lowers before handing a panel the raw key
/// (`app.session.*`, `app.tree.*`, `app.tools.expand`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelAction {
	/// Ctrl+P: full vs relative session path.
	TogglePath,
	/// Ctrl+S: mtime vs creation-date sort.
	ToggleSort,
	/// Ctrl+R: inline rename prompt.
	Rename,
	/// Ctrl+D: delete with confirmation.
	Delete,
	/// Ctrl+Backspace (decoded as the readline `ctrl+w` chord): delete
	/// without confirmation.
	DeleteFast,
	/// Ctrl+Left / Alt+Left: fold the subtree or move to the parent.
	FoldUp,
	/// Ctrl+Right / Alt+Right: unfold the subtree or move to the first child.
	UnfoldDown,
	/// Ctrl+O: expand the focused entry.
	Expand,
}

impl PanelAction {
	/// Lowers a decoded key to its pi panel action.
	#[must_use]
	pub const fn from_key(key: Key) -> Option<Self> {
		Some(match key {
			Key::Ctrl('p') => Self::TogglePath,
			Key::Ctrl('s') => Self::ToggleSort,
			Key::Ctrl('r') => Self::Rename,
			Key::Ctrl('d') => Self::Delete,
			Key::Ctrl('w') => Self::DeleteFast,
			Key::WordLeft => Self::FoldUp,
			Key::WordRight => Self::UnfoldDown,
			Key::Ctrl('o') => Self::Expand,
			_ => return None,
		})
	}
}

/// Facts a panel may read while opening or running a call.
pub struct PanelCx<'a> {
	/// Detached session replica.
	pub dom:      &'a Dom,
	/// Console context (convars, registered commands).
	pub con:      &'a Ctx,
	/// Ambient renderer context.
	pub ui:       &'a UiContext,
	/// Current viewport.
	pub viewport: Size,
	/// Application-supplied data feeds; panels that poll a `Pending`
	/// request clone the handle.
	pub services: &'a Arc<dyn Services>,
}

/// A retained observer-local overlay the host stacks and routes to.
pub trait Panel {
	/// Stable identity reported through `HostCommand::Overlay` and the
	/// debug `values` op.
	fn id(&self) -> &'static str;
	/// Where the frame is composited.
	fn anchor(&self) -> PanelAnchor {
		PanelAnchor::Bottom
	}
	/// Applies one lowered pi panel chord. `Ignored` hands the raw key to
	/// [`Panel::key`].
	fn action(&mut self, _action: PanelAction) -> PanelEvent {
		PanelEvent::Ignored
	}
	/// Applies one raw key.
	fn key(&mut self, key: Key) -> PanelEvent;
	/// Applies pasted text.
	fn paste(&mut self, _text: &str) -> PanelEvent {
		PanelEvent::Ignored
	}
	/// Reflows for a viewport and returns the frame to composite.
	fn frame(&mut self, viewport: Size) -> &Frame;
	/// Advances animations (countdowns); returns whether a repaint is due.
	fn tick(&mut self, _now: Duration) -> bool {
		false
	}
	/// Next animation deadline on the host clock.
	fn next_wake(&self) -> Option<Duration> {
		None
	}
	/// Whether the panel finished on its own (an animation ran out); the
	/// host closes it after the tick that reported it.
	fn finished(&self) -> bool {
		false
	}
	/// An effect the panel wants applied after a tick that repainted (an
	/// asynchronous request settled): the host feeds it through the normal
	/// panel-event path.
	fn settled(&mut self) -> Option<PanelEvent> {
		None
	}
}

/// Deferred panel constructor posted by a console command; the host runs
/// it with its live facts. Compared by identity.
#[derive(Clone)]
pub struct PanelOpener(Arc<dyn Fn(&PanelCx<'_>) -> Result<Box<dyn Panel>, Str> + Send + Sync>);

impl PanelOpener {
	/// Wraps a constructor.
	pub fn new(
		open: impl Fn(&PanelCx<'_>) -> Result<Box<dyn Panel>, Str> + Send + Sync + 'static,
	) -> Self {
		Self(Arc::new(open))
	}

	/// Runs the constructor.
	pub fn open(&self, cx: &PanelCx<'_>) -> Result<Box<dyn Panel>, Str> {
		(self.0)(cx)
	}
}

impl fmt::Debug for PanelOpener {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("PanelOpener")
	}
}

impl PartialEq for PanelOpener {
	fn eq(&self, other: &Self) -> bool {
		Arc::ptr_eq(&self.0, &other.0)
	}
}

impl Eq for PanelOpener {}

/// Deferred host-side effect posted by a console command that needs the
/// host's facts but opens nothing; its event runs through the panel path.
#[derive(Clone)]
pub struct PanelCall(Arc<dyn Fn(&PanelCx<'_>) -> PanelEvent + Send + Sync>);

impl PanelCall {
	/// Wraps a call.
	pub fn new(call: impl Fn(&PanelCx<'_>) -> PanelEvent + Send + Sync + 'static) -> Self {
		Self(Arc::new(call))
	}

	/// Runs the call.
	pub fn call(&self, cx: &PanelCx<'_>) -> PanelEvent {
		(self.0)(cx)
	}
}

impl fmt::Debug for PanelCall {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("PanelCall")
	}
}

impl PartialEq for PanelCall {
	fn eq(&self, other: &Self) -> bool {
		Arc::ptr_eq(&self.0, &other.0)
	}
}

impl Eq for PanelCall {}

const MODEL_HINT: &str =
	"↑/↓ models · Enter switch · type to search · Alt+P task model · Esc close";
const MODEL_TASK_HINT: &str =
	"↑/↓ models · Enter use for task subagents · type to search · Alt+P session model · Esc close";
const HISTORY_HINT: &str = "↑/↓ prompts · Enter edit · type to search · Esc close";
const FRAME_ROWS: u16 = 6;
const CONTEXT_WIDTH: u16 = 62;
const INPUT_PRICE_WIDTH: u16 = 76;
const OUTPUT_PRICE_WIDTH: u16 = 88;

/// One open approval prompt projected from `<queues><prompts>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalOverlay {
	/// Stable prompt identity returned with the decision.
	pub id:     Str,
	/// Short user-facing operation title.
	pub title:  Str,
	/// Explanation supplied by host policy.
	pub reason: Str,
	/// Default scope offered by the controller.
	pub scope:   ApprovalScope,
	/// Controller-set deadline after which the kernel answers with the
	/// prompt's default (`timeout-ms`); `None` waits indefinitely.
	pub timeout: Option<Duration>,
}

impl ApprovalOverlay {
	/// Builds the decision represented by an approval hotkey.
	#[must_use]
	pub fn decision(&self, key: char) -> Option<ApprovalDecision> {
		let (approved, scope) = match key {
			'y' => (true, self.scope.clone()),
			'a' => (true, ApprovalScope::Session),
			'n' => (false, ApprovalScope::Once),
			_ => return None,
		};
		Some(ApprovalDecision {
			approved,
			scope,
			source: ApprovalSource::User,
			decided_by: None,
			reason: None,
			audited: false,
		})
	}
}

/// One model shown by the picker, built by the application from the catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRow {
	/// Stable catalog model key (`provider/model`).
	pub key:         Str,
	/// Human-readable model name.
	pub name:        Str,
	/// Stable provider identifier used to resolve its packaged logo.
	pub provider_id: Str,
	/// Human-readable provider name.
	pub provider:    Str,
	/// Context-window size in tokens, when known.
	pub context:     Option<u64>,
	/// Input price in dollars per million tokens, when known.
	pub input_mtok:  Option<f64>,
	/// Output price in dollars per million tokens, when known.
	pub output_mtok: Option<f64>,
	/// Supported thinking efforts ordered least to most intensive; empty for
	/// non-reasoning models.
	pub efforts:     Vec<Str>,
}

/// What a routed picker key did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerEvent {
	/// The picker consumed the key and remains open.
	Consumed,
	/// Close without choosing.
	Close,
	/// Choose the model at this row index for the session.
	Pick(usize),
	/// Choose the model at this row index for task subagents.
	PickTask(usize),
	/// Put this prompt text back into the composer.
	Recall(Str),
}

/// Retained filterable model picker (pi `app.model.selectTemporary`).
pub struct ModelPicker {
	ui:           Ui,
	rows:         Vec<ModelRow>,
	current:      usize,
	task_current: usize,
	task_mode:    bool,
	session_only: bool,
	ctx:          UiContext,
	query:        Str,
	list_rows:    u16,
	width:        u16,
}

impl ModelPicker {
	/// Opens the picker over `rows` with `current` preselected.
	///
	/// `session_only` reports whether the eventual pick should stay out of
	/// `config.cfg` (Alt+P) or be archived (Alt+M).
	#[must_use]
	pub fn open(
		rows: Vec<ModelRow>,
		current: usize,
		task_current: usize,
		session_only: bool,
		width: u16,
		ctx: &UiContext,
	) -> Self {
		let current = current.min(rows.len().saturating_sub(1));
		let task_current = task_current.min(rows.len().saturating_sub(1));
		let mut picker = Self {
			ui: Ui::from_root(dom! { <col/> }, width, ctx.clone()),
			rows,
			current,
			task_current,
			task_mode: false,
			session_only,
			ctx: ctx.clone(),
			query: Str::default(),
			list_rows: 6,
			width,
		};
		picker.rebuild();
		picker
	}

	/// Whether the pick stays session-local.
	#[must_use]
	pub const fn session_only(&self) -> bool {
		self.session_only
	}

	/// Host-supplied rows in picker order.
	#[must_use]
	pub fn rows(&self) -> &[ModelRow] {
		&self.rows
	}

	/// Routes a key into the filter and list.
	pub fn key(&mut self, key: Key) -> PickerEvent {
		if key == Key::Alt('p') {
			self.task_mode = !self.task_mode;
			self.rebuild();
			return PickerEvent::Consumed;
		}
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	/// Routes pasted text into the filter.
	pub fn paste(&mut self, text: &str) -> PickerEvent {
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	/// Reflows for a viewport, returning the frame to composite.
	pub fn frame(&mut self, viewport: Size) -> &Frame {
		let rows = (viewport.height * 2 / 5).saturating_sub(FRAME_ROWS).max(5);
		if rows != self.list_rows {
			self.list_rows = rows;
			self.ui.set_prop("models", Prop::H, rows.saturating_add(1));
		}
		if self.width != viewport.width {
			self.width = viewport.width;
			self.rebuild();
		}
		self.ui.frame()
	}

	fn route(&mut self, event: UiEvent) -> PickerEvent {
		match event {
			UiEvent::Cancel => PickerEvent::Close,
			UiEvent::Changed { id, value } if id.as_str() == "models" => value
				.as_str()
				.parse()
				.map_or(PickerEvent::Consumed, |index| {
					if self.task_mode {
						PickerEvent::PickTask(index)
					} else {
						PickerEvent::Pick(index)
					}
				}),
			UiEvent::Highlighted { id, value } if id.as_str() == "models" => {
				self.show_detail(value.as_str().parse().ok());
				PickerEvent::Consumed
			},
			UiEvent::Filtered { id, query, value } if id.as_str() == "models" => {
				self.query = query;
				self.show_detail(value.and_then(|value| value.as_str().parse().ok()));
				PickerEvent::Consumed
			},
			_ => PickerEvent::Consumed,
		}
	}

	fn rebuild(&mut self) {
		let selected = if self.task_mode {
			self.task_current
		} else {
			self.current
		};
		self.ui = build_models(
			&self.rows,
			selected,
			&self.query,
			self.list_rows,
			self.width,
			self.task_mode,
			&self.ctx,
		);
		self.show_detail((!self.rows.is_empty()).then_some(selected));
	}

	fn show_detail(&mut self, model: Option<usize>) {
		let text = model
			.and_then(|index| self.rows.get(index))
			.map_or_else(|| sf!(" "), model_facts);
		self.ui.set_text("model-facts", text);
	}
}

struct DisplayRow {
	value:    Str,
	label:    Str,
	logo_src: Option<Str>,
	provider: Str,
	name:     Str,
	current:  bool,
	context:  Str,
	input:    Str,
	output:   Str,
}

fn build_models(
	rows: &[ModelRow],
	current: usize,
	query: &str,
	list_rows: u16,
	width: u16,
	task_mode: bool,
	ctx: &UiContext,
) -> Ui {
	let show_context = width >= CONTEXT_WIDTH && rows.iter().any(|row| row.context.is_some());
	let show_input = width >= INPUT_PRICE_WIDTH && rows.iter().any(|row| row.input_mtok.is_some());
	let show_output =
		width >= OUTPUT_PRICE_WIDTH && rows.iter().any(|row| row.output_mtok.is_some());
	let display: Vec<_> = rows
		.iter()
		.enumerate()
		.map(|(index, row)| DisplayRow {
			value:    sf!("{index}"),
			label:    sf!("{} {} {}", row.provider, row.name, row.key),
			logo_src: provider_logo(row.provider_id.as_str())
				.is_some()
				.then(|| sf!("asset://login/{}", row.provider_id)),
			provider: if row.provider.is_empty() {
				row.provider_id.clone()
			} else {
				row.provider.clone()
			},
			name:     if row.name.is_empty() {
				row.key.clone()
			} else {
				row.name.clone()
			},
			current:  index == current,
			context:  row
				.context
				.map_or_else(Str::default, |tokens| sf!("{} ctx", compact_count(tokens))),
			input:    row
				.input_mtok
				.map_or_else(Str::default, |cost| sf!("${cost} in")),
			output:   row
				.output_mtok
				.map_or_else(Str::default, |cost| sf!("${cost} out")),
		})
		.collect();
	let seed = Str::new(query);
	let current_mark = if task_mode {
		Str::new_static(" task")
	} else {
		Str::new_static(" current")
	};
	let title = if task_mode {
		"Switch Task Model"
	} else {
		"Switch Model"
	};
	let hint = if task_mode {
		MODEL_TASK_HINT
	} else {
		MODEL_HINT
	};
	let height = list_rows.saturating_add(1);
	let tree = dom! {
		<box border=round title={title} pad-x=1>
			<col>
				<select id="models" filter={seed} h={height}>
					for row in display {
						<option value={row.value} label={row.label} recommended={row.current}>
							<td>
								if let Some(src) = row.logo_src.clone() { <img src={src} w=2 h=1/> }
							</td>
							<td truncate>
								<pre fg=fg bg=border>{" "}{row.provider}{" "}</pre>
							</td>
							<td truncate=start grow>
								<pre>{row.name}</pre>
								if row.current { <pre fg=ok>{current_mark.clone()}</pre> }
							</td>
							if show_context { <td align=end><pre fg=muted>{row.context}</pre></td> }
							if show_input { <td align=end><pre fg=muted>{row.input}</pre></td> }
							if show_output { <td align=end><pre fg=muted>{row.output}</pre></td> }
						</option>
					}
				</select>
				<hr border=round/>
				<text id="model-facts" fg=muted truncate>{" "}</text>
				<text fg=muted truncate>{hint}</text>
			</col>
		</box>
	};
	Ui::from_root(tree, width, ctx.clone())
}

fn model_facts(row: &ModelRow) -> Str {
	let mut line = StrMut::with_capacity(96);
	let name = if row.name.is_empty() {
		&row.key
	} else {
		&row.name
	};
	push_fact(&mut line, format_args!("{name}"));
	push_fact(&mut line, format_args!("{}", row.provider));
	if let Some(context) = row.context {
		push_fact(&mut line, format_args!("{} context", compact_count(context)));
	}
	match (row.input_mtok, row.output_mtok) {
		(Some(input), Some(output)) => {
			push_fact(&mut line, format_args!("${input}/${output} per Mtok"));
		},
		(Some(input), None) => push_fact(&mut line, format_args!("${input} in per Mtok")),
		(None, Some(output)) => push_fact(&mut line, format_args!("${output} out per Mtok")),
		(None, None) => {},
	}
	if !row.efforts.is_empty() {
		let mut efforts = StrMut::new("thinking ");
		for (index, effort) in row.efforts.iter().enumerate() {
			if index > 0 {
				efforts.push('/');
			}
			efforts.push_str(effort.as_str());
		}
		push_fact(&mut line, format_args!("{}", efforts.as_str()));
	}
	line.freeze()
}

fn push_fact(line: &mut StrMut, fact: fmt::Arguments<'_>) {
	if !line.is_empty() {
		line.push_str(" · ");
	}
	let _ = write!(line, "{fact}");
}

fn compact_count(value: u64) -> Str {
	if value >= 1_000_000 {
		sf!("{:.1}m", value as f64 / 1_000_000.0)
	} else if value >= 1_000 {
		sf!("{:.0}k", value as f64 / 1_000.0)
	} else {
		sf!("{value}")
	}
}

/// Retained prompt-history picker (pi `app.history.search`, Ctrl+R).
pub struct HistoryPicker {
	ui:      Ui,
	prompts: Vec<Str>,
	ctx:     UiContext,
	query:   Str,
	width:   u16,
	rows:    u16,
}

impl HistoryPicker {
	/// Opens the picker over `prompts`, newest first.
	#[must_use]
	pub fn open(prompts: Vec<Str>, width: u16, ctx: &UiContext) -> Self {
		let mut picker = Self {
			ui: Ui::from_root(dom! { <col/> }, width, ctx.clone()),
			prompts,
			ctx: ctx.clone(),
			query: Str::default(),
			width,
			rows: 6,
		};
		picker.rebuild();
		picker
	}

	/// Prompts in picker order (newest first).
	#[must_use]
	pub fn prompts(&self) -> &[Str] {
		&self.prompts
	}

	/// Routes a key into the filter and list.
	pub fn key(&mut self, key: Key) -> PickerEvent {
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	/// Routes pasted text into the filter.
	pub fn paste(&mut self, text: &str) -> PickerEvent {
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	/// Reflows for a viewport, returning the frame to composite.
	pub fn frame(&mut self, viewport: Size) -> &Frame {
		let rows = (viewport.height * 2 / 5).saturating_sub(FRAME_ROWS).max(5);
		if rows != self.rows {
			self.rows = rows;
			self.ui.set_prop("prompts", Prop::H, rows.saturating_add(1));
		}
		if self.width != viewport.width {
			self.width = viewport.width;
			self.rebuild();
		}
		self.ui.frame()
	}

	fn route(&mut self, event: UiEvent) -> PickerEvent {
		match event {
			UiEvent::Cancel => PickerEvent::Close,
			UiEvent::Changed { id, value } if id.as_str() == "prompts" => value
				.as_str()
				.parse::<usize>()
				.ok()
				.and_then(|index| self.prompts.get(index).cloned())
				.map_or(PickerEvent::Consumed, PickerEvent::Recall),
			UiEvent::Filtered { id, query, .. } if id.as_str() == "prompts" => {
				self.query = query;
				PickerEvent::Consumed
			},
			_ => PickerEvent::Consumed,
		}
	}

	fn rebuild(&mut self) {
		let seed = self.query.clone();
		let height = self.rows.saturating_add(1);
		let options = self
			.prompts
			.iter()
			.enumerate()
			.map(|(index, prompt)| {
				let first = prompt.lines().next().unwrap_or_default();
				let more = prompt.lines().count().saturating_sub(1);
				let label = if more > 0 {
					sf!("{first} (+{more} lines)")
				} else {
					Str::new(first)
				};
				(sf!("{index}"), label, prompt.clone())
			})
			.collect::<Vec<_>>();
		let tree = dom! {
			<box border=round title="Search History" pad-x=1>
				<col>
					<select id="prompts" filter={seed} h={height}>
						for (value, label, search) in options {
							<option value={value} label={search}>
								<td truncate grow><pre>{label}</pre></td>
							</option>
						}
					</select>
					<hr border=round/>
					<text fg=muted truncate>{HISTORY_HINT}</text>
				</col>
			</box>
		};
		self.ui = Ui::from_root(tree, self.width, self.ctx.clone());
	}
}

/// Local overlay kind. Overlay state never enters the authoritative DOM.
pub enum Overlay {
	/// Session/task model picker.
	Models(ModelPicker),
	/// Prompt-history picker.
	History(HistoryPicker),
	/// Tool approval prompt projected from the session queue.
	Approval(ApprovalOverlay),
	/// Command-owned panel (session picker, tree, dashboards, side panels).
	Panel(Box<dyn Panel>),
}

impl Overlay {
	/// Stable identity for `HostCommand::Overlay` and the debug `values` op.
	#[must_use]
	pub fn id(&self) -> &'static str {
		match self {
			Self::Models(_) => "models",
			Self::History(_) => "history",
			Self::Approval(_) => "approval",
			Self::Panel(panel) => panel.id(),
		}
	}

	/// Whether this overlay holds keyboard focus (everything but a side
	/// panel, which leaves the composer live).
	#[must_use]
	pub fn modal(&self) -> bool {
		match self {
			Self::Panel(panel) => panel.anchor() != PanelAnchor::Side,
			_ => true,
		}
	}
}

/// Retained local overlay stack plus the one transient status notice.
#[derive(Default)]
pub struct Overlays {
	stack:  Vec<Overlay>,
	notice: Option<Str>,
}

impl Overlays {
	/// Pushes an overlay on top of the stack.
	pub fn show(&mut self, overlay: Overlay) {
		self.stack.push(overlay);
	}

	/// Shows a transient status line (pi `showStatus`), cleared by the next
	/// key; it never displaces an interactive overlay.
	pub fn notify(&mut self, text: impl Into<Str>) {
		self.notice = Some(text.into());
	}

	/// Number of stacked overlays.
	#[must_use]
	pub const fn depth(&self) -> usize {
		self.stack.len()
	}

	/// Whether the topmost overlay is a side-channel panel.
	#[must_use]
	pub fn side_panel(&self) -> bool {
		matches!(self.stack.last(), Some(Overlay::Panel(panel)) if panel.anchor() == PanelAnchor::Side)
	}

	/// Reprojects the first pending approval from the detached DOM replica.
	///
	/// A pending approval always wins; when none is pending, a stale approval
	/// overlay is cleared and any other local overlay is retained.
	pub fn sync_approval(&mut self, dom: &Dom) {
		let approval = dom
			.children(dom.queues())
			.iter()
			.filter_map(|handle| dom.get(*handle))
			.find(|node| node.tag == Tag::Known(KnownTag::Prompts))
			.into_iter()
			.flat_map(|prompts| prompts.kids.iter())
			.filter_map(|handle| dom.get(*handle))
			.find(|node| {
				node.tag == Tag::Known(KnownTag::Prompt)
					&& text_prop(node, PropId::Kind) == Some("approval")
					&& matches!(text_prop(node, PropId::Status), Some("pending" | "open"))
			})
			.and_then(|node| {
				let id = text_prop(node, PropId::Id)?;
				let scope = custom_text(node, "scope")
					.unwrap_or("once")
					.parse::<ApprovalScope>()
					.expect("approval scope parsing is infallible");
				let timeout = node
					.prop(&PropKey::Custom(Str::new_static("timeout-ms")))
					.and_then(|value| match value {
						Value::Int(ms) => u64::try_from(*ms).ok(),
						_ => None,
					})
					.filter(|ms| *ms > 0)
					.map(Duration::from_millis);
				Some(ApprovalOverlay {
					id: Str::new(id),
					title: Str::new(text_prop(node, PropId::Label).unwrap_or("Approval required")),
					reason: Str::new(text_prop(node, PropId::Detail).unwrap_or_default()),
					scope,
					timeout,
				})
			});
		let at = self
			.stack
			.iter()
			.position(|overlay| matches!(overlay, Overlay::Approval(_)));
		match (approval, at) {
			(Some(approval), Some(at)) => {
				self.stack.remove(at);
				self.stack.push(Overlay::Approval(approval));
			},
			(Some(approval), None) => self.stack.push(Overlay::Approval(approval)),
			(None, Some(at)) => {
				self.stack.remove(at);
			},
			(None, None) => {},
		}
	}

	/// Pops the topmost observer-local overlay.
	pub fn dismiss(&mut self) -> Option<Overlay> {
		self.stack.pop()
	}

	/// Drops the transient notice, keeping every interactive overlay.
	pub fn clear_notice(&mut self) {
		self.notice = None;
	}

	/// Returns the topmost overlay.
	#[must_use]
	pub fn active(&self) -> Option<&Overlay> {
		self.stack.last()
	}

	/// Returns the topmost overlay mutably.
	pub fn active_mut(&mut self) -> Option<&mut Overlay> {
		self.stack.last_mut()
	}

	/// Whether the topmost overlay holds keyboard focus.
	#[must_use]
	pub fn modal(&self) -> bool {
		self.stack.last().is_some_and(Overlay::modal)
	}

	/// Returns the pending approval, when one is stacked.
	#[must_use]
	pub fn approval(&self) -> Option<&ApprovalOverlay> {
		self.stack.iter().rev().find_map(|overlay| match overlay {
			Overlay::Approval(approval) => Some(approval),
			_ => None,
		})
	}

	/// Returns the visible notice text.
	#[must_use]
	pub fn notice(&self) -> Option<&str> {
		self.notice.as_deref()
	}
}

/// User prompts on the live chain, newest first, for the history picker.
#[must_use]
pub fn prompt_history(dom: &Dom) -> Vec<Str> {
	let mut prompts = Vec::new();
	for turn in dom.children(dom.body()).iter().rev() {
		for child in dom.children(*turn).iter().rev() {
			let Some(node) = dom.get(*child) else {
				continue;
			};
			if node.tag != Tag::Known(KnownTag::User) {
				continue;
			}
			let Some(text) = node.content.as_ref() else {
				continue;
			};
			if !text.trim().is_empty() && !prompts.contains(text) {
				prompts.push(text.clone());
			}
		}
	}
	prompts
}

fn text_prop(node: &omp_dom::Node, prop: PropId) -> Option<&str> {
	node.prop(&prop.into()).and_then(Value::as_str)
}

fn custom_text<'a>(node: &'a omp_dom::Node, name: &'static str) -> Option<&'a str> {
	node
		.prop(&PropKey::Custom(Str::new_static(name)))
		.and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn row(provider: &'static str, name: &'static str) -> ModelRow {
		ModelRow {
			key:         sf!("{provider}/{name}"),
			name:        Str::new_static(name),
			provider_id: Str::new_static(provider),
			provider:    Str::new_static(provider),
			context:     None,
			input_mtok:  None,
			output_mtok: None,
			efforts:     Vec::new(),
		}
	}

	fn picker(rows: Vec<ModelRow>, current: usize, task_current: usize) -> ModelPicker {
		ModelPicker::open(rows, current, task_current, true, 100, &UiContext::default())
	}

	#[test]
	fn absent_model_facts_are_omitted() {
		let facts = model_facts(&row("p", "Model"));
		assert!(!facts.contains("ctx"));
		assert!(!facts.contains('$'));
		assert!(!facts.contains("thinking"));
	}

	#[test]
	fn typing_filters_models() {
		let mut picker = picker(vec![row("alpha", "first"), row("beta", "second")], 0, 0);
		assert_eq!(picker.key(Key::Char('b')), PickerEvent::Consumed);
		assert_eq!(picker.key(Key::Enter), PickerEvent::Pick(1));
	}

	#[test]
	fn down_then_enter_picks_the_next_model() {
		let mut picker = picker(vec![row("alpha", "first"), row("beta", "second")], 0, 0);
		assert_eq!(picker.key(Key::Down), PickerEvent::Consumed);
		assert_eq!(picker.key(Key::Enter), PickerEvent::Pick(1));
	}

	#[test]
	fn escape_closes_the_picker() {
		let mut picker = picker(vec![row("alpha", "first")], 0, 0);
		assert_eq!(picker.key(Key::Esc), PickerEvent::Close);
	}

	#[test]
	fn alt_p_toggles_task_mode_and_picks_the_task_model() {
		let mut picker = picker(vec![row("alpha", "first"), row("beta", "second")], 0, 1);
		assert_eq!(picker.key(Key::Alt('p')), PickerEvent::Consumed);
		assert_eq!(picker.key(Key::Enter), PickerEvent::PickTask(1));
	}

	#[test]
	fn picker_frame_paints_title_rows_and_hint() {
		let mut picker = picker(vec![row("anthropic", "Claude"), row("openai", "GPT")], 0, 0);
		let frame = picker.frame(Size::new(100, 40));
		let text = omp_tui::frame_text(frame);
		assert!(text.contains("Switch Model"), "{text}");
		assert!(text.contains("Claude"), "{text}");
		assert!(text.contains("current"), "{text}");
		assert!(text.contains("Esc close"), "{text}");
	}

	#[test]
	fn history_picker_recalls_the_selected_prompt() {
		let prompts = vec![Str::new_static("newest"), Str::new_static("older\nsecond line")];
		let mut picker = HistoryPicker::open(prompts, 80, &UiContext::default());
		assert_eq!(picker.key(Key::Down), PickerEvent::Consumed);
		assert_eq!(
			picker.key(Key::Enter),
			PickerEvent::Recall(Str::new_static("older\nsecond line"))
		);
		let text = omp_tui::frame_text(picker.frame(Size::new(80, 30)));
		assert!(text.contains("Search History"), "{text}");
		assert!(text.contains("(+1 lines)"), "{text}");
	}

	#[test]
	fn notices_never_displace_a_modal_overlay_and_clear_on_request() {
		let mut overlays = Overlays::default();
		overlays.notify("hi");
		assert_eq!(overlays.notice(), Some("hi"));
		assert!(!overlays.modal());
		overlays.clear_notice();
		assert!(overlays.active().is_none());
		overlays.show(Overlay::Models(picker(vec![row("a", "b")], 0, 0)));
		overlays.notify("still");
		overlays.clear_notice();
		assert!(overlays.modal());
		assert_eq!(overlays.notice(), None);
	}

	struct Side;

	impl Panel for Side {
		fn id(&self) -> &'static str {
			"side"
		}

		fn anchor(&self) -> PanelAnchor {
			PanelAnchor::Side
		}

		fn key(&mut self, _key: Key) -> PanelEvent {
			PanelEvent::Ignored
		}

		fn frame(&mut self, _viewport: Size) -> &Frame {
			unreachable!("never painted in this test")
		}
	}

	#[test]
	fn overlays_stack_and_a_pending_approval_always_rises_to_the_top() {
		let mut overlays = Overlays::default();
		overlays.show(Overlay::Panel(Box::new(Side)));
		assert!(overlays.side_panel());
		assert!(!overlays.modal());
		overlays.show(Overlay::Models(picker(vec![row("a", "b")], 0, 0)));
		assert!(overlays.modal());
		assert_eq!(overlays.depth(), 2);
		assert_eq!(overlays.dismiss().map(|overlay| overlay.id()), Some("models"));
		assert_eq!(overlays.active().map(Overlay::id), Some("side"));
	}

	#[test]
	fn panel_actions_lower_the_pi_session_and_tree_chords() {
		assert_eq!(PanelAction::from_key(Key::Ctrl('p')), Some(PanelAction::TogglePath));
		assert_eq!(PanelAction::from_key(Key::Ctrl('s')), Some(PanelAction::ToggleSort));
		assert_eq!(PanelAction::from_key(Key::Ctrl('r')), Some(PanelAction::Rename));
		assert_eq!(PanelAction::from_key(Key::Ctrl('d')), Some(PanelAction::Delete));
		assert_eq!(PanelAction::from_key(Key::Ctrl('w')), Some(PanelAction::DeleteFast));
		assert_eq!(PanelAction::from_key(Key::WordLeft), Some(PanelAction::FoldUp));
		assert_eq!(PanelAction::from_key(Key::WordRight), Some(PanelAction::UnfoldDown));
		assert_eq!(PanelAction::from_key(Key::Ctrl('o')), Some(PanelAction::Expand));
		assert_eq!(PanelAction::from_key(Key::Char('x')), None);
	}
}
