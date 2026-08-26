use omp_core::{IntoStr, Str};

use super::{
	layout::{stack_height, stack_measure, stack_place},
	text::put_clipped,
};
use crate::{
	component::{Cached, Component, IntoChildren, PaintCtx, Slot, next_slot},
	context::UiContext,
	frame::Rect,
	props::{Prop, PropValue, Props},
	rich::cell_width,
};

/// A compact label/value row backing the `<fact>` markup tag.
///
/// Values retain their own component tree and use the space after the muted
/// label. When the label and the value's minimum width cannot share a row,
/// the value moves below the label rather than being squeezed out.
pub struct Fact {
	props:    Props,
	slot:     Slot,
	label:    Str,
	children: Vec<Cached>,
}

impl Fact {
	/// Creates an empty fact.
	pub fn new() -> Self {
		Self {
			props:    Props::new(),
			slot:     next_slot(),
			label:    Str::default(),
			children: Vec::new(),
		}
	}

	/// Sets one fact property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Sets one fact property from a string.
	pub fn with_str(mut self, prop: Prop, value: &str) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Sets the label shown before the value.
	pub fn label(mut self, label: impl IntoStr) -> Self {
		self.label = label.into_str();
		self
	}

	/// Appends value children.
	pub fn child(mut self, children: impl IntoChildren) -> Self {
		children.extend_children(&mut self.children);
		self
	}

	fn resolved_label(&self) -> &str {
		if self.label.is_empty() {
			self.props.str_of(Prop::Label).map_or("", Str::as_str)
		} else {
			self.label.as_str()
		}
	}

	fn has_value(&self) -> bool {
		self.children.iter().any(|child| child.visible)
	}

	fn geometry(&mut self, ctx: &UiContext, width: u16) -> Geometry {
		let label_width = cell_width(self.resolved_label());
		let has_label = label_width > 0;
		let has_value = self.has_value();
		if !has_label {
			return Geometry {
				label_width: 0,
				value_x:     0,
				value_width: width,
				stacked:     false,
			};
		}
		if !has_value {
			return Geometry {
				label_width: label_width.min(width),
				value_x:     width,
				value_width: 0,
				stacked:     false,
			};
		}
		let (value_min, _) = stack_measure(ctx, &mut self.children);
		let inline = label_width
			.saturating_add(1)
			.saturating_add(value_min.max(1))
			<= width;
		if inline {
			Geometry {
				label_width,
				value_x: label_width.saturating_add(1),
				value_width: width.saturating_sub(label_width.saturating_add(1)),
				stacked: false,
			}
		} else {
			Geometry {
				label_width: label_width.min(width),
				value_x:     0,
				value_width: width,
				stacked:     true,
			}
		}
	}
}

impl Default for Fact {
	fn default() -> Self {
		Self::new()
	}
}

#[derive(Clone, Copy)]
struct Geometry {
	label_width: u16,
	value_x:     u16,
	value_width: u16,
	stacked:     bool,
}

impl Component for Fact {
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
		let label = cell_width(self.resolved_label());
		let (value_min, value_natural) = stack_measure(ctx, &mut self.children);
		match (label > 0, self.has_value()) {
			(false, false) => (0, 0),
			(true, false) => (1, label),
			(false, true) => (value_min, value_natural),
			(true, true) => (value_min.max(1), label.saturating_add(1).saturating_add(value_natural)),
		}
	}

	fn height(&mut self, ctx: &UiContext, width: u16) -> u16 {
		let geometry = self.geometry(ctx, width);
		let label_rows = u16::from(geometry.label_width > 0);
		let value_rows = if geometry.value_width == 0 {
			0
		} else {
			stack_height(ctx, &mut self.children, geometry.value_width, self.props.gap())
		};
		if geometry.stacked {
			label_rows.saturating_add(value_rows)
		} else {
			label_rows.max(value_rows)
		}
	}

	fn place(&mut self, ctx: &UiContext, content: Rect) {
		let geometry = self.geometry(ctx, content.width);
		if geometry.value_width == 0 {
			return;
		}
		let y = content.y.saturating_add(u16::from(geometry.stacked));
		let height = stack_height(ctx, &mut self.children, geometry.value_width, self.props.gap());
		stack_place(
			ctx,
			&mut self.children,
			Rect::new(content.x.saturating_add(geometry.value_x), y, geometry.value_width, height),
			self.props.gap(),
			self.props.valign(),
			self.props.align(),
		);
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		let geometry = self.geometry(pc.ctx, rect.width);
		if geometry.label_width > 0 && rect.height > 0 && rect.y < pc.clip {
			let style = self.props.style(&pc.ctx.theme).fg(pc.ctx.theme.muted);
			put_clipped(
				pc.frame,
				rect.x,
				rect.y,
				rect.x.saturating_add(geometry.label_width),
				self.resolved_label(),
				style,
			);
		}
		for child in self.children.iter_mut().filter(|child| child.visible) {
			child.paint(pc);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::Fact;
	use crate::{
		component::{Component, PaintCtx},
		components::{Button, Col, TextLeaf},
		context::UiContext,
		frame::{Color, Frame, Rect, Size},
		props::Prop,
		test_support::{frame_cell_style, frame_row_text},
	};

	fn paint(mut fact: Fact, width: u16) -> (Fact, Frame) {
		let ctx = UiContext::default();
		let height = fact.height(&ctx, width);
		fact.place(&ctx, Rect::new(0, 0, width, height));
		let mut frame = Frame::new(Size::new(width, height));
		fact.paint(
			&mut PaintCtx::new(&mut frame, &ctx, &mut Vec::new(), &mut Vec::new()),
			Rect::new(0, 0, width, height),
		);
		(fact, frame)
	}

	#[test]
	fn paints_muted_label_and_wrapped_value_on_one_baseline() {
		let ctx = UiContext::default();
		let (_, frame) = paint(
			Fact::new()
				.label("File")
				.child(TextLeaf::new().text("alpha beta gamma")),
			12,
		);
		assert_eq!(frame_row_text(&frame, 0), "File alpha");
		assert_eq!(frame_row_text(&frame, 1), "     beta");
		assert_eq!(frame_row_text(&frame, 2), "     gamma");
		assert_eq!(frame_cell_style(&frame, 0, 0).foreground_color(), ctx.theme.muted);
	}

	#[test]
	fn narrow_fact_stacks_without_hiding_its_value() {
		let (_, frame) = paint(
			Fact::new()
				.with(Prop::Label, "Location")
				.child(TextLeaf::new().text("here")),
			4,
		);
		assert_eq!(frame_row_text(&frame, 0), "Loca");
		assert_eq!(frame_row_text(&frame, 1), "here");
	}

	#[test]
	fn empty_label_and_value_have_deterministic_geometry() {
		let ctx = UiContext::default();
		let mut empty = Fact::new();
		assert_eq!(empty.measure(&ctx), (0, 0));
		assert_eq!(empty.height(&ctx, 5), 0);

		let (_, frame) = paint(Fact::new().child(TextLeaf::new().text("value")), 5);
		assert_eq!(frame_row_text(&frame, 0), "value");
		let (_, frame) = paint(Fact::new().label("Kind"), 3);
		assert_eq!(frame_row_text(&frame, 0), "Kin");
	}

	#[test]
	fn nested_children_keep_styles_and_inherited_fact_style_reaches_label() {
		let accent = Color::Rgb(1, 2, 3);
		let (_, frame) = paint(
			Fact::new()
				.label("Kind")
				.with(Prop::Bold, true)
				.child(TextLeaf::new().text("styled").with(Prop::Fg, accent)),
			12,
		);
		assert!(frame_cell_style(&frame, 0, 0).spec().bold);
		assert_eq!(frame_cell_style(&frame, 5, 0).foreground_color(), accent);
	}

	#[test]
	fn forwards_focus_through_nested_value_children() {
		let fact = Fact::new()
			.label("Action")
			.child(Col::new().child(Button::new().child("run").with(Prop::Focus, true)));
		let mut ring = Vec::new();
		fact.ring(&mut ring);
		assert_eq!(ring.len(), 1);
		assert_ne!(ring[0], fact.slot());
	}
}
