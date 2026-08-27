//! Session-title generation policy and tiny-model consumer.

use std::{borrow::Cow, future::Future, pin::Pin};

use omp_agent::prompt_assets::{PromptAssetId, prompt_asset};
use omp_core::{FastHashMap, Str, StrMut};
#[cfg(feature = "local-text")]
use omp_inference::local::{
	LocalCancellation,
	text::{ChatMessage, ChatRole, GenerationOptions, TextAdapter},
};
use omp_storage::transcript::TitleSource;

/// Online role chain. Role resolution performs one completion using the first
/// available assignment; it never issues one request per role.
pub const ONLINE_TITLE_ROLE_CHAIN: [&str; 3] = ["tiny", "commit", "smol"];

const MAX_TINY_MESSAGE_CHARS: usize = 2_000;
const MIN_STRIPPED_TITLE_CHARS: usize = 12;
const COMMON_TITLE_ACRONYMS: &[&str] = &[
	"API", "CLI", "CPU", "CRUD", "CSS", "DNS", "ETL", "GPU", "HTML", "HTTP", "HTTPS", "ID", "JSON",
	"LLM", "REST", "SDK", "SSH", "TCP", "TLS", "TUI", "UI", "URI", "URL", "UX", "XML", "YAML",
];

const FILLER: &[&str] = &[
	"hi",
	"hii",
	"hiii",
	"hiya",
	"hey",
	"heya",
	"hello",
	"helo",
	"hullo",
	"yo",
	"ya",
	"sup",
	"wassup",
	"whatsup",
	"howdy",
	"greetings",
	"hola",
	"ciao",
	"aloha",
	"gm",
	"gn",
	"good",
	"morning",
	"afternoon",
	"evening",
	"night",
	"day",
	"thanks",
	"thank",
	"thx",
	"ty",
	"tysm",
	"cheers",
	"please",
	"pls",
	"plz",
	"ok",
	"okay",
	"okey",
	"k",
	"kk",
	"yep",
	"yes",
	"yeah",
	"yup",
	"nope",
	"no",
	"nah",
	"sure",
	"cool",
	"nice",
	"great",
	"awesome",
	"perfect",
	"lol",
	"lmao",
	"haha",
	"hehe",
	"test",
	"tests",
	"testing",
	"ping",
	"pong",
	"there",
	"you",
	"u",
	"hmm",
	"hmmm",
	"um",
	"uh",
	"so",
	"well",
	"anyway",
];

/// One online completion boundary. Implementations resolve
/// [`ONLINE_TITLE_ROLE_CHAIN`] once and perform exactly one background request.
pub trait OnlineTitleCompletion: Send + Sync {
	/// Returns raw visible completion text. Errors are fail-open so an untitled
	/// session retries after the next eligible user message.
	fn complete_title<'a>(
		&'a self,
		roles: &'static [&'static str],
		system_prompt: &'a str,
		input: &'a str,
	) -> Pin<Box<dyn Future<Output = Result<Option<Str>, Str>> + Send + 'a>>;
}

/// Durable title authority projected from `Kind::Title` events.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionTitleState {
	/// Current projected title.
	pub title:  Option<Str>,
	/// Authority that assigned the current title.
	pub source: Option<TitleSource>,
}

impl SessionTitleState {
	/// Returns whether this turn may start title generation. User titles are
	/// immutable to automatic refreshes; assistant titles refresh after replans.
	pub fn should_generate(&self, input: &str, replanned: bool) -> bool {
		if self.source == Some(TitleSource::User) || is_low_signal_title_input(input) {
			return false;
		}
		self.title.is_none() || (replanned && self.source == Some(TitleSource::Assistant))
	}

	/// Projects an accepted generated title without overriding a user title.
	pub fn accept_generated(&mut self, title: Str) -> bool {
		if self.source == Some(TitleSource::User) {
			return false;
		}
		self.title = Some(title);
		self.source = Some(TitleSource::Assistant);
		true
	}

	/// Runs the online tiny→commit→smol lane when this state admits automatic
	/// generation, then projects its accepted assistant title.
	pub async fn generate_online(
		&mut self,
		completion: &dyn OnlineTitleCompletion,
		input: &str,
		system_prompt: &str,
		replanned: bool,
	) -> bool {
		if !self.should_generate(input, replanned) {
			return false;
		}
		generate_online_title(completion, input, system_prompt)
			.await
			.is_some_and(|title| self.accept_generated(title))
	}

	/// Runs the configured local tiny-text adapter when this state admits
	/// automatic generation, then projects its accepted assistant title.
	#[cfg(feature = "local-text")]
	pub fn generate_local(
		&mut self,
		adapter: &TextAdapter,
		input: &str,
		system_prompt: &str,
		replanned: bool,
		cancel: &LocalCancellation,
	) -> bool {
		if !self.should_generate(input, replanned) {
			return false;
		}
		generate_local_title(adapter, input, system_prompt, cancel)
			.is_some_and(|title| self.accept_generated(title))
	}
}

/// Runs the landed local tiny-text adapter with bounded title options.
#[cfg(feature = "local-text")]
pub fn generate_local_title(
	adapter: &TextAdapter,
	input: &str,
	system_prompt: &str,
	cancel: &LocalCancellation,
) -> Option<Str> {
	let formatted = prepare_title_user_message(input)?;
	let messages =
		[ChatMessage { role: ChatRole::System, content: Str::new(system_prompt) }, ChatMessage {
			role:    ChatRole::User,
			content: formatted,
		}];
	let generated = adapter
		.generate(&messages, GenerationOptions::title(), cancel, |_| true)
		.ok()?;
	normalize_generated_title(generated.content.as_str(), Some(input))
}

/// Resolves the tiny→commit→smol lane once and normalizes its one completion.
pub async fn generate_online_title(
	completion: &dyn OnlineTitleCompletion,
	input: &str,
	system_prompt: &str,
) -> Option<Str> {
	let formatted = prepare_title_user_message(input)?;
	completion
		.complete_title(&ONLINE_TITLE_ROLE_CHAIN, system_prompt, formatted.as_str())
		.await
		.ok()
		.flatten()
		.and_then(|value| normalize_generated_title(value.as_str(), Some(input)))
}

/// Collapses a user-invoked skill expansion back to its stable title chip.
pub fn skill_title_input(name: &str, args: &str) -> Str {
	let name = name.trim();
	let args = args.trim();
	match (name.is_empty(), args.is_empty()) {
		(false, false) => Str::from(format!("/skill:{name} {args}")),
		(false, true) => Str::from(format!("/skill:{name}")),
		(true, false) => Str::new(args),
		(true, true) => Str::default(),
	}
}

/// Returns the embedded title system prompt or a custom prompt followed by the
/// mandatory marker instruction.
pub fn title_system_prompt(override_text: Option<&str>) -> Str {
	let Some(override_text) = override_text else {
		return Str::new_static(prompt_asset(PromptAssetId::TitleSystem).content);
	};
	let marker = prompt_asset(PromptAssetId::TitleMarkerInstruction).content;
	let mut prompt = StrMut::with_capacity(override_text.len() + 2 + marker.len());
	prompt.push_str(override_text);
	prompt.push_str("\n\n");
	prompt.push_str(marker);
	prompt.freeze()
}

/// Renders the canonical ephemeral recap request with optional current goal and
/// task context.
pub fn recap_user_prompt(goal: Option<&str>, task: Option<&str>) -> Str {
	let goal = goal.filter(|value| !value.trim().is_empty());
	let task = task.filter(|value| !value.trim().is_empty());
	let template = prompt_asset(PromptAssetId::RecapUser).content;
	let mut output =
		StrMut::with_capacity(template.len() + goal.map_or(0, str::len) + task.map_or(0, str::len));
	for line in template.split_inclusive('\n') {
		match line.strip_suffix('\n').unwrap_or(line) {
			"Overall goal: {{goal}}" => {
				if let Some(goal) = goal {
					output.push_str("Overall goal: ");
					output.push_str(goal);
					if line.ends_with('\n') {
						output.push('\n');
					}
				}
			},
			"Active task: {{task}}" => {
				if let Some(task) = task {
					output.push_str("Active task: ");
					output.push_str(task);
					if line.ends_with('\n') {
						output.push('\n');
					}
				}
			},
			_ => output.push_str(line),
		}
	}
	output.freeze()
}

/// Cleans and bounds a user message, then wraps it in the structural title
/// envelope.
pub fn format_title_user_message(input: &str) -> Str {
	format_cleaned_title_user_message(clean_tiny_message(input).as_str())
}

fn prepare_title_user_message(input: &str) -> Option<Str> {
	let cleaned = clean_tiny_message(input);
	(!is_low_signal_cleaned(cleaned.as_str()))
		.then(|| format_cleaned_title_user_message(cleaned.as_str()))
}

fn format_cleaned_title_user_message(cleaned: &str) -> Str {
	let preprocessed = truncate_tiny_message(cleaned);
	let mut message = StrMut::with_capacity(preprocessed.len() + 16);
	message.push_str("<user>\n");
	message.push_str(preprocessed.as_str());
	message.push_str("\n</user>");
	message.freeze()
}

/// Deterministically rejects greetings, acknowledgements, bare numbers, and
/// punctuation-only input before any model request.
pub fn is_low_signal_title_input(input: &str) -> bool {
	is_low_signal_cleaned(clean_tiny_message(input).as_str())
}

fn is_low_signal_cleaned(cleaned: &str) -> bool {
	for token in title_words(cleaned) {
		if !token.chars().all(|character| character.is_ascii_digit())
			&& !FILLER
				.iter()
				.any(|filler| token.eq_ignore_ascii_case(filler))
		{
			return false;
		}
	}
	true
}

/// Normalizes marker/plain/JSON title responses and rejects leaked reasoning,
/// overlong answers, punctuation junk, and the `none` sentinel.
pub fn normalize_generated_title(raw: &str, source_text: Option<&str>) -> Option<Str> {
	let visible = extract_visible_title(raw)?;
	let first_line = visible.trim().lines().next()?.trim();
	let mut title = unwrap_json_title(first_line);
	title = strip_quote_edges(title).trim();
	if is_self_closing_title(title) {
		return None;
	}
	title = strip_ascii_case_prefix(title, "<title>").unwrap_or(title);
	title = strip_ascii_case_suffix(title, "</title>").unwrap_or(title);
	title = strip_quote_edges(title);
	if matches!(title.chars().next_back(), Some('.' | '!' | '?')) {
		title = &title[..title.len() - 1];
	}
	title = title.trim();
	if title.is_empty() || title.eq_ignore_ascii_case("none") {
		return None;
	}
	let words = title_words(title).count();
	if words == 0 || title.chars().count() > 80 || words > 12 {
		return None;
	}
	Some(source_text.map_or_else(|| Str::new(title), |source| reconcile_title_casing(title, source)))
}

/// Reconciles title-token casing against the source message without re-shouting
/// ordinary all-caps prose.
pub fn reconcile_title_casing(title: &str, source: &str) -> Str {
	let source_tokens: Vec<&str> = title_words(source).collect();
	let shouty = source_tokens
		.windows(2)
		.any(|pair| pair.iter().all(|token| is_all_caps_word(token)));
	let mut distinctive = FastHashMap::<String, &str>::default();
	let mut acronyms = FastHashMap::<String, &str>::default();
	for &token in &source_tokens {
		if is_distinctive_casing(token) {
			distinctive.entry(token.to_lowercase()).or_insert(token);
		} else if !shouty && is_all_caps_acronym(token) {
			acronyms.entry(token.to_lowercase()).or_insert(token);
		}
	}
	let mut output = StrMut::with_capacity(title.len());
	let mut cursor = 0;
	for (start, token) in title_word_spans(title) {
		output.push_str(&title[cursor..start]);
		let lower = token.to_lowercase();
		let replacement = if source_tokens.contains(&token) {
			token
		} else if let Some(source_token) = distinctive.get(lower.as_str()) {
			*source_token
		} else if is_title_cased_artifact(token) {
			acronyms.get(lower.as_str()).copied().unwrap_or(token)
		} else if is_camel_artifact(token) {
			lower.as_str()
		} else {
			token
		};
		output.push_str(replacement);
		cursor = start + token.len();
	}
	output.push_str(&title[cursor..]);
	output.freeze()
}

fn clean_tiny_message(message: &str) -> Str {
	let without_ansi = strip_ansi(message);
	let without_xml = strip_xml_blocks(without_ansi.as_ref());
	let shortened = shorten_hashes(without_xml.as_ref());
	let stripped = strip_code_blocks(shortened.as_ref());
	if stripped.chars().count() >= MIN_STRIPPED_TITLE_CHARS {
		Str::from(stripped)
	} else {
		Str::new(shortened.as_ref())
	}
}

fn strip_ansi(message: &str) -> Cow<'_, str> {
	let bytes = message.as_bytes();
	let mut output = None::<String>;
	let mut cursor = 0;
	let mut index = 0;
	while index + 2 < bytes.len() {
		if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
			index += 1;
			continue;
		}
		let mut end = index + 2;
		while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
			end += 1;
		}
		if end >= bytes.len() || bytes[end] != b'm' {
			index += 1;
			continue;
		}
		let target = output.get_or_insert_with(|| String::with_capacity(message.len()));
		target.push_str(&message[cursor..index]);
		cursor = end + 1;
		index = cursor;
	}
	match output {
		Some(mut output) => {
			output.push_str(&message[cursor..]);
			Cow::Owned(output)
		},
		None => Cow::Borrowed(message),
	}
}

fn strip_xml_blocks(message: &str) -> Cow<'_, str> {
	let mut output = None::<String>;
	let mut cursor = 0;
	let mut search = 0;
	while let Some(relative) = message[search..].find('<') {
		let start = search + relative;
		let bytes = message.as_bytes();
		let Some(first) = bytes.get(start + 1).copied() else {
			break;
		};
		if !first.is_ascii_alphabetic() {
			search = start + 1;
			continue;
		}
		let mut tag_end = start + 2;
		while bytes
			.get(tag_end)
			.is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-'))
		{
			tag_end += 1;
		}
		let open_end = match bytes.get(tag_end).copied() {
			Some(b'>') => tag_end + 1,
			Some(_)
				if message[tag_end..]
					.chars()
					.next()
					.is_some_and(char::is_whitespace) =>
			{
				let Some(relative_end) = message[tag_end..].find('>') else {
					break;
				};
				tag_end + relative_end + 1
			},
			_ => {
				search = tag_end;
				continue;
			},
		};
		let tag = &message[start + 1..tag_end];
		let Some(close_start) = message[open_end..]
			.match_indices("</")
			.find_map(|(relative, _)| {
				let close_start = open_end + relative;
				let name_start = close_start + 2;
				let name_end = name_start + tag.len();
				(message.get(name_start..name_end) == Some(tag)
					&& message.as_bytes().get(name_end) == Some(&b'>'))
				.then_some(close_start)
			})
		else {
			search = open_end;
			continue;
		};
		let close_end = close_start + tag.len() + 3;
		let target = output.get_or_insert_with(|| String::with_capacity(message.len()));
		target.push_str(&message[cursor..start]);
		target.push(' ');
		cursor = close_end;
		search = close_end;
	}
	match output {
		Some(mut output) => {
			output.push_str(&message[cursor..]);
			Cow::Owned(output)
		},
		None => Cow::Borrowed(message),
	}
}

fn shorten_hashes(message: &str) -> Cow<'_, str> {
	let bytes = message.as_bytes();
	let mut output = None::<String>;
	let mut cursor = 0;
	let mut index = 0;
	while index < bytes.len() {
		if !bytes[index].is_ascii_hexdigit() {
			index += 1;
			continue;
		}
		let start = index;
		while index < bytes.len() && bytes[index].is_ascii_hexdigit() {
			index += 1;
		}
		let bounded_before = start == 0 || !is_ascii_word(bytes[start - 1]);
		let bounded_after = index == bytes.len() || !is_ascii_word(bytes[index]);
		if index - start < 12 || !bounded_before || !bounded_after {
			continue;
		}
		let target = output.get_or_insert_with(|| String::with_capacity(message.len()));
		target.push_str(&message[cursor..start]);
		target.push_str(&message[start..start + 7]);
		cursor = index;
	}
	match output {
		Some(mut output) => {
			output.push_str(&message[cursor..]);
			Cow::Owned(output)
		},
		None => Cow::Borrowed(message),
	}
}

const fn is_ascii_word(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || byte == b'_'
}

fn strip_code_blocks(message: &str) -> String {
	let mut without_fences = String::with_capacity(message.len());
	let mut cursor = 0;
	while let Some((start, open_end)) = find_fence(message, cursor) {
		without_fences.push_str(&message[cursor..start]);
		without_fences.push(' ');
		cursor = find_fence(message, open_end).map_or(message.len(), |(_, close_end)| close_end);
	}
	without_fences.push_str(&message[cursor..]);

	let mut normalized = String::with_capacity(without_fences.len());
	let mut horizontal_space = false;
	let mut newlines = 0;
	for character in without_fences.chars() {
		if matches!(character, ' ' | '\t') {
			if !horizontal_space {
				normalized.push(' ');
			}
			horizontal_space = true;
			newlines = 0;
		} else {
			horizontal_space = false;
			if character == '\n' {
				if newlines < 2 {
					normalized.push(character);
				}
				newlines += 1;
			} else {
				newlines = 0;
				normalized.push(character);
			}
		}
	}
	normalized.trim().to_owned()
}

fn find_fence(message: &str, from: usize) -> Option<(usize, usize)> {
	let bytes = message.as_bytes();
	let mut index = from;
	while index + 2 < bytes.len() {
		if bytes[index..index + 3] != [b'`'; 3] {
			index += 1;
			continue;
		}
		let start = index;
		index += 3;
		while bytes.get(index) == Some(&b'`') {
			index += 1;
		}
		return Some((start, index));
	}
	None
}

fn truncate_tiny_message(message: &str) -> Str {
	let length = message.chars().count();
	if length <= MAX_TINY_MESSAGE_CHARS {
		return Str::new(message);
	}
	let (head_chars, tail_chars, omitted) =
		(0..2).fold((0, 0, length - MAX_TINY_MESSAGE_CHARS), |(_, _, omitted), _| {
			let marker = format!("\n[… {omitted} chars omitted …]\n");
			let kept_chars = MAX_TINY_MESSAGE_CHARS.saturating_sub(marker.chars().count());
			let head_chars = (kept_chars * 2).div_ceil(3);
			let tail_chars = kept_chars - head_chars;
			(head_chars, tail_chars, length - head_chars - tail_chars)
		});
	let marker = format!("\n[… {omitted} chars omitted …]\n");
	let head_end = byte_index_at(message, head_chars);
	let tail_start = byte_index_at(message, length - tail_chars);
	let mut output =
		StrMut::with_capacity(head_end + marker.len() + message.len().saturating_sub(tail_start));
	output.push_str(&message[..head_end]);
	output.push_str(&marker);
	output.push_str(&message[tail_start..]);
	output.freeze()
}

fn byte_index_at(message: &str, characters: usize) -> usize {
	message
		.char_indices()
		.nth(characters)
		.map_or(message.len(), |(index, _)| index)
}

fn title_words(input: &str) -> impl Iterator<Item = &str> {
	title_word_spans(input).map(|(_, word)| word)
}

fn title_word_spans(input: &str) -> impl Iterator<Item = (usize, &str)> {
	let mut cursor = 0;
	std::iter::from_fn(move || {
		let tail = input.get(cursor..)?;
		let start_relative = tail
			.char_indices()
			.find_map(|(index, character)| character.is_alphanumeric().then_some(index))?;
		let start = cursor + start_relative;
		let end = input[start..]
			.char_indices()
			.find_map(|(index, character)| (!character.is_alphanumeric()).then_some(start + index))
			.unwrap_or(input.len());
		cursor = end;
		Some((start, &input[start..end]))
	})
}

fn strip_quote_edges(mut title: &str) -> &str {
	if matches!(title.chars().next(), Some('"' | '\'')) {
		title = &title[1..];
	}
	if matches!(title.chars().next_back(), Some('"' | '\'')) {
		title = &title[..title.len() - 1];
	}
	title.trim()
}

fn is_self_closing_title(title: &str) -> bool {
	let lower = title.to_ascii_lowercase();
	let Some(inner) = lower.strip_prefix("<title") else {
		return false;
	};
	inner
		.strip_suffix('>')
		.is_some_and(|inner| inner.trim() == "/")
}

fn strip_ascii_case_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
	let candidate = value.get(..prefix.len())?;
	candidate
		.eq_ignore_ascii_case(prefix)
		.then(|| &value[prefix.len()..])
}

fn strip_ascii_case_suffix<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
	let start = value.len().checked_sub(suffix.len())?;
	value
		.get(start..)
		.is_some_and(|candidate| candidate.eq_ignore_ascii_case(suffix))
		.then(|| &value[..start])
}

fn is_distinctive_casing(token: &str) -> bool {
	token.chars().any(char::is_lowercase)
		&& token
			.chars()
			.zip(token.chars().skip(1))
			.any(|(previous, current)| previous.is_alphabetic() && current.is_uppercase())
}

fn is_all_caps_word(token: &str) -> bool {
	token
		.chars()
		.filter(|character| character.is_alphabetic())
		.count()
		>= 2 && token.chars().any(char::is_uppercase)
		&& !token.chars().any(char::is_lowercase)
}

fn is_all_caps_acronym(token: &str) -> bool {
	if !is_all_caps_word(token) {
		return false;
	}
	let upper = token.to_uppercase();
	COMMON_TITLE_ACRONYMS.contains(&upper.as_str())
		|| token.chars().any(char::is_numeric)
		|| !upper
			.chars()
			.any(|character| matches!(character, 'A' | 'E' | 'I' | 'O' | 'U'))
}

fn is_title_cased_artifact(token: &str) -> bool {
	token.chars().next().is_some_and(char::is_uppercase)
		&& token.chars().any(char::is_lowercase)
		&& !token.chars().skip(1).any(char::is_uppercase)
}

fn is_camel_artifact(token: &str) -> bool {
	token.chars().next().is_some_and(char::is_lowercase)
		&& token.chars().skip(1).any(char::is_uppercase)
}

fn extract_visible_title(raw: &str) -> Option<&str> {
	let mut rest = raw.trim();
	loop {
		let lower = rest.to_ascii_lowercase();
		let envelope =
			[("<think>", "</think>"), ("<thinking>", "</thinking>"), ("<reasoning>", "</reasoning>")]
				.into_iter()
				.find(|(open, _)| lower.starts_with(open));
		let Some((_, close)) = envelope else { break };
		let end = lower.find(close)? + close.len();
		rest = rest.get(end..)?.trim_start();
	}
	let lower = rest.to_ascii_lowercase();
	if lower.starts_with("```thinking") || lower.starts_with("```reasoning") {
		let end = rest.get(3..)?.find("```")? + 6;
		return extract_visible_title(rest.get(end..)?.trim_start());
	}
	if let Some(start) = lower.find("<title>") {
		let content = rest.get(start + "<title>".len()..)?;
		let lower_content = content.to_ascii_lowercase();
		let end = lower_content.find("</title>").unwrap_or(content.len());
		return content.get(..end);
	}
	if lower.contains("thinking process:") || lower.contains("reasoning process:") {
		return None;
	}
	Some(rest.lines().next().unwrap_or_default())
}

fn unwrap_json_title(candidate: &str) -> &str {
	let mut text = candidate.trim();
	if let Some(unfenced) = text
		.strip_prefix("```json")
		.or_else(|| text.strip_prefix("```"))
	{
		text = unfenced.trim();
	}
	if let Some(unfenced) = text.strip_suffix("```") {
		text = unfenced.trim();
	}
	let Some(key) = text.find("\"title\"") else {
		return text;
	};
	let Some(colon) = text.get(key + 7..).and_then(|tail| tail.find(':')) else {
		return text;
	};
	let value = text
		.get(key + 7 + colon + 1..)
		.unwrap_or_default()
		.trim_start();
	let Some(value) = value.strip_prefix('\"') else {
		return text;
	};
	let mut escaped = false;
	for (index, character) in value.char_indices() {
		if character == '\"' && !escaped {
			return &value[..index];
		}
		escaped = character == '\\' && !escaped;
		if character != '\\' {
			escaped = false;
		}
	}
	text
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn preprocessing_strips_noise_and_preserves_code_only_fallback() {
		let cases = [
			("\u{1b}[31mfix parser\u{1b}[0m", "fix parser"),
			("before <tool kind=\"call\">ignore me</tool> after", "before after"),
			("inspect abcdef1234567890 now", "inspect abcdef1 now"),
			("please fix parser\n```rust\npanic!()\n```\nsoon", "please fix parser\n \nsoon"),
		];
		for (input, expected) in cases {
			assert_eq!(clean_tiny_message(input), Str::new(expected), "{input:?}");
		}
		let code_only = "```\nhello\n```";
		assert_eq!(clean_tiny_message(code_only), Str::new(code_only));
	}

	#[test]
	fn preprocessing_middle_truncation_counts_the_marker() {
		let input = "a".repeat(2_100);
		let truncated = truncate_tiny_message(&input);
		assert_eq!(truncated.chars().count(), MAX_TINY_MESSAGE_CHARS);
		assert!(truncated.contains("\n[… 125 chars omitted …]\n"));
		assert!(truncated.starts_with('a'));
		assert!(truncated.ends_with('a'));
	}

	#[test]
	fn formatting_wraps_preprocessed_user_content() {
		assert_eq!(
			format_title_user_message("\u{1b}[31mfix parser now\u{1b}[0m"),
			Str::new_static("<user>\nfix parser now\n</user>")
		);
	}

	#[test]
	fn low_signal_defers_until_a_concrete_task_after_cleaning() {
		assert!(is_low_signal_title_input("hi, thanks 123"));
		assert!(is_low_signal_title_input("..."));
		assert!(is_low_signal_title_input("```\nhi hi thanks 123\n```"));
		assert!(!is_low_signal_title_input("fix the OAuth callback"));
	}

	#[test]
	fn normalization_ignores_leaked_reasoning_and_unwraps_json() {
		assert_eq!(
			normalize_generated_title(
				"<thinking>draft <title>Wrong</title></thinking><title>Fix OAuth callback</title>",
				None,
			),
			Some(Str::new_static("Fix OAuth callback"))
		);
		assert_eq!(
			normalize_generated_title("```json {\"title\":\"Repair session index\"} ```", None,),
			Some(Str::new_static("Repair session index"))
		);
		assert_eq!(
			normalize_generated_title("\"Fix parser!!\"", None),
			Some(Str::new_static("Fix parser!"))
		);
		assert_eq!(normalize_generated_title("<title/>", None), None);
	}

	#[test]
	fn normalization_reconciles_source_casing() {
		let cases = [
			("Restore tinyvmm snapshot", "Restore TinyVMM snapshot", "Restore TinyVMM snapshot"),
			("Repair Cnpg backups", "Repair CNPG backups", "Repair CNPG backups"),
			("Restart dAemon service", "Restart daemon service", "Restart daemon service"),
			("Fix GitHub auth", "Fix repository auth", "Fix GitHub auth"),
			("Fix Api bug", "FIX THE API BUG", "Fix Api bug"),
			("Repair dAemon bug", "Repair dAemon bug", "Repair dAemon bug"),
		];
		for (raw, source, expected) in cases {
			assert_eq!(
				normalize_generated_title(raw, Some(source)),
				Some(Str::new(expected)),
				"{raw:?}"
			);
		}
	}

	#[test]
	fn title_system_prompt_appends_the_mandatory_marker_to_overrides() {
		assert_eq!(
			title_system_prompt(None),
			Str::new_static(prompt_asset(PromptAssetId::TitleSystem).content)
		);
		assert_eq!(
			title_system_prompt(Some("Custom title policy")),
			Str::from(format!(
				"Custom title policy\n\n{}",
				prompt_asset(PromptAssetId::TitleMarkerInstruction).content
			))
		);
	}

	#[test]
	fn recap_prompt_renders_conditional_context() {
		let static_lines = "<recap>\nUser stepped away; returning. Recap: <40 words, 1–2 plain \
		                    sentences, no markdown. Lead: overall goal, current task; then one next \
		                    action. Skip: root-cause narrative, fix internals, secondary to-dos, \
		                    em-dash tangents.\n";
		assert_eq!(recap_user_prompt(None, None), Str::from(format!("{static_lines}</recap>\n")));
		assert_eq!(
			recap_user_prompt(Some("Ship recap"), Some("Wire the bridge")),
			Str::from(format!(
				"{static_lines}Overall goal: Ship recap\nActive task: Wire the bridge\n</recap>\n"
			))
		);
		assert_eq!(
			recap_user_prompt(Some("Ship recap"), None),
			Str::from(format!("{static_lines}Overall goal: Ship recap\n</recap>\n"))
		);
		assert_eq!(
			recap_user_prompt(None, Some("Wire the bridge")),
			Str::from(format!("{static_lines}Active task: Wire the bridge\n</recap>\n"))
		);
	}

	#[test]
	fn user_title_blocks_automatic_refresh() {
		let mut state = SessionTitleState {
			title:  Some(Str::new_static("Chosen")),
			source: Some(TitleSource::User),
		};
		assert!(!state.should_generate("replan the storage migration", true));
		assert!(!state.accept_generated(Str::new_static("Generated")));
		assert_eq!(state.title, Some(Str::new_static("Chosen")));
	}
}
