//! Callback-capable native session construction.

use std::{
	error, iter,
	path::PathBuf,
	sync::Arc,
	time::{Instant, SystemTime, UNIX_EPOCH},
};

use omp_agent::{
	Agent, AgentSnapshot, Journal, PromptError, PromptFacts, PromptHash, PromptSource,
	RenderedPrompt, RpcTurnClient,
};
use omp_catalog::{CandidateProvenance, Catalog, TransportKind};
use omp_core::{Hash32, Str, sf};
use omp_env::EnvClient;
use omp_inference::transport::http::{HttpTransport, PreconnectLaunch};
use omp_secrets::obfuscator::SecretObfuscator;
use omp_telemetry::firehose::{Envelope, Event as TelemetryEvent, Firehose, SessionStart};
use omp_tool::{CapsBase, Registry};
use parking_lot::Mutex;
use thiserror::Error;
use url::Url;

use super::{
	LaunchDiagnostic, LspSessionBinding, ModelCandidateState, ModelFallbackDiagnostic,
	ServiceTierDiagnostic, SessionDiagnostics, SessionHandle, SessionHandleError, SessionIdentity,
	SessionOptions, SessionRevivalFactory, SessionRuntime, ThinkingCeiling, ThinkingDiagnostic,
};
use crate::{
	CallbackSet, ContextPatchHandler, CredentialCallback, EventCallback, FirstDispatchCallback,
	LocalProtocolResolver, ModelPlan, ModelPlanError, PromptCompiler, PromptContribution,
	PromptPatchError, RequestTuningCallback, RuntimeCallbacks, SystemPromptCallback,
	UiContextCallback, UsageConfirmationCallback, model::default_model_plan, resolve_model_plan,
};

/// Stable Environment root descriptor prepared for session composition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRootDescriptor {
	/// Stable id within this session's ordered grant set.
	pub id:      Str,
	/// Canonical file URI passed to the Environment authority.
	pub uri:     Url,
	/// Original host path retained for diagnostics.
	pub path:    PathBuf,
	/// Whether this is the primary working root.
	pub primary: bool,
}

/// Installs session-bound callbacks into the production title, context,
/// credential, request-tuning, and usage-selection owners.
///
/// Implementations retain the clone-cheap authority for the life of the
/// runtime. Installation must complete before provider work can start.
pub trait ProductionCallbackBoundary: Send + Sync + 'static {
	/// Installs every callback dispatcher into its named production subsystem.
	fn install(
		&self,
		callbacks: RuntimeCallbacks,
	) -> Result<(), Box<dyn error::Error + Send + Sync>>;
}

/// Authority inputs needed to compose a production SDK session.
///
/// All types are re-exported by `omp-sdk`; embedders do not need to depend on
/// internal crates or construct an [`Agent`] themselves.
pub struct ProductionSessionComposition {
	/// Established owner-authenticated inference channel.
	pub inference:         RpcTurnClient,
	/// Environment authority used by tools and extension CONTROL.
	pub environment:       EnvClient,
	/// Durable session journal.
	pub journal:           Journal,
	/// Initial per-turn policy. The blueprint installs its registry, prompt
	/// facts, and compiled prompt source before launch.
	pub snapshot:          AgentSnapshot,
	/// Hard output and context ceilings.
	pub caps:              CapsBase,
	/// Application-owned installer for callback classes that cross the agent,
	/// inference, and title-generation boundaries.
	pub callback_boundary: Option<Arc<dyn ProductionCallbackBoundary>>,
}

/// Failure while composing and launching a complete production session.
#[derive(Debug, Error)]
pub enum ProductionSessionError {
	/// Workspace prompt facts could not be projected into agent state.
	#[error(transparent)]
	Prompt(#[from] PromptError),
	/// A registered production callback has no subsystem installer.
	#[error("production callbacks require a callback boundary")]
	MissingCallbackBoundary,
	/// The application callback boundary rejected installation.
	#[error("production callback installation failed")]
	CallbackInstallation {
		/// Typed application installation failure.
		#[source]
		source: Box<dyn error::Error + Send + Sync>,
	},
	/// The live handle could not be launched.
	#[error(transparent)]
	Launch(#[from] SessionHandleError),
}

/// Failure from the high-level build-and-launch entrypoint.
#[derive(Debug, Error)]
pub enum SessionCreateError {
	/// Credential-blind session planning failed.
	#[error(transparent)]
	Build(#[from] SessionBuildError),
	/// Production authority composition failed.
	#[error(transparent)]
	Production(#[from] ProductionSessionError),
}

#[derive(Clone)]
struct CompiledPromptSource(Arc<[omp_agent::Item]>);

impl PromptSource for CompiledPromptSource {
	fn render(&self, _workspace: &omp_agent::Props) -> Result<Vec<omp_agent::Item>, PromptError> {
		Ok(self.0.to_vec())
	}
}

/// Fully resolved, credential-blind session construction result.
pub struct SessionBlueprint {
	options: SessionOptions,
	roots: Box<[WorkspaceRootDescriptor]>,
	model_plan: ModelPlan,
	prompt: RenderedPrompt,
	workspace: PromptFacts,
	registry: Arc<Registry>,
	callbacks: CallbackSet,
	shape: Hash32,
	inherited_prompt_cache_key: Option<PromptHash>,
	diagnostics: SessionDiagnostics,
	constructed_at: Instant,
	firehose: Option<Arc<Firehose>>,
	secret_obfuscator: Option<Arc<Mutex<SecretObfuscator>>>,
}

impl SessionBlueprint {
	/// Returns the immutable owned session options.
	pub const fn options(&self) -> &SessionOptions {
		&self.options
	}

	/// Returns the ordered primary and granted roots.
	pub fn roots(&self) -> &[WorkspaceRootDescriptor] {
		&self.roots
	}

	/// Returns the credential-blind model fallback plan.
	pub const fn model_plan(&self) -> &ModelPlan {
		&self.model_plan
	}

	/// Returns the canonical compiled prompt.
	pub const fn prompt(&self) -> &RenderedPrompt {
		&self.prompt
	}

	/// Returns the immutable prompt/workspace authority snapshot.
	pub const fn prompt_facts(&self) -> &PromptFacts {
		&self.workspace
	}

	/// Returns the authority-built registry shared with the production loop.
	pub const fn registry(&self) -> &Arc<Registry> {
		&self.registry
	}

	/// Returns callback subscriptions and opaque credential resolver.
	pub const fn callbacks(&self) -> &CallbackSet {
		&self.callbacks
	}

	/// Installs builder-owned runtime authorities on a newly composed native
	/// loop.
	pub fn configure_agent<C: omp_agent::TurnClient + Clone>(
		&self,
		agent: &mut omp_agent::Agent<C>,
	) {
		if let Some(firehose) = &self.firehose {
			agent.set_firehose(Arc::clone(firehose));
		}
	}

	/// Returns typed model, thinking, LSP, and launch diagnostics.
	pub const fn diagnostics(&self) -> &SessionDiagnostics {
		&self.diagnostics
	}

	/// Returns the typed provider obfuscation authority, when the host supplied
	/// the complete bidirectional transform path.
	pub const fn secret_obfuscator(&self) -> Option<&Arc<Mutex<SecretObfuscator>>> {
		self.secret_obfuscator.as_ref()
	}

	/// Composes the production agent loop and returns a launchable session
	/// handle without exposing internal construction APIs.
	pub fn launch_production(
		self,
		identity: SessionIdentity,
		mut composition: ProductionSessionComposition,
		revival: Option<SessionRevivalFactory>,
	) -> Result<SessionHandle, ProductionSessionError> {
		let runtime_callbacks = RuntimeCallbacks::new(identity.id.clone(), self.callbacks.clone());
		install_production_callbacks(
			&self.callbacks,
			&runtime_callbacks,
			composition.callback_boundary.as_ref(),
		)?;
		composition.snapshot.registry = Arc::clone(&self.registry);
		composition.snapshot.props = self.workspace.props()?;
		composition.snapshot.prompt_source =
			Arc::new(CompiledPromptSource(Arc::clone(&self.prompt.items)));
		if let Some(candidate) = self.model_plan.candidates().first() {
			composition.snapshot.turn.params.model = candidate.selector.to_string();
		}
		if let Some(active_tools) = &self.options.policies.active_tools {
			composition.snapshot.enabled_tools = Arc::from(active_tools.to_vec());
		}
		if let Some(deadline) = self.options.turn_deadline {
			composition.snapshot.deadline = Instant::now().checked_add(deadline);
		}
		let mut agent = Agent::new(
			composition.inference,
			composition.environment,
			omp_agent::AgentState::new(composition.snapshot),
			composition.journal,
			composition.caps,
		);
		self.configure_agent(&mut agent);
		let runtime = SessionRuntime::from_agent(agent);
		Ok(self.launch_with_callbacks(identity, runtime, revival, runtime_callbacks)?)
	}

	/// Consumes the blueprint into a durable handle over a fully composed live
	/// runtime and an optional journal-backed cold-revival factory.
	///
	/// Callback classes crossing subsystem ownership require the same explicit
	/// production boundary as [`Self::launch_production`].
	pub fn launch(
		self,
		identity: SessionIdentity,
		runtime: SessionRuntime,
		revival: Option<SessionRevivalFactory>,
		callback_boundary: Option<Arc<dyn ProductionCallbackBoundary>>,
	) -> Result<SessionHandle, ProductionSessionError> {
		let callbacks = RuntimeCallbacks::new(identity.id.clone(), self.callbacks.clone());
		install_production_callbacks(&self.callbacks, &callbacks, callback_boundary.as_ref())?;
		Ok(self.launch_with_callbacks(identity, runtime, revival, callbacks)?)
	}

	fn launch_with_callbacks(
		self,
		identity: SessionIdentity,
		runtime: SessionRuntime,
		revival: Option<SessionRevivalFactory>,
		callbacks: RuntimeCallbacks,
	) -> Result<SessionHandle, SessionHandleError> {
		SessionHandle::launch(
			identity,
			self.diagnostics,
			callbacks,
			Some(runtime),
			revival,
			self.constructed_at,
			self.firehose,
		)
	}

	/// Consumes the blueprint into a cold handle that revives from its journal
	/// before accepting the first submission in this process.
	pub fn revive(
		self,
		identity: SessionIdentity,
		revival: SessionRevivalFactory,
	) -> Result<SessionHandle, SessionHandleError> {
		let callbacks = RuntimeCallbacks::new(identity.id.clone(), self.callbacks.clone());
		SessionHandle::launch(
			identity,
			self.diagnostics,
			callbacks,
			None,
			Some(revival),
			self.constructed_at,
			self.firehose,
		)
	}

	/// Returns the complete shape fingerprint used for fork cache inheritance.
	pub const fn shape(&self) -> Hash32 {
		self.shape
	}

	/// Returns the inherited parent prompt-cache key when the complete session
	/// shape is unchanged.
	pub const fn inherited_prompt_cache_key(&self) -> Option<PromptHash> {
		self.inherited_prompt_cache_key
	}

	/// Consumes the blueprint and returns its shared versioned tool registry.
	pub fn into_registry(self) -> Arc<Registry> {
		self.registry
	}
}

/// Session construction failure before any loop or credential acquisition.
#[derive(Debug, Error)]
pub enum SessionBuildError {
	/// A root cannot be represented as an Environment file URI.
	#[error("session root cannot be represented as a file URI: {path:?}")]
	InvalidRoot {
		/// Rejected root path.
		path: PathBuf,
	},
	/// Two root grants resolve to the same URI.
	#[error("session contains duplicate workspace root {uri}")]
	DuplicateRoot {
		/// Duplicate URI.
		uri: Url,
	},
	/// The embedded catalog has no default candidate.
	#[error("model catalog contains no selectable default")]
	NoDefaultModel,
	/// Semantic model planning failed.
	#[error(transparent)]
	Model(#[from] ModelPlanError),
	/// Prompt callback or canonical rendering failed.
	#[error(transparent)]
	Prompt(#[from] PromptPatchError),
}

/// Public callback-capable session builder.
pub struct SessionBuilder {
	options:                 SessionOptions,
	registry:                Arc<Registry>,
	callbacks:               CallbackSet,
	contributions:           Vec<PromptContribution>,
	parent_shape:            Option<(Hash32, PromptHash)>,
	callback_shape_revision: Option<Hash32>,
	lsp_bindings:            Vec<LspSessionBinding>,
	firehose:                Option<Arc<Firehose>>,
	secret_obfuscator:       Option<Arc<Mutex<SecretObfuscator>>>,
	constructed_at:          Instant,
}

impl SessionBuilder {
	/// Starts a session over owned options and an authority-built registry.
	pub fn new(options: SessionOptions, registry: Arc<Registry>) -> Self {
		Self {
			options,
			registry,
			callbacks: CallbackSet::default(),
			contributions: Vec::new(),
			parent_shape: None,
			callback_shape_revision: None,
			lsp_bindings: Vec::new(),
			firehose: None,
			secret_obfuscator: None,
			constructed_at: Instant::now(),
		}
	}

	/// Starts a full fork, tentatively inheriting the parent's provider affinity
	/// and prompt-cache key. Build-time shape comparison invalidates the cache
	/// key when any shape-changing option differs.
	pub fn fork_from(
		parent: &SessionBlueprint,
		options: SessionOptions,
		registry: Arc<Registry>,
	) -> Self {
		Self::new(options, registry).with_parent_shape(parent.shape, parent.prompt.hash)
	}

	fn with_parent_shape(mut self, shape: Hash32, cache_key: PromptHash) -> Self {
		self.parent_shape = Some((shape, cache_key));
		self
	}

	/// Declares the stable implementation revision of shape-changing context or
	/// request-tuning callbacks. Without this revision, full forks remain safe
	/// by declining prompt-cache inheritance.
	pub fn callback_shape_revision(mut self, revision: Hash32) -> Self {
		self.callback_shape_revision = Some(revision);
		self
	}

	/// Adds one static typed prompt contribution.
	pub fn prompt_contribution(mut self, contribution: PromptContribution) -> Self {
		self.contributions.push(contribution);
		self
	}

	/// Installs a deterministic provider-system-prompt callback.
	pub fn system_prompt_callback(mut self, callback: SystemPromptCallback) -> Self {
		self.callbacks.system_prompt = Some(callback);
		self
	}

	/// Installs a deterministic title-system-prompt callback.
	pub fn title_prompt_callback(mut self, callback: SystemPromptCallback) -> Self {
		self.callbacks.title_prompt = Some(callback);
		self
	}

	/// Installs a stable-id provider-context projection callback.
	pub fn context_callback(mut self, callback: ContextPatchHandler) -> Self {
		self.callbacks.context = Some(callback);
		self
	}

	/// Installs inference-owned opaque credential resolution.
	pub fn credential_callback(mut self, callback: CredentialCallback) -> Self {
		self.callbacks.credential = Some(callback);
		self
	}

	/// Installs typed provider-request tuning.
	pub fn request_tuning_callback(mut self, callback: RequestTuningCallback) -> Self {
		self.callbacks.request_tuning = Some(callback);
		self
	}

	/// Subscribes to read-only agent events.
	pub fn on_event(mut self, callback: EventCallback) -> Self {
		self.callbacks.events.push(callback);
		self
	}

	/// Installs the first-provider-dispatch notification.
	pub fn on_first_dispatch(mut self, callback: FirstDispatchCallback) -> Self {
		self.callbacks.first_dispatch = Some(callback);
		self
	}

	/// Installs deferred usage-reserve confirmation at the typed host boundary.
	pub fn usage_confirmation(mut self, callback: UsageConfirmationCallback) -> Self {
		self.callbacks.usage_confirmation = Some(callback);
		self
	}

	/// Adds one generation-fenced opaque LSP binding and its warmup state.
	pub fn lsp_binding(mut self, binding: LspSessionBinding) -> Self {
		self.lsp_bindings.push(binding);
		self
	}

	/// Installs the consent-configured telemetry fan-out used for launch facts.
	pub fn firehose(mut self, firehose: Arc<Firehose>) -> Self {
		self.firehose = Some(firehose);
		self
	}

	/// Installs a complete bidirectional provider secret-transform authority.
	///
	/// The builder retains only the process-local transform handle. Credential
	/// bytes and callback-returned leases never enter the blueprint shape or
	/// journal state.
	pub fn secret_obfuscator(mut self, obfuscator: Arc<Mutex<SecretObfuscator>>) -> Self {
		self.secret_obfuscator = Some(obfuscator);
		self
	}

	/// Installs UI-context updates at the host boundary.
	pub fn ui_context_callback(mut self, callback: UiContextCallback) -> Self {
		self.callbacks.ui_context = Some(callback);
		self
	}

	/// Declares one host-local protocol resolver.
	pub fn local_protocol(
		mut self,
		scheme: impl Into<Str>,
		resolver: LocalProtocolResolver,
	) -> Self {
		self
			.callbacks
			.local_protocols
			.push((scheme.into(), resolver));
		self
	}

	/// Mutably exposes the typed callback collection for event, context,
	/// credential, title, UI, and local-protocol registration.
	pub const fn callbacks_mut(&mut self) -> &mut CallbackSet {
		&mut self.callbacks
	}

	/// Builds and launches a complete production session through the stable SDK
	/// facade.
	pub fn create_session(
		self,
		catalog: &Catalog,
		workspace: &PromptFacts,
		identity: SessionIdentity,
		composition: ProductionSessionComposition,
		revival: Option<SessionRevivalFactory>,
	) -> Result<SessionHandle, SessionCreateError> {
		Ok(self
			.build(catalog, workspace)?
			.launch_production(identity, composition, revival)?)
	}

	/// Resolves roots, models, prompt bytes, and fork-cache inheritance without
	/// touching credential material or starting processes.
	pub fn build(
		self,
		catalog: &Catalog,
		workspace: &PromptFacts,
	) -> Result<SessionBlueprint, SessionBuildError> {
		let mut options = self.options;
		let session_id = options
			.identity
			.id
			.get_or_insert_with(|| Str::from(omp_core::Ulid::generate().to_string()))
			.clone();
		let roots = prepare_roots(&options)?;
		let mut model_plan = if options.model_selectors.is_empty() {
			default_model_plan(catalog).ok_or(SessionBuildError::NoDefaultModel)?
		} else {
			resolve_model_plan(
				catalog,
				&options.model_selectors,
				&options.model_roles,
				&options.enabled_models,
			)?
		};
		let requested_thinking = model_plan
			.candidates()
			.first()
			.and_then(|candidate| candidate.selected.as_ref())
			.and_then(|selected| selected.thinking.clone());
		let thinking_rank = match options.thinking_ceiling {
			ThinkingCeiling::Off => 0,
			ThinkingCeiling::Minimal => 1,
			ThinkingCeiling::Low => 2,
			ThinkingCeiling::Medium => 3,
			ThinkingCeiling::High => 4,
			ThinkingCeiling::ExtraHigh => 5,
			ThinkingCeiling::Max => 6,
		};
		model_plan.clamp_thinking(thinking_rank);
		let effective_thinking = model_plan
			.candidates()
			.first()
			.and_then(|candidate| candidate.selected.as_ref())
			.and_then(|selected| selected.thinking.clone());
		let preconnect = preconnect_model_host(catalog, &model_plan);
		let models = model_plan
			.candidates()
			.iter()
			.enumerate()
			.map(|(ordinal, candidate)| ModelFallbackDiagnostic {
				ordinal:  u32::try_from(ordinal).unwrap_or(u32::MAX),
				selector: candidate.selector.clone(),
				fallback: ordinal != 0,
				state:    match candidate.provenance {
					CandidateProvenance::Catalog => ModelCandidateState::Catalog,
					CandidateProvenance::ConfiguredDeclared => {
						ModelCandidateState::ConfiguredUndiscoverable
					},
				},
			})
			.collect::<Vec<_>>()
			.into_boxed_slice();
		let diagnostics = SessionDiagnostics {
			models,
			thinking: ThinkingDiagnostic {
				clamped:   requested_thinking != effective_thinking,
				requested: requested_thinking,
				effective: effective_thinking,
			},
			service_tier: ServiceTierDiagnostic {
				requested: options.service_tier.clone(),
				effective: options.service_tier.clone(),
				clamped:   false,
			},
			launch: Arc::new(parking_lot::RwLock::new(LaunchDiagnostic {
				preconnect,
				first_dispatch_ms: None,
			})),
			lsp: self.lsp_bindings.into_boxed_slice(),
		};
		let mut compiler = PromptCompiler::new();
		for contribution in self.contributions {
			compiler = compiler.contribution(contribution);
		}
		if let Some(callback) = self.callbacks.system_prompt.clone() {
			compiler = compiler.callback(callback);
		}
		let prompt = compiler.compile(&workspace.props().map_err(PromptPatchError::from)?)?;
		let shape = session_shape(
			&options,
			&roots,
			&model_plan,
			&prompt,
			&self.registry,
			self.callback_shape_revision,
		);
		let callback_shape_is_stable = (self.callbacks.context.is_none()
			&& self.callbacks.request_tuning.is_none())
			|| self.callback_shape_revision.is_some();
		let inherited_prompt_cache_key = if callback_shape_is_stable {
			self
				.parent_shape
				.and_then(|(parent_shape, cache_key)| (parent_shape == shape).then_some(cache_key))
		} else {
			None
		};
		if let Some(firehose) = &self.firehose {
			firehose.publish(TelemetryEvent::SessionStart(Box::new(SessionStart {
				envelope:      Envelope {
					session_id: session_id.clone(),
					agent_id: options
						.identity
						.display_name
						.clone()
						.unwrap_or_else(|| session_id.clone()),
					occurred_at_ms: now_ms(),
					..Envelope::default()
				},
				schema_rev:    omp_proto::SCHEMA_REV,
				registry_hash: Str::from(self.registry.projection_hash().to_string()),
			})));
		}
		Ok(SessionBlueprint {
			options,
			roots,
			model_plan,
			prompt,
			workspace: workspace.clone(),
			registry: self.registry,
			callbacks: self.callbacks,
			shape,
			inherited_prompt_cache_key,
			diagnostics,
			constructed_at: self.constructed_at,
			firehose: self.firehose,
			secret_obfuscator: self.secret_obfuscator,
		})
	}
}

fn preconnect_model_host(catalog: &Catalog, plan: &ModelPlan) -> PreconnectLaunch {
	let Some(selected) = plan
		.candidates()
		.first()
		.and_then(|candidate| candidate.selected.as_ref())
	else {
		return PreconnectLaunch::UnsupportedEndpoint;
	};
	let route = selected
		.route
		.as_ref()
		.and_then(|route| catalog.route(route))
		.or_else(|| {
			catalog
				.model_for_provider(&selected.provider, &selected.model)
				.and_then(|model| {
					model.routes.iter().find_map(|route| {
						catalog
							.route(route)
							.filter(|route| route.provider == selected.provider)
					})
				})
		});
	let Some(route) = route else {
		return PreconnectLaunch::UnsupportedEndpoint;
	};
	if route.transport == TransportKind::Local {
		return PreconnectLaunch::UnsupportedEndpoint;
	}
	let Ok(url) = Url::parse(route.endpoint.base_url.as_str()) else {
		return PreconnectLaunch::InvalidEndpoint;
	};
	HttpTransport::preconnect_host(&url)
}

fn install_production_callbacks(
	callbacks: &CallbackSet,
	runtime_callbacks: &RuntimeCallbacks,
	boundary: Option<&Arc<dyn ProductionCallbackBoundary>>,
) -> Result<(), ProductionSessionError> {
	if !callbacks.requires_production_install() {
		return Ok(());
	}
	let boundary = boundary.ok_or(ProductionSessionError::MissingCallbackBoundary)?;
	boundary
		.install(runtime_callbacks.clone())
		.map_err(|source| ProductionSessionError::CallbackInstallation { source })
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

fn prepare_roots(
	options: &SessionOptions,
) -> Result<Box<[WorkspaceRootDescriptor]>, SessionBuildError> {
	let mut roots = Vec::with_capacity(options.additional_roots.len() + 1);
	for (index, path) in iter::once(&options.cwd)
		.chain(options.additional_roots.iter())
		.enumerate()
	{
		let uri = Url::from_directory_path(path)
			.map_err(|()| SessionBuildError::InvalidRoot { path: path.clone() })?;
		if roots
			.iter()
			.any(|root: &WorkspaceRootDescriptor| root.uri == uri)
		{
			return Err(SessionBuildError::DuplicateRoot { uri });
		}
		roots.push(WorkspaceRootDescriptor {
			id: if index == 0 {
				sf!("primary")
			} else {
				sf!("root-{index}")
			},
			uri,
			path: path.clone(),
			primary: index == 0,
		});
	}
	Ok(roots.into_boxed_slice())
}

fn session_shape(
	options: &SessionOptions,
	roots: &[WorkspaceRootDescriptor],
	models: &ModelPlan,
	prompt: &RenderedPrompt,
	registry: &Registry,
	callback_shape_revision: Option<Hash32>,
) -> Hash32 {
	let mut hasher = Hash32::hasher();
	hasher.update(prompt.hash.as_bytes());
	if let Some(revision) = callback_shape_revision {
		hasher.update(revision.as_bytes());
	}
	for root in roots {
		hasher.update(root.id.as_bytes());
		hasher.update(root.uri.as_str().as_bytes());
	}
	for candidate in models.candidates() {
		hasher.update(candidate.selector.as_bytes());
	}
	for (name, revision) in registry.live_identities() {
		hasher.update(name.as_bytes());
		hasher.update(revision.family.as_bytes());
		hasher.update(revision.n.to_le_bytes());
		let presentation: &'static str = registry
			.presentation(name)
			.expect("live registry identity has a presentation")
			.into();
		hasher.update(presentation.as_bytes());
	}
	hasher.update(&[options.thinking_ceiling as u8]);
	if let Some(service_tier) = &options.service_tier {
		hasher.update(service_tier.as_bytes());
	}
	if let Some(active_tools) = &options.policies.active_tools {
		for name in active_tools {
			hasher.update(name.as_bytes());
		}
	}
	for path in options
		.discovery
		.extension_paths
		.iter()
		.chain(options.discovery.skill_paths.iter())
		.chain(options.discovery.context_paths.iter())
		.chain(options.discovery.template_paths.iter())
		.chain(options.discovery.command_paths.iter())
		.chain(options.discovery.mcp_paths.iter())
	{
		hasher.update(path.as_os_str().as_encoded_bytes());
	}
	hasher.update(&[
		options.subsystems.eval as u8,
		options.subsystems.mcp as u8,
		options.subsystems.lsp as u8,
		options.subsystems.irc as u8,
		options.subsystems.mnemopi as u8,
		options.subsystems.workspace_tree as u8,
		options.policies.allow_spawns as u8,
		options.policies.restricted as u8,
		options.policies.prewalk as u8,
		options.policies.plan_yolo as u8,
		options.policies.interactive_prompt as u8,
	]);
	hasher.update(options.policies.max_depth.to_le_bytes());
	if let Some(display_name) = &options.identity.display_name {
		hasher.update(display_name.as_bytes());
	}
	hasher.update(options.identity.depth.to_le_bytes());
	if let Some(schema) = &options.output_schema {
		let encoded = serde_json::to_vec(schema).expect("JSON values serialize");
		hasher.update(encoded);
	}
	hasher.update(&[options.strict_output_schema as u8, options.defer_usage_confirmation as u8]);
	hasher.finalize()
}
#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};

	use omp_agent::{PromptPatchSet, Props};

	use super::*;

	struct TitleBoundary {
		installs: Arc<AtomicUsize>,
	}

	impl ProductionCallbackBoundary for TitleBoundary {
		fn install(
			&self,
			callbacks: RuntimeCallbacks,
		) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
			self.installs.fetch_add(1, Ordering::Relaxed);
			callbacks
				.title_prompt(&Props::default())
				.expect("title callback installed")
				.expect("title callback succeeds");
			Ok(())
		}
	}

	#[test]
	fn production_callbacks_cannot_launch_without_their_boundary() {
		let calls = Arc::new(AtomicUsize::new(0));
		let mut callbacks = CallbackSet::default();
		let callback_calls = Arc::clone(&calls);
		callbacks.title_prompt = Some(Arc::new(move |_| {
			callback_calls.fetch_add(1, Ordering::Relaxed);
			PromptPatchSet::new(Vec::new(), PromptPatchSet::DEFAULT_MAX_BYTE_EXPANSION)
		}));
		let runtime = RuntimeCallbacks::new("session".into(), callbacks.clone());
		assert!(matches!(
			install_production_callbacks(&callbacks, &runtime, None),
			Err(ProductionSessionError::MissingCallbackBoundary)
		));

		let installs = Arc::new(AtomicUsize::new(0));
		let boundary: Arc<dyn ProductionCallbackBoundary> =
			Arc::new(TitleBoundary { installs: Arc::clone(&installs) });
		install_production_callbacks(&callbacks, &runtime, Some(&boundary))
			.expect("callback boundary installs");
		assert_eq!(installs.load(Ordering::Relaxed), 1);
		assert_eq!(calls.load(Ordering::Relaxed), 2);
	}
}
