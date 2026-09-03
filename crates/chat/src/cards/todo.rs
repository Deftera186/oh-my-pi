//! Typed card for the session checklist reducer.

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, components::STRIKE_TOTAL_FRAMES, dom};
use serde_json::Value;

use super::{
	Card, CardStatus, CardView, Component, elapsed_badge, typed_fault, typed_input, typed_result,
};

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
	dom! {
		<row gap=1><i:pending/><text>{"Todo"}</text><text>{op}</text>
			if let Some(badge) = elapsed_badge(view) { {badge} }
		</row>
	}
	.into_component()
}

/// pi `#updateTodoStrikeAnimation` ticks the completion strike every 65 ms
/// for `TODO_STRIKE_TOTAL_FRAMES` frames; `<strike reveal>` sweeps over the
/// same total on the shared clock.
const TODO_STRIKE_FRAME_MS: u64 = 65;

/// The task a settled `done` op just completed (pi `completedTasks`):
/// `(phase, item)` from the call's own arguments, so only that row sweeps.
fn newly_completed(view: &CardView<'_>) -> Option<(Option<Str>, Str)> {
	let args = typed_input::<omp_tools::todo::Params>(view)?;
	if args.get("op").and_then(Value::as_str) != Some("done") {
		return None;
	}
	let item = args.get("item").and_then(Value::as_str).map(Str::new)?;
	let phase = args.get("phase").and_then(Value::as_str).map(Str::new);
	Some((phase, item))
}

fn render_checklist(view: &CardView<'_>, _expanded: bool, ui: &UiContext) -> Component {
	let completed_now = newly_completed(view);
	let sweep = sf!("{}ms", TODO_STRIKE_FRAME_MS * u64::from(STRIKE_TOTAL_FRAMES));
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
			let sweeping = completed
				&& completed_now.as_ref().is_some_and(|(phase, item)| {
					*item == text && phase.as_ref().is_none_or(|phase| phase == title)
				});
			phase_rows.push(
				dom! {
					<row gap=1 pad-x=2>
						if last { <i:tree-last/> } else { <i:tree-branch/> }
						if completed { <i:checked/> } else { <i:unchecked/> }
						if sweeping { <strike reveal={sweep.clone()}>{text}</strike> }
						else if completed { <text strike>{text}</text> }
						else { <text>{text}</text> }
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

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use omp_core::Str;
	use omp_dom::{KnownTag, Node, PropId, Value};
	use omp_tui::{CellContent, Ui, UiContext, test_support::frame_row_text};

	use super::TodoCard;
	use crate::cards::{Card as _, CardStatus, CardView};

	const RESULT: &str = r#"{"phases":[{"title":"Foundation","tasks":[{"text":"Scaffold crate","status":"completed"},{"text":"Wire workspace","status":"completed"}]}]}"#;

	fn text_node(tag: KnownTag, text: &'static str) -> Node {
		let mut props = smallvec::SmallVec::new();
		props.push((PropId::Text.into(), Value::Str(Str::new_static(text))));
		Node { tag: tag.into(), props, kids: Vec::new(), content: None }
	}

	fn struck(ui: &Ui, row: u16, from: u16, len: u16) -> Vec<bool> {
		(from..from + len)
			.filter(|x| matches!(ui.frame().cell(*x, row).content(), CellContent::Grapheme { .. }))
			.map(|x| ui.frame().cell(x, row).style().spec().strikethrough)
			.collect()
	}

	#[test]
	fn todo_strike_reveals_progressively_then_settles() {
		let input = text_node(
			KnownTag::Input,
			r#"{"op":"done","phase":"Foundation","item":"Scaffold crate"}"#,
		);
		let result = text_node(KnownTag::Result, RESULT);
		let view = CardView {
			input:   &input,
			result:  Some(&result),
			diag:    None,
			usage:   None,
			status:  CardStatus::Done,
			output:  None,
			started: None,
		};
		let mut ui = Ui::from_root(TodoCard.render(&view, false, &UiContext::default()), 40, UiContext::default());
		let row = frame_row_text(ui.frame(), 2);
		let at = u16::try_from(row.find("Scaffold").expect("task row")).unwrap();
		let len = u16::try_from("Scaffold crate".len()).unwrap();
		// The task the op just completed starts plain and sweeps; the other
		// completed task was struck already and stays struck throughout.
		assert!(struck(&ui, 2, at, len).iter().all(|s| !s), "frame 0 holds plain: {row}");
		assert!(struck(&ui, 3, at, len).iter().all(|s| *s));
		assert_eq!(ui.next_wake(), Some(Duration::from_millis(65)));
		ui.tick(Duration::from_millis(455));
		let mid = struck(&ui, 2, at, len);
		let count = mid.iter().filter(|s| **s).count();
		assert!(count > 0 && count < usize::from(len), "mid-sweep: {count}");
		assert!(mid[..count].iter().all(|s| *s), "the strike grows from the start");
		ui.tick(Duration::from_millis(910));
		assert!(struck(&ui, 2, at, len).iter().all(|s| *s));
		assert_eq!(ui.next_wake(), None, "settled sweeps stop waking");
		assert_eq!(frame_row_text(ui.frame(), 2), row, "the text itself never changes");
	}

	#[test]
	fn todo_without_a_done_op_strikes_statically() {
		let input = text_node(KnownTag::Input, r#"{"op":"view"}"#);
		let result = text_node(KnownTag::Result, RESULT);
		let view = CardView {
			input:   &input,
			result:  Some(&result),
			diag:    None,
			usage:   None,
			status:  CardStatus::Done,
			output:  None,
			started: None,
		};
		let ui = Ui::from_root(TodoCard.render(&view, false, &UiContext::default()), 40, UiContext::default());
		let at = u16::try_from(frame_row_text(ui.frame(), 2).find("Scaffold").unwrap()).unwrap();
		assert!(struck(&ui, 2, at, 14).iter().all(|s| *s));
		assert_eq!(ui.next_wake(), None);
	}
}
