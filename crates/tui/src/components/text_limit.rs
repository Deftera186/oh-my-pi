//! UTF-16-compatible character limiting for retained text leaves.

use std::borrow::Cow;

use crate::markup::Truncate;

const ELLIPSIS: char = '…';
const REPLACEMENT: char = '\u{fffd}';

/// Limits `text` by JavaScript UTF-16 code units and marks truncation with an
/// ellipsis.
///
/// `max_chars` is the retained body limit; the ellipsis is additional. A cut
/// through an astral character emits the replacement character, matching the
/// UTF-8 encoding of a JavaScript string containing the resulting lone
/// surrogate. Unchanged input is returned borrowed.
pub fn limit_utf16(text: &str, max_chars: usize, truncate_from: Truncate) -> Cow<'_, str> {
	let utf16_len = text.chars().map(char::len_utf16).sum::<usize>();
	if utf16_len <= max_chars {
		return Cow::Borrowed(text);
	}

	Cow::Owned(match truncate_from {
		Truncate::End => limit_end(text, max_chars),
		Truncate::Start => limit_start(text, max_chars),
	})
}

fn limit_end(text: &str, max_chars: usize) -> String {
	let mut retained_units = 0;
	let mut end = 0;
	let mut split_surrogate = false;
	for (at, character) in text.char_indices() {
		let next_units = retained_units + character.len_utf16();
		if next_units > max_chars {
			split_surrogate = retained_units < max_chars;
			break;
		}
		retained_units = next_units;
		end = at + character.len_utf8();
	}

	let mut limited = String::with_capacity(end + usize::from(split_surrogate) * 3 + 3);
	limited.push_str(&text[..end]);
	if split_surrogate {
		limited.push(REPLACEMENT);
	}
	limited.push(ELLIPSIS);
	limited
}

fn limit_start(text: &str, max_chars: usize) -> String {
	let mut retained_units = 0;
	let mut start = text.len();
	let mut split_surrogate = false;
	for (at, character) in text.char_indices().rev() {
		let next_units = retained_units + character.len_utf16();
		if next_units > max_chars {
			split_surrogate = retained_units < max_chars;
			break;
		}
		retained_units = next_units;
		start = at;
	}

	let mut limited =
		String::with_capacity(3 + usize::from(split_surrogate) * 3 + text.len() - start);
	limited.push(ELLIPSIS);
	if split_surrogate {
		limited.push(REPLACEMENT);
	}
	limited.push_str(&text[start..]);
	limited
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use super::limit_utf16;
	use crate::markup::Truncate;

	#[test]
	fn ascii_limits_in_both_directions() {
		assert_eq!(limit_utf16("abcdef", 3, Truncate::End), "abc…");
		assert_eq!(limit_utf16("abcdef", 3, Truncate::Start), "…def");
	}

	#[test]
	fn multibyte_bmp_characters_each_use_one_unit() {
		assert_eq!(limit_utf16("é界z", 2, Truncate::End), "é界…");
		assert_eq!(limit_utf16("é界z", 2, Truncate::Start), "…界z");
	}

	#[test]
	fn astral_boundaries_match_split_surrogate_replacement() {
		assert_eq!(limit_utf16("a😀z", 2, Truncate::End), "a�…");
		assert_eq!(limit_utf16("a😀z", 2, Truncate::Start), "…�z");
		assert_eq!(limit_utf16("😀z", 2, Truncate::End), "😀…");
		assert_eq!(limit_utf16("a😀", 2, Truncate::Start), "…😀");
	}

	#[test]
	fn zero_and_one_limits_preserve_the_ellipsis() {
		assert_eq!(limit_utf16("ab", 0, Truncate::End), "…");
		assert_eq!(limit_utf16("ab", 0, Truncate::Start), "…");
		assert_eq!(limit_utf16("ab", 1, Truncate::End), "a…");
		assert_eq!(limit_utf16("ab", 1, Truncate::Start), "…b");
		assert_eq!(limit_utf16("😀", 1, Truncate::End), "�…");
		assert_eq!(limit_utf16("😀", 1, Truncate::Start), "…�");
	}

	#[test]
	fn exact_boundaries_and_empty_input_stay_borrowed() {
		for (text, limit) in [("", 0), ("abc", 3), ("é界", 2), ("a😀", 3)] {
			for direction in [Truncate::End, Truncate::Start] {
				let limited = limit_utf16(text, limit, direction);
				let Cow::Borrowed(borrowed) = limited else {
					panic!("unchanged input must remain borrowed");
				};
				assert_eq!(borrowed.as_ptr(), text.as_ptr());
				assert_eq!(borrowed.len(), text.len());
			}
		}
	}

	#[test]
	fn every_utf16_cut_matches_javascript_slice_semantics() {
		let samples = ["a", "abcdef", "é界z", "😀", "a😀z", "😀界🚀", "e\u{301}😀x"];
		for text in samples {
			let units = text.encode_utf16().collect::<Vec<_>>();
			for max_chars in 0..=units.len() + 1 {
				for direction in [Truncate::End, Truncate::Start] {
					let expected = javascript_reference(&units, max_chars, direction);
					assert_eq!(
						limit_utf16(text, max_chars, direction),
						expected,
						"text={text:?}, max_chars={max_chars}, direction={direction:?}",
					);
				}
			}
		}
	}

	fn javascript_reference(units: &[u16], max_chars: usize, direction: Truncate) -> String {
		if units.len() <= max_chars {
			return String::from_utf16(units).expect("source text is valid UTF-16");
		}
		let retained = match direction {
			Truncate::End => &units[..max_chars],
			Truncate::Start => &units[units.len() - max_chars..],
		};
		let body = String::from_utf16_lossy(retained);
		match direction {
			Truncate::End => format!("{body}…"),
			Truncate::Start => format!("…{body}"),
		}
	}
}
