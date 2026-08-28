//! Non-blocking extension completion adapter for the synchronous editor hook.

use std::{
	cell::RefCell,
	collections::BTreeMap,
	rc::Rc,
	sync::Arc,
	thread,
	time::{Duration, Instant},
};

use arc_swap::ArcSwapOption;
use flume::{Receiver, Sender};
use omp_core::Str;
use omp_tui::{Command, EditorCompletion, SlashCommands, Suggestion, SuggestionList, Suggestions};
use smallvec::SmallVec;

/// One completion trigger accepted by [`DeferredCompletion`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionTrigger {
	/// A slash command prefix.
	Slash,
	/// A mention prefix.
	Mention,
	/// A hash/topic prefix.
	Hash,
	/// An extension-defined trigger.
	Custom,
	/// A manifest-projected extension completion trigger.
	Extension,
}

/// One literal completion trigger and its host-side scheduling policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionRule {
	/// Literal prefix which arms this provider.
	pub prefix:         Str,
	/// Trigger family supplied to the completion source.
	pub trigger:        CompletionTrigger,
	/// Whether the prefix must begin the current line.
	pub at_line_start:  bool,
	/// Minimum query length before a request is scheduled.
	pub min_chars:      usize,
	/// Quiet period before a request reaches the completion source.
	pub debounce:       Duration,
	/// Maximum visible rows retained from one result.
	pub max_results:    usize,
	/// Exact-query and locally refinable result lifetime.
	pub cache:          Duration,
	/// Whether a growing query filters a cached result without a source call.
	pub refine_locally: bool,
}

impl CompletionRule {
	/// Builds a rule with the SDK defaults.
	pub fn new(prefix: impl Into<Str>, trigger: CompletionTrigger) -> Self {
		Self {
			prefix: prefix.into(),
			trigger,
			at_line_start: false,
			min_chars: 0,
			debounce: Duration::from_millis(90),
			max_results: 20,
			cache: Duration::from_secs(2),
			refine_locally: true,
		}
	}

	/// Builds a native in-process rule without extension debounce latency.
	pub fn native(prefix: impl Into<Str>, trigger: CompletionTrigger) -> Self {
		let mut rule = Self::new(prefix, trigger);
		rule.debounce = Duration::ZERO;
		rule
	}
}

impl From<(char, CompletionTrigger)> for CompletionRule {
	fn from((prefix, trigger): (char, CompletionTrigger)) -> Self {
		let mut encoded = [0_u8; 4];
		Self::native(&*prefix.encode_utf8(&mut encoded), trigger)
	}
}

/// A request delivered to the asynchronous completion worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionQuery {
	/// Trigger byte offset in the editor buffer.
	pub prefix_start: usize,
	/// Exact literal prefix which armed the provider.
	pub prefix:       Str,
	/// Trigger family selected by the typed sigil.
	pub trigger:      CompletionTrigger,
	/// Typed query after the trigger.
	pub query:        Str,
	/// Scheduling and cache policy projected with the trigger.
	pub rule:         CompletionRule,
}

/// Extension-owned completion source. It runs outside the editor's key path.
pub trait CompletionSource: Send + Sync + 'static {
	/// Resolves one query in ranked order.
	fn complete(&self, query: CompletionQuery) -> SuggestionList;
}

struct CompletionResult {
	query: CompletionQuery,
	items: SuggestionList,
}

/// Ordered completion composition. The first source with visible rows wins.
pub struct CompletionChain {
	sources: SmallVec<Box<dyn EditorCompletion>, 2>,
}

impl CompletionChain {
	/// Builds an empty ordered source chain.
	pub const fn new() -> Self {
		Self { sources: SmallVec::new() }
	}

	/// Appends a lower-precedence source.
	pub fn source(mut self, source: Box<dyn EditorCompletion>) -> Self {
		self.sources.push(source);
		self
	}
}

impl Default for CompletionChain {
	fn default() -> Self {
		Self::new()
	}
}

impl EditorCompletion for CompletionChain {
	fn suggest(&mut self, text: &str, cursor: usize) -> Option<Suggestions> {
		self
			.sources
			.iter_mut()
			.find_map(|source| source.suggest(text, cursor))
	}

	fn hint(&mut self, text: &str, cursor: usize) -> Option<Str> {
		self
			.sources
			.iter_mut()
			.find_map(|source| source.hint(text, cursor))
	}
}
/// Stable slash-command source shared by the editor chain and backend roster
/// refreshes.
///
/// Replacing the roster mutates only this source, preserving every
/// lower-precedence completion provider installed beside it.
#[derive(Clone)]
pub struct ReloadableSlashCommands {
	inner: Rc<RefCell<SlashCommands>>,
	usage: Arc<dyn Fn(&str) -> u64 + Send + Sync>,
}

impl ReloadableSlashCommands {
	/// Creates a reloadable source with usage-based ranking.
	pub fn new(commands: Vec<Command>, usage: impl Fn(&str) -> u64 + Send + Sync + 'static) -> Self {
		let usage: Arc<dyn Fn(&str) -> u64 + Send + Sync> = Arc::new(usage);
		let rank = Arc::clone(&usage);
		Self {
			inner: Rc::new(RefCell::new(
				SlashCommands::new(commands).with_usage(move |name| rank(name)),
			)),
			usage,
		}
	}

	/// Replaces command data while retaining the supplied usage ranker.
	pub fn replace(&self, commands: Vec<Command>) {
		let usage = Arc::clone(&self.usage);
		*self.inner.borrow_mut() = SlashCommands::new(commands).with_usage(move |name| usage(name));
	}

	/// Replaces command data and the usage ranker atomically.
	pub fn replace_ranked(
		&mut self,
		commands: Vec<Command>,
		usage: impl Fn(&str) -> u64 + Send + Sync + 'static,
	) {
		self.usage = Arc::new(usage);
		self.replace(commands);
	}
}

impl EditorCompletion for ReloadableSlashCommands {
	fn suggest(&mut self, text: &str, cursor: usize) -> Option<Suggestions> {
		self.inner.borrow_mut().suggest(text, cursor)
	}

	fn hint(&mut self, text: &str, cursor: usize) -> Option<Str> {
		self.inner.borrow_mut().hint(text, cursor)
	}
}

struct CachedCompletion {
	received: Instant,
	query:    CompletionQuery,
	items:    SuggestionList,
}

/// Bridges asynchronous extension completion to [`EditorCompletion`].
///
/// `suggest` only drains a flume receiver, locally reranks an already-visible
/// set, and `try_send`s work. It never waits for an extension response.
pub struct DeferredCompletion {
	rules:     SmallVec<CompletionRule, 4>,
	request:   Sender<CompletionQuery>,
	response:  Receiver<CompletionResult>,
	requested: Option<CompletionQuery>,
	cache:     BTreeMap<(Str, Str), CachedCompletion>,
	shown:     Option<Suggestions>,
	ghost:     ArcSwapOption<Str>,
}

impl DeferredCompletion {
	/// Starts one worker for `source` with the supplied trigger table.
	pub fn new<R>(rules: impl IntoIterator<Item = R>, source: Arc<dyn CompletionSource>) -> Self
	where
		R: Into<CompletionRule>,
	{
		let (request, requests): (Sender<CompletionQuery>, Receiver<CompletionQuery>) =
			flume::bounded(1);
		let (responses, response): (Sender<CompletionResult>, Receiver<CompletionResult>) =
			flume::unbounded();
		thread::spawn(move || {
			while let Ok(mut query) = requests.recv() {
				loop {
					match requests.recv_timeout(query.rule.debounce) {
						Ok(newer) => query = newer,
						Err(flume::RecvTimeoutError::Timeout) => break,
						Err(flume::RecvTimeoutError::Disconnected) => return,
					}
				}
				let mut items = source.complete(query.clone());
				items.truncate(query.rule.max_results);
				if responses.send(CompletionResult { query, items }).is_err() {
					return;
				}
			}
		});
		Self {
			rules: rules.into_iter().map(Into::into).collect(),
			request,
			response,
			requested: None,
			cache: BTreeMap::new(),
			shown: None,
			ghost: ArcSwapOption::empty(),
		}
	}

	/// Updates the ghost hint without locking the keystroke path.
	pub fn set_ghost(&self, hint: Option<impl Into<Str>>) {
		self.ghost.store(hint.map(|hint| Arc::new(hint.into())));
	}

	fn query(&self, text: &str, cursor: usize) -> Option<CompletionQuery> {
		let before = text.get(..cursor)?;
		let (offset, rule) = self
			.rules
			.iter()
			.filter_map(|rule| {
				let offset = before.rfind(rule.prefix.as_str())?;
				let line_start = before[..offset].rfind('\n').map_or(0, |line| line + 1);
				(!rule.at_line_start || offset == line_start).then_some((offset, rule))
			})
			.max_by_key(|(offset, rule)| (*offset, rule.prefix.len()))?;
		let after = before.get(offset + rule.prefix.len()..)?;
		if after.chars().count() < rule.min_chars {
			return None;
		}
		if rule.trigger == CompletionTrigger::Custom {
			let token_start = before[..offset]
				.char_indices()
				.rev()
				.find(|(_, character)| character.is_whitespace())
				.map_or(0, |(at, character)| at + character.len_utf8());
			return Some(CompletionQuery {
				prefix_start: token_start,
				prefix:       rule.prefix.clone(),
				trigger:      rule.trigger,
				query:        Str::new(&before[token_start..]),
				rule:         rule.clone(),
			});
		}
		Some(CompletionQuery {
			prefix_start: offset,
			prefix:       rule.prefix.clone(),
			trigger:      rule.trigger,
			query:        Str::new(after),
			rule:         rule.clone(),
		})
	}

	fn drain(&mut self) {
		let now = Instant::now();
		while let Ok(result) = self.response.try_recv() {
			let key = (result.query.prefix.clone(), result.query.query.clone());
			self.cache.insert(key, CachedCompletion {
				received: now,
				query:    result.query,
				items:    result.items,
			});
		}
		self.cache.retain(|_, cached| {
			now.saturating_duration_since(cached.received) <= cached.query.rule.cache
		});
		while self.cache.len() > 32 {
			let Some(oldest) = self
				.cache
				.iter()
				.min_by_key(|(_, cached)| cached.received)
				.map(|(key, _)| key.clone())
			else {
				break;
			};
			self.cache.remove(&oldest);
		}
	}

	fn cached(&self, query: &CompletionQuery) -> Option<SuggestionList> {
		let exact = (query.prefix.clone(), query.query.clone());
		if let Some(cached) = self.cache.get(&exact) {
			return Some(cached.items.clone());
		}
		if !query.rule.refine_locally {
			return None;
		}
		self
			.cache
			.values()
			.filter(|cached| {
				cached.query.prefix == query.prefix
					&& query.query.starts_with(cached.query.query.as_str())
					&& cached.query.rule.refine_locally
			})
			.max_by_key(|cached| cached.query.query.len())
			.map(|cached| locally_rank(&cached.items, query.query.as_str(), query.rule.max_results))
	}

	fn show(&mut self, query: &CompletionQuery, items: SuggestionList) {
		let hint = items.first().and_then(|item| {
			item
				.value()
				.strip_prefix(query.query.as_str())
				.filter(|hint| !hint.is_empty())
		});
		self.ghost.store(hint.map(|hint| Arc::new(Str::new(hint))));
		self.shown = Some(Suggestions { prefix_start: query.prefix_start, items });
	}
}

impl EditorCompletion for DeferredCompletion {
	fn suggest(&mut self, text: &str, cursor: usize) -> Option<Suggestions> {
		self.drain();
		let Some(query) = self.query(text, cursor) else {
			self.shown = None;
			return None;
		};
		if let Some(items) = self.cached(&query) {
			self.show(&query, items);
		} else if self.requested.as_ref() != Some(&query) {
			self.shown = None;
			if self.request.try_send(query.clone()).is_ok() {
				self.requested = Some(query);
			}
		}
		self.shown.clone().filter(|shown| !shown.items.is_empty())
	}

	fn hint(&mut self, _text: &str, _cursor: usize) -> Option<Str> {
		self.ghost.load_full().as_deref().cloned()
	}
}

fn locally_rank(items: &SuggestionList, query: &str, limit: usize) -> SuggestionList {
	let needle = query.to_ascii_lowercase();
	let mut ranked: SmallVec<(u16, usize, Suggestion), 8> = items
		.iter()
		.enumerate()
		.filter_map(|(index, item)| {
			local_score(&item.value().to_ascii_lowercase(), &needle)
				.map(|score| (score, index, item.clone()))
		})
		.collect();
	ranked.sort_by_key(|(score, index, _)| (std::cmp::Reverse(*score), *index));
	ranked
		.into_iter()
		.take(limit)
		.map(|(_, _, item)| item)
		.collect()
}

fn local_score(value: &str, query: &str) -> Option<u16> {
	if query.is_empty() {
		return Some(1);
	}
	if value.starts_with(query) {
		return Some(u16::MAX.saturating_sub(value.len().min(u16::MAX as usize) as u16));
	}
	let mut matched = 0_usize;
	let mut first = None;
	let mut previous = 0_usize;
	let mut gap = 0_usize;
	for (index, character) in value.char_indices() {
		if query[matched..].starts_with(character) {
			first.get_or_insert(index);
			if matched != 0 {
				gap = gap.saturating_add(index.saturating_sub(previous + 1));
			}
			previous = index;
			matched += character.len_utf8();
			if matched == query.len() {
				let start = first.unwrap_or_default();
				let penalty = start.saturating_mul(8).saturating_add(gap);
				return Some(u16::MAX.saturating_sub(penalty.min(u16::MAX as usize) as u16));
			}
		}
	}
	None
}
#[cfg(test)]
mod tests {
	use std::{
		sync::{
			Arc,
			atomic::{AtomicUsize, Ordering},
		},
		thread,
		time::Duration,
	};

	use omp_tui::{Command, EditorCompletion, Suggestion, Suggestions};

	use super::{
		CompletionChain, CompletionQuery, CompletionRule, CompletionSource, CompletionTrigger,
		DeferredCompletion, ReloadableSlashCommands, SuggestionList,
	};

	struct MentionCompletion;

	impl EditorCompletion for MentionCompletion {
		fn suggest(&mut self, text: &str, _cursor: usize) -> Option<Suggestions> {
			text.starts_with('@').then(|| Suggestions {
				prefix_start: 0,
				items:        [Suggestion::new("@project", "@project")]
					.into_iter()
					.collect(),
			})
		}
	}

	#[test]
	fn slash_refresh_preserves_chained_project_completion() {
		let slash =
			ReloadableSlashCommands::new(vec![Command::new("old", "old command", &[])], |_| 0);
		let mut chain = CompletionChain::new()
			.source(Box::new(slash.clone()))
			.source(Box::new(MentionCompletion));

		slash.replace(vec![Command::new("new", "new command", &[])]);

		let suggestions = chain
			.suggest("@pro", 4)
			.expect("project completion survives");
		assert_eq!(suggestions.items[0].value(), "@project");
	}

	#[test]
	fn slash_refresh_preserves_usage_ranking() {
		let slash = ReloadableSlashCommands::new(
			vec![Command::new("alpha", "alpha", &[]), Command::new("beta", "beta", &[])],
			|name| u64::from(name == "beta") * 10,
		);
		let mut completion = slash.clone();
		let before = completion.suggest("/", 1).expect("slash suggestions");
		assert_eq!(before.items[0].value(), "/beta ");

		slash.replace(vec![
			Command::new("alpha", "alpha", &[]),
			Command::new("beta", "beta", &[]),
			Command::new("gamma", "gamma", &[]),
		]);
		let after = completion
			.suggest("/", 1)
			.expect("refreshed slash suggestions");
		assert_eq!(after.items[0].value(), "/beta ");
		assert!(after.items.iter().any(|item| item.value() == "/gamma "));
	}

	struct CountingCompletion {
		calls: Arc<AtomicUsize>,
	}

	impl CompletionSource for CountingCompletion {
		fn complete(&self, _query: CompletionQuery) -> SuggestionList {
			self.calls.fetch_add(1, Ordering::Relaxed);
			[
				Suggestion::new("alpha", "alpha"),
				Suggestion::new("alpine", "alpine"),
				Suggestion::new("beta", "beta"),
			]
			.into_iter()
			.collect()
		}
	}

	#[test]
	fn extension_completion_debounces_and_refines_cached_rows_locally() {
		let calls = Arc::new(AtomicUsize::new(0));
		let mut rule = CompletionRule::new("#", CompletionTrigger::Extension);
		rule.debounce = Duration::from_millis(15);
		rule.cache = Duration::from_secs(1);
		rule.refine_locally = true;
		let mut completion = DeferredCompletion::new(
			[rule],
			Arc::new(CountingCompletion { calls: Arc::clone(&calls) }),
		);
		assert!(completion.suggest("#a", 2).is_none());
		assert_eq!(calls.load(Ordering::Relaxed), 0);
		thread::sleep(Duration::from_millis(30));
		let first = completion.suggest("#a", 2).expect("debounced result");
		assert_eq!(calls.load(Ordering::Relaxed), 1);
		assert_eq!(first.items.len(), 3);

		let refined = completion
			.suggest("#alp", 4)
			.expect("locally refined result");
		assert_eq!(calls.load(Ordering::Relaxed), 1);
		assert_eq!(refined.items.len(), 2);
		thread::sleep(Duration::from_millis(20));
		assert_eq!(calls.load(Ordering::Relaxed), 1);
	}
}
