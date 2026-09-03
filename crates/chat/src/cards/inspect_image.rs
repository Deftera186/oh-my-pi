//! Typed card for `inspect_image@1`.

use omp_core::Str;
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, elapsed_badge, inline_images, result_image};

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
		// The inspected image is shown under the answer when the terminal
		// has a graphics protocol. pi's result carries no image block (only
		// `imagePath`), so there is no text placeholder on the cells tier:
		// the card stays byte-identical to pi there.
		let image = (!path.is_empty() && inline_images(ui)).then(|| {
			let src = Str::new(path.as_str());
			let filename = path.rsplit('/').next().filter(|name| !name.is_empty());
			let mime = if mime.is_empty() { "image" } else { mime.as_str() };
			result_image(&src, mime, filename, ui)
		});
		dom! {
			<col>
				match view.status {
				CardStatus::StreamingArgs | CardStatus::InProgress => {
					<col>
						<row gap=1><i:pending/><text>{"Inspect:"}</text><text>{path}</text>
							if let Some(badge) = elapsed_badge(view) { {badge} }
						</row>
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
							if let Some(image) = image { {image} }
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

#[cfg(test)]
mod tests {
	use std::{env, fs};

	use omp_core::{Str, sf};
	use omp_dom::{KnownTag, Node, PropId, Value};
	use omp_tui::{CellContent, Graphics, Ui, UiContext, test_support::frame_row_text};

	use super::InspectImageCard;
	use crate::cards::{Card as _, CardStatus, CardView};

	fn text_node(tag: KnownTag, text: &str) -> Node {
		Node {
			tag:     tag.into(),
			props:   std::iter::once((PropId::Text.into(), Value::Str(Str::new(text)))).collect(),
			kids:    Vec::new(),
			content: None,
		}
	}

	fn render(path: &str, ui: &UiContext) -> Vec<String> {
		let input = text_node(KnownTag::Input, &sf!(r#"{{"path":"{path}","question":"what?"}}"#));
		let result = text_node(
			KnownTag::Result,
			&sf!(
				r#"{{"answer":"A chart.","model":"m","image_path":"{path}","mime_type":"image/png"}}"#
			),
		);
		let view = CardView {
			input:   &input,
			result:  Some(&result),
			diag:    None,
			usage:   None,
			status:  CardStatus::Done,
			output:  None,
			started: None,
		};
		let component = InspectImageCard.render(&view, false, ui);
		let ui = Ui::from_root(component, 60, ui.clone());
		let frame = ui.frame();
		let mut has_image = false;
		let mut rows = Vec::new();
		for y in 0..frame.size().height {
			rows.push(frame_row_text(frame, y));
			for x in 0..frame.size().width {
				has_image |= matches!(frame.cell(x, y).content(), CellContent::Image { .. });
			}
		}
		rows.push(sf!("image-cells={has_image}").to_string());
		rows
	}

	#[test]
	fn inspect_image_card_embeds_image_when_graphics_supported() {
		let path = env::temp_dir().join(format!("omp-chat-inspect-{}.png", std::process::id()));
		let png = omp_tui::assets::provider_logo("anthropic").expect("packaged png");
		fs::write(&path, png).expect("fixture png");
		let source = path.to_string_lossy().into_owned();

		let kitty = UiContext { graphics: Graphics::KittyPlaceholders, ..UiContext::default() };
		let rows = render(&source, &kitty);
		assert_eq!(rows.last().map(String::as_str), Some("image-cells=true"), "{rows:?}");

		// pi's inspect_image result carries no image block, so the cells tier
		// shows neither an image nor a placeholder line.
		let cells = render(&source, &UiContext::default());
		assert_eq!(cells.last().map(String::as_str), Some("image-cells=false"), "{cells:?}");
		assert!(cells.iter().all(|row| !row.contains("[Image:")), "{cells:?}");
		assert!(cells.iter().any(|row| row.contains("A chart.")), "{cells:?}");
		let _ = fs::remove_file(path);
	}
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
