use std::fmt::Write as _;

use bytes::Bytes;
use omp_core::{Str, sf};
use omp_tool::BlobRef;
use xutf::{Encoding as _, Utf8};

use crate::read::{Fault as ReadFault, ReadBlobs};

/// Default maximum number of rendered output lines.
pub const DEFAULT_MAX_LINES: usize = 3000;
/// Default maximum rendered output size in UTF-8 bytes.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Default maximum number of UTF-16 code units in one rendered line.
pub const DEFAULT_MAX_COLUMN: u32 = 512;

/// Limit that caused a head truncation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TruncatedBy {
	/// The line limit was reached first.
	Lines,
	/// The byte limit was reached first.
	Bytes,
}

/// Limits applied by [`truncate_head`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TruncationOptions {
	/// Maximum number of complete lines to retain.
	pub max_lines: usize,
	/// Maximum number of UTF-8 bytes to retain.
	pub max_bytes: usize,
}

impl Default for TruncationOptions {
	fn default() -> Self {
		Self { max_lines: DEFAULT_MAX_LINES, max_bytes: DEFAULT_MAX_BYTES }
	}
}

/// A borrowed head-truncation result with counts for notices and blob spills.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TruncationResult<'a> {
	/// Complete retained lines from the start of the input.
	pub content:                  &'a str,
	/// Whether any input was omitted.
	pub truncated:                bool,
	/// The limit that caused truncation, when truncated.
	pub truncated_by:             Option<TruncatedBy>,
	/// Number of lines in the original input.
	pub total_lines:              usize,
	/// UTF-8 byte length of the original input.
	pub total_bytes:              usize,
	/// Number of complete lines retained when truncated.
	pub output_lines:             Option<usize>,
	/// UTF-8 byte length retained when truncated.
	pub output_bytes:             Option<usize>,
	/// Whether the retained last line is partial.
	pub last_line_partial:        bool,
	/// Whether no content fit because the first line exceeded the byte limit.
	pub first_line_exceeds_limit: bool,
}

impl TruncationResult<'_> {
	/// Number of lines represented by `content`.
	pub fn shown_lines(&self) -> usize {
		self.output_lines.unwrap_or(self.total_lines)
	}
}

/// Complete pre-projection text after applying the shared output bounds.
///
/// When content was omitted, `blob` retains the content reference and
/// `artifact_uri` is the resolver-valid recovery address named by the footer.
pub struct SpilledText {
	pub content:      Str,
	pub blob:         Option<BlobRef>,
	/// Resolver-valid address of the complete output.
	pub artifact_uri: Option<Str>,
	pub shown_lines:  u64,
	pub total_lines:  u64,
}

/// Applies the standard text bounds and durably stores the complete text before
/// returning a bounded projection.
pub async fn spill_truncated_text<B: ReadBlobs>(
	full_text: String,
	blobs: &B,
) -> Result<SpilledText, ReadFault> {
	let truncation = truncate_head(&full_text, TruncationOptions::default());
	let shown_lines = u64::try_from(truncation.shown_lines()).unwrap_or(u64::MAX);
	let total_lines = u64::try_from(truncation.total_lines).unwrap_or(u64::MAX);
	if !truncation.truncated {
		return Ok(SpilledText {
			content: Str::new(full_text),
			blob: None,
			artifact_uri: None,
			shown_lines,
			total_lines,
		});
	}

	let content = truncation.content.to_owned();
	let bytes = Bytes::from(full_text);
	let artifact = blobs
		.store_artifact(bytes, sf!("text/plain; charset=utf-8"))
		.await?;
	let mut content = content;
	append_blob_truncation_notice_counts(&mut content, shown_lines, total_lines, &artifact.uri);
	Ok(SpilledText {
		content: Str::new(content),
		blob: Some(artifact.blob),
		artifact_uri: Some(artifact.uri),
		shown_lines,
		total_lines,
	})
}

/// A borrowed result from [`truncate_head_bytes`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteTruncationResult<'a> {
	/// Longest valid UTF-8 prefix within the byte limit.
	pub text:  &'a str,
	/// UTF-8 byte length of `text`.
	pub bytes: usize,
}

/// Retains the longest valid UTF-8 prefix no larger than `max_bytes`.
///
/// The returned text borrows the input and never ends inside a UTF-8 scalar.
pub fn truncate_head_bytes(text: &str, max_bytes: usize) -> ByteTruncationResult<'_> {
	if text.len() <= max_bytes {
		return ByteTruncationResult { text, bytes: text.len() };
	}

	let mut rest = text.as_bytes();
	let mut end = 0usize;
	while !rest.is_empty() {
		let mut tail = rest;
		Utf8::decode(&mut tail);
		let decoded_bytes = rest.len() - tail.len();
		if end + decoded_bytes > max_bytes {
			break;
		}
		end += decoded_bytes;
		rest = tail;
	}
	ByteTruncationResult { text: &text[..end], bytes: end }
}

/// Retains complete lines from the head within both line and UTF-8 byte limits.
///
/// No partial line is returned. If the first line exceeds the byte budget,
/// `content` is empty and `first_line_exceeds_limit` is set.
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult<'_> {
	let total_bytes = content.len();
	let total_lines = content.bytes().filter(|byte| *byte == b'\n').count() + 1;

	if total_lines <= options.max_lines && total_bytes <= options.max_bytes {
		return TruncationResult {
			content,
			truncated: false,
			truncated_by: None,
			total_lines,
			total_bytes,
			output_lines: None,
			output_bytes: None,
			last_line_partial: false,
			first_line_exceeds_limit: false,
		};
	}

	let mut included_lines = 0usize;
	let mut bytes_used = 0usize;
	let mut cut_index = 0usize;
	let mut cursor = 0usize;
	let mut truncated_by = TruncatedBy::Lines;

	while included_lines < options.max_lines {
		let newline = content[cursor..].find('\n').map(|offset| cursor + offset);
		let line_end = newline.unwrap_or(content.len());
		let separator_bytes = usize::from(included_lines > 0);
		let Some(remaining) = options
			.max_bytes
			.checked_sub(bytes_used)
			.and_then(|remaining| remaining.checked_sub(separator_bytes))
		else {
			truncated_by = TruncatedBy::Bytes;
			break;
		};

		let line_bytes = line_end - cursor;
		if line_bytes > remaining {
			if included_lines == 0 {
				return TruncationResult {
					content: "",
					truncated: true,
					truncated_by: Some(TruncatedBy::Bytes),
					total_lines,
					total_bytes,
					output_lines: Some(0),
					output_bytes: Some(0),
					last_line_partial: false,
					first_line_exceeds_limit: true,
				};
			}
			truncated_by = TruncatedBy::Bytes;
			break;
		}

		bytes_used += separator_bytes + line_bytes;
		included_lines += 1;
		cut_index = newline.unwrap_or(content.len());
		let Some(newline) = newline else {
			break;
		};
		cursor = newline + 1;
	}

	if included_lines >= options.max_lines && bytes_used <= options.max_bytes {
		truncated_by = TruncatedBy::Lines;
	}

	TruncationResult {
		content: &content[..cut_index],
		truncated: true,
		truncated_by: Some(truncated_by),
		total_lines,
		total_bytes,
		output_lines: Some(included_lines),
		output_bytes: Some(bytes_used),
		last_line_partial: false,
		first_line_exceeds_limit: false,
	}
}

/// Appends pi's read continuation notice when `truncation` omitted content.
pub fn append_head_truncation_notice(
	output: &mut String,
	truncation: &TruncationResult<'_>,
	start_line: usize,
	total_file_lines: Option<usize>,
) {
	if !truncation.truncated {
		return;
	}
	let total_file_lines = total_file_lines.unwrap_or(truncation.total_lines);
	let end_line = start_line
		.saturating_add(truncation.shown_lines())
		.saturating_sub(1);
	let next_offset = end_line + 1;
	let _ = write!(
		output,
		"\n\n[Showing lines {start_line}-{end_line} of {total_file_lines}. Use :{next_offset} to \
		 continue]"
	);
}

/// Appends the exact footer used after spilling the complete output to a
/// session artifact.
pub fn append_blob_truncation_notice(
	output: &mut String,
	truncation: &TruncationResult<'_>,
	artifact_uri: &str,
) {
	if !truncation.truncated {
		return;
	}
	append_blob_truncation_notice_counts(
		output,
		u64::try_from(truncation.shown_lines()).unwrap_or(u64::MAX),
		u64::try_from(truncation.total_lines).unwrap_or(u64::MAX),
		artifact_uri,
	);
}

fn append_blob_truncation_notice_counts(
	output: &mut String,
	shown_lines: u64,
	total_lines: u64,
	artifact_uri: &str,
) {
	let _ = write!(
		output,
		"\n\n[truncated: {shown_lines} of {total_lines} lines shown; read {artifact_uri} for full \
		 output]"
	);
}
