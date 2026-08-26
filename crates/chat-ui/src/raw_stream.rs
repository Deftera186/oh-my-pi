//! Sanitized provider-stream viewer with tail-follow and drop accounting.

use omp_core::{Str, sf};
use omp_tui::{Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, dom};

use crate::{OverlayPanel, panel_divider};

/// One inference-owned, already-redacted stream frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawFrame {
	/// Monotonic ring sequence.
	pub sequence: u64,
	/// Session binding, when known.
	pub session:  Option<Str>,
	/// Provider event name or frame category.
	pub event:    Str,
	/// Sanitized frame payload.
	pub payload:  Str,
}

/// Ring summary projected by inference.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StreamSummary {
	/// Frames currently retained.
	pub retained:         usize,
	/// Frames evicted from the bounded ring.
	pub evicted:          u64,
	/// Subscriber deliveries dropped due to backpressure.
	pub subscriber_drops: u64,
}

/// Raw-stream viewer interaction result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawStreamEvent {
	/// Viewer remains open.
	Consumed,
	/// Close the overlay.
	Close,
	/// Copy the complete sanitized retained stream.
	Copy(String),
}

/// Pretty SSE/JSON viewer over inference-owned snapshots and subscriptions.
pub struct RawStreamViewer {
	frames:  Vec<RawFrame>,
	summary: StreamSummary,
	follow:  bool,
	pretty:  bool,
	ui:      Ui,
	ctx:     UiContext,
	options: OverlayOptions,
	width:   u16,
	rows:    u16,
}

impl RawStreamViewer {
	/// Opens a viewer on a bounded ring snapshot.
	pub fn open(frames: Vec<RawFrame>, summary: StreamSummary, ctx: &UiContext) -> Self {
		let mut viewer = Self {
			frames,
			summary,
			follow: true,
			pretty: true,
			ui: Ui::from_root(dom! { <text/> }, 1, ctx.clone()),
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(88))
				.z(20),
			width: 88,
			rows: 18,
		};
		viewer.rebuild();
		viewer.scroll_to_tail();
		viewer
	}

	/// Appends one subscribed frame and preserves explicit navigation away from
	/// tail.
	pub fn push(&mut self, frame: RawFrame, summary: StreamSummary) {
		self.frames.push(frame);
		self.summary = summary;
		self.refresh_content();
		if self.follow {
			self.scroll_to_tail();
		}
	}

	/// Replaces the viewer from a fresh bounded snapshot after ring eviction.
	pub fn replace(&mut self, frames: Vec<RawFrame>, summary: StreamSummary) {
		self.frames = frames;
		self.summary = summary;
		self.refresh_content();
		if self.follow {
			self.scroll_to_tail();
		}
	}

	/// Routes navigation, tail-follow, pretty-print, and clipboard keys.
	pub fn handle_key(&mut self, key: Key) -> RawStreamEvent {
		match key {
			Key::Esc => return RawStreamEvent::Close,
			Key::Up | Key::PageUp | Key::Home => {
				self.follow = false;
				let _ = self.ui.handle_key(key);
			},
			Key::Down | Key::PageDown => {
				let _ = self.ui.handle_key(key);
			},
			Key::End => {
				self.follow = true;
				self.scroll_to_tail();
			},
			Key::Char('f') => {
				self.follow = !self.follow;
				if self.follow {
					self.scroll_to_tail();
				}
			},
			Key::Char('p') => {
				self.pretty = !self.pretty;
				self.rebuild();
				if self.follow {
					self.scroll_to_tail();
				}
			},
			Key::Copy | Key::Ctrl('c') => return RawStreamEvent::Copy(self.complete_text()),
			_ => return RawStreamEvent::Consumed,
		}
		RawStreamEvent::Consumed
	}

	/// Routes wheel navigation; any upward navigation disables tail-follow.
	pub fn handle_mouse(&mut self, kind: Mouse) -> RawStreamEvent {
		match kind {
			Mouse::WheelUp => {
				self.follow = false;
				for _ in 0..3 {
					let _ = self.ui.handle_key(Key::Up);
				}
			},
			Mouse::WheelDown => {
				for _ in 0..3 {
					let _ = self.ui.handle_key(Key::Down);
				}
			},
			_ => return RawStreamEvent::Consumed,
		}
		RawStreamEvent::Consumed
	}

	/// Returns the responsive overlay layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(4).max(36);
		let rows = viewport.height.saturating_sub(8).max(6);
		if width != self.width || rows != self.rows {
			self.width = width;
			self.rows = rows;
			self.rebuild();
		}
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn complete_text(&self) -> String {
		let mut output = String::new();
		for frame in &self.frames {
			use std::fmt::Write as _;
			let _ = writeln!(output, "#{} {}", frame.sequence, frame.event);
			if self.pretty {
				output.push_str(&pretty_payload(&frame.payload));
			} else {
				output.push_str(frame.payload.as_str());
			}
			if !output.ends_with('\n') {
				output.push('\n');
			}
		}
		output
	}

	fn scroll_to_tail(&mut self) {
		let _ = self.ui.handle_key(Key::End);
	}

	fn refresh_content(&mut self) {
		let latest = self
			.frames
			.last()
			.map_or_else(|| sf!("No frames"), |frame| sf!("#{} · {}", frame.sequence, frame.event));
		self.ui.set_text("raw-stream-title", latest.as_str());
		self.ui.set_text("raw-stream-content", self.complete_text());
		self
			.ui
			.set_text("raw-stream-summary", self.summary_text().as_str());
	}

	fn summary_text(&self) -> Str {
		sf!(
			"{} retained · {} evicted · {} subscriber drops · {}",
			self.summary.retained,
			self.summary.evicted,
			self.summary.subscriber_drops,
			if self.follow {
				"following tail"
			} else {
				"paused"
			}
		)
	}

	fn rebuild(&mut self) {
		let title = self
			.frames
			.last()
			.map_or_else(|| sf!("No frames"), |frame| sf!("#{} · {}", frame.sequence, frame.event));
		let payload = self.complete_text();
		let summary = self.summary_text();
		let height = self.rows;
		self.ui = Ui::from_root(
			OverlayPanel::new("Raw provider stream").child(dom! {
				<col gap=1>
					<text id="raw-stream-title" bold truncate>{title}</text>
					<text id="raw-stream-summary" dim truncate>{summary}</text>
					<scroll id="raw-stream-scroll" h={height}><text id="raw-stream-content" wrap>{payload}</text></scroll>
					{panel_divider()}
					<text dim truncate>{"↑/↓ navigate · F follow · P pretty · Ctrl+C copy · Esc close"}</text>
				</col>
			}),
			self.width,
			self.ctx.clone(),
		);
		let _ = self.ui.focus_id("raw-stream-scroll");
	}
}

fn pretty_payload(payload: &str) -> String {
	if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
		return serde_json::to_string_pretty(&value).unwrap_or_else(|_| payload.to_owned());
	}
	let mut output = String::with_capacity(payload.len());
	for line in payload.lines() {
		if let Some(data) = line.strip_prefix("data:") {
			output.push_str("data:");
			let data = data.trim();
			if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
				output.push('\n');
				output
					.push_str(&serde_json::to_string_pretty(&value).unwrap_or_else(|_| data.to_owned()));
			} else {
				output.push(' ');
				output.push_str(data);
			}
		} else {
			output.push_str(line);
		}
		output.push('\n');
	}
	output
}
#[cfg(test)]
mod tests {
	use super::*;

	fn frame(sequence: u64, payload: &'static str) -> RawFrame {
		RawFrame { sequence, session: None, event: sf!("delta"), payload: Str::new_static(payload) }
	}

	#[test]
	fn scrolling_pauses_follow_and_copy_returns_complete_stream() {
		let ctx = UiContext::default();
		let mut viewer = RawStreamViewer::open(
			vec![frame(1, "{\"one\":1}"), frame(2, "{\"two\":2}")],
			StreamSummary { retained: 2, ..StreamSummary::default() },
			&ctx,
		);
		assert!(viewer.follow);
		assert_eq!(viewer.handle_key(Key::PageUp), RawStreamEvent::Consumed);
		assert!(!viewer.follow);
		viewer.push(frame(3, "{\"three\":3}"), StreamSummary {
			retained: 3,
			..StreamSummary::default()
		});
		let RawStreamEvent::Copy(copied) = viewer.handle_key(Key::Ctrl('c')) else {
			panic!("copy action");
		};
		for sequence in [1, 2, 3] {
			assert!(copied.contains(&format!("#{sequence} delta")));
		}
		assert_eq!(viewer.handle_key(Key::End), RawStreamEvent::Consumed);
		assert!(viewer.follow);
	}
}
