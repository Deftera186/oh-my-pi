//! Environment-scoped MCP authority shared by every local consumer.
//!
//! The service is owned by one [`crate::server::EnvServer`]. Dynamic
//! invocation, URI resolution, UI, and Python CONTROL therefore observe one
//! definition epoch without a process-global singleton.

pub mod auth_authority;
pub mod client;
pub mod config;
pub mod config_store;
pub(crate) mod config_values;
pub mod control;
pub mod device;
pub(crate) mod filter;
pub(crate) mod header_policy;
pub(crate) mod http;
pub(crate) mod invoke;
pub mod json_rpc;
pub(crate) mod legacy_sse;
pub mod manager;
pub mod oauth;
pub(crate) mod prompts;
pub(crate) mod resources;
pub(crate) mod settings;
pub(crate) mod stdio;
pub(crate) mod timeout;
pub mod transport;

use std::{
	collections::{BTreeMap, VecDeque},
	fmt,
	path::{Path, PathBuf},
	sync::{
		Arc, Weak,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
};

use bytes::Bytes;
use flume::Receiver;
use futures::future::BoxFuture;
use omp_core::Str;
use omp_proto::env::v1 as pb;
use omp_storage::mcp_cache::{McpCacheError, McpDefinitionCache};
use omp_tool::{
	LeafCatalogSnapshot, LeafOwner, LeafReplacementError, LeafReplacementRegistry, LeafVersion,
	RegistryLeaf, Rev,
};
use parking_lot::RwLock;
use tokio::task;
use tokio_util::sync::CancellationToken;

use super::exthost::control::ControlConnectionIdentity;

const NOTIFICATION_HISTORY: usize = 256;
const SUBSCRIBER_CAPACITY: usize = 64;

/// Event emitted to one Environment MCP subscription.
#[derive(Clone, Debug)]
pub enum SubscriptionEvent {
	/// Server protocol notification.
	Notification(pb::McpNotification),
	/// Lifecycle or definition transition.
	Status(pb::McpServerStatus),
}

/// Live subscription receiver.
pub struct ServiceSubscription {
	receiver: Receiver<SubscriptionEvent>,
}

impl ServiceSubscription {
	/// Receives the next event until caller cancellation or service closure.
	pub async fn next(
		&self,
		cancel: &CancellationToken,
	) -> Result<Option<SubscriptionEvent>, McpServiceError> {
		tokio::select! {
			biased;
			() = cancel.cancelled() => Err(McpServiceError::Cancelled),
			result = self.receiver.recv_async() => Ok(result.ok()),
		}
	}
}

/// Dynamic MCP server implementation installed by the supervisor/transport
/// layer. The allocation is confined to cold network operations at this dyn
/// boundary.
pub trait McpServerBackend: Send + Sync {
	/// Performs a manual lifecycle reset.
	fn reset(&self, cancel: CancellationToken) -> BoxFuture<'_, Result<(), McpServiceError>>;
	/// Produces origin-scoped live headers without credential-owner metadata.
	fn live_header(
		&self,
		request: pb::McpLiveHeaderRequest,
		cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpLiveHeader, McpServiceError>>;
	/// Reads one MCP resource.
	fn resource(
		&self,
		request: pb::McpResourceRequest,
		cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpResourceResult, McpServiceError>>;
	/// Gets one MCP prompt.
	fn prompt(
		&self,
		request: pb::McpPromptRequest,
		cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpPromptResult, McpServiceError>>;
	/// Invokes one MCP tool.
	fn invoke(
		&self,
		request: pb::McpInvokeRequest,
		cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpInvokeResult, McpServiceError>>;
}

struct ServerEntry {
	status:  pb::McpServerStatus,
	backend: Arc<dyn McpServerBackend>,
}

struct State {
	servers:     BTreeMap<Str, ServerEntry>,
	history:     VecDeque<pb::McpNotification>,
	subscribers: BTreeMap<u64, Subscriber>,
}

struct Subscriber {
	name:   Option<Str>,
	sender: flume::Sender<SubscriptionEvent>,
}

/// Runtime MCP leaf payload retained by the revisioned registry.
#[derive(Clone, Debug)]
pub struct McpLeaf {
	/// Owning MCP server name.
	pub server:              Str,
	/// MCP definition kind (`tool`, `resource`, `resource-template`, or
	/// `prompt`).
	pub kind:                Str,
	/// Canonical bounded definition JSON.
	pub definition_json:     Bytes,
	/// Bounded server instructions retained as device documentation.
	pub documentation:       Option<Str>,
	/// Original bounded server instructions, before per-endpoint documentation
	/// projection.
	pub server_instructions: Option<Str>,
	/// Python claimant precedence retained without narrowing.
	pub precedence:          i32,
	/// Default approval tier retained without narrowing.
	pub tier:                Str,
}

/// Input leaf for one complete server publication.
pub struct McpLeafDefinition {
	/// Canonical leaf name.
	pub name:                Str,
	/// MCP definition kind.
	pub kind:                Str,
	/// Semantic revision.
	pub rev:                 Rev,
	/// Definition/implementation binding digest.
	pub code:                omp_core::Hash32,
	/// Canonical bounded definition JSON.
	pub definition_json:     Bytes,
	/// Bounded server instructions retained as device documentation.
	pub documentation:       Option<Str>,
	/// Original bounded server instructions.
	pub server_instructions: Option<Str>,
	/// Python claimant precedence.
	pub precedence:          i32,
	/// Default approval tier.
	pub tier:                Str,
}

/// One environment's MCP service and monotone definition authority.
pub struct McpService {
	state:                 RwLock<State>,
	config_paths:          RwLock<Option<McpConfigPaths>>,
	manager:               RwLock<Option<Weak<manager::McpManager>>>,
	enable_project_config: AtomicBool,
	leaves:                LeafReplacementRegistry<McpLeaf>,
	cache:                 Arc<McpDefinitionCache>,
	definition_epoch:      AtomicU64,
	next_subscriber:       AtomicU64,
}

impl McpService {
	/// Opens an empty, ready-to-mount Environment authority with its
	/// storage-authority definition cache.
	pub fn open(cache_path: impl AsRef<Path>) -> Result<Arc<Self>, McpCacheError> {
		Ok(Arc::new(Self {
			state:                 RwLock::new(State {
				servers:     BTreeMap::new(),
				history:     VecDeque::with_capacity(NOTIFICATION_HISTORY),
				subscribers: BTreeMap::new(),
			}),
			config_paths:          RwLock::new(None),
			manager:               RwLock::new(None),
			enable_project_config: AtomicBool::new(true),
			leaves:                LeafReplacementRegistry::new(),
			cache:                 Arc::new(McpDefinitionCache::open(cache_path)?),
			definition_epoch:      AtomicU64::new(0),
			next_subscriber:       AtomicU64::new(1),
		}))
	}

	/// Binds this Environment's native user/project MCP mutation roots.
	pub fn bind_config_paths(&self, data_dir: &Path, project_root: &Path) {
		*self.config_paths.write() = Some(McpConfigPaths {
			user:    data_dir.join("mcp.json"),
			project: project_root.join(".omp/mcp.json"),
			root:    project_root.join(".mcp.json"),
		});
	}

	/// Binds the supervisor which owns live transports for successful config
	/// mutations.
	pub fn bind_manager(&self, manager: &Arc<manager::McpManager>) {
		*self.manager.write() = Some(Arc::downgrade(manager));
	}

	/// Loads, precedence-resolves, and mounts every persisted native source.
	/// This must be called once after transport credential authorities are
	/// bound; subsequent config mutations reuse the retained discovery policy.
	pub async fn start_native_configs(
		&self,
		enable_project_config: bool,
	) -> Result<manager::StartupSnapshot, McpServiceError> {
		self
			.enable_project_config
			.store(enable_project_config, Ordering::Release);
		self.reload_native_configs().await
	}

	/// Reloads persisted native sources using the retained settings policy.
	/// Late-bound credential authorities call this after composition so OAuth
	/// headers and native Exa imports participate in the mounted specs.
	pub async fn reload_native_configs(&self) -> Result<manager::StartupSnapshot, McpServiceError> {
		let paths = self
			.config_paths
			.read()
			.clone()
			.ok_or(McpServiceError::InvalidRequest)?;
		let enable_project_config = self.enable_project_config.load(Ordering::Acquire);
		let resolved =
			task::spawn_blocking(move || load_resolved_config(paths, enable_project_config))
				.await
				.map_err(|_| McpServiceError::InvalidRequest)??;
		let manager = self
			.manager
			.read()
			.as_ref()
			.and_then(Weak::upgrade)
			.ok_or(McpServiceError::InvalidRequest)?;
		Ok(manager.replace_resolved_config(resolved).await)
	}

	/// Lists live concrete advertised URIs for `mcp://` completion.
	pub(crate) fn resource_uris(&self) -> Vec<Str> {
		self
			.manager
			.read()
			.as_ref()
			.and_then(Weak::upgrade)
			.map_or_else(Vec::new, |manager| manager.resource_uris())
	}

	/// Resolves a PI-compatible opaque `mcp://<advertised-uri>` payload to the
	/// deterministic current server which advertised it.
	pub(crate) fn resolve_resource_server(&self, uri: &str) -> Option<pb::McpServerRef> {
		let name = self
			.manager
			.read()
			.as_ref()
			.and_then(Weak::upgrade)
			.and_then(|manager| manager.resolve_resource_server(uri))?;
		Some(pb::McpServerRef {
			name:             name.to_string(),
			definition_epoch: self.definition_epoch(),
		})
	}

	/// Builds one extension-scoped MCP CONTROL projection over the live manager.
	pub(crate) fn control(
		self: &Arc<Self>,
		identity: Arc<ControlConnectionIdentity>,
		cancellation: CancellationToken,
	) -> Option<control::McpControl> {
		let manager = self.manager.read().as_ref().and_then(Weak::upgrade)?;
		let resolver = Arc::new(manager::ManagerControlMountResolver::new(
			Arc::clone(&manager),
			Arc::clone(&identity),
			cancellation,
		));
		Some(control::McpControl::new(manager, resolver, identity))
	}

	/// Executes one finite native MCP configuration RPC.
	pub async fn config(
		&self,
		request: pb::McpConfigRequest,
	) -> Result<pb::McpConfigResult, McpServiceError> {
		let paths = self
			.config_paths
			.read()
			.clone()
			.ok_or(McpServiceError::InvalidRequest)?;
		let action = pb::McpConfigAction::try_from(request.action)
			.map_err(|_| McpServiceError::InvalidRequest)?;
		let result = task::spawn_blocking(move || config_request(paths, request))
			.await
			.map_err(|_| McpServiceError::InvalidRequest)??;
		let manager = self.manager.read().as_ref().and_then(Weak::upgrade);
		if matches!(
			action,
			pb::McpConfigAction::Add
				| pb::McpConfigAction::Update
				| pb::McpConfigAction::Remove
				| pb::McpConfigAction::Enable
				| pb::McpConfigAction::Disable
		) && manager.is_some()
		{
			self.reload_native_configs().await?;
		}
		Ok(result)
	}

	/// Borrows the environment's one persistent definition cache.
	pub fn cache(&self) -> &Arc<McpDefinitionCache> {
		&self.cache
	}

	/// Atomically replaces one server's complete dynamic leaf set and publishes
	/// exactly one catalog epoch. Older manager generations/definition epochs
	/// are fenced by `omp-tool`; historical `(name, rev)` bindings remain.
	pub fn replace_leaves(
		&self,
		owner: LeafOwner,
		version: LeafVersion,
		leaves: Vec<McpLeafDefinition>,
	) -> Result<u64, McpServiceError> {
		let server = owner.root.clone();
		let leaves = leaves
			.into_iter()
			.map(|leaf| RegistryLeaf {
				name:  leaf.name,
				rev:   leaf.rev,
				code:  leaf.code,
				value: Arc::new(McpLeaf {
					server:              server.clone(),
					kind:                leaf.kind,
					definition_json:     leaf.definition_json,
					documentation:       leaf.documentation,
					server_instructions: leaf.server_instructions,
					precedence:          leaf.precedence,
					tier:                leaf.tier,
				}),
			})
			.collect();
		let epoch = self.leaves.replace(owner, version, leaves)?;
		self.definition_epoch.store(epoch, Ordering::Release);
		Ok(epoch)
	}

	/// Returns an immutable old-or-new leaf catalog snapshot.
	pub fn leaf_snapshot(&self) -> LeafCatalogSnapshot<McpLeaf> {
		self.leaves.snapshot()
	}

	/// Returns the current environment-wide definition epoch.
	pub fn definition_epoch(&self) -> u64 {
		self.definition_epoch.load(Ordering::Acquire)
	}

	/// Installs or replaces one supervisor-owned server generation. The caller
	/// supplies the catalog-published definition epoch; stale installation is
	/// fenced before mutation.
	pub fn install(
		&self,
		mut status: pb::McpServerStatus,
		backend: Arc<dyn McpServerBackend>,
	) -> Result<(), McpServiceError> {
		let server = status
			.server
			.as_mut()
			.ok_or(McpServiceError::InvalidRequest)?;
		if server.name.is_empty() {
			return Err(McpServiceError::InvalidRequest);
		}
		let published = self.definition_epoch();
		server.definition_epoch = published;
		status.definition_epoch = published;
		let name = Str::from(server.name.as_str());
		let mut state = self.state.write();
		if let Some(current) = state.servers.get(&name)
			&& current.status.generation > status.generation
		{
			return Err(McpServiceError::StaleGeneration);
		}
		self
			.definition_epoch
			.store(server.definition_epoch, Ordering::Release);
		state
			.servers
			.insert(name, ServerEntry { status: status.clone(), backend });
		broadcast(&mut state, SubscriptionEvent::Status(status));
		Ok(())
	}

	/// Returns a supervisor-installed backend for lifecycle status replacement.
	pub(crate) fn backend_for_manager(&self, name: &str) -> Option<Arc<dyn McpServerBackend>> {
		self
			.state
			.read()
			.servers
			.get(name)
			.map(|entry| Arc::clone(&entry.backend))
	}

	/// Removes one server only at its current definition epoch.
	pub fn remove(&self, server: &pb::McpServerRef) -> Result<bool, McpServiceError> {
		self.fence(server)?;
		let mut state = self.state.write();
		let removed = state.servers.remove(server.name.as_str()).is_some();
		if removed {
			let epoch = self.definition_epoch();
			broadcast(
				&mut state,
				SubscriptionEvent::Status(pb::McpServerStatus {
					server:           Some(pb::McpServerRef {
						name:             server.name.clone(),
						definition_epoch: epoch,
					}),
					state:            pb::McpLifecycleState::Stopped.into(),
					detail:           String::new(),
					generation:       0,
					definition_epoch: epoch,
				}),
			);
		}
		Ok(removed)
	}

	/// Publishes one sequenced server notification and retains bounded replay.
	pub fn notify(&self, notification: pb::McpNotification) -> Result<(), McpServiceError> {
		let server = notification
			.server
			.as_ref()
			.ok_or(McpServiceError::InvalidRequest)?;
		self.fence(server)?;
		let mut state = self.state.write();
		if let Some(previous) = state.history.iter().rev().find(|event| {
			event
				.server
				.as_ref()
				.is_some_and(|candidate| candidate.name == server.name)
		}) && notification.sequence <= previous.sequence
		{
			return Err(McpServiceError::StaleSequence);
		}
		if state.history.len() == NOTIFICATION_HISTORY {
			state.history.pop_front();
		}
		state.history.push_back(notification.clone());
		broadcast(&mut state, SubscriptionEvent::Notification(notification));
		Ok(())
	}

	/// Returns deterministic lifecycle status, optionally filtered by name.
	pub fn status(&self, name: Option<&str>) -> pb::McpStatusResult {
		let state = self.state.read();
		let definition_epoch = self.definition_epoch();
		let servers = state
			.servers
			.iter()
			.filter(|(server, _)| name.is_none_or(|wanted| wanted == server.as_str()))
			.map(|(_, entry)| {
				let mut status = entry.status.clone();
				if let Some(server) = status.server.as_mut() {
					server.definition_epoch = definition_epoch;
				}
				status.definition_epoch = definition_epoch;
				status
			})
			.collect();
		pb::McpStatusResult { servers, definition_epoch }
	}

	/// Opens a subscription after replaying retained notifications newer than
	/// `after_sequence`. A requested watermark older than retained history fails
	/// closed so callers reopen from a fresh status snapshot.
	pub fn subscribe(
		&self,
		name: Option<&str>,
		after_sequence: u64,
	) -> Result<ServiceSubscription, McpServiceError> {
		let (sender, receiver) = flume::bounded(SUBSCRIBER_CAPACITY);
		let mut state = self.state.write();
		if after_sequence != 0 {
			let oldest = state
				.history
				.iter()
				.filter(|event| {
					name.is_none_or(|wanted| {
						event
							.server
							.as_ref()
							.is_some_and(|server| server.name == wanted)
					})
				})
				.map(|event| event.sequence)
				.min();
			if oldest.is_some_and(|oldest| after_sequence.saturating_add(1) < oldest) {
				return Err(McpServiceError::ContinuityLost);
			}
		}
		for event in state.history.iter().filter(|event| {
			event.sequence > after_sequence
				&& name.is_none_or(|wanted| {
					event
						.server
						.as_ref()
						.is_some_and(|server| server.name == wanted)
				})
		}) {
			if sender
				.try_send(SubscriptionEvent::Notification(event.clone()))
				.is_err()
			{
				return Err(McpServiceError::ContinuityLost);
			}
		}
		let id = self.next_subscriber.fetch_add(1, Ordering::Relaxed);
		state
			.subscribers
			.insert(id, Subscriber { name: name.map(Str::from), sender });
		Ok(ServiceSubscription { receiver })
	}

	/// Resets one current server generation.
	pub async fn reset(
		&self,
		request: pb::McpResetRequest,
		cancel: CancellationToken,
	) -> Result<pb::McpResetResult, McpServiceError> {
		let server = request
			.server
			.as_ref()
			.ok_or(McpServiceError::InvalidRequest)?;
		let backend = self.backend(server)?;
		backend.reset(cancel).await?;
		let status = self
			.state
			.read()
			.servers
			.get(server.name.as_str())
			.map(|entry| entry.status.clone())
			.ok_or(McpServiceError::ServerNotFound)?;
		Ok(pb::McpResetResult { status: Some(status) })
	}

	/// Resolves origin-scoped headers for one current server generation.
	pub async fn live_header(
		&self,
		request: pb::McpLiveHeaderRequest,
		cancel: CancellationToken,
	) -> Result<pb::McpLiveHeader, McpServiceError> {
		let backend = self.backend(
			request
				.server
				.as_ref()
				.ok_or(McpServiceError::InvalidRequest)?,
		)?;
		backend.live_header(request, cancel).await
	}

	/// Reads one MCP resource.
	pub async fn resource(
		&self,
		request: pb::McpResourceRequest,
		cancel: CancellationToken,
	) -> Result<pb::McpResourceResult, McpServiceError> {
		let backend = self.backend(
			request
				.server
				.as_ref()
				.ok_or(McpServiceError::InvalidRequest)?,
		)?;
		backend.resource(request, cancel).await
	}

	/// Gets one MCP prompt.
	pub async fn prompt(
		&self,
		request: pb::McpPromptRequest,
		cancel: CancellationToken,
	) -> Result<pb::McpPromptResult, McpServiceError> {
		let backend = self.backend(
			request
				.server
				.as_ref()
				.ok_or(McpServiceError::InvalidRequest)?,
		)?;
		backend.prompt(request, cancel).await
	}

	/// Invokes one MCP tool.
	pub async fn invoke(
		&self,
		request: pb::McpInvokeRequest,
		cancel: CancellationToken,
	) -> Result<pb::McpInvokeResult, McpServiceError> {
		let backend = self.backend(
			request
				.server
				.as_ref()
				.ok_or(McpServiceError::InvalidRequest)?,
		)?;
		backend.invoke(request, cancel).await
	}

	fn backend(
		&self,
		server: &pb::McpServerRef,
	) -> Result<Arc<dyn McpServerBackend>, McpServiceError> {
		self.fence(server)?;
		self
			.state
			.read()
			.servers
			.get(server.name.as_str())
			.map(|entry| Arc::clone(&entry.backend))
			.ok_or(McpServiceError::ServerNotFound)
	}

	fn fence(&self, server: &pb::McpServerRef) -> Result<(), McpServiceError> {
		let expected = self.definition_epoch();
		if server.definition_epoch != expected {
			return Err(McpServiceError::StaleDefinitionEpoch {
				expected,
				actual: server.definition_epoch,
			});
		}
		Ok(())
	}
}

#[derive(Clone)]
struct McpConfigPaths {
	user:    PathBuf,
	project: PathBuf,
	root:    PathBuf,
}

fn load_resolved_config(
	paths: McpConfigPaths,
	enable_project_config: bool,
) -> Result<config::ResolvedConfig, McpServiceError> {
	use config::{ConfigSource, ConfigSourceKind};

	let sources = [
		(ConfigSourceKind::User, paths.user),
		(ConfigSourceKind::Project, paths.project),
		(ConfigSourceKind::Root, paths.root),
	]
	.into_iter()
	.map(|(kind, path)| {
		let file = config_store::McpConfigStore::new(path.clone())
			.read()
			.map_err(|_| McpServiceError::InvalidRequest)?;
		Ok(ConfigSource { path, kind, file })
	})
	.collect::<Result<Vec<_>, McpServiceError>>()?;
	Ok(config::resolve_sources(&sources, enable_project_config))
}

fn config_request(
	paths: McpConfigPaths,
	request: pb::McpConfigRequest,
) -> Result<pb::McpConfigResult, McpServiceError> {
	use config_store::{McpConfigStore, set_server_enabled};

	let user = McpConfigStore::new(paths.user);
	let project = McpConfigStore::new(paths.project);
	let root = McpConfigStore::new(paths.root);
	let action =
		pb::McpConfigAction::try_from(request.action).map_err(|_| McpServiceError::InvalidRequest)?;
	let scope =
		pb::McpConfigScope::try_from(request.scope).map_err(|_| McpServiceError::InvalidRequest)?;
	if scope == pb::McpConfigScope::Unspecified
		&& matches!(
			action,
			pb::McpConfigAction::Add | pb::McpConfigAction::Update | pb::McpConfigAction::Remove
		) {
		return Err(McpServiceError::InvalidRequest);
	}
	let stores: Vec<(pb::McpConfigScope, &McpConfigStore)> = match scope {
		pb::McpConfigScope::Unspecified => vec![
			(pb::McpConfigScope::Project, &project),
			(pb::McpConfigScope::User, &user),
			(pb::McpConfigScope::Root, &root),
		],
		pb::McpConfigScope::User => vec![(scope, &user)],
		pb::McpConfigScope::Project => vec![(scope, &project)],
		pb::McpConfigScope::Root => vec![(scope, &root)],
	};
	let selected = || {
		stores
			.first()
			.map(|(_, store)| *store)
			.ok_or(McpServiceError::InvalidRequest)
	};
	let parse = || {
		serde_json::from_slice::<config::McpServerConfig>(&request.server_json)
			.map_err(|_| McpServiceError::InvalidRequest)
	};
	match action {
		pb::McpConfigAction::Unspecified => return Err(McpServiceError::InvalidRequest),
		pb::McpConfigAction::Add => selected()?
			.add(&request.name, parse()?)
			.map_err(|_| McpServiceError::InvalidRequest)?,
		pb::McpConfigAction::Update => selected()?
			.update(&request.name, parse()?)
			.map_err(|_| McpServiceError::InvalidRequest)?,
		pb::McpConfigAction::Remove => selected()?
			.remove(&request.name)
			.map_err(|_| McpServiceError::InvalidRequest)?,
		pb::McpConfigAction::Enable | pb::McpConfigAction::Disable => set_server_enabled(
			&user,
			&project,
			Some((&root, true)),
			&request.name,
			action == pb::McpConfigAction::Enable,
		)
		.map_err(|_| McpServiceError::InvalidRequest)?,
		pb::McpConfigAction::Get | pb::McpConfigAction::List => {},
	}
	let mut entries = Vec::new();
	for (scope, store) in stores {
		let file = store.read().map_err(|_| McpServiceError::InvalidRequest)?;
		for (name, server) in file.mcp_servers {
			if action == pb::McpConfigAction::Get && name.as_str() != request.name {
				continue;
			}
			let server_json =
				serde_json::to_vec(&server).map_err(|_| McpServiceError::InvalidRequest)?;
			entries.push(pb::McpConfigEntry {
				scope:       scope as i32,
				name:        name.to_string(),
				server_json: server_json.into(),
				writable:    true,
			});
		}
		if action == pb::McpConfigAction::Get && !entries.is_empty() {
			break;
		}
	}
	Ok(pb::McpConfigResult { entries })
}

impl fmt::Debug for McpService {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("McpService")
			.field("definition_epoch", &self.definition_epoch())
			.field("servers", &self.state.read().servers.len())
			.finish()
	}
}

/// Environment MCP service failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum McpServiceError {
	/// Revisioned leaf replacement was fenced or conflicted.
	#[error(transparent)]
	LeafReplacement(#[from] LeafReplacementError),
	/// Request omitted or contradicted required typed fields.
	#[error("MCP request is invalid")]
	InvalidRequest,
	/// Requested server is not mounted in this Environment.
	#[error("MCP server is not mounted")]
	ServerNotFound,
	/// Request names an obsolete catalog definition epoch.
	#[error("MCP definition epoch is stale: expected {expected}, received {actual}")]
	StaleDefinitionEpoch {
		/// Current environment epoch.
		expected: u64,
		/// Request epoch.
		actual:   u64,
	},
	/// Supervisor generation was superseded.
	#[error("MCP server generation is stale")]
	StaleGeneration,
	/// Notification sequence is not monotone.
	#[error("MCP notification sequence is stale")]
	StaleSequence,
	/// Subscription replay cannot prove continuity.
	#[error("MCP notification continuity was lost")]
	ContinuityLost,
	/// Caller cancelled an operation.
	#[error("MCP operation was cancelled")]
	Cancelled,
	/// Definition epoch counter exhausted.
	#[error("MCP definition epoch is exhausted")]
	EpochExhausted,
	/// Mounted transport/backend failed without secret-bearing detail.
	#[error("MCP server operation failed")]
	Backend,
}

fn broadcast(state: &mut State, event: SubscriptionEvent) {
	state.subscribers.retain(|_, subscriber| {
		let matches = subscriber.name.as_ref().is_none_or(|wanted| match &event {
			SubscriptionEvent::Notification(notification) => notification
				.server
				.as_ref()
				.is_some_and(|server| server.name == wanted.as_str()),
			SubscriptionEvent::Status(status) => status
				.server
				.as_ref()
				.is_some_and(|server| server.name == wanted.as_str()),
		});
		!matches || subscriber.sender.try_send(event.clone()).is_ok()
	});
}

#[cfg(test)]
mod config_tests {
	use std::{fs, future::Future, pin::Pin};

	use super::*;
	use crate::mcp::manager::{ConnectedClient, ManagerError, McpConnector, McpManager, MountSpec};

	struct RejectConnector;

	impl McpConnector for RejectConnector {
		fn connect<'a>(
			&'a self,
			_spec: &'a MountSpec,
			_roots: Arc<[Str]>,
			_cancel: CancellationToken,
		) -> Pin<Box<dyn Future<Output = Result<ConnectedClient, ManagerError>> + Send + 'a>> {
			Box::pin(async { Err(ManagerError::InvalidConfig) })
		}
	}

	#[tokio::test]
	async fn startup_loads_persisted_sources_and_honors_project_policy() {
		let scratch = tempfile::tempdir().expect("scratch");
		let data = scratch.path().join("data");
		let project = scratch.path().join("project");
		fs::create_dir_all(project.join(".omp")).expect("project config directory");
		fs::create_dir_all(&data).expect("data directory");
		fs::write(
			data.join("mcp.json"),
			br#"{"mcpServers":{"user-server":{"type":"stdio","command":"user"}}}"#,
		)
		.expect("user config");
		fs::write(
			project.join(".mcp.json"),
			br#"{"mcpServers":{"root-server":{"type":"stdio","command":"root"}}}"#,
		)
		.expect("root config");
		let service = McpService::open(scratch.path().join("cache.sqlite3")).expect("service");
		service.bind_config_paths(&data, &project);
		let manager = McpManager::new(
			Arc::clone(&service),
			Arc::new(RejectConnector),
			Arc::from([]),
			project.clone(),
		);
		service.bind_manager(&manager);

		let snapshot = service
			.start_native_configs(false)
			.await
			.expect("startup config");
		assert_eq!(snapshot.status.servers.len(), 1);
		assert_eq!(
			snapshot.status.servers[0]
				.server
				.as_ref()
				.expect("server")
				.name,
			"user-server"
		);
	}

	#[tokio::test]
	async fn native_config_rpc_mutates_and_lists_one_environment_store() {
		let scratch = tempfile::tempdir().expect("scratch");
		let data = scratch.path().join("data");
		let project = scratch.path().join("project");
		fs::create_dir_all(&project).expect("project");
		let service = McpService::open(scratch.path().join("cache.sqlite3")).expect("service");
		service.bind_config_paths(&data, &project);
		service
			.config(pb::McpConfigRequest {
				action:        pb::McpConfigAction::Add as i32,
				scope:         pb::McpConfigScope::Project as i32,
				name:          "fixture".to_owned(),
				server_json:   br#"{"type":"stdio","command":"fixture"}"#[..].into(),
				wire_revision: omp_proto::SCHEMA_REV,
			})
			.await
			.expect("add");
		let listed = service
			.config(pb::McpConfigRequest {
				action: pb::McpConfigAction::List as i32,
				scope: pb::McpConfigScope::Project as i32,
				wire_revision: omp_proto::SCHEMA_REV,
				..pb::McpConfigRequest::default()
			})
			.await
			.expect("list");
		assert_eq!(listed.entries.len(), 1);
		assert_eq!(listed.entries[0].name, "fixture");
		assert!(listed.entries[0].writable);
	}
}
