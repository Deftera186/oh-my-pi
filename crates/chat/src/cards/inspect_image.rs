//! Typed card for `inspect_image@1`.

use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component};

/// Vision-inspection card.
pub struct InspectImageCard;

impl Card for InspectImageCard {
	fn tool(&self) -> &'static str {
		"inspect_image"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		let args = view.args_json();
		let result = view.result_json();
		let path = result
			.as_ref()
			.and_then(|value| value.get("image_path"))
			.and_then(Value::as_str)
			.or_else(|| args.as_ref()?.get("path")?.as_str())
			.unwrap_or_default()
			.to_owned();
		let question = args
			.as_ref()
			.and_then(|value| value.get("question"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let answer = result
			.as_ref()
			.and_then(|value| value.get("answer"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let model = result
			.as_ref()
			.and_then(|value| value.get("model"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let mime = result
			.as_ref()
			.and_then(|value| value.get("mime_type"))
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let fault = diag_text(view).unwrap_or_default();
		let answer_lines = answer.lines().count();
		let preview = answer.lines().take(4).collect::<Vec<_>>().join("\n");
		let mut done_title = format!("{} Inspect: {path}", icon(ui, "inspect-image"));
		if !model.is_empty() {
			done_title.push_str(" · ");
			done_title.push_str(&model);
		}
		if !mime.is_empty() {
			done_title.push_str(" · ");
			done_title.push_str(&mime);
		}
		let failed_title = format!("{} Inspect: {path}", icon(ui, "error"));
		dom! {
			<col>
				match view.status {
				CardStatus::StreamingArgs | CardStatus::InProgress => {
					<col>
						<row gap=1><i:pending/><text>{"Inspect:"}</text><text>{path}</text></row>
						if !question.is_empty() {
							<row pad-x=1 gap=1><i:tree-last/><text fg=muted>{"Question:"}</text><text fg=accent wrap=word>{question}</text></row>
						}
					</col>
				},
				CardStatus::Done => {
					<box border=round pad-x=1 title={done_title} title_pad=3 bc=muted>
						<col gap=1>
							if !question.is_empty() { <row gap=1><text fg=muted>{"Question:"}</text><text fg=accent wrap=word>{question}</text></row> }
							if !answer.is_empty() {
								if expanded {
									<pre>{answer}</pre>
								} else {
									<col>
										<pre>{preview}</pre>
										if answer_lines > 4 {
											<row gap=1 fg=muted>
												<text>{format!("… {} more lines", answer_lines - 4)}</text>
												<row><i:bracket-left/><text>{"Ctrl+O: Expand"}</text><i:bracket-right/></row>
											</row>
										}
									</col>
								}
							}
						</col>
					</box>
				},
				CardStatus::Failed => {
					<box border=round pad-x=1 title={failed_title} title_pad=3 bc=err>
						if !question.is_empty() { <row gap=1><text fg=muted>{"Question:"}</text><text fg=accent wrap=word>{question}</text></row> }
						<text pad-x=2 fg=err>{fault}</text>
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
