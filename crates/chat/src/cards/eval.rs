//! Typed card for `eval@1`.

use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, typed_input, typed_result};

/// Persistent Python-kernel cell card.
pub struct EvalCard;

impl Card for EvalCard {
	fn tool(&self) -> &'static str {
		"eval"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::eval::Params>(view);
		let result = typed_result::<omp_tools::eval::Payload>(view);
		let code = args
			.as_ref()
			.and_then(|value| value.get("code"))
			.and_then(Value::as_str)
			.map(str::to_owned)
			.or_else(|| partial_string(view.args_text().unwrap_or_default(), "code"))
			.or_else(|| {
				result
					.as_ref()
					.and_then(|value| value.get("code"))
					.and_then(Value::as_str)
					.map(str::to_owned)
			})
			.unwrap_or_default();
		let title = args
			.as_ref()
			.and_then(|value| value.get("title"))
			.and_then(Value::as_str)
			.map(str::to_owned)
			.or_else(|| partial_string(view.args_text().unwrap_or_default(), "title"))
			.or_else(|| {
				result
					.as_ref()
					.and_then(|value| value.get("title"))
					.and_then(Value::as_str)
					.map(str::to_owned)
			})
			.unwrap_or_default();
		let output = result.as_ref().map(output_text).unwrap_or_default();
		let displays = result.as_ref().map(display_values).unwrap_or_default();
		let duration = result.as_ref().and_then(|value| {
			value
				.get("wall_ms")
				.or_else(|| value.pointer("/status/duration_ms"))
				.and_then(Value::as_u64)
		});
		let fault = diag_text(view).or_else(|| {
			result
				.as_ref()
				.and_then(|value| value.get("fault"))
				.and_then(Value::as_str)
				.map(str::to_owned)
		});
		let state = match view.status {
			CardStatus::StreamingArgs | CardStatus::InProgress => {
				format!("{} running", icon(ui, if expanded { "spin-2" } else { "spin-3" }))
			},
			CardStatus::Done => icon(ui, "done").to_owned(),
			CardStatus::Failed => icon(ui, "error").to_owned(),
		};
		let mut card_title = format!("{} {state}", icon(ui, "python"));
		if !title.is_empty() {
			card_title.push(' ');
			card_title.push_str(&title);
		}
		if let Some(duration) = duration {
			card_title.push_str(&format!(" · ({duration}ms)"));
		}
		dom! {
			<col>
				<box border=round pad-x=1 title={card_title} title_pad=3>
					if !code.is_empty() {
						<pre>{code}</pre>
					}
					if matches!(view.status, CardStatus::Done | CardStatus::Failed) && (!output.is_empty() || fault.is_some()) {
						<hr title="Output" title_pad=3/>
						if !output.is_empty() {
							<pre>{output}</pre>
						}
						if let Some(fault) = fault {
							<pre>{fault}</pre>
						}
					}
				</box>
				if view.status == CardStatus::Done && !displays.is_empty() {
					<col>
						for (index, display) in displays.iter().enumerate() {
							<row gap=1>
								<icon name={if index + 1 == displays.len() { "tree-last" } else { "tree-branch" }}/>
								<i:file/><text>{format!("[{index}]: \"{display}\"")}</text>
							</row>
						}
					</col>
				}
			</col>
		}
		.into_component()
	}
}

fn icon<'a>(ui: &'a UiContext, name: &'a str) -> &'a str {
	ui.charset.icon_named(name).unwrap_or(name)
}

fn output_text(result: &Value) -> String {
	if let Some(frames) = result.get("frames").and_then(Value::as_array) {
		return frames
			.iter()
			.filter_map(|frame| frame.get("data"))
			.map(bytes_or_text)
			.collect();
	}
	result
		.get("output")
		.and_then(Value::as_str)
		.unwrap_or_default()
		.to_owned()
}

fn display_values(result: &Value) -> Vec<String> {
	if let Some(groups) = result.get("json_outputs").and_then(Value::as_array) {
		return groups
			.iter()
			.flat_map(|group| group.as_array().into_iter().flatten())
			.map(display_value)
			.collect();
	}
	result
		.get("display_outputs")
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter_map(|entry| entry.get("data"))
		.map(display_value)
		.collect()
}

fn display_value(value: &Value) -> String {
	value
		.as_str()
		.map(str::to_owned)
		.unwrap_or_else(|| serde_json::to_string(value).unwrap_or_default())
}

fn bytes_or_text(value: &Value) -> String {
	if let Some(text) = value.as_str() {
		return text.to_owned();
	}
	value
		.as_array()
		.map(|bytes| {
			String::from_utf8_lossy(
				&bytes
					.iter()
					.filter_map(Value::as_u64)
					.filter_map(|byte| u8::try_from(byte).ok())
					.collect::<Vec<_>>(),
			)
			.into_owned()
		})
		.unwrap_or_default()
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
