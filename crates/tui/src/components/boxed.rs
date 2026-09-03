use super::layout::{stack_height, stack_measure, stack_place};
use crate::{
	component::{
		Cached, Component, EventCtx, Flow, HitTag, IntoChildren, PaintCtx, Slot, next_slot,
	},
	context::UiContext,
	frame::Rect,
	input::{Key, Mouse, UiEvent},
	markup::Border,
	props::{Prop, PropValue, Props},
};

/// A bordered child stack backing the `<box>` markup tag.
pub struct Boxed {
	props:    Props,
	slot:     Slot,
	children: Vec<Cached>,
}

impl Boxed {
	/// Creates an empty box with the default border.
	pub fn new() -> Self {
		Self {
			props:    Props::new().with(Prop::Border, Border::default()),
			slot:     next_slot(),
			children: Vec::new(),
		}
	}

	/// Sets one box property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Sets one box property from a string.
	pub fn with_str(mut self, prop: Prop, value: &str) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Appends child components to the box.
	pub fn child(mut self, children: impl IntoChildren) -> Self {
		children.extend_children(&mut self.children);
		self
	}
}

impl Default for Boxed {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for Boxed {
	fn props(&self) -> &Props {
		&self.props
	}

	fn props_mut(&mut self) -> &mut Props {
		&mut self.props
	}

	fn slot(&self) -> Slot {
		self.slot
	}

	fn children(&self) -> &[Cached] {
		&self.children
	}

	fn children_mut(&mut self) -> &mut [Cached] {
		&mut self.children
	}

	fn measure(&mut self, ctx: &UiContext) -> (u16, u16) {
		stack_measure(ctx, &mut self.children)
	}

	fn height(&mut self, ctx: &UiContext, width: u16) -> u16 {
		stack_height(ctx, &mut self.children, width, self.props.gap())
	}

	fn place(&mut self, ctx: &UiContext, content: Rect) {
		stack_place(
			ctx,
			&mut self.children,
			content,
			self.props.gap(),
			self.props.valign(),
			self.props.align(),
		);
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, _rect: Rect) {
		for child in self.children.iter_mut().filter(|child| child.visible) {
			child.paint(pc);
		}
	}

	/// A focusable, `id`-carrying box presses like a button: Enter emits
	/// [`UiEvent::Pressed`] with its id.
	fn key(&mut self, _ec: &mut EventCtx<'_>, key: Key) -> Flow {
		if key == Key::Enter
			&& self.props.flag(Prop::Focus)
			&& let Some(id) = self.props.id()
		{
			return Flow::Event(UiEvent::Pressed(id.clone()));
		}
		Flow::Skip
	}

	/// Clicking the pointer zone presses the same way.
	fn mouse(
		&mut self,
		_ec: &mut EventCtx<'_>,
		tag: HitTag,
		_at: (u16, u16),
		_rect: Rect,
		mouse: Mouse,
	) -> Flow {
		if tag == HitTag::Zone
			&& mouse == Mouse::Click
			&& self.props.flag(Prop::Focus)
			&& let Some(id) = self.props.id()
		{
			return Flow::Event(UiEvent::Pressed(id.clone()));
		}
		Flow::Skip
	}
}

#[cfg(test)]
mod tests {
	use super::Boxed;
	use crate::{
		component::{Cached, PaintCtx},
		components::{Hr, TextLeaf},
		context::UiContext,
		frame::{Frame, Rect, Size},
		markup::Border,
		props::Prop,
		test_support::frame_row_text,
	};

	#[test]
	fn cached_paints_box_border_and_title() {
		let ctx = UiContext::default();
		let mut root = Cached::new(Box::new(
			Boxed::new()
				.with(Prop::Border, Border::Round)
				.with(Prop::Title, "Panel")
				.child(TextLeaf::new().text("body")),
		));
		let height = root.height(&ctx, 14);
		root.place(&ctx, Rect::new(0, 0, 14, height));
		let mut frame = Frame::new(Size::new(14, height));
		let mut hits = Vec::new();
		root.paint(&mut PaintCtx::new(&mut frame, &ctx, &mut hits, &mut Vec::new()));
		assert!(frame_row_text(&frame, 0).starts_with("╭─ Panel "));
		assert_eq!(frame_row_text(&frame, 1), "│body        │");
		assert_eq!(frame_row_text(&frame, height - 1), "╰────────────╯");
	}

	#[test]
	fn child_rule_joins_box_border() {
		let ctx = UiContext::default();
		let mut root = Cached::new(Box::new(
			Boxed::new()
				.with(Prop::Border, Border::Round)
				.child(TextLeaf::new().text("above"))
				.child(
					Hr::new()
						.with(Prop::Title, "Output")
						.with(Prop::TitlePad, 3_u16),
				)
				.child(TextLeaf::new().text("below")),
		));
		let height = root.height(&ctx, 16);
		root.place(&ctx, Rect::new(0, 0, 16, height));
		let mut frame = Frame::new(Size::new(16, height));
		let mut hits = Vec::new();
		root.paint(&mut PaintCtx::new(&mut frame, &ctx, &mut hits, &mut Vec::new()));
		assert_eq!(frame_row_text(&frame, 2), "├─── Output ───┤");
	}

	#[test]
	fn title_pad_retains_requested_rule_cells_before_title() {
		let ctx = UiContext::default();
		let mut root = Cached::new(Box::new(
			Boxed::new()
				.with(Prop::Border, Border::Round)
				.with(Prop::Title, "Panel")
				.with(Prop::TitlePad, 3_u16)
				.child(TextLeaf::new().text("body")),
		));
		let height = root.height(&ctx, 16);
		root.place(&ctx, Rect::new(0, 0, 16, height));
		let mut frame = Frame::new(Size::new(16, height));
		let mut hits = Vec::new();
		root.paint(&mut PaintCtx::new(&mut frame, &ctx, &mut hits, &mut Vec::new()));
		assert!(frame_row_text(&frame, 0).starts_with("╭─── Panel "));
	}

	#[test]
	fn box_title_truncates_between_corner_cells_at_boundary_widths() {
		let ctx = UiContext::default();
		for (width, expected) in [(7, "╭ al… ╮"), (5, "╭ … ╮"), (4, "╭ …╮"), (3, "╭…╮")]
		{
			let mut root = Cached::new(Box::new(
				Boxed::new()
					.with(Prop::Border, Border::Round)
					.with(Prop::Title, "alphabet")
					.child(TextLeaf::new().text("")),
			));
			let height = root.height(&ctx, width);
			root.place(&ctx, Rect::new(0, 0, width, height));
			let mut frame = Frame::new(Size::new(width, height));
			let mut hits = Vec::new();
			root.paint(&mut PaintCtx::new(&mut frame, &ctx, &mut hits, &mut Vec::new()));
			assert_eq!(frame_row_text(&frame, 0), expected);
		}
	}
}
