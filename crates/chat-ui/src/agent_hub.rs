//! Interactive projection of the core-owned agent hierarchy.
//!
//! This module owns selection and presentation only. Lifecycle and message
//! actions are returned to the host and must be decided by the backend's
//! `AgentTree` authority.

use std::{
	collections::BTreeMap,
	time::{Duration, Instant},
};

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, UiEvent, dom,
};

use crate::{
	AgentRow,
	overlays::{OverlayPanel, panel_divider},
};

const FRAME_ROWS: u16 = 7;
const WIDE_INSPECTOR: u16 = 72;

/// Action requested from the selected agent row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentHubEvent {
	/// The overlay consumed input and remains open.
	Consumed,
	/// Close the hub and restore root composer focus.
	Close,
	/// Open prompt input for immediate steering.
	Steer(Str),
	/// Ask the backend to revive a cold agent.
	Revive(Str),
	/// Ask the backend to kill a live child agent.
	Kill(Str),
}

/// Whether the left pane presents a flat roster or hierarchy labels.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum HubView {
	Roster,
	#[default]
	Tree,
	Transcript,
}

/// Retained responsive Agent Hub overlay.
const LEFT_TAP_WINDOW: Duration = Duration::from_millis(500);
/// Retained overlay for navigating and acting on the backend-owned agent
/// hierarchy.
pub struct AgentHub {
	ui:        Ui,
	rows:      Vec<AgentRow>,
	frozen:    BTreeMap<Str, AgentRow>,
	previews:  BTreeMap<Str, Vec<Str>>,
	selected:  usize,
	view:      HubView,
	ctx:       UiContext,
	options:   OverlayOptions,
	list_rows: u16,
	width:     u16,
	last_left: Option<Instant>,
}

impl AgentHub {
	/// Opens a hub over a snapshot projected from the sole-authority agent tree.
	pub fn open(rows: &[AgentRow], ctx: &UiContext) -> Self {
		let rows = rows.to_vec();
		let frozen = BTreeMap::new();
		let previews = preview_accumulator(&rows);
		let selected = 0;
		let width = 100;
		let list_rows = 8;
		let ui = build(&rows, selected, HubView::Tree, list_rows, width, ctx);
		let mut hub = Self {
			ui,
			rows,
			frozen,
			previews,
			selected,
			view: HubView::Tree,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Bottom)
				.width(Dim::Pct(100))
				.z(10),
			list_rows,
			width,
			last_left: None,
		};
		hub.ui.focus_first();
		hub.capture_terminal_rows();
		hub.refresh_inspector();
		hub
	}

	/// Replaces the live projection while preserving selection by stable id.
	pub fn update_rows(&mut self, rows: &[AgentRow]) {
		let selected_id = self.rows.get(self.selected).map(|row| row.id.clone());
		self.rows = rows.to_vec();
		self.capture_terminal_rows();
		for (id, frozen) in &self.frozen {
			if !self.rows.iter().any(|row| row.id == *id) {
				self.rows.push(frozen.clone());
			}
		}
		accumulate_previews(&mut self.previews, &self.rows);
		self.selected = selected_id
			.as_ref()
			.and_then(|id| self.rows.iter().position(|row| row.id == *id))
			.unwrap_or(0)
			.min(self.rows.len().saturating_sub(1));
		self.rebuild();
	}

	/// Arms the hub's left-arrow close gesture with the tap that opened it.
	pub fn arm_close_tap(&mut self) {
		self.last_left = Some(Instant::now());
	}

	/// Routes keyboard selection, view toggles, transcript inspection, and
	/// lifecycle requests.
	pub fn handle_key(&mut self, key: Key) -> AgentHubEvent {
		match key {
			Key::Esc => return AgentHubEvent::Close,
			Key::Left => {
				let now = Instant::now();
				if self
					.last_left
					.is_some_and(|last| now.duration_since(last) <= LEFT_TAP_WINDOW)
				{
					self.last_left = None;
					return AgentHubEvent::Close;
				}
				self.last_left = Some(now);
			},
			Key::Char('t') => {
				self.view = match self.view {
					HubView::Roster => HubView::Tree,
					HubView::Tree => HubView::Roster,
					HubView::Transcript => HubView::Tree,
				};
				self.rebuild();
				return AgentHubEvent::Consumed;
			},
			Key::Char('v') => {
				self.view = if self.view == HubView::Transcript {
					HubView::Tree
				} else {
					HubView::Transcript
				};
				self.rebuild();
				return AgentHubEvent::Consumed;
			},
			Key::Char('s') => {
				return self.capability_event(|row| row.can_steer, AgentHubEvent::Steer);
			},
			Key::Enter => return self.activate_selected(),
			Key::Char('r') => {
				return self.capability_event(|row| row.can_revive, AgentHubEvent::Revive);
			},
			Key::Char('k') => return self.capability_event(|row| row.can_kill, AgentHubEvent::Kill),
			_ => {},
		}
		let routed = self.ui.handle_key(key);
		self.route(routed)
	}

	/// Routes pointer selection and outside-click dismissal.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> AgentHubEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => self.route(event),
			None if kind == Mouse::Click => AgentHubEvent::Close,
			None => AgentHubEvent::Consumed,
		}
	}

	/// Returns the responsive bottom-anchored layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let rows = (viewport.height * 3 / 5).saturating_sub(FRAME_ROWS).max(5);
		if rows != self.list_rows || viewport.width != self.width {
			self.list_rows = rows;
			self.width = viewport.width;
			self.rebuild();
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn activate_selected(&mut self) -> AgentHubEvent {
		let Some(row) = self.rows.get(self.selected) else {
			return AgentHubEvent::Consumed;
		};
		if row.can_steer {
			return AgentHubEvent::Steer(row.id.clone());
		}
		if row.can_revive {
			return AgentHubEvent::Revive(row.id.clone());
		}
		if row.frozen
			|| row.status.eq_ignore_ascii_case("aborted")
			|| row.status.eq_ignore_ascii_case("dead")
		{
			self.view = HubView::Transcript;
			self.rebuild();
		}
		AgentHubEvent::Consumed
	}

	fn capability_event(
		&self,
		allowed: impl FnOnce(&AgentRow) -> bool,
		event: impl FnOnce(Str) -> AgentHubEvent,
	) -> AgentHubEvent {
		self
			.rows
			.get(self.selected)
			.filter(|row| allowed(row))
			.map_or(AgentHubEvent::Consumed, |row| event(row.id.clone()))
	}

	fn route(&mut self, event: UiEvent) -> AgentHubEvent {
		match event {
			UiEvent::Cancel => AgentHubEvent::Close,
			UiEvent::Changed { id, value } if id.as_str() == "agent-hub-list" => {
				if let Ok(index) = value.as_str().parse() {
					self.selected = index;
				}
				self.refresh_inspector();
				AgentHubEvent::Consumed
			},
			UiEvent::Highlighted { id, value } if id.as_str() == "agent-hub-list" => {
				if let Ok(index) = value.as_str().parse() {
					self.selected = index;
				}
				self.refresh_inspector();
				AgentHubEvent::Consumed
			},
			_ => AgentHubEvent::Consumed,
		}
	}

	fn rebuild(&mut self) {
		self.selected = self.selected.min(self.rows.len().saturating_sub(1));
		self.ui = build(&self.rows, self.selected, self.view, self.list_rows, self.width, &self.ctx);
		self.ui.focus_first();
		self.refresh_inspector();
	}

	fn capture_terminal_rows(&mut self) {
		for row in &self.rows {
			if row.terminal_kind.is_none() {
				continue;
			}
			let mut frozen = row.clone();
			frozen.frozen = true;
			frozen.can_steer = false;
			frozen.can_kill = false;
			self.frozen.insert(row.id.clone(), frozen);
		}
	}

	fn refresh_inspector(&mut self) {
		let detail = self.rows.get(self.selected).map_or_else(
			|| sf!("No agents in this session."),
			|row| {
				let tool = row.tool.as_deref().unwrap_or("idle");
				let tokens = row
					.tokens
					.map_or_else(|| sf!("unknown"), |tokens| sf!("{tokens}"));
				let definition = row.definition.as_deref().unwrap_or("native");
				let model = row
					.serving_model
					.as_deref()
					.or(row.model.as_deref())
					.unwrap_or("default");
				let progress = sf!(
					"{} requests · {} tools · {} context · ${:.6}",
					row.requests,
					row.tool_calls,
					row.context_tokens,
					row.cost_micros as f64 / 1_000_000.0,
				);
				let verdict = review_badge(row);
				let assignment = row
					.assignment
					.as_deref()
					.unwrap_or("assignment unavailable");
				let previews = self
					.previews
					.get(&row.id)
					.map(|sections| {
						sections
							.iter()
							.map(Str::as_str)
							.collect::<Vec<_>>()
							.join("\n")
					})
					.unwrap_or_default();
				let terminal = row.terminal_summary.as_deref().unwrap_or("live");
				let artifact = row.artifact_uri.as_deref().unwrap_or("inline");
				sf!(
					"{} {} · {} · {} · {} · {} tokens\n{}\nassignment: {}\ncurrent: {}\nterminal: {} · \
					 {}\n{}",
					row.name,
					verdict,
					definition,
					model,
					row.status,
					tokens,
					progress,
					assignment,
					tool,
					terminal,
					artifact,
					if previews.is_empty() {
						row.transcript.as_str()
					} else {
						previews.as_str()
					},
				)
			},
		);
		self.ui.set_text("agent-hub-inspector", detail);
	}
}

fn preview_accumulator(rows: &[AgentRow]) -> BTreeMap<Str, Vec<Str>> {
	let mut previews = BTreeMap::new();
	accumulate_previews(&mut previews, rows);
	previews
}

fn accumulate_previews(previews: &mut BTreeMap<Str, Vec<Str>>, rows: &[AgentRow]) {
	const MAX_SECTIONS: usize = 16;
	for row in rows {
		if row.transcript.trim().is_empty() {
			continue;
		}
		let sections = previews.entry(row.id.clone()).or_default();
		if sections.last() == Some(&row.transcript) {
			continue;
		}
		sections.push(row.transcript.clone());
		if sections.len() > MAX_SECTIONS {
			sections.remove(0);
		}
	}
}

fn review_badge(row: &AgentRow) -> Str {
	let reviewer = row
		.definition
		.as_deref()
		.is_some_and(|definition| definition.to_ascii_lowercase().contains("review"));
	if !reviewer {
		return Str::default();
	}
	match row.terminal_kind.as_deref() {
		Some("succeeded") => sf!("[PASS]"),
		Some(kind) => sf!("[FAIL:{kind}]"),
		None => sf!("[REVIEW]"),
	}
}

fn build(
	rows: &[AgentRow],
	selected: usize,
	view: HubView,
	list_rows: u16,
	width: u16,
	ctx: &UiContext,
) -> Ui {
	let labels = rows
		.iter()
		.enumerate()
		.map(|(index, row)| {
			let indent = if view == HubView::Tree {
				"  ".repeat(usize::from(row.depth))
			} else {
				String::new()
			};
			let badge = review_badge(row);
			let frozen = if row.frozen { " [frozen]" } else { "" };
			let identity = if row.name == row.id {
				row.name.clone()
			} else {
				sf!("{} · {}", row.name, row.id)
			};
			let label = sf!("{indent}{identity}{frozen} {badge}");
			(index, label, row)
		})
		.collect::<Vec<_>>();
	let title = match view {
		HubView::Roster => "Agent Hub · roster",
		HubView::Tree => "Agent Hub · tree",
		HubView::Transcript => "Agent Hub · transcript inspect",
	};
	let height = list_rows.saturating_add(1);
	let list_width = if width >= WIDE_INSPECTOR {
		width.saturating_mul(2) / 5
	} else {
		width.saturating_sub(4)
	};
	let root = if view == HubView::Transcript {
		OverlayPanel::new(title).child(dom! {
			<col>
				<text id="agent-hub-inspector" h={height} wrap>{" "}</text>
				{panel_divider()}
				<text fg=muted truncate>{"v back · Esc root"}</text>
			</col>
		})
	} else if width >= WIDE_INSPECTOR {
		OverlayPanel::new(title).child(dom! {
			<col>
				<row gap=2>
					<select id="agent-hub-list" w={list_width} h={height}>
						for (index, label, row) in labels {
							<option value={sf!("{index}")} label={label.clone()} recommended={index == selected}>
								<td truncate grow><pre fg=fg>{label}</pre></td>
								<td truncate><pre fg=muted>{row.status.clone()}</pre></td>
							</option>
						}
					</select>
					<text id="agent-hub-inspector" grow wrap>{" "}</text>
				</row>
				{panel_divider()}
				<text fg=muted truncate>{"t roster/tree · v transcript · Enter/s steer · r revive · k kill · Esc root"}</text>
			</col>
		})
	} else {
		OverlayPanel::new(title).child(dom! {
			<col>
				<select id="agent-hub-list" w={list_width} h={height}>
					for (index, label, row) in labels {
						<option value={sf!("{index}")} label={label.clone()} recommended={index == selected}>
							<td truncate grow><pre fg=fg>{label}</pre></td>
							<td truncate><pre fg=muted>{row.status.clone()}</pre></td>
						</option>
					}
				</select>
				{panel_divider()}
				<text id="agent-hub-inspector" h=4 wrap>{" "}</text>
				<text fg=muted truncate>{"t view · v transcript · Enter/s steer · r revive · k kill · Esc"}</text>
			</col>
		})
	};
	Ui::from_root(root, width, ctx.clone())
}
#[cfg(test)]
mod tests {
	use omp_core::Str;
	use omp_tui::{Key, UiContext};

	use super::{AgentHub, AgentHubEvent, HubView};
	use crate::AgentRow;

	fn row(status: &'static str, can_steer: bool, can_revive: bool) -> AgentRow {
		AgentRow {
			id: Str::new_static("agent"),
			name: Str::new_static("agent"),
			parent: Some(Str::new_static("root")),
			depth: 1,
			status: Str::new_static(status),
			tool: None,
			tokens: None,
			definition: None,
			model: None,
			serving_model: None,
			transcript: Str::new_static("final transcript"),
			assignment: None,
			requests: 0,
			tool_calls: 0,
			context_tokens: 0,
			cost_micros: 0,
			terminal_kind: None,
			terminal_summary: None,
			artifact_uri: None,
			frozen: false,
			can_steer,
			can_revive,
			can_kill: false,
		}
	}

	#[test]
	fn enter_opens_terminal_transcripts_and_preserves_live_actions() {
		let ctx = UiContext::default();
		let mut dead = AgentHub::open(&[row("dead", false, false)], &ctx);
		assert_eq!(dead.handle_key(Key::Enter), AgentHubEvent::Consumed);
		assert_eq!(dead.view, HubView::Transcript);

		let mut aborted = AgentHub::open(&[row("aborted", false, false)], &ctx);
		assert_eq!(aborted.handle_key(Key::Enter), AgentHubEvent::Consumed);
		assert_eq!(aborted.view, HubView::Transcript);

		let mut live = AgentHub::open(&[row("running", true, false)], &ctx);
		assert_eq!(live.handle_key(Key::Enter), AgentHubEvent::Steer(Str::new_static("agent")),);

		let mut parked = AgentHub::open(&[row("parked", false, true)], &ctx);
		assert_eq!(parked.handle_key(Key::Enter), AgentHubEvent::Revive(Str::new_static("agent")),);
	}
	#[test]
	fn parallel_siblings_remain_individually_selectable() {
		let ctx = UiContext::default();
		let mut rows = ["alpha", "beta", "gamma", "delta"]
			.into_iter()
			.map(|id| {
				let mut row = row("running", true, false);
				row.id = Str::new(id);
				row.name = Str::new(id);
				row
			})
			.collect::<Vec<_>>();
		let mut hub = AgentHub::open(&rows, &ctx);
		let frame = hub.ui.frame();
		let painted = (0..frame.size().height)
			.map(|line| omp_tui::test_support::frame_row_text(frame, line))
			.collect::<Vec<_>>()
			.join("\n");
		for id in ["alpha", "beta", "gamma", "delta"] {
			assert!(painted.contains(id), "missing {id} from {painted:?}");
		}

		assert_eq!(hub.handle_key(Key::Down), AgentHubEvent::Consumed);
		assert_eq!(hub.selected, 1);
		assert_eq!(hub.handle_key(Key::Enter), AgentHubEvent::Steer(Str::new_static("beta")));

		rows.remove(0);
		hub.update_rows(&rows);
		assert_eq!(hub.selected, 0);
		assert_eq!(hub.handle_key(Key::Enter), AgentHubEvent::Steer(Str::new_static("beta")));
	}
}
