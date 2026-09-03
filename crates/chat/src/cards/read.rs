//! Typed card for filesystem and resource reads, including grouped read
//! rollups.

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{
	Card, CardStatus, CardView, Component, elapsed_badge, result_image, typed_fault, typed_input,
	typed_result,
};

/// Card for `read` calls.
pub struct ReadCard;

impl Card for ReadCard {
	fn tool(&self) -> &'static str {
		"read"
	}

	fn render(&self, view: &CardView<'_>, _expanded: bool, ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::read::Params>(view).unwrap_or(Value::Null);
		if let Some(targets) = args.get("targets").and_then(Value::as_array) {
			return render_group(targets, view.status);
		}
		let target = string_at(&args, "path")
			.or_else(|| partial_string(view.args_text().unwrap_or_default(), "path"))
			.unwrap_or_default();
		match view.status {
			CardStatus::StreamingArgs | CardStatus::InProgress => dom! {
				<row gap=1><i:pending/><text bold>{"Read:"}</text><text>{target}</text>
					if let Some(badge) = elapsed_badge(view) { {badge} }
				</row>
			}
			.into_component(),
			CardStatus::Done => render_done(view, target, ui),
			CardStatus::Failed => render_failed(view, target, ui),
		}
	}
}

fn render_done(view: &CardView<'_>, target: &str, ui: &UiContext) -> Component {
	let result = typed_result::<omp_tools::read::Payload>(view).unwrap_or(Value::Null);
	let preview = string_at(&result, "preview_text")
		.or_else(|| {
			result
				.get("parts")?
				.as_array()?
				.iter()
				.find_map(|part| string_at(part, "text"))
		})
		.map(Str::new);
	let start = result
		.get("start_line")
		.or_else(|| result.get("preview").and_then(|p| p.get("start")))
		.and_then(Value::as_u64)
		.unwrap_or(1);
	let preview = preview.map(|text| number_preview(text.as_str(), start));
	let src = string_at(&result, "resolved_path").map(Str::new);
	let title = sf!("{} Read {target}", icon(ui, "card-bullet"));
	let images = result_images(&result, target, ui);
	dom! {
		<box border=round bc=muted title={title} title_pad=3>
			if let Some(preview) = preview { <pre wrap=word>{preview}</pre> }
			for image in images { {image} }
			if let Some(src) = src {
				<hr title="Output" title_pad=3/>
				<row gap=1 fg=muted pad-x=1><text>{"⟨Resolved path:"}</text><text>{sf!("{src}⟩")}</text></row>
			}
		</box>
	}
	.into_component()
}

fn render_failed(view: &CardView<'_>, target: &str, ui: &UiContext) -> Component {
	let fault = typed_fault::<omp_tools::read::Fault>(view)
		.or_else(|| diag_text(view.diag))
		.unwrap_or_else(|| Str::new_static("read failed"));
	let title = sf!("{} Read {target}", icon(ui, "error"));
	dom! {
		<box border=round bc=err title={title} title_pad=3>
			<text fg=err wrap=word pad-x=1>{fault}</text>
		</box>
	}
	.into_component()
}

fn render_group(targets: &[Value], status: CardStatus) -> Component {
	let count = targets.len();
	dom! {
		<col pad-x=1>
			<row gap=1><i:bullet/><text bold>{"Read"}</text><text>{sf!("({count})")}</text></row>
			<col pad-x=2>
				for (index, target) in targets.iter().enumerate() {
					<row gap=1>
						if index + 1 == targets.len() { <i:tree-last/> } else { <i:tree-branch/> }
						if target.get("error").and_then(Value::as_bool) == Some(true) { <i:error/> }
						else if matches!(status, CardStatus::StreamingArgs | CardStatus::InProgress) { <i:pending/> }
						<text>{string_at(target, "label").or_else(|| string_at(target, "path")).unwrap_or_default()}</text>
					</row>
					if let Some(usage) = target.get("usage") {
						if index + 1 == targets.len() {
							<row fg=muted gap=2 pad-x=3>
								<text>{string_at(usage, "timestamp").unwrap_or_default()}</text>
								<row gap=1><i:input/><text>{string_at(usage, "input").unwrap_or_default()}</text></row>
								<row gap=1><i:output/><text>{string_at(usage, "output").unwrap_or_default()}</text></row>
								<row gap=1><i:cache/><text>{string_at(usage, "cache").unwrap_or_default()}</text></row>
								<row gap=1><i:time/><text>{string_at(usage, "time").unwrap_or_default()}</text></row>
								<row gap=1><i:throughput/><text>{string_at(usage, "throughput").unwrap_or_default()}</text></row>
							</row>
						} else {
							<row fg=muted gap=2>
								<i:tree-vertical/><text>{string_at(usage, "timestamp").unwrap_or_default()}</text>
								<row gap=1><i:input/><text>{string_at(usage, "input").unwrap_or_default()}</text></row>
								<row gap=1><i:output/><text>{string_at(usage, "output").unwrap_or_default()}</text></row>
								<row gap=1><i:cache/><text>{string_at(usage, "cache").unwrap_or_default()}</text></row>
								<row gap=1><i:time/><text>{string_at(usage, "time").unwrap_or_default()}</text></row>
								<row gap=1><i:throughput/><text>{string_at(usage, "throughput").unwrap_or_default()}</text></row>
							</row>
						}
					}
				}
			</col>
		</col>
	}
	.into_component()
}

/// pi `read-tool-group.ts`: consecutive `read` calls of one turn as a single
/// compact tree — the bullet header with the call count, one branch per
/// call (label from its path and range; a pending spinner while it runs; an
/// error mark when it failed), and, when the turn contained only reads, the
/// turn's usage row attached under the last branch (`TC-13`).
pub fn render_calls_group(
	views: &[CardView<'_>],
	_expanded: bool,
	usage: Option<Str>,
	ui: &UiContext,
) -> Component {
	let count = views.len();
	let calls = views
		.iter()
		.map(|view| {
			let args = typed_input::<omp_tools::read::Params>(view).unwrap_or(Value::Null);
			let label = string_at(&args, "path")
				.map(str::to_owned)
				.or_else(|| partial_string(view.args_text().unwrap_or_default(), "path").map(str::to_owned))
				.unwrap_or_default();
			let label = match args.get("offset").and_then(Value::as_u64) {
				Some(offset) => match args.get("limit").and_then(Value::as_u64) {
					Some(limit) => sf!("{label}:{offset}-{}", offset.saturating_add(limit).saturating_sub(1)),
					None => sf!("{label}:{offset}-"),
				},
				None => Str::new(label),
			};
			(label, view.status)
		})
		.collect::<Vec<_>>();
	let title = sf!("{} Read ({count})", icon(ui, "card-bullet"));
	dom! {
		<col pad-x=1>
			<text bold>{title}</text>
			<col pad-x=2>
				for (index, (label, status)) in calls.iter().enumerate() {
					<row gap=1>
						if index + 1 == count { <i:tree-last/> } else { <i:tree-branch/> }
						match status {
							CardStatus::Failed => <i:error/>,
							CardStatus::StreamingArgs | CardStatus::InProgress => <spinner kind=status/>,
							CardStatus::Done => <text>{""}</text>,
						}
						<text>{label.clone()}</text>
					</row>
				}
				if let Some(usage) = usage.clone() {
					<row fg=muted pad-x=3><text>{usage}</text></row>
				}
			</col>
		</col>
	}
	.into_component()
}

/// Image blob parts of a read payload (pi `tool-execution.ts` image
/// blocks): each renders inline through `<img src="artifact://sha256/…">`
/// when the terminal has a graphics protocol, else as pi's
/// `[Image: <name> [<mime>]]` placeholder. The `artifact://` source is
/// resolved to the session blob store by the application's image-source
/// resolver ([`omp_tui::register_image_scheme`]).
fn result_images(result: &Value, target: &str, ui: &UiContext) -> Vec<Component> {
	let Some(parts) = result.get("parts").and_then(Value::as_array) else {
		return Vec::new();
	};
	let filename = target
		.rsplit('/')
		.next()
		.filter(|name| !name.is_empty());
	parts
		.iter()
		.filter(|part| string_at(part, "kind") == Some("blob"))
		.filter_map(|part| {
			let blob = part.get("blob")?;
			let mime = string_at(blob, "media_type")?;
			if !mime.starts_with("image/") {
				return None;
			}
			let hash = string_at(blob, "hash")?;
			Some(result_image(&sf!("artifact://sha256/{hash}"), mime, filename, ui))
		})
		.collect()
}

fn number_preview(text: &str, start: u64) -> Str {
	let mut out = String::new();
	for (offset, source) in text.lines().enumerate() {
		let line = source.replace('\t', "   ");
		let mut display = sf!("{} {line}", start.saturating_add(offset as u64));
		while display.len() > 96 {
			let Some(split) = display[..=96].rfind(' ') else {
				break;
			};
			if !out.is_empty() {
				out.push('\n');
			}
			out.push(' ');
			out.push_str(display[..split].trim_end());
			display = sf!("{}", display[split + 1..].trim_start());
		}
		if !out.is_empty() {
			out.push('\n');
		}
		out.push(' ');
		out.push_str(&display);
	}
	Str::new(out)
}

fn icon<'a>(ui: &'a UiContext, name: &str) -> &'a str {
	ui.charset.icon_named(name).unwrap_or_default()
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

#[cfg(test)]
mod tests {
	use std::{env, fs, sync::Arc};

	use omp_core::{Str, sf};
	use omp_dom::{KnownTag, Node, PropId, Value};
	use omp_tui::{CellContent, Graphics, Ui, UiContext, test_support::frame_row_text};

	use super::ReadCard;
	use crate::cards::{Card as _, CardStatus, CardView};

	const HASH: &str = "3a7bd3e2360a3d29eea436fcfb7e44c735d117c42d1c1835420b6b9942dd4f1b";

	fn text_node(tag: KnownTag, text: Str) -> Node {
		Node {
			tag:     tag.into(),
			props:   std::iter::once((PropId::Text.into(), Value::Str(text))).collect(),
			kids:    Vec::new(),
			content: None,
		}
	}

	fn render(ui: &UiContext) -> (Vec<String>, bool) {
		let input = text_node(KnownTag::Input, Str::new(r#"{"path":"docs/logo.png"}"#));
		let payload = sf!(
			r#"{{"parts":[{{"kind":"text","text":"Image file docs/logo.png"}},{{"kind":"blob","blob":{{"hash":"{HASH}","media_type":"image/png","byte_len":12}},"alt":"logo"}}]}}"#
		);
		let result = text_node(KnownTag::Result, payload);
		let view = CardView {
			input:   &input,
			result:  Some(&result),
			diag:    None,
			usage:   None,
			status:  CardStatus::Done,
			output:  None,
			started: None,
		};
		let component = ReadCard.render(&view, false, ui);
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
		(rows, has_image)
	}

	#[test]
	fn read_card_embeds_image_blob_or_pi_placeholder() {
		// The application resolves `artifact://sha256/<hex>` to the blob
		// store; here a temp file stands in for the CAS entry.
		let path = env::temp_dir().join(format!("omp-chat-read-blob-{}.png", std::process::id()));
		let png = omp_tui::assets::provider_logo("anthropic").expect("packaged png");
		fs::write(&path, png).expect("fixture png");
		let resolved = path.clone();
		omp_tui::register_image_scheme(
			"artifact",
			Arc::new(move |source| {
				(source == sf!("artifact://sha256/{HASH}").as_str()).then(|| resolved.clone())
			}),
		);

		let kitty = UiContext { graphics: Graphics::KittyPlaceholders, ..UiContext::default() };
		let (rows, has_image) = render(&kitty);
		assert!(has_image, "graphics tier paints the blob inline: {rows:?}");
		assert!(rows.iter().all(|row| !row.contains("[Image:")), "{rows:?}");

		let (rows, has_image) = render(&UiContext::default());
		assert!(!has_image, "{rows:?}");
		assert!(
			rows.iter().any(|row| row.contains("[Image: logo.png [image/png]]")),
			"cells tier shows pi's placeholder: {rows:?}"
		);
		let _ = fs::remove_file(path);
	}
}
