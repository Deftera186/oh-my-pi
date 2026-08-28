//! Persistent facts-driven session rail for the immediate-mode chat scene.
//!
//! The non-modal rail rides each inline present as a raw layer. It never
//! becomes part of the transcript, so terminal scrollback remains full width.

use std::time::Instant;

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Prop, Size, Ui, UiContext, UiEvent,
	components::{compaction_threshold_color, spend_label},
	dom,
};

use crate::{CompactionSpeculationStatus, StatusFacts, scene::RailWidths};

/// Rail width in cells, vertical rule included.
const WIDTH: u16 = 30;
/// Smallest viewport at which the rail is composited.
const MIN_VIEWPORT: Size = Size::new(96, 20);

/// Sums all visible right rail widths instead of privileging one sidebar.
pub fn reserved_rails(viewport: Size, widths: impl IntoIterator<Item = u16>) -> RailWidths {
	widths
		.into_iter()
		.fold(RailWidths::default(), |rails, width| {
			rails.accumulate(false, width.min(viewport.width))
		})
}

/// Retained session facts composited as a right-anchored viewport layer.
pub struct Sidebar {
	ui:              Ui,
	ctx:             UiContext,
	options:         OverlayOptions,
	open:            bool,
	focused:         bool,
	turn_started:    Option<Instant>,
	elapsed_seconds: u64,
	height:          u16,
}

impl Sidebar {
	/// Builds a passive, initially visible rail from backend status facts.
	pub fn new(facts: &StatusFacts, ctx: &UiContext) -> Self {
		Self::with_visibility(facts, ctx, true)
	}

	/// Builds a passive rail that remains hidden until the user opens it.
	pub fn new_hidden(facts: &StatusFacts, ctx: &UiContext) -> Self {
		Self::with_visibility(facts, ctx, false)
	}

	fn with_visibility(facts: &StatusFacts, ctx: &UiContext, open: bool) -> Self {
		let options = OverlayOptions::default()
			.anchor(OverlayAnchor::Right)
			.width(Dim::Cells(WIDTH))
			.non_modal()
			.min_viewport(MIN_VIEWPORT);
		let mut ui = build(facts, ctx);
		ui.blur();
		Self {
			ui,
			ctx: ctx.clone(),
			options,
			open,
			focused: false,
			turn_started: facts.turn_started,
			elapsed_seconds: 0,
			height: 0,
		}
	}

	const fn visible(&self, viewport: Size) -> bool {
		self.open && viewport.width >= MIN_VIEWPORT.width && viewport.height >= MIN_VIEWPORT.height
	}

	/// Returns the columns reserved by the visible rail.
	pub const fn reserved(&self, viewport: Size) -> u16 {
		if self.visible(viewport) { WIDTH } else { 0 }
	}

	/// Reports whether the rail currently owns keyboard focus.
	pub const fn focused(&self) -> bool {
		self.focused
	}

	/// Toggles the rail; opening it transfers keyboard focus into the rail.
	pub fn toggle(&mut self) {
		self.open = !self.open;
		if self.open {
			self.focused = true;
			self.ui.focus_first();
		} else {
			self.blur();
		}
	}

	/// Routes a key while the rail is focused; Escape returns focus to chat.
	pub fn handle_key(&mut self, key: Key) {
		if self.ui.handle_key(key) == UiEvent::Cancel {
			self.blur();
		}
	}

	/// Routes a pointer event through the rail's composited band.
	///
	/// Returns `false` when the gesture belongs to the transcript beneath it.
	pub fn handle_mouse(&mut self, col: u16, row: u16, kind: Mouse, viewport: Size) -> bool {
		if !self.open {
			return false;
		}
		if self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
			.is_some()
		{
			if kind == Mouse::Click && !self.focused {
				self.focused = true;
				self.ui.focus_first();
			}
			true
		} else {
			if kind == Mouse::Click {
				self.blur();
			}
			false
		}
	}

	/// Rebuilds the rail from authoritative backend status facts.
	pub fn set_status(&mut self, facts: &StatusFacts) {
		let focused = self.focused;
		let height = self.height;
		self.ui = build(facts, &self.ctx);
		self.turn_started = facts.turn_started;
		self.elapsed_seconds = 0;
		if height != 0 {
			self.ui.set_prop("rail", Prop::H, height);
			self.ui.set_prop("body", Prop::H, height);
		}
		if focused {
			self.ui.focus_first();
		} else {
			self.ui.blur();
		}
	}

	/// Returns the rail layer for this frame, or `None` when hidden or gated.
	pub fn layer(&mut self, viewport: Size, now: Instant) -> Option<Layer<'_>> {
		if !self.visible(viewport) {
			if self.focused {
				self.blur();
			}
			return None;
		}
		if self.height != viewport.height {
			self.height = viewport.height;
			self.ui.set_prop("rail", Prop::H, viewport.height);
			self.ui.set_prop("body", Prop::H, viewport.height);
		}
		let seconds = self
			.turn_started
			.map_or(0, |started| now.saturating_duration_since(started).as_secs());
		if seconds != self.elapsed_seconds {
			self.elapsed_seconds = seconds;
			self.ui.set_text("elapsed", elapsed_label(seconds));
		}
		Some(Layer { frame: self.ui.frame(), options: &self.options, active: self.focused })
	}

	fn blur(&mut self) {
		self.focused = false;
		self.ui.blur();
	}
}

fn elapsed_label(seconds: u64) -> Str {
	sf!("{}:{:02}", seconds / 60, seconds % 60)
}

fn context_label(facts: &StatusFacts) -> Str {
	match facts.context_window {
		Some(window) if window > 0 => {
			sf!("{} / {}%", facts.context_tokens, facts.context_tokens.saturating_mul(100) / window)
		},
		_ => sf!("{}", facts.context_tokens),
	}
}

fn build(facts: &StatusFacts, ctx: &UiContext) -> Ui {
	let state = if facts.working { "working" } else { "idle" };
	let context = context_label(facts);
	let context_color = compaction_threshold_color(&ctx.theme);
	let cost = spend_label(facts.cost_nanos, facts.model_subscription, ctx.charset);
	let compaction = match facts.compaction_speculation {
		CompactionSpeculationStatus::Idle => None,
		CompactionSpeculationStatus::Running => Some("running"),
		CompactionSpeculationStatus::Armed => Some("armed"),
	};
	let activity = sf!("q{} · jobs {}", facts.queued, facts.jobs);
	let git = facts
		.git
		.as_ref()
		.map(|git| sf!("{} *{} +{}", git.branch, git.dirty, git.staged));
	Ui::from_root(
		dom! {
			<row id="rail" h=24>
				<hr/>
				<col id="body" h=24 grow pad="0 1" gap=1>
					<text bold fg=info>{"session"}</text>
					<col>
						<row gap=1>
							<text fg=muted w=8>{"model"}</text>
							<text id="model" truncate>{facts.model.clone()}</text>
						</row>
						<row gap=1>
							<text fg=muted w=8>{"state"}</text>
							<text>{state}</text>
						</row>
						<row gap=1>
							<text fg=muted w=8>{"elapsed"}</text>
							<text id="elapsed">{"0:00"}</text>
						</row>
						if let Some(git) = git {
							<row gap=1>
								<text fg=muted w=8>{"branch"}</text>
								<text truncate>{git}</text>
							</row>
						}
					</col>
					<hr/>
					<text bold fg=info>{"usage"}</text>
					<col>
						<row gap=1><text fg=muted w=8>{"context"}</text><text fg={context_color} truncate>{context}</text></row>
						if let Some(compaction) = compaction {
							<row gap=1>
								<text fg=muted w=8>{"compact"}</text>
								<text fg=accent>{compaction}</text>
							</row>
						}
						<row gap=1><text fg=muted w=8>{"cost"}</text><text>{cost}</text></row>
						<row gap=1><text fg=muted w=8>{"activity"}</text><text truncate>{activity}</text></row>
						if facts.attempt > 0 || facts.dropped > 0 {
							<row gap=1>
								<text fg=muted w=8>{"attempt"}</text>
								<text>{sf!("{} · drop {}", facts.attempt, facts.dropped)}</text>
							</row>
						}
					</col>
					<spacer grow/>
					<text dim truncate>{"ctrl+b rail · esc back"}</text>
				</col>
			</row>
		},
		WIDTH,
		ctx.clone(),
	)
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use omp_core::sf;
	use omp_tui::{Color, Size, UiContext, test_support::frame_row_text};

	use super::Sidebar;
	use crate::{CompactionSpeculationStatus, GitFacts, StatusFacts};

	fn facts() -> StatusFacts {
		StatusFacts {
			model: sf!("Claude Fable 5"),
			working: false,
			turn_started: None,
			context_tokens: 42,
			context_window: Some(1_000),
			cost_nanos: 250_000_000,
			queued: 0,
			jobs: 0,
			attempt: 0,
			dropped: 0,
			git: Some(GitFacts { branch: sf!("main"), dirty: 1, staged: 0, untracked: 0 }),
			..StatusFacts::default()
		}
	}

	#[test]
	fn rail_surfaces_armed_compaction() {
		let ctx = UiContext::default();
		let viewport = Size::new(120, 30);
		let mut facts = facts();
		facts.compaction_speculation = CompactionSpeculationStatus::Armed;
		let mut sidebar = Sidebar::new(&facts, &ctx);
		let layer = sidebar
			.layer(viewport, Instant::now())
			.expect("rail visible");
		assert!((0..30).any(|row| {
			let text = frame_row_text(layer.frame, row);
			text.contains("compact") && text.contains("armed")
		}));
	}

	#[test]
	fn rail_starts_passive_and_renders_backend_facts() {
		let ctx = UiContext::default();
		let viewport = Size::new(120, 30);
		let mut sidebar = Sidebar::new(&facts(), &ctx);
		let passive: Vec<String> = {
			let layer = sidebar
				.layer(viewport, Instant::now())
				.expect("rail visible");
			assert!(!layer.active);
			(0..30)
				.map(|row| frame_row_text(layer.frame, row))
				.collect()
		};
		assert!(passive.iter().any(|row| row.contains("Claude Fable 5")));
		assert!(passive.iter().any(|row| row.contains("main")));

		sidebar.toggle();
		sidebar.toggle();
		let layer = sidebar
			.layer(viewport, Instant::now() + Duration::from_secs(1))
			.expect("rail reopened");
		assert!(layer.active);
	}

	#[test]
	fn rail_preserves_sub_cent_cost_precision() {
		let ctx = UiContext::default();
		let viewport = Size::new(120, 30);
		let mut facts = facts();
		facts.cost_nanos = 1_918_000;
		let mut sidebar = Sidebar::new(&facts, &ctx);
		let layer = sidebar
			.layer(viewport, Instant::now())
			.expect("rail visible");
		assert!((0..30).any(|row| frame_row_text(layer.frame, row).contains("$0.0019")));
	}

	#[test]
	fn context_usage_uses_the_themed_compaction_threshold_color() {
		let accent = Color::Rgb(12, 34, 56);
		let mut ctx = UiContext::default();
		ctx.theme.accent = accent;
		let viewport = Size::new(120, 30);
		let mut sidebar = Sidebar::new(&facts(), &ctx);
		let layer = sidebar
			.layer(viewport, Instant::now())
			.expect("rail visible");
		let (row, column) = (0..layer.frame.size().height)
			.find_map(|row| {
				let text = frame_row_text(layer.frame, row);
				let byte = text.find("42 / 4%")?;
				Some((row, u16::try_from(xutf::width_str(&text[..byte])).unwrap_or(u16::MAX)))
			})
			.expect("context usage visible");
		assert_eq!(layer.frame.cell(column, row).style().foreground_color(), accent);
	}
}
