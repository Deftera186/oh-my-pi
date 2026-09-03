//! Application-supplied data feeds for dashboards and account commands.
//!
//! The chat actor is a projection over the session DOM (ADR 0005): facts
//! that live outside the journal — provider quotas, the kernel's tool
//! roster, extension and MCP status, stored OAuth accounts, on-disk
//! sessions, marketplace plugins — reach panels only through this seam.
//! The application implements [`Services`] once over `omp-inference`,
//! `omp-envd`, `omp-driver`, and `omp-cache`; the chat crate never depends
//! on those engines. Every method has a default that reports the feature
//! as unavailable, so a headless or test host needs no implementation.

use std::{path::PathBuf, time::Duration};

use flume::{Receiver, Sender};
use omp_core::Str;
use thiserror::Error;

/// Why a service request could not be served.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ServiceError {
	/// The application did not wire this feed (a headless or test host).
	#[error("{0} is unavailable in this host")]
	Unavailable(&'static str),
	/// The feed exists but the request failed.
	#[error("{0}")]
	Failed(Str),
}

impl ServiceError {
	/// Wraps any error as a failed request.
	pub fn failed(error: impl std::fmt::Display) -> Self {
		Self::Failed(Str::new(error.to_string()))
	}
}

/// Result of a service request.
pub type ServiceResult<T> = Result<T, ServiceError>;

/// Completion of an asynchronous service request: the host polls it from
/// a panel's `tick`, never blocking the actor.
pub type Pending<T> = Receiver<ServiceResult<T>>;

/// One quota window on a provider account (pi `UsageWindow`).
#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
	/// Window label (`5h`, `weekly`, `daily`).
	pub label:     Str,
	/// Fraction of the window consumed, `0.0..=1.0`.
	pub fraction:  f64,
	/// Time until the window resets, when the provider reports one.
	pub resets_in: Option<Duration>,
	/// Health of this window.
	pub status:    UsageStatus,
}

/// Health of a usage window or account card.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageStatus {
	/// Under the warning threshold.
	Ok,
	/// Near exhaustion.
	Warning,
	/// Exhausted until reset.
	Exhausted,
	/// No usage recorded (pi `IDLE_FRACTION`).
	Idle,
	/// The provider could not be queried.
	Unknown,
}

/// One provider account's quota card (pi `UsageReport`).
#[derive(Clone, Debug, PartialEq)]
pub struct UsageAccount {
	/// Provider identifier.
	pub provider: Str,
	/// Human-readable provider name.
	pub title:    Str,
	/// Account labels sharing this card.
	pub accounts: Vec<Str>,
	/// Quota windows, most granular first.
	pub windows:  Vec<UsageWindow>,
	/// Query failure, when the provider could not be reached.
	pub error:    Option<Str>,
}

/// One day of local cost activity for the heatmap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageDay {
	/// Day start, milliseconds since the Unix epoch (UTC).
	pub day_ms:        u64,
	/// Cost in nano-dollars.
	pub cost_nano_usd: u64,
	/// Inference requests.
	pub requests:      u64,
}

/// Everything the usage dashboard shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageReport {
	/// When the provider quotas were last fetched (Unix milliseconds).
	pub checked_at_ms: Option<u64>,
	/// Provider quota cards.
	pub accounts:      Vec<UsageAccount>,
	/// Daily activity, oldest first.
	pub activity:      Vec<UsageDay>,
	/// Why `activity` is empty when the host has no local cost history
	/// (pi `activityError`); `None` means the heatmap is authoritative.
	pub activity_note: Option<Str>,
	/// Preformatted per-account detail report (pi `renderDetail`).
	pub detail:        Str,
}

/// One kernel-registered tool (pi `/tools`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRow {
	/// Tool name.
	pub name:        Str,
	/// Tool description (first line is the summary).
	pub description: Str,
	/// Schema revision.
	pub rev:         u32,
	/// Trust tier, when the registry assigns one.
	pub tier:        Option<Str>,
	/// Whether the tool is active in the current session roster.
	pub active:      bool,
	/// Where the tool comes from (`builtin`, `mcp:<server>`, `ext:<name>`).
	pub source:      Str,
}

/// One configured SSH host (pi `/ssh list`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshHostRow {
	/// Host alias.
	pub name:    Str,
	/// `user@host:port`.
	pub target:  Str,
	/// Scope the declaration lives in (`project` or `user`).
	pub scope:   Str,
	/// Authentication policy (`agent` or the key path).
	pub auth:    Str,
}

/// Where an extension row comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionKind {
	/// MCP server.
	Mcp,
	/// Built-in extension shipped with the binary.
	Builtin,
	/// Python extension loaded by envd.
	Python,
	/// Marketplace plugin.
	Plugin,
}

impl ExtensionKind {
	/// Tab label (pi provider tabs).
	#[must_use]
	pub const fn label(self) -> &'static str {
		match self {
			Self::Mcp => "mcp",
			Self::Builtin => "builtin",
			Self::Python => "python",
			Self::Plugin => "plugin",
		}
	}
}

/// Runtime health of an extension or MCP server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionStatus {
	/// Starting or handshaking.
	Connecting,
	/// Loaded and serving.
	Ready,
	/// Cleanly stopped.
	Disconnected,
	/// Failed to load or crashed.
	Failed,
	/// Disabled by configuration.
	Disabled,
}

/// One extension dashboard row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionRow {
	/// Stable identifier.
	pub id:          Str,
	/// Display name.
	pub name:        Str,
	/// Source kind (dashboard tab).
	pub kind:        ExtensionKind,
	/// Runtime health.
	pub status:      ExtensionStatus,
	/// Whether configuration enables it.
	pub enabled:     bool,
	/// Implementation name and version, when reported.
	pub version:     Option<Str>,
	/// Free-form description.
	pub description: Option<Str>,
	/// Tools it registers.
	pub tools:       Vec<Str>,
	/// Resources it exposes (MCP).
	pub resources:   Vec<Str>,
	/// Prompts it exposes (MCP).
	pub prompts:     Vec<Str>,
	/// Last error, when failed.
	pub error:       Option<Str>,
}

/// One stored provider account (pi `/logout` selector row).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountRow {
	/// Stable account identifier.
	pub id:            Str,
	/// Provider identifier.
	pub provider:      Str,
	/// Human-readable provider name.
	pub provider_name: Str,
	/// Account label (email or account id).
	pub label:         Str,
	/// Secondary detail (plan, expiry).
	pub detail:        Str,
	/// Credential kind (`oauth`, `api-key`).
	pub kind:          Str,
	/// Whether this account currently serves the provider.
	pub active:        bool,
}

/// One provider offered by `/login` and `/setup`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRow {
	/// Provider identifier.
	pub id:        Str,
	/// Human-readable provider name.
	pub name:      Str,
	/// Whether the provider supports interactive OAuth sign-in.
	pub oauth:     bool,
	/// Whether a credential is already stored.
	pub logged_in: bool,
}

/// An in-flight interactive login (pi `LoginDialogComponent`).
///
/// The driver pushes what the dialog must show; the dialog feeds pasted
/// callback URLs or codes back through `input`; `done` settles once.
pub struct LoginFlow {
	/// Provider being signed in.
	pub provider:      Str,
	/// Human-readable provider name for the title.
	pub provider_name: Str,
	/// Dialog updates from the driver, in order.
	pub events:        Receiver<LoginEvent>,
	/// Pasted redirect URL or verification code.
	pub input:         Sender<Str>,
	/// Final outcome with a user-facing message.
	pub done:          Pending<Str>,
	/// Aborts the flow when the dialog is cancelled.
	pub cancel:        Sender<()>,
}

/// One login dialog update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginEvent {
	/// Open (or show) this authorization URL.
	OpenUrl {
		/// Authorization URL.
		url:      Str,
		/// Whether the driver launched a browser itself.
		launched: bool,
	},
	/// Show a device code to enter at `verification_url`.
	DeviceCode {
		/// User code.
		code:             Str,
		/// Where to enter it.
		verification_url: Str,
	},
	/// Ask the user to paste a callback URL or code.
	Prompt {
		/// Prompt label.
		label: Str,
	},
	/// Informational line.
	Info(Str),
}

/// One journal entry as the tree selector sees it (pi `tree-selector.ts`
/// node): only user turns, assistant messages, and branch points carry
/// text; everything else is a structural link.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry {
	/// Journal entry id.
	pub id:     omp_journal::EntryId,
	/// Parent entry on the tree: the explicit `prior` when the entry
	/// branched, else the preceding entry in the file.
	pub parent: Option<omp_journal::EntryId>,
	/// Journal kind name (`turn.start`, `msg.user`, `msg.assistant.start`, …).
	pub kind:   Str,
	/// Preview text for user/assistant messages; empty for structure.
	pub text:   Str,
	/// Whether the entry is on the live chain that ends at the head.
	pub live:   bool,
	/// Whether this entry is the current head.
	pub head:   bool,
}

/// One update from a `/btw` side answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SideEvent {
	/// Streamed answer text.
	Delta(Str),
	/// The answer finished.
	Done,
	/// The side kernel failed.
	Error(Str),
}

/// One on-disk session (pi session picker row / agents hub child).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRow {
	/// Stable session id (journal stem).
	pub id:          Str,
	/// Journal path.
	pub path:        PathBuf,
	/// Title, when named.
	pub title:       Option<Str>,
	/// Creation time, Unix milliseconds.
	pub created_ms:  u64,
	/// Last modification, Unix milliseconds.
	pub modified_ms: u64,
	/// User + assistant message count.
	pub messages:    u32,
	/// Parent session id for subagent children.
	pub parent:      Option<Str>,
	/// Agent class name for subagent children.
	pub agent:       Option<Str>,
	/// Whether the session is pinned to the top of the resume list.
	pub pinned:      bool,
}

/// One agent definition (pi agents hub row).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRow {
	/// Agent class name.
	pub name:        Str,
	/// Where it is defined (`bundled`, `project`, `user`).
	pub source:      Str,
	/// One-line description.
	pub description: Str,
	/// Model pattern bound to the agent, when configured.
	pub model:       Option<Str>,
	/// Tools the agent may use; empty means the full roster.
	pub tools:       Vec<Str>,
	/// Whether the agent is enabled for spawning.
	pub enabled:     bool,
	/// Path of its definition file, when on disk.
	pub path:        Option<PathBuf>,
}

/// One marketplace plugin (pi plugin selector row).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginRow {
	/// Stable plugin id.
	pub id:          Str,
	/// Display name.
	pub name:        Str,
	/// Version tag.
	pub version:     Option<Str>,
	/// Description.
	pub description: Str,
	/// Marketplace the plugin comes from.
	pub marketplace: Str,
	/// Whether it is installed.
	pub installed:   bool,
	/// Whether it is enabled.
	pub enabled:     bool,
	/// Installation scope (`user`, `project`); empty when not installed.
	pub scope:       Str,
	/// Whether a project-scope install shadows this user-scope entry.
	pub shadowed:    bool,
}

/// One configured marketplace source (pi `/marketplace list` row).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarketplaceSource {
	/// Catalog name.
	pub name: Str,
	/// Source URI it was added from (`owner/repo`, URL, or path).
	pub uri:  Str,
}

/// Marketplace state (pi `/marketplace`, `/plugins`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PluginsReport {
	/// Configured marketplace sources.
	pub marketplaces: Vec<Str>,
	/// Known plugins, installed first.
	pub plugins:      Vec<PluginRow>,
	/// Configured marketplace sources with their URIs; the same set as
	/// `marketplaces`, in the same order.
	pub sources:      Vec<MarketplaceSource>,
}

/// `/memory` subcommands (pi `builtin-lifecycle.ts` `/memory`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::EnumString, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum MemoryOp {
	/// Show the injected memory payload.
	View,
	/// Show bank counts.
	Stats,
	/// Run bank diagnostics.
	Diagnose,
	/// Clear the memory bank.
	Clear,
	/// Enqueue a consolidation pass.
	Enqueue,
}

/// `/cleanse [request] [--all]` options.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CleanseRequest {
	/// Free-form checker request; `None` runs discovered checkers.
	pub request: Option<Str>,
	/// Run every discovered checker.
	pub all:     bool,
	/// Include configured project tests.
	pub tests:   bool,
}

/// Settled cleanse run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CleanseOutcome {
	/// Terminal status word (`clean`, `unresolved`, `unsupported`, `cancelled`).
	pub status:    Str,
	/// One-paragraph summary for the panel.
	pub summary:   Str,
	/// Remaining file groups (`path: N issues`), at most 50.
	pub remainder: Vec<Str>,
}

/// An in-flight cleanse run.
pub struct CleanseRun {
	/// Final outcome.
	pub done:   Pending<CleanseOutcome>,
	/// Cancels the run (Esc in the side panel).
	pub cancel: Sender<()>,
}

/// One `/ssh add` declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshHostSpec {
	/// Host alias.
	pub alias:    Str,
	/// DNS name or address.
	pub address:  Str,
	/// Remote account.
	pub user:     Str,
	/// SSH port.
	pub port:     u16,
	/// `SHA256:` host-key fingerprint.
	pub host_key: Str,
	/// Private key path; `None` uses the agent.
	pub key:      Option<PathBuf>,
	/// `true` writes the project scope (`.omp/hosts.toml`), else the user
	/// scope.
	pub project:  bool,
}

/// Application-supplied feeds for commands and dashboards. Every method
/// defaults to [`ServiceError::Unavailable`].
pub trait Services: Send + Sync {
	/// Provider quotas and local cost activity. Quota refreshes contact
	/// every provider, so the report settles asynchronously.
	fn usage(&self) -> ServiceResult<Pending<UsageReport>> {
		Err(ServiceError::Unavailable("usage"))
	}

	/// `/usage reset [account|active]`: spend a saved rate-limit reset.
	fn reset_usage(&self, _target: &str) -> ServiceResult<Str> {
		Err(ServiceError::Unavailable("usage reset"))
	}

	/// The kernel's registered tools.
	fn tools(&self) -> ServiceResult<Vec<ToolRow>> {
		Err(ServiceError::Unavailable("tool roster"))
	}

	/// Extension and MCP server status.
	fn extensions(&self) -> ServiceResult<Vec<ExtensionRow>> {
		Err(ServiceError::Unavailable("extensions"))
	}

	/// Enables or disables one extension by id.
	fn set_extension_enabled(&self, _id: &str, _enabled: bool) -> ServiceResult<()> {
		Err(ServiceError::Unavailable("extension toggling"))
	}

	/// `/reload-plugins`: reload every extension runtime from disk.
	fn reload_extensions(&self) -> ServiceResult<Pending<Str>> {
		Err(ServiceError::Unavailable("extension reload"))
	}

	/// Stored provider accounts.
	fn accounts(&self) -> ServiceResult<Vec<AccountRow>> {
		Err(ServiceError::Unavailable("stored accounts"))
	}

	/// Providers that can be signed in.
	fn providers(&self) -> ServiceResult<Vec<ProviderRow>> {
		Err(ServiceError::Unavailable("provider roster"))
	}

	/// Starts an interactive login for `provider`.
	fn login(&self, _provider: &str) -> ServiceResult<LoginFlow> {
		Err(ServiceError::Unavailable("login"))
	}

	/// Deletes one stored account.
	fn logout(&self, _account: &AccountRow) -> ServiceResult<Pending<()>> {
		Err(ServiceError::Unavailable("logout"))
	}

	/// Pins (or unpins, when `pinned` is false) the account that serves
	/// `provider` in this session.
	fn pin_account(&self, _account: &AccountRow, _pinned: bool) -> ServiceResult<Str> {
		Err(ServiceError::Unavailable("account pinning"))
	}

	/// Exports the live session; `None` picks the default path beside the
	/// journal. Returns the written path.
	fn export(&self, _path: Option<&std::path::Path>) -> ServiceResult<PathBuf> {
		Err(ServiceError::Unavailable("export"))
	}

	/// On-disk sessions, newest first.
	fn sessions(&self) -> ServiceResult<Vec<SessionRow>> {
		Err(ServiceError::Unavailable("session index"))
	}

	/// Pins or unpins a stored session in the resume list.
	fn pin_session(&self, _id: &str, _pinned: bool) -> ServiceResult<()> {
		Err(ServiceError::Unavailable("session pinning"))
	}

	/// Id (journal stem) of the live session, for `/pin` without an
	/// argument. `Failed` when the session is in-memory only.
	fn live_session_id(&self) -> ServiceResult<Str> {
		Err(ServiceError::Unavailable("live session id"))
	}

	/// Renames a stored session in the index (session picker Ctrl+R).
	fn rename_session(&self, _id: &str, _title: &str) -> ServiceResult<()> {
		Err(ServiceError::Unavailable("session rename"))
	}

	/// Deletes a stored session file (session picker Ctrl+D).
	fn delete_session(&self, _id: &str) -> ServiceResult<()> {
		Err(ServiceError::Unavailable("session delete"))
	}

	/// Reads a session-local artifact (`local://PLAN.md`) as text.
	fn read_local(&self, _url: &str) -> ServiceResult<Str> {
		Err(ServiceError::Unavailable("local artifacts"))
	}

	/// Session-local artifact URLs matching a suffix (`-plan.md`, `.md`),
	/// newest first.
	fn list_local(&self, _suffix: &str) -> ServiceResult<Vec<Str>> {
		Err(ServiceError::Unavailable("local artifacts"))
	}

	/// The live session's journal as a branch DAG (`/tree`): every entry
	/// with its parent link, so the tree selector can draw forks.
	fn journal_tree(&self) -> ServiceResult<Vec<TreeEntry>> {
		Err(ServiceError::Unavailable("journal tree"))
	}

	/// `/btw`: answers a side question on a tool-less child kernel seeded
	/// with `context`, streaming text deltas then one terminal event.
	fn btw(&self, _question: &str, _context: &str) -> ServiceResult<Receiver<SideEvent>> {
		Err(ServiceError::Unavailable("side questions"))
	}

	/// Agent definitions available to `task`.
	fn agents(&self) -> ServiceResult<Vec<AgentRow>> {
		Err(ServiceError::Unavailable("agent definitions"))
	}

	/// Enables or disables one agent definition.
	fn set_agent_enabled(&self, _name: &str, _enabled: bool) -> ServiceResult<()> {
		Err(ServiceError::Unavailable("agent toggling"))
	}

	/// Marketplace sources and plugins.
	fn plugins(&self) -> ServiceResult<PluginsReport> {
		Err(ServiceError::Unavailable("marketplace"))
	}

	/// Installs a plugin; the receiver settles with a status line.
	fn install_plugin(&self, _id: &str) -> ServiceResult<Pending<Str>> {
		Err(ServiceError::Unavailable("plugin install"))
	}

	/// Uninstalls a plugin; the receiver settles with a status line.
	fn uninstall_plugin(&self, _id: &str) -> ServiceResult<Pending<Str>> {
		Err(ServiceError::Unavailable("plugin uninstall"))
	}

	/// Enables or disables an installed plugin.
	fn set_plugin_enabled(&self, _id: &str, _enabled: bool) -> ServiceResult<()> {
		Err(ServiceError::Unavailable("plugin toggling"))
	}

	/// Adds a marketplace source.
	fn add_marketplace(&self, _source: &str) -> ServiceResult<Str> {
		Err(ServiceError::Unavailable("marketplace sources"))
	}

	/// Removes a marketplace source.
	fn remove_marketplace(&self, _name: &str) -> ServiceResult<Str> {
		Err(ServiceError::Unavailable("marketplace sources"))
	}

	/// `/marketplace update [name]`: re-fetches one or every catalog; the
	/// receiver settles with a status line.
	fn update_marketplace(&self, _name: Option<&str>) -> ServiceResult<Pending<Str>> {
		Err(ServiceError::Unavailable("marketplace update"))
	}

	/// `/marketplace upgrade [name@marketplace]`: upgrades one or every
	/// outdated plugin; the receiver settles with a status line.
	fn upgrade_plugins(&self, _spec: Option<&str>) -> ServiceResult<Pending<Str>> {
		Err(ServiceError::Unavailable("plugin upgrade"))
	}

	/// `/memory` operations; returns the text to show.
	fn memory(&self, _op: MemoryOp) -> ServiceResult<Str> {
		Err(ServiceError::Unavailable("memory bank"))
	}

	/// Release notes shipped with the binary (markdown).
	fn changelog(&self) -> ServiceResult<Str> {
		Err(ServiceError::Unavailable("changelog"))
	}

	/// Configured SSH hosts.
	fn ssh_hosts(&self) -> ServiceResult<Vec<SshHostRow>> {
		Err(ServiceError::Unavailable("ssh hosts"))
	}

	/// Adds or replaces one SSH host declaration; returns the status line.
	fn ssh_add(&self, _spec: &SshHostSpec) -> ServiceResult<Str> {
		Err(ServiceError::Unavailable("ssh hosts"))
	}

	/// Removes one SSH host declaration; returns the status line.
	fn ssh_remove(&self, _alias: &str, _project: bool) -> ServiceResult<Str> {
		Err(ServiceError::Unavailable("ssh hosts"))
	}

	/// Seals and uploads a share snapshot; settles with the viewer URL.
	fn share(&self, _snapshot: serde_json::Value) -> ServiceResult<Pending<Str>> {
		Err(ServiceError::Unavailable("share"))
	}

	/// Starts a cleanse run over the project.
	fn cleanse(&self, _request: CleanseRequest) -> ServiceResult<CleanseRun> {
		Err(ServiceError::Unavailable("cleanse"))
	}
}

/// A host with no application feeds (tests, `omp render`).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoServices;

impl Services for NoServices {}
