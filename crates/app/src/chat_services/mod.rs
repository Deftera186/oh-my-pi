//! Application implementation of the chat host's [`Services`] seam: the
//! data feeds behind `/usage`, `/tools`, `/extensions`, `/login`, `/hub`,
//! `/plugins`, `/export`, … built once from the composed kernel and handed
//! to the actor as `HostOptions.services` (ADR 0005: the actor stays a
//! projection; engines stay in the app).

use std::{path::PathBuf, sync::Arc};

use omp_chat::overlays::services::{
	AccountRow, AgentRow, CleanseRequest, CleanseRun, ExtensionRow, LoginFlow, MemoryOp, Pending,
	PluginsReport, ServiceResult, Services, SessionRow, SshHostRow, SshHostSpec, ToolRow,
	UsageReport,
};
use omp_core::Str;
use omp_driver::registry::ProductionInference as ProductionStack;

mod accounts;
mod extensions;
mod misc;
mod plugins;
mod session_ops;
mod sessions;
mod tools;
mod usage;

/// Everything the feeds need, captured once at chat launch.
pub struct ServiceState {
	/// User data directory (`credentials.db`, caches).
	pub data_dir:     PathBuf,
	/// Canonical project root.
	pub project:      PathBuf,
	/// Project state directory (`sessions/`, `ssh/`).
	pub state_dir:    PathBuf,
	/// Durable session directory.
	pub sessions_dir: PathBuf,
	/// Journal path at launch.
	pub journal:      PathBuf,
	/// Current journal path: `/new`, `/resume`, and `/fork` swap sessions in
	/// process, and the controller writes the new path here on every switch.
	pub live_journal: Arc<parking_lot::RwLock<PathBuf>>,
	/// Resolved launch model key (child kernels for `/btw`).
	pub model:        Str,
	/// Catalog snapshot; `None` behind a remote gateway.
	pub catalog:      Option<Arc<omp_catalog::snapshot::Catalog>>,
	/// Kernel tool registry.
	pub registry:     Arc<omp_tool::Registry>,
	/// Process console.
	pub con:          Arc<omp_con::Ctx>,
	/// MCP inspection authority.
	pub mcp:          omp_envd::McpInspectorHandle,
	/// Extension hot-reload authority.
	pub reload:       omp_envd::ExtensionReloadHandle,
	/// The session's memory runtime (`/memory`).
	pub memory:       Arc<omp_memory::MemoryRuntime>,
	/// Production auth + usage stack; `None` behind a remote gateway.
	pub stack:        Option<StackHandles>,
	/// Runtime the asynchronous feeds spawn onto.
	pub runtime:      tokio::runtime::Handle,
}

/// Cloneable handles into the production authentication and usage stack.
#[derive(Clone)]
pub struct StackHandles {
	/// Authentication owner.
	pub auth:         omp_inference::auth::AuthManager,
	/// Lifecycle CONTROL view of the same owner.
	pub auth_control: omp_inference::auth::AuthControlHandle,
	/// Provider usage fetchers.
	pub usage:        omp_inference::operation::usage::UsageFetcherRegistry,
	/// Combined credential authority (GitHub gist uploads for `/share`).
	pub credential_authority: Arc<dyn omp_envd::github_url::CredentialAuthority>,
}

impl StackHandles {
	/// Clones the handles out of a composed production stack.
	#[must_use]
	pub fn from_stack(stack: &ProductionStack) -> Self {
		Self {
			auth:                 stack.auth_manager.clone(),
			auth_control:         stack.auth_control.clone(),
			usage:                stack.usage_fetchers.clone(),
			credential_authority: Arc::clone(&stack.credential_authority),
		}
	}
}

/// [`Services`] over the composed kernel.
pub struct AppServices {
	state: Arc<ServiceState>,
}

impl AppServices {
	/// Wraps the captured state and points `<img src="artifact://sha256/…">`
	/// (tool-result image blobs in the transcript) at the project blob
	/// store.
	#[must_use]
	pub fn new(state: ServiceState) -> Self {
		if let Ok(blobs) = omp_journal::blob::BlobStore::open(state.state_dir.join("blobs")) {
			omp_tui::register_image_scheme(
				"artifact",
				Arc::new(move |source: &str| {
					let hex = source.strip_prefix("artifact://sha256/")?;
					let reference = omp_journal::blob::BlobRef::parse_hex(hex, 0).ok()?;
					Some(blobs.path(&reference))
				}),
			);
		}
		Self { state: Arc::new(state) }
	}
}

impl Services for AppServices {
	fn usage(&self) -> ServiceResult<Pending<UsageReport>> {
		usage::fetch(&self.state)
	}

	fn reset_usage(&self, target: &str) -> ServiceResult<Str> {
		usage::reset(&self.state, target)
	}

	fn tools(&self) -> ServiceResult<Vec<ToolRow>> {
		tools::roster(&self.state)
	}

	fn extensions(&self) -> ServiceResult<Vec<ExtensionRow>> {
		extensions::rows(&self.state)
	}

	fn set_extension_enabled(&self, id: &str, enabled: bool) -> ServiceResult<()> {
		extensions::set_enabled(&self.state, id, enabled)
	}

	fn reload_extensions(&self) -> ServiceResult<Pending<Str>> {
		extensions::reload(&self.state)
	}

	fn accounts(&self) -> ServiceResult<Vec<AccountRow>> {
		accounts::rows(&self.state)
	}

	fn providers(&self) -> ServiceResult<Vec<omp_chat::overlays::services::ProviderRow>> {
		accounts::providers(&self.state)
	}

	fn login(&self, provider: &str) -> ServiceResult<LoginFlow> {
		accounts::login(&self.state, provider)
	}

	fn logout(&self, account: &AccountRow) -> ServiceResult<Pending<()>> {
		accounts::logout(&self.state, account)
	}

	fn pin_account(&self, account: &AccountRow, pinned: bool) -> ServiceResult<Str> {
		accounts::pin(&self.state, account, pinned)
	}

	fn live_session_id(&self) -> ServiceResult<Str> {
		accounts::live_session_id(&self.state)
	}

	fn export(&self, path: Option<&std::path::Path>) -> ServiceResult<PathBuf> {
		misc::export(&self.state, path)
	}

	fn sessions(&self) -> ServiceResult<Vec<SessionRow>> {
		sessions::rows(&self.state)
	}

	fn pin_session(&self, id: &str, pinned: bool) -> ServiceResult<()> {
		sessions::pin(&self.state, id, pinned)
	}

	fn agents(&self) -> ServiceResult<Vec<AgentRow>> {
		sessions::agents(&self.state)
	}

	fn set_agent_enabled(&self, name: &str, enabled: bool) -> ServiceResult<()> {
		sessions::set_agent_enabled(&self.state, name, enabled)
	}

	fn plugins(&self) -> ServiceResult<PluginsReport> {
		plugins::report(&self.state)
	}

	fn install_plugin(&self, id: &str) -> ServiceResult<Pending<Str>> {
		plugins::install(&self.state, id)
	}

	fn uninstall_plugin(&self, id: &str) -> ServiceResult<Pending<Str>> {
		plugins::uninstall(&self.state, id)
	}

	fn set_plugin_enabled(&self, id: &str, enabled: bool) -> ServiceResult<()> {
		plugins::set_enabled(&self.state, id, enabled)
	}

	fn add_marketplace(&self, source: &str) -> ServiceResult<Str> {
		plugins::add_marketplace(&self.state, source)
	}

	fn remove_marketplace(&self, name: &str) -> ServiceResult<Str> {
		plugins::remove_marketplace(&self.state, name)
	}

	fn update_marketplace(&self, name: Option<&str>) -> ServiceResult<Pending<Str>> {
		plugins::update_marketplace(&self.state, name)
	}

	fn upgrade_plugins(&self, spec: Option<&str>) -> ServiceResult<Pending<Str>> {
		plugins::upgrade(&self.state, spec)
	}

	fn memory(&self, op: MemoryOp) -> ServiceResult<Str> {
		misc::memory(&self.state, op)
	}

	fn changelog(&self) -> ServiceResult<Str> {
		misc::changelog()
	}

	fn ssh_hosts(&self) -> ServiceResult<Vec<SshHostRow>> {
		misc::ssh_hosts(&self.state)
	}

	fn ssh_add(&self, spec: &SshHostSpec) -> ServiceResult<Str> {
		misc::ssh_add(&self.state, spec)
	}

	fn ssh_remove(&self, alias: &str, project: bool) -> ServiceResult<Str> {
		misc::ssh_remove(&self.state, alias, project)
	}

	fn share(&self, snapshot: serde_json::Value) -> ServiceResult<Pending<Str>> {
		misc::share(&self.state, snapshot)
	}

	fn cleanse(&self, request: CleanseRequest) -> ServiceResult<CleanseRun> {
		misc::cleanse(&self.state, request)
	}

	fn rename_session(&self, id: &str, title: &str) -> ServiceResult<()> {
		session_ops::rename(&self.state, id, title)
	}

	fn delete_session(&self, id: &str) -> ServiceResult<()> {
		session_ops::delete(&self.state, id)
	}

	fn read_local(&self, url: &str) -> ServiceResult<Str> {
		session_ops::read_local(&self.state, url)
	}

	fn list_local(&self, suffix: &str) -> ServiceResult<Vec<Str>> {
		session_ops::list_local(&self.state, suffix)
	}

	fn journal_tree(&self) -> ServiceResult<Vec<omp_chat::overlays::services::TreeEntry>> {
		session_ops::journal_tree(&self.state)
	}

	fn btw(
		&self,
		question: &str,
		context: &str,
	) -> ServiceResult<flume::Receiver<omp_chat::overlays::services::SideEvent>> {
		session_ops::btw(&self.state, question, context)
	}
}
