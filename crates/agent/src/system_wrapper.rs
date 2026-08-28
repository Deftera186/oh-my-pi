//! Projection helpers for model-facing system-message envelopes.

/// Returns the readable body when `text` is exactly one outer `<system-*>`
/// envelope.
///
/// The scan is quote-aware while reading opening tags, preserves nested tags,
/// and returns a subslice of the input without allocating.
pub fn strip_system_wrapper(text: &str) -> Option<&str> {
	let trimmed = text.trim();
	let bytes = trimmed.as_bytes();
	const PREFIX: &[u8] = b"<system-";
	if bytes.len() <= PREFIX.len() || !bytes[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
		return None;
	}

	let mut name_end = PREFIX.len();
	while bytes
		.get(name_end)
		.is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
	{
		name_end += 1;
	}
	if name_end == PREFIX.len() || !tag_name_delimiter(trimmed, name_end) {
		return None;
	}
	let opening_end = opening_tag_end(trimmed, name_end)?;
	let tag_name = &bytes[1..name_end];
	let mut depth = 1_usize;
	let mut cursor = opening_end + 1;

	while cursor < bytes.len() {
		let Some(relative) = bytes[cursor..].iter().position(|byte| *byte == b'<') else {
			return None;
		};
		let tag_start = cursor + relative;
		let closing = bytes.get(tag_start + 1) == Some(&b'/');
		let name_start = tag_start + if closing { 2 } else { 1 };
		let mut nested_name_end = name_start;
		while bytes
			.get(nested_name_end)
			.is_some_and(|byte| xml_name_byte(*byte))
		{
			nested_name_end += 1;
		}
		if nested_name_end == name_start {
			cursor = tag_start + 1;
			continue;
		}

		let is_same_tag = bytes[name_start..nested_name_end].eq_ignore_ascii_case(tag_name);
		if closing {
			if bytes.get(nested_name_end) != Some(&b'>') {
				cursor = tag_start + 1;
				continue;
			}
			let closing_end = nested_name_end + 1;
			if is_same_tag {
				depth -= 1;
				if depth == 0 {
					return (closing_end == bytes.len())
						.then(|| trimmed[opening_end + 1..tag_start].trim());
				}
			}
			cursor = closing_end;
			continue;
		}

		if opening_name_delimiter(trimmed, nested_name_end)
			&& let Some(nested_end) = opening_tag_end(trimmed, nested_name_end)
		{
			let self_closing = bytes[nested_name_end..nested_end]
				.iter()
				.rev()
				.find(|byte| !byte.is_ascii_whitespace())
				== Some(&b'/');
			if is_same_tag && !self_closing {
				depth += 1;
			}
			cursor = nested_end + 1;
			continue;
		}
		cursor = tag_start + 1;
	}
	None
}

fn tag_name_delimiter(text: &str, index: usize) -> bool {
	text.as_bytes().get(index) == Some(&b'>')
		|| text[index..]
			.chars()
			.next()
			.is_some_and(char::is_whitespace)
}

fn opening_name_delimiter(text: &str, index: usize) -> bool {
	matches!(text.as_bytes().get(index), Some(b'>') | Some(b'/'))
		|| text[index..]
			.chars()
			.next()
			.is_some_and(char::is_whitespace)
}

const fn xml_name_byte(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':' | b'.')
}

fn opening_tag_end(text: &str, attributes_start: usize) -> Option<usize> {
	let bytes = text.as_bytes();
	let mut quote = None;
	for (index, byte) in bytes.iter().enumerate().skip(attributes_start) {
		if let Some(expected) = quote {
			if *byte == expected {
				quote = None;
			}
			continue;
		}
		match byte {
			b'\'' | b'"' => quote = Some(*byte),
			b'<' => return None,
			b'>' => return Some(index),
			_ => {},
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use super::strip_system_wrapper;

	#[test]
	fn strips_plain_envelope() {
		assert_eq!(
			strip_system_wrapper("  <system-reminder>\nKeep working.\n</system-reminder>  "),
			Some("Keep working.")
		);
	}

	#[test]
	fn preserves_nested_system_tags() {
		assert_eq!(
			strip_system_wrapper(
				"<system-notice>Result: <system-reminder>literal</system-reminder></system-notice>"
			),
			Some("Result: <system-reminder>literal</system-reminder>")
		);
		assert_eq!(
			strip_system_wrapper(
				"<system-notice>outer <system-notice>inner</system-notice></system-notice>"
			),
			Some("outer <system-notice>inner</system-notice>")
		);
		assert_eq!(
			strip_system_wrapper(concat!(
				r#"<system-notice><detail value="</system-notice>">literal</detail>"#,
				"<system-notice /></system-notice>",
			)),
			Some(
				concat!(r#"<detail value="</system-notice>">literal</detail>"#, "<system-notice />",)
			)
		);
	}

	#[test]
	fn handles_greater_than_signs_in_quoted_attributes() {
		assert_eq!(
			strip_system_wrapper(concat!(
				r#"<system-interrupt rule="coverage > 80%" path='rules/watch>dog.md'>"#,
				"Output interrupted.</system-interrupt>",
			)),
			Some("Output interrupted.")
		);
	}

	#[test]
	fn rejects_unterminated_and_non_wrapper_text() {
		assert_eq!(strip_system_wrapper("<system-notice>unfinished"), None);
		assert_eq!(strip_system_wrapper("ordinary user text"), None);
	}

	#[test]
	fn rejects_multiple_envelopes_and_trailing_text() {
		assert_eq!(
			strip_system_wrapper(
				"<system-notice>one</system-notice><system-notice>two</system-notice>"
			),
			None
		);
		assert_eq!(strip_system_wrapper("<system-notice>one</system-notice> trailing"), None);
	}
}
