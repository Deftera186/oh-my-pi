//! Correlated retained extension dialogs and overlays.

use omp_core::Str;
use omp_proto::omp::ui::v1::{
	Dialog, DialogOutcome, OverlayEvent, ShowOverlay, UiError, UiResponse, Values, ui_response,
};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Props, Size, Ui, UiContext, UiEvent, dom,
};

use crate::{OverlayPanel, panel_divider};

/// Result of routing input through an extension-owned modal surface.
#[derive(Clone, Debug, PartialEq)]
pub enum ExtensionModalEvent {
	/// Input was consumed without a protocol event.
	Consumed,
	/// A dialog settled with this canonical response.
	Dialog(UiResponse),
	/// A retained overlay emitted an interaction event.
	Overlay(OverlayEvent),
	/// The user requested idempotent overlay closure.
	CloseOverlay(Str),
}

/// One correlated retained extension dialog.
pub struct ExtensionDialog {
	ui:            Ui,
	options:       OverlayOptions,
	multi:         bool,
	choice_dialog: bool,
}

impl ExtensionDialog {
	/// Builds a dialog from the canonical UI protocol.
	pub fn open(dialog: &Dialog, ctx: &UiContext) -> Result<Self, UiError> {
		let width = 72;
		let multi = dialog.kind.contains("multi") || dialog.kind == "form";
		let choice_dialog = dialog.content.is_none();
		let mut ui = if let Some(content) = dialog.content.as_ref() {
			let source = std::str::from_utf8(&content.source)
				.map_err(|_| ui_error("invalid_tml", "dialog TML is not UTF-8"))?;
			Ui::from_extension_markup(Str::new(source), width, ctx.clone())
				.map_err(|error| ui_error("invalid_tml", &error.to_string()))?
		} else {
			let title = Str::new(dialog.title.as_str());
			let choices = dialog.choices.iter().map(Str::new).collect::<Vec<_>>();
			let rows = u16::try_from(choices.len())
				.unwrap_or(u16::MAX)
				.clamp(2, 12);
			Ui::from_root(
				OverlayPanel::new(title).child(dom! {
					<col>
						<select id="extension-dialog" multi={multi} h={rows}>
							for choice in choices {
								<option value={choice.clone()} label={choice.clone()}>
									<text>{choice}</text>
								</option>
							}
						</select>
						{panel_divider()}
						<text dim>{if multi { "Space toggle · Enter confirm · Esc cancel" } else { "Enter choose · Esc cancel" }}</text>
					</col>
				}),
				width,
				ctx.clone(),
			)
		};
		ui.focus_first();
		Ok(Self {
			ui,
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(width))
				.z(35),
			multi,
			choice_dialog,
		})
	}

	/// Routes one key through the dialog.
	pub fn handle_key(&mut self, key: Key) -> ExtensionModalEvent {
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	/// Routes pasted text through the dialog.
	pub fn handle_paste(&mut self, text: &str) -> ExtensionModalEvent {
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	/// Routes pointer input; an outside click cancels.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> ExtensionModalEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => self.route(event),
			None if kind == Mouse::Click => ExtensionModalEvent::Dialog(dialog_cancelled()),
			None => ExtensionModalEvent::Consumed,
		}
	}

	/// Returns the centered retained layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		self.options = self.options.width(Dim::Cells(viewport.width.min(72)));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn route(&mut self, event: UiEvent) -> ExtensionModalEvent {
		match event {
			UiEvent::Cancel => ExtensionModalEvent::Dialog(dialog_cancelled()),
			UiEvent::Changed { value, .. } if self.choice_dialog && !self.multi => {
				ExtensionModalEvent::Dialog(dialog_accepted(vec![value]))
			},
			UiEvent::Submit => ExtensionModalEvent::Dialog(dialog_accepted(self.values())),
			_ => ExtensionModalEvent::Consumed,
		}
	}

	fn values(&self) -> Vec<Str> {
		string_values(&self.ui)
	}
}

/// One retained custom extension overlay.
pub struct ExtensionOverlay {
	id:      Str,
	ui:      Ui,
	options: OverlayOptions,
}

impl ExtensionOverlay {
	/// Builds an overlay and assigns its stable host correlation id.
	pub fn open(id: Str, show: &ShowOverlay, ctx: &UiContext) -> Result<Self, UiError> {
		let content = show
			.content
			.as_ref()
			.ok_or_else(|| ui_error("invalid_overlay", "overlay content is required"))?;
		let source = std::str::from_utf8(&content.source)
			.map_err(|_| ui_error("invalid_tml", "overlay TML is not UTF-8"))?;
		let mut ui = Ui::from_extension_markup(Str::new(source), 72, ctx.clone())
			.map_err(|error| ui_error("invalid_tml", &error.to_string()))?;
		ui.focus_first();
		Ok(Self {
			id,
			ui,
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(72))
				.z(35),
		})
	}

	/// Stable overlay id returned to the extension.
	pub const fn id(&self) -> &Str {
		&self.id
	}

	/// Current string values in deterministic component traversal order.
	pub fn values(&self) -> Vec<Str> {
		string_values(&self.ui)
	}

	/// Replaces one text-bearing node from a canonical patch.
	pub fn patch_text(&mut self, node_id: &str, source: &[u8]) -> bool {
		std::str::from_utf8(source).is_ok_and(|text| self.ui.set_text(node_id, text))
	}

	/// Applies validated dynamic properties to one retained node.
	pub fn patch_props(
		&mut self,
		node_id: &str,
		props: &std::collections::BTreeMap<String, omp_proto::omp::ui::v1::PropValue>,
	) -> bool {
		use omp_proto::omp::ui::v1::prop_value;
		let mut changed = false;
		for (name, value) in props {
			let Some(prop) = Props::prop_of(name) else {
				continue;
			};
			let Some(source) = value.value.as_ref().map(|value| match value {
				prop_value::Value::StringValue(value) => value.clone(),
				prop_value::Value::IntegerValue(value) => value.to_string(),
				prop_value::Value::BoolValue(value) => value.to_string(),
				prop_value::Value::NumberValue(value) => value.to_string(),
				prop_value::Value::BytesValue(_) => String::new(),
			}) else {
				continue;
			};
			if !source.is_empty() {
				changed |= self.ui.set_prop(node_id, prop, Str::new(source));
			}
		}
		changed
	}

	/// Transfers keyboard focus into this overlay.
	pub fn focus(&mut self) {
		self.ui.focus_first();
	}

	/// Removes keyboard focus from this overlay.
	pub fn blur(&mut self) {
		self.ui.blur();
	}

	/// Routes one key through the overlay.
	pub fn handle_key(&mut self, key: Key) -> ExtensionModalEvent {
		if key == Key::Esc {
			return ExtensionModalEvent::CloseOverlay(self.id.clone());
		}
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	/// Routes pasted text through the overlay.
	pub fn handle_paste(&mut self, text: &str) -> ExtensionModalEvent {
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	/// Routes pointer input without closing on outside motion/clicks.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> ExtensionModalEvent {
		self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
			.map_or(ExtensionModalEvent::Consumed, |event| self.route(event))
	}

	/// Returns the centered retained layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		self.options = self.options.width(Dim::Cells(viewport.width.min(72)));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn route(&mut self, event: UiEvent) -> ExtensionModalEvent {
		let (kind, value) = match event {
			UiEvent::Changed { value, .. } => ("changed", Some(value)),
			UiEvent::Highlighted { value, .. } => ("highlighted", Some(value)),
			UiEvent::Pressed(id) => ("pressed", Some(id)),
			UiEvent::Submit => ("submit", self.values().into_iter().next()),
			UiEvent::Cancel => return ExtensionModalEvent::CloseOverlay(self.id.clone()),
			_ => return ExtensionModalEvent::Consumed,
		};
		ExtensionModalEvent::Overlay(OverlayEvent {
			overlay_id: self.id.to_string(),
			kind:       kind.to_owned(),
			value:      value.map(|value| value.to_string()),
		})
	}
}

/// Creates a protocol error without losing correlation at the host boundary.
pub fn ui_error(code: &str, message: &str) -> UiError {
	UiError { code: code.to_owned(), message: message.to_owned() }
}

/// Wraps one UI error as a canonical response.
pub fn error_response(error: UiError) -> UiResponse {
	UiResponse { kind: Some(ui_response::Kind::Error(error)), props: None }
}

/// Wraps current overlay values as a canonical response.
pub fn values_response(values: Vec<Str>) -> UiResponse {
	UiResponse {
		kind:  Some(ui_response::Kind::Values(Values {
			values: values.into_iter().map(|value| value.to_string()).collect(),
		})),
		props: None,
	}
}

fn dialog_accepted(values: Vec<Str>) -> UiResponse {
	let value = values.first().map(ToString::to_string);
	UiResponse {
		kind:  Some(ui_response::Kind::DialogOutcome(DialogOutcome {
			accepted: true,
			cancelled: false,
			value,
			values: values.into_iter().map(|item| item.to_string()).collect(),
		})),
		props: None,
	}
}

fn dialog_cancelled() -> UiResponse {
	UiResponse {
		kind:  Some(ui_response::Kind::DialogOutcome(DialogOutcome {
			accepted:  false,
			cancelled: true,
			value:     None,
			values:    Vec::new(),
		})),
		props: None,
	}
}

fn string_values(ui: &Ui) -> Vec<Str> {
	let serde_json::Value::Object(values) = ui.values() else {
		return Vec::new();
	};
	values
		.into_values()
		.flat_map(|value| match value {
			serde_json::Value::String(value) => vec![Str::new(value)],
			serde_json::Value::Array(values) => values
				.into_iter()
				.filter_map(|value| value.as_str().map(Str::new))
				.collect(),
			_ => Vec::new(),
		})
		.collect()
}
