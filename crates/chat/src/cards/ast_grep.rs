//! Typed card for `ast_grep@1`.

use std::collections::BTreeSet;

use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, elapsed_badge, typed_input, typed_result};

/// Structural-search card.
pub struct AstGrepCard;

impl Card for AstGrepCard {
	fn tool(&self) -> &'static str {
		"ast_grep"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, _ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::ast_grep::Params>(view);
		let pattern = args
			.as_ref()
			.and_then(|value| value.get("pat"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let path = args
			.as_ref()
			.and_then(|value| value.get("path"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let result = typed_result::<omp_tools::ast_grep::Payload>(view);
		let matches = result
			.as_ref()
			.and_then(|value| value.get("matches"))
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();
		let match_count = result
			.as_ref()
			.and_then(|value| value.get("match_count").or_else(|| value.get("total")))
			.and_then(Value::as_u64)
			.unwrap_or(matches.len() as u64);
		let file_count = result
			.as_ref()
			.and_then(|value| value.get("file_count"))
			.and_then(Value::as_u64)
			.unwrap_or_else(|| {
				matches
					.iter()
					.filter_map(|entry| entry.get("path").and_then(Value::as_str))
					.collect::<BTreeSet<_>>()
					.len() as u64
			});
		let searched = result
			.as_ref()
			.and_then(|value| value.get("files_searched"))
			.and_then(Value::as_u64);
		let scope = result
			.as_ref()
			.and_then(|value| value.get("scope_path"))
			.and_then(Value::as_str)
			.unwrap_or(path.as_str())
			.to_owned();
		let fault = diag_text(view);
		dom! {
			<col>
				match view.status {
					CardStatus::StreamingArgs | CardStatus::InProgress => {
						<row kind=title gap=1>
							<i:pending/>
							<text bold>{"AST Grep:"}</text>
							<text bold>{pattern}</text>
							if !path.is_empty() {
								<text fg=muted>{"in"}</text><text>{path}</text>
							}
							if let Some(badge) = elapsed_badge(view) { {badge} }
						</row>
					},
					CardStatus::Done => {
						<row kind=title gap=1>
							<i:search/>
							<text bold>{"AST Grep:"}</text><text bold>{pattern}</text>
							<text>{match_count.to_string()}</text><text>{"matches"}</text>
							<text fg=muted>{"·"}</text><text>{file_count.to_string()}</text><text>{"files"}</text>
							if !scope.is_empty() {
								<text fg=muted>{"·"}</text><text fg=muted>{"in"}</text><text>{scope.clone()}</text>
							}
							if let Some(searched) = searched {
								<text fg=muted>{"·"}</text><text fg=muted>{"searched"}</text><text>{searched.to_string()}</text>
							}
						</row>
						if expanded {
							<col>
								<row gap=1><i:tree-last/><text>{format!("# {scope}/")}</text></row>
								for (index, entry) in matches.iter().enumerate() {
									if index == 0 || matches[index - 1].get("path") != entry.get("path") {
										<text pad-x=3>{format!("## {}", file_name(entry))}</text>
									}
									<text pad-x=3>{match_line(entry)}</text>
									if let Some(binding) = binding_text(entry) {
										<text pad-x=5>{format!("meta: {binding}")}</text>
									}
								}
							</col>
						} else if match_count > 1 {
							<row gap=1><i:tree-last/><text>{format!("… {} more match", match_count - 1)}</text></row>
						}
					},
					CardStatus::Failed => {
						<row kind=title gap=1><i:error/><text bold>{"Error:"}</text><text fg=err wrap=word>{fault.unwrap_or_default()}</text></row>
					},
				}
			</col>
		}
		.into_component()
	}
}

fn file_name(entry: &Value) -> String {
	entry
		.get("path")
		.and_then(Value::as_str)
		.and_then(|path| path.rsplit('/').next())
		.unwrap_or_default()
		.to_owned()
}

fn match_line(entry: &Value) -> String {
	let line = entry
		.get("line")
		.and_then(Value::as_u64)
		.unwrap_or_default();
	let text = entry
		.get("text")
		.and_then(Value::as_str)
		.unwrap_or_default();
	format!("*{line}│{text}")
}

fn binding_text(entry: &Value) -> Option<String> {
	if let Some(text) = entry.get("bindings").and_then(Value::as_str) {
		return (!text.is_empty()).then(|| text.to_owned());
	}
	let (key, value) = entry.get("bindings")?.as_object()?.iter().next()?;
	let value = value
		.as_str()
		.map(str::to_owned)
		.unwrap_or_else(|| value.to_string());
	Some(format!("${key}={value}"))
}

fn diag_text(view: &CardView<'_>) -> Option<String> {
	view.diag.and_then(|node| {
		node
			.content
			.as_deref()
			.or_else(|| {
				node
					.prop(&omp_dom::PropId::Text.into())
					.and_then(omp_dom::Value::as_str)
			})
			.filter(|text| !text.is_empty())
			.map(str::to_owned)
	})
}
