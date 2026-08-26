//! Owned SDK session configuration.

use std::{path::PathBuf, time::Duration};

use omp_catalog::ModelRole;
use omp_core::Str;

/// Maximum reasoning effort accepted from model selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ThinkingCeiling {
	/// Disable explicit reasoning effort.
	Off,
	/// Minimal effort.
	Minimal,
	/// Low effort.
	Low,
	/// Medium effort.
	Medium,
	/// High effort.
	High,
	/// Extra-high effort.
	ExtraHigh,
	/// Provider maximum.
	Max,
}

/// Static discovery inputs selected for one session.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryPolicy {
	/// Native extension manifests or roots.
	pub extension_paths:  Box<[PathBuf]>,
	/// Installed plugin roots.
	pub plugin_paths:     Box<[PathBuf]>,
	/// Explicit skill roots.
	pub skill_paths:      Box<[PathBuf]>,
	/// Plugin agent-definition roots.
	pub agent_paths:      Box<[PathBuf]>,
	/// Explicit persistent context files.
	pub context_paths:    Box<[PathBuf]>,
	/// Rule roots distinct from general context.
	pub rule_paths:       Box<[PathBuf]>,
	/// Prompt-template roots.
	pub template_paths:   Box<[PathBuf]>,
	/// Slash-command roots.
	pub command_paths:    Box<[PathBuf]>,
	/// MCP declaration roots.
	pub mcp_paths:        Box<[PathBuf]>,
	/// Hook roots.
	pub hook_paths:       Box<[PathBuf]>,
	/// Tool module roots.
	pub tool_paths:       Box<[PathBuf]>,
	/// LSP configuration roots.
	pub lsp_paths:        Box<[PathBuf]>,
	/// DAP configuration roots.
	pub dap_paths:        Box<[PathBuf]>,
	/// JavaScript extension-module roots.
	pub javascript_paths: Box<[PathBuf]>,
}

/// Session policy restrictions applied before the first turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPolicies {
	/// Optional exact active-tool family list.
	pub active_tools:       Option<Box<[Str]>>,
	/// Whether child-agent spawning is admitted.
	pub allow_spawns:       bool,
	/// Maximum descendant depth, including this session's depth.
	pub max_depth:          u16,
	/// Require prewalk reasoning before mutations.
	pub prewalk:            bool,
	/// Permit plan-mode mutation after explicit execution.
	pub plan_yolo:          bool,
	/// Apply restricted-child tool inheritance.
	pub restricted:         bool,
	/// Whether host interactive prompts are available.
	pub interactive_prompt: bool,
}

impl Default for SessionPolicies {
	fn default() -> Self {
		Self {
			active_tools:       None,
			allow_spawns:       true,
			max_depth:          u16::MAX,
			prewalk:            false,
			plan_yolo:          false,
			restricted:         false,
			interactive_prompt: false,
		}
	}
}

/// Optional runtime subsystems composed into one session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SubsystemToggles {
	/// Persistent Python eval.
	pub eval:           bool,
	/// MCP devices.
	pub mcp:            bool,
	/// Language-server intelligence.
	pub lsp:            bool,
	/// IRC collaboration.
	pub irc:            bool,
	/// Default-off Mnemopi state.
	pub mnemopi:        bool,
	/// Workspace tree prompt projection.
	pub workspace_tree: bool,
}

/// Stable agent identity and lineage facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentIdentity {
	/// Stable agent id. The builder generates one when omitted.
	pub id:           Option<Str>,
	/// Caller-specified display name.
	pub display_name: Option<Str>,
	/// Stable parent id for child sessions.
	pub parent_id:    Option<Str>,
	/// Current descendant depth.
	pub depth:        u16,
}

impl Default for AgentIdentity {
	fn default() -> Self {
		Self { id: None, display_name: None, parent_id: None, depth: 0 }
	}
}

/// Complete owned configuration for one embedded session.
#[derive(Clone, Debug)]
pub struct SessionOptions {
	/// Primary working root.
	pub cwd: PathBuf,
	/// Additional Environment-authorized roots, in stable grant order.
	pub additional_roots: Box<[PathBuf]>,
	/// Optional agent configuration root.
	pub agent_root: Option<PathBuf>,
	/// Static content discovery policy.
	pub discovery: DiscoveryPolicy,
	/// Ordered model selectors or roles.
	pub model_selectors: Box<[Str]>,
	/// Configured role definitions.
	pub model_roles: Box<[ModelRole]>,
	/// Path-scoped enabled-model patterns.
	pub enabled_models: Box<[Str]>,
	/// Maximum accepted thinking effort.
	pub thinking_ceiling: ThinkingCeiling,
	/// Optional semantic service tier.
	pub service_tier: Option<Str>,
	/// Optional complete-turn deadline.
	pub turn_deadline: Option<Duration>,
	/// Optional tool-call deadline.
	pub tool_deadline: Option<Duration>,
	/// Spawn, tool, and interaction policy.
	pub policies: SessionPolicies,
	/// Optional subsystem composition.
	pub subsystems: SubsystemToggles,
	/// Agent identity and lineage.
	pub identity: AgentIdentity,
	/// Optional output JSON Schema.
	pub output_schema: Option<serde_json::Value>,
	/// Strict output validation rather than bounded permissive salvage.
	pub strict_output_schema: bool,
	/// Defer usage-reserve confirmation until the host has installed its
	/// interactive authority.
	pub defer_usage_confirmation: bool,
	/// Guarded session id requested for cold revival.
	pub revive_session_id: Option<Str>,
	/// Expected journal revision for guarded revival.
	pub expected_revision: Option<u64>,
}

impl SessionOptions {
	/// Creates options for one required primary root.
	pub fn new(cwd: impl Into<PathBuf>) -> Self {
		Self {
			cwd: cwd.into(),
			additional_roots: Box::new([]),
			agent_root: None,
			discovery: DiscoveryPolicy::default(),
			model_selectors: Box::new([]),
			model_roles: Box::new([]),
			enabled_models: Box::new([]),
			thinking_ceiling: ThinkingCeiling::Max,
			service_tier: None,
			turn_deadline: None,
			tool_deadline: None,
			policies: SessionPolicies::default(),
			subsystems: SubsystemToggles::default(),
			identity: AgentIdentity::default(),
			output_schema: None,
			strict_output_schema: false,
			defer_usage_confirmation: false,
			revive_session_id: None,
			expected_revision: None,
		}
	}
}
