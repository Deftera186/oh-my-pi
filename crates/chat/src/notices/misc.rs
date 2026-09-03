//! Custom transcript notices: late LSP diagnostics, `/tan` breadcrumbs,
//! advisor notes, collaboration guest bubbles, and collapsed synthetic input.

use omp_core::{Str, StrMut, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, dom};

use super::prop_text;
use crate::cards::Component;

/// Dispatches a `<notice kind=K>` custom kind to its renderer; `None` for
/// the controller kinds (`error | warn | info | success`) and anything else.
#[must_use]
pub fn custom_notice(kind: &str, node: &Node) -> Option<Component> {
	match kind {
		"diagnostics" => Some(diagnostics_card(node)),
		"tangent" => Some(tangent_pill(node)),
		"advisor" => Some(advisor_card(node)),
		_ => None,
	}
}

/// One parsed `path:line:col [severity] [source] message (code)` line
/// (pi `parseDiagnosticMessage`, `tools/render-utils.ts:346-359`).
struct Diagnostic<'a> {
	location: String,
	severity: &'a str,
	message:  &'a str,
	code:     Option<&'a str>,
}

impl<'a> Diagnostic<'a> {
	fn parse(line: &'a str) -> Option<Self> {
		let (path, rest) = line.split_once(':')?;
		let (line_no, rest) = rest.split_once(':')?;
		let (col, rest) = rest.split_once(' ')?;
		if path.is_empty() || !is_digits(line_no) || !is_digits(col) {
			return None;
		}
		let rest = rest.trim_start();
		let severity = rest.strip_prefix('[')?;
		let (severity, rest) = severity.split_once(']')?;
		if !matches!(severity, "error" | "warning" | "info" | "hint") {
			return None;
		}
		let mut message = rest.trim_start();
		if let Some(tail) = message.strip_prefix('[')
			&& let Some((_, tail)) = tail.split_once(']')
		{
			message = tail.trim_start();
		}
		let code = message
			.strip_suffix(')')
			.and_then(|body| body.rfind(" ("))
			.map(|at| (&message[at + 2..message.len() - 1], &message[..at]));
		let (code, message) = match code {
			Some((code, body)) => (Some(code), body),
			None => (None, message),
		};
		Some(Self { location: format!("{path}:{line_no}:{col}"), severity, message, code })
	}

	fn icon(&self) -> &'static str {
		match self.severity {
			"error" => "error",
			"warning" => "warning-status",
			_ => "info-status",
		}
	}

	fn color(&self) -> &'static str {
		match self.severity {
			"error" => "error",
			"warning" => "warning",
			_ => "muted",
		}
	}
}

fn is_digits(text: &str) -> bool {
	!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Late LSP diagnostics that arrived after an edit tool returned
/// (pi `late-diagnostics-message.ts` over `formatDiagnostics`,
/// `tools/render-utils.ts:361-487`): a `Late diagnostics` header naming the
/// server, then one tree row per line with a severity glyph, `path:line:col`,
/// and the message tinted by severity.
#[must_use]
pub fn diagnostics_card(node: &Node) -> Component {
	let server = prop_text(node, PropId::Name);
	let lines: Vec<&str> = node
		.content
		.as_deref()
		.map(|content| {
			content
				.lines()
				.map(str::trim_end)
				.filter(|line| !line.trim().is_empty())
				.collect()
		})
		.unwrap_or_default();
	let count = lines.len();
	let errored = lines
		.iter()
		.any(|line| line.contains("[error]"));
	let rows: Vec<Component> = lines
		.into_iter()
		.enumerate()
		.map(|(index, line)| {
			let last = index + 1 == count;
			match Diagnostic::parse(line) {
				Some(diagnostic) => {
					let icon = diagnostic.icon();
					let color = diagnostic.color();
					let message = diagnostic.message.replace('\t', "    ");
					let location = diagnostic.location;
					let code = diagnostic.code.map(|code| sf!("({code})"));
					dom! {
						<row gap=1>
							<icon name={if last { "tree-last" } else { "tree-branch" }} fg=muted dim/>
							<icon name={icon} fg={color}/>
							<text fg=muted dim>{location}</text>
							<text fg={color}>{message}</text>
							if let Some(code) = code { <text fg=muted dim>{code}</text> }
						</row>
					}
					.into_component()
				},
				None => {
					let color = if line.contains("[error]") {
						"error"
					} else if line.contains("[warning]") {
						"warning"
					} else {
						"muted"
					};
					let text = line.replace('\t', "    ");
					dom! {
						<row gap=1>
							<icon name={if last { "tree-last" } else { "tree-branch" }} fg=muted dim/>
							<text fg={color} grow>{text}</text>
						</row>
					}
					.into_component()
				},
			}
		})
		.collect();
	dom! {
		<col pad-x=1>
			<row gap=1>
				<icon name="lsp" fg=accent/>
				if errored { <icon name="error" fg=error/> } else { <icon name="warning-status" fg=warning/> }
				<text bold>{"Late diagnostics"}</text>
				if let Some(server) = server { <text fg=muted dim>{sf!("({server})")}</text> }
			</row>
			<col pad-x=1>{rows}</col>
		</col>
	}
	.into_component()
}

/// pi `TAN_WORK_PREVIEW_LENGTH` (`background-tan-message.ts:7`).
const TAN_WORK_PREVIEW_LENGTH: usize = 56;

/// pi `previewWork` (`background-tan-message.ts:9-13`): tabs to spaces,
/// whitespace runs collapsed, cut to 55 characters plus `…`.
fn preview_work(work: &str) -> Str {
	let mut text = StrMut::with_capacity(work.len());
	for word in work.split_whitespace() {
		if !text.is_empty() {
			text.push(' ');
		}
		text.push_str(word);
	}
	if text.chars().count() <= TAN_WORK_PREVIEW_LENGTH {
		return text.freeze();
	}
	let mut cut = StrMut::with_capacity(TAN_WORK_PREVIEW_LENGTH + 3);
	cut.extend(text.chars().take(TAN_WORK_PREVIEW_LENGTH - 1));
	cut.push('…');
	cut.freeze()
}

/// `/tan` background-dispatch breadcrumb (pi `background-tan-message.ts`):
/// one muted row `<output> Tangent dispatched [task] <id> — <work>`, with the
/// job id in accent and the work preview dimmed.
#[must_use]
pub fn tangent_pill(node: &Node) -> Component {
	let id = prop_text(node, PropId::Id).unwrap_or_else(|| Str::new_static("unknown"));
	let work = prop_text(node, PropId::Label).map(|label| preview_work(&label));
	dom! {
		<row gap=1 pad-x=1>
			<icon name="output" fg=muted/>
			<text fg=muted>{"Tangent dispatched"}</text>
			<text fg=muted dim>{"[task]"}</text>
			<text fg=accent>{id}</text>
			if let Some(work) = work {
				<icon name="dash" fg=muted dim/>
				<text fg=muted dim>{work}</text>
			}
		</row>
	}
	.into_component()
}

/// pi `severityColor` (`advisor-message.ts:32-41`).
fn severity_color(severity: Option<&str>) -> &'static str {
	match severity {
		Some("blocker") => "error",
		Some("concern") => "warning",
		_ => "muted",
	}
}

/// Advisor note injected into the primary session (pi `advisor-message.ts`):
/// a bold `Advisor` header tag with the severity badge, then a heavy rail
/// tinted per severity beside the bold summary and the note's paragraphs.
#[must_use]
pub fn advisor_card(node: &Node) -> Component {
	let severity = prop_text(node, PropId::Severity);
	let color = severity_color(severity.as_deref());
	let summary = prop_text(node, PropId::Label);
	let paragraphs: Vec<Str> = node
		.content
		.as_deref()
		.map(|content| {
			content
				.split('\n')
				.filter(|paragraph| !paragraph.trim().is_empty())
				.map(|paragraph| Str::new(paragraph.replace('\t', "    ")))
				.collect()
		})
		.unwrap_or_default();
	dom! {
		<col pad-x=1>
			<row gap=1>
				<icon name="advisor" fg=accent/>
				<text bold fg=accent>{"Advisor"}</text>
				if let Some(severity) = severity {
					<row>
						<icon name="bracket-left" fg={color}/>
						<text fg={color}>{severity}</text>
						<icon name="bracket-right" fg={color}/>
					</row>
				}
			</row>
			<row gap=1 pad-x=1>
				<hr vertical border=heavy fg={color}/>
				<col grow>
					if let Some(summary) = summary { <text bold>{summary}</text> }
					for paragraph in paragraphs { <text>{paragraph}</text> }
				</col>
			</row>
		</col>
	}
	.into_component()
}

/// Collaboration guest prompt (pi `collab-prompt-message.ts`): the user
/// bubble under a bold accent `«author» ›` tag naming who typed it.
#[must_use]
pub fn guest_bubble(author: &str, text: Str) -> Component {
	let author = author.trim();
	let tag = sf!("«{}» ›", if author.is_empty() { "guest" } else { author });
	dom! {
		<col>
			<text fg=accent bold pad-x=1>{tag}</text>
			{bubble(text)}
		</col>
	}
	.into_component()
}

/// pi `user-message.ts` bubble: `userMessageBg` tint with one cell of
/// padding on every side.
fn bubble(text: Str) -> Component {
	dom! { <text bg=surface pad="1 1">{text}</text> }.into_component()
}

/// pi `formatBytes` (`utils/format.ts:54-59`): `512B`, `1.5KB`, `2.3MB`.
fn format_bytes(bytes: usize) -> String {
	const KB: f64 = 1024.0;
	#[allow(clippy::cast_precision_loss, reason = "display rounding only")]
	let n = bytes as f64;
	if n < KB {
		format!("{bytes}B")
	} else if n < KB * KB {
		format!("{:.1}KB", n / KB)
	} else if n < KB * KB * KB {
		format!("{:.1}MB", n / (KB * KB))
	} else {
		format!("{:.1}GB", n / (KB * KB * KB))
	}
}

/// pi `syntheticInputLabel` (`user-message.ts:184-192`): the first Markdown
/// heading's text, else `Synthetic input`.
fn synthetic_label(text: &str) -> &str {
	for raw in text.lines() {
		let line = raw.trim();
		if line.is_empty() {
			continue;
		}
		let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
		if (1..=6).contains(&hashes)
			&& let Some(heading) = line[hashes..].strip_prefix(char::is_whitespace)
		{
			let heading = heading.trim();
			return if heading.is_empty() { "Synthetic input" } else { heading };
		}
		return "Synthetic input";
	}
	"Synthetic input"
}

/// pi `summarizeSyntheticInput` (`user-message.ts:176-181`):
/// `<label> · <size> · <n> lines`.
fn synthetic_summary(text: &str) -> Str {
	let lines = if text.is_empty() { 0 } else { text.split('\n').count() };
	sf!(
		"{} · {} · {lines} line{}",
		synthetic_label(text),
		format_bytes(text.len()),
		if lines == 1 { "" } else { "s" }
	)
}

/// Synthetic (agent-attributed) user input (pi
/// `CollapsedSyntheticMessageComponent`, `user-message.ts:100-150`): one dim
/// `<label> · <size> · <n> lines · ctrl+o` row; expanded, the full bubble
/// follows it.
#[must_use]
pub fn synthetic_row(text: &str, expanded: bool) -> Component {
	let summary = sf!("{} · ctrl+o", synthetic_summary(text));
	let body = expanded.then(|| bubble(Str::new(text)));
	dom! {
		<col>
			<text fg=muted dim pad-x=1 truncate=end>{summary}</text>
			if let Some(body) = body { {body} }
		</col>
	}
	.into_component()
}

#[cfg(test)]
mod tests {
	use omp_dom::{KnownTag, PropKey, Tag, Value};
	use omp_tui::{CellContent, Ui, UiContext, frame_text};
	use smallvec::smallvec;

	use super::*;

	fn notice(kind: &str, props: &[(PropId, &str)], content: Option<&str>) -> Node {
		let mut all: smallvec::SmallVec<(PropKey, Value), 4> =
			smallvec![(PropId::Kind.into(), Value::Str(Str::new(kind)))];
		for (prop, value) in props {
			all.push(((*prop).into(), Value::Str(Str::new(value))));
		}
		Node {
			tag: Tag::Known(KnownTag::Notice),
			props: all,
			kids: Vec::new(),
			content: content.map(Str::new),
		}
	}

	fn render(component: Component, width: u16) -> String {
		let ui = Ui::from_root(component, width, UiContext::default());
		frame_text(ui.frame())
	}

	#[test]
	fn tangent_pill_text() {
		let node = notice(
			"tangent",
			&[(PropId::Id, "job-7"), (PropId::Label, "refactor\tthe   parser\nand add tests")],
			None,
		);
		let text = render(tangent_pill(&node), 80);
		assert_eq!(text, " ⤴ Tangent dispatched [task] job-7 — refactor the parser and add tests");

		let long = "a".repeat(40) + " " + &"b".repeat(40);
		let preview = preview_work(&long);
		assert_eq!(preview.chars().count(), TAN_WORK_PREVIEW_LENGTH);
		assert!(preview.ends_with('…'));
		let exact = "x".repeat(TAN_WORK_PREVIEW_LENGTH);
		assert_eq!(preview_work(&exact), exact.as_str(), "exactly 56 characters is not cut");

		let bare = notice("tangent", &[(PropId::Id, "job-8")], None);
		assert_eq!(render(tangent_pill(&bare), 80), " ⤴ Tangent dispatched [task] job-8");
		assert!(custom_notice("tangent", &bare).is_some());
		assert!(custom_notice("error", &bare).is_none());
	}

	fn rail_color(component: Component) -> omp_tui::Color {
		let ui = Ui::from_root(component, 40, UiContext::default());
		let frame = ui.frame();
		for y in 0..frame.size().height {
			for x in 0..frame.size().width {
				if let CellContent::Grapheme { text, .. } = frame.cell(x, y).content()
					&& text == "┃"
				{
					return frame.cell(x, y).style().foreground_color();
				}
			}
		}
		panic!("no rail painted:\n{}", frame_text(frame));
	}

	#[test]
	fn advisor_rail_color_follows_severity() {
		let theme = UiContext::default().theme;
		let body = "The retry loop re-reads the config every time.\n\nCache it outside the loop.";
		let blocker = notice(
			"advisor",
			&[(PropId::Severity, "blocker"), (PropId::Label, "Config reread per retry")],
			Some(body),
		);
		assert_eq!(rail_color(advisor_card(&blocker)), theme.err);
		let concern = notice("advisor", &[(PropId::Severity, "concern")], Some(body));
		assert_eq!(rail_color(advisor_card(&concern)), theme.warn);
		let plain = notice("advisor", &[], Some(body));
		assert_eq!(rail_color(advisor_card(&plain)), theme.muted);

		let text = render(advisor_card(&blocker), 60);
		let lines: Vec<&str> = text.lines().collect();
		assert!(lines[0].contains("Advisor ⟦blocker⟧"), "{text:?}");
		assert!(lines[1].contains("┃ Config reread per retry"), "{text:?}");
		assert!(text.contains("┃ Cache it outside the loop."), "{text:?}");
		assert_eq!(
			lines.iter().filter(|line| line.contains('┃')).count(),
			3,
			"the rail spans header and both paragraphs: {text:?}"
		);
		let narrow = render(advisor_card(&blocker), 30);
		assert_eq!(
			narrow.lines().filter(|line| line.contains('┃')).count(),
			5,
			"the rail follows wrapped paragraph rows: {narrow:?}"
		);
	}

	#[test]
	fn synthetic_row_collapses_size_and_lines() {
		let mut text = String::from("# Session update\n");
		for index in 0..13 {
			text.push_str(&format!("line {index} of the replay dump {}\n", "x".repeat(64)));
		}
		let text = text.trim_end().to_owned();
		assert_eq!(text.split('\n').count(), 14);
		assert_eq!(text.len(), 1202, "1202 bytes is 1.2KB");
		assert_eq!(synthetic_summary(&text), "Session update · 1.2KB · 14 lines");
		assert_eq!(synthetic_summary(""), "Synthetic input · 0B · 0 lines");
		assert_eq!(synthetic_summary("one"), "Synthetic input · 3B · 1 line");
		assert_eq!(synthetic_summary("#no heading\nmore"), "Synthetic input · 16B · 2 lines");

		let collapsed = render(synthetic_row(&text, false), 60);
		assert_eq!(collapsed, " Session update · 1.2KB · 14 lines · ctrl+o");
		let expanded = render(synthetic_row(&text, true), 60);
		assert!(expanded.starts_with(" Session update · 1.2KB · 14 lines · ctrl+o\n"));
		assert!(expanded.contains("\n # Session update\n line 0 of the replay dump"), "{expanded:?}");
	}

	#[test]
	fn guest_bubble_prefixes_author() {
		let text = render(guest_bubble("alice", Str::new_static("can we ship today?")), 40);
		let lines: Vec<&str> = text.lines().collect();
		assert_eq!(lines[0], " «alice» ›");
		assert_eq!(lines[1], "", "tinted padding row above the bubble body");
		assert_eq!(lines[2], " can we ship today?");
		let anonymous = render(guest_bubble("  ", Str::new_static("hi")), 40);
		assert!(anonymous.starts_with(" «guest» ›\n"));
	}

	#[test]
	fn diagnostics_rows_carry_severity_and_location() {
		let node = notice(
			"diagnostics",
			&[(PropId::Name, "rust-analyzer")],
			Some(
				"src/lib.rs:12:5 [error] [rustc] mismatched types (E0308)\nsrc/lib.rs:40:1 [warning] unused import\nserver restarted",
			),
		);
		let text = render(diagnostics_card(&node), 80);
		let lines: Vec<&str> = text.lines().collect();
		assert_eq!(lines[0], " 💡 ✘ Late diagnostics (rust-analyzer)");
		assert_eq!(lines[1], "  ├─ ✘ src/lib.rs:12:5 mismatched types (E0308)");
		assert_eq!(lines[2], "  ├─ ⚠ src/lib.rs:40:1 unused import");
		assert_eq!(lines[3], "  └─ server restarted");
	}
}
