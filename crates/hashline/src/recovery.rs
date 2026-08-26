//! Conservative exact-byte recovery of edits authored against retained
//! snapshots.

use std::{collections::BTreeMap, ops};

use bytes::Bytes;
use similar::{Algorithm, DiffOp, capture_diff_slices};
use smallvec::SmallVec;

use crate::snapshots::{RevisionToken, SnapshotLookupError, SnapshotStore};

/// A half-open byte range in one explicitly named coordinate space.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ByteRange {
	start: u64,
	end:   u64,
}

impl ByteRange {
	/// Creates a valid half-open byte range.
	pub const fn new(start: u64, end: u64) -> Result<Self, RecoveryError> {
		if start > end {
			return Err(RecoveryError::ReversedRange { start, end });
		}
		Ok(Self { start, end })
	}

	/// Returns the inclusive start byte offset.
	pub const fn start(self) -> u64 {
		self.start
	}

	/// Returns the exclusive end byte offset.
	pub const fn end(self) -> u64 {
		self.end
	}

	/// Returns whether this range contains no bytes.
	pub const fn is_empty(self) -> bool {
		self.start == self.end
	}
}

/// One exact replacement authored in retained-base byte coordinates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryEdit {
	range:       ByteRange,
	replacement: Bytes,
}

impl RecoveryEdit {
	/// Creates a base-coordinate exact-byte replacement.
	pub const fn new(range: ByteRange, replacement: Bytes) -> Self {
		Self { range, replacement }
	}

	/// Returns the retained-base byte range.
	pub const fn range(&self) -> ByteRange {
		self.range
	}

	/// Returns the exact replacement bytes.
	pub const fn replacement(&self) -> &Bytes {
		&self.replacement
	}
}

/// One canonical exact-byte edit against the live bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactByteEdit {
	range:       ByteRange,
	replacement: Bytes,
}

impl ExactByteEdit {
	/// Returns the live-coordinate byte range.
	pub const fn range(&self) -> ByteRange {
		self.range
	}

	/// Returns the exact replacement bytes.
	pub const fn replacement(&self) -> &Bytes {
		&self.replacement
	}
}

/// A one-indexed inclusive logical-line range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LineRange {
	start: usize,
	end:   usize,
}

impl LineRange {
	/// Returns the first one-indexed line.
	pub const fn start(self) -> usize {
		self.start
	}

	/// Returns the last one-indexed line.
	pub const fn end(self) -> usize {
		self.end
	}
}

/// Coordinate provenance for one recovered authored edit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveredEdit {
	original_range: ByteRange,
	current_range:  ByteRange,
	final_range:    ByteRange,
	original_lines: LineRange,
	current_lines:  LineRange,
}

impl RecoveredEdit {
	/// Returns the authored retained-base byte range.
	pub const fn original_range(&self) -> ByteRange {
		self.original_range
	}

	/// Returns the relocated live pre-edit byte range.
	pub const fn current_range(&self) -> ByteRange {
		self.current_range
	}

	/// Returns the replacement's post-edit byte range.
	pub const fn final_range(&self) -> ByteRange {
		self.final_range
	}

	/// Returns the retained-base logical lines used as anchors.
	pub const fn original_lines(&self) -> LineRange {
		self.original_lines
	}

	/// Returns the uniquely mapped live logical lines.
	pub const fn current_lines(&self) -> LineRange {
		self.current_lines
	}
}

/// A proven one-to-one mapping of an unchanged retained line to a live line.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LineMapping {
	original: usize,
	current:  usize,
}

impl LineMapping {
	/// Returns the retained-base one-indexed line.
	pub const fn original(self) -> usize {
		self.original
	}

	/// Returns the live one-indexed line.
	pub const fn current(self) -> usize {
		self.current
	}
}

/// Successful stale recovery with exact output and all coordinate facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryResult {
	content:         Bytes,
	canonical_edits: Vec<ExactByteEdit>,
	live_edits:      Vec<ExactByteEdit>,
	recovered_edits: Vec<RecoveredEdit>,
	changed_ranges:  Vec<ByteRange>,
	line_mappings:   Vec<LineMapping>,
}

impl RecoveryResult {
	/// Returns the exact finalized bytes, preserving untouched BOM and newline
	/// bytes.
	pub const fn content(&self) -> &Bytes {
		&self.content
	}

	/// Returns canonical live-coordinate edits producing `content`.
	pub fn canonical_edits(&self) -> &[ExactByteEdit] {
		&self.canonical_edits
	}

	/// Returns live-coordinate edits retaining each authored edit's mapped
	/// identity.
	///
	/// Unlike a canonical before/after diff, these ranges cannot migrate among
	/// repeated equal lines. Callers that will apply or rebase the edit must use
	/// this representation.
	pub fn live_edits(&self) -> &[ExactByteEdit] {
		&self.live_edits
	}

	/// Returns authored original/live/final coordinate provenance.
	pub fn recovered_edits(&self) -> &[RecoveredEdit] {
		&self.recovered_edits
	}

	/// Returns changed byte ranges in finalized-output coordinates.
	pub fn changed_ranges(&self) -> &[ByteRange] {
		&self.changed_ranges
	}

	/// Returns every validated retained-to-live line mapping used by recovery.
	pub fn line_mappings(&self) -> &[LineMapping] {
		&self.line_mappings
	}
}

/// A conservative recovery or edit-validation failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum RecoveryError {
	/// Snapshot selection failed or its short tag was ambiguous.
	#[error(transparent)]
	Snapshot(#[from] Box<SnapshotLookupError>),
	/// A half-open byte range has its end before its start.
	#[error("byte range {start}..{end} is reversed; use a start no greater than the end")]
	ReversedRange {
		/// Inclusive byte start supplied by the caller.
		start: u64,
		/// Exclusive byte end supplied by the caller.
		end:   u64,
	},
	/// A half-open byte range extends past the retained content.
	#[error(
		"byte range {start}..{end} exceeds the {length}-byte snapshot; re-read the file and \
		 re-issue the edit against its fresh tag"
	)]
	OutOfBounds {
		/// Inclusive byte start supplied by the caller.
		start:  u64,
		/// Exclusive byte end supplied by the caller.
		end:    u64,
		/// Exact retained content length.
		length: u64,
	},
	/// Authored edits or their live relocations overlap.
	#[error(
		"edits {}..{} and {}..{} overlap; submit non-overlapping ranges in ascending order",
		.previous.start,
		.previous.end,
		.next.start,
		.next.end
	)]
	Overlap {
		/// Earlier conflicting byte range.
		previous: ByteRange,
		/// Later conflicting byte range.
		next:     ByteRange,
	},
	/// A base line was changed or deleted in the live revision.
	#[error(
		"retained line {line} changed in the live revision; re-read the file and re-issue the edit \
		 against its fresh tag"
	)]
	ChangedLine {
		/// One-indexed retained-base line that could not be mapped.
		line: usize,
	},
	/// Equal content admits more than one safe live destination.
	#[error(
		"retained line {line} has {candidates} valid live destinations; include more unchanged \
		 context or re-read the file and re-issue against its fresh tag"
	)]
	AmbiguousLine {
		/// One-indexed retained-base line whose destination is ambiguous.
		line:       usize,
		/// Number of context-valid candidate destinations.
		candidates: usize,
	},
	/// Neighboring unchanged context does not validate a candidate relocation.
	#[error(
		"unchanged neighbors do not validate retained line {line}; re-read the file and re-issue \
		 the edit against its fresh tag"
	)]
	ContextMismatch {
		/// One-indexed retained-base line whose context failed validation.
		line: usize,
	},
	/// An in-bounds byte position could not be assigned to a retained line.
	#[error(
		"byte position {position} cannot be mapped to a retained line; re-read the file and \
		 re-issue the edit against its fresh tag"
	)]
	PositionUnmappable {
		/// Retained-base byte position that could not be mapped.
		position: u64,
	},
	/// Final output size overflowed the platform address space.
	#[error("recovered output is too large for this platform")]
	OutputTooLarge,
}

impl RecoveryError {
	/// Returns the stable machine-readable diagnostic code.
	pub fn code(&self) -> &'static str {
		self.into()
	}
}

/// Resolves a collision-safe retained base and recovers its edits onto live
/// bytes.
pub fn recover_from_store(
	store: &mut SnapshotStore,
	path: &str,
	tag: &str,
	revision: Option<&RevisionToken>,
	current: &Bytes,
	edits: &[RecoveryEdit],
) -> Result<RecoveryResult, RecoveryError> {
	let snapshot = store.resolve(path, tag, revision)?;
	recover_exact(snapshot.bytes(), current, edits)
}

/// Recovers base-coordinate byte edits only through uniquely unchanged live
/// lines.
pub fn recover_exact(
	base: &Bytes,
	current: &Bytes,
	edits: &[RecoveryEdit],
) -> Result<RecoveryResult, RecoveryError> {
	validate_authored_edits(base.len(), edits)?;
	let base_lines = split_lines(base);
	let current_lines = split_lines(current);
	let edit_anchors = collect_edit_anchors(&base_lines, edits)?;

	let line_map = if base == current {
		(0..base_lines.len()).map(|index| (index, index)).collect()
	} else {
		validate_and_map_lines(&base_lines, &current_lines, &edit_anchors)?
	};

	let mut mapped = Vec::with_capacity(edits.len());
	for (edit, anchors) in edits.iter().zip(&edit_anchors) {
		let start = map_position(
			&base_lines,
			&current_lines,
			&line_map,
			edit.range.start,
			PositionBias::Start,
		)?;
		let end_bias = if edit.range.is_empty() {
			PositionBias::Start
		} else {
			PositionBias::End
		};
		let end = map_position(&base_lines, &current_lines, &line_map, edit.range.end, end_bias)?;
		let current_range = ByteRange::new(start, end)?;
		let current_line_start = line_map[anchors.start()];
		let current_line_end = line_map[anchors.end()];
		mapped.push((edit, current_range, anchors.clone(), LineRange {
			start: current_line_start + 1,
			end:   current_line_end + 1,
		}));
	}
	validate_mapped_edits(&mapped)?;

	let output_len = mapped
		.iter()
		.try_fold(current.len(), |length, (edit, range, ..)| {
			let removed =
				usize::try_from(range.end - range.start).map_err(|_| RecoveryError::OutputTooLarge)?;
			length
				.checked_sub(removed)
				.and_then(|remaining| remaining.checked_add(edit.replacement.len()))
				.ok_or(RecoveryError::OutputTooLarge)
		})?;
	let mut output = Vec::with_capacity(output_len);
	let mut cursor = 0usize;
	let mut final_delta: i128 = 0;
	let mut live_edits = Vec::with_capacity(mapped.len());
	let mut recovered_edits = Vec::with_capacity(mapped.len());
	for (edit, range, original_lines, current_lines) in &mapped {
		let start = usize::try_from(range.start).map_err(|_| RecoveryError::OutputTooLarge)?;
		let end = usize::try_from(range.end).map_err(|_| RecoveryError::OutputTooLarge)?;
		output.extend_from_slice(&current[cursor..start]);
		output.extend_from_slice(&edit.replacement);
		cursor = end;
		let final_start = add_signed(range.start, final_delta)?;
		let replacement_len =
			u64::try_from(edit.replacement.len()).map_err(|_| RecoveryError::OutputTooLarge)?;
		let final_end = final_start
			.checked_add(replacement_len)
			.ok_or(RecoveryError::OutputTooLarge)?;
		recovered_edits.push(RecoveredEdit {
			original_range: edit.range,
			current_range:  *range,
			final_range:    ByteRange { start: final_start, end: final_end },
			original_lines: LineRange {
				start: *original_lines.start() + 1,
				end:   *original_lines.end() + 1,
			},
			current_lines:  *current_lines,
		});
		live_edits.push(ExactByteEdit { range: *range, replacement: edit.replacement.clone() });
		final_delta += i128::from(replacement_len) - i128::from(range.end - range.start);
	}
	output.extend_from_slice(&current[cursor..]);
	let content = Bytes::from(output);
	let canonical_edits = canonical_edits(current, &content)?;
	let changed_ranges = changed_ranges(&canonical_edits)?;
	let mut line_mappings = edit_anchors
		.iter()
		.flat_map(|range| range.clone())
		.map(|original| LineMapping { original: original + 1, current: line_map[&original] + 1 })
		.collect::<Vec<_>>();
	line_mappings.sort_unstable_by_key(|mapping| mapping.original);
	line_mappings.dedup();
	Ok(RecoveryResult {
		content,
		canonical_edits,
		live_edits,
		recovered_edits,
		changed_ranges,
		line_mappings,
	})
}

#[derive(Clone, Copy, Debug)]
struct LineRecord<'a> {
	start: usize,
	end:   usize,
	bytes: &'a [u8],
}

fn split_lines(bytes: &[u8]) -> Vec<LineRecord<'_>> {
	let mut lines = Vec::new();
	let mut start = 0;
	for (index, byte) in bytes.iter().enumerate() {
		if *byte == b'\n' {
			let end = index + 1;
			lines.push(LineRecord { start, end, bytes: &bytes[start..end] });
			start = end;
		}
	}
	lines.push(LineRecord { start, end: bytes.len(), bytes: &bytes[start..] });
	lines
}

fn validate_authored_edits(base_len: usize, edits: &[RecoveryEdit]) -> Result<(), RecoveryError> {
	let length = u64::try_from(base_len).map_err(|_| RecoveryError::OutputTooLarge)?;
	let mut previous: Option<ByteRange> = None;
	for edit in edits {
		if edit.range.start > edit.range.end {
			return Err(RecoveryError::ReversedRange {
				start: edit.range.start,
				end:   edit.range.end,
			});
		}
		if edit.range.end > length {
			return Err(RecoveryError::OutOfBounds {
				start: edit.range.start,
				end: edit.range.end,
				length,
			});
		}
		if let Some(prior) = previous
			&& (edit.range.start < prior.end
				|| (edit.range.start == prior.start && (edit.range.is_empty() || prior.is_empty())))
		{
			return Err(RecoveryError::Overlap { previous: prior, next: edit.range });
		}
		previous = Some(edit.range);
	}
	Ok(())
}

fn collect_edit_anchors(
	lines: &[LineRecord<'_>],
	edits: &[RecoveryEdit],
) -> Result<Vec<ops::RangeInclusive<usize>>, RecoveryError> {
	edits
		.iter()
		.map(|edit| {
			let start = line_at(lines, edit.range.start, PositionBias::Start)?;
			let end = if edit.range.is_empty() {
				start
			} else {
				line_at(lines, edit.range.end, PositionBias::End)?
			};
			Ok(start..=end)
		})
		.collect()
}

#[derive(Clone, Copy)]
enum PositionBias {
	Start,
	End,
}

fn line_at(
	lines: &[LineRecord<'_>],
	position: u64,
	bias: PositionBias,
) -> Result<usize, RecoveryError> {
	let position = usize::try_from(position).map_err(|_| RecoveryError::OutputTooLarge)?;
	if position > lines.last().map_or(0, |line| line.end) {
		return Err(RecoveryError::OutOfBounds {
			start:  position as u64,
			end:    position as u64,
			length: lines.last().map_or(0, |line| line.end as u64),
		});
	}
	match bias {
		PositionBias::Start => {
			if position == lines.last().map_or(0, |line| line.end)
				&& lines.last().is_some_and(|line| line.start == line.end)
				&& lines.len() > 1
			{
				return Ok(lines.len() - 2);
			}
			lines
				.iter()
				.position(|line| {
					line.start <= position && position < line.end
						|| line.start == line.end && position == line.start
				})
				.ok_or(RecoveryError::PositionUnmappable { position: position as u64 })
		},
		PositionBias::End => lines
			.iter()
			.rposition(|line| {
				line.start < position && position <= line.end
					|| line.start == line.end && position == line.start
			})
			.ok_or(RecoveryError::PositionUnmappable { position: position as u64 }),
	}
}

fn raw_line_map(base: &[LineRecord<'_>], current: &[LineRecord<'_>]) -> BTreeMap<usize, usize> {
	let base_slices = base.iter().map(|line| line.bytes).collect::<Vec<_>>();
	let current_slices = current.iter().map(|line| line.bytes).collect::<Vec<_>>();
	let mut map = BTreeMap::new();
	for operation in capture_diff_slices(Algorithm::Myers, &base_slices, &current_slices) {
		if let DiffOp::Equal { old_index, new_index, len } = operation {
			for offset in 0..len {
				map.insert(old_index + offset, new_index + offset);
			}
		}
	}
	map
}

fn validate_and_map_lines(
	base: &[LineRecord<'_>],
	current: &[LineRecord<'_>],
	edit_anchors: &[ops::RangeInclusive<usize>],
) -> Result<BTreeMap<usize, usize>, RecoveryError> {
	let raw = raw_line_map(base, current);
	let mut anchors = edit_anchors
		.iter()
		.flat_map(|range| range.clone())
		.collect::<Vec<_>>();
	anchors.sort_unstable();
	anchors.dedup();
	let mut runs = Vec::new();
	for anchor in anchors {
		match runs.last_mut() {
			Some((_, end)) if anchor == *end + 1 => *end = anchor,
			_ => runs.push((anchor, anchor)),
		}
	}

	for (start, end) in runs {
		for line in start..=end {
			if !raw.contains_key(&line) {
				return Err(RecoveryError::ChangedLine { line: line + 1 });
			}
		}
		let expected = raw[&start];
		if (start..=end).any(|line| raw[&line] != expected + line - start) {
			return Err(RecoveryError::ChangedLine { line: start + 1 });
		}
		let width = end - start + 1;
		let mut candidates = SmallVec::<usize, 4>::new();
		if width <= current.len() {
			for candidate in 0..=current.len() - width {
				if (0..width)
					.all(|offset| base[start + offset].bytes == current[candidate + offset].bytes)
				{
					candidates.push(candidate);
				}
			}
		}
		let base_run_count = (0..=base.len() - width)
			.filter(|candidate| {
				(0..width).all(|offset| base[start + offset].bytes == base[*candidate + offset].bytes)
			})
			.count();
		if base_run_count == 1 && candidates.len() == 1 {
			if candidates[0] != expected {
				return Err(RecoveryError::ContextMismatch { line: start + 1 });
			}
			continue;
		}
		let before = start.checked_sub(1);
		let after = (end + 1 < base.len()).then_some(end + 1);
		if before.is_none() && after.is_none() {
			return Err(RecoveryError::ContextMismatch { line: start + 1 });
		}
		let base_candidate_count = (0..=base.len() - width)
			.filter(|candidate| {
				let run_matches = (0..width)
					.all(|offset| base[start + offset].bytes == base[*candidate + offset].bytes);
				let before_matches = before
					.is_none_or(|line| *candidate > 0 && base[line].bytes == base[*candidate - 1].bytes);
				let after_candidate = *candidate + width;
				let after_matches = after.is_none_or(|line| {
					after_candidate < base.len() && base[line].bytes == base[after_candidate].bytes
				});
				run_matches && before_matches && after_matches
			})
			.count();
		if base_candidate_count != 1 {
			return Err(RecoveryError::AmbiguousLine {
				line:       start + 1,
				candidates: base_candidate_count,
			});
		}
		candidates.retain(|candidate| {
			let before_ok = before
				.is_none_or(|line| *candidate > 0 && base[line].bytes == current[*candidate - 1].bytes);
			let after_candidate = *candidate + width;
			let after_ok = after.is_none_or(|line| {
				after_candidate < current.len() && base[line].bytes == current[after_candidate].bytes
			});
			before_ok && after_ok
		});
		if candidates.is_empty() {
			return Err(RecoveryError::ContextMismatch { line: start + 1 });
		}
		if candidates.len() != 1 {
			return Err(RecoveryError::AmbiguousLine {
				line:       start + 1,
				candidates: candidates.len(),
			});
		}
		if candidates[0] != expected {
			return Err(RecoveryError::ContextMismatch { line: start + 1 });
		}
	}
	Ok(raw)
}

fn map_position(
	base: &[LineRecord<'_>],
	current: &[LineRecord<'_>],
	map: &BTreeMap<usize, usize>,
	position: u64,
	bias: PositionBias,
) -> Result<u64, RecoveryError> {
	let line_index = line_at(base, position, bias)?;
	let current_index = *map
		.get(&line_index)
		.ok_or(RecoveryError::ChangedLine { line: line_index + 1 })?;
	if base[line_index].bytes != current[current_index].bytes {
		return Err(RecoveryError::ChangedLine { line: line_index + 1 });
	}
	let position = usize::try_from(position).map_err(|_| RecoveryError::OutputTooLarge)?;
	let offset = position
		.checked_sub(base[line_index].start)
		.ok_or(RecoveryError::ContextMismatch { line: line_index + 1 })?;
	let mapped = current[current_index]
		.start
		.checked_add(offset)
		.ok_or(RecoveryError::OutputTooLarge)?;
	u64::try_from(mapped).map_err(|_| RecoveryError::OutputTooLarge)
}

fn validate_mapped_edits(
	mapped: &[(&RecoveryEdit, ByteRange, ops::RangeInclusive<usize>, LineRange)],
) -> Result<(), RecoveryError> {
	let mut previous: Option<ByteRange> = None;
	for (_, range, ..) in mapped {
		if let Some(prior) = previous
			&& (range.end < prior.end
				|| (range.start == prior.start && (range.is_empty() || prior.is_empty())))
		{
			return Err(RecoveryError::Overlap { previous: prior, next: *range });
		}
		previous = Some(*range);
	}
	Ok(())
}

fn add_signed(value: u64, delta: i128) -> Result<u64, RecoveryError> {
	let adjusted = i128::from(value)
		.checked_add(delta)
		.ok_or(RecoveryError::OutputTooLarge)?;
	u64::try_from(adjusted).map_err(|_| RecoveryError::OutputTooLarge)
}

fn canonical_edits(base: &Bytes, output: &Bytes) -> Result<Vec<ExactByteEdit>, RecoveryError> {
	let mut edits = Vec::new();
	for operation in capture_diff_slices(Algorithm::Myers, &base[..], &output[..]) {
		let (old_start, old_len, new_start, new_len) = match operation {
			DiffOp::Equal { .. } => continue,
			DiffOp::Delete { old_index, old_len, new_index } => (old_index, old_len, new_index, 0),
			DiffOp::Insert { old_index, new_index, new_len } => (old_index, 0, new_index, new_len),
			DiffOp::Replace { old_index, old_len, new_index, new_len } => {
				(old_index, old_len, new_index, new_len)
			},
		};
		let old_end = old_start
			.checked_add(old_len)
			.ok_or(RecoveryError::OutputTooLarge)?;
		let new_end = new_start
			.checked_add(new_len)
			.ok_or(RecoveryError::OutputTooLarge)?;
		edits.push(ExactByteEdit {
			range:       ByteRange {
				start: u64::try_from(old_start).map_err(|_| RecoveryError::OutputTooLarge)?,
				end:   u64::try_from(old_end).map_err(|_| RecoveryError::OutputTooLarge)?,
			},
			replacement: output.slice(new_start..new_end),
		});
	}
	Ok(edits)
}

fn changed_ranges(edits: &[ExactByteEdit]) -> Result<Vec<ByteRange>, RecoveryError> {
	let mut ranges = Vec::with_capacity(edits.len());
	let mut delta = 0i128;
	for edit in edits {
		let start = add_signed(edit.range.start, delta)?;
		let replacement_len =
			u64::try_from(edit.replacement.len()).map_err(|_| RecoveryError::OutputTooLarge)?;
		let end = start
			.checked_add(replacement_len)
			.ok_or(RecoveryError::OutputTooLarge)?;
		ranges.push(ByteRange { start, end });
		delta += i128::from(replacement_len) - i128::from(edit.range.end - edit.range.start);
	}
	Ok(ranges)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::snapshots::{SnapshotStore, compute_snapshot_tag};

	fn edit(start: u64, end: u64, replacement: &'static [u8]) -> RecoveryEdit {
		RecoveryEdit::new(ByteRange::new(start, end).unwrap(), Bytes::from_static(replacement))
	}

	#[test]
	fn remaps_unchanged_line_after_insertion_and_keeps_exact_newlines() {
		let base = Bytes::from_static(b"\xef\xbb\xbfL1\r\nL2\r\nL3\r\nL4\r\n");
		let current = Bytes::from_static(b"\xef\xbb\xbfNEW\r\nL1\r\nL2\r\nL3\r\nL4\r\n");
		let start = base
			.windows(4)
			.position(|window| window == b"L3\r\n")
			.unwrap() as u64;
		let result = recover_exact(&base, &current, &[edit(start, start + 4, b"MODEL\r\n")]).unwrap();
		assert_eq!(
			result.content(),
			&Bytes::from_static(b"\xef\xbb\xbfNEW\r\nL1\r\nL2\r\nMODEL\r\nL4\r\n")
		);
		assert_eq!(result.recovered_edits()[0].original_lines(), LineRange { start: 3, end: 3 });
		assert_eq!(result.recovered_edits()[0].current_lines(), LineRange { start: 4, end: 4 });
		assert_eq!(result.line_mappings(), &[LineMapping { original: 3, current: 4 }]);
	}

	#[test]
	fn remaps_unique_anchor_when_an_adjacent_line_changed() {
		let base = Bytes::from_static(b"fn main() {\n    let value = 1;\n}\n");
		let current = Bytes::from_static(b"fn main() {\n    let VALUE = 2;\n}\n");
		let result =
			recover_exact(&base, &current, &[edit(0, 12, b"fn main() { // concurrent\n")]).unwrap();
		assert_eq!(
			result.content(),
			&Bytes::from_static(b"fn main() { // concurrent\n    let VALUE = 2;\n}\n")
		);
	}

	#[test]
	fn rejects_changed_anchor_line() {
		let base = Bytes::from_static(b"L1\nL2\nL3\n");
		let current = Bytes::from_static(b"L1\nCHANGED\nL3\n");
		assert!(matches!(
			recover_exact(&base, &current, &[edit(3, 6, b"MODEL\n")]),
			Err(RecoveryError::ChangedLine { line: 2 })
		));
	}

	#[test]
	fn rejects_duplicated_target_when_context_is_not_unique() {
		let block = b"head\nTARGET\ntail\n";
		let mut base_vec = Vec::new();
		base_vec.extend_from_slice(block);
		base_vec.extend_from_slice(b"middle\n");
		base_vec.extend_from_slice(block);
		let base = Bytes::from(base_vec.clone());
		let mut current_vec = b"inserted\n".to_vec();
		current_vec.extend_from_slice(&base_vec);
		let current = Bytes::from(current_vec);
		let start = base
			.windows(6)
			.position(|window| window == b"TARGET")
			.unwrap() as u64;
		let failure =
			recover_exact(&base, &current, &[edit(start, start + 6, b"MODEL")]).unwrap_err();
		assert!(matches!(
			failure,
			RecoveryError::ContextMismatch { .. } | RecoveryError::AmbiguousLine { .. }
		));
	}

	#[test]
	fn rejects_overlapping_authored_edits() {
		let base = Bytes::from_static(b"one\ntwo\n");
		let edits = [edit(0, 4, b"ONE\n"), edit(3, 8, b"X")];
		assert!(matches!(recover_exact(&base, &base, &edits), Err(RecoveryError::Overlap { .. })));
	}

	#[test]
	fn collision_requires_explicit_revision_before_recovery() {
		let a = Bytes::from_static(b"line one 263\nline two 4471\n");
		let b = Bytes::from_static(b"line one 410\nline two 6970\n");
		assert_eq!(compute_snapshot_tag(&a), "1D84");
		assert_eq!(compute_snapshot_tag(&b), "1D84");
		let mut store = SnapshotStore::default();
		let ra = RevisionToken::new("a");
		let rb = RevisionToken::new("b");
		store.record("p", ra.clone(), a, [1, 2]).unwrap();
		store.record("p", rb, b, [1, 2]).unwrap();
		let current = Bytes::from_static(b"prefix\nline one 263\nline two 4471\n");
		let authored = [edit(13, 27, b"MODEL\n")];
		assert!(matches!(
			recover_from_store(&mut store, "p", "1D84", None, &current, &authored),
			Err(RecoveryError::Snapshot(error)) if matches!(*error, SnapshotLookupError::Ambiguous { .. })
		));
		let result =
			recover_from_store(&mut store, "p", "1D84", Some(&ra), &current, &authored).unwrap();
		assert_eq!(result.content(), &Bytes::from_static(b"prefix\nline one 263\nMODEL\n"));
	}

	#[test]
	fn reports_canonical_edits_and_final_changed_ranges() {
		let bytes = Bytes::from_static(b"a\nb\nc\n");
		let result = recover_exact(&bytes, &bytes, &[edit(2, 4, b"B-long\n")]).unwrap();
		assert_eq!(result.content(), &Bytes::from_static(b"a\nB-long\nc\n"));
		assert!(!result.canonical_edits().is_empty());
		assert!(!result.changed_ranges().is_empty());
		assert_eq!(result.recovered_edits()[0].current_range(), ByteRange { start: 2, end: 4 });
		assert_eq!(result.recovered_edits()[0].final_range(), ByteRange { start: 2, end: 9 });
	}
	#[test]
	fn live_edits_preserve_authored_duplicate_line_identity() {
		let bytes = Bytes::from_static(b"top\nduplicate\nmiddle\nduplicate\nbottom\n");
		let start = b"top\nduplicate\nmiddle\n".len() as u64;
		let end = start + b"duplicate\n".len() as u64;
		let result = recover_exact(&bytes, &bytes, &[edit(start, end, b"")]).unwrap();
		assert_eq!(result.live_edits().len(), 1);
		assert_eq!(result.live_edits()[0].range(), ByteRange { start, end });
		assert!(result.live_edits()[0].replacement().is_empty());
	}
}
