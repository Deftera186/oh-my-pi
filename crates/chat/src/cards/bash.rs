//! Typed card for `bash@1`.

use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, typed_fault, typed_input, typed_result};

/// Shell-command card with durable transcript and terminal metadata.
pub struct BashCard;

impl Card for BashCard {
	fn tool(&self) -> &'static str {
		"bash"
	}

	fn render(&self, view: &CardView<'_>, _expanded: bool, _ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::shell::Params>(view);
		let command = args
			.as_ref()
			.and_then(|value| value.get("command"))
			.and_then(Value::as_str)
			.map(str::to_owned)
			.or_else(|| partial_string(view.args_text().unwrap_or_default(), "command"))
			.unwrap_or_default();
		let cwd = args
			.as_ref()
			.and_then(|value| value.get("cwd"))
			.and_then(Value::as_str);
		let shown_command =
			cwd.map_or_else(|| command.clone(), |cwd| format!("cd {cwd} && {command}"));
		let result = typed_result::<omp_tools::shell::Payload>(view);
		let output = result.as_ref().map(output_text).unwrap_or_default();
		let fault = diag_text(view).or_else(|| {
			result
				.as_ref()
				.and_then(|value| value.get("text").or_else(|| value.get("error")))
				.and_then(Value::as_str)
				.map(str::to_owned)
		});
		let wall_ms = result.as_ref().and_then(|value| {
			value
				.get("wall_ms")
				.or_else(|| value.pointer("/status/wall_clock_ms"))
				.and_then(Value::as_u64)
		});
		let timeout = args
			.as_ref()
			.and_then(|value| value.get("timeout"))
			.and_then(Value::as_f64);
		let exit = result.as_ref().and_then(|value| {
			value
				.get("exit")
				.or_else(|| value.pointer("/status/exit_code"))
				.and_then(Value::as_i64)
		});
		let meta = wall_ms.map(|wall| {
			let mut text = format!("Wall: {:.2}s", wall as f64 / 1_000.0);
			if let Some(timeout) = timeout {
				text.push_str(&format!(" | Timeout: {timeout}s"));
			}
			if view.status == CardStatus::Failed || exit.is_some_and(|code| code != 0) {
				if let Some(exit) = exit {
					text.push_str(&format!(" | Exit: {exit}"));
				}
			}
			text
		});
		dom! {
			<box border=round>
				<row pad-x=1 gap=1><text>{"$"}</text><text>{shown_command}</text></row>
				if matches!(view.status, CardStatus::Done | CardStatus::Failed) && (!output.is_empty() || fault.is_some()) {
					<hr title="Output" title_pad=3/>
					<col pad-x=1>
						if !output.is_empty() {
							<pre>{output}</pre>
						}
						if let Some(message) = fault {
							<pre>{message}</pre>
						}
						if let Some(meta) = meta {
							<row fg=muted><i:bracket-left/><text>{meta}</text><i:bracket-right/></row>
						}
					</col>
				}
			</box>
		}
		.into_component()
	}
}

fn output_text(result: &Value) -> String {
	if let Some(frames) = result.get("transcript").and_then(Value::as_array) {
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
	typed_fault::<omp_tools::shell::Fault>(view)
		.map(|fault| fault.to_string())
		.or_else(|| {
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
		})
}

#[cfg(test)]
mod tests {
	#[test]
	fn reads_partial_streamed_command() {
		assert_eq!(
			super::partial_string(r#"{"command":"git status --short"#, "command").as_deref(),
			Some("git status --short")
		);
	}
}
