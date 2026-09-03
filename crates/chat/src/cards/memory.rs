//! Typed cards for the recall, reflect, and retain memory tools.

use omp_core::Str;
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardView, Component, typed_fault, typed_input, typed_result};

/// Renders scored memory-search results.
pub struct RecallCard;
/// Renders a synthesis over recalled memories.
pub struct ReflectCard;
/// Renders memory items being retained.
pub struct RetainCard;

impl Card for RecallCard {
	fn tool(&self) -> &'static str {
		"recall"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, _ui: &UiContext) -> Component {
		render_recall(view, expanded)
	}
}
impl Card for ReflectCard {
	fn tool(&self) -> &'static str {
		"reflect"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, _ui: &UiContext) -> Component {
		render_reflect(view, expanded)
	}
}
impl Card for RetainCard {
	fn tool(&self) -> &'static str {
		"retain"
	}

	fn render(&self, view: &CardView<'_>, _expanded: bool, _ui: &UiContext) -> Component {
		render_retain(view)
	}
}

fn render_recall(view: &CardView<'_>, expanded: bool) -> Component {
	let args =
		typed_input::<omp_tools::memory::RecallParams>(view).unwrap_or_else(|| partial_input(view));
	let query = field(&args, "query").unwrap_or_default();
	if failed(view) {
		let message = failure(view);
		return dom! { <row gap=1><i:error/><text>{format!("Error: {message}")}</text></row> }
			.into_component();
	}
	let Some(result) = typed_result::<omp_tools::memory::RecallPayload>(view) else {
		return dom! { <row gap=1><i:pending/><text>{format!("Recall: {query}")}</text></row> }
			.into_component();
	};
	let items = result
		.get("items")
		.and_then(Value::as_array)
		.cloned()
		.unwrap_or_default();
	let result_query = field(&result, "query").unwrap_or(query);
	let title = format!("Recall: {result_query} {} found", items.len());
	dom! {
		<col>
			<row gap=1><icon name="memory-tool"/><text>{title}</text></row>
			if expanded {
				for (index, item) in items.iter().enumerate() {
					<text pad-x=2>{format!("{}. [{:.2}] {}", index + 1, item.get("score").and_then(Value::as_f64).unwrap_or_default(), field(item, "content").unwrap_or_default())}</text>
					if let Some(context) = item.get("context").and_then(Value::as_str) {
						<text pad-x=5 fg=muted>{format!("({context})")}</text>
					}
				}
			} else {
				<text pad-x=2 fg=muted>{"⟨Ctrl+O: Expand⟩"}</text>
			}
		</col>
	}.into_component()
}

fn render_reflect(view: &CardView<'_>, expanded: bool) -> Component {
	let query = field(
		&typed_input::<omp_tools::memory::ReflectParams>(view).unwrap_or_else(|| partial_input(view)),
		"query",
	)
	.unwrap_or_default();
	if failed(view) {
		let message = failure(view);
		return dom! { <row gap=1><i:error/><text>{format!("Error: {message}")}</text></row> }
			.into_component();
	}
	let Some(result) = typed_result::<omp_tools::memory::ReflectPayload>(view) else {
		return dom! { <row gap=1><i:pending/><text>{format!("Reflect: {query}")}</text></row> }
			.into_component();
	};
	let answer = field(&result, "answer").unwrap_or_default();
	let lines: Vec<&str> = answer.lines().collect();
	let shown = if expanded {
		lines.len()
	} else {
		lines.len().min(3)
	};
	dom! {
		<col>
			<row gap=1><icon name="memory-tool"/><text>{format!("Reflect: {query}")}</text></row>
			for line in lines.iter().take(shown) { <text pad-x=2>{*line}</text> }
			if shown < lines.len() {
				<text pad-x=2 fg=muted>{format!("… {} more lines ⟨Ctrl+O: Expand⟩", lines.len() - shown)}</text>
			}
		</col>
	}
	.into_component()
}

fn render_retain(view: &CardView<'_>) -> Component {
	let args =
		typed_input::<omp_tools::memory::RetainParams>(view).unwrap_or_else(|| partial_input(view));
	if failed(view) {
		let message = failure(view);
		return dom! { <row gap=1><i:error/><text>{format!("Error: {message}")}</text></row> }
			.into_component();
	}
	let items = args
		.get("items")
		.and_then(Value::as_array)
		.cloned()
		.unwrap_or_default();
	let settled = view.result::<omp_tools::memory::RetainPayload>().is_some();
	let title = if settled {
		format!("Retain {} memories stored", items.len())
	} else {
		"Retain".to_owned()
	};
	dom! {
		<col>
			<row gap=1>
				if settled { <icon name="memory-tool"/> } else { <i:pending/> }
				<text>{title}</text>
			</row>
			for item in &items {
				<row gap=1 pad-x=2><i:enabled/><text>{field(item, "content").unwrap_or_default()}</text></row>
			}
		</col>
	}
	.into_component()
}

fn partial_input(view: &CardView<'_>) -> Value {
	let raw = node_text(view.input).unwrap_or_default();
	partial_object(raw.as_str())
}
fn field(value: &Value, key: &str) -> Option<String> {
	value.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn partial_object(raw: &str) -> Value {
	let mut map = serde_json::Map::new();
	for key in ["query"] {
		if let Some(value) = extract_string(raw, key) {
			map.insert(key.into(), Value::String(value));
		}
	}
	Value::Object(map)
}
fn extract_string(raw: &str, key: &str) -> Option<String> {
	let marker = format!("\"{key}\":\"");
	let rest = raw.split_once(&marker)?.1;
	Some(rest.split('"').next().unwrap_or(rest).to_owned())
}
fn failed(view: &CardView<'_>) -> bool {
	view.status.as_str() == "error"
}
fn failure(view: &CardView<'_>) -> Str {
	if let Some(fault) = typed_fault::<omp_tools::memory::Fault>(view) {
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
