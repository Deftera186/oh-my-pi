//! Typed card for `ast_edit@1`.

use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, typed_input, typed_result};

/// Structural-rewrite proposal card.
pub struct AstEditCard;

impl Card for AstEditCard {
	fn tool(&self) -> &'static str {
		"ast_edit"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::ast_edit::Params>(view);
		let pattern = args
			.as_ref()
			.and_then(|value| value.pointer("/ops/0/pat"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let target = args
			.as_ref()
			.and_then(|value| value.get("paths"))
			.and_then(Value::as_array)
			.and_then(|paths| paths.first())
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let result = typed_result::<omp_tools::ast_edit::Payload>(view);
		let files = result
			.as_ref()
			.and_then(|value| value.get("files"))
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();
		let replacements = result
			.as_ref()
			.and_then(|value| value.get("total_replacements"))
			.and_then(Value::as_u64)
			.unwrap_or_else(|| {
				files
					.iter()
					.filter_map(|file| file.get("replacements")?.as_u64())
					.sum()
			});
		let file_count = result
			.as_ref()
			.and_then(|value| value.get("files_touched"))
			.and_then(Value::as_u64)
			.unwrap_or(files.len() as u64);
		let state = result.as_ref().map(|value| {
			if value
				.get("recovery_root")
				.is_some_and(|root| !root.is_null())
				|| value.get("applied").and_then(Value::as_bool) == Some(true)
			{
				"applied"
			} else {
				"proposed"
			}
		});
		let scope = result
			.as_ref()
			.and_then(|value| value.get("scope_path"))
			.and_then(Value::as_str)
			.unwrap_or_else(|| target.trim_end_matches("/**/*.ts"))
			.to_owned();
		let fault = diag_text(view).unwrap_or_default();
		let done_title = format!(
			"{} AST Edit: {pattern} {}{}{} {replacements} replacements · {file_count} files · in \
			 {scope}",
			icon(ui, "success"),
			icon(ui, "bracket-left"),
			state.unwrap_or("proposed"),
			icon(ui, "bracket-right"),
		);
		let failed_title = format!("{} AST Edit", icon(ui, "error"));
		dom! {
			<col>
				match view.status {
				CardStatus::StreamingArgs | CardStatus::InProgress => {
					<row kind=title gap=1><i:pending/><text bold>{"AST Edit:"}</text><text bold>{pattern}</text><text fg=muted>{"in"}</text><text>{target}</text></row>
				},
				CardStatus::Done => {
					<box border=round pad-x=1 title={done_title} title_pad=3>
						<col>
							for (index, file) in files.iter().enumerate() {
								if expanded || index == 0 {
									if expanded && index > 0 {
										<text>{""}</text>
									}
									<row gap=1><text>{"#"}</text><text>{format!("{}/", parent_dir(file))}</text></row>
									<row gap=1><text>{"##"}</text><text>{file_name(file)}</text><text>{replacement_label(file)}</text></row>
																			for line in diff_lines(file) {
											<pre>{line}</pre>
										}
								}
							}
							if !expanded && file_count > 1 {
								<row gap=1 fg=muted><text>{"…"}</text><text>{(file_count - 1).to_string()}</text><text>{"more"}</text><text>{"change"}</text></row>
							}
						</col>
					</box>
				},
				CardStatus::Failed => {
					<box border=round pad-x=1 title={failed_title} title_pad=3>
						<text pad-x=2 fg=err wrap=word>{fault}</text>
					</box>
				},
				}
			</col>
		}
		.into_component()
	}
}

fn icon<'a>(ui: &'a UiContext, name: &'a str) -> &'a str {
	ui.charset.icon_named(name).unwrap_or(name)
}

fn file_path(file: &Value) -> &str {
	file.get("path").and_then(Value::as_str).unwrap_or_default()
}

fn parent_dir(file: &Value) -> &str {
	file_path(file)
		.rsplit_once('/')
		.map_or("", |(parent, _)| parent)
}

fn file_name(file: &Value) -> &str {
	file_path(file).rsplit('/').next().unwrap_or_default()
}

fn replacement_label(file: &Value) -> String {
	let count = file
		.get("replacements")
		.and_then(Value::as_u64)
		.unwrap_or_default();
	format!("({count} replacement{})", if count == 1 { "" } else { "s" })
}

fn diff_lines(file: &Value) -> Vec<String> {
	let Some(diff) = file.get("diff") else {
		return Vec::new();
	};
	if let Some(text) = diff.as_str() {
		return text.lines().map(str::to_owned).collect();
	}
	diff
		.as_array()
		.into_iter()
		.flatten()
		.map(|line| {
			let kind = line.get("kind").and_then(Value::as_str).unwrap_or_default();
			let marker = if kind == "add" {
				'+'
			} else if kind == "del" {
				'-'
			} else {
				' '
			};
			let number = line.get("line").and_then(Value::as_u64).unwrap_or_default();
			let raw = line.get("text").and_then(Value::as_str).unwrap_or_default();
			let tabs = raw.bytes().take_while(|byte| *byte == b'\t').count();
			let leading = if tabs == 1 { 1 } else { tabs.saturating_mul(2) };
			format!("{marker}{number}   {}{}", " ".repeat(leading), raw.trim_start_matches('\t'))
		})
		.collect()
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
