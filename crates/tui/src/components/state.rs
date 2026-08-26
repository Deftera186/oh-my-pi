//! Compact semantic lifecycle-state indicator backing the `<state>` markup tag.

use crate::{
	Icon, UiContext,
	component::{Component, PaintCtx, Slot, next_slot},
	frame::{Color, Rect, Style},
	props::{Prop, PropValue, Props},
	rich::cell_width,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SemanticState {
	Running,
	Active,
	Idle,
	Parked,
	Completed,
	Failed,
	Stopped,
	Unknown,
}

impl SemanticState {
	fn from_props(props: &Props) -> Self {
		let Some(status) = props.str_of(Prop::Status) else {
			return Self::Unknown;
		};
		let status = status.trim();
		if status.eq_ignore_ascii_case("running") {
			Self::Running
		} else if status.eq_ignore_ascii_case("active") {
			Self::Active
		} else if status.eq_ignore_ascii_case("idle") {
			Self::Idle
		} else if status.eq_ignore_ascii_case("parked") {
			Self::Parked
		} else if status.eq_ignore_ascii_case("completed") {
			Self::Completed
		} else if status.eq_ignore_ascii_case("failed") {
			Self::Failed
		} else if status.eq_ignore_ascii_case("stopped") {
			Self::Stopped
		} else {
			Self::Unknown
		}
	}

	const fn icon(self) -> Icon {
		match self {
			Self::Running => Icon::Running,
			Self::Active => Icon::Enabled,
			Self::Idle => Icon::Idle,
			Self::Parked => Icon::Parked,
			Self::Completed => Icon::Completed,
			Self::Failed => Icon::Failed,
			Self::Stopped => Icon::Stopped,
			Self::Unknown => Icon::Ask,
		}
	}

	const fn color(self, ctx: &UiContext) -> Color {
		match self {
			Self::Running => ctx.theme.info,
			Self::Active => ctx.theme.accent,
			Self::Idle => ctx.theme.muted,
			Self::Parked => ctx.theme.warn,
			Self::Completed => ctx.theme.ok,
			Self::Failed => ctx.theme.err,
			Self::Stopped => ctx.theme.muted,
			Self::Unknown => ctx.theme.muted,
		}
	}
}

/// A one-line, icon-only lifecycle state indicator.
///
/// Known states select a semantic theme color and a charset-aware catalog
/// icon. `running` reuses the shared spinner clock and only schedules frames
/// while it is actually painted.
pub struct State {
	props: Props,
	slot:  Slot,
}

impl State {
	/// Creates an indicator whose missing status safely resolves to `unknown`.
	pub fn new() -> Self {
		Self { props: Props::new(), slot: next_slot() }
	}

	/// Creates an indicator with `status` normalized case-insensitively.
	pub fn status(status: impl Into<PropValue>) -> Self {
		Self::new().with(Prop::Status, status)
	}

	/// Sets one indicator property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	fn semantic(&self) -> SemanticState {
		SemanticState::from_props(&self.props)
	}

	fn glyph<'a>(&self, ctx: &'a UiContext) -> &'a str {
		let state = self.semantic();
		ctx.charset.icon(state.icon())
	}

	fn style(&self, ctx: &UiContext) -> Style {
		let style = self.props.style(&ctx.theme);
		if matches!(style.foreground_color(), Color::Default) {
			style.fg(self.semantic().color(ctx))
		} else {
			style
		}
	}
}

impl Default for State {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for State {
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
		let width = if self.semantic() == SemanticState::Running {
			cell_width(ctx.charset.spinner().at(Default::default()))
		} else {
			cell_width(self.glyph(ctx))
		};
		(width, width)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		1
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.y >= pc.clip || rect.width == 0 || rect.height == 0 {
			return;
		}
		let state = self.semantic();
		let (glyph, wake) = if state == SemanticState::Running {
			let frames = pc.ctx.charset.spinner();
			(frames.at(pc.now), Some(frames.next_change(pc.now)))
		} else {
			(self.glyph(pc.ctx), None)
		};
		if cell_width(glyph) > rect.width {
			return;
		}
		pc.frame.put(rect.x, rect.y, glyph, self.style(pc.ctx));
		if let Some(deadline) = wake {
			pc.wake(self.slot, deadline);
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;
	use crate::{Charset, test_support::frame_row_text, ui::Ui};

	const STATUSES: [(&str, &str); 8] = [
		("running", "running"),
		("active", "enabled"),
		("idle", "idle"),
		("parked", "parked"),
		("completed", "completed"),
		("failed", "failed"),
		("stopped", "stopped"),
		("unknown", "ask"),
	];

	#[test]
	fn every_status_degrades_through_each_charset() {
		for charset in [Charset::Ascii, Charset::Unicode, Charset::NerdFont] {
			for (status, icon) in STATUSES {
				let ctx = UiContext { charset, ..UiContext::default() };
				let expected = if status == "running" {
					charset.spinner().at(Duration::ZERO)
				} else {
					charset
						.icon_named(icon)
						.expect("state icon belongs to the catalog")
				};
				let ui = Ui::from_root(State::status(status), 8, ctx);
				assert_eq!(frame_row_text(ui.frame(), 0), expected, "{charset:?} {status}");
			}
		}
	}

	#[test]
	fn statuses_use_deliberate_semantic_colors() {
		let ctx = UiContext::default();
		let expected = [
			("running", ctx.theme.info),
			("active", ctx.theme.accent),
			("idle", ctx.theme.muted),
			("parked", ctx.theme.warn),
			("completed", ctx.theme.ok),
			("failed", ctx.theme.err),
			("stopped", ctx.theme.muted),
			("unknown", ctx.theme.muted),
		];
		for (status, color) in expected {
			let ui = Ui::from_root(State::status(status), 8, ctx.clone());
			assert_eq!(ui.frame().cell(0, 0).style.foreground_color(), color, "{status}");
		}
	}

	#[test]
	fn normalization_and_unrecognized_values_are_safe() {
		let ctx = UiContext::default();
		let normalized = Ui::from_root(State::status("  CoMpLeTeD  "), 8, ctx.clone());
		let unknown = Ui::from_root(State::status("future-state"), 8, ctx.clone());
		assert_eq!(
			frame_row_text(normalized.frame(), 0),
			ctx.charset.icon_named("completed").unwrap()
		);
		assert_eq!(frame_row_text(unknown.frame(), 0), ctx.charset.icon_named("ask").unwrap());
		assert_eq!(unknown.frame().cell(0, 0).style.foreground_color(), ctx.theme.muted);
	}

	#[test]
	fn running_wakes_only_when_visible_and_static_states_never_wake() {
		let visible = Ui::from_root(State::status("running"), 1, UiContext::default());
		assert_eq!(visible.next_wake(), Some(Duration::from_millis(80)));

		let hidden = Ui::from_root(State::status("running"), 0, UiContext::default());
		assert_eq!(hidden.next_wake(), None);

		let parked = Ui::from_root(State::status("parked"), 8, UiContext::default());
		assert_eq!(parked.next_wake(), None);
	}
}
