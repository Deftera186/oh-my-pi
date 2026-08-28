//! Daemon-scoped discovery refresh coordination and catalog publication.

use std::{
	collections::{BTreeMap, BTreeSet},
	path::PathBuf,
	sync::Arc,
	time::{Duration, Instant},
};

use async_trait::async_trait;
use omp_catalog::{
	CatalogOverlay, CatalogOverlayBuilder, ClassId, DiscoveredModel, DiscoveryNormalizer,
	DiscoveryPollGate, DiscoveryPollKey, DiscoverySpec, EvidenceConfidence, ModelLimits,
	ModelOverlay, ModelPatch, ModelSpec, OperationBits, OperationKind, OverlaySource, OverlayStore,
	ProvenanceKind, ProvenanceSource, ProviderId, ScopedAlias, UnsafeTrustScope, WireModelId,
	snapshot,
};
use omp_core::{Principal, Str, sf};
use omp_envd::exthost::control::{
	ControlAuthorityFactory, ControlConnectionIdentity, ControlProtocolError,
};
use omp_inference::{
	Client, ModelsDiscoverHookRequest, ProviderResponseHooks, Registry,
	call::{CallMeta, DiscoveryRequest, Target},
	discovery::{
		DiscoveryCacheKey, DiscoveryHttpClient, DiscoveryProbe, DiscoveryStore, DiscoveryStoreError,
		ProbeError, ProbeHttpFuture, ProbeHttpRequest, ProviderDiscoveryState, ProviderLifecycle,
	},
	id::RequestId,
	receipt::ExecutionBudget,
	router,
};
use omp_proto::inference::v1::{
	self as pb, Effort, inference_client::InferenceClient, model_event, price,
	provider_operation_request, provider_operation_response,
};
use parking_lot::Mutex as SyncMutex;
use serde_json::{Map, Value};
use tokio::{
	sync::{Mutex, watch},
	task::JoinHandle,
	time,
};
use tokio_util::sync::CancellationToken;
use tonic::{Code, Status, transport::Channel};

use crate::{
	chat::{ChatProviderControlBackend, RegimeControlResolver},
	model_controls::{
		ProductionProviderApplicationOwner, ProviderCatalogCursor, ProviderControlAuthorityFactory,
		ProviderControlBackend, ProviderControlError, ProviderControlRequest, ProviderControlResult,
		ProviderDeclarationDocument, ProviderModelCard, ProviderModelEvent, ProviderPrice,
		ProviderRequestKind,
	},
};

const SHARED_CATALOG_URL: &str = "https://catalog.stencil.so/models.json.zstd";
const SHARED_CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);
const SHARED_CATALOG_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
const SHARED_CATALOG_TRANSPORT_DEADLINE: Duration = Duration::from_secs(15);
const SHARED_CATALOG_BACKGROUND_SUBSCRIBER_DEADLINE: Duration = Duration::from_secs(30);

type RegimeConstructor =
	dyn Fn(Option<&str>) -> Result<Box<dyn omp_agent::Regime>, ControlProtocolError> + Send + Sync;

/// One callback constructor bound to a declaration accepted at FREEZE.
#[derive(Clone)]
pub struct SealedRegimeDeclaration {
	/// Authenticated extension which owns this declaration.
	pub extension:          Str,
	/// Child generation whose frozen table supplied the declaration.
	pub host_generation:    u64,
	/// Session generation whose agent owner may start it.
	pub session_generation: u64,
	/// Immutable Core regime policy lowered from the sealed manifest.
	pub spec:               Arc<omp_agent::RegimeSpec>,
	constructor:            Arc<RegimeConstructor>,
}

impl SealedRegimeDeclaration {
	/// Binds a verified declaration to its generation-fenced regime
	/// constructor.
	pub fn new<F>(
		extension: impl Into<Str>,
		host_generation: u64,
		session_generation: u64,
		spec: Arc<omp_agent::RegimeSpec>,
		constructor: F,
	) -> Self
	where
		F: Fn(Option<&str>) -> Result<Box<dyn omp_agent::Regime>, ControlProtocolError>
			+ Send
			+ Sync
			+ 'static,
	{
		Self {
			extension: extension.into(),
			host_generation,
			session_generation,
			spec,
			constructor: Arc::new(constructor),
		}
	}
}

/// Resolver over the exact regime declaration generation retained at FREEZE.
///
/// Regime constructors are admitted together with their immutable specs; a
/// child request can select only by declaration id and can never supply an
/// executable regime handler.
pub struct SealedRegimeControlResolver {
	declarations: BTreeMap<Str, SealedRegimeDeclaration>,
}

impl SealedRegimeControlResolver {
	/// Seals one deterministic declaration table.
	pub fn new(
		declarations: impl IntoIterator<Item = SealedRegimeDeclaration>,
	) -> Result<Self, ControlProtocolError> {
		let mut table = BTreeMap::new();
		for declaration in declarations {
			if declaration.spec.id.is_empty()
				|| declaration.host_generation == 0
				|| declaration.session_generation == 0
				|| table
					.insert(declaration.spec.id.clone(), declaration)
					.is_some()
			{
				return Err(ControlProtocolError::new(
					"InvalidRegimeDeclaration",
					"sealed regime declarations contain an invalid or duplicate identity",
				));
			}
		}
		Ok(Self { declarations: table })
	}
}

impl RegimeControlResolver for SealedRegimeControlResolver {
	fn resolve(
		&self,
		identity: &ControlConnectionIdentity,
		regime: &str,
		state: Option<&[u8]>,
		_state_revision: Option<u32>,
	) -> Result<(Arc<omp_agent::RegimeSpec>, Box<dyn omp_agent::Regime>), ControlProtocolError> {
		let declaration = self.declarations.get(regime).ok_or_else(|| {
			ControlProtocolError::new(
				"TargetNotFound",
				"regime is absent from the sealed declaration table",
			)
		})?;
		if declaration.extension != identity.extension {
			return Err(ControlProtocolError::new(
				"AuthorizationError",
				"regime declaration belongs to another extension",
			));
		}
		if declaration.host_generation != identity.host_generation
			|| declaration.session_generation != identity.session_generation
		{
			return Err(ControlProtocolError::new(
				"StaleGeneration",
				"regime declaration belongs to a replaced host or session generation",
			));
		}
		let state = state
			.map(|state| {
				std::str::from_utf8(state).map_err(|_| {
					ControlProtocolError::new("InvalidRegimeState", "regime state must be UTF-8")
				})
			})
			.transpose()?;
		let regime = (declaration.constructor)(state)?;
		Ok((Arc::clone(&declaration.spec), regime))
	}

	fn owner(&self, regime: &str) -> Option<Str> {
		self
			.declarations
			.get(regime)
			.map(|declaration| declaration.extension.clone())
	}
}

/// Reqwest transport shared by endpoint probes and shared-catalog refreshes.
#[derive(Clone, Debug)]
pub struct RuntimeDiscoveryHttpClient {
	client:             reqwest::Client,
	shared_catalog_url: Str,
}

impl RuntimeDiscoveryHttpClient {
	/// Creates the production transport with the well-known shared-catalog URL.
	pub fn new() -> Self {
		Self {
			client:             reqwest::Client::new(),
			shared_catalog_url: Str::new_static(SHARED_CATALOG_URL),
		}
	}

	#[cfg(test)]
	fn with_shared_catalog_url(url: impl Into<Str>) -> Self {
		Self { client: reqwest::Client::new(), shared_catalog_url: url.into() }
	}

	async fn fetch_shared_catalog(
		&self,
		etag: Option<&str>,
		cancellation: CancellationToken,
	) -> Result<SharedCatalogHttpResponse, SharedCatalogFetchError> {
		let mut request = self
			.client
			.get(self.shared_catalog_url.as_str())
			.header(reqwest::header::ACCEPT, "application/zstd, application/json");
		if let Some(etag) = etag {
			request = request.header(reqwest::header::IF_NONE_MATCH, etag);
		}
		let execute = async {
			let response = request
				.send()
				.await
				.map_err(SharedCatalogFetchError::Transport)?;
			if response.status() == reqwest::StatusCode::NOT_MODIFIED {
				return Ok(SharedCatalogHttpResponse::NotModified);
			}
			if !response.status().is_success() {
				return Err(SharedCatalogFetchError::Status { status: response.status().as_u16() });
			}
			let etag = response
				.headers()
				.get(reqwest::header::ETAG)
				.and_then(|value| value.to_str().ok())
				.map(Str::new);
			let body = response
				.bytes()
				.await
				.map_err(SharedCatalogFetchError::Transport)?;
			Ok(SharedCatalogHttpResponse::Payload { body, etag })
		};
		tokio::select! {
			_ = cancellation.cancelled() => Err(SharedCatalogFetchError::Cancelled),
			result = time::timeout(SHARED_CATALOG_TRANSPORT_DEADLINE, execute) => {
				result.map_err(|_| SharedCatalogFetchError::Timeout)?
			},
		}
	}
}

impl Default for RuntimeDiscoveryHttpClient {
	fn default() -> Self {
		Self::new()
	}
}

impl DiscoveryHttpClient for RuntimeDiscoveryHttpClient {
	fn request(
		&self,
		request: ProbeHttpRequest,
		cancellation: CancellationToken,
	) -> ProbeHttpFuture {
		let client = self.client.clone();
		Box::pin(async move {
			let deadline = request.deadline;
			let execute = async move {
				let mut builder = client.request(request.method, request.url.as_str());
				if !request.body.is_empty() {
					builder = builder.body(request.body);
				}
				let response = builder.send().await.map_err(|_| ProbeError::Transport)?;
				if !response.status().is_success() {
					return Err(ProbeError::Protocol);
				}
				response.bytes().await.map_err(|_| ProbeError::Transport)
			};
			tokio::select! {
				_ = cancellation.cancelled() => Err(ProbeError::Cancelled),
				result = time::timeout(deadline, execute) => {
					result.map_err(|_| ProbeError::Timeout)?
				},
			}
		})
	}
}

enum SharedCatalogHttpResponse {
	NotModified,
	Payload { body: bytes::Bytes, etag: Option<Str> },
}

#[derive(Debug, thiserror::Error)]
enum SharedCatalogFetchError {
	#[error("shared catalog request was cancelled")]
	Cancelled,
	#[error("shared catalog request timed out")]
	Timeout,
	#[error("shared catalog transport failed")]
	Transport(#[source] reqwest::Error),
	#[error("shared catalog returned HTTP status {status}")]
	Status { status: u16 },
}

/// Shared-catalog compilation or persistence failure.
#[derive(Debug, thiserror::Error)]
pub enum SharedCatalogRefreshError {
	/// The remote payload could not be compiled into safe additive rows.
	#[error("shared catalog payload was rejected")]
	Catalog(#[source] snapshot::SharedCatalogError),
	/// The credential-blind overlay cache could not be persisted.
	#[error("shared catalog cache publication failed")]
	Cache(#[source] snapshot::OverlayCacheError),
	/// A blocking compiler or cache task could not be joined.
	#[error("shared catalog worker failed")]
	Worker(#[source] tokio::task::JoinError),
}

/// Source supplying the currently published shared-catalog slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedCatalogSource {
	/// No remote addition is available; only bundled rows are active.
	Bundled,
	/// A persisted additive slice is serving an offline or failed refresh.
	DiskCache,
	/// The current conditional fetch or revalidation succeeded.
	Remote,
}

/// Stable category recorded for the most recent failed revalidation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedCatalogFailureKind {
	/// Subscriber-independent transport cancellation.
	Cancelled,
	/// The transport hard deadline elapsed.
	Timeout,
	/// The HTTP exchange failed.
	Transport,
	/// The server returned a non-success status.
	Status,
	/// The remote catalog failed compilation or additive admission.
	Compile,
}

/// Truthful status of one shared-catalog refresh attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedCatalogRefreshOutcome {
	/// Source contributing non-bundled model rows after this attempt.
	pub source:                 SharedCatalogSource,
	/// Whether the remote source failed or has not yet been revalidated.
	pub stale:                  bool,
	/// Number of additive shared-catalog model entries.
	pub models:                 usize,
	/// Last successful remote observation time.
	pub updated_at_ms:          Option<u64>,
	/// Most recent failed revalidation time.
	pub revalidation_failed_ms: Option<u64>,
	/// Stable failure category for the most recent revalidation.
	pub revalidation_failure:   Option<SharedCatalogFailureKind>,
}

/// Summary of one on-demand inference-routed discovery pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RegistryDiscoveryRefreshOutcome {
	/// Secret-free model rows published across provider caches.
	pub models:   usize,
	/// Provider routes skipped because another caller owns the same gate.
	pub skipped:  usize,
	/// Provider routes that failed without replacing their prior slice.
	pub failures: usize,
}

type SharedCatalogResult = Result<SharedCatalogRefreshOutcome, Arc<SharedCatalogRefreshError>>;

struct SharedCatalogState {
	inflight:      Option<watch::Receiver<Option<SharedCatalogResult>>>,
	etag:          Option<Str>,
	last_attempt:  Option<Instant>,
	cache_overlay: Option<CatalogOverlay>,
	outcome:       SharedCatalogRefreshOutcome,
}

impl SharedCatalogState {
	fn from_cache(cache_overlay: Option<CatalogOverlay>) -> Self {
		let contributed = cache_overlay
			.as_ref()
			.is_some_and(|overlay| !overlay.is_empty());
		let updated_at_ms = cache_overlay
			.as_ref()
			.and_then(|overlay| overlay.source().observed_at_ms);
		let models = cache_overlay
			.as_ref()
			.map_or(0, CatalogOverlay::model_count);
		Self {
			inflight: None,
			etag: None,
			last_attempt: None,
			cache_overlay,
			outcome: SharedCatalogRefreshOutcome {
				source:                 if contributed {
					SharedCatalogSource::DiskCache
				} else {
					SharedCatalogSource::Bundled
				},
				stale:                  true,
				models:                 if contributed { models } else { 0 },
				updated_at_ms:          if contributed { updated_at_ms } else { None },
				revalidation_failed_ms: None,
				revalidation_failure:   None,
			},
		}
	}
}

#[derive(Default)]
struct LiveDiscoveryState {
	providers: BTreeMap<ProviderId, CatalogOverlay>,
	shared:    Option<CatalogOverlay>,
}

#[derive(Default)]
struct ExtensionDiscoveryState {
	generations: BTreeMap<ProviderId, u64>,
	models:      BTreeMap<ProviderId, BTreeMap<Str, Value>>,
}

/// One daemon-wide discovery coordinator shared by every attached session.
pub struct DiscoveryRuntime {
	gate:            DiscoveryPollGate,
	cache:           Arc<DiscoveryStore>,
	overlays:        Arc<OverlayStore>,
	base:            Arc<snapshot::Catalog>,
	overlay_cache:   PathBuf,
	http:            Arc<RuntimeDiscoveryHttpClient>,
	disabled:        BTreeSet<omp_catalog::ProviderId>,
	disk_endpoint:   SyncMutex<Option<CatalogOverlay>>,
	shared_cache:    SyncMutex<Option<CatalogOverlay>>,
	route_refreshes: SyncMutex<BTreeSet<DiscoveryPollKey>>,
	live:            Mutex<LiveDiscoveryState>,
	shared:          Mutex<SharedCatalogState>,
	extension:       Mutex<ExtensionDiscoveryState>,
}

impl DiscoveryRuntime {
	/// Creates a coordinator, loading and sanitizing its credential-blind shared
	/// catalog cache before any network work begins.
	pub fn new(
		cache: Arc<DiscoveryStore>,
		overlays: Arc<OverlayStore>,
		disabled: impl IntoIterator<Item = ProviderId>,
		base: Arc<snapshot::Catalog>,
		overlay_cache: PathBuf,
	) -> Result<Self, DiscoveryRuntimeError> {
		Self::with_http_client(
			cache,
			overlays,
			disabled,
			base,
			overlay_cache,
			Arc::new(RuntimeDiscoveryHttpClient::new()),
		)
	}

	fn with_http_client(
		cache: Arc<DiscoveryStore>,
		overlays: Arc<OverlayStore>,
		disabled: impl IntoIterator<Item = ProviderId>,
		base: Arc<snapshot::Catalog>,
		overlay_cache: PathBuf,
		http: Arc<RuntimeDiscoveryHttpClient>,
	) -> Result<Self, DiscoveryRuntimeError> {
		let cached = snapshot::read_discovery_overlay_cache(&overlay_cache)?
			.map(|overlay| base.sanitize_shared_catalog_overlay(overlay))
			.transpose()?;
		if let Some(overlay) = cached.as_ref() {
			overlays.replace(OverlaySource::DiskCache, overlay.clone());
		}
		Ok(Self {
			gate: DiscoveryPollGate::default(),
			cache,
			overlays,
			base,
			overlay_cache,
			http,
			disabled: disabled.into_iter().collect(),
			disk_endpoint: SyncMutex::new(None),
			shared_cache: SyncMutex::new(cached.clone()),
			route_refreshes: SyncMutex::new(BTreeSet::new()),
			live: Mutex::new(LiveDiscoveryState { providers: BTreeMap::new(), shared: None }),
			shared: Mutex::new(SharedCatalogState::from_cache(cached)),
			extension: Mutex::new(ExtensionDiscoveryState::default()),
		})
	}

	/// Reports picker/call eligibility. Explicit disable is the only discovery
	/// state that erases a configured declaration; missing or failed discovery
	/// remains selectable.
	pub fn provider_selectable(&self, provider: &omp_catalog::ProviderId<str>) -> bool {
		!self.disabled.contains(provider)
	}

	/// Returns the HTTP client shared with native endpoint discovery probes.
	pub fn http_client(&self) -> Arc<dyn DiscoveryHttpClient> {
		self.http.clone()
	}

	/// Returns the current shared-catalog source and revalidation state without
	/// scheduling network work.
	pub async fn shared_catalog_status(&self) -> SharedCatalogRefreshOutcome {
		self.shared.lock().await.outcome
	}

	/// Materializes the latest immutable overlay generation over the exact base
	/// catalog retained by this daemon authority.
	pub fn catalog(&self) -> Result<Arc<snapshot::Catalog>, DiscoveryRuntimeError> {
		self
			.base
			.with_overlay_stack(&self.overlays.load(), UnsafeTrustScope::ALL)
			.map(Arc::new)
			.map_err(DiscoveryRuntimeError::Catalog)
	}

	/// Starts one detached startup refresh. The transport owns its hard
	/// deadline, while this subscriber may stop waiting independently.
	pub fn spawn_shared_catalog_refresh(
		self: &Arc<Self>,
		cancellation: CancellationToken,
	) -> JoinHandle<Result<SharedCatalogRefreshOutcome, DiscoveryRuntimeError>> {
		let runtime = Arc::clone(self);
		tokio::spawn(async move {
			runtime
				.refresh_shared_catalog(SHARED_CATALOG_BACKGROUND_SUBSCRIBER_DEADLINE, cancellation)
				.await
		})
	}

	/// Conditionally refreshes the shared models.dev-style catalog.
	///
	/// Concurrent subscribers join one transport request. A subscriber timeout
	/// or cancellation never aborts that shared transport; the transport's own
	/// hard deadline remains authoritative.
	pub async fn refresh_shared_catalog(
		self: &Arc<Self>,
		subscriber_deadline: Duration,
		cancellation: CancellationToken,
	) -> Result<SharedCatalogRefreshOutcome, DiscoveryRuntimeError> {
		let now = Instant::now();
		let receiver = {
			let mut state = self.shared.lock().await;
			if let Some(receiver) = state.inflight.as_ref() {
				receiver.clone()
			} else {
				let interval = if state.outcome.stale {
					SHARED_CATALOG_RETRY_INTERVAL
				} else {
					SHARED_CATALOG_REFRESH_INTERVAL
				};
				if state
					.last_attempt
					.is_some_and(|last| now.saturating_duration_since(last) < interval)
				{
					return Ok(state.outcome);
				}
				let (sender, receiver) = watch::channel(None);
				state.last_attempt = Some(now);
				state.inflight = Some(receiver.clone());
				let runtime = Arc::clone(self);
				let _task = tokio::spawn(async move {
					let result = Arc::clone(&runtime)
						.perform_shared_catalog_refresh(unix_time_ms())
						.await;
					runtime.shared.lock().await.inflight = None;
					let _ = sender.send(Some(result));
				});
				receiver
			}
		};
		wait_for_shared_catalog(receiver, subscriber_deadline, cancellation).await
	}

	/// Runs one inference-routed pass across the registry's discoverable routes,
	/// deduplicating route work through the daemon gate and publishing complete
	/// provider slices only when every route for that provider succeeds.
	pub async fn refresh_registry_discovery(
		&self,
		registry: &Registry,
		subscriber_deadline: Duration,
		cancellation: CancellationToken,
	) -> Result<RegistryDiscoveryRefreshOutcome, DiscoveryRuntimeError> {
		let catalog = registry.catalog();
		let mut providers = BTreeMap::<ProviderId, Vec<_>>::new();
		for route in catalog
			.routes()
			.iter()
			.filter(|route| route.discovery.is_some())
		{
			let spec_id = route.discovery.clone().expect("filtered discovery route");
			let Some(spec) = catalog.discovery_spec(&spec_id) else {
				continue;
			};
			providers.entry(route.provider.clone()).or_default().push((
				route.id.clone(),
				spec_id,
				spec.clone(),
			));
		}
		let now = Instant::now();
		let now_ms = unix_time_ms();
		let mut outcome = RegistryDiscoveryRefreshOutcome::default();
		for (provider, routes) in providers {
			if self.disabled.contains(&provider) {
				outcome.skipped = outcome.skipped.saturating_add(routes.len());
				continue;
			}
			let mut builder = CatalogOverlayBuilder::new(ProvenanceSource {
				kind:           ProvenanceKind::Discovered,
				origin:         sf!("registry-discovery:{}", provider),
				revision:       None,
				confidence:     EvidenceConfidence::Verified,
				observed_at_ms: Some(now_ms),
			});
			let mut rows = Vec::new();
			let mut provider_failed = false;
			for (route, spec_id, spec) in routes {
				let key = DiscoveryPollKey {
					provider: provider.clone(),
					route:    route.clone(),
					spec:     spec_id,
				};
				if spec.polling_interval().is_some()
					&& !self.gate.claim_interval(key.clone(), &spec, now)
				{
					outcome.skipped = outcome.skipped.saturating_add(1);
					continue;
				}
				if !self.route_refreshes.lock().insert(key.clone()) {
					outcome.skipped = outcome.skipped.saturating_add(1);
					continue;
				}
				self.cache.set_lifecycle(&ProviderLifecycle {
					provider:       provider.clone(),
					state:          ProviderDiscoveryState::Probing,
					error_code:     None,
					observed_at_ms: now_ms,
					retry_at_ms:    None,
				})?;
				let planner = router::Router::new(registry.clone(), subscriber_deadline);
				let meta = CallMeta {
					id:             RequestId::from(format!(
						"runtime-model-refresh-{}-{}",
						provider.as_str(),
						route.as_str()
					)),
					target:         Target::ProviderService(provider.clone()),
					deadline:       None,
					budget:         ExecutionBudget::default(),
					session:        None,
					response_hooks: Default::default(),
				};
				let mut cursor = None;
				let mut route_failed = false;
				loop {
					let mut client = Client::new(registry.service(), planner.clone(), meta.clone());
					let request = client
						.execute(DiscoveryRequest {
							provider:  Some(provider.clone()),
							route:     Some(route.clone()),
							cursor:    cursor.clone(),
							page_size: 500,
							operation: None,
						});
					let page = tokio::select! {
						_ = cancellation.cancelled() => None,
						result = time::timeout(subscriber_deadline, request) => {
							result.ok().and_then(Result::ok)
						},
					};
					let Some(page) = page else {
						route_failed = true;
						break;
					};
					for model in page.models {
						if let Some(row) = registry_discovered_model(&model, &provider, &route, now_ms) {
							rows.push(row);
						}
						builder = builder.with_model(ModelOverlay {
							selector: omp_catalog::ExactSelector::new(provider.clone(), model.key.clone()),
							added:    Some(model),
							patch:    ModelPatch::default(),
						});
					}
					cursor = page.next_cursor;
					if cursor.is_none() {
						break;
					}
				}
				self.route_refreshes.lock().remove(&key);
				if route_failed {
					self.gate.release(&key);
					provider_failed = true;
					outcome.failures = outcome.failures.saturating_add(1);
					self.cache.set_lifecycle(&ProviderLifecycle {
						provider:       provider.clone(),
						state:          ProviderDiscoveryState::Failed,
						error_code:     Some(Str::new_static("inference-discovery")),
						observed_at_ms: now_ms,
						retry_at_ms:    Some(now_ms.saturating_add(5_000)),
					})?;
				}
			}
			if provider_failed || rows.is_empty() {
				continue;
			}
			self.cache.publish(
				&DiscoveryCacheKey::provider(provider.clone()),
				&rows,
				now_ms,
				Duration::from_secs(24 * 60 * 60),
			)?;
			self
				.live
				.lock()
				.await
				.providers
				.insert(provider.clone(), builder.build());
			self.cache.set_lifecycle(&ProviderLifecycle {
				provider,
				state: ProviderDiscoveryState::Ready,
				error_code: None,
				observed_at_ms: now_ms,
				retry_at_ms: None,
			})?;
			outcome.models = outcome.models.saturating_add(rows.len());
		}
		self.publish_live_discovery(now_ms).await;
		Ok(outcome)
	}

	async fn perform_shared_catalog_refresh(self: Arc<Self>, now_ms: u64) -> SharedCatalogResult {
		let etag = self.shared.lock().await.etag.clone();
		let response = match self
			.http
			.fetch_shared_catalog(etag.as_deref(), CancellationToken::new())
			.await
		{
			Ok(response) => response,
			Err(error) => {
				let kind = match error {
					SharedCatalogFetchError::Cancelled => SharedCatalogFailureKind::Cancelled,
					SharedCatalogFetchError::Timeout => SharedCatalogFailureKind::Timeout,
					SharedCatalogFetchError::Transport(_) => SharedCatalogFailureKind::Transport,
					SharedCatalogFetchError::Status { .. } => SharedCatalogFailureKind::Status,
				};
				return Ok(self.record_shared_catalog_failure(kind, now_ms).await);
			},
		};
		if matches!(&response, SharedCatalogHttpResponse::NotModified) {
			let mut state = self.shared.lock().await;
			state.outcome = SharedCatalogRefreshOutcome {
				source:                 SharedCatalogSource::Remote,
				stale:                  false,
				models:                 state
					.cache_overlay
					.as_ref()
					.map_or(0, CatalogOverlay::model_count),
				updated_at_ms:          Some(now_ms),
				revalidation_failed_ms: None,
				revalidation_failure:   None,
			};
			return Ok(state.outcome);
		}
		let SharedCatalogHttpResponse::Payload { body, etag } = response else {
			unreachable!("not-modified handled above")
		};
		let base = Arc::clone(&self.base);
		let overlay = match tokio::task::spawn_blocking(move || {
			base.additive_shared_catalog_overlay(&body, now_ms)
		})
		.await
		{
			Ok(Ok(overlay)) => overlay,
			Ok(Err(_error)) => {
				return Ok(self
					.record_shared_catalog_failure(SharedCatalogFailureKind::Compile, now_ms)
					.await);
			},
			Err(source) => {
				return Err(Arc::new(SharedCatalogRefreshError::Worker(source)));
			},
		};
		let cache_path = self.overlay_cache.clone();
		let cached = overlay.clone();
		match tokio::task::spawn_blocking(move || {
			snapshot::write_discovery_overlay_cache(&cache_path, &cached)
		})
		.await
		{
			Ok(Ok(())) => {},
			Ok(Err(source)) => {
				return Err(Arc::new(SharedCatalogRefreshError::Cache(source)));
			},
			Err(source) => {
				return Err(Arc::new(SharedCatalogRefreshError::Worker(source)));
			},
		}
		*self.shared_cache.lock() = Some(overlay.clone());
		self.publish_disk_cache(now_ms);
		{
			let mut live = self.live.lock().await;
			live.shared = Some(overlay.clone());
		}
		self.publish_live_discovery(now_ms).await;
		let mut state = self.shared.lock().await;
		state.etag = etag;
		state.cache_overlay = Some(overlay.clone());
		state.outcome = SharedCatalogRefreshOutcome {
			source:                 SharedCatalogSource::Remote,
			stale:                  false,
			models:                 overlay.model_count(),
			updated_at_ms:          Some(now_ms),
			revalidation_failed_ms: None,
			revalidation_failure:   None,
		};
		Ok(state.outcome)
	}

	async fn record_shared_catalog_failure(
		&self,
		kind: SharedCatalogFailureKind,
		now_ms: u64,
	) -> SharedCatalogRefreshOutcome {
		let mut state = self.shared.lock().await;
		let models = state
			.cache_overlay
			.as_ref()
			.map_or(0, CatalogOverlay::model_count);
		state.outcome = SharedCatalogRefreshOutcome {
			source: if models == 0 {
				SharedCatalogSource::Bundled
			} else {
				SharedCatalogSource::DiskCache
			},
			stale: true,
			models,
			updated_at_ms: if models == 0 {
				None
			} else {
				state.outcome.updated_at_ms
			},
			revalidation_failed_ms: Some(now_ms),
			revalidation_failure: Some(kind),
		};
		state.outcome
	}

	fn publish_disk_cache(&self, now_ms: u64) {
		let shared = self.shared_cache.lock().clone();
		let endpoint = self.disk_endpoint.lock().clone();
		let overlay = CatalogOverlay::combined(
			ProvenanceSource {
				kind:           ProvenanceKind::Discovered,
				origin:         Str::new_static("discovery:disk-cache"),
				revision:       None,
				confidence:     EvidenceConfidence::Verified,
				observed_at_ms: Some(now_ms),
			},
			shared.into_iter().chain(endpoint),
		);
		self.overlays.replace(OverlaySource::DiskCache, overlay);
	}

	async fn publish_live_discovery(&self, now_ms: u64) {
		let state = self.live.lock().await;
		let overlay = CatalogOverlay::combined(
			ProvenanceSource {
				kind:           ProvenanceKind::Discovered,
				origin:         Str::new_static("discovery:runtime"),
				revision:       None,
				confidence:     EvidenceConfidence::Verified,
				observed_at_ms: Some(now_ms),
			},
			state
				.shared
				.clone()
				.into_iter()
				.chain(state.providers.values().cloned()),
		);
		self.overlays.replace(OverlaySource::Discovery, overlay);
	}

	/// Hydrates exact provider/account cache namespaces without network access.
	///
	/// Callers pass the current opaque credential affinities and current route
	/// normalizers. Repeating this pass replaces the disk-cache layer, so a
	/// credential change is observed rather than hidden behind a process-wide
	/// once guard.
	pub fn hydrate_cached(
		&self,
		requests: &[CachedDiscoveryHydration],
		now_ms: u64,
	) -> Result<usize, DiscoveryRuntimeError> {
		let source = ProvenanceSource {
			kind:           ProvenanceKind::Discovered,
			origin:         sf!("discovery:disk-cache"),
			revision:       None,
			confidence:     EvidenceConfidence::Verified,
			observed_at_ms: Some(now_ms),
		};
		let mut builder = CatalogOverlayBuilder::new(source);
		let mut hydrated = 0;
		for request in requests {
			if self.disabled.contains(&request.key.provider) {
				continue;
			}
			let Some(cached) = self.cache.load_fresh(&request.key, now_ms)? else {
				continue;
			};
			let normalized = request
				.normalizer
				.normalize_batch(&cached.rows)
				.map_err(DiscoveryRuntimeError::Normalize)?;
			hydrated += normalized.len();
			for item in normalized {
				let selector =
					omp_catalog::ExactSelector::new(item.provider.clone(), item.model.key.clone());
				builder = builder.with_model(ModelOverlay {
					selector,
					added: Some(item.model),
					patch: ModelPatch::default(),
				});
				builder = builder.with_aliases(
					item
						.aliases
						.into_vec()
						.into_iter()
						.map(|definition| ScopedAlias { provider: item.provider.clone(), definition }),
				);
			}
		}
		*self.disk_endpoint.lock() = Some(builder.build());
		self.publish_disk_cache(now_ms);
		Ok(hydrated)
	}

	/// Runs one extension `models_discover` page and publishes it under a
	/// provider-scoped generation fence.
	///
	/// Hook failure and malformed rows retain the prior extension overlay.
	/// A newer in-flight generation suppresses this invocation's publication.
	pub async fn refresh_extension(
		&self,
		hooks: &ProviderResponseHooks,
		request: ModelsDiscoverHookRequest,
		normalizer: &DiscoveryNormalizer,
		now_ms: u64,
	) -> Result<RefreshOutcome, DiscoveryRuntimeError> {
		if self.disabled.contains(&request.provider) {
			return Ok(RefreshOutcome::Disabled);
		}
		if !hooks.models_discover_subscribed(&request.provider) {
			return Ok(RefreshOutcome::NotDue);
		}
		let generation = {
			let mut state = self.extension.lock().await;
			let generation = state
				.generations
				.entry(request.provider.clone())
				.or_default();
			*generation = generation.saturating_add(1);
			*generation
		};
		let provider = request.provider.clone();
		let route = request.route.clone();
		let page = match hooks.models_discover(request).await {
			Ok(page) => page,
			Err(_) => return Ok(RefreshOutcome::Retained),
		};
		let mut state = self.extension.lock().await;
		if state.generations.get(&provider).copied() != Some(generation) {
			return Ok(RefreshOutcome::Superseded);
		}
		let mut next = if page.authoritative {
			BTreeMap::new()
		} else {
			state.models.get(&provider).cloned().unwrap_or_default()
		};
		for model in page.models {
			let Some(id) = model
				.get("id")
				.and_then(Value::as_str)
				.filter(|id| !id.is_empty())
			else {
				return Ok(RefreshOutcome::Retained);
			};
			next.insert(Str::new(id), model);
		}
		let rows = next
			.values()
			.map(|model| discovered_hook_model(&provider, &route, model, now_ms))
			.collect::<Option<Vec<_>>>();
		let Some(rows) = rows else {
			return Ok(RefreshOutcome::Retained);
		};
		let normalized = match normalizer.normalize_batch(&rows) {
			Ok(normalized) => normalized,
			Err(_) => return Ok(RefreshOutcome::Retained),
		};
		let source = ProvenanceSource {
			kind:           ProvenanceKind::Discovered,
			origin:         sf!("models-discover:{}", provider),
			revision:       None,
			confidence:     EvidenceConfidence::Declared,
			observed_at_ms: Some(now_ms),
		};
		let mut builder = CatalogOverlayBuilder::new(source);
		for item in normalized {
			let selector =
				omp_catalog::ExactSelector::new(item.provider.clone(), item.model.key.clone());
			builder = builder.with_model(ModelOverlay {
				selector,
				added: Some(item.model),
				patch: ModelPatch::default(),
			});
			builder = builder.with_aliases(
				item
					.aliases
					.into_vec()
					.into_iter()
					.map(|definition| ScopedAlias { provider: item.provider.clone(), definition }),
			);
		}
		let models = next.len();
		self.overlays.replace(
			OverlaySource::Extension { id: sf!("models-discover:{}", provider) },
			builder.build(),
		);
		state.models.insert(provider, next);
		Ok(RefreshOutcome::Published { models })
	}

	/// Runs one due probe, writes its complete SQLite generation, and atomically
	/// publishes a credential-blind discovery overlay.
	pub async fn refresh(
		&self,
		key: DiscoveryPollKey,
		cache_key: &DiscoveryCacheKey,
		spec: &DiscoverySpec,
		probe: &DiscoveryProbe,
		normalizer: &DiscoveryNormalizer,
		client: &dyn DiscoveryHttpClient,
		now: Instant,
		now_ms: u64,
		ttl: Duration,
		cancellation: CancellationToken,
	) -> Result<RefreshOutcome, DiscoveryRuntimeError> {
		if self.disabled.contains(&key.provider) {
			return Ok(RefreshOutcome::Disabled);
		}
		if cache_key.provider != key.provider {
			return Err(DiscoveryRuntimeError::CacheScopeMismatch {
				poll:  key.provider,
				cache: cache_key.provider.clone(),
			});
		}
		if !self.gate.claim_interval(key.clone(), spec, now) {
			return Ok(RefreshOutcome::NotDue);
		}
		self.cache.set_lifecycle(&ProviderLifecycle {
			provider:       key.provider.clone(),
			state:          ProviderDiscoveryState::Probing,
			error_code:     None,
			observed_at_ms: now_ms,
			retry_at_ms:    None,
		})?;
		let rows = match probe.probe(client, cancellation).await {
			Ok(rows) => rows,
			Err(error) => {
				self.gate.release(&key);
				self.cache.set_lifecycle(&ProviderLifecycle {
					provider:       key.provider,
					state:          ProviderDiscoveryState::Failed,
					error_code:     Some(probe_error_code(error)),
					observed_at_ms: now_ms,
					retry_at_ms:    Some(now_ms.saturating_add(5_000)),
				})?;
				return Err(DiscoveryRuntimeError::Probe(error));
			},
		};
		let normalized = normalizer
			.normalize_batch(&rows)
			.map_err(DiscoveryRuntimeError::Normalize)?;
		let source = ProvenanceSource {
			kind:           ProvenanceKind::Discovered,
			origin:         sf!("discovery:{}", key.provider),
			revision:       None,
			confidence:     EvidenceConfidence::Verified,
			observed_at_ms: Some(now_ms),
		};
		let mut builder = CatalogOverlayBuilder::new(source);
		for item in normalized {
			let selector =
				omp_catalog::ExactSelector::new(item.provider.clone(), item.model.key.clone());
			builder = builder.with_model(ModelOverlay {
				selector,
				added: Some(item.model),
				patch: ModelPatch::default(),
			});
			builder = builder.with_aliases(
				item
					.aliases
					.into_vec()
					.into_iter()
					.map(|definition| ScopedAlias { provider: item.provider.clone(), definition }),
			);
		}
		self.cache.publish(cache_key, &rows, now_ms, ttl)?;
		self
			.live
			.lock()
			.await
			.providers
			.insert(key.provider, builder.build());
		self.publish_live_discovery(now_ms).await;
		Ok(RefreshOutcome::Published { models: rows.len() })
	}
}

async fn wait_for_shared_catalog(
	mut receiver: watch::Receiver<Option<SharedCatalogResult>>,
	deadline: Duration,
	cancellation: CancellationToken,
) -> Result<SharedCatalogRefreshOutcome, DiscoveryRuntimeError> {
	let wait = async {
		loop {
			if let Some(result) = receiver.borrow().clone() {
				return result.map_err(DiscoveryRuntimeError::SharedCatalog);
			}
			receiver
				.changed()
				.await
				.map_err(|_| DiscoveryRuntimeError::SharedCatalogCoordination)?;
		}
	};
	tokio::select! {
		_ = cancellation.cancelled() => Err(DiscoveryRuntimeError::SubscriberCancelled),
		result = time::timeout(deadline, wait) => {
			result.map_err(|_| DiscoveryRuntimeError::SubscriberDeadline)?
		},
	}
}

fn unix_time_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

fn registry_discovered_model(
	model: &ModelSpec,
	provider: &ProviderId<str>,
	route: &omp_catalog::RouteId<str>,
	now_ms: u64,
) -> Option<DiscoveredModel> {
	let wire_model = model
		.wire_ids
		.iter()
		.find_map(|(candidate, wire)| (candidate == route).then(|| wire.clone()))?;
	Some(DiscoveredModel {
		provider: provider.to_owned(),
		route: route.to_owned(),
		wire_model,
		aliases: Box::new([]),
		display_name: Some(model.display_name.clone()),
		declared_class: Some(model.class.clone()),
		declared_operations: OperationBits::empty(),
		declared_capabilities: Some(model.capabilities.clone()),
		declared_limits: Some(model.limits),
		declared_pricing: Box::new([]),
		extended_context_mode: None,
		availability: Some(model.availability),
		source: Str::new_static("runtime-inference-discovery"),
		observed_at_ms: Some(now_ms),
		updated_at_ms: model.provenance.updated_at_ms,
		deprecated: Some(model.provenance.deprecated),
	})
}

/// One exact local-only cache hydration request.
#[derive(Clone)]
pub struct CachedDiscoveryHydration {
	/// Provider plus optional opaque credential affinity.
	pub key:        DiscoveryCacheKey,
	/// Current route-bound normalizer; current route auth/header configuration
	/// remains authoritative and is never read from SQLite.
	pub normalizer: DiscoveryNormalizer,
}

fn discovered_hook_model(
	provider: &ProviderId<str>,
	route: &omp_catalog::RouteId<str>,
	model: &Value,
	now_ms: u64,
) -> Option<DiscoveredModel> {
	let object = model.as_object()?;
	let id = object.get("id")?.as_str().filter(|id| !id.is_empty())?;
	let wire_model = object
		.get("wire_ids")
		.and_then(Value::as_object)
		.and_then(|wire_ids| {
			wire_ids.get(route.as_str()).or_else(|| {
				route
					.as_str()
					.rsplit('/')
					.next()
					.and_then(|id| wire_ids.get(id))
			})
		})
		.and_then(Value::as_str)
		.unwrap_or(id);
	let mut operations = OperationBits::empty();
	for operation in object
		.get("operations")
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter_map(Value::as_str)
		.filter_map(|operation| operation.parse::<OperationKind>().ok())
	{
		operations.insert_kind(operation);
	}
	if operations.is_empty() {
		operations.insert_kind(OperationKind::Chat);
	}
	let limits = ModelLimits {
		context_window:        object.get("context_window").and_then(Value::as_u64),
		maximum_input_tokens:  object.get("max_input_tokens").and_then(Value::as_u64),
		maximum_output_tokens: object.get("max_output_tokens").and_then(Value::as_u64),
		maximum_batch:         object
			.get("max_batch")
			.and_then(Value::as_u64)
			.and_then(|value| value.try_into().ok()),
	};
	Some(DiscoveredModel {
		provider:              provider.to_owned(),
		route:                 route.to_owned(),
		wire_model:            WireModelId::from(wire_model),
		aliases:               Box::new([]),
		display_name:          object
			.get("display_name")
			.and_then(Value::as_str)
			.map(Str::new),
		declared_class:        object
			.get("family")
			.and_then(Value::as_str)
			.map(ClassId::from),
		declared_operations:   operations,
		declared_capabilities: None,
		declared_limits:       Some(limits),
		declared_pricing:      Box::new([]),
		extended_context_mode: None,
		availability:          object
			.get("availability")
			.and_then(Value::as_str)
			.and_then(|value| value.parse().ok()),
		source:                sf!("models-discover:{}", provider),
		observed_at_ms:        Some(now_ms),
		updated_at_ms:         object.get("updated_at_ms").and_then(Value::as_u64),
		deprecated:            object.get("deprecated").and_then(Value::as_bool),
	})
}

fn probe_error_code(error: ProbeError) -> Str {
	match error {
		ProbeError::Timeout => Str::new_static("timeout"),
		ProbeError::Cancelled => Str::new_static("cancelled"),
		ProbeError::Transport => Str::new_static("transport"),
		ProbeError::Protocol => Str::new_static("protocol"),
	}
}

/// Result of a refresh scheduling attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshOutcome {
	/// Explicit disabled-provider policy won.
	Disabled,
	/// Another session/process-local caller owns the interval or no extension
	/// handler applies.
	NotDue,
	/// The hook failed open and the prior published rows were retained.
	Retained,
	/// A newer provider generation completed or claimed publication first.
	Superseded,
	/// A complete generation was published.
	Published {
		/// Number of normalized model rows.
		models: usize,
	},
}

/// Discovery orchestration failure.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryRuntimeError {
	/// Cache namespace belongs to another provider.
	#[error("discovery cache provider {cache} does not match poll provider {poll}")]
	CacheScopeMismatch {
		/// Scheduled poll provider.
		poll:  ProviderId,
		/// Supplied cache provider.
		cache: ProviderId,
	},
	/// Endpoint probing failed.
	#[error(transparent)]
	Probe(#[from] ProbeError),
	/// Discovery normalization failed.
	#[error(transparent)]
	Normalize(#[from] omp_catalog::DiscoveryError),
	/// SQLite publication failed.
	#[error(transparent)]
	Store(#[from] DiscoveryStoreError),
	/// Persisted shared-catalog overlay loading failed.
	#[error(transparent)]
	OverlayCache(#[from] snapshot::OverlayCacheError),
	/// Persisted shared-catalog rows failed current additive admission.
	#[error(transparent)]
	SharedCatalogAdmission(#[from] snapshot::SharedCatalogError),
	/// Current overlays could not materialize into an immutable catalog.
	#[error("runtime catalog materialization failed")]
	Catalog(#[source] snapshot::SnapshotError),
	/// Shared-catalog compilation or persistence failed.
	#[error("shared catalog refresh failed")]
	SharedCatalog(#[source] Arc<SharedCatalogRefreshError>),
	/// The shared refresh task ended without publishing a result.
	#[error("shared catalog refresh coordination ended unexpectedly")]
	SharedCatalogCoordination,
	/// This subscriber stopped waiting without cancelling the shared transport.
	#[error("shared catalog subscriber was cancelled")]
	SubscriberCancelled,
	/// This subscriber's deadline elapsed without cancelling the shared
	/// transport.
	#[error("shared catalog subscriber deadline elapsed")]
	SubscriberDeadline,
}

/// Server-side bridge from the gateway protocol to the one authoritative
/// provider application owner.
pub struct LocalProviderGatewayAuthority {
	owner:   Arc<ProductionProviderApplicationOwner>,
	backend: ChatProviderControlBackend,
	serial:  Mutex<()>,
}

/// Publishes a local provider owner on `InferenceRpc`.
pub fn gateway_provider_rpc_authority(
	owner: Arc<ProductionProviderApplicationOwner>,
) -> Arc<dyn omp_serve::inference::ProviderGatewayAuthority> {
	Arc::new(LocalProviderGatewayAuthority {
		backend: ChatProviderControlBackend::new(owner.clone()),
		owner,
		serial: Mutex::new(()),
	})
}

impl LocalProviderGatewayAuthority {
	fn cursor(&self) -> pb::Cursor {
		use crate::chat::ProviderApplicationOwner as _;
		let registry = self.owner.registry();
		pb::Cursor {
			epoch:      registry
				.catalog_revision()
				.as_str()
				.as_bytes()
				.to_vec()
				.into(),
			generation: registry.generation(),
		}
	}

	fn caller(caller: Option<pb::ProviderCaller>) -> Result<ControlConnectionIdentity, Status> {
		let caller = caller.ok_or_else(|| Status::unauthenticated("provider caller is required"))?;
		if caller.extension.is_empty()
			|| caller.artifact_digest.is_empty()
			|| caller.principal_id.is_empty()
			|| caller.layer.is_empty()
			|| caller.tier.is_empty()
			|| caller.trust.is_empty()
			|| caller.host_generation == 0
			|| caller.session_generation == 0
		{
			return Err(Status::unauthenticated("provider caller identity is incomplete"));
		}
		Ok(ControlConnectionIdentity {
			extension:          caller.extension.into(),
			principal:          Principal::new(
				caller.principal_id.into(),
				caller.principal_display.into(),
			),
			artifact_digest:    caller.artifact_digest.into(),
			layer:              caller.layer.into(),
			tier:               caller.tier.into(),
			trust:              caller.trust.into(),
			host_generation:    caller.host_generation,
			session_generation: caller.session_generation,
			capabilities:       Arc::new(caller.capabilities.into_iter().map(Str::from).collect()),
		})
	}

	fn check_generation(&self, expected: u64) -> Result<(), Status> {
		use crate::chat::ProviderApplicationOwner as _;
		if self.owner.registry().generation() == expected {
			Ok(())
		} else {
			Err(Status::aborted("provider catalog generation is stale"))
		}
	}

	fn declaration(
		request: pb::ProviderDeclarationRequest,
	) -> Result<(ControlConnectionIdentity, ProviderDeclarationDocument, u64), Status> {
		if request.provider.is_empty() {
			return Err(Status::invalid_argument("provider is required"));
		}
		let caller = Self::caller(request.caller)?;
		let document = serde_json::from_slice(&request.document_json)
			.map_err(|error| Status::invalid_argument(format!("provider declaration: {error}")))?;
		Ok((
			caller,
			ProviderDeclarationDocument { provider: request.provider.into(), document },
			request.expected_generation,
		))
	}
}

#[async_trait]
impl omp_serve::inference::ProviderGatewayAuthority for LocalProviderGatewayAuthority {
	async fn catalog(
		&self,
		request: pb::ProviderCatalogRequest,
	) -> Result<pb::ProviderCatalogResponse, Status> {
		use crate::model_controls::ProviderControlBackend as _;
		let models = self
			.backend
			.models(request.provider.as_deref())
			.await
			.map_err(provider_status)?
			.into_iter()
			.map(provider_model_to_pb)
			.collect();
		Ok(pb::ProviderCatalogResponse { models, cursor: Some(self.cursor()) })
	}

	async fn watch_catalog(
		&self,
		request: pb::WatchProviderCatalogRequest,
	) -> Result<pb::WatchProviderCatalogResponse, Status> {
		use crate::model_controls::ProviderControlBackend as _;
		let since = request.since.map(|cursor| ProviderCatalogCursor {
			epoch:      cursor.epoch.to_vec().into_boxed_slice(),
			generation: cursor.generation,
		});
		let events = self
			.backend
			.watch_models(since)
			.await
			.map_err(provider_status)?
			.into_iter()
			.map(provider_event_to_pb)
			.collect();
		Ok(pb::WatchProviderCatalogResponse { events })
	}

	async fn authenticated(
		&self,
		request: pb::ProviderAuthenticatedRequest,
	) -> Result<pb::ProviderAuthenticatedResponse, Status> {
		use crate::model_controls::ProviderControlBackend as _;
		if request.provider.is_empty() {
			return Err(Status::invalid_argument("provider is required"));
		}
		let authenticated = self
			.backend
			.is_authenticated(&request.provider)
			.await
			.map_err(provider_status)?;
		Ok(pb::ProviderAuthenticatedResponse { authenticated })
	}

	async fn declare(
		&self,
		request: pb::ProviderDeclarationRequest,
	) -> Result<pb::ProviderMutationResponse, Status> {
		let (caller, declaration, generation) = Self::declaration(request)?;
		let _guard = self.serial.lock().await;
		self.check_generation(generation)?;
		self
			.owner
			.declare_provider(&caller, declaration)
			.map_err(provider_status)?;
		Ok(pb::ProviderMutationResponse { cursor: Some(self.cursor()) })
	}

	async fn replace(
		&self,
		request: pb::ProviderDeclarationRequest,
	) -> Result<pb::ProviderMutationResponse, Status> {
		use crate::chat::ProviderApplicationOwner as _;
		let (caller, declaration, generation) = Self::declaration(request)?;
		let _guard = self.serial.lock().await;
		self.check_generation(generation)?;
		self
			.owner
			.replace_provider(&caller, declaration)
			.await
			.map_err(provider_status)?;
		Ok(pb::ProviderMutationResponse { cursor: Some(self.cursor()) })
	}

	async fn retract(
		&self,
		request: pb::RetractProviderRequest,
	) -> Result<pb::ProviderMutationResponse, Status> {
		use crate::chat::ProviderApplicationOwner as _;
		if request.provider.is_empty() {
			return Err(Status::invalid_argument("provider is required"));
		}
		let caller = Self::caller(request.caller)?;
		let _guard = self.serial.lock().await;
		self.check_generation(request.expected_generation)?;
		self
			.owner
			.retract_provider(&caller, &request.provider)
			.await
			.map_err(provider_status)?;
		Ok(pb::ProviderMutationResponse { cursor: Some(self.cursor()) })
	}

	async fn request(
		&self,
		request: pb::ProviderOperationRequest,
	) -> Result<pb::ProviderOperationResponse, Status> {
		self.execute(request, false).await
	}

	async fn mint_session(
		&self,
		request: pb::ProviderOperationRequest,
	) -> Result<pb::ProviderOperationResponse, Status> {
		self.execute(request, true).await
	}
}

impl LocalProviderGatewayAuthority {
	async fn execute(
		&self,
		request: pb::ProviderOperationRequest,
		session_only: bool,
	) -> Result<pb::ProviderOperationResponse, Status> {
		use crate::chat::ProviderApplicationOwner as _;
		let caller = Self::caller(request.caller)?;
		if request.provider.is_empty() {
			return Err(Status::invalid_argument("provider is required"));
		}
		let operation = provider_kind_from_pb(request.kind)?;
		if session_only && operation != ProviderRequestKind::Realtime {
			return Err(Status::invalid_argument(
				"MintProviderSession requires a realtime provider operation",
			));
		}
		if !session_only && operation == ProviderRequestKind::Realtime {
			return Err(Status::invalid_argument("realtime endpoints must use MintProviderSession"));
		}
		let payload: Map<String, Value> = serde_json::from_slice(&request.payload_json)
			.map_err(|error| Status::invalid_argument(format!("provider payload: {error}")))?;
		let _guard = self.serial.lock().await;
		self.check_generation(request.expected_generation)?;
		let result = self
			.owner
			.provider_request(&caller, ProviderControlRequest {
				provider: request.provider.into(),
				operation,
				payload,
			})
			.await
			.map_err(provider_status)?;
		Ok(provider_result_to_pb(result))
	}
}

/// Authenticated gateway adapter implementing the same provider backend used by
/// local CONTROL composition. It never constructs or mutates a local registry.
#[derive(Clone)]
pub struct RemoteProviderControlBackend {
	channel: Channel,
}

impl RemoteProviderControlBackend {
	/// Creates a backend that forwards provider CONTROL calls over this gateway
	/// channel.
	pub fn new(channel: Channel) -> Self {
		Self { channel }
	}

	fn client(&self) -> InferenceClient<Channel> {
		InferenceClient::new(self.channel.clone())
	}

	async fn generation(&self) -> Result<u64, ProviderControlError> {
		let response = self
			.client()
			.provider_catalog(pb::ProviderCatalogRequest { provider: None })
			.await
			.map_err(provider_error)?
			.into_inner();
		response
			.cursor
			.map(|cursor| cursor.generation)
			.ok_or_else(|| {
				ProviderControlError::Request(sf!("gateway omitted provider catalog cursor"))
			})
	}
}

/// Constructs the connection-scoped provider CONTROL factory used in gateway
/// mode.
pub fn gateway_provider_control_factory(channel: Channel) -> Arc<dyn ControlAuthorityFactory> {
	let backend: Arc<dyn ProviderControlBackend> =
		Arc::new(RemoteProviderControlBackend::new(channel));
	Arc::new(ProviderControlAuthorityFactory::new(backend))
}

#[async_trait]
impl ProviderControlBackend for RemoteProviderControlBackend {
	async fn models(
		&self,
		provider: Option<&str>,
	) -> Result<Vec<ProviderModelCard>, ProviderControlError> {
		let response = self
			.client()
			.provider_catalog(pb::ProviderCatalogRequest { provider: provider.map(ToOwned::to_owned) })
			.await
			.map_err(provider_error)?
			.into_inner();
		Ok(response
			.models
			.into_iter()
			.map(provider_model_from_pb)
			.collect())
	}

	async fn watch_models(
		&self,
		since: Option<ProviderCatalogCursor>,
	) -> Result<Vec<ProviderModelEvent>, ProviderControlError> {
		let response = self
			.client()
			.watch_provider_catalog(pb::WatchProviderCatalogRequest {
				since: since.map(|cursor| pb::Cursor {
					epoch:      cursor.epoch.to_vec().into(),
					generation: cursor.generation,
				}),
			})
			.await
			.map_err(provider_error)?
			.into_inner();
		response
			.events
			.into_iter()
			.map(provider_event_from_pb)
			.collect()
	}

	async fn is_authenticated(&self, provider: &str) -> Result<bool, ProviderControlError> {
		Ok(self
			.client()
			.provider_authenticated(pb::ProviderAuthenticatedRequest { provider: provider.to_owned() })
			.await
			.map_err(provider_error)?
			.into_inner()
			.authenticated)
	}

	async fn replace(
		&self,
		identity: &ControlConnectionIdentity,
		declaration: ProviderDeclarationDocument,
	) -> Result<(), ProviderControlError> {
		let generation = self.generation().await?;
		let request = declaration_request(identity, declaration, generation)?;
		self
			.client()
			.replace_provider(request)
			.await
			.map_err(provider_error)?;
		Ok(())
	}

	async fn retract(
		&self,
		identity: &ControlConnectionIdentity,
		provider: &str,
	) -> Result<(), ProviderControlError> {
		let generation = self.generation().await?;
		self
			.client()
			.retract_provider(pb::RetractProviderRequest {
				caller:              Some(caller_to_pb(identity)),
				provider:            provider.to_owned(),
				expected_generation: generation,
			})
			.await
			.map_err(provider_error)?;
		Ok(())
	}

	async fn request(
		&self,
		identity: &ControlConnectionIdentity,
		request: ProviderControlRequest,
	) -> Result<ProviderControlResult, ProviderControlError> {
		let generation = self.generation().await?;
		let realtime = request.operation == ProviderRequestKind::Realtime;
		let request = operation_request(identity, request, generation)?;
		let response = if realtime {
			self.client().mint_provider_session(request).await
		} else {
			self.client().execute_provider_request(request).await
		}
		.map_err(provider_error)?
		.into_inner();
		provider_result_from_pb(response)
	}
}

fn caller_to_pb(identity: &ControlConnectionIdentity) -> pb::ProviderCaller {
	pb::ProviderCaller {
		extension:          identity.extension.to_string(),
		artifact_digest:    identity.artifact_digest.to_string(),
		host_generation:    identity.host_generation,
		session_generation: identity.session_generation,
		principal_id:       identity.principal.id().to_owned(),
		principal_display:  identity.principal.display().to_owned(),
		layer:              identity.layer.to_string(),
		tier:               identity.tier.to_string(),
		trust:              identity.trust.to_string(),
		capabilities:       identity
			.capabilities
			.iter()
			.map(ToString::to_string)
			.collect(),
	}
}

fn declaration_request(
	identity: &ControlConnectionIdentity,
	declaration: ProviderDeclarationDocument,
	expected_generation: u64,
) -> Result<pb::ProviderDeclarationRequest, ProviderControlError> {
	let document_json = serde_json::to_vec(&declaration.document)
		.map_err(|error| ProviderControlError::Request(error.to_string().into()))?;
	Ok(pb::ProviderDeclarationRequest {
		caller: Some(caller_to_pb(identity)),
		provider: declaration.provider.to_string(),
		document_json: document_json.into(),
		expected_generation,
	})
}

fn operation_request(
	identity: &ControlConnectionIdentity,
	request: ProviderControlRequest,
	expected_generation: u64,
) -> Result<pb::ProviderOperationRequest, ProviderControlError> {
	let payload_json = serde_json::to_vec(&request.payload)
		.map_err(|error| ProviderControlError::Request(error.to_string().into()))?;
	Ok(pb::ProviderOperationRequest {
		caller: Some(caller_to_pb(identity)),
		provider: request.provider.to_string(),
		kind: match request.operation {
			ProviderRequestKind::GenerateImage => {
				provider_operation_request::Kind::GenerateImage as i32
			},
			ProviderRequestKind::Speak => provider_operation_request::Kind::Speak as i32,
			ProviderRequestKind::Transcribe => provider_operation_request::Kind::Transcribe as i32,
			ProviderRequestKind::Realtime => provider_operation_request::Kind::Realtime as i32,
		},
		payload_json: payload_json.into(),
		expected_generation,
	})
}

fn provider_kind_from_pb(kind: i32) -> Result<ProviderRequestKind, Status> {
	match provider_operation_request::Kind::try_from(kind)
		.unwrap_or(provider_operation_request::Kind::Unspecified)
	{
		provider_operation_request::Kind::GenerateImage => Ok(ProviderRequestKind::GenerateImage),
		provider_operation_request::Kind::Speak => Ok(ProviderRequestKind::Speak),
		provider_operation_request::Kind::Transcribe => Ok(ProviderRequestKind::Transcribe),
		provider_operation_request::Kind::Realtime => Ok(ProviderRequestKind::Realtime),
		provider_operation_request::Kind::Unspecified => {
			Err(Status::invalid_argument("provider operation kind is required"))
		},
	}
}

fn provider_status(error: ProviderControlError) -> Status {
	use crate::model_controls::ProviderControlError;
	match error {
		ProviderControlError::Authorization => Status::permission_denied(error.to_string()),
		ProviderControlError::Conflict => Status::already_exists(error.to_string()),
		ProviderControlError::InvalidDeclaration(_) => Status::invalid_argument(error.to_string()),
		ProviderControlError::CapabilityDenied => Status::permission_denied(error.to_string()),
		ProviderControlError::NotFound => Status::not_found(error.to_string()),
		ProviderControlError::Unauthenticated => Status::unauthenticated(error.to_string()),
		ProviderControlError::StaleGeneration => Status::aborted(error.to_string()),
		ProviderControlError::Request(_) => Status::internal(error.to_string()),
	}
}

fn provider_error(status: Status) -> ProviderControlError {
	use crate::model_controls::ProviderControlError;
	match status.code() {
		Code::PermissionDenied if status.message().contains("capability") => {
			ProviderControlError::CapabilityDenied
		},
		Code::PermissionDenied => ProviderControlError::Authorization,
		Code::AlreadyExists => ProviderControlError::Conflict,
		Code::InvalidArgument => ProviderControlError::InvalidDeclaration(status.message().into()),
		Code::NotFound => ProviderControlError::NotFound,
		Code::Unauthenticated => ProviderControlError::Unauthenticated,
		Code::Aborted => ProviderControlError::StaleGeneration,
		_ => ProviderControlError::Request(status.message().into()),
	}
}

fn provider_cursor_to_pb(cursor: ProviderCatalogCursor) -> pb::Cursor {
	pb::Cursor { epoch: cursor.epoch.to_vec().into(), generation: cursor.generation }
}

fn provider_event_to_pb(event: ProviderModelEvent) -> pb::ModelEvent {
	use crate::model_controls::ProviderModelEvent;
	match event {
		ProviderModelEvent::Upsert { cursor, card } => pb::ModelEvent {
			cursor: Some(provider_cursor_to_pb(cursor)),
			event:  Some(model_event::Event::Upserted(provider_model_to_pb(card))),
		},
		ProviderModelEvent::Remove { cursor, id } => pb::ModelEvent {
			cursor: Some(provider_cursor_to_pb(cursor)),
			event:  Some(model_event::Event::RemovedId(id.to_string())),
		},
		ProviderModelEvent::Reset { cursor } => pb::ModelEvent {
			cursor: Some(provider_cursor_to_pb(cursor)),
			event:  Some(model_event::Event::Reset(pb::model_event::Reset {})),
		},
	}
}

fn provider_event_from_pb(
	event: pb::ModelEvent,
) -> Result<ProviderModelEvent, ProviderControlError> {
	let cursor = event
		.cursor
		.ok_or_else(|| ProviderControlError::Request(sf!("gateway omitted provider event cursor")))?;
	let cursor = ProviderCatalogCursor {
		epoch:      cursor.epoch.to_vec().into_boxed_slice(),
		generation: cursor.generation,
	};
	match event.event {
		Some(model_event::Event::Upserted(card)) => {
			Ok(ProviderModelEvent::Upsert { cursor, card: provider_model_from_pb(card) })
		},
		Some(model_event::Event::RemovedId(id)) => {
			Ok(ProviderModelEvent::Remove { cursor, id: id.into() })
		},
		Some(model_event::Event::Reset(_)) => Ok(ProviderModelEvent::Reset { cursor }),
		None => Err(ProviderControlError::Request(sf!("gateway omitted provider event"))),
	}
}

fn provider_model_to_pb(card: ProviderModelCard) -> pb::ModelCard {
	pb::ModelCard {
		id:                card.id.to_string(),
		provider:          card.provider.to_string(),
		model:             card.model.to_string(),
		name:              card.name.to_string(),
		family:            card
			.family
			.map_or_else(String::new, |value| value.to_string()),
		facets:            card
			.facets
			.iter()
			.map(|value| facet_from_str(value) as i32)
			.collect(),
		inputs:            card
			.inputs
			.iter()
			.map(|value| modality_from_str(value) as i32)
			.collect(),
		outputs:           card
			.outputs
			.iter()
			.map(|value| modality_from_str(value) as i32)
			.collect(),
		reasoning:         card.reasoning,
		efforts:           card
			.efforts
			.iter()
			.map(|value| effort_from_str(value) as i32)
			.collect(),
		context_window:    card.context_window.unwrap_or(0),
		max_output_tokens: card.max_output_tokens.unwrap_or(0),
		pricing:           card
			.pricing
			.iter()
			.map(|price| pb::Price {
				unit:      price_unit_from_str(&price.unit) as i32,
				nanos_usd: price.nanos_usd,
			})
			.collect(),
		availability:      availability_from_str(&card.availability) as i32,
		source:            i32::from(card.source),
		blocked_until_ms:  card.blocked_until_ms.unwrap_or(0),
		deprecated:        card.deprecated,
		updated_at_ms:     card.updated_at_ms.unwrap_or(0),
		supports_tools:    card.supports_tools,
		props:             None,
	}
}

fn provider_model_from_pb(card: pb::ModelCard) -> ProviderModelCard {
	ProviderModelCard {
		id:                card.id.into(),
		provider:          card.provider.into(),
		model:             card.model.into(),
		name:              card.name.into(),
		family:            (!card.family.is_empty()).then(|| card.family.into()),
		facets:            card
			.facets
			.into_iter()
			.filter_map(|value| pb::Facet::try_from(value).ok())
			.map(|value| facet_name(value).into())
			.collect(),
		inputs:            card
			.inputs
			.into_iter()
			.filter_map(|value| pb::Modality::try_from(value).ok())
			.map(|value| modality_name(value).into())
			.collect(),
		outputs:           card
			.outputs
			.into_iter()
			.filter_map(|value| pb::Modality::try_from(value).ok())
			.map(|value| modality_name(value).into())
			.collect(),
		reasoning:         card.reasoning,
		efforts:           card
			.efforts
			.into_iter()
			.filter_map(|value| Effort::try_from(value).ok())
			.map(|value| effort_name(value).into())
			.collect(),
		context_window:    (card.context_window != 0).then_some(card.context_window),
		max_output_tokens: (card.max_output_tokens != 0).then_some(card.max_output_tokens),
		pricing:           card
			.pricing
			.into_iter()
			.map(|price| ProviderPrice {
				unit:      price::Unit::try_from(price.unit)
					.map_or("unknown", price_unit_name)
					.into(),
				nanos_usd: price.nanos_usd,
			})
			.collect(),
		availability:      pb::Availability::try_from(card.availability)
			.map_or("unknown", availability_name)
			.into(),
		source:            u8::try_from(card.source).unwrap_or(0),
		blocked_until_ms:  (card.blocked_until_ms != 0).then_some(card.blocked_until_ms),
		deprecated:        card.deprecated,
		updated_at_ms:     (card.updated_at_ms != 0).then_some(card.updated_at_ms),
		supports_tools:    card.supports_tools,
		props:             Map::new(),
	}
}

fn facet_from_str(value: &str) -> pb::Facet {
	match value {
		"chat" => pb::Facet::Chat,
		"embed" => pb::Facet::Embed,
		"image_gen" => pb::Facet::ImageGen,
		"video_gen" => pb::Facet::VideoGen,
		"speak" => pb::Facet::Speak,
		"transcribe" => pb::Facet::Transcribe,
		"realtime" => pb::Facet::Realtime,
		"search" => pb::Facet::Search,
		_ => pb::Facet::Unspecified,
	}
}
fn facet_name(value: pb::Facet) -> &'static str {
	match value {
		pb::Facet::Chat => "chat",
		pb::Facet::Embed => "embed",
		pb::Facet::ImageGen => "image_gen",
		pb::Facet::VideoGen => "video_gen",
		pb::Facet::Speak => "speak",
		pb::Facet::Transcribe => "transcribe",
		pb::Facet::Realtime => "realtime",
		pb::Facet::Search => "search",
		pb::Facet::Unspecified => "unspecified",
	}
}
fn modality_from_str(value: &str) -> pb::Modality {
	match value {
		"text" => pb::Modality::Text,
		"image" => pb::Modality::Image,
		"audio" => pb::Modality::Audio,
		"video" => pb::Modality::Video,
		"document" | "pdf" => pb::Modality::Pdf,
		_ => pb::Modality::Unspecified,
	}
}
fn modality_name(value: pb::Modality) -> &'static str {
	match value {
		pb::Modality::Text => "text",
		pb::Modality::Image => "image",
		pb::Modality::Audio => "audio",
		pb::Modality::Video => "video",
		pb::Modality::Pdf => "document",
		pb::Modality::Unspecified => "unspecified",
	}
}
fn effort_from_str(value: &str) -> Effort {
	match value {
		"off" => Effort::Off,
		"minimal" => Effort::Minimal,
		"low" => Effort::Low,
		"medium" => Effort::Medium,
		"high" => Effort::High,
		"max" => Effort::Max,
		"xhigh" => Effort::Xhigh,
		_ => Effort::Unspecified,
	}
}
fn effort_name(value: Effort) -> &'static str {
	match value {
		Effort::Off => "off",
		Effort::Minimal => "minimal",
		Effort::Low => "low",
		Effort::Medium => "medium",
		Effort::High => "high",
		Effort::Max => "max",
		Effort::Xhigh => "xhigh",
		Effort::Unspecified => "unspecified",
	}
}
fn availability_from_str(value: &str) -> pb::Availability {
	match value {
		"available" => pb::Availability::Available,
		"login_required" => pb::Availability::LoginRequired,
		"blocked" => pb::Availability::Blocked,
		"disabled" => pb::Availability::Disabled,
		_ => pb::Availability::Unspecified,
	}
}
fn availability_name(value: pb::Availability) -> &'static str {
	match value {
		pb::Availability::Available => "available",
		pb::Availability::LoginRequired => "login_required",
		pb::Availability::Blocked => "blocked",
		pb::Availability::Disabled => "disabled",
		pb::Availability::Unspecified => "unspecified",
	}
}
fn price_unit_from_str(value: &str) -> price::Unit {
	match value {
		"mtok_input" => price::Unit::MtokInput,
		"mtok_output" => price::Unit::MtokOutput,
		"mtok_cache_read" => price::Unit::MtokCacheRead,
		"mtok_cache_write" => price::Unit::MtokCacheWrite,
		"image" => price::Unit::Image,
		"video_second" => price::Unit::VideoSecond,
		"audio_second" => price::Unit::AudioSecond,
		"mchar_input" => price::Unit::McharInput,
		"request" => price::Unit::Request,
		_ => price::Unit::Unspecified,
	}
}
fn price_unit_name(value: price::Unit) -> &'static str {
	match value {
		price::Unit::MtokInput => "mtok_input",
		price::Unit::MtokOutput => "mtok_output",
		price::Unit::MtokCacheRead => "mtok_cache_read",
		price::Unit::MtokCacheWrite => "mtok_cache_write",
		price::Unit::Image => "image",
		price::Unit::VideoSecond => "video_second",
		price::Unit::AudioSecond => "audio_second",
		price::Unit::McharInput => "mchar_input",
		price::Unit::Request => "request",
		price::Unit::Unspecified => "unknown",
	}
}

fn provider_result_to_pb(result: ProviderControlResult) -> pb::ProviderOperationResponse {
	use crate::model_controls::ProviderControlResult;
	let result = match result {
		ProviderControlResult::Image { images, cost_nanos_usd } => {
			provider_operation_response::Result::Image(pb::provider_operation_response::Image {
				images: images
					.into_vec()
					.into_iter()
					.map(|blob| pb::ProviderBlob { hash: blob.hash.to_string(), size: blob.size })
					.collect(),
				cost_nanos_usd,
			})
		},
		ProviderControlResult::Speech { audio, format, cost_nanos_usd } => {
			provider_operation_response::Result::Speech(pb::provider_operation_response::Speech {
				audio: Some(pb::ProviderBlob { hash: audio.hash.to_string(), size: audio.size }),
				format: format.to_string(),
				cost_nanos_usd,
			})
		},
		ProviderControlResult::Transcription { text, language, cost_nanos_usd } => {
			provider_operation_response::Result::Transcription(
				pb::provider_operation_response::Transcription {
					text: text.to_string(),
					language: language.map(|value| value.to_string()),
					cost_nanos_usd,
				},
			)
		},
		ProviderControlResult::Realtime { id, endpoint, credential, expires_at_ms, transport } => {
			provider_operation_response::Result::Realtime(pb::provider_operation_response::Realtime {
				id: id.to_string(),
				endpoint: endpoint.to_string(),
				credential: credential.to_string(),
				expires_at_ms,
				transport: transport.to_string(),
			})
		},
	};
	pb::ProviderOperationResponse { result: Some(result) }
}

fn provider_result_from_pb(
	response: pb::ProviderOperationResponse,
) -> Result<ProviderControlResult, ProviderControlError> {
	use crate::model_controls::{ProviderBlobRef, ProviderControlError, ProviderControlResult};
	match response.result {
		Some(provider_operation_response::Result::Image(image)) => Ok(ProviderControlResult::Image {
			images:         image
				.images
				.into_iter()
				.map(|blob| ProviderBlobRef { hash: blob.hash.into(), size: blob.size })
				.collect(),
			cost_nanos_usd: image.cost_nanos_usd,
		}),
		Some(provider_operation_response::Result::Speech(speech)) => {
			let audio = speech
				.audio
				.ok_or_else(|| ProviderControlError::Request(sf!("gateway omitted speech blob")))?;
			Ok(ProviderControlResult::Speech {
				audio:          ProviderBlobRef { hash: audio.hash.into(), size: audio.size },
				format:         speech.format.into(),
				cost_nanos_usd: speech.cost_nanos_usd,
			})
		},
		Some(provider_operation_response::Result::Transcription(transcription)) => {
			Ok(ProviderControlResult::Transcription {
				text:           transcription.text.into(),
				language:       transcription.language.map(Into::into),
				cost_nanos_usd: transcription.cost_nanos_usd,
			})
		},
		Some(provider_operation_response::Result::Realtime(realtime)) => {
			Ok(ProviderControlResult::Realtime {
				id:            realtime.id.into(),
				endpoint:      realtime.endpoint.into(),
				credential:    realtime.credential.into(),
				expires_at_ms: realtime.expires_at_ms,
				transport:     realtime.transport.into(),
			})
		},
		None => Err(ProviderControlError::Request(sf!("gateway omitted provider operation result"))),
	}
}

#[cfg(test)]
mod tests {
	use std::{
		future::Future,
		iter,
		pin::Pin,
		sync::atomic::{AtomicBool, AtomicUsize, Ordering},
	};

	use omp_catalog::{
		ContextStrategy, DiscoveredModel, DiscoveryDefaults, ModelAvailability, OperationBits,
		Pricing, RouteId, WireModelId, WirePolicyId,
	};
	use omp_inference::{
		ModelsDiscoverHookPage, ProviderHookError, ProviderHookObserver, ProviderResponseObservation,
		ProviderResponseObserver,
		discovery::{DiscoveryEndpoint, DiscoveryEndpointKind, EndpointOrigin},
	};
	use serde_json::json;
	use tokio::{io::AsyncWriteExt as _, sync::Notify};

	use super::*;

	fn row(provider: &ProviderId<str>, model: &str) -> DiscoveredModel {
		DiscoveredModel {
			provider:              provider.to_owned(),
			route:                 RouteId::from("configured-route"),
			wire_model:            WireModelId::from(model),
			aliases:               Box::new([]),
			display_name:          None,
			declared_class:        None,
			declared_operations:   OperationBits::empty(),
			declared_capabilities: None,
			declared_limits:       None,
			declared_pricing:      Box::new([]),
			extended_context_mode: None,
			availability:          Some(ModelAvailability::Available),
			source:                Str::new_static("fixture"),
			observed_at_ms:        Some(100),
			updated_at_ms:         None,
			deprecated:            None,
		}
	}

	fn normalizer() -> DiscoveryNormalizer {
		DiscoveryNormalizer::new(DiscoveryDefaults {
			wire_policy:          WirePolicyId::from("configured-wire"),
			extended_wire_policy: None,
			context:              ContextStrategy::Replay,
			thinking:             None,
			pricing:              Pricing::default(),
		})
	}

	fn shared_cache_overlay(
		base: &snapshot::Catalog,
		observed_at_ms: u64,
		addition: bool,
	) -> (CatalogOverlay, omp_catalog::ModelKey) {
		let bundled = base
			.models()
			.iter()
			.find(|model| !model.routes.is_empty())
			.expect("bundled model")
			.clone();
		let provider = base
			.route(&bundled.routes[0])
			.expect("bundled route")
			.provider
			.clone();
		let mut model = bundled.clone();
		if addition {
			model.key =
				omp_catalog::ModelKey::from(format!("{}/runtime-cache-fixture", provider.as_str()));
		}
		let key = model.key.clone();
		let source = ProvenanceSource {
			kind:           ProvenanceKind::Discovered,
			origin:         Str::new_static("models.dev"),
			revision:       None,
			confidence:     EvidenceConfidence::Declared,
			observed_at_ms: Some(observed_at_ms),
		};
		let overlay = CatalogOverlayBuilder::new(source)
			.with_model(ModelOverlay {
				selector: omp_catalog::ExactSelector::new(provider, model.key.clone()),
				added:    Some(model),
				patch:    ModelPatch::default(),
			})
			.build();
		(overlay, key)
	}

	struct StubExthost {
		fail: AtomicBool,
	}

	struct FailingDiscoveryClient;

	impl DiscoveryHttpClient for FailingDiscoveryClient {
		fn request(
			&self,
			_request: ProbeHttpRequest,
			_cancellation: CancellationToken,
		) -> ProbeHttpFuture {
			Box::pin(async { Err(ProbeError::Transport) })
		}
	}

	impl ProviderHookObserver for StubExthost {
		fn models_discover_subscribed(&self, provider: &ProviderId<str>) -> bool {
			provider.as_str() == "extension-provider"
		}

		fn models_discover<'a>(
			&'a self,
			_request: ModelsDiscoverHookRequest,
		) -> Pin<
			Box<dyn Future<Output = Result<ModelsDiscoverHookPage, ProviderHookError>> + Send + 'a>,
		> {
			Box::pin(async move {
				if self.fail.load(Ordering::Acquire) {
					return Err(ProviderHookError::Failed);
				}
				Ok(ModelsDiscoverHookPage {
					models:        Box::new([json!({
						"id": "dynamic-model",
						"display_name": "Dynamic Model",
						"routes": ["configured-route"],
						"operations": ["chat"],
						"context_window": 32_000,
					})]),
					next_cursor:   None,
					authoritative: true,
				})
			})
		}
	}

	impl ProviderResponseObserver for StubExthost {
		fn subscribed(&self) -> bool {
			false
		}

		fn observe(&self, _observation: ProviderResponseObservation) {}
	}

	#[tokio::test]
	async fn extension_discovery_publishes_and_failure_retains_previous_generation() {
		let directory = tempfile::tempdir().expect("directory");
		let overlays = Arc::new(OverlayStore::default());
		let runtime = DiscoveryRuntime::new(
			Arc::new(DiscoveryStore::open(&directory.path().join("models.db")).expect("store")),
			Arc::clone(&overlays),
			iter::empty::<ProviderId>(),
			Arc::new(snapshot::Catalog::embedded().clone()),
			directory.path().join("catalog-discovery.json"),
		)
		.expect("runtime");
		let exthost = Arc::new(StubExthost { fail: AtomicBool::new(false) });
		let hooks = ProviderResponseHooks::new(exthost.clone());
		let request = ModelsDiscoverHookRequest {
			provider:  ProviderId::from("extension-provider"),
			route:     RouteId::from("configured-route"),
			cursor:    None,
			page_size: Some(100),
			trigger:   sf!("session_start"),
		};
		assert_eq!(
			runtime
				.refresh_extension(&hooks, request.clone(), &normalizer(), 200)
				.await
				.expect("publish"),
			RefreshOutcome::Published { models: 1 }
		);
		let published = overlays.load();
		assert!(
			published
				.sources()
				.contains(&OverlaySource::Extension { id: sf!("models-discover:extension-provider") })
		);
		let generation = published.generation();
		exthost.fail.store(true, Ordering::Release);
		assert_eq!(
			runtime
				.refresh_extension(&hooks, request, &normalizer(), 201)
				.await
				.expect("retain"),
			RefreshOutcome::Retained
		);
		assert_eq!(overlays.load().generation(), generation);
	}

	#[test]
	fn explicit_disable_is_authoritative_but_failure_is_not() {
		let directory = tempfile::tempdir().expect("directory");
		let disabled = ProviderId::from("disabled");
		let runtime = DiscoveryRuntime::new(
			Arc::new(DiscoveryStore::open(&directory.path().join("models.db")).expect("store")),
			Arc::new(OverlayStore::default()),
			[disabled.clone()],
			Arc::new(snapshot::Catalog::embedded().clone()),
			directory.path().join("catalog-discovery.json"),
		)
		.expect("runtime");
		assert!(!runtime.provider_selectable(&disabled));
		assert!(runtime.provider_selectable(ProviderId::from_ref("offline")));
	}

	#[tokio::test]
	async fn offline_shared_catalog_cache_is_served_and_static_only_cache_is_bundled() {
		let directory = tempfile::tempdir().expect("directory");
		let base = Arc::new(snapshot::Catalog::embedded().clone());
		let cache_path = directory.path().join("catalog-discovery.json");
		let (overlay, added_key) = shared_cache_overlay(&base, 100, true);
		snapshot::write_discovery_overlay_cache(&cache_path, &overlay).expect("write cache");
		let runtime = DiscoveryRuntime::new(
			Arc::new(DiscoveryStore::open(&directory.path().join("models.db")).expect("store")),
			Arc::new(OverlayStore::default()),
			iter::empty::<ProviderId>(),
			Arc::clone(&base),
			cache_path.clone(),
		)
		.expect("runtime");
		assert_eq!(runtime.shared_catalog_status().await, SharedCatalogRefreshOutcome {
			source:                 SharedCatalogSource::DiskCache,
			stale:                  true,
			models:                 1,
			updated_at_ms:          Some(100),
			revalidation_failed_ms: None,
			revalidation_failure:   None,
		});
		assert!(
			runtime
				.catalog()
				.expect("catalog")
				.model(&added_key)
				.is_some()
		);

		let (static_only, _) = shared_cache_overlay(&base, 101, false);
		snapshot::write_discovery_overlay_cache(&cache_path, &static_only)
			.expect("write static-only cache");
		let static_runtime = DiscoveryRuntime::new(
			Arc::new(DiscoveryStore::open(&directory.path().join("static-models.db")).expect("store")),
			Arc::new(OverlayStore::default()),
			iter::empty::<ProviderId>(),
			base,
			cache_path,
		)
		.expect("static runtime");
		let status = static_runtime.shared_catalog_status().await;
		assert_eq!(status.source, SharedCatalogSource::Bundled);
		assert!(status.stale);
		assert_eq!(status.models, 0);
		assert_eq!(status.updated_at_ms, None);
	}

	#[tokio::test]
	async fn failed_revalidation_preserves_prior_good_shared_catalog_state() {
		let directory = tempfile::tempdir().expect("directory");
		let base = Arc::new(snapshot::Catalog::embedded().clone());
		let cache_path = directory.path().join("catalog-discovery.json");
		let (overlay, added_key) = shared_cache_overlay(&base, 100, true);
		snapshot::write_discovery_overlay_cache(&cache_path, &overlay).expect("write cache");
		let runtime = Arc::new(
			DiscoveryRuntime::with_http_client(
				Arc::new(DiscoveryStore::open(&directory.path().join("models.db")).expect("store")),
				Arc::new(OverlayStore::default()),
				iter::empty::<ProviderId>(),
				base,
				cache_path,
				Arc::new(RuntimeDiscoveryHttpClient::with_shared_catalog_url(
					"http://127.0.0.1:1/models.json.zstd",
				)),
			)
			.expect("runtime"),
		);
		let status = runtime
			.refresh_shared_catalog(Duration::from_secs(1), CancellationToken::new())
			.await
			.expect("fallback status");
		assert_eq!(status.source, SharedCatalogSource::DiskCache);
		assert!(status.stale);
		assert_eq!(status.updated_at_ms, Some(100));
		assert!(status.revalidation_failure.is_some());
		assert!(
			runtime
				.catalog()
				.expect("catalog")
				.model(&added_key)
				.is_some()
		);
	}

	#[tokio::test]
	async fn subscriber_deadlines_do_not_abort_joined_shared_catalog_transport() {
		let directory = tempfile::tempdir().expect("directory");
		let base = Arc::new(snapshot::Catalog::embedded().clone());
		let cache_path = directory.path().join("catalog-discovery.json");
		let (overlay, _) = shared_cache_overlay(&base, 100, true);
		snapshot::write_discovery_overlay_cache(&cache_path, &overlay).expect("write cache");
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
			.await
			.expect("listener");
		let url =
			format!("http://{}/models.json.zstd", listener.local_addr().expect("listener address"));
		let accepted = Arc::new(AtomicUsize::new(0));
		let release = Arc::new(Notify::new());
		let server_accepted = Arc::clone(&accepted);
		let server_release = Arc::clone(&release);
		let server = tokio::spawn(async move {
			let (mut stream, _) = listener.accept().await.expect("accept");
			server_accepted.fetch_add(1, Ordering::SeqCst);
			server_release.notified().await;
			stream
				.write_all(
					b"HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
				)
				.await
				.expect("response");
		});
		let runtime = Arc::new(
			DiscoveryRuntime::with_http_client(
				Arc::new(DiscoveryStore::open(&directory.path().join("models.db")).expect("store")),
				Arc::new(OverlayStore::default()),
				iter::empty::<ProviderId>(),
				base,
				cache_path,
				Arc::new(RuntimeDiscoveryHttpClient::with_shared_catalog_url(url)),
			)
			.expect("runtime"),
		);
		let short_runtime = Arc::clone(&runtime);
		let short = tokio::spawn(async move {
			short_runtime
				.refresh_shared_catalog(Duration::from_millis(25), CancellationToken::new())
				.await
		});
		time::sleep(Duration::from_millis(5)).await;
		let long_runtime = Arc::clone(&runtime);
		let long = tokio::spawn(async move {
			long_runtime
				.refresh_shared_catalog(Duration::from_secs(1), CancellationToken::new())
				.await
		});
		assert!(matches!(
			short.await.expect("short task"),
			Err(DiscoveryRuntimeError::SubscriberDeadline)
		));
		assert_eq!(accepted.load(Ordering::SeqCst), 1);
		release.notify_one();
		let outcome = long.await.expect("long task").expect("long subscriber");
		assert_eq!(outcome.source, SharedCatalogSource::Remote);
		assert!(!outcome.stale);
		server.await.expect("server");
		assert_eq!(accepted.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn static_catalog_fallback_survives_dynamic_refresh_failure() {
		let directory = tempfile::tempdir().expect("directory");
		let base = Arc::new(snapshot::Catalog::embedded().clone());
		let static_model = base.models().first().expect("bundled model").clone();
		let route = base
			.route(static_model.routes.first().expect("bundled model route"))
			.expect("bundled route")
			.clone();
		let mut spec = base
			.discovery_specs()
			.first()
			.expect("discovery spec")
			.clone();
		spec.interval = Some(Duration::from_secs(5));
		let cache =
			Arc::new(DiscoveryStore::open(&directory.path().join("models.db")).expect("store"));
		let runtime = DiscoveryRuntime::new(
			cache,
			Arc::new(OverlayStore::default()),
			iter::empty::<ProviderId>(),
			Arc::clone(&base),
			directory.path().join("catalog-discovery.json"),
		)
		.expect("runtime");
		let probe = DiscoveryProbe {
			provider: route.provider.clone(),
			route:    route.id.clone(),
			endpoint: DiscoveryEndpoint {
				kind:     DiscoveryEndpointKind::OpenAi,
				base_url: Str::new_static("http://127.0.0.1:1"),
				origin:   EndpointOrigin::Configured,
			},
		};
		let error = runtime
			.refresh(
				DiscoveryPollKey {
					provider: route.provider.clone(),
					route:    route.id.clone(),
					spec:     spec.id.clone(),
				},
				&DiscoveryCacheKey::provider(route.provider),
				&spec,
				&probe,
				&normalizer(),
				&FailingDiscoveryClient,
				Instant::now(),
				200,
				Duration::from_secs(60),
				CancellationToken::new(),
			)
			.await
			.expect_err("dynamic refresh fails");
		assert!(matches!(error, DiscoveryRuntimeError::Probe(ProbeError::Transport)));
		assert_eq!(runtime.catalog().expect("catalog").model(&static_model.key), Some(&static_model));
	}

	#[test]
	fn credential_cache_hydration_repeats_after_affinity_changes() {
		let directory = tempfile::tempdir().expect("directory");
		let cache =
			Arc::new(DiscoveryStore::open(&directory.path().join("models.db")).expect("store"));
		let overlays = Arc::new(OverlayStore::default());
		let runtime = DiscoveryRuntime::new(
			Arc::clone(&cache),
			Arc::clone(&overlays),
			iter::empty::<ProviderId>(),
			Arc::new(snapshot::Catalog::embedded().clone()),
			directory.path().join("catalog-discovery.json"),
		)
		.expect("runtime");
		let provider = ProviderId::from("opencode-go");
		let first = DiscoveryCacheKey::credential(provider.clone(), "affinity-first");
		let second = DiscoveryCacheKey::credential(provider.clone(), "affinity-second");
		cache
			.publish(&first, &[row(&provider, "first-model")], 100, Duration::from_secs(60))
			.expect("first cache");
		cache
			.publish(&second, &[row(&provider, "second-model")], 101, Duration::from_secs(60))
			.expect("second cache");
		let cached = cache
			.load_fresh(&first, 102)
			.expect("load cache")
			.expect("fresh cache");
		let restored = normalizer()
			.normalize(&cached.rows[0])
			.expect("current configured route policy reattaches");
		assert_eq!(restored.model.routes.as_ref(), [RouteId::from("configured-route")]);
		assert_eq!(restored.model.wire_policy, WirePolicyId::from("configured-wire"));

		assert_eq!(
			runtime
				.hydrate_cached(
					&[CachedDiscoveryHydration { key: first, normalizer: normalizer() }],
					102,
				)
				.expect("first hydration"),
			1
		);
		let first_generation = overlays.load().generation();
		assert_eq!(
			runtime
				.hydrate_cached(
					&[CachedDiscoveryHydration { key: second, normalizer: normalizer() }],
					102,
				)
				.expect("second hydration"),
			1
		);
		assert!(overlays.load().generation() > first_generation);
	}
}
