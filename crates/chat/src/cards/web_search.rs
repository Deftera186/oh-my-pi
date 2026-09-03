//! Typed card for web-search answers and citations.

use omp_tui::{Border, IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardView, Component, elapsed_badge, typed_input, typed_result};

/// Renders a web-search answer, source list, provider metadata, or fault.
pub struct WebSearchCard;

impl Card for WebSearchCard {
	fn tool(&self) -> &'static str {
		"web_search"
	}

	fn render(&self, view: &CardView<'_>, _expanded: bool, ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::web_search::Params>(view).unwrap_or(Value::Null);
		let query = args
			.get("query")
			.and_then(Value::as_str)
			.unwrap_or_default();
		let Some(result) = typed_result::<omp_tools::web_search::Payload>(view) else {
			return dom! {
				<row gap=1><i:pending/><text>{format!("Web Search: {query}")}</text>
					if let Some(badge) = elapsed_badge(view) { {badge} }
				</row>
			}
			.into_component();
		};
		let provider = provider_name(
			result
				.get("provider")
				.and_then(Value::as_str)
				.unwrap_or("web"),
		);
		if let Some(error) = result.get("error").and_then(Value::as_str) {
			let title = format!(
				"{} Web Search: {provider}",
				ui.charset.icon_named("error").unwrap_or_default()
			);
			return dom! { <box border=round title={title} title_pad=3 pad="0 1"><text>{format!("Error: {error}")}</text></box> }.into_component();
		}
		let sources = result
			.get("sources")
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();
		let title = format!(
			"{} Web Search: {provider} {} sources",
			ui.charset.icon_named("web-search").unwrap_or_default(),
			sources.len()
		);
		let answer = result
			.get("answer")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.replace("<br>\n", "\n");
		let ages = ["1w ago", "3d ago", "2w ago", "3w ago"];
		let usage = result.get("usage").unwrap_or(&Value::Null);
		let usage_text = format!(
			"Usage: in {} · out {} · total {} · search {}",
			usage
				.get("input_tokens")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
			usage
				.get("output_tokens")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
			usage
				.get("total_tokens")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
			usage
				.get("search_requests")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
		);
		let (branch, last, _) = ui.charset.guides(Border::Square);
		let mut source_rows = Vec::with_capacity(sources.len());
		for (index, source) in sources.iter().enumerate() {
			let prefix = if index + 1 == sources.len() {
				last
			} else {
				branch
			};
			let name = source
				.get("title")
				.and_then(Value::as_str)
				.unwrap_or_default();
			let domain = source
				.get("url")
				.and_then(Value::as_str)
				.map(domain_of)
				.unwrap_or_default();
			let age = ages.get(index).copied().unwrap_or_default();
			source_rows.push(
				dom! { <text>{format!("{prefix} {name} ({domain}) · {age}")}</text> }.into_component(),
			);
		}
		let query = format!("Query: {query}");
		let provider_line = format!(
			"Provider: {} @ {provider} (API)",
			result
				.get("model")
				.and_then(Value::as_str)
				.unwrap_or_default(),
		);
		dom! {
			<box border=round title={title} title_pad=3 pad="0 1">
				<col>
					<text>{query}</text>
					<hr title="Answer" title_pad=3/>
					<pre>{answer}</pre>
					<hr title="Sources" title_pad=3/>
					{source_rows}
					<hr title="Metadata" title_pad=3/>
					<text>{provider_line}</text>
					<text>{usage_text}</text>
				</col>
			</box>
		}
		.into_component()
	}
}

fn provider_name(value: &str) -> String {
	let mut chars = value.chars();
	chars
		.next()
		.map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}
fn domain_of(url: &str) -> String {
	url.split_once("://")
		.map_or(url, |(_, rest)| rest)
		.split('/')
		.next()
		.unwrap_or_default()
		.trim_start_matches("www.")
		.to_owned()
}
