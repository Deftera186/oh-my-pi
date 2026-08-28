use omp_core::{IntoStr, Str, sf};
use omp_tui::{
	Border, Cached, Component, Dim, IntoChildren, Key, Layer, Mouse, OverlayAnchor, OverlayOptions,
	PaintCtx, Prop, Props, Rect, Size, Slot, Ui, UiContext, UiEvent,
	components::{Boxed, Hr},
	dom,
};

use crate::PickerEvent;

const LIST_HINT: &str = "↑/↓ choose · Enter select · type to search · Esc close";
const PROMPT_HINT: &str = "Enter submit · Esc cancel";
/// Shared rounded overlay chrome with a title inset into its top rule.
///
/// Dialogs and pickers add their content through [`OverlayPanel::child`]
/// instead of constructing their own outer border.
pub struct OverlayPanel {
	inner: Boxed,
}

impl OverlayPanel {
	/// Creates an empty titled panel with the standard horizontal inset.
	pub fn new(title: impl IntoStr) -> Self {
		let title: Str = title.into_str();
		Self {
			inner: Boxed::new()
				.with(Prop::Border, Border::Round)
				.with(Prop::Title, title)
				.with(Prop::PadX, 1_u16),
		}
	}

	/// Adds vertical padding inside the shared border.
	pub fn pad_y(mut self, rows: u16) -> Self {
		self.inner.props_mut().set(Prop::PadY, rows);
		self
	}

	/// Appends panel content.
	pub fn child(mut self, children: impl IntoChildren) -> Self {
		self.inner = self.inner.child(children);
		self
	}
}

impl Component for OverlayPanel {
	fn props(&self) -> &Props {
		self.inner.props()
	}

	fn props_mut(&mut self) -> &mut Props {
		self.inner.props_mut()
	}

	fn slot(&self) -> Slot {
		self.inner.slot()
	}

	fn children(&self) -> &[Cached] {
		self.inner.children()
	}

	fn children_mut(&mut self) -> &mut [Cached] {
		self.inner.children_mut()
	}

	fn measure(&mut self, ctx: &UiContext) -> (u16, u16) {
		self.inner.measure(ctx)
	}

	fn height(&mut self, ctx: &UiContext, width: u16) -> u16 {
		self.inner.height(ctx, width)
	}

	fn place(&mut self, ctx: &UiContext, content: Rect) {
		self.inner.place(ctx, content);
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		self.inner.paint(pc, rect);
	}
}

/// Creates the standard rounded horizontal section rule for an overlay panel.
pub fn panel_divider() -> Hr {
	Hr::new().with(Prop::Border, Border::Round)
}

/// One host-supplied row for [`ListPicker`].
#[derive(Clone, Debug)]
pub struct ListRow {
	/// Stable value associated with this row.
	pub key:    Str,
	/// Primary visible label.
	pub label:  Str,
	/// Secondary visible detail.
	pub detail: Str,
}

/// Single-column filterable picker for sessions, rewind targets, or providers.
pub struct ListPicker {
	ui:        Ui,
	title:     Str,
	rows:      Vec<ListRow>,
	current:   usize,
	ctx:       UiContext,
	options:   OverlayOptions,
	query:     Str,
	list_rows: u16,
}

impl ListPicker {
	/// Opens a titled picker over host-supplied rows.
	pub fn open(title: impl IntoStr, rows: &[ListRow], current: usize, ctx: &UiContext) -> Self {
		let title = title.into_str();
		let rows = rows.to_vec();
		let current = current.min(rows.len().saturating_sub(1));
		Self {
			ui: build_list(&title, &rows, current, "", 7, 64, ctx),
			title,
			rows,
			current,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(64))
				.z(10),
			query: Str::default(),
			list_rows: 7,
		}
	}

	/// Routes a key into the filter and list.
	pub fn handle_key(&mut self, key: Key) -> PickerEvent {
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	/// Routes pasted query text into the filter.
	pub fn handle_paste(&mut self, text: &str) -> PickerEvent {
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	/// Routes a pointer event; clicking outside dismisses the picker.
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

	/// Returns a centered, viewport-responsive composited layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(4).clamp(1, 72);
		let rows = (viewport.height / 2).saturating_sub(4).max(5);
		if rows != self.list_rows {
			self.list_rows = rows;
			self
				.ui
				.set_prop("list-picker", Prop::H, rows.saturating_add(1));
		}
		if self.ui.frame().size().width != width {
			self.ui = build_list(
				&self.title,
				&self.rows,
				self.current,
				&self.query,
				self.list_rows,
				width,
				&self.ctx,
			);
		}
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	/// Returns the stable key for a picked row index.
	pub fn key(&self, index: usize) -> Option<&Str> {
		self.rows.get(index).map(|row| &row.key)
	}

	/// Returns the row index currently under the selection cursor.
	pub const fn selected(&self) -> usize {
		self.current
	}

	/// Returns the stable key of the row under the selection cursor.
	pub fn selected_key(&self) -> Option<&Str> {
		self.rows.get(self.current).map(|row| &row.key)
	}

	/// Replaces one row's secondary detail and repaints the list, keeping
	/// the filter query and cursor; used for in-place confirm prompts.
	pub fn set_row_detail(&mut self, index: usize, detail: Str) {
		let Some(row) = self.rows.get_mut(index) else {
			return;
		};
		row.detail = detail;
		let width = self.ui.frame().size().width.max(1);
		self.ui = build_list(
			&self.title,
			&self.rows,
			self.current,
			&self.query,
			self.list_rows,
			width,
			&self.ctx,
		);
	}

	fn route(&mut self, event: UiEvent) -> PickerEvent {
		match event {
			UiEvent::Cancel => PickerEvent::Close,
			UiEvent::Changed { value, .. } => value
				.as_str()
				.parse()
				.map_or(PickerEvent::Consumed, PickerEvent::Pick),
			UiEvent::Filtered { query, value, .. } => {
				self.query = query;
				if let Some(index) = value.as_ref().and_then(|value| value.as_str().parse().ok()) {
					self.current = index;
				}
				PickerEvent::Consumed
			},
			UiEvent::Highlighted { value, .. } => {
				if let Ok(index) = value.as_str().parse() {
					self.current = index;
				}
				PickerEvent::Consumed
			},
			UiEvent::None
			| UiEvent::Submit
			| UiEvent::Pressed(_)
			| UiEvent::Copied(_)
			| UiEvent::TreeActivated { .. }
			| UiEvent::TreeToggled { .. }
			| UiEvent::TreeAction { .. }
			| UiEvent::DiffAction { .. } => PickerEvent::Consumed,
		}
	}
}

fn build_list(
	title: &str,
	rows: &[ListRow],
	current: usize,
	query: &str,
	list_rows: u16,
	width: u16,
	ctx: &UiContext,
) -> Ui {
	let display: Vec<_> = rows
		.iter()
		.enumerate()
		.map(|(index, row)| {
			(
				sf!("{index}"),
				sf!("{} {}", row.label, row.detail),
				row.label.clone(),
				row.detail.clone(),
				index == current,
			)
		})
		.collect();
	let title = Str::new(title);
	let seed = Str::new(query);
	let height = list_rows.saturating_add(1);
	Ui::from_root(
		OverlayPanel::new(title).child(dom! {
			<col>
				<select id="list-picker" filter={seed} h={height}>
					for (value, haystack, label, detail, selected) in display {
						<option value={value} label={haystack} recommended={selected}>
							<td truncate><pre fg=fg>{label}</pre></td>
							<td truncate grow><pre fg=muted>{detail}</pre></td>
						</option>
					}
				</select>
				{panel_divider()}
				<text dim truncate>{LIST_HINT}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

/// Result of routing input through a [`PromptOverlay`].
pub enum PromptEvent {
	/// Event consumed while the prompt remains open.
	Consumed,
	/// Prompt cancelled without a value.
	Cancel,
	/// Prompt submitted with the unmasked value.
	Submit(Str),
}

/// Small rounded-box input overlay for backend authentication prompts.
pub struct PromptOverlay {
	ui:      Ui,
	title:   Str,
	masked:  bool,
	ctx:     UiContext,
	options: OverlayOptions,
	value:   Str,
}

impl PromptOverlay {
	/// Opens a plain or masked prompt and focuses its input.
	pub fn open(title: impl IntoStr, masked: bool, ctx: &UiContext) -> Self {
		let title = title.into_str();
		let mut ui = build_prompt(&title, masked, 56, ctx);
		ui.focus_first();
		Self {
			ui,
			title,
			masked,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(56))
				.z(20),
			value: Str::default(),
		}
	}

	/// Opens a plain prompt with an editable suggested value.
	pub fn open_prefilled(title: impl IntoStr, value: impl IntoStr, ctx: &UiContext) -> Self {
		let mut prompt = Self::open(title, false, ctx);
		let value = value.into_str();
		prompt.ui.blur();
		prompt.ui.set_text("prompt-input", value.as_str());
		prompt.ui.focus_first();
		prompt.value = value;
		prompt
	}

	/// Routes a key into the prompt.
	pub fn handle_key(&mut self, key: Key) -> PromptEvent {
		if key == Key::Esc {
			return PromptEvent::Cancel;
		}
		if key == Key::Enter {
			return PromptEvent::Submit(self.value.clone());
		}
		let event = self.ui.handle_key(key);
		self.sync_value();
		self.route(event)
	}

	/// Routes pasted text into the prompt input.
	pub fn handle_paste(&mut self, text: &str) -> PromptEvent {
		let event = self.ui.handle_paste(text);
		self.sync_value();
		self.route(event)
	}

	/// Routes a pointer event; clicking outside cancels the prompt.
	pub fn handle_mouse(&mut self, col: u16, row: u16, kind: Mouse, viewport: Size) -> PromptEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => {
				self.sync_value();
				self.route(event)
			},
			None if kind == Mouse::Click => PromptEvent::Cancel,
			None => PromptEvent::Consumed,
		}
	}

	/// Returns a centered rounded-box composited layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(4).clamp(1, 56);
		if self.ui.frame().size().width != width {
			let value = self.value.clone();
			self.ui = build_prompt(&self.title, self.masked, width, &self.ctx);
			self.ui.set_text("prompt-input", value.as_str());
			self.ui.focus_first();
		}
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn route(&self, event: UiEvent) -> PromptEvent {
		match event {
			UiEvent::Cancel => PromptEvent::Cancel,
			UiEvent::Submit => PromptEvent::Submit(self.value.clone()),
			UiEvent::None
			| UiEvent::Changed { .. }
			| UiEvent::Highlighted { .. }
			| UiEvent::Filtered { .. }
			| UiEvent::Pressed(_)
			| UiEvent::Copied(_)
			| UiEvent::TreeActivated { .. }
			| UiEvent::TreeToggled { .. }
			| UiEvent::TreeAction { .. }
			| UiEvent::DiffAction { .. } => PromptEvent::Consumed,
		}
	}

	fn sync_value(&mut self) {
		if let Some(value) = self.ui.values()["prompt-input"].as_str() {
			self.value = Str::new(value);
		}
	}
}

fn build_prompt(title: &str, masked: bool, width: u16, ctx: &UiContext) -> Ui {
	let title = Str::new(title);
	Ui::from_root(
		OverlayPanel::new(title).pad_y(1).child(dom! {
			<col>
				<input id="prompt-input" submit mask={masked} placeholder="Enter value"/>
				{panel_divider()}
				<text dim truncate>{PROMPT_HINT}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn overlay_panel_owns_standard_chrome() {
		let panel = OverlayPanel::new("Models");
		assert_eq!(panel.props().border(), Some(Border::Round));
		assert_eq!(panel.props().title().map(Str::as_str), Some("Models"));
		assert_eq!(panel.props().pad(), (0, 1));
		assert_eq!(panel_divider().props().border(), Some(Border::Round));
	}

	#[test]
	fn list_picker_keeps_host_keys_out_of_option_values() {
		let rows = vec![ListRow {
			key:    sf!("session/opaque"),
			label:  sf!("Session"),
			detail: sf!("today"),
		}];
		let picker = ListPicker::open("Resume", &rows, 0, &UiContext::default());
		assert_eq!(picker.key(0).map(Str::as_str), Some("session/opaque"));
	}

	#[test]
	fn prefilled_prompt_submits_default_or_custom_destination() {
		let mut suggested =
			PromptOverlay::open_prefilled("Save", "TOPIC_PLAN.md", &UiContext::default());
		match suggested.handle_key(Key::Enter) {
			PromptEvent::Submit(path) => assert_eq!(path, "TOPIC_PLAN.md"),
			PromptEvent::Consumed | PromptEvent::Cancel => panic!("prefilled prompt did not submit"),
		}

		let mut custom =
			PromptOverlay::open_prefilled("Save", "TOPIC_PLAN.md", &UiContext::default());
		for _ in 0.."TOPIC_PLAN.md".len() {
			let _ = custom.handle_key(Key::Backspace);
		}
		for character in "plans/custom.md".chars() {
			let _ = custom.handle_key(Key::Char(character));
		}
		assert!(matches!(
			custom.handle_key(Key::Enter),
			PromptEvent::Submit(path) if path == "plans/custom.md"
		));
	}

	#[test]
	fn masked_prompt_returns_original_value() {
		let mut prompt = PromptOverlay::open("Token", true, &UiContext::default());
		for ch in "secret".chars() {
			assert!(matches!(prompt.handle_key(Key::Char(ch)), PromptEvent::Consumed));
		}
		match prompt.handle_key(Key::Enter) {
			PromptEvent::Submit(value) => assert_eq!(value.as_str(), "secret"),
			_ => panic!("prompt did not submit"),
		}
	}
}
