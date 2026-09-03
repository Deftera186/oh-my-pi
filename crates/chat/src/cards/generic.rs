//! Generic card for tools without a specialized renderer.

use omp_core::Str;
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, elapsed_badge};

/// Fallback renderer that presents every standard child of a tool element.
pub struct GenericCard;

impl GenericCard {
	pub(super) fn render_named(
		&self,
		tool: &str,
		view: &CardView<'_>,
		expanded: bool,
		_ui: &UiContext,
	) -> Component {
		let title = Str::new(tool);
		let args = parsed_args(view.args_text().unwrap_or_default());
		let detail = compact_args(args.as_ref());
		let result = display_result(view.result_text());
		let fault = view
			.diag
			.and_then(node_text)
			.filter(|text| !text.is_empty())
			.map(Str::new);
		dom! {
			<col pad="1 0">
				<row pad-x=1 gap=1>
					match view.status {
						CardStatus::Failed => <i:error/>,
						CardStatus::Done => <i:done/>,
						CardStatus::StreamingArgs | CardStatus::InProgress => <spinner kind=status fg=muted/>,
					}
					<text bold>{title}</text>
					if let Some(badge) = elapsed_badge(view) { {badge} }
				</row>
				if expanded {
					<row><i:space/></row>
					if let Some(Value::Object(fields)) = args.as_ref() {
						<text pad-x=1>{"Args"}</text>
						for (name, value) in fields {
							<row pad="0 1" gap=1><i:tree-last/><i:file/><text>{format!("{name}:")}</text><text>{json_text(value)}</text></row>
						}
						<row><i:space/></row>
					}
				} else if let Some(detail) = detail {
					<row pad-x=2 gap=1><text>{"└─"}</text><text>{detail}</text></row>
				}
				if let Some(result) = result { <text pad-x=1>{result}</text> }
				if let Some(fault) = fault { <text pad-x=1>{fault}</text> }
				if !expanded && matches!(view.status, CardStatus::Done | CardStatus::Failed) {
					<text pad-x=1>{"⟨Ctrl+O: Expand⟩"}</text>
				}
			</col>
		}
		.into_component()
	}
}

impl Card for GenericCard {
	fn tool(&self) -> &'static str {
		"*"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		self.render_named("tool", view, expanded, ui)
	}
}

fn parsed_args(text: &str) -> Option<Value> {
	serde_json::from_str(text).ok().or_else(|| {
		let mut repaired = String::with_capacity(text.len() + 2);
		repaired.push_str(text);
		if repaired.matches('"').count() % 2 == 1 {
			repaired.push('"');
		}
		if !repaired.trim_end().ends_with('}') {
			repaired.push('}');
		}
		serde_json::from_str(&repaired).ok()
	})
}

fn compact_args(args: Option<&Value>) -> Option<Str> {
	let Value::Object(fields) = args? else {
		return None;
	};
	let mut text = String::new();
	for (name, value) in fields {
		if !text.is_empty() {
			text.push_str(", ");
		}
		text.push_str(name);
		text.push('=');
		text.push_str(&json_text(value));
	}
	(!text.is_empty()).then(|| Str::new(text))
}

fn display_result(text: Option<&str>) -> Option<Str> {
	let text = text?.trim();
	if text.is_empty() {
		return None;
	}
	let display = serde_json::from_str::<Value>(text).ok().map_or_else(
		|| text.to_owned(),
		|value| match &value {
			Value::Object(fields) if fields.len() == 1 => fields
				.values()
				.next()
				.and_then(Value::as_str)
				.map_or_else(|| value.to_string(), str::to_owned),
			Value::String(text) => text.clone(),
			other => other.to_string(),
		},
	);
	Some(Str::new(display))
}

fn json_text(value: &Value) -> String {
	serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

fn node_text(node: &Node) -> Option<&str> {
	node.content.as_deref().or_else(|| {
		node
			.prop(&PropId::Text.into())
			.and_then(|value| value.as_str())
	})
}
