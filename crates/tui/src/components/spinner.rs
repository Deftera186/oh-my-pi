//! Indeterminate activity indicator backing the `<spinner>` markup tag.

use std::time::Duration;

use omp_core::{IntoStr, Str};

use crate::{
	component::{Component, PaintCtx, Slot, next_slot},
	context::UiContext,
	frame::Rect,
	props::{Prop, PropValue, Props},
	rich::cell_width,
};

const RENDER_BACKPRESSURE_MULTIPLIER: u32 = 9;
const MAX_BACKPRESSURE_FRAME_COST: Duration = Duration::from_millis(200);

/// An animated one-cell spinner with an optional trailing label.
///
/// The reference consumer of the animation clock: it paints the
/// [`Frames`] glyph for [`PaintCtx::now`] and requests its next repaint
/// with [`PaintCtx::wake`], so it animates only while presented and stops
/// costing anything the moment it leaves the tree.
pub struct Spinner {
	props: Props,
	slot:  Slot,
	label: Str,
}

impl Spinner {
	/// Returns the post-frame idle time used to keep pending-work animations at
	/// or below ten percent render duty, bounded after pathological frame
	/// stalls.
	pub(crate) fn animation_backpressure(frame_cost: Duration) -> Duration {
		frame_cost
			.min(MAX_BACKPRESSURE_FRAME_COST)
			.saturating_mul(RENDER_BACKPRESSURE_MULTIPLIER)
	}

	/// Creates a bare spinner.
	pub fn new() -> Self {
		Self { props: Props::new(), slot: next_slot(), label: Str::default() }
	}

	/// Sets the text following the spinner glyph.
	pub fn label(mut self, label: impl IntoStr) -> Self {
		self.label = label.into_str();
		self
	}

	/// Sets one spinner property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}
}

impl Default for Spinner {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for Spinner {
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
		let natural = if self.label.is_empty() {
			1
		} else {
			cell_width(&self.label).saturating_add(2)
		};
		(1, natural)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		1
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.y >= pc.clip || rect.width == 0 {
			return;
		}
		let frames = pc.ctx.charset.spinner();
		let style = self.props.style(&pc.ctx.theme);
		let mut column = pc.frame.put(rect.x, rect.y, frames.at(pc.now), style);
		if !self.label.is_empty() {
			column = pc.frame.put(column, rect.y, " ", style);
			pc.frame.put(column, rect.y, &self.label, style);
		}
		pc.wake(self.slot, frames.next_change(pc.now));
	}

	fn set_text(&mut self, _ctx: &UiContext, text: Str) -> bool {
		if self.label == text {
			return false;
		}
		self.label = text;
		true
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;
	use crate::{context::Charset, test_support::frame_row_text, ui::Ui};

	#[test]
	fn ticking_advances_the_glyph_and_reschedules() {
		let mut ui = Ui::from_root(Spinner::new().label("busy"), 10, UiContext::default());
		assert_eq!(frame_row_text(ui.frame(), 0), "⠋ busy");
		assert_eq!(ui.next_wake(), Some(Duration::from_millis(80)));

		assert!(!ui.tick(Duration::from_millis(79)), "no deadline is due yet");
		assert!(ui.tick(Duration::from_millis(80)));
		assert_eq!(frame_row_text(ui.frame(), 0), "⠙ busy");
		assert_eq!(ui.next_wake(), Some(Duration::from_millis(160)));
	}

	#[test]
	fn animation_backpressure_uses_full_frame_cost_and_caps_pathological_stalls() {
		assert_eq!(
			Spinner::animation_backpressure(Duration::from_millis(40)),
			Duration::from_millis(360)
		);
		assert_eq!(
			Spinner::animation_backpressure(Duration::from_secs(5)),
			Duration::from_millis(1_800)
		);
	}

	#[test]
	fn ascii_charset_uses_spoke_frames() {
		let ctx = UiContext { charset: Charset::Ascii, ..UiContext::default() };
		let mut ui = Ui::from_root(Spinner::new(), 4, ctx);
		assert_eq!(frame_row_text(ui.frame(), 0), "|");
		ui.tick(Duration::from_millis(120));
		assert_eq!(frame_row_text(ui.frame(), 0), "/");
	}

	#[test]
	fn set_text_replaces_the_label() {
		let mut ui = Ui::from_root(
			Spinner::new().label("indexing").with(Prop::Id, "spin"),
			24,
			UiContext::default(),
		);
		assert!(ui.set_text("spin", "linking"));
		assert_eq!(frame_row_text(ui.frame(), 0), "⠋ linking");
		assert!(!ui.set_text("spin", "linking"), "unchanged text reports no update");
	}
}
