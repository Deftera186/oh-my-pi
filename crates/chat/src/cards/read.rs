//! Typed card for filesystem and resource reads, including grouped read
//! rollups.

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, typed_fault, typed_input, typed_result};

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
				<row gap=1><i:pending/><text bold>{"Read:"}</text><text>{target}</text></row>
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
	dom! {
		<box border=round bc=muted title={title} title_pad=3>
			if let Some(preview) = preview { <pre wrap=word>{preview}</pre> }
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
