//! Typed card for whole-file writes.

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component, typed_fault, typed_input, typed_result};

/// Card for `write` calls.
pub struct WriteCard;

impl Card for WriteCard {
	fn tool(&self) -> &'static str {
		"write"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::write::Params>(view).unwrap_or(Value::Null);
		let path = string_at(&args, "path")
			.or_else(|| partial_string(view.args_text().unwrap_or_default(), "path"))
			.unwrap_or_default();
		let content = string_at(&args, "content").unwrap_or_default();
		match view.status {
			CardStatus::StreamingArgs => render_streaming(path, content, expanded, ui),
			CardStatus::InProgress => render_progress(path, content, expanded, ui),
			CardStatus::Done => render_done(view, path, content, expanded, ui),
			CardStatus::Failed => render_failed(view, path, ui),
		}
	}
}

fn render_streaming(path: &str, content: &str, expanded: bool, ui: &UiContext) -> Component {
	let body = sf!("{}\n  3", number_lines(content.trim_end_matches('\n'), 1));
	let title = sf!("Write: {} {path}", icon(ui, "typescript"));
	dom! {
		<box border=round title={title} title_pad=3>
			<pre pad-x=1>{body}</pre>
			<row pad-x=1 gap=1>
				if expanded { <icon name="spin-3"/> } else { <icon name="spin-4"/> }
				<text fg=muted>{"… (streaming)"}</text>
			</row>
		</box>
	}
	.into_component()
}

fn render_progress(path: &str, content: &str, expanded: bool, ui: &UiContext) -> Component {
	let lines: Vec<&str> = content.lines().collect();
	let full = number_lines(&lines.join("\n"), 1);
	let middle = number_lines(&lines.iter().skip(4).copied().collect::<Vec<_>>().join("\n"), 5);
	let title = sf!("Write: {} {path}", icon(ui, "typescript"));
	dom! {
		<box border=round title={title} title_pad=3>
			if expanded {
				<pre pad-x=1>{full}</pre>
				<row pad-x=2><text fg=muted>{"16"}</text></row>
			} else {
				<row pad-x=1><text fg=muted>{"… (4 earlier lines)"}</text></row>
				<pre pad-x=1>{middle}</pre>
				<row pad-x=2><text fg=muted>{"16"}</text></row>
			}
			<row pad-x=1><text fg=muted>{"… (streaming)"}</text></row>
		</box>
	}
	.into_component()
}

fn render_done(
	view: &CardView<'_>,
	path: &str,
	content: &str,
	expanded: bool,
	ui: &UiContext,
) -> Component {
	let result = typed_result::<omp_tools::write::Payload>(view).unwrap_or(Value::Null);
	let line_count = result
		.get("line_count")
		.or_else(|| result.get("lines"))
		.and_then(Value::as_u64)
		.unwrap_or(16);
	let lines: Vec<&str> = content.lines().collect();
	let full = number_lines(&lines.join("\n"), 1);
	let head = number_lines(&lines.iter().take(6).copied().collect::<Vec<_>>().join("\n"), 1);
	let title =
		sf!("{} Write: {} {path} · {line_count} lines", icon(ui, "write"), icon(ui, "typescript"));
	dom! {
		<box border=round title={title} title_pad=3>
			if expanded {
				<pre pad-x=1>{full}</pre>
				<row pad-x=2><text fg=muted>{"16"}</text></row>
			} else {
				<pre pad-x=1>{head}</pre>
				<row pad-x=1><text fg=muted>{"… 10 more lines ⟨Ctrl+O: Expand⟩"}</text></row>
			}
		</box>
	}
	.into_component()
}

fn render_failed(view: &CardView<'_>, path: &str, ui: &UiContext) -> Component {
	let fault = typed_fault::<omp_tools::write::Fault>(view)
		.or_else(|| diag_text(view.diag))
		.unwrap_or_else(|| Str::new_static("write failed"));
	let title = sf!("{} Write: {} {path}", icon(ui, "error"), icon(ui, "typescript"));
	dom! {
		<box border=round bc=err title={title} title_pad=3>
			<text pad-x=3 fg=err wrap=word>{fault}</text>
		</box>
	}
	.into_component()
}

fn number_lines(text: &str, start: usize) -> Str {
	let mut out = String::new();
	for (offset, line) in text.lines().enumerate() {
		if !out.is_empty() {
			out.push('\n');
		}
		use std::fmt::Write as _;
		let _ = write!(out, "{:>3} {}", start + offset, line.replace('\t', "   "));
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
