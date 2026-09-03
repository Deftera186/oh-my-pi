//! Boot chrome: the welcome banner, the composer status band, and the
//! composer shell. Every glyph and color comes from the ambient
//! [`UiContext`]; the shapes follow pi's `welcome.ts` and the status-band
//! composer.

use core::fmt::Write as _;
use std::time::Duration;

use omp_core::{Str, sf};
use omp_tui::{
	Border, Cached, Charset, Color, Component, Icon, PaintCtx, Prop, Props, Rect, Slot, Style,
	UiContext,
	anim::{Easing, Tween},
	cell_width,
	components::{
		Col, CompactionBoundaries, ComposerStyle, ContextGauge, EditorPane, GaugeCell, Segment,
		Spacer, Status, compaction_boundary_color, compaction_threshold_color,
		write_compact_count,
	},
	next_slot,
};
use smallvec::SmallVec;

/// Element id of the composer editor inside the chrome tree.
pub const COMPOSER_ID: &str = "composer";
/// Element id of the status band inside the chrome tree.
pub const STATUS_ID: &str = "status-band";
/// Composer placeholder shared with the gallery composer previews.
pub const PLACEHOLDER: &str = "Ask anything, edit files, run tools";

/// Widest welcome box, in cells (pi `welcome.ts` `maxWidth`).
const BOX_MAX_WIDTH: u16 = 100;
const PREFERRED_LEFT: u16 = 26;
const MIN_LEFT: u16 = 12;
const MIN_RIGHT: u16 = 20;
/// Fixed slot counts so the box height never depends on live data.
const SESSION_SLOTS: usize = 4;
const LSP_SLOTS: usize = 4;
/// Block-grid brand mark shared with pi's welcome and setup surfaces.
const LOGO: [&str; 5] =
	["████████████", "   ██  ██   ", "   ██  ██   ", "   ▒▒  ██   ", "       ██   "];
/// Longest path label in the status band (pi `clampPathLength` default).
const PATH_MAX: u16 = 40;

/// Startup tips, one shown per session.
const TIPS: [&str; 6] = [
	"Try task isolation to create CoW worktrees",
	"Press shift+tab to cycle through reasoning effort levels",
	"`/force read` pins the next turn to one specific tool when the model keeps reaching for the \
	 wrong one",
	"Tired of typing \"keep going\"? Just send a '.'",
	"Ctrl+T toggles the assistant's thinking in the transcript",
	"Type / to browse commands and # to browse prompt actions",
];

/// Model facts the host learns once at launch and never journals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelBadge {
	/// Canonical `provider/model` identifier the session was launched with.
	pub identifier:     Str,
	/// Human-readable model name (catalog display name).
	pub name:           Str,
	/// Provider identifier.
	pub provider:       Str,
	/// Total context window in tokens when the catalog knows it.
	pub context_window: Option<u64>,
	/// Whether the model can reason (the band then shows the thinking level).
	pub reasoning:      bool,
}

impl ModelBadge {
	/// Derives a badge from a `provider/model` identifier when no catalog
	/// record is available.
	#[must_use]
	pub fn from_identifier(identifier: &str) -> Self {
		let (provider, name) = identifier.split_once('/').unwrap_or(("", identifier));
		Self {
			identifier:     Str::new(identifier),
			name:           Str::new(name),
			provider:       Str::new(provider),
			context_window: None,
			reasoning:      false,
		}
	}

	/// Model label for the status band: pi drops the `Claude ` prefix
	/// (`status-line/segments.ts` `modelSegment`).
	#[must_use]
	pub fn short_name(&self) -> Str {
		match self.name.as_str().strip_prefix("Claude ") {
			Some(short) => Str::new(short),
			None => self.name.clone(),
		}
	}
}

/// Picks the session's startup tip deterministically from its working
/// directory so a resumed session shows the same line.
#[must_use]
pub fn tip_for(cwd: &str) -> Str {
	let hash = cwd
		.bytes()
		.fold(0_usize, |acc, byte| acc.wrapping_mul(31).wrapping_add(usize::from(byte)));
	Str::new_static(TIPS[hash % TIPS.len()])
}

/// Welcome banner: two-column box with the brand mark, model, tips, LSP and
/// recent-session slots, followed by the startup tip.
pub struct Welcome {
	props:    Props,
	slot:     Slot,
	version:  Str,
	model:    Str,
	provider: Str,
	tip:      Str,
}

struct WelcomeGeometry {
	box_width:  u16,
	left_col:   u16,
	right_col:  u16,
	show_right: bool,
}

impl Welcome {
	/// Creates the banner for one launch.
	#[must_use]
	pub fn new(version: Str, badge: &ModelBadge, tip: Str) -> Self {
		Self {
			props: Props::new(),
			slot: next_slot(),
			version,
			model: badge.name.clone(),
			provider: badge.provider.clone(),
			tip,
		}
	}

	/// Mirrors pi's responsive breakpoint arithmetic (`welcome.ts`
	/// `#renderLines`).
	fn geometry(width: u16) -> Option<WelcomeGeometry> {
		let box_width = BOX_MAX_WIDTH.min(width.saturating_sub(2));
		if box_width < 4 {
			return None;
		}
		let dual_content = box_width - 3;
		let left_min_content = MIN_LEFT.max(cell_width("Welcome back!"));
		let scaled = (f64::from(dual_content) * 0.35).floor() as u16;
		let desired_left = PREFERRED_LEFT
			.min(MIN_LEFT.max(scaled))
			.max(left_min_content);
		let dual_left = if dual_content > MIN_RIGHT {
			desired_left.min(dual_content - MIN_RIGHT)
		} else {
			dual_content.saturating_sub(1).max(1)
		};
		let dual_right = dual_content.saturating_sub(dual_left).max(1);
		let show_right = dual_left >= left_min_content && dual_right >= MIN_RIGHT;
		let left_col = if show_right { dual_left } else { box_width - 2 };
		let right_col = if show_right { dual_right } else { 0 };
		Some(WelcomeGeometry { box_width, left_col, right_col, show_right })
	}

	fn content_rows(show_right: bool) -> u16 {
		let left = 3 + LOGO.len() + 1 + 2;
		let right = 1 + 4 + 1 + 1 + LSP_SLOTS + 1 + 1 + SESSION_SLOTS + 1;
		let rows = if show_right { left.max(right) } else { left };
		u16::try_from(rows).unwrap_or(u16::MAX)
	}

	fn tip_lines(&self, box_width: u16) -> Vec<Str> {
		let budget = box_width.saturating_sub(1 + cell_width("Tip: "));
		if budget < 8 {
			return Vec::new();
		}
		wrap_words(self.tip.as_str(), budget)
	}
}

/// Greedy word wrap on cell width; words longer than the budget are kept
/// whole on their own line.
fn wrap_words(text: &str, budget: u16) -> Vec<Str> {
	let mut lines = Vec::new();
	let mut current = String::new();
	for word in text.split_whitespace() {
		let candidate_width = cell_width(&current)
			.saturating_add(u16::from(!current.is_empty()))
			.saturating_add(cell_width(word));
		if !current.is_empty() && candidate_width > budget {
			lines.push(Str::new(std::mem::take(&mut current)));
		}
		if !current.is_empty() {
			current.push(' ');
		}
		current.push_str(word);
	}
	if !current.is_empty() {
		lines.push(Str::new(current));
	}
	lines
}

/// Paints `text` centered inside `width` cells starting at `x`.
fn put_centered(pc: &mut PaintCtx<'_>, x: u16, y: u16, width: u16, text: &str, style: Style) {
	let text_width = cell_width(text);
	if text_width >= width {
		let clipped = clip_to_width(text, width);
		pc.frame.put(x, y, clipped, style);
		return;
	}
	let left_pad = (width - text_width) / 2;
	pc.frame.put(x.saturating_add(left_pad), y, text, style);
}

fn clip_to_width(text: &str, width: u16) -> &str {
	let mut end = 0;
	let mut used = 0;
	for (index, grapheme) in text.char_indices() {
		let glyph = cell_width(&text[index..index + grapheme.len_utf8()]);
		if used + glyph > width {
			break;
		}
		used += glyph;
		end = index + grapheme.len_utf8();
	}
	&text[..end]
}

/// Linear blend between two theme colors; non-RGB colors fall back to
/// `from`.
fn blend(from: Color, to: Color, t: f32) -> Color {
	match (from, to) {
		(Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) => {
			let channel = |a: u8, b: u8| {
				(f32::from(b) - f32::from(a))
					.mul_add(t, f32::from(a))
					.round() as u8
			};
			Color::Rgb(channel(r0, r1), channel(g0, g1), channel(b0, b1))
		},
		_ => from,
	}
}

impl Component for Welcome {
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
		(MIN_LEFT + 4, BOX_MAX_WIDTH + 2)
	}

	fn height(&mut self, _ctx: &UiContext, width: u16) -> u16 {
		let Some(geometry) = Self::geometry(width) else {
			return 0;
		};
		let tips = u16::try_from(self.tip_lines(geometry.box_width).len()).unwrap_or(u16::MAX);
		1_u16
			.saturating_add(1)
			.saturating_add(Self::content_rows(geometry.show_right))
			.saturating_add(1)
			.saturating_add(tips)
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		let Some(geometry) = Self::geometry(rect.width) else {
			return;
		};
		let theme = pc.ctx.theme;
		let (tl, tr, bl, br, horizontal, vertical) = pc.ctx.charset.border(Border::Round);
		let dim = Style::new().fg(theme.border);
		let muted = Style::new().fg(theme.muted);
		let accent = Style::new().fg(theme.accent).bold();
		let x = rect.x;
		let mut y = rect.y.saturating_add(1);
		let WelcomeGeometry { box_width, left_col, right_col, show_right } = geometry;

		// Top border with the embedded title after three rule cells.
		let title = format!(" omp v{} ", self.version);
		let mut column = pc.frame.put(x, y, tl.encode_utf8(&mut [0; 4]), dim);
		let title_space = box_width - 2;
		let prefix = repeat_char(horizontal, 3);
		let title_width = 3 + cell_width(&title);
		column = pc.frame.put(column, y, &prefix, dim);
		if title_width >= title_space {
			let clipped = clip_to_width(&title, title_space.saturating_sub(3));
			column = pc.frame.put(column, y, clipped, muted);
		} else {
			column = pc.frame.put(column, y, &title, muted);
			column = pc
				.frame
				.put(column, y, &repeat_char(horizontal, title_space - title_width), dim);
		}
		pc.frame.put(column, y, tr.encode_utf8(&mut [0; 4]), dim);
		y = y.saturating_add(1);

		// Content rows.
		let rows = Self::content_rows(show_right);
		let left_x = x.saturating_add(1);
		let right_x = left_x.saturating_add(left_col).saturating_add(1);
		let vertical_glyph = vertical.encode_utf8(&mut [0; 4]).to_owned();
		let logo_top = 3_u16;
		let logo_rows = u16::try_from(LOGO.len()).unwrap_or(u16::MAX);
		let model_row = logo_top + logo_rows + 1;
		let separator = format!(" {}", repeat_char(horizontal, right_col.saturating_sub(2)));
		let lsp_top = 7_u16;
		let sessions_top = lsp_top + 1 + u16::try_from(LSP_SLOTS).unwrap_or(u16::MAX) + 1;
		for row in 0..rows {
			if y >= pc.clip {
				return;
			}
			pc.frame.put(x, y, &vertical_glyph, dim);
			match row {
				1 => put_centered(pc, left_x, y, left_col, "Welcome back!", Style::new().bold()),
				row if row >= logo_top && row < logo_top + logo_rows => {
					let line = LOGO[usize::from(row - logo_top)];
					let width = cell_width(line);
					let start = left_x.saturating_add(left_col.saturating_sub(width) / 2);
					let mut cursor = start;
					for (index, glyph) in line.chars().enumerate() {
						let t = index as f32 / (line.chars().count().max(2) - 1) as f32;
						let style = if glyph == '▒' {
							muted
						} else {
							Style::new().fg(blend(theme.accent, theme.secondary, t))
						};
						cursor = pc
							.frame
							.put(cursor, y, glyph.encode_utf8(&mut [0; 4]), style);
					}
				},
				row if row == model_row => put_centered(pc, left_x, y, left_col, &self.model, muted),
				row if row == model_row + 1 => {
					put_centered(pc, left_x, y, left_col, &self.provider, dim);
				},
				_ => {},
			}
			if show_right {
				pc.frame
					.put(right_x.saturating_sub(1), y, &vertical_glyph, dim);
				let content_x = right_x.saturating_add(1);
				match row {
					0 => {
						pc.frame.put(content_x, y, "Tips", accent);
					},
					1..=4 => {
						let (key, text) = [
							("#", "for prompt actions"),
							("/", "for commands"),
							("!", "to run bash"),
							("$", "to run python"),
						][usize::from(row - 1)];
						let column = pc.frame.put(content_x, y, key, dim);
						let column = pc.frame.put(column, y, " ", muted);
						pc.frame.put(column, y, text, muted);
					},
					5 => {
						pc.frame.put(right_x, y, &separator, dim);
					},
					row if row == lsp_top - 1 => {
						pc.frame.put(content_x, y, "LSP Servers", accent);
					},
					row if row == lsp_top => {
						pc.frame.put(content_x, y, "No LSP servers", dim);
					},
					row if row == sessions_top - 2 => {
						pc.frame.put(right_x, y, &separator, dim);
					},
					row if row == sessions_top - 1 => {
						pc.frame.put(content_x, y, "Recent sessions", accent);
					},
					row if row == sessions_top => {
						pc.frame.put(content_x, y, "No recent sessions", dim);
					},
					_ => {},
				}
				pc.frame
					.put(right_x.saturating_add(right_col), y, &vertical_glyph, dim);
			} else {
				pc.frame
					.put(left_x.saturating_add(left_col), y, &vertical_glyph, dim);
			}
			y = y.saturating_add(1);
		}

		// Bottom border, with a tee where the column divider meets it.
		if y < pc.clip {
			let mut column = pc.frame.put(x, y, bl.encode_utf8(&mut [0; 4]), dim);
			column = pc
				.frame
				.put(column, y, &repeat_char(horizontal, left_col), dim);
			if show_right {
				let tee = if pc.ctx.charset == omp_tui::Charset::Ascii {
					"+"
				} else {
					"┴"
				};
				column = pc.frame.put(column, y, tee, dim);
				column = pc
					.frame
					.put(column, y, &repeat_char(horizontal, right_col), dim);
			}
			pc.frame.put(column, y, br.encode_utf8(&mut [0; 4]), dim);
			y = y.saturating_add(1);
		}

		// Startup tip.
		let label = Style::new().fg(theme.secondary).italic();
		let body = Style::new().fg(theme.muted).italic();
		let indent = cell_width("Tip: ");
		for (index, line) in self.tip_lines(box_width).into_iter().enumerate() {
			if y >= pc.clip {
				return;
			}
			let column = if index == 0 {
				pc.frame.put(x.saturating_add(1), y, "Tip: ", label)
			} else {
				x.saturating_add(1).saturating_add(indent)
			};
			pc.frame.put(column, y, &line, body);
			y = y.saturating_add(1);
		}
	}
}

fn repeat_char(glyph: char, count: u16) -> String {
	std::iter::repeat_n(glyph, usize::from(count)).collect()
}

/// Facts painted by the composer status band.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusFacts {
	/// Short model label.
	pub model:          Str,
	/// Reasoning level (`off`, `minimal` … `max`) when the model can reason;
	/// `None` for models without thinking. Its glyph replaces the model icon
	/// (pi `statusLine.compactThinkingLevel`).
	pub thinking:       Option<Str>,
	/// Project directory label: home-shortened and root-stripped, not yet
	/// clamped (the band clamps to the width it has).
	pub cwd:            Str,
	/// Whether the project lives under a scratch root (pi `scratchFolder`
	/// icon instead of the folder icon).
	pub scratch:        bool,
	/// Checked-out git branch, an observer-local fact the app supplies.
	pub branch:         Option<Str>,
	/// Tokens in the last inference request (context usage).
	pub tokens:         u64,
	/// Total context window when known.
	pub context_window: Option<u64>,
	/// Auto-compaction threshold as a whole percent of the window
	/// (`ai_compact_threshold`), the gauge's tick position.
	pub compact_percent: u8,
	/// Start of the in-flight turn on the presentation clock; `Some` swaps
	/// the brand glyph for the spinner and elapsed-time timer.
	pub working:        Option<Duration>,
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

/// Glyph of a reasoning level for the compact model icon (pi
/// `thinkingGlyph`): the first token of the themed level label.
fn thinking_glyph(charset: Charset, level: &str) -> &'static str {
	let icon = match level {
		"off" => Icon::Disabled,
		"auto" => Icon::AutoPending,
		"minimal" => Icon::Minimal,
		"low" => Icon::Low,
		"medium" => Icon::Medium,
		"high" => Icon::High,
		"xhigh" => Icon::Xhigh,
		"max" => Icon::Max,
		_ => Icon::Model,
	};
	charset
		.icon(icon)
		.split_whitespace()
		.next()
		.unwrap_or_default()
}

/// Brand-color fade across working-state edges (pi `BRAND_FADE_MS`).
const BRAND_FADE: Duration = Duration::from_millis(450);
/// Repaint cadence while the brand fade is in flight (pi `BRAND_FADE_FRAME_MS`).
const BRAND_FADE_FRAME: Duration = Duration::from_millis(40);
/// Narrowest path label pi keeps before dropping other segments.
const PATH_MIN: u16 = 4;

/// Identity of one band segment, for overflow policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Chip {
	Brand,
	Model,
	Path,
	Git,
}

/// One-row composer status in pi's band layout: the powerline group
/// (brand, model, path, git) bridged to the right edge by the embedded
/// context gauge.
///
/// Overflow follows pi's `#buildStatusLine`: the gauge keeps room for its
/// labels, the path shrinks first, then non-path segments drop from the
/// right so the working directory survives the longest.
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

	/// Segment labels at `path_max`, in band order.
	fn labels(&self, pc: &PaintCtx<'_>, path_max: u16) -> SmallVec<(Chip, Str, Color), 4> {
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
		let brand_color = self
			.fade
			.map_or(theme.muted, |fade| fade.sample(pc.now));
		labels.push((Chip::Brand, Str::new(brand), brand_color));
		let model_icon = self
			.facts
			.thinking
			.as_deref()
			.map_or_else(|| charset.icon(Icon::Model), |level| thinking_glyph(charset, level));
		labels.push((Chip::Model, sf!("{model_icon} {}", self.facts.model), theme.ok));
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
			labels.push((Chip::Git, sf!("{} {branch}", charset.icon(Icon::Branch)), theme.info));
		}
		labels
	}

	/// Cells the group needs: labels, separators with their pads, the
	/// interior pads, and both caps (pi `groupWidth`).
	fn group_width(labels: &[(Chip, Str, Color)], charset: Charset) -> u16 {
		let (left_cap, separator, cap) = charset.status_band();
		let text = labels
			.iter()
			.fold(0_u16, |sum, (_, label, _)| sum.saturating_add(cell_width(label)));
		let separators = u16::try_from(labels.len().saturating_sub(1))
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

	/// Fits the group into `width` alongside the gauge: shrink the path,
	/// then drop non-path chips from the right (pi `#buildStatusLine`).
	fn fitted(&self, pc: &PaintCtx<'_>, width: u16) -> SmallVec<(Chip, Str, Color), 4> {
		let charset = pc.ctx.charset;
		let gauge_min = self.gauge_min_width();
		let mut path_max = PATH_MAX;
		let mut labels = self.labels(pc, path_max);
		loop {
			let overflow = Self::group_width(&labels, charset)
				.saturating_add(gauge_min)
				.saturating_sub(width);
			if overflow == 0 || labels.is_empty() {
				return labels;
			}
			let path_width = labels
				.iter()
				.find(|(chip, ..)| *chip == Chip::Path)
				.map(|(_, label, _)| cell_width(label));
			if let Some(current) = path_width
				&& path_max > PATH_MIN
				&& current > PATH_MIN
			{
				path_max = path_max
					.min(current)
					.saturating_sub(overflow)
					.max(PATH_MIN);
				labels = self.labels(pc, path_max);
				continue;
			}
			let drop = labels
				.iter()
				.rposition(|(chip, ..)| *chip != Chip::Path)
				.unwrap_or(labels.len() - 1);
			labels.remove(drop);
		}
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

		let labels = self.fitted(pc, rect.width);
		let natural = Self::group_width(&labels, charset).min(rect.width);
		let mut group = Status::new()
			.with(Prop::Bg, theme.panel)
			.with(Prop::Fg, theme.fg);
		for (_, label, color) in labels {
			group = group.segment(Segment::new().label(label).with(Prop::Fg, color));
		}
		let mut group = Cached::new(Box::new(group));
		group.place(pc.ctx, Rect::new(rect.x, rect.y, natural, 1));
		group.paint(pc);

		let gap = rect.width.saturating_sub(natural);
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
		let percent = if gauge.overflowed() {
			Style::new().fg(theme.err)
		} else {
			used
		};
		let tick = charset.icon(Icon::ContextCompaction);
		let mut column = rect.x.saturating_add(natural);
		for index in 0..gauge.width() {
			column = match gauge.cell(index) {
				GaugeCell::Used => pc.frame.put(column, rect.y, &rule, used),
				GaugeCell::Unused => pc.frame.put(column, rect.y, &rule, unused),
				GaugeCell::Threshold | GaugeCell::Speculation => {
					pc.frame.put(column, rect.y, tick, boundary)
				},
				GaugeCell::Percent(text) => pc.frame.put(column, rect.y, text, percent),
				GaugeCell::Window(text) => pc.frame.put(column, rect.y, text, boundary),
			};
		}
	}

	fn paints_background(&self) -> bool {
		false
	}
}

/// Builds the composer chrome tree: a spacer row, then the borderless
/// editor with its status band above the prompt.
#[must_use]
pub fn composer_root(facts: StatusFacts) -> Col {
	let editor = EditorPane::new()
		.composer_style(ComposerStyle::Borderless)
		.with(Prop::Id, COMPOSER_ID)
		.with(Prop::Submit, true)
		.with(Prop::Placeholder, PLACEHOLDER)
		.status(StatusBand::new(facts));
	Col::new().child(Spacer::new()).child(editor)
}

#[cfg(test)]
mod tests {
	use omp_tui::{Ui, frame_text};

	use super::*;

	fn rows(component: impl omp_tui::IntoComponent, width: u16) -> Vec<String> {
		let ui = Ui::from_root(component, width, UiContext::default());
		frame_text(ui.frame())
			.lines()
			.map(|line| line.trim_end().to_owned())
			.collect()
	}

	#[test]
	fn welcome_matches_pi_geometry_at_120_columns() {
		let badge = ModelBadge {
			identifier:     Str::new_static("anthropic/claude-fable-5"),
			name:           Str::new_static("Claude Fable 5"),
			provider:       Str::new_static("anthropic"),
			context_window: Some(1_000_000),
			reasoning:      true,
		};
		let welcome = Welcome::new(Str::new_static("18.0.11"), &badge, tip_for("/tmp"));
		let rows = rows(welcome, 120);
		assert_eq!(rows[0], "");
		assert!(rows[1].starts_with("╭─── omp v18.0.11 ─"), "{}", rows[1]);
		assert_eq!(cell_width(&rows[1]), 100);
		assert_eq!(rows[3], "│      Welcome back!       │ # for prompt actions                                                  │");
		assert_eq!(rows[20], format!("╰{}┴{}╯", "─".repeat(26), "─".repeat(71)));
		assert!(rows[21].starts_with(" Tip: "), "{}", rows[21]);
		assert_eq!(rows.len(), 22);
	}

	#[test]
	fn welcome_drops_the_right_column_on_narrow_terminals() {
		let badge = ModelBadge::from_identifier("anthropic/claude-sonnet-4-5");
		let welcome = Welcome::new(Str::new_static("0.1.0"), &badge, Str::new_static("tip"));
		let rows = rows(welcome, 30);
		assert!(rows.iter().any(|row| row.contains("Welcome back!")));
		assert!(!rows.iter().any(|row| row.contains("Tips")));
	}

	#[test]
	fn display_path_strips_roots_and_labels_scratch_dirs() {
		let label = |path: &str| display_path(path, Some("/home/me"), Some("/var/folders/x/T"));
		assert_eq!(label("/home/me/src"), PathLabel { text: Str::new_static("~/src"), scratch: false });
		assert_eq!(label("/home/me").text.as_str(), "~");
		assert_eq!(label("/home/mesa").text.as_str(), "/home/mesa");
		assert_eq!(label("/work/omp"), PathLabel { text: Str::new_static("omp"), scratch: false });
		assert_eq!(label("/home/me/Projects/app/sub").text.as_str(), "app/sub");
		assert_eq!(
			label("/tmp/pi-face-filler-boot-120x40-parent-C61sEN/pi-capture"),
			PathLabel {
				text:    Str::new_static("pi-face-filler-boot-120x40-parent-C61sEN/pi-capture"),
				scratch: true,
			}
		);
		assert_eq!(label("/var/folders/x/T/scratch").text.as_str(), "scratch");
		assert_eq!(label("/home/me/tmp/scratch"), PathLabel { text: Str::new_static("scratch"), scratch: true });
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

	fn facts() -> StatusFacts {
		StatusFacts {
			model:          Str::new_static("Sonnet 4.5"),
			thinking:       None,
			cwd:            Str::new_static("~/proj"),
			scratch:        false,
			branch:         Some(Str::new_static("main")),
			tokens:         20_000,
			context_window: Some(200_000),
			compact_percent: 80,
			working:        None,
		}
	}

	#[test]
	fn status_band_embeds_the_context_gauge_after_the_group() {
		let rows = rows(StatusBand::new(facts()), 80);
		let row = &rows[0];
		assert!(row.starts_with(" π  > ⬢ Sonnet 4.5 > 📁 ~/proj > ⑂ main ▶"), "{row}");
		assert!(row.contains("10%"), "{row}");
		assert!(row.ends_with("200K─"), "{row}");
		assert!(row.contains('┃'), "{row}");
		assert_eq!(cell_width(row), 80, "the gauge runs to the edge");
	}

	#[test]
	fn status_band_shows_the_thinking_glyph_and_scratch_icon() {
		let rows = rows(
			StatusBand::new(StatusFacts {
				thinking: Some(Str::new_static("high")),
				scratch: true,
				branch: None,
				..facts()
			}),
			80,
		);
		assert!(rows[0].starts_with(" π  > ◒ Sonnet 4.5 > 🗑 ~/proj ▶"), "{}", rows[0]);
	}

	#[test]
	fn status_band_shrinks_the_path_then_drops_chips_from_the_right() {
		let long = StatusFacts {
			cwd: Str::new(format!("~/{}/tail", "segment/".repeat(8))),
			..facts()
		};
		let wide = rows(StatusBand::new(long.clone()), 70);
		let row = &wide[0];
		assert!(row.contains("📁 …"), "path shrinks first: {row}");
		assert!(row.contains("⑂ main"), "git survives while the path can shrink: {row}");
		assert!(row.ends_with("200K─"), "{row}");

		let narrow = rows(StatusBand::new(long), 36);
		let row = &narrow[0];
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
	fn composer_root_paints_status_then_prompt_gutter() {
		let root = composer_root(StatusFacts { tokens: 0, ..facts() });
		let mut ui = Ui::from_root(root, 80, UiContext::default());
		ui.focus_first();
		let rows = frame_text(ui.frame())
			.lines()
			.map(str::to_owned)
			.collect::<Vec<_>>();
		assert_eq!(rows[0].trim(), "");
		assert!(rows[1].starts_with(" π  >"), "{}", rows[1]);
		assert!(rows[2].starts_with("╰─ Ask anything"), "{}", rows[2]);
		assert_eq!(ui.frame().cursor(), Some((3, 2)), "caret sits after the prompt gutter");
	}
}
