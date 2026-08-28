//! Immediate-mode chat scene with a fixed-height viewport and explicit block
//! retirement.

use std::{
	cell::RefCell,
	collections::{BTreeMap, VecDeque},
	fmt::Write as _,
	ops::Range,
	rc::Rc,
	time::{Duration, Instant},
};

use omp_core::{IntoStr, Str, StrMut, fmts_mut, sf};
use omp_tui::{
	Border, Cached, CellContent, Charset, Color, Command, Component, Decor, DecorKind, Frame,
	HistoryReplay, Icon, Key, MarkupOrigin, MouseReport, PaintCtx, Prop, PropValue, Props, Rect,
	Size, Slot, SpellingFeatures, Style, Theme, Ui, UiContext, UiEvent,
	anim::{self, Easing, Shimmer, Tween},
	components::{
		Attachment, AttachmentContent, Attachments, ComposerStatusAttachment, ComposerStyle,
		ContextGauge, ContextGaugeMode, EditorPane, GaugeCell, Img, InlineAccent, KeywordAccent,
		Markdown, Segment, Status, TextLeaf, ToolCard, ToolState, advisor_spend_label,
		boundary_layout, collapse_hud_line, compaction_boundary_color, compaction_threshold_color,
		hr::truncate_to_width, spend_label, write_compact_count,
	},
	next_slot, parse_with_origin,
};
use smallvec::SmallVec;

use crate::{
	ActivityWaveform, AgentRow, AssistantUsage, BackendEvent, CompactionSpeculationStatus,
	ModelDownloadProgress, QueuedPrompt, StatusFacts, StatusLayout, StatusSeparator, SubmitMode,
	ThinkingLevel, TodoHud, ToolTerminal, ToolViewContent, TranscriptFrame, TranscriptFrameKind,
	TurnAnchor, WorkingIndicator,
	blocks::{BlockOrdinal, Blocks},
	completion::{CompletionChain, ReloadableSlashCommands},
	frame::{FrameError, FrameMutation, RetainedFrames, render_frame_tml},
	slots::{Mount, Slots},
};

/// Column cap for inline tool-result images inside committed cards.
const TOOL_IMAGE_MAX_COLS: u16 = 64;
/// Row cap for inline tool-result images inside committed cards.
const TOOL_IMAGE_MAX_ROWS: u16 = 12;
const SHIMMER_PERIOD: Duration = Duration::from_millis(1900);
const BRAND_FADE: Duration = Duration::from_millis(450);
const FADE_FRAME: Duration = Duration::from_millis(40);
const STREAM_REVEAL_FRAME: Duration = Duration::from_millis(33);
const STREAM_REVEAL_MIN: usize = 3;
const STREAM_REVEAL_MAX: usize = 64;
const SPECULATION_PULSE: Duration = Duration::from_millis(600);
const STATUS_ID: &str = "status";

use std::{borrow, fs, mem};

use omp_proto::omp::ui::v1;
use strum::IntoStaticStr;

use crate::{queue, slots::Damage};
const INPUT_ID: &str = "input";
const LIVE_TOOL_CARD_ID: &str = "live-tool-card";
const LIVE_ASSISTANT_ID: &str = "live-assistant";

const LIVE_VOICE_ROWS: u16 = 4;
const LIVE_VOICE_FRAME: Duration = Duration::from_millis(50);
/// Memory bound on live (uncommitted) blocks: reaching it forces retirement
/// offers even while the screen still has room. Uses the transcript cap.
const MAX_LIVE_BLOCKS: u64 = 256;

/// Provider phase displayed while realtime voice owns the composer.
#[derive(Clone, Copy, Debug, Default, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "lowercase")]
pub enum LiveVoicePhase {
	/// Establishing signaling and media channels.
	#[default]
	Connecting,
	/// Waiting for user speech.
	Listening,
	/// Provider is preparing a response.
	Thinking,
	/// Remote audio is playing.
	Speaking,
	/// Durable coding work is active.
	Working,
	/// Transport is closing.
	Closing,
	/// Session failed and is awaiting teardown.
	Error,
}

/// Host action produced by realtime voice takeover key handling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveVoiceAction {
	/// Apply the new microphone mute state.
	SetMuted(bool),
	/// Terminate realtime voice and restore the composer.
	Close,
}

/// Animated realtime voice composer takeover.
#[derive(Clone, Debug)]
pub struct LiveVoiceVisualizer {
	phase:        LiveVoicePhase,
	muted:        bool,
	input_level:  f32,
	output_level: f32,
	history:      VecDeque<u8>,
	transcript:   Str,
}

impl Default for LiveVoiceVisualizer {
	fn default() -> Self {
		Self {
			phase:        LiveVoicePhase::Connecting,
			muted:        false,
			input_level:  0.0,
			output_level: 0.0,
			history:      VecDeque::with_capacity(32),
			transcript:   Str::default(),
		}
	}
}

impl LiveVoiceVisualizer {
	/// Updates provider phase.
	pub const fn set_phase(&mut self, phase: LiveVoicePhase) {
		self.phase = phase;
	}

	/// Records bounded microphone and playback levels.
	pub fn set_levels(&mut self, input: f32, output: f32) {
		self.input_level = sanitize_level(input);
		self.output_level = sanitize_level(output);
		let combined = self.input_level.max(self.output_level);
		self.history.push_back((combined * 8.0).round() as u8);
		while self.history.len() > 32 {
			self.history.pop_front();
		}
	}

	/// Replaces the volatile user transcript displayed beneath the meter.
	pub fn set_transcript(&mut self, transcript: Str) {
		self.transcript = transcript;
	}

	/// Whether microphone transmission is muted.
	pub const fn muted(&self) -> bool {
		self.muted
	}

	fn toggle_mute(&mut self) -> LiveVoiceAction {
		self.muted = !self.muted;
		LiveVoiceAction::SetMuted(self.muted)
	}
}

fn sanitize_level(level: f32) -> f32 {
	if level.is_finite() {
		level.clamp(0.0, 1.0)
	} else {
		0.0
	}
}
fn draw_live_voice_visualizer(
	frame: &mut Frame,
	rect: Rect,
	visualizer: &LiveVoiceVisualizer,
	elapsed: Duration,
	ctx: &UiContext,
) {
	if rect.width < 4 || rect.height < LIVE_VOICE_ROWS {
		return;
	}
	let state_color = match visualizer.phase {
		LiveVoicePhase::Connecting | LiveVoicePhase::Thinking => ctx.theme.info,
		LiveVoicePhase::Listening => ctx.theme.ok,
		LiveVoicePhase::Speaking => ctx.theme.accent,
		LiveVoicePhase::Working => ctx.theme.warn,
		LiveVoicePhase::Closing => ctx.theme.muted,
		LiveVoicePhase::Error => ctx.theme.err,
	};
	draw_box(frame, rect, ink(state_color), panel_style(ctx.theme), ctx.charset, ctx.native_decor);
	let icon = match (ctx.charset, visualizer.phase) {
		(Charset::Ascii, LiveVoicePhase::Listening) => ">",
		(Charset::Ascii, LiveVoicePhase::Speaking) => "<",
		(Charset::Ascii, LiveVoicePhase::Thinking | LiveVoicePhase::Connecting) => "*",
		(Charset::Ascii, LiveVoicePhase::Working) => "+",
		(Charset::Ascii, LiveVoicePhase::Closing | LiveVoicePhase::Error) => "!",
		(_, LiveVoicePhase::Listening) => "●",
		(_, LiveVoicePhase::Speaking) => "◖",
		(_, LiveVoicePhase::Thinking | LiveVoicePhase::Connecting) => "◌",
		(_, LiveVoicePhase::Working) => "◆",
		(_, LiveVoicePhase::Closing | LiveVoicePhase::Error) => "×",
	};
	let phase: &'static str = visualizer.phase.into();
	let mute = if visualizer.muted {
		"muted · space unmutes"
	} else {
		"space mutes"
	};
	draw_line(
		frame,
		rect.x.saturating_add(1),
		rect.y.saturating_add(1),
		rect.width.saturating_sub(2),
		&[
			Span::new(icon, ink(state_color).bold()),
			Span::new(" ", ink(ctx.theme.muted)),
			Span::new(phase, ink(state_color).bold()),
			Span::new(" · ", ink(ctx.theme.muted)),
			Span::new(mute, ink(ctx.theme.muted)),
			Span::new(" · esc closes", ink(ctx.theme.muted)),
		],
	);
	let mut x = rect.x.saturating_add(1);
	let available = rect.width.saturating_sub(2);
	let meter_width = available.min(32);
	let glyphs = if ctx.charset == Charset::Ascii {
		[".", ":", "-", "=", "#"]
	} else {
		["▁", "▂", "▄", "▆", "█"]
	};
	let phase_offset = usize::try_from(elapsed.as_millis() / 100).unwrap_or(0);
	for index in 0..meter_width {
		let history_index = visualizer
			.history
			.len()
			.saturating_sub(usize::from(meter_width - index));
		let level = visualizer.history.get(history_index).copied().unwrap_or(0);
		let animated =
			if matches!(visualizer.phase, LiveVoicePhase::Connecting | LiveVoicePhase::Thinking) {
				level.max(((phase_offset + usize::from(index)) % 5) as u8)
			} else {
				level
			};
		let glyph = glyphs[usize::from(animated).min(8) * (glyphs.len() - 1) / 8];
		x = frame.put(x, rect.y.saturating_add(2), glyph, ink(state_color));
	}
	if meter_width < available && !visualizer.transcript.is_empty() {
		let text = truncate_to_width(
			visualizer.transcript.as_str(),
			available.saturating_sub(meter_width).saturating_sub(1),
		);
		frame.put(
			x.saturating_add(1),
			rect.y.saturating_add(2),
			text.text,
			ink(ctx.theme.fg).italic(),
		);
	}
}

/// Exactly one fixed-height viewport paint and its local repaint ranges.
pub struct ViewportFrame<'a> {
	/// Exactly viewport-sized presentation frame.
	pub frame:  &'a Frame,
	/// Half-open viewport row ranges changed since the previous render.
	pub damage: SmallVec<(u16, u16), 4>,
}
/// One immutable terminal-history transaction prepared by the scene.
pub struct RetirementBatch {
	/// Half-open block ordinal range committed by a successful transaction.
	///
	/// Replay transactions do not advance this range.
	pub range:     Range<u64>,
	/// Width-dependent finalized rows written by a commit transaction.
	pub frame:     Frame,
	replay_frames: Option<Vec<Frame>>,
	kind:          RetirementKind,
}

impl RetirementBatch {
	/// Returns the replay policy and ordered row segments, or `None` for a
	/// commit transaction.
	pub(crate) fn replay_plan(&self) -> Option<(HistoryReplay, &[Frame])> {
		match (self.kind, self.replay_frames.as_deref()) {
			(RetirementKind::Replay(mode), Some(frames)) => Some((mode, frames)),
			(RetirementKind::Commit | RetirementKind::Band, None) => None,
			_ => unreachable!("replay kind and segmented frame plan must agree"),
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetirementKind {
	Commit,
	Replay(HistoryReplay),
	/// Resident band rows scrolled into native history ahead of newer content.
	Band,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Replay {
	end:  u64,
	mode: HistoryReplay,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetirementPolicy {
	Pressure,
	Flush,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdmissionMode {
	Allow,
	Defer,
}

/// Protocol placements used by [`Bands`] without allocating a per-frame
/// collection of rendered rows.
pub mod placement {
	/// Extension content above the transcript.
	pub const HEADER: i32 = 1;
	/// Extension content below the transcript.
	pub const FOOTER: i32 = 2;
	/// A left out-of-tree rail.
	pub const LEFT_RAIL: i32 = 3;
	/// A right out-of-tree rail.
	pub const RIGHT_RAIL: i32 = 4;
	/// Extension content above the editor.
	pub const ABOVE_EDITOR: i32 = 5;
	/// Extension content below the editor.
	pub const BELOW_EDITOR: i32 = 6;
}

/// Total columns consumed by all visible out-of-tree rails.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RailWidths {
	/// Sum of left rail widths.
	pub left:  u16,
	/// Sum of right rail widths.
	pub right: u16,
}

impl RailWidths {
	/// Adds one rail width to the requested side, saturating at terminal size.
	pub const fn accumulate(mut self, left: bool, width: u16) -> Self {
		if left {
			self.left = self.left.saturating_add(width);
		} else {
			self.right = self.right.saturating_add(width);
		}
		self
	}

	/// Returns columns remaining for the transcript.
	pub const fn content_width(self, viewport: u16) -> u16 {
		viewport.saturating_sub(self.left.saturating_add(self.right))
	}
}

/// Streaming compositor for extension bands and rails.
///
/// Each mount owns a retained [`Ui`]. Composition measures it and blits one
/// row at a time into the supplied viewport frame without a frame-local line
/// collection.
pub struct Bands;

impl Bands {
	/// Streams all visible extension mounts over `frame` and returns total rail
	/// reservation. Core callers pass their retained [`Slots`] registry.
	pub fn compose(frame: &mut Frame, slots: &mut Slots, viewport: Size) -> RailWidths {
		let mut rails = RailWidths::default();
		for mount in slots.mounts_at_mut(placement::LEFT_RAIL) {
			if mount.visible() {
				rails = rails.accumulate(true, mount.preferred_width().unwrap_or(0));
			}
		}
		for mount in slots.mounts_at_mut(placement::RIGHT_RAIL) {
			if mount.visible() {
				rails = rails.accumulate(false, mount.preferred_width().unwrap_or(0));
			}
		}
		let content = Rect::new(
			rails.left.min(viewport.width),
			0,
			rails.content_width(viewport.width),
			viewport.height,
		);
		let mut left = 0;
		Self::stream_rail(
			frame,
			slots.mounts_at_mut(placement::LEFT_RAIL),
			&mut left,
			true,
			viewport,
		);
		let mut right = viewport.width;
		Self::stream_rail(
			frame,
			slots.mounts_at_mut(placement::RIGHT_RAIL),
			&mut right,
			false,
			viewport,
		);
		let mut top = 0;
		Self::stream_stack(frame, slots.mounts_at_mut(placement::HEADER), &mut top, content);
		Self::stream_stack(frame, slots.mounts_at_mut(placement::ABOVE_EDITOR), &mut top, content);
		let mut bottom = viewport.height;
		Self::stream_stack_up(frame, slots.mounts_at_mut(placement::FOOTER), &mut bottom, content);
		Self::stream_stack_up(
			frame,
			slots.mounts_at_mut(placement::BELOW_EDITOR),
			&mut bottom,
			content,
		);
		rails
	}

	/// Composes extension layers, then paints core attribution in the reserved
	/// z-band above them.
	pub fn compose_with_attribution(
		frame: &mut Frame,
		slots: &mut Slots,
		viewport: Size,
		attribution: &Attribution,
		theme: Theme,
	) -> RailWidths {
		let rails = Self::compose(frame, slots, viewport);
		attribution.render(frame, viewport.width, theme);
		rails
	}

	fn stream_rail<'a>(
		frame: &mut Frame,
		mounts: impl Iterator<Item = &'a mut Mount>,
		cursor: &mut u16,
		left: bool,
		viewport: Size,
	) {
		for mount in mounts {
			if !mount.visible() {
				continue;
			}
			let width = mount
				.preferred_width()
				.unwrap_or(0)
				.min(viewport.width.saturating_sub(*cursor));
			if width == 0 {
				continue;
			}
			let x = if left {
				*cursor
			} else {
				cursor.saturating_sub(width)
			};
			mount.ui_mut().resize(width.max(1));
			let height = mount.ui_mut().frame().size().height.min(viewport.height);
			let rect = Rect::new(x, 0, width, height);
			mount.resolve(rect);
			Self::stream(frame, mount, rect);
			if left {
				*cursor = cursor.saturating_add(width);
			} else {
				*cursor = x;
			}
		}
	}

	fn stream_stack<'a>(
		frame: &mut Frame,
		mounts: impl Iterator<Item = &'a mut Mount>,
		cursor: &mut u16,
		content: Rect,
	) {
		for mount in mounts {
			if !mount.visible() || content.width == 0 {
				continue;
			}
			mount.ui_mut().resize(content.width);
			let height = mount
				.preferred_height()
				.unwrap_or(mount.ui_mut().frame().size().height);
			let height = height.min(content.height.saturating_sub(*cursor));
			let rect = Rect::new(content.x, *cursor, content.width, height);
			mount.resolve(rect);
			Self::stream(frame, mount, rect);
			*cursor = cursor.saturating_add(height);
		}
	}

	fn stream_stack_up<'a>(
		frame: &mut Frame,
		mounts: impl Iterator<Item = &'a mut Mount>,
		cursor: &mut u16,
		content: Rect,
	) {
		for mount in mounts {
			if !mount.visible() || content.width == 0 {
				continue;
			}
			mount.ui_mut().resize(content.width);
			let height = mount
				.preferred_height()
				.unwrap_or(mount.ui_mut().frame().size().height)
				.min(*cursor);
			*cursor = cursor.saturating_sub(height);
			let rect = Rect::new(content.x, *cursor, content.width, height);
			mount.resolve(rect);
			Self::stream(frame, mount, rect);
		}
	}

	fn stream(frame: &mut Frame, mount: &mut Mount, rect: Rect) {
		for row in 0..rect.height {
			frame.blit(mount.ui_mut().frame(), row, 1, rect.x, rect.y.saturating_add(row));
		}
	}
}

/// Core-owned provenance labels rendered above every extension layer.
///
/// This deliberately lives outside extension markup: `<approval>` and
/// `<attribution>` authored by extensions degrade through `MarkupOrigin`.
pub struct Attribution {
	septet: [Str; 7],
}

impl Attribution {
	/// Creates the reserved attribution band from its seven provenance fields.
	pub const fn new(septet: [Str; 7]) -> Self {
		Self { septet }
	}

	/// Streams the provenance septet into the reserved top z-band.
	pub fn render(&self, frame: &mut Frame, width: u16, theme: Theme) {
		let mut line = String::new();
		for item in &self.septet {
			if item.is_empty() {
				continue;
			}
			if !line.is_empty() {
				line.push_str(" · ");
			}
			line.push_str(item.as_str());
		}
		let _ = draw_line(frame, 0, 0, width, &[Span::new(&line, prose_style(theme))]);
	}
}

/// Result of routing one key through the focused composer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatKey {
	/// The composer handled the key.
	Consumed,
	/// The composer did not handle the key.
	Ignored,
	/// Clear the composer or shut down when repeated within the host window.
	Clear,
	/// Request orderly host shutdown while preserving the current draft.
	Exit,
	/// Ask the backend voice coordinator to start or stop a session.
	ToggleLive,
}

#[derive(Clone, Copy)]
struct Span<'a> {
	text:  &'a str,
	style: Style,
}

impl<'a> Span<'a> {
	const fn new(text: &'a str, style: Style) -> Self {
		Self { text, style }
	}
}
/// Presentation flavor of one transcript prose body.
#[derive(Clone, Copy)]
enum Flavor {
	/// Plain assistant prose.
	Prose,
	/// Reasoning prose rendered dim and italic, matching the live stream.
	Thinking,
	/// User prompt on a padded panel-colored band spanning the transcript
	/// width, matching pi's user-message bubble.
	User,
}

struct RichText {
	text:   String,
	width:  u16,
	flavor: Flavor,
	view:   Option<Ui>,
}

impl RichText {
	fn new(text: impl Into<String>, width: u16, ctx: &UiContext) -> Self {
		Self::styled(text, width, Flavor::Prose, ctx)
	}

	/// Builds a reasoning body rendered dim and italic.
	fn thinking(text: impl Into<String>, width: u16, ctx: &UiContext) -> Self {
		Self::styled(text, width, Flavor::Thinking, ctx)
	}

	/// Builds a user prompt rendered as a panel-filled bubble.
	fn user(text: impl Into<String>, width: u16, ctx: &UiContext) -> Self {
		Self::styled(text, width, Flavor::User, ctx)
	}

	fn styled(text: impl Into<String>, width: u16, flavor: Flavor, ctx: &UiContext) -> Self {
		let text = text.into();
		let view = Self::view(&text, width, flavor, ctx);
		Self { text, width, flavor, view }
	}

	fn view(text: &str, width: u16, flavor: Flavor, ctx: &UiContext) -> Option<Ui> {
		let mut markdown = Markdown::new();
		match flavor {
			Flavor::Prose => {},
			Flavor::Thinking => {
				markdown = markdown.with(Prop::Dim, true).with(Prop::Italic, true);
			},
			Flavor::User => {
				markdown = markdown
					.with_str(Prop::Bg, "panel")
					.with(Prop::PadX, 1_u16)
					.with(Prop::PadY, 1_u16);
			},
		}
		Some(Ui::from_root(markdown.text(Str::new(text)), width.max(1), ctx.clone()))
	}

	fn resize(&mut self, width: u16, ctx: &UiContext) {
		if self.width != width {
			self.width = width;
			self.view = Self::view(&self.text, width, self.flavor, ctx);
		}
	}

	fn height(&self) -> u16 {
		self
			.view
			.as_ref()
			.map_or_else(|| explicit_line_count(&self.text), Ui::height)
	}
}

struct AssistantEntry {
	body: RichText,
}

struct UsageEntry {
	label:          Str,
	elapsed_ms:     Option<u64>,
	visible:        bool,
	show_turn_time: bool,
}

impl UsageEntry {
	fn display_label(&self) -> Str {
		if self.show_turn_time
			&& let Some(elapsed_ms) = self.elapsed_ms
		{
			sf!("{} · Δ{}", self.label, format_turn_elapsed(elapsed_ms))
		} else {
			self.label.clone()
		}
	}
}

impl AssistantEntry {
	fn new(text: String, width: u16, ctx: &UiContext) -> Self {
		Self { body: RichText::new(text, width, ctx) }
	}
}

struct UserEntry {
	body:   RichText,
	chips:  Vec<Str>,
	/// Waiting in the agent queue: rendered dim and kept unretired until the
	/// queue settles or the host restores it to the composer.
	queued: bool,
	/// Pre-rendered dequeue hint shown beside the newest queued row.
	hint:   Option<Str>,
}

/// One persisted tool-result image with its probed pixel dimensions.
#[derive(Clone)]
struct ToolImageEntry {
	source: Str,
	px:     omp_tui::imagefmt::ImageDimensions,
}
/// Host chrome requested by a tool view's root `chrome` attribute.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ViewChrome {
	/// Chat-owned rail card: header line, guide rail, and footer.
	#[default]
	Card,
	/// Self-presenting view: the host draws no card chrome around it.
	Flush,
}

struct ToolView {
	source:   Str,
	width:    u16,
	rendered: Ui,
	/// Chrome the renderer requested on its root element.
	chrome:   ViewChrome,
	/// The source is appended plain output, never markup.
	plain:    bool,
}

impl ToolView {
	fn structured(source: Str, width: u16, ctx: &UiContext) -> Self {
		let rendered = Self::render(&source, width, ctx);
		let chrome = Self::probe_chrome(&rendered);
		Self { source, width, rendered, chrome, plain: false }
	}

	fn plain(source: Str, width: u16, ctx: &UiContext) -> Self {
		let rendered = Ui::from_root(
			TextLeaf::new()
				.with(Prop::Wrap, "char")
				.text(source.clone()),
			width.max(1),
			ctx.clone(),
		);
		Self { source, width, rendered, chrome: ViewChrome::Card, plain: true }
	}

	fn from_content(content: ToolViewContent, width: u16, ctx: &UiContext) -> Self {
		match content {
			ToolViewContent::Markup(source) => Self::structured(source, width, ctx),
			ToolViewContent::Plain(source) => Self::plain(source, width, ctx),
		}
	}

	/// Reads the renderer's `chrome` request from the parsed root element.
	fn probe_chrome(rendered: &Ui) -> ViewChrome {
		match rendered.root_custom("chrome") {
			Some(PropValue::Str(value)) if value == "flush" => ViewChrome::Flush,
			_ => ViewChrome::Card,
		}
	}

	/// The card-embeddable body: the parsed markup tree, or a text leaf for
	/// plain appended output and unparseable markup.
	fn body(&self, ctx: &UiContext) -> Cached {
		if !self.plain
			&& let Ok(root) = parse_with_origin(&self.source, ctx, MarkupOrigin::Core)
		{
			return root;
		}
		Cached::new(Box::new(
			TextLeaf::new()
				.with(Prop::Wrap, "char")
				.text(self.source.clone()),
		))
	}

	fn render(source: &Str, width: u16, ctx: &UiContext) -> Ui {
		Ui::from_markup(source.clone(), width.max(1), ctx.clone()).unwrap_or_else(|_| {
			Ui::from_root(TextLeaf::new().text(source.clone()), width.max(1), ctx.clone())
		})
	}

	fn replace_content(&mut self, content: ToolViewContent, ctx: &UiContext) {
		let replacement = Self::from_content(content, self.width, ctx);
		if self.source != replacement.source || self.plain != replacement.plain {
			*self = replacement;
		}
	}

	fn append_plain(&mut self, chunk: &str, ctx: &UiContext) {
		let mut source = self.source.to_string();
		source.push_str(chunk);
		let source = Str::new(source);
		self.rendered = Ui::from_root(
			TextLeaf::new()
				.with(Prop::Wrap, "char")
				.text(source.clone()),
			self.width.max(1),
			ctx.clone(),
		);
		self.source = source;
		self.chrome = ViewChrome::Card;
		self.plain = true;
	}

	fn resize(&mut self, width: u16, ctx: &UiContext) {
		let width = width.max(1);
		if self.width != width {
			self.width = width;
			self.rendered = if self.plain {
				Ui::from_root(
					TextLeaf::new()
						.with(Prop::Wrap, "char")
						.text(self.source.clone()),
					width,
					ctx.clone(),
				)
			} else {
				Self::render(&self.source, width, ctx)
			};
		}
	}

	const fn height(&self) -> u16 {
		self.rendered.height()
	}
}

struct ToolEntry {
	label:    Str,
	terminal: ToolTerminal,
	expanded: bool,
	view:     ToolView,
	images:   Vec<ToolImageEntry>,
}
fn tool_label(icon: &str, name: &str) -> Str {
	fmts_mut!("{icon} {name}").freeze()
}

struct CompactionEntry {
	label: Str,
}

fn sanitize_thinking_text(text: &str, prose_only: bool) -> Option<String> {
	if text.trim().is_empty() {
		return None;
	}
	let canonical = text.trim();
	if canonical.is_empty()
		|| canonical
			.bytes()
			.all(|byte| matches!(byte, b'.' | b' ' | b'\t' | b'\n' | b'\r' | 0xe2 | 0x80 | 0xa6))
	{
		return None;
	}
	let mut output = Vec::<String>::new();
	let mut fence: Option<(u8, usize)> = None;
	let lines = text.split('\n').collect::<Vec<_>>();
	for (index, line) in lines.iter().enumerate() {
		if let Some((marker, length)) = fence {
			if fence_marker(line).is_some_and(|(candidate, candidate_len, suffix)| {
				candidate == marker && candidate_len >= length && suffix.trim().is_empty()
			}) {
				fence = None;
			}
			if !prose_only {
				output.push((*line).to_owned());
			}
			continue;
		}
		if comment_noise(line, index + 1 == lines.len()) {
			continue;
		}
		if let Some((marker, length, suffix)) = fence_marker(line)
			&& !(marker == b'`' && suffix.contains('`'))
		{
			fence = Some((marker, length));
			if prose_only {
				append_thinking_ellipsis(&mut output);
			} else {
				output.push((*line).to_owned());
			}
			continue;
		}
		output.push((*line).to_owned());
	}
	let formatted = output.join("\n");
	(!formatted.trim().is_empty()).then_some(formatted)
}

fn fence_marker(line: &str) -> Option<(u8, usize, &str)> {
	let indentation = line.bytes().take_while(|byte| *byte == b' ').count();
	if indentation > 3 {
		return None;
	}
	let bytes = line.as_bytes();
	let marker = *bytes.get(indentation)?;
	if !matches!(marker, b'`' | b'~') {
		return None;
	}
	let length = bytes[indentation..]
		.iter()
		.take_while(|byte| **byte == marker)
		.count();
	(length >= 3).then(|| (marker, length, &line[indentation + length..]))
}

fn comment_noise(line: &str, last: bool) -> bool {
	let trimmed = line.trim();
	let empty = trimmed
		.strip_prefix("<!--")
		.and_then(|body| body.strip_suffix("-->"))
		.is_some_and(|body| body.trim().is_empty());
	empty
		|| (last
			&& trimmed
				.strip_prefix("<!--")
				.is_some_and(|body| body.trim().is_empty()))
}

fn append_thinking_ellipsis(lines: &mut Vec<String>) {
	if let Some(last) = lines.iter_mut().rev().find(|line| !line.trim().is_empty()) {
		let trimmed = last.trim_end();
		if trimmed.ends_with("...") {
			last.truncate(trimmed.len());
		} else if trimmed.ends_with('.') {
			last.truncate(trimmed.len() - 1);
			last.push_str("...");
		} else {
			last.truncate(trimmed.len());
			last.push_str("...");
		}
	} else {
		lines.push("...".to_owned());
	}
}

struct LiveAssistant {
	ordinal:     BlockOrdinal,
	id:          Str,
	text:        StrMut,
	revealed:    usize,
	last_reveal: Duration,
	view:        Ui,
	thinking:    bool,
	allocation:  u16,
}

impl LiveAssistant {
	fn new(
		ordinal: BlockOrdinal,
		id: Str,
		width: u16,
		ctx: &UiContext,
		started: Duration,
		thinking: bool,
	) -> Self {
		let mut markdown = Markdown::new()
			.with(Prop::Id, LIVE_ASSISTANT_ID)
			.with(Prop::Partial, true);
		if thinking {
			markdown = markdown.with(Prop::Dim, true).with(Prop::Italic, true);
		}
		let view = Ui::from_root(markdown.text(""), width.max(1), ctx.clone());
		Self {
			ordinal,
			id,
			text: StrMut::new(""),
			revealed: 0,
			last_reveal: started,
			view,
			thinking,
			allocation: 0,
		}
	}

	fn append(&mut self, delta: &str, smooth: bool, now: Duration) -> bool {
		let caught_up = self.revealed == self.text.len();
		let was_empty = self.text.is_empty();
		self.text.push_str(delta);
		if !smooth {
			return self.flush();
		}
		if caught_up {
			self.last_reveal = if was_empty { Duration::ZERO } else { now };
		}
		false
	}

	fn replace(&mut self, text: &str) {
		self.text = StrMut::new(text);
		self.revealed = self.text.len();
		let _ = self.view.set_text(LIVE_ASSISTANT_ID, text);
	}

	fn advance(&mut self, now: Duration, smooth: bool) -> bool {
		if !smooth {
			return self.flush();
		}
		if self.revealed >= self.text.len() {
			return false;
		}
		let frames =
			now.saturating_sub(self.last_reveal).as_millis() / STREAM_REVEAL_FRAME.as_millis();
		if frames == 0 {
			return false;
		}
		let backlog = xutf::graphemes_str(&self.text.as_str()[self.revealed..]).count();
		// Hold the final cluster until a later cluster or an ordering boundary
		// proves its boundary. Provider chunks can split a base character from
		// combining marks or a ZWJ sequence.
		let revealable = if frames >= 2 {
			backlog
		} else {
			backlog.saturating_sub(1)
		};
		if revealable == 0 {
			return false;
		}
		let per_frame = STREAM_REVEAL_MIN
			.max(revealable.div_ceil(8))
			.min(STREAM_REVEAL_MAX);
		let frame_count = usize::try_from(frames).unwrap_or(usize::MAX).min(4);
		self.last_reveal = now;
		self.reveal(per_frame.saturating_mul(frame_count).min(revealable))
	}

	fn reveal(&mut self, count: usize) -> bool {
		let tail = &self.text.as_str()[self.revealed..];
		let bytes = xutf::graphemes_str(tail)
			.take(count)
			.map(str::len)
			.sum::<usize>();
		if bytes == 0 {
			return false;
		}
		self.revealed = self.revealed.saturating_add(bytes).min(self.text.len());
		let _ = self
			.view
			.set_text(LIVE_ASSISTANT_ID, &self.text.as_str()[..self.revealed]);
		true
	}

	fn flush(&mut self) -> bool {
		if self.revealed >= self.text.len() {
			return false;
		}
		self.revealed = self.text.len();
		let _ = self.view.set_text(LIVE_ASSISTANT_ID, self.text.as_str());
		true
	}

	fn next_reveal(&self) -> Option<Duration> {
		let backlog = xutf::graphemes_str(&self.text.as_str()[self.revealed..]).count();
		(backlog > 0).then_some(self.last_reveal.saturating_add(if backlog == 1 {
			STREAM_REVEAL_FRAME.saturating_mul(2)
		} else {
			STREAM_REVEAL_FRAME
		}))
	}

	fn resize(&mut self, width: u16) {
		if self.view.frame().size().width != width.max(1) {
			self.view.resize(width.max(1));
		}
	}

	fn height(&self) -> u16 {
		self.view.height().max(1)
	}
}

struct DownloadActivity {
	received:  Duration,
	completed: Option<Duration>,
	label:     Str,
}

impl DownloadActivity {
	fn new(progress: ModelDownloadProgress, received: Duration) -> Self {
		let completed = progress.complete.then_some(received);
		let label = download_label(&progress);
		Self { received, completed, label }
	}

	fn visible(&self, now: Duration) -> bool {
		now >= self.received.saturating_add(Duration::from_secs(1))
			&& self
				.completed
				.is_none_or(|completed| now < completed.saturating_add(Duration::from_secs(3)))
	}
}
fn retained_expiry(frame: &v1::RetainedFrame, now: Duration) -> Option<Duration> {
	let key = frame.key.as_ref()?;
	if key.kind != "irc" {
		return None;
	}
	let payload = serde_json::from_slice::<serde_json::Value>(&frame.payload).ok()?;
	let ttl = payload.get("ttl_ms")?.as_u64()?.min(300_000);
	Some(now.saturating_add(Duration::from_millis(ttl)))
}

struct LiveTool {
	ordinal:           BlockOrdinal,
	id:                Str,
	name:              Str,
	expanded:          bool,
	view:              ToolView,
	images:            Vec<ToolImageEntry>,
	card_ui:           Ui,
	target_height:     u16,
	target_changed_at: Duration,
	body_folded:       bool,
}

struct RetainedEntry {
	view:       ToolView,
	expires_at: Option<Duration>,
}

struct ThinkingEntry {
	body: RichText,
}

enum Entry {
	User(UserEntry),
	Welcome(WelcomeEntry),
	Assistant(AssistantEntry),
	Usage(UsageEntry),
	Thinking(ThinkingEntry),
	Peer { title: Str, detail: Option<Str> },
	Tool(ToolEntry),
	Compaction(CompactionEntry),
	Retained(RetainedEntry),
	Notice { text: Str, error: bool },
}

fn restyle_entry(entry: &mut Entry, ctx: &UiContext) {
	match entry {
		Entry::User(user) => {
			if let Some(view) = user.body.view.as_mut() {
				let _ = view.set_context(ctx.clone());
			}
		},
		Entry::Assistant(assistant) => {
			if let Some(view) = assistant.body.view.as_mut() {
				let _ = view.set_context(ctx.clone());
			}
		},
		Entry::Usage(_) => {},
		Entry::Thinking(thinking) => {
			if let Some(view) = thinking.body.view.as_mut() {
				let _ = view.set_context(ctx.clone());
			}
		},
		Entry::Peer { .. } => {},
		Entry::Tool(tool) => {
			let _ = tool.view.rendered.set_context(ctx.clone());
		},
		Entry::Retained(frame) => {
			let _ = frame.view.rendered.set_context(ctx.clone());
		},
		Entry::Welcome(_) | Entry::Compaction(_) | Entry::Notice { .. } => {},
	}
}

fn activity_waveform_label(waveform: &ActivityWaveform, charset: Charset) -> Str {
	let glyphs = match charset {
		Charset::Ascii => ['.', ':', '-', '*', '#'],
		Charset::Unicode | Charset::NerdFont => ['▁', '▂', '▄', '▆', '█'],
	};
	let mut label = String::with_capacity(5 + waveform.bands().len().saturating_mul(3));
	label.push_str("live ");
	if waveform.bands().is_empty() {
		label.push(glyphs[0]);
	} else {
		for band in waveform.bands() {
			label.push(glyphs[usize::from(*band).min(glyphs.len() - 1)]);
		}
	}
	label.into()
}

struct StatusLabels {
	model:     Str,
	activity:  Option<Str>,
	git:       Option<Str>,
	context:   Option<(Str, bool)>,
	velocity:  Option<Str>,
	folder:    Option<Str>,
	resources: SmallVec<Str, 5>,
	hooks:     Option<Str>,
	tasks:     Option<Str>,
	collab:    Option<Str>,
	account:   Option<Str>,
	quota:     Option<(Str, u8)>,
	queued:    Option<Str>,
	jobs:      Option<Str>,
	attempt:   Option<Str>,
	dropped:   Option<Str>,
}

impl StatusLabels {
	fn new(facts: &StatusFacts, charset: Charset) -> Self {
		let icon = facts
			.thinking
			.map_or_else(|| charset.icon(Icon::Model), |level| thinking_glyph(charset, level));
		let mut model = fmts_mut!("{icon} {}", facts.model);
		if let Some(advisor) = &facts.advisor_model {
			let _ = write!(model, " {} {advisor}", charset.icon(Icon::Advisor));
		}
		let activity = facts
			.live_activity
			.as_ref()
			.map(|waveform| activity_waveform_label(waveform, charset));
		let git = facts.git.as_ref().map(|git| {
			let mut label = fmts_mut!("{} {}", charset.icon(Icon::Branch), git.branch);
			if git.dirty > 0 {
				let _ = write!(label, " *{}", git.dirty);
			}
			if git.staged > 0 {
				let _ = write!(label, " +{}", git.staged);
			}
			if git.untracked > 0 {
				let _ = write!(label, " ?{}", git.untracked);
			}
			label.freeze()
		});
		let context = (facts.context_tokens > 0 || facts.context_window.is_some()).then(|| {
			let (usage, overflow) = context_usage_label(facts.context_tokens, facts.context_window);
			let mut label = fmts_mut!("{} {usage}", charset.icon(Icon::Context));
			if !matches!(facts.compaction_speculation, CompactionSpeculationStatus::Idle) {
				let _ = write!(label, " {}", charset.icon(Icon::Auto));
			}
			(label.freeze(), overflow)
		});
		let mut labels = Self {
			model: model.freeze(),
			activity,
			git,
			context,
			velocity: facts
				.tokens_per_second
				.map(|rate| fmts_mut!("{rate} tok/s").freeze()),
			folder: facts.cwd.as_ref().map(|cwd| {
				let name = cwd
					.trim_end_matches('/')
					.rsplit('/')
					.next()
					.filter(|name| !name.is_empty())
					.unwrap_or(cwd.as_str());
				fmts_mut!("{} {name}", charset.icon(Icon::Folder)).freeze()
			}),
			resources: facts
				.visible_resources
				.iter()
				.map(|resource| {
					let mut label = fmts_mut!("{} {}", resource.resource, resource.owner);
					if resource.queue_depth > 0 {
						let _ = write!(label, " +{}", resource.queue_depth);
					}
					label.freeze()
				})
				.collect(),
			hooks: (facts.hooks > 0).then(|| fmts_mut!("hooks {}", facts.hooks).freeze()),
			tasks: (facts.tasks > 0).then(|| fmts_mut!("tasks {}", facts.tasks).freeze()),
			collab: (facts.collab_peers > 0)
				.then(|| fmts_mut!("collab {}", facts.collab_peers).freeze()),
			account: facts
				.account_override
				.as_ref()
				.map(|account| fmts_mut!("acct {account}").freeze()),
			quota: facts
				.quota
				.and_then(|quota| quota.daily)
				.map(|window| (crate::status_line::daily_quota_label(window), window.percent)),
			queued: (facts.queued > 0).then(|| fmts_mut!("queued {}", facts.queued).freeze()),
			jobs: (facts.jobs > 0).then(|| fmts_mut!("jobs {}", facts.jobs).freeze()),
			attempt: (facts.attempt > 0).then(|| fmts_mut!("retry {}", facts.attempt).freeze()),
			dropped: (facts.dropped > 0).then(|| fmts_mut!("dropped {}", facts.dropped).freeze()),
		};
		labels.decorate(facts.separator, charset);
		labels
	}

	fn decorate(&mut self, separator: StatusSeparator, charset: Charset) {
		if separator == StatusSeparator::Bracket {
			self.model = bracketed(&self.model);
		}
		for label in [
			&mut self.activity,
			&mut self.velocity,
			&mut self.hooks,
			&mut self.tasks,
			&mut self.collab,
			&mut self.account,
			&mut self.queued,
			&mut self.jobs,
			&mut self.attempt,
			&mut self.dropped,
		] {
			if let Some(text) = label {
				*text = separated(text, separator, charset);
			}
		}
		for label in &mut self.resources {
			*label = separated(label, separator, charset);
		}
		if let Some((text, _)) = &mut self.context {
			*text = separated(text, separator, charset);
		}
		if let Some((text, _)) = &mut self.quota {
			*text = separated(text, separator, charset);
		}
	}
}

fn thinking_glyph(charset: Charset, level: ThinkingLevel) -> &'static str {
	let icon = match level {
		ThinkingLevel::Minimal => Icon::Minimal,
		ThinkingLevel::Low => Icon::Low,
		ThinkingLevel::Medium => Icon::Medium,
		ThinkingLevel::High => Icon::High,
		ThinkingLevel::Xhigh => Icon::Xhigh,
		ThinkingLevel::Max => Icon::Max,
	};
	charset
		.icon(icon)
		.split_whitespace()
		.next()
		.unwrap_or_default()
}

fn bracketed(text: &str) -> Str {
	fmts_mut!("[{text}]").freeze()
}

fn separated(text: &str, separator: StatusSeparator, charset: Charset) -> Str {
	match separator {
		StatusSeparator::Dot => {
			let dot = if charset == Charset::Ascii { "." } else { "·" };
			fmts_mut!("{dot} {text}").freeze()
		},
		StatusSeparator::Bracket => bracketed(text),
	}
}

struct WorkState {
	facts:         StatusFacts,
	labels:        StatusLabels,
	/// Session title shown at the right end of the status row.
	title:         Str,
	elapsed_label: Option<(u64, Str)>,
	active_brand:  StrMut,
	indicator:     Option<WorkingIndicator>,
	fade:          Tween<Color>,
}

impl WorkState {
	fn update_active_brand(&mut self, now: Duration, charset: Charset) {
		if !self.facts.working {
			return;
		}
		let elapsed = self
			.facts
			.turn_started
			.map_or(Duration::ZERO, |started| Instant::now().saturating_duration_since(started));
		let key = elapsed_label_key(elapsed);
		if self
			.elapsed_label
			.as_ref()
			.is_none_or(|(cached, _)| *cached != key)
		{
			self.elapsed_label = Some((key, elapsed_label(elapsed)));
		}
		self.active_brand.truncate(0);
		if let Some(indicator) = &self.indicator {
			if indicator.frames.is_empty() {
				return;
			}
			let interval = Duration::from_millis(indicator.interval_ms.max(1));
			let index = usize::try_from(now.as_millis() / interval.as_millis()).unwrap_or(0)
				% indicator.frames.len();
			self.active_brand.push_str(&indicator.frames[index]);
		} else {
			self.active_brand.push_str(charset.spinner().at(now));
		}
		self.active_brand.push(' ');
		if let Some((_, label)) = &self.elapsed_label {
			self.active_brand.push_str(label);
		}
	}
}

struct ChatStatus {
	props:      Props,
	slot:       Slot,
	work:       Rc<RefCell<WorkState>>,
	idle_brand: Str,
	charset:    Charset,
	theme:      Theme,
	style:      ComposerStyle,
}

fn context_gauge_min_width(facts: &StatusFacts) -> u16 {
	let Some(window) = facts.context_window.filter(|window| *window > 0) else {
		return 1;
	};
	let (label, _) = context_usage_label(facts.context_tokens, Some(window));
	let Some((percent, window)) = label.split_once('/') else {
		return 1;
	};
	visible_width(percent)
		.saturating_add(visible_width(window))
		.saturating_add(4)
}

fn fit_status_group_widths(budget: u16, left: u16, right: u16) -> (u16, u16) {
	if left == 0 {
		return (0, right.min(budget));
	}
	if right == 0 {
		return (left.min(budget), 0);
	}
	if budget < 2 {
		return (budget.min(left), 0);
	}
	let mut fitted_left = left.min(budget.div_ceil(2)).max(1);
	let mut fitted_right = right.min(budget.saturating_sub(fitted_left)).max(1);
	let mut spare = budget.saturating_sub(fitted_left.saturating_add(fitted_right));
	let left_extra = left.saturating_sub(fitted_left).min(spare);
	fitted_left = fitted_left.saturating_add(left_extra);
	spare = spare.saturating_sub(left_extra);
	fitted_right = fitted_right.saturating_add(right.saturating_sub(fitted_right).min(spare));
	(fitted_left, fitted_right)
}

impl ChatStatus {
	fn new(
		work: Rc<RefCell<WorkState>>,
		charset: Charset,
		theme: Theme,
		style: ComposerStyle,
	) -> Self {
		let mut props = Props::new();
		props.set(Prop::Id, STATUS_ID);
		props.set(Prop::NoSelect, true);
		let idle_brand = fmts_mut!("{} omp", charset.icon(Icon::Omp)).freeze();
		Self { props, slot: next_slot(), work, idle_brand, charset, theme, style }
	}

	const fn set_composer_style(&mut self, style: ComposerStyle) {
		self.style = style;
	}

	const fn set_theme(&mut self, theme: Theme) {
		self.theme = theme;
	}

	fn group(&self) -> Status {
		Status::new()
			.with(Prop::Bg, self.theme.panel)
			.with(Prop::Fg, self.theme.fg)
	}

	fn brand_segment(&self, now: Duration) -> Segment {
		let work = self.work.borrow();
		let label = if work.facts.working {
			work.active_brand.clone().freeze()
		} else {
			self.idle_brand.clone()
		};
		Segment::new()
			.label(label)
			.with(Prop::Fg, work.fade.sample(now))
	}

	fn left_group(&self, now: Duration) -> Status {
		let work = self.work.borrow();
		let facts = &work.facts;
		let minimal = facts.layout == StatusLayout::Minimal;
		let model = work.labels.model.clone();
		let folder = work.labels.folder.clone();
		let git = work.labels.git.clone();
		let spend = spend_label(facts.cost_nanos, facts.model_subscription, self.charset);
		let advisor_spend =
			advisor_spend_label(facts.advisor_cost_nanos, facts.advisor_subscription, self.charset);
		drop(work);
		let mut status = self
			.group()
			.segment(self.brand_segment(now))
			.segment(Segment::new().label(model).with(Prop::Fg, self.theme.ok));
		if minimal {
			return status;
		}
		if let Some(folder) = folder {
			status = status.segment(
				Segment::new()
					.label(folder)
					.with(Prop::Fg, self.theme.secondary),
			);
		}
		if let Some(git) = git {
			status = status.segment(Segment::new().label(git).with(Prop::Fg, self.theme.info));
		}
		if !spend.is_empty() {
			status = status.segment(
				Segment::new()
					.label(spend)
					.with(Prop::Fg, self.theme.secondary),
			);
		}
		if !advisor_spend.is_empty() {
			status = status.segment(
				Segment::new()
					.label(advisor_spend)
					.with(Prop::Fg, self.theme.secondary),
			);
		}
		status
	}

	fn right_group(&self, context_gauge: ContextGaugeMode, now: Duration) -> Status {
		let work = self.work.borrow();
		let facts = &work.facts;
		let mut status = self.group().with_str(Prop::Align, "right");
		if matches!(facts.layout, StatusLayout::Full | StatusLayout::Developer)
			&& let Some(velocity) = &work.labels.velocity
		{
			status = status.segment(
				Segment::new()
					.label(velocity.clone())
					.with(Prop::Fg, self.theme.accent),
			);
		}
		if let Some(activity) = &work.labels.activity {
			if facts.layout != StatusLayout::Minimal {
				status = status.segment(
					Segment::new()
						.label(activity.clone())
						.with(Prop::Fg, self.theme.accent),
				);
			}
		}
		if facts.layout != StatusLayout::Minimal {
			for label in &work.labels.resources {
				status = status.segment(
					Segment::new()
						.label(label.clone())
						.with(Prop::Fg, self.theme.accent),
				);
			}
		}
		if matches!(facts.layout, StatusLayout::Full | StatusLayout::Compact)
			&& let Some(tasks) = &work.labels.tasks
		{
			status = status.segment(
				Segment::new()
					.label(tasks.clone())
					.with(Prop::Fg, self.theme.warn),
			);
		}
		if facts.layout == StatusLayout::Full {
			for label in [&work.labels.hooks, &work.labels.collab, &work.labels.account]
				.into_iter()
				.flatten()
			{
				status = status.segment(
					Segment::new()
						.label(label.clone())
						.with(Prop::Fg, self.theme.secondary),
				);
			}
		}
		if matches!(context_gauge, ContextGaugeMode::Numeric)
			&& (facts.context_tokens > 0 || facts.context_window.is_some())
		{
			let Some((label, overflow)) = &work.labels.context else {
				unreachable!("visible numeric context has a cached label")
			};
			let color = if *overflow {
				self.theme.err
			} else {
				compaction_threshold_color(&self.theme)
			};
			let speculation_color = match facts.compaction_speculation {
				CompactionSpeculationStatus::Idle => None,
				CompactionSpeculationStatus::Running => {
					let phase = (now.as_millis() / SPECULATION_PULSE.as_millis()).is_multiple_of(2);
					Some(if phase {
						self.theme.accent
					} else {
						self.theme.muted
					})
				},
				CompactionSpeculationStatus::Armed => Some(self.theme.accent),
			};
			status = status.segment(
				Segment::new()
					.label(label.clone())
					.with(Prop::Fg, speculation_color.unwrap_or(color)),
			);
		}
		if facts.layout != StatusLayout::Minimal {
			if let Some((quota, percent)) = &work.labels.quota {
				let color = if *percent >= 80 {
					self.theme.err
				} else if *percent >= 50 {
					self.theme.warn
				} else {
					self.theme.muted
				};
				status = status.segment(Segment::new().label(quota.clone()).with(Prop::Fg, color));
			}
			if let Some(queued) = &work.labels.queued {
				status = status.segment(
					Segment::new()
						.label(queued.clone())
						.with(Prop::Fg, self.theme.warn),
				);
			}
			if let Some(jobs) = &work.labels.jobs {
				status = status.segment(
					Segment::new()
						.label(jobs.clone())
						.with(Prop::Fg, self.theme.info),
				);
			}
			if let Some(attempt) = &work.labels.attempt {
				status = status.segment(
					Segment::new()
						.label(attempt.clone())
						.with(Prop::Fg, self.theme.warn),
				);
			}
			if let Some(dropped) = &work.labels.dropped {
				status = status.segment(
					Segment::new()
						.label(dropped.clone())
						.with(Prop::Fg, self.theme.err),
				);
			}
		}
		if facts.layout != StatusLayout::Minimal && !work.title.is_empty() {
			status = status.segment(
				Segment::new()
					.label(work.title.clone())
					.with(Prop::Fg, self.theme.accent),
			);
		}
		status
	}

	fn has_more(&self) -> bool {
		let work = self.work.borrow();
		let facts = &work.facts;
		if facts.layout == StatusLayout::Minimal {
			return facts.context_tokens > 0 || facts.context_window.is_some();
		}
		facts.live_activity.is_some()
			|| facts.tokens_per_second.is_some()
			|| !facts.visible_resources.is_empty()
			|| facts.hooks > 0
			|| facts.tasks > 0
			|| facts.collab_peers > 0
			|| facts.account_override.is_some()
			|| facts.quota.and_then(|quota| quota.daily).is_some()
			|| facts.context_tokens > 0
			|| !work.title.is_empty()
	}

	fn paint_left(&self, pc: &mut PaintCtx<'_>, rect: Rect) {
		let mut left = self.left_group(pc.now);
		let (_, width) = left.measure(pc.ctx);
		left.paint(pc, Rect::new(rect.x, rect.y, width.min(rect.width), 1));
	}

	fn paint_right(&self, pc: &mut PaintCtx<'_>, rect: Rect, gauge: ContextGaugeMode) {
		let mut right = self.right_group(gauge, pc.now);
		let (_, width) = right.measure(pc.ctx);
		let width = width.min(rect.width);
		let x = rect.x.saturating_add(rect.width.saturating_sub(width));
		right.paint(pc, Rect::new(x, rect.y, width, 1));
	}

	fn paint_full(&self, pc: &mut PaintCtx<'_>, rect: Rect, gauge: ContextGaugeMode) {
		let mut left = self.left_group(pc.now);
		let mut right = self.right_group(gauge, pc.now);
		let (_, natural_left) = left.measure(pc.ctx);
		let (_, natural_right) = right.measure(pc.ctx);
		let (left_width, right_width, minimum_boundary) = if matches!(gauge, ContextGaugeMode::Bar) {
			let facts = &self.work.borrow().facts;
			let desired = context_gauge_min_width(facts);
			let surviving_group = u16::from(natural_left > 0 || natural_right > 0);
			let minimum = if desired.saturating_add(surviving_group) <= rect.width {
				desired
			} else {
				1
			};
			let (left, right) = fit_status_group_widths(
				rect.width.saturating_sub(minimum),
				natural_left,
				natural_right,
			);
			(left, right, minimum)
		} else {
			(natural_left, natural_right, 2)
		};
		if let Some(layout) =
			boundary_layout(rect.x, rect.width, left_width, right_width, minimum_boundary)
		{
			left.paint(pc, Rect::new(layout.left_x, rect.y, left_width, 1));
			if matches!(gauge, ContextGaugeMode::Bar) {
				let facts = &self.work.borrow().facts;
				let plan = ContextGauge::plan(
					layout.boundary_width,
					facts.context_tokens,
					facts.context_window,
					facts.compaction_boundaries,
				);
				let (_, _, _, _, horizontal, _) = pc.ctx.charset.border(Border::Round);
				let mut bytes = [0_u8; 4];
				let line = horizontal.encode_utf8(&mut bytes);
				let accent = compaction_threshold_color(&self.theme);
				let boundary = compaction_boundary_color(&self.theme);
				for offset in 0..layout.boundary_width {
					let x = layout.boundary_x.saturating_add(offset);
					let (glyph, color): (&str, Color) = match plan.cell(offset) {
						GaugeCell::Used => (line, accent),
						GaugeCell::Unused => (line, self.theme.border),
						GaugeCell::Threshold => (pc.ctx.charset.icon(Icon::ContextCompaction), boundary),
						GaugeCell::Speculation => {
							(pc.ctx.charset.icon(Icon::ContextSpeculation), self.theme.muted)
						},
						GaugeCell::Percent(cell) => {
							let color = if plan.overflowed() {
								self.theme.err
							} else {
								accent
							};
							(cell, color)
						},
						GaugeCell::Window(cell) => (cell, boundary),
					};
					pc.frame.put(x, rect.y, glyph, Style::new().fg(color));
				}
			}
			right.paint(pc, Rect::new(layout.right_x, rect.y, right_width, 1));
		} else {
			let mut combined = self.left_group(pc.now);
			if self.has_more() {
				combined = combined.segment(Segment::new().label("…").with(Prop::Fg, self.theme.muted));
			}
			combined.paint(pc, rect);
		}
	}
}

impl Component for ChatStatus {
	fn props(&self) -> &Props {
		&self.props
	}

	fn props_mut(&mut self) -> &mut Props {
		&mut self.props
	}

	fn slot(&self) -> Slot {
		self.slot
	}

	fn measure(&mut self, ctx: &UiContext) -> (u16, u16) {
		let mut left = self.left_group(Duration::ZERO);
		left.measure(ctx)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		1
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.width == 0 || rect.height == 0 {
			return;
		}
		self
			.work
			.borrow_mut()
			.update_active_brand(pc.now, self.charset);
		let layout = self.style.layout(self.charset);
		match layout.status_attachment {
			ComposerStatusAttachment::TopBorder => {
				self.paint_full(
					pc,
					Rect::new(rect.x.saturating_add(1), rect.y, rect.width.saturating_sub(2), 1),
					layout.context_gauge,
				);
			},
			ComposerStatusAttachment::TopRuleChip => {
				self.paint_right(
					pc,
					Rect::new(rect.x, rect.y, rect.width.saturating_sub(1), 1),
					ContextGaugeMode::Numeric,
				);
				self.paint_left(
					pc,
					Rect::new(
						rect.x,
						rect.y.saturating_add(rect.height.saturating_sub(1)),
						rect.width,
						1,
					),
				);
			},
			ComposerStatusAttachment::Standalone => {
				self.paint_full(
					pc,
					Rect::new(
						rect.x,
						rect.y.saturating_add(rect.height.saturating_sub(1)),
						rect.width,
						1,
					),
					layout.context_gauge,
				);
			},
		}
		let work = self.work.borrow();
		let fade_frame = work
			.fade
			.settles_at()
			.min(pc.now.saturating_add(FADE_FRAME));
		let animation_deadline = match (work.facts.working, work.fade.is_settled(pc.now)) {
			(true, true) => Some(work.indicator.as_ref().map_or_else(
				|| pc.ctx.charset.spinner().next_change(pc.now),
				|indicator| {
					pc.now
						.saturating_add(Duration::from_millis(indicator.interval_ms.max(1)))
				},
			)),
			(true, false) => Some(
				work
					.indicator
					.as_ref()
					.map_or_else(
						|| pc.ctx.charset.spinner().next_change(pc.now),
						|indicator| {
							pc.now
								.saturating_add(Duration::from_millis(indicator.interval_ms.max(1)))
						},
					)
					.min(fade_frame),
			),
			(false, false) => Some(fade_frame),
			(false, true) => None,
		};
		let speculation_deadline =
			matches!(work.facts.compaction_speculation, CompactionSpeculationStatus::Running)
				.then(|| pc.now.saturating_add(SPECULATION_PULSE));
		if let Some(at) = match (animation_deadline, speculation_deadline) {
			(Some(animation), Some(speculation)) => Some(animation.min(speculation)),
			(Some(animation), None) => Some(animation),
			(None, speculation) => speculation,
		} {
			pc.wake(self.slot, at);
		}
	}

	fn paints_background(&self) -> bool {
		false
	}
}

/// Per-frame chrome row budget shared by rendering and retirement pressure.
#[derive(Clone, Copy, Debug, Default)]
struct ChromeLayout {
	editor_height: u16,
	editor_y:      u16,
	title_y:       u16,
	working_y:     u16,
	error_rows:    u16,
	download_rows: u16,
	/// Rows reserved for the sticky todo HUD.
	todo_rows:     u16,
	/// Rows available to the live transcript region.
	h_live:        u16,
}

/// Platform-default dequeue chord, mirroring the app's keybinding fallback;
/// hosts with a resolved binding override it via [`Chat::set_dequeue_hint`].
const DEQUEUE_HINT_DEFAULT: &str = if cfg!(target_os = "macos") {
	"shift+up"
} else {
	"ctrl+up"
};

/// Immediate-mode designed chat scene driven entirely by host data.
pub struct Chat {
	started_at:              Instant,
	ctx:                     UiContext,
	editor_ui:               Ui,
	slash_commands:          ReloadableSlashCommands,
	attachments:             Attachments,
	pending_submit:          VecDeque<(String, Vec<Attachment>, SubmitMode)>,
	copied:                  Option<Str>,
	work:                    Rc<RefCell<WorkState>>,
	blocks:                  Blocks,
	replay:                  Option<Replay>,
	/// Committed transcript tail resident in the viewport's leading rows.
	///
	/// A settled-resize replay leaves these rows on screen instead of
	/// scrolling all of them into native history, keeping the viewport
	/// contiguous with the transcript. They repaint every frame and retire
	/// ahead of any newer commit; a width change drops them and the next
	/// settled replay re-derives the split from `entries`.
	band:                    Option<Frame>,
	/// Finalized semantic snapshots by ordinal. Snapshots at or past the
	/// commit frontier are settled (viewport-resident); earlier ones are
	/// retained so one buffered display replay can re-render them at a new
	/// width.
	entries:                 BTreeMap<BlockOrdinal, Entry>,
	last_viewport:           Size,
	last_editor_height:      u16,
	frame:                   Frame,
	clip_scratch:            Frame,
	live_assistant:          Option<LiveAssistant>,
	live_layout:             SmallVec<(BlockOrdinal, u16, u16), 8>,
	live_tools:              Vec<LiveTool>,
	live_revision:           u64,
	drawn_live:              u64,
	host_right_inset:        u16,
	slot_right_inset:        u16,
	layout_width:            u16,
	slots:                   Slots,
	agents:                  Vec<AgentRow>,
	agent_labels:            Vec<Str>,
	todo_lines:              Vec<Str>,
	todo_collapsed_lines:    Vec<Str>,
	todo_expanded:           bool,
	retained_frames:         RetainedFrames,
	pinned_error:            Option<Str>,
	idle_recap:              Option<Str>,
	download_activity:       Option<DownloadActivity>,
	celebration_until:       Option<Duration>,
	attribution:             Option<Attribution>,
	keyword_accent:          KeywordAccent,
	live_voice:              Option<LiveVoiceVisualizer>,
	live_voice_action:       Option<LiveVoiceAction>,
	reduced_motion:          bool,
	smooth_streaming:        bool,
	show_token_usage:        bool,
	show_turn_time:          bool,
	turn_started_at_ms:      Option<u64>,
	last_completed_at_ms:    Option<u64>,
	suppress_history_replay: bool,
	hide_thinking:           bool,
	hide_tools:              bool,
	/// Dequeue chord rendered in pending queued-row hints.
	dequeue_hint:            Str,
	hidden_thinking_label:   Option<Str>,
	tools_expanded:          bool,
}

impl Chat {
	/// Creates an empty scene using the host's detected presentation context.
	pub fn new(ctx: &UiContext) -> Self {
		let facts = StatusFacts::default();
		let labels = StatusLabels::new(&facts, ctx.charset);
		let work = Rc::new(RefCell::new(WorkState {
			facts,
			labels,
			title: Str::default(),
			elapsed_label: None,
			active_brand: StrMut::new(""),
			indicator: None,
			fade: Tween::settled(ctx.theme.muted),
		}));
		let style = ComposerStyle::default();
		let slash_commands = ReloadableSlashCommands::new(Vec::new(), |_| 0);
		let mut pane = EditorPane::new()
			.composer_style(style)
			.completion(Box::new(slash_commands.clone()))
			.with(Prop::Id, INPUT_ID)
			.with(Prop::Submit, true)
			.status(ChatStatus::new(Rc::clone(&work), ctx.charset, ctx.theme, style));
		let attachments = pane.attachments();
		pane.set_inline_decorator(Some(Box::new(|text| {
			crate::queue::decoration_spans(text)
				.into_iter()
				.map(|(start, end, span)| {
					(start, end, match span {
						crate::queue::QueueSpan::Prefix => InlineAccent::Dim,
						crate::queue::QueueSpan::Marker => InlineAccent::Accent,
					})
				})
				.collect()
		})));
		let mut editor_ui = Ui::from_root(pane, 0, ctx.clone());
		editor_ui.focus_first();
		Self {
			started_at: Instant::now(),
			ctx: ctx.clone(),
			editor_ui,
			slash_commands,
			attachments,
			pending_submit: VecDeque::new(),
			copied: None,
			work,
			blocks: Blocks::new(),
			replay: None,
			band: None,
			entries: BTreeMap::new(),
			last_viewport: Size::new(0, 0),
			last_editor_height: 0,
			frame: Frame::new(Size::new(0, 0)),
			clip_scratch: Frame::new(Size::new(0, 0)),
			live_assistant: None,
			live_layout: SmallVec::new(),
			live_tools: Vec::new(),
			live_revision: 0,
			drawn_live: 0,
			host_right_inset: 0,
			slot_right_inset: 0,
			layout_width: 0,
			agents: Vec::new(),
			agent_labels: Vec::new(),
			todo_lines: Vec::new(),
			todo_collapsed_lines: Vec::new(),
			todo_expanded: false,
			retained_frames: RetainedFrames::new(),
			pinned_error: None,
			idle_recap: None,
			download_activity: None,
			celebration_until: None,
			slots: Slots::new(ctx.clone()),
			attribution: None,
			keyword_accent: KeywordAccent::default(),
			live_voice: None,
			live_voice_action: None,
			reduced_motion: false,
			smooth_streaming: true,
			show_token_usage: false,
			show_turn_time: false,
			turn_started_at_ms: None,
			last_completed_at_ms: None,
			suppress_history_replay: false,
			hide_thinking: false,
			hide_tools: false,
			hidden_thinking_label: None,
			tools_expanded: true,
			dequeue_hint: Str::new_static(DEQUEUE_HINT_DEFAULT),
		}
	}

	/// Hides provider thinking blocks from this scene without changing the
	/// underlying transcript.
	pub fn set_hide_thinking(&mut self, hide_thinking: bool) {
		self.hide_thinking = hide_thinking;
		self.bump_live();
	}

	/// Applies one non-visible prompt/run boundary to the elapsed-time anchor.
	pub fn apply_turn_anchor(&mut self, anchor: TurnAnchor) {
		match anchor {
			TurnAnchor::AgentStart => {
				if self
					.turn_started_at_ms
					.zip(self.last_completed_at_ms)
					.is_some_and(|(started, completed)| started < completed)
				{
					self.turn_started_at_ms = None;
				}
			},
			TurnAnchor::User { submitted_at_ms } => {
				if submitted_at_ms > 0 {
					self.turn_started_at_ms = Some(submitted_at_ms);
				}
			},
			TurnAnchor::Developer { submitted_at_ms, synthetic, user_initiated } => {
				if synthetic {
					self.turn_started_at_ms = if user_initiated && submitted_at_ms > 0 {
						Some(submitted_at_ms)
					} else {
						None
					};
				}
			},
		}
	}

	/// Appends one final usage row and derives elapsed time from local clocks.
	pub fn push_assistant_usage(&mut self, row: AssistantUsage) {
		let elapsed_ms = self
			.turn_started_at_ms
			.and_then(|started| row.completed_at_ms.checked_sub(started))
			.filter(|elapsed| *elapsed > 0);
		self.last_completed_at_ms = Some(row.completed_at_ms);
		self.enqueue_final(Entry::Usage(UsageEntry {
			label: format_usage_label(&row.usage),
			elapsed_ms,
			visible: self.show_token_usage,
			show_turn_time: self.show_turn_time,
		}));
		self.bump_live();
	}

	/// Shows or hides transcript usage rows and rebuilds committed scrollback.
	pub fn set_show_token_usage(&mut self, show: bool) {
		if self.show_token_usage == show {
			return;
		}
		self.show_token_usage = show;
		for entry in self.entries.values_mut() {
			if let Entry::Usage(usage) = entry {
				usage.visible = show;
			}
		}
		self.rebuild_usage_display();
	}

	/// Shows or hides prompt-to-yield deltas and rebuilds committed scrollback.
	pub fn set_show_turn_time(&mut self, show: bool) {
		if self.show_turn_time == show {
			return;
		}
		self.show_turn_time = show;
		for entry in self.entries.values_mut() {
			if let Entry::Usage(usage) = entry {
				usage.show_turn_time = show;
			}
		}
		self.rebuild_usage_display();
	}

	fn rebuild_usage_display(&mut self) {
		if self.blocks.frontier() > 0 {
			self.begin_history_replay(HistoryReplay::Rebuild);
		}
		self.bump_live();
	}

	/// Replaces the placeholder label associated with hidden reasoning.
	pub fn set_hidden_thinking_label(&mut self, label: Option<Str>) {
		self.hidden_thinking_label = label.clone();
		if self.hide_thinking
			&& let Some(label) = label
		{
			let width = Self::message_width(self.layout_width);
			for entry in self.entries.values_mut() {
				if let Entry::Thinking(thinking) = entry {
					thinking.body = RichText::thinking(label.to_string(), width, &self.ctx);
				}
			}
		}
		self.bump_live();
	}

	/// Returns the global transcript tool-card disclosure state.
	pub const fn tools_expanded(&self) -> bool {
		self.tools_expanded
	}

	/// Applies one disclosure state to every current and future tool card.
	pub fn set_tools_expanded(&mut self, expanded: bool) {
		self.tools_expanded = expanded;
		for tool in &mut self.live_tools {
			tool.expanded = expanded;
		}
		for entry in self.entries.values_mut() {
			if let Entry::Tool(tool) = entry {
				tool.expanded = expanded;
			}
		}
		self.bump_live();
	}

	/// Replaces the editor's live spelling feature policy.
	pub fn set_spelling_features(&mut self, features: SpellingFeatures) {
		self
			.editor_ui
			.update_component::<EditorPane>(INPUT_ID, |pane| {
				pane.set_spelling_features(features);
				true
			});
	}

	/// Switches the built-in composer chrome and its status attachment.
	pub fn set_composer_style(&mut self, style: ComposerStyle) {
		self
			.editor_ui
			.update_component::<EditorPane>(INPUT_ID, |pane| {
				pane.set_composer_style(style);
				true
			});
		self
			.editor_ui
			.update_component::<ChatStatus>(STATUS_ID, |status| {
				status.set_composer_style(style);
				true
			});
		self.refresh_composer();
	}

	/// Replaces the prompt-policy keyword data used by editor accent and replay
	/// masking.
	pub fn set_keyword_accent(&mut self, accent: KeywordAccent) {
		self.keyword_accent = accent.clone();
		self
			.editor_ui
			.update_component::<EditorPane>(INPUT_ID, |pane| {
				pane.set_keyword_accent(accent);
				true
			});
	}

	/// Borrows retained extension slots for composition or headless inspection.
	pub const fn slots_mut(&mut self) -> &mut Slots {
		&mut self.slots
	}

	/// Applies an extension UI effect synchronously and repaints its retained
	/// slot surface on the next frame.
	pub fn apply_ui_effect(&mut self, effect: &v1::UiEffect) -> Damage {
		let damage = self.slots.apply(effect);
		if !damage.is_empty() {
			self.bump_live();
		}
		damage
	}

	/// Sets the core-owned provenance septet shown above extension layers.
	pub fn set_attribution(&mut self, septet: [Str; 7]) {
		self.attribution = Some(Attribution::new(septet));
		self.bump_live();
	}

	/// Starts realtime voice composer takeover.
	pub fn start_live_voice(&mut self) {
		self.live_voice = Some(LiveVoiceVisualizer::default());
		self.live_voice_action = None;
		self.bump_live();
	}

	/// Restores the ordinary composer after realtime voice teardown.
	pub fn stop_live_voice(&mut self) {
		self.live_voice = None;
		self.live_voice_action = None;
		self.refresh_composer();
		self.bump_live();
	}

	/// Mutably borrows the active visualizer for provider event projection.
	pub fn live_voice_mut(&mut self) -> Option<&mut LiveVoiceVisualizer> {
		self.bump_live();
		self.live_voice.as_mut()
	}

	/// Whether realtime voice currently owns composer input.
	pub const fn live_voice_active(&self) -> bool {
		self.live_voice.is_some()
	}

	/// Takes the most recent mute/close action.
	pub const fn take_live_voice_action(&mut self) -> Option<LiveVoiceAction> {
		self.live_voice_action.take()
	}

	/// Routes a key through the composer.
	pub fn handle_key(&mut self, key: Key) -> ChatKey {
		self.clear_idle_recap();
		if key == Key::Ctrl('l') {
			return ChatKey::ToggleLive;
		}
		if let Some(visualizer) = self.live_voice.as_mut() {
			self.live_voice_action = match key {
				Key::Char(' ') => Some(visualizer.toggle_mute()),
				Key::Esc | Key::Ctrl('c') => {
					visualizer.set_phase(LiveVoicePhase::Closing);
					Some(LiveVoiceAction::Close)
				},
				_ => None,
			};
			self.bump_live();
			return ChatKey::Consumed;
		}
		if key == Key::Ctrl('o') {
			let _ = self.toggle_latest_tool();
			return ChatKey::Consumed;
		}
		if key == Key::Ctrl('t') {
			self.toggle_thinking();
			return ChatKey::Consumed;
		}
		if key == Key::ToggleToolVisibility {
			self.toggle_tool_visibility();
			return ChatKey::Consumed;
		}
		if key == Key::CopyPrompt {
			self.copied = self.entries.values().rev().find_map(|entry| match entry {
				Entry::User(user) => Some(Str::new(user.body.text.as_str())),
				_ => None,
			});
			return ChatKey::Consumed;
		}
		if key == Key::CopyLine {
			let mut line = None;
			self
				.editor_ui
				.update_component::<EditorPane>(INPUT_ID, |pane| {
					line = Some(Str::new(pane.current_line()));
					false
				});
			self.copied = line.filter(|line| !line.is_empty());
			return ChatKey::Consumed;
		}
		if key == Key::Enter && self.composer_empty() && self.is_working() {
			self
				.pending_submit
				.push_back((String::new(), Vec::new(), SubmitMode::Steer));
			return ChatKey::Consumed;
		}
		if key == Key::FollowUp {
			self.stage_submission(SubmitMode::FollowUp);
			return ChatKey::Consumed;
		}
		if key == Key::Ctrl('c') {
			return ChatKey::Clear;
		}
		if key == Key::Ctrl('d') {
			return ChatKey::Exit;
		}
		match self.editor_ui.handle_key(key) {
			UiEvent::Submit => {
				self.stage_submission(SubmitMode::Steer);
				ChatKey::Consumed
			},
			UiEvent::Copied(text) => {
				self.copied = Some(text);
				ChatKey::Consumed
			},
			UiEvent::None if key == Key::Esc => ChatKey::Ignored,
			UiEvent::None => ChatKey::Consumed,
			_ => ChatKey::Consumed,
		}
	}

	/// Stages the composer's non-empty text as a pending submission and
	/// clears the input; staged attachments ride along unless the text's
	/// slash command preserves them.
	fn stage_submission(&mut self, mode: SubmitMode) {
		let text = self.composer_text();
		if text.trim().is_empty() {
			return;
		}
		let mut attachments = if preserves_attachments(&text) {
			Vec::new()
		} else {
			self.attachments.take()
		};
		let items = if text.trim_start().starts_with('/') {
			vec![crate::queue::QueueItem {
				text:             Str::new(text.as_str()),
				yield_after_turn: false,
			}]
		} else {
			queue::split(&text)
		};
		for (index, item) in items.into_iter().enumerate() {
			let item_mode = if index > 0 || item.yield_after_turn {
				SubmitMode::FollowUp
			} else {
				mode
			};
			let item_attachments = if index == 0 {
				mem::take(&mut attachments)
			} else {
				Vec::new()
			};
			self
				.pending_submit
				.push_back((item.text.to_string(), item_attachments, item_mode));
		}
		self.editor_ui.set_text(INPUT_ID, "");
		self.refresh_composer();
	}

	/// Routes sanitized bracketed-paste text through the composer.
	pub fn handle_paste(&mut self, text: &str) {
		self.clear_idle_recap();
		let _ = self.editor_ui.handle_paste(text);
		self.refresh_composer();
	}

	/// Routes clipboard text verbatim, bypassing attachment staging.
	pub fn handle_paste_raw(&mut self, text: &str) {
		self.clear_idle_recap();
		let _ = self.editor_ui.handle_paste_raw(text);
		self.refresh_composer();
	}

	/// Routes a viewport-space mouse report into the composer.
	pub fn handle_mouse(&mut self, report: &MouseReport) {
		if report.kind == omp_tui::Mouse::Click {
			let hit = self
				.live_layout
				.iter()
				.find(|(_, top, height)| report.row == *top && *height > 0)
				.map(|(ordinal, ..)| *ordinal);
			if let Some(ordinal) = hit
				&& self.toggle_tool_ordinal(ordinal).is_some()
			{
				return;
			}
		}
		let rows = self.composer_rows();
		let y = self.frame.size().height.saturating_sub(rows);
		if report.row >= y && report.row < y.saturating_add(rows) {
			self.clear_idle_recap();
			let _ = self
				.editor_ui
				.handle_mouse(report.col, report.row - y, report.kind);
		}
	}

	/// Takes text copied or cut by the composer.
	pub const fn take_copied(&mut self) -> Option<Str> {
		self.copied.take()
	}

	/// Takes the next composer submission: its text, staged attachments,
	/// and active-turn delivery mode.
	pub fn take_submission(&mut self) -> Option<(String, Vec<Attachment>, SubmitMode)> {
		self.pending_submit.pop_front()
	}

	/// Stages the current composer contents as one immediate submission.
	///
	/// This follows the same queue splitting and attachment extraction path as
	/// pressing Enter, and is intended for hosts with an initial message.
	pub fn submit_composer(&mut self) {
		self.stage_submission(SubmitMode::Steer);
	}

	/// Clones the staged attachment descriptors for read-only overlays.
	pub fn composer_attachments(&self) -> Vec<Attachment> {
		self.attachments.snapshot()
	}

	/// Returns whether the composer contains no non-whitespace text.
	pub fn composer_empty(&self) -> bool {
		self.composer_text().trim().is_empty()
	}

	/// Clears composer text and staged attachments.
	pub fn clear_composer(&mut self) {
		self.clear_idle_recap();
		self.editor_ui.set_text(INPUT_ID, "");
		let _ = self.attachments.take();
		self.refresh_composer();
	}

	/// Replaces composer text, preserving staged attachments.
	pub fn set_composer_text(&mut self, text: &str) {
		self.clear_idle_recap();
		self.editor_ui.set_text(INPUT_ID, text);
		self.refresh_composer();
	}

	/// Returns the composer block height used for pointer hit testing.
	pub fn composer_rows(&mut self) -> u16 {
		if self.live_voice.is_some() {
			LIVE_VOICE_ROWS
		} else {
			self.editor_ui.height()
		}
	}

	/// Returns whether the latest status snapshot says a turn is active.
	pub fn is_working(&self) -> bool {
		self.work.borrow().facts.working
	}

	/// Returns a copy of the latest status snapshot.
	pub fn status(&self) -> StatusFacts {
		self.work.borrow().facts.clone()
	}

	/// Replaces the composer's completion source.
	pub fn set_completion(&mut self, completion: Box<dyn omp_tui::EditorCompletion>) {
		let completion = CompletionChain::new()
			.source(completion)
			.source(Box::new(self.slash_commands.clone()));
		self
			.editor_ui
			.update_component::<EditorPane>(INPUT_ID, |pane| {
				pane.set_completion(Box::new(completion));
				true
			});
	}

	/// Replaces slash-command completion data.
	pub fn set_slash_commands(&mut self, commands: Vec<Command>) {
		self.slash_commands.replace(commands);
	}

	/// Replaces slash-command data and its persisted usage ranker without
	/// disturbing the completion providers chained behind it.
	pub fn set_ranked_slash_commands(
		&mut self,
		commands: Vec<Command>,
		usage: impl Fn(&str) -> u64 + Send + Sync + 'static,
	) {
		self.slash_commands.replace_ranked(commands, usage);
	}

	/// Reserves right-edge columns for host-composited chrome.
	pub const fn set_right_inset(&mut self, cols: u16) {
		self.host_right_inset = cols;
	}

	/// Appends a finalized user message.
	pub fn push_user(&mut self, text: impl Into<String>, chips: Vec<Str>) {
		let text = mask_keywords(text.into(), &self.keyword_accent);
		self.enqueue_final(Entry::User(UserEntry {
			body: RichText::user(text, self.layout_width.max(1), &self.ctx),
			chips,
			queued: false,
			hint: None,
		}));
	}

	/// Appends a pending queued user message: dim, hinted, and kept live
	/// (unretired) until [`Chat::settle_queued`] or a dequeue restore.
	pub fn push_user_queued(&mut self, text: impl Into<String>, chips: Vec<Str>) {
		let text = mask_keywords(text.into(), &self.keyword_accent);
		for entry in self.entries.values_mut() {
			if let Entry::User(user) = entry {
				user.hint = None;
			}
		}
		let ordinal = self.blocks.create();
		self.entries.insert(
			ordinal,
			Entry::User(UserEntry {
				body: RichText::user(text, self.layout_width.max(1), &self.ctx),
				chips,
				queued: true,
				hint: Some(fmts_mut!(" · {} to edit", self.dequeue_hint).freeze()),
			}),
		);
		self.bump_live();
	}

	/// Finalizes queued user rows once the agent consumed or dropped the
	/// queue, restyling them as settled transcript messages.
	pub fn settle_queued(&mut self) {
		let mut settled = SmallVec::<BlockOrdinal, 4>::new();
		for (ordinal, entry) in &mut self.entries {
			if let Entry::User(user) = entry
				&& user.queued
			{
				user.queued = false;
				user.hint = None;
				settled.push(*ordinal);
			}
		}
		if settled.is_empty() {
			return;
		}
		for ordinal in settled {
			self.blocks.finalize(ordinal);
		}
		self.bump_live();
	}

	/// Overrides the dequeue chord rendered beside pending queued rows.
	pub fn set_dequeue_hint(&mut self, chord: impl IntoStr) {
		self.dequeue_hint = chord.into_str();
	}

	/// Stages attachments recovered from durable history into the composer
	/// band, re-probing image sources and re-collapsing pastes.
	pub fn stage_attachments(&mut self, attachments: Vec<crate::RestoredAttachment>) {
		if attachments.is_empty() {
			return;
		}
		for attachment in attachments {
			match attachment {
				crate::RestoredAttachment::Image { source } => {
					self.attachments.push_image(source);
				},
				crate::RestoredAttachment::Text(text) => {
					self.attachments.push_text(text.as_str());
				},
			}
		}
		self.refresh_composer();
	}

	/// Appends the welcome banner card as a finalized transcript block.
	pub fn push_welcome(&mut self, banner: crate::WelcomeBanner) {
		let intro_until = if self.reduced_motion {
			Duration::ZERO
		} else {
			self.started_at.elapsed().saturating_add(WELCOME_INTRO)
		};
		self.enqueue_final(Entry::Welcome(WelcomeEntry { banner, intro_until }));
		self.bump_live();
	}

	/// Begins a live assistant prose message.
	pub fn begin_assistant(&mut self, id: impl Into<Str>) {
		self.begin_stream(id.into(), false);
	}

	/// Begins a live assistant reasoning stream, rendered dim and italic.
	pub fn begin_thinking(&mut self, id: impl Into<Str>) {
		self.begin_stream(id.into(), true);
	}

	fn begin_stream(&mut self, id: Str, thinking: bool) {
		self.finalize_abandoned_streams();
		let ordinal = self.blocks.create();
		self.live_assistant = Some(LiveAssistant::new(
			ordinal,
			id,
			Self::message_width(self.layout_width),
			&self.ctx,
			self.started_at.elapsed(),
			thinking,
		));
		self.bump_live();
	}

	/// Settles streams which did not receive their terminal event.
	///
	/// Text and thinking parts end implicitly when the next part starts, so an
	/// assistant stream still live here settles exactly what it streamed;
	/// dropping it would erase transcript content. Abandoned tools settle as
	/// aborted cards.
	fn finalize_abandoned_streams(&mut self) {
		if let Some(assistant) = self.live_assistant.take() {
			self.settle_assistant(assistant);
		}
		for tool in self.live_tools.drain(..) {
			let label = tool_label(self.ctx.charset.icon(Icon::Cancellable), &tool.name);
			self.entries.insert(
				tool.ordinal,
				Entry::Tool(ToolEntry {
					label,
					terminal: ToolTerminal::Aborted,
					expanded: tool.expanded,
					view: tool.view,
					images: tool.images,
				}),
			);
			self.blocks.finalize(tool.ordinal);
		}
	}

	/// Appends a delta to a matching live assistant message.
	pub fn append_assistant(&mut self, id: &str, text: &str) {
		let now = self.started_at.elapsed();
		let smooth = self.smooth_streaming;
		if let Some(message) = &mut self.live_assistant
			&& message.id.as_str() == id
		{
			if message.append(text, smooth, now) {
				self.bump_live();
			}
		}
	}

	/// Replaces a matching streamed assistant body before settlement.
	pub fn replace_assistant(&mut self, id: &str, text: &str) {
		if let Some(message) = &mut self.live_assistant
			&& message.id.as_str() == id
		{
			message.replace(text);
			self.bump_live();
		}
	}

	/// Finalizes a matching live assistant message with an immutable semantic
	/// snapshot.
	pub fn end_assistant(&mut self, id: &str) {
		if self
			.live_assistant
			.as_ref()
			.is_some_and(|message| message.id.as_str() == id)
		{
			let message = self
				.live_assistant
				.take()
				.expect("matching live assistant exists");
			self.settle_assistant(message);
		}
	}

	/// Converts one finished live assistant stream into its immutable
	/// transcript entry and retires its allocation block.
	fn settle_assistant(&mut self, mut message: LiveAssistant) {
		let _ = message.flush();
		if message.thinking {
			if let Some(body) = sanitize_thinking_text(message.text.as_str(), true) {
				let body = if self.hide_thinking {
					self
						.hidden_thinking_label
						.as_ref()
						.map_or(body, ToString::to_string)
				} else {
					body
				};
				let body = RichText::thinking(body, Self::message_width(self.layout_width), &self.ctx);
				self
					.entries
					.insert(message.ordinal, Entry::Thinking(ThinkingEntry { body }));
			}
		} else {
			let body = AssistantEntry::new(
				message.text.as_str().to_owned(),
				Self::message_width(self.layout_width),
				&self.ctx,
			);
			self.entries.insert(message.ordinal, Entry::Assistant(body));
		}
		self.blocks.finalize(message.ordinal);
		self.bump_live();
	}

	/// Discards a matching live assistant message without settling an entry.
	///
	/// Used when a retry attempt is about to re-stream the same content from
	/// the start; settling the partial would duplicate the transcript.
	pub fn abandon_assistant(&mut self, id: &str) {
		if let Some(message) = self
			.live_assistant
			.take_if(|message| message.id.as_str() == id)
		{
			self.blocks.finalize(message.ordinal);
			self.bump_live();
		}
	}

	/// Inserts one replayed reasoning block as a settled thinking entry.
	pub fn push_thinking_replay(&mut self, text: &str) {
		let Some(body) = sanitize_thinking_text(text, true) else {
			return;
		};
		let body = RichText::thinking(body, Self::message_width(self.layout_width), &self.ctx);
		self.enqueue_final(Entry::Thinking(ThinkingEntry { body }));
	}

	/// Toggles thinking-block visibility scene-wide without mutating
	/// transcript truth: every unretired block and all future ones follow.
	pub fn toggle_thinking(&mut self) {
		self.hide_thinking = !self.hide_thinking;
		self.bump_live();
	}

	/// Toggles tool-card visibility scene-wide without mutating transcript
	/// truth.
	pub fn toggle_tool_visibility(&mut self) {
		self.hide_tools = !self.hide_tools;
		self.bump_live();
	}

	/// Begins a live tool card.
	pub fn tool_started(&mut self, id: impl Into<Str>, name: impl Into<Str>) {
		if self
			.live_assistant
			.as_mut()
			.is_some_and(LiveAssistant::flush)
		{
			self.bump_live();
		}
		let id = id.into();
		let name = name.into();
		let ordinal = self.blocks.create();
		let duration = if self.reduced_motion { "0ms" } else { "180ms" };
		let card = ToolCard::new()
			.with(Prop::Id, LIVE_TOOL_CARD_ID)
			.with(Prop::H, 0_u16)
			.with(Prop::Anim, duration)
			.with(Prop::Ease, "out")
			.name(name.clone())
			.folded(false)
			.child(TextLeaf::new().text(""));
		let mut card_ui = Ui::from_root(card, self.layout_width.max(1), self.ctx.clone());
		card_ui.tick(self.started_at.elapsed());
		self.live_tools.push(LiveTool {
			ordinal,
			id,
			name,
			expanded: self.tools_expanded,
			view: ToolView::plain(
				Default::default(),
				Self::tool_view_width(self.layout_width),
				&self.ctx,
			),
			images: Vec::new(),
			card_ui,
			target_height: 0,
			target_changed_at: self.started_at.elapsed(),
			body_folded: false,
		});
		self.bump_live();
	}

	/// Appends unstructured output to a matching live tool card.
	pub fn tool_output(&mut self, id: &str, chunk: &str) {
		let ctx = self.ctx.clone();
		if let Some(tool) = self
			.live_tools
			.iter_mut()
			.find(|tool| tool.id.as_str() == id)
		{
			tool.view.append_plain(chunk, &ctx);
			Self::refresh_live_tool_card(tool, self.layout_width.max(1), &ctx);
			self.bump_live();
		}
	}

	/// Replaces a matching live tool card's renderer-produced view.
	pub fn tool_view(&mut self, id: &str, view: ToolViewContent) {
		let ctx = self.ctx.clone();
		if let Some(tool) = self
			.live_tools
			.iter_mut()
			.find(|tool| tool.id.as_str() == id)
		{
			tool.view.replace_content(view, &ctx);
			Self::refresh_live_tool_card(tool, self.layout_width.max(1), &ctx);
			self.bump_live();
		}
	}

	/// Attaches a persisted PNG to a matching live tool card; the committed
	/// card renders it inline. Sources whose headers fail to probe are
	/// ignored, keeping the text fallback.
	pub fn tool_image(&mut self, id: &str, source: impl Into<Str>) {
		let source = source.into();
		let ctx = self.ctx.clone();
		let Some(px) = fs::read(source.as_str())
			.ok()
			.and_then(|bytes| omp_tui::imagefmt::dimensions(&bytes))
		else {
			return;
		};
		if let Some(tool) = self
			.live_tools
			.iter_mut()
			.find(|tool| tool.id.as_str() == id)
		{
			tool.images.push(ToolImageEntry { source, px });
			Self::refresh_live_tool_card(tool, self.layout_width.max(1), &ctx);
			self.bump_live();
		}
	}

	/// Finalizes a matching live tool card with its terminal branch and view.
	pub fn tool_finished(&mut self, id: &str, terminal: ToolTerminal, view: ToolViewContent) {
		if let Some(index) = self
			.live_tools
			.iter()
			.position(|tool| tool.id.as_str() == id)
		{
			let tool = self.live_tools.remove(index);
			let ordinal = tool.ordinal;
			let width = tool.view.width;
			let images = tool.images.clone();
			let icon = match terminal {
				ToolTerminal::Succeeded => self.ctx.charset.check(),
				ToolTerminal::Failed => self.ctx.charset.icon(Icon::Error),
				ToolTerminal::ArgsRejected => self.ctx.charset.icon(Icon::Warning),
				ToolTerminal::Aborted => self.ctx.charset.icon(Icon::Cancellable),
				ToolTerminal::Skipped => self.ctx.charset.icon(Icon::Cancellable),
			};
			let label = tool_label(icon, &tool.name);
			let entry = ToolEntry {
				label,
				terminal,
				expanded: tool.expanded,
				view: ToolView::from_content(view, width, &self.ctx),
				images,
			};
			self.entries.insert(ordinal, Entry::Tool(entry));
			self.blocks.finalize(ordinal);
			self.bump_live();
		}
	}

	/// Toggles the most recent active tool card.
	///
	/// Returns its new expansion state, or [`None`] when no tool card exists.
	pub fn toggle_latest_tool(&mut self) -> Option<bool> {
		let live = self.live_tools.last().map(|tool| tool.ordinal);
		let settled = self
			.entries
			.range(BlockOrdinal(self.blocks.frontier())..)
			.rev()
			.find_map(|(ordinal, entry)| matches!(entry, Entry::Tool(_)).then_some(*ordinal));
		if let Some(ordinal) = live.into_iter().chain(settled).max() {
			return self.toggle_tool_ordinal(ordinal);
		}
		None
	}

	fn toggle_tool_ordinal(&mut self, ordinal: BlockOrdinal) -> Option<bool> {
		if let Some(tool) = self
			.live_tools
			.iter_mut()
			.find(|tool| tool.ordinal == ordinal)
		{
			tool.expanded = !tool.expanded;
			if tool.expanded && tool.body_folded {
				tool.body_folded = false;
				tool
					.card_ui
					.update_component::<ToolCard>(LIVE_TOOL_CARD_ID, |card| card.set_folded(false));
			}
			let expanded = tool.expanded;
			self.bump_live();
			return Some(expanded);
		}
		if let Some(Entry::Tool(tool)) = self.entries.get_mut(&ordinal) {
			tool.expanded = !tool.expanded;
			let expanded = tool.expanded;
			self.bump_live();
			return Some(expanded);
		}
		None
	}

	/// Appends an in-place compaction divider with method, token delta, and
	/// optional preview title.
	pub fn push_compaction(
		&mut self,
		summary: Str,
		title: Option<Str>,
		method: Option<Str>,
		tokens_before: u64,
		tokens_after: Option<u64>,
	) {
		let preview = title
			.as_deref()
			.filter(|title| !title.trim().is_empty())
			.or_else(|| summary.lines().find(|line| !line.trim().is_empty()));
		let method = compaction_method_label(method.as_deref());
		let mut label = fmts_mut!("{} {method}", self.ctx.charset.icon(Icon::Camera));
		if let Some(tokens_after) = tokens_after {
			if tokens_before > 0 {
				let arrow = if self.ctx.charset == Charset::Ascii {
					"->"
				} else {
					"→"
				};
				let _ = write!(
					label,
					" · {}{arrow}{}",
					compact_count(tokens_before),
					compact_count(tokens_after),
				);
			} else {
				let _ = write!(label, " to {} tokens", compact_count(tokens_after));
			}
		}
		if let Some(preview) = preview {
			let _ = write!(label, " · {preview}");
		}
		self.enqueue_final(Entry::Compaction(CompactionEntry { label: label.freeze() }));
	}

	/// Appends an informational transcript notice.
	pub fn push_notice(&mut self, text: impl IntoStr) {
		self.enqueue_final(Entry::Notice { text: text.into_str(), error: false });
	}

	/// Appends an error transcript notice.
	///
	/// The transcript entry is the sole surface for one-off errors; the pinned
	/// live-chrome status line is reserved for turn failures delivered via
	/// [`Self::push_transcript_frame`], which own a retained diagnostic card.
	pub fn push_error(&mut self, text: impl IntoStr) {
		self.enqueue_final(Entry::Notice { text: text.into_str(), error: true });
	}

	/// Applies an exact-key retained frame, enhancing known revisions and
	/// retaining the producer fallback for unknown revisions.
	pub fn apply_retained_frame(
		&mut self,
		envelope: v1::RetainedFrameEnvelope,
	) -> Result<(), FrameError> {
		match self.retained_frames.apply(envelope)? {
			FrameMutation::Upserted(identity) => {
				let frame = self
					.retained_frames
					.get(&identity)
					.expect("an upserted retained frame is present");
				let source = render_frame_tml(frame);
				let expires_at = retained_expiry(frame, self.started_at.elapsed());
				let width = Self::tool_view_width(self.layout_width.max(1));
				self.enqueue_final(Entry::Retained(RetainedEntry {
					view: ToolView::structured(source, width, &self.ctx),
					expires_at,
				}));
			},
			FrameMutation::Removed { identity, .. } => {
				let _ = identity;
			},
		}
		self.bump_live();
		Ok(())
	}

	/// Applies a non-persistent theme preview and invalidates every retained
	/// presentation cache derived from the previous semantic palette.
	pub fn preview_theme(&mut self, theme: Theme) {
		if self.ctx.theme == theme {
			return;
		}
		self.ctx.theme = theme;
		let context = self.ctx.clone();
		let _ = self.editor_ui.set_context(context.clone());
		self
			.editor_ui
			.update_component::<ChatStatus>(STATUS_ID, |status| {
				status.set_theme(theme);
				true
			});
		for entry in &mut self.entries.values_mut() {
			restyle_entry(entry, &context);
		}
		if let Some(assistant) = &mut self.live_assistant {
			let _ = assistant.view.set_context(context.clone());
		}
		for tool in &mut self.live_tools {
			let _ = tool.view.rendered.set_context(context.clone());
			let _ = tool.card_ui.set_context(context.clone());
		}
		self.bump_live();
	}

	/// Appends a semantic transcript boundary with core-owned styling.
	///
	/// [`TranscriptFrameKind::Error`] only pins the failure status line; the
	/// durable transcript surface for a failed turn is its retained
	/// diagnostic card, so no duplicate notice entry is appended.
	pub fn push_transcript_frame(&mut self, frame: TranscriptFrame) {
		if frame.kind == TranscriptFrameKind::Peer {
			self.enqueue_final(Entry::Peer { title: frame.title, detail: frame.detail });
			return;
		}
		if frame.kind == TranscriptFrameKind::Recovery {
			self.pinned_error = None;
		}
		let marker = match frame.kind {
			TranscriptFrameKind::Compaction => "compact",
			TranscriptFrameKind::Branch => "branch",
			TranscriptFrameKind::Handoff => "handoff",
			TranscriptFrameKind::CacheBreak => "cache break",
			TranscriptFrameKind::Recovery => "recovery",
			TranscriptFrameKind::Peer => "peer",
			TranscriptFrameKind::Error => "error",
		};
		let text = match frame.detail {
			Some(detail) if !detail.is_empty() => sf!("{marker} · {} — {detail}", frame.title),
			_ => sf!("{marker} · {}", frame.title),
		};
		if frame.kind == TranscriptFrameKind::Error {
			self.pinned_error = Some(text);
			self.bump_live();
		} else {
			self.push_notice(text);
		}
	}

	/// Replaces the anchored `AgentTree` HUD projection.
	pub fn set_agent_roster(&mut self, rows: Vec<AgentRow>) {
		self.agent_labels = rows
			.iter()
			.map(|agent| agent_label(agent, self.ctx.charset))
			.collect();
		self.agents = rows;
		self.bump_live();
	}

	/// Borrows the current `AgentTree` roster projection.
	pub fn agent_roster(&self) -> &[AgentRow] {
		&self.agents
	}

	/// Replaces the sticky todo HUD from a canonical environment projection.
	pub fn set_todo_hud(&mut self, todo: TodoHud) {
		self.todo_lines = todo.lines;
		self.todo_collapsed_lines.clear();
		let mut visible_tasks = 0_usize;
		for line in &self.todo_lines {
			let task = line.trim_start().starts_with("- [");
			if task && visible_tasks == 5 {
				break;
			}
			self.todo_collapsed_lines.push(line.clone());
			visible_tasks += usize::from(task);
		}
		let hidden = todo.total_tasks.saturating_sub(visible_tasks);
		if hidden > 0 {
			self
				.todo_collapsed_lines
				.push(sf!("{hidden} more todo{}", if hidden == 1 { "" } else { "s" }));
		}
		self.bump_live();
	}

	/// Selects the full or bounded sticky todo HUD presentation.
	pub fn set_todo_expanded(&mut self, expanded: bool) {
		if self.todo_expanded != expanded {
			self.todo_expanded = expanded;
			self.bump_live();
		}
	}

	/// Replaces the complete status snapshot.
	pub fn set_status(&mut self, facts: StatusFacts) {
		let now = self.started_at.elapsed();
		if facts.working {
			self.idle_recap = None;
		}
		self.set_reduced_motion(facts.reduced_motion);
		let quota_reset = {
			let previous = &self.work.borrow().facts;
			!previous.quota_reset && facts.quota_reset && !facts.reduced_motion
		};
		if quota_reset {
			self.celebration_until = Some(now.saturating_add(Duration::from_secs(2)));
		}
		let labels = StatusLabels::new(&facts, self.ctx.charset);
		let mut work = self.work.borrow_mut();
		if !facts.working {
			work.indicator = None;
		}
		if work.facts.working != facts.working {
			work.fade.retarget(
				now,
				if facts.working {
					self.ctx.theme.ok
				} else {
					self.ctx.theme.muted
				},
				BRAND_FADE,
				Easing::EaseInOut,
			);
		}
		work.facts = facts;
		work.labels = labels;
		work.elapsed_label = None;
		work.update_active_brand(now, self.ctx.charset);
		drop(work);
		self
			.editor_ui
			.update_component::<EditorPane>(INPUT_ID, |_| true);
		self.bump_live();
	}

	/// Replaces the active turn's core-timed working indicator.
	pub fn set_working_indicator(&mut self, indicator: WorkingIndicator) {
		let now = self.started_at.elapsed();
		let mut work = self.work.borrow_mut();
		work.indicator = Some(indicator);
		work.update_active_brand(now, self.ctx.charset);
		drop(work);
		self.bump_live();
	}

	fn set_idle_recap(&mut self, text: Str) {
		let prefix = if self.ctx.charset == Charset::Ascii {
			"recap: "
		} else {
			"※ recap: "
		};
		let mut label = StrMut::with_capacity(prefix.len().saturating_add(text.len()));
		label.push_str(prefix);
		label.push_str(text.as_str());
		self.idle_recap = Some(label.freeze());
		self.bump_live();
	}

	fn clear_idle_recap(&mut self) {
		if self.idle_recap.take().is_some() {
			self.bump_live();
		}
	}

	/// Replaces the session title shown at the right end of the status row.
	pub fn set_session_title(&mut self, title: impl Into<Str>) {
		self.work.borrow_mut().title = hud_line(title.into(), self.ctx.charset);
		self
			.editor_ui
			.update_component::<EditorPane>(INPUT_ID, |_| true);
		self.bump_live();
	}

	/// Restores a prompt that the backend dropped before committing its first
	/// turn, without overwriting a draft started while cancellation settled.
	pub fn restore_dropped_prompt(&mut self, text: Str, attachments: Vec<Attachment>) {
		if let Some(index) = self.entries.values().rposition(
			|entry| matches!(entry, Entry::User(user) if user.body.text.as_str() == text.as_str()),
		) {
			let ordinal = *self.entries.keys().nth(index).expect("entries index");
			self
				.entries
				.insert(ordinal, Entry::Notice { text: Str::default(), error: false });
			self.bump_live();
		}
		if !self.composer_empty() || !self.attachments.is_empty() {
			return;
		}
		for attachment in attachments {
			match attachment.content {
				AttachmentContent::Image { source, .. } => {
					self.attachments.push_image(source);
				},
				AttachmentContent::Text { text, .. } => {
					self.attachments.push_text(text.as_str());
				},
			}
		}
		self.set_composer_text(text.as_str());
	}

	/// Prepends every unstarted queued prompt to the current draft and restores
	/// its attachment descriptors without re-probing their sources.
	pub fn restore_queued_prompts(&mut self, prompts: Vec<QueuedPrompt>) {
		if prompts.is_empty() {
			return;
		}
		let mut queued = String::new();
		let mut attachments = Vec::new();
		for prompt in prompts {
			let masked = mask_keywords(prompt.text.to_string(), &self.keyword_accent);
			if let Some(index) = self
				.entries
				.values()
				.rposition(|entry| matches!(entry, Entry::User(user) if user.body.text == masked))
			{
				let ordinal = *self.entries.keys().nth(index).expect("entries index");
				self
					.entries
					.insert(ordinal, Entry::Notice { text: Str::default(), error: false });
				self.blocks.finalize(ordinal);
			}
			if !queued.is_empty() {
				queued.push_str("\n\n");
			}
			queued.push_str(prompt.text.as_str());
			attachments.extend(prompt.attachments);
		}
		let draft = self.composer_text();
		if !draft.trim().is_empty() {
			queued.push_str("\n\n");
			queued.push_str(&draft);
		}
		self.attachments.restore(attachments);
		self.set_composer_text(&queued);
	}

	/// Drops uncommitted snapshots at a selected user boundary and appends a
	/// finalized rewind marker.
	pub fn rewind_user(&mut self, user_index: usize, text: &str) -> bool {
		let selected = self
			.entries
			.iter()
			.filter(|(_, entry)| matches!(entry, Entry::User(_)))
			.nth(user_index)
			.and_then(|(ordinal, entry)| match entry {
				Entry::User(user) if user.body.text == text => Some(*ordinal),
				_ => None,
			})
			.or_else(|| {
				self
					.entries
					.iter()
					.find_map(|(ordinal, entry)| match entry {
						Entry::User(user) if user.body.text == text => Some(*ordinal),
						_ => None,
					})
			});
		let matched = selected.is_some();
		let frontier = BlockOrdinal(self.blocks.frontier());
		let start = selected.map_or(frontier, |selected| selected.max(frontier));
		for (_, entry) in self.entries.range_mut(start..) {
			*entry = Entry::Notice { text: Str::default(), error: false };
		}
		self.cancel_active("cancelled by history rewind");
		self.pinned_error = None;
		self.enqueue_final(Entry::Notice { text: "history rewound".into(), error: false });
		self.bump_live();
		matched
	}

	/// Finalizes every active producer with an explicit cancellation snapshot.
	pub fn cancel_active(&mut self, reason: impl IntoStr) {
		let reason = reason.into_str();
		if let Some(assistant) = self.live_assistant.take() {
			if assistant.text.is_empty() {
				self
					.entries
					.insert(assistant.ordinal, Entry::Notice { text: reason.clone(), error: true });
			} else {
				let mut text = assistant.text.as_str().to_owned();
				if !text.ends_with("\n\n") {
					if text.ends_with('\n') {
						text.push('\n');
					} else {
						text.push_str("\n\n");
					}
				}
				text.push_str(reason.as_str());
				self.entries.insert(
					assistant.ordinal,
					Entry::Assistant(AssistantEntry::new(
						text,
						Self::message_width(self.layout_width),
						&self.ctx,
					)),
				);
			}
			self.blocks.finalize(assistant.ordinal);
		}
		for tool in self.live_tools.drain(..) {
			let label = sf!("{} · {reason}", tool.name);
			self
				.entries
				.insert(tool.ordinal, Entry::Notice { text: label, error: true });
			self.blocks.finalize(tool.ordinal);
		}
		self.bump_live();
	}

	/// Drops every uncommitted snapshot, clears live state, and appends a
	/// finalized history-cleared divider. Native history is unaffected.
	pub fn clear_history(&mut self) {
		let frontier = BlockOrdinal(self.blocks.frontier());
		for (_, entry) in self.entries.range_mut(frontier..) {
			*entry = Entry::Notice { text: Str::default(), error: false };
		}
		self.cancel_active("cancelled because history was cleared");
		for (_, entry) in self.entries.range_mut(frontier..) {
			*entry = Entry::Notice { text: Str::default(), error: false };
		}
		self.retained_frames = RetainedFrames::new();
		self.pinned_error = None;
		self.turn_started_at_ms = None;
		self.last_completed_at_ms = None;
		self.enqueue_final(Entry::Notice { text: "history cleared".into(), error: false });
		self.bump_live();
	}

	/// Applies scene-owned backend mutations and returns events owned by host
	/// overlays.
	pub fn apply_backend_event(&mut self, event: BackendEvent) -> Option<BackendEvent> {
		match event {
			BackendEvent::TurnAnchor(anchor) => self.apply_turn_anchor(anchor),
			BackendEvent::AssistantUsage(row) => self.push_assistant_usage(row),
			BackendEvent::ShowTokenUsageChanged(show) => self.set_show_token_usage(show),
			BackendEvent::ShowTurnTimeChanged(show) => self.set_show_turn_time(show),
			BackendEvent::UserReplayed { text, chips, queued } => {
				if !self.suppress_history_replay {
					let text = omp_agent::strip_system_wrapper(text.as_str()).unwrap_or(text.as_str());
					if queued {
						self.push_user_queued(text, chips);
					} else {
						self.push_user(text, chips);
					}
				}
			},
			BackendEvent::WelcomeBanner(banner) => self.push_welcome(banner),
			BackendEvent::ThinkingReplayed { text } => {
				if !self.suppress_history_replay {
					self.push_thinking_replay(text.as_str());
				}
			},
			BackendEvent::PromptDropped { text, attachments } => {
				self.restore_dropped_prompt(text, attachments);
			},
			BackendEvent::QueuedPromptsRestored(prompts) => self.restore_queued_prompts(prompts),
			BackendEvent::QueuedPromptsSettled => self.settle_queued(),
			BackendEvent::AssistantBegin { id, thinking } => {
				if thinking {
					self.begin_thinking(id);
				} else {
					self.begin_assistant(id);
				}
			},
			BackendEvent::AssistantDelta { id, text } => {
				self.append_assistant(id.as_str(), text.as_str());
			},
			BackendEvent::AssistantReplace { id, text } => {
				self.replace_assistant(id.as_str(), text.as_str());
			},
			BackendEvent::AssistantEnd { id } => self.end_assistant(id.as_str()),
			BackendEvent::AssistantAbandoned { id } => self.abandon_assistant(id.as_str()),
			BackendEvent::ToolStarted { id, name } => self.tool_started(id, name),
			BackendEvent::ToolOutput { id, chunk } => self.tool_output(id.as_str(), chunk.as_str()),
			BackendEvent::ToolView { id, view } => self.tool_view(id.as_str(), view),
			BackendEvent::ToolImage { id, source } => self.tool_image(id.as_str(), source),
			BackendEvent::ToolFinished { id, terminal, view } => {
				self.tool_finished(id.as_str(), terminal, view);
			},
			BackendEvent::Compacted { summary, title, method, tokens_before, tokens_after } => {
				self.push_compaction(summary, title, method, tokens_before, tokens_after);
			},
			BackendEvent::TranscriptFrame(frame) => self.push_transcript_frame(frame),
			BackendEvent::RetainedFrame(envelope) => {
				if let Err(error) = self.apply_retained_frame(envelope) {
					self.push_error(sf!("Rejected retained frame: {error}"));
				}
			},
			BackendEvent::AgentRoster(rows) => self.set_agent_roster(rows),
			BackendEvent::TodoHud(todo) => self.set_todo_hud(todo),
			BackendEvent::TodoExpanded(expanded) => self.set_todo_expanded(expanded),
			BackendEvent::SlashCommands(commands) => self.set_slash_commands(commands),
			BackendEvent::Notice(text) => self.push_notice(text),
			BackendEvent::Error(text) => self.push_error(text),
			BackendEvent::Status(facts) => self.set_status(facts),
			BackendEvent::WorkingIndicator(indicator) => self.set_working_indicator(indicator),
			BackendEvent::ToolsExpanded(expanded) => self.set_tools_expanded(expanded),
			BackendEvent::HiddenThinkingLabel(label) => self.set_hidden_thinking_label(label),
			BackendEvent::Recap(text) => {
				if !self.is_working() && self.composer_empty() {
					self.set_idle_recap(text);
				}
			},
			BackendEvent::RecapPolicy { .. } => {},
			BackendEvent::ThemePreview(theme) => self.preview_theme(theme),
			BackendEvent::ComposerReplaced(text) => self.set_composer_text(text.as_str()),
			BackendEvent::ComposerPaste(text) => self.handle_paste(text.as_str()),
			BackendEvent::ApplyUiEffect(effect) => {
				let _ = self.apply_ui_effect(&effect);
			},
			BackendEvent::TerminalNotification(_) | BackendEvent::TerminalProgress(_) => {},
			BackendEvent::ComposerStyleChanged(style) => self.set_composer_style(style),
			BackendEvent::SpellingFeaturesChanged(features) => self.set_spelling_features(features),
			BackendEvent::SmoothStreamingChanged(smooth) => self.set_smooth_streaming(smooth),
			BackendEvent::ModelDownloadProgress(progress) => {
				let now = self.started_at.elapsed();
				self.download_activity = Some(DownloadActivity::new(progress, now));
				self.bump_live();
			},
			BackendEvent::LiveVoiceStarted => self.start_live_voice(),
			BackendEvent::LiveVoiceUpdated { phase, input_level, output_level, transcript } => {
				if self.live_voice.is_none() {
					self.start_live_voice();
				}
				if let Some(visualizer) = self.live_voice.as_mut() {
					visualizer.set_phase(phase);
					visualizer.set_levels(input_level, output_level);
					visualizer.set_transcript(transcript);
				}
				self.bump_live();
			},
			BackendEvent::LiveVoiceStopped => self.stop_live_voice(),
			BackendEvent::SessionTitle(title) => self.set_session_title(title),
			BackendEvent::HistoryRewind { user_index, text, attachments } => {
				self.suppress_history_replay = true;
				let _ = self.rewind_user(user_index, text.as_str());
				self.stage_attachments(attachments);
			},
			BackendEvent::HistoryReplayFinished => {
				self.suppress_history_replay = false;
			},
			BackendEvent::HistoryCleared => self.clear_history(),
			BackendEvent::Ack { interrupted } => {
				if interrupted {
					self.push_notice("Interrupted.");
				}
			},
			event @ (BackendEvent::ApprovalPending(_)
			| BackendEvent::OpenGitWorkbench(_)
			| BackendEvent::Git(_)
			| BackendEvent::AutoQaConsent(_)
			| BackendEvent::HistoryInspect { .. }
			| BackendEvent::OpenGuidedGoal
			| BackendEvent::OpenPlanReview { .. }
			| BackendEvent::OpenPlanSavePrompt { .. }
			| BackendEvent::OpenExtensionInspector(_)
			| BackendEvent::ExtensionSnapshotUpdated(_)
			| BackendEvent::ExtensionMcpUpdated(_)
			| BackendEvent::ExtensionProviderDisabled(_)
			| BackendEvent::ApprovalSettled { .. }
			| BackendEvent::PtyStarted { .. }
			| BackendEvent::PtyOutput { .. }
			| BackendEvent::PtyFinished { .. }
			| BackendEvent::OpenModelPicker { .. }
			| BackendEvent::ModelsUpdated { .. }
			| BackendEvent::OpenModelHub(_)
			| BackendEvent::ModelHubUpdated(_)
			| BackendEvent::Sessions(_)
			| BackendEvent::WelcomeLspServers(_)
			| BackendEvent::LoginProviders(_)
			| BackendEvent::LogoutChoices { .. }
			| BackendEvent::RewindTargets(_)
			| BackendEvent::LoginPanel { .. }
			| BackendEvent::LoginPanelClose
			| BackendEvent::ApplySettings { .. }
			| BackendEvent::Select { .. }
			| BackendEvent::SettingsSchema(_)
			| BackendEvent::OpenSelection { .. }
			| BackendEvent::OpenAgentTree
			| BackendEvent::UiRequest { .. }
			| BackendEvent::OpenRawStream { .. }
			| BackendEvent::OpenDebugTools(_)
			| BackendEvent::OpenProtocolProbe
			| BackendEvent::OpenLogs { .. }
			| BackendEvent::OlderLogs { .. }
			| BackendEvent::RawStreamFrame { .. }
			| BackendEvent::RawStreamSnapshot { .. }
			| BackendEvent::RawStreamClosed
			| BackendEvent::CopyToClipboard(_)
			| BackendEvent::Pause
			| BackendEvent::NewSessionRequested
			| BackendEvent::SessionResumeRequested(_)) => return Some(event),
		}
		None
	}

	/// Renders one exactly viewport-sized, history-neutral frame.
	pub fn render(&mut self, viewport: Size) -> ViewportFrame<'_> {
		self.render_at(viewport, self.started_at.elapsed())
	}

	/// Renders the viewport that must remain after `batch` succeeds without
	/// advancing the real commit frontier before the terminal write.
	pub fn render_after_retirement(
		&mut self,
		viewport: Size,
		batch: &RetirementBatch,
	) -> ViewportFrame<'_> {
		let frontier = match batch.kind {
			RetirementKind::Commit => batch.range.end,
			RetirementKind::Replay(_) | RetirementKind::Band => self.blocks.frontier(),
		};
		self.render_at_with_frontier(
			viewport,
			self.started_at.elapsed(),
			frontier,
			AdmissionMode::Defer,
		)
	}

	/// Returns the delay until the composer's next requested animation frame.
	///
	/// A settled idle chat returns `None`, allowing custom hosts to block on
	/// input and backend events without polling.
	pub fn next_wake(&self) -> Option<Duration> {
		if self.layout_width != self.content_width(self.last_viewport) {
			return Some(Duration::ZERO);
		}
		let elapsed = self.started_at.elapsed();
		let editor = self
			.editor_ui
			.next_wake()
			.map(|deadline| deadline.saturating_sub(elapsed));
		let download = self.download_activity.as_ref().and_then(|activity| {
			let reveal = activity.received.saturating_add(Duration::from_secs(1));
			let hide = activity
				.completed
				.map(|completed| completed.saturating_add(Duration::from_secs(3)));
			let deadline = if elapsed < reveal {
				Some(reveal)
			} else {
				hide.filter(|hide| elapsed < *hide)
			};
			deadline.map(|deadline| deadline.saturating_sub(elapsed))
		});
		let retained = self
			.entries
			.range(BlockOrdinal(self.blocks.frontier())..)
			.filter_map(|(_, entry)| match entry {
				Entry::Retained(frame) => frame.expires_at,
				_ => None,
			})
			.filter(|deadline| elapsed < *deadline)
			.map(|deadline| deadline.saturating_sub(elapsed))
			.min();
		let welcome_intro = self
			.entries
			.range(BlockOrdinal(self.blocks.frontier())..)
			.any(
				|(_, entry)| matches!(entry, Entry::Welcome(welcome) if elapsed < welcome.intro_until),
			)
			.then_some(anim::FRAME);
		let celebration = self
			.celebration_until
			.filter(|deadline| elapsed < *deadline)
			.map(|deadline| {
				deadline
					.saturating_sub(elapsed)
					.min(Duration::from_millis(50))
			});
		let active = (!self.live_tools.is_empty()).then_some(anim::FRAME);
		let reveal = self.live_assistant.as_ref().and_then(|assistant| {
			assistant
				.next_reveal()
				.map(|deadline| deadline.saturating_sub(elapsed))
		});
		let voice = self.live_voice.is_some().then_some(LIVE_VOICE_FRAME);
		[editor, download, retained, welcome_intro, celebration, active, reveal, voice]
			.into_iter()
			.flatten()
			.min()
	}

	/// Returns the current unsent composer text.
	pub fn composer_text(&self) -> String {
		self.editor_ui.values()[INPUT_ID]
			.as_str()
			.unwrap_or_default()
			.to_owned()
	}

	fn refresh_composer(&mut self) {
		let width = self.editor_ui.frame().size().width;
		if width > 0 {
			self.editor_ui.resize(width);
		}
	}

	const fn bump_live(&mut self) {
		self.live_revision = self.live_revision.wrapping_add(1);
	}

	const fn right_inset(&self) -> u16 {
		self.host_right_inset.saturating_add(self.slot_right_inset)
	}

	fn content_width(&self, viewport: Size) -> u16 {
		viewport.width.saturating_sub(self.right_inset()).max(1)
	}

	/// Resolves the chrome row budget shared by rendering and retirement.
	fn chrome_layout(&mut self, viewport: Size, elapsed: Duration) -> ChromeLayout {
		if viewport.width == 0 || viewport.height == 0 {
			return ChromeLayout::default();
		}
		let content_width = self.content_width(viewport);
		if self.editor_ui.frame().size().width != content_width {
			self.editor_ui.resize(content_width);
		}
		let editor_height = self.composer_rows().min(viewport.height);
		let editor_y = viewport.height.saturating_sub(editor_height);
		let title_y = editor_y.saturating_sub(1);
		let working_y = title_y.saturating_sub(1);
		let available_chrome = working_y;
		let error_rows = self
			.pinned_error
			.as_deref()
			.map(|error| {
				flowed_height(error, content_width.saturating_sub(2).max(1))
					.min(3)
					.min(available_chrome)
			})
			.unwrap_or(0);
		let download_rows = u16::from(
			self
				.download_activity
				.as_ref()
				.is_some_and(|activity| activity.visible(elapsed)),
		)
		.min(available_chrome.saturating_sub(error_rows));
		let todo_len = if self.todo_expanded {
			self.todo_lines.len()
		} else {
			self.todo_collapsed_lines.len()
		};
		let todo_rows = if todo_len == 0 {
			0
		} else {
			u16::try_from(todo_len.saturating_add(1))
				.unwrap_or(u16::MAX)
				.min(available_chrome.saturating_sub(error_rows.saturating_add(download_rows)))
		};
		let h_live = working_y.saturating_sub(
			error_rows
				.saturating_add(download_rows)
				.saturating_add(todo_rows),
		);
		ChromeLayout {
			editor_height,
			editor_y,
			title_y,
			working_y,
			error_rows,
			download_rows,
			todo_rows,
			h_live,
		}
	}

	/// Renders one exactly viewport-sized frame at an explicit timeline
	/// instant, letting deterministic hosts (galleries, snapshots, tests)
	/// settle admission and entrance animation without waiting on wall time.
	pub fn render_at(&mut self, viewport: Size, elapsed: Duration) -> ViewportFrame<'_> {
		let frontier = self.blocks.frontier();
		self.render_at_with_frontier(viewport, elapsed, frontier, AdmissionMode::Allow)
	}

	fn render_at_with_frontier(
		&mut self,
		viewport: Size,
		elapsed: Duration,
		frontier: u64,
		admission: AdmissionMode,
	) -> ViewportFrame<'_> {
		let smooth = self.smooth_streaming;
		if self
			.live_assistant
			.as_mut()
			.is_some_and(|assistant| assistant.advance(elapsed, smooth))
		{
			self.bump_live();
		}
		self.last_viewport = viewport;
		if self.frame.size() != viewport {
			self.frame = Frame::new(viewport);
		}
		if viewport.width == 0 || viewport.height == 0 {
			return ViewportFrame { frame: &self.frame, damage: SmallVec::new() };
		}
		self
			.frame
			.fill(Rect::new(0, 0, viewport.width, viewport.height), base_style(self.ctx.theme));
		// Resident committed tail from the last settled-resize replay: these
		// rows live on screen (not yet in native history) and repaint every
		// frame so history-neutral presents cannot erase them.
		match self.band.as_ref() {
			Some(band) if band.size().width == viewport.width => {
				let rows = band.size().height;
				self.frame.blit(band, 0, rows, 0, 0);
			},
			// Stale width: drop it; the pending settled replay re-derives it.
			Some(_) => self.band = None,
			None => {},
		}
		let content_width = self.content_width(viewport);
		self.layout_width = content_width;
		self.editor_ui.tick(elapsed);
		let chrome = self.chrome_layout(viewport, elapsed);
		let ChromeLayout {
			editor_height,
			editor_y,
			title_y,
			working_y,
			error_rows,
			download_rows,
			todo_rows,
			h_live,
		} = chrome;
		// Settled snapshots stay in the mutable viewport, re-measured at the
		// current width every frame (so resizes reflow and theme changes
		// restyle them), until ordered retirement moves them into native
		// history under capacity pressure.
		let mut settled = SmallVec::<(BlockOrdinal, u16), 8>::new();
		for (ordinal, entry) in self.entries.range_mut(BlockOrdinal(frontier)..) {
			if self.hide_thinking
				&& self.hidden_thinking_label.is_none()
				&& matches!(entry, Entry::Thinking(_))
				|| self.hide_tools && matches!(entry, Entry::Tool(_))
			{
				continue;
			}
			Self::resize_entry(entry, content_width, &self.ctx);
			let height = Self::entry_height(entry, content_width, self.ctx.charset);
			if height > 0 {
				settled.push((*ordinal, height));
			}
		}
		let mut sampled = SmallVec::<(BlockOrdinal, u16), 16>::new();
		let mut natural = SmallVec::<(BlockOrdinal, u16), 16>::new();
		if let Some(assistant) = self
			.live_assistant
			.as_mut()
			.filter(|assistant| !(self.hide_thinking && assistant.thinking))
		{
			assistant.resize(content_width);
			sampled.push((assistant.ordinal, assistant.allocation));
			natural.push((assistant.ordinal, assistant.height()));
		}
		for tool in &mut self.live_tools {
			if self.hide_tools {
				continue;
			}
			if tool.card_ui.frame().size().width != content_width {
				tool.card_ui.resize(content_width);
			}
			tool
				.view
				.resize(Self::tool_view_width(content_width), &self.ctx);
			tool.card_ui.tick(elapsed);
			sampled.push((tool.ordinal, tool.card_ui.height()));
			natural.push((
				tool.ordinal,
				if tool.view.chrome == ViewChrome::Flush {
					tool.view.height().max(1)
				} else if tool.expanded {
					tool.view.height().saturating_add(2).max(1)
				} else {
					1
				},
			));
		}
		// Merged allocation across settled snapshots and live cards. Every
		// represented block gets one row first; surplus favors transcript text
		// before tool cards, newest-first within each class.
		let mut merged = SmallVec::<(BlockOrdinal, u16, bool), 16>::new();
		for (ordinal, height) in &settled {
			merged.push((*ordinal, *height, true));
		}
		for ((ordinal, painted), (_, wanted)) in sampled.iter().zip(natural.iter()) {
			if self.blocks.phase(*ordinal) == Some(crate::BlockPhase::Active) {
				merged.push((*ordinal, (*painted).max(*wanted), false));
			}
		}
		merged.sort_unstable_by_key(|(ordinal, ..)| *ordinal);
		let tool_flags = merged
			.iter()
			.map(|(ordinal, _, is_settled)| {
				if *is_settled {
					matches!(self.entries.get(ordinal), Some(Entry::Tool(_)))
				} else {
					self.live_tools.iter().any(|tool| tool.ordinal == *ordinal)
				}
			})
			.collect::<SmallVec<bool, 16>>();
		let layout_overflow = merged.len() > usize::from(h_live);
		let summary_rows = u16::from(layout_overflow && h_live > 0);
		let mut allocs = SmallVec::<u16, 16>::new();
		let wanted_total = merged
			.iter()
			.map(|(_, desired, _)| u32::from(*desired))
			.sum::<u32>();
		if wanted_total <= u32::from(h_live) {
			allocs.extend(merged.iter().map(|(_, desired, _)| *desired));
		} else {
			allocs.resize(merged.len(), 0);
			let mut budget = h_live.saturating_sub(summary_rows);
			for alloc in allocs.iter_mut().rev() {
				if budget == 0 {
					break;
				}
				*alloc = 1;
				budget -= 1;
			}
			let mut surplus_order = SmallVec::<usize, 16>::new();
			for index in (0..merged.len()).rev() {
				if !tool_flags[index] {
					surplus_order.push(index);
				}
			}
			for index in (0..merged.len()).rev() {
				if tool_flags[index] {
					surplus_order.push(index);
				}
			}
			for index in surplus_order {
				if budget == 0 {
					break;
				}
				let alloc = &mut allocs[index];
				if *alloc == 0 {
					continue;
				}
				let grant = merged[index].1.saturating_sub(*alloc).min(budget);
				*alloc += grant;
				budget -= grant;
			}
		}

		// An active head can pin later settled answers outside native-history
		// retirement. Keep the latest assistant prose represented by stealing
		// one row from the visible tail; thinking entries are a separate variant
		// and never qualify.
		let emergency_index =
			(wanted_total > u32::from(h_live) && self.blocks.retirement_batch().is_none())
				.then(|| {
					merged.iter().enumerate().rev().find_map(
						|(index, (ordinal, desired, is_settled))| {
							(*is_settled
								&& allocs[index] < *desired
								&& matches!(
									self.entries.get(ordinal),
									Some(Entry::Assistant(assistant)) if !assistant.body.text.trim().is_empty()
								))
							.then_some(index)
						},
					)
				})
				.flatten();
		let mut emergency_ordinal = None;
		if let Some(index) = emergency_index {
			if allocs[index] > 0 {
				emergency_ordinal = Some(merged[index].0);
			} else {
				let donor = (index + 1..merged.len())
					.find(|candidate| tool_flags[*candidate] && allocs[*candidate] > 0)
					.or_else(|| (index + 1..merged.len()).find(|candidate| allocs[*candidate] > 0));
				if let Some(donor) = donor {
					allocs[donor] -= 1;
					allocs[index] = 1;
					emergency_ordinal = Some(merged[index].0);
				}
			}
		}
		let settled_rows: u32 = merged
			.iter()
			.zip(&allocs)
			.filter(|((.., is_settled), _)| *is_settled)
			.map(|(_, alloc)| u32::from(*alloc))
			.sum();
		let tick_budget = u16::try_from(
			u32::from(h_live)
				.saturating_sub(u32::from(summary_rows))
				.saturating_sub(settled_rows),
		)
		.unwrap_or(u16::MAX);
		let sampled_height = |ordinal| {
			sampled
				.iter()
				.find(|(candidate, _)| *candidate == ordinal)
				.map_or(0, |(_, height)| *height)
		};
		let natural_height = |ordinal| {
			natural
				.iter()
				.find(|(candidate, _)| *candidate == ordinal)
				.map_or(1, |(_, height)| *height)
		};
		let plan = match admission {
			AdmissionMode::Allow => self
				.blocks
				.tick(tick_budget, sampled_height, natural_height),
			AdmissionMode::Defer => {
				self
					.blocks
					.tick_without_admission(tick_budget, sampled_height, natural_height)
			},
		};
		let transition = if self.reduced_motion {
			Duration::ZERO
		} else {
			Duration::from_millis(180)
		};
		for target in &plan.targets {
			if let Some(assistant) = self
				.live_assistant
				.as_mut()
				.filter(|assistant| assistant.ordinal == target.ordinal)
			{
				assistant.allocation = target.height;
				continue;
			}
			let Some(tool) = self
				.live_tools
				.iter_mut()
				.find(|tool| tool.ordinal == target.ordinal)
			else {
				continue;
			};
			if tool.target_height == target.height {
				continue;
			}
			if target.height > tool.target_height && tool.body_folded {
				tool.body_folded = false;
				tool
					.card_ui
					.update_component::<ToolCard>(LIVE_TOOL_CARD_ID, |card| card.set_folded(false));
			}
			let duration = if self.reduced_motion { "0ms" } else { "180ms" };
			tool
				.card_ui
				.set_prop(LIVE_TOOL_CARD_ID, Prop::Anim, duration);
			tool
				.card_ui
				.set_prop(LIVE_TOOL_CARD_ID, Prop::H, target.height);
			tool.target_height = target.height;
			tool.target_changed_at = elapsed;
		}
		for tool in &mut self.live_tools {
			let sampled_height = tool.card_ui.height();
			let settled = elapsed.saturating_sub(tool.target_changed_at) >= transition;
			if tool.target_height <= 2 && sampled_height == tool.target_height && settled {
				if !tool.body_folded {
					tool.body_folded = true;
					tool
						.card_ui
						.update_component::<ToolCard>(LIVE_TOOL_CARD_ID, |card| card.set_folded(true));
				}
			} else if tool.target_height > 2 && tool.body_folded {
				tool.body_folded = false;
				tool
					.card_ui
					.update_component::<ToolCard>(LIVE_TOOL_CARD_ID, |card| card.set_folded(false));
			}
		}
		let visible = plan.overflow.as_ref().map(|overflow| &overflow.visible);
		// One allocation pass, then a bottom-anchored draw pass: when the
		// transcript does not fill the live area, blocks hug the composer (the
		// newest content sits directly above the editor, matching pi) instead
		// of stranding at the top of the viewport.
		let mut final_allocs = SmallVec::<u16, 16>::new();
		let mut total_rows = 0_u16;
		for ((ordinal, _, is_settled), alloc) in merged.iter().zip(&allocs) {
			let mut allocation = if *is_settled {
				*alloc
			} else if self
				.live_assistant
				.as_ref()
				.is_some_and(|assistant| assistant.ordinal == *ordinal)
			{
				self
					.live_assistant
					.as_ref()
					.map_or(0, |assistant| assistant.allocation)
			} else {
				self
					.live_tools
					.iter()
					.find(|tool| tool.ordinal == *ordinal)
					.map_or(0, |tool| tool.card_ui.height())
			};
			allocation = allocation.min(*alloc);
			if !*is_settled
				&& let Some(visible) = visible
				&& !visible.contains(ordinal)
			{
				allocation = 0;
			}
			allocation = allocation.min(h_live.saturating_sub(total_rows));
			total_rows = total_rows.saturating_add(allocation);
			final_allocs.push(allocation);
		}
		// The leading welcome banner pins to the top of the live area while the
		// conversation hugs the composer; the blank band between them shrinks
		// as messages accumulate and disappears once content fills the screen.
		// Leading notices (e.g. the history-cleared divider) ride along in the
		// top group, but only when a banner is actually present.
		let leading = merged
			.iter()
			.take_while(|(ordinal, _, is_settled)| {
				*is_settled
					&& matches!(
						self.entries.get(ordinal),
						Some(Entry::Welcome(_) | Entry::Notice { .. })
					)
			})
			.count();
		let top_len = merged[..leading]
			.iter()
			.rposition(|(ordinal, ..)| matches!(self.entries.get(ordinal), Some(Entry::Welcome(_))))
			.map_or(0, |last| last + 1);
		let top_rows: u16 = final_allocs.iter().take(top_len).sum();
		let bottom_rows = total_rows.saturating_sub(top_rows);
		let mut y = 0_u16;
		self.live_layout.clear();
		for (index, ((ordinal, desired, is_settled), allocation)) in
			merged.iter().zip(&final_allocs).enumerate()
		{
			let allocation = *allocation;
			if index == top_len {
				y = y.max(h_live.saturating_sub(bottom_rows));
			}
			if allocation == 0 {
				continue;
			}
			if *is_settled && matches!(self.entries.get(ordinal), Some(Entry::Tool(_))) {
				self.live_layout.push((*ordinal, y, allocation));
			}
			if *is_settled && emergency_ordinal == Some(*ordinal) {
				self.draw_settled_assistant_emergency(*ordinal, y, content_width);
			} else if *is_settled {
				self.draw_settled_clipped(*ordinal, y, allocation, *desired, content_width);
			} else if self
				.live_assistant
				.as_ref()
				.is_some_and(|assistant| assistant.ordinal == *ordinal)
			{
				self.draw_live_assistant_clipped(*ordinal, y, allocation, content_width);
			} else {
				self.live_layout.push((*ordinal, y, allocation));
				self.draw_live_tool_clipped(*ordinal, y, allocation, content_width);
			}
			y = y.saturating_add(allocation);
		}
		if layout_overflow && summary_rows > 0 {
			let hidden = allocs.iter().filter(|allocation| **allocation == 0).count();
			let summary_y = h_live.saturating_sub(1);
			self.frame.fill(
				Rect::new(0, summary_y, content_width, summary_rows),
				panel_style(self.ctx.theme),
			);
			let label = sf!("+{hidden} blocks");
			draw_line(&mut self.frame, 1, summary_y, content_width.saturating_sub(2), &[Span::new(
				&label,
				ink(self.ctx.theme.muted).bold(),
			)]);
		}
		let mut chrome_y = h_live;
		if error_rows > 0
			&& let Some(error) = self.pinned_error.as_deref()
		{
			draw_flowed(
				&mut self.frame,
				Rect::new(1, chrome_y, content_width.saturating_sub(2), error_rows),
				&[Span::new(error, ink(self.ctx.theme.err))],
			);
			chrome_y = chrome_y.saturating_add(error_rows);
		}
		if download_rows > 0
			&& let Some(activity) = self.download_activity.as_ref()
		{
			draw_line(&mut self.frame, 1, chrome_y, content_width.saturating_sub(2), &[Span::new(
				&activity.label,
				ink(self.ctx.theme.muted),
			)]);
			chrome_y = chrome_y.saturating_add(download_rows);
		}
		if todo_rows > 0 {
			draw_line(&mut self.frame, 1, chrome_y, content_width.saturating_sub(2), &[Span::new(
				"TODO",
				ink(self.ctx.theme.accent).bold(),
			)]);
			let lines = if self.todo_expanded {
				&self.todo_lines
			} else {
				&self.todo_collapsed_lines
			};
			for (offset, line) in lines
				.iter()
				.take(usize::from(todo_rows.saturating_sub(1)))
				.enumerate()
			{
				draw_line(
					&mut self.frame,
					1,
					chrome_y
						.saturating_add(u16::try_from(offset).unwrap_or(u16::MAX))
						.saturating_add(1),
					content_width.saturating_sub(2),
					&[Span::new(line.as_str(), ink(self.ctx.theme.muted))],
				);
			}
		}
		if self.is_working() {
			self.draw_working_owned(working_y, elapsed);
		} else if let Some(recap) = self.idle_recap.as_deref() {
			draw_line(&mut self.frame, 1, working_y, content_width.saturating_sub(2), &[Span::new(
				recap,
				ink(self.ctx.theme.muted).dim().italic(),
			)]);
		}
		if self
			.celebration_until
			.is_some_and(|deadline| elapsed < deadline)
		{
			draw_quota_celebration(
				&mut self.frame,
				title_y,
				elapsed,
				self.ctx.charset,
				self.ctx.theme,
			);
		}
		if let Some(visualizer) = self.live_voice.as_ref() {
			draw_live_voice_visualizer(
				&mut self.frame,
				Rect::new(0, editor_y, content_width, LIVE_VOICE_ROWS.min(editor_height)),
				visualizer,
				elapsed,
				&self.ctx,
			);
		} else {
			self
				.frame
				.blit(self.editor_ui.frame(), 0, editor_height, 0, editor_y);
		}
		let frame_size = self.frame.size();
		let rails = if let Some(attribution) = self.attribution.as_ref() {
			Bands::compose_with_attribution(
				&mut self.frame,
				&mut self.slots,
				frame_size,
				attribution,
				self.ctx.theme,
			)
		} else {
			Bands::compose(&mut self.frame, &mut self.slots, frame_size)
		};
		self.slot_right_inset = rails.right;
		self.last_editor_height = editor_height;
		self.drawn_live = self.live_revision;
		let mut damage = SmallVec::new();
		damage.push((0, viewport.height));
		ViewportFrame { frame: &self.frame, damage }
	}

	/// Selects zero-duration allocation transitions for reduced-motion hosts.
	pub fn set_reduced_motion(&mut self, reduced: bool) {
		if self.reduced_motion == reduced {
			return;
		}
		self.reduced_motion = reduced;
		let now = self.started_at.elapsed();
		let duration = if reduced { "0ms" } else { "180ms" };
		for tool in &mut self.live_tools {
			if reduced {
				let sampled = tool.card_ui.height();
				tool.card_ui.set_prop(LIVE_TOOL_CARD_ID, Prop::H, sampled);
			}
			tool
				.card_ui
				.set_prop(LIVE_TOOL_CARD_ID, Prop::Anim, duration);
			if reduced {
				tool
					.card_ui
					.set_prop(LIVE_TOOL_CARD_ID, Prop::H, tool.target_height);
				tool.target_changed_at = now;
			}
		}
	}

	/// Enables or disables paced grapheme reveal for streamed assistant text.
	///
	/// Disabling the controller immediately flushes any buffered suffix.
	pub fn set_smooth_streaming(&mut self, smooth: bool) {
		if self.smooth_streaming == smooth {
			return;
		}
		self.smooth_streaming = smooth;
		if !smooth
			&& self
				.live_assistant
				.as_mut()
				.is_some_and(LiveAssistant::flush)
		{
			self.bump_live();
		}
	}

	/// Renders the next pressure-required finalized prefix or pending replay.
	pub fn retirement_batch(&mut self, viewport: Size) -> Option<RetirementBatch> {
		self.retirement_batch_with(viewport, RetirementPolicy::Pressure)
	}

	/// Renders the complete eligible prefix for graceful shutdown.
	pub fn flush_retirement_batch(&mut self, viewport: Size) -> Option<RetirementBatch> {
		self.retirement_batch_with(viewport, RetirementPolicy::Flush)
	}

	fn retirement_batch_with(
		&mut self,
		viewport: Size,
		policy: RetirementPolicy,
	) -> Option<RetirementBatch> {
		if let Some(replay) = self.replay {
			return Some(self.render_replay_batch(replay, viewport.width.max(1)));
		}
		let elapsed = self.started_at.elapsed();
		let h_live = self.chrome_layout(viewport, elapsed).h_live;
		let content_width = self.content_width(viewport);
		let live_end = self.blocks.frontier() + self.blocks.live_count();
		let eligible = self.blocks.retirement_batch();

		// Measure the complete uncommitted demand.
		let mut prefix = SmallVec::<u32, 8>::new();
		let mut total = 0_u32;
		for ordinal in self.blocks.frontier()..live_end {
			let height = match self.entries.get_mut(&BlockOrdinal(ordinal)) {
				Some(entry)
					if self.hide_thinking
						&& self.hidden_thinking_label.is_none()
						&& matches!(entry, Entry::Thinking(_)) =>
				{
					0
				},
				Some(entry) if self.hide_tools && matches!(entry, Entry::Tool(_)) => 0,
				Some(entry) => {
					Self::resize_entry(entry, content_width, &self.ctx);
					u32::from(Self::entry_height(entry, content_width, self.ctx.charset))
				},
				None => 0,
			};
			if eligible
				.as_ref()
				.is_some_and(|range| ordinal >= range.start && ordinal < range.end)
			{
				prefix.push(height);
			}
			total = total.saturating_add(height);
		}
		if let Some(assistant) = self
			.live_assistant
			.as_ref()
			.filter(|assistant| !(self.hide_thinking && assistant.thinking))
		{
			let wanted = assistant.height();
			total = total.saturating_add(u32::from(assistant.allocation.max(wanted)));
		}
		for tool in &self.live_tools {
			if self.hide_tools {
				continue;
			}
			let wanted = if tool.expanded {
				tool.view.height().saturating_add(2).max(1)
			} else {
				1
			};
			total = total.saturating_add(u32::from(tool.card_ui.height().max(wanted)));
		}

		let live_count = self.blocks.live_count();
		let row_pressure = total > u32::from(h_live);
		let pressured = row_pressure || live_count >= MAX_LIVE_BLOCKS;
		let width = viewport.width.max(1);
		let band_rows = self.band_rows(width);

		// Pressure retires the longest finalized prefix whose remainder still
		// fills the viewport, so committing rows never blanks the screen: a
		// block taller than the live area stays visible (tail-clipped) until
		// newer content can replace it. Only the block-count memory bound
		// overrides that floor. Flush takes every currently eligible head.
		let commit = eligible
			.filter(|_| policy != RetirementPolicy::Pressure || pressured)
			.and_then(|eligible| {
				let mut end = eligible.start;
				let mut freed = 0_u32;
				for height in &prefix {
					let taken = end - eligible.start;
					let count_pressured = live_count.saturating_sub(taken) >= MAX_LIVE_BLOCKS;
					let remainder_fills_viewport =
						total.saturating_sub(freed).saturating_sub(*height) >= u32::from(h_live);
					if policy == RetirementPolicy::Pressure
						&& !count_pressured
						&& !remainder_fills_viewport
					{
						break;
					}
					freed = freed.saturating_add(*height);
					end += 1;
				}
				(end > eligible.start).then_some(eligible.start..end)
			});
		if let Some(range) = commit {
			let mut batch = self.render_retirement_batch(range, width, RetirementKind::Commit);
			// Resident band rows are older than every commit: they must enter
			// native history first or scrollback order breaks.
			if band_rows > 0 {
				let body = mem::replace(&mut batch.frame, Frame::new(Size::new(width, 0)));
				match self.prepend_band(body, band_rows, width) {
					Some(frame) => batch.frame = frame,
					// The combined height overflows a frame: retire the band
					// alone; the commit re-offers on the next paint.
					None => return Some(self.band_overflow_batch(band_rows, width)),
				}
			}
			return Some(batch);
		}
		if band_rows > 0 {
			// Live content growing into the resident band pushes its oldest
			// rows into native history before a paint can cover them.
			let demand = u16::try_from(total.min(u32::from(h_live))).expect("clamped to h_live");
			let overflow = band_rows.saturating_sub(h_live.saturating_sub(demand));
			if overflow > 0 {
				return Some(self.band_overflow_batch(overflow, width));
			}
		}
		None
	}

	/// Height of the resident band when it matches the current width; a
	/// stale-width band is dropped (the pending settled replay re-derives it).
	fn band_rows(&mut self, width: u16) -> u16 {
		match self.band.as_ref() {
			Some(band) if band.size().width == width => band.size().height,
			Some(_) => {
				self.band = None;
				0
			},
			None => 0,
		}
	}

	/// Returns `[band ‖ commit]` as one retirement frame, or `None` when the
	/// combined height exceeds the frame limit. Consumes the band on success.
	fn prepend_band(&mut self, commit: Frame, band_rows: u16, width: u16) -> Option<Frame> {
		let height = band_rows.checked_add(commit.size().height)?;
		let band = self.band.take().expect("band rows imply a resident band");
		let mut frame = Frame::new(Size::new(width, height));
		frame.blit(&band, 0, band_rows, 0, 0);
		frame.blit(&commit, 0, commit.size().height, 0, band_rows);
		Some(frame)
	}

	/// Splits the oldest `rows` off the resident band into a standalone
	/// retirement frame, keeping the remainder resident.
	fn band_overflow_batch(&mut self, rows: u16, width: u16) -> RetirementBatch {
		let band = self.band.take().expect("overflow requires a resident band");
		let mut frame = Frame::new(Size::new(width, rows));
		frame.blit(&band, 0, rows, 0, 0);
		let kept = band.size().height.saturating_sub(rows);
		if kept > 0 {
			let mut rest = Frame::new(Size::new(width, kept));
			rest.blit(&band, rows, kept, 0, 0);
			self.band = Some(rest);
		}
		let frontier = self.blocks.frontier();
		RetirementBatch {
			range: frontier..frontier,
			frame,
			replay_frames: None,
			kind: RetirementKind::Band,
		}
	}

	fn render_replay_batch(&mut self, replay: Replay, width: u16) -> RetirementBatch {
		let mut frames = Vec::new();
		for ordinal in 0..replay.end {
			let Some(entry) = self.entries.get_mut(&BlockOrdinal(ordinal)) else {
				continue;
			};
			if self.hide_thinking
				&& self.hidden_thinking_label.is_none()
				&& matches!(entry, Entry::Thinking(_))
				|| self.hide_tools && matches!(entry, Entry::Tool(_))
			{
				continue;
			}
			Self::resize_entry(entry, width, &self.ctx);
			let height = Self::replay_entry_height(entry, width, self.ctx.charset);
			if height == 0 {
				continue;
			}
			let mut frame = Frame::new(Size::new(width, height));
			frame.fill(Rect::new(0, 0, width, height), base_style(self.ctx.theme));
			Self::draw_replay_entry(&mut frame, entry, 0, width, &self.ctx);
			frames.push(frame);
		}
		self.split_replay_band(&mut frames, width);
		RetirementBatch {
			range:         0..replay.end,
			frame:         Frame::new(Size::new(width, 0)),
			replay_frames: Some(frames),
			kind:          RetirementKind::Replay(replay.mode),
		}
	}

	/// Moves the replay's trailing rows into the resident band so the
	/// viewport's leading blank space shows the transcript tail; only the
	/// remainder scrolls into native history.
	///
	/// The scene owning the split is what keeps the tail on screen: rows the
	/// renderer moved into blank viewport space without scene knowledge were
	/// erased by the very next history-neutral paint.
	fn split_replay_band(&mut self, frames: &mut Vec<Frame>, width: u16) {
		// Rows the outgoing band covered are reusable space: the replay
		// re-derives their content from `entries`.
		let prior = match self.band.take() {
			Some(band) if band.size().width == width => band.size().height,
			_ => 0,
		};
		let mut available = prior;
		if self.frame.size().width == width {
			let height = self.frame.size().height;
			while available < height
				&& (0..width).all(|column| {
					matches!(self.frame.cell(column, available).content(), CellContent::Blank)
				}) {
				available += 1;
			}
		} else {
			available = 0;
		}
		let total: usize = frames
			.iter()
			.map(|frame| usize::from(frame.size().height))
			.sum();
		let moved = usize::from(available).min(total);
		let Ok(band_height) = u16::try_from(moved) else {
			return;
		};
		if band_height == 0 {
			return;
		}
		let mut band = Frame::new(Size::new(width, band_height));
		band.fill(Rect::new(0, 0, width, band_height), base_style(self.ctx.theme));
		let mut remaining = band_height;
		while remaining > 0 {
			let source = frames
				.last_mut()
				.expect("moved rows come from replay frames");
			let height = source.size().height;
			let take = height.min(remaining);
			remaining -= take;
			band.blit(source, height - take, take, 0, remaining);
			if take == height {
				frames.pop();
			} else {
				source.resize_height(height - take, base_style(self.ctx.theme));
			}
		}
		self.band = Some(band);
	}

	fn render_retirement_batch(
		&mut self,
		range: Range<u64>,
		width: u16,
		kind: RetirementKind,
	) -> RetirementBatch {
		let start = range.start;
		let mut end = start;
		let mut frame_height = 0_u32;
		for ordinal in range {
			let entry_height = if let Some(entry) = self.entries.get_mut(&BlockOrdinal(ordinal))
				&& !(self.hide_thinking
					&& self.hidden_thinking_label.is_none()
					&& matches!(entry, Entry::Thinking(_)))
				&& !(self.hide_tools && matches!(entry, Entry::Tool(_)))
			{
				Self::resize_entry(entry, width, &self.ctx);
				u32::from(Self::entry_height(entry, width, self.ctx.charset))
			} else {
				0
			};
			if end > start && frame_height + entry_height > u32::from(u16::MAX) {
				break;
			}
			frame_height += entry_height;
			end = ordinal + 1;
		}
		let range = start..end;
		let height = u16::try_from(frame_height).expect("retirement frame height is capped");
		let mut frame = Frame::new(Size::new(width, height));
		frame.fill(Rect::new(0, 0, width, height), base_style(self.ctx.theme));
		let mut y = 0_u16;
		for ordinal in range.clone() {
			let Some(entry) = self.entries.get(&BlockOrdinal(ordinal)) else {
				continue;
			};
			if self.hide_thinking
				&& self.hidden_thinking_label.is_none()
				&& matches!(entry, Entry::Thinking(_))
				|| self.hide_tools && matches!(entry, Entry::Tool(_))
			{
				continue;
			}
			y = y.saturating_add(Self::draw_entry(
				&mut frame,
				entry,
				y,
				width,
				&self.ctx,
				Duration::MAX,
			));
		}
		RetirementBatch { range, frame, replay_frames: None, kind }
	}

	/// Starts one complete replay snapshot with `mode` without changing block
	/// phases or the logical commit frontier.
	pub fn begin_history_replay(&mut self, mode: HistoryReplay) {
		let end = self.blocks.frontier();
		self.replay = (end > 0 || mode == HistoryReplay::Rebuild).then_some(Replay { end, mode });
		self.bump_live();
	}

	/// Cancels an unoffered replay so shutdown flushes only unretired rows.
	pub fn begin_history_flush(&mut self) {
		self.replay = None;
		self.bump_live();
	}

	/// Acknowledges one successful commit or replay transaction.
	pub fn mark_retired(&mut self, batch: &RetirementBatch) {
		match batch.kind {
			RetirementKind::Commit => self.blocks.mark_committed(batch.range.end),
			RetirementKind::Replay(_) => self.replay = None,
			// The band shrinks when its batch is rendered; success needs no ack
			// and failure poisons the renderer, ending the session.
			RetirementKind::Band => {},
		}
	}

	fn enqueue_final(&mut self, entry: Entry) -> BlockOrdinal {
		let ordinal = self.blocks.create();
		self.entries.insert(ordinal, entry);
		self.blocks.finalize(ordinal);
		ordinal
	}

	fn draw_live_assistant_clipped(
		&mut self,
		ordinal: BlockOrdinal,
		y: u16,
		height: u16,
		width: u16,
	) {
		let Some(message) = self
			.live_assistant
			.as_mut()
			.filter(|message| message.ordinal == ordinal)
		else {
			return;
		};
		message.resize(width);
		let natural = message.height();
		self
			.frame
			.blit(message.view.frame(), natural.saturating_sub(height), height, 0, y);
	}

	/// Draws the first prose row of a settled assistant under emergency
	/// pressure, never a thinking marker or the assistant's clipped tail.
	fn draw_settled_assistant_emergency(&mut self, ordinal: BlockOrdinal, y: u16, width: u16) {
		let Some(Entry::Assistant(assistant)) = self.entries.get(&ordinal) else {
			return;
		};
		let natural = assistant.body.height().saturating_add(1).max(1);
		let size = Size::new(width, natural);
		if self.clip_scratch.size() != size {
			self.clip_scratch = Frame::new(size);
		}
		self
			.clip_scratch
			.fill(Rect::new(0, 0, width, natural), base_style(self.ctx.theme));
		draw_rich(&mut self.clip_scratch, 0, &assistant.body, 0, width, self.ctx.theme);
		self.frame.blit(&self.clip_scratch, 0, 1, 0, y);
	}

	fn draw_live_tool_clipped(&mut self, ordinal: BlockOrdinal, y: u16, height: u16, _width: u16) {
		let Some(tool) = self.live_tools.iter().find(|tool| tool.ordinal == ordinal) else {
			return;
		};
		self.frame.blit(tool.card_ui.frame(), 0, height, 0, y);
	}

	/// Draws one settled snapshot into the live viewport, keeping its latest
	/// content rows when the allocation is smaller than its natural height.
	///
	/// Every entry renders one trailing spacer row; clipping drops that spacer
	/// first so a one-row allocation still shows content (the latest reasoning
	/// or prose row) instead of a blank line.
	fn draw_settled_clipped(
		&mut self,
		ordinal: BlockOrdinal,
		y: u16,
		height: u16,
		natural: u16,
		width: u16,
	) {
		let Some(entry) = self.entries.get(&ordinal) else {
			return;
		};
		let now = self.started_at.elapsed();
		if height >= natural {
			Self::draw_entry(&mut self.frame, entry, y, width, &self.ctx, now);
			return;
		}
		let size = Size::new(width, natural);
		if self.clip_scratch.size() != size {
			self.clip_scratch = Frame::new(size);
		}
		self
			.clip_scratch
			.fill(Rect::new(0, 0, width, natural), base_style(self.ctx.theme));
		Self::draw_entry(&mut self.clip_scratch, entry, 0, width, &self.ctx, now);
		self.frame.blit(
			&self.clip_scratch,
			natural.saturating_sub(height.saturating_add(1)),
			height,
			0,
			y,
		);
	}

	fn refresh_live_tool_card(tool: &mut LiveTool, width: u16, ctx: &UiContext) {
		let mut body = Vec::with_capacity(tool.images.len().saturating_add(1));
		body.push(tool.view.body(ctx));
		for image in &tool.images {
			let (cols, rows) = tool_image_box(image, width);
			body.push(Cached::new(Box::new(
				Img::new()
					.with(Prop::Src, image.source.clone())
					.with(Prop::W, cols)
					.with(Prop::H, rows),
			)));
		}
		tool
			.card_ui
			.update_component::<ToolCard>(LIVE_TOOL_CARD_ID, |card| {
				let mut dirty = false;
				dirty |= card.set_state(ToolState::Streaming);
				dirty |= card.set_flush(tool.view.chrome == ViewChrome::Flush);
				dirty |= card.replace_body(body);
				dirty
			});
		let _ = tool.card_ui.set_context(ctx.clone());
		if tool.card_ui.frame().size().width != width {
			tool.card_ui.resize(width);
		}
	}

	const fn message_width(width: u16) -> u16 {
		let narrowed = width.saturating_sub(3);
		if narrowed == 0 { 1 } else { narrowed }
	}

	const fn tool_view_width(width: u16) -> u16 {
		let narrowed = width.saturating_sub(4);
		if narrowed == 0 { 1 } else { narrowed }
	}

	fn resize_entry(entry: &mut Entry, width: u16, ctx: &UiContext) {
		match entry {
			Entry::User(user) => user.body.resize(width.max(1), ctx),
			Entry::Assistant(assistant) => assistant.body.resize(width.max(1), ctx),
			Entry::Usage(_) => {},
			Entry::Thinking(thinking) => thinking.body.resize(width.max(1), ctx),
			Entry::Peer { .. } => {},
			Entry::Tool(tool) => tool.view.resize(Self::tool_view_width(width), ctx),
			Entry::Retained(frame) => frame.view.resize(Self::tool_view_width(width), ctx),
			Entry::Welcome(_) | Entry::Compaction(_) | Entry::Notice { .. } => {},
		}
	}

	fn entry_height(entry: &Entry, width: u16, charset: Charset) -> u16 {
		match entry {
			Entry::User(user) => {
				if user.body.text.trim().is_empty() && user.chips.is_empty() {
					0
				} else if user.queued {
					queued_user_height(user)
				} else {
					user
						.body
						.height()
						.saturating_add(u16::from(!user.chips.is_empty()))
						.saturating_add(1)
				}
			},
			Entry::Welcome(entry) => welcome_height(&entry.banner, width, charset).saturating_add(1),
			Entry::Assistant(assistant) => {
				if assistant.body.text.trim().is_empty() {
					0
				} else {
					assistant.body.height().saturating_add(1)
				}
			},
			Entry::Usage(usage) => u16::from(usage.visible).saturating_add(u16::from(usage.visible)),
			Entry::Thinking(thinking) => {
				if thinking.body.text.trim().is_empty() {
					0
				} else {
					thinking.body.height().saturating_add(1)
				}
			},
			Entry::Peer { title, detail } => flowed_height(title, width.saturating_sub(4))
				.saturating_add(
					detail
						.as_ref()
						.map_or(0, |detail| flowed_height(detail, width.saturating_sub(4))),
				)
				.saturating_add(2),
			Entry::Tool(tool) => tool_height(tool, width).saturating_add(1),
			Entry::Compaction(compaction) => {
				flowed_height(&compaction.label, width.saturating_sub(2)).saturating_add(1)
			},
			Entry::Retained(frame) => frame.view.height().saturating_add(1),
			Entry::Notice { text, .. } => {
				if text.trim().is_empty() {
					0
				} else {
					flowed_height(text, width.saturating_sub(2)).saturating_add(1)
				}
			},
		}
	}

	fn replay_entry_height(entry: &Entry, width: u16, charset: Charset) -> u16 {
		match entry {
			Entry::Assistant(assistant) => {
				if assistant.body.text.trim().is_empty() {
					0
				} else {
					assistant.body.height().saturating_add(1)
				}
			},
			_ => Self::entry_height(entry, width, charset),
		}
	}

	fn draw_replay_entry(
		frame: &mut Frame,
		entry: &Entry,
		y: u16,
		width: u16,
		ctx: &UiContext,
	) -> u16 {
		match entry {
			Entry::Assistant(assistant) => {
				if assistant.body.text.trim().is_empty() {
					0
				} else {
					draw_rich(frame, y, &assistant.body, 0, width, ctx.theme).saturating_add(1)
				}
			},
			_ => Self::draw_entry(frame, entry, y, width, ctx, Duration::MAX),
		}
	}

	fn draw_entry(
		frame: &mut Frame,
		entry: &Entry,
		y: u16,
		width: u16,
		ctx: &UiContext,
		now: Duration,
	) -> u16 {
		match entry {
			Entry::User(user) => {
				if user.body.text.trim().is_empty() && user.chips.is_empty() {
					0
				} else if user.queued {
					draw_user_queued(frame, y, user, ctx)
				} else {
					draw_user(frame, y, user, ctx)
				}
			},
			Entry::Welcome(entry) => draw_welcome(frame, y, entry, width, ctx, now).saturating_add(1),
			Entry::Assistant(assistant) => {
				if assistant.body.text.trim().is_empty() {
					0
				} else {
					draw_rich(frame, y, &assistant.body, 0, width, ctx.theme).saturating_add(1)
				}
			},
			Entry::Usage(usage) => {
				if !usage.visible {
					0
				} else {
					let label = usage.display_label();
					draw_flowed(
						frame,
						Rect::new(1, y, width.saturating_sub(2), frame.size().height.saturating_sub(y)),
						&[Span::new(label.as_str(), ink(ctx.theme.muted))],
					)
					.saturating_add(1)
				}
			},
			Entry::Thinking(thinking) => {
				if thinking.body.text.trim().is_empty() {
					0
				} else {
					draw_rich(frame, y, &thinking.body, 0, width, ctx.theme).saturating_add(1)
				}
			},
			Entry::Peer { title, detail } => {
				let body = detail.as_deref().unwrap_or("");
				let used = draw_flowed(
					frame,
					Rect::new(2, y, width.saturating_sub(4), frame.size().height.saturating_sub(y)),
					&[
						Span::new(title, ink(ctx.theme.secondary).bold()),
						Span::new("\n", ink(ctx.theme.secondary)),
						Span::new(body, ink(ctx.theme.fg)),
					],
				);
				used.saturating_add(1)
			},
			Entry::Tool(tool) => draw_tool(frame, y, width, tool, ctx).saturating_add(1),
			Entry::Compaction(compaction) => draw_flowed(
				frame,
				Rect::new(1, y, width.saturating_sub(2), frame.size().height.saturating_sub(y)),
				&[Span::new(&compaction.label, ink(ctx.theme.info).bold())],
			)
			.saturating_add(1),
			Entry::Retained(entry) => {
				let height = entry.view.height();
				if height > 0 {
					frame.blit(entry.view.rendered.frame(), 0, height, 1, y);
				}
				height.saturating_add(1)
			},
			Entry::Notice { text, error } => {
				if text.trim().is_empty() {
					return 0;
				}
				let style = if *error {
					ink(ctx.theme.err)
				} else {
					ink(ctx.theme.muted).italic()
				};
				draw_flowed(
					frame,
					Rect::new(1, y, width.saturating_sub(2), frame.size().height.saturating_sub(y)),
					&[Span::new(text, style)],
				)
				.saturating_add(1)
			},
		}
	}

	fn draw_working_owned(&mut self, y: u16, elapsed: Duration) {
		draw_working_impl(
			&mut self.frame,
			y,
			elapsed,
			self.ctx.charset.icon(Icon::Cancellable),
			self.ctx.native_decor,
			self.ctx.theme,
		);
	}
}

fn download_label(progress: &ModelDownloadProgress) -> Str {
	let mut label = fmts_mut!("model · {}", progress.label);
	if let Some(total) = progress.total.filter(|total| *total > 0) {
		let percent = progress.downloaded.saturating_mul(100) / total;
		let _ = write!(label, " · {}/{} bytes · {percent}%", progress.downloaded.min(total), total);
	} else {
		let _ = write!(label, " · {} bytes", progress.downloaded);
	}
	if progress.complete {
		label.push_str(" · ready");
	}
	label.freeze()
}

fn draw_working_impl(
	frame: &mut Frame,
	y: u16,
	elapsed: Duration,
	hint: &str,
	native: bool,
	theme: Theme,
) {
	if y >= frame.size().height || frame.size().width < 4 {
		return;
	}
	let label = "Working";
	let start = u16::from(frame.size().width >= 50);
	let mut column = start;
	let length = visible_width(hint)
		.saturating_add(visible_width(label))
		.saturating_add(1);
	let shimmer = Shimmer::new(elapsed, SHIMMER_PERIOD, length);
	let right = frame.size().width.saturating_sub(1);
	for (text, high) in [(hint, theme.info), (" ", theme.ok), (label, theme.ok)] {
		for grapheme in xutf::graphemes_str(text) {
			if column >= right {
				break;
			}
			let style = if native {
				ink(high)
			} else {
				shimmer.pick(column - start, ink(theme.border), ink(theme.muted), ink(high))
			};
			let next = frame.put(column, y, grapheme, style);
			if next == column {
				break;
			}
			column = next;
		}
	}
	if native {
		frame.push_decor(Decor {
			rect: Rect::new(start, y, column.saturating_sub(start), 1),
			kind: DecorKind::Shimmer { period: SHIMMER_PERIOD },
		});
	}
}

fn draw_quota_celebration(
	frame: &mut Frame,
	y: u16,
	elapsed: Duration,
	charset: Charset,
	theme: Theme,
) {
	if y >= frame.size().height || frame.size().width < 12 {
		return;
	}
	let glyphs = if charset == Charset::Ascii {
		["*", "+", "."]
	} else {
		["✦", "✧", "·"]
	};
	let phase = usize::try_from(elapsed.as_millis() / 100).unwrap_or(0);
	for index in 0..6_u16 {
		let x = frame
			.size()
			.width
			.saturating_sub(2 + index.saturating_mul(2));
		if x == 0 {
			break;
		}
		let glyph = glyphs[(phase + usize::from(index)) % glyphs.len()];
		frame.put(
			x,
			y,
			glyph,
			ink(if index.is_multiple_of(2) {
				theme.accent
			} else {
				theme.ok
			}),
		);
	}
}

fn draw_user(frame: &mut Frame, y: u16, user: &UserEntry, ctx: &UiContext) -> u16 {
	draw_user_body(frame, y, &user.body, &user.chips, ctx)
}
/// Rows used by one pending queued user message: hinted header, optional
/// chip row, dim flowed body, trailing spacer.
fn queued_user_height(user: &UserEntry) -> u16 {
	flowed_height(&user.body.text, user.body.width.saturating_sub(2).max(1))
		.saturating_add(u16::from(!user.chips.is_empty()))
		.saturating_add(2)
}

/// Paints one pending queued user message: a dim `Queued` header with the
/// dequeue hint, then the message body flowed dim under a 2-column indent.
fn draw_user_queued(frame: &mut Frame, y: u16, user: &UserEntry, ctx: &UiContext) -> u16 {
	let dim = ink(ctx.theme.muted).dim();
	let mut at = y;
	let mut x = frame.put(1, at, ctx.charset.icon(Icon::Selected), dim);
	x = frame.put(x, at, " Queued", dim);
	if let Some(hint) = &user.hint {
		frame.put(x, at, hint, dim);
	}
	at = at.saturating_add(1);
	if !user.chips.is_empty() {
		let mut cx = frame.put(2, at, ctx.charset.icon(Icon::Image), dim);
		for chip in &user.chips {
			cx = frame.put(cx, at, " ", dim);
			cx = frame.put(cx, at, chip, dim);
		}
		at = at.saturating_add(1);
	}
	let width = user.body.width.saturating_sub(2).max(1);
	let used =
		draw_flowed(frame, Rect::new(2, at, width, frame.size().height.saturating_sub(at)), &[
			Span::new(&user.body.text, dim),
		]);
	at.saturating_sub(y).saturating_add(used).saturating_add(1)
}

fn draw_user_body(
	frame: &mut Frame,
	y: u16,
	body: &RichText,
	chips: &[Str],
	ctx: &UiContext,
) -> u16 {
	let mut at = y;
	if !chips.is_empty() {
		let mut x = frame.put(1, at, ctx.charset.icon(Icon::Image), ink(ctx.theme.warn));
		for chip in chips {
			x = frame.put(x, at, " ", ink(ctx.theme.muted));
			x = frame.put(x, at, chip, ink(ctx.theme.warn).bold());
		}
		at = at.saturating_add(1);
	}
	let used = draw_rich(frame, at, body, 0, body.width, ctx.theme);
	at.saturating_sub(y).saturating_add(used).saturating_add(1)
}

fn draw_rich(frame: &mut Frame, y: u16, body: &RichText, x: u16, width: u16, theme: Theme) -> u16 {
	if let Some(view) = &body.view {
		let height = view.height();
		frame.blit(view.frame(), 0, height, x, y);
		height
	} else {
		draw_flowed(frame, Rect::new(x, y, width, frame.size().height.saturating_sub(y)), &[
			Span::new(&body.text, prose_style(theme)),
		])
	}
}

fn draw_tool(frame: &mut Frame, y: u16, width: u16, tool: &ToolEntry, ctx: &UiContext) -> u16 {
	let height = tool_height(tool, width);
	let rect = Rect::new(0, y, width, height);
	if tool.view.chrome == ViewChrome::Flush {
		let view_height = tool.view.height().min(height);
		if view_height > 0 {
			frame.blit(tool.view.rendered.frame(), 0, view_height, rect.x, y);
		}
		let mut row = y.saturating_add(view_height);
		let bottom = y.saturating_add(height);
		for image in &tool.images {
			let (cols, rows) = tool_image_box(image, width);
			if rows == 0 || row.saturating_add(rows) > bottom {
				break;
			}
			omp_tui::components::draw_image_inline(
				frame,
				ctx,
				rect.x,
				row,
				image.source.as_str(),
				cols,
				rows,
			);
			row = row.saturating_add(rows);
		}
		return height;
	}
	let state = match tool.terminal {
		ToolTerminal::Succeeded => ctx.theme.ok,
		ToolTerminal::Failed => ctx.theme.err,
		ToolTerminal::ArgsRejected => ctx.theme.warn,
		ToolTerminal::Aborted => ctx.theme.muted,
		ToolTerminal::Skipped => ctx.theme.secondary,
	};
	let style = ink(state);
	let leading = if tool.expanded && height >= 3 {
		ctx.charset.expander(true)
	} else {
		"  "
	};
	let x = frame.put(rect.x, y, leading, style);
	let label_width = rect.x.saturating_add(rect.width).saturating_sub(x);
	draw_line(frame, x, y, label_width, &[Span::new(&tool.label, style.bold())]);
	if height == 1 {
		return height;
	}
	let mut row = y.saturating_add(1);
	let bottom = y.saturating_add(height).saturating_sub(1);
	let view_height = tool.view.height().min(bottom.saturating_sub(row));
	if view_height > 0 {
		frame.blit(tool.view.rendered.frame(), 0, view_height, rect.x.saturating_add(2), row);
		row = row.saturating_add(view_height);
	}
	for image in &tool.images {
		let (cols, rows) = tool_image_box(image, width);
		if rows == 0 || row.saturating_add(rows) > bottom {
			break;
		}
		omp_tui::components::draw_image_inline(
			frame,
			ctx,
			rect.x.saturating_add(2),
			row,
			image.source.as_str(),
			cols,
			rows,
		);
		row = row.saturating_add(rows);
	}
	let (_, last, rail) = ctx.charset.guides(Border::Round);
	for rail_row in y.saturating_add(1)..bottom {
		frame.put(rect.x, rail_row, rail, style);
	}
	let mut bx = frame.put(rect.x, bottom, last, style);
	let mut buf = [0_u8; 4];
	let rule = ctx.charset.rule().encode_utf8(&mut buf);
	for _ in 0..rect.width.saturating_sub(2) {
		bx = frame.put(bx, bottom, rule, style);
	}
	height
}

/// Aspect-fit cell box for one tool image inside a card of `width` columns.
fn tool_image_box(image: &ToolImageEntry, width: u16) -> (u16, u16) {
	let interior = width.saturating_sub(4).min(TOOL_IMAGE_MAX_COLS);
	if interior == 0 {
		return (0, 0);
	}
	omp_tui::components::image_cell_box(image.px, interior, TOOL_IMAGE_MAX_ROWS)
}

fn tool_height(tool: &ToolEntry, width: u16) -> u16 {
	let image_rows = tool
		.images
		.iter()
		.fold(0_u16, |rows, image| rows.saturating_add(tool_image_box(image, width).1));
	if tool.view.chrome == ViewChrome::Flush {
		return tool.view.height().saturating_add(image_rows).max(1);
	}
	if !tool.expanded {
		return 1;
	}
	let body = tool.view.height().saturating_add(image_rows);
	if body == 0 { 1 } else { body.saturating_add(2) }
}

fn mask_keywords(mut text: String, accent: &KeywordAccent) -> String {
	for (start, end) in accent.matched_spans(&text).into_iter().rev() {
		text.replace_range(start..end, &"•".repeat(end - start));
	}
	text
}
/// Transcript welcome banner and its intro deadline on the scene timeline.
struct WelcomeEntry {
	banner:      crate::WelcomeBanner,
	/// Scene instant the intro gradient sweep ends; `ZERO` = resting frame.
	intro_until: Duration,
}

/// Banner box cap, matching pi's welcome card.
const WELCOME_MAX_WIDTH: u16 = 100;
/// Fixed logo-column width; dynamic labels truncate inside it.
const WELCOME_LEFT_COL: u16 = 26;
/// Narrow terminals use one generous column rather than two cramped ones.
const WELCOME_MIN_TWO_COL_WIDTH: u16 = 72;
/// Minimum width retained for the tips column.
const WELCOME_MIN_RIGHT_COL: u16 = 20;
/// Language-server rows shown before overflow is sliced.
const WELCOME_LSP_SLOTS: usize = 4;
/// Intro sweep length; afterwards the banner settles on the resting gradient.
const WELCOME_INTRO: Duration = Duration::from_millis(3000);
/// Full gradient rotations the intro sweeps through before settling.
const WELCOME_SWEEPS: f32 = 2.5;
/// Diagonal crossings of the shine highlight across the intro.
const WELCOME_SHINE_TRAVERSALS: f32 = 3.0;
/// Half-width of the shine band in gradient-t units.
const WELCOME_SHINE_HALF_WIDTH: f32 = 0.18;
/// Block-grid brand mark painted with the diagonal gradient.
const WELCOME_LOGO: [&str; 5] =
	["████████████", "   ██  ██   ", "   ██  ██   ", "   ▒▒  ██   ", "       ██   "];
/// Brand gradient stops (pink → purple → cyan), shared with pi's welcome logo.
const WELCOME_GRADIENT: [(f32, f32, f32); 3] =
	[(248.0, 79.0, 204.0), (147.0, 98.0, 244.0), (0.0, 219.0, 228.0)];

/// Resolves `(box_width, left_col, right_col)`; a zero right column collapses
/// the card to the logo column only.
fn welcome_columns(width: u16) -> (u16, u16, u16) {
	let box_width = width.saturating_sub(2).min(WELCOME_MAX_WIDTH);
	if box_width < 17 {
		return (box_width, 0, 0);
	}
	let content = box_width - 3;
	let left_min = 13; // "Welcome back!"
	let desired = WELCOME_LEFT_COL
		.min((content * 35 / 100).max(12))
		.max(left_min);
	if box_width >= WELCOME_MIN_TWO_COL_WIDTH && content > WELCOME_MIN_RIGHT_COL {
		let left = desired.min(content - WELCOME_MIN_RIGHT_COL);
		let right = content - left;
		if left >= left_min && right >= WELCOME_MIN_RIGHT_COL {
			return (box_width, left, right);
		}
	}
	(box_width, box_width - 2, 0)
}

/// Left-column row count: blank framing, greeting, logo, model, provider.
const fn welcome_left_rows(charset: Charset) -> u16 {
	match charset {
		Charset::Ascii => 5,
		Charset::Unicode | Charset::NerdFont => 5 + WELCOME_LOGO.len() as u16 + 1,
	}
}

/// Rows in the tips/LSP column.
fn welcome_right_rows(banner: &crate::WelcomeBanner) -> u16 {
	let lsp = banner.lsp_servers.len().clamp(1, WELCOME_LSP_SLOTS) as u16;
	// Tips header + 4 shortcuts + rule + LSP header + rows + trailing blank.
	8 + lsp
}

/// Full banner height, borders and tip line included.
fn welcome_height(banner: &crate::WelcomeBanner, width: u16, charset: Charset) -> u16 {
	let (box_width, left, right) = welcome_columns(width);
	if box_width < 17 || left == 0 {
		return 0;
	}
	let content = if right == 0 {
		welcome_left_rows(charset)
	} else {
		welcome_left_rows(charset).max(welcome_right_rows(banner))
	};
	content
		.saturating_add(2)
		.saturating_add(u16::from(banner.tip.is_some()))
}

/// Gradient phase and shine band for a normalized intro progress in `[0, 1)`.
///
/// Ease-out cubic: the sweep decelerates into the resting frame (phase 0)
/// while the shine crosses the diagonal at a steady pace and fades out.
fn welcome_intro_frame(progress: f32) -> (f32, Option<(f32, f32)>) {
	let eased = 1.0 - (1.0 - progress).powi(3);
	let phase = ((1.0 - eased) * WELCOME_SWEEPS).fract();
	let shine_pos = (progress * WELCOME_SHINE_TRAVERSALS).fract();
	let strength = (1.0 - eased).powf(1.5);
	(phase, Some((strength, shine_pos)))
}

/// Foreground for a normalized diagonal position `t`, compositing the sliding
/// shine highlight toward white.
fn welcome_gradient_color(t: f32, shine: Option<(f32, f32)>) -> Color {
	let stops = WELCOME_GRADIENT;
	let seg = (t * (stops.len() - 1) as f32).clamp(0.0, (stops.len() - 1) as f32);
	let index = (seg as usize).min(stops.len() - 2);
	let fraction = seg - index as f32;
	let (ar, ag, ab) = stops[index];
	let (br, bg, bb) = stops[index + 1];
	let mut r = ar + (br - ar) * fraction;
	let mut g = ag + (bg - ag) * fraction;
	let mut b = ab + (bb - ab) * fraction;
	if let Some((strength, pos)) = shine
		&& strength > 0.0
	{
		let intensity = (1.0 - (t - pos).abs() / WELCOME_SHINE_HALF_WIDTH).max(0.0) * strength;
		r += (255.0 - r) * intensity;
		g += (255.0 - g) * intensity;
		b += (255.0 - b) * intensity;
	}
	Color::Rgb(r.round() as u8, g.round() as u8, b.round() as u8)
}

/// Draws `text` centered inside `[x, x + width)`, truncating when oversized.
fn put_centered(frame: &mut Frame, x: u16, y: u16, width: u16, text: &str, style: Style) {
	let text_width = visible_width(text);
	let pad = width.saturating_sub(text_width) / 2;
	draw_line(frame, x.saturating_add(pad), y, width.saturating_sub(pad), &[Span::new(text, style)]);
}

/// Paints the gradient brand mark centered in the left column.
fn draw_welcome_logo(
	frame: &mut Frame,
	x: u16,
	y: u16,
	left_col: u16,
	phase: f32,
	shine: Option<(f32, f32)>,
) {
	let cols = WELCOME_LOGO
		.iter()
		.map(|line| line.chars().count())
		.max()
		.unwrap_or(1);
	let x_span = cols.saturating_sub(1).max(1) as f32;
	let y_span = (WELCOME_LOGO.len() - 1).max(1) as f32;
	let pad = left_col.saturating_sub(cols as u16) / 2;
	let mut glyph = [0_u8; 4];
	for (row, line) in WELCOME_LOGO.iter().enumerate() {
		for (col, character) in line.chars().enumerate() {
			if character == ' ' {
				continue;
			}
			let base = (col as f32 / x_span + row as f32 / y_span) / 2.0;
			let t = if phase == 0.0 {
				base
			} else {
				(base + phase).fract()
			};
			frame.put(
				x.saturating_add(pad).saturating_add(col as u16),
				y.saturating_add(row as u16),
				character.encode_utf8(&mut glyph),
				ink(welcome_gradient_color(t, shine)),
			);
		}
	}
}

/// Draws the welcome banner card: rounded dim border with an embedded title,
/// the gradient logo column, the tips/LSP column, and the tip footer.
/// Height always equals [`welcome_height`].
fn draw_welcome(
	frame: &mut Frame,
	y: u16,
	entry: &WelcomeEntry,
	width: u16,
	ctx: &UiContext,
	now: Duration,
) -> u16 {
	let banner = &entry.banner;
	let charset = ctx.charset;
	let theme = ctx.theme;
	let height = welcome_height(banner, width, charset);
	if height == 0 {
		return 0;
	}
	let (box_width, left_col, right_col) = welcome_columns(width);
	let (tl, tr, bl, br, h, v) = charset.border(Border::Round);
	let mut glyph = [0_u8; 4];
	let dim = ink(theme.border);
	let content_rows = height
		.saturating_sub(2)
		.saturating_sub(u16::from(banner.tip.is_some()));

	// Top border with the embedded `omp v<version>` title.
	let mut x = frame.put(0, y, tl.encode_utf8(&mut glyph), dim);
	for _ in 0..3 {
		x = frame.put(x, y, h.encode_utf8(&mut glyph), dim);
	}
	let title = fmts_mut!(" omp v{} ", banner.version);
	x = draw_line(frame, x, y, box_width.saturating_sub(x + 1), &[Span::new(
		title.as_str(),
		ink(theme.muted),
	)]);
	while x < box_width - 1 {
		x = frame.put(x, y, h.encode_utf8(&mut glyph), dim);
	}
	frame.put(box_width - 1, y, tr.encode_utf8(&mut glyph), dim);

	// Column frames.
	let left_x = 1;
	let right_x = left_x + left_col + 1;
	for row in 0..content_rows {
		let row_y = y + 1 + row;
		frame.put(0, row_y, v.encode_utf8(&mut glyph), dim);
		if right_col > 0 {
			frame.put(left_x + left_col, row_y, v.encode_utf8(&mut glyph), dim);
		}
		frame.put(box_width - 1, row_y, v.encode_utf8(&mut glyph), dim);
	}

	// Left column: greeting, gradient logo, model, provider — centered.
	let logo_rows = if charset == Charset::Ascii {
		0
	} else {
		WELCOME_LOGO.len() as u16
	};
	put_centered(frame, left_x, y + 2, left_col, "Welcome back!", ink(theme.fg).bold());
	if logo_rows > 0 {
		let intro_remaining = entry.intro_until.saturating_sub(now);
		let (phase, shine) = if intro_remaining.is_zero() {
			(0.0, None)
		} else {
			let progress =
				1.0 - (intro_remaining.as_secs_f32() / WELCOME_INTRO.as_secs_f32()).clamp(0.0, 1.0);
			welcome_intro_frame(progress)
		};
		draw_welcome_logo(frame, left_x, y + 4, left_col, phase, shine);
	}
	let model_y = if logo_rows > 0 {
		y + 5 + logo_rows
	} else {
		y + 4
	};
	put_centered(frame, left_x, model_y, left_col, banner.model.as_str(), ink(theme.muted));
	put_centered(frame, left_x, model_y + 1, left_col, banner.provider.as_str(), dim);

	// Right column: tips, rule, language servers.
	if right_col > 0 {
		let text_x = right_x + 1;
		let text_width = right_col.saturating_sub(1);
		let mut row_y = y + 1;
		draw_line(frame, text_x, row_y, text_width, &[Span::new("Tips", ink(theme.accent).bold())]);
		row_y += 1;
		for (symbol, hint) in [
			("#", " for prompt actions"),
			("/", " for commands"),
			("!", " to run shell"),
			("$", " to run python"),
		] {
			draw_line(frame, text_x, row_y, text_width, &[
				Span::new(symbol, dim),
				Span::new(hint, ink(theme.muted)),
			]);
			row_y += 1;
		}
		for column in 0..text_width.saturating_sub(1) {
			frame.put(text_x + column, row_y, h.encode_utf8(&mut glyph), dim);
		}
		row_y += 1;
		draw_line(frame, text_x, row_y, text_width, &[Span::new(
			"LSP Servers",
			ink(theme.accent).bold(),
		)]);
		row_y += 1;
		if banner.lsp_servers.is_empty() {
			draw_line(frame, text_x, row_y, text_width, &[Span::new("No LSP servers", dim)]);
		} else {
			for server in banner.lsp_servers.iter().take(WELCOME_LSP_SLOTS) {
				let (icon, color) = if server.failed {
					(charset.icon(Icon::Error), theme.err)
				} else if server.stage_label.starts_with("ready") {
					(charset.icon(Icon::Enabled), theme.ok)
				} else {
					(charset.icon(Icon::Enabled), theme.muted)
				};
				draw_line(frame, text_x, row_y, text_width, &[
					Span::new(icon, ink(color)),
					Span::new(" ", dim),
					Span::new(server.name.as_str(), ink(theme.muted)),
					Span::new(" ", dim),
					Span::new(server.stage_label.as_str(), dim),
				]);
				row_y += 1;
			}
		}
	}

	// Bottom border with a column junction.
	let bottom_y = y + 1 + content_rows;
	x = frame.put(0, bottom_y, bl.encode_utf8(&mut glyph), dim);
	while x < box_width - 1 {
		x = frame.put(x, bottom_y, h.encode_utf8(&mut glyph), dim);
	}
	if right_col > 0 {
		let junction = charset.grid().bottom.1;
		frame.put(left_x + left_col, bottom_y, junction.encode_utf8(&mut glyph), dim);
	}
	frame.put(box_width - 1, bottom_y, br.encode_utf8(&mut glyph), dim);

	// Tip footer under the card.
	if let Some(tip) = &banner.tip {
		draw_line(frame, 1, bottom_y + 1, box_width.saturating_sub(1), &[
			Span::new("Tip: ", ink(theme.info).italic()),
			Span::new(tip.as_str(), ink(theme.muted).italic()),
		]);
	}
	height
}

fn preserves_attachments(text: &str) -> bool {
	let first = text.split_whitespace().next().unwrap_or_default();
	first.starts_with('/') && first.get(1..).is_some_and(|command| !command.contains('/'))
}

fn draw_box(
	frame: &mut Frame,
	rect: Rect,
	border: Style,
	fill: Style,
	charset: Charset,
	native: bool,
) {
	if rect.width < 2 || rect.height < 2 {
		return;
	}
	let (tl, tr, bl, br, h, v) = charset.border(Border::Round);
	let mut glyph = [0_u8; 4];
	frame.fill(rect, fill);
	frame.put(rect.x, rect.y, tl.encode_utf8(&mut glyph), border);
	frame.put(rect.x + rect.width - 1, rect.y, tr.encode_utf8(&mut glyph), border);
	frame.put(rect.x, rect.y + rect.height - 1, bl.encode_utf8(&mut glyph), border);
	frame.put(rect.x + rect.width - 1, rect.y + rect.height - 1, br.encode_utf8(&mut glyph), border);
	for x in rect.x + 1..rect.x + rect.width - 1 {
		frame.put(x, rect.y, h.encode_utf8(&mut glyph), border);
		frame.put(x, rect.y + rect.height - 1, h.encode_utf8(&mut glyph), border);
	}
	for y in rect.y + 1..rect.y + rect.height - 1 {
		frame.put(rect.x, y, v.encode_utf8(&mut glyph), border);
		frame.put(rect.x + rect.width - 1, y, v.encode_utf8(&mut glyph), border);
	}
	if native {
		frame.push_noselect(rect);
	}
}

fn draw_line(frame: &mut Frame, x: u16, y: u16, width: u16, spans: &[Span<'_>]) -> u16 {
	let right = x.saturating_add(width);
	let mut at = x;
	for span in spans {
		for grapheme in xutf::graphemes_str(span.text) {
			if at >= right {
				return at;
			}
			let next = frame.put(at, y, grapheme, span.style);
			if next == at {
				return at;
			}
			at = next;
		}
	}
	at
}

fn draw_flowed(frame: &mut Frame, rect: Rect, spans: &[Span<'_>]) -> u16 {
	if rect.width == 0 || rect.height == 0 {
		return 0;
	}
	let mut x = rect.x;
	let mut y = rect.y;
	let right = rect.x.saturating_add(rect.width);
	let bottom = rect.y.saturating_add(rect.height);
	for span in spans {
		for grapheme in xutf::graphemes_str(span.text) {
			if grapheme == "\n" {
				x = rect.x;
				y = y.saturating_add(1);
				if y >= bottom {
					return y.saturating_sub(rect.y);
				}
				continue;
			}
			let width = visible_width(grapheme);
			if x > rect.x && x.saturating_add(width) > right {
				frame.set_soft_wrap(y);
				x = rect.x;
				y = y.saturating_add(1);
			}
			if y >= bottom {
				return y.saturating_sub(rect.y);
			}
			x = frame.put(x, y, grapheme, span.style);
		}
	}
	y.saturating_sub(rect.y).saturating_add(1)
}

fn flowed_height(text: &str, width: u16) -> u16 {
	if width == 0 {
		return 0;
	}
	let mut rows = 1_u16;

	let mut column = 0_u16;
	for grapheme in xutf::graphemes_str(text) {
		if grapheme == "\n" {
			rows = rows.saturating_add(1);
			column = 0;
			continue;
		}
		let size = visible_width(grapheme);
		if column > 0 && column.saturating_add(size) > width {
			rows = rows.saturating_add(1);
			column = 0;
		}
		column = column.saturating_add(size);
	}
	rows
}
fn compaction_method_label(method: Option<&str>) -> &'static str {
	match method {
		Some("prune") => "pruned",
		Some("drop_media") => "media-dropped",
		Some("elide") => "elided",
		Some("local") => "locally-compacted",
		Some("remote") => "remote-compacted",
		Some("handoff") => "handed-off",
		Some(_) | None => "compacted",
	}
}

fn hud_line(text: Str, charset: Charset) -> Str {
	match collapse_hud_line(&text, charset) {
		borrow::Cow::Borrowed(_) => text,
		borrow::Cow::Owned(collapsed) => Str::new(collapsed),
	}
}

fn explicit_line_count(text: &str) -> u16 {
	u16::try_from(text.lines().count().max(1)).unwrap_or(u16::MAX)
}

fn agent_label(agent: &AgentRow, charset: Charset) -> Str {
	let mut label = StrMut::with_capacity(64);
	for _ in 0..agent.depth.min(4) {
		label.push_str("  ");
	}
	let _ = write!(label, "{} {} · {}", charset.icon(Icon::Task), agent.name, agent.status);
	if let Some(tool) = &agent.tool {
		let _ = write!(label, " · {tool}");
	}
	if let Some(tokens) = agent.tokens {
		let _ = write!(label, " · {}", compact_count(tokens));
	}
	label.freeze()
}

const fn elapsed_label_key(elapsed: Duration) -> u64 {
	let seconds = elapsed.as_secs();
	if seconds < 60 {
		seconds
	} else if seconds < 3_600 {
		60 + seconds / 60
	} else {
		let hours = seconds / 3_600;
		3_600 + if hours > 99 { 99 } else { hours }
	}
}

fn elapsed_label(elapsed: Duration) -> Str {
	let seconds = elapsed.as_secs();
	if seconds < 60 {
		fmts_mut!("{seconds}s").freeze()
	} else if seconds < 3_600 {
		fmts_mut!("{}m", seconds / 60).freeze()
	} else {
		fmts_mut!("{}h", (seconds / 3_600).min(99)).freeze()
	}
}

fn format_turn_elapsed(elapsed_ms: u64) -> Str {
	if elapsed_ms < 1_000 {
		sf!("{elapsed_ms}ms")
	} else {
		elapsed_label(Duration::from_millis(elapsed_ms))
	}
}

fn format_usage_label(usage: &omp_proto::inference::v1::Usage) -> Str {
	let input = usage.input_tokens.saturating_add(usage.cache_write_tokens);
	let mut label =
		fmts_mut!("{} in · {} out", compact_count(input), compact_count(usage.output_tokens));
	if usage.cache_read_tokens > 0 {
		let _ = write!(label, " · {} cached", compact_count(usage.cache_read_tokens));
	}
	label.freeze()
}

fn compact_count(value: u64) -> Str {
	let mut label = StrMut::default();
	let _ = write_compact_count(&mut label, value);
	label.freeze()
}

fn context_usage_label(tokens: u64, window: Option<u64>) -> (Str, bool) {
	let Some(window) = window.filter(|window| *window > 0) else {
		return (compact_count(tokens), false);
	};
	let overflow = tokens > window;
	let percent = tokens as f64 / window as f64 * 100.0;
	let window = compact_count(window);
	let label = if percent > 0.0 && percent < 1.0 {
		sf!("{percent:.1}%/{window}")
	} else {
		sf!("{percent:.0}%/{window}")
	};
	(label, overflow)
}

fn visible_width(text: &str) -> u16 {
	omp_tui::cell_width(text)
}
const fn base_style(theme: Theme) -> Style {
	Style::new().fg(theme.fg)
}
const fn panel_style(theme: Theme) -> Style {
	Style::new().fg(theme.fg).bg(theme.panel)
}
const fn ink(color: Color) -> Style {
	Style::new().fg(color)
}
const fn prose_style(theme: Theme) -> Style {
	Style::new().fg(theme.muted).italic()
}

#[cfg(test)]
mod tests {
	use std::{env, fs, process};

	use super::*;
	use crate::{BlockPhase, GitFacts};

	fn ctx() -> UiContext {
		UiContext::default()
	}

	fn frame_text(frame: &Frame) -> String {
		(0..frame.size().height)
			.map(|row| omp_tui::test_support::frame_row_text(frame, row))
			.collect::<Vec<_>>()
			.join("\n")
	}

	fn batch_text(batch: &RetirementBatch) -> String {
		batch.replay_frames.as_ref().map_or_else(
			|| frame_text(&batch.frame),
			|frames| frames.iter().map(frame_text).collect::<Vec<_>>().join("\n"),
		)
	}

	fn settle(chat: &mut Chat, viewport: Size) {
		for frame in 0..8 {
			let _ = chat.render_at(viewport, Duration::from_millis(frame * 60));
		}
	}
	fn assistant_usage(completed_at_ms: u64) -> AssistantUsage {
		AssistantUsage {
			usage: omp_proto::inference::v1::Usage {
				input_tokens: 4_200,
				output_tokens: 7,
				..Default::default()
			},
			completed_at_ms,
		}
	}

	#[test]
	fn settling_a_message_taller_than_the_viewport_keeps_the_tail_visible() {
		let mut chat = Chat::new(&ctx());
		let viewport = Size::new(40, 12);
		let long = (0..200)
			.map(|row| format!("row-{row:03}"))
			.collect::<Vec<_>>()
			.join("\n\n");
		chat.begin_assistant("long");
		chat.append_assistant("long", &long);
		settle(&mut chat, viewport);
		chat.end_assistant("long");
		settle(&mut chat, viewport);

		while let Some(batch) = chat.retirement_batch(viewport) {
			let projected = frame_text(chat.render_after_retirement(viewport, &batch).frame);
			chat.mark_retired(&batch);
			assert!(projected.contains("row-"), "retirement blanked the viewport: {projected:?}");
		}
		let rendered = frame_text(chat.render(viewport).frame);
		assert!(rendered.contains("row-199"), "settled tail no longer visible: {rendered:?}");
	}
	#[test]
	fn render_is_exactly_viewport_sized() {
		let mut chat = Chat::new(&ctx());
		for viewport in [Size::new(0, 0), Size::new(1, 1), Size::new(80, 24)] {
			assert_eq!(chat.render(viewport).frame.size(), viewport);
		}
	}
	#[test]
	fn idle_brand_becomes_the_working_indicator() {
		let ctx = UiContext { charset: Charset::NerdFont, ..UiContext::default() };
		let mut chat = Chat::new(&ctx);
		let brand = fmts_mut!("{} omp", ctx.charset.icon(Icon::Omp)).freeze();
		let idle = frame_text(chat.render(Size::new(100, 32)).frame);

		assert!(idle.contains(brand.as_str()), "idle brand is missing: {idle}");
		chat.set_status(StatusFacts { working: true, ..StatusFacts::default() });
		let working = frame_text(chat.render(Size::new(100, 32)).frame);
		assert!(!working.contains(brand.as_str()), "idle brand remained while working: {working}");
		assert!(working.contains("0s"), "working indicator is missing: {working}");
	}
	#[test]
	fn compact_status_matches_the_reference_segment_contract() {
		let ctx = UiContext { charset: Charset::Unicode, ..UiContext::default() };
		let mut chat = Chat::new(&ctx);
		chat.set_composer_style(ComposerStyle::Box);
		chat.set_status(StatusFacts {
			model: "DeepSeek V4 Flash".into(),
			cwd: Some("/tmp/project".into()),
			git: Some(GitFacts { branch: sf!("main"), dirty: 2, staged: 1, untracked: 3 }),
			context_tokens: 40_000,
			context_window: Some(1_000_000),
			cost_nanos: 1_500_000_000,
			layout: StatusLayout::Compact,
			..StatusFacts::default()
		});
		let _ = chat.apply_backend_event(BackendEvent::SessionTitle(sf!("Fix segment bars")));
		let rendered = frame_text(chat.render(Size::new(140, 32)).frame);

		for obsolete in ["session ", "context  ", "cost     ", "state    "] {
			assert!(!rendered.contains(obsolete), "obsolete segment `{obsolete}`: {rendered}");
		}
		assert!(rendered.contains("⬢ DeepSeek V4 Flash"), "model icon is missing: {rendered}");
		assert!(rendered.contains("📁 project"), "folder icon is missing: {rendered}");
		assert!(rendered.contains("⑂ main *2 +1 ?3"), "git icon is missing: {rendered}");
		assert!(rendered.contains("$1.50"), "compact spend is missing: {rendered}");
		assert!(rendered.contains("4%"), "embedded context percent is missing: {rendered}");
		assert!(rendered.contains("1m"), "embedded context window is missing: {rendered}");
		assert!(rendered.contains("Fix segment bars"), "session title is missing: {rendered}");
	}
	#[test]
	fn clear_and_exit_bypass_editor_mutation() {
		let mut chat = Chat::new(&ctx());
		chat.set_composer_text("draft");

		assert_eq!(chat.handle_key(Key::Ctrl('c')), ChatKey::Clear);
		assert_eq!(chat.composer_text(), "draft");
		assert_eq!(chat.handle_key(Key::Ctrl('d')), ChatKey::Exit);
		assert_eq!(chat.composer_text(), "draft");
	}

	#[test]
	fn host_can_stage_initial_composer_text_through_enter_path() {
		let mut chat = Chat::new(&ctx());
		chat.set_composer_text("launch message");
		chat.submit_composer();

		let (text, attachments, mode) = chat.take_submission().expect("initial submission");
		assert_eq!(text, "launch message");
		assert!(attachments.is_empty());
		assert_eq!(mode, SubmitMode::Steer);
		assert!(chat.composer_empty());
	}

	#[test]
	fn resource_status_names_the_owner_and_fifo_queue_depth() {
		let facts = StatusFacts {
			model: sf!("Fable 5"),
			visible_resources: std::sync::Arc::from([crate::VisibleResourceFacts {
				resource:    sf!("mode"),
				owner:       sf!("plan"),
				queue_depth: 2,
			}]),
			..StatusFacts::default()
		};

		let labels = StatusLabels::new(&facts, Charset::Ascii);
		assert_eq!(labels.resources.len(), 1);
		assert_eq!(labels.resources[0].as_str(), ". mode plan +2");
	}

	#[test]
	fn thinking_replaces_the_model_icon_without_rendering_a_level_label() {
		let ctx = UiContext { charset: Charset::NerdFont, ..UiContext::default() };
		let mut chat = Chat::new(&ctx);
		chat.set_status(StatusFacts {
			model: "Fable 5".into(),
			thinking: Some(ThinkingLevel::Max),
			..StatusFacts::default()
		});

		let text = frame_text(chat.render(Size::new(80, 8)).frame);
		assert!(text.contains(" Fable 5"), "{text}");
		assert!(!text.contains("think"), "{text}");
	}

	#[test]
	fn boxed_composer_embeds_the_context_gauge_in_its_top_border() {
		let mut chat = Chat::new(&ctx());
		let viewport = Size::new(100, 10);
		chat.set_composer_style(ComposerStyle::Box);
		chat.set_status(StatusFacts {
			model: "Fable 5".into(),
			context_tokens: 160_000,
			context_window: Some(200_000),
			compaction_boundaries: Some(crate::CompactionBoundaries {
				threshold_percent:   80.0,
				speculation_percent: Some(70.0),
			}),
			..StatusFacts::default()
		});
		let text = frame_text(chat.render(viewport).frame);
		let border = text
			.lines()
			.find(|line| line.contains("80%"))
			.unwrap_or_else(|| panic!("top border embeds the usage percent: {text}"));
		assert!(border.contains("200k"), "window label rides the gauge: {border}");
		assert!(border.contains('┃'), "compaction threshold tick: {border}");
		assert!(border.contains('╎'), "speculation tick: {border}");
		assert!(!border.contains("80%/"), "numeric context segment is absorbed: {border}");
	}
	#[test]
	fn context_gauge_labels_survive_long_status_groups() {
		let mut chat = Chat::new(&ctx());
		chat.set_composer_style(ComposerStyle::Box);
		chat.set_status(StatusFacts {
			model: "An Extremely Long Model Name".into(),
			context_tokens: 160_000,
			context_window: Some(200_000),
			..StatusFacts::default()
		});
		let _ = chat.apply_backend_event(BackendEvent::SessionTitle(sf!(
			"An extremely long session title that must shed first"
		)));

		let text = frame_text(chat.render(Size::new(34, 8)).frame);
		let border = text
			.lines()
			.find(|line| line.contains("80%"))
			.unwrap_or_else(|| panic!("gauge percent was shed before long status groups: {text}"));
		assert!(border.contains("200k"), "gauge window was shed before status groups: {border}");
	}

	#[test]
	fn context_gauge_degrades_to_one_cell_only_after_labels_cannot_fit() {
		let facts = StatusFacts {
			context_tokens: 160_000,
			context_window: Some(200_000),
			..StatusFacts::default()
		};
		let desired = context_gauge_min_width(&facts);
		assert_eq!(desired, 11);
		let (left, right) = fit_status_group_widths(12, 40, 30);
		let labelled = boundary_layout(0, 23, left, right, desired).expect("labelled gauge layout");
		assert_eq!(labelled.boundary_width, desired);

		let minimum = if desired.saturating_add(1) <= 8 {
			desired
		} else {
			1
		};
		let (left, right) = fit_status_group_widths(8_u16.saturating_sub(minimum), 40, 30);
		let tiny = boundary_layout(0, 8, left, right, minimum).expect("one-cell gauge layout");
		assert_eq!(tiny.boundary_width, 1);
		assert!(left > 0 || right > 0, "a final status segment must survive beside the gauge");
	}

	#[test]
	fn daily_quota_is_rendered_with_reset_context() {
		let mut chat = Chat::new(&ctx());
		chat.set_status(StatusFacts {
			model: "Fable 5".into(),
			quota: Some(crate::status_line::StatusQuota {
				daily: Some(crate::status_line::StatusQuotaWindow {
					percent:       83,
					reset_minutes: Some(125),
				}),
			}),
			..StatusFacts::default()
		});

		let text = frame_text(chat.render(Size::new(100, 8)).frame);
		assert!(text.contains("1d 83% (2h 5m)"), "{text}");
	}

	#[test]
	fn composer_style_event_replaces_live_chrome() {
		let mut chat = Chat::new(&ctx());
		let viewport = Size::new(50, 8);
		assert!(frame_text(chat.render(viewport).frame).contains("╰─"));

		let _ = chat.apply_backend_event(BackendEvent::ComposerStyleChanged(ComposerStyle::Rail));
		let text = frame_text(chat.render(viewport).frame);
		assert!(text.contains('▎'), "{text}");
		assert!(!text.contains("╰─"), "{text}");
	}

	#[test]
	fn boxed_composer_repaints_status_and_input_together() {
		let mut chat = Chat::new(&ctx());
		let viewport = Size::new(50, 10);
		chat.set_composer_style(ComposerStyle::Box);
		chat.set_status(StatusFacts { model: "Fable 5".into(), ..StatusFacts::default() });
		let _ = chat.render(viewport);

		assert_eq!(chat.handle_key(Key::Char('x')), ChatKey::Consumed);
		let typed = frame_text(chat.render(viewport).frame);
		assert!(typed.contains("Fable 5"), "{typed}");
		assert!(typed.contains('x'), "{typed}");

		assert_eq!(chat.handle_key(Key::Enter), ChatKey::Consumed);
		chat.set_status(StatusFacts {
			model: "Fable 5".into(),
			working: true,
			..StatusFacts::default()
		});
		let submitted = frame_text(chat.render(viewport).frame);
		assert!(submitted.contains("Fable 5"), "{submitted}");
		assert!(submitted.contains('╰'), "{submitted}");
	}

	#[test]
	fn idle_recap_paints_transient_air_row_and_clears_on_activity() {
		let mut chat = Chat::new(&UiContext { charset: Charset::Unicode, ..UiContext::default() });
		let viewport = Size::new(60, 10);
		let _ = chat.apply_backend_event(BackendEvent::Recap(sf!("Continue with the focused test.")));

		let rendered = chat.render(viewport).frame;
		let text = frame_text(rendered);
		assert!(text.contains("※ recap: Continue with the focused test."), "{text}");
		let row = text
			.lines()
			.position(|line| line.contains("※ recap:"))
			.expect("recap air row");
		let row = u16::try_from(row).expect("viewport row fits u16");
		let style = omp_tui::test_support::frame_cell_style(rendered, 1, row).spec();
		assert!(style.dim && style.italic, "recap must render dim + italic");

		assert_eq!(chat.handle_key(Key::Char('x')), ChatKey::Consumed);
		let typed = frame_text(chat.render(viewport).frame);
		assert!(!typed.contains("recap:"), "{typed}");

		chat.clear_composer();
		let _ = chat.apply_backend_event(BackendEvent::Recap(sf!("One newer recap.")));
		let _ = chat.apply_backend_event(BackendEvent::Status(StatusFacts {
			working: true,
			..StatusFacts::default()
		}));
		let working = frame_text(chat.render(viewport).frame);
		assert!(!working.contains("recap:"), "{working}");
		let ascii = UiContext { charset: Charset::Ascii, ..UiContext::default() };
		let mut chat = Chat::new(&ascii);
		let _ = chat.apply_backend_event(BackendEvent::Recap(sf!("ASCII-safe.")));
		let ascii_text = frame_text(chat.render(viewport).frame);
		assert!(ascii_text.contains("recap: ASCII-safe."), "{ascii_text}");
		assert!(!ascii_text.contains('※'), "{ascii_text}");
	}

	#[test]
	fn minimal_status_omits_every_ancillary_right_segment() {
		let mut chat = Chat::new(&ctx());
		chat.set_status(StatusFacts {
			model: "Fable 5".into(),
			working: true,
			context_tokens: 1_000,
			context_window: Some(10_000),
			cost_nanos: 2_000_000_000,
			advisor_cost_nanos: 3_000_000_000,
			queued: 4,
			jobs: 5,
			attempt: 2,
			dropped: 7,
			layout: StatusLayout::Minimal,
			..StatusFacts::default()
		});
		let text = frame_text(chat.render(Size::new(160, 8)).frame);
		assert!(text.contains("Fable 5"), "{text}");
		assert!(text.contains("10%"), "{text}");
		assert!(text.contains("10k"), "{text}");
		for hidden in ["queued 4", "jobs 5", "retry 2", "dropped 7", "$2", "$3"] {
			assert!(!text.contains(hidden), "minimal status leaked {hidden}: {text}");
		}
	}

	#[test]
	fn working_indicator_is_core_timed_and_cleared_at_turn_end() {
		let mut chat = Chat::new(&ctx());
		chat.set_status(StatusFacts { working: true, ..StatusFacts::default() });
		chat.set_working_indicator(WorkingIndicator {
			frames:      vec![sf!("a"), sf!("b")].into_boxed_slice(),
			interval_ms: 10,
		});
		chat
			.work
			.borrow_mut()
			.update_active_brand(Duration::from_millis(15), chat.ctx.charset);
		assert!(chat.work.borrow().active_brand.as_str().starts_with('b'));
		chat.set_status(StatusFacts { working: false, ..StatusFacts::default() });
		assert!(chat.work.borrow().indicator.is_none());
	}

	#[test]
	fn empty_working_indicator_hides_the_active_brand() {
		let mut chat = Chat::new(&ctx());
		chat.set_status(StatusFacts { working: true, ..StatusFacts::default() });
		chat.set_working_indicator(WorkingIndicator {
			frames:      Vec::new().into_boxed_slice(),
			interval_ms: 80,
		});
		chat
			.work
			.borrow_mut()
			.update_active_brand(Duration::from_millis(15), chat.ctx.charset);
		assert!(chat.work.borrow().active_brand.is_empty());
	}

	#[test]
	fn viewport_damage_is_local() {
		let mut chat = Chat::new(&ctx());
		chat.begin_assistant("assistant");
		chat.append_assistant("assistant", "streaming");
		let viewport = Size::new(40, 8);
		let rendered = chat.render(viewport);
		assert!(
			rendered
				.damage
				.iter()
				.all(|(start, end)| { start <= end && *end <= viewport.height })
		);
	}

	#[test]
	fn streaming_assistant_uses_partial_markdown_view() {
		let mut chat = Chat::new(&ctx());
		chat.begin_assistant("assistant");
		chat.append_assistant("assistant", "**streaming** tail");
		let viewport = Size::new(40, 8);
		settle(&mut chat, viewport);
		let text = frame_text(chat.render_at(viewport, Duration::from_millis(500)).frame);
		assert!(text.contains("streaming tail"), "{text}");
		assert!(!text.contains("**"), "{text}");
	}

	#[test]
	fn smooth_streaming_reveals_whole_graphemes_and_flushes_at_tool_boundary() {
		let mut chat = Chat::new(&ctx());
		chat.begin_assistant("assistant");
		chat.append_assistant("assistant", "👨‍👩‍👧‍👦abcdef");
		let family_bytes = "👨‍👩‍👧‍👦".len();
		let assistant = chat.live_assistant.as_ref().expect("live assistant");
		assert_eq!(assistant.revealed, 0);

		let _ = chat.render_at(Size::new(40, 8), Duration::from_millis(33));
		let assistant = chat.live_assistant.as_ref().expect("live assistant");
		assert!(assistant.revealed >= family_bytes);
		assert!(assistant.revealed < assistant.text.len());

		chat.tool_started("tool", "read");
		let assistant = chat.live_assistant.as_ref().expect("live assistant");
		assert_eq!(assistant.revealed, assistant.text.len());
	}

	#[test]
	fn disabling_smooth_streaming_flushes_the_buffered_suffix() {
		let mut chat = Chat::new(&ctx());
		chat.begin_assistant("assistant");
		chat.append_assistant("assistant", "buffered suffix");
		assert!(
			chat
				.live_assistant
				.as_ref()
				.is_some_and(|assistant| assistant.revealed < assistant.text.len())
		);

		chat.set_smooth_streaming(false);

		assert!(
			chat
				.live_assistant
				.as_ref()
				.is_some_and(|assistant| assistant.revealed == assistant.text.len())
		);
	}

	#[test]
	fn settled_markdown_treats_literal_closing_tag_as_message_text() {
		let body = RichText::new("**bold** literal </md> tail", 40, &ctx());
		assert!(body.view.is_some());
		let text = frame_text(body.view.as_ref().expect("markdown view").frame());
		assert!(text.contains("bold literal </md> tail"), "{text}");
		assert!(!text.contains("**"), "{text}");
	}

	#[test]
	fn next_assistant_begin_settles_orphan_stream_content() {
		let mut chat = Chat::new(&ctx());
		chat.begin_assistant("orphan");
		chat.append_assistant("orphan", "partial transport output");
		assert_eq!(chat.blocks.phase(BlockOrdinal(0)), Some(BlockPhase::Queued));

		chat.begin_assistant("next");

		assert_eq!(chat.blocks.phase(BlockOrdinal(0)), Some(BlockPhase::FinalizedPending),);
		assert!(matches!(
			chat.entries.get(&BlockOrdinal(0)),
			Some(Entry::Assistant(assistant)) if assistant.body.text.contains("partial transport output")
		));
		assert_eq!(
			chat
				.live_assistant
				.as_ref()
				.map(|message| message.id.as_str()),
			Some("next")
		);
	}

	#[test]
	fn next_assistant_begin_settles_streamed_thinking_as_entry() {
		let mut chat = Chat::new(&ctx());
		chat.begin_thinking("reasoning");
		chat.append_assistant("reasoning", "weighing the options carefully.");

		chat.begin_assistant("prose");

		assert_eq!(chat.blocks.phase(BlockOrdinal(0)), Some(BlockPhase::FinalizedPending));
		assert!(matches!(
			chat.entries.get(&BlockOrdinal(0)),
			Some(Entry::Thinking(thinking)) if thinking.body.text.contains("weighing the options")
		));
	}

	#[test]
	fn retry_abandon_drops_partial_stream_without_entry() {
		let mut chat = Chat::new(&ctx());
		chat.begin_assistant("attempt-one");
		chat.append_assistant("attempt-one", "partial answer the retry re-streams");

		chat.abandon_assistant("attempt-one");

		assert_eq!(chat.blocks.phase(BlockOrdinal(0)), Some(BlockPhase::FinalizedPending));
		assert!(!chat.entries.contains_key(&BlockOrdinal(0)));
		assert!(chat.live_assistant.is_none());
	}

	#[test]
	fn replayed_thinking_persists_with_visible_body() {
		let mut chat = Chat::new(&ctx());
		let passthrough = chat.apply_backend_event(BackendEvent::ThinkingReplayed {
			text: sf!("Restored reasoning from durable history."),
		});
		assert!(passthrough.is_none());
		assert!(matches!(
			chat.entries.get(&BlockOrdinal(0)),
			Some(Entry::Thinking(thinking)) if thinking.body.text.contains("Restored reasoning")
		));

		let viewport = Size::new(60, 12);
		settle(&mut chat, viewport);
		let text = frame_text(chat.render_at(viewport, Duration::from_millis(500)).frame);
		assert!(text.contains("Restored reasoning from durable history"), "{text}");
	}

	#[test]
	fn one_row_clip_of_thinking_shows_a_dim_italic_reasoning_row() {
		let mut chat = Chat::new(&ctx());
		chat.begin_thinking("reasoning");
		chat.append_assistant("reasoning", "short reasoning line.");
		chat.end_assistant("reasoning");
		let viewport = Size::new(60, 12);
		settle(&mut chat, viewport);

		chat.draw_settled_clipped(BlockOrdinal(0), 0, 1, 2, viewport.width);

		let row = omp_tui::test_support::frame_row_text(&chat.frame, 0);
		assert!(row.contains("short reasoning line"), "{row}");
		let style = omp_tui::test_support::frame_cell_style(&chat.frame, 0, 0).spec();
		assert!(style.dim && style.italic, "reasoning rows must render dim + italic");
	}

	#[test]
	fn next_assistant_begin_seals_orphaned_tool_card() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("orphan", "bash");
		chat.tool_output("orphan", "partial output");
		settle(&mut chat, Size::new(40, 12));

		chat.begin_assistant("next-turn");

		assert!(chat.live_tools.is_empty());
		assert_eq!(chat.blocks.phase(BlockOrdinal(0)), Some(BlockPhase::FinalizedPending));
		assert!(matches!(
			chat.entries.get(&BlockOrdinal(0)),
			Some(Entry::Tool(ToolEntry { terminal: ToolTerminal::Aborted, .. }))
		));
	}

	#[test]
	fn todo_hud_expands_and_collapses_deterministically() {
		let mut chat = Chat::new(&ctx());
		let lines = (1..=8)
			.map(|task| sf!("- [ ] Task {task}"))
			.collect::<Vec<_>>();
		chat.set_todo_hud(TodoHud { lines, total_tasks: 8 });
		let viewport = Size::new(60, 20);

		let collapsed = frame_text(chat.render(viewport).frame);
		assert!(collapsed.contains("Task 5"), "{collapsed}");
		assert!(!collapsed.contains("Task 8"), "{collapsed}");
		assert!(collapsed.contains("3 more todos"), "{collapsed}");

		chat.set_todo_expanded(true);
		let expanded = frame_text(chat.render(viewport).frame);
		assert!(expanded.contains("Task 8"), "{expanded}");
		assert!(!expanded.contains("more todos"), "{expanded}");

		chat.set_todo_expanded(false);
		let collapsed_again = frame_text(chat.render(viewport).frame);
		assert!(!collapsed_again.contains("Task 8"), "{collapsed_again}");
	}

	#[test]
	fn settled_tool_card_leaves_call_detail_to_renderer() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("card", "bash");
		chat.tool_finished(
			"card",
			ToolTerminal::Succeeded,
			ToolViewContent::Markup("<text>$ echo ok</text>".into()),
		);
		let batch = chat
			.retirement_batch(Size::new(60, 0))
			.expect("settled card retires");
		let text = frame_text(&batch.frame);
		assert!(text.contains("▾ ✓ bash"), "{text}");
		assert!(text.contains("│ $ echo ok"), "{text}");
		assert!(!text.contains('@'), "{text}");
		assert!(!text.contains("bash ·"), "{text}");
		assert_eq!(text.matches("echo ok").count(), 1, "{text}");
		assert!(text.contains('╰'), "{text}");
	}

	#[test]
	fn settled_tool_preserves_and_toggles_the_live_collapse_state() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("card", "read");
		assert_eq!(chat.toggle_latest_tool(), Some(false));
		assert!(!chat.live_tools[0].expanded);
		chat.tool_finished(
			"card",
			ToolTerminal::Succeeded,
			ToolViewContent::Plain("line one\nline two".into()),
		);
		let Some(Entry::Tool(tool)) = chat.entries.get(&BlockOrdinal(0)) else {
			panic!("settled tool entry");
		};
		assert!(!tool.expanded);

		assert_eq!(chat.toggle_latest_tool(), Some(true));
		let Some(Entry::Tool(tool)) = chat.entries.get(&BlockOrdinal(0)) else {
			panic!("settled tool entry");
		};
		assert!(tool.expanded);
	}

	#[test]
	fn plain_tool_fallback_never_parses_core_markup() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("plain", "unknown");
		chat.tool_finished(
			"plain",
			ToolTerminal::ArgsRejected,
			ToolViewContent::Plain("<approval>not chrome</approval>".into()),
		);

		let Some(Entry::Tool(tool)) = chat.entries.get(&BlockOrdinal(0)) else {
			panic!("settled tool entry");
		};
		assert!(tool.view.plain);
		assert_eq!(tool.view.chrome, ViewChrome::Card);
		assert_eq!(tool.terminal, ToolTerminal::ArgsRejected);
		assert_eq!(tool.view.source, "<approval>not chrome</approval>");
	}

	#[test]
	fn flush_tool_view_settles_without_card_chrome() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("flush", "read");
		chat.tool_finished(
			"flush",
			ToolTerminal::Succeeded,
			ToolViewContent::Markup(
				"<row gap=1 chrome=flush><text>✔ read src/lib.rs</text></row>".into(),
			),
		);
		let batch = chat
			.retirement_batch(Size::new(60, 0))
			.expect("flush view retires");
		let text = frame_text(&batch.frame);
		assert!(text.contains("✔ read src/lib.rs"), "{text}");
		assert!(!text.contains('│'), "{text}");
		assert!(!text.contains('╰'), "{text}");
		assert!(!text.contains("read@1"), "{text}");
	}

	#[test]
	fn replay_renders_the_same_committed_rows() {
		let mut chat = Chat::new(&ctx());
		for id in ["a", "b"] {
			chat.tool_started(id, "read");
			chat.tool_finished(id, ToolTerminal::Succeeded, ToolViewContent::Plain(Str::from(id)));
		}
		let viewport = Size::new(40, 0);
		let batch = chat
			.retirement_batch(viewport)
			.expect("queued finals retire");
		let committed = frame_text(&batch.frame);
		chat.mark_retired(&batch);

		chat.begin_history_replay(HistoryReplay::Append);
		let replay = chat
			.retirement_batch(viewport)
			.expect("committed rows replay");
		assert_eq!(batch_text(&replay), committed);
	}

	#[test]
	fn dropped_prompt_restores_text_and_attachments_without_history_row() {
		let mut chat = Chat::new(&ctx());
		chat.push_user("recover me", Vec::new());
		chat.restore_dropped_prompt("recover me".into(), Vec::new());
		assert_eq!(chat.composer_text(), "recover me");
		// The tombstone contributes no rows, so it never creates pressure by
		// itself; it retires contiguously with the next pressured batch.
		assert!(chat.retirement_batch(Size::new(40, 0)).is_none());
		chat.push_notice("follow-up");
		let batch = chat
			.retirement_batch(Size::new(40, 0))
			.expect("tombstone remains contiguous");
		assert_eq!(batch.range.start, 0);
		assert!(frame_text(&batch.frame).contains("follow-up"));
	}
	#[test]
	fn queued_user_rows_render_dim_and_settle_into_plain_messages() {
		let mut chat = Chat::new(&ctx());
		let _ = chat.apply_backend_event(BackendEvent::UserReplayed {
			text:   sf!("queued follow-up"),
			chips:  Vec::new(),
			queued: true,
		});
		let viewport = Size::new(60, 12);
		let text = frame_text(chat.render(viewport).frame);
		assert!(text.contains("Queued"), "{text}");
		assert!(text.contains("to edit"), "{text}");
		assert!(text.contains("queued follow-up"), "{text}");
		// Pending rows stay unretired so a later dequeue can remove them.
		assert!(matches!(
			chat.blocks.phase(BlockOrdinal(0)),
			Some(crate::BlockPhase::Queued | crate::BlockPhase::Active)
		));
		let _ = chat.apply_backend_event(BackendEvent::QueuedPromptsSettled);
		let text = frame_text(chat.render(viewport).frame);
		assert!(!text.contains("Queued"), "{text}");
		assert!(text.contains("queued follow-up"), "{text}");
	}
	#[test]
	fn replayed_todo_nudge_strips_only_the_outer_system_wrapper() {
		let mut chat = Chat::new(&ctx());
		let _ = chat.apply_backend_event(BackendEvent::UserReplayed {
			text:   sf!("<system-reminder>Finish the todo list.</system-reminder>"),
			chips:  Vec::new(),
			queued: false,
		});

		let text = frame_text(chat.render(Size::new(60, 12)).frame);
		assert!(text.contains("Finish the todo list."), "{text}");
		assert!(!text.contains("<system-reminder>"), "{text}");
	}
	#[test]
	fn todo_continuation_preserves_the_user_turn_anchor() {
		let mut chat = Chat::new(&ctx());
		chat.set_show_token_usage(true);
		chat.set_show_turn_time(true);
		chat.apply_turn_anchor(TurnAnchor::User { submitted_at_ms: 1_000 });
		chat.apply_turn_anchor(TurnAnchor::Developer {
			submitted_at_ms: 2_000,
			synthetic:       false,
			user_initiated:  false,
		});
		chat.push_assistant_usage(assistant_usage(61_000));

		let text = frame_text(chat.render(Size::new(60, 12)).frame);
		assert!(text.contains("Δ1m"), "{text}");
	}

	#[test]
	fn synthetic_follow_up_clears_the_user_turn_anchor() {
		let mut chat = Chat::new(&ctx());
		chat.set_show_token_usage(true);
		chat.set_show_turn_time(true);
		chat.apply_turn_anchor(TurnAnchor::User { submitted_at_ms: 1_000 });
		chat.apply_turn_anchor(TurnAnchor::Developer {
			submitted_at_ms: 2_000,
			synthetic:       true,
			user_initiated:  false,
		});
		chat.push_assistant_usage(assistant_usage(61_000));

		let text = frame_text(chat.render(Size::new(60, 12)).frame);
		assert!(text.contains("4k in"), "{text}");
		assert!(!text.contains('Δ'), "{text}");
	}
	#[test]
	fn outer_agent_start_clears_an_anchor_from_a_completed_run() {
		let mut chat = Chat::new(&ctx());
		chat.set_show_token_usage(true);
		chat.set_show_turn_time(true);
		chat.apply_turn_anchor(TurnAnchor::User { submitted_at_ms: 1_000 });
		chat.push_assistant_usage(assistant_usage(61_000));
		chat.apply_turn_anchor(TurnAnchor::AgentStart);
		chat.push_assistant_usage(assistant_usage(121_000));

		let text = frame_text(chat.render(Size::new(60, 12)).frame);
		assert_eq!(text.matches('Δ').count(), 1, "{text}");
	}

	#[test]
	fn skill_and_user_continue_each_seed_a_turn_anchor() {
		let mut skill = Chat::new(&ctx());
		skill.set_show_token_usage(true);
		skill.set_show_turn_time(true);
		skill.apply_turn_anchor(TurnAnchor::User { submitted_at_ms: 1_000 });
		skill.push_assistant_usage(assistant_usage(61_000));
		let skill_text = frame_text(skill.render(Size::new(60, 12)).frame);
		assert!(skill_text.contains("Δ1m"), "{skill_text}");

		let mut continued = Chat::new(&ctx());
		continued.set_show_token_usage(true);
		continued.set_show_turn_time(true);
		continued.apply_turn_anchor(TurnAnchor::Developer {
			submitted_at_ms: 1_000,
			synthetic:       true,
			user_initiated:  true,
		});
		continued.push_assistant_usage(assistant_usage(61_000));
		let continued_text = frame_text(continued.render(Size::new(60, 12)).frame);
		assert!(continued_text.contains("Δ1m"), "{continued_text}");
	}

	#[test]
	fn journal_replay_reproduces_live_elapsed_value() {
		let drive = |chat: &mut Chat| {
			chat.set_show_token_usage(true);
			chat.set_show_turn_time(true);
			chat.apply_turn_anchor(TurnAnchor::User { submitted_at_ms: 1_000 });
			chat.push_assistant_usage(assistant_usage(61_000));
		};
		let mut live = Chat::new(&ctx());
		drive(&mut live);
		let live_text = frame_text(live.render(Size::new(60, 12)).frame);

		let mut replay = Chat::new(&ctx());
		drive(&mut replay);
		let replay_text = frame_text(replay.render(Size::new(60, 12)).frame);
		assert_eq!(replay_text, live_text, "live:\n{live_text}\nreplay:\n{replay_text}");
		assert!(replay_text.contains("Δ1m"), "{replay_text}");
	}

	#[test]
	fn turn_time_setting_hides_delta_and_rebuilds_when_toggled() {
		let mut chat = Chat::new(&ctx());
		chat.set_show_token_usage(true);
		chat.apply_turn_anchor(TurnAnchor::User { submitted_at_ms: 1_000 });
		chat.push_assistant_usage(assistant_usage(61_000));
		let viewport = Size::new(60, 12);
		let before = frame_text(chat.render(viewport).frame);
		assert!(before.contains("4k in"), "{before}");
		assert!(!before.contains('Δ'), "{before}");

		let batch = chat.retirement_batch(viewport).expect("usage row retires");
		chat.mark_retired(&batch);
		chat.set_show_turn_time(true);
		let replay = chat
			.retirement_batch(viewport)
			.expect("setting toggle rebuilds transcript");
		let after = batch_text(&replay);
		assert!(after.contains("Δ1m"), "{after}");
	}

	#[test]
	fn newest_queued_row_owns_the_dequeue_hint() {
		let mut chat = Chat::new(&ctx());
		for message in ["first queued", "second queued"] {
			let _ = chat.apply_backend_event(BackendEvent::UserReplayed {
				text:   Str::new_static(message),
				chips:  Vec::new(),
				queued: true,
			});
		}
		let text = frame_text(chat.render(Size::new(60, 16)).frame);
		assert_eq!(text.matches("to edit").count(), 1, "{text}");
	}

	#[test]
	fn dequeue_restore_removes_queued_rows_and_refills_composer() {
		let mut chat = Chat::new(&ctx());
		let _ = chat.apply_backend_event(BackendEvent::UserReplayed {
			text:   sf!("later message"),
			chips:  Vec::new(),
			queued: true,
		});
		let _ = chat.apply_backend_event(BackendEvent::QueuedPromptsRestored(vec![QueuedPrompt {
			text:        sf!("later message"),
			attachments: Vec::new(),
		}]));
		assert_eq!(chat.composer_text(), "later message");
		let text = frame_text(chat.render(Size::new(60, 12)).frame);
		assert!(!text.contains("Queued"), "{text}");
	}

	#[test]
	fn history_rewind_stages_recovered_attachments() {
		let mut chat = Chat::new(&ctx());
		chat.push_user("original prompt", Vec::new());
		let _ = chat.apply_backend_event(BackendEvent::HistoryRewind {
			user_index:  0,
			text:        sf!("original prompt"),
			attachments: vec![
				crate::RestoredAttachment::Image { source: sf!("/nope/rewound.png") },
				crate::RestoredAttachment::Text(sf!("pasted body\nsecond line")),
			],
		});
		let staged = chat.composer_attachments();
		assert_eq!(staged.len(), 2, "image and paste both restage");
	}

	#[test]
	fn tool_result_images_render_inline_in_committed_cards() {
		let mut chat = Chat::new(&ctx());
		let path = env::temp_dir().join(format!("omp-scene-{}.png", process::id()));
		fs::write(&path, [
			137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8,
			6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 215, 99, 248, 207, 192, 240,
			31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
		])
		.expect("temporary PNG");
		let source: Str = path.to_string_lossy().into_owned().into();
		chat.tool_started("image", "render");
		chat.tool_image("image", source.clone());
		chat.tool_finished("image", ToolTerminal::Succeeded, ToolViewContent::Plain(source.clone()));
		let batch = chat
			.retirement_batch(Size::new(60, 0))
			.expect("queued image snapshot retires");
		let text = frame_text(&batch.frame);
		assert!(text.contains("omp-s") && text.contains(".png"), "{text:?}");
		let _ = fs::remove_file(path);
	}

	#[test]
	fn active_tools_contract_before_next_admission() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("one", "read");
		chat.tool_output("one", "a\nb\nc\nd");
		settle(&mut chat, Size::new(40, 14));
		chat.tool_started("two", "bash");
		let _ = chat.render_at(Size::new(40, 10), Duration::from_millis(500));
		assert_eq!(chat.blocks.phase(BlockOrdinal(1)), Some(crate::BlockPhase::Queued));
		for frame in 10..18 {
			let _ = chat.render_at(Size::new(40, 10), Duration::from_millis(frame * 60));
		}
		assert_eq!(chat.blocks.phase(BlockOrdinal(1)), Some(crate::BlockPhase::Active));
	}

	#[test]
	fn allocator_observes_mid_tween_tool_height() {
		let mut chat = Chat::new(&ctx());
		let roomy = Size::new(40, 10);
		let pressured = Size::new(40, 6);
		chat.tool_started("one", "read");
		chat.tool_output("one", "a\nb\nc\nd");
		let _ = chat.render_at(roomy, Duration::ZERO);
		let _ = chat.render_at(roomy, Duration::from_millis(200));
		let _ = chat.render_at(roomy, Duration::from_millis(400));
		chat.tool_started("two", "bash");
		let _ = chat.render_at(pressured, Duration::from_millis(400));
		let _ = chat.render_at(pressured, Duration::from_millis(580));
		let composer_rows = chat.composer_rows();
		assert_eq!(
			chat.live_tools[0].card_ui.height(),
			2,
			"target={} phase={:?} composer={}",
			chat.live_tools[0].target_height,
			chat.blocks.phase(BlockOrdinal(0)),
			composer_rows,
		);
		assert_eq!(chat.live_tools[0].target_height, 1);
		assert_eq!(chat.blocks.phase(BlockOrdinal(1)), Some(crate::BlockPhase::Queued));
		let _ = chat.render_at(pressured, Duration::from_millis(800));
		assert_eq!(chat.blocks.phase(BlockOrdinal(1)), Some(crate::BlockPhase::Active));
	}

	#[test]
	fn live_chrome_draws_error_download_and_celebration() {
		let mut chat = Chat::new(&ctx());
		chat.push_transcript_frame(TranscriptFrame {
			kind:   TranscriptFrameKind::Error,
			title:  Str::new_static("pinned failure"),
			detail: None,
		});
		chat.download_activity = Some(DownloadActivity::new(
			ModelDownloadProgress {
				label:      "weights".into(),
				downloaded: 5,
				total:      Some(10),
				complete:   false,
			},
			Duration::ZERO,
		));
		chat.celebration_until = Some(Duration::from_secs(3));
		let rendered = chat.render_at(Size::new(50, 12), Duration::from_secs(2));
		let text = frame_text(rendered.frame);
		assert!(text.contains("pinned failure"));
		assert!(text.contains("model · weights"));
		assert!(text.contains('✦') || text.contains('✧'));
	}

	#[test]
	fn live_blocks_and_voice_request_frame_cadence() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("one", "read");
		assert!(chat.next_wake().is_some_and(|wake| wake <= anim::FRAME));
		chat.live_tools.clear();
		chat.start_live_voice();
		assert!(
			chat
				.next_wake()
				.is_some_and(|wake| wake <= LIVE_VOICE_FRAME)
		);
	}

	#[test]
	fn settled_blocks_stay_in_viewport_until_capacity_pressure() {
		let mut chat = Chat::new(&ctx());
		chat.push_user("hello there", Vec::new());
		chat.push_notice("finalized notice");
		// Roomy viewport: nothing retires and the settled snapshots render in
		// the mutable viewport.
		let roomy = Size::new(40, 20);
		assert!(chat.retirement_batch(roomy).is_none());
		let rendered = chat.render(roomy);
		let text = frame_text(rendered.frame);
		assert!(text.contains("hello there"), "{text:?}");
		assert!(text.contains("finalized notice"), "{text:?}");
		// Shrinking below the live tail retires the head, but only while the
		// remainder still fills the live area — a gap above the composer is
		// worse than keeping a clipped block on screen.
		let tight = Size::new(40, 7);
		for index in 0..8 {
			chat.push_notice(format!("tail {index}"));
		}
		let batch = chat.retirement_batch(tight).expect("pressure retires");
		assert_eq!(batch.range.start, 0);
		chat.mark_retired(&batch);
		assert!(chat.retirement_batch(tight).is_none());
	}

	#[test]
	fn retirement_projects_the_post_commit_viewport_before_acknowledgement() {
		let mut chat = Chat::new(&ctx());
		chat.push_notice("head\nhead\nhead\nhead");
		chat.push_notice("newly visible\nnewly visible\nnewly visible\nnewly visible");
		let viewport = Size::new(40, 7);
		let batch = chat
			.retirement_batch(viewport)
			.expect("pressure retires head");
		assert_eq!(batch.range, 0..1);

		let projected = frame_text(chat.render_after_retirement(viewport, &batch).frame);
		assert!(!projected.contains("head"), "{projected}");
		assert!(projected.contains("newly visible"), "{projected}");
	}

	#[test]
	fn settled_blocks_reflow_to_the_current_width() {
		let mut chat = Chat::new(&ctx());
		chat.push_notice("alpha beta gamma delta epsilon zeta");
		let wide = chat.render(Size::new(60, 20));
		let wide_text = frame_text(wide.frame);
		assert!(
			wide_text
				.lines()
				.any(|line| line.contains("alpha beta gamma delta epsilon zeta"))
		);
		let narrow = chat.render(Size::new(18, 20));
		let narrow_text = frame_text(narrow.frame);
		assert!(narrow_text.contains("alpha"), "{narrow_text:?}");
		assert!(
			!narrow_text
				.lines()
				.any(|line| line.contains("alpha beta gamma delta epsilon zeta")),
			"{narrow_text:?}"
		);
	}

	#[test]
	fn history_flush_cancels_destructive_replay_and_emits_only_unretired_rows() {
		let mut chat = Chat::new(&ctx());
		chat.push_notice("already committed");
		let drained = Size::new(40, 0);
		let committed = chat.retirement_batch(drained).expect("initial retire");
		chat.mark_retired(&committed);
		chat.push_notice("unretired tail");
		chat.begin_history_replay(HistoryReplay::Rebuild);

		chat.begin_history_flush();
		let flush = chat
			.flush_retirement_batch(drained)
			.expect("unretired tail flushes");
		assert!(flush.replay_plan().is_none());
		let text = frame_text(&flush.frame);
		assert!(text.contains("unretired tail"), "{text}");
		assert!(!text.contains("already committed"), "{text}");
	}

	#[test]
	fn replay_reoffers_committed_rows_without_rewinding_the_frontier() {
		let mut chat = Chat::new(&ctx());
		chat.push_notice("replayed row");
		let drained = Size::new(40, 0);
		let batch = chat.retirement_batch(drained).expect("initial retire");
		chat.mark_retired(&batch);
		let frontier = chat.blocks.frontier();
		assert!(chat.retirement_batch(drained).is_none());

		chat.begin_history_replay(HistoryReplay::Append);
		assert_eq!(chat.blocks.frontier(), frontier);
		let replay = chat.retirement_batch(drained).expect("replay retire");
		assert_eq!(replay.range, 0..frontier);
		assert!(batch_text(&replay).contains("replayed row"));
		chat.mark_retired(&replay);
		assert_eq!(chat.blocks.frontier(), frontier);
		assert!(chat.retirement_batch(drained).is_none());
	}

	#[test]
	fn settled_replay_adopts_the_committed_tail_and_keeps_it_across_paints() {
		let mut chat = Chat::new(&ctx());
		let small = Size::new(40, 0);
		for index in 0..6 {
			chat.push_notice(format!("hist-{index}"));
		}
		let batch = chat.retirement_batch(small).expect("finalized rows retire");
		chat.mark_retired(&batch);
		let frontier = chat.blocks.frontier();

		// A taller settled viewport leaves a blank band above the composer.
		let tall = Size::new(40, 24);
		let before = frame_text(chat.render(tall).frame);
		assert!(!before.contains("hist-5"), "committed rows left the viewport: {before}");

		chat.begin_history_replay(HistoryReplay::Rebuild);
		let replay = chat.retirement_batch(tall).expect("replay batch");
		let rendered = frame_text(chat.render_after_retirement(tall, &replay).frame);
		assert!(rendered.contains("hist-5"), "replay adopts the tail on screen: {rendered}");
		// Adopted rows never also scroll into native history.
		assert!(!batch_text(&replay).contains("hist-5"), "adopted rows duplicated into history");
		chat.mark_retired(&replay);
		assert_eq!(chat.blocks.frontier(), frontier, "adoption never rewinds the frontier");

		// The regression: the very next history-neutral paints kept erasing the
		// adopted rows, flashing the transcript for a single frame.
		for _ in 0..3 {
			let after = frame_text(chat.render(tall).frame);
			assert!(after.contains("hist-5"), "post-replay paint erased the resident band: {after}");
		}
	}

	#[test]
	fn resident_band_retires_ahead_of_newer_commits() {
		let mut chat = Chat::new(&ctx());
		for index in 0..4 {
			chat.push_notice(format!("hist-{index}"));
		}
		let small = Size::new(40, 0);
		let batch = chat.retirement_batch(small).expect("finalized rows retire");
		chat.mark_retired(&batch);
		let tall = Size::new(40, 24);
		let _ = chat.render(tall);
		chat.begin_history_replay(HistoryReplay::Rebuild);
		let replay = chat.retirement_batch(tall).expect("replay batch");
		chat.mark_retired(&replay);
		assert!(chat.band.is_some(), "replay leaves a resident band");

		for index in 0..40 {
			chat.push_notice(format!("new-{index}"));
		}
		let batch = chat
			.retirement_batch(tall)
			.expect("pressure commits the finalized prefix");
		let committed = frame_text(&batch.frame);
		let band_row = committed.find("hist-0").expect("band rows lead the commit");
		let new_row = committed.find("new-0").expect("commit rows follow");
		assert!(band_row < new_row, "band rows must precede newer commits: {committed}");
		assert!(chat.band.is_none(), "a commit consumes the whole band");
		chat.mark_retired(&batch);
	}

	#[test]
	fn live_growth_scrolls_band_overflow_into_history_without_a_commit() {
		let mut chat = Chat::new(&ctx());
		for index in 0..4 {
			chat.push_notice(format!("hist-{index}"));
		}
		let small = Size::new(40, 0);
		let batch = chat.retirement_batch(small).expect("finalized rows retire");
		chat.mark_retired(&batch);
		let tall = Size::new(40, 20);
		let _ = chat.render(tall);
		chat.begin_history_replay(HistoryReplay::Rebuild);
		let replay = chat.retirement_batch(tall).expect("replay batch");
		chat.mark_retired(&replay);
		assert!(chat.band.is_some(), "replay leaves a resident band");

		// An active (unfinalized) stream grows into the band: its oldest rows
		// must scroll into native history before a paint can cover them.
		chat.set_smooth_streaming(false);
		chat.begin_assistant("live");
		let long = (0..40)
			.map(|row| format!("line-{row}"))
			.collect::<Vec<_>>()
			.join("\n\n");
		chat.append_assistant("live", &long);
		let batch = chat
			.retirement_batch(tall)
			.expect("band overflow retires without a commit");
		assert!(matches!(batch.kind, RetirementKind::Band));
		assert!(batch.range.is_empty(), "band retirement never advances the frontier");
		assert!(frame_text(&batch.frame).contains("hist-0"), "oldest band rows scroll first");
		chat.mark_retired(&batch);
	}

	#[test]
	fn clear_history_does_not_mutate_committed_replay_rows() {
		let mut chat = Chat::new(&ctx());
		chat.push_notice("immutable committed row");
		let drained = Size::new(40, 0);
		let batch = chat.retirement_batch(drained).expect("initial retire");
		chat.mark_retired(&batch);

		chat.clear_history();
		chat.begin_history_replay(HistoryReplay::Append);
		let replay = chat
			.retirement_batch(drained)
			.expect("committed row replays");
		assert!(batch_text(&replay).contains("immutable committed row"));
		assert!(!batch_text(&replay).contains("history cleared"));
	}

	#[test]
	fn live_block_memory_bound_forces_retirement_offers() {
		let mut chat = Chat::new(&ctx());
		for index in 0..MAX_LIVE_BLOCKS {
			chat.push_notice(format!("row {index}"));
		}
		// A huge viewport has room, but the memory bound still forces an offer.
		let batch = chat
			.retirement_batch(Size::new(40, u16::MAX))
			.expect("memory pressure retires");
		assert_eq!(batch.range.start, 0);
	}

	#[test]
	fn emergency_row_keeps_latest_settled_assistant_prose_visible() {
		let mut chat = Chat::new(&ctx());
		chat.begin_assistant("active-prefix");
		chat.append_assistant("active-prefix", "stale active prefix");
		let answer = AssistantEntry::new("Implemented answer".to_owned(), 40, &chat.ctx);
		chat.enqueue_final(Entry::Assistant(answer));
		chat.tool_started("tool", "read");
		settle(&mut chat, Size::new(40, 16));

		let tight = Size::new(40, 7);
		assert!(
			chat.retirement_batch(tight).is_none(),
			"active prefix must block ordered retirement"
		);
		let text = frame_text(chat.render_at(tight, Duration::from_millis(900)).frame);
		assert!(text.contains("Implemented answer"), "{text}");
	}

	#[test]
	fn transcript_text_receives_surplus_before_live_tool_cards() {
		let mut chat = Chat::new(&ctx());
		chat.push_notice("Answer one\nAnswer two\nAnswer three\nAnswer four");
		chat.tool_started("tool", "bash");
		chat.tool_output("tool", "one\ntwo\nthree\nfour\nfive");
		settle(&mut chat, Size::new(40, 18));
		let tight = (6..18)
			.map(|height| Size::new(40, height))
			.find(|viewport| chat.chrome_layout(*viewport, Duration::from_secs(1)).h_live == 6)
			.expect("fixture can create a six-row transcript viewport");

		let text = frame_text(chat.render_at(tight, Duration::from_secs(1)).frame);
		assert!(text.contains("Answer one"), "{text}");
		assert_eq!(chat.live_tools[0].target_height, 1);
	}

	#[test]
	fn later_tool_finishing_first_blocks_retirement() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("head", "bash");
		chat.tool_started("later", "read");
		settle(&mut chat, Size::new(40, 8));
		chat.tool_finished("later", ToolTerminal::Succeeded, ToolViewContent::Plain("done".into()));
		settle(&mut chat, Size::new(40, 8));
		assert!(chat.retirement_batch(Size::new(40, 0)).is_none());
	}

	#[test]
	fn head_finalization_releases_contiguous_ordinal_run() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("head", "bash");
		chat.tool_started("later", "read");
		settle(&mut chat, Size::new(40, 8));
		chat.tool_finished(
			"later",
			ToolTerminal::Succeeded,
			ToolViewContent::Plain("later done".into()),
		);
		chat.tool_finished(
			"head",
			ToolTerminal::Succeeded,
			ToolViewContent::Plain("head done".into()),
		);
		settle(&mut chat, Size::new(40, 8));
		let batch = chat
			.retirement_batch(Size::new(40, 0))
			.expect("head unblocks later final");
		assert_eq!(batch.range, 0..2);
		let text = frame_text(&batch.frame);
		assert!(text.find("head done").unwrap() < text.find("later done").unwrap());
	}

	#[test]
	fn overflow_summary_appears_under_tiny_viewport() {
		let mut chat = Chat::new(&ctx());
		for id in ["a", "b", "c"] {
			chat.tool_started(id, "read");
		}
		settle(&mut chat, Size::new(30, 10));
		let rendered = chat.render_at(Size::new(30, 5), Duration::from_millis(500));
		assert!(frame_text(rendered.frame).contains("blocks"));
	}

	#[test]
	fn queued_demand_does_not_displace_the_only_active_row() {
		let mut chat = Chat::new(&ctx());
		let viewport = Size::new(30, 5);
		chat.tool_started("active", "read");
		let _ = chat.render_at(viewport, Duration::from_millis(100));
		chat.tool_started("queued", "read");
		let rendered = chat.render_at(viewport, Duration::from_millis(200));
		let text = frame_text(rendered.frame);

		assert_eq!(chat.blocks.phase(BlockOrdinal(0)), Some(crate::BlockPhase::Active));
		assert_eq!(chat.blocks.phase(BlockOrdinal(1)), Some(crate::BlockPhase::Queued));
		assert!(text.contains("read"), "{text}");
		assert!(!text.contains("blocks"), "{text}");
	}

	#[test]
	fn queued_only_demand_admits_without_painting_a_summary() {
		let mut chat = Chat::new(&ctx());
		let viewport = Size::new(30, 5);
		chat.tool_started("first", "first-tool");
		chat.tool_started("second", "second-tool");
		let first = frame_text(chat.render_at(viewport, Duration::from_millis(100)).frame);

		assert_eq!(chat.blocks.phase(BlockOrdinal(0)), Some(crate::BlockPhase::Active));
		assert_eq!(chat.blocks.phase(BlockOrdinal(1)), Some(crate::BlockPhase::Queued));
		assert!(!first.contains("blocks"), "{first}");
		let second = frame_text(chat.render_at(viewport, Duration::from_millis(200)).frame);
		assert!(second.contains("first-tool"), "{second}");
		assert!(!second.contains("blocks"), "{second}");
	}

	#[test]
	fn streaming_assistant_stays_viewport_owned_until_finalization() {
		let mut chat = Chat::new(&ctx());
		chat.begin_assistant("assistant");
		chat.append_assistant("assistant", "completed paragraph\n\nmutable tail");
		let viewport = Size::new(40, 0);

		assert!(
			chat.retirement_batch(viewport).is_none(),
			"active assistant content must never enter native history"
		);
		chat.append_assistant("assistant", "\nrevised suffix");
		chat.end_assistant("assistant");

		let batch = chat
			.flush_retirement_batch(viewport)
			.expect("finalized assistant retires as one immutable value");
		let text = frame_text(&batch.frame);
		assert!(text.contains("completed paragraph"), "{text}");
		assert!(text.contains("revised suffix"), "{text}");
	}

	#[test]
	fn finalized_thinking_retires_only_after_finalization() {
		let mut chat = Chat::new(&ctx());
		chat.begin_thinking("thinking");
		chat.append_assistant("thinking", "private reasoning");
		assert!(chat.retirement_batch(Size::new(40, 0)).is_none());
		chat.end_assistant("thinking");

		let batch = chat
			.flush_retirement_batch(Size::new(40, 0))
			.expect("final thinking snapshot retires");
		let text = frame_text(&batch.frame);
		assert!(text.contains("private reasoning"), "{text}");
	}

	#[test]
	fn assistant_stream_is_clipped_to_allocation() {
		let mut chat = Chat::new(&ctx());
		chat.begin_assistant("a");
		chat.append_assistant("a", "oldest\nmiddle\nnewest");
		settle(&mut chat, Size::new(30, 8));
		let rendered = chat.render_at(Size::new(30, 6), Duration::from_millis(500));
		let text = frame_text(rendered.frame);
		assert!(text.contains("newest"));
		assert!(!text.contains("oldest"));
	}

	#[test]
	fn hidden_thinking_never_renders_stream_or_snapshot() {
		let mut chat = Chat::new(&ctx());
		let viewport = Size::new(40, 10);
		chat.set_hide_thinking(true);
		chat.begin_thinking("a");
		chat.append_assistant("a", "private reasoning");

		assert!(!frame_text(chat.render(viewport).frame).contains("private reasoning"));
		chat.end_assistant("a");
		assert!(!frame_text(chat.render(viewport).frame).contains("private reasoning"));
		// The entry is retained so Ctrl+T can reveal it later.
		assert!(matches!(chat.entries.get(&BlockOrdinal(0)), Some(Entry::Thinking(_))));
	}

	#[test]
	fn ctrl_t_toggles_thinking_visibility_for_all_blocks_including_future_ones() {
		let mut chat = Chat::new(&ctx());
		let viewport = Size::new(40, 14);
		chat.begin_thinking("first");
		chat.append_assistant("first", "first deliberation");
		chat.end_assistant("first");
		settle(&mut chat, viewport);
		assert!(frame_text(chat.render(viewport).frame).contains("first deliberation"));

		assert_eq!(chat.handle_key(Key::Ctrl('t')), ChatKey::Consumed);
		assert!(!frame_text(chat.render(viewport).frame).contains("first deliberation"));

		// Future blocks stay hidden while the toggle is off.
		chat.begin_thinking("second");
		chat.append_assistant("second", "second deliberation");
		chat.end_assistant("second");
		let text = frame_text(chat.render(viewport).frame);
		assert!(!text.contains("second deliberation"), "{text}");

		// Toggling back reveals every unretired block.
		chat.toggle_thinking();
		let text = frame_text(chat.render(viewport).frame);
		assert!(text.contains("first deliberation"), "{text}");
		assert!(text.contains("second deliberation"), "{text}");
	}
	#[test]
	fn toggle_tool_visibility_hides_tool_cards() {
		let mut chat = Chat::new(&ctx());
		let viewport = Size::new(40, 14);
		chat.tool_started("read", "read");
		chat.tool_output("read", "tool output");
		settle(&mut chat, viewport);
		assert!(frame_text(chat.render(viewport).frame).contains("tool output"));

		assert_eq!(chat.handle_key(Key::ToggleToolVisibility), ChatKey::Consumed);
		assert!(!frame_text(chat.render(viewport).frame).contains("tool output"));
	}

	#[test]
	fn copy_prompt_uses_latest_user_entry() {
		let mut chat = Chat::new(&ctx());
		chat.push_user("first prompt", Vec::new());
		chat.push_user("latest prompt", Vec::new());

		assert_eq!(chat.handle_key(Key::CopyPrompt), ChatKey::Consumed);
		assert_eq!(chat.take_copied(), Some(sf!("latest prompt")));
	}

	#[test]
	fn clear_history_preserves_composer_and_retires_marker() {
		let mut chat = Chat::new(&ctx());
		chat.set_composer_text("draft");
		chat.push_notice("old");
		chat.clear_history();
		assert_eq!(chat.composer_text(), "draft");
		let batch = chat
			.retirement_batch(Size::new(40, 0))
			.expect("tombstone and marker retire");
		assert!(frame_text(&batch.frame).contains("history cleared"));
	}

	#[test]
	fn rail_width_accumulates_all_visible_rails() {
		let rails = RailWidths::default()
			.accumulate(true, 12)
			.accumulate(false, 30)
			.accumulate(true, 8);
		assert_eq!(rails, RailWidths { left: 20, right: 30 });
		assert_eq!(rails.content_width(80), 30);
	}
	#[test]
	fn error_transcript_frame_pins_status_without_a_duplicate_entry() {
		let mut chat = Chat::new(&ctx());
		let before = chat.entries.len();
		chat.push_transcript_frame(TranscriptFrame {
			kind:   TranscriptFrameKind::Error,
			title:  Str::new_static("Agent error"),
			detail: Some(Str::new_static("terminal turn error (Auth)")),
		});
		assert_eq!(chat.entries.len(), before, "error frames must not append a notice entry");
		assert_eq!(
			chat.pinned_error.as_deref(),
			Some("error · Agent error — terminal turn error (Auth)")
		);
		chat.push_transcript_frame(TranscriptFrame {
			kind:   TranscriptFrameKind::Recovery,
			title:  Str::new_static("Recovered on attempt 2"),
			detail: None,
		});
		assert!(chat.pinned_error.is_none(), "recovery clears the pinned failure");
	}
}
