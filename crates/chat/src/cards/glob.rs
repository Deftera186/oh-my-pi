//! Typed card for workspace glob searches.

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, typed_fault, typed_input, typed_result};

/// Card for `glob` calls.
pub struct GlobCard;

impl Card for GlobCard {
	fn tool(&self) -> &'static str {
		"glob"
	}

	fn render(&self, view: &CardView<'_>, _expanded: bool, _ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::glob::Params>(view).unwrap_or(Value::Null);
		let query = string_at(&args, "path")
			.or_else(|| partial_string(view.args_text().unwrap_or_default(), "path"))
			.unwrap_or_default();
		match view.status {
			CardStatus::StreamingArgs | CardStatus::InProgress => {
				let limit = args
					.get("limit")
					.and_then(Value::as_f64)
					.map(|value| value as u64);
				dom! {
					<row gap=1 pad-x=1>
						<i:pending/><text bold>{"Glob:"}</text><text>{query}</text>
						if let Some(limit) = limit { <text>{sf!("limit:{limit}")}</text> }
					</row>
				}
				.into_component()
			},
			CardStatus::Done => render_done(view, query),
			CardStatus::Failed => render_failed(view),
		}
	}
}

fn render_done(view: &CardView<'_>, query: &str) -> Component {
	let result = typed_result::<omp_tools::glob::Payload>(view).unwrap_or(Value::Null);
	let files = result
		.get("files")
		.or_else(|| result.get("matches"))
		.and_then(Value::as_array)
		.cloned()
		.unwrap_or_default();
	let count = result
		.get("file_count")
		.or_else(|| result.get("partial_match_count"))
		.and_then(Value::as_u64)
		.unwrap_or(files.len() as u64);
	let scope = glob_scope(query);
	dom! {
		<col pad-x=1>
			<row gap=1>
				<i:search/><text bold>{"Glob:"}</text><text>{query}</text><text>{&count}</text><text>{"files"}</text>
				<text fg=muted>{"· in"}</text><text>{scope}</text>
			</row>
			<col>
				for (index, file) in files.iter().enumerate() {
					<row gap=1>
						if index + 1 == files.len() { <i:tree-last/> } else { <i:tree-branch/> }
						<i:typescript/><text>{file_path(file)}</text>
					</row>
				}
			</col>
		</col>
	}
	.into_component()
}

fn render_failed(view: &CardView<'_>) -> Component {
	let fault = typed_fault::<omp_tools::glob::Fault>(view)
		.or_else(|| diag_text(view.diag))
		.unwrap_or_else(|| Str::new_static("glob failed"));
	dom! { <row gap=1 pad-x=1><i:error/><text bold>{"Error:"}</text><text fg=err>{fault}</text></row> }
		.into_component()
}

fn glob_scope(pattern: &str) -> &str {
	let prefix = pattern
		.find(['*', '?', '[', '{'])
		.map_or(pattern, |wildcard| &pattern[..wildcard]);
	let prefix = prefix.trim_end_matches('/');
	if prefix.len() == pattern.len() {
		prefix.rsplit_once('/').map_or(".", |(parent, _)| parent)
	} else if prefix.is_empty() {
		"."
	} else {
		prefix
	}
}

fn file_path(value: &Value) -> &str {
	value
		.as_str()
		.or_else(|| string_at(value, "path"))
		.unwrap_or_default()
}

fn string_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
	value.get(key).and_then(Value::as_str)
}

fn partial_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
	let marker = sf!("\"{key}\":\"");
	let rest = json.get(json.find(marker.as_str())? + marker.len()..)?;
	Some(rest.split('"').next().unwrap_or(rest))
}

fn diag_text(node: Option<&Node>) -> Option<Str> {
	let raw = node.and_then(|node| {
		node.content.as_deref().or_else(|| {
			node
				.prop(&PropId::Text.into())
				.and_then(omp_dom::Value::as_str)
		})
	})?;
	let value: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()));
	value
		.as_str()
		.or_else(|| string_at(&value, "message"))
		.map(Str::new)
}
