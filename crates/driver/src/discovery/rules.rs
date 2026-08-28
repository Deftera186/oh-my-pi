//! Declarative rule parsing, scope normalization, and deterministic discovery.

use std::{
	collections::BTreeSet,
	fs,
	path::{Path, PathBuf},
	str::FromStr,
};

use omp_core::Str;
use omp_walker::WalkRequest;
use serde::Deserialize;

use super::{
	manifest::{
		CapabilityPayload, DiscoveredCapability, RuleInterruptMode, RulePayload, SourceProvenance,
		SourceScope,
	},
	skills::glob_matches,
};

/// One ordered rule source.
#[derive(Clone, Debug)]
pub struct RuleSource {
	/// Stable source/provider ID.
	pub id:        Str,
	/// Rule file or directory.
	pub root:      PathBuf,
	/// Source scope.
	pub scope:     SourceScope,
	/// Whether mutation commands must refuse this source.
	pub read_only: bool,
}

/// Rule discovery warning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleWarning {
	/// Malformed or suppressed source path.
	pub path:    PathBuf,
	/// Stable diagnostic.
	pub message: Str,
}

/// Parsed rule provider output.
#[derive(Clone, Debug, Default)]
pub struct RuleDiscovery {
	/// Parsed declarations in source precedence then path order.
	pub declarations: Vec<DiscoveredCapability>,
	/// Non-fatal diagnostics.
	pub warnings:     Vec<RuleWarning>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleHeader {
	description:    Option<String>,
	#[serde(default)]
	globs:          StringList,
	#[serde(default)]
	always_apply:   bool,
	#[serde(default, alias = "ttsr_trigger", alias = "ttsrTrigger")]
	condition:      StringList,
	#[serde(default)]
	ast_condition:  StringList,
	#[serde(default)]
	scope:          StringList,
	interrupt_mode: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum StringList {
	One(String),
	Many(Vec<String>),
	#[default]
	None,
}

impl StringList {
	fn strings(self) -> Vec<String> {
		match self {
			Self::One(value) => vec![value],
			Self::Many(values) => values,
			Self::None => Vec::new(),
		}
	}
}

/// Loads nested Markdown rules, normalizes TTSR scope shorthand, and retains
/// only declarative (never executable) conditions.
#[tracing::instrument(
	level = "debug",
	skip_all,
	name = "rule_discovery",
	fields(source_count = sources.len())
)]
pub fn discover(sources: &[RuleSource]) -> RuleDiscovery {
	let mut result = RuleDiscovery::default();
	let mut names = BTreeSet::new();
	for source in sources {
		for path in rule_files(&source.root) {
			let (header, content) = match parse_rule(&path) {
				Ok(value) => value,
				Err(_) => {
					result.warnings.push(RuleWarning {
						path,
						message: Str::from("failed to parse rule frontmatter"),
					});
					continue;
				},
			};
			let name = path
				.file_stem()
				.and_then(|name| name.to_str())
				.unwrap_or("rule");
			let key = Str::from(name);
			if !names.insert(key.clone()) {
				result.warnings.push(RuleWarning {
					path,
					message: Str::from("rule name is already claimed by a higher-priority source"),
				});
				continue;
			}
			let (conditions, inferred_scopes) = normalize_conditions(header.condition.strings());
			let mut scopes = header
				.scope
				.strings()
				.into_iter()
				.flat_map(|value| split_scope_tokens(&value))
				.map(Str::from)
				.collect::<Vec<_>>();
			scopes.extend(inferred_scopes);
			dedupe(&mut scopes);
			let interrupt_mode = match header.interrupt_mode.as_deref() {
				Some(value) => match RuleInterruptMode::from_str(value) {
					Ok(mode) => Some(mode),
					Err(_) => {
						result.warnings.push(RuleWarning {
							path:    path.clone(),
							message: Str::from("unsupported rule interruptMode"),
						});
						None
					},
				},
				None => None,
			};
			let payload = RulePayload {
				name: key.clone(),
				path: path.clone(),
				content: Str::from(content),
				globs: header
					.globs
					.strings()
					.into_iter()
					.flat_map(|value| {
						value
							.split(',')
							.map(str::trim)
							.filter(|v| !v.is_empty())
							.map(Str::from)
							.collect::<Vec<_>>()
					})
					.collect(),
				always_apply: header.always_apply,
				description: header
					.description
					.map(|value| Str::from(value.trim().to_owned())),
				conditions,
				ast_conditions: header
					.ast_condition
					.strings()
					.into_iter()
					.map(|value| Str::from(value.trim().to_owned()))
					.filter(|value| !value.is_empty())
					.collect(),
				scopes,
				interrupt_mode,
			};
			let mut provenance = SourceProvenance::native(source.id.clone(), path, source.scope);
			provenance.read_only = source.read_only;
			result.declarations.push(DiscoveredCapability::keyed(
				key,
				CapabilityPayload::Rules(payload),
				provenance,
			));
		}
	}
	result
}

fn rule_files(root: &Path) -> Vec<PathBuf> {
	if root.is_file() {
		return vec![root.to_path_buf()];
	}
	let mut files = WalkRequest::new(root)
		.hidden(false)
		.gitignore(true)
		.skip_git(true)
		.depth(1, 16)
		.collect_files()
		.unwrap_or_default()
		.into_iter()
		.map(|entry| entry.absolute_path(root))
		.filter(|path| {
			path
				.extension()
				.is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("mdc"))
		})
		.collect::<Vec<_>>();
	files.sort();
	files
}

fn parse_rule(path: &Path) -> Result<(RuleHeader, String), serde_yaml::Error> {
	let source = fs::read_to_string(path).unwrap_or_default();
	parse_header(&source)
}

fn parse_header(source: &str) -> Result<(RuleHeader, String), serde_yaml::Error> {
	let Some(rest) = source.strip_prefix("---\n") else {
		return Ok((RuleHeader::default(), source.to_owned()));
	};
	let Some((header, body)) = rest.split_once("\n---\n") else {
		return Ok((RuleHeader::default(), source.to_owned()));
	};
	Ok((serde_yaml::from_str(header)?, body.trim().to_owned()))
}

/// Parses an embedded/static Markdown rule through the same frontmatter and
/// shorthand pipeline as authored files.
pub fn parse_static(
	name: &str,
	path: PathBuf,
	source: &str,
) -> Result<RulePayload, serde_yaml::Error> {
	let (header, content) = parse_header(source)?;
	let (conditions, inferred_scopes) = normalize_conditions(header.condition.strings());
	let mut scopes = header
		.scope
		.strings()
		.into_iter()
		.flat_map(|value| split_scope_tokens(&value))
		.map(Str::from)
		.collect::<Vec<_>>();
	scopes.extend(inferred_scopes);
	dedupe(&mut scopes);
	Ok(RulePayload {
		name: Str::from(name),
		path,
		content: Str::from(content),
		globs: header
			.globs
			.strings()
			.into_iter()
			.flat_map(|value| {
				value
					.split(',')
					.map(str::trim)
					.filter(|v| !v.is_empty())
					.map(Str::from)
					.collect::<Vec<_>>()
			})
			.collect(),
		always_apply: header.always_apply,
		description: header
			.description
			.map(|value| Str::from(value.trim().to_owned())),
		conditions,
		ast_conditions: header
			.ast_condition
			.strings()
			.into_iter()
			.map(|value| Str::from(value.trim().to_owned()))
			.filter(|value| !value.is_empty())
			.collect(),
		scopes,
		interrupt_mode: header
			.interrupt_mode
			.as_deref()
			.and_then(|value| RuleInterruptMode::from_str(value).ok()),
	})
}

fn normalize_conditions(values: Vec<String>) -> (Vec<Str>, Vec<Str>) {
	let mut conditions = Vec::new();
	let mut scopes = Vec::new();
	for value in values
		.into_iter()
		.map(|value| value.trim().to_owned())
		.filter(|value| !value.is_empty())
	{
		if likely_file_glob(&value) {
			scopes.push(Str::from(format!("tool:edit({value})")));
			scopes.push(Str::from(format!("tool:write({value})")));
		} else {
			conditions.push(Str::from(normalize_pcre_inline_flags(&value)));
		}
	}
	if conditions.is_empty() && !scopes.is_empty() {
		conditions.push(Str::from(".*"));
	}
	(conditions, scopes)
}

fn likely_file_glob(value: &str) -> bool {
	!value
		.bytes()
		.any(|byte| matches!(byte, b'\\' | b'^' | b'$' | b'+' | b'|' | b'(' | b')'))
		&& value
			.bytes()
			.any(|byte| matches!(byte, b'?' | b'*' | b'[' | b']' | b'{' | b'}'))
		&& (value.contains('/') || (value.starts_with("*.") && !value[2..].contains('/')))
}

/// Splits comma-separated scopes without breaking nested tool glob arguments,
/// bracket expressions, braces, or quoted tokens.
pub fn split_scope_tokens(value: &str) -> Vec<String> {
	let mut output = Vec::new();
	let mut start = 0;
	let (mut paren, mut bracket, mut brace, mut quote) = (0_u16, 0_u16, 0_u16, None);
	let bytes = value.as_bytes();
	for (index, byte) in bytes.iter().copied().enumerate() {
		if let Some(active) = quote {
			if byte == active
				&& index
					.checked_sub(1)
					.is_none_or(|previous| bytes[previous] != b'\\')
			{
				quote = None;
			}
			continue;
		}
		match byte {
			b'\'' | b'"' => quote = Some(byte),
			b'(' => paren += 1,
			b')' => paren = paren.saturating_sub(1),
			b'[' => bracket += 1,
			b']' => bracket = bracket.saturating_sub(1),
			b'{' => brace += 1,
			b'}' => brace = brace.saturating_sub(1),
			b',' if paren == 0 && bracket == 0 && brace == 0 => {
				push_scope(&value[start..index], &mut output);
				start = index + 1;
			},
			_ => {},
		}
	}
	push_scope(&value[start..], &mut output);
	output
}

fn push_scope(value: &str, output: &mut Vec<String>) {
	let mut token = value.trim();
	if token.len() >= 2
		&& matches!(token.as_bytes()[0], b'\'' | b'"')
		&& token.as_bytes()[0] == token.as_bytes()[token.len() - 1]
	{
		token = token[1..token.len() - 1].trim();
	}
	if !token.is_empty() && !output.iter().any(|existing| existing == token) {
		output.push(token.to_owned());
	}
}

/// Converts a leading PCRE flag group into Rust-regex scoped flags. Supported
/// `i`, `m`, and `s` retain their meaning; unknown flags stay literal so the
/// runtime compiler can diagnose them instead of silently changing meaning.
pub fn normalize_pcre_inline_flags(pattern: &str) -> String {
	let Some(flags_end) = pattern
		.strip_prefix("(?")
		.and_then(|rest| rest.find(')').map(|end| end + 2))
	else {
		return pattern.to_owned();
	};
	let flags = &pattern[2..flags_end];
	if flags.is_empty() || !flags.bytes().all(|byte| matches!(byte, b'i' | b'm' | b's')) {
		return pattern.to_owned();
	}
	format!("(?{flags}:{})", &pattern[flags_end + 1..])
}

fn dedupe(values: &mut Vec<Str>) {
	let mut seen = BTreeSet::new();
	values.retain(|value| seen.insert(value.clone()));
}

/// Tests whether a rule's applicability globs include a path.
pub fn applies_to(rule: &RulePayload, path: &str) -> bool {
	rule.globs.is_empty()
		|| rule
			.globs
			.iter()
			.any(|glob| glob_matches(glob.as_str(), path))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn scope_split_preserves_nested_commas() {
		assert_eq!(split_scope_tokens("text, tool:edit(*.{rs,go}), 'thinking'"), vec![
			"text",
			"tool:edit(*.{rs,go})",
			"thinking"
		]);
	}

	#[test]
	fn glob_condition_becomes_edit_and_write_scope() {
		let (condition, scope) = normalize_conditions(vec!["*.rs".to_owned()]);
		assert_eq!(condition, vec![Str::from(".*")]);
		assert_eq!(scope, vec![Str::from("tool:edit(*.rs)"), Str::from("tool:write(*.rs)")]);
	}

	#[test]
	fn pcre_flags_are_scoped() {
		assert_eq!(normalize_pcre_inline_flags("(?im)^hello$"), "(?im:^hello$)");
	}
}
