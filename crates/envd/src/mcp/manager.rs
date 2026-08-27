//! Environment-scoped MCP lifecycle supervisor and definition catalog.

use std::{
	collections::{BTreeMap, BTreeSet, VecDeque},
	ffi::OsString,
	path::{Path, PathBuf},
	pin::Pin,
	sync::{Arc, Weak, atomic},
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures::{
	Future, FutureExt as _, StreamExt as _, future::BoxFuture, stream::FuturesUnordered,
};
use http::{HeaderMap, HeaderName, HeaderValue, header::WWW_AUTHENTICATE};
use omp_core::Str;
use omp_inference::{
	auth::{
		AuthControlHandle, StoreError,
		command::{CommandCredentialExecutor, CommandCredentialResolver},
	},
	id::PrincipalId,
};
use omp_oauth::{AuthChallenge, ChallengeKind, discover_auth_challenge};
use omp_proto::env::v1 as pb;
use omp_tool::{LeafOwner, LeafVersion};
use parking_lot::{Mutex, RwLock};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::{runtime, sync::Notify, task, time};
use tokio_util::sync::CancellationToken;
use url::Url;

mod control_gate {
	use tokio::sync::{Mutex, MutexGuard};

	pub(super) struct ControlGate(Mutex<()>);

	impl ControlGate {
		pub(super) fn new() -> Self {
			Self(Mutex::new(()))
		}

		pub(super) async fn lock(&self) -> MutexGuard<'_, ()> {
			self.0.lock().await
		}
	}
}

use control_gate::ControlGate;

use super::{
	super::exthost::control::ControlConnectionIdentity,
	McpLeaf, McpServerBackend, McpService, McpServiceError,
	auth_authority::CombinedAuthAuthority,
	client::{ClientError, InitializedServer, McpClient},
	config::{
		AuthConfig, AuthKind, EnvironmentPolicy, HeaderPolicy, McpServerConfig,
		RequestIdFormat as ConfigRequestIdFormat, ResolvedConfig, TransportKind, validate_server,
	},
	config_values::{
		ConfigValueError, ResolvedConfigValue, ResolvedTransportValues, resolve_transport_values,
	},
	control::{ControlMountResolver, McpControlError},
	device::{DeviceError, McpDeviceDefinitions, McpDeviceProjection},
	filter::{NativeCoverage, filter_native_coverage, import_exa_keys},
	http::{RefreshableHeaders, ReqwestExchange, StreamableHttpConfig, StreamableHttpTransport},
	invoke,
	json_rpc::RequestIdFormat,
	legacy_sse::{LegacySseConfig, LegacySseTransport},
	oauth::{AuthorityHeaders, McpOAuth, OAuthAttempt, OAuthFlowError},
	prompts::{PromptContent, PromptDefinition, PromptError, PromptsClient},
	resources::{
		ResourceDefinition, ResourceError, ResourceTemplate, ResourcesClient, template_match_score,
	},
	stdio::{StdioConfig, StdioTransport},
	timeout::{McpDeadlineError, McpTimeout},
	transport::{McpTransport, TransportError, TransportFailure},
};

const STARTUP_RACE: Duration = Duration::from_millis(250);
const RECONNECT_WINDOW: Duration = Duration::from_secs(30);
const RECONNECT_BURST_LIMIT: usize = 5;
const RECONNECT_DELAYS: [Duration; 4] = [
	Duration::from_millis(500),
	Duration::from_secs(1),
	Duration::from_secs(2),
	Duration::from_secs(4),
];
const MAX_INSTRUCTIONS_BYTES: usize = 10_000;
const MAX_TOOL_PAGES: usize = 1_024;

/// Fully resolved declaration mounted into one Environment supervisor.
#[derive(Clone)]
pub struct MountSpec {
	/// Stable server name.
	pub name:             Str,
	/// Validated declaration used as the persistent cache identity.
	pub config:           Arc<McpServerConfig>,
	/// Canonical original declaration JSON, without resolved credential bytes.
	pub config_json:      Bytes,
	/// Secret-typed dynamic values exposed only during transport construction.
	pub values:           ResolvedTransportValues,
	/// Optional live combined-authority header lease for HTTP-like transports.
	pub auth_headers:     Option<Arc<dyn RefreshableHeaders>>,
	/// Native tools that this mount must not publish through the generic MCP
	/// device because an exact native implementation already owns them.
	pub suppressed_tools: BTreeSet<Str>,
	/// Full Python endpoint projection and device policy.
	pub projection:       Arc<McpDeviceProjection>,
	/// Credential requirement retained without credential bytes.
	pub auth:             ControlMountAuth,
	/// Declared child reconnect behavior.
	pub restart:          McpRestartPolicy,
	/// Authenticated extension owner for CONTROL mounts; native mounts have no
	/// extension owner.
	pub owner:            Option<Arc<ControlConnectionIdentity>>,
}
/// Credential requirement retained from one Python mount declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlMountAuth {
	/// No transport credential.
	None,
	/// Environment-owned OAuth flow with exact requested scopes.
	OAuth {
		/// Exact scopes the authenticated extension asks the Environment
		/// credential authority to grant.
		scopes: Box<[Str]>,
	},
	/// Environment-owned named API key.
	ApiKey {
		/// Environment-owned credential name to resolve; this is an authority
		/// lookup key, not secret bytes.
		name: Str,
	},
}

/// Automatic reconnect behavior retained from one Python mount.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum McpRestartPolicy {
	/// Never reconnect after the transport closes.
	Never,
	/// Reconnect after an unexpected transport failure.
	#[default]
	OnFailure,
	/// Reconnect whenever the transport exits.
	Always,
}

/// Extension-scoped declaration resolver backed by the live MCP supervisor.
pub struct ManagerControlMountResolver {
	manager:      Arc<McpManager>,
	identity:     Arc<ControlConnectionIdentity>,
	cancellation: CancellationToken,
}

impl ManagerControlMountResolver {
	/// Binds Python declarations to one authenticated extension generation.
	pub fn new(
		manager: Arc<McpManager>,
		identity: Arc<ControlConnectionIdentity>,
		cancellation: CancellationToken,
	) -> Self {
		if let Ok(runtime) = runtime::Handle::try_current() {
			let weak = Arc::downgrade(&manager);
			let owner = Arc::clone(&identity);
			let cancelled = cancellation.clone();
			runtime.spawn(async move {
				cancelled.cancelled().await;
				if let Some(manager) = weak.upgrade() {
					manager.control_unmount_all(&owner).await;
				}
			});
		}
		Self { manager, identity, cancellation }
	}

	/// Returns the exact authenticated owner stamped onto resolved mounts.
	pub fn identity(&self) -> &Arc<ControlConnectionIdentity> {
		&self.identity
	}
}

impl ControlMountResolver for ManagerControlMountResolver {
	fn resolve<'a>(
		&'a self,
		declaration: Value,
	) -> Pin<Box<dyn Future<Output = Result<MountSpec, McpControlError>> + Send + 'a>> {
		Box::pin(async move {
			if self.cancellation.is_cancelled() || self.manager.shutdown.is_cancelled() {
				return Err(McpControlError::Manager(ManagerError::Cancelled));
			}
			let config_json = serde_json::to_vec(&declaration)
				.map(Bytes::from)
				.map_err(|_| McpControlError::DeclarationRejected)?;
			let declaration: ControlMountDeclaration = serde_json::from_value(declaration)
				.map_err(|_| McpControlError::DeclarationRejected)?;
			let mut spec = declaration.resolve(Arc::clone(&self.identity), config_json)?;
			spec.values = self
				.manager
				.resolve_values(&spec.config, &self.cancellation)
				.await
				.map_err(McpControlError::Manager)?;
			Ok(spec)
		})
	}
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlMountDeclaration {
	server:     Str,
	transport:  ControlTransport,
	#[serde(default)]
	auth:       ControlAuthDeclaration,
	#[serde(default = "default_include")]
	include:    Vec<Str>,
	#[serde(default)]
	exclude:    Vec<Str>,
	#[serde(default)]
	rename:     BTreeMap<Str, Str>,
	#[serde(default)]
	docs:       BTreeMap<Str, Str>,
	#[serde(default)]
	precedence: i32,
	#[serde(default = "default_tier")]
	tier:       Str,
	#[serde(default = "default_control_timeout")]
	timeout:    Str,
	#[serde(default = "default_restart")]
	restart:    Str,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum ControlTransport {
	Stdio {
		command: Str,
		#[serde(default)]
		args:    Vec<Str>,
		#[serde(default)]
		env:     BTreeMap<Str, Str>,
		#[serde(default)]
		cwd:     Option<PathBuf>,
	},
	Http {
		url:     Str,
		#[serde(default)]
		headers: BTreeMap<Str, Str>,
	},
	Sse {
		url:     Str,
		#[serde(default)]
		headers: BTreeMap<Str, Str>,
	},
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ControlAuthDeclaration {
	#[serde(rename = "oauth")]
	OAuth {
		#[serde(default)]
		scopes: Vec<Str>,
		#[serde(default)]
		name:   Option<Str>,
	},
	ApiKey {
		#[serde(default)]
		scopes: Vec<Str>,
		name:   Option<Str>,
	},
	None {
		#[serde(default)]
		scopes: Vec<Str>,
		#[serde(default)]
		name:   Option<Str>,
	},
}

impl Default for ControlAuthDeclaration {
	fn default() -> Self {
		Self::None { scopes: Vec::new(), name: None }
	}
}

fn default_include() -> Vec<Str> {
	vec![Str::new_static("*")]
}

fn default_tier() -> Str {
	Str::new_static("write")
}

fn default_control_timeout() -> Str {
	Str::new_static("30s")
}

fn default_restart() -> Str {
	Str::new_static("on-failure")
}

impl ControlMountDeclaration {
	fn resolve(
		self,
		identity: Arc<ControlConnectionIdentity>,
		config_json: Bytes,
	) -> Result<MountSpec, McpControlError> {
		if !valid_device_segment(&self.server)
			|| !unique_clean(&self.include)
			|| !unique_clean(&self.exclude)
			|| self
				.include
				.iter()
				.chain(&self.exclude)
				.any(|pattern| pattern.contains('/'))
			|| !matches!(self.precedence, -500 | 0 | 500 | 700 | 1000)
			|| !matches!(self.tier.as_str(), "read" | "write" | "exec" | "privileged")
		{
			return Err(McpControlError::DeclarationRejected);
		}
		let mut targets = BTreeSet::new();
		for (endpoint, device) in &self.rename {
			if !clean_nonempty(endpoint)
				|| endpoint.contains('/')
				|| !valid_device_segment(device)
				|| !targets.insert(device)
			{
				return Err(McpControlError::DeclarationRejected);
			}
		}
		if self.docs.iter().any(|(endpoint, documentation)| {
			!clean_nonempty(endpoint) || !clean_text(documentation, true)
		}) {
			return Err(McpControlError::DeclarationRejected);
		}
		let timeout = self
			.timeout
			.parse::<omp_core::Duration>()
			.and_then(omp_core::Duration::to_std)
			.map_err(|_| McpControlError::DeclarationRejected)?;
		if !timeout.subsec_nanos().is_multiple_of(1_000_000) {
			return Err(McpControlError::DeclarationRejected);
		}
		let timeout_ms =
			u64::try_from(timeout.as_millis()).map_err(|_| McpControlError::DeclarationRejected)?;
		let restart = match self.restart.as_str() {
			"no" => McpRestartPolicy::Never,
			"on-failure" => McpRestartPolicy::OnFailure,
			"always" => McpRestartPolicy::Always,
			_ => return Err(McpControlError::DeclarationRejected),
		};
		let (auth, config_auth) = resolve_control_auth(self.auth)?;
		let (transport, command, args, env, cwd, url, headers) = match self.transport {
			ControlTransport::Stdio { command, args, env, cwd } => {
				require_capability(&identity, "env.process")?;
				if !clean_nonempty(&command)
					|| args.iter().any(|value| !clean_nonempty(value))
					|| env
						.iter()
						.any(|(name, value)| !clean_nonempty(name) || !clean_text(value, true))
					|| cwd.as_ref().is_some_and(|path| path.as_os_str().is_empty())
				{
					return Err(McpControlError::DeclarationRejected);
				}
				(Some(TransportKind::Stdio), Some(command), args, env, cwd, None, BTreeMap::new())
			},
			ControlTransport::Http { url, headers } => {
				require_capability(&identity, "env.net")?;
				validate_remote(&url, &headers, &auth)?;
				(Some(TransportKind::Http), None, Vec::new(), BTreeMap::new(), None, Some(url), headers)
			},
			ControlTransport::Sse { url, headers } => {
				require_capability(&identity, "env.net")?;
				validate_remote(&url, &headers, &auth)?;
				(Some(TransportKind::Sse), None, Vec::new(), BTreeMap::new(), None, Some(url), headers)
			},
		};
		let config = McpServerConfig {
			transport,
			enabled: true,
			command,
			args,
			env,
			env_policy: Some(EnvironmentPolicy::Literal),
			cwd,
			url,
			headers,
			header_policy: Some(HeaderPolicy::OriginLocked),
			timeout: Some(timeout_ms),
			request_id_format: None,
			auth: config_auth,
			oauth: None,
			protocol_versions: Vec::new(),
		};
		if !validate_server(&self.server, &config).is_empty() {
			return Err(McpControlError::DeclarationRejected);
		}
		let values = ResolvedTransportValues::default();
		let projection = McpDeviceProjection::new(
			&self.include,
			&self.exclude,
			self.rename,
			self.docs,
			self.precedence,
			self.tier,
		)
		.map(Arc::new)
		.map_err(|_| McpControlError::DeclarationRejected)?;
		Ok(MountSpec {
			name: self.server,
			config: Arc::new(config),
			config_json,
			values,
			auth_headers: None,
			suppressed_tools: BTreeSet::new(),
			projection,
			auth,
			restart,
			owner: Some(identity),
		})
	}
}

fn resolve_control_auth(
	auth: ControlAuthDeclaration,
) -> Result<(ControlMountAuth, Option<AuthConfig>), McpControlError> {
	match auth {
		ControlAuthDeclaration::OAuth { scopes, name } => {
			if name.is_some() || !unique_clean(&scopes) {
				return Err(McpControlError::DeclarationRejected);
			}
			Ok((
				ControlMountAuth::OAuth { scopes: scopes.into_boxed_slice() },
				Some(AuthConfig {
					kind:          AuthKind::Oauth,
					credential_id: None,
					token_url:     None,
					client_id:     None,
					secret_ref:    None,
					resource:      None,
				}),
			))
		},
		ControlAuthDeclaration::ApiKey { scopes, name } => {
			let Some(name) = name.filter(|name| clean_nonempty(name)) else {
				return Err(McpControlError::DeclarationRejected);
			};
			if !scopes.is_empty() {
				return Err(McpControlError::DeclarationRejected);
			}
			Ok((
				ControlMountAuth::ApiKey { name: name.clone() },
				Some(AuthConfig {
					kind:          AuthKind::Apikey,
					credential_id: Some(name),
					token_url:     None,
					client_id:     None,
					secret_ref:    None,
					resource:      None,
				}),
			))
		},
		ControlAuthDeclaration::None { scopes, name } => {
			if !scopes.is_empty() || name.is_some() {
				return Err(McpControlError::DeclarationRejected);
			}
			Ok((ControlMountAuth::None, None))
		},
	}
}

fn require_capability(
	identity: &ControlConnectionIdentity,
	capability: &str,
) -> Result<(), McpControlError> {
	if identity.capabilities.contains(capability) {
		Ok(())
	} else {
		Err(McpControlError::DeclarationRejected)
	}
}

fn validate_remote(
	raw_url: &str,
	headers: &BTreeMap<Str, Str>,
	auth: &ControlMountAuth,
) -> Result<(), McpControlError> {
	let url = Url::parse(raw_url).map_err(|_| McpControlError::DeclarationRejected)?;
	if !matches!(url.scheme(), "http" | "https")
		|| url.host_str().is_none()
		|| !url.username().is_empty()
		|| url.password().is_some()
		|| url.fragment().is_some()
		|| headers
			.iter()
			.any(|(name, value)| !clean_nonempty(name) || !clean_text(value, true))
		|| !matches!(auth, ControlMountAuth::None)
			&& headers
				.keys()
				.any(|name| name.eq_ignore_ascii_case("authorization"))
	{
		return Err(McpControlError::DeclarationRejected);
	}
	Ok(())
}

fn unique_clean(values: &[Str]) -> bool {
	let mut unique = BTreeSet::new();
	values
		.iter()
		.all(|value| clean_nonempty(value) && unique.insert(value))
}

fn clean_nonempty(value: &str) -> bool {
	!value.is_empty() && clean_text(value, false)
}

fn clean_text(value: &str, allow_empty: bool) -> bool {
	(allow_empty || !value.is_empty())
		&& !value
			.bytes()
			.any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
}

fn valid_device_segment(value: &str) -> bool {
	(1..=64).contains(&value.len())
		&& value.as_bytes()[0].is_ascii_lowercase()
		&& value
			.bytes()
			.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}
/// One sanitized startup-race observation.
#[derive(Clone, Debug)]
pub struct StartupSnapshot {
	/// Deterministically ordered server status after the startup race.
	pub status:    pb::McpStatusResult,
	/// Whether every initial connection completed before 250 ms.
	pub completed: bool,
}

/// Connected initialized client returned by a transport connector.
pub struct ConnectedClient {
	/// Initialized protocol client.
	pub client:      Arc<McpClient>,
	/// Negotiated server facts.
	pub initialized: InitializedServer,
}

/// Cold transport-construction boundary used by the supervisor.
pub trait McpConnector: Send + Sync {
	/// Connects and initializes one server.
	fn connect<'a>(
		&'a self,
		spec: &'a MountSpec,
		roots: Arc<[Str]>,
		cancel: CancellationToken,
	) -> Pin<Box<dyn Future<Output = Result<ConnectedClient, ManagerError>> + Send + 'a>>;
}

/// Combined-authority hook for tool-level `mcp/www_authenticate` challenges.
pub trait McpAuthChallengeHandler: Send + Sync {
	/// Refreshes the credential lease named by one server response.
	fn refresh<'a>(
		&'a self,
		server: &'a str,
		challenges: &'a [Str],
		cancel: CancellationToken,
	) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

/// Production connector for stdio, Streamable HTTP, and legacy HTTP+SSE.
pub struct ProductionConnector {
	workspace_root: PathBuf,
	http:           Arc<ReqwestExchange>,
}

impl ProductionConnector {
	/// Creates a connector whose relative stdio paths belong to this
	/// Environment.
	pub fn new(workspace_root: PathBuf) -> Self {
		Self { workspace_root, http: Arc::new(ReqwestExchange::new()) }
	}
}

impl McpConnector for ProductionConnector {
	fn connect<'a>(
		&'a self,
		spec: &'a MountSpec,
		roots: Arc<[Str]>,
		cancel: CancellationToken,
	) -> Pin<Box<dyn Future<Output = Result<ConnectedClient, ManagerError>> + Send + 'a>> {
		Box::pin(async move {
			let request_id_format = match spec.config.request_id_format.unwrap_or_default() {
				ConfigRequestIdFormat::Number => RequestIdFormat::Number,
				ConfigRequestIdFormat::String => RequestIdFormat::String,
			};
			let timeout = McpTimeout::resolve(None, spec.config.timeout).duration();
			let transport: Arc<dyn McpTransport> = match spec.config.resolved_transport() {
				TransportKind::Stdio => {
					let command = spec
						.config
						.command
						.as_ref()
						.ok_or(ManagerError::InvalidConfig)?;
					let cwd = spec.config.cwd.as_ref().map_or_else(
						|| self.workspace_root.clone(),
						|cwd| {
							if cwd.is_absolute() {
								cwd.clone()
							} else {
								self.workspace_root.join(cwd)
							}
						},
					);
					let env = expose_env(&spec.values.env);
					Arc::new(
						StdioTransport::spawn(StdioConfig {
							command: PathBuf::from(command.as_str()),
							args: spec.config.args.clone(),
							env,
							cwd,
							timeout,
							request_id_format,
						})
						.await?,
					)
				},
				TransportKind::Http => {
					let url = parse_url(&spec.config)?;
					let headers = expose_headers(&spec.values.headers)?;
					Arc::new(StreamableHttpTransport::new(
						StreamableHttpConfig {
							url,
							headers,
							origin_locked: spec.config.header_policy.is_some(),
							timeout,
							request_id_format,
							auth: spec.auth_headers.clone(),
						},
						self.http.clone(),
					)?)
				},
				TransportKind::Sse => {
					let url = parse_url(&spec.config)?;
					let headers = expose_headers(&spec.values.headers)?;
					Arc::new(
						LegacySseTransport::connect(
							LegacySseConfig {
								url,
								headers,
								origin_locked: spec.config.header_policy.is_some(),
								timeout,
								request_id_format,
								auth: spec.auth_headers.clone(),
							},
							self.http.clone(),
							cancel.child_token(),
						)
						.await?,
					)
				},
			};
			let client = Arc::new(McpClient::new(transport, roots));
			let initialized = client.initialize(cancel).await?;
			Ok(ConnectedClient { client, initialized })
		})
	}
}

fn expose_env(values: &BTreeMap<Str, ResolvedConfigValue>) -> BTreeMap<Str, OsString> {
	values
		.iter()
		.map(|(name, value)| {
			let exposed = value.with_exposed(|text| OsString::from(text));
			(name.clone(), exposed)
		})
		.collect()
}

fn expose_headers(values: &BTreeMap<Str, ResolvedConfigValue>) -> Result<HeaderMap, ManagerError> {
	let mut headers = HeaderMap::new();
	for (name, value) in values {
		let name =
			HeaderName::from_bytes(name.as_bytes()).map_err(|_| ManagerError::InvalidConfig)?;
		let value = value
			.with_exposed(HeaderValue::from_str)
			.map_err(|_| ManagerError::InvalidConfig)?;
		headers.insert(name, value);
	}
	Ok(headers)
}

fn parse_url(config: &McpServerConfig) -> Result<Url, ManagerError> {
	let raw = config.url.as_deref().ok_or(ManagerError::InvalidConfig)?;
	Url::parse(raw).map_err(|_| ManagerError::InvalidConfig)
}

/// Read-only lifecycle state projected for the `/extensions` inspector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpInspectorHealth {
	/// Initial connection or reconnect is in progress.
	Connecting,
	/// A live connection owns the current catalogs.
	Connected,
	/// No live connection currently exists.
	Disconnected,
	/// Automatic reconnects stopped after terminal failure.
	Failed,
}

/// One immutable MCP server/catalog snapshot for presentation.
#[derive(Clone, Debug)]
pub struct McpInspectorSnapshot {
	/// Declared server name.
	pub server:           Str,
	/// Current lifecycle state.
	pub health:           McpInspectorHealth,
	/// Mount generation.
	pub generation:       u64,
	/// Shared definition catalog epoch.
	pub definition_epoch: u64,
	/// Negotiated server implementation name.
	pub implementation:   Option<Str>,
	/// Negotiated server implementation version.
	pub version:          Option<Str>,
	/// Server-provided display title.
	pub title:            Option<Str>,
	/// Server-provided description.
	pub description:      Option<Str>,
	/// Initialize instructions.
	pub instructions:     Option<Str>,
	/// Raw MCP tool definitions.
	pub tools:            Arc<[Value]>,
	/// Concrete resource definitions.
	pub resources:        Arc<[ResourceDefinition]>,
	/// Prompt definitions.
	pub prompts:          Arc<[PromptDefinition]>,
}

/// Post-handling MCP notification offered to Core-owned hook dispatch.
#[derive(Clone, Debug)]
pub struct McpHookNotification {
	/// Raw mounted server name.
	pub server:   Str,
	/// Complete JSON-RPC notification method.
	pub method:   Str,
	/// Validated opaque JSON parameters.
	pub params:   Value,
	/// Monotonic sequence local to the mounted server.
	pub sequence: u64,
}

/// Non-blocking sink for post-handling MCP hook observations.
pub trait McpNotificationSink: Send + Sync {
	/// Returns whether any mirrored static filter admits the raw notification.
	fn interested(&self, server: &str, method: &str) -> bool;
	/// Offers one notification after built-in refresh handling has completed.
	fn offer(&self, notification: McpHookNotification);
}

pub(crate) struct LiveConnection {
	pub(crate) client:      Arc<McpClient>,
	pub(crate) initialized: InitializedServer,
	tools:                  RwLock<Arc<[Value]>>,
	resources:              RwLock<Arc<[ResourceDefinition]>>,
	templates:              RwLock<Arc<[ResourceTemplate]>>,
	prompts:                RwLock<Arc<[PromptDefinition]>>,
}

struct MountState {
	spec:                  MountSpec,
	generation:            u64,
	definition_version:    u64,
	notification_sequence: u64,
	connection:            Option<Arc<LiveConnection>>,
	connecting:            bool,
	reconnecting:          bool,
	terminal_failure:      bool,
	reconnects:            VecDeque<Instant>,
	tools:                 Arc<[Value]>,
}

struct ManagerState {
	mounts: BTreeMap<Str, MountState>,
}

struct SubscriptionState {
	enabled: bool,
	epoch:   u64,
	active:  BTreeMap<Str, BTreeSet<Str>>,
}

/// Environment-owned multiprocess MCP supervisor.
pub struct McpManager {
	service:       Arc<McpService>,
	connector:     Arc<dyn McpConnector>,
	workspace:     Arc<[Str]>,
	local_root:    PathBuf,
	environment:   BTreeMap<Str, Str>,
	commands:      RwLock<Option<Arc<CommandCredentialResolver>>>,
	authority:     RwLock<Option<Arc<CombinedAuthAuthority>>>,
	native_auth:   RwLock<Option<AuthControlHandle>>,
	oauth:         RwLock<Option<Arc<McpOAuth>>>,
	state:         Mutex<ManagerState>,
	subscriptions: Mutex<SubscriptionState>,
	auth:          RwLock<Option<Arc<dyn McpAuthChallengeHandler>>>,
	notifications: RwLock<Option<Arc<dyn McpNotificationSink>>>,
	control_gate:  ControlGate,
	changed:       Notify,
	shutdown:      CancellationToken,
	generation:    atomic::AtomicU64,
}

impl McpManager {
	/// Creates an Environment-scoped supervisor. Call [`Self::start`] to mount a
	/// complete resolved declaration set.
	pub fn new(
		service: Arc<McpService>,
		connector: Arc<dyn McpConnector>,
		workspace: Arc<[Str]>,
		local_root: PathBuf,
	) -> Arc<Self> {
		Arc::new(Self {
			service,
			connector,
			workspace,
			local_root,
			environment: std::env::vars()
				.map(|(name, value)| (Str::from(name), Str::from(value)))
				.collect(),
			commands: RwLock::new(None),
			authority: RwLock::new(None),
			native_auth: RwLock::new(None),
			oauth: RwLock::new(None),
			state: Mutex::new(ManagerState { mounts: BTreeMap::new() }),
			subscriptions: Mutex::new(SubscriptionState {
				enabled: false,
				epoch:   0,
				active:  BTreeMap::new(),
			}),
			auth: RwLock::new(None),
			notifications: RwLock::new(None),
			control_gate: ControlGate::new(),
			changed: Notify::new(),
			shutdown: CancellationToken::new(),
			generation: atomic::AtomicU64::new(1),
		})
	}

	/// Binds Core-owned filtered hook delivery for server notifications.
	pub fn bind_notification_sink(&self, sink: Arc<dyn McpNotificationSink>) {
		*self.notifications.write() = Some(sink);
	}

	/// Binds the combined credential authority's reactive challenge hook.
	pub fn bind_auth_handler(&self, handler: Arc<dyn McpAuthChallengeHandler>) {
		*self.auth.write() = Some(handler);
	}

	/// Binds the composition-owned `!command` executor shared with model auth.
	pub fn bind_command_executor(&self, executor: Arc<dyn CommandCredentialExecutor>) {
		*self.commands.write() =
			Some(Arc::new(CommandCredentialResolver::new(executor, Duration::from_secs(1))));
	}

	/// Binds the MCP OAuth encrypted credential authority.
	pub fn bind_auth_authority(&self, authority: Arc<CombinedAuthAuthority>) {
		*self.authority.write() = Some(authority);
	}

	/// Binds the canonical native provider authority used for Exa imports.
	pub fn bind_native_auth(&self, authority: AuthControlHandle) {
		*self.native_auth.write() = Some(authority);
	}

	/// Binds the OAuth flow used to attach live token headers and react to
	/// unauthenticated MCP endpoints.
	pub fn bind_oauth(&self, oauth: Arc<McpOAuth>) {
		*self.oauth.write() = Some(oauth);
	}

	async fn resolve_values(
		&self,
		config: &McpServerConfig,
		cancellation: &CancellationToken,
	) -> Result<ResolvedTransportValues, ManagerError> {
		let commands = self.commands.read().clone();
		resolve_transport_values(config, &self.environment, commands.as_deref(), cancellation)
			.await
			.map_err(ManagerError::ConfigValues)
	}

	/// Starts all declarations in parallel, waits at most 250 ms, and leaves
	/// unfinished cache/connect work running in the background.
	pub async fn start(self: &Arc<Self>, specs: Vec<MountSpec>) -> StartupSnapshot {
		let requested = specs
			.iter()
			.map(|spec| spec.name.clone())
			.collect::<BTreeSet<_>>();
		let mounted = self
			.state
			.lock()
			.mounts
			.keys()
			.filter(|name| !requested.contains(*name))
			.cloned()
			.collect::<Vec<_>>();
		for name in mounted {
			let _ = self.unmount(&name).await;
		}
		let mut completions = FuturesUnordered::new();
		for spec in specs {
			if self.state.lock().mounts.contains_key(&spec.name) {
				let _ = self.unmount(&spec.name).await;
			}
			let name = spec.name.clone();
			let generation = self.next_generation();
			self.install_mount(spec, generation);
			let cache_manager = Arc::clone(self);
			let cache_name = name.clone();
			tokio::spawn(async move {
				cache_manager.publish_cached(cache_name, generation).await;
			});
			let manager = Arc::clone(self);
			completions.push(tokio::spawn(async move {
				let _ = manager.connect_initial(name, generation).await;
			}));
		}

		let mut completed = true;
		let deadline = time::sleep(STARTUP_RACE);
		tokio::pin!(deadline);
		while !completions.is_empty() {
			tokio::select! {
				biased;
				_ = &mut deadline => {
					completed = false;
					break;
				},
				Some(_) = completions.next() => {},
			}
		}
		StartupSnapshot { status: self.service.status(None), completed }
	}

	/// Atomically refreshes live mounts from the precedence-resolved native
	/// configuration authority.
	pub async fn replace_resolved_config(
		self: &Arc<Self>,
		resolved: ResolvedConfig,
	) -> StartupSnapshot {
		let mut filtered = filter_native_coverage(&resolved.servers, &NativeCoverage::default());
		let native_exa_ready = if filtered.covered_exa.is_empty() {
			false
		} else if let Some(authority) = self.native_auth.read().clone() {
			match import_exa_keys(&authority, PrincipalId::from("default"), filtered.exa_keys.clone())
			{
				Ok(ready) => ready,
				Err(error) => {
					tracing::warn!(%error, "native Exa credential import failed; retaining MCP fallback");
					false
				},
			}
		} else {
			false
		};
		if native_exa_ready {
			for name in &filtered.covered_exa {
				filtered.mounts.remove(name);
			}
		}
		let mut declarations = BTreeMap::new();
		for (name, mut filtered_mount) in filtered.mounts {
			if filtered.covered_exa.contains(&name) && !native_exa_ready {
				filtered_mount.suppressed_tools.clear();
			}
			let config = Arc::clone(&filtered_mount.server.config);
			let values = match self.resolve_values(&config, &self.shutdown).await {
				Ok(values) => values,
				Err(error) => {
					tracing::warn!(server = %name, %error, "MCP config refresh skipped unresolved values");
					continue;
				},
			};
			let Ok(config_json) = serde_json::to_vec(config.as_ref()).map(Bytes::from) else {
				continue;
			};
			let auth_headers =
				if matches!(config.resolved_transport(), TransportKind::Http | TransportKind::Sse)
					&& config
						.auth
						.as_ref()
						.is_some_and(|auth| auth.kind == AuthKind::Oauth)
				{
					let oauth = { self.oauth.read().clone() };
					match oauth {
						Some(oauth) => oauth
							.authority_headers("default", name.as_str(), &config)
							.await
							.ok(),
						None => None,
					}
				} else {
					None
				};
			declarations.insert(name.clone(), MountSpec {
				name,
				config,
				config_json,
				values,
				auth_headers,
				suppressed_tools: filtered_mount.suppressed_tools,
				projection: McpDeviceProjection::all(),
				auth: ControlMountAuth::None,
				restart: McpRestartPolicy::OnFailure,
				owner: None,
			});
		}
		self.start(declarations.into_values().collect()).await
	}

	/// Enables or disables exact resource subscriptions across every live
	/// server. Reconnects replay the desired set and stale completions roll
	/// themselves back.
	pub fn set_notifications_enabled(self: &Arc<Self>, enabled: bool) {
		let connections = {
			let mut subscriptions = self.subscriptions.lock();
			if subscriptions.enabled == enabled {
				return;
			}
			subscriptions.enabled = enabled;
			subscriptions.epoch = subscriptions.epoch.saturating_add(1);
			self
				.state
				.lock()
				.mounts
				.iter()
				.filter_map(|(name, mount)| {
					mount
						.connection
						.as_ref()
						.map(|connection| (name.clone(), Arc::clone(connection)))
				})
				.collect::<Vec<_>>()
		};
		for (name, connection) in connections {
			let manager = Arc::clone(self);
			tokio::spawn(async move {
				manager.sync_subscriptions(name, connection).await;
			});
		}
	}

	/// Mounts one declaration without replacing unrelated servers.
	pub async fn mount(self: &Arc<Self>, spec: MountSpec) -> pb::McpServerStatus {
		let name = spec.name.clone();
		if self.state.lock().mounts.contains_key(&name) {
			let _ = self.unmount(&name).await;
		}
		let generation = self.next_generation();
		self.install_mount(spec, generation);
		let cache_manager = Arc::clone(self);
		let cache_name = name.clone();
		tokio::spawn(async move {
			cache_manager.publish_cached(cache_name, generation).await;
		});
		let manager = Arc::clone(self);
		let connect_name = name.clone();
		let completion = tokio::spawn(async move {
			let _ = manager.connect_initial(connect_name, generation).await;
		});
		tokio::select! {
			biased;
			_ = completion => {},
			() = tokio::time::sleep(STARTUP_RACE) => {},
		}
		self.status_for(&name)
	}

	/// Unmounts one server, closes its live transport, and removes its current
	/// leaves in one owner-fenced publication.
	pub async fn unmount(&self, name: &str) -> Result<bool, ManagerError> {
		let removed = self.state.lock().mounts.remove(name);
		let Some(removed) = removed else {
			return Ok(false);
		};
		self.subscriptions.lock().active.remove(name);
		if let Some(connection) = removed.connection {
			let _ = connection.client.transport().close().await;
		}
		let owner = leaf_owner(name);
		self.service.replace_leaves(
			owner,
			LeafVersion {
				manager_generation: removed.generation,
				definition_epoch:   removed.definition_version.saturating_add(1),
			},
			Vec::new(),
		)?;
		let server = pb::McpServerRef {
			name:             name.to_owned(),
			definition_epoch: self.service.definition_epoch(),
		};
		let _ = self.service.remove(&server);
		self.changed.notify_waiters();
		Ok(true)
	}

	/// Returns deterministic server inventory from the shared Environment owner.
	pub fn servers(&self) -> pb::McpStatusResult {
		self.service.status(None)
	}

	/// Returns the immutable current MCP definition catalog for CONTROL and dyn
	/// registry epoch consumers.
	pub fn catalog_snapshot(&self) -> omp_tool::LeafCatalogSnapshot<McpLeaf> {
		self.service.leaf_snapshot()
	}

	/// Captures every live MCP catalog once for UI inspection.
	pub fn inspector_snapshots(&self) -> Vec<McpInspectorSnapshot> {
		let state = self.state.lock();
		let definition_epoch = self.service.definition_epoch();
		state
			.mounts
			.iter()
			.map(|(name, mount)| {
				let connection = mount.connection.as_ref();
				let health = if mount.terminal_failure {
					McpInspectorHealth::Failed
				} else if mount.connecting || mount.reconnecting {
					McpInspectorHealth::Connecting
				} else if connection.is_some() {
					McpInspectorHealth::Connected
				} else {
					McpInspectorHealth::Disconnected
				};
				McpInspectorSnapshot {
					server: name.clone(),
					health,
					generation: mount.generation,
					definition_epoch,
					implementation: connection.map(|live| live.initialized.name.clone()),
					version: connection.and_then(|live| live.initialized.version.clone()),
					title: connection.and_then(|live| live.initialized.title.clone()),
					description: connection.and_then(|live| live.initialized.description.clone()),
					instructions: connection
						.and_then(|live| bounded_instructions(live.initialized.instructions.as_ref())),
					tools: connection
						.map(|live| live.tools.read().clone())
						.unwrap_or_else(|| Arc::from([])),
					resources: connection
						.map(|live| live.resources.read().clone())
						.unwrap_or_else(|| Arc::from([])),
					prompts: connection
						.map(|live| live.prompts.read().clone())
						.unwrap_or_else(|| Arc::from([])),
				}
			})
			.collect()
	}

	/// Mounts one declaration only when its stamped owner is the authenticated
	/// caller and no other extension generation owns the server name.
	pub async fn control_mount(
		self: &Arc<Self>,
		identity: &ControlConnectionIdentity,
		spec: MountSpec,
		cancellation: &CancellationToken,
	) -> Result<pb::McpServerStatus, ManagerError> {
		if cancellation.is_cancelled() {
			return Err(ManagerError::Cancelled);
		}
		let _gate = self.control_gate.lock().await;
		let Some(owner) = spec.owner.as_deref() else {
			return Err(ManagerError::OwnershipDenied);
		};
		if !same_control_owner(owner, identity) {
			return Err(ManagerError::OwnershipDenied);
		}
		if let Some(current) = self.state.lock().mounts.get(&spec.name) {
			match current.spec.owner.as_deref() {
				Some(owner) if same_control_owner(owner, identity) => {},
				Some(_) | None => return Err(ManagerError::OwnershipDenied),
			}
		}
		let name = spec.name.clone();
		let manager = Arc::clone(self);
		let mut completion = tokio::spawn(async move { manager.mount(spec).await });
		tokio::select! {
			biased;
			() = cancellation.cancelled() => {
				let manager = Arc::clone(self);
				tokio::spawn(async move {
					let Ok(status) = completion.await else {
						return;
					};
					let current = manager
						.state
						.lock()
						.mounts
						.get(&name)
						.is_some_and(|mount| mount.generation == status.generation);
					if current {
						let _ = manager.unmount(&name).await;
					}
				});
				Err(ManagerError::Cancelled)
			},
			status = &mut completion => status.map_err(|_| ManagerError::ConnectionUnavailable),
		}
	}

	/// Removes only a server owned by this exact extension generation.
	pub async fn control_unmount(
		&self,
		identity: &ControlConnectionIdentity,
		name: &str,
	) -> Result<bool, ManagerError> {
		let _gate = self.control_gate.lock().await;
		let ownership = self.state.lock().mounts.get(name).map(|mount| {
			mount
				.spec
				.owner
				.as_deref()
				.is_some_and(|owner| same_control_owner(owner, identity))
		});
		match ownership {
			None => Ok(false),
			Some(true) => self.unmount(name).await,
			Some(false) => Err(ManagerError::OwnershipDenied),
		}
	}

	/// Returns whether one exact extension generation owns a mounted server.
	pub fn control_owns(&self, identity: &ControlConnectionIdentity, name: &str) -> bool {
		self
			.state
			.lock()
			.mounts
			.get(name)
			.and_then(|mount| mount.spec.owner.as_deref())
			.is_some_and(|owner| same_control_owner(owner, identity))
	}

	/// Returns deterministic server names owned by one extension generation.
	pub fn control_server_names(&self, identity: &ControlConnectionIdentity) -> BTreeSet<Str> {
		self
			.state
			.lock()
			.mounts
			.iter()
			.filter_map(|(name, mount)| {
				mount
					.spec
					.owner
					.as_deref()
					.filter(|owner| same_control_owner(owner, identity))
					.map(|_| name.clone())
			})
			.collect()
	}

	/// Removes every mount owned by one cancelled extension generation.
	pub async fn control_unmount_all(&self, identity: &ControlConnectionIdentity) {
		let names = self.control_server_names(identity);
		for name in names {
			let _ = self.control_unmount(identity, &name).await;
		}
	}

	/// Invokes only through a server owned by this exact extension generation.
	pub async fn control_invoke_scoped(
		self: &Arc<Self>,
		identity: &ControlConnectionIdentity,
		server: &str,
		tool: &str,
		arguments: Value,
		cancel: CancellationToken,
	) -> Result<pb::McpInvokeResult, McpServiceError> {
		if !self.control_owns(identity, server) {
			return Err(McpServiceError::InvalidRequest);
		}
		self.control_invoke(server, tool, arguments, cancel).await
	}

	/// Invokes a CONTROL-originated MCP tool through the same receipt-bearing
	/// bridge used by Environment RPC.
	pub async fn control_invoke(
		self: &Arc<Self>,
		server: &str,
		tool: &str,
		arguments: Value,
		cancel: CancellationToken,
	) -> Result<pb::McpInvokeResult, McpServiceError> {
		let arguments_json =
			serde_json::to_vec(&arguments).map_err(|_| McpServiceError::InvalidRequest)?;
		invoke::invoke(
			Arc::clone(self),
			pb::McpInvokeRequest {
				server:         Some(pb::McpServerRef {
					name:             server.to_owned(),
					definition_epoch: self.service.definition_epoch(),
				}),
				tool:           tool.to_owned(),
				arguments_json: arguments_json.into(),
				timeout_ms:     0,
				max_bytes:      8 * 1024 * 1024,
				wire_revision:  omp_proto::SCHEMA_REV,
			},
			cancel,
		)
		.await
	}

	/// Performs a manual reconnect, clearing the burst circuit breaker.
	pub async fn reset(self: &Arc<Self>, name: &str) -> Result<(), ManagerError> {
		self.reconnect(name, true).await.map(|_| ())
	}

	/// Deletes one MCP credential from the shared authority and drops its live
	/// authenticated connection.
	pub async fn clear_authorization(self: &Arc<Self>, name: &str) -> Result<bool, ManagerError> {
		let (generation, config) = {
			let state = self.state.lock();
			let mount = state.mounts.get(name).ok_or(ManagerError::ServerNotFound)?;
			if !matches!(
				mount.spec.config.resolved_transport(),
				TransportKind::Http | TransportKind::Sse
			) {
				return Err(ManagerError::UnsupportedAuthorization);
			}
			(mount.generation, Arc::clone(&mount.spec.config))
		};
		let authority = self
			.authority
			.read()
			.clone()
			.ok_or(ManagerError::CredentialAuthorityUnavailable)?;
		let affinity =
			CombinedAuthAuthority::mcp_affinity("default", name, PrincipalId::from("default"));
		let removed = authority.delete_mcp(&affinity)?;
		let stale = {
			let mut state = self.state.lock();
			let mount = state
				.mounts
				.get_mut(name)
				.ok_or(ManagerError::ServerNotFound)?;
			if mount.generation != generation || mount.spec.config != config {
				return Err(ManagerError::StaleGeneration);
			}
			mount.spec.auth_headers = None;
			mount.terminal_failure = true;
			mount.connection.take()
		};
		if let Some(stale) = stale {
			let _ = stale.client.transport().close().await;
		}
		self.publish_status(
			name,
			generation,
			pb::McpLifecycleState::Failed,
			"authentication cleared",
		);
		self.changed.notify_waiters();
		Ok(removed)
	}

	/// Replaces one MCP OAuth grant and installs its live header lease.
	pub async fn reauthorize(
		self: &Arc<Self>,
		name: &str,
		present: &(dyn Fn(&str) + Send + Sync),
	) -> Result<bool, ManagerError> {
		let removed = self.clear_authorization(name).await?;
		let (generation, config, server_url) = {
			let state = self.state.lock();
			let mount = state.mounts.get(name).ok_or(ManagerError::ServerNotFound)?;
			let server_url = mount
				.spec
				.config
				.url
				.clone()
				.ok_or(ManagerError::UnsupportedAuthorization)?;
			(mount.generation, Arc::clone(&mount.spec.config), server_url)
		};
		let oauth = self
			.oauth
			.read()
			.clone()
			.ok_or(ManagerError::CredentialAuthorityUnavailable)?;
		let challenge = AuthChallenge {
			kind:                   ChallengeKind::OAuth,
			authorization_endpoint: None,
			token_endpoint:         None,
			registration_endpoint:  None,
			resource_metadata:      None,
			auth_server:            None,
			resource:               None,
			scopes:                 Box::new([]),
			client_id:              None,
		};
		let state = oauth
			.authorize_presented(
				OAuthAttempt {
					profile:      "default",
					server:       name,
					server_url:   server_url.as_str(),
					config:       &config,
					challenge:    &challenge,
					listener_uri: "http://127.0.0.1:3000/callback",
					cancel:       self.shutdown.child_token(),
				},
				Some(present),
			)
			.await?;
		let headers = AuthorityHeaders::new(oauth, state).await?;
		let mut manager_state = self.state.lock();
		let mount = manager_state
			.mounts
			.get_mut(name)
			.ok_or(ManagerError::ServerNotFound)?;
		if mount.generation != generation {
			return Err(ManagerError::StaleGeneration);
		}
		mount.spec.auth_headers = Some(headers);
		Ok(removed)
	}

	pub(crate) fn local_root(&self) -> &Path {
		&self.local_root
	}

	pub(crate) fn mount_timeout(&self, name: &str) -> Option<u64> {
		self
			.state
			.lock()
			.mounts
			.get(name)
			.and_then(|mount| mount.spec.config.timeout)
	}

	pub(crate) fn tool_definition(&self, name: &str, tool: &str) -> Option<Value> {
		self
			.state
			.lock()
			.mounts
			.get(name)?
			.tools
			.iter()
			.find(|definition| definition.get("name").and_then(Value::as_str) == Some(tool))
			.cloned()
	}

	/// Lists live concrete resource URIs for `mcp://` completion.
	pub(crate) fn resource_uris(&self) -> Vec<Str> {
		let state = self.state.lock();
		let mut uris = state
			.mounts
			.values()
			.filter_map(|mount| mount.connection.as_ref())
			.flat_map(|connection| {
				connection
					.resources
					.read()
					.iter()
					.map(|resource| resource.uri.clone())
					.collect::<Vec<_>>()
			})
			.collect::<Vec<_>>();
		uris.sort_unstable();
		uris.dedup();
		uris
	}

	/// Resolves an opaque advertised resource URI to its owning live server.
	/// Concrete resources precede templates; template ties are stable by
	/// template text and then server name.
	pub(crate) fn resolve_resource_server(&self, uri: &str) -> Option<Str> {
		let state = self.state.lock();
		for (name, mount) in &state.mounts {
			if mount.connection.as_ref().is_some_and(|connection| {
				connection
					.resources
					.read()
					.iter()
					.any(|resource| resource.uri == uri)
			}) {
				return Some(name.clone());
			}
		}
		let mut best: Option<(usize, Str, Str)> = None;
		for (name, mount) in &state.mounts {
			let Some(connection) = mount.connection.as_ref() else {
				continue;
			};
			for template in connection.templates.read().iter() {
				let Some(score) = template_match_score(template.uri_template.as_str(), uri) else {
					continue;
				};
				let replace = best
					.as_ref()
					.is_none_or(|(best_score, best_template, best_name)| {
						score > *best_score
							|| (score == *best_score
								&& (template.uri_template < *best_template
									|| (template.uri_template == *best_template && name < best_name)))
					});
				if replace {
					best = Some((score, template.uri_template.clone(), name.clone()));
				}
			}
		}
		best.map(|(_, _, name)| name)
	}

	pub(crate) async fn connection(
		&self,
		name: &str,
		cancel: &CancellationToken,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		loop {
			let notified = self.changed.notified();
			{
				let state = self.state.lock();
				let mount = state.mounts.get(name).ok_or(ManagerError::ServerNotFound)?;
				if let Some(connection) = &mount.connection {
					return Ok(Arc::clone(connection));
				}
				if mount.terminal_failure && !mount.connecting && !mount.reconnecting {
					return Err(ManagerError::ConnectionUnavailable);
				}
			}
			tokio::select! {
				biased;
				() = cancel.cancelled() => return Err(ManagerError::Cancelled),
				() = notified => {},
			}
		}
	}

	pub(crate) async fn reconnect_for_invoke(
		self: &Arc<Self>,
		name: &str,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		self.reconnect(name, false).await
	}

	pub(crate) async fn refresh_auth(
		&self,
		name: &str,
		challenges: &[Str],
		cancel: CancellationToken,
	) -> bool {
		let handler = self.auth.read().clone();
		if let Some(handler) = handler
			&& handler
				.refresh(name, challenges, cancel.child_token())
				.await
		{
			return true;
		}
		self.authorize_challenge(name, challenges, cancel).await
	}

	async fn authorize_challenge(
		&self,
		name: &str,
		challenges: &[Str],
		cancel: CancellationToken,
	) -> bool {
		let (config, server_url) = {
			let state = self.state.lock();
			let Some(mount) = state.mounts.get(name) else {
				return false;
			};
			let Some(server_url) = mount.spec.config.url.clone() else {
				return false;
			};
			(Arc::clone(&mount.spec.config), server_url)
		};
		let Some(oauth) = self.oauth.read().clone() else {
			return false;
		};
		let mut headers = HeaderMap::new();
		for challenge in challenges {
			let Ok(value) = HeaderValue::from_str(challenge) else {
				continue;
			};
			headers.append(WWW_AUTHENTICATE, value);
		}
		let Some(challenge) = discover_auth_challenge(&headers, "") else {
			return false;
		};
		let Ok(state) = oauth
			.authorize(OAuthAttempt {
				profile: "default",
				server: name,
				server_url: server_url.as_str(),
				config: &config,
				challenge: &challenge,
				listener_uri: "http://127.0.0.1:3000/callback",
				cancel,
			})
			.await
		else {
			return false;
		};
		let Ok(headers) = AuthorityHeaders::new(oauth, state).await else {
			return false;
		};
		let mut state = self.state.lock();
		let Some(mount) = state.mounts.get_mut(name) else {
			return false;
		};
		mount.spec.auth_headers = Some(headers);
		true
	}

	fn install_mount(self: &Arc<Self>, spec: MountSpec, generation: u64) {
		let name = spec.name.clone();
		let backend: Arc<dyn McpServerBackend> =
			Arc::new(ManagedBackend { manager: Arc::downgrade(self), name: name.clone() });
		self.state.lock().mounts.insert(name.clone(), MountState {
			spec,
			generation,
			definition_version: 0,
			notification_sequence: 0,
			connection: None,
			connecting: true,
			reconnecting: false,
			terminal_failure: false,
			reconnects: VecDeque::new(),
			tools: Arc::from([]),
		});
		let _ = self.service.install(
			status(
				&name,
				pb::McpLifecycleState::Starting,
				generation,
				self.service.definition_epoch(),
				"",
			),
			backend,
		);
	}

	async fn publish_cached(self: Arc<Self>, name: Str, generation: u64) {
		let (cache, spec) = {
			let state = self.state.lock();
			let Some(mount) = state.mounts.get(&name) else {
				return;
			};
			(Arc::clone(self.service.cache()), mount.spec.clone())
		};
		let cache_name = name.clone();
		let config_json = spec.config_json.clone();
		let loaded =
			task::spawn_blocking(move || cache.get(&cache_name, &config_json, now_ms())).await;
		let Ok(Ok(Some(cached))) = loaded else {
			return;
		};
		let Ok(tools) = serde_json::from_slice::<Vec<Value>>(&cached.definitions_json) else {
			return;
		};
		{
			let mut state = self.state.lock();
			let Some(mount) = state.mounts.get_mut(&name) else {
				return;
			};
			if mount.generation != generation || mount.connection.is_some() {
				return;
			}
			mount.tools = Arc::from(tools.clone());
		}
		if self
			.publish_definitions(&name, generation, tools, Vec::new(), Vec::new(), Vec::new(), None)
			.is_ok()
		{
			self.publish_status(
				&name,
				generation,
				pb::McpLifecycleState::Degraded,
				"cached definitions; connection pending",
			);
		}
	}

	async fn connect_initial(
		self: Arc<Self>,
		name: Str,
		generation: u64,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		let mut result = self.connect_once(&name, generation).await;
		if is_unauthorized(&result) && self.authorize_initial(&name, generation).await {
			result = self.connect_once(&name, generation).await;
		}
		{
			let mut state = self.state.lock();
			if let Some(mount) = state.mounts.get_mut(&name)
				&& mount.generation == generation
			{
				mount.connecting = false;
				mount.terminal_failure = result.is_err();
			}
		}
		if result.is_err() {
			self.publish_status(&name, generation, pb::McpLifecycleState::Failed, "connection failed");
		}
		self.changed.notify_waiters();
		result
	}

	async fn authorize_initial(&self, name: &str, generation: u64) -> bool {
		let (config, server_url) = {
			let state = self.state.lock();
			let Some(mount) = state.mounts.get(name) else {
				return false;
			};
			if mount.generation != generation
				|| mount
					.spec
					.auth_headers
					.as_ref()
					.is_some_and(|headers| !headers.should_reauthorize())
			{
				return false;
			}
			let Some(server_url) = mount.spec.config.url.clone() else {
				return false;
			};
			(Arc::clone(&mount.spec.config), server_url)
		};
		let Some(oauth) = self.oauth.read().clone() else {
			return false;
		};
		let challenge = AuthChallenge {
			kind:                   ChallengeKind::OAuth,
			authorization_endpoint: None,
			token_endpoint:         None,
			registration_endpoint:  None,
			resource_metadata:      None,
			auth_server:            None,
			resource:               None,
			scopes:                 Box::new([]),
			client_id:              None,
		};
		let Ok(state) = oauth
			.authorize(OAuthAttempt {
				profile:      "default",
				server:       name,
				server_url:   server_url.as_str(),
				config:       &config,
				challenge:    &challenge,
				listener_uri: "http://127.0.0.1:3000/callback",
				cancel:       self.shutdown.child_token(),
			})
			.await
		else {
			return false;
		};
		let Ok(headers) = AuthorityHeaders::new(oauth, state).await else {
			return false;
		};
		let mut state = self.state.lock();
		let Some(mount) = state.mounts.get_mut(name) else {
			return false;
		};
		if mount.generation != generation {
			return false;
		}
		mount.spec.auth_headers = Some(headers);
		true
	}

	async fn connect_once(
		self: &Arc<Self>,
		name: &str,
		generation: u64,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		let timeout_ms = {
			let state = self.state.lock();
			let mount = state.mounts.get(name).ok_or(ManagerError::ServerNotFound)?;
			if mount.generation != generation {
				return Err(ManagerError::StaleGeneration);
			}
			mount.spec.config.timeout
		};
		let cancellation = self.shutdown.child_token();
		match McpTimeout::resolve(None, timeout_ms)
			.run(&cancellation, self.connect_once_with_no_outer_deadline(name, generation))
			.await
		{
			Ok(result) => result,
			Err(McpDeadlineError::Cancelled) => Err(ManagerError::Cancelled),
			Err(McpDeadlineError::TimedOut) => Err(ManagerError::TimedOut),
		}
	}

	async fn connect_once_with_no_outer_deadline(
		self: &Arc<Self>,
		name: &str,
		generation: u64,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		let spec = {
			let state = self.state.lock();
			let mount = state.mounts.get(name).ok_or(ManagerError::ServerNotFound)?;
			if mount.generation != generation {
				return Err(ManagerError::StaleGeneration);
			}
			mount.spec.clone()
		};
		let connected = self
			.connector
			.connect(&spec, Arc::clone(&self.workspace), self.shutdown.child_token())
			.await?;
		let supports_tools = connected.initialized.capabilities.get("tools").is_some();
		let tools = if supports_tools {
			list_tools(connected.client.transport(), self.shutdown.child_token()).await?
		} else {
			Vec::new()
		};
		let supports_resources = connected
			.initialized
			.capabilities
			.get("resources")
			.is_some();
		let supports_prompts = connected.initialized.capabilities.get("prompts").is_some();
		let (resources, templates) = if supports_resources {
			let client = ResourcesClient::new(Arc::clone(connected.client.transport()));
			let resources = client
				.list(self.shutdown.child_token())
				.await
				.unwrap_or_default();
			let templates = client
				.templates(self.shutdown.child_token())
				.await
				.unwrap_or_default();
			(resources, templates)
		} else {
			(Vec::new(), Vec::new())
		};
		let prompts = if supports_prompts {
			PromptsClient::new(Arc::clone(connected.client.transport()))
				.list(self.shutdown.child_token())
				.await
				.unwrap_or_default()
		} else {
			Vec::new()
		};
		let instructions = bounded_instructions(connected.initialized.instructions.as_ref());
		let connection = Arc::new(LiveConnection {
			client:      connected.client,
			initialized: connected.initialized,
			tools:       RwLock::new(Arc::from(tools.clone())),
			resources:   RwLock::new(Arc::from(resources.clone())),
			templates:   RwLock::new(Arc::from(templates.clone())),
			prompts:     RwLock::new(Arc::from(prompts.clone())),
		});
		let stale = {
			let mut state = self.state.lock();
			let mount = state
				.mounts
				.get_mut(name)
				.ok_or(ManagerError::ServerNotFound)?;
			if mount.generation != generation {
				true
			} else {
				mount.connection = Some(Arc::clone(&connection));
				mount.tools = Arc::from(tools.clone());
				false
			}
		};
		if stale {
			let _ = connection.client.transport().close().await;
			return Err(ManagerError::StaleGeneration);
		}
		self.publish_definitions(
			name,
			generation,
			tools.clone(),
			resources,
			templates,
			prompts,
			instructions,
		)?;
		let cache = Arc::clone(self.service.cache());
		let cache_name = Str::from(name);
		let config_json = spec.config_json;
		if let Ok(definitions_json) = serde_json::to_vec(&tools) {
			task::spawn_blocking(move || {
				let _ = cache.put(&cache_name, &config_json, &definitions_json, now_ms());
			});
		}
		self.publish_status(name, generation, pb::McpLifecycleState::Ready, "");
		self.changed.notify_waiters();
		self.spawn_message_loop(Str::from(name), generation, Arc::clone(&connection));
		let subscriptions = Arc::clone(self);
		let subscription_name = Str::from(name);
		let subscription_connection = Arc::clone(&connection);
		tokio::spawn(async move {
			subscriptions
				.sync_subscriptions(subscription_name, subscription_connection)
				.await;
		});
		Ok(connection)
	}

	fn publish_definitions(
		&self,
		name: &str,
		generation: u64,
		tools: Vec<Value>,
		resources: Vec<ResourceDefinition>,
		templates: Vec<ResourceTemplate>,
		prompts: Vec<PromptDefinition>,
		instructions: Option<Str>,
	) -> Result<u64, ManagerError> {
		let (definition_version, protocol_version, suppressed_tools, projection) = {
			let mut state = self.state.lock();
			let mount = state
				.mounts
				.get_mut(name)
				.ok_or(ManagerError::ServerNotFound)?;
			if mount.generation != generation {
				return Err(ManagerError::StaleGeneration);
			}
			mount.definition_version = mount.definition_version.saturating_add(1);
			let protocol_version = mount
				.connection
				.as_ref()
				.map_or("2025-11-25", |connection| connection.initialized.protocol_version.as_str());
			(
				mount.definition_version,
				Str::from(protocol_version),
				mount.spec.suppressed_tools.clone(),
				Arc::clone(&mount.spec.projection),
			)
		};
		let leaves = McpDeviceDefinitions {
			server: Str::from(name),
			tools,
			resources,
			templates,
			prompts,
			instructions,
			suppressed_tools,
			projection,
		}
		.into_leaves(&protocol_version)?;
		Ok(self.service.replace_leaves(
			leaf_owner(name),
			LeafVersion { manager_generation: generation, definition_epoch: definition_version },
			leaves,
		)?)
	}

	fn spawn_message_loop(
		self: &Arc<Self>,
		name: Str,
		generation: u64,
		connection: Arc<LiveConnection>,
	) {
		let manager = Arc::downgrade(self);
		let shutdown = self.shutdown.clone();
		tokio::spawn(async move {
			loop {
				let message = connection.client.next(shutdown.child_token()).await;
				let Some(manager) = manager.upgrade() else {
					return;
				};
				match message {
					Ok(Some((method, params))) => {
						manager
							.handle_notification(&name, generation, &connection, &method, params)
							.await;
					},
					Ok(None) | Err(_) => break,
				}
				drop(manager);
			}
			let Some(manager) = manager.upgrade() else {
				return;
			};
			if manager.is_current_connection(&name, generation, &connection) {
				let _ = manager.reconnect(&name, false).await;
			}
		});
	}

	async fn handle_notification(
		&self,
		name: &str,
		generation: u64,
		connection: &Arc<LiveConnection>,
		method: &str,
		params: Value,
	) {
		if !self.is_current_connection(name, generation, connection) {
			return;
		}
		let refresh = match method {
			"notifications/tools/list_changed" => Some(RefreshKind::Tools),
			"notifications/resources/list_changed" => Some(RefreshKind::Resources),
			"notifications/prompts/list_changed" => Some(RefreshKind::Prompts),
			"notifications/resources/updated" => {
				if ResourcesClient::decode_update(params.clone()).is_err() {
					return;
				}
				None
			},
			_ => None,
		};
		if let Some(refresh) = refresh {
			let _ = self.refresh_definitions(name, generation, refresh).await;
		}
		let sequence = {
			let mut state = self.state.lock();
			let Some(mount) = state.mounts.get_mut(name) else {
				return;
			};
			if mount.generation != generation {
				return;
			}
			mount.notification_sequence = mount.notification_sequence.saturating_add(1);
			mount.notification_sequence
		};
		if let Some(sink) = self.notifications.read().clone()
			&& sink.interested(name, method)
		{
			sink.offer(McpHookNotification {
				server: Str::from(name),
				method: Str::from(method),
				params: params.clone(),
				sequence,
			});
		}
		let params_json = serde_json::to_vec(&params).unwrap_or_else(|_| b"null".to_vec());
		let _ = self.service.notify(pb::McpNotification {
			server: Some(pb::McpServerRef {
				name:             name.to_owned(),
				definition_epoch: self.service.definition_epoch(),
			}),
			sequence,
			method: method.to_owned(),
			params_json: params_json.into(),
		});
	}

	async fn refresh_definitions(
		&self,
		name: &str,
		generation: u64,
		kind: RefreshKind,
	) -> Result<(), ManagerError> {
		let connection = {
			let state = self.state.lock();
			let mount = state.mounts.get(name).ok_or(ManagerError::ServerNotFound)?;
			if mount.generation != generation {
				return Err(ManagerError::StaleGeneration);
			}
			mount
				.connection
				.clone()
				.ok_or(ManagerError::ConnectionUnavailable)?
		};
		let mut tools = connection.tools.read().to_vec();
		let mut resources = connection.resources.read().to_vec();
		let mut templates = connection.templates.read().to_vec();
		let mut prompts = connection.prompts.read().to_vec();
		match kind {
			RefreshKind::Tools => {
				tools = list_tools(connection.client.transport(), self.shutdown.child_token()).await?;
			},
			RefreshKind::Resources => {
				let client = ResourcesClient::new(Arc::clone(connection.client.transport()));
				resources = client.list(self.shutdown.child_token()).await?;
				templates = client.templates(self.shutdown.child_token()).await?;
			},
			RefreshKind::Prompts => {
				prompts = PromptsClient::new(Arc::clone(connection.client.transport()))
					.list(self.shutdown.child_token())
					.await?;
			},
		}
		self.publish_definitions(
			name,
			generation,
			tools.clone(),
			resources.clone(),
			templates.clone(),
			prompts.clone(),
			bounded_instructions(connection.initialized.instructions.as_ref()),
		)?;
		*connection.tools.write() = Arc::from(tools.clone());
		*connection.resources.write() = Arc::from(resources);
		*connection.templates.write() = Arc::from(templates);
		*connection.prompts.write() = Arc::from(prompts);
		if matches!(kind, RefreshKind::Tools) {
			if let Some(mount) = self.state.lock().mounts.get_mut(name)
				&& mount.generation == generation
			{
				mount.tools = Arc::from(tools);
			}
		} else if matches!(kind, RefreshKind::Resources) {
			self.sync_subscriptions(Str::from(name), connection).await;
		}
		Ok(())
	}

	async fn sync_subscriptions(&self, name: Str, connection: Arc<LiveConnection>) {
		let supports = connection
			.initialized
			.capabilities
			.get("resources")
			.and_then(Value::as_object)
			.and_then(|resources| resources.get("subscribe"))
			.and_then(Value::as_bool)
			.unwrap_or(false);
		let (globally_enabled, epoch, current) = {
			let subscriptions = self.subscriptions.lock();
			(
				subscriptions.enabled,
				subscriptions.epoch,
				subscriptions.active.get(&name).cloned().unwrap_or_default(),
			)
		};
		let enabled = globally_enabled && supports;
		let desired = if enabled {
			connection
				.resources
				.read()
				.iter()
				.map(|resource| resource.uri.clone())
				.collect::<BTreeSet<_>>()
		} else {
			BTreeSet::new()
		};
		let client = ResourcesClient::new(Arc::clone(connection.client.transport()));
		for uri in current.difference(&desired) {
			if client
				.unsubscribe(uri, self.shutdown.child_token())
				.await
				.is_err()
			{
				return;
			}
		}
		let mut added: Vec<Str> = Vec::new();
		for uri in desired.difference(&current) {
			if client
				.subscribe(uri, self.shutdown.child_token())
				.await
				.is_err()
			{
				for rollback in added {
					let _ = client
						.unsubscribe(&rollback, self.shutdown.child_token())
						.await;
				}
				return;
			}
			added.push(uri.clone());
		}
		let stale = {
			let mut subscriptions = self.subscriptions.lock();
			if subscriptions.epoch != epoch || subscriptions.enabled != globally_enabled {
				true
			} else {
				if desired.is_empty() {
					subscriptions.active.remove(&name);
				} else {
					subscriptions.active.insert(name.clone(), desired);
				}
				false
			}
		};
		if stale {
			for uri in added {
				let _ = client.unsubscribe(&uri, self.shutdown.child_token()).await;
			}
		}
	}

	async fn reconnect(
		self: &Arc<Self>,
		name: &str,
		manual: bool,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		loop {
			let notified = self.changed.notified();
			let decision = {
				let mut state = self.state.lock();
				let mount = state
					.mounts
					.get_mut(name)
					.ok_or(ManagerError::ServerNotFound)?;
				if manual {
					mount.reconnects.clear();
				}
				if !manual && mount.spec.restart == McpRestartPolicy::Never {
					mount.connection = None;
					mount.terminal_failure = true;
					ReconnectStart::Disabled(mount.generation)
				} else if mount.reconnecting {
					ReconnectStart::Wait
				} else {
					let now = Instant::now();
					while mount
						.reconnects
						.front()
						.is_some_and(|attempt| now.duration_since(*attempt) >= RECONNECT_WINDOW)
					{
						mount.reconnects.pop_front();
					}
					mount.reconnects.push_back(now);
					if mount.reconnects.len() > RECONNECT_BURST_LIMIT {
						mount.connection = None;
						mount.terminal_failure = true;
						ReconnectStart::CircuitOpen(mount.generation)
					} else {
						mount.reconnecting = true;
						mount.terminal_failure = false;
						ReconnectStart::Begin(mount.generation, mount.connection.take())
					}
				}
			};
			let (generation, stale) = match decision {
				ReconnectStart::Wait => {
					notified.await;
					continue;
				},
				ReconnectStart::Disabled(generation) => {
					self.publish_status(
						name,
						generation,
						pb::McpLifecycleState::Failed,
						"automatic reconnect disabled",
					);
					self.changed.notify_waiters();
					return Err(ManagerError::ConnectionUnavailable);
				},
				ReconnectStart::CircuitOpen(generation) => {
					self.publish_status(
						name,
						generation,
						pb::McpLifecycleState::Failed,
						"automatic reconnect suspended",
					);
					return Err(ManagerError::CircuitOpen);
				},
				ReconnectStart::Begin(generation, stale) => (generation, stale),
			};
			if let Some(stale) = stale {
				let _ = stale.client.transport().close().await;
			}
			self.publish_status(name, generation, pb::McpLifecycleState::Starting, "reconnecting");
			let mut result = self.connect_once(name, generation).await;
			let mut reauthorized = false;
			if is_unauthorized(&result) && self.authorize_initial(name, generation).await {
				reauthorized = true;
				result = self.connect_once(name, generation).await;
			}
			for delay in RECONNECT_DELAYS {
				if result.is_ok() {
					break;
				}
				tokio::select! {
					biased;
					() = self.shutdown.cancelled() => {
						result = Err(ManagerError::Cancelled);
						break;
					},
					() = tokio::time::sleep(delay) => {},
				}
				result = self.connect_once(name, generation).await;
				if !reauthorized
					&& is_unauthorized(&result)
					&& self.authorize_initial(name, generation).await
				{
					reauthorized = true;
					result = self.connect_once(name, generation).await;
				}
			}
			{
				let mut state = self.state.lock();
				if let Some(mount) = state.mounts.get_mut(name)
					&& mount.generation == generation
				{
					mount.reconnecting = false;
					mount.terminal_failure = result.is_err();
				}
			}
			if result.is_err() {
				self.publish_status(
					name,
					generation,
					pb::McpLifecycleState::Failed,
					"reconnect failed",
				);
			}
			self.changed.notify_waiters();
			return result;
		}
	}

	fn is_current_connection(
		&self,
		name: &str,
		generation: u64,
		connection: &Arc<LiveConnection>,
	) -> bool {
		self.state.lock().mounts.get(name).is_some_and(|mount| {
			mount.generation == generation
				&& mount
					.connection
					.as_ref()
					.is_some_and(|current| Arc::ptr_eq(current, connection))
		})
	}

	fn publish_status(
		&self,
		name: &str,
		generation: u64,
		state: pb::McpLifecycleState,
		detail: &str,
	) {
		let backend = self
			.service
			.backend_for_manager(name)
			.unwrap_or_else(|| Arc::new(UnavailableBackend));
		let _ = self.service.install(
			status(name, state, generation, self.service.definition_epoch(), detail),
			backend,
		);
	}

	fn status_for(&self, name: &str) -> pb::McpServerStatus {
		self
			.service
			.status(Some(name))
			.servers
			.into_iter()
			.next()
			.unwrap_or_else(|| {
				status(name, pb::McpLifecycleState::Stopped, 0, self.service.definition_epoch(), "")
			})
	}

	fn next_generation(&self) -> u64 {
		self.generation.fetch_add(1, atomic::Ordering::Relaxed)
	}
}

impl Drop for McpManager {
	fn drop(&mut self) {
		self.shutdown.cancel();
	}
}

#[derive(Clone, Copy)]
enum RefreshKind {
	Tools,
	Resources,
	Prompts,
}

enum ReconnectStart {
	Wait,
	Disabled(u64),
	CircuitOpen(u64),
	Begin(u64, Option<Arc<LiveConnection>>),
}

fn leaf_owner(name: &str) -> LeafOwner {
	LeafOwner { root: Str::from(name), claimant: Str::new_static("mcp") }
}

fn same_control_owner(left: &ControlConnectionIdentity, right: &ControlConnectionIdentity) -> bool {
	left.extension == right.extension
		&& left.principal == right.principal
		&& left.artifact_digest == right.artifact_digest
		&& left.layer == right.layer
		&& left.tier == right.tier
		&& left.trust == right.trust
		&& left.host_generation == right.host_generation
		&& left.session_generation == right.session_generation
		&& left.capabilities == right.capabilities
}

fn status(
	name: &str,
	state: pb::McpLifecycleState,
	generation: u64,
	definition_epoch: u64,
	detail: &str,
) -> pb::McpServerStatus {
	pb::McpServerStatus {
		server: Some(pb::McpServerRef { name: name.to_owned(), definition_epoch }),
		state: state.into(),
		detail: detail.to_owned(),
		generation,
		definition_epoch,
	}
}

async fn list_tools(
	transport: &Arc<dyn McpTransport>,
	cancel: CancellationToken,
) -> Result<Vec<Value>, ManagerError> {
	let mut output = Vec::new();
	let mut cursor: Option<Str> = None;
	let mut seen = BTreeSet::new();
	for _ in 0..MAX_TOOL_PAGES {
		let params = cursor
			.as_ref()
			.map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
		let response = transport
			.request("tools/list", params, cancel.child_token())
			.await?;
		let mut object = response
			.result
			.as_object()
			.cloned()
			.ok_or(ManagerError::MalformedDefinitions)?;
		let tools = object
			.remove("tools")
			.ok_or(ManagerError::MalformedDefinitions)?;
		output.extend(
			serde_json::from_value::<Vec<Value>>(tools)
				.map_err(|_| ManagerError::MalformedDefinitions)?,
		);
		cursor = object.remove("nextCursor").and_then(|value| {
			value
				.as_str()
				.filter(|value| !value.is_empty())
				.map(Str::from)
		});
		let Some(next) = cursor.as_ref() else {
			output.sort_unstable_by(|left, right| {
				left
					.get("name")
					.and_then(Value::as_str)
					.cmp(&right.get("name").and_then(Value::as_str))
			});
			return Ok(output);
		};
		if !seen.insert(next.clone()) {
			return Err(ManagerError::MalformedDefinitions);
		}
	}
	Err(ManagerError::MalformedDefinitions)
}

fn bounded_instructions(instructions: Option<&Str>) -> Option<Str> {
	instructions.map(|instructions| {
		let end = floor_char_boundary(instructions, MAX_INSTRUCTIONS_BYTES);
		instructions.slice(..end)
	})
}

fn floor_char_boundary(value: &str, limit: usize) -> usize {
	let mut end = value.len().min(limit);
	while !value.is_char_boundary(end) {
		end -= 1;
	}
	end
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

struct ManagedBackend {
	manager: Weak<McpManager>,
	name:    Str,
}

impl McpServerBackend for ManagedBackend {
	fn reset(&self, cancel: CancellationToken) -> BoxFuture<'_, Result<(), McpServiceError>> {
		Box::pin(async move {
			let manager = self.manager.upgrade().ok_or(McpServiceError::Backend)?;
			tokio::select! {
				biased;
				() = cancel.cancelled() => Err(McpServiceError::Cancelled),
				result = manager.reset(&self.name) => result.map_err(|_| McpServiceError::Backend),
			}
		})
	}

	fn live_header(
		&self,
		request: pb::McpLiveHeaderRequest,
		_cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpLiveHeader, McpServiceError>> {
		Box::pin(async move {
			Ok(pb::McpLiveHeader {
				server:        request.server,
				headers:       Vec::new(),
				expires_at_ms: 0,
			})
		})
	}

	fn resource(
		&self,
		request: pb::McpResourceRequest,
		cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpResourceResult, McpServiceError>> {
		Box::pin(async move {
			let manager = self.manager.upgrade().ok_or(McpServiceError::Backend)?;
			let connection = manager
				.connection(&self.name, &cancel)
				.await
				.map_err(manager_service_error)?;
			let contents = ResourcesClient::new(Arc::clone(connection.client.transport()))
				.read(&request.uri, cancel)
				.await
				.map_err(|_| McpServiceError::Backend)?;
			let max = usize::try_from(request.max_bytes).unwrap_or(usize::MAX);
			let mut bytes = Vec::new();
			let mut mime_type = None;
			let mut truncated = false;
			for content in contents {
				mime_type = mime_type.or(content.mime_type);
				let remaining = max.saturating_sub(bytes.len());
				if content.bytes.len() > remaining {
					bytes.extend_from_slice(&content.bytes[..remaining]);
					truncated = true;
					break;
				}
				bytes.extend_from_slice(&content.bytes);
			}
			Ok(pb::McpResourceResult {
				server: request.server,
				uri: request.uri,
				mime_type: mime_type.map_or_else(String::new, |mime| mime.to_string()),
				content: bytes.into(),
				truncated,
			})
		})
	}

	fn prompt(
		&self,
		request: pb::McpPromptRequest,
		cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpPromptResult, McpServiceError>> {
		Box::pin(async move {
			let manager = self.manager.upgrade().ok_or(McpServiceError::Backend)?;
			let connection = manager
				.connection(&self.name, &cancel)
				.await
				.map_err(manager_service_error)?;
			let arguments = serde_json::from_slice::<Map<String, Value>>(&request.arguments_json)
				.map_err(|_| McpServiceError::InvalidRequest)?;
			let messages = PromptsClient::new(Arc::clone(connection.client.transport()))
				.get(&request.name, arguments, cancel)
				.await
				.map_err(|_| McpServiceError::Backend)?;
			let encoded = messages
				.into_iter()
				.map(|message| {
					let content = match message.content {
						PromptContent::Text(text) => json!({ "type": "text", "text": text }),
						PromptContent::Image { mime_type, bytes } => json!({
							"type": "image",
							"mimeType": mime_type,
							"data": omp_core::base64::encode(&bytes),
						}),
						PromptContent::Audio { mime_type, bytes } => json!({
							"type": "audio",
							"mimeType": mime_type,
							"data": omp_core::base64::encode(&bytes),
						}),
						PromptContent::Resource(resource) if resource.text => json!({
							"type": "resource",
							"resource": {
								"uri": resource.uri,
								"mimeType": resource.mime_type,
								"text": String::from_utf8_lossy(&resource.bytes),
							}
						}),
						PromptContent::Resource(resource) => json!({
							"type": "resource",
							"resource": {
								"uri": resource.uri,
								"mimeType": resource.mime_type,
								"blob": omp_core::base64::encode(&resource.bytes),
							}
						}),
					};
					json!({ "role": message.role, "content": content })
				})
				.collect::<Vec<_>>();
			let mut messages_json =
				serde_json::to_vec(&encoded).map_err(|_| McpServiceError::Backend)?;
			let max = usize::try_from(request.max_bytes).unwrap_or(usize::MAX);
			let truncated = messages_json.len() > max;
			if truncated {
				messages_json = br#"[{"role":"assistant","content":{"type":"text","text":"MCP prompt exceeded the configured size limit."}}]"#.to_vec();
			}
			Ok(pb::McpPromptResult {
				server: request.server,
				name: request.name,
				messages_json: messages_json.into(),
				truncated,
			})
		})
	}

	fn invoke(
		&self,
		request: pb::McpInvokeRequest,
		cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpInvokeResult, McpServiceError>> {
		Box::pin(async move {
			let manager = self.manager.upgrade().ok_or(McpServiceError::Backend)?;
			invoke::invoke(manager, request, cancel).await
		})
	}
}

struct UnavailableBackend;
impl McpServerBackend for UnavailableBackend {
	fn reset(&self, _: CancellationToken) -> BoxFuture<'_, Result<(), McpServiceError>> {
		async { Err(McpServiceError::Backend) }.boxed()
	}

	fn live_header(
		&self,
		_: pb::McpLiveHeaderRequest,
		_: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpLiveHeader, McpServiceError>> {
		async { Err(McpServiceError::Backend) }.boxed()
	}

	fn resource(
		&self,
		_: pb::McpResourceRequest,
		_: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpResourceResult, McpServiceError>> {
		async { Err(McpServiceError::Backend) }.boxed()
	}

	fn prompt(
		&self,
		_: pb::McpPromptRequest,
		_: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpPromptResult, McpServiceError>> {
		async { Err(McpServiceError::Backend) }.boxed()
	}

	fn invoke(
		&self,
		_: pb::McpInvokeRequest,
		_: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpInvokeResult, McpServiceError>> {
		async { Err(McpServiceError::Backend) }.boxed()
	}
}

fn manager_service_error(error: ManagerError) -> McpServiceError {
	match error {
		ManagerError::Cancelled => McpServiceError::Cancelled,
		_ => McpServiceError::Backend,
	}
}

fn is_unauthorized(result: &Result<Arc<LiveConnection>, ManagerError>) -> bool {
	match result {
		Err(ManagerError::Client(ClientError::Transport(error)))
		| Err(ManagerError::Transport(error)) => {
			matches!(error.cause, TransportFailure::HttpStatus { status: 401 | 403 })
		},
		_ => false,
	}
}

/// Lifecycle, transport, or definition publication failure.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
	/// Dynamic transport values could not be resolved without exposing secrets.
	#[error(transparent)]
	ConfigValues(#[from] ConfigValueError),
	/// The shared encrypted MCP credential authority is not attached.
	#[error("MCP credential authority is unavailable")]
	CredentialAuthorityUnavailable,
	/// The selected transport cannot use Environment-owned OAuth.
	#[error("MCP server does not declare an HTTP OAuth authorization flow")]
	UnsupportedAuthorization,
	/// Encrypted credential persistence failed.
	#[error(transparent)]
	CredentialStore(#[from] StoreError),
	/// OAuth discovery, authorization, or token exchange failed.
	#[error(transparent)]
	OAuth(#[from] OAuthFlowError),
	/// Declaration cannot construct the selected transport.
	#[error("MCP declaration is invalid")]
	InvalidConfig,
	/// Server is no longer mounted.
	#[error("MCP server is not mounted")]
	ServerNotFound,
	/// Requested supervisor generation was superseded.
	#[error("MCP manager generation is stale")]
	StaleGeneration,
	/// Server has no usable live connection.
	#[error("MCP server connection is unavailable")]
	ConnectionUnavailable,
	/// Automatic reconnect burst circuit is open.
	#[error("MCP automatic reconnect circuit is open")]
	CircuitOpen,
	/// Caller cancelled the operation.
	#[error("MCP manager operation was cancelled")]
	Cancelled,
	/// Effective connection deadline elapsed.
	#[error("MCP server connection timed out")]
	TimedOut,
	/// A CONTROL caller attempted to cross an extension-generation ownership
	/// boundary.
	#[error("MCP mount is owned by another extension generation")]
	OwnershipDenied,
	/// Tool list was malformed or exceeded pagination limits.
	#[error("MCP tool definitions are malformed")]
	MalformedDefinitions,
	/// Transport failed with dispatch evidence.
	#[error(transparent)]
	Transport(#[from] TransportError),
	/// Protocol initialization failed.
	#[error(transparent)]
	Client(#[from] ClientError),
	/// Dynamic device projection failed.
	#[error(transparent)]
	Device(#[from] DeviceError),
	/// Resource discovery failed.
	#[error(transparent)]
	Resource(#[from] ResourceError),
	/// Prompt discovery failed.
	#[error(transparent)]
	Prompt(#[from] PromptError),
	/// Revisioned leaf publication failed.
	#[error(transparent)]
	Service(#[from] McpServiceError),
}
#[cfg(test)]
mod tests {
	use super::*;
	use crate::mcp::{
		config_values::ResolvedTransportValues,
		json_rpc::RequestId,
		transport::{
			DispatchState, IncomingMessage, ServerResponseError, TransportFuture, TransportResponse,
		},
	};

	struct CatalogTransport {
		methods: Mutex<Vec<Str>>,
	}

	impl McpTransport for CatalogTransport {
		fn request<'a>(
			&'a self,
			method: &'a str,
			_params: Value,
			_cancellation: CancellationToken,
		) -> TransportFuture<'a, Result<TransportResponse, TransportError>> {
			self.methods.lock().push(Str::from(method));
			let result = match method {
				"resources/list" => json!({ "resources": [] }),
				"resources/templates/list" => json!({ "resourceTemplates": [] }),
				"prompts/list" => json!({ "prompts": [] }),
				_ => panic!("unexpected method {method}"),
			};
			Box::pin(async move {
				Ok(TransportResponse {
					id: RequestId::Number(1),
					result,
					dispatch: DispatchState::Responded,
				})
			})
		}

		fn notify<'a>(
			&'a self,
			_method: &'a str,
			_params: Value,
			_cancellation: CancellationToken,
		) -> TransportFuture<'a, Result<DispatchState, TransportError>> {
			Box::pin(async { Ok(DispatchState::Dispatched) })
		}

		fn next_message<'a>(
			&'a self,
			cancellation: CancellationToken,
		) -> TransportFuture<'a, Result<IncomingMessage, TransportError>> {
			Box::pin(async move {
				cancellation.cancelled().await;
				Err(TransportError::pre_dispatch(TransportFailure::Cancelled))
			})
		}

		fn respond<'a>(
			&'a self,
			_id: RequestId,
			_result: Result<Value, ServerResponseError>,
			_cancellation: CancellationToken,
		) -> TransportFuture<'a, Result<DispatchState, TransportError>> {
			Box::pin(async { Ok(DispatchState::Dispatched) })
		}

		fn close(&self) -> TransportFuture<'_, Result<(), TransportError>> {
			Box::pin(async { Ok(()) })
		}
	}

	struct CatalogConnector {
		transport: Arc<CatalogTransport>,
	}

	impl McpConnector for CatalogConnector {
		fn connect<'a>(
			&'a self,
			_spec: &'a MountSpec,
			roots: Arc<[Str]>,
			_cancel: CancellationToken,
		) -> Pin<Box<dyn Future<Output = Result<ConnectedClient, ManagerError>> + Send + 'a>> {
			let transport: Arc<dyn McpTransport> = self.transport.clone();
			Box::pin(async move {
				Ok(ConnectedClient {
					client:      Arc::new(McpClient::new(transport, roots)),
					initialized: InitializedServer {
						protocol_version: Str::from("2025-11-25"),
						name:             Str::from("resource-only"),
						version:          None,
						title:            None,
						description:      None,
						capabilities:     json!({ "resources": {}, "prompts": {} }),
						instructions:     None,
					},
				})
			})
		}
	}

	#[tokio::test]
	async fn resource_and_prompt_only_server_never_receives_tools_list() {
		let scratch = tempfile::tempdir().expect("scratch");
		let service = McpService::open(scratch.path().join("cache.sqlite3")).expect("service");
		let transport = Arc::new(CatalogTransport { methods: Mutex::new(Vec::new()) });
		let manager = McpManager::new(
			Arc::clone(&service),
			Arc::new(CatalogConnector { transport: transport.clone() }),
			Arc::from([]),
			scratch.path().to_path_buf(),
		);
		service.bind_manager(&manager);
		let config = Arc::new(
			serde_json::from_value::<McpServerConfig>(json!({
				"type": "http",
				"url": "https://example.test/mcp"
			}))
			.expect("config"),
		);
		let config_json = Bytes::from(serde_json::to_vec(config.as_ref()).expect("config JSON"));
		manager
			.start(vec![MountSpec {
				name: Str::from("resource-only"),
				config,
				config_json,
				values: ResolvedTransportValues::default(),
				auth_headers: None,
				suppressed_tools: BTreeSet::new(),
				projection: McpDeviceProjection::all(),
				auth: ControlMountAuth::None,
				restart: McpRestartPolicy::Never,
				owner: None,
			}])
			.await;
		assert_eq!(transport.methods.lock().clone(), vec![
			Str::from("resources/list"),
			Str::from("resources/templates/list"),
			Str::from("prompts/list"),
		]);
	}
}
