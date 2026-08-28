//! Data-only production bridge between extension presentation authorities and
//! an attached UI.
//!
//! The bridge deliberately carries typed presentation values rather than
//! terminal handles. An attachment is generation-scoped: replacing or dropping
//! it fails every correlated waiter, and dropping a request future removes its
//! reply slot.

use std::{
	collections::BTreeMap,
	future::Future,
	mem,
	pin::Pin,
	sync::{
		Arc, Weak,
		atomic::{AtomicU64, Ordering},
	},
	time::{self, Duration, Instant},
};

use async_trait::async_trait;
use flume::Receiver;
use futures::future::join_all;
use omp_chat_ui::completion::{
	CompletionQuery as ComposerCompletionQuery, CompletionRule, CompletionSource, CompletionTrigger,
	DeferredCompletion,
};
use omp_core::{Str, sf};
use omp_envd::{
	exthost::{
		CallbackConcurrency, UiCallbackDispatch, UiCallbackOwner, UiCommandRosterEntry, UiRoster,
		UiRosterConflict, UiShortcutRosterEntry, VerifiedMarkdownTransformer,
		control::{ControlConnectionIdentity, ControlDispatch, ControlInvocationAuthority},
		dispatch::CallbackDispatcher,
		lifecycle::HeadlessLifecycleKind,
	},
	worker::{HostKey, SealedRegistryEvidence},
};
use omp_proto::ui::v1::{
	CommandDispatchResult, CommandInvoked, CompletionCandidate,
	CompletionQuery as WireCompletionQuery, ShortcutInvoked, UiDispatch, UiDispatchResult,
	command_dispatch_result, ui_dispatch, ui_dispatch_result,
};
use omp_tui::{Icon, Suggestion, SuggestionList};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::oneshot;

use super::{
	commands::{
		CommandDeclaration, CommandGeneration, CommandProvenance, CommandResult, CommandSourceKind,
		CommandSurface, ConsumedResult, ExtensionCommandHandler, ExtensionCommandInvocation,
		PromptResult,
	},
	input::completion_arg_query,
	presentation_authority::{
		COMPLETION_CALLBACK_DEADLINE, PresentationAuthorityError, PresentationCallback,
		PresentationCallbackDispatcher, PresentationCallbackKind, PresentationClient,
		PresentationEffect, PresentationIdentity, PresentationRequest, PresentationResponse,
		RENDER_CALLBACK_DEADLINE,
	},
};

const RENDER_TML_MAX_BYTES: usize = 256 * 1024;

/// One data-only operation delivered to the attached presentation surface.
#[derive(Clone, Debug, PartialEq)]
pub enum PresentationOperation {
	/// Apply a retained UI effect under its authenticated extension identity.
	Effect {
		/// Authenticated extension incarnation owning every retained key.
		identity: Arc<PresentationIdentity>,
		/// Validated effect body.
		effect:   PresentationEffect,
	},
	/// Resolve a correlated UI request.
	Request(PresentationRequest),
	/// Switch the attached interactive owner to an already-durable session.
	SessionTransition(Str),
}

/// A generation-fenced operation received by the real UI actor.
#[derive(Clone, Debug, PartialEq)]
pub struct PresentationDispatch {
	/// Bridge-issued correlation identifier.
	pub id:         u64,
	/// Attachment generation which owns this dispatch.
	pub generation: u64,
	/// Typed, data-only operation.
	pub operation:  PresentationOperation,
}

pub type PresentationResult = Result<PresentationResponse, PresentationAuthorityError>;

struct BridgeState {
	generation: u64,
	next_id:    u64,
	sender:     Option<flume::Sender<PresentationDispatch>>,
	pending:    BTreeMap<u64, oneshot::Sender<PresentationResult>>,
}

struct BridgeInner {
	capacity: usize,
	state:    Mutex<BridgeState>,
}

/// Cloneable presentation client installed before the interactive renderer
/// attaches.
///
/// It is safe to put this object in per-connection authority factories. A later
/// call to [`Self::attach`] atomically replaces the old surface and tears down
/// its outstanding dialogs.
#[derive(Clone)]
pub struct PresentationBridge {
	inner: Arc<BridgeInner>,
}

impl PresentationBridge {
	/// Creates a bounded bridge. Capacity must be non-zero.
	#[must_use]
	pub fn new(capacity: usize) -> Self {
		assert!(capacity != 0, "presentation bridge capacity must be non-zero");
		Self {
			inner: Arc::new(BridgeInner {
				capacity,
				state: Mutex::new(BridgeState {
					generation: 0,
					next_id:    1,
					sender:     None,
					pending:    BTreeMap::new(),
				}),
			}),
		}
	}

	/// Attaches the real UI actor, cancelling every request owned by an older
	/// surface.
	#[must_use]
	pub fn attach(&self) -> PresentationEndpoint {
		let (sender, receiver) = flume::bounded(self.inner.capacity);
		let (generation, pending) = {
			let mut state = self.inner.state.lock();
			state.generation = state.generation.wrapping_add(1).max(1);
			state.sender = Some(sender);
			let generation = state.generation;
			let pending = mem::take(&mut state.pending);
			(generation, pending)
		};
		fail_pending(pending, PresentationAuthorityError::Unavailable);
		PresentationEndpoint { inner: Arc::downgrade(&self.inner), generation, receiver }
	}

	async fn dispatch(
		&self,
		operation: PresentationOperation,
	) -> Result<PresentationResponse, PresentationAuthorityError> {
		let (id, generation, sender, receiver) = {
			let mut state = self.inner.state.lock();
			let sender = state
				.sender
				.clone()
				.ok_or(PresentationAuthorityError::Unavailable)?;
			let id = state.next_id;
			state.next_id = state.next_id.wrapping_add(1).max(1);
			let generation = state.generation;
			let (reply, receiver) = oneshot::channel();
			state.pending.insert(id, reply);
			(id, generation, sender, receiver)
		};
		let mut guard = PendingGuard { inner: Arc::downgrade(&self.inner), id };
		if sender
			.send_async(PresentationDispatch { id, generation, operation })
			.await
			.is_err()
		{
			guard.remove();
			return Err(PresentationAuthorityError::Unavailable);
		}
		let result = receiver
			.await
			.map_err(|_| PresentationAuthorityError::Unavailable)?;
		guard.disarm();
		result
	}

	/// Requests one post-durability switch from the attached interactive owner.
	pub async fn transition(&self, session: Str) -> Result<(), PresentationAuthorityError> {
		match self
			.dispatch(PresentationOperation::SessionTransition(session))
			.await?
		{
			PresentationResponse::Ack => Ok(()),
			_ => Err(PresentationAuthorityError::Owner(Str::new_static(
				"presentation surface returned a non-ack session transition",
			))),
		}
	}
}

impl Default for PresentationBridge {
	fn default() -> Self {
		Self::new(256)
	}
}

#[async_trait]
impl PresentationClient for PresentationBridge {
	async fn effect(
		&self,
		identity: Arc<PresentationIdentity>,
		effect: PresentationEffect,
	) -> Result<(), PresentationAuthorityError> {
		match self
			.dispatch(PresentationOperation::Effect { identity, effect })
			.await?
		{
			PresentationResponse::Ack => Ok(()),
			_ => Err(PresentationAuthorityError::Owner(Str::new_static(
				"presentation surface returned a non-ack effect response",
			))),
		}
	}

	async fn request(
		&self,
		_identity: Arc<PresentationIdentity>,
		request: PresentationRequest,
	) -> Result<PresentationResponse, PresentationAuthorityError> {
		self.dispatch(PresentationOperation::Request(request)).await
	}
}

/// The receiving half owned exclusively by the real renderer actor.
pub struct PresentationEndpoint {
	inner:      Weak<BridgeInner>,
	generation: u64,
	receiver:   Receiver<PresentationDispatch>,
}

impl PresentationEndpoint {
	/// Receives the next typed operation.
	pub async fn recv(&self) -> Result<PresentationDispatch, flume::RecvError> {
		self.receiver.recv_async().await
	}

	/// Completes one exact request. Stale attachments and duplicate replies are
	/// rejected.
	pub fn complete(
		&self,
		id: u64,
		result: PresentationResult,
	) -> Result<(), PresentationAuthorityError> {
		let inner = self
			.inner
			.upgrade()
			.ok_or(PresentationAuthorityError::Unavailable)?;
		let reply = {
			let mut state = inner.state.lock();
			if state.generation != self.generation {
				return Err(PresentationAuthorityError::Cancelled);
			}
			state.pending.remove(&id).ok_or_else(|| {
				PresentationAuthorityError::Owner(sf!("unknown presentation request {id}"))
			})?
		};
		reply
			.send(result)
			.map_err(|_| PresentationAuthorityError::Cancelled)
	}

	/// Acknowledges a successful effect.
	pub fn acknowledge(&self, id: u64) -> Result<(), PresentationAuthorityError> {
		self.complete(id, Ok(PresentationResponse::Ack))
	}
}

impl Drop for PresentationEndpoint {
	fn drop(&mut self) {
		let Some(inner) = self.inner.upgrade() else {
			return;
		};
		let pending = {
			let mut state = inner.state.lock();
			if state.generation != self.generation {
				return;
			}
			state.sender = None;
			mem::take(&mut state.pending)
		};
		fail_pending(pending, PresentationAuthorityError::Unavailable);
	}
}

struct PendingGuard {
	inner: Weak<BridgeInner>,
	id:    u64,
}

impl PendingGuard {
	fn remove(&mut self) {
		if let Some(inner) = self.inner.upgrade() {
			inner.state.lock().pending.remove(&self.id);
		}
		self.id = 0;
	}

	fn disarm(&mut self) {
		self.id = 0;
	}
}

impl Drop for PendingGuard {
	fn drop(&mut self) {
		if self.id != 0 {
			self.remove();
		}
	}
}

fn fail_pending(
	pending: BTreeMap<u64, oneshot::Sender<PresentationResult>>,
	error: PresentationAuthorityError,
) {
	for reply in pending.into_values() {
		let _ = reply.send(Err(error.clone()));
	}
}

/// Boxed callback result used by the production registry.
pub type PresentationCallbackFuture =
	Pin<Box<dyn Future<Output = Result<Value, PresentationAuthorityError>> + Send + 'static>>;

/// One exact extension callback body. It receives JSON only and cannot acquire
/// a terminal.
pub trait PresentationCallbackHandler: Send + Sync + 'static {
	/// Executes the registered callback.
	fn call(&self, arguments: Value) -> PresentationCallbackFuture;
}

impl<F, Fut> PresentationCallbackHandler for F
where
	F: Fn(Value) -> Fut + Send + Sync + 'static,
	Fut: Future<Output = Result<Value, PresentationAuthorityError>> + Send + 'static,
{
	fn call(&self, arguments: Value) -> PresentationCallbackFuture {
		Box::pin(self(arguments))
	}
}

struct CallbackEntry {
	registration: u64,
	handler:      Arc<dyn PresentationCallbackHandler>,
}

struct CallbackState {
	next_registration: u64,
	entries:           BTreeMap<(u8, Str), CallbackEntry>,
}

struct CallbackInner {
	identity: Arc<PresentationIdentity>,
	state:    Mutex<CallbackState>,
}

/// Exact-generation callback registry used for completions, renderers,
/// commands, shortcuts, and activation handlers.
#[derive(Clone)]
pub struct PresentationCallbackRegistry {
	inner: Arc<CallbackInner>,
}

impl PresentationCallbackRegistry {
	/// Creates a registry fenced to one authenticated extension incarnation.
	#[must_use]
	pub fn new(identity: Arc<PresentationIdentity>) -> Self {
		Self {
			inner: Arc::new(CallbackInner {
				identity,
				state: Mutex::new(CallbackState {
					next_registration: 1,
					entries:           BTreeMap::new(),
				}),
			}),
		}
	}

	/// Registers or atomically replaces one exact callback operation.
	#[must_use]
	pub fn register(
		&self,
		kind: PresentationCallbackKind,
		operation: impl Into<Str>,
		handler: Arc<dyn PresentationCallbackHandler>,
	) -> PresentationCallbackRegistration {
		let operation = operation.into();
		let class = callback_class(kind);
		let registration = {
			let mut state = self.inner.state.lock();
			let registration = state.next_registration;
			state.next_registration = state.next_registration.wrapping_add(1).max(1);
			state
				.entries
				.insert((class, operation.clone()), CallbackEntry { registration, handler });
			registration
		};
		PresentationCallbackRegistration {
			inner: Arc::downgrade(&self.inner),
			class,
			operation,
			registration,
		}
	}

	/// Removes every callback immediately during host teardown.
	/// Registers a completion callback.
	#[must_use]
	pub fn register_completion(
		&self,
		operation: impl Into<Str>,
		handler: Arc<dyn PresentationCallbackHandler>,
	) -> PresentationCallbackRegistration {
		self.register(PresentationCallbackKind::Completion, operation, handler)
	}

	/// Registers a renderer callback.
	#[must_use]
	pub fn register_renderer(
		&self,
		operation: impl Into<Str>,
		handler: Arc<dyn PresentationCallbackHandler>,
	) -> PresentationCallbackRegistration {
		self.register(PresentationCallbackKind::Renderer, operation, handler)
	}

	/// Registers a command, shortcut, or activation action callback.
	#[must_use]
	pub fn register_action(
		&self,
		operation: impl Into<Str>,
		handler: Arc<dyn PresentationCallbackHandler>,
	) -> PresentationCallbackRegistration {
		self.register(PresentationCallbackKind::Action, operation, handler)
	}

	/// Removes every callback immediately during host teardown.
	pub fn clear(&self) {
		self.inner.state.lock().entries.clear();
	}
}

#[async_trait]
impl PresentationCallbackDispatcher for PresentationCallbackRegistry {
	async fn dispatch(
		&self,
		identity: Arc<PresentationIdentity>,
		_invocation: ControlInvocationAuthority,
		callback: PresentationCallback,
	) -> Result<Value, PresentationAuthorityError> {
		if identity.as_ref() != self.inner.identity.as_ref() {
			return Err(PresentationAuthorityError::Identity);
		}
		let handler = self
			.inner
			.state
			.lock()
			.entries
			.get(&(callback_class(callback.kind), callback.operation.clone()))
			.map(|entry| entry.handler.clone())
			.ok_or_else(|| {
				PresentationAuthorityError::Owner(sf!(
					"presentation callback `{}` is not registered",
					callback.operation
				))
			})?;
		handler.call(callback.arguments).await
	}
}
struct PublishedUiState {
	roster:            UiRoster,
	routes:            BTreeMap<HostKey, PublishedUiRoute>,
	markdown:          BTreeMap<HostKey, Box<[VerifiedMarkdownTransformer]>>,
	markdown_revision: u64,
	markdown_cache:    BTreeMap<(u64, Str, u64), Str>,
	subscribers:       Vec<flume::Sender<HeadlessLifecycleKind>>,
}

struct PublishedUiRoute {
	identity:   Arc<ControlConnectionIdentity>,
	dispatcher: Arc<dyn CallbackDispatcher>,
}

/// App-owned, atomically replaced projection of manifest-verified extension
/// command and shortcut declarations.
pub struct PublishedUiRoster {
	state: Mutex<PublishedUiState>,
}

impl Default for PublishedUiRoster {
	fn default() -> Self {
		Self {
			state: Mutex::new(PublishedUiState {
				roster:            UiRoster::default(),
				routes:            BTreeMap::new(),
				markdown:          BTreeMap::new(),
				markdown_revision: 0,
				markdown_cache:    BTreeMap::new(),
				subscribers:       Vec::new(),
			}),
		}
	}
}

impl PublishedUiRoster {
	/// Projects manifest completion triggers and dynamic command argument
	/// completers into the composer's static-no-Python trigger table.
	pub fn completion_rules(&self) -> Vec<CompletionRule> {
		let state = self.state.lock();
		let mut rules = BTreeMap::<Str, CompletionRule>::new();
		for entry in state.roster.completions() {
			let declaration = &entry.declaration;
			merge_completion_rule(&mut rules, CompletionRule {
				prefix:         Str::from(declaration.prefix.as_str()),
				trigger:        CompletionTrigger::Extension,
				at_line_start:  declaration.at_line_start,
				min_chars:      declaration.min_chars as usize,
				debounce:       Duration::from_millis(declaration.debounce_ms),
				max_results:    declaration.max_results as usize,
				cache:          Duration::from_millis(declaration.cache_ms),
				refine_locally: declaration.refine_locally,
			});
		}
		for entry in state.roster.commands() {
			if entry.declaration.arg_completion_callback.is_none() {
				continue;
			}
			for spelling in std::iter::once(entry.declaration.name.as_str())
				.chain(entry.declaration.aliases.iter().map(String::as_str))
			{
				merge_completion_rule(&mut rules, CompletionRule {
					prefix:         sf!("/{spelling} "),
					trigger:        CompletionTrigger::Extension,
					at_line_start:  true,
					min_chars:      0,
					debounce:       Duration::from_millis(90),
					max_results:    20,
					cache:          Duration::from_secs(2),
					refine_locally: true,
				});
			}
		}
		rules.into_values().collect()
	}

	/// Builds the non-blocking composer adapter around this live roster and a
	/// lower-level native completion source.
	pub fn completion_adapter(
		self: &Arc<Self>,
		session: Str,
		fallback_rules: impl IntoIterator<Item = CompletionRule>,
		fallback: Arc<dyn CompletionSource>,
	) -> DeferredCompletion {
		let mut rules = BTreeMap::<Str, CompletionRule>::new();
		for rule in fallback_rules {
			merge_completion_rule(&mut rules, rule);
		}
		for rule in self.completion_rules() {
			merge_completion_rule(&mut rules, rule);
		}
		DeferredCompletion::new(
			rules.into_values(),
			Arc::new(PublishedCompletionSource {
				roster: Arc::clone(self),
				session,
				runtime: tokio::runtime::Handle::current(),
				fallback,
				next_id: AtomicU64::new(1),
			}),
		)
	}

	/// Returns whether any live verified generation declares a markdown
	/// transformer.
	pub fn has_markdown_transformers(&self) -> bool {
		self
			.state
			.lock()
			.markdown
			.values()
			.any(|declarations| !declarations.is_empty())
	}

	/// Returns whether an exact-revision extension renderer can affect this
	/// tool view under the native renderer authority rule.
	pub fn has_tool_renderer(
		&self,
		identity: &omp_tool::ToolIdentity,
		native_authoritative: bool,
	) -> bool {
		let state = self.state.lock();
		state.roster.renderers(identity).any(|entry| {
			state.routes.contains_key(&entry.owner.host)
				&& (!native_authoritative || entry.declaration.decorates)
		})
	}

	#[cfg(test)]
	pub(super) fn install_test_renderer(
		&self,
		declaration: omp_envd::exthost::VerifiedRendererDeclaration,
		target: Arc<ControlConnectionIdentity>,
		dispatcher: Arc<dyn CallbackDispatcher>,
	) {
		let host = HostKey::new(target.layer.clone(), target.tier.clone(), target.extension.clone());
		let mut state = self.state.lock();
		state
			.roster
			.install(host.clone(), &omp_envd::exthost::VerifiedUiRoster {
				generation: target.host_generation,
				extension: target.extension.clone(),
				renderers: vec![declaration].into_boxed_slice(),
				..Default::default()
			})
			.expect("install renderer fixture");
		state
			.routes
			.insert(host, PublishedUiRoute { identity: target, dispatcher });
	}

	/// Atomically replaces the entire app-owned roster after startup or reload.
	pub fn replace(
		&self,
		evidence: impl IntoIterator<Item = Arc<SealedRegistryEvidence>>,
		dispatcher: Arc<dyn CallbackDispatcher>,
	) -> Result<(), UiRosterConflict> {
		let mut roster = UiRoster::default();
		let mut routes = BTreeMap::new();
		let mut markdown = BTreeMap::new();
		for evidence in evidence {
			let host = HostKey::new(
				evidence.identity.layer.clone(),
				evidence.identity.tier.clone(),
				evidence.identity.extension.clone(),
			);
			roster.install(host.clone(), &evidence.ui)?;
			markdown.insert(host.clone(), evidence.ui.markdown_transformers.clone());
			routes.insert(host, PublishedUiRoute {
				identity:   Arc::clone(&evidence.identity),
				dispatcher: Arc::clone(&dispatcher),
			});
		}
		let mut state = self.state.lock();
		state.roster = roster;
		state.routes = routes;
		state.markdown = markdown;
		state.markdown_revision = state.markdown_revision.wrapping_add(1);
		state.markdown_cache.clear();
		publish_command_invalidation(&mut state);
		Ok(())
	}

	/// Installs one exact sealed generation and invalidates every attached
	/// command surface after the atomic replacement succeeds.
	pub fn install(
		&self,
		host: HostKey,
		evidence: &SealedRegistryEvidence,
		dispatcher: Arc<dyn CallbackDispatcher>,
	) -> Result<(), UiRosterConflict> {
		let mut state = self.state.lock();
		state.roster.install(host.clone(), &evidence.ui)?;
		state
			.markdown
			.insert(host.clone(), evidence.ui.markdown_transformers.clone());
		state
			.routes
			.insert(host, PublishedUiRoute { identity: Arc::clone(&evidence.identity), dispatcher });
		state.markdown_revision = state.markdown_revision.wrapping_add(1);
		state.markdown_cache.clear();
		publish_command_invalidation(&mut state);
		Ok(())
	}

	/// Removes every callback owned by one host during reload or teardown and
	/// invalidates attached command surfaces.
	pub fn remove(&self, host: &HostKey) {
		let mut state = self.state.lock();
		state.roster.remove(host);
		state.routes.remove(host);
		state.markdown.remove(host);
		state.markdown_revision = state.markdown_revision.wrapping_add(1);
		state.markdown_cache.clear();
		publish_command_invalidation(&mut state);
	}

	/// Subscribes to roster replacement using the shared lifecycle vocabulary.
	pub fn subscribe(&self) -> Receiver<HeadlessLifecycleKind> {
		let (sender, receiver) = flume::unbounded();
		self.state.lock().subscribers.push(sender);
		receiver
	}

	/// Projects the current exact-generation slash declarations into the live
	/// structural command registry.
	pub fn command_generations(&self, session: &Str) -> Vec<CommandGeneration> {
		let state = self.state.lock();
		let mut declarations = BTreeMap::<HostKey, Vec<CommandDeclaration>>::new();
		for entry in state.roster.commands() {
			let Some(route) = state.routes.get(&entry.owner.host) else {
				continue;
			};
			let provenance = CommandProvenance {
				source:     sf!(
					"extension:{}:{}",
					entry.owner.host.extension(),
					entry.owner.generation
				),
				label:      Str::from(entry.owner.host.extension().as_str()),
				kind:       CommandSourceKind::Extension,
				generation: entry.owner.generation,
			};
			let callback = ControlPresentationCallbackDispatcher::new(
				Arc::clone(&route.identity),
				Arc::clone(&route.dispatcher),
			);
			declarations
				.entry(entry.owner.host.clone())
				.or_default()
				.push(CommandDeclaration::verified_extension(
					&entry.declaration,
					provenance,
					callback.command_handler(entry.clone(), session.clone()),
				));
		}
		declarations
			.into_iter()
			.map(|(host, declarations)| CommandGeneration {
				provenance:   CommandProvenance {
					source:     sf!("extension:{}", host.extension()),
					label:      Str::from(host.extension().as_str()),
					kind:       CommandSourceKind::Extension,
					generation: declarations
						.first()
						.map_or(0, |declaration| declaration.provenance.generation),
				},
				declarations: declarations.into(),
			})
			.collect()
	}

	/// Folds one settled markdown revision through every verified transformer.
	///
	/// Results are cached by roster generation, item identity, and item
	/// revision, so retained rendering never re-enters Python.
	pub async fn transform_markdown(
		&self,
		item: Str,
		item_revision: u64,
		markdown: Str,
		session: Str,
	) -> Str {
		let (roster_revision, transforms) = {
			let state = self.state.lock();
			let key = (state.markdown_revision, item.clone(), item_revision);
			if let Some(cached) = state.markdown_cache.get(&key) {
				return cached.clone();
			}
			let transforms = state
				.markdown
				.iter()
				.flat_map(|(host, declarations)| {
					let route = state.routes.get(host);
					declarations.iter().filter_map(move |declaration| {
						route.map(|route| {
							(
								declaration.clone(),
								Arc::clone(&route.identity),
								Arc::clone(&route.dispatcher),
							)
						})
					})
				})
				.collect::<Vec<_>>();
			(state.markdown_revision, transforms)
		};
		let original = markdown.clone();
		let mut transformed = markdown;
		for (declaration, identity, dispatcher) in transforms {
			let authority = ui_invocation_authority(
				sf!("markdown:{}:{item_revision}", declaration.declaration_id),
				omp_core::InvocationPhase::Settled,
				session.clone(),
			);
			let dispatch = ControlDispatch {
				operation: sf!("omp.ui.markdown_transformer"),
				arguments: serde_json::Map::from_iter([
					("name".to_owned(), Value::String(declaration.name.to_string())),
					("markdown".to_owned(), Value::String(transformed.to_string())),
				]),
				authority,
				policy: CallbackConcurrency::Serialized,
				deadline: omp_envd::exthost::EventDeadline {
					at: Instant::now() + RENDER_CALLBACK_DEADLINE,
				},
			};
			match dispatcher.dispatch(identity, dispatch).await {
				Ok(Value::String(value)) => transformed = Str::new(value),
				Ok(_) => {
					tracing::warn!(
						declaration = %declaration.declaration_id,
						item = %item,
						item_revision,
						"markdown transformer returned a non-string; original markdown retained"
					);
					transformed = original.clone();
					break;
				},
				Err(error) => {
					tracing::warn!(
						declaration = %declaration.declaration_id,
						item = %item,
						item_revision,
						%error,
						"markdown transformer failed; original markdown retained"
					);
					transformed = original.clone();
					break;
				},
			}
		}
		let mut state = self.state.lock();
		if state.markdown_revision == roster_revision {
			state
				.markdown_cache
				.insert((roster_revision, item, item_revision), transformed.clone());
		}
		transformed
	}

	/// Resolves one normalized shortcut to its exact callback route.
	pub fn shortcuts(&self) -> Vec<UiShortcutRosterEntry> {
		self.state.lock().roster.shortcuts().cloned().collect()
	}

	/// Resolves one normalized shortcut to its exact callback route.
	pub fn shortcut(
		&self,
		chord: &str,
	) -> Option<(UiShortcutRosterEntry, Arc<ControlConnectionIdentity>, Arc<dyn CallbackDispatcher>)>
	{
		let state = self.state.lock();
		let entry = state.roster.shortcut(chord)?.clone();
		let route = state.routes.get(&entry.owner.host)?;
		Some((entry, Arc::clone(&route.identity), Arc::clone(&route.dispatcher)))
	}

	/// Runs exact-revision Python folds for one host-observed render transition.
	///
	/// `native_authoritative` keeps a native exact-revision base and admits only
	/// extension decorations. Otherwise the extension base may replace the
	/// supplied generic fallback. The returned TML is retained by the caller;
	/// repaint and replay consume that retained value without re-entering
	/// Python.
	pub async fn render_tool(
		&self,
		identity: &omp_tool::ToolIdentity,
		view: Value,
		ctx: Value,
		native: omp_chat_ui::ToolViewContent,
		native_authoritative: bool,
		session: Str,
	) -> omp_chat_ui::ToolViewContent {
		let routes = {
			let state = self.state.lock();
			state
				.roster
				.renderers(identity)
				.filter_map(|entry| {
					state.routes.get(&entry.owner.host).map(|route| {
						(entry.clone(), Arc::clone(&route.identity), Arc::clone(&route.dispatcher))
					})
				})
				.collect::<Vec<_>>()
		};
		let call_id = view
			.get("call_id")
			.and_then(Value::as_str)
			.unwrap_or("renderer");
		let settled = view
			.get("verdict")
			.is_some_and(|verdict| !verdict.is_null());
		let mut rendered = native;
		for (entry, target, dispatcher) in routes {
			if native_authoritative && !entry.declaration.decorates {
				continue;
			}
			let authority = ui_invocation_authority(
				sf!("renderer:{}:{call_id}", entry.declaration.declaration_id),
				if settled {
					omp_core::InvocationPhase::Settled
				} else {
					omp_core::InvocationPhase::Open
				},
				session.clone(),
			);
			let dispatch = ControlDispatch {
				operation: sf!("omp.ui.renderer"),
				arguments: serde_json::Map::from_iter([
					("name".to_owned(), Value::String(identity.name.to_string())),
					("family".to_owned(), Value::String(identity.rev.family.to_string())),
					("rev".to_owned(), Value::from(identity.rev.n)),
					("view".to_owned(), view.clone()),
					("ctx".to_owned(), ctx.clone()),
				]),
				authority,
				policy: CallbackConcurrency::Serialized,
				deadline: omp_envd::exthost::EventDeadline {
					at: Instant::now() + RENDER_CALLBACK_DEADLINE,
				},
			};
			let Ok(value) = dispatcher.dispatch(target, dispatch).await else {
				continue;
			};
			let source = value
				.as_object()
				.and_then(|value| value.get("source"))
				.and_then(Value::as_str)
				.or_else(|| value.as_str());
			let Some(source) = source else {
				continue;
			};
			if source.len() > RENDER_TML_MAX_BYTES
				|| omp_tui::Ui::from_markup(Str::new(source), 1, omp_tui::UiContext::default()).is_err()
			{
				continue;
			}
			if entry.declaration.decorates {
				let base = match rendered {
					omp_chat_ui::ToolViewContent::Markup(source) => source,
					omp_chat_ui::ToolViewContent::Plain(source) => tml_text(&source),
				};
				let mut composed = omp_core::StrMut::with_capacity(base.len() + source.len());
				composed.push_str(&base);
				composed.push_str(source);
				rendered = omp_chat_ui::ToolViewContent::Markup(composed.freeze());
			} else {
				rendered = omp_chat_ui::ToolViewContent::Markup(Str::new(source));
			}
		}
		rendered
	}
}

fn tml_text(value: &str) -> Str {
	let mut escaped = omp_core::StrMut::with_capacity(value.len() + 13);
	escaped.push_str("<text>");
	for character in value.chars() {
		match character {
			'&' => escaped.push_str("&amp;"),
			'<' => escaped.push_str("&lt;"),
			'>' => escaped.push_str("&gt;"),
			_ => escaped.push(character),
		}
	}
	escaped.push_str("</text>");
	escaped.freeze()
}

fn merge_completion_rule(rules: &mut BTreeMap<Str, CompletionRule>, rule: CompletionRule) {
	if let Some(existing) = rules.get_mut(&rule.prefix) {
		existing.at_line_start &= rule.at_line_start;
		existing.min_chars = existing.min_chars.min(rule.min_chars);
		existing.debounce = existing.debounce.min(rule.debounce);
		existing.max_results = existing
			.max_results
			.saturating_add(rule.max_results)
			.min(100);
		existing.cache = existing.cache.min(rule.cache);
		existing.refine_locally &= rule.refine_locally;
	} else {
		rules.insert(rule.prefix.clone(), rule);
	}
}

struct PublishedCompletionSource {
	roster:   Arc<PublishedUiRoster>,
	session:  Str,
	runtime:  tokio::runtime::Handle,
	fallback: Arc<dyn CompletionSource>,
	next_id:  AtomicU64,
}

struct CompletionInvocation {
	owner:       UiCallbackOwner,
	target:      Arc<ControlConnectionIdentity>,
	dispatcher:  Arc<dyn CallbackDispatcher>,
	query:       WireCompletionQuery,
	max_results: usize,
	stem:        Option<Str>,
}

impl CompletionSource for PublishedCompletionSource {
	fn complete(&self, query: ComposerCompletionQuery) -> SuggestionList {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed).max(1);
		let mut items =
			self
				.runtime
				.block_on(complete_published_ui(&self.roster, &self.session, id, &query));
		items.extend(self.fallback.complete(query));
		items
	}
}

async fn complete_published_ui(
	roster: &PublishedUiRoster,
	session: &Str,
	id: u64,
	query: &ComposerCompletionQuery,
) -> SuggestionList {
	let invocations = {
		let state = roster.state.lock();
		let command = query
			.prefix
			.strip_prefix("/")
			.and_then(|prefix| prefix.strip_suffix(" "))
			.and_then(|spelling| state.roster.command(spelling.as_str()))
			.filter(|entry| entry.declaration.arg_completion_callback.is_some());
		if let Some(entry) = command {
			let Some(route) = state.routes.get(&entry.owner.host) else {
				return SuggestionList::new();
			};
			let arguments = completion_arg_query(query.query.as_str());
			let stem_len = query.query.len().saturating_sub(arguments.prefix.len());
			let mut stem =
				omp_core::StrMut::with_capacity(query.prefix.len().saturating_add(stem_len));
			stem.push_str(&query.prefix);
			stem.push_str(&query.query[..stem_len]);
			let mut owner = entry.owner.clone();
			owner.callback = Str::from(
				entry
					.declaration
					.arg_completion_callback
					.as_deref()
					.expect("filtered command has an argument completion callback"),
			);
			vec![CompletionInvocation {
				owner,
				target: Arc::clone(&route.identity),
				dispatcher: Arc::clone(&route.dispatcher),
				query: WireCompletionQuery {
					trigger: query.prefix.to_string(),
					text:    arguments.prefix.to_string(),
					cursor:  arguments.prefix.len().min(u32::MAX as usize) as u32,
					argv:    arguments.argv.iter().map(ToString::to_string).collect(),
					command: Some(entry.declaration.name.clone()),
				},
				max_results: query.rule.max_results,
				stem: Some(stem.freeze()),
			}]
		} else {
			state
				.roster
				.completions_for(query.prefix.as_str())
				.filter_map(|entry| {
					let route = state.routes.get(&entry.owner.host)?;
					Some(CompletionInvocation {
						owner:       entry.owner.clone(),
						target:      Arc::clone(&route.identity),
						dispatcher:  Arc::clone(&route.dispatcher),
						query:       WireCompletionQuery {
							trigger: query.prefix.to_string(),
							text:    query.query.to_string(),
							cursor:  query.query.len().min(u32::MAX as usize) as u32,
							argv:    Vec::new(),
							command: None,
						},
						max_results: entry.declaration.max_results as usize,
						stem:        None,
					})
				})
				.collect()
		}
	};
	let results = join_all(
		invocations
			.into_iter()
			.enumerate()
			.map(|(offset, invocation)| {
				let session = session.clone();
				async move {
					let authority = ui_invocation_authority(
						sf!(
							"ui-completion:{}:{}",
							invocation.owner.declaration_id,
							id.wrapping_add(offset as u64).max(1)
						),
						omp_core::InvocationPhase::Open,
						session,
					);
					let dispatch = UiCallbackDispatch {
						dispatch: UiDispatch {
							kind:           Some(ui_dispatch::Kind::Completion(invocation.query)),
							generation:     invocation.owner.generation,
							declaration_id: invocation.owner.declaration_id.to_string(),
							props:          None,
						},
						owner:    invocation.owner,
					};
					let result = invocation
						.dispatcher
						.dispatch_ui(invocation.target, authority, dispatch, COMPLETION_CALLBACK_DEADLINE)
						.await;
					(invocation.max_results, invocation.stem, result)
				}
			}),
	)
	.await;
	let mut suggestions = SuggestionList::new();
	for (max_results, stem, result) in results {
		let Ok(mut result) = result else {
			continue;
		};
		result
			.candidates
			.sort_by_key(|candidate| std::cmp::Reverse(candidate.sort));
		for candidate in result.candidates.into_iter().take(max_results) {
			if let Some(suggestion) = completion_suggestion(candidate, stem.as_deref()) {
				suggestions.push(suggestion);
			}
		}
	}
	suggestions
}

fn completion_suggestion(candidate: CompletionCandidate, stem: Option<&str>) -> Option<Suggestion> {
	if candidate.value.is_empty() {
		return None;
	}
	let insert = if let Some(stem) = stem {
		let mut value = omp_core::StrMut::with_capacity(stem.len() + candidate.value.len());
		value.push_str(stem);
		value.push_str(&candidate.value);
		value.freeze()
	} else {
		Str::new(candidate.value.as_str())
	};
	let label = candidate
		.display
		.as_deref()
		.unwrap_or(candidate.value.as_str());
	let mut suggestion = Suggestion::new(insert, label);
	if let Some(description) = candidate.description {
		suggestion = suggestion.with_description(description);
	}
	if let Some(hint) = candidate.hint {
		suggestion = suggestion.with_hint(hint);
	}
	if let Some(group) = candidate.group {
		suggestion = suggestion.with_category(group);
	}
	if let Some(icon) = candidate.icon.and_then(|icon| Icon::from_name(&icon)) {
		suggestion = suggestion.with_icon(icon);
	}
	Some(suggestion)
}

fn publish_command_invalidation(state: &mut PublishedUiState) {
	state.subscribers.retain(|subscriber| {
		subscriber
			.send(HeadlessLifecycleKind::CommandRosterInvalidated)
			.is_ok()
	});
}

/// CONTROL-backed dispatcher which forwards callbacks to the exact live
/// extension worker.
pub struct ControlPresentationCallbackDispatcher {
	target:     Arc<ControlConnectionIdentity>,
	dispatcher: Arc<dyn CallbackDispatcher>,
}

impl ControlPresentationCallbackDispatcher {
	/// Binds an authenticated generation to the live supervisor dispatcher.
	#[must_use]
	pub fn new(
		target: Arc<ControlConnectionIdentity>,
		dispatcher: Arc<dyn CallbackDispatcher>,
	) -> Self {
		Self { target, dispatcher }
	}

	/// Builds a slash-command handler bound to one manifest-verified roster
	/// entry and the exact live worker generation.
	pub fn command_handler(
		&self,
		entry: UiCommandRosterEntry,
		session: Str,
	) -> Arc<dyn ExtensionCommandHandler> {
		Arc::new(ControlExtensionCommandHandler {
			target: Arc::clone(&self.target),
			dispatcher: Arc::clone(&self.dispatcher),
			owner: entry.owner,
			session,
			next_id: AtomicU64::new(1),
		})
	}

	/// Dispatches one locally matched shortcut. Callback failures are dropped
	/// after the chord has already been consumed.
	pub async fn dispatch_shortcut(
		&self,
		entry: &UiShortcutRosterEntry,
		session: Str,
		chord: Str,
		phase: Str,
	) -> bool {
		let authority = ui_invocation_authority(
			sf!("ui-shortcut:{}:{}", entry.owner.declaration_id, entry.owner.generation),
			omp_core::InvocationPhase::Open,
			session,
		);
		let dispatch = UiCallbackDispatch {
			owner:    entry.owner.clone(),
			dispatch: UiDispatch {
				kind:           Some(ui_dispatch::Kind::Shortcut(ShortcutInvoked {
					action_id: entry.declaration.action_id.clone(),
					chord:     chord.to_string(),
					phase:     phase.to_string(),
				})),
				generation:     entry.owner.generation,
				declaration_id: entry.owner.declaration_id.to_string(),
				props:          None,
			},
		};
		self
			.dispatcher
			.dispatch_ui(Arc::clone(&self.target), authority, dispatch, Duration::from_secs(30))
			.await
			.ok()
			.and_then(|result| result.result)
			.is_some_and(|result| matches!(result, ui_dispatch_result::Result::Shortcut(_)))
	}
}
struct ControlExtensionCommandHandler {
	target:     Arc<ControlConnectionIdentity>,
	dispatcher: Arc<dyn CallbackDispatcher>,
	owner:      UiCallbackOwner,
	session:    Str,
	next_id:    AtomicU64,
}

impl ExtensionCommandHandler for ControlExtensionCommandHandler {
	fn call(
		&self,
		invocation: ExtensionCommandInvocation,
		provenance: CommandProvenance,
	) -> super::commands::ExtensionCommandFuture {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed).max(1);
		let target = Arc::clone(&self.target);
		let dispatcher = Arc::clone(&self.dispatcher);
		let owner = self.owner.clone();
		let session = self.session.clone();
		Box::pin(async move {
			let authority = ui_invocation_authority(
				sf!("ui-command:{}:{id}", owner.declaration_id),
				omp_core::InvocationPhase::EffectsAuthorized,
				session,
			);
			let dispatch = UiCallbackDispatch {
				dispatch: UiDispatch {
					kind:           Some(ui_dispatch::Kind::Command(CommandInvoked {
						name: invocation.name.to_string(),
						argv: invocation.argv.iter().map(ToString::to_string).collect(),
						raw:  invocation.raw.to_string(),
						mode: command_surface_name(invocation.surface).to_owned(),
					})),
					generation:     owner.generation,
					declaration_id: owner.declaration_id.to_string(),
					props:          None,
				},
				owner,
			};
			let result = dispatcher
				.dispatch_ui(target, authority, dispatch, Duration::from_secs(30))
				.await
				.map_err(|error| miette::miette!("{error}"))?;
			command_result(result, provenance)
		})
	}
}
fn command_result(
	result: UiDispatchResult,
	provenance: CommandProvenance,
) -> miette::Result<CommandResult> {
	match result.result {
		Some(ui_dispatch_result::Result::Command(CommandDispatchResult {
			outcome: Some(command_dispatch_result::Outcome::Prompt(text)),
			..
		})) => Ok(CommandResult::Prompt(PromptResult { text: Str::new(text), provenance })),
		Some(ui_dispatch_result::Result::Command(CommandDispatchResult {
			outcome: Some(command_dispatch_result::Outcome::Consumed(tml)),
			..
		})) => Ok(CommandResult::Consumed(ConsumedResult {
			status:        (!tml.source.is_empty())
				.then(|| Str::from(String::from_utf8_lossy(&tml.source).as_ref())),
			agent_invoked: false,
		})),
		Some(ui_dispatch_result::Result::Command(_)) => {
			Ok(CommandResult::Consumed(ConsumedResult::silent()))
		},
		Some(ui_dispatch_result::Result::Error(error)) => {
			Err(miette::miette!("extension command failed: {}", error.message))
		},
		_ => Err(miette::miette!("extension command returned an incompatible UI result")),
	}
}

fn ui_invocation_authority(
	invocation: Str,
	phase: omp_core::InvocationPhase,
	session: Str,
) -> ControlInvocationAuthority {
	ControlInvocationAuthority {
		invocation,
		phase,
		session,
		turn: None,
		event: None,
		call: None,
		device: None,
		effects: Box::new([]),
		place_kind: sf!("host"),
		lifecycle: omp_core::LifecyclePhase::Active,
		roots: Box::new([]),
		remote: false,
		has_ui: true,
		headless: false,
		settings: serde_json::Map::new(),
		secret_settings: Box::new([]),
		data: None,
		direct_filesystem: None,
	}
}

const fn command_surface_name(surface: CommandSurface) -> &'static str {
	match surface {
		CommandSurface::Tui => "interactive",
		CommandSurface::Acp => "acp",
		CommandSurface::Text => "text",
	}
}

#[async_trait]
impl PresentationCallbackDispatcher for ControlPresentationCallbackDispatcher {
	async fn dispatch(
		&self,
		identity: Arc<PresentationIdentity>,
		invocation: ControlInvocationAuthority,
		callback: PresentationCallback,
	) -> Result<Value, PresentationAuthorityError> {
		if self.target.principal.id() != identity.principal.as_str()
			|| self.target.extension != identity.extension
			|| self.target.artifact_digest != identity.artifact_digest
			|| self.target.host_generation != identity.host_generation
			|| self.target.session_generation != identity.session_generation
		{
			return Err(PresentationAuthorityError::Identity);
		}
		if callback.kind == PresentationCallbackKind::Action
			&& matches!(callback.operation.as_str(), "omp.ui.command" | "omp.ui.shortcut")
		{
			return Err(PresentationAuthorityError::Owner(Str::new_static(
				"commands and shortcuts require the typed UI callback route",
			)));
		}
		let arguments = match callback.arguments {
			Value::Object(arguments) => arguments,
			value => serde_json::Map::from_iter([("value".to_owned(), value)]),
		};
		let timeout = match callback.kind {
			PresentationCallbackKind::Completion => COMPLETION_CALLBACK_DEADLINE,
			PresentationCallbackKind::Renderer => RENDER_CALLBACK_DEADLINE,
			PresentationCallbackKind::Action => time::Duration::from_secs(365 * 24 * 60 * 60),
		};
		self
			.dispatcher
			.dispatch(self.target.clone(), ControlDispatch {
				operation: callback.operation,
				arguments,
				authority: invocation,
				policy: CallbackConcurrency::Serialized,
				deadline: omp_envd::exthost::EventDeadline { at: Instant::now() + timeout },
			})
			.await
			.map_err(|error| PresentationAuthorityError::Owner(Str::new(error.to_string())))
	}
}

/// Drop guard for one exact callback registration. Replacing a callback makes
/// an older guard inert.
pub struct PresentationCallbackRegistration {
	inner:        Weak<CallbackInner>,
	class:        u8,
	operation:    Str,
	registration: u64,
}

impl Drop for PresentationCallbackRegistration {
	fn drop(&mut self) {
		let Some(inner) = self.inner.upgrade() else {
			return;
		};
		let mut state = inner.state.lock();
		let key = (self.class, self.operation.clone());
		if state
			.entries
			.get(&key)
			.is_some_and(|entry| entry.registration == self.registration)
		{
			state.entries.remove(&key);
		}
	}
}

const fn callback_class(kind: PresentationCallbackKind) -> u8 {
	match kind {
		PresentationCallbackKind::Completion => 0,
		PresentationCallbackKind::Renderer => 1,
		PresentationCallbackKind::Action => 2,
	}
}
#[cfg(test)]
mod tests {
	use std::{
		collections::BTreeSet,
		sync::atomic::{AtomicUsize, Ordering},
	};

	use super::*;

	fn identity() -> Arc<PresentationIdentity> {
		Arc::new(PresentationIdentity {
			principal:          Str::new_static("principal"),
			extension:          Str::new_static("ui"),
			artifact_digest:    Str::new_static("digest"),
			host_generation:    1,
			session_generation: 1,
			capabilities:       Arc::new(BTreeSet::new()),
		})
	}

	fn invocation() -> ControlInvocationAuthority {
		ControlInvocationAuthority {
			invocation:        sf!("command:1"),
			phase:             omp_core::InvocationPhase::EffectsAuthorized,
			session:           sf!("session"),
			turn:              None,
			event:             None,
			call:              None,
			device:            None,
			effects:           Box::new([]),
			place_kind:        sf!("host"),
			lifecycle:         omp_core::LifecyclePhase::Active,
			roots:             Box::new([]),
			remote:            false,
			has_ui:            true,
			headless:          false,
			settings:          serde_json::Map::new(),
			secret_settings:   Box::new([]),
			data:              None,
			direct_filesystem: None,
		}
	}

	struct MarkdownDispatcher {
		calls: Arc<AtomicUsize>,
		fail:  bool,
	}

	struct CompletionDispatcher {
		dispatches: Arc<Mutex<Vec<UiDispatch>>>,
	}

	#[async_trait]
	impl CallbackDispatcher for CompletionDispatcher {
		async fn dispatch(
			&self,
			_target: Arc<ControlConnectionIdentity>,
			_dispatch: ControlDispatch,
		) -> Result<Value, omp_envd::exthost::control::ControlProtocolError> {
			Ok(Value::Null)
		}

		async fn dispatch_ui(
			&self,
			_target: Arc<ControlConnectionIdentity>,
			_authority: ControlInvocationAuthority,
			dispatch: UiCallbackDispatch,
			_timeout: Duration,
		) -> Result<UiDispatchResult, omp_envd::exthost::control::ControlProtocolError> {
			self.dispatches.lock().push(dispatch.dispatch);
			Ok(UiDispatchResult {
				candidates: vec![CompletionCandidate {
					value:       "two".to_owned(),
					display:     Some("Two".to_owned()),
					description: Some("second".to_owned()),
					hint:        None,
					group:       Some("Fixture".to_owned()),
					icon:        None,
					sort:        7,
				}],
				..Default::default()
			})
		}
	}

	struct RendererDispatcher {
		source: &'static str,
	}

	#[async_trait]
	impl CallbackDispatcher for RendererDispatcher {
		async fn dispatch(
			&self,
			_target: Arc<ControlConnectionIdentity>,
			dispatch: ControlDispatch,
		) -> Result<Value, omp_envd::exthost::control::ControlProtocolError> {
			assert_eq!(dispatch.operation, "omp.ui.renderer");
			Ok(serde_json::json!({ "source": self.source }))
		}
	}

	#[async_trait]
	impl CallbackDispatcher for MarkdownDispatcher {
		async fn dispatch(
			&self,
			_target: Arc<ControlConnectionIdentity>,
			dispatch: ControlDispatch,
		) -> Result<Value, omp_envd::exthost::control::ControlProtocolError> {
			self.calls.fetch_add(1, Ordering::Relaxed);
			if self.fail {
				return Err(omp_envd::exthost::control::ControlProtocolError::new(
					"TransformerFailed",
					"transformer raised",
				));
			}
			let markdown = dispatch
				.arguments
				.get("markdown")
				.and_then(Value::as_str)
				.unwrap_or_default();
			Ok(Value::String(format!("{markdown}!")))
		}
	}

	fn control_identity() -> Arc<ControlConnectionIdentity> {
		Arc::new(ControlConnectionIdentity {
			extension:          sf!("ui"),
			principal:          omp_envd::exthost::Principal::new(sf!("principal"), sf!("Principal")),
			artifact_digest:    sf!("digest"),
			layer:              sf!("workspace"),
			tier:               sf!("trusted"),
			trust:              sf!("trusted"),
			host_generation:    1,
			session_generation: 1,
			capabilities:       Arc::new(BTreeSet::new()),
		})
	}

	fn markdown_roster(dispatcher: Arc<dyn CallbackDispatcher>) -> PublishedUiRoster {
		let roster = PublishedUiRoster::default();
		let host = HostKey::new(sf!("workspace"), sf!("trusted"), sf!("ui"));
		let mut state = roster.state.lock();
		state.markdown_revision = 1;
		state.markdown.insert(
			host.clone(),
			vec![VerifiedMarkdownTransformer {
				declaration_id: sf!("markdown"),
				name:           sf!("math"),
				callback:       sf!("fixture:transform"),
				module:         sf!("fixture"),
			}]
			.into_boxed_slice(),
		);
		state
			.routes
			.insert(host, PublishedUiRoute { identity: control_identity(), dispatcher });
		drop(state);
		roster
	}

	fn completion_roster(dispatcher: Arc<dyn CallbackDispatcher>) -> Arc<PublishedUiRoster> {
		let roster = Arc::new(PublishedUiRoster::default());
		let host = HostKey::new(sf!("workspace"), sf!("trusted"), sf!("ui"));
		let verified = omp_envd::exthost::VerifiedUiRoster {
			generation: 1,
			extension: sf!("ui"),
			commands: vec![omp_proto::ui::v1::CommandDecl {
				name: "review".to_owned(),
				description: "Review".to_owned(),
				declaration_id: "command".to_owned(),
				callback: "fixture.command".to_owned(),
				module: "fixture".to_owned(),
				activation_trigger: "before_ui_input".to_owned(),
				arg_completion_callback: Some("fixture.complete_args".to_owned()),
				..Default::default()
			}]
			.into_boxed_slice(),
			triggers: vec![omp_proto::ui::v1::TriggerDecl {
				prefix: "#".to_owned(),
				kind: "completion".to_owned(),
				min_chars: 1,
				debounce_ms: 90,
				max_results: 9,
				cache_ms: 2_000,
				refine_locally: true,
				declaration_id: "issues".to_owned(),
				callback: "fixture.issues".to_owned(),
				module: "fixture".to_owned(),
				activation_trigger: "before_ui_input".to_owned(),
				..Default::default()
			}]
			.into_boxed_slice(),
			..Default::default()
		};
		let mut state = roster.state.lock();
		state
			.roster
			.install(host.clone(), &verified)
			.expect("completion roster");
		state
			.routes
			.insert(host, PublishedUiRoute { identity: control_identity(), dispatcher });
		drop(state);
		roster
	}

	#[tokio::test]
	async fn completion_projection_and_typed_round_trip_reach_composer_rows() {
		let dispatches = Arc::new(Mutex::new(Vec::new()));
		let roster =
			completion_roster(Arc::new(CompletionDispatcher { dispatches: Arc::clone(&dispatches) }));
		let rules = roster.completion_rules();
		assert!(
			rules
				.iter()
				.any(|rule| { rule.prefix == "#" && rule.min_chars == 1 && rule.max_results == 9 })
		);
		assert!(rules.iter().any(|rule| rule.prefix == "/review "));

		let trigger_rule = rules
			.iter()
			.find(|rule| rule.prefix == "#")
			.expect("trigger rule")
			.clone();
		let trigger = ComposerCompletionQuery {
			prefix_start: 0,
			prefix:       sf!("#"),
			trigger:      CompletionTrigger::Extension,
			query:        sf!("12"),
			rule:         trigger_rule,
		};
		let rows = complete_published_ui(&roster, &sf!("session"), 1, &trigger).await;
		assert_eq!(rows[0].value(), "two");

		let command_rule = rules
			.iter()
			.find(|rule| rule.prefix == "/review ")
			.expect("command rule")
			.clone();
		let command = ComposerCompletionQuery {
			prefix_start: 0,
			prefix:       sf!("/review "),
			trigger:      CompletionTrigger::Extension,
			query:        sf!("one tw"),
			rule:         command_rule,
		};
		let rows = complete_published_ui(&roster, &sf!("session"), 2, &command).await;
		assert_eq!(rows[0].value(), "/review one two");

		let dispatches = dispatches.lock();
		let Some(ui_dispatch::Kind::Completion(trigger)) = dispatches[0].kind.as_ref() else {
			panic!("completion dispatch");
		};
		assert_eq!(trigger.trigger, "#");
		assert_eq!(trigger.text, "12");
		let Some(ui_dispatch::Kind::Completion(command)) = dispatches[1].kind.as_ref() else {
			panic!("command completion dispatch");
		};
		assert_eq!(command.command.as_deref(), Some("review"));
		assert_eq!(command.argv, ["one"]);
		assert_eq!(command.text, "tw");
	}

	fn renderer_declaration(
		identity: omp_tool::ToolIdentity,
		decorates: bool,
	) -> omp_envd::exthost::VerifiedRendererDeclaration {
		omp_envd::exthost::VerifiedRendererDeclaration {
			declaration_id: sf!("renderer"),
			identity,
			callback: sf!("fixture.render"),
			reduce: None,
			decorates,
			module: sf!("fixture"),
		}
	}

	#[tokio::test]
	async fn extension_renderer_replaces_generic_base_and_decorates_native_base() {
		let identity = omp_tool::ToolIdentity {
			name: sf!("counter"),
			rev:  omp_tool::Rev { family: sf!("counter"), n: 2 },
		};
		let roster = PublishedUiRoster::default();
		let base_host = HostKey::new(sf!("workspace"), sf!("trusted"), sf!("base"));
		let decoration_host = HostKey::new(sf!("workspace"), sf!("trusted"), sf!("decoration"));
		{
			let mut state = roster.state.lock();
			state
				.roster
				.install(base_host.clone(), &omp_envd::exthost::VerifiedUiRoster {
					generation: 1,
					extension: sf!("base"),
					renderers: vec![renderer_declaration(identity.clone(), false)].into_boxed_slice(),
					..Default::default()
				})
				.unwrap();
			state
				.roster
				.install(decoration_host.clone(), &omp_envd::exthost::VerifiedUiRoster {
					generation: 1,
					extension: sf!("decoration"),
					renderers: vec![renderer_declaration(identity.clone(), true)].into_boxed_slice(),
					..Default::default()
				})
				.unwrap();
			state.routes.insert(base_host, PublishedUiRoute {
				identity:   control_identity(),
				dispatcher: Arc::new(RendererDispatcher { source: "<text>base</text>" }),
			});
			state.routes.insert(decoration_host, PublishedUiRoute {
				identity:   control_identity(),
				dispatcher: Arc::new(RendererDispatcher { source: "<text>decoration</text>" }),
			});
		}
		let view = serde_json::json!({ "call_id": "call-1", "updates": [], "verdict": null });
		let ctx = serde_json::json!({});
		assert_eq!(
			roster
				.render_tool(
					&identity,
					view.clone(),
					ctx.clone(),
					omp_chat_ui::ToolViewContent::Plain(sf!("<generic/>")),
					false,
					sf!("session"),
				)
				.await,
			omp_chat_ui::ToolViewContent::Markup(sf!("<text>base</text><text>decoration</text>")),
		);
		assert_eq!(
			roster
				.render_tool(
					&identity,
					view,
					ctx,
					omp_chat_ui::ToolViewContent::Markup(sf!("<text>native</text>")),
					true,
					sf!("session"),
				)
				.await,
			omp_chat_ui::ToolViewContent::Markup(sf!("<text>native</text><text>decoration</text>")),
		);
	}

	#[tokio::test]
	async fn markdown_transformer_caches_each_revision_and_fails_open() {
		let calls = Arc::new(AtomicUsize::new(0));
		let roster =
			markdown_roster(Arc::new(MarkdownDispatcher { calls: calls.clone(), fail: false }));
		for _ in 0..2 {
			assert_eq!(
				roster
					.transform_markdown(sf!("item"), 1, sf!("math"), sf!("session"))
					.await
					.as_str(),
				"math!"
			);
		}
		assert_eq!(calls.load(Ordering::Relaxed), 1);
		let _ = roster
			.transform_markdown(sf!("item"), 2, sf!("math"), sf!("session"))
			.await;
		assert_eq!(calls.load(Ordering::Relaxed), 2);

		let failing = markdown_roster(Arc::new(MarkdownDispatcher {
			calls: Arc::new(AtomicUsize::new(0)),
			fail:  true,
		}));
		assert_eq!(
			failing
				.transform_markdown(sf!("item"), 1, sf!("original"), sf!("session"))
				.await
				.as_str(),
			"original"
		);
	}

	#[tokio::test]
	async fn bridge_correlates_replies_and_tears_down_waiters() {
		let bridge = PresentationBridge::new(4);
		let endpoint = bridge.attach();
		let client = bridge.clone();
		let request = tokio::spawn(async move {
			client
				.request(identity(), PresentationRequest::Presentation)
				.await
		});
		let dispatch = endpoint.recv().await.unwrap();
		endpoint
			.complete(
				dispatch.id,
				Ok(PresentationResponse::Presentation(serde_json::json!({"attached": true}))),
			)
			.unwrap();
		assert_eq!(
			request.await.unwrap().unwrap(),
			PresentationResponse::Presentation(serde_json::json!({"attached": true}))
		);

		let client = bridge.clone();
		let request = tokio::spawn(async move {
			client
				.request(identity(), PresentationRequest::EditorText)
				.await
		});
		endpoint.recv().await.unwrap();
		drop(endpoint);
		assert_eq!(request.await.unwrap(), Err(PresentationAuthorityError::Unavailable));
	}
	#[tokio::test]
	async fn action_callbacks_deliver_exact_arguments_and_refuse_stale_generations() {
		let identity = identity();
		let registry = PresentationCallbackRegistry::new(identity.clone());
		let calls = Arc::new(AtomicUsize::new(0));
		let received = Arc::new(Mutex::new(None));
		let calls_for_handler = calls.clone();
		let received_for_handler = received.clone();
		let registration = registry.register_action(
			"extension.command",
			Arc::new(move |arguments: Value| {
				calls_for_handler.fetch_add(1, Ordering::Relaxed);
				*received_for_handler.lock() = Some(arguments.clone());
				async move { Ok(arguments) }
			}),
		);
		let arguments = serde_json::json!({
			"name": "alias",
			"argv": ["one", "two"],
			"raw": "one two",
		});
		let result = registry
			.dispatch(identity.clone(), invocation(), PresentationCallback {
				kind:      PresentationCallbackKind::Action,
				operation: sf!("extension.command"),
				arguments: arguments.clone(),
			})
			.await
			.expect("exact callback");
		assert_eq!(result, arguments);
		assert_eq!(received.lock().as_ref(), Some(&arguments));
		assert_eq!(calls.load(Ordering::Relaxed), 1);

		let mut stale = (*identity).clone();
		stale.host_generation += 1;
		assert_eq!(
			registry
				.dispatch(Arc::new(stale), invocation(), PresentationCallback {
					kind:      PresentationCallbackKind::Action,
					operation: sf!("extension.command"),
					arguments: Value::Null,
				},)
				.await,
			Err(PresentationAuthorityError::Identity)
		);
		drop(registration);
		assert!(matches!(
			registry
				.dispatch(identity, invocation(), PresentationCallback {
					kind:      PresentationCallbackKind::Action,
					operation: sf!("extension.command"),
					arguments: Value::Null,
				},)
				.await,
			Err(PresentationAuthorityError::Owner(_))
		));
	}
}
