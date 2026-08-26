use std::{
	io,
	path::{Path, PathBuf},
	str,
};

use ast_grep_core::{
	MatchStrictness,
	matcher::{Pattern, PatternError},
	meta_var::MetaVariable,
	source::Edit,
	tree_sitter::LanguageExt,
};
use ignore::WalkBuilder;
use omp_core::Str;
use omp_walker::glob::CompiledGlobSet;
use smallvec::SmallVec;

use crate::{AstError, Result, language::SupportLang};

/// Pattern matching strictness exposed by the AST API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AstMatchStrictness {
	/// Exact concrete-syntax matching.
	Cst,
	/// Ast-grep smart matching.
	Smart,
	/// Named AST-node matching.
	Ast,
	/// Relaxed AST matching.
	Relaxed,
	/// Signature-level matching.
	Signature,
	/// Template-style matching.
	Template,
}

impl From<AstMatchStrictness> for MatchStrictness {
	fn from(value: AstMatchStrictness) -> Self {
		match value {
			AstMatchStrictness::Cst => Self::Cst,
			AstMatchStrictness::Smart => Self::Smart,
			AstMatchStrictness::Ast => Self::Ast,
			AstMatchStrictness::Relaxed => Self::Relaxed,
			AstMatchStrictness::Signature => Self::Signature,
			AstMatchStrictness::Template => Self::Template,
		}
	}
}

/// One structural source match and its source coordinates.
#[derive(Debug, Clone)]
pub struct AstMatch {
	/// One-based start line.
	pub line:       usize,
	/// One-based start column.
	pub column:     usize,
	/// One-based end line.
	pub end_line:   usize,
	/// One-based end column.
	pub end_column: usize,
	/// Inclusive start byte offset.
	pub byte_start: usize,
	/// Exclusive end byte offset.
	pub byte_end:   usize,
	/// Matched source text.
	pub text:       Str,
	/// Deterministically ordered metavariable captures.
	pub bindings:   Vec<AstBinding>,
}

/// One metavariable captured by a structural match.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AstBinding {
	/// Pattern spelling, including `$` or `$$$`.
	pub name:  Str,
	/// Exact source text captured for the metavariable.
	pub value: Str,
}

/// A filesystem match with absolute and workspace-relative paths.
#[derive(Debug, Clone)]
pub struct MatchedFile {
	/// Absolute file path.
	pub absolute_path: PathBuf,
	/// Workspace-relative slash-separated path.
	pub relative_path: Str,
}

/// A replacement template paired with its compiled search patterns.
#[derive(Debug, Clone)]
pub struct CompiledRewrite {
	/// Replacement template.
	pub out:      Str,
	/// Compiled patterns that trigger this replacement.
	pub patterns: SmallVec<Pattern, 2>,
}

/// Resolves an optional API strictness to ast-grep strictness.
pub fn resolve_strictness(value: Option<AstMatchStrictness>) -> MatchStrictness {
	value.map_or(MatchStrictness::Smart, Into::into)
}

/// Returns supported language aliases as a comma-separated list.
pub fn supported_lang_list() -> String {
	SupportLang::sorted_aliases().join(", ")
}

/// Resolves a language alias or returns a descriptive error.
pub fn resolve_supported_lang(value: &str) -> Result<SupportLang> {
	SupportLang::from_alias(value).ok_or_else(|| AstError::UnsupportedLanguage {
		value:     Str::new(value),
		supported: Str::from(supported_lang_list()),
	})
}

/// Resolves an explicit language or infers one from a file path.
pub fn resolve_language(lang: Option<&str>, file_path: &Path) -> Result<SupportLang> {
	if let Some(lang) = lang.map(str::trim).filter(|lang| !lang.is_empty()) {
		return resolve_supported_lang(lang);
	}
	SupportLang::from_path(file_path)
		.ok_or_else(|| AstError::InferLanguageFailed { path: file_path.to_path_buf() })
}

/// Reports whether a file has an explicit or inferable language.
pub fn is_supported_file(file_path: &Path, explicit_lang: Option<&str>) -> bool {
	if explicit_lang.is_some() {
		return true;
	}
	resolve_language(None, file_path).is_ok()
}

/// Compiles one structural pattern with optional contextual selection.
pub fn compile_pattern(
	pattern: &str,
	selector: Option<&str>,
	strictness: &MatchStrictness,
	lang: SupportLang,
) -> Result<Pattern> {
	let selector = selector.map(str::trim).filter(|s| !s.is_empty());
	let mut compiled = if let Some(selector) = selector {
		Pattern::contextual(pattern, selector, lang)
			.map_err(|source| AstError::InvalidPattern { source })?
	} else {
		match Pattern::try_new(pattern, lang) {
			Ok(compiled) => compiled,
			Err(err @ PatternError::MultipleNode(_)) => {
				match compile_wrapped_fallback(pattern, strictness, lang) {
					Some(compiled) => return Ok(compiled),
					None => return Err(AstError::InvalidPattern { source: err }),
				}
			},
			Err(err) => return Err(AstError::InvalidPattern { source: err }),
		}
	};
	compiled.strictness = strictness.clone();
	Ok(compiled)
}

/// Language-specific wrapper template used to turn a multi-node fragment into a
/// single selectable node. `None` for languages without a template — those keep
/// the original `MultipleNode` error.
const fn wrapper_template(lang: SupportLang) -> Option<(&'static str, &'static str, &'static str)> {
	// (prefix, suffix, selector-kind); the fragment is spliced between
	// prefix/suffix.
	match lang {
		SupportLang::Json => Some(("{", "}", "pair")),
		_ => None,
	}
}

/// Retry a fragment that failed as `MultipleNode` by wrapping it in a minimal
/// valid context and selecting the node kind that spans it. Returns the
/// compiled pattern (with `strictness` applied) or `None` if this language has
/// no template or the wrapped form still fails to compile.
fn compile_wrapped_fallback(
	pattern: &str,
	strictness: &MatchStrictness,
	lang: SupportLang,
) -> Option<Pattern> {
	let (prefix, suffix, selector) = wrapper_template(lang)?;
	// JSON only accepts a bare `$V` inside a string, so quote value-position
	// metavars; ast-grep still reads the quoted `"$V"` as capture `V`.
	let prepared = if lang == SupportLang::Json {
		quote_bare_metavars(pattern)
	} else {
		pattern.to_string()
	};
	let context = format!("{prefix} {prepared} {suffix}");
	let mut compiled = Pattern::contextual(&context, selector, lang).ok()?;
	compiled.strictness = strictness.clone();
	Some(compiled)
}

/// Wrap bare `$NAME` / `$$$NAME` metavars in double quotes so a JSON wrapper
/// parses. Metavars already inside a string literal (including `"$V"`) are left
/// untouched; a quote toggles in/out of string context.
fn quote_bare_metavars(pattern: &str) -> String {
	let bytes = pattern.as_bytes();
	let mut out = String::with_capacity(pattern.len() + 4);
	let mut in_string = false;
	let mut index = 0;
	while index < bytes.len() {
		let byte = bytes[index];
		if byte == b'"' && (index == 0 || bytes[index - 1] != b'\\') {
			in_string = !in_string;
			out.push('"');
			index += 1;
			continue;
		}
		if byte == b'$' && !in_string {
			// Consume `$`, an optional `$$` ellipsis, then the identifier.
			let start = index;
			index += 1;
			if bytes[index..].starts_with(b"$$") {
				index += 2;
			}
			while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
			{
				index += 1;
			}
			out.push('"');
			out.push_str(&pattern[start..index]);
			out.push('"');
			continue;
		}
		// Copy this byte's full UTF-8 char so multi-byte content is preserved.
		let char_end = next_char_boundary(bytes, index);
		out.push_str(&pattern[index..char_end]);
		index = char_end;
	}
	out
}

/// Byte index of the end of the UTF-8 character starting at `index`.
const fn next_char_boundary(bytes: &[u8], index: usize) -> usize {
	let mut end = index + 1;
	while end < bytes.len() && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
		end += 1;
	}
	end
}

/// Compiles search patterns, including language-specific contextual variants.
pub fn compile_search_patterns(
	pattern: &str,
	language: SupportLang,
) -> Result<SmallVec<Pattern, 2>, PatternError> {
	let mut compiled = SmallVec::new();
	match Pattern::try_new(pattern, language) {
		Ok(pattern) => compiled.push(pattern),
		// Multi-node fragments (e.g. `"key": $V`) get the same auto-wrap fallback
		// as the edit path; other errors propagate unchanged.
		Err(err @ PatternError::MultipleNode(_)) => {
			match compile_wrapped_fallback(pattern, &MatchStrictness::Smart, language) {
				Some(pattern) => compiled.push(pattern),
				None => return Err(err),
			}
		},
		Err(err) => return Err(err),
	}
	if language == SupportLang::Rust {
		let trimmed = pattern.trim_end();
		if let Some(contextual) = compile_rust_contextual_pattern(trimmed) {
			compiled.push(contextual);
		}
	}
	Ok(compiled)
}

/// Compiles ordered pattern/replacement rules.
pub fn compile_rewrite_rules(
	rules: &[(String, String)],
	language: SupportLang,
) -> Result<Vec<CompiledRewrite>, (usize, PatternError)> {
	rules
		.iter()
		.enumerate()
		.map(|(index, (pattern, out))| {
			compile_search_patterns(pattern, language)
				.map(|patterns| CompiledRewrite { out: Str::new(out), patterns })
				.map_err(|error| (index, error))
		})
		.collect()
}

/// Collects all matches for compiled patterns in source order per pattern.
pub fn collect_matches(source: &str, language: SupportLang, patterns: &[Pattern]) -> Vec<AstMatch> {
	let ast = language.ast_grep(source);
	let mut matches = Vec::new();
	for pattern in patterns {
		for matched in ast.root().find_all(pattern.clone()) {
			let start = matched.start_pos();
			let end = matched.end_pos();
			let range = matched.range();
			let node = matched.get_node();
			let environment = matched.get_env();
			let mut bindings = environment
				.get_matched_variables()
				.filter_map(|variable| {
					let name = match &variable {
						MetaVariable::Capture(name, _) => format!("${name}"),
						MetaVariable::MultiCapture(name) => format!("$$${name}"),
						MetaVariable::Dropped(_) | MetaVariable::Multiple => return None,
					};
					let value = environment.get_var_bytes(&variable)?;
					Some(AstBinding {
						name:  Str::new(name),
						value: Str::new(String::from_utf8_lossy(value)),
					})
				})
				.collect::<Vec<_>>();
			bindings.sort_unstable_by(|left, right| left.name.cmp(&right.name));
			matches.push(AstMatch {
				line: start.line() + 1,
				column: start.column(node) + 1,
				end_line: end.line() + 1,
				end_column: end.column(node) + 1,
				byte_start: range.start,
				byte_end: range.end,
				text: Str::new(matched.text()),
				bindings,
			});
		}
	}
	matches
}

/// Applies compiled rewrite operations and returns source plus replacement
/// count.
pub fn rewrite_source(
	source: &str,
	language: SupportLang,
	ops: &[CompiledRewrite],
) -> Result<(String, u32)> {
	let mut ast = language.ast_grep(source);
	let mut replacements = 0_u32;
	for op in ops {
		for pattern in &op.patterns {
			let edits = ast.root().replace_all(pattern.clone(), op.out.as_str());
			if edits.is_empty() {
				continue;
			}
			replacements = replacements.saturating_add(edits.len() as u32);
			let updated = apply_edits(ast.root().text().as_ref(), &edits)?;
			ast = language.ast_grep(updated);
		}
	}
	Ok((ast.root().text().into_owned(), replacements))
}

/// Applies deterministic, non-overlapping ast-grep edits to UTF-8 content.
pub fn apply_edits(content: &str, edits: &[Edit<String>]) -> Result<String> {
	let mut sorted: SmallVec<&Edit<String>, 8> = edits.iter().collect();
	sorted.sort_unstable_by(|a, b| {
		a.position
			.cmp(&b.position)
			.then(a.deleted_length.cmp(&b.deleted_length))
			.then(a.inserted_text.cmp(&b.inserted_text))
	});
	// Byte-identical edits (same span, same replacement) are one deterministic
	// edit: multiple patterns matching the same node collapse instead of
	// tripping the overlap check. Only divergent overlaps are ambiguous.
	sorted.dedup_by(|a, b| {
		a.position == b.position
			&& a.deleted_length == b.deleted_length
			&& a.inserted_text == b.inserted_text
	});
	let mut prev_end = 0usize;
	for edit in &sorted {
		if edit.position < prev_end {
			return Err(AstError::OverlappingReplacements);
		}
		let end = edit
			.position
			.checked_add(edit.deleted_length)
			.ok_or(AstError::EditRangeOutOfBounds)?;
		if end > content.len()
			|| !content.is_char_boundary(edit.position)
			|| !content.is_char_boundary(end)
		{
			return Err(AstError::EditRangeOutOfBounds);
		}
		str::from_utf8(&edit.inserted_text)
			.map_err(|source| AstError::NonUtf8Replacement { source })?;
		prev_end = end;
	}

	let mut output = content.to_string();
	for edit in sorted.into_iter().rev() {
		let end = edit.position + edit.deleted_length;
		let replacement = str::from_utf8(&edit.inserted_text)
			.expect("replacement UTF-8 was validated before applying edits");
		output.replace_range(edit.position..end, replacement);
	}
	Ok(output)
}

/// Walks a directory and collects files matched by paths or glob patterns.
pub fn collect_matched_files(
	cwd: &Path,
	patterns: &[String],
) -> Result<Vec<MatchedFile>, io::Error> {
	let globset = build_globset(patterns)?;
	let mut builder = WalkBuilder::new(cwd);
	builder
		.hidden(false)
		.git_ignore(true)
		.git_global(true)
		.git_exclude(true);
	let mut files = Vec::new();
	for entry in builder.build() {
		let entry = match entry {
			Ok(entry) => entry,
			Err(error) => return Err(io::Error::other(error)),
		};
		if !entry.file_type().is_some_and(|ft| ft.is_file()) {
			continue;
		}
		let absolute_path = entry.into_path();
		let relative_path = absolute_path
			.strip_prefix(cwd)
			.unwrap_or(&absolute_path)
			.to_string_lossy();
		let relative_path = if relative_path.contains('\\') {
			Str::from(relative_path.replace('\\', "/"))
		} else {
			Str::new(relative_path.as_ref())
		};
		if globset.matches(relative_path.as_str())
			|| patterns
				.iter()
				.any(|pattern| pattern == relative_path.as_str())
		{
			files.push(MatchedFile { absolute_path, relative_path });
		}
	}
	files.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
	Ok(files)
}

fn build_globset(patterns: &[String]) -> Result<CompiledGlobSet, io::Error> {
	let globby = patterns
		.iter()
		.filter(|pattern| has_glob_syntax(pattern))
		.map(String::as_str);
	CompiledGlobSet::new(globby).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

/// Reports whether a path pattern contains supported glob syntax.
pub fn has_glob_syntax(pattern: &str) -> bool {
	pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

fn compile_rust_contextual_pattern(pattern: &str) -> Option<Pattern> {
	let language = SupportLang::Rust;
	let context = format!("fn __rwp_wrapper() {{ {pattern}; }}");
	let ast = language.ast_grep(&context);
	let selector = ast.root().find("expression_statement")?;
	Pattern::contextual(pattern, selector.kind().as_ref(), language).ok()
}

#[cfg(test)]
mod tests {
	use ast_grep_core::source::Edit;
	use omp_core::Str;

	use super::{SupportLang, apply_edits, compile_search_patterns};

	#[test]
	fn compile_search_patterns_compiles_rust_patterns() {
		let patterns = compile_search_patterns("foo($$$ARGS)", SupportLang::Rust)
			.expect("rust pattern should compile");
		assert!(!patterns.is_empty());
	}

	#[test]
	fn collect_matches_retains_sorted_metavariable_bindings() {
		let patterns = compile_search_patterns("const $NAME = $VALUE", SupportLang::TypeScript)
			.expect("TypeScript pattern should compile");
		let matches =
			super::collect_matches("const answer = 42;", SupportLang::TypeScript, &patterns);
		assert_eq!(matches.len(), 1);
		assert_eq!(matches[0].bindings, [
			super::AstBinding { name: Str::new("$NAME"), value: Str::new("answer") },
			super::AstBinding { name: Str::new("$VALUE"), value: Str::new("42") },
		]);
	}

	#[test]
	fn apply_edits_rejects_overlaps() {
		let source = "abcdef";
		let edits = vec![
			Edit::<String> { position: 1, deleted_length: 3, inserted_text: b"x".to_vec() },
			Edit::<String> { position: 2, deleted_length: 1, inserted_text: b"y".to_vec() },
		];
		assert!(apply_edits(source, &edits).is_err());
	}

	#[test]
	fn apply_edits_dedupes_identical_edits() {
		let source = "abcdef";
		let edits = vec![
			Edit::<String> { position: 1, deleted_length: 3, inserted_text: b"x".to_vec() },
			Edit::<String> { position: 1, deleted_length: 3, inserted_text: b"x".to_vec() },
		];
		let output = apply_edits(source, &edits).expect("identical edits should collapse to one");
		assert_eq!(output, "axef");
	}
}
