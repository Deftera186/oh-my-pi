//! Typed card for `browser@1`.

use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, typed_input, typed_result};

/// Browser automation code-cell card.
pub struct BrowserCard;

impl Card for BrowserCard {
	fn tool(&self) -> &'static str {
		"browser"
	}

	fn render(&self, view: &CardView<'_>, _expanded: bool, ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::browser::Params>(view);
		let result = typed_result::<omp_tools::browser::Payload>(view);
		let name = result
			.as_ref()
			.and_then(|value| value.get("name"))
			.and_then(Value::as_str)
			.or_else(|| args.as_ref()?.get("name")?.as_str())
			.unwrap_or("main")
			.to_owned();
		let code = args
			.as_ref()
			.and_then(|value| value.get("code"))
			.and_then(Value::as_str)
			.map(str::to_owned)
			.or_else(|| partial_string(view.args_text().unwrap_or_default(), "code"))
			.unwrap_or_default();
		let url = result
			.as_ref()
			.and_then(|value| value.get("url"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let kind = result
			.as_ref()
			.and_then(|value| value.get("browser"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let displays = result
			.as_ref()
			.and_then(|value| value.get("display"))
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();
		let returned = result
			.as_ref()
			.and_then(|value| value.get("result"))
			.map(display_value);
		let fault = diag_text(view).or_else(|| {
			result
				.as_ref()
				.and_then(|value| value.get("error"))
				.and_then(Value::as_str)
				.map(str::to_owned)
		});
		let state = match view.status {
			CardStatus::StreamingArgs | CardStatus::InProgress => {
				format!("{} running tab \"{name}\"", icon(ui, "spin-2"))
			},
			CardStatus::Done => format!("{} tab \"{name}\"", icon(ui, "done")),
			CardStatus::Failed => format!("{} tab \"{name}\"", icon(ui, "error")),
		};
		let mut title = state;
		if !url.is_empty() {
			title.push_str(" · ");
			title.push_str(&url);
		}
		if !kind.is_empty() {
			title.push_str(" · ");
			title.push_str(&kind);
		}
		dom! {
			<box border=round pad-x=1 title={title} title_pad=3>
				if !code.is_empty() { <pre>{code}</pre> }
				if matches!(view.status, CardStatus::Done | CardStatus::Failed) {
					<hr title="Output" title_pad=3/>
					if let Some(fault) = fault {
						<pre>{fault}</pre>
					} else {
						for display in displays { <pre>{display_value(&display)}</pre> }
						if let Some(returned) = returned { <pre>{returned}</pre> }
					}
				}
			</box>
		}
		.into_component()
	}
}

fn icon<'a>(ui: &'a UiContext, name: &'a str) -> &'a str {
	ui.charset.icon_named(name).unwrap_or(name)
}

fn display_value(value: &Value) -> String {
	match value {
		Value::String(text) => format!("\"{text}\""),
		Value::Object(fields) => {
			let body = fields
				.iter()
				.map(|(key, value)| format!("{key}: {}", display_value(value)))
				.collect::<Vec<_>>()
				.join(", ");
			format!("{{ {body} }}")
		},
		_ => value.to_string(),
	}
}

fn partial_string(raw: &str, key: &str) -> Option<String> {
	let start = raw.find(&format!("\"{key}\""))?;
	let value = raw[start..].find(':')? + start + 1;
	let quote = raw[value..].find('"')? + value + 1;
	let bytes = raw.as_bytes();
	let mut escaped = false;
	for index in quote..bytes.len() {
		match (bytes[index], escaped) {
			(b'"', false) => return serde_json::from_str(&raw[quote - 1..=index]).ok(),
			(b'\\', false) => escaped = true,
			_ => escaped = false,
		}
	}
	Some(raw[quote..].replace("\\n", "\n").replace("\\\"", "\""))
}

fn diag_text(view: &CardView<'_>) -> Option<String> {
	view.diag.and_then(|node| {
		node
			.content
			.as_deref()
			.or_else(|| {
				node
					.prop(&omp_dom::PropId::Text.into())
					.and_then(omp_dom::Value::as_str)
			})
			.filter(|text| !text.is_empty())
			.map(str::to_owned)
	})
}
