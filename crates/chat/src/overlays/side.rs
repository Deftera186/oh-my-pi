//! Side-channel panel above the editor for `/btw` (pi `btw-panel.ts`,
//! `btw-controller.ts`): the question as a header with a status
//! indicator, the answer streamed in below it, `c` to copy, Esc to close
//! (aborting the side kernel when it is still answering). The composer
//! stays live underneath (`PanelAnchor::Side`).

use std::time::Duration;

use flume::Receiver;
use omp_core::{Str, StrMut};
use omp_tui::{Frame, Key, Prop, Size, Ui, UiContext, UiEvent, dom};

use super::{Panel, PanelAnchor, PanelCx, PanelEvent, services::SideEvent};

/// Streaming poll cadence while the side kernel is answering.
const POLL: Duration = Duration::from_millis(33);
const HINT_RUNNING: &str = "esc close (aborts) · c copy answer";
const HINT_DONE: &str = "esc close · c copy answer";
/// Border, header, rule, hint rows around the answer pane.
const CHROME_ROWS: u16 = 5;
/// Answer pane cap: the side panel never eats the transcript.
const MAX_ROWS: u16 = 12;

/// pi `BtwPanelComponent` status badge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SideStatus {
	/// The side kernel is still streaming.
	Running,
	/// The answer is complete.
	Complete,
	/// The side kernel failed.
	Error,
	/// The user closed the panel while streaming.
	Aborted,
}

impl SideStatus {
	const fn label(self) -> &'static str {
		match self {
			Self::Running => "running",
			Self::Complete => "complete",
			Self::Error => "error",
			Self::Aborted => "aborted",
		}
	}
}

/// Retained `/btw` side panel.
pub struct SidePanel {
	question: Str,
	answer:   StrMut,
	error:    Option<Str>,
	status:   SideStatus,
	events:   Receiver<SideEvent>,
	ui:       Ui,
	ctx:      UiContext,
	width:    u16,
	rows:     u16,
	last:     Duration,
}

impl SidePanel {
	/// Opens the panel over a streaming side answer.
	#[must_use]
	pub fn btw(question: Str, events: Receiver<SideEvent>, cx: &PanelCx<'_>) -> Self {
		let mut panel = Self {
			question,
			answer: StrMut::new(""),
			error: None,
			status: SideStatus::Running,
			events,
			ui: Ui::from_root(dom! { <col/> }, cx.viewport.width, cx.ui.clone()),
			ctx: cx.ui.clone(),
			width: cx.viewport.width,
			rows: 3,
			last: Duration::ZERO,
		};
		panel.rebuild();
		panel
	}

	/// Current status badge.
	#[must_use]
	pub const fn status(&self) -> SideStatus {
		self.status
	}

	/// Answer text received so far.
	#[must_use]
	pub fn answer(&self) -> &str {
		self.answer.as_str()
	}

	/// Drains queued side events; returns whether anything changed.
	fn drain(&mut self) -> bool {
		let mut changed = false;
		while let Ok(event) = self.events.try_recv() {
			changed = true;
			match event {
				SideEvent::Delta(text) => self.answer.push_str(text.as_str()),
				SideEvent::Done => self.status = SideStatus::Complete,
				SideEvent::Error(error) => {
					self.error = Some(error);
					self.status = SideStatus::Error;
				},
			}
		}
		if self.status == SideStatus::Running && self.events.is_disconnected() {
			self.status = SideStatus::Complete;
			changed = true;
		}
		changed
	}

	fn rebuild(&mut self) {
		let question = self.question.clone();
		let status = self.status.label();
		let status_fg = match self.status {
			SideStatus::Running => "accent",
			SideStatus::Complete => "success",
			SideStatus::Error | SideStatus::Aborted => "err",
		};
		let body = match &self.error {
			Some(error) => Str::new(format!("Error: {error}")),
			None if self.answer.is_empty() => Str::new_static("…"),
			None => Str::new(self.answer.as_str()),
		};
		let hint = if self.status == SideStatus::Running {
			HINT_RUNNING
		} else {
			HINT_DONE
		};
		let rows = self.rows;
		let tree = dom! {
			<box border=round title="btw" pad-x=1>
				<col>
					<row gap=1>
						<text bold truncate grow>{question}</text>
						<text fg={status_fg}>{status}</text>
					</row>
					<hr border=round/>
					<scroll id="answer" h={rows} focus>
						<md>{body}</md>
					</scroll>
					<text fg=muted truncate>{hint}</text>
				</col>
			</box>
		};
		self.ui = Ui::from_root(tree, self.width, self.ctx.clone());
	}
}

impl Panel for SidePanel {
	fn id(&self) -> &'static str {
		"btw"
	}

	fn anchor(&self) -> PanelAnchor {
		PanelAnchor::Side
	}

	fn key(&mut self, key: Key) -> PanelEvent {
		match key {
			Key::Esc => {
				if self.status == SideStatus::Running {
					self.status = SideStatus::Aborted;
				}
				PanelEvent::Close
			},
			Key::Char('c') => {
				if self.answer.is_empty() {
					PanelEvent::Notice(Str::new_static("No /btw answer to copy yet"))
				} else {
					PanelEvent::Copy(Str::new(self.answer.as_str()))
				}
			},
			Key::Up | Key::Down | Key::PageUp | Key::PageDown => match self.ui.handle_key(key) {
				UiEvent::Cancel => PanelEvent::Close,
				_ => PanelEvent::Consumed,
			},
			_ => PanelEvent::Ignored,
		}
	}

	fn frame(&mut self, viewport: Size) -> &Frame {
		let rows = (viewport.height / 3)
			.saturating_sub(CHROME_ROWS)
			.clamp(3, MAX_ROWS);
		if viewport.width != self.width || rows != self.rows {
			self.width = viewport.width;
			self.rows = rows;
			self.rebuild();
		}
		self.ui.frame()
	}

	fn tick(&mut self, now: Duration) -> bool {
		self.last = now;
		if self.drain() {
			self.rebuild();
			// Follow the stream: keep the newest answer rows visible.
			self.ui.set_prop("answer", Prop::H, self.rows);
			return true;
		}
		false
	}

	fn next_wake(&self) -> Option<Duration> {
		(self.status == SideStatus::Running).then(|| self.last + POLL)
	}
}

#[cfg(test)]
mod tests {
	use omp_con::Ctx;
	use omp_dom::Dom;

	use super::*;
	use std::sync::Arc;

	use crate::overlays::{NoServices, Services};

	fn cx<'a>(dom: &'a Dom, con: &'a Ctx, ui: &'a UiContext, services: &'a Arc<dyn Services>) -> PanelCx<'a> {
		PanelCx { dom, con, ui, viewport: Size { width: 60, height: 24 }, services }
	}

	#[test]
	fn streams_deltas_then_settles_and_copies() {
		let dom = Dom::new();
		let con = Ctx::new();
		let ui = UiContext::default();
		let services: Arc<dyn Services> = Arc::new(NoServices);
		let (tx, rx) = flume::unbounded();
		let mut panel = SidePanel::btw(Str::new_static("why is the sky blue?"), rx, &cx(&dom, &con, &ui, &services));
		assert_eq!(panel.status(), SideStatus::Running);
		assert!(panel.next_wake().is_some(), "running panels poll the stream");
		tx.send(SideEvent::Delta(Str::new_static("Rayleigh "))).unwrap();
		tx.send(SideEvent::Delta(Str::new_static("scattering."))).unwrap();
		assert!(panel.tick(Duration::from_millis(40)));
		assert_eq!(panel.answer(), "Rayleigh scattering.");
		let text = omp_tui::frame_text(panel.frame(Size { width: 60, height: 24 }));
		assert!(text.contains("why is the sky blue?"), "question header missing:\n{text}");
		assert!(text.contains("running"), "status badge missing:\n{text}");
		tx.send(SideEvent::Done).unwrap();
		assert!(panel.tick(Duration::from_millis(80)));
		assert_eq!(panel.status(), SideStatus::Complete);
		assert!(panel.next_wake().is_none(), "settled panels stop polling");
		assert_eq!(
			panel.key(Key::Char('c')),
			PanelEvent::Copy(Str::new_static("Rayleigh scattering."))
		);
		assert_eq!(panel.key(Key::Esc), PanelEvent::Close);
	}

	#[test]
	fn escape_while_running_marks_the_answer_aborted() {
		let dom = Dom::new();
		let con = Ctx::new();
		let ui = UiContext::default();
		let services: Arc<dyn Services> = Arc::new(NoServices);
		let (_tx, rx) = flume::unbounded::<SideEvent>();
		let mut panel = SidePanel::btw(Str::new_static("q"), rx, &cx(&dom, &con, &ui, &services));
		assert_eq!(panel.key(Key::Char('c')), PanelEvent::Notice(Str::new_static("No /btw answer to copy yet")));
		assert_eq!(panel.key(Key::Esc), PanelEvent::Close);
		assert_eq!(panel.status(), SideStatus::Aborted);
	}
}
