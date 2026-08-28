//! Compact session-model picker backed exclusively by host-supplied catalog
//! rows.

use std::fmt::{self, Write as _};

use omp_core::{Str, StrMut, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Prop, Size, Ui, UiContext, UiEvent,
	assets::provider_logo, dom,
};

use crate::{
	ModelRow,
	overlays::{OverlayPanel, panel_divider},
};

const HINT: &str = "↑/↓ models · Enter switch · type to search · Alt+P task model · Esc close";
const TASK_HINT: &str =
	"↑/↓ models · Enter use for task subagents · type to search · Alt+P session model · Esc close";
const FRAME_ROWS: u16 = 6;
const CONTEXT_WIDTH: u16 = 62;
const INPUT_PRICE_WIDTH: u16 = 76;
const OUTPUT_PRICE_WIDTH: u16 = 88;

/// What a routed picker event did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerEvent {
	/// The picker consumed the event and remains open.
	Consumed,
	/// Close without choosing a row.
	Close,
	/// Choose the row at this index.
	Pick(usize),
	/// Choose the row at this index for task subagents.
	PickTask(usize),
}

/// Retained filterable model-picker overlay.
pub struct ModelPicker {
	ui:           Ui,
	rows:         Vec<ModelRow>,
	current:      usize,
	task_current: usize,
	task_mode:    bool,
	ctx:          UiContext,
	options:      OverlayOptions,
	query:        Str,
	list_rows:    u16,
}

impl ModelPicker {
	/// Opens the picker over host-supplied rows with `current` preselected.
	///
	/// `task_current` is preselected after toggling into task-subagent mode.
	pub fn open(rows: &[ModelRow], current: usize, task_current: usize, ctx: &UiContext) -> Self {
		let rows = rows.to_vec();
		let current = current.min(rows.len().saturating_sub(1));
		let task_current = task_current.min(rows.len().saturating_sub(1));
		let ui = build(&rows, current, "", 6, 100, false, ctx);
		let mut picker = Self {
			ui,
			rows,
			current,
			task_current,
			task_mode: false,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Bottom)
				.width(Dim::Pct(100))
				.z(10),
			query: Str::default(),
			list_rows: 6,
		};
		picker.show_detail((!picker.rows.is_empty()).then_some(current));
		picker
	}

	/// Routes a key into the model filter and list.
	pub fn handle_key(&mut self, key: Key) -> PickerEvent {
		if key == Key::Alt('p') {
			self.task_mode = !self.task_mode;
			let width = self.ui.frame().size().width;
			self.rebuild(width);
			return PickerEvent::Consumed;
		}
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	/// Routes pasted query text into the model filter.
	pub fn handle_paste(&mut self, text: &str) -> PickerEvent {
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	/// Routes a pointer event; clicking outside dismisses the overlay.
	pub fn handle_mouse(&mut self, col: u16, row: u16, kind: Mouse, viewport: Size) -> PickerEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => self.route(event),
			None if kind == Mouse::Click => PickerEvent::Close,
			None => PickerEvent::Consumed,
		}
	}

	/// Returns the bottom-anchored composited layer for this frame.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let rows = (viewport.height * 2 / 5).saturating_sub(FRAME_ROWS).max(5);
		if rows != self.list_rows {
			self.list_rows = rows;
			self.ui.set_prop("models", Prop::H, rows.saturating_add(1));
		}
		if self.ui.frame().size().width != viewport.width {
			self.rebuild(viewport.width);
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	/// Replaces catalog rows while preserving the active query.
	pub fn update_rows(&mut self, rows: &[ModelRow], current: usize, task_current: usize) {
		let width = self.ui.frame().size().width;
		self.rows = rows.to_vec();
		self.current = current.min(self.rows.len().saturating_sub(1));
		self.task_current = task_current.min(self.rows.len().saturating_sub(1));
		self.rebuild(width);
	}

	fn route(&mut self, event: UiEvent) -> PickerEvent {
		match event {
			UiEvent::Cancel => PickerEvent::Close,
			UiEvent::Changed { id, value } if id.as_str() == "models" => value
				.as_str()
				.parse()
				.map_or(PickerEvent::Consumed, |index| {
					if self.task_mode {
						PickerEvent::PickTask(index)
					} else {
						PickerEvent::Pick(index)
					}
				}),
			UiEvent::Highlighted { id, value } if id.as_str() == "models" => {
				self.show_detail(value.as_str().parse().ok());
				PickerEvent::Consumed
			},
			UiEvent::Filtered { id, query, value } if id.as_str() == "models" => {
				self.query = query;
				self.show_detail(value.and_then(|value| value.as_str().parse().ok()));
				PickerEvent::Consumed
			},
			UiEvent::None
			| UiEvent::Submit
			| UiEvent::Pressed(_)
			| UiEvent::Copied(_)
			| UiEvent::TreeActivated { .. }
			| UiEvent::TreeToggled { .. }
			| UiEvent::TreeAction { .. }
			| UiEvent::DiffAction { .. }
			| UiEvent::Changed { .. }
			| UiEvent::Highlighted { .. }
			| UiEvent::Filtered { .. } => PickerEvent::Consumed,
		}
	}

	fn rebuild(&mut self, width: u16) {
		let selected = if self.task_mode {
			self.task_current
		} else {
			self.current
		};
		self.ui =
			build(&self.rows, selected, &self.query, self.list_rows, width, self.task_mode, &self.ctx);
		self.show_detail((!self.rows.is_empty()).then_some(selected));
	}

	fn show_detail(&mut self, model: Option<usize>) {
		let text = model
			.and_then(|index| self.rows.get(index))
			.map_or_else(|| sf!(" "), facts);
		self.ui.set_text("model-facts", text);
	}
}

struct DisplayRow {
	value:    Str,
	label:    Str,
	logo_src: Option<Str>,
	provider: Str,
	name:     Str,
	color:    Str,
	current:  bool,
	context:  Str,
	input:    Str,
	output:   Str,
}

fn build(
	rows: &[ModelRow],
	current: usize,
	query: &str,
	list_rows: u16,
	width: u16,
	task_mode: bool,
	ctx: &UiContext,
) -> Ui {
	let show_context = width >= CONTEXT_WIDTH && rows.iter().any(|row| row.context.is_some());
	let show_input = width >= INPUT_PRICE_WIDTH && rows.iter().any(|row| row.input_mtok.is_some());
	let show_output =
		width >= OUTPUT_PRICE_WIDTH && rows.iter().any(|row| row.output_mtok.is_some());
	let display: Vec<_> = rows
		.iter()
		.enumerate()
		.map(|(index, row)| DisplayRow {
			value:    sf!("{index}"),
			label:    sf!("{} {} {}", row.provider, row.name, row.key),
			logo_src: provider_logo(row.provider_id.as_str())
				.is_some()
				.then(|| sf!("asset://login/{}", row.provider_id)),
			provider: if row.provider.is_empty() {
				row.provider_id.clone()
			} else {
				row.provider.clone()
			},
			name:     if row.name.is_empty() {
				row.key.clone()
			} else {
				row.name.clone()
			},
			color:    row.color.clone().unwrap_or_else(|| sf!("fg")),
			current:  index == current,
			context:  row
				.context
				.map_or_else(Str::default, |tokens| sf!("{} ctx", compact_count(tokens))),
			input:    row
				.input_mtok
				.map_or_else(Str::default, |cost| sf!("${cost} in")),
			output:   row
				.output_mtok
				.map_or_else(Str::default, |cost| sf!("${cost} out")),
		})
		.collect();
	let seed = Str::new(query);
	let current_mark = if task_mode {
		sf!(" task")
	} else {
		sf!(" current")
	};
	let title = if task_mode {
		"Switch Task Model"
	} else {
		"Switch Model"
	};
	let hint = if task_mode { TASK_HINT } else { HINT };
	let height = list_rows.saturating_add(1);
	Ui::from_root(
		OverlayPanel::new(title).child(dom! {
			<col>
				<select id="models" filter={seed} h={height}>
					for row in display {
						<option value={row.value} label={row.label} recommended={row.current}>
							<td>
								if let Some(src) = row.logo_src.clone() { <img src={src} w=2 h=1/> }
							</td>
							<td truncate>
								<pre fg=fg bg=border>{" "}{row.provider}{" "}</pre>
							</td>
							<td truncate=start grow>
								<pre fg={row.color}>{row.name}</pre>
								if row.current { <pre fg=ok>{current_mark.clone()}</pre> }
							</td>
							if show_context { <td align=end><pre fg=muted>{row.context}</pre></td> }
							if show_input { <td align=end><pre fg=muted>{row.input}</pre></td> }
							if show_output { <td align=end><pre fg=muted>{row.output}</pre></td> }
						</option>
					}
				</select>
				{panel_divider()}
				<text id="model-facts" fg=muted truncate>{" "}</text>
				<text fg=muted truncate>{hint}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

fn facts(row: &ModelRow) -> Str {
	let mut line = StrMut::with_capacity(96);
	let name = if row.name.is_empty() {
		&row.key
	} else {
		&row.name
	};
	push_fact(&mut line, format_args!("{name}"));
	push_fact(&mut line, format_args!("{}", row.provider));
	if let Some(context) = row.context {
		push_fact(&mut line, format_args!("{} context", compact_count(context)));
	}
	match (row.input_mtok, row.output_mtok) {
		(Some(input), Some(output)) => {
			push_fact(&mut line, format_args!("${input}/${output} per Mtok"));
		},
		(Some(input), None) => push_fact(&mut line, format_args!("${input} in per Mtok")),
		(None, Some(output)) => push_fact(&mut line, format_args!("${output} out per Mtok")),
		(None, None) => {},
	}
	line.freeze()
}

fn push_fact(line: &mut StrMut, fact: fmt::Arguments<'_>) {
	if !line.is_empty() {
		line.push_str(" · ");
	}
	let _ = write!(line, "{fact}");
}

fn compact_count(value: u64) -> Str {
	if value >= 1_000_000 {
		sf!("{:.1}m", value as f64 / 1_000_000.0)
	} else if value >= 1_000 {
		sf!("{:.0}k", value as f64 / 1_000.0)
	} else {
		sf!("{value}")
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	fn row(provider: &'static str, name: &'static str) -> ModelRow {
		ModelRow {
			key:         sf!("{provider}/{name}"),
			name:        sf!(name),
			color:       None,
			provider_id: sf!(provider),
			provider:    sf!(provider),
			context:     None,
			input_mtok:  None,
			output_mtok: None,
			efforts:     std::sync::Arc::from([]),
		}
	}

	#[test]
	fn absent_facts_are_omitted() {
		let row = ModelRow {
			key:         sf!("p/m"),
			name:        sf!("Model"),
			color:       None,
			provider_id: sf!("p"),
			provider:    sf!("Provider"),
			context:     None,
			input_mtok:  None,
			output_mtok: None,
			efforts:     std::sync::Arc::from([]),
		};
		let facts = facts(&row);
		assert!(!facts.contains("ctx"));
		assert!(!facts.contains('$'));
	}

	#[test]
	fn typing_filters_models() {
		let rows = [row("alpha", "first"), row("beta", "second")];
		let mut picker = ModelPicker::open(&rows, 0, 0, &UiContext::default());
		assert_eq!(picker.handle_key(Key::Char('b')), PickerEvent::Consumed);
		assert_eq!(picker.handle_key(Key::Enter), PickerEvent::Pick(1));
	}

	#[test]
	fn down_then_enter_picks_the_next_model() {
		let rows = [row("alpha", "first"), row("beta", "second")];
		let mut picker = ModelPicker::open(&rows, 0, 0, &UiContext::default());
		assert_eq!(picker.handle_key(Key::Down), PickerEvent::Consumed);
		assert_eq!(picker.handle_key(Key::Enter), PickerEvent::Pick(1));
	}

	#[test]
	fn task_mode_is_reachable_and_picks_the_task_model() {
		let rows = [row("alpha", "first"), row("beta", "second")];
		let mut picker = ModelPicker::open(&rows, 0, 1, &UiContext::default());

		assert_eq!(picker.handle_key(Key::Alt('p')), PickerEvent::Consumed);
		assert_eq!(picker.handle_key(Key::Enter), PickerEvent::PickTask(1));
	}

	#[test]
	fn catalog_refresh_rebuilds_the_model_list() {
		let rows = [row("alpha", "first"), row("beta", "second")];
		let mut picker = ModelPicker::open(&rows, 0, 0, &UiContext::default());
		let refreshed = [row("alpha", "replacement"), row("beta", "new-second")];

		picker.update_rows(&refreshed, 0, 0);

		assert_eq!(picker.handle_key(Key::Down), PickerEvent::Consumed);
		assert_eq!(picker.handle_key(Key::Enter), PickerEvent::Pick(1));
	}
}
