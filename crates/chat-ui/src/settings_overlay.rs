//! Schema-driven retained settings editor.

use std::collections::BTreeMap;

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Prop, Size, Ui, UiContext, UiEvent,
	components::{Field, Form, Tabs},
	dom,
};
use serde_json::Value;

use crate::{OverlayPanel, SettingRow, panel_divider};

/// One value mutation emitted by the settings surface.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingChange {
	/// Owning registered settings domain.
	pub domain: Str,
	/// Stable dotted field path.
	pub path:   Str,
	/// Typed JSON value produced by the reflected widget.
	pub value:  Value,
}

/// Action emitted by [`SettingsOverlay`].
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsEvent {
	/// Input was consumed without changing a value.
	Consumed,
	/// Dismiss without committing the preview generation.
	Close,
	/// Preview changed values without persisting them.
	Preview(Vec<SettingChange>),
	/// Persist the complete visible settings generation.
	Commit(Vec<SettingChange>),
}

/// Retained tabbed settings modal built from registered field descriptors.
pub struct SettingsOverlay {
	ui:       Ui,
	rows:     Vec<SettingRow>,
	ctx:      UiContext,
	options:  OverlayOptions,
	query:    Str,
	width:    u16,
	budget:   u16,
	baseline: BTreeMap<Str, Value>,
}

impl SettingsOverlay {
	/// Opens the editor over the backend's merged, secret-safe schema
	/// projection.
	pub fn open(rows: Vec<SettingRow>, ctx: &UiContext) -> Self {
		let width = 84;
		let budget = 14;
		let query = Str::default();
		let mut ui = build(&rows, &query, width, budget, 0, ctx);
		ui.focus_first();
		let baseline = collect_values(&ui, &rows);
		Self {
			ui,
			rows,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(width))
				.z(20),
			query,
			width,
			budget,
			baseline,
		}
	}

	/// Routes keyboard editing, popup navigation, preview, and commit.
	pub fn handle_key(&mut self, key: Key) -> SettingsEvent {
		if key == Key::Ctrl('s') {
			return SettingsEvent::Commit(self.changes(true));
		}
		let (mut event, claimed) = self.ui.handle_key_claimed(key);
		// Global type-to-search: a printable key nothing claimed jumps to
		// the search field and lands there, wherever focus was parked.
		if !claimed && matches!(key, Key::Char(_)) && self.ui.focus_id("settings-search") {
			event = self.ui.handle_key_claimed(key).0;
		}
		match &event {
			// Esc backs out one layer at a time: an open dropdown consumes
			// it, then a live search clears, then the overlay closes.
			UiEvent::Cancel if self.query.is_empty() => return SettingsEvent::Close,
			UiEvent::Cancel => {
				self.query = Str::default();
				self.rebuild();
				return SettingsEvent::Consumed;
			},
			UiEvent::Changed { id, value } if id.as_str() == "settings-search" => {
				self.query = value.clone();
				self.rebuild();
				return SettingsEvent::Consumed;
			},
			_ => {},
		}
		let changes = self.changes(false);
		if changes.is_empty() {
			SettingsEvent::Consumed
		} else {
			SettingsEvent::Preview(changes)
		}
	}

	/// Routes pasted text to the focused search or secret-safe text field.
	pub fn handle_paste(&mut self, text: &str) -> SettingsEvent {
		let event = self.ui.handle_paste(text);
		if let UiEvent::Changed { id, value } = &event
			&& id.as_str() == "settings-search"
		{
			self.query = value.clone();
			self.rebuild();
			return SettingsEvent::Consumed;
		}
		let changes = self.changes(false);
		if changes.is_empty() {
			SettingsEvent::Consumed
		} else {
			SettingsEvent::Preview(changes)
		}
	}

	/// Routes pointer events; an outside click cancels the preview generation.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> SettingsEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(_) => {
				let changes = self.changes(false);
				if changes.is_empty() {
					SettingsEvent::Consumed
				} else {
					SettingsEvent::Preview(changes)
				}
			},
			None if kind == Mouse::Click => SettingsEvent::Close,
			None => SettingsEvent::Consumed,
		}
	}

	/// Returns a centered viewport-responsive retained layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(4).clamp(1, 96);
		// Overlay chrome around the field viewport: borders, search row,
		// dividers, tab bar and rule, pinned description, hint row.
		let budget = viewport.height.saturating_sub(12).max(4);
		if width != self.width || budget != self.budget {
			self.width = width;
			self.budget = budget;
			self.rebuild();
		}
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn rebuild(&mut self) {
		let current = collect_values(&self.ui, &self.rows);
		let tab = active_tab(&self.ui, &self.rows);
		self.ui = build(&self.rows, &self.query, self.width, self.budget, tab, &self.ctx);
		self.ui.focus_first();
		self.baseline.extend(current);
	}

	fn changes(&mut self, include_unchanged: bool) -> Vec<SettingChange> {
		let current = collect_values(&self.ui, &self.rows);
		let mut changes = Vec::new();
		for row in &self.rows {
			let Some(value) = current.get(&row.path) else {
				continue;
			};
			if include_unchanged || self.baseline.get(&row.path) != Some(value) {
				changes.push(SettingChange {
					domain: row.domain.clone(),
					path:   row.path.clone(),
					value:  value.clone(),
				});
			}
		}
		if !include_unchanged {
			self.baseline = current;
		}
		changes
	}
}

fn build(
	rows: &[SettingRow],
	query: &str,
	width: u16,
	budget: u16,
	tab: u16,
	ctx: &UiContext,
) -> Ui {
	let query_folded = query.to_ascii_lowercase();
	let panels = panels_of(rows);
	let matches: Vec<Vec<&SettingRow>> = panels
		.iter()
		.map(|panel| {
			rows
				.iter()
				.filter(|row| {
					row.visible
						&& (row.panel == *panel || (row.panel.is_empty() && row.domain == *panel))
						&& (query_folded.is_empty()
							|| row.label.to_ascii_lowercase().contains(&query_folded)
							|| row.path.to_ascii_lowercase().contains(&query_folded)
							|| row.description.to_ascii_lowercase().contains(&query_folded))
				})
				.collect()
		})
		.collect();
	// While filtering, an emptied active pane jumps to the first pane that
	// still matches, so results are never hidden behind a blank tab.
	let tab = if !query_folded.is_empty()
		&& matches
			.get(usize::from(tab))
			.is_none_or(|visible| visible.is_empty())
	{
		matches
			.iter()
			.position(|visible| !visible.is_empty())
			.map_or(tab, |index| index as u16)
	} else {
		tab
	};
	let mut tabs = Tabs::new().with(Prop::Id, "settings-tabs").select(tab);
	for (index, (panel, visible)) in panels.iter().zip(&matches).enumerate() {
		let mut form = Form::new().with(Prop::Id, sf!("settings-form-{index}"));
		for row in visible {
			let kind = match row.kind.as_str() {
				"bool" | "boolean" => "bool",
				"enum" => "select",
				"multi" | "string-list" => "multi",
				"number" | "integer" => "number",
				_ => "text",
			};
			let mut field = Field::new()
				.with(Prop::Id, row.path.clone())
				.with(Prop::Kind, kind)
				.with(Prop::Desc, row.description.clone())
				.with(Prop::Mask, row.secret)
				.label(row.label.clone());
			if let Some(value) = &row.value {
				field = field.with(Prop::Value, value.clone());
			}
			if !row.options.is_empty() {
				field = field.with(
					Prop::Options,
					Str::from(
						row.options
							.iter()
							.map(Str::as_str)
							.collect::<Vec<_>>()
							.join(" "),
					),
				);
			}
			form = form.field(field);
		}
		form = form.with(Prop::H, budget);
		let title = if query_folded.is_empty() {
			Str::new(panel_title(panel))
		} else {
			sf!("{} ({})", panel_title(panel), visible.len())
		};
		tabs = tabs.pane_icon(panel_icon(panel), title, form);
	}
	let seed = Str::new(query);
	Ui::from_root(
		OverlayPanel::new("Settings").child(dom! {
			<col>
				<input id="settings-search" value={seed} placeholder="Search settings"/>
				{panel_divider()}
				{tabs}
				{panel_divider()}
				<text dim truncate>"Type to search · Tab focus · ←/→ panes/change · Enter edit · Ctrl+S save · Esc close"</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}
/// Distinct panel ids of visible rows, in first-seen (backend) order.
fn panels_of(rows: &[SettingRow]) -> Vec<Str> {
	let mut panels = Vec::<Str>::new();
	for row in rows.iter().filter(|row| row.visible) {
		let panel = if row.panel.is_empty() {
			row.domain.clone()
		} else {
			row.panel.clone()
		};
		if !panels.contains(&panel) {
			panels.push(panel);
		}
	}
	panels
}

/// Display label for a stable settings panel id.
fn panel_title(panel: &str) -> &str {
	match panel {
		"appearance" => "Appearance",
		"model" => "Model",
		"interaction" => "Interaction",
		"context" => "Context",
		"files_shell" => "Files & Shell",
		"tools_tasks" => "Tools & Tasks",
		"orchestration" => "Orchestration",
		"providers" => "Providers",
		"extensions" => "Extensions",
		"lifecycle" => "Lifecycle",
		other => other,
	}
}
/// Chip icon (an `icons.tsv` name) for a stable settings panel id.
fn panel_icon(panel: &str) -> &'static str {
	match panel {
		"appearance" => "appearance",
		"model" => "model",
		"interaction" => "interaction",
		"context" => "context",
		"files_shell" => "files",
		"tools_tasks" => "tools",
		"orchestration" => "agents",
		"providers" => "providers",
		"extensions" => "puzzle",
		"lifecycle" => "gear",
		_ => "config",
	}
}

/// Index of the active tab in the retained tree, so a rebuild (search
/// keystroke, resize) keeps the user's pane.
fn active_tab(ui: &Ui, rows: &[SettingRow]) -> u16 {
	let Some(title) = ui
		.values()
		.get("settings-tabs")
		.and_then(Value::as_str)
		.map(Str::new)
	else {
		return 0;
	};
	panels_of(rows)
		.iter()
		.position(|panel| title.as_str().starts_with(panel_title(panel)))
		.map_or(0, |index| index as u16)
}

fn collect_values(ui: &Ui, rows: &[SettingRow]) -> BTreeMap<Str, Value> {
	let values = ui.values();
	let mut collected = BTreeMap::new();
	let Some(root) = values.as_object() else {
		return collected;
	};
	for form in root.values().filter_map(Value::as_object) {
		for row in rows {
			if let Some(value) = form.get(row.path.as_str()) {
				collected.insert(row.path.clone(), value.clone());
			}
		}
	}
	collected
}
