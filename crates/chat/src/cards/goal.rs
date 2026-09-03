//! Typed card for durable goal operations.

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{
	Card, CardStatus, CardView, Component, elapsed_badge, typed_fault, typed_input, typed_result,
};

/// Durable goal card.
pub struct GoalCard;

impl Card for GoalCard {
	fn tool(&self) -> &'static str {
		"goal"
	}

	fn render(&self, view: &CardView<'_>, _expanded: bool, ui: &UiContext) -> Component {
		match view.status {
			CardStatus::StreamingArgs | CardStatus::InProgress => render_live(view),
			CardStatus::Done => render_done(view, ui),
			CardStatus::Failed => render_failed(view, ui),
		}
	}
}

fn render_live(view: &CardView<'_>) -> Component {
	let args = typed_input::<omp_tools::goal::Params>(view);
	let text = view.args_text().unwrap_or_default();
	let op = args
		.as_ref()
		.and_then(|value| string_at(value, "op"))
		.or_else(|| partial_string(text, "op"))
		.unwrap_or("create");
	let verb = if op == "create" { "set" } else { op };
	let objective = args
		.as_ref()
		.and_then(|value| string_at(value, "objective"))
		.or_else(|| partial_string(text, "objective"))
		.map(|value| truncate(value, 60));
	let budget = args
		.as_ref()
		.and_then(|value| value.get("token_budget"))
		.and_then(Value::as_u64)
		.map(compact_tokens);
	dom! {
		<row gap=1>
			<i:pending/>
			<text bold>{"Goal:"}</text>
			<text bold>{verb}</text>
			if let Some(objective) = objective { <text>{sf!("\"{objective}\"")}</text> }
			if let Some(budget) = budget {
				<text fg=muted>{"· budget"}</text><text fg=muted>{budget}</text>
			}
			if let Some(badge) = elapsed_badge(view) { {badge} }
		</row>
	}
	.into_component()
}

fn render_done(view: &CardView<'_>, ui: &UiContext) -> Component {
	let result = typed_result::<omp_tools::goal::Payload>(view).unwrap_or(Value::Null);
	let op = string_at(&result, "op").unwrap_or("create");
	let verb = if op == "create" { "set" } else { op };
	let goal = result.get("goal");
	let objective = goal
		.and_then(|value| string_at(value, "objective"))
		.map(|value| truncate(value.trim(), 180));
	let state = goal
		.and_then(|value| string_at(value, "status"))
		.map(Str::new);
	let detail = goal.and_then(|goal| {
		let budget = goal.get("tokenBudget")?.as_u64()?;
		let used = goal.get("tokensUsed").and_then(Value::as_u64).unwrap_or(0);
		let left = result
			.get("remainingTokens")
			.and_then(Value::as_u64)
			.unwrap_or_else(|| budget.saturating_sub(used));
		let mut detail = sf!(
			"{} / {} tokens ({} left)",
			compact_tokens(used),
			compact_tokens(budget),
			compact_tokens(left)
		);
		if let Some(seconds) = goal.get("timeUsedSeconds").and_then(Value::as_u64)
			&& seconds > 0
		{
			detail = sf!("{detail} · {}m elapsed", seconds / 60);
		}
		Some(detail)
	});
	let report = result
		.get("completionBudgetReport")
		.and_then(Value::as_str)
		.filter(|text| !text.is_empty())
		.map(Str::new);
	let title = sf!(
		"{} Goal: {verb}{}",
		ui.charset.icon_named("goal-tool").unwrap_or("(o)"),
		state
			.as_ref()
			.map_or_else(Str::default, |state| sf!(" ⟨{state}⟩")),
	);
	dom! {
		<box border=round title={title} title_pad=3 pad="0 1">
			if let Some(objective) = objective { <text>{sf!("\"{objective}\"")}</text> }
			if let Some(detail) = detail { <text fg=muted>{detail}</text> }
			if let Some(report) = report { <hr title="Report"/><pre>{report}</pre> }
		</box>
	}
	.into_component()
}

fn render_failed(view: &CardView<'_>, ui: &UiContext) -> Component {
	let fault = typed_fault::<omp_tools::goal::Fault>(view)
		.or_else(|| diag_text(view.diag))
		.unwrap_or_else(|| Str::new_static("operation failed"));
	let args = typed_input::<omp_tools::goal::Params>(view);
	let op = args
		.as_ref()
		.and_then(|value| string_at(value, "op"))
		.unwrap_or("create");
	let verb = if op == "create" { "set" } else { op };
	let title = sf!("{} Goal: {verb}", ui.charset.icon_named("error").unwrap_or("[!!]"));
	dom! {
		<box border=round title={title} title_pad=3 pad="0 1">
			<text fg=err pad-x=2>{fault}</text>
		</box>
	}
	.into_component()
}

fn string_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
	value.get(key).and_then(Value::as_str)
}

fn partial_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
	let marker = sf!("\"{key}\":\"");
	let start = json.find(marker.as_str())? + marker.len();
	let rest = &json[start..];
	Some(rest.split('"').next().unwrap_or(rest))
}

fn truncate(text: &str, max_chars: usize) -> Str {
	if text.chars().count() <= max_chars {
		return Str::new(text);
	}
	let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
	out.push('…');
	Str::new(out)
}

fn compact_tokens(tokens: u64) -> Str {
	sf!("{}K", (tokens + 500) / 1_000)
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
