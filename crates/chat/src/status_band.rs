//! Composer status band: pi's band-layout status line docked to the
//! composer (`status-line/component.ts` `#buildStatusLine`).

use core::fmt::Write as _;
use std::time::Duration;

use omp_core::{Str, sf};
use omp_tui::{
	Cached, Charset, Color, Component, Icon, PaintCtx, Prop, Props, Rect, Slot, Style, UiContext,
	anim::{Easing, Tween},
	cell_width,
	components::{
		CompactionBoundaries, ContextGauge, GaugeCell, Segment, Status, compaction_boundary_color,
		compaction_threshold_color, spend_label, write_compact_count,
	},
	next_slot,
};
use smallvec::SmallVec;

use crate::chrome::STATUS_ID;

/// Longest path label in the status band (pi `clampPathLength` default).
const PATH_MAX: u16 = 40;

/// Background compaction speculation state shown on the gauge tick (pi
/// `compactionSpeculation`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Speculation {
	/// No speculative compaction in flight.
	#[default]
	None,
	/// A background summary is being produced; the tick pulses.
	Running,
	/// A summary is ready to apply at the threshold; the tick holds accent.
	Armed,
}

/// Worst status across the advisor roster (pi `getAdvisorStatusOverview`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdvisorHealth {
	/// Every advisor is running.
	Running,
	/// At least one advisor is out of quota.
	QuotaExhausted,
	/// At least one advisor failed.
	Error,
	/// Everything is paused or has no model.
	Paused,
}

/// Advisor badge after the model name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdvisorBadge {
	/// Roster health; picks the badge color.
	pub health:  AdvisorHealth,
	/// Every advisor finished reviewing the yielded turn (closed eye).
	pub yielded: bool,
}

/// Facts painted by the composer status band.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusFacts {
	/// Short model label.
	pub model:             Str,
	/// Reasoning level (`off`, `minimal` … `max`) when the model can reason;
	/// `None` for models without thinking.
	pub thinking:          Option<Str>,
	/// Whether the thinking glyph replaces the model icon instead of trailing
	/// the name as ` · <level>` (pi `statusLine.compactThinkingLevel`).
	pub compact_thinking:  bool,
	/// Fast mode is on (`ai_fastmode`); the fast icon trails the model name.
	pub fast:              bool,
	/// Advisor roster badge after the model name, when advisors are
	/// configured.
	pub advisor:           Option<AdvisorBadge>,
	/// Project directory label: home-shortened and root-stripped, not yet
	/// clamped (the band clamps to the width it has).
	pub cwd:               Str,
	/// Whether the project lives under a scratch root (pi `scratchFolder`
	/// icon instead of the folder icon).
	pub scratch:           bool,
	/// Checked-out git branch, an observer-local fact the app supplies.
	pub branch:            Option<Str>,
	/// Working tree has staged, unstaged, or untracked changes.
	pub dirty:             bool,
	/// User-facing session title, the elastic right-group chip.
	pub session_name:      Option<Str>,
	/// Tokens in the last inference request (context usage).
	pub tokens:            u64,
	/// Total context window when known.
	pub context_window:    Option<u64>,
	/// Auto-compaction threshold as a whole percent of the window
	/// (`ai_compact_threshold`), the gauge's tick position.
	pub compact_percent:   u8,
	/// Background compaction speculation, animating the threshold tick.
	pub speculation:       Speculation,
	/// Cumulative input tokens across the session.
	pub tokens_in:         u64,
	/// Cumulative output tokens across the session.
	pub tokens_out:        u64,
	/// Cumulative prompt-cache tokens read (excluded from the total: it
	/// re-reads the whole cached context every turn).
	pub cache_read:        u64,
	/// Cumulative prompt-cache tokens written.
	pub cache_write:       u64,
	/// Output throughput of the last receipt.
	pub tokens_per_second: Option<f32>,
	/// Cumulative spend in nano-US dollars.
	pub cost_nano_usd:     u64,
	/// The route bills to a subscription rather than metered usage.
	pub subscription:      bool,
	/// Premium requests consumed (Copilot-style plans).
	pub premium_requests:  u64,
	/// Account label of the resolved credential, when known.
	pub account:           Option<Str>,
	/// Start of the in-flight turn on the presentation clock; `Some` swaps
	/// the brand glyph for the spinner and elapsed-time timer.
	pub working:           Option<Duration>,
}

impl Default for StatusFacts {
	fn default() -> Self {
		Self {
			model:             Str::default(),
			thinking:          None,
			compact_thinking:  true,
			fast:              false,
			advisor:           None,
			cwd:               Str::default(),
			scratch:           false,
			branch:            None,
			dirty:             false,
			session_name:      None,
			tokens:            0,
			context_window:    None,
			compact_percent:   80,
			speculation:       Speculation::None,
			tokens_in:         0,
			tokens_out:        0,
			cache_read:        0,
			cache_write:       0,
			tokens_per_second: None,
			cost_nano_usd:     0,
			subscription:      false,
			premium_requests:  0,
			account:           None,
			working:           None,
		}
	}
}

/// Project label for the status band.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathLabel {
	/// Display text, not yet clamped.
	pub text:    Str,
	/// Whether the path sits under a scratch (temporary) root.
	pub scratch: bool,
}

/// Scratch roots pi's `path` segment relabels with the trash icon: the
/// platform temp dir plus the conventional temp locations.
const SCRATCH_ROOTS: [&str; 4] = ["/tmp", "/var/tmp", "/private/tmp", "/private/var/tmp"];
/// Roots pi's `stripWorkPrefix` drops from the label.
const DISPLAY_ROOTS: [&str; 1] = ["/work"];

/// Path relative to `root` when `path` sits strictly inside it.
fn within_root<'a>(root: &str, path: &'a str) -> Option<&'a str> {
	let root = root.trim_end_matches('/');
	if root.is_empty() {
		return None;
	}
	path
		.strip_prefix(root)
		.and_then(|rest| rest.strip_prefix('/'))
		.filter(|rest| !rest.is_empty())
}

/// Labels a project path for the status band like pi's `path` segment.
///
/// Scratch roots become relative labels with the scratch icon, `/work` and
/// `~/Projects` are stripped, and the home prefix becomes `~`. `tmp` is the
/// platform temp directory (`std::env::temp_dir`).
#[must_use]
pub fn display_path(path: &str, home: Option<&str>, tmp: Option<&str>) -> PathLabel {
	let home = home.filter(|home| !home.is_empty());
	let home_tmp = home.map(|home| format!("{home}/tmp"));
	let scratch_roots = tmp
		.into_iter()
		.chain(home_tmp.as_deref())
		.chain(SCRATCH_ROOTS);
	for root in scratch_roots {
		if path == root.trim_end_matches('/') {
			return PathLabel { text: shorten_home(path, home), scratch: true };
		}
		if let Some(relative) = within_root(root, path) {
			return PathLabel { text: Str::new(relative), scratch: true };
		}
	}
	let projects = home.map(|home| format!("{home}/Projects"));
	for root in projects.as_deref().into_iter().chain(DISPLAY_ROOTS) {
		if let Some(relative) = within_root(root, path) {
			return PathLabel { text: Str::new(relative), scratch: false };
		}
	}
	PathLabel { text: shorten_home(path, home), scratch: false }
}

/// `~` for the home prefix (pi `shortenPath`).
fn shorten_home(path: &str, home: Option<&str>) -> Str {
	match home {
		Some(home) if path == home => Str::new_static("~"),
		Some(home) => match path.strip_prefix(home) {
			Some(rest) if rest.starts_with('/') => Str::new(format!("~{rest}")),
			_ => Str::new(path),
		},
		None => Str::new(path),
	}
}

/// Left-clamps a label to `max` cells with a leading ellipsis (pi
/// `clampPathLength`).
fn clamp_path(text: &str, max: u16) -> Str {
	if cell_width(text) <= max {
		return Str::new(text);
	}
	let budget = max.saturating_sub(1);
	let mut start = text.len();
	let mut used = 0;
	for (index, ch) in text.char_indices().rev() {
		let glyph = cell_width(&text[index..index + ch.len_utf8()]);
		if used + glyph > budget {
			break;
		}
		used += glyph;
		start = index;
	}
	Str::new(format!("…{}", &text[start..]))
}

/// Right-clamps a label to `max` cells with a trailing ellipsis (pi
/// `truncateToWidth` on the session title).
fn clamp_end(text: &str, max: u16) -> Str {
	if cell_width(text) <= max {
		return Str::new(text);
	}
	let budget = max.saturating_sub(1);
	let mut end = 0;
	let mut used = 0;
	for (index, ch) in text.char_indices() {
		let glyph = cell_width(&text[index..index + ch.len_utf8()]);
		if used + glyph > budget {
			break;
		}
		used += glyph;
		end = index + ch.len_utf8();
	}
	Str::new(format!("{}…", &text[..end]))
}

/// Turn timer in the brand slot: whole seconds, then minutes, then hours
/// capped at 99 (pi `brandTimer`).
fn elapsed_label(out: &mut String, elapsed: Duration) {
	let seconds = elapsed.as_secs();
	if seconds < 60 {
		let _ = write!(out, "{seconds}s");
	} else if seconds < 3_600 {
		let _ = write!(out, "{}m", seconds / 60);
	} else {
		let _ = write!(out, "{}h", (seconds / 3_600).min(99));
	}
}

/// Themed icon of a reasoning level (pi `theme.thinking[level]`).
fn thinking_icon(level: &str) -> Icon {
	match level {
		"off" => Icon::Disabled,
		"auto" => Icon::AutoPending,
		"minimal" => Icon::Minimal,
		"low" => Icon::Low,
		"medium" => Icon::Medium,
		"high" => Icon::High,
		"xhigh" => Icon::Xhigh,
		"max" => Icon::Max,
		_ => Icon::Model,
	}
}

/// Glyph of a reasoning level for the compact model icon (pi
/// `thinkingGlyph`): the first token of the themed level label.
fn thinking_glyph(charset: Charset, level: &str) -> &'static str {
	charset
		.icon(thinking_icon(level))
		.split_whitespace()
		.next()
		.unwrap_or_default()
}

/// Brand-color fade across working-state edges (pi `BRAND_FADE_MS`).
const BRAND_FADE: Duration = Duration::from_millis(450);
/// Repaint cadence while the brand fade is in flight (pi
/// `BRAND_FADE_FRAME_MS`).
const BRAND_FADE_FRAME: Duration = Duration::from_millis(40);
/// Narrowest path label pi keeps before dropping other segments.
const PATH_MIN: u16 = 4;
/// Narrowest session title pi keeps before dropping right segments.
const SESSION_NAME_MIN: u16 = 8;
/// Half-period of the compaction speculation pulse (pi
/// `#syncSpeculationBlink` `setInterval(…, 600)`).
const SPECULATION_BLINK: Duration = Duration::from_millis(600);

/// Whether the speculation pulse shows the accent phase at `now`: pi starts
/// `on` and toggles every blink period.
const fn speculation_on(now: Duration) -> bool {
	(now.as_millis() / SPECULATION_BLINK.as_millis()).is_multiple_of(2)
}

/// Next presentation instant the speculation pulse flips.
fn speculation_flip(now: Duration) -> Duration {
	let period = SPECULATION_BLINK.as_millis();
	let next = (now.as_millis() / period + 1) * period;
	Duration::from_millis(u64::try_from(next).unwrap_or(u64::MAX))
}

/// Identity of one band segment, for overflow policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Chip {
	Brand,
	Model,
	Path,
	Git,
	Session,
	TokenIn,
	TokenOut,
	TokenTotal,
	TokenRate,
	Cost,
}

/// One rendered chip: identity, text, and foreground.
type Label = (Chip, Str, Color);

/// Both fitted groups of the band.
struct Layout {
	left:  SmallVec<Label, 4>,
	right: SmallVec<Label, 6>,
}

/// One-row composer status in pi's band layout.
///
/// The powerline group (brand, model, path, git) is bridged by the embedded
/// context gauge to the right-docked group (session title, token counts,
/// throughput, spend). Overflow follows pi's `#buildStatusLine`: the gauge
/// keeps room for its labels, the session title shrinks first, then right
/// chips pop from the right, then the path shrinks, then non-path left chips
/// drop from the right so the working directory survives the longest.
pub struct StatusBand {
	props: Props,
	slot:  Slot,
	facts: StatusFacts,
	/// Brand foreground easing between idle and working; `None` until the
	/// first paint knows the theme.
	fade:  Option<Tween<Color>>,
}

impl StatusBand {
	/// Creates a band for the launch facts.
	#[must_use]
	pub fn new(facts: StatusFacts) -> Self {
		let mut props = Props::new();
		props.set(Prop::Id, STATUS_ID);
		Self { props, slot: next_slot(), facts, fade: None }
	}

	/// Replaces the facts; returns whether anything changed.
	pub fn set_facts(&mut self, facts: StatusFacts) -> bool {
		if self.facts == facts {
			return false;
		}
		self.facts = facts;
		true
	}

	/// Model icon: the thinking glyph in compact mode, else the model icon.
	fn model_icon(&self, charset: Charset) -> &'static str {
		match self.facts.thinking.as_deref() {
			Some(level) if self.facts.compact_thinking => thinking_glyph(charset, level),
			_ => charset.icon(Icon::Model),
		}
	}

	/// Advisor badge glyph and its cell offset inside the model chip, when
	/// advisors are configured (pi paints it as its own span between the
	/// name and the tail).
	fn advisor_span(&self, charset: Charset) -> Option<(u16, &'static str)> {
		let badge = self.facts.advisor?;
		let icon = charset.icon(if badge.yielded {
			Icon::AdvisorClosed
		} else {
			Icon::Advisor
		});
		let mut offset = cell_width(self.model_icon(charset))
			.saturating_add(1)
			.saturating_add(cell_width(&self.facts.model));
		if self.facts.fast {
			offset = offset
				.saturating_add(1)
				.saturating_add(cell_width(charset.icon(Icon::Fast)));
		}
		Some((offset.saturating_add(1), icon))
	}

	/// Model chip text (pi `modelSegment`): icon, name, fast icon, advisor
	/// badge, and the ` · <level>` tail when the level is not compact.
	fn model_label(&self, charset: Charset) -> Str {
		let mut text = format!("{} {}", self.model_icon(charset), self.facts.model);
		if self.facts.fast {
			let _ = write!(text, " {}", charset.icon(Icon::Fast));
		}
		if let Some((_, icon)) = self.advisor_span(charset) {
			let _ = write!(text, " {icon}");
		}
		if let Some(level) = self.facts.thinking.as_deref()
			&& !self.facts.compact_thinking
		{
			let _ =
				write!(text, "{}{} {level}", charset.icon(Icon::Dot), thinking_glyph(charset, level));
		}
		Str::new(text)
	}

	/// Left-group labels at `path_max`, in band order.
	fn left_labels(&self, pc: &PaintCtx<'_>, path_max: u16) -> SmallVec<Label, 4> {
		let charset = pc.ctx.charset;
		let theme = pc.ctx.theme;
		let mut labels = SmallVec::new();
		let mut brand = String::new();
		match self.facts.working {
			Some(started) => {
				brand.push_str(charset.spinner().at(pc.now));
				brand.push(' ');
				elapsed_label(&mut brand, pc.now.saturating_sub(started));
			},
			None => brand.push_str(charset.icon(Icon::Omp)),
		}
		brand.push(' ');
		let brand_color = self.fade.map_or(theme.muted, |fade| fade.sample(pc.now));
		labels.push((Chip::Brand, Str::new(brand), brand_color));
		labels.push((Chip::Model, self.model_label(charset), theme.ok));
		if !self.facts.cwd.is_empty() {
			let icon = charset.icon(if self.facts.scratch {
				Icon::ScratchFolder
			} else {
				Icon::Folder
			});
			let path = clamp_path(&self.facts.cwd, path_max);
			labels.push((Chip::Path, sf!("{icon} {path}"), theme.secondary));
		}
		if let Some(branch) = self.facts.branch.as_deref().filter(|b| !b.is_empty()) {
			let icon = charset.icon(Icon::Branch);
			let (label, color) = if self.facts.dirty {
				(sf!("{icon} {branch} *"), theme.warn)
			} else {
				(sf!("{icon} {branch}"), theme.info)
			};
			labels.push((Chip::Git, label, color));
		}
		labels
	}

	/// Right-group labels with the session title clamped to `name_max`, in
	/// pi's default right order.
	fn right_labels(&self, pc: &PaintCtx<'_>, name_max: u16) -> SmallVec<Label, 6> {
		let charset = pc.ctx.charset;
		let theme = pc.ctx.theme;
		let facts = &self.facts;
		let mut labels = SmallVec::new();
		if let Some(name) = facts.session_name.as_deref().filter(|n| !n.is_empty()) {
			labels.push((Chip::Session, clamp_end(name, name_max), theme.accent));
		}
		let count = |icon: Icon, value: u64| {
			let mut text = String::from(charset.icon(icon));
			text.push(' ');
			let _ = write_compact_count(&mut text, value);
			Str::new(text)
		};
		if facts.tokens_in > 0 {
			labels.push((Chip::TokenIn, count(Icon::Input, facts.tokens_in), theme.secondary));
		}
		if facts.tokens_out > 0 {
			labels.push((Chip::TokenOut, count(Icon::Output, facts.tokens_out), theme.info));
		}
		let total = facts
			.tokens_in
			.saturating_add(facts.tokens_out)
			.saturating_add(facts.cache_write);
		if total > 0 {
			labels.push((Chip::TokenTotal, count(Icon::Tokens, total), theme.secondary));
		}
		if let Some(rate) = facts.tokens_per_second.filter(|rate| *rate > 0.0) {
			let label = sf!("{} {rate:.1} tok/s", charset.icon(Icon::Throughput));
			labels.push((Chip::TokenRate, label, theme.info));
		}
		let mut cost =
			String::from(spend_label(facts.cost_nano_usd, facts.subscription, charset).as_str());
		if facts.premium_requests > 0 {
			if !cost.is_empty() {
				cost.push(' ');
			}
			cost.push_str(charset.icon(Icon::Star));
			cost.push(' ');
			let _ = write_compact_count(&mut cost, facts.premium_requests);
		}
		if !cost.is_empty() {
			labels.push((Chip::Cost, Str::new(cost), theme.secondary));
		}
		labels
	}

	/// Cells a group needs: labels, separators with their pads, the interior
	/// pads, and both caps (pi `groupWidth`); zero for an empty group.
	fn group_width(labels: &[Label], chrome: (&str, &str, &str)) -> u16 {
		if labels.is_empty() {
			return 0;
		}
		let (left_cap, separator, cap) = chrome;
		let text = labels
			.iter()
			.fold(0_u16, |sum, (_, label, _)| sum.saturating_add(cell_width(label)));
		let separators = u16::try_from(labels.len() - 1)
			.unwrap_or(u16::MAX)
			.saturating_mul(cell_width(separator).saturating_add(2));
		text
			.saturating_add(separators)
			.saturating_add(2)
			.saturating_add(cell_width(left_cap))
			.saturating_add(cell_width(cap))
	}

	/// Narrowest gauge that still carries both labels (pi
	/// `embeddedContextGaugeMinWidth`); one cell without a window.
	fn gauge_min_width(&self) -> u16 {
		let Some(window) = self.facts.context_window.filter(|window| *window > 0) else {
			return 1;
		};
		let percent = self.facts.tokens as f64 / window as f64 * 100.0;
		let mut percent_label = String::new();
		if percent > 0.0 && percent < 1.0 {
			let _ = write!(percent_label, "{percent:.1}%");
		} else {
			let _ = write!(percent_label, "{percent:.0}%");
		}
		let mut window_label = String::new();
		let _ = write_compact_count(&mut window_label, window);
		cell_width(&percent_label)
			.saturating_add(cell_width(&window_label))
			.saturating_add(4)
	}

	/// Fits both groups into `width` around the gauge (pi `#buildStatusLine`):
	/// clamp the session title, pop right chips, shrink the path, then drop
	/// non-path left chips from the right.
	fn fitted(&self, pc: &PaintCtx<'_>, width: u16) -> Layout {
		let charset = pc.ctx.charset;
		let left_chrome = charset.status_band();
		let right_chrome = charset.status_band_end();
		let gauge_min = self.gauge_min_width();
		let mut path_max = PATH_MAX;
		let mut left = self.left_labels(pc, path_max);
		let mut right = self.right_labels(pc, u16::MAX);
		// pi `minimumGapWidth`: a lone surviving chip that cannot share the
		// row with both gauge labels keeps the one-cell gauge instead of
		// losing the whole band.
		let overflow = |left: &[Label], right: &[Label]| {
			let groups = Self::group_width(left, left_chrome)
				.saturating_add(Self::group_width(right, right_chrome));
			let gap = if left.len() + right.len() == 1 && groups.saturating_add(gauge_min) > width {
				1
			} else {
				gauge_min
			};
			groups.saturating_add(gap).saturating_sub(width)
		};
		let excess = overflow(&left, &right);
		if excess > 0
			&& let Some(index) = right.iter().position(|(chip, ..)| *chip == Chip::Session)
		{
			let current = cell_width(&right[index].1);
			let shrink = current.saturating_sub(SESSION_NAME_MIN).min(excess);
			if shrink > 0 {
				right = self.right_labels(pc, current - shrink);
			}
		}
		while overflow(&left, &right) > 0 && !right.is_empty() {
			right.pop();
		}
		loop {
			let excess = overflow(&left, &right);
			if excess == 0 || left.is_empty() {
				return Layout { left, right };
			}
			let path_width = left
				.iter()
				.find(|(chip, ..)| *chip == Chip::Path)
				.map(|(_, label, _)| cell_width(label));
			if let Some(current) = path_width
				&& path_max > PATH_MIN
				&& current > PATH_MIN
			{
				path_max = path_max.min(current).saturating_sub(excess).max(PATH_MIN);
				left = self.left_labels(pc, path_max);
				continue;
			}
			let drop = left
				.iter()
				.rposition(|(chip, ..)| *chip != Chip::Path)
				.unwrap_or(left.len() - 1);
			left.remove(drop);
		}
	}

	/// Paints one powerline group at `rect` and returns the labels' first
	/// columns, so callers can overpaint spans inside a chip.
	fn paint_group(
		pc: &mut PaintCtx<'_>,
		labels: &[Label],
		rect: Rect,
		end: bool,
		mut on_label: impl FnMut(Chip, u16),
	) {
		let theme = pc.ctx.theme;
		let (left_cap, separator, _) = if end {
			pc.ctx.charset.status_band_end()
		} else {
			pc.ctx.charset.status_band()
		};
		let mut group = Status::new()
			.with(Prop::Bg, theme.panel)
			.with(Prop::Fg, theme.fg);
		if end {
			group = group.with_str(Prop::Align, "right");
		}
		let mut column = rect
			.x
			.saturating_add(cell_width(left_cap))
			.saturating_add(1);
		let step = cell_width(separator).saturating_add(2);
		for (index, (chip, label, color)) in labels.iter().enumerate() {
			if index > 0 {
				column = column.saturating_add(step);
			}
			on_label(*chip, column);
			column = column.saturating_add(cell_width(label));
			group = group.segment(Segment::new().label(label.clone()).with(Prop::Fg, *color));
		}
		let mut group = Cached::new(Box::new(group));
		group.place(pc.ctx, rect);
		group.paint(pc);
	}
}

impl Component for StatusBand {
	fn props(&self) -> &Props {
		&self.props
	}

	fn props_mut(&mut self) -> &mut Props {
		&mut self.props
	}

	fn slot(&self) -> Slot {
		self.slot
	}

	fn measure(&mut self, _ctx: &UiContext) -> (u16, u16) {
		(16, 120)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		1
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.y >= pc.clip || rect.width == 0 {
			return;
		}
		let theme = pc.ctx.theme;
		let charset = pc.ctx.charset;
		// Brand color eases between idle and working (pi `brandFgAnsi`).
		let target = if self.facts.working.is_some() {
			theme.accent
		} else {
			theme.muted
		};
		let fade = self.fade.get_or_insert_with(|| Tween::settled(target));
		fade.retarget(pc.now, target, BRAND_FADE, Easing::EaseInOut);
		if !fade.is_settled(pc.now) {
			pc.wake(self.slot, pc.now.saturating_add(BRAND_FADE_FRAME));
		}
		if let Some(started) = self.facts.working {
			let spinner = charset.spinner().next_change(pc.now);
			let elapsed = pc.now.saturating_sub(started);
			let next_second = started.saturating_add(Duration::from_secs(elapsed.as_secs() + 1));
			pc.wake(self.slot, spinner.min(next_second));
		}

		let Layout { left, right } = self.fitted(pc, rect.width);
		let left_width = Self::group_width(&left, charset.status_band()).min(rect.width);
		let right_width = Self::group_width(&right, charset.status_band_end())
			.min(rect.width.saturating_sub(left_width));
		let advisor = self.advisor_span(charset);
		let mut advisor_column = None;
		if left_width > 0 {
			Self::paint_group(
				pc,
				&left,
				Rect::new(rect.x, rect.y, left_width, 1),
				false,
				|chip, x| {
					if chip == Chip::Model {
						advisor_column = advisor.map(|(offset, icon)| (x.saturating_add(offset), icon));
					}
				},
			);
		}
		if let Some(((column, icon), badge)) = advisor_column.zip(self.facts.advisor)
			&& column.saturating_add(cell_width(icon)) <= rect.x.saturating_add(left_width)
		{
			// pi paints the badge as its own span inside the model chip, so
			// the roster health reads apart from the model color.
			let color = match badge.health {
				AdvisorHealth::Error => theme.err,
				AdvisorHealth::QuotaExhausted => theme.warn,
				AdvisorHealth::Running => theme.ok,
				AdvisorHealth::Paused => theme.muted,
			};
			pc.frame
				.put(column, rect.y, icon, Style::new().fg(color).bg(theme.panel));
		}
		if right_width > 0 {
			let x = rect.x.saturating_add(rect.width - right_width);
			Self::paint_group(pc, &right, Rect::new(x, rect.y, right_width, 1), true, |_, _| {});
		}

		let gap = rect
			.width
			.saturating_sub(left_width)
			.saturating_sub(right_width);
		if gap == 0 {
			return;
		}
		let rule = charset.rule().encode_utf8(&mut [0; 4]).to_owned();
		let gauge = ContextGauge::plan(
			gap,
			self.facts.tokens,
			self.facts.context_window,
			Some(CompactionBoundaries {
				threshold_percent:   f64::from(self.facts.compact_percent),
				speculation_percent: None,
			}),
		);
		let used = Style::new().fg(compaction_threshold_color(&theme));
		let unused = Style::new().fg(theme.border);
		let boundary = Style::new().fg(compaction_boundary_color(&theme));
		// Background speculation animates the compaction tick: pulsing
		// accent/muted while a summary is produced, solid accent once armed
		// (pi `contextPctSegment`).
		let threshold = match self.facts.speculation {
			Speculation::None => boundary,
			Speculation::Armed => Style::new().fg(theme.accent),
			Speculation::Running => {
				pc.wake(self.slot, speculation_flip(pc.now));
				Style::new().fg(if speculation_on(pc.now) {
					theme.accent
				} else {
					theme.muted
				})
			},
		};
		let percent = if gauge.overflowed() {
			Style::new().fg(theme.err)
		} else {
			used
		};
		let tick = charset.icon(Icon::ContextCompaction);
		let mut column = rect.x.saturating_add(left_width);
		for index in 0..gauge.width() {
			column = match gauge.cell(index) {
				GaugeCell::Used => pc.frame.put(column, rect.y, &rule, used),
				GaugeCell::Unused => pc.frame.put(column, rect.y, &rule, unused),
				GaugeCell::Threshold => pc.frame.put(column, rect.y, tick, threshold),
				GaugeCell::Speculation => pc.frame.put(column, rect.y, tick, boundary),
				GaugeCell::Percent(text) => pc.frame.put(column, rect.y, text, percent),
				GaugeCell::Window(text) => pc.frame.put(column, rect.y, text, boundary),
			};
		}
	}

	fn paints_background(&self) -> bool {
		false
	}
}

#[cfg(test)]
pub(crate) mod tests {
	use omp_tui::{Ui, frame_text};

	use super::*;

	fn rows(component: impl omp_tui::IntoComponent, width: u16) -> Vec<String> {
		let ui = Ui::from_root(component, width, UiContext::default());
		frame_text(ui.frame())
			.lines()
			.map(|line| line.trim_end().to_owned())
			.collect()
	}

	fn row(facts: StatusFacts, width: u16) -> String {
		rows(StatusBand::new(facts), width).remove(0)
	}

	#[test]
	fn display_path_strips_roots_and_labels_scratch_dirs() {
		let label = |path: &str| display_path(path, Some("/home/me"), Some("/var/folders/x/T"));
		assert_eq!(label("/home/me/src"), PathLabel {
			text:    Str::new_static("~/src"),
			scratch: false,
		});
		assert_eq!(label("/home/me").text.as_str(), "~");
		assert_eq!(label("/home/mesa").text.as_str(), "/home/mesa");
		assert_eq!(label("/work/omp"), PathLabel { text: Str::new_static("omp"), scratch: false });
		assert_eq!(label("/home/me/Projects/app/sub").text.as_str(), "app/sub");
		assert_eq!(label("/tmp/pi-face-filler-boot-120x40-parent-C61sEN/pi-capture"), PathLabel {
			text:    Str::new_static("pi-face-filler-boot-120x40-parent-C61sEN/pi-capture"),
			scratch: true,
		});
		assert_eq!(label("/var/folders/x/T/scratch").text.as_str(), "scratch");
		assert_eq!(label("/home/me/tmp/scratch"), PathLabel {
			text:    Str::new_static("scratch"),
			scratch: true,
		});
		assert_eq!(label("/tmp"), PathLabel { text: Str::new_static("/tmp"), scratch: true });
	}

	#[test]
	fn clamp_path_keeps_a_left_ellipsis_within_the_budget() {
		let long = format!("/very/{}/tail", "long".repeat(20));
		let shown = clamp_path(&long, PATH_MAX);
		assert!(shown.starts_with('…'));
		assert_eq!(cell_width(&shown), PATH_MAX);
		assert!(shown.ends_with("/tail"));
		assert_eq!(clamp_path("short", PATH_MAX).as_str(), "short");
	}

	#[test]
	fn clamp_end_keeps_a_trailing_ellipsis_within_the_budget() {
		assert_eq!(clamp_end("refactor the auth layer", 8).as_str(), "refacto…");
		assert_eq!(cell_width(&clamp_end("refactor the auth layer", 8)), 8);
		assert_eq!(clamp_end("short", 8).as_str(), "short");
	}

	pub(crate) fn facts() -> StatusFacts {
		StatusFacts {
			model: Str::new_static("Sonnet 4.5"),
			cwd: Str::new_static("~/proj"),
			branch: Some(Str::new_static("main")),
			tokens: 20_000,
			context_window: Some(200_000),
			compact_percent: 80,
			..StatusFacts::default()
		}
	}

	/// Facts with every right-group chip populated.
	fn spending() -> StatusFacts {
		StatusFacts {
			session_name: Some(Str::new_static("refactor the auth layer")),
			tokens_in: 12_000,
			tokens_out: 3_400,
			cache_read: 90_000,
			cache_write: 600,
			tokens_per_second: Some(42.4),
			cost_nano_usd: 120_000_000,
			premium_requests: 2,
			..facts()
		}
	}

	#[test]
	fn status_band_embeds_the_context_gauge_after_the_group() {
		let row = row(facts(), 80);
		assert!(row.starts_with(" π  > ⬢ Sonnet 4.5 > 📁 ~/proj > ⑂ main ▶"), "{row}");
		assert!(row.contains("10%"), "{row}");
		assert!(row.ends_with("200K─"), "{row}");
		assert!(row.contains('┃'), "{row}");
		assert_eq!(cell_width(&row), 80, "the gauge runs to the edge");
	}

	#[test]
	fn status_band_shows_the_thinking_glyph_and_scratch_icon() {
		let row = row(
			StatusFacts {
				thinking: Some(Str::new_static("high")),
				scratch: true,
				branch: None,
				..facts()
			},
			80,
		);
		assert!(row.starts_with(" π  > ◒ Sonnet 4.5 > 🗑 ~/proj ▶"), "{row}");
	}

	#[test]
	fn model_chip_trails_fast_icon_and_thinking_level_when_not_compact() {
		let row = row(
			StatusFacts {
				thinking: Some(Str::new_static("high")),
				compact_thinking: false,
				fast: true,
				..facts()
			},
			100,
		);
		assert!(row.starts_with(" π  > ⬢ Sonnet 4.5 ⚡ · ◒ high > 📁 ~/proj"), "{row}");
		let compact = self::row(
			StatusFacts { thinking: Some(Str::new_static("high")), fast: true, ..facts() },
			100,
		);
		assert!(compact.starts_with(" π  > ◒ Sonnet 4.5 ⚡ > 📁 ~/proj"), "{compact}");
		let off = self::row(
			StatusFacts { thinking: Some(Str::new_static("off")), compact_thinking: false, ..facts() },
			100,
		);
		assert!(off.contains("⬢ Sonnet 4.5 · ⦸ off >"), "{off}");
	}

	#[test]
	fn advisor_badge_sits_between_the_name_and_the_tail_in_its_own_color() {
		let theme = UiContext::default().theme;
		let paint = |badge: AdvisorBadge| {
			let ui = Ui::from_root(
				StatusBand::new(StatusFacts {
					advisor: Some(badge),
					fast: true,
					thinking: Some(Str::new_static("high")),
					compact_thinking: false,
					..facts()
				}),
				100,
				UiContext::default(),
			);
			let row = frame_text(ui.frame()).lines().next().unwrap().to_owned();
			let column = cell_width(" π  > ⬢ Sonnet 4.5 ⚡ ");
			(row, ui.frame().cell(column, 0).style().foreground_color())
		};
		let (row, color) = paint(AdvisorBadge { health: AdvisorHealth::Running, yielded: false });
		assert!(row.starts_with(" π  > ⬢ Sonnet 4.5 ⚡ 👁 · ◒ high >"), "{row}");
		assert_eq!(color, theme.ok);
		let (row, color) = paint(AdvisorBadge { health: AdvisorHealth::Error, yielded: true });
		assert!(
			row.starts_with(" π  > ⬢ Sonnet 4.5 ⚡ 🙈 · ◒ high >"),
			"closed eye once yielded: {row}"
		);
		assert_eq!(color, theme.err);
		let (_, color) =
			paint(AdvisorBadge { health: AdvisorHealth::QuotaExhausted, yielded: false });
		assert_eq!(color, theme.warn);
		let (_, color) = paint(AdvisorBadge { health: AdvisorHealth::Paused, yielded: false });
		assert_eq!(color, theme.muted);
	}

	#[test]
	fn git_chip_marks_a_dirty_tree_in_the_warning_color() {
		let ui = Ui::from_root(
			StatusBand::new(StatusFacts { dirty: true, ..facts() }),
			80,
			UiContext::default(),
		);
		let row = frame_text(ui.frame()).lines().next().unwrap().to_owned();
		assert!(row.contains("> ⑂ main * ▶"), "{row}");
		let column = cell_width(" π  > ⬢ Sonnet 4.5 > 📁 ~/proj > ");
		assert_eq!(
			ui.frame().cell(column, 0).style().foreground_color(),
			UiContext::default().theme.warn
		);
	}

	#[test]
	fn right_group_docks_session_tokens_rate_and_cost_against_the_edge() {
		let row = row(spending(), 170);
		assert!(row.starts_with(" π  > ⬢ Sonnet 4.5 > 📁 ~/proj > ⑂ main ▶─"), "{row}");
		assert!(
			row.ends_with(
				"◀ refactor the auth layer < ⤵ 12K < ⤴ 3.4K < 🪙 16K < ⚡ 42.4 tok/s < $0.12 ★ 2"
			),
			"{row}"
		);
		assert!(!row.contains("90K"), "cache reads stay out of the total: {row}");
		assert!(row.contains("200K─◀"), "the gauge bridges the groups up to the right cap: {row}");
		let subscribed = self::row(StatusFacts { subscription: true, ..spending() }, 170);
		assert!(subscribed.ends_with("< S0.12 ★ 2"), "{subscribed}");
		let free = self::row(
			StatusFacts { cost_nano_usd: 0, premium_requests: 0, subscription: true, ..spending() },
			170,
		);
		assert!(free.ends_with("tok/s < (sub)"), "a zero-cost subscription keeps its marker: {free}");
	}

	#[test]
	fn overflow_clamps_the_title_then_pops_right_chips_before_the_path_shrinks() {
		let long_path =
			StatusFacts { cwd: Str::new(format!("~/{}tail", "segment/".repeat(4))), ..spending() };
		// Left group 73 cells, right group 80, gauge floor 11: 164 fits whole.
		let full = row(long_path.clone(), 170);
		assert!(full.contains("refactor the auth layer"), "{full}");
		assert!(full.contains("📁 ~/segment/"), "{full}");

		// Title clamps first (to its 8-cell floor) while every other chip stays.
		let clamped = row(long_path.clone(), 149);
		assert!(clamped.contains("◀ refacto… <"), "{clamped}");
		assert!(clamped.contains("$0.12 ★ 2"), "{clamped}");
		assert!(clamped.contains("📁 ~/segment/"), "path untouched: {clamped}");

		// Then right chips pop right to left: cost first, then rate, total…
		let popped = row(long_path.clone(), 140);
		assert!(!popped.contains("$0.12"), "cost pops first: {popped}");
		assert!(popped.contains("tok/s"), "{popped}");
		assert!(popped.contains("◀ refacto… <"), "{popped}");
		assert!(popped.contains("📁 ~/segment/"), "path still untouched: {popped}");
		let popped = row(long_path.clone(), 116);
		assert!(!popped.contains("tok/s"), "{popped}");
		assert!(!popped.contains("🪙"), "{popped}");
		assert!(popped.contains("⤵ 12K < ⤴ 3.4K"), "{popped}");
		assert!(popped.contains("📁 ~/segment/"), "path still untouched: {popped}");

		// Only once the right group is gone does the path shrink.
		let squeezed = row(long_path, 60);
		assert!(!squeezed.contains("refacto"), "{squeezed}");
		assert!(!squeezed.contains('◀'), "{squeezed}");
		assert!(squeezed.contains("📁 …"), "{squeezed}");
		assert!(squeezed.contains("⑂ main"), "{squeezed}");
	}

	#[test]
	fn status_band_shrinks_the_path_then_drops_chips_from_the_right() {
		let long =
			StatusFacts { cwd: Str::new(format!("~/{}/tail", "segment/".repeat(8))), ..facts() };
		let row = self::row(long.clone(), 70);
		assert!(row.contains("📁 …"), "path shrinks first: {row}");
		assert!(row.contains("⑂ main"), "git survives while the path can shrink: {row}");
		assert!(row.ends_with("200K─"), "{row}");

		let row = self::row(long, 36);
		assert!(!row.contains("⑂ main"), "git drops before the path: {row}");
		assert!(!row.contains("Sonnet"), "model drops before the path: {row}");
		assert!(row.contains("📁 …"), "the working directory survives: {row}");
	}

	#[test]
	fn status_band_swaps_the_brand_for_spinner_and_timer_while_working() {
		let mut ui = Ui::from_root(
			StatusBand::new(StatusFacts { working: Some(Duration::ZERO), ..facts() }),
			80,
			UiContext::default(),
		);
		let row = frame_text(ui.frame()).lines().next().unwrap().to_owned();
		assert!(row.starts_with(" ⠋ 0s  > ⬢ Sonnet 4.5"), "{row}");
		assert!(ui.next_wake().is_some(), "spinner schedules a wake");
		ui.tick(Duration::from_millis(3_300));
		let row = frame_text(ui.frame()).lines().next().unwrap().to_owned();
		assert!(row.starts_with(" ⠙ 3s  >"), "{row}");
		ui.tick(Duration::from_secs(61));
		let row = frame_text(ui.frame()).lines().next().unwrap().to_owned();
		assert!(row.contains(" 1m  >"), "{row}");
	}

	#[test]
	fn speculation_pulses_the_compaction_tick_then_holds_accent_once_armed() {
		let theme = UiContext::default().theme;
		let tick_color = |ui: &Ui| {
			let row = frame_text(ui.frame()).lines().next().unwrap().to_owned();
			let column = row
				.chars()
				.take_while(|ch| *ch != '┃')
				.map(|ch| cell_width(ch.encode_utf8(&mut [0; 4])))
				.sum::<u16>();
			ui.frame().cell(column, 0).style().foreground_color()
		};
		let mut ui = Ui::from_root(
			StatusBand::new(StatusFacts { speculation: Speculation::Running, ..facts() }),
			80,
			UiContext::default(),
		);
		assert_eq!(tick_color(&ui), theme.accent, "pi starts the blink on");
		assert_eq!(ui.next_wake(), Some(SPECULATION_BLINK), "wakes at the flip");
		ui.tick(SPECULATION_BLINK);
		assert_eq!(tick_color(&ui), theme.muted);
		assert_eq!(ui.next_wake(), Some(SPECULATION_BLINK * 2));
		ui.tick(SPECULATION_BLINK * 2);
		assert_eq!(tick_color(&ui), theme.accent);

		let mut ui = Ui::from_root(
			StatusBand::new(StatusFacts { speculation: Speculation::Armed, ..facts() }),
			80,
			UiContext::default(),
		);
		assert_eq!(tick_color(&ui), theme.accent);
		assert_eq!(ui.next_wake(), None, "an armed tick is static");
		ui.tick(SPECULATION_BLINK);
		assert_eq!(tick_color(&ui), theme.accent);

		let idle = Ui::from_root(StatusBand::new(facts()), 80, UiContext::default());
		assert_eq!(idle.next_wake(), None);
		assert_eq!(tick_color(&idle), compaction_boundary_color(&theme));
	}
}
