//! Symbol targeting and bounded normalization for LSP navigation results.

use omp_core::Str;
use omp_docserver::position::PositionEncoding;
use serde_json::Value;

/// Parsed `symbol#N` target, where occurrence is one-based.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolTarget {
	/// Identifier text.
	pub symbol:     Str,
	/// One-based occurrence on the requested line.
	pub occurrence: usize,
}

/// Parses an identifier target with an optional one-based `#N` suffix.
pub fn parse_symbol_target(value: &str) -> Result<SymbolTarget, &'static str> {
	let (symbol, occurrence) = match value.rsplit_once('#') {
		Some((symbol, occurrence))
			if !symbol.is_empty() && occurrence.bytes().all(|byte| byte.is_ascii_digit()) =>
		{
			let occurrence = occurrence
				.parse::<usize>()
				.map_err(|_| "symbol occurrence is too large")?;
			if occurrence == 0 {
				return Err("symbol occurrence must be one-based");
			}
			(symbol, occurrence)
		},
		_ => (value, 1),
	};
	if symbol.is_empty() || !symbol.chars().all(is_word_character) {
		return Err("symbol must contain only identifier word characters");
	}
	Ok(SymbolTarget { symbol: Str::from(symbol), occurrence })
}

/// Resolves a target's zero-based column in the negotiated LSP position
/// encoding on one source line.
pub fn resolve_symbol_column(
	line: &str,
	target: &SymbolTarget,
	encoding: PositionEncoding,
) -> Option<u32> {
	let bytes = line.as_bytes();
	let needle = target.symbol.as_bytes();
	let mut offset = 0;
	let mut occurrence = 0;
	while offset + needle.len() <= bytes.len() {
		let relative = bytes[offset..]
			.windows(needle.len())
			.position(|candidate| candidate == needle)?;
		let start = offset + relative;
		let end = start + needle.len();
		let left_boundary = start == 0 || !is_word_byte(bytes[start - 1]);
		let right_boundary = end == bytes.len() || !is_word_byte(bytes[end]);
		if left_boundary && right_boundary {
			occurrence += 1;
			if occurrence == target.occurrence {
				return encoding
					.offset_to_position(line, start)
					.ok()
					.map(|position| position.character);
			}
		}
		offset = end;
	}
	None
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn symbol_columns_follow_negotiated_position_encoding() {
		let line = r#"let _ = "😀"; foo();"#;
		let target = parse_symbol_target("foo").expect("valid target");
		assert_eq!(resolve_symbol_column(line, &target, PositionEncoding::Utf8), Some(16));
		assert_eq!(resolve_symbol_column(line, &target, PositionEncoding::Utf16), Some(14));
		assert_eq!(resolve_symbol_column(line, &target, PositionEncoding::Utf32), Some(13));
	}
}

const fn is_word_byte(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_word_character(character: char) -> bool {
	character == '_' || character.is_alphanumeric()
}

/// Normalizes LSP Location and LocationLink results to a bounded location list.
pub fn normalize_locations(value: &Value, limit: usize) -> Vec<Value> {
	let values = value
		.as_array()
		.map_or_else(|| vec![value], |values| values.iter().collect());
	values
		.into_iter()
		.filter_map(|location| {
			if location.get("uri").is_some() && location.get("range").is_some() {
				return Some(location.clone());
			}
			let uri = location.get("targetUri")?.clone();
			let range = location
				.get("targetSelectionRange")
				.or_else(|| location.get("targetRange"))?
				.clone();
			Some(serde_json::json!({ "uri": uri, "range": range }))
		})
		.take(limit)
		.collect()
}

/// Extracts Markdown, MarkedString, and plaintext hover contents.
pub fn hover_text(contents: &Value) -> Str {
	fn append(value: &Value, output: &mut String) {
		match value {
			Value::String(text) => output.push_str(text),
			Value::Array(values) => {
				for (index, value) in values.iter().enumerate() {
					if index > 0 {
						output.push_str("\n\n");
					}
					append(value, output);
				}
			},
			Value::Object(object) => {
				if let Some(Value::String(value)) = object.get("value") {
					output.push_str(value);
				}
			},
			_ => {},
		}
	}
	let mut output = String::new();
	append(contents, &mut output);
	Str::from(output)
}
