//! Typed debugger session and stack-trace card.

use omp_core::Str;
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardView, Component, typed_fault, typed_input, typed_result};

/// Renders debugger session state and stack frames.
pub struct DebugCard;

impl Card for DebugCard {
	fn tool(&self) -> &'static str {
		"debug"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		let raw_args = node_text(view.input).unwrap_or_default();
		let args = typed_input::<omp_tools::debug::Params>(view).unwrap_or(Value::Null);
		let action = args
			.get("action")
			.and_then(Value::as_str)
			.map(str::to_owned)
			.or_else(|| extract_string(raw_args.as_str(), "action"))
			.unwrap_or_default()
			.replace('_', " ");
		if view.status.as_str() == "error" {
			let title =
				format!("{} Debug {action}", ui.charset.icon_named("error").unwrap_or_default());
			let fault = failure(view);
			return dom! {
				<box border=round title={title} title_pad=3 pad="0 1"><col><hr title="Output" title_pad=3/><text>{fault}</text></col></box>
			}.into_component();
		}
		let Some(result) = typed_result::<omp_tools::debug::Payload>(view) else {
			return dom! { <row gap=1><i:pending/><text>{format!("Debug: {action}")}</text></row> }
				.into_component();
		};
		let session = result.get("session").unwrap_or(&Value::Null);
		let frames = result
			.get("frames")
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();
		let shown = if expanded {
			frames.len()
		} else {
			frames.len().min(2)
		};
		let title = format!("{} Debug {action}", ui.charset.icon_named("debug").unwrap_or_default());
		let location = format!(
			"{}:{}:{}",
			str_field(session, "path"),
			session
				.get("line")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
			session
				.get("col")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
		);
		dom! {
			<box border=round title={title} title_pad=3 pad="0 1">
				<col>
					<hr title="Session" title_pad=3/>
					<text>{format!("Session {}", str_field(session, "id"))}</text>
					<text>{format!("Adapter: {}", str_field(session, "adapter"))}</text>
					<text>{format!("Status: {}", str_field(session, "status"))}</text>
					<text>{format!("CWD: {}", str_field(session, "cwd"))}</text>
					<text>{format!("Program: {}", str_field(session, "program"))}</text>
					<text>{format!("Stop reason: {}", str_field(session, "reason"))}</text>
					<text>{format!("Frame: {}", str_field(session, "frame"))}</text>
					<text>{format!("Instruction pointer: {}", str_field(session, "instruction_pointer"))}</text>
					<text>{format!("Location: {location}")}</text>
					<hr title="Output" title_pad=3/>
					<text>{"Stack trace:"}</text>
					for frame in frames.iter().take(shown) {
						<text>{format!("- #{} {} @ {}:{}:{}", frame.get("id").and_then(Value::as_u64).unwrap_or_default(), str_field(frame, "name"), str_field(frame, "path"), frame.get("line").and_then(Value::as_u64).unwrap_or_default(), frame.get("col").and_then(Value::as_u64).unwrap_or_default())}</text>
					}
					if shown < frames.len() { <text fg=muted>{format!("… {} more lines ⟨Ctrl+O: Expand⟩", frames.len() - shown)}</text> }
				</col>
			</box>
		}.into_component()
	}
}

fn str_field(value: &Value, key: &str) -> String {
	value
		.get(key)
		.and_then(Value::as_str)
		.unwrap_or_default()
		.to_owned()
}
fn extract_string(raw: &str, key: &str) -> Option<String> {
	let marker = format!("\"{key}\":\"");
	let rest = raw.split_once(&marker)?.1;
	Some(rest.split('"').next().unwrap_or(rest).to_owned())
}
fn failure(view: &CardView<'_>) -> Str {
	if let Some(fault) = typed_fault::<omp_tools::debug::Fault>(view) {
		return fault;
	}
	let raw = view.diag.and_then(node_text).unwrap_or_default();
	serde_json::from_str::<String>(raw.as_str())
		.map(Str::new)
		.unwrap_or(raw)
}
fn node_text(node: &Node) -> Option<Str> {
	node.content.clone().or_else(|| {
		node
			.prop(&PropId::Text.into())
			.and_then(|value| value.as_str())
			.map(Str::new)
	})
}
