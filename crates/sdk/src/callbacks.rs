//! Typed callback seams for embedded sessions.
//!
//! Callbacks can contribute prompt patches, context projection operations,
//! opaque credential leases, and read-only events. They cannot replace provider
//! message arrays or observe secret material after lease construction.

use std::{pin::Pin, sync::Arc, time::Duration};

use futures::Future;
use omp_agent::{
	Agent, AgentEvent, ContextPatchSet as AgentContextPatchSet, ContextProjectionError,
	ContextProjectionHandler, ContextView, EventBus, PatchOp, PromptError, PromptPatchSet, Props,
	TurnClient,
};
pub use omp_core::SecretString;
use omp_core::Str;
use omp_inference::auth::{AuthRejection, CredentialSource};
pub use omp_inference::{
	AccountId, PrincipalId,
	auth::{CredentialError, CredentialLease, CredentialNeed, LeaseMeta},
};
use thiserror::Error;
use url::Url;

/// Rejected context callback output.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ContextPatchError {
	/// A committed patch must advance to a nonzero derived-IR revision.
	#[error("context patch derived IR revision must be nonzero")]
	InvalidRevision,
	/// Synthetic context bytes exceed the per-snapshot expansion ceiling.
	#[error("context patch expansion {expansion} bytes exceeds budget {budget} bytes")]
	BudgetExceeded {
		/// Maximum accepted callback bytes.
		budget:    usize,
		/// Requested callback bytes.
		expansion: usize,
	},
}

/// Context projection callback output tied to one immutable snapshot.
#[derive(Clone, Debug)]
pub struct ContextPatchCommit {
	base_snapshot_rev:   u64,
	derived_ir_revision: u32,
	patches:             Box<[PatchOp]>,
}

impl ContextPatchCommit {
	/// Default maximum synthetic context bytes per snapshot.
	pub const DEFAULT_MAX_BYTE_EXPANSION: usize = 64 * 1024;

	/// Validates one stable-id context patch commit.
	pub fn new(
		base_snapshot_rev: u64,
		derived_ir_revision: u32,
		patches: Vec<PatchOp>,
		max_byte_expansion: usize,
	) -> Result<Self, ContextPatchError> {
		if derived_ir_revision == 0 {
			return Err(ContextPatchError::InvalidRevision);
		}
		let expansion = patches.iter().fold(0usize, |total, patch| {
			total.saturating_add(match patch {
				PatchOp::Replace { text, .. } | PatchOp::Insert { text, .. } => text.len(),
				PatchOp::Prune { .. } | PatchOp::DropParts { .. } | PatchOp::Reorder { .. } => 0,
			})
		});
		if expansion > max_byte_expansion {
			return Err(ContextPatchError::BudgetExceeded { budget: max_byte_expansion, expansion });
		}
		Ok(Self { base_snapshot_rev, derived_ir_revision, patches: patches.into_boxed_slice() })
	}

	/// Returns the immutable snapshot revision observed by the callback.
	pub const fn base_snapshot_rev(&self) -> u64 {
		self.base_snapshot_rev
	}

	/// Returns the journaled derived-IR revision.
	pub const fn derived_ir_revision(&self) -> u32 {
		self.derived_ir_revision
	}

	/// Returns stable-id projection operations.
	pub fn patches(&self) -> &[PatchOp] {
		&self.patches
	}

	/// Consumes the commit into its revision fence, IR revision, and operations.
	pub fn into_parts(self) -> (u64, u32, Box<[PatchOp]>) {
		(self.base_snapshot_rev, self.derived_ir_revision, self.patches)
	}
}

/// Invalid provider-request tuning.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RequestTuningError {
	/// Authorization, cookie, and proxy-authorization headers belong to the
	/// credential lease authority.
	#[error("request tuning contains a credential-bearing header")]
	SensitiveHeader,
	/// Header syntax is not a lowercase public HTTP token.
	#[error("request tuning contains an invalid public header name")]
	InvalidHeader,
	/// A public header value contains a line break.
	#[error("request tuning contains an invalid public header value")]
	InvalidHeaderValue,
	/// Sampling temperature is non-finite or outside the provider-neutral range.
	#[error("request tuning temperature must be finite and between 0 and 2")]
	InvalidTemperature,
	/// Generated-token ceiling is zero.
	#[error("request tuning max tokens must be greater than zero")]
	InvalidMaxTokens,
	/// Stop strings and public headers exceed the bounded tuning budget.
	#[error("request tuning exceeds the 16 KiB public metadata budget")]
	BudgetExceeded,
}

/// Provider-request tuning that remains independent of wire codecs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestTuning {
	temperature:    Option<f32>,
	max_tokens:     Option<u32>,
	stop_sequences: Box<[Str]>,
	public_headers: Box<[(Str, Str)]>,
}

impl RequestTuning {
	/// Validates typed request tuning before installation.
	pub fn new(
		temperature: Option<f32>,
		max_tokens: Option<u32>,
		stop_sequences: Vec<Str>,
		public_headers: Vec<(Str, Str)>,
	) -> Result<Self, RequestTuningError> {
		if temperature.is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value)) {
			return Err(RequestTuningError::InvalidTemperature);
		}
		if max_tokens == Some(0) {
			return Err(RequestTuningError::InvalidMaxTokens);
		}
		let mut bytes = stop_sequences
			.iter()
			.fold(0usize, |total, stop| total.saturating_add(stop.len()));
		for (name, value) in &public_headers {
			if matches!(name.as_str(), "authorization" | "cookie" | "proxy-authorization") {
				return Err(RequestTuningError::SensitiveHeader);
			}
			if name.is_empty()
				|| !name
					.bytes()
					.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
			{
				return Err(RequestTuningError::InvalidHeader);
			}
			if value.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
				return Err(RequestTuningError::InvalidHeaderValue);
			}
			bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
		}
		if bytes > 16 * 1024 {
			return Err(RequestTuningError::BudgetExceeded);
		}
		Ok(Self {
			temperature,
			max_tokens,
			stop_sequences: stop_sequences.into_boxed_slice(),
			public_headers: public_headers.into_boxed_slice(),
		})
	}

	/// Returns the sampling-temperature override.
	pub const fn temperature(&self) -> Option<f32> {
		self.temperature
	}

	/// Returns the generated-token ceiling.
	pub const fn max_tokens(&self) -> Option<u32> {
		self.max_tokens
	}

	/// Returns ordered stop strings.
	pub fn stop_sequences(&self) -> &[Str] {
		&self.stop_sequences
	}

	/// Returns sanitized public headers.
	pub fn public_headers(&self) -> &[(Str, Str)] {
		&self.public_headers
	}
}

/// Non-secret request facts visible to typed tuning callbacks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestTuningInput {
	/// Canonical provider identity.
	pub provider: Str,
	/// Canonical model identity.
	pub model:    Str,
	/// Zero-based dispatch attempt.
	pub attempt:  u32,
}

/// Typed provider-request tuning callback.
pub type RequestTuningCallback =
	Arc<dyn Fn(&RequestTuningInput) -> RequestTuning + Send + Sync + 'static>;

/// Non-secret facts supplied to an SDK credential callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRequest {
	/// Inference-owned credential requirements, containing only opaque
	/// specification and affinity identities.
	pub need:       CredentialNeed,
	/// Process-local session identity.
	pub session_id: Str,
}

/// Boxed credential callback future.
///
/// Credential resolution is a cold boundary dominated by external secret or
/// OAuth I/O. The single allocation is never on token or event paths.
pub type CredentialFuture =
	Pin<Box<dyn Future<Output = Result<CredentialLease, CredentialError>> + Send + 'static>>;

/// Inference-owned opaque credential resolver.
pub type CredentialCallback =
	Arc<dyn Fn(CredentialRequest) -> CredentialFuture + Send + Sync + 'static>;

/// Adapter that installs an SDK callback at the inference credential-source
/// boundary without exposing credential stores or secret accessors.
pub struct SdkCredentialSource {
	session_id: Str,
	callback:   CredentialCallback,
}
struct SdkContextProjectionHandler(ContextPatchHandler);

impl ContextProjectionHandler for SdkContextProjectionHandler {
	fn project(
		&self,
		base_snapshot_rev: u64,
		view: &ContextView,
	) -> Result<AgentContextPatchSet, ContextProjectionError> {
		let commit = (self.0)(base_snapshot_rev, view)
			.map_err(|error| ContextProjectionError::new(error.to_string()))?;
		let (base_snapshot_rev, derived_ir_revision, patches) = commit.into_parts();
		Ok(AgentContextPatchSet::new(base_snapshot_rev, derived_ir_revision, patches))
	}
}

impl SdkCredentialSource {
	/// Creates a process-local credential source for one session.
	pub const fn new(session_id: Str, callback: CredentialCallback) -> Self {
		Self { session_id, callback }
	}
}

impl CredentialSource for SdkCredentialSource {
	fn lease(
		&self,
		need: CredentialNeed,
	) -> futures::future::BoxFuture<'_, Result<CredentialLease, CredentialError>> {
		(self.callback)(CredentialRequest { need, session_id: self.session_id.clone() })
	}

	fn reject<'a>(
		&'a self,
		_lease: &'a CredentialLease,
		_evidence: AuthRejection,
	) -> futures::future::BoxFuture<'a, Result<(), CredentialError>> {
		Box::pin(std::future::ready(Ok(())))
	}
}

/// Deterministic system-prompt callback.
///
/// The assembler renders callback sources twice against the same immutable
/// workspace and rejects drift. The returned patch set has already enforced
/// its byte-expansion ceiling.
pub type SystemPromptCallback =
	Arc<dyn Fn(&Props) -> Result<PromptPatchSet, PromptError> + Send + Sync + 'static>;

/// Stable-id context projection callback.
///
/// The first argument is the immutable durable context revision that must be
/// copied into the returned [`ContextPatchCommit`].
pub type ContextPatchHandler = Arc<
	dyn Fn(u64, &ContextView) -> Result<ContextPatchCommit, ContextPatchError>
		+ Send
		+ Sync
		+ 'static,
>;

/// Read-only agent event subscriber.
pub type EventCallback = Arc<dyn Fn(&AgentEvent) + Send + Sync + 'static>;

/// First provider dispatch notification.
pub type FirstDispatchCallback = Arc<dyn Fn(Duration) + Send + Sync + 'static>;

/// Non-secret usage-reserve facts requiring a host decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageConfirmationRequest {
	/// Candidate provider identity.
	pub provider:        Str,
	/// Candidate model identity.
	pub model:           Str,
	/// Configured reserve percentage.
	pub reserve_percent: u8,
}

/// Host decision for a usage-reserve candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum UsageConfirmationDecision {
	/// Continue with the selected account and model.
	Continue,
	/// Skip to the next authenticated fallback candidate.
	UseFallback,
}

/// Cold host-confirmation future.
pub type UsageConfirmationFuture =
	Pin<Box<dyn Future<Output = UsageConfirmationDecision> + Send + 'static>>;

/// Deferred usage-reserve confirmation authority.
pub type UsageConfirmationCallback =
	Arc<dyn Fn(UsageConfirmationRequest) -> UsageConfirmationFuture + Send + Sync + 'static>;

/// Immutable UI context update supplied by a host embedder.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UiContextUpdate {
	/// Host-defined focus or surface identifier.
	pub surface:     Option<Str>,
	/// Whether interactive prompts can currently be presented.
	pub interactive: bool,
}

/// UI context subscriber.
pub type UiContextCallback = Arc<dyn Fn(&UiContextUpdate) + Send + Sync + 'static>;

/// Result of resolving a host-owned local protocol URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolResolution {
	/// Canonical resolved URL.
	pub url:        Url,
	/// Optional media type supplied by the host.
	pub media_type: Option<Str>,
}

/// Resolver for a declared host-local URL scheme.
pub type LocalProtocolResolver =
	Arc<dyn Fn(&Url) -> Option<ProtocolResolution> + Send + Sync + 'static>;

/// Callback collection installed by [`crate::SessionBuilder`].
#[derive(Clone, Default)]
pub struct CallbackSet {
	/// Provider-system-prompt patches.
	pub system_prompt:      Option<SystemPromptCallback>,
	/// Optional title-system-prompt patches.
	pub title_prompt:       Option<SystemPromptCallback>,
	/// Provider-facing context projection.
	pub context:            Option<ContextPatchHandler>,
	/// Opaque inference credential resolution.
	pub credential:         Option<CredentialCallback>,
	/// Typed provider-request tuning.
	pub request_tuning:     Option<RequestTuningCallback>,
	/// Read-only event subscribers.
	pub events:             Vec<EventCallback>,
	/// First-dispatch notification.
	pub first_dispatch:     Option<FirstDispatchCallback>,
	/// Deferred usage-reserve confirmation.
	pub usage_confirmation: Option<UsageConfirmationCallback>,
	/// UI context subscriber.
	pub ui_context:         Option<UiContextCallback>,
	/// Declared host-local protocol resolvers.
	pub local_protocols:    Vec<(Str, LocalProtocolResolver)>,
	events_bus:             EventBus,
}

impl CallbackSet {
	/// Returns the handle-owned typed event fan-out.
	pub const fn events_bus(&self) -> &EventBus {
		&self.events_bus
	}

	pub(crate) const fn requires_production_install(&self) -> bool {
		self.title_prompt.is_some()
			|| self.credential.is_some()
			|| self.request_tuning.is_some()
			|| self.usage_confirmation.is_some()
	}
}
/// Session-bound callback authority installed into production subsystems.
///
/// The authority is clone-cheap and contains no credential material. Cold
/// revival receives the same authority so reconstructed runtimes install the
/// identical callback set rather than silently reverting to defaults.
#[derive(Clone)]
pub struct RuntimeCallbacks {
	session_id: Str,
	callbacks:  CallbackSet,
}

impl RuntimeCallbacks {
	pub(crate) const fn new(session_id: Str, callbacks: CallbackSet) -> Self {
		Self { session_id, callbacks }
	}

	/// Installs agent-owned callback adapters on a newly composed runtime.
	///
	/// [`crate::SessionHandle`] applies this automatically to both warm and
	/// cold-revived runtimes before they can accept a turn.
	pub fn configure_agent<C>(&self, agent: &mut Agent<C>)
	where
		C: TurnClient + Clone,
	{
		if let Some(callback) = &self.callbacks.context {
			agent.set_context_projection_handler(Arc::new(SdkContextProjectionHandler(Arc::clone(
				callback,
			))));
		}
	}

	/// Returns title-system-prompt patches for immutable workspace facts.
	pub fn title_prompt(&self, props: &Props) -> Option<Result<PromptPatchSet, PromptError>> {
		self.callbacks.title_prompt.as_ref().map(|callback| {
			let first = callback(props)?;
			let second = callback(props)?;
			if first != second {
				return Err(PromptError::Volatile);
			}
			Ok(first)
		})
	}

	/// Returns provider-context patches for an immutable projection.
	pub fn project_context(
		&self,
		base_snapshot_rev: u64,
		view: &ContextView,
	) -> Option<Result<ContextPatchCommit, ContextPatchError>> {
		self
			.callbacks
			.context
			.as_ref()
			.map(|callback| callback(base_snapshot_rev, view))
	}

	/// Returns the session-bound inference credential source.
	pub fn credential_source(&self) -> Option<Arc<dyn CredentialSource>> {
		self.callbacks.credential.as_ref().map(|callback| {
			Arc::new(SdkCredentialSource::new(self.session_id.clone(), Arc::clone(callback)))
				as Arc<dyn CredentialSource>
		})
	}

	/// Returns typed tuning for one provider dispatch attempt.
	pub fn tune_request(
		&self,
		provider: impl Into<Str>,
		model: impl Into<Str>,
		attempt: u32,
	) -> Option<RequestTuning> {
		self.callbacks.request_tuning.as_ref().map(|callback| {
			callback(&RequestTuningInput { provider: provider.into(), model: model.into(), attempt })
		})
	}

	/// Resolves a usage-reserve decision at candidate selection.
	pub async fn confirm_usage(
		&self,
		request: UsageConfirmationRequest,
	) -> UsageConfirmationDecision {
		match &self.callbacks.usage_confirmation {
			Some(callback) => callback(request).await,
			None => UsageConfirmationDecision::Continue,
		}
	}

	/// Publishes one host-owned UI context update.
	pub fn update_ui_context(&self, update: &UiContextUpdate) {
		if let Some(callback) = &self.callbacks.ui_context {
			callback(update);
		}
	}

	/// Resolves one URL through its declared host-local protocol boundary.
	pub fn resolve_local_protocol(&self, url: &Url) -> Option<ProtocolResolution> {
		self
			.callbacks
			.local_protocols
			.iter()
			.find(|(scheme, _)| scheme.as_str().eq_ignore_ascii_case(url.scheme()))
			.and_then(|(_, resolver)| resolver(url))
	}

	pub(crate) const fn callback_set(&self) -> &CallbackSet {
		&self.callbacks
	}
}
#[cfg(test)]
mod tests {
	use std::{
		sync::{
			Arc,
			atomic::{AtomicUsize, Ordering},
		},
		time::SystemTime,
	};

	use omp_agent::ContextView;
	use omp_catalog::AuthSpecId;
	use omp_inference::auth::{CredentialError, CredentialNeed};

	use super::*;

	#[tokio::test]
	async fn runtime_authority_dispatches_every_production_callback() {
		let title_calls = Arc::new(AtomicUsize::new(0));
		let context_calls = Arc::new(AtomicUsize::new(0));
		let credential_calls = Arc::new(AtomicUsize::new(0));
		let tuning_calls = Arc::new(AtomicUsize::new(0));
		let usage_calls = Arc::new(AtomicUsize::new(0));
		let ui_calls = Arc::new(AtomicUsize::new(0));
		let protocol_calls = Arc::new(AtomicUsize::new(0));

		let mut callbacks = CallbackSet::default();
		let count = Arc::clone(&title_calls);
		callbacks.title_prompt = Some(Arc::new(move |_| {
			count.fetch_add(1, Ordering::Relaxed);
			PromptPatchSet::new(Vec::new(), PromptPatchSet::DEFAULT_MAX_BYTE_EXPANSION)
		}));
		let count = Arc::clone(&context_calls);
		callbacks.context = Some(Arc::new(move |base_snapshot_rev, _| {
			assert_eq!(base_snapshot_rev, 7);
			count.fetch_add(1, Ordering::Relaxed);
			ContextPatchCommit::new(
				base_snapshot_rev,
				1,
				Vec::new(),
				ContextPatchCommit::DEFAULT_MAX_BYTE_EXPANSION,
			)
		}));
		let count = Arc::clone(&credential_calls);
		callbacks.credential = Some(Arc::new(move |request| {
			assert_eq!(request.session_id, "session-7");
			count.fetch_add(1, Ordering::Relaxed);
			Box::pin(std::future::ready(Err(CredentialError::Unavailable)))
		}));
		let count = Arc::clone(&tuning_calls);
		callbacks.request_tuning = Some(Arc::new(move |input| {
			assert_eq!(input.provider, "provider");
			assert_eq!(input.model, "model");
			assert_eq!(input.attempt, 2);
			count.fetch_add(1, Ordering::Relaxed);
			RequestTuning::default()
		}));
		let count = Arc::clone(&usage_calls);
		callbacks.usage_confirmation = Some(Arc::new(move |request| {
			assert_eq!(request.reserve_percent, 90);
			count.fetch_add(1, Ordering::Relaxed);
			Box::pin(std::future::ready(UsageConfirmationDecision::UseFallback))
		}));
		let count = Arc::clone(&ui_calls);
		callbacks.ui_context = Some(Arc::new(move |update| {
			assert!(update.interactive);
			count.fetch_add(1, Ordering::Relaxed);
		}));
		let count = Arc::clone(&protocol_calls);
		callbacks.local_protocols.push((
			Str::new_static("omp-local"),
			Arc::new(move |url| {
				count.fetch_add(1, Ordering::Relaxed);
				Some(ProtocolResolution {
					url:        url.clone(),
					media_type: Some("text/plain".into()),
				})
			}),
		));

		let runtime = RuntimeCallbacks::new("session-7".into(), callbacks);
		assert!(runtime.title_prompt(&Props::default()).unwrap().is_ok());
		assert!(
			runtime
				.project_context(7, &ContextView { refs: Default::default() })
				.unwrap()
				.is_ok()
		);
		let source = runtime.credential_source().expect("credential source");
		let need = CredentialNeed {
			spec:        AuthSpecId::from("auth"),
			account:     None,
			principal:   None,
			valid_after: SystemTime::UNIX_EPOCH,
		};
		assert_eq!(source.lease(need).await.unwrap_err(), CredentialError::Unavailable);
		assert!(runtime.tune_request("provider", "model", 2).is_some());
		assert_eq!(
			runtime
				.confirm_usage(UsageConfirmationRequest {
					provider:        "provider".into(),
					model:           "model".into(),
					reserve_percent: 90,
				})
				.await,
			UsageConfirmationDecision::UseFallback
		);
		runtime.update_ui_context(&UiContextUpdate {
			surface:     Some("terminal".into()),
			interactive: true,
		});
		let local = Url::parse("omp-local://asset/readme").expect("local URL");
		assert_eq!(
			runtime
				.resolve_local_protocol(&local)
				.expect("local resolution")
				.media_type
				.as_deref(),
			Some("text/plain")
		);

		assert_eq!(title_calls.load(Ordering::Relaxed), 2);
		for count in
			[context_calls, credential_calls, tuning_calls, usage_calls, ui_calls, protocol_calls]
		{
			assert_eq!(count.load(Ordering::Relaxed), 1);
		}
	}
}
