use std::{borrow::Cow, slice, str};

use omp_core::{IntoStr, Str, sf};
use smol_bitmap::SmolBitmap;
use strum::{EnumString, IntoStaticStr};

use crate::{
	Icon,
	component::{Component, PaintCtx, Slot, next_slot},
	context::{Charset, UiContext},
	frame::{Rect, Style},
	markup::Border,
	props::{Prop, PropValue, Props},
	rich::cell_width,
};

const VISIBLE_STAGE_LIMIT: usize = 5;
const SPINE_TAIL_CELLS: usize = 6;
const MAX_ROMAN: usize = 3_999;
const PREFIX_CAPACITY: usize = 22;
const ROMAN_DIGITS: [(usize, &[u8]); 13] = [
	(1000, b"M"),
	(900, b"CM"),
	(500, b"D"),
	(400, b"CD"),
	(100, b"C"),
	(90, b"XC"),
	(50, b"L"),
	(40, b"XL"),
	(10, b"X"),
	(9, b"IX"),
	(5, b"V"),
	(4, b"IV"),
	(1, b"I"),
];

struct PhasePrefix {
	bytes: [u8; PREFIX_CAPACITY],
	len:   usize,
}

impl PhasePrefix {
	fn new(ordinal: usize) -> Self {
		let mut prefix = Self { bytes: [0; PREFIX_CAPACITY], len: 0 };
		if (1..=MAX_ROMAN).contains(&ordinal) {
			let mut remaining = ordinal;
			for &(value, digits) in &ROMAN_DIGITS {
				while remaining >= value {
					prefix.push(digits);
					remaining -= value;
				}
			}
		} else {
			let mut reversed = [0u8; 20];
			let mut value = ordinal;
			let mut digits = 0;
			if value == 0 {
				prefix.push(b"0");
			}
			while value > 0 {
				reversed[digits] = b'0' + (value % 10) as u8;
				value /= 10;
				digits += 1;
			}
			while digits > 0 {
				digits -= 1;
				prefix.push(&reversed[digits..=digits]);
			}
		}
		prefix.push(b". ");
		prefix
	}

	fn push(&mut self, bytes: &[u8]) {
		let end = self.len + bytes.len();
		self.bytes[self.len..end].copy_from_slice(bytes);
		self.len = end;
	}

	fn as_str(&self) -> &str {
		str::from_utf8(&self.bytes[..self.len]).expect("phase prefixes are ASCII")
	}
}

fn has_canonical_roman_prefix(label: &str) -> bool {
	let Some((token, rest)) = label.split_once('.') else {
		return false;
	};
	if rest.is_empty() || !rest.starts_with(char::is_whitespace) || token.is_empty() {
		return false;
	}
	let mut ordinal = 0usize;
	let mut previous = 0usize;
	for byte in token.bytes().rev() {
		let value = match byte {
			b'I' => 1,
			b'V' => 5,
			b'X' => 10,
			b'L' => 50,
			b'C' => 100,
			b'D' => 500,
			b'M' => 1000,
			_ => return false,
		};
		if value < previous {
			ordinal = ordinal.saturating_sub(value);
		} else {
			ordinal = ordinal.saturating_add(value);
			previous = value;
		}
	}
	(1..=MAX_ROMAN).contains(&ordinal)
		&& PhasePrefix::new(ordinal).as_str().strip_suffix(". ") == Some(token)
}

/// Collapses multiline HUD copy onto one row using this terminal's return
/// glyph.
///
/// Whitespace touching one or more line breaks is replaced by one padded
/// marker. Single-line input is borrowed without allocation.
pub fn collapse_hud_line(text: &str, charset: Charset) -> Cow<'_, str> {
	if !text.contains('\r') && !text.contains('\n') {
		return Cow::Borrowed(text);
	}
	let marker = charset.icon(Icon::Enter);
	let mut collapsed = String::with_capacity(text.len().saturating_add(marker.len()));
	let mut chars = text.chars().peekable();
	while let Some(ch) = chars.next() {
		if ch != '\r' && ch != '\n' {
			collapsed.push(ch);
			continue;
		}
		while collapsed
			.chars()
			.next_back()
			.is_some_and(char::is_whitespace)
		{
			collapsed.pop();
		}
		while chars.peek().is_some_and(|next| next.is_whitespace()) {
			chars.next();
		}
		collapsed.push(' ');
		collapsed.push_str(marker);
		collapsed.push(' ');
	}
	Cow::Owned(collapsed)
}

/// Lifecycle state of a [`TodoTask`], mirroring the coding agent's todo
/// tracker: open work, the one active item, and the three closed shapes.
#[derive(Clone, Copy, Debug, Default, EnumString, Eq, IntoStaticStr, PartialEq)]
pub enum TaskStatus {
	/// Not started; renders dim with an empty checkbox.
	#[default]
	#[strum(to_string = "pending", serialize = "open", serialize = "queued")]
	Pending,
	/// Currently being worked; renders accent.
	#[strum(to_string = "active", serialize = "in-progress", serialize = "in_progress")]
	Active,
	/// Finished; renders ok with a checked box and struck label.
	#[strum(to_string = "done", serialize = "completed", serialize = "settled")]
	Done,
	/// Abandoned, failed, or cancelled; renders err with a struck label.
	#[strum(
		to_string = "dropped",
		serialize = "abandoned",
		serialize = "failed",
		serialize = "cancelled"
	)]
	Dropped,
	/// Waiting on something external; renders warn with the blocker note.
	#[strum(to_string = "blocked")]
	Blocked,
}

impl TaskStatus {
	/// Parses a markup `status=` value, accepting the agent-side aliases.
	pub fn parse(name: &str) -> Option<Self> {
		name.parse().ok()
	}
}

/// One row of a [`Todo`] list, backing the `<task>` markup tag.
///
/// A task with children renders as a group header with an automatic
/// `closed/total` count over its descendant leaves; a leaf renders a status
/// checkbox and its label. `status=` sets [`TaskStatus`]; `desc=` carries
/// the blocker note shown by [`TaskStatus::Blocked`].
pub struct TodoTask {
	props:        Props,
	label:        Str,
	blocked_note: Str,
	counter:      Str,
	children:     Vec<Self>,
}

impl TodoTask {
	/// Creates a pending, empty task.
	pub fn new() -> Self {
		Self {
			props:        Props::new(),
			label:        Str::default(),
			blocked_note: sf!(" (blocked)"),
			counter:      Str::default(),
			children:     Vec::new(),
		}
	}

	/// Sets one task property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self.refresh_labels();
		self
	}

	/// Appends label text.
	pub fn label(mut self, label: impl IntoStr) -> Self {
		let suffix = label.into_str();
		if self.label.is_empty() {
			self.label = suffix;
		} else {
			self.label = sf!("{}{}", self.label, suffix);
		}
		self
	}

	/// Sets the lifecycle state.
	pub fn status(mut self, status: TaskStatus) -> Self {
		let name: &'static str = status.into();
		self.props.set(Prop::Status, name);
		self
	}

	/// Appends a child task, turning this task into a group header.
	pub fn task(mut self, task: Self) -> Self {
		self.children.push(task);
		self.refresh_labels();
		self
	}

	fn refresh_labels(&mut self) {
		self.blocked_note = self
			.props
			.str_of(Prop::Desc)
			.map_or_else(|| sf!(" (blocked)"), |reason| sf!(" (blocked: {reason})"));
		let (closed, total) = leaf_counts(&self.children);
		self.counter = if self.children.is_empty() {
			Str::default()
		} else {
			sf!(" {closed}/{total}")
		};
	}

	fn effective_label(&self) -> &str {
		if self.label.is_empty() {
			self.props.str_of(Prop::Label).map_or("", Str::as_str)
		} else {
			&self.label
		}
	}

	fn effective_status(&self) -> TaskStatus {
		self
			.props
			.str_of(Prop::Status)
			.and_then(|name| TaskStatus::parse(name))
			.unwrap_or_default()
	}
}

impl Default for TodoTask {
	fn default() -> Self {
		Self::new()
	}
}

/// A static todo list backing the `<todo>` markup tag.
///
/// Children are [`TodoTask`] records; nesting produces tree guides in the
/// family chosen by `guides=` (square by default). The header is followed by
/// a progress-colored outer spine and tail. When every root is a group, roots
/// are treated as stages: the active stage plus four successors are shown and
/// any remaining stages collapse into a summary row. The list is display-only
/// and has no focus or keys. `numbering="roman"` prefixes root phases with
/// canonical Roman numerals; canonical authored prefixes are preserved, and
/// ordinals above 3,999 fall back to decimal.
pub struct Todo {
	props:           Props,
	slot:            Slot,
	tasks:           Vec<TodoTask>,
	stage_summaries: [Str; 2],
}

impl Todo {
	/// Creates an empty todo list.
	pub fn new() -> Self {
		Self {
			props:           Props::new(),
			slot:            next_slot(),
			tasks:           Vec::new(),
			stage_summaries: [Str::default(), Str::default()],
		}
	}

	/// Sets one list property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Appends a root task.
	pub fn task(mut self, task: TodoTask) -> Self {
		self.tasks.push(task);
		let (_, end) = self.stage_window();
		let hidden = self.tasks.len().saturating_sub(end);
		let suffix = if hidden == 1 { "" } else { "s" };
		self.stage_summaries =
			[sf!("... {hidden} more stage{suffix}"), sf!("… {hidden} more stage{suffix}")];
		self
	}

	/// Leaf `(closed, total)` across the whole list.
	///
	/// Finished and dropped work are both closed; pending, active, and blocked
	/// work remain open.
	pub fn counts(&self) -> (usize, usize) {
		leaf_counts(&self.tasks)
	}

	fn family(&self) -> Border {
		self.props.guides().unwrap_or(Border::Square)
	}

	fn roman_numbering(&self) -> bool {
		self
			.props
			.str_of(Prop::Numbering)
			.is_some_and(|value| value.as_str() == "roman")
	}

	fn row_count(tasks: &[TodoTask]) -> usize {
		tasks.iter().fold(0usize, |rows, task| {
			rows
				.saturating_add(1)
				.saturating_add(Self::row_count(&task.children))
		})
	}

	fn is_stage_list(&self) -> bool {
		!self.tasks.is_empty() && self.tasks.iter().all(|task| !task.children.is_empty())
	}

	fn stage_window(&self) -> (usize, usize) {
		if !self.is_stage_list() {
			return (0, self.tasks.len());
		}
		let active = self
			.tasks
			.iter()
			.position(has_open_work)
			.unwrap_or_else(|| self.tasks.len().saturating_sub(1));
		(
			active,
			active
				.saturating_add(VISIBLE_STAGE_LIMIT)
				.min(self.tasks.len()),
		)
	}

	fn visible_row_count(&self) -> usize {
		let (start, end) = self.stage_window();
		if !self.is_stage_list() {
			return Self::row_count(&self.tasks[start..end]);
		}
		let active_rows = self
			.tasks
			.get(start)
			.map_or(0, |task| 1 + Self::row_count(&task.children));
		active_rows
			.saturating_add(end.saturating_sub(start).saturating_sub(1))
			.saturating_add(usize::from(end < self.tasks.len()))
	}

	fn max_width(tasks: &[TodoTask], depth: u16) -> u16 {
		let mut widest = 0u16;
		for task in tasks {
			// outer spine + nested gutters + checkbox/count slack
			let width = cell_width(task.effective_label())
				.saturating_add(depth.saturating_mul(2))
				.saturating_add(13);
			widest = widest
				.max(width)
				.max(Self::max_width(&task.children, depth + 1));
		}
		widest
	}
}

fn has_open_work(task: &TodoTask) -> bool {
	if task.children.is_empty() {
		matches!(task.effective_status(), TaskStatus::Pending | TaskStatus::Active)
	} else {
		task.children.iter().any(has_open_work)
	}
}

/// Leaf `(closed, total)` under `tasks`; groups contribute their descendants.
fn leaf_counts(tasks: &[TodoTask]) -> (usize, usize) {
	let (mut closed, mut total) = (0, 0);
	for task in tasks {
		if task.children.is_empty() {
			total += 1;
			closed +=
				usize::from(matches!(task.effective_status(), TaskStatus::Done | TaskStatus::Dropped));
		} else {
			let (child_closed, child_total) = leaf_counts(&task.children);
			closed += child_closed;
			total += child_total;
		}
	}
	(closed, total)
}

impl Default for Todo {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for Todo {
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
		let numbering_width = if self.roman_numbering() {
			cell_width(PhasePrefix::new(self.tasks.len().max(1)).as_str()).max(11)
		} else {
			0
		};
		(
			12,
			Self::max_width(&self.tasks, 0)
				.saturating_add(numbering_width)
				.max(20),
		)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		if self.tasks.is_empty() {
			0
		} else {
			u16::try_from(self.visible_row_count().saturating_add(2)).unwrap_or(u16::MAX)
		}
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if self.tasks.is_empty() || rect.y >= pc.clip {
			return;
		}
		let (branch, last, cont) = pc.ctx.charset.guides(self.family());
		let glyphs = Glyphs {
			branch,
			last,
			cont,
			checked: pc.ctx.charset.checkbox(true),
			unchecked: pc.ctx.charset.checkbox(false),
		};
		let mut y = rect.y;
		pc.frame
			.put(rect.x, y, "TODO", Style::new().fg(pc.ctx.theme.accent).bold());
		y = y.saturating_add(1);

		let content_rows = self.visible_row_count();
		let (closed, total) = self.counts();
		let path_cells = content_rows.saturating_add(SPINE_TAIL_CELLS);
		let mut filled = closed
			.saturating_mul(path_cells)
			.saturating_add(total / 2)
			.checked_div(total)
			.unwrap_or(0);
		if closed > 0 {
			filled = filled.max(1);
		}
		if closed < total {
			filled = filled.min(path_cells.saturating_sub(1));
		}
		let mut spine = Spine { filled, row: 0 };
		let mut trail: SmolBitmap = SmolBitmap::with_capacity(8);
		let (start, end) = self.stage_window();
		let roman_numbering = self.roman_numbering();
		if self.is_stage_list() {
			if let Some(active) = self.tasks.get(start) {
				paint_tasks(
					pc,
					rect,
					&glyphs,
					slice::from_ref(active),
					&mut trail,
					0,
					&mut spine,
					&mut y,
					true,
					roman_numbering.then_some(start + 1),
				);
			}
			for (index, stage) in self.tasks[start.saturating_add(1)..end].iter().enumerate() {
				paint_tasks(
					pc,
					rect,
					&glyphs,
					slice::from_ref(stage),
					&mut trail,
					0,
					&mut spine,
					&mut y,
					false,
					roman_numbering.then_some(start + index + 2),
				);
			}
			if end < self.tasks.len() && y < rect.y.saturating_add(rect.height).min(pc.clip) {
				let x = paint_spine(pc, rect.x, y, glyphs.branch, &mut spine);
				let summary =
					&self.stage_summaries[usize::from(!matches!(pc.ctx.charset, Charset::Ascii))];
				pc.frame
					.put(x, y, summary, Style::new().fg(pc.ctx.theme.muted));
				y = y.saturating_add(1);
			}
		} else {
			paint_tasks(
				pc,
				rect,
				&glyphs,
				&self.tasks[start..end],
				&mut trail,
				0,
				&mut spine,
				&mut y,
				true,
				roman_numbering.then_some(start + 1),
			);
		}
		paint_spine_tail(pc, rect, &glyphs, &spine, y);
	}
}

struct Spine {
	filled: usize,
	row:    usize,
}

struct Glyphs {
	branch:    &'static str,
	last:      &'static str,
	cont:      &'static str,
	checked:   &'static str,
	unchecked: &'static str,
}

fn paint_spine(pc: &mut PaintCtx<'_>, x: u16, y: u16, glyph: &str, spine: &mut Spine) -> u16 {
	let color = if spine.row < spine.filled {
		pc.ctx.theme.accent
	} else {
		pc.ctx.theme.muted
	};
	spine.row = spine.row.saturating_add(1);
	let x = pc.frame.put(x, y, glyph, Style::new().fg(color));
	pc.frame.put(x, y, " ", Style::new().fg(color))
}

fn paint_spine_tail(pc: &mut PaintCtx<'_>, rect: Rect, glyphs: &Glyphs, spine: &Spine, y: u16) {
	if y >= rect.y.saturating_add(rect.height).min(pc.clip) {
		return;
	}
	let mut parts = xutf::graphemes_str(glyphs.last);
	let hook = parts.next().unwrap_or(glyphs.last);
	let horizontal = parts.next().unwrap_or("-");
	let mut x = rect.x;
	for cell in 0..SPINE_TAIL_CELLS {
		let glyph = if cell == 0 { hook } else { horizontal };
		let color = if spine.row.saturating_add(cell) < spine.filled {
			pc.ctx.theme.accent
		} else {
			pc.ctx.theme.muted
		};
		x = pc.frame.put(x, y, glyph, Style::new().fg(color));
	}
}

fn paint_tasks(
	pc: &mut PaintCtx<'_>,
	rect: Rect,
	glyphs: &Glyphs,
	tasks: &[TodoTask],
	trail: &mut SmolBitmap,
	trail_depth: usize,
	spine: &mut Spine,
	y: &mut u16,
	descend: bool,
	root_ordinal: Option<usize>,
) {
	let bottom = rect.y.saturating_add(rect.height).min(pc.clip);
	let count = tasks.len();
	for (index, task) in tasks.iter().enumerate() {
		if *y >= bottom {
			return;
		}
		let is_last = index + 1 == count;
		let mut x = paint_spine(
			pc,
			rect.x,
			*y,
			if trail_depth == 0 {
				glyphs.branch
			} else {
				glyphs.cont
			},
			spine,
		);
		let guide = Style::new().fg(pc.ctx.theme.muted);
		// Ancestor gutters, then this row's nested connector. The outer
		// progress spine already owns the root connector.
		if trail_depth != 0 {
			for level in 1..trail_depth {
				let more = trail.get(level);
				x = pc
					.frame
					.put(x, *y, if more { glyphs.cont } else { "  " }, guide);
			}
			x = pc
				.frame
				.put(x, *y, if is_last { glyphs.last } else { glyphs.branch }, guide);
			x = pc.frame.put(x, *y, " ", guide);
		}
		let label = task.effective_label();
		let prefix = root_ordinal
			.map(|first| first.saturating_add(index))
			.filter(|_| !has_canonical_roman_prefix(label))
			.map(PhasePrefix::new);
		if task.children.is_empty() {
			let theme = &pc.ctx.theme;
			let status = task.effective_status();
			let (glyph, style) = match status {
				TaskStatus::Done => (glyphs.checked, Style::new().fg(theme.ok)),
				TaskStatus::Active => (glyphs.unchecked, Style::new().fg(theme.accent)),
				TaskStatus::Dropped => (glyphs.unchecked, Style::new().fg(theme.err)),
				TaskStatus::Blocked => (glyphs.unchecked, Style::new().fg(theme.warn)),
				TaskStatus::Pending => (glyphs.unchecked, Style::new().dim()),
			};
			x = pc.frame.put(x, *y, glyph, style);
			x = pc.frame.put(x, *y, " ", style);
			if let Some(prefix) = &prefix {
				x = pc
					.frame
					.put(x, *y, prefix.as_str(), Style::new().fg(pc.ctx.theme.muted).bold());
			}
			let label_style = match status {
				TaskStatus::Done | TaskStatus::Dropped => style.strikethrough(),
				_ => style,
			};
			x = pc.frame.put(x, *y, label, label_style);
			if status == TaskStatus::Blocked {
				pc.frame.put(x, *y, &task.blocked_note, Style::new().dim());
			}
		} else {
			// Group header: bold label plus an automatic closed/total count
			// over its descendant leaves.
			if let Some(prefix) = &prefix {
				x = pc
					.frame
					.put(x, *y, prefix.as_str(), Style::new().fg(pc.ctx.theme.muted).bold());
			}
			x = pc
				.frame
				.put(x, *y, label, Style::new().fg(pc.ctx.theme.fg).bold());
			pc.frame.put(x, *y, &task.counter, Style::new().dim());
		}
		*y = y.saturating_add(1);
		if descend && !task.children.is_empty() {
			trail.set(trail_depth, !is_last);
			paint_tasks(
				pc,
				rect,
				glyphs,
				&task.children,
				trail,
				trail_depth + 1,
				spine,
				y,
				true,
				None,
			);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		component::PaintCtx,
		frame::{Frame, Size},
		test_support::frame_row_text,
	};

	fn paint(todo: &mut Todo) -> (Frame, UiContext) {
		let ctx = UiContext::default();
		let height = todo.height(&ctx, 48);
		let mut frame = Frame::new(Size::new(48, height));
		let mut hits = Vec::new();
		todo.paint(
			&mut PaintCtx::new(&mut frame, &ctx, &mut hits, &mut Vec::new()),
			Rect::new(0, 0, 48, height),
		);
		(frame, ctx)
	}

	fn painted_text(todo: &mut Todo) -> String {
		let (frame, _) = paint(todo);
		(0..frame.size().height)
			.map(|row| frame_row_text(&frame, row))
			.collect::<Vec<_>>()
			.join("\n")
	}
	#[test]
	fn counts_walk_nested_closed_leaves_only() {
		let todo = Todo::new()
			.task(
				TodoTask::new()
					.label("phase")
					.task(TodoTask::new().label("a").status(TaskStatus::Done))
					.task(TodoTask::new().label("b")),
			)
			.task(TodoTask::new().label("flat").status(TaskStatus::Dropped));
		assert_eq!(todo.counts(), (2, 3));
	}

	#[test]
	fn status_parse_accepts_agent_aliases_and_rejects_junk() {
		assert_eq!(TaskStatus::parse("in_progress"), Some(TaskStatus::Active));
		assert_eq!(TaskStatus::parse("completed"), Some(TaskStatus::Done));
		assert_eq!(TaskStatus::parse("abandoned"), Some(TaskStatus::Dropped));
		assert_eq!(TaskStatus::parse("nope"), None);
	}

	#[test]
	fn spine_fill_pins_zero_half_and_full_progress() {
		let make = |closed: usize| {
			let mut todo = Todo::new();
			for index in 0..2 {
				let status = if index < closed {
					TaskStatus::Done
				} else {
					TaskStatus::Pending
				};
				todo = todo.task(
					TodoTask::new()
						.label(format!("task {index}"))
						.status(status),
				);
			}
			todo
		};
		for (closed, expected_accent) in [(0, 0), (1, 4), (2, 8)] {
			let (frame, ctx) = paint(&mut make(closed));
			assert_eq!(frame_row_text(&frame, 0).trim_end(), "TODO");
			assert!(frame_row_text(&frame, 1).contains("task 0"));
			assert_eq!(frame_row_text(&frame, 3).trim_end(), "└─────");
			let path = [
				frame.cell(0, 1).style.foreground_color(),
				frame.cell(0, 2).style.foreground_color(),
				frame.cell(0, 3).style.foreground_color(),
				frame.cell(1, 3).style.foreground_color(),
				frame.cell(2, 3).style.foreground_color(),
				frame.cell(3, 3).style.foreground_color(),
				frame.cell(4, 3).style.foreground_color(),
				frame.cell(5, 3).style.foreground_color(),
			];
			assert_eq!(
				path
					.iter()
					.filter(|&&color| color == ctx.theme.accent)
					.count(),
				expected_accent,
				"{closed}/2 progress path: {path:?}",
			);
			assert!(
				path
					.iter()
					.all(|&color| color == ctx.theme.accent || color == ctx.theme.muted)
			);
		}
	}

	#[test]
	fn phase_prefixes_are_canonical_and_bounded() {
		for (ordinal, expected) in [
			(0, "0. "),
			(1, "I. "),
			(4, "IV. "),
			(9, "IX. "),
			(49, "XLIX. "),
			(944, "CMXLIV. "),
			(3_999, "MMMCMXCIX. "),
			(4_000, "4000. "),
		] {
			assert_eq!(PhasePrefix::new(ordinal).as_str(), expected);
		}
		assert_eq!(PhasePrefix::new(usize::MAX).as_str(), format!("{}. ", usize::MAX),);
		assert!(has_canonical_roman_prefix("XLIX. phase"));
		assert!(!has_canonical_roman_prefix("IL. phase"));
		assert!(!has_canonical_roman_prefix("IV.phase"));
	}

	#[test]
	fn roman_numbering_only_prefixes_unprefixed_roots() {
		let mut todo = Todo::new()
			.with(Prop::Numbering, "roman")
			.task(
				TodoTask::new()
					.label("Plan")
					.task(TodoTask::new().label("Nested")),
			)
			.task(
				TodoTask::new()
					.label("II. Authored")
					.task(TodoTask::new().label("Later")),
			);
		let text = painted_text(&mut todo);
		assert!(text.contains("I. Plan"), "{text}");
		assert!(text.contains("Nested"), "{text}");
		assert!(!text.contains("I. Nested"), "{text}");
		assert_eq!(text.matches("II. Authored").count(), 1, "{text}");
		assert!(!text.contains("II. II. Authored"), "{text}");
	}

	#[test]
	fn absent_numbering_keeps_root_labels_unchanged() {
		let mut todo = Todo::new().task(TodoTask::new().label("Plan"));
		let text = painted_text(&mut todo);
		assert!(text.contains("Plan"), "{text}");
		assert!(!text.contains("I. Plan"), "{text}");
	}

	#[test]
	fn stage_window_ends_with_overflow_summary() {
		let mut todo = Todo::new();
		for index in 1..=7 {
			todo = todo.task(
				TodoTask::new()
					.label(format!("Stage {index}"))
					.task(TodoTask::new().label(format!("work {index}"))),
			);
		}
		let (frame, _) = paint(&mut todo);
		let text = (0..frame.size().height)
			.map(|row| frame_row_text(&frame, row))
			.collect::<Vec<_>>()
			.join("\n");
		assert!(text.contains("TODO"), "{text}");
		assert!(text.contains("Stage 5"), "{text}");
		assert!(!text.contains("Stage 6"), "{text}");
		assert!(text.contains("├─ … 2 more stages"), "{text}");
	}

	#[test]
	fn multiline_hud_copy_uses_charset_return_marker() {
		assert_eq!(
			collapse_hud_line("First line\n\n  Second line", Charset::Unicode),
			"First line ↵ Second line",
		);
		assert_eq!(collapse_hud_line("Task\r\n  preview", Charset::Ascii), "Task enter preview",);
		assert!(matches!(collapse_hud_line("one line", Charset::Unicode), Cow::Borrowed(_)));
	}
}
