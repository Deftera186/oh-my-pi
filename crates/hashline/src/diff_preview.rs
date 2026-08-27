//! Compact current-coordinate previews for numbered exact-edit diffs.

use std::{
	collections::{BTreeMap, BTreeSet, HashSet},
	fmt::Write as _,
	path::Path,
	str::{self, Utf8Error},
};

use omp_ast::block::{EnclosingBoundaryOptions, LineRange, enclosing_block_boundaries};
use omp_core::{Str, StrMut};
use similar::{Algorithm, DiffOp, capture_diff_slices};

use crate::{
	format::split_addressable_file_lines,
	normalize::{normalize_to_lf, strip_bom},
};

const DEFAULT_ADDED_RUN_CONTEXT_LINES: usize = 2;
const ELISION: &str = "…";
/// Maximum physical terminal rows shown by a compact, collapsed edit diff.
pub const COLLAPSED_DIFF_ROWS: u16 = 40;

/// Canonical numbered diff over exact base and current UTF-8 bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NumberedDiff {
	/// Stable ` N|context`, `-N|old`, and `+N|new` rows.
	pub text:          Str,
	/// Number of current rows added.
	pub added_lines:   usize,
	/// Number of base rows removed.
	pub removed_lines: usize,
}

/// Produces a stable line diff with one-based original/current coordinates.
///
/// Unchanged regions retain two rows adjacent to a change and omit their
/// untouched middle. The visible line-number jump conveys that omission without
/// an invented footer or marker.
///
/// Context and deletion row numbers address `base`; insertion row numbers
/// address `current`. UTF-8 BOM and line-ending conventions are normalized
/// before comparison, while row text remains otherwise exact.
///
/// When `path` is present, matching syntactic boundary rows outside the
/// ordinary window are inserted via tree-sitter, with a lexical bracket
/// fallback for unknown or temporarily invalid sources.
pub fn numbered_diff(
	base: &[u8],
	current: &[u8],
	path: Option<&Path>,
) -> Result<NumberedDiff, Utf8Error> {
	let base = normalize_to_lf(strip_bom(str::from_utf8(base)?).text);
	let current = normalize_to_lf(strip_bom(str::from_utf8(current)?).text);
	let base_lines: Vec<&str> = split_addressable_file_lines(&base).collect();
	let current_lines: Vec<&str> = split_addressable_file_lines(&current).collect();
	let mut text = StrMut::with_capacity(base.len().saturating_add(current.len()));
	let mut added_lines = 0;
	let mut removed_lines = 0;
	let mut first = true;

	let operations = capture_diff_slices(Algorithm::Myers, &base_lines, &current_lines);
	for (index, operation) in operations.iter().enumerate() {
		match *operation {
			DiffOp::Equal { old_index, new_index: _, len } => {
				let follows_change =
					index > 0 && !matches!(operations[index - 1], DiffOp::Equal { .. });
				let precedes_change = index + 1 < operations.len()
					&& !matches!(operations[index + 1], DiffOp::Equal { .. });
				let (leading, trailing) = match (follows_change, precedes_change) {
					(true, true) if len > 4 => (2, 2),
					(true, true) => (len, 0),
					(true, false) => (len.min(2), 0),
					(false, true) => (0, len.min(2)),
					(false, false) => (0, 0),
				};
				for offset in 0..leading {
					push_numbered_row(
						&mut text,
						&mut first,
						' ',
						old_index + offset + 1,
						base_lines[old_index + offset],
					);
				}
				for offset in len.saturating_sub(trailing)..len {
					push_numbered_row(
						&mut text,
						&mut first,
						' ',
						old_index + offset + 1,
						base_lines[old_index + offset],
					);
				}
			},
			DiffOp::Delete { old_index, old_len, new_index: _ } => {
				for offset in 0..old_len {
					push_numbered_row(
						&mut text,
						&mut first,
						'-',
						old_index + offset + 1,
						base_lines[old_index + offset],
					);
					removed_lines += 1;
				}
			},
			DiffOp::Insert { old_index: _, new_index, new_len } => {
				for offset in 0..new_len {
					push_numbered_row(
						&mut text,
						&mut first,
						'+',
						new_index + offset + 1,
						current_lines[new_index + offset],
					);
					added_lines += 1;
				}
			},
			DiffOp::Replace { old_index, old_len, new_index, new_len } => {
				for offset in 0..old_len {
					push_numbered_row(
						&mut text,
						&mut first,
						'-',
						old_index + offset + 1,
						base_lines[old_index + offset],
					);
					removed_lines += 1;
				}
				for offset in 0..new_len {
					push_numbered_row(
						&mut text,
						&mut first,
						'+',
						new_index + offset + 1,
						current_lines[new_index + offset],
					);
					added_lines += 1;
				}
			},
		}
	}
	let text = add_structural_context(text.freeze(), &base, &current, path);
	Ok(NumberedDiff { text, added_lines, removed_lines })
}
/// Append-only view of successive Myers diffs for one pinned document revision.
///
/// Streaming edit arguments are append-only, but a fresh Myers alignment can
/// move an already visible row when a later complete operation supplies more
/// context. This window retains every row it has exposed and appends only rows
/// first observed in a later alignment. Callers must advance it only after the
/// parser reports another complete operation; an incomplete trailing operation
/// therefore cannot appear as a committed change.
#[derive(Clone, Default)]
pub struct PinnedMyersWindow {
	revision:      Option<Str>,
	text:          StrMut,
	rows:          HashSet<Str>,
	added_lines:   usize,
	removed_lines: usize,
}

impl PinnedMyersWindow {
	/// Creates an empty streaming window.
	pub fn new() -> Self {
		Self::default()
	}

	/// Advances the window from the latest complete candidate.
	///
	/// A different `revision` starts a new window because row coordinates from
	/// the prior base are no longer meaningful. For one revision, the returned
	/// text is prefix-monotonic: bytes previously returned by
	/// [`text`](Self::text) are never changed or removed.
	///
	/// # Errors
	/// Returns the UTF-8 fault from [`numbered_diff`].
	pub fn advance(
		&mut self,
		revision: &str,
		base: &[u8],
		current: &[u8],
		path: Option<&Path>,
	) -> Result<bool, Utf8Error> {
		let diff = numbered_diff(base, current, path)?;
		if self.revision.as_deref() != Some(revision) {
			self.reset(Str::new(revision));
		}

		self.added_lines = diff.added_lines;
		self.removed_lines = diff.removed_lines;
		let has_novel_change = diff
			.text
			.lines()
			.any(|row| is_change_row(row) && !self.rows.contains(row));
		if !has_novel_change {
			return Ok(false);
		}

		let initial_len = self.text.len();
		for row in diff.text.lines() {
			if self.rows.contains(row) {
				continue;
			}
			if !self.text.is_empty() {
				self.text.push('\n');
			}
			self.text.push_str(row);
			self.rows.insert(Str::new(row));
		}
		Ok(self.text.len() != initial_len)
	}

	/// Exact append-only numbered rows currently retained by the window.
	pub fn text(&self) -> &str {
		self.text.as_str()
	}

	/// Number of distinct rows retained across the streamed alignments.
	pub fn row_count(&self) -> usize {
		self.rows.len()
	}

	/// Exact number of added rows in the latest complete candidate.
	pub const fn added_lines(&self) -> usize {
		self.added_lines
	}

	/// Exact number of removed rows in the latest complete candidate.
	pub const fn removed_lines(&self) -> usize {
		self.removed_lines
	}

	fn reset(&mut self, revision: Str) {
		self.revision = Some(revision);
		self.text = StrMut::default();
		self.rows.clear();
		self.added_lines = 0;
		self.removed_lines = 0;
	}
}

fn add_structural_context(text: Str, base: &str, current: &str, path: Option<&Path>) -> Str {
	let Some(path) = path else { return text };
	let mut rows = if text.is_empty() {
		Vec::new()
	} else {
		text
			.as_str()
			.split('\n')
			.map(str::to_owned)
			.collect::<Vec<_>>()
	};
	if rows.is_empty() {
		return text;
	}
	let mut old_visible = BTreeSet::new();
	let mut new_visible = BTreeSet::new();
	let mut changes = Vec::<(i64, i64)>::new();
	let mut offset = 0i64;
	for row in &rows {
		let Some(parsed) = parse_line(row) else {
			continue;
		};
		match parsed.kind {
			b'-' => {
				old_visible.insert(parsed.number);
				changes.push((parsed.number.saturating_add(offset), -1));
				offset = offset.saturating_sub(1);
			},
			b'+' => {
				new_visible.insert(parsed.number);
				changes.push((parsed.number, 1));
				offset = offset.saturating_add(1);
			},
			_ => {
				old_visible.insert(parsed.number);
				new_visible.insert(parsed.number.saturating_add(offset));
			},
		}
	}

	let path = path.to_string_lossy().into_owned();
	let mut context = structural_boundaries(base, &old_visible, &path);
	for (new_line, value) in structural_boundaries(current, &new_visible, &path) {
		let shift = changes
			.iter()
			.filter(|(position, _)| *position <= new_line)
			.fold(0i64, |total, (_, delta)| total.saturating_add(*delta));
		let old_line = new_line.saturating_sub(shift);
		context.entry(old_line).or_insert(value);
	}
	insert_context_rows(&mut rows, context);
	Str::from(rows.join("\n"))
}

fn structural_boundaries(code: &str, visible: &BTreeSet<i64>, path: &str) -> BTreeMap<i64, Str> {
	let ranges = visible
		.iter()
		.filter_map(|line| u32::try_from(*line).ok())
		.map(|line| LineRange { start_line: line, end_line: line })
		.collect();
	let native = enclosing_block_boundaries(EnclosingBoundaryOptions {
		code: code.to_owned(),
		lang: None,
		path: Some(path.to_owned()),
		ranges,
	})
	.ok()
	.flatten();
	let lines = split_addressable_file_lines(code).collect::<Vec<_>>();
	let Some(boundaries) = native else {
		return lexical_boundaries(&lines, visible);
	};
	boundaries
		.into_iter()
		.filter_map(|line| {
			let number = i64::from(line);
			(!visible.contains(&number))
				.then(|| (number, Str::new(lines.get(line as usize - 1).copied().unwrap_or(""))))
		})
		.collect()
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScannerMode {
	Code,
	Single,
	Double,
	Template,
	BlockComment,
}

fn lexical_boundaries(lines: &[&str], visible: &BTreeSet<i64>) -> BTreeMap<i64, Str> {
	let mut context = BTreeMap::new();
	let mut stack = Vec::<(u8, i64, bool)>::new();
	let mut mode = ScannerMode::Code;
	let mut escaped = false;
	for (line_index, line) in lines.iter().enumerate() {
		let line_number = i64::try_from(line_index + 1).unwrap_or(i64::MAX);
		let line_visible = visible.contains(&line_number);
		let bytes = line.as_bytes();
		let mut index = 0;
		while index < bytes.len() {
			let byte = bytes[index];
			let next = bytes.get(index + 1).copied();
			if mode == ScannerMode::BlockComment {
				if byte == b'*' && next == Some(b'/') {
					mode = ScannerMode::Code;
					index += 2;
				} else {
					index += 1;
				}
				continue;
			}
			if matches!(mode, ScannerMode::Single | ScannerMode::Double | ScannerMode::Template) {
				if escaped {
					escaped = false;
					index += 1;
					continue;
				}
				if byte == b'\\' {
					escaped = true;
					index += 1;
					continue;
				}
				let closes = matches!(
					(mode, byte),
					(ScannerMode::Single, b'\'')
						| (ScannerMode::Double, b'"')
						| (ScannerMode::Template, b'`')
				);
				if closes {
					mode = ScannerMode::Code;
				}
				index += 1;
				continue;
			}
			if byte == b'/' && next == Some(b'/') {
				break;
			}
			if byte == b'/' && next == Some(b'*') {
				mode = ScannerMode::BlockComment;
				index += 2;
				continue;
			}
			if byte == b'#' && bytes[..index].iter().all(u8::is_ascii_whitespace) {
				break;
			}
			if matches!(byte, b'\'' | b'"' | b'`') {
				mode = match byte {
					b'\'' => ScannerMode::Single,
					b'"' => ScannerMode::Double,
					_ => ScannerMode::Template,
				};
				escaped = false;
				index += 1;
				continue;
			}
			if matches!(byte, b'(' | b'[' | b'{') {
				stack.push((byte, line_number, line_visible));
				index += 1;
				continue;
			}
			let opener = match byte {
				b')' => Some(b'('),
				b']' => Some(b'['),
				b'}' => Some(b'{'),
				_ => None,
			};
			if let Some(opener) = opener
				&& let Some(match_index) = stack.iter().rposition(|entry| entry.0 == opener)
			{
				let (_, open_line, open_visible) = stack.remove(match_index);
				if line_visible && !open_visible {
					context.insert(open_line, Str::new(lines[open_line as usize - 1]));
				}
				if open_visible && !line_visible {
					context.insert(line_number, Str::new(*line));
				}
			}
			index += 1;
		}
		if matches!(mode, ScannerMode::Single | ScannerMode::Double) {
			mode = ScannerMode::Code;
			escaped = false;
		}
	}
	context
}

fn insert_context_rows(rows: &mut Vec<String>, context: BTreeMap<i64, Str>) {
	let mut seen = rows.iter().cloned().collect::<BTreeSet<_>>();
	for (line_number, value) in context {
		let row = format!(" {line_number}|{value}");
		if seen.contains(&row) {
			continue;
		}
		let mut insert_index = rows.len();
		let mut previous_source = None;
		let mut next_source = None;
		for (index, existing) in rows.iter().enumerate() {
			let Some(parsed) = parse_line(existing) else {
				continue;
			};
			if parsed.kind == b'+' {
				continue;
			}
			if parsed.number < line_number {
				previous_source = Some(parsed.number);
				continue;
			}
			next_source = Some(parsed.number);
			insert_index = index;
			break;
		}
		let mut start = insert_index;
		while start > 0 && is_change_row(&rows[start - 1]) {
			start -= 1;
		}
		let mut end = insert_index;
		while end < rows.len() && is_change_row(&rows[end]) {
			end += 1;
		}
		if insert_index > start && insert_index < end {
			insert_index = end;
		}
		let mut chunk = Vec::with_capacity(3);
		if previous_source.is_some_and(|previous| line_number > previous + 1) {
			chunk.push(String::new());
		}
		chunk.push(row.clone());
		if next_source.is_some_and(|next| next > line_number + 1) {
			chunk.push(String::new());
		}
		rows.splice(insert_index..insert_index, chunk);
		seen.insert(row);
	}
	normalize_gap_rows(rows);
}

fn is_change_row(row: &str) -> bool {
	row.starts_with('+') || row.starts_with('-')
}

fn normalize_gap_rows(rows: &mut Vec<String>) {
	let mut kept = Vec::with_capacity(rows.len());
	for (index, row) in rows.iter().enumerate() {
		if !row.is_empty() {
			kept.push(row.clone());
			continue;
		}
		if kept.is_empty() || kept.last().is_some_and(String::is_empty) {
			continue;
		}
		let before = kept.iter().rev().find_map(|row| {
			parse_line(row).and_then(|parsed| (parsed.kind != b'+').then_some(parsed.number))
		});
		let after = rows[index + 1..].iter().find_map(|row| {
			parse_line(row).and_then(|parsed| (parsed.kind != b'+').then_some(parsed.number))
		});
		if before
			.zip(after)
			.is_some_and(|(before, after)| after > before + 1)
		{
			kept.push(String::new());
		}
	}
	*rows = kept;
}

fn push_numbered_row(output: &mut StrMut, first: &mut bool, kind: char, line: usize, value: &str) {
	if !*first {
		output.push('\n');
	}
	*first = false;
	write!(output, "{kind}{line}|{value}").expect("writing to StrMut cannot fail");
}

/// Rendering controls for a compact numbered diff preview.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactDiffOptions {
	/// Number of lines retained at each edge of a long contiguous added run.
	pub max_added_run_context: usize,
}

impl Default for CompactDiffOptions {
	fn default() -> Self {
		Self { max_added_run_context: DEFAULT_ADDED_RUN_CONTEXT_LINES }
	}
}

/// A compact post-edit-coordinate preview and exact line statistics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactDiffPreview {
	/// Numbered visible current lines with removed rows omitted.
	pub preview:       Str,
	/// Number of added rows in the source diff.
	pub added_lines:   usize,
	/// Number of removed rows in the source diff.
	pub removed_lines: usize,
}

#[derive(Clone, Copy)]
struct ParsedLine<'a> {
	kind:    u8,
	number:  i64,
	content: &'a str,
}

#[derive(Clone, Copy)]
enum PreviewLine<'a> {
	Literal(&'a str),
	Formatted { number: i64, content: &'a str },
}

/// Builds a compact preview from `+N|text`, `-N|text`, and ` N|text` rows.
///
/// Added rows already use post-edit numbers. Context rows are renumbered by the
/// running add/remove offset, removed text is omitted, and long added runs
/// elide their middle without losing statistics.
pub fn build_compact_diff_preview(diff: &str, options: CompactDiffOptions) -> CompactDiffPreview {
	let edge_lines = options.max_added_run_context.max(1);
	let mut formatted = Vec::<PreviewLine<'_>>::new();
	let mut added_run = Vec::<PreviewLine<'_>>::new();
	let mut added_lines = 0usize;
	let mut removed_lines = 0usize;

	if !diff.is_empty() {
		for line in diff.split('\n') {
			let Some(parsed) = parse_line(line) else {
				append_added_run(&mut formatted, &mut added_run, edge_lines);
				append_line(&mut formatted, line);
				continue;
			};
			match parsed.kind {
				b'+' => {
					added_lines += 1;
					push_formatted_line(&mut added_run, parsed.number, parsed.content);
				},
				b'-' => {
					append_added_run(&mut formatted, &mut added_run, edge_lines);
					removed_lines += 1;
				},
				_ => {
					append_added_run(&mut formatted, &mut added_run, edge_lines);
					let offset = i64::try_from(added_lines).unwrap_or(i64::MAX)
						- i64::try_from(removed_lines).unwrap_or(i64::MAX);
					push_formatted_line(
						&mut formatted,
						parsed.number.saturating_add(offset),
						parsed.content,
					);
				},
			}
		}
	}
	append_added_run(&mut formatted, &mut added_run, edge_lines);
	while formatted.last().is_some_and(PreviewLine::is_separator) {
		formatted.pop();
	}
	let mut preview = StrMut::with_capacity(
		diff
			.len()
			.saturating_add(formatted.len().saturating_mul(21)),
	);
	for (index, line) in formatted.into_iter().enumerate() {
		if index > 0 {
			preview.push('\n');
		}
		match line {
			PreviewLine::Literal(line) => preview.push_str(line),
			PreviewLine::Formatted { number, content } => {
				append_formatted_line(&mut preview, number, content);
			},
		}
	}
	CompactDiffPreview { preview: preview.freeze(), added_lines, removed_lines }
}

fn parse_line(line: &str) -> Option<ParsedLine<'_>> {
	let kind = *line.as_bytes().first()?;
	if !matches!(kind, b'+' | b'-' | b' ') {
		return None;
	}
	let body = &line[1..];
	let separator = body.find('|')?;
	let number_text = &body[..separator];
	let digit_end = number_text
		.char_indices()
		.take_while(|(index, ch)| ch.is_ascii_digit() || *index == 0 && matches!(ch, '+' | '-'))
		.map(|(index, ch)| index + ch.len_utf8())
		.last()?;
	let number = number_text[..digit_end].parse().ok()?;
	Some(ParsedLine { kind, number, content: &body[separator + 1..] })
}

fn append_added_run<'a>(
	output: &mut Vec<PreviewLine<'a>>,
	run: &mut Vec<PreviewLine<'a>>,
	edge_lines: usize,
) {
	if run.is_empty() {
		return;
	}
	let threshold = edge_lines.saturating_mul(2).saturating_add(1);
	if run.len() <= threshold {
		output.append(run);
		return;
	}
	let mut tail = run.split_off(run.len() - edge_lines);
	run.truncate(edge_lines);
	output.append(run);
	append_line(output, ELISION);
	output.append(&mut tail);
}

fn append_line<'a>(output: &mut Vec<PreviewLine<'a>>, line: &'a str) {
	let normalized = if matches!(line, "..." | "…" | "+…") {
		ELISION
	} else {
		line
	};
	if is_separator(normalized)
		&& (output.is_empty() || output.last().is_some_and(PreviewLine::is_separator))
	{
		return;
	}
	output.push(PreviewLine::Literal(normalized));
}

fn push_formatted_line<'a>(output: &mut Vec<PreviewLine<'a>>, number: i64, content: &'a str) {
	output.push(PreviewLine::Formatted { number, content });
}

fn append_formatted_line(output: &mut StrMut, number: i64, content: &str) {
	write!(output, "{number}:").expect("writing to StrMut cannot fail");
	output.push_str(content);
}

impl PreviewLine<'_> {
	fn is_separator(&self) -> bool {
		match self {
			Self::Literal(line) => is_separator(line),
			Self::Formatted { .. } => false,
		}
	}
}

fn is_separator(line: &str) -> bool {
	line.is_empty() || line == ELISION
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn renumbers_current_rows_and_omits_removed_content() {
		let preview = build_compact_diff_preview(
			" 1|alpha\n-2|beta\n+2|DELTA\n+3|EPSILON\n 3|gamma",
			CompactDiffOptions::default(),
		);
		assert_eq!(preview.preview, "1:alpha\n2:DELTA\n3:EPSILON\n4:gamma");
		assert_eq!(preview.added_lines, 2);
		assert_eq!(preview.removed_lines, 1);
	}

	#[test]
	fn collapses_long_added_runs_to_edges() {
		let diff = (0..7)
			.map(|index| format!("+{}|line {}", 10 + index, index + 1))
			.collect::<Vec<_>>()
			.join("\n");
		let preview = build_compact_diff_preview(&diff, CompactDiffOptions::default());
		assert_eq!(preview.preview, "10:line 1\n11:line 2\n…\n15:line 6\n16:line 7");
		assert_eq!(preview.added_lines, 7);
	}

	#[test]
	fn normalizes_elision_and_gap_separators() {
		let preview = build_compact_diff_preview(
			"\n 1|alpha\n\n-5|beta\n\n...\n…\n 9|gamma\n\n-12|omitted",
			CompactDiffOptions::default(),
		);
		assert_eq!(preview.preview, "1:alpha\n\n8:gamma");
		assert_eq!(preview.removed_lines, 2);
	}

	#[test]
	fn retains_short_added_runs_in_full() {
		let preview = build_compact_diff_preview("+3|x\n+4|y\n+5|z", CompactDiffOptions {
			max_added_run_context: 1,
		});
		assert_eq!(preview.preview, "3:x\n4:y\n5:z");
	}
	#[test]
	fn pinned_myers_window_is_prefix_monotonic_across_complete_fragments() {
		let base = b"alpha\nsame\nsame\nomega\n";
		let mut window = PinnedMyersWindow::new();
		assert!(
			window
				.advance("rev-1", base, b"alpha\nFIRST\nsame\nomega\n", None)
				.unwrap()
		);
		let first = window.text().to_owned();
		assert!(
			window
				.advance("rev-1", base, b"prefix\nalpha\nFIRST\nsame\nomega\n", None,)
				.unwrap()
		);
		assert!(window.text().starts_with(&first));
		assert!(window.text().len() > first.len());
		assert_eq!((window.added_lines(), window.removed_lines()), (2, 1));
	}

	#[test]
	fn pinned_myers_window_ignores_repeated_and_incomplete_candidates() {
		let base = b"one\ntwo\nthree\n";
		let current = b"one\nTWO\nthree\n";
		let mut window = PinnedMyersWindow::new();
		window.advance("rev-1", base, current, None).unwrap();
		let complete = window.text().to_owned();
		assert!(!window.advance("rev-1", base, current, None).unwrap());
		assert_eq!(window.text(), complete);
		assert!(
			!window
				.advance("rev-1", base, b"one\ntwo\nthree\n", None)
				.unwrap()
		);
		assert_eq!(window.text(), complete);
	}

	#[test]
	fn pinned_myers_window_resets_on_revision_change() {
		let mut window = PinnedMyersWindow::new();
		window.advance("rev-1", b"a\nb\n", b"a\nB\n", None).unwrap();
		assert!(window.text().contains("+2|B"));
		let retained = window.text().to_owned();
		assert!(window.advance("rev-2", b"x\n", b"\xff", None).is_err());
		assert_eq!(window.text(), retained);
		window.advance("rev-2", b"x\ny\n", b"x\nY\n", None).unwrap();
		assert!(!window.text().contains("+2|B"));
		assert!(window.text().contains("+2|Y"));
	}

	#[test]
	fn numbered_replacement_has_true_counts_and_context() {
		let diff = numbered_diff(b"alpha\nold\nomega\n", b"alpha\nnew\nomega\n", None).unwrap();
		assert_eq!(diff.text, " 1|alpha\n-2|old\n+2|new\n 3|omega");
		assert_eq!(diff.added_lines, 1);
		assert_eq!(diff.removed_lines, 1);
	}

	#[test]
	fn numbered_diff_retains_distant_hunks_and_unicode() {
		let diff = numbered_diff(
			"α\nleft\nmiddle\nright\n終\n".as_bytes(),
			"α\nLEFT\nmiddle\nRIGHT\n終\n".as_bytes(),
			None,
		)
		.unwrap();
		assert_eq!(diff.text, " 1|α\n-2|left\n+2|LEFT\n 3|middle\n-4|right\n+4|RIGHT\n 5|終");
		assert_eq!((diff.added_lines, diff.removed_lines), (2, 2));
	}

	#[test]
	fn numbered_diff_elides_untouched_middle_like_pi() {
		let base = (1..=12)
			.map(|line| format!("L{line}"))
			.collect::<Vec<_>>()
			.join("\n")
			+ "\n";
		let mut current = (1..=12).map(|line| format!("L{line}")).collect::<Vec<_>>();
		current[1] = "TWO".into();
		current[10] = "ELEVEN".into();
		let current = current.join("\n") + "\n";
		let diff = numbered_diff(base.as_bytes(), current.as_bytes(), None).unwrap();
		assert_eq!(
			diff.text,
			" 1|L1\n-2|L2\n+2|TWO\n 3|L3\n 4|L4\n 9|L9\n 10|L10\n-11|L11\n+11|ELEVEN\n 12|L12"
		);
		let preview = build_compact_diff_preview(&diff.text, CompactDiffOptions::default());
		assert_eq!(preview.preview, "1:L1\n2:TWO\n3:L3\n4:L4\n9:L9\n10:L10\n11:ELEVEN\n12:L12");
	}

	#[test]
	fn path_aware_diff_adds_elided_matching_block_boundary() {
		let base = b"function outer() {\n  const value = 1;\n  const two = 2;\n  const three = 3;\n  const four = 4;\n  return value;\n}\n";
		let current = b"function renamed() {\n  const value = 1;\n  const two = 2;\n  const three = 3;\n  const four = 4;\n  return value;\n}\n";
		let diff = numbered_diff(base, current, Some(Path::new("sample.ts"))).unwrap();
		assert!(diff.text.contains("-1|function outer() {"));
		assert!(diff.text.contains("+1|function renamed() {"));
		assert!(diff.text.contains("\n\n 7|}"));
		assert!(!diff.text.contains("..."));
		assert!(!diff.text.contains(" 5|  const four = 4;"));
	}

	#[test]
	fn unknown_languages_use_lexical_bracket_context() {
		let base = b"outer {\n  one\n  two\n  three\n  four\n}\n";
		let current = b"renamed {\n  one\n  two\n  three\n  four\n}\n";
		let diff = numbered_diff(base, current, Some(Path::new("sample.unknown"))).unwrap();
		assert!(diff.text.contains("\n\n 6|}"));
	}

	#[test]
	fn new_file_boundaries_are_translated_to_pre_edit_coordinates() {
		let base = b"function outer() {\n  const a = 1;\n  const keep = 2;\n  const b = 3;\n  const c = 4;\n  const d = 5;\n  const e = 6;\n  const f = 7;\n  return a;\n}\n";
		let current = b"function outer() {\n  const a = 10;\n  const a2 = 11;\n  const keep = 2;\n  const b = 30;\n  const b2 = 31;\n  const c = 4;\n  const d = 5;\n  const e = 6;\n  const f = 7;\n  return a;\n}\n";
		let diff = numbered_diff(base, current, Some(Path::new("sample.ts"))).unwrap();
		assert_eq!(
			diff
				.text
				.lines()
				.filter(|line| line.ends_with("|}"))
				.collect::<Vec<_>>(),
			vec![" 10|}"]
		);
		let context = diff
			.text
			.lines()
			.filter_map(|line| line.strip_prefix(' '))
			.filter_map(|line| line.split_once('|'))
			.filter_map(|(line, _)| line.parse::<usize>().ok())
			.collect::<Vec<_>>();
		assert!(context.windows(2).all(|pair| pair[0] <= pair[1]));
	}

	#[test]
	fn structural_boundaries_never_leave_adjacent_or_stranded_gaps() {
		let base = [
			"function alpha() {",
			"  const a1 = 1;",
			"  const a2 = 2;",
			"  const a3 = 3;",
			"  const a4 = 4;",
			"  return a1;",
			"}",
			"// spacer",
			"function beta() {",
			"  const b1 = 1;",
			"  const b2 = 2;",
			"  const b3 = 3;",
			"  const b4 = 4;",
			"  return b1;",
			"}",
		]
		.join("\n");
		let current = base
			.replace("const a1 = 1", "const a1 = 100")
			.replace("return b1;", "return b1 + 1;");
		let diff =
			numbered_diff(base.as_bytes(), current.as_bytes(), Some(Path::new("sample.ts"))).unwrap();
		let rows = diff.text.split("\n").collect::<Vec<_>>();
		let closer = rows.iter().position(|row| *row == " 7|}").unwrap();
		let opener = rows
			.iter()
			.position(|row| *row == " 9|function beta() {")
			.unwrap();
		assert!(opener > closer);
		assert_eq!(rows[closer - 1], "");
		assert_eq!(rows[closer + 1], "");
		assert_eq!(rows[opener - 1], "");
		assert_eq!(rows[opener + 1], "");
		assert!(
			rows
				.windows(2)
				.all(|pair| !(pair[0].is_empty() && pair[1].is_empty()))
		);

		let contiguous = base.replace("// spacer\n", "");
		let contiguous_current = contiguous
			.replace("const a1 = 1", "const a1 = 100")
			.replace("return b1;", "return b1 + 1;");
		let contiguous_diff = numbered_diff(
			contiguous.as_bytes(),
			contiguous_current.as_bytes(),
			Some(Path::new("sample.ts")),
		)
		.unwrap();
		let rows = contiguous_diff.text.split("\n").collect::<Vec<_>>();
		let closer = rows.iter().position(|row| *row == " 7|}").unwrap();
		let opener = rows
			.iter()
			.position(|row| *row == " 8|function beta() {")
			.unwrap();
		assert_eq!(opener, closer + 1);
	}
	#[test]
	fn numbered_diff_uses_final_formatter_altered_bytes() {
		let diff = numbered_diff(b"fn x(){ }\n", b"fn x() {\n\tbody();\n}\n", None).unwrap();
		assert_eq!(diff.text, "-1|fn x(){ }\n+1|fn x() {\n+2|\tbody();\n+3|}");
		assert_eq!((diff.added_lines, diff.removed_lines), (3, 1));
	}
}
