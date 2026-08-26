use std::time::Duration;

use omp_core::{IntoStr, Str};

use crate::{
	Icon,
	component::{Cached, Component, IntoChildren, PaintCtx, Slot, next_slot},
	components::layout::{stack_height, stack_measure, stack_place},
	context::UiContext,
	frame::{Color, Rect, Style},
	markup::{Align, Border, VAlign},
	props::{Prop, PropValue, Props},
	rich::cell_width,
};

/// Public state vocabulary for a tool call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum ToolState {
	/// The tool is currently running/streaming output.
	#[default]
	Streaming,
	/// The tool completed successfully.
	Success,
	/// The tool failed.
	Failure,
}

/// A themed card component representing one tool call across its lifecycle.
///
/// Parents must not paint or hit-test a card granted zero rows.
pub struct ToolCard {
	props:         Props,
	slot:          Slot,
	state:         ToolState,
	name:          Str,
	intent:        Str,
	activity:      Str,
	badge:         Str,
	folded:        bool,
	flush:         bool,
	last_paint_at: Duration,
	finalized_at:  Option<Duration>,
	children:      Vec<Cached>,
}

impl ToolCard {
	/// Creates a new tool card in the streaming state, unfolded by default.
	pub fn new() -> Self {
		Self {
			props:         Props::new(),
			slot:          next_slot(),
			state:         ToolState::Streaming,
			name:          Str::default(),
			intent:        Str::default(),
			activity:      Str::default(),
			badge:         Str::default(),
			folded:        false,
			flush:         false,
			last_paint_at: Duration::ZERO,
			finalized_at:  None,
			children:      Vec::new(),
		}
	}

	/// Sets one generic property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	/// In-place update: tool name.
	pub fn set_name(&mut self, name: impl IntoStr) -> bool {
		let name = name.into_str();
		if self.name == name {
			return false;
		}
		self.name = name;
		true
	}

	/// Sets the tool name (e.g. `read`).
	pub fn name(mut self, name: impl IntoStr) -> Self {
		self.set_name(name);
		self
	}

	/// In-place update: tool state.
	pub fn set_state(&mut self, state: ToolState) -> bool {
		if self.state == state {
			return false;
		}
		self.state = state;
		self.finalized_at = match state {
			ToolState::Streaming => None,
			ToolState::Success | ToolState::Failure => Some(self.last_paint_at),
		};
		true
	}

	/// Sets the tool state.
	pub fn state(mut self, state: ToolState) -> Self {
		self.set_state(state);
		self
	}

	/// In-place update: intent/summary text.
	pub fn set_intent(&mut self, intent: impl IntoStr) -> bool {
		let intent = intent.into_str();
		if self.intent == intent {
			return false;
		}
		self.intent = intent;
		true
	}

	/// Sets the intent or summary text.
	pub fn intent(mut self, intent: impl IntoStr) -> Self {
		self.set_intent(intent);
		self
	}

	/// In-place update: the one-line semantic progress shown when collapsed.
	///
	/// An empty activity restores the stable intent as the collapsed fallback.
	pub fn set_activity(&mut self, activity: impl IntoStr) -> bool {
		let activity = activity.into_str();
		if self.activity == activity {
			return false;
		}
		self.activity = activity;
		true
	}

	/// Sets the one-line semantic progress shown when collapsed.
	///
	/// An empty activity restores the stable intent as the collapsed fallback.
	pub fn activity(mut self, activity: impl IntoStr) -> Self {
		self.set_activity(activity);
		self
	}

	/// In-place update: badge text.
	pub fn set_badge(&mut self, badge: impl IntoStr) -> bool {
		let badge = badge.into_str();
		if self.badge == badge {
			return false;
		}
		self.badge = badge;
		true
	}

	/// Sets the right-aligned badge text (e.g. elapsed time).
	pub fn badge(mut self, badge: impl IntoStr) -> Self {
		self.set_badge(badge);
		self
	}

	/// In-place update: fold state.
	pub fn set_folded(&mut self, folded: bool) -> bool {
		if self.folded == folded {
			return false;
		}
		self.folded = folded;
		for child in &mut self.children {
			child.visible = !folded || self.flush;
		}
		true
	}

	/// Sets whether the card is folded (hides children).
	pub fn folded(mut self, folded: bool) -> Self {
		self.set_folded(folded);
		self
	}

	/// In-place update: chrome suppression for self-presenting views.
	///
	/// A flush card draws no header, rail, or footer: its children paint at
	/// the card's full rect and own the entire presentation, including any
	/// state or progress indication. Fold state is ignored while flush.
	pub fn set_flush(&mut self, flush: bool) -> bool {
		if self.flush == flush {
			return false;
		}
		self.flush = flush;
		let visible = !self.folded || flush;
		for child in &mut self.children {
			child.visible = visible;
		}
		true
	}

	/// Sets chrome suppression for self-presenting views.
	pub fn flush(mut self, flush: bool) -> Self {
		self.set_flush(flush);
		self
	}

	/// Replaces the card body children.
	pub fn replace_body(&mut self, children: impl IntoChildren) -> bool {
		self.children.clear();
		children.extend_children(&mut self.children);
		let visible = !self.folded || self.flush;
		for child in &mut self.children {
			child.visible = visible;
		}
		true
	}

	/// Appends child components to the card's body.
	pub fn child(mut self, children: impl IntoChildren) -> Self {
		children.extend_children(&mut self.children);
		let visible = !self.folded || self.flush;
		for child in &mut self.children {
			child.visible = visible;
		}
		self
	}

	const fn header_color(&self, ctx: &UiContext) -> Color {
		match self.state {
			ToolState::Streaming => ctx.theme.accent,
			ToolState::Success => ctx.theme.ok,
			ToolState::Failure => ctx.theme.err,
		}
	}

	fn collapsed_indicator(&self, pc: &mut PaintCtx<'_>) -> &'static str {
		const STEP: Duration = Duration::from_millis(120);
		const END: Duration = Duration::from_millis(240);

		let pulse = pc.ctx.charset.pulse();
		let Some(start) = self.finalized_at else {
			pc.wake(self.slot, pulse.next_change(pc.now));
			return pulse.at(pc.now);
		};
		let elapsed = pc.now.saturating_sub(start);
		if elapsed < STEP {
			pc.wake(self.slot, start.saturating_add(STEP));
			pulse.at(END)
		} else if elapsed < END {
			pc.wake(self.slot, start.saturating_add(END));
			pulse.at(STEP)
		} else {
			pulse.at(Duration::ZERO)
		}
	}
}

impl Default for ToolCard {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for ToolCard {
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
		if self.flush && !self.children.is_empty() {
			return stack_measure(ctx, &mut self.children);
		}
		let name_len = cell_width(&self.name);
		let intent_len = cell_width(&self.intent).max(cell_width(&self.activity));
		let badge_len = cell_width(&self.badge);
		let header_min = 5 + name_len + intent_len + badge_len;
		let header_nat = header_min.saturating_add(if badge_len > 0 { 2 } else { 0 });

		if self.folded || self.children.is_empty() {
			(header_min, header_nat.max(30))
		} else {
			let (child_min, child_nat) = stack_measure(ctx, &mut self.children);
			(
				header_min.max(child_min.saturating_add(2)),
				header_nat.max(child_nat.saturating_add(2)).max(30),
			)
		}
	}

	fn height(&mut self, ctx: &UiContext, width: u16) -> u16 {
		if self.flush && !self.children.is_empty() {
			return stack_height(ctx, &mut self.children, width, 0).max(1);
		}
		if self.folded || self.children.is_empty() {
			1
		} else {
			let child_width = width.saturating_sub(2);
			let child_h = stack_height(ctx, &mut self.children, child_width, 0);
			1 + child_h + 1
		}
	}

	fn place(&mut self, ctx: &UiContext, content: Rect) {
		if self.flush && !self.children.is_empty() {
			stack_place(ctx, &mut self.children, content, 0, Some(VAlign::Start), Align::Start);
			return;
		}
		if !self.folded && !self.children.is_empty() {
			let child_rect = Rect::new(
				content.x.saturating_add(2),
				content.y.saturating_add(1),
				content.width.saturating_sub(2),
				content.height.saturating_sub(2),
			);
			stack_place(ctx, &mut self.children, child_rect, 0, Some(VAlign::Start), Align::Start);
		}
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.y >= pc.clip || rect.width == 0 || rect.height == 0 {
			return;
		}
		self.last_paint_at = pc.now;
		let granted_height = rect.height;
		if self.flush && !self.children.is_empty() {
			let outer_clip = pc.clip;
			pc.clip = pc.clip.min(rect.y.saturating_add(granted_height));
			for child in self.children.iter_mut().filter(|child| child.visible) {
				child.paint(pc);
			}
			pc.clip = outer_clip;
			return;
		}

		let header_color = self.header_color(pc.ctx);
		let header_style = Style::new().fg(header_color);
		let normal_style = Style::new().fg(pc.ctx.theme.fg);
		let muted_style = Style::new().fg(pc.ctx.theme.muted);

		let mut x = rect.x;
		let y = rect.y;

		let leading = if granted_height >= 3 {
			pc.ctx
				.charset
				.expander(!self.folded && !self.children.is_empty())
		} else {
			"  "
		};
		x = pc.frame.put(x, y, leading, header_style);

		if granted_height == 1 {
			let indicator = self.collapsed_indicator(pc);
			x = pc.frame.put(x, y, indicator, header_style);
		} else {
			match self.state {
				ToolState::Streaming => {
					let frames = pc.ctx.charset.spinner();
					x = pc.frame.put(x, y, frames.at(pc.now), header_style);
					pc.wake(self.slot, frames.next_change(pc.now));
				},
				ToolState::Success => {
					x = pc.frame.put(x, y, pc.ctx.charset.check(), header_style);
				},
				ToolState::Failure => {
					x = pc
						.frame
						.put(x, y, pc.ctx.charset.icon(Icon::Error), header_style);
				},
			}
		}
		x = pc.frame.put(x, y, " ", header_style);

		if !self.name.is_empty() {
			x = pc.frame.put(x, y, &self.name, header_style.bold());
			x = pc.frame.put(x, y, " ", normal_style);
		}

		let summary = if granted_height == 1 && !self.activity.is_empty() {
			&self.activity
		} else {
			&self.intent
		};
		if !summary.is_empty() {
			let badge_width = cell_width(&self.badge);
			let mut available = rect.x.saturating_add(rect.width).saturating_sub(x);
			if !self.badge.is_empty() {
				available = available.saturating_sub(badge_width + 1);
			}

			x = pc.frame.put_clipped(x, y, available, summary, normal_style);
		}

		if !self.badge.is_empty() {
			let badge_start = rect
				.x
				.saturating_add(rect.width)
				.saturating_sub(cell_width(&self.badge));
			let badge_x = x.max(badge_start);
			pc.frame.put(badge_x, y, &self.badge, muted_style);
		}

		if granted_height == 1 {
			return;
		}

		let bottom_y = y.saturating_add(granted_height.saturating_sub(1));
		if granted_height >= 3 && !self.folded && !self.children.is_empty() {
			let outer_clip = pc.clip;
			pc.clip = pc.clip.min(bottom_y);
			for child in self.children.iter_mut().filter(|child| child.visible) {
				child.paint(pc);
			}
			pc.clip = outer_clip;
		}

		let (_, last, rail) = pc.ctx.charset.guides(Border::Round);
		for row in 0..granted_height.saturating_sub(2) {
			let cy = y + 1 + row;
			if cy < pc.clip {
				pc.frame.put(rect.x, cy, rail, header_style);
			}
		}

		if bottom_y < pc.clip {
			let mut bx = pc.frame.put(rect.x, bottom_y, last, header_style);
			let width = rect.width.saturating_sub(2);
			let mut buf = [0; 4];
			let rule = pc.ctx.charset.rule().encode_utf8(&mut buf);
			for _ in 0..width {
				bx = pc.frame.put(bx, bottom_y, rule, header_style);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{components::TextLeaf, test_support::frame_row_text, ui::Ui};

	#[test]
	fn formats_streaming_state_and_badge() {
		let card = ToolCard::new()
			.name("read")
			.intent("src/lib.rs")
			.badge("12ms")
			.state(ToolState::Streaming);

		let ui = Ui::from_root(card, 40, UiContext::default());
		assert!(frame_row_text(ui.frame(), 0).contains("read src/lib.rs"));
		assert!(frame_row_text(ui.frame(), 0).contains("12ms"));
	}

	#[test]
	fn formats_success_and_failure_states() {
		let ui = Ui::from_root(
			ToolCard::new()
				.name("grep")
				.intent("foo")
				.state(ToolState::Success),
			20,
			UiContext::default(),
		);
		let row_success = frame_row_text(ui.frame(), 0);
		assert!(row_success.contains("grep foo"));

		let ui_fail = Ui::from_root(
			ToolCard::new()
				.name("fail")
				.intent("bar")
				.state(ToolState::Failure),
			20,
			UiContext::default(),
		);
		let row_fail = frame_row_text(ui_fail.frame(), 0);
		assert!(row_fail.contains("fail bar"));
	}

	#[test]
	fn truncates_intent_narrow_width_without_panic() {
		let card = ToolCard::new()
			.name("long")
			.intent("this is a very long intent with 🚀 emoji")
			.badge("ok");

		let ui = Ui::from_root(card, 20, UiContext::default());
		let row = frame_row_text(ui.frame(), 0);
		assert!(row.contains("long"));
		assert!(row.contains("ok"));
		// check it has graphemes
		assert!(row.chars().count() > 10);
	}

	#[test]
	fn renders_open_children_with_rails() {
		let card = ToolCard::new()
			.name("bash")
			.child(TextLeaf::new().text("echo ok"))
			.state(ToolState::Success);

		let ui = Ui::from_root(card, 20, UiContext::default());
		let row_0 = frame_row_text(ui.frame(), 0);
		assert!(row_0.contains("bash"));
		assert_eq!(frame_row_text(ui.frame(), 1), "│ echo ok");
		assert_eq!(frame_row_text(ui.frame(), 2), "╰───────────────────");
	}
	#[test]
	fn flush_card_paints_children_without_chrome() {
		let card = ToolCard::new()
			.name("read")
			.intent("src/lib.rs")
			.flush(true)
			.child(TextLeaf::new().text("✔ read src/lib.rs"))
			.state(ToolState::Success);

		let ui = Ui::from_root(card, 30, UiContext::default());
		assert_eq!(ui.frame().size().height, 1);
		let row = frame_row_text(ui.frame(), 0);
		assert_eq!(row, "✔ read src/lib.rs");
		assert!(!row.contains('│'));
	}

	#[test]
	fn mutable_transitions_and_narrow_rendering() {
		let card = ToolCard::new()
			.with(Prop::Id, "t1")
			.name("edit")
			.intent("src/file.txt")
			.state(ToolState::Streaming);

		let mut ui = Ui::from_root(card, 15, UiContext::default());
		assert!(frame_row_text(ui.frame(), 0).contains("edit src/"));

		let changed = ui.update_component::<ToolCard>("t1", |card| {
			let mut dirty = false;
			dirty |= card.set_state(ToolState::Success);
			dirty |= card.set_badge("1s");
			dirty |= card.replace_body(TextLeaf::new().text("done"));
			dirty
		});
		assert!(changed);

		let row_0 = frame_row_text(ui.frame(), 0);
		assert!(row_0.contains("edit"));
		assert!(row_0.contains("1s"));
		assert_eq!(frame_row_text(ui.frame(), 1), "│ done");

		let changed = ui.update_component::<ToolCard>("t1", |card| {
			let mut dirty = false;
			dirty |= card.set_state(ToolState::Failure);
			dirty |= card.set_folded(true);
			dirty
		});
		assert!(changed);

		let row_0_fail = frame_row_text(ui.frame(), 0);
		assert!(row_0_fail.contains("edit"));
		// Verify the component folded by checking that the row is cleared, independent
		// of the monotonic frame height.
		assert_eq!(frame_row_text(ui.frame(), 1), "");
	}
	fn gallery_card(height: u16) -> ToolCard {
		ToolCard::new()
			.with(Prop::H, height)
			.name("read")
			.intent("src/lib.rs")
			.badge("2ms")
			.child(TextLeaf::new().text("body row"))
	}
	fn text_column(row: &str, text: &str) -> u16 {
		let byte = row.find(text).expect("text is present");
		cell_width(&row[..byte])
	}

	#[test]
	fn granted_heights_preserve_header_identity() {
		let tall = Ui::from_root(gallery_card(3), 32, UiContext::default());
		let bridge = Ui::from_root(gallery_card(2), 32, UiContext::default());
		let collapsed = Ui::from_root(gallery_card(1), 32, UiContext::default());
		let tall_row = frame_row_text(tall.frame(), 0);
		let bridge_row = frame_row_text(bridge.frame(), 0);
		let collapsed_row = frame_row_text(collapsed.frame(), 0);

		assert_eq!(tall_row, "▾ ⠋ read src/lib.rs          2ms");
		assert_eq!(bridge_row, "  ⠋ read src/lib.rs          2ms");
		assert_eq!(collapsed_row, "  · read src/lib.rs          2ms");
		assert_eq!(text_column(&tall_row, "read"), 4);
		assert_eq!(text_column(&bridge_row, "read"), 4);
		assert_eq!(text_column(&collapsed_row, "read"), 4);
	}

	#[test]
	fn two_rows_are_header_and_closing_rail_only() {
		let ui = Ui::from_root(gallery_card(2), 32, UiContext::default());

		assert_eq!(frame_row_text(ui.frame(), 0), "  ⠋ read src/lib.rs          2ms");
		assert_eq!(frame_row_text(ui.frame(), 1), format!("╰{}", "─".repeat(31)));
		assert!(!frame_row_text(ui.frame(), 1).contains("body row"));
	}

	#[test]
	fn finalized_one_row_pulse_attenuates_on_shared_clock() {
		let card = ToolCard::new()
			.with(Prop::Id, "pulse")
			.with(Prop::H, 1_u16)
			.name("read")
			.activity("active");
		let mut ui = Ui::from_root(card, 20, UiContext::default());
		ui.tick(Duration::from_millis(240));
		assert_eq!(frame_row_text(ui.frame(), 0), "  ● read active");

		assert!(
			ui.update_component::<ToolCard>("pulse", |card| { card.set_state(ToolState::Success) })
		);
		assert_eq!(frame_row_text(ui.frame(), 0), "  ● read active");
		ui.tick(Duration::from_millis(360));
		assert_eq!(frame_row_text(ui.frame(), 0), "  • read active");
		ui.tick(Duration::from_millis(480));
		assert_eq!(frame_row_text(ui.frame(), 0), "  · read active");
	}

	#[test]
	fn streaming_body_growth_is_clipped_to_granted_height() {
		let card = ToolCard::new()
			.with(Prop::Id, "growing")
			.with(Prop::H, 3_u16)
			.name("bash")
			.intent("running")
			.child(TextLeaf::new().text("one"));
		let mut ui = Ui::from_root(card, 24, UiContext::default());

		assert!(ui.update_component::<ToolCard>("growing", |card| {
			card.replace_body(TextLeaf::new().text("one\ntwo\nthree\nfour"))
		}));
		assert_eq!(ui.frame().size().height, 3);
		assert_eq!(frame_row_text(ui.frame(), 1), "│ one");
		assert_eq!(frame_row_text(ui.frame(), 2), format!("╰{}", "─".repeat(23)));
	}

	#[test]
	fn one_row_shows_pulse_name_activity_and_badge_without_chrome() {
		let card = ToolCard::new()
			.with(Prop::H, 1_u16)
			.name("read")
			.intent("stable intent")
			.activity("reading lines 1-8")
			.badge("2ms")
			.child(TextLeaf::new().text("hidden body"));
		let ui = Ui::from_root(card, 32, UiContext::default());
		let row = frame_row_text(ui.frame(), 0);

		assert_eq!(row, "  · read reading lines 1-8   2ms");
	}
}
