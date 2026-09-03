//! Typed card for parallel subagent task batches.

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, elapsed_badge, typed_result};

/// Parallel subagent task card.
pub struct TaskCard;

impl Card for TaskCard {
	fn tool(&self) -> &'static str {
		"task"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		let _input = view.input::<omp_tools::task::Params>();
		let _fault = view.fault::<omp_tools::task::Fault>();
		match view.status {
			CardStatus::StreamingArgs | CardStatus::InProgress => {
				dom! {
					<box border=round title_pad=3 pad="0 1">
						<row kind=title gap=1 bold>
							<i:task/><text bold>{"Task: task"}</text>
							if let Some(badge) = elapsed_badge(view) { {badge} }
						</row>
					</box>
				}
				.into_component()
			},
			CardStatus::Done | CardStatus::Failed => render_settled(view, expanded, ui),
		}
	}
}

fn render_settled(view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
	let result = view
		.result_json()
		.filter(|value| value.get("results").is_some())
		.or_else(|| typed_result::<omp_tools::task::Payload>(view))
		.unwrap_or(Value::Null);
	let rows = result
		.get("results")
		.or_else(|| result.get("children"))
		.and_then(Value::as_array)
		.map(Vec::as_slice)
		.unwrap_or_default();
	if rows.is_empty() {
		let fault = diag_text(view.diag).unwrap_or_else(|| Str::new_static("operation failed"));
		let title = sf!("{} Task 1 agent", ui.charset.icon_named("error").unwrap_or("[!!]"));
		return dom! {
			<box border=round title={title} title_pad=3 pad="0 1">
				<text fg=err pad-x=2>{fault}</text>
			</box>
		}
		.into_component();
	}
	let failed = rows.iter().any(row_failed);
	let count = rows.len();
	let agent_word = if count == 1 { "agent" } else { "agents" };
	let title = sf!(
		"{} Task {count} {agent_word}",
		ui.charset
			.icon_named(if failed { "error" } else { "done" })
			.unwrap_or(if failed { "[!!]" } else { "*" })
	);
	let mut rendered_rows = Vec::with_capacity(rows.len());
	for row in rows {
		let job = Str::new(
			row.get("job")
				.or_else(|| row.get("id"))
				.and_then(Value::as_str)
				.unwrap_or("agent"),
		);
		let desc = row
			.get("description")
			.or_else(|| row.get("agent"))
			.and_then(Value::as_str)
			.filter(|text| !text.is_empty())
			.map(Str::new);
		let ok = !row_failed(row);
		let state = if ok { "⟨done⟩" } else { "⟨failed⟩" };
		let detail = task_detail(row);
		let assignment = row
			.get("assignment")
			.and_then(Value::as_str)
			.filter(|text| !text.is_empty())
			.map(|text| preview(text, 70));
		let output = row
			.get("output")
			.or_else(|| row.get("text"))
			.and_then(Value::as_str)
			.filter(|text| !text.is_empty())
			.map(|text| preview(text, 70));
		let error = row
			.get("error")
			.and_then(Value::as_str)
			.filter(|text| !text.is_empty())
			.map(|text| preview(text, 70));
		rendered_rows.push(
			dom! {
				<col>
					<row gap=1>
						if ok { <i:done/> } else { <i:error/> }
						<text bold>{sf!("{job}:")}</text>
						if let Some(desc) = desc { <text>{desc}</text> }
						<text fg=muted>{state}</text>
						if let Some(detail) = detail {
							<text fg=muted>{"·"}</text><text fg=muted>{detail}</text>
						}
					</row>
					if expanded {
						if let Some(assignment) = assignment {
							<text fg=muted pad-x=2>{"Task"}</text><pre pad-x=4>{assignment}</pre>
						}
					}
					if let Some(output) = output {
						<text fg=muted pad-x=2>{"Output"}</text><pre pad-x=4>{output}</pre>
					}
					if let Some(error) = error { <text fg=err pad-x=2>{error}</text> }
				</col>
			}
			.into_component(),
		);
	}
	let requests = result
		.get("requests")
		.and_then(Value::as_u64)
		.unwrap_or_else(|| {
			rows
				.iter()
				.filter_map(|row| row.get("requests")?.as_u64())
				.sum()
		});
	let duration = result
		.get("total_duration_ms")
		.and_then(Value::as_u64)
		.unwrap_or_else(|| {
			rows
				.iter()
				.filter_map(|row| row.get("wall_ms")?.as_u64())
				.max()
				.unwrap_or(0)
		});
	let status = if failed { "failed" } else { "succeeded" };
	let summary = sf!("⟨{count} {status} · {requests} req · {:.1}s⟩", duration as f64 / 1_000.0);
	dom! {
		<box border=round title={title} title_pad=3 pad="0 1">
			{rendered_rows}
			<text fg=muted>{summary}</text>
		</box>
	}
	.into_component()
}

fn row_failed(row: &Value) -> bool {
	row.get("error").is_some_and(|value| !value.is_null())
		|| row
			.get("exit")
			.and_then(Value::as_i64)
			.is_some_and(|exit| exit != 0)
}

fn task_detail(row: &Value) -> Option<Str> {
	let requests = row.get("requests").and_then(Value::as_u64)?;
	let mut parts = vec![sf!("{requests} req")];
	if let (Some(tokens), Some(window)) = (
		row.get("context_tokens").and_then(Value::as_u64),
		row.get("context_window").and_then(Value::as_u64),
	) {
		parts.push(sf!("{:.1}%/{}K", tokens as f64 * 100.0 / window as f64, window / 1_000));
	}
	if let Some(cost) = row.get("cost").and_then(Value::as_f64) {
		parts.push(sf!("${cost:.2}"));
	}
	if let Some(wall) = row.get("wall_ms").and_then(Value::as_u64) {
		parts.push(sf!("{:.1}s", wall as f64 / 1_000.0));
	}
	Some(Str::new(parts.join(" · ")))
}

fn preview(text: &str, max_chars: usize) -> Str {
	let lines = text
		.lines()
		.map(|line| {
			if line.chars().count() <= max_chars {
				line.to_owned()
			} else {
				let mut cut: String = line.chars().take(max_chars.saturating_sub(1)).collect();
				cut.push('…');
				cut
			}
		})
		.collect::<Vec<_>>()
		.join("\n");
	Str::new(lines)
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
