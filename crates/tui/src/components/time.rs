//! Compact duration and live relative-time presentation.

use std::{fmt::Write as _, time::Duration};

use crate::{
	component::{Component, PaintCtx, Slot, next_slot},
	context::UiContext,
	frame::Rect,
	props::{Prop, PropValue, Props},
};

const SECOND_MS: u64 = 1_000;
const MINUTE_MS: u64 = 60 * SECOND_MS;
const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
	Duration,
	Relative,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelativeUnit {
	Now,
	Second,
	Minute,
	Hour,
	Day,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormatKey {
	Duration(u64),
	Relative(RelativeUnit, u64),
}

/// A compact duration or live relative age backing the `<time>` markup tag.
pub struct Time {
	props:  Props,
	slot:   Slot,
	text:   String,
	key:    Option<FormatKey>,
	anchor: Option<(u64, Duration)>,
}

impl Time {
	/// Creates an empty time display; absent `ms` is treated as zero.
	pub fn new() -> Self {
		Self {
			props:  Props::new(),
			slot:   next_slot(),
			text:   String::with_capacity(32),
			key:    None,
			anchor: None,
		}
	}

	/// Sets one time property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	fn mode(&self) -> Mode {
		if self
			.props
			.str_of(Prop::Kind)
			.is_some_and(|kind| kind == "relative")
		{
			Mode::Relative
		} else {
			Mode::Duration
		}
	}

	fn source_ms(&self) -> u64 {
		self.props.ms().unwrap_or(0)
	}

	fn age_at(&mut self, now: Duration) -> u64 {
		let source = self.source_ms();
		let (base, started) = match self.anchor {
			Some(anchor) if anchor.0 == source => anchor,
			_ => {
				self.anchor = Some((source, now));
				(source, now)
			},
		};
		let elapsed = u64::try_from(now.saturating_sub(started).as_millis()).unwrap_or(u64::MAX);
		base.saturating_add(elapsed)
	}

	fn sync(&mut self, now: Duration) -> Option<u64> {
		let (key, next) = match self.mode() {
			Mode::Duration => {
				self.anchor = None;
				(FormatKey::Duration(self.source_ms()), None)
			},
			Mode::Relative => {
				let age = self.age_at(now);
				let (unit, value, period) = relative_parts(age);
				let delta = period - age % period;
				let next = age.checked_add(delta).map(|_| delta);
				(FormatKey::Relative(unit, value), next)
			},
		};
		if self.key != Some(key) {
			self.text.clear();
			match key {
				FormatKey::Duration(ms) => write_duration(&mut self.text, ms),
				FormatKey::Relative(RelativeUnit::Now, _) => self.text.push_str("now"),
				FormatKey::Relative(RelativeUnit::Second, value) => {
					write!(self.text, "{value}s ago").expect("writing to String cannot fail");
				},
				FormatKey::Relative(RelativeUnit::Minute, value) => {
					write!(self.text, "{value}m ago").expect("writing to String cannot fail");
				},
				FormatKey::Relative(RelativeUnit::Hour, value) => {
					write!(self.text, "{value}h ago").expect("writing to String cannot fail");
				},
				FormatKey::Relative(RelativeUnit::Day, value) => {
					write!(self.text, "{value}d ago").expect("writing to String cannot fail");
				},
			}
			self.key = Some(key);
		}
		next
	}
}

impl Default for Time {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for Time {
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
		self.sync(ctx.now);
		let width = u16::try_from(xutf::width_str(&self.text)).unwrap_or(u16::MAX);
		(width, width)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		1
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		let next = self.sync(pc.now);
		if rect.y >= pc.clip || rect.width == 0 || rect.height == 0 {
			return;
		}
		if let Some(delta_ms) = next
			&& let Some(at) = pc.now.checked_add(Duration::from_millis(delta_ms))
		{
			pc.wake(self.slot, at);
		}
		pc.frame
			.put(rect.x, rect.y, &self.text, self.props.style(&pc.ctx.theme));
	}
}

fn relative_parts(age: u64) -> (RelativeUnit, u64, u64) {
	if age < SECOND_MS {
		(RelativeUnit::Now, 0, SECOND_MS)
	} else if age < MINUTE_MS {
		(RelativeUnit::Second, age / SECOND_MS, SECOND_MS)
	} else if age < HOUR_MS {
		(RelativeUnit::Minute, age / MINUTE_MS, MINUTE_MS)
	} else if age < DAY_MS {
		(RelativeUnit::Hour, age / HOUR_MS, HOUR_MS)
	} else {
		(RelativeUnit::Day, age / DAY_MS, DAY_MS)
	}
}

fn write_duration(out: &mut String, ms: u64) {
	if ms < SECOND_MS {
		write!(out, "{ms}ms").expect("writing to String cannot fail");
	} else if ms < MINUTE_MS {
		let tenths = ms / 100;
		write!(out, "{}.{}s", tenths / 10, tenths % 10).expect("writing to String cannot fail");
	} else if ms < HOUR_MS {
		let seconds = ms / SECOND_MS;
		write!(out, "{}m{:02}s", seconds / 60, seconds % 60).expect("writing to String cannot fail");
	} else {
		let minutes = ms / MINUTE_MS;
		write!(out, "{}h{:02}m", minutes / 60, minutes % 60).expect("writing to String cannot fail");
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Frame, Size, component::Wake, test_support::frame_row_text};

	fn duration(ms: u64) -> Time {
		Time::new().with(Prop::Ms, ms).with(Prop::Kind, "duration")
	}

	fn relative(ms: u64) -> Time {
		Time::new().with(Prop::Ms, ms).with(Prop::Kind, "relative")
	}

	fn paint_at(time: &mut Time, now_ms: u64) -> (String, Vec<Wake>) {
		let mut ctx = UiContext::default();
		ctx.now = Duration::from_millis(now_ms);
		let mut frame = Frame::new(Size::new(32, 1));
		let mut hits = Vec::new();
		let mut wakes = Vec::new();
		let mut pc = PaintCtx::new(&mut frame, &ctx, &mut hits, &mut wakes);
		pc.now = ctx.now;
		time.paint(&mut pc, Rect::new(0, 0, 32, 1));
		(frame_row_text(&frame, 0), wakes)
	}

	#[test]
	fn duration_matches_compact_tui_boundaries() {
		for (ms, expected) in [
			(0, "0ms"),
			(999, "999ms"),
			(1_000, "1.0s"),
			(59_999, "59.9s"),
			(60_000, "1m00s"),
			(3_599_999, "59m59s"),
			(3_600_000, "1h00m"),
		] {
			assert_eq!(paint_at(&mut duration(ms), 8_000).0, expected);
		}
	}

	#[test]
	fn relative_boundaries_and_wakes_follow_visible_units() {
		let cases = [
			(0, "now", 1_000),
			(999, "now", 1),
			(1_000, "1s ago", 1_000),
			(59_999, "59s ago", 1),
			(60_000, "1m ago", 60_000),
			(3_599_999, "59m ago", 1),
			(3_600_000, "1h ago", 3_600_000),
			(86_399_999, "23h ago", 1),
			(86_400_000, "1d ago", 86_400_000),
		];
		for (age, expected, delta) in cases {
			let mut time = relative(age);
			let (text, wakes) = paint_at(&mut time, 500);
			assert_eq!(text, expected);
			assert_eq!(wakes, vec![Wake {
				slot:   time.slot,
				at:     Duration::from_millis(500 + delta),
				layout: false,
			}]);
		}
	}

	#[test]
	fn relative_age_advances_from_first_paint_and_saturates() {
		let mut time = relative(999);
		assert_eq!(paint_at(&mut time, 500).0, "now");
		let (text, wakes) = paint_at(&mut time, 501);
		assert_eq!(text, "1s ago");
		assert_eq!(wakes[0].at, Duration::from_millis(1_501));

		for age in [u64::MAX - 1, u64::MAX] {
			let mut saturated = relative(age);
			let (text, wakes) = paint_at(&mut saturated, 10);
			assert_eq!(text, format!("{}d ago", age / DAY_MS));
			assert!(wakes.is_empty());
			assert_eq!(paint_at(&mut saturated, u64::MAX).0, text);
		}
	}

	#[test]
	fn dimensions_and_cached_text_are_stable_between_boundaries() {
		let mut time = relative(1_234);
		let mut ctx = UiContext::default();
		ctx.now = Duration::from_millis(10);
		assert_eq!(time.measure(&ctx), (6, 6));
		assert_eq!(time.height(&ctx, 0), 1);
		let pointer = time.text.as_ptr();
		assert_eq!(paint_at(&mut time, 500).0, "1s ago");
		assert_eq!(paint_at(&mut time, 700).0, "1s ago");
		assert_eq!(time.text.as_ptr(), pointer);
	}
}
