use omp_core::{IntoStr, Str};

use super::{overflow_plan, paint_overflow_footer};
use crate::{
	component::{Component, PaintCtx, Slot, next_slot},
	components::text::paint_rich,
	context::UiContext,
	frame::{Color, Rect, Style},
	props::{Prop, PropValue, Props},
	rich::{Pipeline, Prefix, RichSink, RichText, width_config_epoch},
};

/// The type of a line in a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
	/// A file header or metadata line.
	Header,
	/// An unchanged context line.
	Context,
	/// An added line.
	Add,
	/// A removed line.
	Remove,
	/// A revision-bound diagnostic attached to the diff.
	Diagnostic,
}

/// A single line in a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
	/// The type of the diff line.
	pub kind: DiffKind,
	/// The text content of the diff line.
	pub text: Str,
}

/// Width-sensitive diff presentation policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DiffLayout {
	/// Select side-by-side at wide widths and unified otherwise.
	#[default]
	Auto,
	/// Always render one canonical unified stream.
	Unified,
	/// Render adjacent remove/add pairs in two columns when space permits.
	SideBySide,
}

/// A component that renders a diff with semantic styles.
pub struct DiffView {
	props:              Props,
	slot:               Slot,
	lines:              Vec<DiffLine>,
	rich:               RichText,
	rendered_lines:     usize,
	cached_width:       u16,
	cached_width_epoch: u64,
	cached_revision:    u64,
	cached_context:     Option<u16>,
	layout:             DiffLayout,
	colorblind:         bool,
	cached_side:        bool,
	cached_colorblind:  bool,
}

impl DiffView {
	/// Creates a new empty diff view.
	pub fn new() -> Self {
		Self {
			props:              Props::new(),
			slot:               next_slot(),
			lines:              Vec::new(),
			rich:               RichText::default(),
			rendered_lines:     0,
			cached_width:       0,
			cached_width_epoch: 0,
			cached_revision:    0,
			cached_context:     None,
			layout:             DiffLayout::Auto,
			colorblind:         false,
			cached_side:        false,
			cached_colorblind:  false,
		}
	}

	/// Sets one diff property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Selects unified, side-by-side, or width-sensitive automatic layout.
	pub const fn set_layout(&mut self, layout: DiffLayout) {
		self.layout = layout;
	}

	/// Enables a blue/amber palette in addition to semantic +/- glyphs.
	pub const fn set_colorblind(&mut self, colorblind: bool) {
		self.colorblind = colorblind;
	}

	/// Appends a new line to the diff view.
	pub fn push(&mut self, kind: DiffKind, text: impl IntoStr) {
		self.lines.push(DiffLine { kind, text: text.into_str() });
	}

	/// Clears all lines from the diff view.
	///
	/// Returns whether the view contained any lines before clearing.
	pub fn clear(&mut self) -> bool {
		if self.lines.is_empty() {
			return false;
		}
		self.lines.clear();
		self.rendered_lines = 0;
		self.rich.clear();
		true
	}

	/// Appends multiple lines to the diff view.
	///
	/// Returns whether any lines were added.
	pub fn extend(&mut self, lines: impl IntoIterator<Item = DiffLine>) -> bool {
		let start = self.lines.len();
		self.lines.extend(lines);
		self.lines.len() > start
	}

	/// Replaces all lines in the diff view.
	pub fn replace(&mut self, lines: Vec<DiffLine>) {
		self.lines = lines;
		self.rendered_lines = 0;
		self.rich.clear();
	}

	fn render(&mut self, ctx: &UiContext, width: u16) {
		let width = width.max(1);
		let width_epoch = width_config_epoch();
		let revision = ctx.revision;
		let context = self.props.context();
		let side = match self.layout {
			DiffLayout::Auto => width >= 100,
			DiffLayout::Unified => false,
			DiffLayout::SideBySide => width >= 40,
		};

		if self.cached_width == width
			&& self.cached_width_epoch == width_epoch
			&& self.cached_revision == revision
			&& self.cached_context == context
			&& self.cached_side == side
			&& self.cached_colorblind == self.colorblind
		{
			if self.rendered_lines == self.lines.len() {
				return;
			}
			if context.is_some() {
				// A newly appended change can make earlier trailing context visible.
				self.rich.clear();
				self.rendered_lines = 0;
			}
		} else {
			self.rich.clear();
			self.rendered_lines = 0;
			self.cached_width = width;
			self.cached_width_epoch = width_epoch;
			self.cached_revision = revision;
			self.cached_context = context;
			self.cached_side = side;
			self.cached_colorblind = self.colorblind;
		}

		let (info, muted, ok, err) = if self.colorblind {
			(ctx.theme.info, ctx.theme.muted, ctx.theme.secondary, ctx.theme.accent)
		} else {
			(ctx.theme.info, ctx.theme.muted, ctx.theme.ok, ctx.theme.err)
		};
		if side {
			self.render_side_by_side(width, info, muted, ok, err);
			return;
		}

		let prefixes = ctx.charset.diff_prefixes();
		let mut p_header = Prefix::default();
		p_header.push(Style::new().fg(info).bold(), prefixes.header);
		let mut p_context = Prefix::default();
		p_context.push(Style::new().fg(muted), prefixes.context);
		let mut p_add = Prefix::default();
		p_add.push(Style::new().fg(ok), prefixes.add);
		let mut p_remove = Prefix::default();
		p_remove.push(Style::new().fg(err), prefixes.remove);
		let mut p_diagnostic = Prefix::default();
		p_diagnostic.push(Style::new().fg(ctx.theme.warn).bold(), "! ");

		let mut c_header = Prefix::default();
		c_header.push(Style::new().fg(info).bold(), prefixes.continuation);
		let mut c_context = Prefix::default();
		c_context.push(Style::new().fg(muted), prefixes.continuation);
		let mut c_add = Prefix::default();
		c_add.push(Style::new().fg(ok), prefixes.continuation);
		let mut c_remove = Prefix::default();
		c_remove.push(Style::new().fg(err), prefixes.continuation);
		let mut c_diagnostic = Prefix::default();
		c_diagnostic.push(Style::new().fg(ctx.theme.warn), prefixes.continuation);

		let mut context_run = (0, 0, false, false);
		for (offset, line) in self.lines[self.rendered_lines..].iter().enumerate() {
			let index = self.rendered_lines + offset;
			if let Some(count) = context
				&& line.kind == DiffKind::Context
			{
				if index >= context_run.1 {
					let end = self.lines[index..]
						.iter()
						.position(|candidate| candidate.kind != DiffKind::Context)
						.map_or(self.lines.len(), |offset| index + offset);
					let changed_before = index > 0
						&& matches!(self.lines[index - 1].kind, DiffKind::Add | DiffKind::Remove);
					let changed_after = end < self.lines.len()
						&& matches!(self.lines[end].kind, DiffKind::Add | DiffKind::Remove);
					context_run = (index, end, changed_before, changed_after);
				}
				let count = usize::from(count);
				let near_before = context_run.2 && index - context_run.0 < count;
				let near_after = context_run.3 && context_run.1 - index <= count;
				if !near_before && !near_after {
					let first_omitted = context_run.0 + usize::from(context_run.2).saturating_mul(count);
					let last_omitted = context_run
						.1
						.saturating_sub(usize::from(context_run.3).saturating_mul(count));
					if index == first_omitted && first_omitted < last_omitted {
						let omitted = last_omitted - first_omitted;
						let mut wrap = (&mut self.rich).wrap_chars_prefixed(width, &p_header, &c_header);
						wrap.run(
							Style::new().fg(muted).italic(),
							&format!("… {omitted} unchanged lines …"),
						);
						wrap.newline();
					}
					continue;
				}
			}
			let (prefix, cont, text_style) = match line.kind {
				DiffKind::Header => (&p_header, &c_header, Style::new().fg(info).bold()),
				DiffKind::Context => (&p_context, &c_context, Style::new().fg(ctx.theme.fg)),
				DiffKind::Add => (&p_add, &c_add, Style::new().fg(ok)),
				DiffKind::Remove => (&p_remove, &c_remove, Style::new().fg(err)),
				DiffKind::Diagnostic => (&p_diagnostic, &c_diagnostic, Style::new().fg(ctx.theme.warn)),
			};

			let mut wrap = (&mut self.rich).wrap_chars_prefixed(width, prefix, cont);
			for (index, physical_line) in line.text.split("\n").enumerate() {
				if index > 0 {
					wrap.newline();
				}
				if !physical_line.is_empty() {
					wrap.run(text_style, physical_line.as_str());
				}
			}
			wrap.newline();
		}
		self.rendered_lines = self.lines.len();
	}

	fn render_side_by_side(
		&mut self,
		width: u16,
		info: Color,
		muted: Color,
		add: Color,
		remove: Color,
	) {
		self.rich.clear();
		let split = usize::from(width.saturating_sub(3) / 2);
		let mut index = 0;
		while index < self.lines.len() {
			let line = &self.lines[index];
			if line.kind == DiffKind::Remove
				&& let Some(next) = self.lines.get(index + 1)
				&& next.kind == DiffKind::Add
			{
				let left = truncate_cells(line.text.as_str(), split.saturating_sub(2));
				let right = truncate_cells(next.text.as_str(), split.saturating_sub(2));
				let mut wrap = (&mut self.rich).wrap_chars(width);
				wrap.run(Style::new().fg(remove), "- ");
				wrap.run(Style::new().fg(remove), left.as_str());
				wrap.run(Style::new().fg(muted), " │ ");
				wrap.run(Style::new().fg(add), "+ ");
				wrap.run(Style::new().fg(add), right.as_str());
				wrap.newline();
				index += 2;
				continue;
			}
			let color = match line.kind {
				DiffKind::Header => info,
				DiffKind::Context => muted,
				DiffKind::Add => add,
				DiffKind::Remove => remove,
				DiffKind::Diagnostic => self.cached_colorblind.then_some(add).unwrap_or(info),
			};
			let prefix = match line.kind {
				DiffKind::Header | DiffKind::Context => "  ",
				DiffKind::Add => "+ ",
				DiffKind::Remove => "- ",
				DiffKind::Diagnostic => "! ",
			};
			let mut wrap = (&mut self.rich).wrap_chars(width);
			wrap.run(Style::new().fg(color), prefix);
			wrap.run(Style::new().fg(color), line.text.as_str());
			wrap.newline();
			index += 1;
		}
		self.rendered_lines = self.lines.len();
	}
}

fn truncate_cells(text: &str, width: usize) -> Str {
	if xutf::width_str(text) <= width {
		return Str::new(text);
	}
	let mut out = String::new();
	for character in text.chars() {
		if xutf::width_str(&out).saturating_add(xutf::width_char(character)) > width {
			break;
		}
		out.push(character);
	}
	Str::new(out)
}

impl Default for DiffView {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for DiffView {
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
		(1, u16::MAX) // DiffView flows to any width
	}

	fn height(&mut self, ctx: &UiContext, width: u16) -> u16 {
		self.render(ctx, width);
		let natural = RichText::rows(&self.rich);
		self
			.props
			.max_rows()
			.map_or(natural, |max| natural.min(max))
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		self.render(pc.ctx, rect.width);
		let natural = RichText::rows(&self.rich);
		let plan = overflow_plan(&self.props, natural, rect.height);
		let content_rows = plan.map_or(rect.height, |plan| plan.content_rows);
		paint_rich(
			pc,
			Rect::new(rect.x, rect.y, rect.width, content_rows),
			&self.rich,
			self.props.align(),
		);
		if let Some(plan) = plan {
			paint_overflow_footer(pc, rect, plan);
		}
	}
}

#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;
	use crate::{
		UiContext,
		component::{Component, PaintCtx},
		frame::{Frame, Rect, Size},
		test_support::{frame_cell_style, frame_row_text},
	};

	fn paint(component: &mut dyn Component, width: u16, height: u16) -> Frame {
		let ctx = UiContext::default();
		let mut frame = Frame::new(Size::new(width, height));
		let mut hits = Vec::new();
		let mut wakes = Vec::new();
		let mut pc = PaintCtx::new(&mut frame, &ctx, &mut hits, &mut wakes);
		component.paint(&mut pc, Rect::new(0, 0, width, height));
		frame
	}

	#[test]
	fn renders_mixed_hunks_with_semantic_styles() {
		let mut diff = DiffView::new();
		diff.push(DiffKind::Header, "src/main.rs");
		diff.push(DiffKind::Context, "fn main() {");
		diff.push(DiffKind::Remove, "    println!(\"Hello\");");
		diff.push(DiffKind::Add, "    println!(\"World\");");
		diff.push(DiffKind::Context, "}");

		let frame = paint(&mut diff, 40, 5);
		assert_eq!(frame_row_text(&frame, 0).trim_end(), "  src/main.rs");
		assert_eq!(frame_row_text(&frame, 1).trim_end(), "  fn main() {");
		assert_eq!(frame_row_text(&frame, 2).trim_end(), "-     println!(\"Hello\");");
		assert_eq!(frame_row_text(&frame, 3).trim_end(), "+     println!(\"World\");");
		assert_eq!(frame_row_text(&frame, 4).trim_end(), "  }");

		let ctx = UiContext::default();
		assert_eq!(frame_cell_style(&frame, 0, 0).foreground, ctx.theme.info);
		assert_eq!(frame_cell_style(&frame, 0, 1).foreground, ctx.theme.muted);
		assert_eq!(frame_cell_style(&frame, 0, 2).foreground, ctx.theme.err);
		assert_eq!(frame_cell_style(&frame, 0, 3).foreground, ctx.theme.ok);
	}

	#[test]
	fn incremental_replacement() {
		let mut diff = DiffView::new();
		diff.push(DiffKind::Add, "a");
		let frame1 = paint(&mut diff, 10, 2);
		assert_eq!(frame_row_text(&frame1, 0).trim_end(), "+ a");

		diff.push(DiffKind::Add, "b");
		let frame2 = paint(&mut diff, 10, 2);
		assert_eq!(frame_row_text(&frame2, 1).trim_end(), "+ b");

		for i in 0..RichText::rows(&diff.rich) {
			assert!(!diff.rich.row_soft_wrap(i), "wrapped DiffView rows should not be soft");
		}

		diff.replace(vec![DiffLine { kind: DiffKind::Remove, text: sf!("c") }]);
		let frame3 = paint(&mut diff, 10, 2);
		assert_eq!(frame_row_text(&frame3, 0).trim_end(), "- c");
		assert_eq!(frame_row_text(&frame3, 1).trim_end(), "");
	}

	#[test]
	fn unicode_clipping() {
		let mut diff = DiffView::new();
		diff.push(DiffKind::Add, "한글");
		let frame = paint(&mut diff, 5, 2);
		assert_eq!(frame_row_text(&frame, 0).trim_end(), "+ 한");
		assert_eq!(frame_row_text(&frame, 1).trim_end(), "  글");
	}
	#[test]
	fn max_rows_bounds_wrapped_physical_rows() {
		let mut diff = DiffView::new().with(Prop::MaxRows, 3_u16);
		diff.push(DiffKind::Add, "abcdefghijklmnopqrstuvwxyz");
		diff.push(DiffKind::Add, "another logical line");
		let ctx = UiContext::default();

		assert_eq!(diff.height(&ctx, 8), 3);
	}
	#[test]
	fn overflow_footer_reserves_a_row_and_counts_wrapped_rows() {
		let mut diff = DiffView::new()
			.with(Prop::MaxRows, 3_u16)
			.with(Prop::Overflow, "diff rows");
		diff.push(DiffKind::Add, "abcdefghijklmnopqrstuvwxyz");
		let ctx = UiContext::default();
		assert_eq!(diff.height(&ctx, 8), 3);
		let frame = paint(&mut diff, 8, 3);
		assert_eq!(frame_row_text(&frame, 2), "… 3 more");
	}
	fn paint_with_ctx(
		component: &mut dyn Component,
		ctx: UiContext,
		width: u16,
		height: u16,
	) -> Frame {
		let mut frame = Frame::new(Size::new(width, height));
		let mut hits = Vec::new();
		let mut wakes = Vec::new();
		let mut pc = PaintCtx::new(&mut frame, &ctx, &mut hits, &mut wakes);
		component.paint(&mut pc, Rect::new(0, 0, width, height));
		frame
	}

	#[test]
	fn verifies_append_cache_matches_fresh_build() {
		let mut incremental = DiffView::new();
		let mut fresh = DiffView::new();
		let ctx = UiContext::default();

		incremental.push(DiffKind::Header, "file.txt");
		let _ = paint_with_ctx(&mut incremental, ctx.clone(), 20, 10);

		incremental.extend(vec![
			DiffLine { kind: DiffKind::Context, text: sf!("line 1") },
			DiffLine { kind: DiffKind::Remove, text: sf!("line 2") },
		]);
		let _ = paint_with_ctx(&mut incremental, ctx.clone(), 20, 10);

		incremental.push(DiffKind::Add, "line 3");
		let frame_incremental = paint_with_ctx(&mut incremental, ctx.clone(), 20, 10);

		fresh.extend(vec![
			DiffLine { kind: DiffKind::Header, text: sf!("file.txt") },
			DiffLine { kind: DiffKind::Context, text: sf!("line 1") },
			DiffLine { kind: DiffKind::Remove, text: sf!("line 2") },
			DiffLine { kind: DiffKind::Add, text: sf!("line 3") },
		]);
		let frame_fresh = paint_with_ctx(&mut fresh, ctx, 20, 10);

		assert_eq!(frame_row_text(&frame_incremental, 0), frame_row_text(&frame_fresh, 0));
		assert_eq!(frame_row_text(&frame_incremental, 1), frame_row_text(&frame_fresh, 1));
		assert_eq!(frame_row_text(&frame_incremental, 2), frame_row_text(&frame_fresh, 2));
		assert_eq!(frame_row_text(&frame_incremental, 3), frame_row_text(&frame_fresh, 3));
	}

	#[test]
	fn clear_and_extend_return_semantic_changes() {
		let mut diff = DiffView::new();
		assert!(!diff.clear());
		assert!(diff.extend(vec![DiffLine { kind: DiffKind::Add, text: sf!("x") }]));
		assert!(!diff.extend(vec![]));
		assert!(diff.clear());
	}

	#[test]
	fn empty_diff() {
		let mut diff = DiffView::new();
		let frame = paint(&mut diff, 10, 2);
		assert_eq!(frame_row_text(&frame, 0).trim_end(), "");
	}
}
