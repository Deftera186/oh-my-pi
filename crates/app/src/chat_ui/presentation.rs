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
	sync::{Arc, Weak},
	time::{self, Instant},
};

use async_trait::async_trait;
use flume::Receiver;
use omp_core::{Str, sf};
use omp_envd::exthost::{
	CallbackConcurrency,
	control::{ControlConnectionIdentity, ControlDispatch, ControlInvocationAuthority},
	dispatch::CallbackDispatcher,
};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::oneshot;

use super::presentation_authority::{
	COMPLETION_CALLBACK_DEADLINE, PresentationAuthorityError, PresentationCallback,
	PresentationCallbackDispatcher, PresentationCallbackKind, PresentationClient,
	PresentationEffect, PresentationIdentity, PresentationRequest, PresentationResponse,
	RENDER_CALLBACK_DEADLINE,
};

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
