//! Exact stale hash rejection diagnostics.

use std::{
	error,
	fmt::{self, Display},
};

use omp_core::{Str, sf};

/// Lines of context displayed on either side of a rejected anchor.
pub const MISMATCH_CONTEXT: usize = 2;

/// A stale tag was recovered from a snapshot recorded before external drift.
pub const RECOVERY_EXTERNAL_WARNING: &str = "Recovered from a stale file hash using a previous \
                                             read snapshot (file changed externally between read \
                                             and edit).";
/// A stale tag was recovered through an earlier edit in this session.
pub const RECOVERY_SESSION_CHAIN_WARNING: &str = "Recovered from a stale file hash using an \
                                                  earlier in-session snapshot (a prior edit in \
                                                  this session advanced the hash).";
/// Stale line anchors were proven to map to unchanged live rows.
pub const RECOVERY_LINE_REMAP_WARNING: &str = "Recovered by remapping stale line anchors to \
                                               unchanged current lines (file changed since the \
                                               tagged read). Verify the diff matches your intent.";
/// A content-independent head/tail insertion landed despite stale content.
pub const HEADTAIL_DRIFT_WARNING: &str =
	"Applied the `PUT <1:`/`PUT >$:` edit despite a stale snapshot tag (file changed since your \
	 read) — head/tail position is content-independent. Re-read if the drift was unexpected.";

/// Facts needed to explain an unrecoverable stale snapshot tag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MismatchDetails {
	/// Authored section path, when available.
	pub path:               Option<Str>,
	/// Snapshot tag carried by the authored section.
	pub expected_file_hash: Str,
	/// Hash of the current file content.
	pub actual_file_hash:   Str,
	/// Current file rows without line terminators.
	pub file_lines:         Vec<Str>,
	/// One-indexed source anchors named by the patch.
	pub anchor_lines:       Vec<usize>,
	/// Whether the expected tag names a snapshot retained by this session.
	pub hash_recognized:    bool,
}

/// An unrecoverable mismatch between an authored snapshot and live content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MismatchError {
	details: MismatchDetails,
	message: Str,
}

impl MismatchError {
	/// Builds the exact model-facing rejection message.
	pub fn new(details: MismatchDetails) -> Self {
		let message = format_mismatch(&details);
		Self { details, message: message.into() }
	}

	/// Returns all structured mismatch facts.
	pub const fn details(&self) -> &MismatchDetails {
		&self.details
	}

	/// Returns the exact model-facing rejection text.
	pub fn display_message(&self) -> &str {
		&self.message
	}
}

impl Display for MismatchError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.message)
	}
}

impl error::Error for MismatchError {}

/// Formats numbered current-file context around stale anchors.
pub fn format_anchored_context(anchor_lines: &[usize], file_lines: &[Str]) -> Vec<Str> {
	let mut display_lines = Vec::new();
	for &line in anchor_lines {
		if line == 0 || line > file_lines.len() {
			continue;
		}
		let start = line.saturating_sub(MISMATCH_CONTEXT).max(1);
		let end = line.saturating_add(MISMATCH_CONTEXT).min(file_lines.len());
		display_lines.extend(start..=end);
	}
	display_lines.sort_unstable();
	display_lines.dedup();

	let mut rows = Vec::with_capacity(display_lines.len());
	let mut previous = None;
	for line in display_lines {
		if previous.is_some_and(|prior| line > prior + 1) {
			rows.push(sf!("..."));
		}
		previous = Some(line);
		let marker = if anchor_lines.contains(&line) {
			'*'
		} else {
			' '
		};
		rows.push(sf!("{marker}{line}:{}", file_lines[line - 1]));
	}
	rows
}

fn format_mismatch(details: &MismatchDetails) -> String {
	let path = details
		.path
		.as_deref()
		.map_or_else(String::new, |path| format!(" for {path}"));
	let mut lines = if details.hash_recognized {
		vec![
			format!("Edit rejected{path}: file changed between read and edit."),
			format!(
				"Section is bound to #{}, but the current file hashes to #{}. If a prior edit in this \
				 session modified this file, copy the [path#newhash] header from that edit's \
				 response; otherwise re-read the file with `read` to refresh the tag before retrying.",
				details.expected_file_hash, details.actual_file_hash
			),
		]
	} else {
		vec![
			format!(
				"Edit rejected{path}: hash #{} is not from this session.",
				details.expected_file_hash
			),
			format!(
				"The current file hashes to #{}. Re-read the file with `read` to copy a current \
				 [path#tag] header — never invent the tag and never reuse one from a prior session.",
				details.actual_file_hash
			),
		]
	};
	let context = format_anchored_context(&details.anchor_lines, &details.file_lines);
	if !context.is_empty() {
		lines.push(String::new());
		lines.extend(context.into_iter().map(String::from));
	}
	lines.join("\n")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn details(recognized: bool) -> MismatchDetails {
		MismatchDetails {
			path:               Some("src/a.rs".into()),
			expected_file_hash: "1A2B".into(),
			actual_file_hash:   "C3D4".into(),
			file_lines:         (1..=10).map(|line| format!("L{line}").into()).collect(),
			anchor_lines:       vec![2, 8],
			hash_recognized:    recognized,
		}
	}

	#[test]
	fn recognized_stale_tag_matches_pi_with_context() {
		assert_eq!(
			MismatchError::new(details(true)).to_string(),
			"Edit rejected for src/a.rs: file changed between read and edit.\nSection is bound to \
			 #1A2B, but the current file hashes to #C3D4. If a prior edit in this session modified \
			 this file, copy the [path#newhash] header from that edit's response; otherwise re-read \
			 the file with `read` to refresh the tag before retrying.\n\n 1:L1\n*2:L2\n 3:L3\n \
			 4:L4\n...\n 6:L6\n 7:L7\n*8:L8\n 9:L9\n 10:L10"
		);
	}

	#[test]
	fn unknown_tag_matches_pi_and_requires_a_fresh_read() {
		let mut details = details(false);
		details.anchor_lines.clear();
		assert_eq!(
			MismatchError::new(details).display_message(),
			"Edit rejected for src/a.rs: hash #1A2B is not from this session.\nThe current file \
			 hashes to #C3D4. Re-read the file with `read` to copy a current [path#tag] header — \
			 never invent the tag and never reuse one from a prior session."
		);
	}
}
