//! Queue and steering shorthand parsing.

use omp_core::Str;
use smallvec::SmallVec;

/// One message extracted from a composer submission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueItem {
	/// Message body without queue syntax.
	pub text:             Str,
	/// Whether `->`/`=>` requested yield/follow-up delivery.
	pub yield_after_turn: bool,
}

/// Byte-span classification for composer queue-shorthand decoration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueSpan {
	/// The leading `->` or `=>` queue prefix.
	Prefix,
	/// A list marker or delimiter inside the shorthand body.
	Marker,
}

/// Returns decoration spans into the original composer text.
pub fn decoration_spans(text: &str) -> SmallVec<(usize, usize, QueueSpan), 8> {
	let trimmed_start = text.trim_start();
	let prefix_at = text.len() - trimmed_start.len();
	let Some(body) = yield_body(trimmed_start) else {
		return SmallVec::new();
	};
	let body_at = prefix_at + 2;
	let body_trimmed = body.trim();
	let body_trimmed_at = body_at + body.len() - body.trim_start().len();
	let mut spans = SmallVec::from_buf([(prefix_at, body_at, QueueSpan::Prefix)]);

	delimiter_spans(body_trimmed, body_trimmed_at, &mut spans);
	if let Some(markers) = sequential_marker_spans(body_trimmed, body_trimmed_at) {
		spans.extend(markers);
	}
	spans.sort_unstable_by_key(|span| span.0);
	spans
}

/// Splits delimiter or sequential-list shorthand into queued messages.
/// Ordinary prose always returns exactly one item.
pub fn split(text: &str) -> Vec<QueueItem> {
	let trimmed = text.trim();
	if let Some(body) = yield_body(trimmed) {
		return split_plain(body.trim())
			.into_iter()
			.map(yield_item)
			.collect();
	}
	split_plain(trimmed).into_iter().map(item).collect()
}

fn yield_body(text: &str) -> Option<&str> {
	text
		.strip_prefix("->")
		.or_else(|| text.strip_prefix("=>"))
		.filter(|body| body.starts_with(char::is_whitespace))
}

fn split_plain(text: &str) -> Vec<&str> {
	let delimited = split_delimiters(text);
	if delimited.len() > 1 {
		return delimited;
	}
	if let Some(list) = split_sequential_list(text) {
		return list;
	}
	vec![text]
}

fn item(text: &str) -> QueueItem {
	QueueItem { text: Str::new(text.trim()), yield_after_turn: false }
}

fn yield_item(text: &str) -> QueueItem {
	QueueItem { text: Str::new(text.trim()), yield_after_turn: true }
}

fn split_delimiters(text: &str) -> Vec<&str> {
	let mut items = Vec::new();
	let mut start = 0;
	let mut offset = 0;
	for line in text.split_inclusive('\n') {
		let body = line.trim();
		if body == "---" || body == "///" {
			let candidate = text[start..offset].trim();
			if !candidate.is_empty() {
				items.push(candidate);
			}
			start = offset + line.len();
		}
		offset += line.len();
	}
	let tail = text[start..].trim();
	if !tail.is_empty() {
		items.push(tail);
	}
	items
}
fn delimiter_spans(text: &str, base: usize, spans: &mut SmallVec<(usize, usize, QueueSpan), 8>) {
	let mut offset = 0;
	for line in text.split_inclusive('\n') {
		let trimmed = line.trim();
		if trimmed == "---" || trimmed == "///" {
			let marker_at = offset + line.len() - line.trim_start().len();
			spans.push((base + marker_at, base + marker_at + 3, QueueSpan::Marker));
		}
		offset += line.len();
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Marker {
	Decimal(u16),
	Alpha(u16),
	Roman(u16),
}

impl Marker {
	const fn ordinal(self) -> u16 {
		match self {
			Self::Decimal(n) | Self::Alpha(n) | Self::Roman(n) => n,
		}
	}

	const fn family(self) -> u8 {
		match self {
			Self::Decimal(_) => 0,
			Self::Alpha(_) => 1,
			Self::Roman(_) => 2,
		}
	}
}

fn split_sequential_list(text: &str) -> Option<Vec<&str>> {
	let mut starts = Vec::new();
	let mut family = None;
	let mut expected = None;
	let mut offset = 0;
	for line in text.split_inclusive('\n') {
		let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
		if indent == 0
			&& let Some((marker, body_at)) = marker(line)
		{
			if let Some(wanted) = family
				&& (wanted != marker.family() || expected != Some(marker.ordinal()))
			{
				return None;
			}
			family.get_or_insert(marker.family());
			expected = Some(marker.ordinal().saturating_add(1));
			starts.push((offset, body_at));
		}
		offset += line.len();
	}
	if starts.len() < 2 {
		return None;
	}
	let mut items = Vec::with_capacity(starts.len());
	for (index, &(line_start, body_at)) in starts.iter().enumerate() {
		let end = starts.get(index + 1).map_or(text.len(), |(next, _)| *next);
		items.push(text[line_start + body_at..end].trim());
	}
	Some(items)
}
fn sequential_marker_spans(
	text: &str,
	base: usize,
) -> Option<SmallVec<(usize, usize, QueueSpan), 8>> {
	let mut spans = SmallVec::new();
	let mut family = None;
	let mut expected = None;
	let mut offset = 0;
	for line in text.split_inclusive('\n') {
		let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
		if indent == 0
			&& let Some((marker, marker_end)) = decoration_marker(line)
		{
			if let Some(wanted) = family
				&& (wanted != marker.family() || expected != Some(marker.ordinal()))
			{
				return None;
			}
			family.get_or_insert(marker.family());
			expected = Some(marker.ordinal().saturating_add(1));
			spans.push((base + offset, base + offset + marker_end, QueueSpan::Marker));
		}
		offset += line.len();
	}
	(spans.len() >= 2).then_some(spans)
}

fn marker(line: &str) -> Option<(Marker, usize)> {
	let (marker, body_at) = marker_token(line)?;
	if !line[body_at..].starts_with(char::is_whitespace) {
		return None;
	}
	Some((marker, body_at))
}

fn decoration_marker(line: &str) -> Option<(Marker, usize)> {
	let (marker, body_at) = marker_token(line)?;
	if !line[body_at..].is_empty() && !line[body_at..].starts_with(char::is_whitespace) {
		return None;
	}
	Some((marker, body_at))
}

fn marker_token(line: &str) -> Option<(Marker, usize)> {
	let token_end = line.find(['.', ')'])?;
	let punctuation = line.as_bytes().get(token_end)?;
	if !matches!(punctuation, b'.' | b')') {
		return None;
	}
	let body_at = token_end + 1;
	let token = &line[..token_end];
	let marker = if let Ok(number) = token.parse::<u16>() {
		Marker::Decimal(number)
	} else if token
		.chars()
		.all(|ch| matches!(ch.to_ascii_lowercase(), 'i' | 'v' | 'x' | 'l' | 'c'))
	{
		Marker::Roman(parse_roman(token)?)
	} else if token.len() == 1 && token.as_bytes()[0].is_ascii_alphabetic() {
		Marker::Alpha(u16::from(token.as_bytes()[0].to_ascii_lowercase() - b'a' + 1))
	} else {
		return None;
	};
	Some((marker, body_at))
}

fn parse_roman(token: &str) -> Option<u16> {
	let mut total = 0_u16;
	let mut previous = 0_u16;
	for ch in token.chars().rev() {
		let value = match ch.to_ascii_lowercase() {
			'i' => 1,
			'v' => 5,
			'x' => 10,
			'l' => 50,
			'c' => 100,
			_ => return None,
		};
		if value < previous {
			total = total.checked_sub(value)?;
		} else {
			total = total.checked_add(value)?;
		}
		previous = value;
	}
	(total > 0).then_some(total)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn texts(items: &[QueueItem]) -> Vec<&str> {
		items.iter().map(|item| item.text.as_str()).collect()
	}

	#[test]
	fn delimiters_split_without_leaking_syntax() {
		assert_eq!(texts(&split("one\n---\ntwo\n///\nthree")), ["one", "two", "three"]);
	}

	#[test]
	fn yield_body_single_message_stays_one_item() {
		let yielded = split("-> single message");
		assert_eq!(texts(&yielded), ["single message"]);
		assert!(yielded[0].yield_after_turn);
	}

	#[test]
	fn yield_body_sequential_lists_split() {
		let yielded = split("->\n1. first\n2. second");
		assert_eq!(texts(&yielded), ["first", "second"]);
		assert!(yielded.iter().all(|item| item.yield_after_turn));
	}

	#[test]
	fn yield_body_delimiters_split() {
		let yielded = split("=>\nfirst\n---\nsecond\n///\nthird");
		assert_eq!(texts(&yielded), ["first", "second", "third"]);
		assert!(yielded.iter().all(|item| item.yield_after_turn));
	}

	#[test]
	fn sequential_decimal_alpha_and_roman_lists_split() {
		assert_eq!(texts(&split("1. first\n2) second")), ["first", "second"]);
		assert_eq!(texts(&split("a) first\nb. second")), ["first", "second"]);
		assert_eq!(texts(&split("i. first\nii) second\niii. third")), ["first", "second", "third"]);
		assert_eq!(texts(&split("1. first\n3. not sequential")), ["1. first\n3. not sequential"]);
	}
	#[test]
	fn decoration_spans_track_original_prefix_offsets() {
		assert_eq!(decoration_spans("  -> hi").as_slice(), &[(2, 4, QueueSpan::Prefix)]);
		assert_eq!(decoration_spans("=>  x").as_slice(), &[(0, 2, QueueSpan::Prefix)]);
	}

	#[test]
	fn decoration_spans_mark_lists_and_delimiters() {
		assert_eq!(decoration_spans(" \t->\n1. first\n2) second").as_slice(), &[
			(2, 4, QueueSpan::Prefix),
			(5, 7, QueueSpan::Marker),
			(14, 16, QueueSpan::Marker),
		]);
		assert_eq!(decoration_spans("->\none\n---\ntwo\n///\nthree").as_slice(), &[
			(0, 2, QueueSpan::Prefix),
			(7, 10, QueueSpan::Marker),
			(15, 18, QueueSpan::Marker),
		]);
		assert_eq!(decoration_spans("->\n1. first\n2.").as_slice(), &[
			(0, 2, QueueSpan::Prefix),
			(3, 5, QueueSpan::Marker),
			(12, 14, QueueSpan::Marker),
		]);
	}

	#[test]
	fn decoration_spans_ignore_plain_text_and_lists() {
		assert!(decoration_spans("plain prose").is_empty());
		assert!(decoration_spans("1. first\n2. second").is_empty());
	}
}
