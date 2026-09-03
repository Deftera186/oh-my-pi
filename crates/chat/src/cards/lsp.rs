//! Typed card for `lsp@1`.

use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, typed_input, typed_result};

/// Language-server request and reference-result card.
pub struct LspCard;

impl Card for LspCard {
	fn tool(&self) -> &'static str {
		"lsp"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::lsp::Params>(view);
		let action = args
			.as_ref()
			.and_then(|value| value.get("action"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let path = args
			.as_ref()
			.and_then(|value| value.get("file"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let line = args
			.as_ref()
			.and_then(|value| value.get("line"))
			.and_then(Value::as_u64);
		let symbol = args
			.as_ref()
			.and_then(|value| value.get("symbol"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let result = typed_result::<omp_tools::lsp::Payload>(view);
		let files = result
			.as_ref()
			.and_then(|value| value.get("references"))
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();
		let count: usize = files
			.iter()
			.filter_map(|file| file.get("locations").and_then(Value::as_array))
			.map(Vec::len)
			.sum();
		let fault = diag_text(view).unwrap_or_default();
		let title = format!(
			"{} LSP {action}",
			icon(
				ui,
				if view.status == CardStatus::Failed {
					"error"
				} else {
					"lsp"
				}
			),
		);
		dom! {
			<col>
				match view.status {
				CardStatus::StreamingArgs | CardStatus::InProgress => {
					<row gap=1>
						<i:pending/><text>{"LSP:"}</text><text>{action}</text>
						if !path.is_empty() {
							<text>{if let Some(line) = line { format!("{path}:{line}") } else { path.clone() }}</text>
						}
						if !symbol.is_empty() {
							<text>{format!("({symbol})")}</text>
						}
					</row>
				},
				CardStatus::Done | CardStatus::Failed => {
					<box border=round pad-x=1 title={title} title_pad=3 bc={if view.status == CardStatus::Failed { "err" } else { "accent" }}>
						if !path.is_empty() { <text>{path}</text> }
						if let Some(line) = line { <row gap=1><text>{"line"}</text><text>{line.to_string()}</text></row> }
						if !symbol.is_empty() { <row gap=1><text>{"symbol:"}</text><text>{symbol}</text></row> }
						<hr title="Response" title_pad=3/>
						if view.status == CardStatus::Failed {
							if expanded {
								<row gap=1><i:info-status/><text>{"Output"}</text></row>
								<row pad-x=1 gap=1><i:tree-last/><text>{fault}</text></row>
							} else {
								<row gap=1><i:info-status/><text>{fault}</text></row>
							}
						} else if count > 0 {
							<row gap=1><i:lsp/><text>{if expanded { format!("{count} found") } else { format!("{count} found⟨Ctrl+O: Expand⟩") }}</text></row>
							<col pad-x=1>
								for (file_index, file) in files.iter().enumerate() {
									if expanded || file_index < 3 {
										<row>
											<icon name={if expanded && file_index + 1 == files.len() { "tree-last" } else { "tree-branch" }}/><text>{" "}</text>
											<text>{file_path(file)}</text><text>{" "}</text><text>{reference_label(file)}</text>
										</row>
										for (location_index, location) in locations(file).iter().enumerate() {
											if expanded || location_index == 0 {
												<row>
													if expanded && file_index + 1 == files.len() { <spacer w=2/> } else { <i:tree-vertical/><text>{"   "}</text> }
													<icon pad-x=1 name={if expanded && location_index + 1 == locations(file).len() || !expanded && locations(file).len() == 1 { "tree-last" } else { "tree-branch" }}/>
													<text>{format!("line {}", location_label(location))}</text>
												</row>
												if expanded {
													<row>
														if expanded && file_index + 1 == files.len() { <spacer w=3/> } else { <i:tree-vertical/><spacer w=2/> }
														if expanded && location_index + 1 == locations(file).len() || !expanded && locations(file).len() == 1 { <spacer w=3/> } else { <i:tree-vertical/><spacer w=2/> }
														<text>{format!("at {}", location_href(file, location))}</text>
													</row>
												}
											}
										}
										if !expanded && locations(file).len() > 1 {
											<row><i:tree-vertical/><text>{"   "}</text><icon name="tree-last" pad-x=1/><text>{format!("… {} more", locations(file).len() - 1)}</text></row>
										}
									}
								}
								if !expanded && files.len() > 3 {
									<row><i:tree-last/><text pad-x=1>{format!("… {} more file", files.len() - 3)}</text></row>
								}
							</col>
						}
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

fn file_path(file: &Value) -> String {
	file
		.get("path")
		.and_then(Value::as_str)
		.unwrap_or_default()
		.to_owned()
}

fn locations(file: &Value) -> &[Value] {
	file
		.get("locations")
		.and_then(Value::as_array)
		.map(Vec::as_slice)
		.unwrap_or_default()
}

fn reference_label(file: &Value) -> String {
	let count = locations(file).len();
	format!("{count} reference{}", if count == 1 { "" } else { "s" })
}

fn location_label(location: &Value) -> String {
	let line = location
		.get("line")
		.and_then(Value::as_u64)
		.unwrap_or_default();
	location
		.get("col")
		.and_then(Value::as_u64)
		.map_or_else(|| line.to_string(), |column| format!("{line}, col {column}"))
}

fn location_href(file: &Value, location: &Value) -> String {
	let mut href = format!(
		"{}:{}",
		file_path(file),
		location
			.get("line")
			.and_then(Value::as_u64)
			.unwrap_or_default()
	);
	if let Some(column) = location.get("col").and_then(Value::as_u64) {
		href.push_str(&format!(":{column}"));
	}
	href
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
