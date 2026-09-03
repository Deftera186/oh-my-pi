//! Typed card for grouped text search matches.

use std::collections::BTreeMap;

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, typed_fault, typed_input, typed_result};

/// Card for `grep` calls.
pub struct GrepCard;

impl Card for GrepCard {
	fn tool(&self) -> &'static str {
		"grep"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::grep::Params>(view).unwrap_or(Value::Null);
		let query = string_at(&args, "pattern")
			.or_else(|| partial_string(view.args_text().unwrap_or_default(), "pattern"))
			.unwrap_or_default();
		let path = string_at(&args, "path");
		match view.status {
			CardStatus::StreamingArgs | CardStatus::InProgress => dom! {
				<row gap=1 pad-x=1>
					<i:pending/><text bold>{"Grep:"}</text><text>{query}</text>
					if let Some(path) = path { <text fg=muted>{"in"}</text><text>{path}</text> }
				</row>
			}
			.into_component(),
			CardStatus::Done => render_done(view, query, path, expanded, ui),
			CardStatus::Failed => render_failed(view),
		}
	}
}

fn render_done(
	view: &CardView<'_>,
	query: &str,
	path: Option<&str>,
	expanded: bool,
	ui: &UiContext,
) -> Component {
	let result = typed_result::<omp_tools::grep::Payload>(view).unwrap_or(Value::Null);
	let groups = normalize_groups(&result);
	let match_count = result
		.get("total")
		.and_then(Value::as_u64)
		.unwrap_or_else(|| {
			groups
				.iter()
				.flat_map(|group| &group.files)
				.map(|file| file.matches.len() as u64)
				.sum()
		});
	let file_count = result
		.get("total_files")
		.and_then(Value::as_u64)
		.unwrap_or_else(|| groups.iter().map(|group| group.files.len() as u64).sum());
	let shown_matches = if expanded { match_count } else { 2 };
	let hidden = match_count.saturating_sub(shown_matches);
	dom! {
		<col pad-x=1 w="100%">
			<row gap=1>
				<i:search/><text bold>{"Grep:"}</text><text>{query}</text><text>{&match_count}</text><text>{"matches"}</text>
				<text fg=muted>{"·"}</text><text>{&file_count}</text><text>{"files"}</text>
				if let Some(path) = path { <text fg=muted>{"· in"}</text><text>{path}</text> }
			</row>
			<col>
				for (group_index, group) in groups.iter().enumerate() {
					if expanded || group_index == 0 {
						<row gap=1>
							if group_index + 1 == groups.len() { <i:tree-last/> } else { <i:tree-branch/> }
							<text>{"#"}</text><text bold>{group.dir.clone()}</text>
						</row>
						for (file_index, file) in group.files.iter().enumerate() {
							if expanded || file_index < 2 {
								if group_index + 1 == groups.len() {
									<text pad-x=3>{sf!("## {}", file.name)}</text>
								} else {
									<text>{sf!("{}  ## {}", icon(ui, "tree-vertical"), file.name)}</text>
								}
								for row in &file.matches {
									if expanded || file_index == 0 {
										if group_index + 1 == groups.len() {
											<text fg=muted pad_x={3_u16.saturating_add(line_padding(file, row))} w="100%">
												{sf!("*{}│{}", row.line, row.text)}
											</text>
										} else {
											<text fg=muted w="100%">{match_line(file, row, icon(ui, "tree-vertical"))}</text>
										}
									}
								}
							}
						}
					}
				}
				if !expanded && hidden > 0 {
					<row gap=1><i:tree-last/><text fg=muted>{"…"}</text><text fg=muted>{&hidden}</text><text fg=muted>{"more matches"}</text></row>
				}
			</col>
		</col>
	}
	.into_component()
}

fn icon<'a>(ui: &'a UiContext, name: &str) -> &'a str {
	ui.charset.icon_named(name).unwrap_or_default()
}

fn line_padding(file: &FileMatches, row: &Match) -> u16 {
	let padding = file
		.matches
		.iter()
		.map(|row| decimal_width(row.line))
		.max()
		.unwrap_or(1)
		.saturating_sub(decimal_width(row.line));
	u16::try_from(padding).unwrap_or(u16::MAX)
}

fn match_line(file: &FileMatches, row: &Match, rail: &str) -> Str {
	let padding = usize::from(line_padding(file, row));
	sf!("{rail}  {}*{}│{}", " ".repeat(padding), row.line, row.text)
}

const fn decimal_width(mut value: u64) -> usize {
	let mut width = 1;
	while value >= 10 {
		value /= 10;
		width += 1;
	}
	width
}

fn render_failed(view: &CardView<'_>) -> Component {
	let fault = typed_fault::<omp_tools::grep::Fault>(view)
		.or_else(|| diag_text(view.diag))
		.unwrap_or_else(|| Str::new_static("grep failed"));
	dom! { <row gap=1 pad-x=1><i:error/><text bold>{"Error:"}</text><text fg=err>{fault}</text></row> }
		.into_component()
}

struct Group {
	dir:   Str,
	files: Vec<FileMatches>,
}

struct FileMatches {
	path:    Str,
	name:    Str,
	matches: Vec<Match>,
}

struct Match {
	line: u64,
	text: Str,
}

fn normalize_groups(result: &Value) -> Vec<Group> {
	let mut by_dir: BTreeMap<String, Vec<FileMatches>> = BTreeMap::new();
	if let Some(matches) = result.get("matches").and_then(Value::as_array) {
		for row in matches {
			let path = string_at(row, "path").unwrap_or_default();
			let (dir, name) = path.rsplit_once('/').unwrap_or((".", path));
			let files = by_dir.entry(format!("{dir}/")).or_default();
			let file = if let Some(index) = files.iter().position(|file| file.path.as_str() == path) {
				&mut files[index]
			} else {
				files.push(FileMatches {
					path:    Str::new(path),
					name:    Str::new(name),
					matches: Vec::new(),
				});
				files.last_mut().expect("file was just inserted")
			};
			file.matches.push(Match {
				line: row.get("line").and_then(Value::as_u64).unwrap_or_default(),
				text: Str::new(string_at(row, "text").unwrap_or_default()),
			});
		}
	} else if let Some(files) = result.get("files").and_then(Value::as_array) {
		for value in files {
			let path = string_at(value, "path").unwrap_or_default();
			let (dir, name) = path.rsplit_once('/').unwrap_or((".", path));
			let matches = value
				.get("matches")
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
				.map(|row| Match {
					line: row
						.get("line_number")
						.and_then(Value::as_u64)
						.unwrap_or_default(),
					text: Str::new(string_at(row, "line").unwrap_or_default()),
				})
				.collect();
			by_dir
				.entry(format!("{dir}/"))
				.or_default()
				.push(FileMatches { path: Str::new(path), name: Str::new(name), matches });
		}
	}
	by_dir
		.into_iter()
		.map(|(dir, files)| Group { dir: Str::new(dir), files })
		.collect()
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
