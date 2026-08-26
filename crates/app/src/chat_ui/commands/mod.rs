//! Structural slash-command router and capability-scoped host contracts.

mod advisor;
pub(super) mod asides;
pub(crate) mod browser;
pub(crate) mod collab;
mod config;
mod diagnostics;
pub(crate) use diagnostics::{render_debug, run_cleanse};
pub mod context;
mod export;
#[path = "extensions/runtime.rs"]
pub(crate) mod extension_runtime;
mod extensions;
pub(crate) use extensions::{build_inspector_snapshot_from_declarations, snapshot_live_mcp};
mod flow;
mod git;
mod green;
mod mcp;
mod memory;
mod model;
pub(crate) use model::resolve_extended_context;
pub mod registry;
pub mod result;
mod review;
mod security;
mod session;
mod share;
pub(crate) mod ssh;
pub(crate) mod utility;
pub(crate) mod workspace;

use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use omp_agent::ManualCompactionRequest;
use omp_core::{Str, sf};
pub use registry::{
	AdvertisedCommand, ArgumentHint, CommandCapability, CommandDeclaration, CommandGeneration,
	CommandImplementation, CommandProvenance, CommandRole, CommandRoster, CommandSourceKind,
	CommandSurface, ShadowPolicy, ShadowRule,
};
pub use result::{CommandResult, ConsumedResult, DispatchResult, PromptResult};

/// Cold command future allocated only after an explicit user command.
pub type CommandFuture<'a> =
	Pin<Box<dyn Future<Output = miette::Result<CommandResult>> + Send + 'a>>;

/// Erased structural handler generated beside its command declaration.
pub type CommandHandler =
	for<'a> fn(&'a mut dyn CommandHost, &'a str, &'a CommandProvenance) -> CommandFuture<'a>;

/// Parsed `/session` operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionRequest {
	/// Show durable session information.
	Info,
	/// Delete through the guarded session authority.
	Delete {
		/// Skip the 30-second re-run confirmation window (`/drop`).
		force: bool,
	},
	/// Toggle one durable session's resume-list pin.
	Pin(Option<Str>),
}

/// Parsed workspace-root operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceRequest {
	/// Replace the future primary root.
	Move(Str),
	/// Add a supplementary root.
	Add(Str),
	/// Remove a supplementary root.
	Remove(Str),
	/// List current roots.
	List,
}

/// Parsed command-line flags with optional values.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedFlags(pub Vec<(Str, Option<Str>)>);
/// Parsed advisor watchdog operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdvisorRequest {
	/// Toggle the current advisor state.
	Toggle,
	/// Set advisor enablement explicitly.
	SetEnabled(bool),
	/// Show advisor state and budget.
	Status,
	/// Copy the raw advisor transcript projection.
	DumpRaw,
	/// Apply advisor settings expressed in the native command grammar.
	Configure(Str),
}

/// Parsed version-history view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangelogRequest {
	/// Show the latest release entries.
	Recent,
	/// Show complete bundled version history.
	Full,
}

/// Parsed desktop automation override.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComputerRequest {
	/// Enable desktop automation for the session.
	On,
	/// Disable desktop automation for the session.
	Off,
	/// Follow model and host capabilities.
	Auto,
	/// Show the effective override and permissions.
	Status,
	/// Run permission diagnostics.
	Diagnose,
}

/// Parsed image-tool exposure override.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisionRequest {
	/// Expose image inspection.
	On,
	/// Hide image inspection.
	Off,
	/// Follow model capabilities.
	Auto,
	/// Show effective image-tool exposure.
	Status,
}
/// Parsed browser surface-mode operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserRequest {
	/// Invert the current headless setting.
	Toggle,
	/// Use an offscreen frame surface.
	Headless,
	/// Use an engine-owned visible window.
	Visible,
}

/// Parsed utility command operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UtilityRequest {
	/// Render bundled version history.
	Changelog(ChangelogRequest),
	/// List active and disabled tools.
	Tools,
	/// Control desktop automation.
	Computer(ComputerRequest),
	/// Control image-tool delegation.
	Vision(VisionRequest),
}

/// Parsed branch creation operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchRequest {
	/// Optional durable checkpoint selector.
	pub checkpoint: Option<Str>,
}

/// Parsed native SSH host operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SshRequest {
	/// List effective declarations using project-over-user precedence, with an
	/// optional scope filter.
	List(Option<ConfigScope>),
	/// Add or replace one scoped host declaration.
	Add {
		/// Host alias.
		alias:    Str,
		/// Address or DNS name.
		host:     Str,
		/// Remote user.
		user:     Str,
		/// Validated TCP port.
		port:     u16,
		/// Pinned SHA-256 host-key fingerprint.
		host_key: Str,
		/// Optional unencrypted private-key path; omission uses the native SSH
		/// agent.
		key:      Option<PathBuf>,
		/// Writable declaration scope.
		scope:    ConfigScope,
	},
	/// Remove one scoped declaration.
	Remove {
		/// Host alias.
		alias: Str,
		/// Writable declaration scope.
		scope: ConfigScope,
	},
	/// Render native SSH help.
	Help,
}

/// Shell-scoped command capabilities.
pub trait ShellCommandHost {
	/// Render help from the live roster.
	fn help(&mut self) -> CommandFuture<'_>;
	/// Start a new durable session.
	fn new_session(&mut self) -> CommandFuture<'_>;
	/// List active background jobs.
	fn jobs(&mut self) -> CommandFuture<'_>;
	/// Open the live agent hierarchy.
	fn agents(&mut self) -> CommandFuture<'_>;
	/// Pause the interactive session.
	fn pause(&mut self) -> CommandFuture<'_>;
	/// Exit the initiating client.
	fn quit(&mut self) -> CommandFuture<'_>;
}

/// Session/workspace-scoped command capabilities.
pub trait SessionCommandHost {
	/// Append an in-journal context reset.
	fn clear(&mut self) -> CommandFuture<'_>;
	/// Append a provider-reset hint.
	fn fresh(&mut self) -> CommandFuture<'_>;
	/// Assign a durable title.
	fn rename(&mut self, title: Str) -> CommandFuture<'_>;
	/// Retry the latest durable user turn.
	fn retry(&mut self) -> CommandFuture<'_>;
	/// Resume a native selector, or open the picker.
	fn resume(&mut self, selector: Option<Str>) -> CommandFuture<'_>;
	/// Execute a structured session operation.
	fn session(&mut self, request: SessionRequest) -> CommandFuture<'_>;
	/// Execute a structured workspace operation.
	fn workspace(&mut self, request: WorkspaceRequest) -> CommandFuture<'_>;
	/// Open the interactive Git workbench, optionally pinned to a revision.
	fn git(&mut self, revision: Option<Str>) -> CommandFuture<'_>;
	/// Summarize the session into a handoff document and compact it in place.
	fn handoff(&mut self, instructions: Option<Str>) -> CommandFuture<'_> {
		let _ = instructions;
		Box::pin(async { Err(miette::miette!("session handoff is unavailable")) })
	}
	/// Create a lineage child at an optional durable checkpoint.
	fn branch(&mut self, request: BranchRequest) -> CommandFuture<'_> {
		let _ = request;
		Box::pin(async { Err(miette::miette!("session branching is unavailable")) })
	}
	/// Fork the current live projection into an independent session.
	fn fork(&mut self, title: Option<Str>) -> CommandFuture<'_> {
		let _ = title;
		Box::pin(async { Err(miette::miette!("session forking is unavailable")) })
	}
	/// Render durable branch lineage.
	fn branch_tree(&mut self) -> CommandFuture<'_> {
		Box::pin(async { Err(miette::miette!("session branch navigation is unavailable")) })
	}
	/// Open a named inspector or the inspector selector.
	fn debug(&mut self, inspector: Option<Str>) -> CommandFuture<'_> {
		let _ = inspector;
		Box::pin(async { Err(miette::miette!("session inspection is unavailable")) })
	}
}

/// Model-scoped command capabilities.
pub trait ModelCommandHost {
	/// Set or select the durable model preference.
	fn model(&mut self, selector: Option<Str>) -> CommandFuture<'_>;
	/// Set a resume-stable session override, or request interactive selection.
	fn switch(&mut self, selector: Option<Str>) -> CommandFuture<'_>;
	/// Toggle or inspect the current model's catalog-backed extended context
	/// selection.
	fn extended_context(&mut self, action: Str) -> CommandFuture<'_> {
		let _ = action;
		Box::pin(async { Err(miette::miette!("extended context control is unavailable")) })
	}
}

/// Configuration/credential-scoped command capabilities.
pub trait ConfigCommandHost {
	/// Open settings.
	fn settings(&mut self) -> CommandFuture<'_>;
	/// Open provider setup.
	fn setup(&mut self, section: Option<Str>) -> CommandFuture<'_>;
	/// Show configured providers.
	fn providers(&mut self) -> CommandFuture<'_>;
	/// Start a guarded provider login.
	fn login(&mut self, provider: Option<Str>) -> CommandFuture<'_>;
	/// Remove provider authorization.
	fn logout(&mut self, provider: Option<Str>) -> CommandFuture<'_>;
}

/// Context and execution-flow command capabilities.
/// Writable MCP configuration scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigScope {
	/// User profile configuration.
	User,
	/// Current project configuration.
	Project,
}

/// Parsed `/mcp` operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpRequest {
	/// List effective servers and lifecycle state.
	List,
	/// Add one validated server declaration.
	Add {
		/// Writable target scope.
		scope:       ConfigScope,
		/// Server identity.
		name:        Str,
		/// Exact JSON declaration.
		server_json: Str,
	},
	/// Remove one server.
	Remove(Str),
	/// Enable one server.
	Enable(Str),
	/// Disable one server.
	Disable(Str),
	/// Test one live server.
	Test(Str),
	/// Restart one live server at its current definition epoch.
	Reconnect(Str),
	/// Replace the managed OAuth credential for one server.
	Reauth(Str),
	/// Remove the managed credential for one server.
	Unauth(Str),
	/// Render native MCP help.
	Help,
}

/// Parsed live collaboration operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollabRequest {
	/// Show collaboration state and participants.
	Status,
	/// Show the current link and participant roster.
	View,
	/// Start hosting with optional relay and web URLs.
	Start(ParsedFlags),
	/// Stop hosting the current collaboration.
	Stop,
}

/// Parsed transcript export operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExportRequest {
	/// Write a styled HTML transcript.
	Html(Option<Str>),
	/// Copy a transcript dump, optionally with request JSON sidecars.
	Dump { requests: bool },
	/// Copy a transcript selection, code block, or last command.
	Copy(Str),
}

/// Parsed extension marketplace or plugin operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionRequest {
	/// Open the retained extension inspector.
	Inspect,
	/// Operate on signed extension indexes and installations.
	Marketplace(MarketplaceRequest),
	/// List or change installed extension enablement.
	Plugins(PluginRequest),
	/// Rediscover and atomically replace the active extension generation.
	Reload,
}
/// Parsed signed-extension marketplace operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarketplaceRequest {
	/// List configured indexes.
	List,
	/// Add an index source.
	Add(Str),
	/// Remove an index.
	Remove(Str),
	/// Refresh one index or all indexes.
	Update(Option<Str>),
	/// Discover packages, optionally in one index.
	Discover(Option<Str>),
	/// Install one signed package.
	Install { spec: Str, scope: ConfigScope, force: bool },
	/// Uninstall one signed package.
	Uninstall { spec: Str, scope: ConfigScope },
	/// List installed marketplace packages.
	Installed,
	/// Upgrade one package or every installed package.
	Upgrade { spec: Option<Str>, scope: ConfigScope },
	/// Render marketplace help.
	Help,
}

/// Parsed installed-extension operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginRequest {
	/// List effective extensions and shadowing state.
	List,
	/// Enable one extension.
	Enable {
		/// Extension identifier.
		id:    Str,
		/// Configuration scope receiving the enablement.
		scope: ConfigScope,
	},
	/// Disable one extension.
	Disable {
		/// Extension identifier.
		id:    Str,
		/// Configuration scope receiving the disablement.
		scope: ConfigScope,
	},
}
/// Parsed local security workflow operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecurityRequest {
	/// Launch one ordinary restricted reviewer child over the workspace or a
	/// bounded relative path.
	Review(Option<Str>),
}
pub trait FlowCommandHost {
	/// Return the latest complete anchored context snapshot.
	fn context(&mut self) -> CommandFuture<'_>;
	/// Run canonical manual compaction.
	fn compact(&mut self, request: ManualCompactionRequest) -> CommandFuture<'_>;
	/// Reclaim replaceable context.
	fn shake(&mut self, args: Str) -> CommandFuture<'_>;
	/// Query or reset durable usage.
	fn usage(&mut self, args: Str) -> CommandFuture<'_>;
	/// Start or inspect the stats service.
	fn stats(&mut self, flags: ParsedFlags) -> CommandFuture<'_>;
	/// Control planning mode.
	fn plan(&mut self, args: Str) -> CommandFuture<'_>;
	/// Control director/worker mode.
	fn vibe(&mut self, args: Str) -> CommandFuture<'_>;
	/// Inspect or mutate session tasks.
	fn todo(&mut self, args: Str) -> CommandFuture<'_>;
	/// Review the current plan.
	fn plan_review(&mut self, args: Str) -> CommandFuture<'_>;
	/// Start the guided-goal interview.
	fn guided_goal(&mut self, args: Str) -> CommandFuture<'_>;
	/// Configure bounded continuation.
	fn loop_command(&mut self, args: Str) -> CommandFuture<'_>;
	/// Queue work at the next boundary.
	fn queue(&mut self, prompt: Str) -> CommandFuture<'_>;
	/// Force a next-turn tool choice.
	fn force(&mut self, tool: Str) -> CommandFuture<'_>;
	/// Control the fast service tier.
	fn fast(&mut self, args: Str) -> CommandFuture<'_>;
	/// Control dynamic cheap-model prewalk.
	fn prewalk(&mut self, args: Str) -> CommandFuture<'_>;
	/// Run an ephemeral aside.
	fn btw(&mut self, prompt: Str) -> CommandFuture<'_>;
	/// Run a background aside.
	fn tan(&mut self, prompt: Str) -> CommandFuture<'_>;
	/// Generate a durable TTSR rule.
	fn omfg(&mut self, instruction: Str) -> CommandFuture<'_>;
	/// Start or stop realtime voice.
	fn live(&mut self, args: Str) -> CommandFuture<'_>;
	/// Persist browser mode and restart live surfaces so it takes effect.
	fn browser(&mut self, request: BrowserRequest) -> CommandFuture<'_> {
		let _ = request;
		Box::pin(async { Err(miette::miette!("browser mode control is unavailable")) })
	}
	/// Control the synthetic advisor watchdog.
	fn advisor(&mut self, request: AdvisorRequest) -> CommandFuture<'_> {
		let _ = request;
		Box::pin(async { Err(miette::miette!("advisor control is unavailable")) })
	}
	/// Execute a utility, capability-inspection, or device-mode operation.
	fn utility(&mut self, request: UtilityRequest) -> CommandFuture<'_> {
		let _ = request;
		Box::pin(async { Err(miette::miette!("utility command is unavailable")) })
	}
	/// Manage native scoped SSH host declarations.
	fn ssh(&mut self, request: SshRequest) -> CommandFuture<'_> {
		let _ = request;
		Box::pin(async { Err(miette::miette!("SSH host management is unavailable")) })
	}
	/// Manage Environment-owned MCP declarations and lifecycle.
	fn mcp(&mut self, request: McpRequest) -> CommandFuture<'_>;
	/// Inspect or maintain the session's Mnemopi authority.
	fn memory(&mut self, args: Str) -> CommandFuture<'_>;
	/// Host or inspect a live collaboration.
	fn collab(&mut self, request: CollabRequest) -> CommandFuture<'_> {
		let _ = request;
		Box::pin(async { Err(miette::miette!("collaboration is unavailable")) })
	}
	/// Join a live collaboration link.
	fn join_collab(&mut self, link: Str) -> CommandFuture<'_> {
		let _ = link;
		Box::pin(async { Err(miette::miette!("collaboration is unavailable")) })
	}
	/// Leave the active collaboration.
	fn leave_collab(&mut self) -> CommandFuture<'_> {
		Box::pin(async { Err(miette::miette!("collaboration is unavailable")) })
	}
	/// Export or copy transcript material.
	fn export(&mut self, request: ExportRequest) -> CommandFuture<'_> {
		let _ = request;
		Box::pin(async { Err(miette::miette!("transcript export is unavailable")) })
	}
	/// Create an encrypted, redacted share link.
	fn share(&mut self, args: Str) -> CommandFuture<'_> {
		let _ = args;
		Box::pin(async { Err(miette::miette!("sharing is unavailable")) })
	}
	/// Manage native signed extensions.
	fn extensions(&mut self, request: ExtensionRequest) -> CommandFuture<'_> {
		let _ = request;
		Box::pin(async { Err(miette::miette!("extension management is unavailable")) })
	}
	/// Synthesize a bounded CI-remediation turn.
	fn green(&mut self, target: Option<Str>) -> CommandFuture<'_> {
		Box::pin(async move {
			let target = target.as_deref().unwrap_or("the current branch");
			Ok(CommandResult::Prompt(PromptResult {
				text:       Str::from(format!(
					"Make {target} green. Gather branch status and failing CI evidence only through \
					 the Environment git authority and direct GitHub resources; do not invoke git, gh, \
					 or a shell. Ignore successful jobs. Diagnose the smallest root-cause fix, \
					 implement it, and run only the checks covering the failure. Keep the evidence set \
					 bounded and report the final verification."
				)),
				provenance: CommandProvenance::builtin(),
			}))
		})
	}
	/// Synthesize a bounded native git/GitHub review turn.
	fn review(&mut self, target: Option<Str>) -> CommandFuture<'_> {
		Box::pin(async move {
			let target = target.as_deref().unwrap_or("uncommitted work");
			Ok(CommandResult::Prompt(PromptResult {
				text:       Str::from(format!(
					"Review {target}. Resolve git evidence only through the Environment VCS authority \
					 and resolve pull requests through pr:// resources; do not invoke git, gh, jj, or \
					 a shell. Exclude lockfiles, generated or minified files, and binary changes. Size \
					 reviewer fan-out to the remaining files. Report only actionable correctness, \
					 security, and maintainability findings with file and line evidence, ranked by \
					 severity; say explicitly when none remain."
				)),
				provenance: CommandProvenance::builtin(),
			}))
		})
	}
	/// Execute one local-only immutable security workflow operation.
	fn security(&mut self, _request: SecurityRequest) -> CommandFuture<'_> {
		let SecurityRequest::Review(path) = _request;
		Box::pin(async move {
			let target = path.as_deref().unwrap_or(".");
			Ok(CommandResult::Prompt(PromptResult {
				text:       Str::from(format!(
					"Launch exactly one ordinary local child agent to review `{target}`. Use the `{}` \
					 profile, its strict result JSON schema with strict schema mode, LSP enabled, and \
					 no isolated worktree. Do not grant exec, mutation, network, web, MCP, extension, \
					 raw-environment, or credential access. Return the child `agent://` handle and \
					 `details.artifact` reference. Reuse ordinary child job status, cancellation, \
					 journal, and private artifact spill; do not create a scan coordinator, database, \
					 SARIF, comparison, validation workflow, bundle, cloud client, or security:// \
					 resource.",
					omp_driver::security_review::profile::PROFILE_ID
				)),
				provenance: CommandProvenance::builtin(),
			}))
		})
	}
	/// Run the project diagnostic cleanse workflow.
	fn cleanse(&mut self, args: omp_driver::cleanse::CleanseArgs) -> CommandFuture<'_> {
		let _ = args;
		Box::pin(async { Err(miette::miette!("project cleansing is unavailable")) })
	}
}

/// Complete command host assembled from capability-scoped interfaces.
pub trait CommandHost:
	ShellCommandHost
	+ SessionCommandHost
	+ ModelCommandHost
	+ ConfigCommandHost
	+ FlowCommandHost
	+ Send
{
}

impl<T> CommandHost for T where
	T: ShellCommandHost
		+ SessionCommandHost
		+ ModelCommandHost
		+ ConfigCommandHost
		+ FlowCommandHost
		+ Send
{
}

pub(super) fn declaration(
	order: u16,
	name: &'static str,
	icon: omp_tui::Icon,
	aliases: &'static [&'static str],
	description: &'static str,
	argument_hint: &'static str,
	candidates: &'static [(&'static str, &'static str)],
	capabilities: &'static [CommandCapability],
	guest_visible: bool,
	handler: CommandHandler,
) -> CommandDeclaration {
	CommandDeclaration {
		order,
		name: sf!(name),
		icon,
		aliases: aliases.iter().copied().map(Str::new).collect(),
		description: sf!(description),
		argument_hint: (!argument_hint.is_empty()).then(|| sf!(argument_hint)),
		hints: candidates
			.iter()
			.copied()
			.map(|(value, description)| ArgumentHint {
				value: Str::new_static(value),
				description: Str::new_static(description),
			})
			.collect(),
		capabilities: Arc::from(capabilities),
		surfaces: Arc::from([CommandSurface::Tui, CommandSurface::Acp, CommandSurface::Text]),
		guest_visible,
		acp_description: None,
		provenance: CommandProvenance::builtin(),
		implementation: CommandImplementation::Handler(handler),
	}
}

pub(super) fn parse_none(args: &str, usage: &'static str) -> miette::Result<()> {
	if args.is_empty() {
		Ok(())
	} else {
		Err(miette::miette!("usage: {usage}"))
	}
}

pub(super) fn parse_required(args: &str, usage: &'static str) -> miette::Result<Str> {
	if args.is_empty() {
		Err(miette::miette!("usage: {usage}"))
	} else {
		Ok(Str::new(args))
	}
}

pub(super) fn parse_optional(args: &str) -> miette::Result<Option<Str>> {
	Ok((!args.is_empty()).then(|| Str::new(args)))
}

pub(super) fn parse_raw(args: &str) -> miette::Result<Str> {
	Ok(Str::new(args))
}

pub(super) fn parse_selector(args: &str) -> miette::Result<Option<Str>> {
	let selector = parse_optional(args)?;
	if selector
		.as_ref()
		.is_some_and(|selector| selector.starts_with('@'))
	{
		Err(miette::miette!("foreign session selectors are not supported"))
	} else {
		Ok(selector)
	}
}

pub(super) fn parse_flags(args: &str) -> miette::Result<ParsedFlags> {
	let mut parsed = Vec::new();
	let mut words = args.split_whitespace().peekable();
	while let Some(flag) = words.next() {
		if !flag.starts_with("--") {
			return Err(miette::miette!("expected a --flag, found `{flag}`"));
		}
		let value = if words.peek().is_some_and(|next| !next.starts_with("--")) {
			Some(Str::new(words.next().expect("peeked flag value remains available")))
		} else {
			None
		};
		parsed.push((Str::new(flag), value));
	}
	Ok(ParsedFlags(parsed))
}

macro_rules! command_icon {
	() => {
		omp_tui::Icon::SlashCommand
	};
	($icon:ident) => {
		omp_tui::Icon::$icon
	};
}

macro_rules! command_common {
	($module:ident, $order:literal, $name:literal, $(icon: $icon:ident,)? [$($alias:literal),* $(,)?], $description:literal,
	 $hint:literal, [$(($candidate:literal, $candidate_description:literal)),* $(,)?], [$($capability:ident),* $(,)?], $guest:literal,
		$parse:expr, |$host:ident, $args:ident| $body:expr) => {
		mod $module {
			#[allow(unused_imports, reason = "commands reference file-scope parsers and types")]
			use super::{super::*, *};

			fn handle<'a>(
				$host: &'a mut dyn $crate::chat_ui::commands::CommandHost,
				raw: &'a str,
				_: &'a $crate::chat_ui::commands::CommandProvenance,
			) -> $crate::chat_ui::commands::CommandFuture<'a> {
				Box::pin(async move {
					let $args = ($parse)(raw)?;
					$body.await
				})
			}
			fn build() -> $crate::chat_ui::commands::CommandDeclaration {
				$crate::chat_ui::commands::declaration(
					$order,
					$name,
					$crate::chat_ui::commands::command_icon!($($icon)?),
					&[$($alias),*],
					$description,
					$hint,
					&[$(($candidate, $candidate_description)),*],
					&[$($crate::chat_ui::commands::CommandCapability::$capability),*],
					$guest,
					handle,
				)
			}
			inventory::submit! {
				$crate::chat_ui::commands::registry::BuiltinRegistration { declaration: build }
			}
		}
	};
}

macro_rules! command {
	($module:ident, $order:literal, $name:literal, $(icon: $icon:ident,)? [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, none => |$host:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, $(icon: $icon,)? [$($alias),*], $description, "", [],
			[$($capability),*], $guest,
			|raw| $crate::chat_ui::commands::parse_none(raw, concat!("/", $name)),
			|$host, _args| $body);
	};
	($module:ident, $order:literal, $name:literal, $(icon: $icon:ident,)? [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, required($hint:literal) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, $(icon: $icon,)? [$($alias),*], $description, $hint, [],
			[$($capability),*], $guest,
			|raw| $crate::chat_ui::commands::parse_required(
				raw, concat!("/", $name, " ", $hint)
			),
			|$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, $(icon: $icon:ident,)? [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, optional($hint:literal) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, $(icon: $icon,)? [$($alias),*], $description, $hint, [],
			[$($capability),*], $guest,
			$crate::chat_ui::commands::parse_optional, |$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, $(icon: $icon:ident,)? [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, selector($hint:literal) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, $(icon: $icon,)? [$($alias),*], $description, $hint, [],
			[$($capability),*], $guest, $crate::chat_ui::commands::parse_selector,
			|$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, $(icon: $icon:ident,)? [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, raw($hint:literal, [$($candidate:literal),* $(,)?]) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, $(icon: $icon,)? [$($alias),*], $description, $hint,
			[$(($candidate, "")),*], [$($capability),*], $guest,
			$crate::chat_ui::commands::parse_raw, |$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, $(icon: $icon:ident,)? [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, flags($hint:literal, [$($candidate:literal),* $(,)?]) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, $(icon: $icon,)? [$($alias),*], $description, $hint,
			[$(($candidate, "")),*], [$($capability),*], $guest,
			$crate::chat_ui::commands::parse_flags, |$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, $(icon: $icon:ident,)? [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, typed($hint:literal, [$(($candidate:literal, $candidate_description:literal)),* $(,)?], $parse:path) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, $(icon: $icon,)? [$($alias),*], $description, $hint,
			[$(($candidate, $candidate_description)),*], [$($capability),*], $guest, $parse, |$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, $(icon: $icon:ident,)? [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, typed($hint:literal, [$($candidate:literal),* $(,)?], $parse:path) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, $(icon: $icon,)? [$($alias),*], $description, $hint,
			[$(($candidate, "")),*], [$($capability),*], $guest, $parse, |$host, $arg| $body);
	};
}

pub(super) use command;
pub(crate) use command_common;
pub(crate) use command_icon;
