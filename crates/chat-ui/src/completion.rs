//! Non-blocking extension completion adapter for the synchronous editor hook.

use std::{cell::RefCell, rc::Rc, sync::Arc, thread};

use arc_swap::ArcSwapOption;
use flume::{Receiver, Sender};
use omp_core::Str;
use omp_tui::{Command, EditorCompletion, SlashCommands, SuggestionList, Suggestions};
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
}

/// A request delivered to the asynchronous completion worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionQuery {
	/// Trigger byte offset in the editor buffer.
	pub prefix_start: usize,
	/// Trigger family selected by the typed sigil.
	pub trigger:      CompletionTrigger,
	/// Typed query after the trigger.
	pub query:        Str,
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

/// Bridges asynchronous extension completion to [`EditorCompletion`].
///
/// `suggest` only drains a flume receiver, locally reranks an already-visible
/// set, and `try_send`s work. It never waits for an extension response.
pub struct DeferredCompletion {
	triggers: SmallVec<(char, CompletionTrigger), 4>,
	request:  Sender<CompletionQuery>,
	response: Receiver<CompletionResult>,
	active:   Option<CompletionQuery>,
	shown:    Option<Suggestions>,
	ghost:    ArcSwapOption<Str>,
}

impl DeferredCompletion {
	/// Starts one worker for `source` with the supplied trigger table.
	pub fn new(
		triggers: impl IntoIterator<Item = (char, CompletionTrigger)>,
		source: Arc<dyn CompletionSource>,
	) -> Self {
		let (request, requests): (Sender<CompletionQuery>, Receiver<CompletionQuery>) =
			flume::bounded(1);
		let (responses, response): (Sender<CompletionResult>, Receiver<CompletionResult>) =
			flume::unbounded();
		thread::spawn(move || {
			while let Ok(query) = requests.recv() {
				let items = source.complete(query.clone());
				if responses.send(CompletionResult { query, items }).is_err() {
					return;
				}
			}
		});
		Self {
			triggers: triggers.into_iter().collect(),
			request,
			response,
			active: None,
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
		let (offset, trigger) = before.char_indices().rev().find(|(_, character)| {
			self
				.triggers
				.iter()
				.any(|(candidate, _)| candidate == character)
		})?;
		let kind = self
			.triggers
			.iter()
			.find(|(candidate, _)| candidate == &trigger)
			.map(|(_, kind)| *kind)?;
		let after = before.get(offset + trigger.len_utf8()..)?;
		if after.chars().any(char::is_whitespace) {
			return None;
		}
		if kind == CompletionTrigger::Custom {
			let token_start = before[..offset]
				.char_indices()
				.rev()
				.find(|(_, character)| character.is_whitespace())
				.map_or(0, |(at, character)| at + character.len_utf8());
			return Some(CompletionQuery {
				prefix_start: token_start,
				trigger:      kind,
				query:        Str::new(&before[token_start..]),
			});
		}
		Some(CompletionQuery {
			prefix_start: offset,
			trigger:      kind,
			query:        Str::new(after),
		})
	}

	fn drain(&mut self) {
		while let Ok(result) = self.response.try_recv() {
			if self
				.active
				.as_ref()
				.is_some_and(|active| active.query == result.query.query)
			{
				let hint = result.items.first().and_then(|item| {
					item
						.value()
						.strip_prefix(result.query.query.as_str())
						.filter(|hint| !hint.is_empty())
				});
				self.ghost.store(hint.map(|hint| Arc::new(Str::new(hint))));
				self.shown = Some(Suggestions {
					prefix_start: result.query.prefix_start,
					items:        result.items,
				});
			}
		}
	}

	fn rerank(&mut self, query: &CompletionQuery) {
		let Some(shown) = self.shown.as_mut() else {
			return;
		};
		shown.prefix_start = query.prefix_start;
		shown
			.items
			.retain(|item| item.value().contains(query.query.as_str()));
	}
}

impl EditorCompletion for DeferredCompletion {
	fn suggest(&mut self, text: &str, cursor: usize) -> Option<Suggestions> {
		self.drain();
		let query = self.query(text, cursor)?;
		let grew = self
			.active
			.as_ref()
			.is_some_and(|active| query.query.starts_with(active.query.as_str()));
		if grew {
			self.rerank(&query);
		}
		if self.active.as_ref() != Some(&query) {
			// A full request queue means the worker is already resolving an older
			// query. Keep the visible stale set instead of blocking or clearing it.
			let _ = self.request.try_send(query.clone());
			self.active = Some(query);
		}
		self.shown.clone().filter(|shown| !shown.items.is_empty())
	}

	fn hint(&mut self, _text: &str, _cursor: usize) -> Option<Str> {
		self.ghost.load_full().as_deref().cloned()
	}
}
#[cfg(test)]
mod tests {
	use omp_tui::{Command, EditorCompletion, Suggestion, Suggestions};

	use super::{CompletionChain, ReloadableSlashCommands};

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
}
