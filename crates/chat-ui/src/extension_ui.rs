//! Correlated retained extension dialogs and overlays.

use std::time::{Duration, Instant};

use omp_core::Str;
use omp_proto::omp::{
	inference::v1::{Value as ProtoValue, ValueMap, value},
	ui::v1::{
		Dialog, DialogOutcome, OverlayEvent, ShowOverlay, UiError, UiResponse, Values, ui_response,
	},
};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Prop, Props, Size, Ui, UiContext,
	UiEvent,
	components::{
		Button, Col, Countdown, EditorPane, Field, Form, Input, Row, Select, SelectOption, TextLeaf,
		Wizard,
	},
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
	kind:          Str,
	choice_dialog: bool,
	ask_ids:       Vec<Str>,
	clock_start:   Duration,
	opened_at:     Instant,
}

impl ExtensionDialog {
	/// Builds a dialog from the canonical UI protocol.
	pub fn open(dialog: &Dialog, ctx: &UiContext) -> Result<Self, UiError> {
		let width = 72;
		let props = dialog
			.props
			.as_ref()
			.map(proto_map_to_json)
			.unwrap_or_default();
		let kind = Str::new(dialog.kind.as_str());
		let choice_dialog =
			dialog.content.is_none() && matches!(kind.as_str(), "select" | "multi_select" | "ask");
		let (mut ui, ask_ids) = if let Some(content) = dialog.content.as_ref()
			&& !matches!(kind.as_str(), "confirm" | "input" | "editor" | "form" | "ask_user")
		{
			let source = std::str::from_utf8(&content.source)
				.map_err(|_| ui_error("invalid_tml", "dialog TML is not UTF-8"))?;
			(
				Ui::from_extension_markup(Str::new(source), width, ctx.clone())
					.map_err(|error| ui_error("invalid_tml", &error.to_string()))?,
				Vec::new(),
			)
		} else {
			build_dialog(dialog, &props, width, ctx)
		};
		ui.focus_first();
		Ok(Self {
			ui,
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(width))
				.z(35),
			kind,
			choice_dialog,
			ask_ids,
			clock_start: ctx.now,
			opened_at: Instant::now(),
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
		let now = self.clock_start.saturating_add(self.opened_at.elapsed());
		self.ui.tick(now);
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn route(&mut self, event: UiEvent) -> ExtensionModalEvent {
		match event {
			UiEvent::Cancel => ExtensionModalEvent::Dialog(dialog_cancelled()),
			UiEvent::Pressed(id) if self.kind == "confirm" && id == "dialog-yes" => {
				ExtensionModalEvent::Dialog(dialog_confirm(true))
			},
			UiEvent::Pressed(id) if self.kind == "confirm" && id == "dialog-no" => {
				ExtensionModalEvent::Dialog(dialog_confirm(false))
			},
			UiEvent::Changed { value, .. } if self.choice_dialog && self.kind != "multi_select" => {
				ExtensionModalEvent::Dialog(dialog_value(value.as_str()))
			},
			UiEvent::Submit if self.kind == "form" => {
				let answers = self
					.ui
					.values()
					.get("answers")
					.and_then(serde_json::Value::as_object)
					.cloned()
					.unwrap_or_default();
				ExtensionModalEvent::Dialog(dialog_answers(answers))
			},
			UiEvent::Submit if self.kind == "ask_user" => {
				ExtensionModalEvent::Dialog(dialog_answers(self.ask_answers()))
			},
			UiEvent::Submit if self.kind == "multi_select" => {
				ExtensionModalEvent::Dialog(dialog_values(self.values()))
			},
			UiEvent::Submit => {
				let value = self
					.ui
					.values()
					.get("value")
					.and_then(serde_json::Value::as_str)
					.map(Str::new)
					.or_else(|| self.values().into_iter().next());
				ExtensionModalEvent::Dialog(dialog_optional_value(value))
			},
			_ => ExtensionModalEvent::Consumed,
		}
	}

	fn values(&self) -> Vec<Str> {
		string_values(&self.ui)
	}

	fn ask_answers(&self) -> serde_json::Map<String, serde_json::Value> {
		let values = self.ui.values();
		self
			.ask_ids
			.iter()
			.map(|id| {
				let selected = values
					.get(&format!("question:{id}"))
					.and_then(|value| value.get("selected"))
					.map(json_strings)
					.unwrap_or_default();
				let freeform = values
					.get(&format!("freeform:{id}"))
					.and_then(nonempty_json_string);
				let note = values
					.get(&format!("note:{id}"))
					.and_then(nonempty_json_string);
				(
					id.to_string(),
					serde_json::json!({
						"selected": selected,
						"freeform": freeform,
						"note": note,
						"timed_out": false,
					}),
				)
			})
			.collect()
	}
}

fn build_dialog(
	dialog: &Dialog,
	props: &serde_json::Map<String, serde_json::Value>,
	width: u16,
	ctx: &UiContext,
) -> (Ui, Vec<Str>) {
	let title = Str::new(dialog.title.as_str());
	let hint = || {
		TextLeaf::new()
			.with(Prop::Dim, true)
			.text("Tab: move; Enter: activate; Esc: dismiss")
	};
	let panel = match dialog.kind.as_str() {
		"confirm" => {
			let message = prop(props, "message")
				.and_then(serde_json::Value::as_str)
				.unwrap_or_default();
			let mut content = Col::new()
				.child(
					TextLeaf::new()
						.with(Prop::Wrap, true)
						.text(Str::new(message)),
				)
				.child(
					Row::new()
						.with(Prop::Justify, "center")
						.child(
							Button::new()
								.with(Prop::Id, "dialog-yes")
								.with(Prop::Accent, true)
								.child("Yes"),
						)
						.child(Button::new().with(Prop::Id, "dialog-no").child("No")),
				);
			if option_bool(props, "countdown", true)
				&& let Some(timeout) = option_duration(props, "timeout")
			{
				content = content.child(Countdown::new("Time remaining", ctx.now, timeout));
			}
			content = content.child(panel_divider()).child(hint());
			Ui::from_root(OverlayPanel::new(title).child(content), width, ctx.clone())
		},
		"input" => {
			let mut input = Input::new()
				.with(Prop::Id, "value")
				.with(Prop::Value, prop_str(props, "prefill"))
				.with(Prop::Placeholder, prop_str(props, "placeholder"));
			if prop_bool(props, "mask", false) {
				input = input.with(Prop::Mask, true);
			}
			if let Some(pattern) = prop(props, "match").and_then(serde_json::Value::as_str) {
				input = input.with(Prop::Match, Str::new(pattern));
			}
			let wizard = Wizard::new()
				.with(Prop::Id, "dialog-wizard")
				.with(Prop::Submit, true)
				.step(title.clone(), input);
			let content = Col::new()
				.child(wizard)
				.child(panel_divider())
				.child(hint());
			Ui::from_root(OverlayPanel::new(title).child(content), width, ctx.clone())
		},
		"editor" => {
			let mut step = Col::new().child(
				EditorPane::new()
					.with(Prop::Id, "value")
					.with(Prop::Value, prop_str(props, "prefill"))
					.with(Prop::H, 10_u16),
			);
			if let Some(syntax) = prop(props, "syntax").and_then(serde_json::Value::as_str) {
				step = step.child(
					TextLeaf::new()
						.with(Prop::Dim, true)
						.text(Str::from(format!("Syntax: {syntax}"))),
				);
			}
			let wizard = Wizard::new()
				.with(Prop::Id, "dialog-wizard")
				.with(Prop::Submit, true)
				.step(title.clone(), step);
			let content = Col::new()
				.child(wizard)
				.child(panel_divider())
				.child(hint());
			Ui::from_root(OverlayPanel::new(title).child(content), width, ctx.clone())
		},
		"form" => {
			let mut form = Form::new().with(Prop::Id, "answers");
			if let Some(fields) = prop(props, "fields").and_then(serde_json::Value::as_array) {
				for field in fields {
					if let Some(field) = dialog_field(field) {
						form = form.field(field);
					}
				}
			}
			let wizard = Wizard::new()
				.with(Prop::Id, "dialog-wizard")
				.with(Prop::Submit, true)
				.step(title.clone(), form);
			let content = Col::new()
				.child(wizard)
				.child(panel_divider())
				.child(hint());
			Ui::from_root(OverlayPanel::new(title).child(content), width, ctx.clone())
		},
		"ask_user" => {
			let questions = match prop(props, "questions").or_else(|| prop(props, "question")) {
				Some(serde_json::Value::Array(questions)) => questions.clone(),
				Some(question @ serde_json::Value::Object(_)) => vec![question.clone()],
				_ => Vec::new(),
			};
			let mut wizard = Wizard::new()
				.with(Prop::Id, "dialog-wizard")
				.with(Prop::Submit, true);
			let mut ids = Vec::with_capacity(questions.len());
			for (index, question) in questions.iter().enumerate() {
				let Some(object) = question.as_object() else {
					continue;
				};
				let id = object
					.get("id")
					.and_then(serde_json::Value::as_str)
					.map(Str::new)
					.unwrap_or_else(|| Str::from(format!("question-{}", index + 1)));
				let prompt = object
					.get("question")
					.and_then(serde_json::Value::as_str)
					.unwrap_or(id.as_str());
				let step_title = object
					.get("header")
					.and_then(serde_json::Value::as_str)
					.unwrap_or(prompt);
				let mut step = Col::new().child(
					TextLeaf::new()
						.with(Prop::Wrap, true)
						.text(Str::new(prompt)),
				);
				let options = object
					.get("options")
					.and_then(serde_json::Value::as_array)
					.cloned()
					.unwrap_or_default();
				if !options.is_empty() {
					let kind = if object
						.get("multi")
						.and_then(serde_json::Value::as_bool)
						.unwrap_or(false)
					{
						"multi"
					} else {
						"select"
					};
					let values = options
						.iter()
						.filter_map(option_value)
						.collect::<Vec<_>>()
						.join(" ");
					let recommended = object
						.get("recommended")
						.and_then(serde_json::Value::as_str)
						.unwrap_or_default();
					let selected = if recommended.is_empty() {
						option_value(&options[0]).unwrap_or_default()
					} else {
						recommended.to_owned()
					};
					let field = Field::new()
						.with(Prop::Id, "selected")
						.with(Prop::Kind, kind)
						.with(Prop::Options, Str::new(values))
						.with(Prop::Value, Str::new(selected))
						.label("Answer");
					step = step.child(
						Form::new()
							.with(Prop::Id, Str::from(format!("question:{id}")))
							.field(field),
					);
				}
				if object
					.get("allow_freeform")
					.and_then(serde_json::Value::as_bool)
					.unwrap_or(true)
				{
					step = step.child(
						Input::new()
							.with(Prop::Id, Str::from(format!("freeform:{id}")))
							.with(Prop::Placeholder, "Other answer"),
					);
				}
				if object
					.get("allow_note")
					.and_then(serde_json::Value::as_bool)
					.unwrap_or(false)
				{
					step = step.child(
						Input::new()
							.with(Prop::Id, Str::from(format!("note:{id}")))
							.with(Prop::Placeholder, "Optional note"),
					);
				}
				wizard = wizard.step(Str::new(step_title), step);
				ids.push(id);
			}
			if ids.is_empty() {
				wizard = wizard.step(title.clone(), TextLeaf::new().text("No questions"));
			}
			let content = Col::new()
				.child(wizard)
				.child(panel_divider())
				.child(hint());
			return (Ui::from_root(OverlayPanel::new(title).child(content), width, ctx.clone()), ids);
		},
		_ => {
			let multi = dialog.kind == "multi_select";
			let mut select = Select::new()
				.with(Prop::Id, "value")
				.with(Prop::Multi, multi)
				.with(
					Prop::H,
					u16::try_from(dialog_items(dialog, props).len())
						.unwrap_or(u16::MAX)
						.clamp(2, 12),
				);
			let checked = prop(props, "checked").map(json_strings).unwrap_or_default();
			for item in dialog_items(dialog, props) {
				let selected = multi && checked.iter().any(|checked| checked == item.value.as_str());
				let mut option = SelectOption::new()
					.with(Prop::Value, item.value)
					.label(item.label);
				if selected {
					option = option.with(Prop::Selected, true);
				}
				if let Some(desc) = item.desc {
					option = option.with(Prop::Desc, desc);
				}
				if item.recommended {
					option = option.with(Prop::Recommended, true);
				}
				select = select.option(option);
			}
			let content = Col::new()
				.child(select)
				.child(panel_divider())
				.child(hint());
			Ui::from_root(OverlayPanel::new(title).child(content), width, ctx.clone())
		},
	};
	(panel, Vec::new())
}

struct DialogItem {
	value:       Str,
	label:       Str,
	desc:        Option<Str>,
	recommended: bool,
}

fn dialog_items(
	dialog: &Dialog,
	props: &serde_json::Map<String, serde_json::Value>,
) -> Vec<DialogItem> {
	let structured = prop(props, "items")
		.or_else(|| prop(props, "choices"))
		.and_then(serde_json::Value::as_array);
	if let Some(items) = structured {
		return items
			.iter()
			.filter_map(|item| {
				if let Some(value) = item.as_str() {
					return Some(DialogItem {
						value:       Str::new(value),
						label:       Str::new(value),
						desc:        None,
						recommended: false,
					});
				}
				let item = item.as_object()?;
				let value = item.get("value")?.as_str()?;
				Some(DialogItem {
					value:       Str::new(value),
					label:       Str::new(
						item
							.get("label")
							.and_then(serde_json::Value::as_str)
							.unwrap_or(value),
					),
					desc:        item
						.get("desc")
						.and_then(serde_json::Value::as_str)
						.map(Str::new),
					recommended: item
						.get("recommended")
						.and_then(serde_json::Value::as_bool)
						.unwrap_or(false),
				})
			})
			.collect();
	}
	dialog
		.choices
		.iter()
		.map(|choice| DialogItem {
			value:       Str::new(choice),
			label:       Str::new(choice),
			desc:        None,
			recommended: false,
		})
		.collect()
}

fn dialog_field(value: &serde_json::Value) -> Option<Field> {
	let object = value.as_object()?;
	let id = object.get("id")?.as_str()?;
	let kind = object
		.get("kind")
		.and_then(serde_json::Value::as_str)
		.unwrap_or("text");
	let mut field = Field::new()
		.with(Prop::Id, Str::new(id))
		.with(Prop::Kind, Str::new(kind))
		.label(Str::new(
			object
				.get("label")
				.and_then(serde_json::Value::as_str)
				.unwrap_or(id),
		));
	if let Some(desc) = object.get("desc").and_then(serde_json::Value::as_str) {
		field = field.with(Prop::Desc, Str::new(desc));
	}
	if let Some(value) = object.get("value") {
		field = field.with(Prop::Value, Str::new(json_display(value)));
	}
	if let Some(options) = object.get("options").and_then(serde_json::Value::as_array) {
		field = field.with(
			Prop::Options,
			Str::new(
				options
					.iter()
					.filter_map(option_value)
					.collect::<Vec<_>>()
					.join(" "),
			),
		);
	}
	for (name, prop_name) in [("min", Prop::Min), ("max", Prop::Max), ("step", Prop::Step)] {
		if let Some(value) = object.get(name).and_then(serde_json::Value::as_i64) {
			field = field.with(prop_name, value);
		}
	}
	if object
		.get("required")
		.and_then(serde_json::Value::as_bool)
		.unwrap_or(false)
	{
		field = field.with(Prop::Required, true);
	}
	if let Some(pattern) = object.get("match").and_then(serde_json::Value::as_str) {
		field = field.with(Prop::Match, Str::new(pattern));
	}
	Some(field)
}

fn option_value(value: &serde_json::Value) -> Option<String> {
	value
		.as_str()
		.map(str::to_owned)
		.or_else(|| value.get("value")?.as_str().map(str::to_owned))
}

fn prop<'a>(
	props: &'a serde_json::Map<String, serde_json::Value>,
	name: &str,
) -> Option<&'a serde_json::Value> {
	props
		.get(name)
		.or_else(|| props.get("options")?.as_object()?.get(name))
}

fn prop_str(props: &serde_json::Map<String, serde_json::Value>, name: &str) -> Str {
	Str::new(
		prop(props, name)
			.and_then(serde_json::Value::as_str)
			.unwrap_or_default(),
	)
}

fn prop_bool(
	props: &serde_json::Map<String, serde_json::Value>,
	name: &str,
	default: bool,
) -> bool {
	prop(props, name)
		.and_then(serde_json::Value::as_bool)
		.unwrap_or(default)
}

fn option_bool(
	props: &serde_json::Map<String, serde_json::Value>,
	name: &str,
	default: bool,
) -> bool {
	props
		.get("options")
		.and_then(serde_json::Value::as_object)
		.and_then(|options| options.get(name))
		.and_then(serde_json::Value::as_bool)
		.unwrap_or(default)
}

fn option_duration(
	props: &serde_json::Map<String, serde_json::Value>,
	name: &str,
) -> Option<Duration> {
	let value = props
		.get("options")
		.and_then(serde_json::Value::as_object)
		.and_then(|options| options.get(name))?;
	if let Some(milliseconds) = value.as_u64() {
		return Some(Duration::from_millis(milliseconds));
	}
	let source = value.as_str()?;
	let split = source
		.find(|character: char| !character.is_ascii_digit())
		.unwrap_or(source.len());
	let amount = source[..split].parse::<u64>().ok()?;
	match &source[split..] {
		"ms" => Some(Duration::from_millis(amount)),
		"s" => Some(Duration::from_secs(amount)),
		"m" => Some(Duration::from_secs(amount.saturating_mul(60))),
		"h" => Some(Duration::from_secs(amount.saturating_mul(3600))),
		_ => None,
	}
}

fn json_display(value: &serde_json::Value) -> String {
	match value {
		serde_json::Value::String(value) => value.clone(),
		serde_json::Value::Bool(value) => value.to_string(),
		serde_json::Value::Number(value) => value.to_string(),
		serde_json::Value::Array(values) => values
			.iter()
			.filter_map(serde_json::Value::as_str)
			.collect::<Vec<_>>()
			.join(" "),
		serde_json::Value::Null | serde_json::Value::Object(_) => String::new(),
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

fn dialog_confirm(accepted: bool) -> UiResponse {
	dialog_outcome(accepted, false, None, Vec::new(), None, None)
}

fn dialog_value(value: &str) -> UiResponse {
	dialog_optional_value(Some(Str::new(value)))
}

fn dialog_optional_value(value: Option<Str>) -> UiResponse {
	dialog_outcome(true, false, value.map(|value| value.to_string()), Vec::new(), None, None)
}

fn dialog_values(values: Vec<Str>) -> UiResponse {
	dialog_outcome(
		true,
		false,
		None,
		values.into_iter().map(|value| value.to_string()).collect(),
		None,
		None,
	)
}

fn dialog_answers(answers: serde_json::Map<String, serde_json::Value>) -> UiResponse {
	dialog_outcome(
		true,
		false,
		None,
		Vec::new(),
		Some(ValueMap {
			fields: answers
				.into_iter()
				.map(|(name, value)| (name, json_to_proto(value)))
				.collect(),
		}),
		None,
	)
}

fn dialog_cancelled() -> UiResponse {
	dialog_outcome(false, true, None, Vec::new(), None, Some("dismissed".to_owned()))
}

fn dialog_outcome(
	accepted: bool,
	cancelled: bool,
	value: Option<String>,
	values: Vec<String>,
	answers: Option<ValueMap>,
	reason: Option<String>,
) -> UiResponse {
	UiResponse {
		kind:  Some(ui_response::Kind::DialogOutcome(DialogOutcome {
			accepted,
			cancelled,
			value,
			values,
			answers,
			reason,
		})),
		props: None,
	}
}

fn proto_map_to_json(map: &ValueMap) -> serde_json::Map<String, serde_json::Value> {
	map.fields
		.iter()
		.map(|(name, value)| (name.clone(), proto_to_json(value)))
		.collect()
}

fn proto_to_json(value: &ProtoValue) -> serde_json::Value {
	match value.kind.as_ref() {
		Some(value::Kind::Null(_)) | None => serde_json::Value::Null,
		Some(value::Kind::Int(value)) => (*value).into(),
		Some(value::Kind::Double(value)) => serde_json::Number::from_f64(*value)
			.map_or(serde_json::Value::Null, serde_json::Value::Number),
		Some(value::Kind::Bool(value)) => (*value).into(),
		Some(value::Kind::String(value)) => value.clone().into(),
		Some(value::Kind::List(value)) => value.values.iter().map(proto_to_json).collect(),
		Some(value::Kind::Map(value)) => serde_json::Value::Object(proto_map_to_json(value)),
		Some(value::Kind::Uint(value)) => (*value).into(),
	}
}

fn json_to_proto(value: serde_json::Value) -> ProtoValue {
	let kind = match value {
		serde_json::Value::Null => value::Kind::Null(true),
		serde_json::Value::Bool(value) => value::Kind::Bool(value),
		serde_json::Value::String(value) => value::Kind::String(value),
		serde_json::Value::Number(value) => value.as_i64().map_or_else(
			|| {
				value.as_u64().map_or_else(
					|| value::Kind::Double(value.as_f64().unwrap_or_default()),
					value::Kind::Uint,
				)
			},
			value::Kind::Int,
		),
		serde_json::Value::Array(values) => {
			value::Kind::List(omp_proto::omp::inference::v1::ValueList {
				values: values.into_iter().map(json_to_proto).collect(),
			})
		},
		serde_json::Value::Object(fields) => value::Kind::Map(ValueMap {
			fields: fields
				.into_iter()
				.map(|(name, value)| (name, json_to_proto(value)))
				.collect(),
		}),
	};
	ProtoValue { kind: Some(kind) }
}

fn json_strings(value: &serde_json::Value) -> Vec<String> {
	match value {
		serde_json::Value::String(value) if !value.is_empty() => vec![value.clone()],
		serde_json::Value::Array(values) => values
			.iter()
			.filter_map(serde_json::Value::as_str)
			.map(str::to_owned)
			.collect(),
		_ => Vec::new(),
	}
}

fn nonempty_json_string(value: &serde_json::Value) -> Option<serde_json::Value> {
	value
		.as_str()
		.filter(|value| !value.trim().is_empty())
		.map(|value| serde_json::Value::String(value.to_owned()))
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
