//! Durable model preferences and journal-restored session overrides.

use std::{
	collections::{BTreeMap, BTreeSet},
	net, ops,
	sync::Arc,
	time::{Duration, SystemTime},
};

use async_trait::async_trait;
use bytes::BytesMut;
use futures::StreamExt as _;
use http::header::{HeaderName, HeaderValue};
use omp_catalog::{
	AccountScope, AuthSpecKind, CredentialSourceSpec, EndpointSpec, ModelKey, ProvenanceKind,
	ThinkingEffort, capability,
	capability::{AudioFormatBits, SearchFeatureBits},
	provider::CodexTransportPreference,
	snapshot,
};
use omp_core::{InvocationPhase, LifecyclePhase, Str, sf};
use omp_envd::exthost::control::{
	ControlAuthority, ControlAuthorityFactory, ControlCompositionError, ControlConnectionIdentity,
	ControlEffect, ControlProtocolError, ControlRequestContext,
};
use omp_inference::{call, layer::stack::BuiltinConfig};
use omp_storage::blob::{BlobRef, BlobStore};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;
use url::Url;

use crate::chat::ProviderApplicationOwner;

/// Direction for model and role cycling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CycleDirection {
	/// Advance and wrap at the end.
	Forward,
	/// Move backward and wrap at the beginning.
	Backward,
}

/// One enabled model in a temporary cycle scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopedModel {
	/// Catalog model key.
	pub model:    ModelKey,
	/// Optional role-specified thinking selection.
	pub thinking: Option<ThinkingEffort>,
}

/// Journal payload for a session-only model override.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournaledModelOverride {
	/// Role whose configured model was temporarily replaced.
	pub role:     Str,
	/// Effective session model.
	pub model:    ModelKey,
	/// Optional temporary thinking selection.
	pub thinking: Option<ThinkingEffort>,
}

/// Result of bidirectional role cycling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleCycleSelection {
	/// Selected configured role.
	pub role:  Str,
	/// Model assigned to the role.
	pub model: ModelKey,
}

/// Split authority for durable preferences and journaled effective overrides.
#[derive(Clone, Debug, Default)]
pub struct ModelControls {
	durable_roles: BTreeMap<Str, ModelKey>,
	override_:     Option<JournaledModelOverride>,
	scoped:        Arc<[ScopedModel]>,
	active_role:   Option<Str>,
}

impl ModelControls {
	/// Restores durable settings without creating a journal event.
	pub fn from_durable(durable_roles: BTreeMap<Str, ModelKey>) -> Self {
		Self { durable_roles, ..Self::default() }
	}

	/// Replaces one durable `/model` preference.
	///
	/// The caller persists this through settings authority. This operation never
	/// creates or changes a session override.
	pub fn set_durable(&mut self, role: impl Into<Str>, model: ModelKey) {
		self.durable_roles.insert(role.into(), model);
	}

	/// Applies a temporary Ctrl-P or `/switch` selection and returns its journal
	/// payload.
	pub fn switch_session(
		&mut self,
		role: impl Into<Str>,
		model: ModelKey,
		thinking: Option<ThinkingEffort>,
	) -> JournaledModelOverride {
		let override_ = JournaledModelOverride { role: role.into(), model, thinking };
		self.active_role = Some(override_.role.clone());
		self.override_ = Some(override_.clone());
		override_
	}

	/// Restores the latest live override from the journal without rewriting
	/// settings.
	pub fn restore_override(&mut self, override_: Option<JournaledModelOverride>) {
		self.active_role = override_.as_ref().map(|selection| selection.role.clone());
		self.override_ = override_;
	}

	/// Clears the effective override while retaining durable preferences.
	pub fn clear_override(&mut self) {
		self.override_ = None;
		self.active_role = None;
	}

	/// Returns the effective model for a role.
	pub fn effective(&self, role: &str) -> Option<&ModelKey> {
		self
			.override_
			.as_ref()
			.filter(|selection| selection.role.as_str() == role)
			.map(|selection| &selection.model)
			.or_else(|| self.durable_roles.get(role))
	}

	/// Returns the active journaled override.
	pub const fn session_override(&self) -> Option<&JournaledModelOverride> {
		self.override_.as_ref()
	}

	/// Replaces the already-enabled temporary cycle scope.
	///
	/// Enabled-model filtering happens before this call, so disabled models
	/// cannot re-enter through cycling.
	pub fn set_scoped_models(&mut self, scoped: Arc<[ScopedModel]>) {
		self.scoped = scoped;
	}

	/// Cycles the filtered scope in either direction and journals the result.
	pub fn cycle_scoped(
		&mut self,
		current: &ModelKey<str>,
		direction: CycleDirection,
	) -> Option<JournaledModelOverride> {
		if self.scoped.len() <= 1 {
			return None;
		}
		let current_index = self
			.scoped
			.iter()
			.position(|entry| &entry.model == current)
			.unwrap_or(0);
		let index = cycle_index(current_index, self.scoped.len(), direction);
		let next = self.scoped[index].clone();
		Some(self.switch_session("temporary", next.model, next.thinking))
	}

	/// Cycles configured role models in fixed role order and either direction.
	///
	/// Missing roles and roles filtered out of `enabled` are skipped before the
	/// current position is selected.
	pub fn cycle_roles(
		&mut self,
		role_order: &[Str],
		enabled: impl Fn(&ModelKey<str>) -> bool,
		direction: CycleDirection,
	) -> Option<RoleCycleSelection> {
		let available: Vec<_> = role_order
			.iter()
			.filter_map(|role| {
				self
					.durable_roles
					.get(role)
					.filter(|model| enabled(model))
					.map(|model| (role.clone(), model.clone()))
			})
			.collect();
		if available.len() <= 1 {
			return None;
		}
		let current_index = self
			.active_role
			.as_ref()
			.and_then(|role| {
				available
					.iter()
					.position(|(candidate, _)| candidate == role)
			})
			.or_else(|| {
				self.override_.as_ref().and_then(|active| {
					available
						.iter()
						.position(|(_, model)| model == &active.model)
				})
			})
			.unwrap_or(0);
		let index = cycle_index(current_index, available.len(), direction);
		let (role, model) = available[index].clone();
		self.switch_session(role.clone(), model.clone(), None);
		Some(RoleCycleSelection { role, model })
	}
}

fn cycle_index(current: usize, len: usize, direction: CycleDirection) -> usize {
	match direction {
		CycleDirection::Forward => (current + 1) % len,
		CycleDirection::Backward => (current + len - 1) % len,
	}
}

/// Stable catalog projection consumed by `omp.provider.models`.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderModelCard {
	/// Stable normalized catalog key used to select this deployment.
	pub id:                Str,
	/// Provider owning the first eligible route selected in catalog order.
	pub provider:          Str,
	/// Normalized model key passed through the provider protocol.
	pub model:             Str,
	/// Human-readable catalog display name.
	pub name:              Str,
	/// Normalized vendor lineage, or `None` when the catalog has no class.
	pub family:            Option<Str>,
	/// Supported operation names, such as `chat`, `speak`, or `image_gen`.
	pub facets:            Box<[Str]>,
	/// Supported chat input modalities; empty means the catalog has no
	/// constraints.
	pub inputs:            Box<[Str]>,
	/// Output modalities derived from the model's declared operations.
	pub outputs:           Box<[Str]>,
	/// Whether the catalog attaches a reasoning policy to the model.
	pub reasoning:         bool,
	/// Reasoning effort names in the attached policy; empty when reasoning is
	/// absent.
	pub efforts:           Box<[Str]>,
	/// Total input-plus-output context capacity in tokens, or `None` when
	/// unknown.
	pub context_window:    Option<u64>,
	/// Maximum generated output in tokens, or `None` when unknown.
	pub max_output_tokens: Option<u64>,
	/// Settled catalog prices, with one entry per billing dimension.
	pub pricing:           Box<[ProviderPrice]>,
	/// Snake-case catalog availability state controlling model selection.
	pub availability:      Str,
	/// Winning provenance class: `1` bundled, `2` discovered, or `3` configured.
	pub source:            u8,
	/// Temporary selection block expiry in Unix milliseconds, or `None`.
	pub blocked_until_ms:  Option<u64>,
	/// Whether the winning catalog evidence marks the deployment deprecated.
	pub deprecated:        bool,
	/// Latest known provider update time in Unix milliseconds, or `None`.
	pub updated_at_ms:     Option<u64>,
	/// `Some(false)` when tools are explicitly unsupported; `None` means
	/// unspecified.
	pub supports_tools:    Option<bool>,
	/// Reserved provider-specific properties; the built-in projection leaves
	/// this empty.
	pub props:             Map<String, Value>,
}

/// One settled catalog price component.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderPrice {
	/// Serialized billing dimension, such as tokens, requests, or media
	/// duration.
	pub unit:      Str,
	/// Charge for one billing unit in billionths of a US dollar.
	pub nanos_usd: u64,
}

/// Resumable catalog position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCatalogCursor {
	/// Opaque catalog-revision bytes that identify the snapshot lineage.
	pub epoch:      Box<[u8]>,
	/// Registry generation paired with the epoch to detect stale snapshots.
	pub generation: u64,
}

/// One ordered model-catalog delta.
#[derive(Clone, Debug)]
pub enum ProviderModelEvent {
	/// Inserts or replaces a model card at the supplied catalog position.
	Upsert {
		/// Catalog position after applying this replacement.
		cursor: ProviderCatalogCursor,
		/// Complete replacement card keyed by its stable `id`.
		card:   ProviderModelCard,
	},
	/// Removes a model key at the supplied catalog position.
	Remove {
		/// Catalog position after applying this removal.
		cursor: ProviderCatalogCursor,
		/// Stable normalized key of the model to remove.
		id:     Str,
	},
	/// Invalidates all prior cards so consumers rebuild from subsequent upserts.
	Reset {
		/// Catalog position from which the replacement snapshot begins.
		cursor: ProviderCatalogCursor,
	},
}

/// Complete non-secret frozen provider declaration.
#[derive(Clone, Debug)]
pub struct ProviderDeclarationDocument {
	/// Stable provider identity, which must match the document's `id`.
	pub provider: Str,
	/// Complete provider specification used to rebuild the runtime catalog.
	pub document: Value,
}

/// Closed provider request vocabulary admitted by Python.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRequestKind {
	/// Generates one or more images and stores final artifacts in the
	/// application blob owner.
	GenerateImage,
	/// Synthesizes text to encoded audio for a declared provider model.
	Speak,
	/// Transcribes an application-owned audio blob with a declared provider
	/// model.
	Transcribe,
	/// Mints a single-use, application-owned realtime transport lease.
	Realtime,
}

/// Exact provider request passed to the application inference facade.
#[derive(Clone, Debug)]
pub struct ProviderControlRequest {
	/// Provider to target; the caller must own its runtime declaration.
	pub provider:  Str,
	/// Media operation that determines payload validation and lowering defaults.
	pub operation: ProviderRequestKind,
	/// Operation-specific values; unsupported keys are rejected rather than
	/// merged.
	pub payload:   Map<String, Value>,
}

/// Blob reference whose bytes remain in the application blob owner.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderBlobRef {
	/// Lowercase hexadecimal content hash understood by the application blob
	/// store.
	pub hash: Str,
	/// Exact blob length in bytes, used with the hash to reopen the content.
	pub size: u64,
}

/// Typed provider request settlement.
#[derive(Clone, Debug)]
pub enum ProviderControlResult {
	/// Final image artifacts and their settled provider charge.
	Image {
		/// Non-empty final artifacts stored in the application blob owner.
		images:         Box<[ProviderBlobRef]>,
		/// Total settled charge in billionths of a US dollar.
		cost_nanos_usd: u64,
	},
	/// Encoded speech artifact, selected format, and settled provider charge.
	Speech {
		/// Complete encoded audio stored in the application blob owner.
		audio:          ProviderBlobRef,
		/// Requested audio format, defaulting to `mp3` when none was supplied.
		format:         Str,
		/// Total settled charge in billionths of a US dollar.
		cost_nanos_usd: u64,
	},
	/// Final transcript, optional detected language, and settled provider
	/// charge.
	Transcription {
		/// Final provider-settled transcript, excluding streaming deltas.
		text:           Str,
		/// Provider-detected language identifier, or `None` when not reported.
		language:       Option<Str>,
		/// Total settled charge in billionths of a US dollar.
		cost_nanos_usd: u64,
	},
	/// Single-use realtime session coordinates owned by the application.
	Realtime {
		/// Opaque lease identifier, currently identical to `endpoint`.
		id:            Str,
		/// Application endpoint identifier used to claim the transport session.
		endpoint:      Str,
		/// Single-use credential required alongside the endpoint identifier.
		credential:    Str,
		/// Lease expiry as Unix time in milliseconds, defaulting to 30 minutes
		/// after minting.
		expires_at_ms: u64,
		/// Transport protocol name; realtime leases currently use `websocket`.
		transport:     Str,
	},
}

/// Structured failure from the real provider owner.
#[derive(Clone, Debug, Error)]
pub enum ProviderControlError {
	/// The caller does not own the declaration or realtime lease it addressed.
	#[error("provider operation is not authorized")]
	Authorization,
	/// A new declaration collides with an existing built-in or runtime provider.
	#[error("provider declaration conflicts with an existing owner")]
	Conflict,
	/// The supplied declaration could not be parsed or compiled into the
	/// catalog.
	#[error("provider declaration is invalid: {0}")]
	InvalidDeclaration(Str),
	/// The owned provider declares neither management nor model support for the
	/// operation.
	#[error("provider capability is not declared")]
	CapabilityDenied,
	/// The requested provider, model, lease, or catalog resource does not exist.
	#[error("provider resource is not found")]
	NotFound,
	/// The provider requires an account but no authenticated principal is
	/// available.
	#[error("provider is not authenticated")]
	Unauthenticated,
	/// A gateway mutation expected an older catalog generation than the current
	/// one.
	#[error("provider catalog generation is stale")]
	StaleGeneration,
	/// Request validation, inference routing, transport, or settlement failed.
	#[error("{0}")]
	Request(Str),
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct ProviderOwnerStamp {
	extension:          Str,
	artifact_digest:    Str,
	host_generation:    u64,
	session_generation: u64,
}

impl ProviderOwnerStamp {
	fn from_identity(identity: &ControlConnectionIdentity) -> Self {
		Self {
			extension:          identity.extension.clone(),
			artifact_digest:    identity.artifact_digest.clone(),
			host_generation:    identity.host_generation,
			session_generation: identity.session_generation,
		}
	}
}

#[derive(Clone)]
struct OwnedProviderDeclaration {
	owner:   ProviderOwnerStamp,
	records: omp_catalog::RuntimeProviderRecords,
}

struct RealtimeLease {
	credential: Str,
	session:    omp_inference::RealtimeSession,
}

struct ProviderApplicationState {
	declarations: BTreeMap<Str, OwnedProviderDeclaration>,
	realtime:     BTreeMap<Str, RealtimeLease>,
}

/// Production owner for extension-declared providers and provider media.
///
/// The owner rebuilds complete immutable registries under a short writer lock
/// and publishes them through [`omp_inference::RegistryHandle`]. Requests load
/// one snapshot before planning, so a replacement never invalidates an
/// outstanding request. Route stacks are rebuilt from the same clone-cheap
/// production dependencies, preserving credential, account, admission, and
/// conversation-session owners.
pub struct ProductionProviderApplicationOwner {
	base_catalog: Arc<snapshot::Catalog>,
	registry:     omp_inference::RegistryHandle,
	builtins:     BuiltinConfig,
	blobs:        BlobStore,
	state:        Mutex<ProviderApplicationState>,
}

impl ProductionProviderApplicationOwner {
	/// Binds the current registry, its reusable production route composition,
	/// and the application blob owner.
	pub fn new(
		registry: omp_inference::Registry,
		builtins: BuiltinConfig,
		blobs: BlobStore,
	) -> Self {
		let base_catalog = Arc::new(registry.catalog().clone());
		Self {
			base_catalog,
			registry: registry.into_handle(),
			builtins,
			blobs,
			state: Mutex::new(ProviderApplicationState {
				declarations: BTreeMap::new(),
				realtime:     BTreeMap::new(),
			}),
		}
	}

	/// Declares a provider exactly once for the authenticated host generation.
	pub fn declare_provider(
		&self,
		identity: &ControlConnectionIdentity,
		declaration: ProviderDeclarationDocument,
	) -> Result<(), ProviderControlError> {
		let records = lower_provider_declaration(&self.base_catalog, &declaration)?;
		let mut state = self.state.lock();
		if state.declarations.contains_key(&declaration.provider)
			|| self
				.base_catalog
				.provider(&omp_catalog::ProviderId::from(declaration.provider.clone()))
				.is_some()
		{
			return Err(ProviderControlError::Conflict);
		}
		let mut next = state.declarations.clone();
		next.insert(declaration.provider.clone(), OwnedProviderDeclaration {
			owner: ProviderOwnerStamp::from_identity(identity),
			records,
		});
		let registry = self.rebuild(&next)?;
		self.registry.replace(registry);
		state.declarations = next;
		Ok(())
	}

	/// Takes an authenticated Core-owned realtime transport session.
	///
	/// Endpoint and credential identifiers are both required; neither value is
	/// a provider credential.
	pub fn take_realtime(
		&self,
		endpoint: &str,
		credential: &str,
	) -> Result<omp_inference::RealtimeSession, ProviderControlError> {
		let mut state = self.state.lock();
		let Some(lease) = state.realtime.remove(endpoint) else {
			return Err(ProviderControlError::NotFound);
		};
		if lease.credential != credential {
			state.realtime.insert(Str::from(endpoint), lease);
			return Err(ProviderControlError::Authorization);
		}
		Ok(lease.session)
	}

	fn registry_snapshot(&self) -> omp_inference::Registry {
		let snapshot = self.registry.load();
		ops::Deref::deref(&snapshot).clone()
	}

	fn rebuild(
		&self,
		declarations: &BTreeMap<Str, OwnedProviderDeclaration>,
	) -> Result<omp_inference::Registry, ProviderControlError> {
		let mut catalog = (*self.base_catalog).clone();
		for declaration in declarations.values() {
			catalog = catalog
				.with_runtime_provider(&declaration.records)
				.map_err(|error| invalid_declaration(error.to_string()))?;
		}
		let generation = self.registry.load().generation().saturating_add(1);
		omp_inference::Registry::builder(Arc::new(catalog))
			.with_builtins(self.builtins.clone())
			.map_err(provider_inference_error)?
			.with_generation(generation)
			.build()
			.map_err(provider_inference_error)
	}

	fn owned<'a>(
		state: &'a ProviderApplicationState,
		identity: &ControlConnectionIdentity,
		provider: &str,
	) -> Result<&'a OwnedProviderDeclaration, ProviderControlError> {
		let declaration = state
			.declarations
			.get(provider)
			.ok_or(ProviderControlError::NotFound)?;
		if declaration.owner != ProviderOwnerStamp::from_identity(identity) {
			return Err(ProviderControlError::Authorization);
		}
		Ok(declaration)
	}

	async fn dispatch(
		&self,
		identity: &ControlConnectionIdentity,
		request: ProviderControlRequest,
	) -> Result<ProviderControlResult, ProviderControlError> {
		let operation_kind = request_kind_operation(request.operation);
		let registry = {
			let state = self.state.lock();
			let declaration = Self::owned(&state, identity, request.provider.as_str())?;
			let declared = declaration
				.records
				.provider
				.management
				.supports(operation_kind)
				|| declaration
					.records
					.models
					.iter()
					.any(|model| model.capabilities.operations.contains_kind(operation_kind));
			if !declared {
				return Err(ProviderControlError::CapabilityDenied);
			}
			self.registry_snapshot()
		};
		let provider = omp_catalog::ProviderId::from(request.provider.clone());
		let speech_format = request
			.payload
			.get("format")
			.and_then(Value::as_str)
			.map(Str::from);
		let (target, operation) = lower_control_request(&registry, &self.blobs, &provider, request)?;
		let meta = omp_inference::CallMeta {
			id: omp_inference::RequestId::from(format!(
				"provider-control-{}",
				omp_core::Ulid::generate()
			)),
			target,
			deadline: None,
			budget: omp_inference::ExecutionBudget::default(),
			session: None,
		};
		let call = omp_inference::Call::new(meta, operation).with_attribution(
			omp_inference::InferenceAttribution {
				principal: omp_inference::PrincipalId::from(identity.extension.clone()),
				extension: identity.extension.clone(),
			},
		);
		let answer =
			omp_inference::router::execute_registry_call(registry, call, Duration::from_secs(30))
				.await
				.map_err(provider_inference_error)?;
		self
			.settle_answer(operation_kind, speech_format, answer)
			.await
	}

	async fn settle_answer(
		&self,
		operation: omp_catalog::OperationKind,
		speech_format: Option<Str>,
		answer: omp_inference::Answer,
	) -> Result<ProviderControlResult, ProviderControlError> {
		let mut cost_nanos_usd = receipt_cost_nanos(answer.receipt.cost)?;
		match answer.body {
			omp_inference::AnswerBody::Images(mut stream) => {
				let mut images = Vec::new();
				let completed_cost = None;
				while let Some(event) = stream.next().await {
					match event.map_err(provider_inference_error)? {
						omp_inference::GenerationEvent::Artifact(image) => {
							images.push(self.store_artifact(image.artifact).await?);
						},
						omp_inference::GenerationEvent::Completed(summary) => {
							cost_nanos_usd =
								cost_nanos_usd.saturating_add(receipt_cost_nanos(summary.cost)?);
						},
						omp_inference::GenerationEvent::Queued { .. }
						| omp_inference::GenerationEvent::Progress { .. }
						| omp_inference::GenerationEvent::Preview(_) => {},
					}
				}
				if images.is_empty() {
					return Err(ProviderControlError::Request(sf!(
						"image transport completed without a final artifact"
					)));
				}
				if let Some(settled) = completed_cost {
					cost_nanos_usd = settled;
				}
				Ok(ProviderControlResult::Image { images: images.into_boxed_slice(), cost_nanos_usd })
			},
			omp_inference::AnswerBody::Speech(mut stream) => {
				let mut bytes = BytesMut::new();
				while let Some(chunk) = stream.next().await {
					bytes.extend_from_slice(&chunk.map_err(provider_inference_error)?.bytes);
				}
				if bytes.is_empty() {
					return Err(invalid_request("speech transport completed without encoded audio"));
				}
				let stored = self
					.blobs
					.put(&bytes)
					.map_err(|error| ProviderControlError::Request(Str::from(error.to_string())))?;
				Ok(ProviderControlResult::Speech {
					audio: ProviderBlobRef {
						hash: Str::from(stored.to_hex().as_str()),
						size: stored.size,
					},
					format: speech_format.unwrap_or_else(|| answer_audio_format(operation)),
					cost_nanos_usd,
				})
			},
			omp_inference::AnswerBody::Transcript(mut stream) => {
				let mut language = None;
				let mut text = None;
				while let Some(event) = stream.next().await {
					match event.map_err(provider_inference_error)? {
						omp_inference::TranscriptEvent::Started { language: detected } => {
							language = detected;
						},
						omp_inference::TranscriptEvent::Completed { text: settled, .. } => {
							text = Some(settled);
						},
						omp_inference::TranscriptEvent::TextDelta { .. }
						| omp_inference::TranscriptEvent::Segment { .. }
						| omp_inference::TranscriptEvent::Word { .. } => {},
					}
				}
				Ok(ProviderControlResult::Transcription {
					text: text.ok_or_else(|| {
						ProviderControlError::Request(sf!(
							"transcription transport completed without final text"
						))
					})?,
					language,
					cost_nanos_usd,
				})
			},
			omp_inference::AnswerBody::Realtime(session) => {
				let id = Str::from(omp_core::Ulid::generate().to_string());
				let credential = Str::from(omp_core::Ulid::generate().to_string());
				self
					.state
					.lock()
					.realtime
					.insert(id.clone(), RealtimeLease { credential: credential.clone(), session });
				let expires_at_ms = SystemTime::now()
					.duration_since(SystemTime::UNIX_EPOCH)
					.unwrap_or_default()
					.as_millis()
					.saturating_add(30 * 60 * 1000)
					.try_into()
					.unwrap_or(u64::MAX);
				Ok(ProviderControlResult::Realtime {
					id: id.clone(),
					endpoint: id,
					credential,
					expires_at_ms,
					transport: sf!("websocket"),
				})
			},
			_ => Err(ProviderControlError::Request(sf!(
				"provider transport returned the wrong media answer"
			))),
		}
	}

	async fn store_artifact(
		&self,
		artifact: omp_inference::Artifact,
	) -> Result<ProviderBlobRef, ProviderControlError> {
		let bytes = match artifact.body {
			omp_inference::ArtifactBody::Bytes(bytes) => bytes,
			omp_inference::ArtifactBody::Stream(mut stream) => {
				let mut bytes = BytesMut::new();
				while let Some(chunk) = stream.next().await {
					bytes.extend_from_slice(&chunk.map_err(provider_inference_error)?);
				}
				bytes.freeze()
			},
			omp_inference::ArtifactBody::Stored(reference) => {
				let size = artifact.size.ok_or_else(|| {
					ProviderControlError::Request(sf!("stored media artifact has no size"))
				})?;
				let blob = BlobRef::parse_hex(reference.id.as_str(), size)
					.map_err(|error| ProviderControlError::Request(Str::from(error.to_string())))?;
				if !self.blobs.has(&blob) {
					return Err(ProviderControlError::Request(sf!(
						"stored media artifact is outside the application blob owner"
					)));
				}
				return Ok(ProviderBlobRef {
					hash: Str::from(blob.to_hex().as_str()),
					size: blob.size,
				});
			},
		};
		let stored = self
			.blobs
			.put(&bytes)
			.map_err(|error| ProviderControlError::Request(Str::from(error.to_string())))?;
		Ok(ProviderBlobRef { hash: Str::from(stored.to_hex().as_str()), size: stored.size })
	}
}

#[async_trait]
impl ProviderApplicationOwner for ProductionProviderApplicationOwner {
	fn registry(&self) -> omp_inference::Registry {
		self.registry_snapshot()
	}

	async fn replace_provider(
		&self,
		identity: &ControlConnectionIdentity,
		declaration: ProviderDeclarationDocument,
	) -> Result<(), ProviderControlError> {
		let records = lower_provider_declaration(&self.base_catalog, &declaration)?;
		let mut state = self.state.lock();
		if self
			.base_catalog
			.provider(&omp_catalog::ProviderId::from(declaration.provider.clone()))
			.is_some()
		{
			return Err(ProviderControlError::Authorization);
		}
		if let Some(current) = state.declarations.get(&declaration.provider)
			&& current.owner != ProviderOwnerStamp::from_identity(identity)
		{
			return Err(ProviderControlError::Authorization);
		}
		let mut next = state.declarations.clone();
		next.insert(declaration.provider.clone(), OwnedProviderDeclaration {
			owner: ProviderOwnerStamp::from_identity(identity),
			records,
		});
		let registry = self.rebuild(&next)?;
		self.registry.replace(registry);
		state.declarations = next;
		Ok(())
	}

	async fn retract_provider(
		&self,
		identity: &ControlConnectionIdentity,
		provider: &str,
	) -> Result<(), ProviderControlError> {
		let mut state = self.state.lock();
		Self::owned(&state, identity, provider)?;
		let mut next = state.declarations.clone();
		next.remove(provider);
		let registry = self.rebuild(&next)?;
		self.registry.replace(registry);
		state.declarations = next;
		Ok(())
	}

	async fn provider_request(
		&self,
		identity: &ControlConnectionIdentity,
		request: ProviderControlRequest,
	) -> Result<ProviderControlResult, ProviderControlError> {
		self.dispatch(identity, request).await
	}
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProviderSpec {
	id:                 String,
	name:               String,
	#[serde(default)]
	routes:             Vec<RawRouteSpec>,
	#[serde(default)]
	models:             Vec<RawModelSpec>,
	#[serde(default)]
	management:         RawManagementSpec,
	#[serde(default = "concrete_mapping")]
	mapping:            Value,
	#[serde(default)]
	aliases:            Vec<Value>,
	#[serde(default)]
	model_overlays:     Vec<Value>,
	#[serde(default)]
	discovery_defaults: Option<Value>,
}

fn concrete_mapping() -> Value {
	Value::String("concrete".to_owned())
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManagementSpec {
	#[serde(default)]
	operations:        BTreeSet<String>,
	#[serde(default)]
	multiple_accounts: bool,
	#[serde(default)]
	refresh:           bool,
	#[serde(default)]
	principal_quota:   bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRouteSpec {
	id:            String,
	base_url:      String,
	api:           String,
	#[serde(default = "default_http")]
	transport:     String,
	#[serde(default)]
	auth:          RawAuthSpec,
	#[serde(default)]
	headers:       BTreeMap<String, String>,
	#[serde(default)]
	region:        Option<String>,
	#[serde(default)]
	discovery:     Option<Value>,
	#[serde(default)]
	trust:         RawTrustDomain,
	#[serde(default)]
	limits:        RawRouteLimits,
	#[serde(default)]
	compat:        Value,
	#[serde(default = "default_codec_profile")]
	codec_profile: String,
	#[serde(default)]
	priority:      Option<u32>,
}

fn default_http() -> String {
	"http".to_owned()
}

fn default_codec_profile() -> String {
	"standard".to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuthSpec {
	#[serde(default = "default_auth_mode")]
	mode:          String,
	#[serde(default = "default_auth_header")]
	header:        Option<String>,
	#[serde(default = "default_auth_prefix")]
	prefix:        Option<String>,
	#[serde(default)]
	query:         Option<String>,
	#[serde(default)]
	scopes:        Vec<String>,
	#[serde(default)]
	audience:      Option<String>,
	#[serde(default = "default_account_scope")]
	account_scope: String,
	#[serde(default)]
	sources:       Vec<RawCredentialSource>,
	#[serde(default)]
	oauth:         Option<Value>,
	#[serde(default)]
	signing:       Option<Value>,
}

impl Default for RawAuthSpec {
	fn default() -> Self {
		Self {
			mode:          default_auth_mode(),
			header:        default_auth_header(),
			prefix:        default_auth_prefix(),
			query:         None,
			scopes:        Vec::new(),
			audience:      None,
			account_scope: default_account_scope(),
			sources:       vec![RawCredentialSource {
				kind:          "stored".to_owned(),
				ordered_names: Vec::new(),
				options:       Map::new(),
			}],
			oauth:         None,
			signing:       None,
		}
	}
}

fn default_auth_mode() -> String {
	"none".to_owned()
}

fn default_auth_header() -> Option<String> {
	Some("authorization".to_owned())
}

fn default_auth_prefix() -> Option<String> {
	Some("Bearer ".to_owned())
}

fn default_account_scope() -> String {
	"provider".to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCredentialSource {
	kind:          String,
	#[serde(default)]
	ordered_names: Vec<String>,
	#[serde(default)]
	options:       Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrustDomain {
	#[serde(default)]
	origin:          String,
	#[serde(default = "default_redirect_trust")]
	redirects:       String,
	#[serde(default)]
	allow_plaintext: bool,
}

impl Default for RawTrustDomain {
	fn default() -> Self {
		Self {
			origin:          String::new(),
			redirects:       default_redirect_trust(),
			allow_plaintext: false,
		}
	}
}

fn default_redirect_trust() -> String {
	"same_origin".to_owned()
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRouteLimits {
	#[serde(default)]
	operations:             Option<BTreeSet<String>>,
	#[serde(default)]
	max_context_tokens:     Option<u64>,
	#[serde(default)]
	max_output_tokens:      Option<u64>,
	#[serde(default)]
	disable_server_state:   bool,
	#[serde(default)]
	disable_prompt_caching: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
	dead_code,
	reason = "compat is honored by pi custom-models.ts but OMP lacks runtime model-specific \
	          WirePolicy records; remaining fields are accepted under deny_unknown_fields"
)]
struct RawModelSpec {
	id: String,
	display_name: String,
	routes: Vec<String>,
	#[serde(default)]
	wire_ids: BTreeMap<String, String>,
	#[serde(default = "default_model_operations")]
	operations: BTreeSet<String>,
	#[serde(default)]
	family: Option<String>,
	#[serde(default)]
	context_window: Option<u64>,
	#[serde(default)]
	max_input_tokens: Option<u64>,
	#[serde(default)]
	max_output_tokens: Option<u64>,
	#[serde(default)]
	max_batch: Option<u32>,
	#[serde(default)]
	input_modalities: BTreeSet<String>,
	#[serde(default)]
	thinking: Option<Value>,
	#[serde(default)]
	thinking_routing: Option<Value>,
	#[serde(default)]
	cost: RawCost,
	#[serde(default)]
	premium_multiplier: Option<Value>,
	#[serde(default)]
	compat: Value,
	#[serde(default)]
	context: Option<Value>,
	#[serde(default)]
	availability: Option<Value>,
	#[serde(default)]
	context_promotion_target: Option<String>,
	#[serde(default)]
	remote_compaction: Option<Value>,
	#[serde(default)]
	chat: Option<Value>,
	#[serde(default)]
	embeddings: Option<Value>,
	#[serde(default)]
	image: Option<RawImageCaps>,
	#[serde(default)]
	video: Option<Value>,
	#[serde(default)]
	speech: Option<RawSpeechCaps>,
	#[serde(default)]
	transcription: Option<RawTranscriptionCaps>,
	#[serde(default)]
	realtime: Option<RawRealtimeCaps>,
	#[serde(default)]
	search: Option<Value>,
	#[serde(default)]
	tokenization: Option<Value>,
}

fn default_model_operations() -> BTreeSet<String> {
	BTreeSet::from(["chat".to_owned()])
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCost {
	#[serde(default)]
	input:        Value,
	#[serde(default)]
	output:       Value,
	#[serde(default)]
	cache_read:   Value,
	#[serde(default)]
	cache_write:  Value,
	#[serde(default)]
	image:        Value,
	#[serde(default)]
	video_second: Value,
	#[serde(default)]
	audio_second: Value,
	#[serde(default)]
	char_input:   Value,
	#[serde(default)]
	request:      Value,
	#[serde(default)]
	tiers:        Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDimensions {
	width:  u32,
	height: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImageCaps {
	#[serde(default)]
	features:       BTreeSet<String>,
	#[serde(default)]
	sizes:          Vec<RawDimensions>,
	#[serde(default)]
	formats:        BTreeSet<String>,
	#[serde(default)]
	max_references: Option<u16>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpeechCaps {
	#[serde(default)]
	features:        BTreeSet<String>,
	#[serde(default)]
	voices:          Vec<String>,
	#[serde(default)]
	formats:         BTreeSet<String>,
	#[serde(default)]
	sample_rates_hz: Vec<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranscriptionCaps {
	#[serde(default)]
	features:     BTreeSet<String>,
	#[serde(default)]
	formats:      BTreeSet<String>,
	#[serde(default)]
	max_duration: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRealtimeCaps {
	#[serde(default)]
	features:   BTreeSet<String>,
	#[serde(default)]
	voices:     Vec<String>,
	#[serde(default)]
	transports: BTreeSet<String>,
}

/// Lowers one sealed Python `ProviderSpec` document into complete catalog
/// records. The compiler accepts only routes supported by the production
/// composer and never invents credential or trust policy.
pub fn lower_provider_declaration(
	base: &snapshot::Catalog,
	declaration: &ProviderDeclarationDocument,
) -> Result<omp_catalog::RuntimeProviderRecords, ProviderControlError> {
	let raw: RawProviderSpec = serde_json::from_value(declaration.document.clone())
		.map_err(|error| invalid_declaration(error.to_string()))?;
	if raw.id != declaration.provider || !valid_provider_id(&raw.id) {
		return Err(invalid_declaration("provider identity is invalid"));
	}
	if raw.name.trim().is_empty() || raw.routes.is_empty() {
		return Err(invalid_declaration("provider name and routes are required"));
	}
	if raw.mapping != Value::String("concrete".to_owned())
		|| !raw.aliases.is_empty()
		|| !raw.model_overlays.is_empty()
		|| raw.discovery_defaults.is_some()
	{
		return Err(invalid_declaration(
			"runtime providers cannot widen aliases, mappings, or bundled overlays",
		));
	}
	let provider_id = omp_catalog::ProviderId::from(raw.id.as_str());
	let empty_headers = base
		.header_profiles()
		.iter()
		.find(|profile| profile.headers.is_empty())
		.ok_or_else(|| invalid_declaration("catalog has no empty header profile"))?
		.id
		.clone();
	let mut routes = Vec::with_capacity(raw.routes.len());
	let mut auth_specs = Vec::with_capacity(raw.routes.len());
	let mut route_names = BTreeMap::new();
	let mut provider_auth = Vec::new();
	let mut provider_wire_policy = None;
	for raw_route in &raw.routes {
		if !valid_component(&raw_route.id) || route_names.contains_key(&raw_route.id) {
			return Err(invalid_declaration("route identity is invalid or duplicated"));
		}
		if raw_route.discovery.is_some() {
			return Err(invalid_declaration("runtime discovery requires a Core-owned projector"));
		}
		if raw_route
			.compat
			.as_object()
			.is_some_and(|compat| compat.values().any(|value| !value.is_null()))
		{
			return Err(invalid_declaration(
				"runtime compatibility flags must use a compiled Core route",
			));
		}
		let codec = api_codec(&raw_route.api)
			.ok_or_else(|| invalid_declaration("provider API is not implemented"))?;
		let transport = parse_transport(&raw_route.transport)?;
		let trust = compile_trust(raw_route, transport)?;
		let header_id = if raw_route.headers.is_empty() {
			empty_headers.clone()
		} else {
			validate_headers(&raw_route.headers)?;
			base
				.header_profiles()
				.iter()
				.find(|profile| {
					profile.headers.len() == raw_route.headers.len()
						&& profile.headers.iter().all(|header| {
							raw_route
								.headers
								.get(header.name.as_str())
								.is_some_and(|value| value == header.value.as_str())
						})
				})
				.map(|profile| profile.id.clone())
				.ok_or_else(|| {
					invalid_declaration("runtime static headers must match a sealed Core header profile")
				})?
		};
		let route_id = omp_catalog::RouteId::from(format!("{}/{}", raw.id, raw_route.id).as_str());
		let auth = compile_auth(&raw.id, &raw_route.id, &raw_route.auth)?;
		provider_auth.push(auth.id.clone());
		let template_route = base
			.routes()
			.iter()
			.find(|route| route.codec.as_str() == codec)
			.ok_or_else(|| invalid_declaration("provider codec has no production route template"))?;
		let template_provider = base
			.provider(&template_route.provider)
			.ok_or_else(|| invalid_declaration("provider codec template has no owner"))?;
		provider_wire_policy.get_or_insert_with(|| template_provider.wire_policy.clone());
		let mut limits = omp_catalog::RouteRestrictions {
			operations:             None,
			maximum_context_tokens: raw_route.limits.max_context_tokens,
			maximum_output_tokens:  raw_route.limits.max_output_tokens,
			disable_server_state:   raw_route.limits.disable_server_state,
			disable_prompt_caching: raw_route.limits.disable_prompt_caching,
			disable_strict_tools:   false,
		};
		if let Some(operations) = &raw_route.limits.operations {
			limits.operations = Some(compile_operations(operations, codec)?);
		}
		routes.push(omp_catalog::RouteDef {
			id: route_id.clone(),
			provider: provider_id.clone(),
			codec_profile: parse_codec_profile(&raw_route.codec_profile)?,
			codec: omp_catalog::CodecId::from(codec),
			transport,
			endpoint: EndpointSpec {
				base_url:    Str::from(raw_route.base_url.as_str()),
				region:      raw_route.region.as_deref().map(Str::from),
				api_version: None,
			},
			auth: auth.id.clone(),
			headers: header_id,
			discovery: None,
			capability_limits: limits,
			trust_domain: trust,
			codex_transport: CodexTransportPreference::HttpOnly,
			use_responses_lite: None,
			priority: raw_route.priority,
		});
		auth_specs.push(auth);
		route_names.insert(raw_route.id.clone(), route_id);
	}
	let wire_policy = provider_wire_policy
		.ok_or_else(|| invalid_declaration("provider has no wire policy template"))?;
	let mut models = Vec::with_capacity(raw.models.len());
	let mut model_ids = BTreeSet::new();
	for raw_model in &raw.models {
		if !valid_component(&raw_model.id) || !model_ids.insert(raw_model.id.clone()) {
			return Err(invalid_declaration("model identity is invalid or duplicated"));
		}
		if raw_model.thinking.is_some()
			|| raw_model.thinking_routing.is_some()
			|| raw_model.premium_multiplier.is_some()
			|| raw_model.context_promotion_target.is_some()
			|| raw_model.remote_compaction.is_some()
			|| raw_model.video.is_some()
		{
			return Err(invalid_declaration(
				"model requests an unsealed policy or unsupported media capability",
			));
		}
		let model_routes = raw_model
			.routes
			.iter()
			.map(|route| {
				route_names
					.get(route)
					.cloned()
					.ok_or_else(|| invalid_declaration("model references an unknown route"))
			})
			.collect::<Result<Vec<_>, _>>()?;
		if model_routes.is_empty() {
			return Err(invalid_declaration("model must name at least one route"));
		}
		let mut allowed = None;
		for route_id in &model_routes {
			let route = routes
				.iter()
				.find(|route| &route.id == route_id)
				.expect("compiled route exists");
			let supported = supported_operations(route.codec.as_str());
			allowed = Some(match allowed {
				None => supported,
				Some(current) => intersect_operations(current, supported),
			});
		}
		let operations = compile_operations(
			&raw_model.operations,
			routes
				.iter()
				.find(|route| route.id == model_routes[0])
				.expect("compiled route exists")
				.codec
				.as_str(),
		)?;
		if let Some(allowed) = allowed
			&& operation_kinds(&operations)
				.iter()
				.any(|operation| !allowed.contains(operation))
		{
			return Err(ProviderControlError::CapabilityDenied);
		}
		let capabilities = compile_model_capabilities(base, raw_model, operations)?;
		let key = omp_catalog::ModelKey::from(format!("{}/{}", raw.id, raw_model.id).as_str());
		let wire_ids = model_routes
			.iter()
			.map(|route| {
				let local = route
					.as_str()
					.strip_prefix(&format!("{}/", raw.id))
					.unwrap_or(route.as_str());
				(
					route.clone(),
					omp_catalog::WireModelId::from(
						raw_model
							.wire_ids
							.get(local)
							.map_or(raw_model.id.as_str(), String::as_str),
					),
				)
			})
			.collect::<Vec<_>>();
		models.push(omp_catalog::ModelSpec {
			key,
			class: omp_catalog::ClassId::from(
				raw_model.family.as_deref().unwrap_or(raw_model.id.as_str()),
			),
			display_name: Str::from(raw_model.display_name.as_str()),
			wire_ids: wire_ids.into_boxed_slice(),
			routes: model_routes.into_boxed_slice(),
			capabilities,
			limits: omp_catalog::ModelLimits {
				context_window:        raw_model.context_window,
				maximum_input_tokens:  raw_model.max_input_tokens,
				maximum_output_tokens: raw_model.max_output_tokens,
				maximum_batch:         raw_model.max_batch,
			},
			thinking: None,
			thinking_routing: omp_catalog::ThinkingRouting::default(),
			wire_policy: wire_policy.clone(),
			context: omp_catalog::ContextStrategy::Replay,
			pricing: compile_pricing(&raw_model.cost)?,
			availability: omp_catalog::ModelAvailability::Available,
			provenance: omp_catalog::ModelProvenance {
				sources:          Box::new([omp_catalog::ProvenanceSource {
					kind:           ProvenanceKind::Configured,
					origin:         declaration.provider.clone(),
					revision:       None,
					confidence:     omp_catalog::EvidenceConfidence::Declared,
					observed_at_ms: None,
				}]),
				updated_at_ms:    None,
				blocked_until_ms: None,
				deprecated:       false,
			},
			context_promotion_target: None,
			compaction_model: None,
			edit_revision: None,
			remote_compaction: None,
			premium_multiplier_millionths: None,
		});
	}
	let management_operations = compile_operations_for_management(&raw.management.operations)?;
	Ok(omp_catalog::RuntimeProviderRecords {
		provider:    omp_catalog::ProviderDef {
			id: provider_id,
			name: Str::from(raw.name),
			auth: provider_auth.into_boxed_slice(),
			management: omp_catalog::ManagementCapabilities {
				operations:        management_operations,
				multiple_accounts: raw.management.multiple_accounts,
				refresh:           raw.management.refresh,
				principal_quota:   raw.management.principal_quota,
			},
			routes: routes.iter().map(|route| route.id.clone()).collect(),
			wire_policy,
			discovery_defaults: None,
			mapping: omp_catalog::RegistryMapping::Concrete,
		},
		auth_specs:  auth_specs.into_boxed_slice(),
		oauth_specs: Box::new([]),
		routes:      routes.into_boxed_slice(),
		models:      models.into_boxed_slice(),
	})
}
fn invalid_declaration(message: impl Into<Str>) -> ProviderControlError {
	ProviderControlError::InvalidDeclaration(message.into())
}

fn provider_inference_error(error: omp_inference::Error) -> ProviderControlError {
	use omp_inference::ErrorKind;
	match error.kind {
		ErrorKind::Authentication => ProviderControlError::Unauthenticated,
		ErrorKind::TargetNotFound => ProviderControlError::NotFound,
		ErrorKind::CapabilityMismatch | ErrorKind::CapabilityUnknown => {
			ProviderControlError::CapabilityDenied
		},
		ErrorKind::StalePlan => ProviderControlError::StaleGeneration,
		_ => ProviderControlError::Request(Str::from(error.to_string())),
	}
}

fn valid_provider_id(value: &str) -> bool {
	valid_component(value)
}

fn valid_component(value: &str) -> bool {
	!value.is_empty()
		&& value.len() <= 128
		&& value
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn api_codec(api: &str) -> Option<&'static str> {
	match api {
		"openai_chat" | "openai_media" | "openai_realtime" => Some("openai-chat"),
		"openai_responses" => Some("openai-responses"),
		"openai_codex" => Some("openai-codex"),
		"anthropic_messages" => Some("anthropic"),
		"gemini" => Some("gemini"),
		"google_cca" => Some("google-cca"),
		"bedrock" => Some("bedrock-converse"),
		"ollama" => Some("ollama"),
		"gitlab_duo" => Some("gitlab"),
		"cursor" => Some("cursor"),
		"devin" => Some("devin"),
		"openai_embedding" => Some("openai-embedding"),
		"search_exa" => Some("search-exa"),
		"search_tavily" => Some("search-tavily"),
		"search_kagi" => Some("search-kagi"),
		"search_perplexity" => Some("search-perplexity"),
		"search_parallel" => Some("search-parallel"),
		_ => None,
	}
}

fn parse_transport(value: &str) -> Result<omp_catalog::TransportKind, ProviderControlError> {
	match value {
		"http" => Ok(omp_catalog::TransportKind::Http),
		"websocket" => Ok(omp_catalog::TransportKind::Websocket),
		"aws_event_stream" => Ok(omp_catalog::TransportKind::AwsEventStream),
		"connect" => Ok(omp_catalog::TransportKind::Connect),
		"webrtc" | "local" => {
			Err(invalid_declaration("runtime provider transport is not application-composed"))
		},
		_ => Err(invalid_declaration("provider transport is invalid")),
	}
}

fn parse_codec_profile(value: &str) -> Result<omp_catalog::CodecProfile, ProviderControlError> {
	match value {
		"standard" => Ok(omp_catalog::CodecProfile::Standard),
		"google-cca-gemini-cli" => Ok(omp_catalog::CodecProfile::GoogleCcaGeminiCli),
		"google-cca-antigravity" => Ok(omp_catalog::CodecProfile::GoogleCcaAntigravity),
		_ => Err(invalid_declaration("codec profile is not application-composed")),
	}
}

fn compile_trust(
	route: &RawRouteSpec,
	transport: omp_catalog::TransportKind,
) -> Result<omp_catalog::TrustDomain, ProviderControlError> {
	let url =
		Url::parse(&route.base_url).map_err(|_| invalid_declaration("route base URL is invalid"))?;
	let scheme = url.scheme();
	if transport != omp_catalog::TransportKind::Websocket && !matches!(scheme, "https" | "http") {
		return Err(invalid_declaration("HTTP route has a non-HTTP URL"));
	}
	if transport == omp_catalog::TransportKind::Websocket
		&& !matches!(scheme, "wss" | "ws" | "https" | "http")
	{
		return Err(invalid_declaration("WebSocket route has an invalid URL"));
	}
	let loopback = url.host_str().is_some_and(|host| {
		host.eq_ignore_ascii_case("localhost")
			|| host
				.parse::<net::IpAddr>()
				.is_ok_and(|address| address.is_loopback())
	});
	let plaintext = matches!(scheme, "http" | "ws");
	if plaintext && (!route.trust.allow_plaintext || !loopback) {
		return Err(invalid_declaration(
			"plaintext provider routes are limited to declared loopback trust",
		));
	}
	if route.trust.allow_plaintext && !loopback {
		return Err(invalid_declaration("plaintext trust cannot be widened beyond loopback"));
	}
	let origin = url.origin().ascii_serialization();
	if origin == "null" {
		return Err(invalid_declaration("route URL has no tuple origin"));
	}
	if !route.trust.origin.is_empty() && route.trust.origin != origin {
		return Err(invalid_declaration("route URL is outside its declared trust origin"));
	}
	let redirects = match route.trust.redirects.as_str() {
		"deny" => omp_catalog::RedirectTrust::Deny,
		"same_origin" => omp_catalog::RedirectTrust::SameOrigin,
		"public_only" => omp_catalog::RedirectTrust::PublicOnly,
		_ => return Err(invalid_declaration("redirect trust is invalid")),
	};
	Ok(omp_catalog::TrustDomain {
		origin: Str::from(origin),
		redirects,
		allow_plaintext: route.trust.allow_plaintext,
	})
}

fn validate_headers(headers: &BTreeMap<String, String>) -> Result<(), ProviderControlError> {
	for (name, value) in headers {
		let parsed = HeaderName::from_bytes(name.as_bytes())
			.map_err(|_| invalid_declaration("static provider header name is invalid"))?;
		HeaderValue::from_bytes(value.as_bytes())
			.map_err(|_| invalid_declaration("static provider header value is invalid"))?;
		if parsed == http::header::AUTHORIZATION
			|| parsed == http::header::COOKIE
			|| parsed == http::header::PROXY_AUTHORIZATION
			|| parsed == http::header::SET_COOKIE
		{
			return Err(invalid_declaration(
				"credential-bearing headers cannot be static provider data",
			));
		}
	}
	Ok(())
}

fn compile_auth(
	provider: &str,
	route: &str,
	raw: &RawAuthSpec,
) -> Result<omp_catalog::AuthSpec, ProviderControlError> {
	if raw.oauth.is_some() || raw.signing.is_some() {
		return Err(invalid_declaration(
			"runtime OAuth and signing flows require sealed application composition",
		));
	}
	let kind = match raw.mode.as_str() {
		"none" => AuthSpecKind::None,
		"api_key" => AuthSpecKind::ApiKey,
		"bearer" => AuthSpecKind::Bearer,
		"gcp_adc" => AuthSpecKind::GcpAdc,
		"omp_session" => AuthSpecKind::OmpSession,
		_ => return Err(invalid_declaration("authentication mode is not supported")),
	};
	if raw.header.is_some() && raw.query.is_some() {
		return Err(invalid_declaration(
			"authentication cannot occupy both a header and query parameter",
		));
	}
	if let Some(header) = &raw.header {
		HeaderName::from_bytes(header.as_bytes())
			.map_err(|_| invalid_declaration("authentication header is invalid"))?;
	}
	let account_scope = match raw.account_scope.as_str() {
		"provider" => AccountScope::Provider,
		"route" => AccountScope::Route,
		"region" => AccountScope::Region,
		_ => return Err(invalid_declaration("authentication account scope is invalid")),
	};
	let sources = raw
		.sources
		.iter()
		.map(|source| match source.kind.as_str() {
			"environment" if !source.ordered_names.is_empty() => {
				Ok(CredentialSourceSpec::Environment {
					ordered_names: source
						.ordered_names
						.iter()
						.map(|name| Str::from(name.as_str()))
						.collect(),
				})
			},
			"stored" => Ok(CredentialSourceSpec::Stored),
			"session" => Ok(CredentialSourceSpec::Session),
			"aws_chain" => Ok(CredentialSourceSpec::AwsChain),
			_ => {
				Err(invalid_declaration("credential source is malformed or not application-composed"))
			},
		})
		.collect::<Result<Vec<_>, _>>()?;
	if kind == AuthSpecKind::None && !sources.is_empty() {
		return Err(invalid_declaration("unauthenticated routes cannot declare credential sources"));
	}
	if kind != AuthSpecKind::None && sources.is_empty() {
		return Err(invalid_declaration("authenticated routes require a credential source"));
	}
	if raw.sources.iter().any(|source| !source.options.is_empty()) {
		return Err(invalid_declaration(
			"credential source options are not sealed by the application",
		));
	}
	Ok(omp_catalog::AuthSpec {
		id: omp_catalog::AuthSpecId::from(format!("runtime-{provider}-{route}-auth").as_str()),
		kind,
		header_name: raw.header.as_deref().map(Str::from),
		query_parameter: raw.query.as_deref().map(Str::from),
		prefix: raw.prefix.as_deref().map(Str::from),
		sealed_body: None,
		scopes: raw
			.scopes
			.iter()
			.map(|scope| Str::from(scope.as_str()))
			.collect(),
		audience: raw.audience.as_deref().map(Str::from),
		account_scope,
		credential_sources: sources.into_boxed_slice(),
		oauth: None,
		signing: None,
	})
}

fn operation_kind(value: &str) -> Option<omp_catalog::OperationKind> {
	match value {
		"chat" => Some(omp_catalog::OperationKind::Chat),
		"count_tokens" => Some(omp_catalog::OperationKind::CountTokens),
		"tokenize" => Some(omp_catalog::OperationKind::Tokenize),
		"detokenize" => Some(omp_catalog::OperationKind::Detokenize),
		"embed" => Some(omp_catalog::OperationKind::Embed),
		"generate_image" => Some(omp_catalog::OperationKind::GenerateImage),
		"generate_video" => Some(omp_catalog::OperationKind::GenerateVideo),
		"speak" => Some(omp_catalog::OperationKind::Speak),
		"transcribe" => Some(omp_catalog::OperationKind::Transcribe),
		"realtime" => Some(omp_catalog::OperationKind::Realtime),
		"search" => Some(omp_catalog::OperationKind::Search),
		"usage" => Some(omp_catalog::OperationKind::Usage),
		"discover_models" => Some(omp_catalog::OperationKind::DiscoverModels),
		"auth" => Some(omp_catalog::OperationKind::Auth),
		"native" => Some(omp_catalog::OperationKind::Native),
		_ => None,
	}
}

fn supported_operations(codec: &str) -> Vec<omp_catalog::OperationKind> {
	use omp_catalog::OperationKind::*;
	match codec {
		"openai-chat" => vec![Chat, Embed, GenerateImage, Speak, Transcribe, Realtime],
		"openai-responses" | "anthropic" | "gemini" => vec![Chat, CountTokens],
		"openai-codex" | "bedrock-converse" | "google-cca" | "ollama" | "gitlab" | "cursor"
		| "devin" => vec![Chat, DiscoverModels],
		"openai-embedding" => vec![Embed],
		codec if codec.starts_with("search-") => vec![Search],
		_ => Vec::new(),
	}
}

fn intersect_operations(
	left: Vec<omp_catalog::OperationKind>,
	right: Vec<omp_catalog::OperationKind>,
) -> Vec<omp_catalog::OperationKind> {
	left
		.into_iter()
		.filter(|kind| right.contains(kind))
		.collect()
}

fn compile_operations(
	values: &BTreeSet<String>,
	codec: &str,
) -> Result<omp_catalog::OperationBits, ProviderControlError> {
	let supported = supported_operations(codec);
	let mut output = omp_catalog::OperationBits::empty();
	for value in values {
		let operation =
			operation_kind(value).ok_or_else(|| invalid_declaration("operation is invalid"))?;
		if !supported.contains(&operation) {
			return Err(ProviderControlError::CapabilityDenied);
		}
		output.insert_kind(operation);
	}
	Ok(output)
}

fn compile_operations_for_management(
	values: &BTreeSet<String>,
) -> Result<omp_catalog::OperationBits, ProviderControlError> {
	let mut output = omp_catalog::OperationBits::empty();
	for value in values {
		output.insert_kind(
			operation_kind(value).ok_or_else(|| invalid_declaration("operation is invalid"))?,
		);
	}
	Ok(output)
}

fn operation_kinds(bits: &omp_catalog::OperationBits) -> Vec<omp_catalog::OperationKind> {
	use omp_catalog::OperationKind::*;
	[
		Chat,
		CountTokens,
		Tokenize,
		Detokenize,
		Embed,
		GenerateImage,
		GenerateVideo,
		Speak,
		Transcribe,
		Realtime,
		Search,
		Usage,
		DiscoverModels,
		Auth,
		Native,
	]
	.into_iter()
	.filter(|kind| bits.contains_kind(*kind))
	.collect()
}
fn compile_model_capabilities(
	base: &snapshot::Catalog,
	raw: &RawModelSpec,
	operations: omp_catalog::OperationBits,
) -> Result<omp_catalog::ModelCapabilities, ProviderControlError> {
	use omp_catalog::capability::{
		ImageCapabilities, ImageFeatureBits, RealtimeCapabilities, RealtimeFeatureBits,
		SpeechCapabilities, SpeechFeatureBits, TranscriptionCapabilities, TranscriptionFeatureBits,
	};
	let seed = base
		.models()
		.first()
		.ok_or_else(|| invalid_declaration("catalog has no model capability template"))?;
	let mut capabilities = seed.capabilities.clone();
	capabilities.operations = operations;
	capabilities.chat = None;
	capabilities.embeddings = None;
	capabilities.image = None;
	capabilities.video = None;
	capabilities.speech = None;
	capabilities.transcription = None;
	capabilities.realtime = None;
	capabilities.search = None;
	capabilities.tokenization = None;
	if operations.contains_kind(omp_catalog::OperationKind::Chat) {
		capabilities.chat = base
			.models()
			.iter()
			.find_map(|model| model.capabilities.chat.clone())
			.ok_or_else(|| invalid_declaration("catalog has no chat capability template"))?
			.into();
	}
	if operations.contains_kind(omp_catalog::OperationKind::Embed) {
		capabilities.embeddings = base
			.models()
			.iter()
			.find_map(|model| model.capabilities.embeddings.clone())
			.ok_or_else(|| invalid_declaration("catalog has no embedding capability template"))?
			.into();
	}
	if operations.contains_kind(omp_catalog::OperationKind::Search) {
		capabilities.search = base
			.models()
			.iter()
			.find_map(|model| model.capabilities.search)
			.or_else(|| {
				Some(omp_catalog::SearchCapabilities {
					features:        SearchFeatureBits::empty(),
					maximum_results: None,
				})
			});
	}
	if operations.contains_kind(omp_catalog::OperationKind::CountTokens)
		|| operations.contains_kind(omp_catalog::OperationKind::Tokenize)
		|| operations.contains_kind(omp_catalog::OperationKind::Detokenize)
	{
		capabilities.tokenization = base
			.models()
			.iter()
			.find_map(|model| model.capabilities.tokenization)
			.ok_or_else(|| invalid_declaration("catalog has no tokenization capability template"))?
			.into();
	}
	if operations.contains_kind(omp_catalog::OperationKind::GenerateImage) {
		let image = raw
			.image
			.as_ref()
			.ok_or_else(|| invalid_declaration("image operation requires image capabilities"))?;
		let mut features = ImageFeatureBits::empty();
		for feature in &image.features {
			features.insert(match feature.as_str() {
				"generate" => ImageFeatureBits::GENERATE,
				"edit" | "reference_images" => ImageFeatureBits::EDIT,
				"mask" => ImageFeatureBits::MASK,
				"transparency" => continue,
				_ => return Err(invalid_declaration("image capability feature is invalid")),
			});
		}
		if image.max_references.unwrap_or(0) > 0 {
			features.insert(ImageFeatureBits::EDIT);
		}
		for format in &image.formats {
			if !matches!(format.as_str(), "png" | "jpeg" | "webp") {
				return Err(invalid_declaration("image format is invalid"));
			}
		}
		let maximum_pixels = image
			.sizes
			.iter()
			.map(|size| u64::from(size.width).saturating_mul(u64::from(size.height)))
			.max();
		capabilities.image = Some(ImageCapabilities {
			features,
			input_modalities: modality_bits(&raw.input_modalities)?,
			maximum_outputs: raw.max_batch.and_then(|value| value.try_into().ok()),
			maximum_pixels,
		});
	}
	if operations.contains_kind(omp_catalog::OperationKind::Speak) {
		let raw = raw
			.speech
			.as_ref()
			.ok_or_else(|| invalid_declaration("speech operation requires speech capabilities"))?;
		let mut features = SpeechFeatureBits::empty();
		for feature in &raw.features {
			features.insert(match feature.as_str() {
				"streaming" => SpeechFeatureBits::STREAMING,
				"speed" => SpeechFeatureBits::SPEED,
				"voice_selection" => SpeechFeatureBits::VOICE_SELECTION,
				"timestamps" => continue,
				_ => return Err(invalid_declaration("speech capability feature is invalid")),
			});
		}
		if !raw.voices.is_empty() {
			features.insert(SpeechFeatureBits::VOICE_SELECTION);
		}
		for rate in &raw.sample_rates_hz {
			if *rate == 0 {
				return Err(invalid_declaration("speech sample rate must be positive"));
			}
		}
		capabilities.speech = Some(SpeechCapabilities {
			features,
			maximum_input_characters: None,
			output_formats: audio_format_bits(&raw.formats)?,
		});
	}
	if operations.contains_kind(omp_catalog::OperationKind::Transcribe) {
		let raw = raw.transcription.as_ref().ok_or_else(|| {
			invalid_declaration("transcription operation requires transcription capabilities")
		})?;
		if raw.max_duration.is_some() {
			return Err(invalid_declaration(
				"runtime transcription duration requires a compiled duration policy",
			));
		}
		let mut features = TranscriptionFeatureBits::empty();
		for feature in &raw.features {
			features.insert(match feature.as_str() {
				"streaming" => TranscriptionFeatureBits::STREAMING,
				"diarization" => TranscriptionFeatureBits::DIARIZATION,
				"translation" => TranscriptionFeatureBits::TRANSLATION,
				"timestamps" => TranscriptionFeatureBits::WORD_TIMESTAMPS,
				"language_hint" => TranscriptionFeatureBits::LANGUAGE_DETECTION,
				_ => {
					return Err(invalid_declaration("transcription capability feature is invalid"));
				},
			});
		}
		capabilities.transcription = Some(TranscriptionCapabilities {
			features,
			input_formats: audio_format_bits(&raw.formats)?,
			maximum_duration_ms: None,
		});
	}
	if operations.contains_kind(omp_catalog::OperationKind::Realtime) {
		let raw = raw
			.realtime
			.as_ref()
			.ok_or_else(|| invalid_declaration("realtime operation requires realtime capabilities"))?;
		let mut features = RealtimeFeatureBits::empty();
		for feature in &raw.features {
			features.insert(match feature.as_str() {
				"audio_in" | "audio_out" => RealtimeFeatureBits::AUDIO,
				"text" => RealtimeFeatureBits::TEXT,
				"tools" => RealtimeFeatureBits::TOOLS,
				"server_vad" => RealtimeFeatureBits::SERVER_VAD,
				"semantic_vad" | "interruption" => continue,
				_ => return Err(invalid_declaration("realtime capability feature is invalid")),
			});
		}
		for transport in &raw.transports {
			features.insert(match transport.as_str() {
				"websocket" => RealtimeFeatureBits::WEBSOCKET,
				"webrtc" => {
					return Err(invalid_declaration(
						"WebRTC realtime transport is not application-composed",
					));
				},
				_ => return Err(invalid_declaration("realtime transport is invalid")),
			});
		}
		if raw.voices.iter().any(|voice| voice.is_empty()) {
			return Err(invalid_declaration("realtime voice is empty"));
		}
		capabilities.realtime = Some(RealtimeCapabilities {
			features,
			maximum_session_ms: None,
			audio_formats: AudioFormatBits::empty(),
		});
	}
	Ok(capabilities)
}

fn modality_bits(
	values: &BTreeSet<String>,
) -> Result<capability::ModalityBits, ProviderControlError> {
	let mut bits = capability::ModalityBits::empty();
	for value in values {
		bits.insert(match value.as_str() {
			"text" => capability::ModalityBits::TEXT,
			"image" => capability::ModalityBits::IMAGE,
			"audio" => capability::ModalityBits::AUDIO,
			"video" => capability::ModalityBits::VIDEO,
			"document" => capability::ModalityBits::DOCUMENT,
			_ => return Err(invalid_declaration("model input modality is invalid")),
		});
	}
	Ok(bits)
}

fn audio_format_bits(values: &BTreeSet<String>) -> Result<AudioFormatBits, ProviderControlError> {
	let mut bits = AudioFormatBits::empty();
	for value in values {
		bits.insert(match value.as_str() {
			"pcm16" | "pcm24" | "f32" => AudioFormatBits::PCM,
			"mp3" => AudioFormatBits::MP3,
			"aac" => AudioFormatBits::AAC,
			"opus" => AudioFormatBits::OPUS,
			"flac" => AudioFormatBits::FLAC,
			"wav" => AudioFormatBits::WAV,
			_ => return Err(invalid_declaration("audio format is invalid")),
		});
	}
	Ok(bits)
}

fn compile_pricing(raw: &RawCost) -> Result<omp_catalog::Pricing, ProviderControlError> {
	use omp_catalog::{Price, PriceUnit};
	if !raw.tiers.is_empty() {
		return Err(invalid_declaration("runtime price tiers require a compiled catalog policy"));
	}
	let values = [
		(PriceUnit::MtokInput, &raw.input),
		(PriceUnit::MtokOutput, &raw.output),
		(PriceUnit::MtokCacheRead, &raw.cache_read),
		(PriceUnit::MtokCacheWrite, &raw.cache_write),
		(PriceUnit::Image, &raw.image),
		(PriceUnit::VideoSecond, &raw.video_second),
		(PriceUnit::AudioSecond, &raw.audio_second),
		(PriceUnit::McharInput, &raw.char_input),
		(PriceUnit::Request, &raw.request),
	];
	let mut components = Vec::new();
	for (unit, value) in values {
		let nanos = decimal_nanos(value)?;
		if nanos > 0 {
			components.push(Price { unit, nanos_usd: nanos });
		}
	}
	omp_catalog::Pricing::new(components, Vec::new())
		.map_err(|error| invalid_declaration(error.to_string()))
}

fn decimal_nanos(value: &Value) -> Result<u64, ProviderControlError> {
	let text = match value {
		Value::Null => return Ok(0),
		Value::Number(number) => number.to_string(),
		Value::String(text) => text.clone(),
		_ => return Err(invalid_declaration("price must be a non-negative decimal")),
	};
	if text.starts_with('-') || text.contains(['e', 'E']) {
		return Err(invalid_declaration("price must be a plain non-negative decimal"));
	}
	let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
	if fraction.len() > 9
		|| !whole.bytes().all(|byte| byte.is_ascii_digit())
		|| !fraction.bytes().all(|byte| byte.is_ascii_digit())
	{
		return Err(invalid_declaration("price exceeds nano-USD precision"));
	}
	let whole = whole
		.parse::<u64>()
		.map_err(|_| invalid_declaration("price is too large"))?;
	let fraction = if fraction.is_empty() {
		0
	} else {
		fraction
			.parse::<u64>()
			.map_err(|_| invalid_declaration("price is invalid"))?
			.saturating_mul(10_u64.pow(u32::try_from(9 - fraction.len()).unwrap_or(0)))
	};
	whole
		.checked_mul(1_000_000_000)
		.and_then(|whole| whole.checked_add(fraction))
		.ok_or_else(|| invalid_declaration("price is too large"))
}
fn request_kind_operation(kind: ProviderRequestKind) -> omp_catalog::OperationKind {
	match kind {
		ProviderRequestKind::GenerateImage => omp_catalog::OperationKind::GenerateImage,
		ProviderRequestKind::Speak => omp_catalog::OperationKind::Speak,
		ProviderRequestKind::Transcribe => omp_catalog::OperationKind::Transcribe,
		ProviderRequestKind::Realtime => omp_catalog::OperationKind::Realtime,
	}
}

fn lower_control_request(
	registry: &omp_inference::Registry,
	blobs: &BlobStore,
	provider: &omp_catalog::ProviderId,
	request: ProviderControlRequest,
) -> Result<(omp_inference::Target, omp_inference::OperationCall), ProviderControlError> {
	use omp_inference::call::{
		Background, Dimensions, ImageQuality, ImageRequest, NegotiationPolicy, RealtimeModality,
		RealtimeRequest, SpeechRequest, TimestampGranularity, TranscriptionRequest,
	};
	match request.operation {
		ProviderRequestKind::GenerateImage => {
			require_payload_keys(&request.payload, &["prompt", "dimensions", "format", "count"])?;
			let prompt = required_string(&request.payload, "prompt")?;
			let dimensions = request
				.payload
				.get("dimensions")
				.and_then(Value::as_object)
				.ok_or_else(|| invalid_request("image dimensions are required"))?;
			let width = required_u32(dimensions, "width")?;
			let height = required_u32(dimensions, "height")?;
			let count = request
				.payload
				.get("count")
				.and_then(Value::as_u64)
				.unwrap_or(1)
				.try_into()
				.map_err(|_| invalid_request("image count is too large"))?;
			if count == 0 {
				return Err(invalid_request("image count must be positive"));
			}
			let format = parse_image_format(required_string(&request.payload, "format")?)?;
			Ok((
				omp_inference::Target::ProviderService(provider.clone()),
				omp_inference::OperationCall::GenerateImage(Arc::new(ImageRequest {
					prompt: Str::from(prompt),
					references: Arc::from([]),
					mask: None,
					count,
					dimensions: call::Setting::Require(Dimensions { width, height }),
					quality: call::Setting::<ImageQuality>::Unset,
					background: call::Setting::<Background>::Unset,
					format: call::Setting::Require(format),
					style: call::Setting::Unset,
					safety: Arc::from([]),
					seed: None,
					negotiation: NegotiationPolicy::default(),
				})),
			))
		},
		ProviderRequestKind::Speak => {
			require_payload_keys(&request.payload, &["model", "text", "voice", "format"])?;
			let model =
				provider_model_key(registry, provider, required_string(&request.payload, "model")?)?;
			let format = request
				.payload
				.get("format")
				.and_then(Value::as_str)
				.map(parse_audio_format)
				.transpose()?
				.map_or(call::Setting::Unset, call::Setting::Require);
			Ok((
				omp_inference::Target::Provider { provider: provider.clone(), model },
				omp_inference::OperationCall::Speak(Arc::new(SpeechRequest {
					text: Str::from(required_string(&request.payload, "text")?),
					voice: Str::from(required_string(&request.payload, "voice")?),
					format,
					sample_rate_hz: call::Setting::Unset,
					speed: call::Setting::Unset,
					timestamps: call::Setting::<TimestampGranularity>::Unset,
					negotiation: NegotiationPolicy::default(),
				})),
			))
		},
		ProviderRequestKind::Transcribe => {
			require_payload_keys(&request.payload, &["model", "audio", "language"])?;
			let model =
				provider_model_key(registry, provider, required_string(&request.payload, "model")?)?;
			let reference = request
				.payload
				.get("audio")
				.and_then(Value::as_object)
				.ok_or_else(|| {
					invalid_request("transcription audio must be an application-owned blob reference")
				})?;
			let hash = required_string(reference, "hash")?;
			let size = reference
				.get("size")
				.and_then(Value::as_u64)
				.ok_or_else(|| invalid_request("transcription blob size is required"))?;
			let reference =
				BlobRef::parse_hex(hash, size).map_err(|error| invalid_request(error.to_string()))?;
			let bytes = blobs
				.get(&reference)
				.map_err(|error| invalid_request(error.to_string()))?;
			Ok((
				omp_inference::Target::Provider { provider: provider.clone(), model },
				omp_inference::OperationCall::Transcribe(Arc::new(TranscriptionRequest {
					audio:                omp_inference::MediaInput::Bytes {
						media_type: sf!("application/octet-stream"),
						data:       bytes,
					},
					language:             request
						.payload
						.get("language")
						.and_then(Value::as_str)
						.map(Str::from),
					translate_to_english: false,
					diarization:          call::Setting::Unset,
					timestamps:           call::Setting::Unset,
					prompt:               None,
					negotiation:          NegotiationPolicy::default(),
				})),
			))
		},
		ProviderRequestKind::Realtime => {
			require_payload_keys(&request.payload, &[
				"instructions",
				"modalities",
				"voice",
				"input_audio",
				"output_audio",
				"turn_detection",
				"tools",
				"negotiation",
			])?;
			let modalities = request
				.payload
				.get("modalities")
				.and_then(Value::as_array)
				.map_or_else(
					|| Ok(Vec::new()),
					|values| {
						values
							.iter()
							.map(|value| match value.as_str() {
								Some("text") => Ok(RealtimeModality::Text),
								Some("audio") => Ok(RealtimeModality::Audio),
								_ => Err(invalid_request("realtime modality is invalid")),
							})
							.collect()
					},
				)?;
			if request
				.payload
				.get("tools")
				.and_then(Value::as_array)
				.is_some_and(|tools| !tools.is_empty())
			{
				return Err(invalid_request("realtime tool names require Core-owned tool definitions"));
			}
			let input_audio = parse_audio_setting(request.payload.get("input_audio"))?;
			let output_audio = parse_audio_setting(request.payload.get("output_audio"))?;
			if request
				.payload
				.get("turn_detection")
				.and_then(Value::as_object)
				.is_some_and(|setting| {
					setting
						.get("kind")
						.and_then(Value::as_str)
						.unwrap_or("unset")
						!= "unset"
				}) {
				return Err(invalid_request(
					"realtime turn detection requires a sealed transport policy",
				));
			}
			Ok((
				omp_inference::Target::ProviderService(provider.clone()),
				omp_inference::OperationCall::Realtime(Arc::new(RealtimeRequest {
					instructions: request
						.payload
						.get("instructions")
						.and_then(Value::as_str)
						.map(Str::from),
					modalities: modalities.into(),
					voice: request
						.payload
						.get("voice")
						.and_then(Value::as_str)
						.map(Str::from),
					input_audio,
					output_audio,
					turn_detection: call::Setting::Unset,
					tools: Arc::from([]),
					negotiation: NegotiationPolicy::default(),
				})),
			))
		},
	}
}

fn provider_model_key(
	registry: &omp_inference::Registry,
	provider: &omp_catalog::ProviderId,
	value: &str,
) -> Result<omp_catalog::ModelKey, ProviderControlError> {
	let key = if value.contains('/') {
		omp_catalog::ModelKey::from(value)
	} else {
		omp_catalog::ModelKey::from(format!("{provider}/{value}").as_str())
	};
	registry
		.catalog()
		.model_for_provider(provider, &key)
		.ok_or(ProviderControlError::NotFound)?;
	Ok(key)
}

fn require_payload_keys(
	payload: &Map<String, Value>,
	allowed: &[&str],
) -> Result<(), ProviderControlError> {
	if payload.keys().any(|key| !allowed.contains(&key.as_str())) {
		return Err(invalid_request("provider request contains an unsupported field"));
	}
	Ok(())
}

fn required_string<'a>(
	payload: &'a Map<String, Value>,
	key: &str,
) -> Result<&'a str, ProviderControlError> {
	payload
		.get(key)
		.and_then(Value::as_str)
		.filter(|value| !value.is_empty())
		.ok_or_else(|| invalid_request(format!("{key} is required")))
}

fn required_u32(payload: &Map<String, Value>, key: &str) -> Result<u32, ProviderControlError> {
	payload
		.get(key)
		.and_then(Value::as_u64)
		.and_then(|value| value.try_into().ok())
		.filter(|value| *value > 0)
		.ok_or_else(|| invalid_request(format!("{key} must be a positive integer")))
}

fn invalid_request(message: impl Into<Str>) -> ProviderControlError {
	ProviderControlError::Request(message.into())
}

fn parse_image_format(value: &str) -> Result<call::ImageFormat, ProviderControlError> {
	match value {
		"png" => Ok(call::ImageFormat::Png),
		"jpeg" => Ok(call::ImageFormat::Jpeg),
		"webp" => Ok(call::ImageFormat::Webp),
		_ => Err(invalid_request("image format is invalid")),
	}
}

fn parse_audio_format(value: &str) -> Result<call::AudioFormat, ProviderControlError> {
	match value {
		"pcm16" => Ok(call::AudioFormat::Pcm16),
		"pcm24" => Ok(call::AudioFormat::Pcm24),
		"f32" => Ok(call::AudioFormat::F32),
		"mp3" => Ok(call::AudioFormat::Mp3),
		"aac" => Ok(call::AudioFormat::Aac),
		"opus" => Ok(call::AudioFormat::Opus),
		"flac" => Ok(call::AudioFormat::Flac),
		"wav" => Ok(call::AudioFormat::Wav),
		_ => Err(invalid_request("audio format is invalid")),
	}
}

fn parse_audio_setting(
	value: Option<&Value>,
) -> Result<call::Setting<call::AudioFormat>, ProviderControlError> {
	let Some(setting) = value.and_then(Value::as_object) else {
		return Ok(call::Setting::Unset);
	};
	match setting
		.get("kind")
		.and_then(Value::as_str)
		.unwrap_or("unset")
	{
		"unset" => Ok(call::Setting::Unset),
		"require" => Ok(call::Setting::Require(parse_audio_format(
			setting
				.get("value")
				.and_then(Value::as_str)
				.ok_or_else(|| invalid_request("required audio setting has no value"))?,
		)?)),
		"prefer" => Ok(call::Setting::Prefer(parse_audio_format(
			setting
				.get("value")
				.and_then(Value::as_str)
				.ok_or_else(|| invalid_request("preferred audio setting has no value"))?,
		)?)),
		_ => Err(invalid_request("audio setting kind is invalid")),
	}
}

fn receipt_cost_nanos(cost: omp_inference::Cost) -> Result<u64, ProviderControlError> {
	if cost.micro_usd < 0 {
		return Err(invalid_request("provider returned a negative media cost"));
	}
	u64::try_from(cost.micro_usd)
		.ok()
		.and_then(|value| value.checked_mul(1_000))
		.ok_or_else(|| invalid_request("provider media cost overflowed nano-USD"))
}

fn answer_audio_format(_operation: omp_catalog::OperationKind) -> Str {
	sf!("mp3")
}

/// Application-owned provider catalog, authentication, and inference seam.
#[async_trait]
pub trait ProviderControlBackend: Send + Sync + 'static {
	/// Returns the current model cards, optionally restricted to one provider.
	async fn models(
		&self,
		provider: Option<&str>,
	) -> Result<Vec<ProviderModelCard>, ProviderControlError>;
	/// Returns ordered changes since a cursor, or a reset snapshot when it
	/// differs.
	async fn watch_models(
		&self,
		since: Option<ProviderCatalogCursor>,
	) -> Result<Vec<ProviderModelEvent>, ProviderControlError>;
	/// Reports whether a provider needs no credentials or has at least one
	/// account.
	async fn is_authenticated(&self, provider: &str) -> Result<bool, ProviderControlError>;
	/// Atomically replaces the caller-owned runtime declaration and advances the
	/// catalog.
	async fn replace(
		&self,
		identity: &ControlConnectionIdentity,
		declaration: ProviderDeclarationDocument,
	) -> Result<(), ProviderControlError>;
	/// Removes the caller-owned runtime declaration and advances the catalog.
	async fn retract(
		&self,
		identity: &ControlConnectionIdentity,
		provider: &str,
	) -> Result<(), ProviderControlError>;
	/// Validates and dispatches an effect-authorized request through the
	/// application owner.
	async fn request(
		&self,
		identity: &ControlConnectionIdentity,
		request: ProviderControlRequest,
	) -> Result<ProviderControlResult, ProviderControlError>;
}

/// Factory for connection-scoped `omp.provider.*` ownership.
pub struct ProviderControlAuthorityFactory {
	backend: Arc<dyn ProviderControlBackend>,
}

impl ProviderControlAuthorityFactory {
	/// Binds the application provider owner.
	pub fn new(backend: Arc<dyn ProviderControlBackend>) -> Self {
		Self { backend }
	}
}

struct ProviderControlAuthority {
	identity: Arc<ControlConnectionIdentity>,
	backend:  Arc<dyn ProviderControlBackend>,
}

impl ControlAuthorityFactory for ProviderControlAuthorityFactory {
	fn bind(
		&self,
		identity: Arc<ControlConnectionIdentity>,
	) -> Result<Arc<dyn ControlAuthority>, ControlCompositionError> {
		Ok(Arc::new(ProviderControlAuthority { identity, backend: Arc::clone(&self.backend) }))
	}
}

impl ProviderControlAuthority {
	fn validate(&self, context: &ControlRequestContext) -> Result<(), ControlProtocolError> {
		if Arc::ptr_eq(&context.connection, &self.identity) {
			Ok(())
		} else {
			Err(ControlProtocolError::new(
				"StaleGeneration",
				"provider CONTROL authority belongs to a replaced connection",
			))
		}
	}

	fn error(error: ProviderControlError) -> ControlProtocolError {
		match error {
			ProviderControlError::Authorization => {
				ControlProtocolError::new("AuthorizationError", "provider operation is not authorized")
			},
			ProviderControlError::Conflict => ControlProtocolError::new(
				"ProviderConflict",
				"provider declaration conflicts with an existing owner",
			),
			ProviderControlError::InvalidDeclaration(message) => {
				ControlProtocolError::new("InvalidProvider", message)
			},
			ProviderControlError::CapabilityDenied => {
				ControlProtocolError::new("CapabilityDenied", "provider capability is not declared")
			},
			ProviderControlError::NotFound => {
				ControlProtocolError::new("TargetNotFound", "provider resource is not found")
			},
			ProviderControlError::Unauthenticated => {
				ControlProtocolError::new("AuthenticationError", "provider is not authenticated")
			},
			ProviderControlError::StaleGeneration => {
				ControlProtocolError::new("StaleGeneration", "provider catalog generation is stale")
					.retryable(true)
			},
			ProviderControlError::Request(message) => {
				ControlProtocolError::new("ProviderRequestError", message)
			},
		}
	}

	fn provider(arguments: &Map<String, Value>) -> Result<&str, ControlProtocolError> {
		arguments
			.get("provider")
			.and_then(Value::as_str)
			.filter(|provider| !provider.is_empty())
			.ok_or_else(|| ControlProtocolError::new("InvalidProvider", "provider is required"))
	}

	fn cursor(value: Option<&Value>) -> Result<Option<ProviderCatalogCursor>, ControlProtocolError> {
		let Some(value) = value else { return Ok(None) };
		if value.is_null() {
			return Ok(None);
		}
		let value = value.as_object().ok_or_else(|| {
			ControlProtocolError::new("InvalidCursor", "model cursor must be an object")
		})?;
		let epoch = value
			.get("epoch")
			.and_then(Value::as_object)
			.and_then(|epoch| epoch.get("$bytes"))
			.and_then(Value::as_str)
			.and_then(|epoch| omp_core::base64::decode(epoch).into_vec().ok())
			.filter(|epoch| !epoch.is_empty())
			.ok_or_else(|| {
				ControlProtocolError::new("InvalidCursor", "model cursor epoch is malformed")
			})?;
		let generation = value
			.get("generation")
			.and_then(Value::as_u64)
			.ok_or_else(|| {
				ControlProtocolError::new("InvalidCursor", "model cursor generation is missing")
			})?;
		Ok(Some(ProviderCatalogCursor { epoch: epoch.into_boxed_slice(), generation }))
	}

	fn cursor_json(cursor: &ProviderCatalogCursor) -> Value {
		json!({
			"epoch": {"$bytes": omp_core::base64::encode(&cursor.epoch).into_string()},
			"generation": cursor.generation,
		})
	}
}

#[async_trait]
impl ControlAuthority for ProviderControlAuthority {
	fn handles(&self, operation: &str) -> bool {
		matches!(
			operation,
			"omp.provider.models"
				| "omp.provider.watch_models"
				| "omp.provider.is_authenticated"
				| "omp.provider.replace"
				| "omp.provider.retract"
				| "omp.provider.request"
		)
	}

	fn authorize(
		&self,
		context: &ControlRequestContext,
		operation: &str,
		_arguments: &Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		self.validate(context)?;
		if context
			.invocation
			.as_ref()
			.is_some_and(|invocation| invocation.lifecycle != LifecyclePhase::Active)
		{
			return Err(ControlProtocolError::new(
				"PhaseError",
				"provider operations require an active extension lifecycle",
			));
		}
		if operation == "omp.provider.request"
			&& !context.invocation.as_ref().is_some_and(|invocation| {
				invocation
					.phase
					.allows_operation(InvocationPhase::EffectsAuthorized)
			}) {
			return Err(ControlProtocolError::new(
				"PhaseError",
				"provider requests require invocation-scoped effect authority",
			));
		}
		Ok(())
	}

	async fn request(
		&self,
		context: ControlRequestContext,
		operation: Str,
		arguments: Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		self.validate(&context)?;
		match operation.as_str() {
			"omp.provider.models" => {
				let provider = arguments.get("provider").and_then(Value::as_str);
				let cards = self.backend.models(provider).await.map_err(Self::error)?;
				serde_json::to_value(cards)
					.map_err(|error| ControlProtocolError::new("CatalogCodecError", sf!("{error}")))
			},
			"omp.provider.watch_models" => {
				let since = Self::cursor(arguments.get("since"))?;
				let events = self
					.backend
					.watch_models(since)
					.await
					.map_err(Self::error)?;
				Ok(Value::Array(
					events
						.into_iter()
						.map(|event| match event {
							ProviderModelEvent::Upsert { cursor, card } => json!({
								"cursor": Self::cursor_json(&cursor),
								"upserted": card,
							}),
							ProviderModelEvent::Remove { cursor, id } => json!({
								"cursor": Self::cursor_json(&cursor),
								"removed_id": id.as_str(),
							}),
							ProviderModelEvent::Reset { cursor } => json!({
								"cursor": Self::cursor_json(&cursor),
								"reset": true,
							}),
						})
						.collect(),
				))
			},
			"omp.provider.is_authenticated" => Ok(Value::Bool(
				self
					.backend
					.is_authenticated(Self::provider(&arguments)?)
					.await
					.map_err(Self::error)?,
			)),
			"omp.provider.replace" => {
				let provider = Self::provider(&arguments)?;
				let document = arguments
					.get("spec")
					.filter(|spec| spec.is_object())
					.cloned()
					.ok_or_else(|| {
						ControlProtocolError::new(
							"InvalidProvider",
							"replacement provider declaration must be an object",
						)
					})?;
				if document.get("id").and_then(Value::as_str) != Some(provider) {
					return Err(ControlProtocolError::new(
						"InvalidProvider",
						"replacement declaration identity does not match provider",
					));
				}
				self
					.backend
					.replace(&self.identity, ProviderDeclarationDocument {
						provider: Str::from(provider),
						document,
					})
					.await
					.map_err(Self::error)?;
				Ok(Value::Null)
			},
			"omp.provider.retract" => {
				self
					.backend
					.retract(&self.identity, Self::provider(&arguments)?)
					.await
					.map_err(Self::error)?;
				Ok(Value::Null)
			},
			"omp.provider.request" => {
				let provider = Str::from(Self::provider(&arguments)?);
				let kind = match arguments.get("operation").and_then(Value::as_str) {
					Some("generate_image") => ProviderRequestKind::GenerateImage,
					Some("speak") => ProviderRequestKind::Speak,
					Some("transcribe") => ProviderRequestKind::Transcribe,
					Some("realtime") => ProviderRequestKind::Realtime,
					_ => {
						return Err(ControlProtocolError::new(
							"InvalidProviderOperation",
							"provider request operation is unsupported",
						));
					},
				};
				let payload = arguments
					.get("request")
					.and_then(Value::as_object)
					.cloned()
					.ok_or_else(|| {
						ControlProtocolError::new(
							"InvalidProviderRequest",
							"provider request payload must be an object",
						)
					})?;
				let result = self
					.backend
					.request(&self.identity, ProviderControlRequest {
						provider,
						operation: kind,
						payload,
					})
					.await
					.map_err(Self::error)?;
				Ok(match result {
					ProviderControlResult::Image { images, cost_nanos_usd } => {
						json!({"images": images, "cost_nanos_usd": cost_nanos_usd})
					},
					ProviderControlResult::Speech { audio, format, cost_nanos_usd } => json!({
						"audio": audio,
						"format": format.as_str(),
						"cost_nanos_usd": cost_nanos_usd,
					}),
					ProviderControlResult::Transcription { text, language, cost_nanos_usd } => json!({
						"text": text.as_str(),
						"language": language.as_deref(),
						"cost_nanos_usd": cost_nanos_usd,
					}),
					ProviderControlResult::Realtime {
						id,
						endpoint,
						credential,
						expires_at_ms,
						transport,
					} => json!({
						"id": id.as_str(),
						"endpoint": {"id": endpoint.as_str()},
						"credential": {"id": credential.as_str()},
						"expires_at_ms": expires_at_ms,
						"transport": transport.as_str(),
					}),
				})
			},
			_ => Err(ControlProtocolError::new(
				"UnknownOperation",
				"provider authority does not own this operation",
			)),
		}
	}

	async fn effect(
		&self,
		context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		self.validate(&context)?;
		Err(ControlProtocolError::new(
			"UnsupportedEffect",
			"provider requests are correlated CONTROL operations",
		))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn model(value: &str) -> ModelKey {
		ModelKey::from(value)
	}

	#[test]
	fn durable_preference_and_override_are_independent_across_restore() {
		let mut controls = ModelControls::from_durable(BTreeMap::from([(
			"default".into(),
			model("provider/preferred"),
		)]));
		let journaled = controls.switch_session("default", model("provider/temporary"), None);
		assert_eq!(controls.effective("default"), Some(&model("provider/temporary")));

		let mut resumed = ModelControls::from_durable(BTreeMap::from([(
			"default".into(),
			model("provider/preferred"),
		)]));
		resumed.restore_override(Some(journaled));
		assert_eq!(resumed.effective("default"), Some(&model("provider/temporary")));
		resumed.clear_override();
		assert_eq!(resumed.effective("default"), Some(&model("provider/preferred")));
	}

	#[test]
	fn scoped_and_role_cycles_wrap_in_both_directions() {
		let mut controls = ModelControls::default();
		controls.set_scoped_models(Arc::from([
			ScopedModel { model: model("p/a"), thinking: None },
			ScopedModel { model: model("p/b"), thinking: Some(ThinkingEffort::High) },
		]));
		let backward = controls
			.cycle_scoped(ModelKey::from_ref("p/a"), CycleDirection::Backward)
			.unwrap();
		assert_eq!(backward.model, model("p/b"));
		assert_eq!(backward.thinking, Some(ThinkingEffort::High));

		controls.set_durable("slow", model("p/slow"));
		controls.set_durable("default", model("p/default"));
		controls.set_durable("smol", model("p/smol"));
		controls.active_role = Some("default".into());
		let roles: Vec<Str> = ["slow", "default", "smol"]
			.into_iter()
			.map(Str::new)
			.collect();
		let previous = controls
			.cycle_roles(
				&roles,
				|model| model != ModelKey::from_ref("p/slow"),
				CycleDirection::Backward,
			)
			.unwrap();
		assert_eq!(previous.role, "smol");
	}
}
