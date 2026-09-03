//! Typed card for the session checklist reducer.

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, typed_fault, typed_input, typed_result};

/// Session todo/checklist card.
pub struct TodoCard;

impl Card for TodoCard {
	fn tool(&self) -> &'static str {
		"todo"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		match view.status {
			CardStatus::StreamingArgs | CardStatus::InProgress => render_live(view),
			CardStatus::Done => render_checklist(view, expanded, ui),
			CardStatus::Failed => render_failed(view, ui),
		}
	}
}

fn render_live(view: &CardView<'_>) -> Component {
	let args = typed_input::<omp_tools::todo::Params>(view);
	let op = args
		.as_ref()
		.and_then(|value| value.get("op"))
		.and_then(Value::as_str)
		.or_else(|| partial_string(view.args_text().unwrap_or_default(), "op"))
		.unwrap_or_default();
	dom! { <row gap=1><i:pending/><text>{"Todo"}</text><text>{op}</text></row> }.into_component()
}

fn render_checklist(view: &CardView<'_>, _expanded: bool, ui: &UiContext) -> Component {
	let result = typed_result::<omp_tools::todo::Payload>(view).unwrap_or(Value::Null);
	let phases = result
		.get("phases")
		.and_then(Value::as_array)
		.map(Vec::as_slice)
		.unwrap_or_default();
	let total: usize = phases
		.iter()
		.filter_map(|phase| phase.get("tasks").and_then(Value::as_array))
		.map(Vec::len)
		.sum();
	let mut phase_rows = Vec::new();
	for (phase_index, phase) in phases.iter().enumerate() {
		let title = phase
			.get("title")
			.and_then(Value::as_str)
			.unwrap_or_default();
		let tasks = phase
			.get("tasks")
			.and_then(Value::as_array)
			.map(Vec::as_slice)
			.unwrap_or_default();
		let done = tasks
			.iter()
			.filter(|task| task.get("status").and_then(Value::as_str) == Some("completed"))
			.count();
		let heading = sf!("{}. {title}", roman_numeral(phase_index + 1));
		phase_rows.push(
			dom! { <row gap=2><text>{heading}</text><text>{sf!("{done}/{}", tasks.len())}</text></row> }
				.into_component(),
		);
		for (task_index, task) in tasks.iter().enumerate() {
			let text = Str::new(task.get("text").and_then(Value::as_str).unwrap_or_default());
			let completed = task.get("status").and_then(Value::as_str) == Some("completed");
			let blocker = task
				.get("blocker")
				.and_then(Value::as_str)
				.filter(|text| !text.is_empty())
				.map(Str::new);
			let last = task_index + 1 == tasks.len();
			phase_rows.push(
				dom! {
					<row gap=1 pad-x=2>
						if last { <i:tree-last/> } else { <i:tree-branch/> }
						if completed { <i:checked/> } else { <i:unchecked/> }
						if completed { <text strike>{text}</text> } else { <text>{text}</text> }
						if let Some(blocker) = blocker { <text fg=muted>{sf!("— {blocker}")}</text> }
					</row>
				}
				.into_component(),
			);
		}
	}
	let title = sf!("{} Todo {total} tasks", ui.charset.icon_named("todo").unwrap_or("[x]"));
	dom! {
		<box border=round title={title} title_pad=3 pad="0 1">
			{phase_rows}
		</box>
	}
	.into_component()
}

fn render_failed(view: &CardView<'_>, ui: &UiContext) -> Component {
	let fault = typed_fault::<omp_tools::todo::Fault>(view)
		.or_else(|| diag_text(view.diag))
		.unwrap_or_else(|| Str::new_static("operation failed"));
	let title = sf!("{} Todo", ui.charset.icon_named("error").unwrap_or("[!!]"));
	dom! {
		<box border=round title={title} title_pad=3 pad="0 1">
			<text pad-x=2>{fault}</text>
		</box>
	}
	.into_component()
}

fn partial_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
	let marker = sf!("\"{key}\":\"");
	let start = json.find(marker.as_str())? + marker.len();
	let rest = &json[start..];
	Some(rest.split('"').next().unwrap_or(rest))
}

fn roman_numeral(mut index: usize) -> String {
	const DIGITS: &[(usize, &str)] = &[
		(1000, "M"),
		(900, "CM"),
		(500, "D"),
		(400, "CD"),
		(100, "C"),
		(90, "XC"),
		(50, "L"),
		(40, "XL"),
		(10, "X"),
		(9, "IX"),
		(5, "V"),
		(4, "IV"),
		(1, "I"),
	];
	let mut roman = String::new();
	for &(value, digit) in DIGITS {
		while index >= value {
			roman.push_str(digit);
			index -= value;
		}
	}
	roman
}

fn diag_text(node: Option<&Node>) -> Option<Str> {
	node.and_then(|node| {
		node.content.clone().or_else(|| {
			node
				.prop(&PropId::Text.into())
				.and_then(|value| value.as_str())
				.map(Str::new)
		})
	})
}
