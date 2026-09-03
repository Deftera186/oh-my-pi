//! Typed card for GitHub operations.

use omp_core::Str;
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardView, Component, elapsed_badge, typed_fault, typed_input, typed_result};

/// Renders GitHub operation summaries and results.
pub struct GithubCard;

impl Card for GithubCard {
	fn tool(&self) -> &'static str {
		"github"
	}

	fn render(&self, view: &CardView<'_>, _expanded: bool, ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::github::Params>(view).unwrap_or(Value::Null);
		let detail = operation_title(args.get("op").and_then(Value::as_str).unwrap_or_default());
		let query = args
			.get("query")
			.and_then(Value::as_str)
			.unwrap_or_default();
		let repo = args.get("repo").and_then(Value::as_str);
		let heading = match repo {
			Some(repo) => format!("GitHub {detail} {query} · {repo}"),
			None => format!("GitHub {detail} {query}"),
		};
		match view.status.as_str() {
			"ok" => {
				let title = format!("{} {heading}", ui.charset.icon_named("gh").unwrap_or_default());
				let output = result_value(view)
					.and_then(|value| {
						value
							.get("output")
							.and_then(Value::as_str)
							.map(str::to_owned)
					})
					.unwrap_or_default();
				dom! { <box border=round title={title} title_pad=3 pad="0 1"><pre>{output}</pre></box> }
					.into_component()
			},
			"error" => {
				let title = format!("{} {heading}", ui.charset.icon_named("error").unwrap_or_default());
				let fault = failure(view);
				dom! { <box border=round title={title} title_pad=3 pad="0 1"><text>{fault}</text></box> }.into_component()
			},
			_ => dom! {
				<row gap=1><i:pending/><text>{heading}</text>
					if let Some(badge) = elapsed_badge(view) { {badge} }
				</row>
			}
			.into_component(),
		}
	}
}

fn operation_title(op: &str) -> &'static str {
	match op {
		"search_prs" => "Search PRs",
		"search_issues" => "Search Issues",
		"search_commits" => "Search Commits",
		"search_code" => "Search Code",
		"run_watch" => "Run Watch",
		_ => "",
	}
}
fn result_value(view: &CardView<'_>) -> Option<Value> {
	typed_result::<omp_tools::github::Payload>(view)
}
fn failure(view: &CardView<'_>) -> Str {
	if let Some(fault) = typed_fault::<omp_tools::github::Fault>(view) {
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
