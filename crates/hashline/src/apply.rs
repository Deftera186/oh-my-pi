//! Pure application of parsed hashline edits to exact UTF-8 bytes.

use std::{
	collections::{BTreeMap, BTreeSet},
	mem, str,
};

use bytes::Bytes;
use omp_core::{Str, StrMut};
use similar::{Algorithm, DiffOp, capture_diff_slices};
use smallvec::SmallVec;

use crate::{
	block::{BlockError, BlockLowering, UnresolvedBlockMode, resolve_block_edits},
	clipboard::{Clipboard, ClipboardError, EmptyPasteMode, resolve_clipboard_edits},
	format::split_addressable_file_lines,
	normalize::{
		LineEnding, detect_line_ending, normalize_to_lf, restore_bom, restore_line_endings, strip_bom,
	},
	syntax,
	types::{Anchor, ApplyWarning, BlockResolution, Cursor, Edit, InsertMode, ParsedPatch},
};

/// One exact replacement in coordinates of the exact input bytes.
///
/// Anchor-scoped operations retain their authored source-line identity rather
/// than being re-diffed into an equivalent location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteEdit {
	/// Inclusive byte start in the input.
	pub start:       usize,
	/// Exclusive byte end in the input.
	pub end:         usize,
	/// Exact replacement bytes.
	pub replacement: Bytes,
}

/// Strict final application or streaming-tolerant preview behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
	/// Reject every unresolved or structurally invalid edit.
	Strict,
	/// Drop incomplete block and empty-register edits while retaining valid
	/// work.
	Partial,
}

/// Application options.
#[derive(Debug, Clone, Copy)]
pub struct ApplyOptions<'a> {
	/// Strict or streaming-tolerant behavior.
	pub mode: ApplyMode,
	/// Optional path used to infer the syntax language.
	pub path: Option<&'a str>,
}
impl Default for ApplyOptions<'_> {
	fn default() -> Self {
		Self { mode: ApplyMode::Strict, path: None }
	}
}

/// Fully materialized application output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResult {
	/// Final bytes with the original BOM and line-ending convention restored.
	pub bytes:              Bytes,
	/// Non-overlapping edits in exact input-byte coordinates, retaining authored
	/// source-line identity for anchor-scoped operations.
	pub edits:              Vec<ByteEdit>,
	/// First changed model-facing line, when any.
	pub first_changed_line: Option<usize>,
	/// Non-fatal application diagnostics.
	pub warnings:           Vec<ApplyWarning>,
	/// Syntax-aware block resolutions.
	pub block_resolutions:  Vec<BlockResolution>,
}

type BoundaryRepair = (Vec<Edit>, Vec<ApplyWarning>);

#[derive(Clone, Copy)]
enum BoundarySide {
	Leading,
	Trailing,
}

/// Structural application failure.
#[derive(Debug, thiserror::Error, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ApplyError {
	/// Input was not exact UTF-8.
	#[error("source is not UTF-8: {0}")]
	InvalidUtf8(#[from] str::Utf8Error),
	/// A syntax-aware block could not be lowered.
	#[error(transparent)]
	Block(#[from] BlockError),
	/// A clipboard operation was invalid.
	#[error(transparent)]
	Clipboard(#[from] ClipboardError),
	/// A high-level operation remained after lowering.
	#[error(
		"an unresolved high-level edit reached materialization; resolve block, CUT, and register \
		 paste operations before applying the patch"
	)]
	UnresolvedEdit,
	/// Anchor-aware byte edits failed to reproduce the materialized output.
	#[error(
		"could not preserve authored line identity while materializing exact byte edits; re-read \
		 the file and re-issue the edit against its fresh tag"
	)]
	AuthoredIdentityLost,
	/// An edit refers to a source line outside the addressable file.
	#[error(
		"line {line} does not exist (file has {total} addressable lines); re-read the file and use \
		 an existing line"
	)]
	LineOutOfRange {
		/// The requested one-indexed source line.
		line:  usize,
		/// The number of addressable source lines.
		total: usize,
	},
	/// Two deletes target the same original source line.
	#[error(
		"overlapping delete at line {line}; combine overlapping operations into one replacement \
		 range"
	)]
	OverlappingDelete {
		/// The multiply deleted source line.
		line: usize,
	},
	/// A replacement boundary could be placed in more than one valid location.
	#[error(
		"`PUT {start}.={end}:` rejected: a selected boundary row is required for the file to parse, \
		 but the body indentation does not establish whether it belongs before or after that row. \
		 Re-read the region and re-issue with a range that excludes every unchanged boundary row."
	)]
	AmbiguousBoundaryPlacement {
		/// The first selected source line.
		start: usize,
		/// The last selected source line.
		end:   usize,
	},
	/// A replacement body begins with an incomplete echo above its range.
	#[error(
		"`PUT {start}.={end}:` rejected: the body starts by restating {count} line(s) just above \
		 the range, but is too short to be the full final content of the selected range. Re-issue \
		 with the range covering exactly the lines that change and the body as their complete final \
		 content."
	)]
	LeadingBoundaryEchoTooShort {
		/// The first selected source line.
		start: usize,
		/// The last selected source line.
		end:   usize,
		/// The number of echoed body rows.
		count: usize,
	},
	/// A replacement body ends with an incomplete echo below its range.
	#[error(
		"`PUT {start}.={end}:` rejected: the body ends by restating {count} line(s) just below the \
		 range, but is too short to be the full final content of the selected range. Re-issue with \
		 the range covering exactly the lines that change and the body as their complete final \
		 content."
	)]
	TrailingBoundaryEchoTooShort {
		/// The first selected source line.
		start: usize,
		/// The last selected source line.
		end:   usize,
		/// The number of echoed body rows.
		count: usize,
	},
}

impl ApplyError {
	/// Returns the stable machine-readable diagnostic code.
	pub fn code(&self) -> &'static str {
		self.into()
	}
}

const fn anchor_line(edit: &Edit) -> Option<usize> {
	match edit {
		Edit::Delete { anchor, .. } => Some(anchor.line),
		Edit::Insert {
			cursor: Cursor::BeforeAnchor { anchor } | Cursor::AfterAnchor { anchor },
			..
		} => Some(anchor.line),
		_ => None,
	}
}
/// Returns whether a parsed patch contains one or more content-independent
/// beginning/end-of-file inserts and no anchor-scoped operation.
pub fn is_head_tail_only(patch: &ParsedPatch) -> bool {
	!patch.edits.is_empty()
		&& patch
			.edits
			.iter()
			.all(|edit| matches!(edit, Edit::Insert { cursor: Cursor::Bof | Cursor::Eof, .. }))
}

fn closer(text: &str) -> bool {
	let t = text.trim();
	let t = t
		.strip_suffix(';')
		.or_else(|| t.strip_suffix(','))
		.unwrap_or(t);
	!t.is_empty() && t.bytes().all(|b| matches!(b, b')' | b']' | b'}'))
}
fn indent(text: &str) -> &str {
	&text[..text.len() - text.trim_start_matches([' ', '\t']).len()]
}

fn repair_replacement_indentation(
	edits: &mut [Edit],
	lines: &[Str],
	warnings: &mut Vec<ApplyWarning>,
) {
	let mut at = 0;
	while at < edits.len() {
		let Edit::Insert {
			cursor: Cursor::BeforeAnchor { anchor },
			line_num,
			mode: InsertMode::Replacement,
			..
		} = &edits[at]
		else {
			at += 1;
			continue;
		};
		let anchor_line = anchor.line;
		let op_line = *line_num;
		let insert_start = at;
		while at < edits.len()
			&& matches!(&edits[at], Edit::Insert { cursor: Cursor::BeforeAnchor { anchor }, line_num, mode: InsertMode::Replacement, .. } if anchor.line == anchor_line && *line_num == op_line)
		{
			at += 1;
		}
		let insert_end = at;
		let delete_start = at;
		let mut expected = anchor_line;
		while at < edits.len()
			&& matches!(&edits[at], Edit::Delete { anchor, line_num, .. } if anchor.line == expected && *line_num == op_line)
		{
			expected += 1;
			at += 1;
		}
		let count = at - delete_start;
		if count == 0 || count != insert_end - insert_start || anchor_line < 2 {
			continue;
		}
		let preceding = &lines[anchor_line - 2];
		let source_first = &lines[anchor_line - 1];
		let Edit::Insert { text: payload_first, .. } = &edits[insert_start] else {
			continue;
		};
		if !preceding.trim_end().ends_with('{')
			|| !indent(source_first).starts_with(indent(preceding))
			|| indent(source_first) == indent(preceding)
			|| (indent(payload_first).starts_with(indent(preceding))
				&& indent(payload_first) != indent(preceding))
		{
			continue;
		}
		let mut shift: Option<String> = None;
		let mut matches = 0;
		let mut consistent = true;
		for offset in 0..count {
			let source = &lines[anchor_line - 1 + offset];
			let Edit::Insert { text: payload, .. } = &edits[insert_start + offset] else {
				continue;
			};
			if source.trim().is_empty() || source.trim_start() != payload.trim_start() {
				continue;
			}
			let si = indent(source);
			let pi = indent(payload);
			if !si.ends_with(pi) {
				consistent = false;
				break;
			}
			let candidate = &si[..si.len() - pi.len()];
			match &shift {
				None => shift = Some(candidate.to_owned()),
				Some(old) if old == candidate => {},
				_ => {
					consistent = false;
					break;
				},
			}
			matches += 1;
		}
		let Some(shift) =
			shift.filter(|s| !s.is_empty() && consistent && matches >= 2 && matches * 2 > count)
		else {
			continue;
		};
		for edit in &mut edits[insert_start..insert_end] {
			if let Edit::Insert { text, .. } = edit
				&& !text.trim().is_empty()
			{
				let mut shifted = StrMut::from(mem::take(text));
				shifted.insert(0, &shift);
				*text = shifted.freeze();
			}
		}
		let bodies = edits[insert_start..insert_end]
			.iter()
			.filter(|edit| matches!(edit, Edit::Insert { text, .. } if !text.trim().is_empty()))
			.count();
		warnings.push(ApplyWarning::ReplacementIndentationRepaired { line: op_line, bodies });
	}
}

fn insertion_body(edits: &[Edit], members: &[usize]) -> String {
	let capacity = members
		.iter()
		.filter_map(|index| match &edits[*index] {
			Edit::Insert { text, .. } => Some(text.len()),
			_ => None,
		})
		.sum::<usize>()
		.saturating_add(members.len().saturating_sub(1));
	let mut body = String::with_capacity(capacity);
	for (offset, index) in members.iter().enumerate() {
		if offset > 0 {
			body.push('\n');
		}
		if let Edit::Insert { text, .. } = &edits[*index] {
			body.push_str(text);
		}
	}
	body
}

fn body_is_relocatable_construct(edits: &[Edit], members: &[usize], path: &str) -> bool {
	let Some(last) = members.iter().rposition(
		|index| matches!(&edits[*index], Edit::Insert { text, .. } if has_non_whitespace(text)),
	) else {
		return false;
	};
	let body = insertion_body(edits, members);
	let mut line = 1;
	while line <= last + 1 {
		let Some(index) = members.get(line - 1) else {
			return false;
		};
		if matches!(&edits[*index], Edit::Insert { text, .. } if !has_non_whitespace(text)) {
			line += 1;
			continue;
		}
		let mut end = 0;
		let spans = syntax::node_chain(&body, path, line);
		for span in &*spans {
			if span.start_line as usize == line {
				end = end.max(span.end_line as usize);
			}
		}
		if end == 0 {
			return false;
		}
		if end > last {
			return end > line;
		}
		line = end + 1;
	}
	false
}

fn repair_landings(
	edits: &mut [Edit],
	lines: &[Str],
	source: &str,
	path: Option<&str>,
	warnings: &mut Vec<ApplyWarning>,
) {
	let mut groups: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
	let targeted: BTreeSet<usize> = edits.iter().filter_map(anchor_line).collect();
	for (index, edit) in edits.iter().enumerate() {
		if let Edit::Insert {
			cursor: Cursor::AfterAnchor { anchor },
			line_num,
			mode: InsertMode::Literal,
			..
		} = edit
		{
			groups
				.entry((anchor.line, *line_num))
				.or_default()
				.push(index);
		}
	}
	for ((anchor, _), members) in groups {
		let mut target: Option<&str> = None;
		let mut comparable = true;
		for &index in &members {
			let Edit::Insert { text, .. } = &edits[index] else {
				continue;
			};
			if text.trim().is_empty() || closer(text) {
				continue;
			}
			let current = indent(text);
			target = match target {
				None => Some(current),
				Some(old) if current.starts_with(old) => Some(old),
				Some(old) if old.starts_with(current) => Some(current),
				_ => {
					comparable = false;
					None
				},
			};
			if !comparable {
				break;
			}
		}
		let Some(target) = target.filter(|_| comparable) else {
			continue;
		};
		let Some(anchor_text) = lines.get(anchor - 1) else {
			continue;
		};

		if indent(anchor_text).starts_with(target) && indent(anchor_text) != target {
			let mut landing = anchor;
			let mut crossed = 0;
			for line in anchor + 1..=lines.len() {
				let text = &lines[line - 1];
				if text.trim().is_empty() {
					continue;
				}
				if !closer(text) || !indent(text).starts_with(target) || targeted.contains(&line) {
					break;
				}
				landing = line;
				crossed += 1;
				if indent(text) == target {
					break;
				}
			}
			if landing != anchor {
				for &index in &members {
					if let Edit::Insert { cursor, .. } = &mut edits[index] {
						*cursor = Cursor::AfterAnchor { anchor: Anchor { line: landing } };
					}
				}
				warnings.push(ApplyWarning::AfterLineLandingShifted {
					from: anchor,
					to: landing,
					crossed,
				});
				continue;
			}
		}

		let Some(path) = path else {
			continue;
		};
		let target_columns = indent_columns(target);
		if target_columns >= indent_columns(anchor_text) {
			continue;
		}
		let chain = syntax::node_chain(source, path, anchor);
		if !chain
			.iter()
			.any(|node| node.start_line as usize == anchor && node.end_line as usize > anchor)
			|| !body_is_relocatable_construct(edits, &members, path)
		{
			continue;
		}
		let mut candidates = BTreeSet::new();
		for node in &*chain {
			let start = node.start_line as usize;
			let end = node.end_line as usize;
			if end > anchor
				&& lines
					.get(start.saturating_sub(1))
					.is_some_and(|line| indent_columns(line) <= target_columns)
			{
				candidates.insert(end);
			}
		}
		for landing in candidates {
			if targeted
				.range(anchor.saturating_add(1)..=landing)
				.next()
				.is_some()
			{
				break;
			}
			let mut trial = edits.to_vec();
			for &index in &members {
				if let Edit::Insert { cursor, .. } = &mut trial[index] {
					*cursor = Cursor::AfterAnchor { anchor: Anchor { line: landing } };
				}
			}
			let trial_text = materialize_for_probe(lines, &trial);
			if syntax::parses_cleanly(Some(path), &trial_text) {
				edits.clone_from_slice(&trial);
				let crossed = lines[anchor..landing]
					.iter()
					.filter(|line| closer(line))
					.count();
				warnings.push(ApplyWarning::AfterLineLandingShifted {
					from: anchor,
					to: landing,
					crossed,
				});
				break;
			}
		}
	}
}

fn validate(edits: &[Edit], lines: &[Str]) -> Result<(), ApplyError> {
	let mut deleted = BTreeSet::new();
	for edit in edits {
		match edit {
			Edit::Block { .. } | Edit::Cut { .. } | Edit::Paste { .. } => {
				return Err(ApplyError::UnresolvedEdit);
			},
			Edit::Delete { anchor, .. } => {
				if anchor.line < 1 || anchor.line > lines.len() {
					return Err(ApplyError::LineOutOfRange { line: anchor.line, total: lines.len() });
				}
				if !deleted.insert(anchor.line) {
					return Err(ApplyError::OverlappingDelete { line: anchor.line });
				}
			},
			Edit::Insert {
				cursor: Cursor::BeforeAnchor { anchor } | Cursor::AfterAnchor { anchor },
				..
			} if anchor.line < 1 || anchor.line > lines.len() => {
				return Err(ApplyError::LineOutOfRange { line: anchor.line, total: lines.len() });
			},
			Edit::Insert { .. } => {},
		}
	}
	Ok(())
}

// Replacement-boundary repair first removes exact outside-row echoes whose
// coverage is unambiguous. If the authored result still fails to parse, a
// bounded whole-patch search retains syntax-essential selected edges and/or
// removes exact echoes. Cost ordering is fewer touched groups, then fewer
// retained rows, then fewer dropped echoes; distinct texts tied at minimum
// cost are never guessed.

#[derive(Clone)]
struct ReplacementGroup {
	insert_indices: SmallVec<usize, 8>,
	delete_indices: SmallVec<usize, 8>,
	payload:        SmallVec<Str, 8>,
	start_line:     usize,
	end_line:       usize,
}

fn find_replacement_group(edits: &[Edit], start: usize) -> Option<ReplacementGroup> {
	let Edit::Insert {
		cursor: Cursor::BeforeAnchor { anchor },
		line_num,
		mode: InsertMode::Replacement,
		..
	} = edits.get(start)?
	else {
		return None;
	};
	let anchor_line = anchor.line;
	let op_line = *line_num;
	let mut insert_indices = SmallVec::new();
	let mut payload = SmallVec::new();
	let mut index = start;
	while let Some(Edit::Insert {
		cursor: Cursor::BeforeAnchor { anchor },
		text,
		line_num,
		mode: InsertMode::Replacement,
		..
	}) = edits.get(index)
	{
		if anchor.line != anchor_line || *line_num != op_line {
			break;
		}
		insert_indices.push(index);
		payload.push(text.clone());
		index += 1;
	}
	let mut delete_indices = SmallVec::new();
	let mut expected = anchor_line;
	while let Some(Edit::Delete { anchor, line_num, .. }) = edits.get(index) {
		if anchor.line != expected || *line_num != op_line {
			break;
		}
		delete_indices.push(index);
		expected += 1;
		index += 1;
	}
	if delete_indices.is_empty() {
		return None;
	}
	Some(ReplacementGroup {
		insert_indices,
		delete_indices,
		payload,
		start_line: anchor_line,
		end_line: expected - 1,
	})
}

fn has_non_whitespace(text: &str) -> bool {
	text
		.bytes()
		.any(|byte| !matches!(byte, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' '))
}

fn count_duplicate_leading(group: &ReplacementGroup, lines: &[Str]) -> usize {
	let max = group.payload.len().min(group.start_line - 1);
	for count in (1..=max).rev() {
		let outside = &lines[group.start_line - 1 - count..group.start_line - 1];
		let payload = &group.payload[..count];
		if payload
			.iter()
			.zip(outside)
			.all(|(left, right)| left == right)
			&& payload.iter().any(|line| has_non_whitespace(line))
		{
			return count;
		}
	}
	0
}

fn count_duplicate_trailing(group: &ReplacementGroup, lines: &[Str]) -> usize {
	let max = group
		.payload
		.len()
		.min(lines.len().saturating_sub(group.end_line));
	for count in (1..=max).rev() {
		let outside = &lines[group.end_line..group.end_line + count];
		let payload = &group.payload[group.payload.len() - count..];
		if payload
			.iter()
			.zip(outside)
			.all(|(left, right)| left == right)
			&& payload.iter().any(|line| has_non_whitespace(line))
		{
			return count;
		}
	}
	0
}

const ANNOTATION_NODE_KINDS: &[&str] = &[
	"attribute_item",
	"inner_attribute_item",
	"decorator",
	"annotation",
	"marker_annotation",
	"attribute_list",
];

fn is_annotation_line(source: &str, path: &str, line: usize) -> bool {
	syntax::node_chain(source, path, line).iter().any(|node| {
		node.start_line as usize == line
			&& node.end_line as usize == line
			&& ANNOTATION_NODE_KINDS.contains(&node.kind.as_str())
	})
}

fn is_annotation_echo_run(
	source: &str,
	path: Option<&str>,
	first_line: usize,
	last_line: usize,
) -> bool {
	let Some(path) = path else { return false };
	(first_line..=last_line).all(|line| is_annotation_line(source, path, line))
}

#[derive(Clone)]
struct BoundaryAmbiguity {
	start_line: usize,
	end_line:   usize,
	side:       BoundarySide,
	count:      usize,
}

struct BoundaryNormalization {
	edits:       Vec<Edit>,
	warnings:    Vec<ApplyWarning>,
	ambiguities: Vec<BoundaryAmbiguity>,
}

const fn textual_boundary_warning(
	start_line: usize,
	leading: usize,
	trailing: usize,
) -> ApplyWarning {
	ApplyWarning::BoundaryEchoDropped { line: start_line, leading, trailing }
}

fn normalize_textual_boundary_echoes(
	edits: &[Edit],
	lines: &[Str],
	source: &str,
	path: Option<&str>,
) -> BoundaryNormalization {
	let mut out = Vec::with_capacity(edits.len());
	let mut warnings = Vec::new();
	let mut ambiguities = Vec::new();
	let mut index = 0;
	while index < edits.len() {
		let Some(group) = find_replacement_group(edits, index) else {
			out.push(edits[index].clone());
			index += 1;
			continue;
		};
		let leading = count_duplicate_leading(&group, lines);
		let trailing = count_duplicate_trailing(&group, lines);
		let range_len = group.delete_indices.len();
		let mut drop_leading = 0;
		let mut drop_trailing = 0;
		if leading > 0 && trailing > 0 {
			if group.payload.len().saturating_sub(leading + trailing) == range_len {
				drop_leading = leading;
				drop_trailing = trailing;
			}
		} else if leading > 0
			&& (range_len > 1
				|| is_annotation_echo_run(
					source,
					path,
					group.start_line - leading,
					group.start_line - 1,
				)) {
			if group.payload.len() - leading >= range_len {
				drop_leading = leading;
			} else {
				ambiguities.push(BoundaryAmbiguity {
					start_line: group.start_line,
					end_line:   group.end_line,
					side:       BoundarySide::Leading,
					count:      leading,
				});
			}
		} else if trailing > 0
			&& (range_len > 1
				|| is_annotation_echo_run(source, path, group.end_line + 1, group.end_line + trailing))
		{
			if group.payload.len() - trailing >= range_len {
				drop_trailing = trailing;
			} else {
				ambiguities.push(BoundaryAmbiguity {
					start_line: group.start_line,
					end_line:   group.end_line,
					side:       BoundarySide::Trailing,
					count:      trailing,
				});
			}
		}
		let insert_end = group.insert_indices.len() - drop_trailing;
		for &edit_index in &group.insert_indices[drop_leading..insert_end] {
			out.push(edits[edit_index].clone());
		}
		for &edit_index in &group.delete_indices {
			out.push(edits[edit_index].clone());
		}
		if drop_leading > 0 || drop_trailing > 0 {
			warnings.push(textual_boundary_warning(group.start_line, drop_leading, drop_trailing));
		}
		index = group.delete_indices.last().copied().unwrap_or(index) + 1;
	}
	BoundaryNormalization { edits: out, warnings, ambiguities }
}

const INDENT_TAB_WIDTH: usize = 4;

fn indent_columns(line: &str) -> usize {
	let mut column = 0;
	for byte in line.bytes() {
		match byte {
			b' ' => column += 1,
			b'\t' => column += INDENT_TAB_WIDTH - column % INDENT_TAB_WIDTH,
			_ => break,
		}
	}
	column
}

fn nearest_content_line(lines: &[Str], start: isize, step: isize) -> Option<&str> {
	let mut index = start;
	while index >= 0 && (index as usize) < lines.len() {
		let line = &lines[index as usize];
		if has_non_whitespace(line) {
			return Some(line);
		}
		index += step;
	}
	None
}

fn payload_edge(payload: &[Str], leading: bool) -> Option<&str> {
	if leading {
		payload
			.iter()
			.find(|line| has_non_whitespace(line))
			.map(|line| line.as_str())
	} else {
		payload
			.iter()
			.rev()
			.find(|line| has_non_whitespace(line))
			.map(|line| line.as_str())
	}
}

fn is_source_line_deleted(edits: &[Edit], line: usize) -> bool {
	edits
		.iter()
		.any(|edit| matches!(edit, Edit::Delete { anchor, .. } if anchor.line == line))
}

fn effective_trailing_boundary(group: &ReplacementGroup, edits: &[Edit], lines: &[Str]) -> usize {
	let mut line = group.end_line;
	let mut survivor = group.end_line + 1;
	while line > group.start_line
		&& survivor <= lines.len()
		&& !is_source_line_deleted(edits, survivor)
		&& lines[line - 1] == lines[survivor - 1]
	{
		line -= 1;
		survivor += 1;
	}
	line
}

fn syntax_essential_row(lines: &[Str], path: &str, line: usize, baseline_parses: bool) -> bool {
	if !baseline_parses {
		return true;
	}
	let capacity = lines.iter().map(Str::len).sum::<usize>() + lines.len().saturating_sub(2);
	let mut without = String::with_capacity(capacity);
	let mut first = true;
	for (index, text) in lines.iter().enumerate() {
		if index + 1 == line {
			continue;
		}
		if !first {
			without.push('\n');
		}
		without.push_str(text);
		first = false;
	}
	!syntax::parses_cleanly(Some(path), &without)
}

#[derive(Clone, Copy)]
struct EdgeEvidence {
	first:             bool,
	last:              bool,
	leading_structure: bool,
}

fn contains_boundary(
	source: &str,
	path: &str,
	start_line: usize,
	end_line: usize,
	boundary: usize,
) -> bool {
	if start_line > end_line {
		return false;
	}
	let Ok(boundary) = u32::try_from(boundary) else {
		return false;
	};
	syntax::is_enclosing_boundary(source, path, start_line, end_line, boundary)
}

fn edge_evidence(
	lines: &[Str],
	source: &str,
	path: &str,
	group: &ReplacementGroup,
	trailing_line: usize,
	baseline_parses: bool,
) -> EdgeEvidence {
	if !baseline_parses {
		return EdgeEvidence {
			first:             true,
			last:              true,
			leading_structure: false,
		};
	}
	let first = syntax_essential_row(lines, path, group.start_line, true);
	let last = if trailing_line == group.start_line {
		first
	} else {
		syntax_essential_row(lines, path, trailing_line, true)
	};
	EdgeEvidence {
		first,
		last,
		leading_structure: contains_boundary(
			source,
			path,
			group.start_line + 1,
			trailing_line,
			group.start_line,
		),
	}
}

#[derive(Clone, Copy)]
struct KeepPlan {
	before_line: Option<usize>,
	after_line:  Option<usize>,
	kept:        usize,
}

struct KeepPlans {
	plans:     SmallVec<KeepPlan, 3>,
	ambiguous: bool,
}

fn build_keep_plans(
	group: &ReplacementGroup,
	trailing_line: usize,
	payload: &[Str],
	lines: &[Str],
	source: &str,
	path: &str,
	evidence: EdgeEvidence,
	baseline_parses: bool,
) -> KeepPlans {
	let mut plans = SmallVec::new();
	plans.push(KeepPlan { before_line: None, after_line: None, kept: 0 });
	let (Some(leading_payload), Some(trailing_payload)) =
		(payload_edge(payload, true), payload_edge(payload, false))
	else {
		return KeepPlans { plans, ambiguous: false };
	};
	let first = lines
		.get(group.start_line - 1)
		.map_or("", |line| line.as_str());
	let last = lines
		.get(trailing_line - 1)
		.map_or("", |line| line.as_str());
	let leading_indent = indent_columns(leading_payload);
	let trailing_indent = indent_columns(trailing_payload);
	let first_indent = indent_columns(first);
	let last_indent = indent_columns(last);
	let mut ambiguous = false;

	if group.start_line == trailing_line {
		let previous = nearest_content_line(lines, group.start_line as isize - 2, -1);
		let fits_before = previous.is_none_or(|line| indent_columns(line) == trailing_indent);
		if evidence.first && fits_before && trailing_indent > first_indent {
			plans.push(KeepPlan {
				before_line: None,
				after_line:  Some(group.start_line),
				kept:        1,
			});
		} else if baseline_parses && evidence.first && trailing_indent == first_indent {
			ambiguous = true;
		}
		return KeepPlans { plans, ambiguous };
	}

	let next = nearest_content_line(lines, group.start_line as isize, 1);
	let previous = nearest_content_line(lines, trailing_line as isize - 2, -1);
	let before_first = nearest_content_line(lines, group.start_line as isize - 2, -1);
	let selected_leading_boundary =
		contains_boundary(source, path, group.start_line + 1, group.end_line, group.start_line);
	let first_text = first.trim();
	let selected_structural_edge = closer(first_text)
		&& first_indent == leading_indent
		&& first_indent
			== lines
				.get(group.end_line - 1)
				.map_or(0, |line| indent_columns(line));
	let underfilled_effective_edge =
		trailing_line < group.end_line && payload.len() < group.end_line - group.start_line + 1;
	let keeps_leading = evidence.first
		&& (evidence.leading_structure
			|| selected_leading_boundary
			|| selected_structural_edge
			|| underfilled_effective_edge)
		&& if next.is_none() || selected_structural_edge {
			leading_indent >= first_indent
		} else {
			next.is_some_and(|line| indent_columns(line) == leading_indent)
		};
	let keeps_trailing = (evidence.last || underfilled_effective_edge)
		&& !keeps_leading
		&& trailing_indent > last_indent
		&& previous.is_none_or(|line| indent_columns(line) == trailing_indent);
	if keeps_leading {
		plans.push(KeepPlan {
			before_line: Some(group.start_line),
			after_line:  None,
			kept:        1,
		});
	}
	if keeps_trailing {
		plans.push(KeepPlan { before_line: None, after_line: Some(trailing_line), kept: 1 });
	}
	if baseline_parses
		&& evidence.first
		&& before_first.is_some_and(|line| first_indent < indent_columns(line))
		&& leading_indent > first_indent
	{
		ambiguous = true;
	}
	KeepPlans { plans, ambiguous }
}

#[derive(Clone)]
struct GroupVariant {
	edits:   Vec<Edit>,
	kept:    usize,
	dropped: usize,
}

struct GroupVariants {
	variants:  Vec<GroupVariant>,
	ambiguous: bool,
}

fn apply_group_variant(
	inserts: &[Edit],
	deletes: &[Edit],
	keep: KeepPlan,
	drop_leading: usize,
	drop_trailing: usize,
	file_line_count: usize,
) -> Vec<Edit> {
	let mut retained_inserts = inserts[drop_leading..inserts.len() - drop_trailing].to_vec();
	let retained_deletes = deletes.iter().filter(|edit| {
		!matches!(edit, Edit::Delete { anchor, .. } if Some(anchor.line) == keep.before_line || Some(anchor.line) == keep.after_line)
	});
	if let Some(before_line) = keep.before_line {
		let cursor = if before_line >= file_line_count {
			Cursor::Eof
		} else {
			Cursor::BeforeAnchor { anchor: Anchor { line: before_line + 1 } }
		};
		for edit in &mut retained_inserts {
			if let Edit::Insert { cursor: old, .. } = edit {
				*old = cursor;
			}
		}
	}
	retained_inserts.extend(retained_deletes.cloned());
	retained_inserts
}

fn build_group_variants(
	group: &ReplacementGroup,
	edits: &[Edit],
	lines: &[Str],
	source: &str,
	path: &str,
	baseline_parses: bool,
) -> GroupVariants {
	let inserts: Vec<Edit> = group
		.insert_indices
		.iter()
		.map(|&index| edits[index].clone())
		.collect();
	let deletes: Vec<Edit> = group
		.delete_indices
		.iter()
		.map(|&index| edits[index].clone())
		.collect();
	let trailing_line = effective_trailing_boundary(group, edits, lines);
	let evidence = edge_evidence(lines, source, path, group, trailing_line, baseline_parses);
	let leading_drop = count_duplicate_leading(group, lines);
	let trailing_drop = count_duplicate_trailing(group, lines);
	let mut leading_drops: SmallVec<usize, 2> = SmallVec::new();
	leading_drops.push(0);
	if leading_drop > 0 {
		leading_drops.push(leading_drop);
	}
	let mut trailing_drops: SmallVec<usize, 2> = SmallVec::new();
	trailing_drops.push(0);
	if trailing_drop > 0 {
		trailing_drops.push(trailing_drop);
	}
	let mut variants = Vec::new();
	let mut ambiguous = false;
	for &drop_leading in &leading_drops {
		for &drop_trailing in &trailing_drops {
			let dropped = drop_leading + drop_trailing;
			if dropped >= inserts.len() {
				continue;
			}
			let payload = &group.payload[drop_leading..group.payload.len() - drop_trailing];
			let keep_plans = build_keep_plans(
				group,
				trailing_line,
				payload,
				lines,
				source,
				path,
				evidence,
				baseline_parses,
			);
			ambiguous |= keep_plans.ambiguous;
			for keep in keep_plans.plans {
				if keep.kept == 0 && dropped == 0 {
					continue;
				}
				if keep.kept > 0
					&& group.delete_indices.len() > 1
					&& payload.len() > group.delete_indices.len()
				{
					continue;
				}
				variants.push(GroupVariant {
					edits: apply_group_variant(
						&inserts,
						&deletes,
						keep,
						drop_leading,
						drop_trailing,
						lines.len(),
					),
					kept: keep.kept,
					dropped,
				});
			}
		}
	}
	variants.sort_by_key(|variant| (variant.kept, variant.dropped));
	GroupVariants { variants, ambiguous }
}

#[derive(Clone)]
struct BoundaryCombo {
	variants: Vec<Option<usize>>,
	touched:  usize,
	kept:     usize,
	dropped:  usize,
}

impl BoundaryCombo {
	const fn cost(&self) -> (usize, usize, usize) {
		(self.touched, self.kept, self.dropped)
	}
}

struct VariantGroup {
	group:    ReplacementGroup,
	variants: Vec<GroupVariant>,
}

const MAX_BOUNDARY_COMBOS: usize = 512;

fn splice_boundary_combo(
	edits: &[Edit],
	groups: &[VariantGroup],
	combo: &BoundaryCombo,
) -> Vec<Edit> {
	let mut chosen = BTreeMap::new();
	for (index, entry) in groups.iter().enumerate() {
		if let Some(variant) = combo.variants[index] {
			chosen.insert(entry.group.insert_indices[0], &entry.variants[variant]);
		}
	}
	let mut out = Vec::with_capacity(edits.len());
	let mut index = 0;
	while index < edits.len() {
		let Some(group) = find_replacement_group(edits, index) else {
			out.push(edits[index].clone());
			index += 1;
			continue;
		};
		if let Some(variant) = chosen.get(&group.insert_indices[0]) {
			out.extend_from_slice(&variant.edits);
		} else {
			for &edit_index in &group.insert_indices {
				out.push(edits[edit_index].clone());
			}
			for &edit_index in &group.delete_indices {
				out.push(edits[edit_index].clone());
			}
		}
		index = group.delete_indices.last().copied().unwrap_or(index) + 1;
	}
	out
}

fn materialize_for_probe(lines: &[Str], edits: &[Edit]) -> String {
	materialize(lines, edits).0
}

const fn ambiguous_placement(group: &ReplacementGroup) -> ApplyError {
	ApplyError::AmbiguousBoundaryPlacement { start: group.start_line, end: group.end_line }
}

fn repair_boundary_variants(
	edits: &[Edit],
	lines: &[Str],
	source: &str,
	path: Option<&str>,
	baseline_parses: bool,
) -> Result<Option<BoundaryRepair>, ApplyError> {
	let Some(path) = path else { return Ok(None) };
	let mut groups = Vec::new();
	let mut ambiguous_group = None;
	let mut index = 0;
	while index < edits.len() {
		let Some(group) = find_replacement_group(edits, index) else {
			index += 1;
			continue;
		};
		let built = build_group_variants(&group, edits, lines, source, path, baseline_parses);
		if built.ambiguous && ambiguous_group.is_none() {
			ambiguous_group = Some(group.clone());
		}
		if !built.variants.is_empty() {
			groups.push(VariantGroup { group: group.clone(), variants: built.variants });
		}
		index = group.delete_indices.last().copied().unwrap_or(index) + 1;
	}
	if groups.is_empty() {
		return ambiguous_group.map_or(Ok(None), |group| Err(ambiguous_placement(&group)));
	}

	let mut combos =
		vec![BoundaryCombo { variants: Vec::new(), touched: 0, kept: 0, dropped: 0 }];
	for group in &groups {
		let mut next = Vec::with_capacity(
			combos
				.len()
				.saturating_mul(group.variants.len().saturating_add(1)),
		);
		for combo in &combos {
			let mut authored = combo.clone();
			authored.variants.push(None);
			next.push(authored);
			for (variant_index, variant) in group.variants.iter().enumerate() {
				let mut candidate = combo.clone();
				candidate.variants.push(Some(variant_index));
				candidate.touched += 1;
				candidate.kept += variant.kept;
				candidate.dropped += variant.dropped;
				next.push(candidate);
			}
		}
		next.sort_by_key(BoundaryCombo::cost);
		next.truncate(MAX_BOUNDARY_COMBOS);
		combos = next;
	}
	let authored = materialize_for_probe(lines, edits);
	combos.retain(|combo| combo.touched > 0);
	combos.sort_by_key(BoundaryCombo::cost);
	let mut best: Option<(BoundaryCombo, String)> = None;
	for combo in combos {
		if best
			.as_ref()
			.is_some_and(|(current, _)| combo.cost() > current.cost())
		{
			break;
		}
		let candidate = splice_boundary_combo(edits, &groups, &combo);
		let text = materialize_for_probe(lines, &candidate);
		if text == authored || !syntax::parses_cleanly(Some(path), &text) {
			continue;
		}
		let Some((_, best_text)) = &best else {
			best = Some((combo, text));
			continue;
		};
		if text.as_str() != best_text.as_str() {
			if let Some(group) = ambiguous_group.as_ref() {
				return Err(ambiguous_placement(group));
			}
			return Ok(None);
		}
	}
	let Some((best, _)) = best else {
		return ambiguous_group.map_or(Ok(None), |group| Err(ambiguous_placement(&group)));
	};
	let mut warnings = Vec::new();
	for (index, entry) in groups.iter().enumerate() {
		if let Some(variant_index) = best.variants[index] {
			let variant = &entry.variants[variant_index];
			let warning = match (variant.kept, variant.dropped) {
				(0, dropped) => {
					ApplyWarning::BoundaryRowsDropped { line: entry.group.start_line, dropped }
				},
				(kept, 0) => ApplyWarning::BoundaryRowsRetained { line: entry.group.start_line, kept },
				(kept, dropped) => ApplyWarning::BoundaryRowsRetainedAndDropped {
					line: entry.group.start_line,
					kept,
					dropped,
				},
			};
			warnings.push(warning);
		}
	}
	Ok(Some((splice_boundary_combo(edits, &groups, &best), warnings)))
}

const fn ambiguous_echo(ambiguity: &BoundaryAmbiguity) -> ApplyError {
	match ambiguity.side {
		BoundarySide::Leading => ApplyError::LeadingBoundaryEchoTooShort {
			start: ambiguity.start_line,
			end:   ambiguity.end_line,
			count: ambiguity.count,
		},
		BoundarySide::Trailing => ApplyError::TrailingBoundaryEchoTooShort {
			start: ambiguity.start_line,
			end:   ambiguity.end_line,
			count: ambiguity.count,
		},
	}
}

fn repair_boundaries(
	edits: &mut Vec<Edit>,
	lines: &[Str],
	source: &str,
	path: Option<&str>,
	baseline_parses: bool,
	warnings: &mut Vec<ApplyWarning>,
) -> Result<(), ApplyError> {
	let normalized = normalize_textual_boundary_echoes(edits, lines, source, path);
	warnings.extend(normalized.warnings);
	*edits = normalized.edits;
	let authored = materialize_for_probe(lines, edits);
	if syntax::parses_cleanly(path, &authored) {
		if let Some(ambiguity) = normalized.ambiguities.first() {
			return Err(ambiguous_echo(ambiguity));
		}
		return Ok(());
	}
	if let Some((repaired, repair_warnings)) =
		repair_boundary_variants(edits, lines, source, path, baseline_parses)?
	{
		*edits = repaired;
		warnings.extend(repair_warnings);
		return Ok(());
	}
	if let Some(ambiguity) = normalized.ambiguities.first() {
		return Err(ambiguous_echo(ambiguity));
	}
	Ok(())
}

fn materialize_line_rows(original: &Str, bucket: &[(usize, &Edit)]) -> Vec<Str> {
	let mut before = Vec::new();
	let mut replacement = Vec::new();
	let mut after = Vec::new();
	let mut delete = false;
	for (_, edit) in bucket {
		match edit {
			Edit::Delete { .. } => delete = true,
			Edit::Insert { text, mode: InsertMode::Replacement, .. } => {
				replacement.push(text.clone());
			},
			Edit::Insert { cursor: Cursor::AfterAnchor { .. }, text, .. } => {
				after.push(text.clone());
			},
			Edit::Insert { text, .. } => before.push(text.clone()),
			_ => {},
		}
	}
	let mut rows = before;
	rows.extend(replacement);
	if !delete {
		rows.push(original.clone());
	}
	rows.extend(after);
	rows
}

fn materialize(lines: &[Str], edits: &[Edit]) -> (String, Option<usize>) {
	let mut buckets: BTreeMap<usize, Vec<(usize, &Edit)>> = BTreeMap::new();
	let mut bof = Vec::new();
	let mut eof = Vec::new();
	for (order, edit) in edits.iter().enumerate() {
		match edit {
			Edit::Insert { cursor: Cursor::Bof, text, .. } => bof.push(text.as_str()),
			Edit::Insert { cursor: Cursor::Eof, text, .. } => eof.push(text.as_str()),
			_ => {
				if let Some(line) = anchor_line(edit) {
					buckets.entry(line).or_default().push((order, edit));
				}
			},
		}
	}
	let mut out = lines.to_vec();
	let mut first = None;
	for (line, bucket) in buckets.into_iter().rev() {
		let idx = line - 1;
		let rows = materialize_line_rows(&out[idx], &bucket);
		if rows.len() == 1 && rows[0] == out[idx] {
			continue;
		}
		out.splice(idx..=idx, rows);
		first = Some(first.map_or(line, |old: usize| old.min(line)));
	}
	if !bof.is_empty() {
		let rows = bof.into_iter().map(Str::new).collect::<Vec<_>>();
		if out.len() == 1 && out[0].is_empty() {
			out = rows;
		} else {
			out.splice(0..0, rows);
		}
		first = Some(1);
	}
	if !eof.is_empty() {
		let at = if out.last().is_some_and(|line| line.is_empty()) {
			out.len() - 1
		} else {
			out.len()
		};
		out.splice(at..at, eof.into_iter().map(Str::new));
		first = Some(first.map_or(at + 1, |old| old.min(at + 1)));
	}
	let capacity = out.iter().map(Str::len).sum::<usize>() + out.len().saturating_sub(1);
	let mut text = String::with_capacity(capacity);
	for (index, line) in out.iter().enumerate() {
		if index > 0 {
			text.push('\n');
		}
		text.push_str(line);
	}
	(text, first)
}

fn exact_line_ranges(text: &str) -> Vec<(usize, usize)> {
	let bytes = text.as_bytes();
	let mut ranges = Vec::new();
	let mut start = 0;
	let mut index = 0;
	while index < bytes.len() {
		let end = match bytes[index] {
			b'\n' => Some(index + 1),
			b'\r' if bytes.get(index + 1) == Some(&b'\n') => Some(index + 2),
			b'\r' => Some(index + 1),
			_ => None,
		};
		if let Some(end) = end {
			ranges.push((start, end));
			start = end;
			index = end;
		} else {
			index += 1;
		}
	}
	ranges.push((start, bytes.len()));
	ranges
}

fn encoded_rows(rows: &[Str], trailing_ending: bool, ending: LineEnding) -> Bytes {
	let capacity = rows.iter().map(Str::len).sum::<usize>()
		+ rows.len().saturating_sub(1)
		+ usize::from(trailing_ending);
	let mut normalized = String::with_capacity(capacity);
	for (index, row) in rows.iter().enumerate() {
		if index > 0 {
			normalized.push('\n');
		}
		normalized.push_str(row);
	}
	if trailing_ending && !rows.is_empty() {
		normalized.push('\n');
	}
	let restored = restore_line_endings(&normalized, ending);
	Bytes::copy_from_slice(restored.as_bytes())
}

fn localized_byte_edit(start: usize, end: usize, source: &[u8], replacement: Bytes) -> ByteEdit {
	let original = &source[start..end];
	let mut prefix = 0;
	while prefix < original.len()
		&& prefix < replacement.len()
		&& original[prefix] == replacement[prefix]
	{
		prefix += 1;
	}
	let mut suffix = 0;
	while suffix < original.len() - prefix
		&& suffix < replacement.len() - prefix
		&& original[original.len() - 1 - suffix] == replacement[replacement.len() - 1 - suffix]
	{
		suffix += 1;
	}
	let localized_start = start + prefix;
	let localized_end = end - suffix;
	if localized_start == localized_end && localized_start == end && start < end {
		return ByteEdit { start, end, replacement };
	}
	ByteEdit {
		start:       localized_start,
		end:         localized_end,
		replacement: replacement.slice(prefix..replacement.len() - suffix),
	}
}

fn anchored_byte_edits(
	source: &[u8],
	exact: &str,
	normalized: &str,
	lines: &[Str],
	edits: &[Edit],
	ending: LineEnding,
	final_bytes: &[u8],
) -> Result<Vec<ByteEdit>, ApplyError> {
	let bom_len = source.len() - exact.len();
	let ranges = exact_line_ranges(exact);
	if ranges.len() != lines.len() {
		return Err(ApplyError::AuthoredIdentityLost);
	}
	let mut buckets: BTreeMap<usize, Vec<(usize, &Edit)>> = BTreeMap::new();
	let mut bof = Vec::new();
	let mut eof = Vec::new();
	for (order, edit) in edits.iter().enumerate() {
		match edit {
			Edit::Insert { cursor: Cursor::Bof, text, .. } => bof.push(text.clone()),
			Edit::Insert { cursor: Cursor::Eof, text, .. } => eof.push(text.clone()),
			_ => {
				if let Some(line) = anchor_line(edit) {
					buckets.entry(line).or_default().push((order, edit));
				}
			},
		}
	}

	let mut exact_edits = Vec::with_capacity(buckets.len() + 2);
	if !bof.is_empty() {
		exact_edits.push(ByteEdit {
			start:       bom_len,
			end:         bom_len,
			replacement: encoded_rows(&bof, !normalized.is_empty(), ending),
		});
	}
	for (line, bucket) in buckets {
		let original = lines
			.get(line - 1)
			.ok_or(ApplyError::AuthoredIdentityLost)?;
		let rows = materialize_line_rows(original, &bucket);
		if rows.len() == 1 && rows[0] == *original {
			continue;
		}
		let (start, end) = ranges[line - 1];
		exact_edits.push(localized_byte_edit(
			bom_len + start,
			bom_len + end,
			source,
			encoded_rows(&rows, line < lines.len(), ending),
		));
	}
	if !eof.is_empty() {
		let mut replacement =
			encoded_rows(&eof, normalized.ends_with('\n') || normalized.is_empty(), ending);
		if !normalized.is_empty() && !normalized.ends_with('\n') {
			let mut prefixed = Vec::with_capacity(replacement.len() + 2);
			prefixed.extend_from_slice(match ending {
				LineEnding::Lf => b"\n",
				LineEnding::CrLf => b"\r\n",
			});
			prefixed.extend_from_slice(&replacement);
			replacement = Bytes::from(prefixed);
		}
		exact_edits.push(ByteEdit { start: source.len(), end: source.len(), replacement });
	}

	let mut replayed = Vec::with_capacity(final_bytes.len());
	let mut cursor = 0;
	for edit in &exact_edits {
		if edit.start < cursor || edit.end > source.len() {
			return Err(ApplyError::AuthoredIdentityLost);
		}
		replayed.extend_from_slice(&source[cursor..edit.start]);
		replayed.extend_from_slice(&edit.replacement);
		cursor = edit.end;
	}
	replayed.extend_from_slice(&source[cursor..]);
	if replayed != final_bytes {
		return Err(ApplyError::AuthoredIdentityLost);
	}
	Ok(exact_edits)
}

fn canonical_edit(base: &[u8], final_bytes: &[u8]) -> Vec<ByteEdit> {
	capture_diff_slices(Algorithm::Myers, base, final_bytes)
		.into_iter()
		.filter_map(|operation| {
			let (start, old_len, new_start, new_len) = match operation {
				DiffOp::Equal { .. } => return None,
				DiffOp::Delete { old_index, old_len, new_index } => (old_index, old_len, new_index, 0),
				DiffOp::Insert { old_index, new_index, new_len } => (old_index, 0, new_index, new_len),
				DiffOp::Replace { old_index, old_len, new_index, new_len } => {
					(old_index, old_len, new_index, new_len)
				},
			};
			Some(ByteEdit {
				start,
				end: start + old_len,
				replacement: Bytes::copy_from_slice(&final_bytes[new_start..new_start + new_len]),
			})
		})
		.collect()
}

/// Applies parsed edits to exact UTF-8 source bytes without filesystem access.
pub fn apply_parsed_patch(
	source: Bytes,
	patch: &ParsedPatch,
	clipboard: &mut Clipboard,
	options: ApplyOptions<'_>,
) -> Result<ApplyResult, ApplyError> {
	let exact = str::from_utf8(&source).map_err(ApplyError::InvalidUtf8)?;
	let ending = detect_line_ending(exact);
	let bom = strip_bom(exact);
	let normalized = normalize_to_lf(bom.text);
	let lines: Vec<Str> = normalized.split('\n').map(Str::new).collect();
	let addressable_count = split_addressable_file_lines(&normalized).count();
	let addressable_lines = &lines[..addressable_count];
	let BlockLowering { edits: blocked, resolutions, mut warnings } = resolve_block_edits(
		&patch.edits,
		&normalized,
		options.path,
		if options.mode == ApplyMode::Strict {
			UnresolvedBlockMode::Strict
		} else {
			UnresolvedBlockMode::Drop
		},
	)?;
	let clip = resolve_clipboard_edits(
		&blocked,
		addressable_lines,
		clipboard,
		if options.mode == ApplyMode::Strict {
			EmptyPasteMode::Strict
		} else {
			EmptyPasteMode::Drop
		},
	)?;
	warnings.extend(clip.warnings);
	let mut concrete = clip.edits;
	validate(&concrete, addressable_lines)?;
	repair_replacement_indentation(&mut concrete, addressable_lines, &mut warnings);
	repair_landings(&mut concrete, addressable_lines, &normalized, options.path, &mut warnings);
	let baseline_parses = syntax::parses_cleanly(options.path, &normalized);
	repair_boundaries(
		&mut concrete,
		addressable_lines,
		&normalized,
		options.path,
		baseline_parses,
		&mut warnings,
	)?;
	let (model, first_changed_line) = materialize(&lines, &concrete);
	if baseline_parses
		&& !syntax::parses_cleanly(options.path, &model)
		&& let Some(line) = first_changed_line
	{
		warnings.push(ApplyWarning::SyntaxBreak { path: options.path.map(Str::new), line });
	}
	let restored_endings = restore_line_endings(&model, ending);
	let restored = restore_bom(&restored_endings, bom.had_bom);
	let bytes = Bytes::copy_from_slice(restored.as_bytes());
	let edits = if concrete.iter().any(|edit| anchor_line(edit).is_some()) {
		anchored_byte_edits(&source, bom.text, &normalized, &lines, &concrete, ending, &bytes)?
	} else {
		canonical_edit(&source, &bytes)
	};
	Ok(ApplyResult { bytes, edits, first_changed_line, warnings, block_resolutions: resolutions })
}

/// Applies already-parsed edits using a private clipboard.
pub fn apply_edits(
	source: Bytes,
	edits: &[Edit],
	options: ApplyOptions<'_>,
) -> Result<ApplyResult, ApplyError> {
	let patch =
		ParsedPatch { edits: edits.to_vec(), file_op: None, diagnostics: Vec::new() };
	apply_parsed_patch(source, &patch, &mut Clipboard::default(), options)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::parser::parse_patch;

	#[test]
	fn canonical_diff_keeps_distant_changes_separate() {
		let base = b"first\nunchanged middle\nlast\n";
		let proposed = b"FIRST\nunchanged middle\nLAST\n";
		let edits = canonical_edit(base, proposed);
		assert_eq!(edits.len(), 2);
		assert!(edits[0].end <= b"first\n".len());
		assert!(edits[1].start >= b"first\nunchanged middle\n".len());

		let mut replayed = Vec::new();
		let mut cursor = 0;
		for edit in &edits {
			replayed.extend_from_slice(&base[cursor..edit.start]);
			replayed.extend_from_slice(&edit.replacement);
			cursor = edit.end;
		}
		replayed.extend_from_slice(&base[cursor..]);
		assert_eq!(replayed, proposed);
		assert_eq!(
			&replayed[b"FIRST\n".len()..b"FIRST\nunchanged middle\n".len()],
			b"unchanged middle\n"
		);
	}

	fn apply_patch(source: &str, patch: &str, path: &str) -> Result<ApplyResult, ApplyError> {
		let parsed = parse_patch(patch).expect("fixture patch parses");
		apply_parsed_patch(
			Bytes::copy_from_slice(source.as_bytes()),
			&parsed,
			&mut Clipboard::default(),
			ApplyOptions { mode: ApplyMode::Strict, path: Some(path) },
		)
	}
	#[test]
	fn repeated_line_edits_retain_the_authored_byte_range() {
		let source = "top\nduplicate\nmiddle\nduplicate\nbottom\n";
		let result = apply_patch(source, "CUT 4", "fixture.txt").expect("delete authored duplicate");
		let authored_start = source
			.find("duplicate\nbottom")
			.expect("fourth line offset");
		assert_eq!(result.edits.len(), 1);
		assert_eq!(result.edits[0].start, authored_start);
		assert_eq!(result.edits[0].end, authored_start + "duplicate\n".len());
		assert_eq!(result.bytes, Bytes::from_static(b"top\nduplicate\nmiddle\nbottom\n"));
	}

	#[test]
	fn retains_syntax_essential_opening_comment_boundary() {
		let source = "class C {\n\t/**\n\t * Old summary.\n\t */\n\tmethod() {}\n}\n";
		let result = apply_patch(source, "PUT 2.=4:\n+\t * New summary.\n+\t */", "fixture.ts")
			.expect("boundary repair succeeds");
		assert_eq!(
			result.bytes,
			Bytes::from_static(b"class C {\n\t/**\n\t * New summary.\n\t */\n\tmethod() {}\n}\n")
		);
		assert!(result.warnings.iter().any(|warning| matches!(
			warning,
			ApplyWarning::BoundaryRowsRetained { .. }
				| ApplyWarning::BoundaryRowsDropped { .. }
				| ApplyWarning::BoundaryRowsRetainedAndDropped { .. }
		)));
	}

	#[test]
	fn searches_boundary_variants_across_replacement_groups() {
		let source = "fn a() {\n\told();\n}\nfn b() {\n\told();\n}\n";
		let patch = "PUT 2.=3:\n+\tnew_a();\nPUT 5.=6:\n+\tnew_b();";
		let result = apply_patch(source, patch, "fixture.rs").expect("combined repair succeeds");
		assert_eq!(
			result.bytes,
			Bytes::from_static(b"fn a() {\n\tnew_a();\n}\nfn b() {\n\tnew_b();\n}\n")
		);
		assert_eq!(
			result
				.warnings
				.iter()
				.filter(|warning| matches!(
					warning,
					ApplyWarning::BoundaryRowsRetained { .. }
						| ApplyWarning::BoundaryRowsDropped { .. }
						| ApplyWarning::BoundaryRowsRetainedAndDropped { .. }
				))
				.count(),
			2
		);
	}

	#[test]
	fn drops_tsx_boundary_echo_only_after_parse_validation() {
		let source = "const view = (\n  <section>\n    <Old />\n  </section>\n);\n";
		let patch = "PUT 3.=3:\n+    <New />\n+  </section>";
		let result = apply_patch(source, patch, "fixture.tsx").expect("echo repair succeeds");
		assert_eq!(
			result.bytes,
			Bytes::from_static(b"const view = (\n  <section>\n    <New />\n  </section>\n);\n")
		);
		assert!(result.warnings.iter().any(|warning| matches!(
			warning,
			ApplyWarning::BoundaryRowsRetained { .. }
				| ApplyWarning::BoundaryRowsDropped { .. }
				| ApplyWarning::BoundaryRowsRetainedAndDropped { .. }
		)));
	}

	#[test]
	fn parse_breakage_is_advisory_not_rejection() {
		let source = "fn f() {\n\tlet value = make()\n\t\t.finish();\n}\n";
		let result = apply_patch(source, "PUT 3.=3:\n+\treturn 1;", "fixture.rs")
			.expect("authored edit remains applied");
		assert!(
			str::from_utf8(&result.bytes)
				.unwrap()
				.contains("\treturn 1;")
		);
		assert!(result.warnings.iter().any(|warning| matches!(
			warning,
			ApplyWarning::SyntaxBreak { path, line: 3 }
				if path.as_deref() == Some("fixture.rs")
		)));
	}

	#[test]
	fn drops_trailing_annotation_echo_on_single_line_replacement() {
		let source =
			"/// Unified-diff hunks\n/// Old details.\n#[napi]\npub fn structured_patch_hunks() {}\n";
		let patch = "PUT 2.=2:\n+/// New details.\n+#[napi]";
		let result = apply_patch(source, patch, "fixture.rs").expect("annotation echo is repaired");
		assert_eq!(
			result.bytes,
			Bytes::from_static(
				b"/// Unified-diff hunks\n/// New details.\n#[napi]\npub fn \
				  structured_patch_hunks() {}\n"
			)
		);
		assert!(
			result.warnings.iter().any(|warning| matches!(
				warning,
				ApplyWarning::BoundaryEchoDropped { trailing: 1, .. }
			))
		);
	}

	#[test]
	fn drops_leading_annotation_echo_on_single_line_replacement() {
		let source = "#[napi]\n/// Old summary.\npub fn f() {}\n";
		let result = apply_patch(source, "PUT 2.=2:\n+#[napi]\n+/// New summary.", "fixture.rs")
			.expect("annotation echo is repaired");
		assert_eq!(result.bytes, Bytes::from_static(b"#[napi]\n/// New summary.\npub fn f() {}\n"));
		assert!(
			result
				.warnings
				.iter()
				.any(|warning| matches!(warning, ApplyWarning::BoundaryEchoDropped { leading: 1, .. }))
		);
	}

	#[test]
	fn rejects_underfilled_single_line_annotation_echo() {
		let source = "/// Old doc.\n#[napi]\npub fn f() {}\n";
		let error = apply_patch(source, "PUT 1.=1:\n+#[napi]", "fixture.rs")
			.expect_err("echo-only replacement must not become a deletion");
		assert!(matches!(error, ApplyError::TrailingBoundaryEchoTooShort { count: 1, .. }));
	}

	#[test]
	fn classifies_typescript_decorator_echo_through_grammar() {
		let source = "/** Old summary. */\n@Injectable()\nclass Service {}\n";
		let patch = "PUT 1.=1:\n+/** Creates request-scoped services. */\n+@Injectable()";
		let result = apply_patch(source, patch, "fixture.ts").expect("decorator echo is repaired");
		assert_eq!(
			result.bytes,
			Bytes::from_static(
				b"/** Creates request-scoped services. */\n@Injectable()\nclass Service {}\n"
			)
		);
	}

	#[test]
	fn keeps_statement_echo_literal_on_single_line_replacement() {
		let source = "foo();\nold();\nbar();\n";
		let result = apply_patch(source, "PUT 2.=2:\n+fresh();\n+bar();", "fixture.ts")
			.expect("literal duplicate remains valid");
		assert_eq!(result.bytes, Bytes::from_static(b"foo();\nfresh();\nbar();\nbar();\n"));
		assert!(
			!result
				.warnings
				.iter()
				.any(|warning| matches!(warning, ApplyWarning::BoundaryEchoDropped { .. }))
		);
	}

	const OPENER_RUST: &str = "mod testing {\n   struct MemStream;\n\n   impl Stream for MemStream \
	                           {\n      fn clone_box(&self) -> u32 {\n         1\n      }\n\n      \
	                           fn try_borrow(&self) -> u32 {\n         7\n      }\n   }\n}\n";
	const OPENER_MOD_PATCH: &str = "PUT >5:\n+\n+\tmod stdout_policy {\n+\t\tuse \
	                                super::MemStream;\n+\n+\t\t#[test]\n+\t\tfn line_policy() \
	                                {\n+\t\t\tassert!(true);\n+\t\t}\n+\t}";

	#[test]
	fn escapes_construct_insert_from_an_opening_line() {
		let result = apply_patch(OPENER_RUST, OPENER_MOD_PATCH, "fixture.rs")
			.expect("construct is relocated past its enclosing implementation");
		let text = str::from_utf8(&result.bytes).unwrap();
		assert!(text.contains("      }\n   }\n\n\tmod stdout_policy {"));
		assert!(!text.contains("fn clone_box(&self) -> u32 {\n\n\tmod stdout_policy"));
		assert!(result.warnings.iter().any(|warning| matches!(
			warning,
			ApplyWarning::AfterLineLandingShifted { from: 5, to: 12, .. }
		)));
	}

	#[test]
	fn opener_escape_keeps_bare_and_equal_depth_bodies_literal() {
		let bare = apply_patch(OPENER_RUST, "PUT >5:\n+   let x = 1;", "fixture.rs")
			.expect("bare statement stays at the adjacent gap");
		assert!(
			str::from_utf8(&bare.bytes)
				.unwrap()
				.contains("fn clone_box(&self) -> u32 {\n   let x = 1;")
		);

		let equal = apply_patch(
			OPENER_RUST,
			"PUT >5:\n+      fn extra(&self) -> u32 {\n+         2\n+      }",
			"fixture.rs",
		)
		.expect("equal-depth item stays inside the opener");
		assert!(
			str::from_utf8(&equal.bytes)
				.unwrap()
				.contains("fn clone_box(&self) -> u32 {\n      fn extra")
		);
	}

	#[test]
	fn opener_escape_requires_a_parseable_relocation_and_path() {
		let invalid = apply_patch(OPENER_RUST, "PUT >5:\n+   Some(1) => {\n+   }", "fixture.rs")
			.expect("unparseable relocation remains literal");
		assert!(
			str::from_utf8(&invalid.bytes)
				.unwrap()
				.contains("fn clone_box(&self) -> u32 {\n   Some(1) => {")
		);

		let parsed = parse_patch(OPENER_MOD_PATCH).unwrap();
		let no_path = apply_parsed_patch(
			Bytes::copy_from_slice(OPENER_RUST.as_bytes()),
			&parsed,
			&mut Clipboard::default(),
			ApplyOptions::default(),
		)
		.expect("missing syntax evidence leaves authored placement literal");
		assert!(
			str::from_utf8(&no_path.bytes)
				.unwrap()
				.contains("fn clone_box(&self) -> u32 {\n\n\tmod stdout_policy")
		);
	}

	#[test]
	fn escapes_shallower_function_past_typescript_class() {
		let source = "class A {\n   method() {\n      return 1;\n   }\n}\n";
		let patch = "PUT >2:\n+function helper() {\n+   return 2;\n+}";
		let result =
			apply_patch(source, patch, "fixture.ts").expect("function relocates past the class");
		assert_eq!(
			result.bytes,
			Bytes::from_static(
				b"class A {\n   method() {\n      return 1;\n   }\n}\nfunction helper() {\n   return \
				  2;\n}\n"
			)
		);
	}

	#[test]
	fn terminal_newline_sentinel_cannot_be_targeted() {
		let error = apply_patch("a\nb\n", "CUT 3", "fixture.txt").expect_err("line 3 is not content");
		assert!(matches!(
			error,
			ApplyError::Clipboard(ClipboardError::RangeOutOfBounds { start: 3, end: 3, total: 2, .. })
		));
	}
}
