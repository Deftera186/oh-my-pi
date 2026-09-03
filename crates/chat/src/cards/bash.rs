//! Typed card for `bash@1`.

use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{
	Card, CardStatus, CardView, Component, elapsed_badge, typed_fault, typed_input, typed_result,
};

/// Collapsed output rows shown while a command runs (pi
/// `BASH_DEFAULT_PREVIEW_LINES` = `DEFAULT_TERMINAL_PREVIEW_LINES`): the tail
/// of the live output behind an "earlier lines" marker.
pub const BASH_DEFAULT_PREVIEW_LINES: usize = 10;

/// Shell-command card with durable transcript and terminal metadata.
pub struct BashCard;

/// The live output window while a command runs: the last
/// [`BASH_DEFAULT_PREVIEW_LINES`] logical lines (all of them when expanded)
/// and, when lines were skipped, pi's dim marker
/// `… (N earlier lines, showing M of T) (ctrl+o to expand)`.
fn output_tail(output: &str, expanded: bool) -> Option<(Option<String>, String)> {
	let output = output.trim_end();
	if output.trim().is_empty() {
		return None;
	}
	let total = output.lines().count();
	if expanded || total <= BASH_DEFAULT_PREVIEW_LINES {
		return Some((None, output.to_owned()));
	}
	let skipped = total - BASH_DEFAULT_PREVIEW_LINES;
	let start = output
		.lines()
		.take(skipped)
		.map(|line| line.len() + 1)
		.sum::<usize>();
	let marker = format!(
		"… ({skipped} earlier lines, showing {BASH_DEFAULT_PREVIEW_LINES} of {total}) (ctrl+o to \
		 expand)"
	);
	Some((Some(marker), output[start..].to_owned()))
}

impl Card for BashCard {
	fn tool(&self) -> &'static str {
		"bash"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, _ui: &UiContext) -> Component {
		let args = typed_input::<omp_tools::shell::Params>(view);
		let command = args
			.as_ref()
			.and_then(|value| value.get("command"))
			.and_then(Value::as_str)
			.map(str::to_owned)
			.or_else(|| partial_string(view.args_text().unwrap_or_default(), "command"))
			.unwrap_or_default();
		let cwd = args
			.as_ref()
			.and_then(|value| value.get("cwd"))
			.and_then(Value::as_str);
		let shown_command =
			cwd.map_or_else(|| command.clone(), |cwd| format!("cd {cwd} && {command}"));
		let result = typed_result::<omp_tools::shell::Payload>(view);
		let output = result.as_ref().map(output_text).unwrap_or_default();
		let fault = diag_text(view).or_else(|| {
			result
				.as_ref()
				.and_then(|value| value.get("text").or_else(|| value.get("error")))
				.and_then(Value::as_str)
				.map(str::to_owned)
		});
		let wall_ms = result.as_ref().and_then(|value| {
			value
				.get("wall_ms")
				.or_else(|| value.pointer("/status/wall_clock_ms"))
				.and_then(Value::as_u64)
		});
		let timeout = args
			.as_ref()
			.and_then(|value| value.get("timeout"))
			.and_then(Value::as_f64);
		let exit = result.as_ref().and_then(|value| {
			value
				.get("exit")
				.or_else(|| value.pointer("/status/exit_code"))
				.and_then(Value::as_i64)
		});
		let meta = wall_ms.map(|wall| {
			let mut text = format!("Wall: {:.2}s", wall as f64 / 1_000.0);
			if let Some(timeout) = timeout {
				text.push_str(&format!(" | Timeout: {timeout}s"));
			}
			if view.status == CardStatus::Failed || exit.is_some_and(|code| code != 0) {
				if let Some(exit) = exit {
					text.push_str(&format!(" | Exit: {exit}"));
				}
			}
			text
		});
		let tail = (view.status == CardStatus::InProgress)
			.then(|| view.output.and_then(|output| output_tail(output, expanded)))
			.flatten();
		dom! {
			<box border=round>
				<row pad-x=1 gap=1><text>{"$"}</text><text>{shown_command}</text>
					if let Some(badge) = elapsed_badge(view) { {badge} }
				</row>
				if let Some((marker, lines)) = tail {
					<hr title="Output" title_pad=3/>
					<col pad-x=1>
						if let Some(marker) = marker { <text fg=muted>{marker}</text> }
						<pre>{lines}</pre>
					</col>
				}
				if matches!(view.status, CardStatus::Done | CardStatus::Failed) && (!output.is_empty() || fault.is_some()) {
					<hr title="Output" title_pad=3/>
					<col pad-x=1>
						if !output.is_empty() {
							<pre>{output}</pre>
						}
						if let Some(message) = fault {
							<pre>{message}</pre>
						}
						if let Some(meta) = meta {
							<row fg=muted><i:bracket-left/><text>{meta}</text><i:bracket-right/></row>
						}
					</col>
				}
			</box>
		}
		.into_component()
	}
}

fn output_text(result: &Value) -> String {
	if let Some(frames) = result.get("transcript").and_then(Value::as_array) {
		return frames
			.iter()
			.filter_map(|frame| frame.get("data"))
			.map(bytes_or_text)
			.collect();
	}
	result
		.get("output")
		.and_then(Value::as_str)
		.unwrap_or_default()
		.to_owned()
}

fn bytes_or_text(value: &Value) -> String {
	if let Some(text) = value.as_str() {
		return text.to_owned();
	}
	value
		.as_array()
		.map(|bytes| {
			String::from_utf8_lossy(
				&bytes
					.iter()
					.filter_map(Value::as_u64)
					.filter_map(|byte| u8::try_from(byte).ok())
					.collect::<Vec<_>>(),
			)
			.into_owned()
		})
		.unwrap_or_default()
}

fn partial_string(raw: &str, key: &str) -> Option<String> {
	let start = raw.find(&format!("\"{key}\""))?;
	let value = raw[start..].find(':')? + start + 1;
	let quote = raw[value..].find('"')? + value + 1;
	let bytes = raw.as_bytes();
	let mut escaped = false;
	for index in quote..bytes.len() {
		match (bytes[index], escaped) {
			(b'"', false) => return serde_json::from_str(&raw[quote - 1..=index]).ok(),
			(b'\\', false) => escaped = true,
			_ => escaped = false,
		}
	}
	Some(raw[quote..].replace("\\n", "\n").replace("\\\"", "\""))
}

fn diag_text(view: &CardView<'_>) -> Option<String> {
	typed_fault::<omp_tools::shell::Fault>(view)
		.map(|fault| fault.to_string())
		.or_else(|| {
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
		})
}

#[cfg(test)]
mod tests {
	use omp_core::Str;
	use omp_dom::{KnownTag, Node, PropId, Value};
	use omp_tui::{Ui, UiContext, test_support::frame_row_text};

	use super::{BASH_DEFAULT_PREVIEW_LINES, BashCard, output_tail};
	use crate::cards::{Card as _, CardStatus, CardView};

	fn text_node(tag: KnownTag, text: &'static str) -> Node {
		let mut props = smallvec::SmallVec::new();
		props.push((PropId::Text.into(), Value::Str(Str::new_static(text))));
		Node { tag: tag.into(), props, kids: Vec::new(), content: None }
	}

	fn rows(view: &CardView<'_>, expanded: bool) -> Vec<String> {
		let ui = Ui::from_root(BashCard.render(view, expanded, &UiContext::default()), 60, UiContext::default());
		(0..ui.frame().size().height)
			.map(|y| frame_row_text(ui.frame(), y))
			.collect()
	}

	#[test]
	fn bash_card_streams_the_last_ten_output_lines_while_running() {
		let input = text_node(KnownTag::Input, r#"{"command":"cargo build"}"#);
		let output = (1..=25).map(|n| format!("line {n}\n")).collect::<String>();
		let view = CardView {
			input:   &input,
			result:  None,
			diag:    None,
			usage:   None,
			status:  CardStatus::InProgress,
			output:  Some(&output),
			started: None,
		};
		let rows = rows(&view, false);
		let joined = rows.join("\n");
		assert!(joined.contains("$ cargo build"), "{joined}");
		assert!(joined.contains("Output"), "{joined}");
		assert!(
			joined.contains("… (15 earlier lines, showing 10 of 25) (ctrl+o to expand)"),
			"{joined}"
		);
		for n in 16..=25 {
			assert!(joined.contains(&format!("line {n}")), "line {n} missing: {joined}");
		}
		assert!(!joined.contains("line 15 ") && !joined.contains("line 1 "), "{joined}");
		let shown = rows.iter().filter(|row| row.contains("line ")).count();
		assert_eq!(shown, BASH_DEFAULT_PREVIEW_LINES);

		// Ctrl+O uncaps the window; a settled card never shows the tail.
		let expanded = rows_join(&view, true);
		assert!(expanded.contains("line 1 ") && expanded.contains("line 25"), "{expanded}");
		assert!(!expanded.contains("earlier lines"), "{expanded}");
		let settled = CardView { status: CardStatus::Done, ..view };
		assert!(!rows_join(&settled, false).contains("line 25"));
	}

	fn rows_join(view: &CardView<'_>, expanded: bool) -> String {
		rows(view, expanded).join("\n")
	}

	#[test]
	fn output_tail_windows_logical_lines() {
		assert_eq!(output_tail("", false), None);
		assert_eq!(output_tail("  \n\n", false), None);
		assert_eq!(output_tail("a\nb\n", false), Some((None, "a\nb".to_owned())));
		let (marker, lines) = output_tail("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n", false).unwrap();
		assert_eq!(
			marker.as_deref(),
			Some("… (1 earlier lines, showing 10 of 11) (ctrl+o to expand)")
		);
		assert_eq!(lines, "2\n3\n4\n5\n6\n7\n8\n9\n10\n11");
		assert_eq!(
			output_tail("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n", true),
			Some((None, "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11".to_owned()))
		);
	}

	#[test]
	fn reads_partial_streamed_command() {
		assert_eq!(
			super::partial_string(r#"{"command":"git status --short"#, "command").as_deref(),
			Some("git status --short")
		);
	}
}
