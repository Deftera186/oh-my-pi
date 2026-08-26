//! Time-traveling stream-rule compilation and incremental matching.
//!
//! The matcher owns only deterministic stream enforcement. Discovery supplies
//! user and bundled declarations in precedence order; the application owns
//! settings and turns matches into durable interruption/replay events.

use std::{
	collections::{HashMap, HashSet},
	path::Path,
};

use globset::{Glob, GlobMatcher};
use omp_ast::{
	AstError, SupportLang,
	ops::{collect_matches, compile_search_patterns},
};
use omp_core::{Hash32, Str};
use regex::Regex;
use smallvec::SmallVec;
use thiserror::Error;

/// Stream carrying content currently evaluated by TTSR.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum StreamSource {
	/// Assistant-visible prose.
	Text,
	/// Assistant reasoning text.
	Thinking,
	/// Incrementally streamed tool arguments or a tool matcher snapshot.
	Tool,
}

/// Treatment of partial assistant output after an interrupting match.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum TtsrContextMode {
	/// Remove the abandoned partial assistant output before replay.
	Discard,
	/// Keep the abandoned partial assistant output in context.
	Keep,
}

/// Streams on which a match may interrupt generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "kebab-case")]
pub enum TtsrInterruptMode {
	/// Never interrupt; deliver the rule after the current operation.
	Never,
	/// Interrupt text and thinking streams only.
	ProseOnly,
	/// Interrupt tool-argument streams only.
	ToolOnly,
	/// Interrupt every matched stream.
	Always,
}

impl TtsrInterruptMode {
	/// Reports whether this mode interrupts `source`.
	pub const fn interrupts(self, source: StreamSource) -> bool {
		match self {
			Self::Never => false,
			Self::ProseOnly => matches!(source, StreamSource::Text | StreamSource::Thinking),
			Self::ToolOnly => matches!(source, StreamSource::Tool),
			Self::Always => true,
		}
	}
}

/// Session-level rule repetition policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "kebab-case")]
pub enum TtsrRepeatMode {
	/// A rule may be injected only once per session.
	Once,
	/// A rule may repeat after the configured completed-message gap.
	AfterGap,
}

/// Frozen TTSR settings used to compile one registry generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TtsrSettings {
	/// Whether stream rules are active.
	pub enabled:        bool,
	/// Partial-output treatment after interruption.
	pub context_mode:   TtsrContextMode,
	/// Default interrupt policy for rules without an override.
	pub interrupt_mode: TtsrInterruptMode,
	/// Session-level repetition policy.
	pub repeat_mode:    TtsrRepeatMode,
	/// Completed messages required before an after-gap rule repeats.
	pub repeat_gap:     u64,
	/// Whether bundled rules participate beneath user rules.
	pub builtin_rules:  bool,
	/// Rule names disabled before precedence resolution.
	pub disabled_rules: HashSet<Str>,
}

impl Default for TtsrSettings {
	fn default() -> Self {
		Self {
			enabled:        true,
			context_mode:   TtsrContextMode::Discard,
			interrupt_mode: TtsrInterruptMode::Always,
			repeat_mode:    TtsrRepeatMode::Once,
			repeat_gap:     10,
			builtin_rules:  true,
			disabled_rules: HashSet::new(),
		}
	}
}

/// One declarative stream rule after discovery/frontmatter normalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TtsrRule {
	/// Stable rule name used for precedence and repeat tracking.
	pub name:           Str,
	/// Reminder body injected when the rule lands.
	pub content:        Str,
	/// Regex conditions combined with OR semantics.
	pub conditions:     Vec<Str>,
	/// omp-ast patterns combined with OR semantics.
	pub ast_conditions: Vec<Str>,
	/// Stream scope tokens such as `text`, `thinking`, or `tool:edit(*.rs)`.
	pub scopes:         Vec<Str>,
	/// Global file applicability globs.
	pub globs:          Vec<Str>,
	/// Optional per-rule interrupt override.
	pub interrupt_mode: Option<TtsrInterruptMode>,
}

/// Borrowed context for one incremental stream match.
#[derive(Clone, Copy, Debug)]
pub struct TtsrMatchContext<'a> {
	/// Stream category.
	pub source:     StreamSource,
	/// Harness tool name for tool streams.
	pub tool_name:  Option<&'a str>,
	/// Candidate file paths supplied by the tool's matcher projection.
	pub file_paths: &'a [&'a str],
	/// Stable buffer identity, normally the tool-call ID.
	pub stream_key: Option<&'a str>,
}

impl TtsrMatchContext<'_> {
	fn buffer_key(&self) -> Str {
		if let Some(key) = self.stream_key.map(str::trim).filter(|key| !key.is_empty()) {
			return Str::new(key);
		}
		match self.source {
			StreamSource::Text => Str::new_static("text"),
			StreamSource::Thinking => Str::new_static("thinking"),
			StreamSource::Tool => self.tool_name.map_or_else(
				|| Str::new_static("tool"),
				|name| sf!("tool:{}", name.trim().to_ascii_lowercase()),
			),
		}
	}
}

/// Rule selected by the shared regex/AST matcher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TtsrMatch {
	/// Stable rule name.
	pub name:           Str,
	/// Reminder body.
	pub content:        Str,
	/// Effective interruption policy after applying the global fallback.
	pub interrupt_mode: TtsrInterruptMode,
}

/// Non-fatal declaration compilation diagnostic.
#[derive(Debug, Error)]
pub enum TtsrCompileError {
	/// One regex condition was invalid and was skipped.
	#[error("invalid TTSR regex for rule {rule}: {pattern}")]
	Regex {
		/// Rule being compiled.
		rule:    Str,
		/// Invalid pattern.
		pattern: Str,
		/// Regex parser failure.
		#[source]
		source:  regex::Error,
	},
	/// One path glob was invalid, making the declaration unsafe to broaden.
	#[error("invalid TTSR path glob for rule {rule}: {pattern}")]
	Glob {
		/// Rule being compiled.
		rule:    Str,
		/// Invalid glob.
		pattern: Str,
		/// Glob parser failure.
		#[source]
		source:  globset::Error,
	},
	/// Every declared stream scope was invalid or unreachable.
	#[error("TTSR rule {rule} has no reachable stream scope")]
	UnreachableScope {
		/// Rejected rule.
		rule: Str,
	},
	/// No valid regex or AST condition remained.
	#[error("TTSR rule {rule} has no valid condition")]
	NoCondition {
		/// Rejected rule.
		rule: Str,
	},
}

#[derive(Debug)]
struct ToolScope {
	tool_name:    Option<Str>,
	path_matcher: Option<GlobMatcher>,
}

#[derive(Debug)]
struct Scope {
	allow_text:     bool,
	allow_thinking: bool,
	allow_any_tool: bool,
	tools:          Vec<ToolScope>,
}

#[derive(Debug)]
struct Entry {
	rule:         TtsrRule,
	regex:        Vec<Regex>,
	scope:        Scope,
	global_globs: Vec<GlobMatcher>,
}

/// Compiled, generation-frozen stream matcher with repeat and snapshot guards.
pub struct TtsrRegistry {
	settings:           TtsrSettings,
	rules:              Vec<Entry>,
	injected_at:        HashMap<Str, u64>,
	buffers:            HashMap<Str, String>,
	last_ast_digests:   HashMap<Str, Hash32>,
	message_count:      u64,
	can_match_text:     bool,
	can_match_thinking: bool,
}

impl TtsrRegistry {
	/// Compiles user rules above bundled fallbacks and returns every rejected
	/// condition/declaration as a non-fatal diagnostic.
	///
	/// Names disabled by settings are removed before precedence resolution. A
	/// user declaration claims its name even when malformed, so a broken user
	/// override never silently re-enables the same bundled rule.
	pub fn from_layers(
		settings: TtsrSettings,
		user_rules: impl IntoIterator<Item = TtsrRule>,
		builtin_rules: impl IntoIterator<Item = TtsrRule>,
	) -> (Self, Vec<TtsrCompileError>) {
		let mut registry = Self {
			settings,
			rules: Vec::new(),
			injected_at: HashMap::new(),
			buffers: HashMap::new(),
			last_ast_digests: HashMap::new(),
			message_count: 0,
			can_match_text: false,
			can_match_thinking: false,
		};
		let mut diagnostics = Vec::new();
		if !registry.settings.enabled {
			return (registry, diagnostics);
		}
		let mut claimed = HashSet::new();
		for rule in user_rules {
			if registry.settings.disabled_rules.contains(&rule.name)
				|| !claimed.insert(rule.name.clone())
			{
				continue;
			}
			registry.compile_and_push(rule, &mut diagnostics);
		}
		if registry.settings.builtin_rules {
			for rule in builtin_rules {
				if registry.settings.disabled_rules.contains(&rule.name)
					|| !claimed.insert(rule.name.clone())
				{
					continue;
				}
				registry.compile_and_push(rule, &mut diagnostics);
			}
		}
		(registry, diagnostics)
	}

	fn compile_and_push(&mut self, rule: TtsrRule, diagnostics: &mut Vec<TtsrCompileError>) {
		let mut regex = Vec::new();
		for pattern in &rule.conditions {
			match Regex::new(pattern.as_str()) {
				Ok(compiled) => regex.push(compiled),
				Err(source) => diagnostics.push(TtsrCompileError::Regex {
					rule: rule.name.clone(),
					pattern: pattern.clone(),
					source,
				}),
			}
		}
		if regex.is_empty()
			&& rule
				.ast_conditions
				.iter()
				.all(|value| value.trim().is_empty())
		{
			diagnostics.push(TtsrCompileError::NoCondition { rule: rule.name });
			return;
		}
		let Some(scope) = compile_scope(&rule, diagnostics) else {
			return;
		};
		let mut global_globs = Vec::new();
		for pattern in &rule.globs {
			let pattern = pattern.trim();
			if pattern.is_empty() {
				continue;
			}
			match Glob::new(pattern.as_str()) {
				Ok(glob) => global_globs.push(glob.compile_matcher()),
				Err(source) => {
					diagnostics.push(TtsrCompileError::Glob {
						rule: rule.name.clone(),
						pattern: Str::new(pattern),
						source,
					});
					return;
				},
			}
		}
		self.can_match_text |= scope.allow_text;
		self.can_match_thinking |= scope.allow_thinking;
		self.rules.push(Entry { rule, regex, scope, global_globs });
	}

	/// Returns registered declarations in deterministic precedence order.
	pub fn rules(&self) -> impl ExactSizeIterator<Item = &TtsrRule> + DoubleEndedIterator {
		self.rules.iter().map(|entry| &entry.rule)
	}

	/// Reports whether at least one active rule has a structural condition.
	pub fn has_ast_rules(&self) -> bool {
		self.settings.enabled
			&& self
				.rules
				.iter()
				.any(|entry| !entry.rule.ast_conditions.is_empty())
	}

	/// Appends one raw stream delta to its isolated buffer and runs regex rules.
	pub fn check_delta(
		&mut self,
		delta: &str,
		context: TtsrMatchContext<'_>,
	) -> SmallVec<TtsrMatch, 4> {
		if (context.source == StreamSource::Text && !self.can_match_text)
			|| (context.source == StreamSource::Thinking && !self.can_match_thinking)
		{
			return SmallVec::new();
		}
		let key = context.buffer_key();
		let buffer = self.buffers.entry(key).or_default();
		buffer.push_str(delta);
		match_regex_entries(
			&self.settings,
			&self.rules,
			&self.injected_at,
			self.message_count,
			buffer,
			context,
		)
	}

	/// Replaces one stream buffer with a tool-provided normalized matcher
	/// snapshot and runs regex rules against the full source projection.
	pub fn check_snapshot(
		&mut self,
		snapshot: &str,
		context: TtsrMatchContext<'_>,
	) -> SmallVec<TtsrMatch, 4> {
		self
			.buffers
			.insert(context.buffer_key(), snapshot.to_owned());
		match_regex_entries(
			&self.settings,
			&self.rules,
			&self.injected_at,
			self.message_count,
			snapshot,
			context,
		)
	}

	/// Runs omp-ast conditions over a normalized tool snapshot. Identical
	/// consecutive snapshots for the same stream key are skipped by content
	/// digest before parsing.
	pub fn check_ast_snapshot(
		&mut self,
		snapshot: &str,
		context: TtsrMatchContext<'_>,
	) -> Result<SmallVec<TtsrMatch, 4>, AstError> {
		if !self.settings.enabled || context.source != StreamSource::Tool {
			return Ok(SmallVec::new());
		}
		let Some(language) = derive_language(context.file_paths) else {
			return Ok(SmallVec::new());
		};
		if !self.rules.iter().any(|entry| {
			!entry.rule.ast_conditions.is_empty()
				&& self.can_trigger(entry.rule.name.as_str())
				&& matches_scope(entry, context)
				&& matches_global_paths(entry, context.file_paths)
		}) {
			return Ok(SmallVec::new());
		}
		let key = context.buffer_key();
		let digest = Hash32::sum(snapshot.as_bytes());
		if self.last_ast_digests.get(&key) == Some(&digest) {
			return Ok(SmallVec::new());
		}
		self.last_ast_digests.insert(key, digest);

		let mut matches = SmallVec::new();
		for entry in &self.rules {
			if entry.rule.ast_conditions.is_empty()
				|| !self.can_trigger(entry.rule.name.as_str())
				|| !matches_scope(entry, context)
				|| !matches_global_paths(entry, context.file_paths)
			{
				continue;
			}
			let mut matched = false;
			for pattern in &entry.rule.ast_conditions {
				if pattern.trim().is_empty() {
					continue;
				}
				let compiled = compile_search_patterns(pattern.as_str(), language)
					.map_err(|source| AstError::InvalidPattern { source })?;
				if !collect_matches(snapshot, language, &compiled).is_empty() {
					matched = true;
					break;
				}
			}
			if matched {
				matches.push(to_match(entry, self.settings.interrupt_mode));
			}
		}
		Ok(matches)
	}

	fn can_trigger(&self, name: &str) -> bool {
		let Some(last) = self.injected_at.get(name) else {
			return true;
		};
		match self.settings.repeat_mode {
			TtsrRepeatMode::Once => false,
			TtsrRepeatMode::AfterGap => {
				self.message_count.saturating_sub(*last) >= self.settings.repeat_gap
			},
		}
	}

	/// Marks delivered rule names as injected at the current message count.
	pub fn mark_injected<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
		for name in names {
			let name = name.trim();
			if !name.is_empty() && self.rules.iter().any(|entry| entry.rule.name == name) {
				self.injected_at.insert(Str::new(name), self.message_count);
			}
		}
	}

	/// Advances the repeat-after-gap counter after a completed assistant turn.
	pub const fn advance_message(&mut self) {
		self.message_count = self.message_count.saturating_add(1);
	}

	/// Clears text, thinking, tool, and structural snapshot buffers at turn
	/// start.
	pub fn reset_streams(&mut self) {
		self.buffers.clear();
		self.last_ast_digests.clear();
	}

	/// Returns the frozen settings for interruption and context handling.
	pub const fn settings(&self) -> &TtsrSettings {
		&self.settings
	}
}

fn match_regex_entries(
	settings: &TtsrSettings,
	rules: &[Entry],
	injected_at: &HashMap<Str, u64>,
	message_count: u64,
	buffer: &str,
	context: TtsrMatchContext<'_>,
) -> SmallVec<TtsrMatch, 4> {
	if !settings.enabled {
		return SmallVec::new();
	}
	rules
		.iter()
		.filter(|entry| {
			can_trigger(settings, injected_at, message_count, entry.rule.name.as_str())
				&& matches_scope(entry, context)
				&& matches_global_paths(entry, context.file_paths)
				&& entry
					.regex
					.iter()
					.any(|condition| condition.is_match(buffer))
		})
		.map(|entry| to_match(entry, settings.interrupt_mode))
		.collect()
}

fn can_trigger(
	settings: &TtsrSettings,
	injected_at: &HashMap<Str, u64>,
	message_count: u64,
	name: &str,
) -> bool {
	let Some(last) = injected_at.get(name) else {
		return true;
	};
	match settings.repeat_mode {
		TtsrRepeatMode::Once => false,
		TtsrRepeatMode::AfterGap => message_count.saturating_sub(*last) >= settings.repeat_gap,
	}
}

fn to_match(entry: &Entry, default_mode: TtsrInterruptMode) -> TtsrMatch {
	TtsrMatch {
		name:           entry.rule.name.clone(),
		content:        entry.rule.content.clone(),
		interrupt_mode: entry.rule.interrupt_mode.unwrap_or(default_mode),
	}
}

fn compile_scope(rule: &TtsrRule, diagnostics: &mut Vec<TtsrCompileError>) -> Option<Scope> {
	if rule.scopes.is_empty() {
		return Some(Scope {
			allow_text:     true,
			allow_thinking: false,
			allow_any_tool: true,
			tools:          Vec::new(),
		});
	}
	let mut scope = Scope {
		allow_text:     false,
		allow_thinking: false,
		allow_any_tool: false,
		tools:          Vec::new(),
	};
	for raw in &rule.scopes {
		let token = raw.trim();
		if token.is_empty() {
			continue;
		}
		if token.eq_ignore_ascii_case("text") {
			scope.allow_text = true;
			continue;
		}
		if token.eq_ignore_ascii_case("thinking") {
			scope.allow_thinking = true;
			continue;
		}
		if token.eq_ignore_ascii_case("tool") || token.eq_ignore_ascii_case("toolcall") {
			scope.allow_any_tool = true;
			continue;
		}
		let Some((tool_name, path_pattern)) = parse_tool_scope(token.as_str()) else {
			continue;
		};
		let path_matcher = if let Some(pattern) = path_pattern {
			match Glob::new(pattern) {
				Ok(glob) => Some(glob.compile_matcher()),
				Err(source) => {
					diagnostics.push(TtsrCompileError::Glob {
						rule: rule.name.clone(),
						pattern: Str::new(pattern),
						source,
					});
					return None;
				},
			}
		} else {
			None
		};
		if tool_name.is_none() && path_matcher.is_none() {
			scope.allow_any_tool = true;
		} else {
			scope
				.tools
				.push(ToolScope { tool_name: tool_name.map(Str::new), path_matcher });
		}
	}
	if scope.allow_text || scope.allow_thinking || scope.allow_any_tool || !scope.tools.is_empty() {
		Some(scope)
	} else {
		diagnostics.push(TtsrCompileError::UnreachableScope { rule: rule.name.clone() });
		None
	}
}

fn parse_tool_scope(token: &str) -> Option<(Option<&str>, Option<&str>)> {
	let (head, path) = if let Some(open) = token.find('(') {
		if !token.ends_with(')') {
			return None;
		}
		(&token[..open], Some(token[open + 1..token.len() - 1].trim()))
	} else {
		(token, None)
	};
	let head = head.trim();
	let tool = if head.eq_ignore_ascii_case("tool") {
		None
	} else if head
		.get(..5)
		.is_some_and(|prefix| prefix.eq_ignore_ascii_case("tool:"))
	{
		let name = head[5..].trim();
		(!name.is_empty()).then_some(name)
	} else if !head.is_empty()
		&& head
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
	{
		Some(head)
	} else {
		return None;
	};
	Some((tool, path.filter(|value| !value.is_empty())))
}

fn matches_scope(entry: &Entry, context: TtsrMatchContext<'_>) -> bool {
	match context.source {
		StreamSource::Text => entry.scope.allow_text,
		StreamSource::Thinking => entry.scope.allow_thinking,
		StreamSource::Tool if entry.scope.allow_any_tool => true,
		StreamSource::Tool => {
			let tool_name = context.tool_name.map(str::trim);
			entry.scope.tools.iter().any(|scope| {
				scope.tool_name.as_ref().is_none_or(|required| {
					tool_name.is_some_and(|actual| required.as_str().eq_ignore_ascii_case(actual))
				}) && scope
					.path_matcher
					.as_ref()
					.is_none_or(|glob| matches_paths(glob, context.file_paths))
			})
		},
	}
}

fn matches_global_paths(entry: &Entry, file_paths: &[&str]) -> bool {
	entry.global_globs.is_empty()
		|| entry
			.global_globs
			.iter()
			.any(|glob| matches_paths(glob, file_paths))
}

fn matches_paths(glob: &GlobMatcher, file_paths: &[&str]) -> bool {
	file_paths.iter().any(|path| {
		let normalized = path.replace('\\', "/");
		glob.is_match(normalized.as_str())
			|| normalized
				.rsplit_once('/')
				.is_some_and(|(_, basename)| glob.is_match(basename))
	})
}

fn derive_language(file_paths: &[&str]) -> Option<SupportLang> {
	file_paths
		.iter()
		.find_map(|path| SupportLang::from_path(Path::new(path)))
}
