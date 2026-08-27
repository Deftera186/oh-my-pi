//! Sparse-edit parsing and pure, atomic text transformation.

use std::{collections::VecDeque, mem};

use omp_core::Str;

const OPENER: &str = "§";
const REWRITE: &str = "»";
const GAP: &str = "…";
const SELECT_OPEN: &str = "⟪";
const SELECT_CLOSE: &str = "⟫";
const SELECT_DIVIDER: &str = "│";
const ADD_LINE: char = '＋';
const LITERAL_OPEN: &str = "\0SLOPPY_OPEN\0";
const LITERAL_CLOSE: &str = "\0SLOPPY_CLOSE\0";
const LITERAL_DIVIDER: &str = "\0SLOPPY_DIVIDER\0";
const MAX_CANDIDATES: usize = 200;

/// One `§path` section of a sloppy payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SloppySection {
	/// Authored relative path.
	pub path:  Str,
	/// Canonical operation body, using bare `§`/`§*` openers.
	pub input: Str,
}

/// Sloppy syntax, matching, or recovery failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SloppyError {
	/// Text appeared before the first path-bearing operation opener.
	#[error("sloppy input must begin with §relative/path")]
	MissingPath,
	/// A section had no operations.
	#[error("sloppy section for {path} has no operations")]
	EmptySection {
		/// Authored repository-relative path of the empty section.
		path: Str,
	},
	/// An operation opener was malformed.
	#[error("operation {operation} has an invalid § opener")]
	InvalidOpener {
		/// One-based position of the operation whose opener was expected.
		operation: usize,
	},
	/// Match/rewrite structure was malformed.
	#[error("operation {operation} is malformed: {reason}")]
	Malformed {
		/// One-based position of the malformed operation.
		operation: usize,
		/// Static explanation of the violated syntax rule.
		reason:    &'static str,
	},
	/// A pattern found no bounded exact or fuzzy match.
	#[error("operation {operation} did not match the source")]
	NoMatch {
		/// One-based position of the unmatched operation.
		operation: usize,
	},
	/// A unique operation matched multiple locations.
	#[error("operation {operation} is ambiguous at source lines {lines:?}; use §* or add context")]
	Ambiguous {
		/// One-based position of the ambiguous operation.
		operation: usize,
		/// One-based source lines where candidate matches begin.
		lines:     Vec<usize>,
	},
	/// Applying selected ranges would overlap incompatibly.
	#[error("operation {operation} overlaps another operation incompatibly")]
	Overlap {
		/// One-based position of the operation that conflicts with another edit.
		operation: usize,
	},
	/// An operation parsed cleanly but changed no bytes.
	#[error("operation {operation} produced no change")]
	NoChange {
		/// One-based position of the operation reported as unchanged.
		operation: usize,
	},
	/// A malformed operation can be retried with one complete corrected payload.
	#[error("{message}")]
	Retry {
		/// One-based position of the operation requiring repair.
		operation: usize,
		/// Copy-ready, path-aware corrected payload.
		message:   Str,
	},
}

/// Extracts path-bearing `§` openers from a possibly incomplete payload.
///
/// Bare continuation openers and recovery-only `§»` lines are ignored. Paths
/// are returned in authored order and are not deduplicated.
pub fn sloppy_paths(input: &str) -> Vec<Str> {
	input
		.replace("\r\n", "\n")
		.replace('\r', "\n")
		.lines()
		.filter_map(|line| parse_section_opener(line).and_then(|(_, path)| path.map(Str::new)))
		.collect()
}

/// Splits a payload into `§path` sections, dropping common foreign-envelope
/// noise. A bare `§` continues the current file.
pub fn split_sloppy_sections(input: &str) -> Result<Vec<SloppySection>, SloppyError> {
	let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
	let lines = strip_envelope_noise(normalized.split('\n').collect());
	let mut sections = Vec::new();
	let mut path: Option<Str> = None;
	let mut body = String::new();

	for line in lines {
		if let Some((all, authored_path)) = parse_section_opener(line) {
			if let Some(authored_path) = authored_path {
				flush_section(&mut sections, &mut path, &mut body)?;
				path = Some(Str::new(authored_path));
			} else if path.is_none() {
				return Err(SloppyError::MissingPath);
			}
			body.push_str(if all { "§*\n" } else { "§\n" });
			continue;
		}
		if path.is_none() {
			if line.trim().is_empty() {
				continue;
			}
			return Err(SloppyError::MissingPath);
		}
		body.push_str(line);
		body.push('\n');
	}
	flush_section(&mut sections, &mut path, &mut body)?;
	if sections.is_empty() {
		return Err(SloppyError::MissingPath);
	}
	Ok(sections)
}

fn flush_section(
	sections: &mut Vec<SloppySection>,
	path: &mut Option<Str>,
	body: &mut String,
) -> Result<(), SloppyError> {
	let Some(path) = path.take() else {
		return Ok(());
	};
	if body.trim().is_empty() {
		return Err(SloppyError::EmptySection { path });
	}
	sections.push(SloppySection { path, input: body.trim_matches('\n').into() });
	body.clear();
	Ok(())
}

fn parse_section_opener(line: &str) -> Option<(bool, Option<&str>)> {
	let line = line.trim();
	if line == "§»" || !line.starts_with(OPENER) {
		return None;
	}
	let mut rest = &line[OPENER.len()..];
	let all = rest.starts_with('*');
	if all {
		rest = &rest[1..];
	}
	let rest = rest.trim();
	if rest.is_empty() {
		return Some((all, None));
	}
	if rest.contains(['*', '§', '«', '»', '\n']) {
		return None;
	}
	Some((all, Some(rest)))
}

fn strip_envelope_noise<'a>(raw: Vec<&'a str>) -> Vec<&'a str> {
	let mut lines = Vec::new();
	let mut skipping = false;
	for line in raw {
		let trimmed = line.trim();
		if envelope_begin(trimmed) {
			skipping = false;
			continue;
		}
		if envelope_end(trimmed) {
			skipping = true;
			continue;
		}
		if envelope_control(trimmed) {
			continue;
		}
		if skipping {
			if parse_section_opener(line)
				.and_then(|(_, path)| path)
				.is_none()
			{
				continue;
			}
			skipping = false;
		}
		lines.push(line);
	}
	lines
}

fn envelope_begin(line: &str) -> bool {
	line.starts_with("*** Begin") || line.eq_ignore_ascii_case("Begin Patch")
}

fn envelope_end(line: &str) -> bool {
	line.starts_with("*** End") || line.eq_ignore_ascii_case("End Patch")
}

fn envelope_control(line: &str) -> bool {
	line == "***"
		|| line.starts_with("*** Abort")
		|| line.starts_with("*** Update File:")
		|| line.starts_with("*** Add File:")
		|| line.starts_with("*** Delete File:")
}

#[derive(Clone, Debug)]
struct Operation {
	all:           bool,
	pattern_raw:   String,
	match_text:    String,
	desired:       Vec<Piece>,
	has_add:       bool,
	block_rewrite: Option<String>,
	recovery_note: Option<Str>,
}

#[derive(Clone, Debug)]
enum Piece {
	Text(String),
	Capture(usize),
	CaptureOrGap(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParseState {
	Outside,
	Pattern,
	Rewrite,
}

fn parse_operations(
	input: &str,
	source: &str,
	path: Option<&str>,
) -> Result<Vec<Operation>, SloppyError> {
	let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
	let mut lines = Vec::<String>::new();
	for line in normalized.split('\n') {
		let trimmed = line.trim_start();
		if let Some(rest) = trimmed.strip_prefix(REWRITE)
			&& !rest.is_empty()
			&& !trimmed.starts_with("§»")
		{
			lines.push(REWRITE.to_owned());
			lines.push(rest.to_owned());
		} else {
			lines.push(line.to_owned());
		}
	}

	let mut operations = Vec::new();
	let mut state = ParseState::Outside;
	let mut all = false;
	let mut pattern = Vec::new();
	let mut rewrite = Vec::new();

	for (line_index, line) in lines.iter().enumerate() {
		let trimmed = line.trim();
		if trimmed == "§»" {
			if state == ParseState::Pattern
				&& pattern.iter().any(|line: &String| !line.trim().is_empty())
			{
				state = ParseState::Rewrite;
			}
			continue;
		}
		if let Some(op_all) = parse_operation_opener(trimmed) {
			if state != ParseState::Outside {
				operations.push(build_operation(
					all,
					&pattern,
					(state == ParseState::Rewrite).then_some(rewrite.as_slice()),
					operations.len() + 1,
					source,
					&lines,
					line_index,
					path,
				)?);
			}
			state = ParseState::Pattern;
			all = op_all;
			pattern.clear();
			rewrite.clear();
			continue;
		}
		match state {
			ParseState::Outside => {
				if !trimmed.is_empty() && trimmed != REWRITE {
					return Err(SloppyError::InvalidOpener { operation: operations.len() + 1 });
				}
			},
			ParseState::Pattern if trimmed == REWRITE => state = ParseState::Rewrite,
			ParseState::Pattern => pattern.push(line.clone()),
			ParseState::Rewrite if trimmed == REWRITE => {
				// A second or trailing separator is recovery-only structure, never
				// text.
			},
			ParseState::Rewrite => rewrite.push(line.clone()),
		}
	}
	if state != ParseState::Outside {
		operations.push(build_operation(
			all,
			&pattern,
			(state == ParseState::Rewrite).then_some(rewrite.as_slice()),
			operations.len() + 1,
			source,
			&lines,
			lines.len(),
			path,
		)?);
	}
	if operations.is_empty() {
		return Err(SloppyError::InvalidOpener { operation: 1 });
	}
	Ok(operations)
}

fn parse_operation_opener(line: &str) -> Option<bool> {
	match line {
		"§" => Some(false),
		"§*" => Some(true),
		_ => None,
	}
}

fn build_operation(
	all: bool,
	pattern_lines: &[String],
	rewrite_lines: Option<&[String]>,
	operation: usize,
	source: &str,
	full_lines: &[String],
	end_index: usize,
	path: Option<&str>,
) -> Result<Operation, SloppyError> {
	let mut pattern_raw = trim_trailing_blank_lines(pattern_lines).join("\n");
	if pattern_raw.trim().is_empty() {
		return Err(SloppyError::Malformed { operation, reason: "MATCH must not be empty" });
	}
	let has_add = has_add_lines(&pattern_raw);
	if has_add {
		pattern_raw = embed_add_lines(&pattern_raw);
	}
	let selections = selection_facts(&pattern_raw, operation)?;
	let explicit = rewrite_lines.map(|lines| {
		trim_trailing_blank_lines(lines)
			.iter()
			.map(|line| add_line_text(line).unwrap_or_else(|| line.clone()))
			.collect::<Vec<_>>()
			.join("\n")
	});

	match explicit {
		Some(rewrite) if selections.bare == 1 && selections.paired == 0 => {
			let expanded = expand_echoed_line_selection(&pattern_raw, &rewrite);
			let pattern = expanded.as_deref().unwrap_or(&pattern_raw);
			let (match_text, desired) = legacy_selection_template(pattern, &rewrite, operation)?;
			Ok(Operation {
				all,
				pattern_raw,
				match_text,
				desired,
				has_add,
				block_rewrite: None,
				recovery_note: expanded.map(|_| {
					Str::new(
						"Note: REWRITE restated the whole selection-bearing line, so the full line was \
						 replaced.",
					)
				}),
			})
		},
		Some(rewrite) if selections.paired > 0 => {
			let (match_text, inline_desired, notes) = inline_template(&pattern_raw, true, operation)?;
			let inline_text = pieces_source(&inline_desired);
			let redundant = rewrite.trim().is_empty()
				|| normalize_text(&rewrite) == normalize_text(&inline_text)
				|| normalize_text(&rewrite) == normalize_text(&match_text)
				|| selection_desired_sides(&pattern_raw)
					.is_some_and(|sides| normalize_text(&sides) == normalize_text(&rewrite));
			let desired = if redundant {
				inline_desired
			} else {
				pieces_from_rewrite(&rewrite, gap_count(&match_text), None, operation)?
			};
			Ok(Operation {
				all,
				pattern_raw,
				match_text,
				desired,
				has_add,
				block_rewrite: (!redundant).then_some(rewrite),
				recovery_note: (!notes.is_empty()).then(|| Str::new(notes.join("\n"))),
			})
		},
		Some(_) if selections.bare > 0 => Err(SloppyError::Malformed {
			operation,
			reason: "block rewrite may accompany exactly one bare ⟪current⟫ selection",
		}),
		Some(rewrite) => {
			let match_text = pattern_raw.clone();
			let desired = pieces_from_rewrite(&rewrite, gap_count(&match_text), None, operation)?;
			Ok(Operation {
				all,
				pattern_raw,
				match_text,
				desired,
				has_add,
				block_rewrite: Some(rewrite),
				recovery_note: None,
			})
		},
		None if selections.paired > 0 || selections.bare > 0 || has_add => {
			let (match_text, desired, notes) = inline_template(&pattern_raw, true, operation)?;
			Ok(Operation {
				all,
				pattern_raw,
				match_text,
				desired,
				has_add,
				block_rewrite: None,
				recovery_note: (!notes.is_empty()).then(|| Str::new(notes.join("\n"))),
			})
		},
		None => {
			if let Some(current) = closest_desired_block(source, &pattern_raw) {
				return Ok(Operation {
					all,
					pattern_raw: pattern_raw.clone(),
					match_text: current,
					desired: vec![Piece::Text(pattern_raw)],
					has_add,
					block_rewrite: None,
					recovery_note: Some(Str::new(format!(
						"Note: operation {operation} stated desired text without markers; the closest \
						 matching block was replaced with it."
					))),
				});
			}
			let mut corrected = full_lines.to_vec();
			corrected.splice(end_index..end_index, [REWRITE.to_owned(), "<new text>".to_owned()]);
			if let Some(path) = path
				&& let Some(opener) = corrected.first_mut()
				&& matches!(opener.as_str(), "§" | "§*")
			{
				opener.push_str(path);
			}
			Err(SloppyError::Retry {
				operation,
				message: Str::new(format!(
					"Operation {operation} needs ».\nCopy-ready corrected payload (fill in the new \
					 text):\n{}",
					corrected.join("\n")
				)),
			})
		},
	}
}

fn trim_trailing_blank_lines(lines: &[String]) -> &[String] {
	let mut end = lines.len();
	while end > 0 && lines[end - 1].trim().is_empty() {
		end -= 1;
	}
	&lines[..end]
}

#[derive(Clone, Copy, Debug, Default)]
struct SelectionFacts {
	paired: usize,
	bare:   usize,
}

fn selection_facts(pattern: &str, operation: usize) -> Result<SelectionFacts, SloppyError> {
	let mut facts = SelectionFacts::default();
	let mut rest = pattern;
	while let Some(open) = rest.find(SELECT_OPEN) {
		let after = &rest[open + SELECT_OPEN.len()..];
		let Some(close) = after.find(SELECT_CLOSE) else {
			return Err(SloppyError::Malformed { operation, reason: "selection is missing ⟫" });
		};
		let selection = &after[..close];
		if selection.contains(SELECT_DIVIDER) {
			facts.paired += 1;
		} else {
			facts.bare += 1;
		}
		rest = &after[close + SELECT_CLOSE.len()..];
	}
	if rest.contains(SELECT_CLOSE) {
		return Err(SloppyError::Malformed { operation, reason: "selection is missing ⟪" });
	}
	Ok(facts)
}

fn expand_echoed_line_selection(pattern: &str, rewrite: &str) -> Option<String> {
	let rewrite_lines = rewrite
		.lines()
		.filter(|line| !line.trim().is_empty())
		.collect::<Vec<_>>();
	if rewrite_lines.len() != 1 {
		return None;
	}
	let mut lines = pattern.lines().map(str::to_owned).collect::<Vec<_>>();
	let selected = lines.iter().position(|line| line.contains(SELECT_OPEN))?;
	if lines
		.iter()
		.filter(|line| line.contains(SELECT_OPEN))
		.count()
		!= 1
	{
		return None;
	}
	let line = &lines[selected];
	if line.contains(GAP) {
		return None;
	}
	let open = line.find(SELECT_OPEN)?;
	let after_open = open + SELECT_OPEN.len();
	let close = line[after_open..].find(SELECT_CLOSE)? + after_open;
	let selection = &line[after_open..close];
	if selection.contains(SELECT_DIVIDER) {
		return None;
	}
	let prefix = &line[..open];
	if prefix.trim().is_empty()
		|| !normalize_text(rewrite_lines[0]).starts_with(&normalize_text(prefix))
	{
		return None;
	}
	let unmarked = line.replace(SELECT_OPEN, "").replace(SELECT_CLOSE, "");
	lines[selected] = format!("{SELECT_OPEN}{unmarked}{SELECT_CLOSE}");
	Some(lines.join("\n"))
}

fn closest_desired_block(source: &str, stated: &str) -> Option<String> {
	let stated_normalized = normalize_text(stated);
	if !(12..=1000).contains(&stated_normalized.len()) {
		return None;
	}
	let count = stated.split('\n').count();
	let lines = source.split('\n').collect::<Vec<_>>();
	if lines.len() < count {
		return None;
	}
	let mut scores = vec![1.0_f64; lines.len() - count + 1];
	let mut best: Option<(usize, f64)> = None;
	for index in 0..=lines.len() - count {
		let window = normalize_text(&lines[index..index + count].join("\n"));
		let maximum = stated_normalized.len().max(window.len()).max(1);
		let affix = stated_normalized.starts_with(window.as_str())
			|| window.starts_with(stated_normalized.as_str())
			|| stated_normalized.ends_with(window.as_str())
			|| window.ends_with(stated_normalized.as_str());
		if window == stated_normalized {
			return None;
		}
		if window.is_empty()
			|| affix
			|| stated_normalized.len().abs_diff(window.len()) as f64 / maximum as f64 > 0.35
		{
			continue;
		}
		let score = levenshtein(&stated_normalized, &window) as f64 / maximum as f64;
		scores[index] = score;
		if best.is_none_or(|(_, current)| score < current) {
			best = Some((index, score));
		}
	}
	let (best_index, best_score) = best?;
	if best_score > 0.35 {
		return None;
	}
	if scores
		.iter()
		.enumerate()
		.any(|(index, score)| index.abs_diff(best_index) >= count && *score - best_score < 0.1)
	{
		return None;
	}
	let text = lines[best_index..best_index + count].join("\n");
	let structural = [SELECT_OPEN, SELECT_CLOSE, SELECT_DIVIDER, GAP, REWRITE, OPENER];
	(!structural.iter().any(|marker| text.contains(marker))).then_some(text)
}

fn has_add_lines(pattern: &str) -> bool {
	pattern.lines().any(|line| add_line_text(line).is_some())
}

fn encode_literal_markers(text: &str) -> String {
	text
		.replace(SELECT_OPEN, LITERAL_OPEN)
		.replace(SELECT_CLOSE, LITERAL_CLOSE)
		.replace(SELECT_DIVIDER, LITERAL_DIVIDER)
}

fn decode_literal_markers(text: &str) -> String {
	text
		.replace(LITERAL_OPEN, SELECT_OPEN)
		.replace(LITERAL_CLOSE, SELECT_CLOSE)
		.replace(LITERAL_DIVIDER, SELECT_DIVIDER)
}

fn add_line_text(line: &str) -> Option<String> {
	let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
	let rest = &line[indent_len..];
	rest
		.strip_prefix(ADD_LINE)
		.map(|text| format!("{}{}", &line[..indent_len], text))
}

fn embed_add_lines(pattern: &str) -> String {
	let mut source = pattern
		.split('\n')
		.map(|line| {
			if line.trim().is_empty() {
				String::new()
			} else {
				line.to_owned()
			}
		})
		.collect::<Vec<_>>();
	let mut compact = Vec::with_capacity(source.len());
	for line in source.drain(..) {
		if line.is_empty() && compact.last().is_some_and(String::is_empty) {
			continue;
		}
		compact.push(line);
	}

	let mut output = Vec::<String>::new();
	let mut index = 0;
	while index < compact.len() {
		if add_line_text(&compact[index]).is_none() {
			output.push(compact[index].clone());
			index += 1;
			continue;
		}
		let mut added = Vec::new();
		while index < compact.len() {
			let Some(line) = add_line_text(&compact[index]) else {
				break;
			};
			added.push(encode_literal_markers(&line));
			index += 1;
		}
		if added.len() == 1
			&& let Some(anchor) = output.last_mut()
			&& is_near_variant(anchor, &added[0])
		{
			let old = mem::take(anchor);
			*anchor = format!("{SELECT_OPEN}{old}{SELECT_DIVIDER}{}{SELECT_CLOSE}", added[0]);
			continue;
		}
		let inserted = added.join("\n");
		if output.last().is_some_and(String::is_empty)
			&& index < compact.len()
			&& compact[index].is_empty()
		{
			output.pop();
		}
		if index < compact.len() {
			let gap_below = compact[index..]
				.iter()
				.find(|line| !line.trim().is_empty())
				.is_some_and(|line| line.trim() == GAP);
			if gap_below && wrap_trailing_add_anchor(&mut output, &inserted) {
				output.push(compact[index].clone());
			} else {
				output.push(format!(
					"{SELECT_OPEN}{SELECT_DIVIDER}{inserted}\n{SELECT_CLOSE}{}",
					compact[index]
				));
			}
			index += 1;
		} else if let Some(previous) = output.last_mut() {
			previous.push_str(&format!("{SELECT_OPEN}{SELECT_DIVIDER}\n{inserted}{SELECT_CLOSE}"));
		} else {
			output.push(format!("{SELECT_OPEN}{SELECT_DIVIDER}{inserted}{SELECT_CLOSE}"));
		}
	}
	output.join("\n")
}

fn wrap_trailing_add_anchor(output: &mut [String], inserted: &str) -> bool {
	let Some(previous) = output.last_mut() else {
		return false;
	};
	if previous.trim().is_empty()
		|| previous.contains(GAP)
		|| previous.contains(SELECT_OPEN)
		|| previous.contains(SELECT_DIVIDER)
	{
		return false;
	}
	let anchor = mem::take(previous);
	*previous = format!("{SELECT_OPEN}{anchor}{SELECT_DIVIDER}{anchor}\n{inserted}{SELECT_CLOSE}");
	true
}

fn is_near_variant(anchor: &str, added: &str) -> bool {
	if anchor.contains(SELECT_OPEN) || anchor.trim().is_empty() || anchor.trim() == added.trim() {
		return false;
	}
	let left = word_tokens(anchor);
	let right = word_tokens(added);
	if left.is_empty() || right.is_empty() {
		return false;
	}
	let shared = left.iter().filter(|token| right.contains(token)).count();
	shared.saturating_mul(2) >= left.len().saturating_add(right.len()).saturating_mul(4) / 5
}

fn word_tokens(text: &str) -> Vec<&str> {
	text
		.split(|character: char| {
			!(character.is_alphanumeric() || character == '_' || character == '$')
		})
		.filter(|token| !token.is_empty())
		.collect()
}

fn resolve_literal_dividers(selection: &str, operation: usize) -> (String, String, Str) {
	let dividers = selection
		.match_indices(SELECT_DIVIDER)
		.map(|(at, _)| at)
		.collect::<Vec<_>>();
	let last = *dividers.last().expect("called for a divided selection");
	let advice = format!(
		"Selections containing literal {SELECT_DIVIDER} are ambiguous; use a {REWRITE} block \
		 rewrite instead."
	);
	if last + SELECT_DIVIDER.len() == selection.len() {
		return (
			selection[..last].to_owned(),
			String::new(),
			Str::new(format!(
				"Note: operation {operation}'s trailing {SELECT_DIVIDER} was read as deletion; inner \
				 dividers were literal. {advice}"
			)),
		);
	}
	if dividers.len() % 2 == 1 {
		let middle = dividers[dividers.len() / 2];
		return (
			selection[..middle].to_owned(),
			selection[middle + SELECT_DIVIDER.len()..].to_owned(),
			Str::new(format!(
				"Note: operation {operation}'s middle {SELECT_DIVIDER} was read as the divider; the \
				 others were literal. {advice}"
			)),
		);
	}
	(
		selection.to_owned(),
		String::new(),
		Str::new(format!(
			"Note: operation {operation}'s even divider count was read as deletion. {advice}"
		)),
	)
}

fn inline_template(
	pattern: &str,
	bare_as_desired: bool,
	operation: usize,
) -> Result<(String, Vec<Piece>, Vec<Str>), SloppyError> {
	let mut match_text = String::new();
	let mut desired = Vec::new();
	let mut notes = Vec::new();
	let mut capture = 0;
	let mut rest = pattern;
	while let Some(open) = rest.find(SELECT_OPEN) {
		append_plain(&rest[..open], &mut match_text, &mut desired, &mut capture);
		let after = &rest[open + SELECT_OPEN.len()..];
		let Some(close) = after.find(SELECT_CLOSE) else {
			return Err(SloppyError::Malformed { operation, reason: "selection is missing ⟫" });
		};
		let selection = &after[..close];
		if let Some((old, new)) = selection.split_once(SELECT_DIVIDER) {
			let (old, new) = if new.contains(SELECT_DIVIDER) {
				let (old, new, note) = resolve_literal_dividers(selection, operation);
				notes.push(note);
				(old, new)
			} else {
				(old.to_owned(), new.to_owned())
			};
			let local = append_match(&old, &mut match_text, &mut capture);
			desired.extend(pieces_from_rewrite(&new, local.len(), Some(&local), operation)?);
		} else if bare_as_desired && !selection.is_empty() {
			match_text.push_str(GAP);
			desired.push(Piece::Text(selection.to_owned()));
			capture += 1;
		} else {
			return Err(SloppyError::Malformed {
				operation,
				reason: "rewrite-less bare selection must contain desired text",
			});
		}
		rest = &after[close + SELECT_CLOSE.len()..];
	}
	append_plain(rest, &mut match_text, &mut desired, &mut capture);
	Ok((match_text, coalesce_text(desired), notes))
}

fn legacy_selection_template(
	pattern: &str,
	rewrite: &str,
	operation: usize,
) -> Result<(String, Vec<Piece>), SloppyError> {
	let Some(open) = pattern.find(SELECT_OPEN) else {
		return Err(SloppyError::Malformed { operation, reason: "selection is missing ⟪" });
	};
	let after = &pattern[open + SELECT_OPEN.len()..];
	let Some(close) = after.find(SELECT_CLOSE) else {
		return Err(SloppyError::Malformed { operation, reason: "selection is missing ⟫" });
	};
	let old = &after[..close];
	if old.contains(SELECT_DIVIDER) || after[close + SELECT_CLOSE.len()..].contains(SELECT_OPEN) {
		return Err(SloppyError::Malformed {
			operation,
			reason: "block rewrite may accompany exactly one bare ⟪current⟫ selection",
		});
	}
	let before = &pattern[..open];
	let after_selection = &after[close + SELECT_CLOSE.len()..];
	let mut match_text = String::new();
	let mut desired = Vec::new();
	let mut capture = 0;
	append_plain(before, &mut match_text, &mut desired, &mut capture);
	let local = append_match(old, &mut match_text, &mut capture);
	desired.extend(pieces_from_rewrite(rewrite, local.len(), Some(&local), operation)?);
	append_plain(after_selection, &mut match_text, &mut desired, &mut capture);
	Ok((match_text, coalesce_text(desired)))
}

fn append_plain(
	text: &str,
	match_text: &mut String,
	desired: &mut Vec<Piece>,
	capture: &mut usize,
) {
	let mut rest = text;
	while let Some(gap) = rest.find(GAP) {
		let literal = &rest[..gap];
		match_text.push_str(literal);
		match_text.push_str(GAP);
		push_text(desired, literal);
		desired.push(Piece::Capture(*capture));
		*capture += 1;
		rest = &rest[gap + GAP.len()..];
	}
	match_text.push_str(rest);
	push_text(desired, rest);
}

fn append_match(text: &str, match_text: &mut String, capture: &mut usize) -> Vec<usize> {
	let mut local = Vec::new();
	let mut rest = text;
	while let Some(gap) = rest.find(GAP) {
		match_text.push_str(&rest[..gap]);
		match_text.push_str(GAP);
		local.push(*capture);
		*capture += 1;
		rest = &rest[gap + GAP.len()..];
	}
	match_text.push_str(rest);
	local
}

fn pieces_from_rewrite(
	rewrite: &str,
	captures: usize,
	mapping: Option<&[usize]>,
	operation: usize,
) -> Result<Vec<Piece>, SloppyError> {
	let mut pieces = Vec::new();
	let mut capture = 0;
	let mut rest = rewrite;
	let mut consumed = 0;
	while let Some(gap) = rest.find(GAP) {
		push_text(&mut pieces, &rest[..gap]);
		let mapped = mapping
			.and_then(|mapping| mapping.get(capture).copied())
			.or_else(|| (capture < captures).then_some(capture));
		if let Some(index) = mapped {
			let absolute = consumed + gap;
			let line_end = rewrite[absolute..]
				.find('\n')
				.map_or(rewrite.len(), |at| absolute + at);
			if rewrite[absolute + GAP.len()..line_end].trim().is_empty() {
				pieces.push(Piece::Capture(index));
			} else {
				pieces.push(Piece::CaptureOrGap(index));
			}
		} else {
			let absolute = consumed + gap;
			let line_start = rewrite[..absolute].rfind('\n').map_or(0, |at| at + 1);
			let line_end = rewrite[absolute..]
				.find('\n')
				.map_or(rewrite.len(), |at| absolute + at);
			if rewrite[line_start..line_end].trim() == GAP {
				return Err(SloppyError::Malformed {
					operation,
					reason: "REWRITE has a whole-line … with no MATCH gap to re-emit; type the elided \
					         lines out",
				});
			}
			push_text(&mut pieces, GAP);
		}
		capture += 1;
		let advance = gap + GAP.len();
		consumed += advance;
		rest = &rest[advance..];
	}
	push_text(&mut pieces, rest);
	Ok(coalesce_text(pieces))
}

fn push_text(pieces: &mut Vec<Piece>, text: &str) {
	if text.is_empty() {
		return;
	}
	if let Some(Piece::Text(previous)) = pieces.last_mut() {
		previous.push_str(text);
	} else {
		pieces.push(Piece::Text(text.to_owned()));
	}
}

fn coalesce_text(pieces: Vec<Piece>) -> Vec<Piece> {
	let mut result = Vec::with_capacity(pieces.len());
	for piece in pieces {
		match piece {
			Piece::Text(text) => push_text(&mut result, &text),
			Piece::Capture(index) => result.push(Piece::Capture(index)),
			Piece::CaptureOrGap(index) => result.push(Piece::CaptureOrGap(index)),
		}
	}
	result
}

fn pieces_source(pieces: &[Piece]) -> String {
	let mut output = String::new();
	for piece in pieces {
		match piece {
			Piece::Text(text) => output.push_str(text),
			Piece::Capture(_) | Piece::CaptureOrGap(_) => output.push_str(GAP),
		}
	}
	output
}

fn selection_desired_sides(pattern: &str) -> Option<String> {
	let mut sides = Vec::new();
	let mut rest = pattern;
	while let Some(open) = rest.find(SELECT_OPEN) {
		let after = &rest[open + SELECT_OPEN.len()..];
		let close = after.find(SELECT_CLOSE)?;
		let selection = &after[..close];
		if let Some((_, new)) = selection.split_once(SELECT_DIVIDER) {
			sides.push(new);
		}
		rest = &after[close + SELECT_CLOSE.len()..];
	}
	(!sides.is_empty()).then(|| sides.join("\n"))
}

fn normalize_text(text: &str) -> String {
	text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn gap_count(text: &str) -> usize {
	text.matches(GAP).count()
}

#[derive(Clone, Debug)]
struct CompiledPattern {
	literals: Vec<String>,
	bounded:  Vec<bool>,
}

fn compile_pattern(text: &str) -> CompiledPattern {
	let mut literals = Vec::new();
	let mut bounded = Vec::new();
	let mut rest = text;
	while let Some(gap) = rest.find(GAP) {
		literals.push(rest[..gap].to_owned());
		let after = &rest[gap + GAP.len()..];
		bounded.push(!after.starts_with('\n') && !after.is_empty());
		rest = after;
	}
	literals.push(rest.to_owned());
	CompiledPattern { literals, bounded }
}

#[derive(Clone, Debug)]
struct Candidate {
	start:    usize,
	end:      usize,
	captures: Vec<String>,
}

#[derive(Clone, Debug)]
struct PlannedEdit {
	start:       usize,
	end:         usize,
	replacement: String,
	operation:   usize,
}

/// Atomic sloppy application result with non-fatal parser recovery notes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SloppyApply {
	/// Final source bytes.
	pub content: String,
	/// Recovery decisions in authored operation order.
	pub notes:   Vec<Str>,
}

/// Applies one section atomically. Operations address the original source;
/// errors never expose a partially applied result.
pub fn apply_sloppy(source: &str, input: &str) -> Result<String, SloppyError> {
	Ok(apply_sloppy_detailed(source, input, None)?.content)
}

/// Applies one path-aware section atomically and returns recovery notes.
pub fn apply_sloppy_detailed(
	source: &str,
	input: &str,
	path: Option<&str>,
) -> Result<SloppyApply, SloppyError> {
	let operations = parse_operations(input, source, path)?;
	let mut planned = Vec::<PlannedEdit>::new();
	let mut queue = (0..operations.len()).collect::<VecDeque<_>>();
	let mut deferred = vec![false; operations.len()];

	while let Some(index) = queue.pop_front() {
		let operation_number = index + 1;
		let exclusions = deferred[index].then(|| {
			planned
				.iter()
				.map(|edit| (edit.start, edit.end))
				.collect::<Vec<_>>()
		});
		match plan_operation(source, &operations[index], operation_number, exclusions.as_deref()) {
			Ok(mut edits) => planned.append(&mut edits),
			Err(SloppyError::Ambiguous { .. }) if !deferred[index] => {
				deferred[index] = true;
				queue.push_back(index);
			},
			Err(error) => return Err(error),
		}
	}

	let mut ordered = reconcile_edits(source, planned)?;
	ordered.sort_by_key(|edit| (edit.start, edit.end));
	let mut output = source.to_owned();
	for edit in ordered.into_iter().rev() {
		output.replace_range(edit.start..edit.end, &edit.replacement);
	}
	if output == source {
		return Err(SloppyError::NoChange { operation: operations.len() });
	}
	let notes = operations
		.into_iter()
		.filter_map(|operation| operation.recovery_note)
		.collect();
	Ok(SloppyApply { content: decode_literal_markers(&output), notes })
}

fn plan_operation(
	source: &str,
	operation: &Operation,
	operation_number: usize,
	exclusions: Option<&[(usize, usize)]>,
) -> Result<Vec<PlannedEdit>, SloppyError> {
	let pattern = compile_pattern(&operation.match_text);
	let mut candidates = locate(source, &pattern);
	if candidates.is_empty() && !operation.has_add && operation.block_rewrite.is_some() {
		candidates = locate_fuzzy_lines(source, &operation.match_text);
	}
	if candidates.is_empty()
		&& let Some(edits) = recover_non_consecutive(source, operation, operation_number)
	{
		return edits;
	}
	if candidates.is_empty() {
		return Err(SloppyError::NoMatch { operation: operation_number });
	}
	if let Some(exclusions) = exclusions
		&& candidates.len() > 1
	{
		let free = candidates
			.iter()
			.filter(|candidate| {
				!exclusions
					.iter()
					.any(|(start, end)| candidate.start < *end && *start < candidate.end)
			})
			.cloned()
			.collect::<Vec<_>>();
		if !free.is_empty() {
			candidates = free;
		}
	}
	let selected = if operation.all {
		candidates
	} else if candidates.len() == 1 {
		candidates
	} else {
		return Err(SloppyError::Ambiguous {
			operation: operation_number,
			lines:     candidates
				.iter()
				.map(|candidate| line_at(source, candidate.start))
				.collect(),
		});
	};

	let mut edits = Vec::new();
	for candidate in selected {
		let replacement = render_pieces(&operation.desired, &candidate.captures);
		if replacement == source[candidate.start..candidate.end] {
			if operation.all {
				continue;
			}
			return Err(SloppyError::NoChange { operation: operation_number });
		}
		let mut edit = minimal_edit(source, &candidate, &replacement, operation_number);
		if edit.replacement.is_empty()
			&& edit.start == line_start(source, edit.start)
			&& edit.end == line_end(source, edit.end)
			&& source.as_bytes().get(edit.end) == Some(&b'\n')
		{
			edit.end += 1;
		}
		edits.push(edit);
	}
	if edits.is_empty() {
		return Err(SloppyError::NoChange { operation: operation_number });
	}
	edits.sort_by_key(|edit| (edit.start, edit.end));
	if edits.windows(2).any(|pair| pair[0].end > pair[1].start) {
		return Err(SloppyError::Overlap { operation: operation_number });
	}
	Ok(edits)
}

fn render_pieces(pieces: &[Piece], captures: &[String]) -> String {
	let mut output = String::new();
	for piece in pieces {
		match piece {
			Piece::Text(text) => output.push_str(text),
			Piece::Capture(index) => {
				if let Some(capture) = captures.get(*index) {
					output.push_str(capture);
				} else {
					output.push_str(GAP);
				}
			},
			Piece::CaptureOrGap(index) => {
				if let Some(capture) = captures.get(*index)
					&& !capture.contains('\n')
				{
					output.push_str(capture);
				} else {
					output.push_str(GAP);
				}
			},
		}
	}
	decode_literal_markers(&output)
}

fn minimal_edit(
	source: &str,
	candidate: &Candidate,
	replacement: &str,
	operation: usize,
) -> PlannedEdit {
	let matched = &source[candidate.start..candidate.end];
	let prefix = common_prefix(matched, replacement);
	let suffix = common_suffix(&matched[prefix..], &replacement[prefix..]);
	PlannedEdit {
		start: candidate.start + prefix,
		end: candidate.end - suffix,
		replacement: replacement[prefix..replacement.len() - suffix].to_owned(),
		operation,
	}
}

fn common_prefix(left: &str, right: &str) -> usize {
	left
		.chars()
		.zip(right.chars())
		.take_while(|(left, right)| left == right)
		.map(|(character, _)| character.len_utf8())
		.sum()
}

fn common_suffix(left: &str, right: &str) -> usize {
	left
		.chars()
		.rev()
		.zip(right.chars().rev())
		.take_while(|(left, right)| left == right)
		.map(|(character, _)| character.len_utf8())
		.sum()
}

fn locate(content: &str, pattern: &CompiledPattern) -> Vec<Candidate> {
	let nonempty = pattern
		.literals
		.iter()
		.enumerate()
		.filter(|(_, literal)| !literal.is_empty())
		.collect::<Vec<_>>();
	if nonempty.is_empty() {
		return Vec::new();
	}
	if pattern.bounded.is_empty() {
		return content
			.match_indices(nonempty[0].1.as_str())
			.take(MAX_CANDIDATES)
			.map(|(start, literal)| Candidate {
				start,
				end: start + literal.len(),
				captures: Vec::new(),
			})
			.collect();
	}

	let mut candidates = Vec::new();
	let (first_index, first_literal) = nonempty[0];
	for (first_start, _) in content.match_indices(first_literal.as_str()) {
		let mut positions = vec![None; pattern.literals.len()];
		positions[first_index] = Some((first_start, first_start + first_literal.len()));
		locate_after(
			content,
			pattern,
			&nonempty,
			1,
			first_start + first_literal.len(),
			&mut positions,
			&mut candidates,
		);
		if candidates.len() >= MAX_CANDIDATES {
			break;
		}
	}
	candidates
}

fn locate_after(
	content: &str,
	pattern: &CompiledPattern,
	nonempty: &[(usize, &String)],
	at: usize,
	cursor: usize,
	positions: &mut [Option<(usize, usize)>],
	candidates: &mut Vec<Candidate>,
) {
	if candidates.len() >= MAX_CANDIDATES {
		return;
	}
	if at == nonempty.len() {
		if let Some(candidate) = candidate_from_positions(content, pattern, positions) {
			candidates.push(candidate);
		}
		return;
	}
	let (literal_index, literal) = nonempty[at];
	for (relative, _) in content[cursor..].match_indices(literal.as_str()) {
		let start = cursor + relative;
		positions[literal_index] = Some((start, start + literal.len()));
		locate_after(
			content,
			pattern,
			nonempty,
			at + 1,
			start + literal.len(),
			positions,
			candidates,
		);
		positions[literal_index] = None;
		if candidates.len() >= MAX_CANDIDATES {
			break;
		}
	}
}

fn candidate_from_positions(
	content: &str,
	pattern: &CompiledPattern,
	positions: &[Option<(usize, usize)>],
) -> Option<Candidate> {
	let first = positions.iter().flatten().next().copied()?;
	let last = positions.iter().flatten().next_back().copied()?;
	let leading = pattern.literals.first().is_some_and(String::is_empty);
	let trailing = pattern.literals.last().is_some_and(String::is_empty);
	let start = if leading {
		line_start(content, first.0)
	} else {
		first.0
	};
	let end = if trailing {
		line_end(content, last.1)
	} else {
		last.1
	};
	let mut captures = Vec::with_capacity(pattern.bounded.len());
	for gap in 0..pattern.bounded.len() {
		let left = positions[..=gap]
			.iter()
			.rev()
			.flatten()
			.next()
			.map_or(start, |position| position.1);
		let right = positions[gap + 1..]
			.iter()
			.flatten()
			.next()
			.map_or(end, |position| position.0);
		if left > right || (pattern.bounded[gap] && content[left..right].contains('\n')) {
			return None;
		}
		captures.push(content[left..right].to_owned());
	}
	Some(Candidate { start, end, captures })
}

fn locate_fuzzy_lines(content: &str, pattern: &str) -> Vec<Candidate> {
	if pattern.contains(GAP) {
		return Vec::new();
	}
	let expected = pattern.split('\n').collect::<Vec<_>>();
	if expected.is_empty() {
		return Vec::new();
	}
	let actual = source_lines(content);
	let mut candidates = Vec::new();
	for row in 0..=actual.len().saturating_sub(expected.len()) {
		if expected
			.iter()
			.zip(&actual[row..row + expected.len()])
			.all(|(left, right)| fuzzy_line_equal(left, &content[right.0..right.1]))
		{
			candidates.push(Candidate {
				start:    actual[row].0,
				end:      actual[row + expected.len() - 1].1,
				captures: Vec::new(),
			});
		}
	}
	candidates
}

fn fuzzy_line_equal(left: &str, right: &str) -> bool {
	let left = left.trim();
	let right = right.trim();
	left == right
		|| (operator_signature(left) == operator_signature(right) && levenshtein(left, right) <= 2)
}

fn operator_signature(text: &str) -> String {
	text
		.chars()
		.filter(|character| !(character.is_alphanumeric() || *character == '_' || *character == '$'))
		.collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
	let right = right.chars().collect::<Vec<_>>();
	let mut previous = (0..=right.len()).collect::<Vec<_>>();
	for (row, left) in left.chars().enumerate() {
		let mut current = Vec::with_capacity(right.len() + 1);
		current.push(row + 1);
		for (column, right) in right.iter().enumerate() {
			current.push(
				(previous[column + 1] + 1)
					.min(current[column] + 1)
					.min(previous[column] + usize::from(left != *right)),
			);
		}
		previous = current;
	}
	previous[right.len()]
}

fn recover_non_consecutive(
	source: &str,
	operation: &Operation,
	operation_number: usize,
) -> Option<Result<Vec<PlannedEdit>, SloppyError>> {
	let rewrite = operation.block_rewrite.as_ref()?;
	if operation.pattern_raw.contains(GAP) || operation.pattern_raw.contains(SELECT_OPEN) {
		return None;
	}
	let patterns = operation
		.pattern_raw
		.split('\n')
		.filter(|line| !line.trim().is_empty())
		.collect::<Vec<_>>();
	let rewrites = rewrite.split('\n').collect::<Vec<_>>();
	if patterns.len() < 2
		|| patterns.len() != rewrites.len()
		|| rewrites.iter().any(|line| line.trim().is_empty())
	{
		return None;
	}
	let lines = source_lines(source);
	let mut found = Vec::with_capacity(patterns.len());
	let mut from = 0;
	for pattern in patterns {
		let matches = lines[from..]
			.iter()
			.enumerate()
			.filter(|(_, range)| fuzzy_line_equal(pattern, &source[range.0..range.1]))
			.map(|(offset, _)| from + offset)
			.collect::<Vec<_>>();
		let [line] = matches.as_slice() else {
			return None;
		};
		found.push(*line);
		from = line + 1;
	}
	if found.windows(2).all(|pair| pair[1] == pair[0] + 1) {
		return None;
	}
	let edits = found
		.into_iter()
		.zip(rewrites)
		.filter_map(|(line, rewrite)| {
			let (start, end) = lines[line];
			let candidate = Candidate { start, end, captures: Vec::new() };
			(&source[start..end] != rewrite)
				.then(|| minimal_edit(source, &candidate, rewrite, operation_number))
		})
		.collect::<Vec<_>>();
	Some(if edits.is_empty() {
		Err(SloppyError::NoChange { operation: operation_number })
	} else {
		Ok(edits)
	})
}

fn source_lines(content: &str) -> Vec<(usize, usize)> {
	let mut lines = Vec::new();
	let mut start = 0;
	for (index, byte) in content.bytes().enumerate() {
		if byte == b'\n' {
			lines.push((start, index));
			start = index + 1;
		}
	}
	if start < content.len() || content.is_empty() {
		lines.push((start, content.len()));
	}
	lines
}

fn reconcile_edits(
	source: &str,
	mut edits: Vec<PlannedEdit>,
) -> Result<Vec<PlannedEdit>, SloppyError> {
	edits.sort_by_key(|edit| (edit.start, edit.end));
	let mut result = Vec::<PlannedEdit>::new();
	for edit in edits {
		let Some(previous) = result.last().cloned() else {
			result.push(edit);
			continue;
		};
		let overlaps = edit.start < previous.end
			|| (edit.start == previous.start
				&& edit.end == edit.start
				&& previous.end == previous.start);
		if !overlaps {
			result.push(edit);
			continue;
		}
		if previous.start == edit.start
			&& previous.end == edit.end
			&& previous.replacement == edit.replacement
		{
			continue;
		}
		if let Some(merged) = merge_contained_deletion(source, &previous, &edit)
			.or_else(|| merge_contained_deletion(source, &edit, &previous))
		{
			*result.last_mut().expect("present") = merged;
			continue;
		}
		let start = previous.start.min(edit.start);
		let end = previous.end.max(edit.end);
		let left = format!(
			"{}{}{}",
			&source[start..previous.start],
			previous.replacement,
			&source[previous.end..end]
		);
		let right =
			format!("{}{}{}", &source[start..edit.start], edit.replacement, &source[edit.end..end]);
		if left == right {
			*result.last_mut().expect("present") =
				PlannedEdit { start, end, replacement: left, operation: edit.operation };
			continue;
		}
		return Err(SloppyError::Overlap { operation: edit.operation });
	}
	Ok(result)
}

fn merge_contained_deletion(
	source: &str,
	outer: &PlannedEdit,
	inner: &PlannedEdit,
) -> Option<PlannedEdit> {
	if !outer.replacement.is_empty()
		|| inner.replacement.is_empty()
		|| inner.start < outer.start
		|| inner.end > outer.end
	{
		return None;
	}
	let mut replacement = inner.replacement.clone();
	if source.as_bytes().get(outer.end.wrapping_sub(1)) == Some(&b'\n')
		&& !replacement.ends_with('\n')
	{
		replacement.push('\n');
	}
	Some(PlannedEdit { start: outer.start, end: outer.end, replacement, operation: inner.operation })
}

fn line_at(content: &str, offset: usize) -> usize {
	content[..offset.min(content.len())]
		.bytes()
		.filter(|byte| *byte == b'\n')
		.count()
		+ 1
}

fn line_start(content: &str, offset: usize) -> usize {
	content[..offset.min(content.len())]
		.rfind('\n')
		.map_or(0, |index| index + 1)
}

fn line_end(content: &str, offset: usize) -> usize {
	content[offset.min(content.len())..]
		.find('\n')
		.map_or(content.len(), |index| offset.min(content.len()) + index)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn extracts_paths_from_partial_payloads() {
		assert_eq!(sloppy_paths("§src/a.rs\nold⟪old│new⟫\n§\nmore\n§*src/b.rs\npartial"), vec![
			Str::new("src/a.rs"),
			Str::new("src/b.rs")
		]);
	}

	#[test]
	fn splits_native_sections_and_continuations() {
		let sections = split_sloppy_sections(
			"*** Begin Patch\n§a\nx⟪x│y⟫\n§\ny⟪y│z⟫\n§*b\nq⟪q│r⟫\n*** End Patch",
		)
		.expect("sections");
		assert_eq!(sections.len(), 2);
		assert_eq!(sections[0].path, "a");
		assert!(sections[0].input.contains("§\ny⟪y│z⟫"));
		assert!(sections[1].input.starts_with("§*\n"));
	}

	#[test]
	fn inline_replacements_apply_together() {
		let source = "const timeout = 1000;\nconst retries = 3;\n";
		let input = "§\nconst timeout = ⟪1000│5000⟫;\nconst retries = ⟪3│5⟫;";
		assert_eq!(
			apply_sloppy(source, input).expect("inline"),
			"const timeout = 5000;\nconst retries = 5;\n"
		);
	}

	#[test]
	fn all_opener_rewrites_every_match() {
		assert_eq!(apply_sloppy("x\nx\n", "§*\n⟪x│y⟫").expect("all"), "y\ny\n");
	}

	#[test]
	fn add_lines_keep_order_and_absolute_indent() {
		let source = "interface P {\n\tlimit: number;\n\tjitter: boolean;\n}\n";
		let input =
			"§\n\tlimit: number;\n＋\tcomment: string;\n＋\tdelay: number;\n\tjitter: boolean;";
		assert_eq!(
			apply_sloppy(source, input).expect("add"),
			"interface P {\n\tlimit: number;\n\tcomment: string;\n\tdelay: number;\n\tjitter: \
			 boolean;\n}\n"
		);
	}

	#[test]
	fn add_line_after_anchor_snaps_to_a_new_line_at_eof() {
		assert_eq!(
			apply_sloppy("alpha = 1\nomega = 2", "§\nomega = 2\n＋zeta = 3").expect("add"),
			"alpha = 1\nomega = 2\nzeta = 3"
		);
		assert_eq!(
			apply_sloppy("omega = 2\n", "§\nomega = 2\n＋zeta = 3").expect("add"),
			"omega = 2\nzeta = 3\n"
		);
	}

	#[test]
	fn add_line_before_anchor_does_not_flatten_it() {
		assert_eq!(
			apply_sloppy("fn x() {\n\tlast();\n}\n", "§\n＋\tfirst();\n\tlast();").expect("add"),
			"fn x() {\n\tfirst();\n\tlast();\n}\n"
		);
	}

	#[test]
	fn whitespace_only_match_rows_around_add_lines_are_normalized() {
		let source = "fn run() {\n  start();\n\n  end();\n}\n";
		let input = "§\n  start();\n\n＋  inserted();\n \n  end();";
		assert_eq!(
			apply_sloppy(source, input).expect("add around blanks"),
			"fn run() {\n  start();\n  inserted();\n\n  end();\n}\n"
		);
	}

	#[test]
	fn add_lines_mix_with_inline_replacements() {
		assert_eq!(
			apply_sloppy(
				"const retries = 3;\nrun();\n",
				"§\nconst retries = ⟪3│5⟫;\n＋const backoff = 250;"
			)
			.expect("mixed add"),
			"const retries = 5;\nconst backoff = 250;\nrun();\n"
		);
	}

	#[test]
	fn block_rewrite_is_verbatim() {
		let source = "fn outer() {\n  old();\n}\n";
		let input = "§\n  old();\n»\nnext();";
		assert_eq!(apply_sloppy(source, input).expect("rewrite"), "fn outer() {\nnext();\n}\n");
	}

	#[test]
	fn markerless_desired_text_repairs_the_closest_block() {
		assert_eq!(
			apply_sloppy("if (!entry)\n  fail();\n", "§\nif (entry)\n  fail();")
				.expect("closest repair"),
			"if (entry)\n  fail();\n"
		);
	}

	#[test]
	fn mixed_redundant_rewrite_is_dropped() {
		assert_eq!(
			apply_sloppy("const x = old;\n", "§\nconst x = ⟪old│new⟫;\n»\nconst x = new;")
				.expect("mixed"),
			"const x = new;\n"
		);
	}

	#[test]
	fn mixed_diverging_rewrite_is_final_text() {
		assert_eq!(
			apply_sloppy(
				"const x = old;\nkeep();\n",
				"§\nconst x = ⟪old│new⟫;\n»\nconst x = new; // updated\nconst y = 1;"
			)
			.expect("mixed"),
			"const x = new; // updated\nconst y = 1;\nkeep();\n"
		);
	}

	#[test]
	fn glued_opener_separator_recovers_after_match_and_drops_elsewhere() {
		assert_eq!(
			apply_sloppy("const x = old;\n", "§\nconst x = old;\n§»\nconst x = new;").expect("glued"),
			"const x = new;\n"
		);
		assert_eq!(apply_sloppy("x\n", "§\nx\n»\ny\n§»").expect("stray"), "y\n");
	}

	#[test]
	fn bare_selection_is_desired_text_without_a_rewrite() {
		assert_eq!(
			apply_sloppy("for (let i = 0; i < 9; i++) {\n", "§\nfor (let i = 0; i < 9; i⟪--⟫) {")
				.expect("bare desired"),
			"for (let i = 0; i < 9; i--) {\n"
		);
	}

	#[test]
	fn legacy_bare_selection_with_rewrite_replaces_only_selection() {
		assert_eq!(
			apply_sloppy(
				"const timeout = config.timeout ?? 1000;\nrun();\n",
				"§\ntimeout = …⟪1000⟫…\nrun()\n»\n5000"
			)
			.expect("legacy"),
			"const timeout = config.timeout ?? 5000;\nrun();\n"
		);
	}

	#[test]
	fn non_consecutive_positional_rewrite_preserves_skipped_rows() {
		let source = "const entries = source\n  ? avlue\n  : typeof value === 'object' &&\n      \
		              Array.isArray(value.models)\n    ? avlue.models\n    : typeof avlue === \
		              'object'\n";
		let input = "§\n  ? avlue\n    ? avlue.models\n    : typeof avlue === 'object'\n»\n  ? \
		             value\n    ? value.models\n    : typeof value === 'object'";
		assert_eq!(
			apply_sloppy(source, input).expect("non-consecutive"),
			source.replace("avlue", "value")
		);
	}

	#[test]
	fn ambiguous_operation_defers_until_sibling_claims_a_span() {
		let source = "left old\nright old\n";
		let input = "§\n⟪old│new⟫\n§\nleft ⟪old│kept⟫";
		assert_eq!(apply_sloppy(source, input).expect("deferred"), "left kept\nright new\n");
	}

	#[test]
	fn operations_address_original_source() {
		let source = "alpha old\nbeta old\n";
		let input = "§\nalpha ⟪old│new⟫\n§\nbeta ⟪old│new⟫";
		assert_eq!(apply_sloppy(source, input).expect("atomic"), "alpha new\nbeta new\n");
	}
	#[test]
	fn fails_closed_on_unclaimed_whole_line_rewrite_gap() {
		let error = apply_sloppy("use a;\nfn f() {}\n", "§\nuse a;\n»\nuse b;\n…\nfn f() {}")
			.expect_err("stray rewrite gap must fail");
		assert!(error.to_string().contains("whole-line …"));
	}

	#[test]
	fn retry_payload_preserves_prior_operations_and_path() {
		let error = apply_sloppy_detailed(
			"const a = 1;\nkeep();\n",
			"§\nconst a = ⟪1│2⟫;\n§\nkeep();",
			Some("src/a.rs"),
		)
		.expect_err("missing rewrite must return skeleton");
		let message = error.to_string();
		assert!(message.contains("§src/a.rs\nconst a = ⟪1│2⟫;\n§\nkeep();\n»\n<new text>"));
		assert_eq!(message.matches("Copy-ready corrected payload").count(), 1);
	}

	#[test]
	fn literal_box_dividers_and_marker_add_lines_are_recovered() {
		let applied = apply_sloppy_detailed(
			"row(\"│ a │\", x);\nrun();\ndone();\n",
			"§\nrow(⟪\"│ a │\", x│\"│ b │\", y⟫);\n§\nrun();\n＋const sel = '⟪a│b⟫';",
			Some("box.ts"),
		)
		.expect("ambiguous box selection has deterministic recovery");
		assert_eq!(applied.content, "row(\"│ b │\", y);\nrun();\nconst sel = '⟪a│b⟫';\ndone();\n");
		assert!(applied.notes.iter().any(|note| note.contains("middle")));
	}

	#[test]
	fn add_run_above_gap_stays_below_its_preceding_anchor() {
		let source = "use std::{\n\tfs,\n\titer,\n};\n\nfn main() {}\n";
		let input = "§\n\tfs,\n＋\tio,\n…\nfn main() {}";
		assert_eq!(
			apply_sloppy(source, input).expect("gap anchored add"),
			"use std::{\n\tfs,\n\tio,\n\titer,\n};\n\nfn main() {}\n"
		);
	}

	#[test]
	fn closest_desired_block_requires_a_decisive_margin() {
		let applied = apply_sloppy_detailed(
			"    if (!entry_row)\n      fail();\n",
			"§\n    if (entry_row)\n      fail();",
			Some("x.ts"),
		)
		.expect("one close block");
		assert_eq!(applied.content, "    if (entry_row)\n      fail();\n");
		assert!(
			applied
				.notes
				.iter()
				.any(|note| note.contains("closest matching block"))
		);

		let ambiguous = "if (!left_entry) { fail(); }\nif (!right_entry) { fail(); }\n";
		assert!(matches!(
			apply_sloppy(ambiguous, "§\nif (entry) { fail(); }"),
			Err(SloppyError::Retry { .. })
		));
	}

	#[test]
	fn echoed_line_rewrite_expands_the_bare_selection() {
		let applied = apply_sloppy_detailed(
			"  screen = [y -> Blank],  \\* viewport\nnext();\n",
			"§\n  screen = [y -> ⟪Blank⟫],  \\* viewport\n»\n  screen = [y -> IF y = 1 THEN Row ELSE \
			 Blank],  \\* row one",
			Some("spec.tla"),
		)
		.expect("echoed line");
		assert_eq!(
			applied.content,
			"  screen = [y -> IF y = 1 THEN Row ELSE Blank],  \\* row one\nnext();\n"
		);
	}

	#[test]
	fn midline_rewrite_ellipsis_does_not_consume_a_multiline_capture() {
		let source = "function f() {\n  a();\n  b();\n}\n";
		let input = "§\nfunction f() {\n…\n}\n»\nfunction f() {\n  return `${x}[… ]${y}`;\n}";
		assert_eq!(
			apply_sloppy(source, input).expect("literal midline ellipsis"),
			"function f() {\n  return `${x}[… ]${y}`;\n}\n"
		);
	}
}
