//! Typed interactive question card.

use std::collections::BTreeMap;

use omp_core::Str;
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::{Value, json};

use super::{Card, CardView, Component, typed_fault, typed_input, typed_result};

/// Renders streamed questions, choices, answers, and cancellation faults.
pub struct AskCard;

impl Card for AskCard {
	fn tool(&self) -> &'static str {
		"ask"
	}

	fn render(&self, view: &CardView<'_>, _expanded: bool, ui: &UiContext) -> Component {
		if view.status.as_str() == "error" {
			let fault = failure(view);
			return dom! {
				<col><row gap=1><icon name="warning-status"/><text>{"Ask"}</text></row><text>{fault}</text></col>
			}.into_component();
		}
		let raw = node_text(view.input).unwrap_or_default();
		let args =
			typed_input::<omp_tools::ask::Params>(view).unwrap_or_else(|| partial_args(raw.as_str()));
		let questions = args
			.get("questions")
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();
		let answers = result_value(view)
			.and_then(|value| value.get("answers").and_then(Value::as_array).cloned())
			.unwrap_or_default();
		let answered = !answers.is_empty();
		let selected: BTreeMap<String, Vec<String>> = answers
			.iter()
			.filter_map(|answer| {
				let id = answer.get("question")?.as_str()?.to_owned();
				let values = answer
					.get("options")?
					.as_array()?
					.iter()
					.filter_map(Value::as_str)
					.map(str::to_owned)
					.collect();
				Some((id, values))
			})
			.collect();
		let title = if answered {
			format!(
				"{} Ask {} questions",
				ui.charset.icon_named("success").unwrap_or_default(),
				questions.len()
			)
		} else {
			format!("Ask {} questions", questions.len())
		};
		let mut question_rows = Vec::new();
		for question in &questions {
			let id = question
				.get("id")
				.and_then(Value::as_str)
				.unwrap_or_default();
			let multi = question
				.get("multi")
				.and_then(Value::as_bool)
				.unwrap_or(false);
			let options = question
				.get("options")
				.and_then(Value::as_array)
				.map(Vec::as_slice)
				.unwrap_or_default();
			let divider = if answered {
				format!("[{id}]")
			} else if multi {
				format!("[{id}] · multi · options:{}", options.len())
			} else {
				format!("[{id}] · options:{}", options.len())
			};
			question_rows.push(dom! { <hr title={divider} title_pad=3/> }.into_component());
			let question_text = Str::new(
				question
					.get("question")
					.and_then(Value::as_str)
					.unwrap_or_default(),
			);
			question_rows.push(dom! { <text pad-x=1>{question_text}</text> }.into_component());
			for option in options {
				let label = Str::new(
					option
						.get("label")
						.and_then(Value::as_str)
						.unwrap_or_default(),
				);
				let checked = selected
					.get(id)
					.is_some_and(|values| values.iter().any(|value| value == label.as_str()));
				question_rows.push(
					dom! {
						<row gap=1 pad-x=1>
							if multi && checked { <i:checked/> }
							else if multi { <i:unchecked/> }
							else if checked { <icon name="radio-selected"/> }
							else { <i:unselected/> }
							<text>{label}</text>
						</row>
					}
					.into_component(),
				);
				if !answered && let Some(desc) = option.get("description").and_then(Value::as_str) {
					let desc = Str::new(format!("↳ {desc}"));
					question_rows.push(dom! { <text pad-x=3 fg=muted>{desc}</text> }.into_component());
				}
			}
		}
		dom! {
			<box border=round title={title} title_pad=3 pad="0 1">
				<col>{question_rows}</col>
			</box>
		}
		.into_component()
	}
}

fn partial_args(raw: &str) -> Value {
	let question = extract_string(raw, "question").unwrap_or_default();
	let label = extract_string(raw, "label").unwrap_or_default();
	json!({"questions":[{"id":extract_string(raw, "id").unwrap_or_default(),"question":question,"options":[{"label":label}]}]})
}
fn extract_string(raw: &str, key: &str) -> Option<String> {
	let marker = format!("\"{key}\":\"");
	let rest = raw.split_once(&marker)?.1;
	Some(rest.split('"').next().unwrap_or(rest).to_owned())
}
fn result_value(view: &CardView<'_>) -> Option<Value> {
	typed_result::<omp_tools::ask::Payload>(view)
}
fn failure(view: &CardView<'_>) -> Str {
	if let Some(fault) = typed_fault::<omp_tools::ask::Fault>(view) {
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
