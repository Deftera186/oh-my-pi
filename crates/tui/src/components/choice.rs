use omp_core::{IntoStr, Str};

use super::text::{append, put_clipped};
use crate::{
	UiContext,
	component::{Component, MemoKey, PaintCtx, Slot, next_slot},
	frame::{Rect, Style},
	props::{Prop, PropValue, Props},
	rich::{Pipeline, RichSink, RichText, cell_width},
};

/// A read-only checkbox or radio label backing the `<choice>` markup tag.
pub struct Choice {
	props:           Props,
	slot:            Slot,
	text:            Str,
	rich:            RichText,
	version:         u64,
	cached_width:    u16,
	cached:          Option<MemoKey>,
	cached_multi:    bool,
	cached_selected: bool,
}

impl Choice {
	/// Creates an empty, unselected single-choice display.
	pub fn new() -> Self {
		Self {
			props:           Props::new(),
			slot:            next_slot(),
			text:            Str::default(),
			rich:            RichText::default(),
			version:         1,
			cached_width:    0,
			cached:          None,
			cached_multi:    false,
			cached_selected: false,
		}
	}

	/// Sets one choice property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self.version = self.version.wrapping_add(1);
		self
	}

	/// Sets one choice property from a string.
	pub fn with_str(self, prop: Prop, value: &str) -> Self {
		self.with(prop, value)
	}

	/// Appends plain label text.
	pub fn text(mut self, text: impl IntoStr) -> Self {
		append(&mut self.text, text.into_str());
		self.version = self.version.wrapping_add(1);
		self
	}

	fn mark<'a>(&self, ctx: &'a UiContext) -> &'a str {
		if self.props.flag(Prop::Multi) {
			ctx.charset.checkbox(self.props.flag(Prop::Selected))
		} else {
			ctx.charset.radio(self.props.flag(Prop::Selected))
		}
	}

	fn indent(&self, ctx: &UiContext) -> u16 {
		cell_width(self.mark(ctx)).saturating_add(u16::from(!self.text.is_empty()))
	}

	fn render(&mut self, ctx: &UiContext, width: u16) {
		let multi = self.props.flag(Prop::Multi);
		let selected = self.props.flag(Prop::Selected);
		let label_width = width.saturating_sub(self.indent(ctx)).max(1);
		let key = MemoKey::new(self.version, ctx);
		if self.cached_width == label_width
			&& self.cached == Some(key)
			&& self.cached_multi == multi
			&& self.cached_selected == selected
		{
			return;
		}
		self.rich.clear();
		let base = self.props.style(&ctx.theme);
		let style = if self.props.foreground(&ctx.theme).is_some() {
			base
		} else {
			base.fg(ctx.theme.fg)
		};
		let mut wrap = (&mut self.rich).wrap(label_width);
		for (index, line) in self.text.split("\n").enumerate() {
			if index > 0 {
				wrap.newline();
			}
			if !line.is_empty() {
				wrap.run(style, line.as_str());
			}
		}
		wrap.finish();
		self.cached_width = label_width;
		self.cached = Some(key);
		self.cached_multi = multi;
		self.cached_selected = selected;
	}
}

impl Default for Choice {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for Choice {
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
		let indent = self.indent(ctx);
		let widest_word = self
			.text
			.split_whitespace()
			.map(cell_width)
			.max()
			.unwrap_or(0);
		let widest_line = self.text.lines().map(cell_width).max().unwrap_or(0);
		(indent.saturating_add(widest_word), indent.saturating_add(widest_line))
	}

	fn height(&mut self, ctx: &UiContext, width: u16) -> u16 {
		self.render(ctx, width);
		RichText::rows(&self.rich).max(1)
	}

	fn place(&mut self, ctx: &UiContext, content: Rect) {
		self.render(ctx, content.width);
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.y >= pc.clip || rect.width == 0 || rect.height == 0 {
			return;
		}
		self.render(pc.ctx, rect.width);
		let selected = self.props.flag(Prop::Selected);
		let mark = self.mark(pc.ctx);
		let mark_width = cell_width(mark);
		let right = rect.x.saturating_add(rect.width);
		if mark_width <= rect.width {
			pc.frame.put(
				rect.x,
				rect.y,
				mark,
				self.props.foreground(&pc.ctx.theme).map_or_else(
					|| {
						Style::new().fg(if selected {
							pc.ctx.theme.accent
						} else {
							pc.ctx.theme.muted
						})
					},
					|color| Style::new().fg(color),
				),
			);
		}
		if self.text.is_empty() {
			return;
		}
		let x = rect.x.saturating_add(mark_width).saturating_add(1);
		for row in 0..RichText::rows(&self.rich).min(rect.height) {
			let y = rect.y.saturating_add(row);
			if y >= pc.clip {
				break;
			}
			let mut run_x = x;
			for (style, text) in self.rich.row_runs(row) {
				run_x = put_clipped(pc.frame, run_x, y, right, text, style);
				if run_x >= right {
					break;
				}
			}
		}
	}

	fn set_text(&mut self, _ctx: &UiContext, text: Str) -> bool {
		if self.text == text {
			return false;
		}
		self.text = text;
		self.version = self.version.wrapping_add(1);
		true
	}
}

#[cfg(test)]
mod tests {
	use super::Choice;
	use crate::{
		Charset, UiContext,
		component::{Component, EventCtx, Flow, HitTag, PaintCtx},
		frame::{Color, Frame, Rect, Size},
		input::{Key, Mouse},
		props::Prop,
		rich::cell_width,
		test_support::frame_row_text,
	};

	fn paint(choice: &mut Choice, charset: Charset, width: u16) -> (Frame, Vec<crate::Hit>) {
		let ctx = UiContext { charset, ..UiContext::default() };
		let height = choice.height(&ctx, width);
		let mut frame = Frame::new(Size::new(width, height));
		let mut hits = Vec::new();
		let mut wakes = Vec::new();
		choice.paint(
			&mut PaintCtx::new(&mut frame, &ctx, &mut hits, &mut wakes),
			Rect::new(0, 0, width, height),
		);
		(frame, hits)
	}

	#[test]
	fn all_choice_states_use_charset_semantics() {
		let cases = [
			(Charset::Ascii, ["( )", "(o)", "[ ]", "[x]"]),
			(Charset::Unicode, ["○", "◉", "☐", "☑"]),
			(Charset::NerdFont, ["\u{f10c}", "\u{f192}", "\u{f096}", "\u{f14a}"]),
		];
		for (charset, expected) in cases {
			for (index, (multi, selected)) in
				[(false, false), (false, true), (true, false), (true, true)]
					.into_iter()
					.enumerate()
			{
				let mut choice = Choice::new()
					.with(Prop::Multi, multi)
					.with(Prop::Selected, selected)
					.text("label");
				let (frame, hits) = paint(&mut choice, charset, 20);
				assert!(frame_row_text(&frame, 0).starts_with(expected[index]));
				let theme = UiContext::default().theme;
				assert_eq!(
					frame.cell(0, 0).style.foreground_color(),
					if selected { theme.accent } else { theme.muted },
				);
				assert!(hits.is_empty(), "display choices do not create pointer targets");
				assert!(!choice.focusable(), "display choices do not enter focus rings");
				let mut values = serde_json::Map::new();
				choice.value(&mut values);
				assert!(values.is_empty(), "display choices do not export values");
				let ctx = UiContext::default();
				let mut events = EventCtx::new(&ctx, 20, 1);
				assert_eq!(choice.key(&mut events, Key::Space), Flow::Skip);
				assert_eq!(
					choice.mouse(
						&mut events,
						HitTag::Press,
						(0, 0),
						Rect::new(0, 0, 20, 1),
						Mouse::Click,
					),
					Flow::Skip,
					"display choices do not handle input or emit events",
				);
			}
		}
	}

	#[test]
	fn explicit_foreground_recolors_mark_and_label() {
		let custom = Color::Rgb(7, 8, 9);
		for prop in [Prop::Fg, Prop::Color] {
			let mut choice = Choice::new().with(prop, custom).text("label");
			let (frame, _) = paint(&mut choice, Charset::Ascii, 20);
			assert_eq!(frame.cell(0, 0).style.foreground_color(), custom, "mark tint");
			assert_eq!(frame.cell(4, 0).style.foreground_color(), custom, "label tint");
		}
	}

	#[test]
	fn long_labels_wrap_under_the_label_column() {
		let mut choice = Choice::new().text("alpha beta gamma delta epsilon");
		let width = 13;
		let (frame, _) = paint(&mut choice, Charset::Ascii, width);
		assert!(frame.size().height > 1);
		let indent = cell_width(Charset::Ascii.radio(false)) + 1;
		let rows: Vec<_> = (0..frame.size().height)
			.map(|row| frame_row_text(&frame, row))
			.collect();
		assert!(rows.iter().any(|row| row.contains("epsilon")));
		for row in rows.iter().skip(1) {
			assert_eq!(row.len() - row.trim_start().len(), usize::from(indent));
		}
	}
}
