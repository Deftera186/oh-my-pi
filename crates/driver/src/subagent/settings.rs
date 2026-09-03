//! Typed settings owned and consumed by the subagent runtime.

use std::collections::BTreeMap;

use omp_con::{CfgLoader, Ctx, Kv, Value};
use omp_core::Str;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

/// Prompt pressure applied to task delegation.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
	strum::VariantNames,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum TaskEagerMode {
	/// Model chooses when delegation helps.
	#[default]
	Default,
	/// Prompt recommends delegation when work decomposes.
	Preferred,
	/// First-turn guidance requires delegation.
	Always,
}

/// Maximum caller-selectable reasoning effort for a subagent.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
	strum::VariantNames,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum TaskEffortCeiling {
	/// Minimal reasoning.
	Minimal,
	/// Low reasoning.
	Low,
	/// Medium reasoning.
	Medium,
	/// High reasoning.
	High,
	/// Extra-high reasoning.
	Xhigh,
	/// Preserve the model's maximum available reasoning.
	#[default]
	Max,
}

/// Environment-owned isolation backend selected for child workspaces.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
	strum::VariantNames,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum TaskIsolationMode {
	/// Run in the parent workspace.
	#[default]
	None,
	/// Let Environment select the best native backend.
	Auto,
	/// APFS clonefile isolation.
	Apfs,
	/// Btrfs subvolume isolation.
	Btrfs,
	/// ZFS clone isolation.
	Zfs,
	/// Native reflink isolation.
	Reflink,
	/// Linux overlay filesystem isolation.
	Overlayfs,
	/// Windows projected filesystem isolation.
	Projfs,
	/// Windows block-clone isolation.
	BlockClone,
	/// Git worktree or recursive-copy fallback.
	Rcopy,
}

/// Merge strategy for a successful isolated child workspace.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
	strum::VariantNames,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum TaskIsolationMerge {
	/// Apply a content-addressed patch.
	#[default]
	Patch,
	/// Merge a retained branch.
	Branch,
}

omp_con::con_enum!(TaskEagerMode);
omp_con::con_enum!(TaskEffortCeiling);
omp_con::con_enum!(TaskIsolationMode);
omp_con::con_enum!(TaskIsolationMerge);

omp_con::var! {
	/// Maximum recursive subagent depth; -1 is unlimited.
	pub static SV_TASK_MAX_RECURSION_DEPTH = sv_task_max_recursion_depth: i32 {
		default: 2,
		min: -1,
		flags: archive | inherit,
	};
	/// Maximum active subagent runs; 0 is unlimited.
	pub static SV_TASK_MAX_CONCURRENCY = sv_task_max_concurrency: u32 {
		default: 32,
		flags: archive | inherit,
	};
	/// Per-run wall-clock limit.
	pub static SV_TASK_MAX_RUNTIME = sv_task_max_runtime: omp_con::Span {
		default: omp_con::Span::Never,
		flags: archive | inherit,
	};
	/// Assistant-request budget before bounded wrap-up; 0 disables it.
	pub static SV_TASK_SOFT_REQUEST_BUDGET = sv_task_soft_request_budget: u32 {
		default: 200,
		flags: archive | inherit,
	};
	/// Emits one wrap-up notice after crossing the soft request budget.
	pub static SV_TASK_SOFT_REQUEST_BUDGET_NOTICE = sv_task_soft_request_budget_notice: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Maximum caller-selectable reasoning effort for a child.
	pub static SV_TASK_MAX_EFFORT = sv_task_max_effort: TaskEffortCeiling {
		default: TaskEffortCeiling::Max,
		flags: archive | inherit,
	};
	/// Prompt pressure applied to task delegation.
	pub static SV_TASK_EAGER = sv_task_eager: TaskEagerMode {
		default: TaskEagerMode::Default,
		flags: archive | inherit,
	};
	/// Grants LSP capability to children.
	pub static SV_TASK_ENABLE_LSP = sv_task_enable_lsp: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Idle interval before a child loop is parked.
	pub static SV_TASK_AGENT_IDLE_TTL = sv_task_agent_idle_ttl: omp_con::Span {
		default: omp_con::Span::Finite(omp_core::Duration::new(
			420,
			omp_core::DurationUnit::Seconds,
		)),
		flags: archive | inherit,
	};
	/// Agent definitions excluded from spawn resolution.
	pub static SV_TASK_DISABLED_AGENTS = sv_task_disabled_agents: Vec<Str> {
		default: Vec::new(),
		flags: archive | inherit,
	};
	/// Definition-specific model-role overrides.
	pub static SV_TASK_AGENT_MODEL_OVERRIDES = sv_task_agent_model_overrides: Kv {
		default: Kv::default(),
		flags: archive | inherit,
	};
	/// Definition-specific prewalk-role overrides.
	pub static SV_TASK_AGENT_PREWALK = sv_task_agent_prewalk: Kv {
		default: Kv::default(),
		flags: archive | inherit,
	};
	/// Definition-specific advisor-role overrides.
	pub static SV_TASK_AGENT_ADVISOR = sv_task_agent_advisor: Kv {
		default: Kv::default(),
		flags: archive | inherit,
	};
	/// Environment backend used for isolated child workspaces.
	pub static SV_TASK_ISOLATION_MODE = sv_task_isolation_mode: TaskIsolationMode {
		default: TaskIsolationMode::None,
		flags: archive | inherit,
	};
	/// Applies successful isolated workspace changes.
	pub static SV_TASK_ISOLATION_APPLY = sv_task_isolation_apply: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Merge strategy for successful isolated workspace changes.
	pub static SV_TASK_ISOLATION_MERGE = sv_task_isolation_merge: TaskIsolationMerge {
		default: TaskIsolationMerge::Patch,
		flags: archive | inherit,
	};
	/// Shows the selected agent-definition badge in task output.
	pub static CL_TASK_SHOW_AGENT_BADGE = cl_task_show_agent_badge: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Shows the serving model in task output.
	pub static CL_TASK_SHOW_RESOLVED_MODEL_BADGE = cl_task_show_resolved_model_badge: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Default peer-message wait interval.
	pub static SV_IRC_TIMEOUT = sv_irc_timeout: omp_con::Span {
		default: omp_con::Span::Finite(omp_core::Duration::new(
			120,
			omp_core::DurationUnit::Seconds,
		)),
		flags: archive | inherit,
	};
	/// Relays peer-to-peer messages to the main transcript.
	pub static CL_IRC_RELAY_TO_MAIN = cl_irc_relay_to_main: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Shows delivery-state badges beside relayed peer messages.
	pub static CL_IRC_SHOW_BADGES = cl_irc_show_badges: bool {
		default: true,
		flags: archive | inherit,
	};
}

/// Child workspace isolation defaults.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TaskIsolationSettings {
	/// Backend selection.
	pub mode:  TaskIsolationMode,
	/// Apply successful changes automatically.
	pub apply: bool,
	/// Merge strategy.
	pub merge: TaskIsolationMerge,
}

impl Default for TaskIsolationSettings {
	fn default() -> Self {
		Self { mode: TaskIsolationMode::None, apply: true, merge: TaskIsolationMerge::Patch }
	}
}

/// Complete typed projection consumed by subagent admission and new spawns.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TaskSettings {
	/// Maximum recursive child depth; `-1` is unlimited.
	pub max_recursion_depth: i16,
	/// Active child run ceiling; `0` is unlimited.
	pub max_concurrency: usize,
	/// Per-run wall-clock cap in milliseconds; `0` disables it.
	pub max_runtime_ms: u64,
	/// Soft assistant-request budget; `0` disables it.
	pub soft_request_budget: u32,
	/// Emit one wrap-up notice when the soft budget is crossed.
	pub soft_request_budget_notice: bool,
	/// Maximum caller-selectable reasoning effort.
	pub max_effort: TaskEffortCeiling,
	/// Delegation prompt pressure.
	pub eager: TaskEagerMode,
	/// Explicitly grant LSP to children; false by default.
	pub enable_lsp: bool,
	/// Idle live-loop TTL before parking; `0` keeps loops loaded.
	pub agent_idle_ttl_ms: u64,
	/// Agent definitions excluded from spawn resolution.
	pub disabled_agents: Vec<Str>,
	/// Case-insensitive definition-to-model override map.
	pub agent_model_overrides: BTreeMap<Str, Str>,
	/// Definition-specific prewalk role overrides.
	pub agent_prewalk: BTreeMap<Str, Str>,
	/// Definition-specific advisor role overrides.
	pub agent_advisor: BTreeMap<Str, Str>,
	/// Child workspace isolation defaults.
	pub isolation: TaskIsolationSettings,
	/// Show the selected agent-definition badge in task output.
	pub show_agent_badge: bool,
	/// Show the serving model rather than only the requested role.
	pub show_resolved_model_badge: bool,
}

impl Default for TaskSettings {
	fn default() -> Self {
		Self {
			max_recursion_depth: 2,
			max_concurrency: 32,
			max_runtime_ms: 0,
			soft_request_budget: 200,
			soft_request_budget_notice: true,
			max_effort: TaskEffortCeiling::Max,
			eager: TaskEagerMode::Default,
			enable_lsp: false,
			agent_idle_ttl_ms: 420_000,
			disabled_agents: Vec::new(),
			agent_model_overrides: BTreeMap::new(),
			agent_prewalk: BTreeMap::new(),
			agent_advisor: BTreeMap::new(),
			isolation: TaskIsolationSettings::default(),
			show_agent_badge: true,
			show_resolved_model_badge: false,
		}
	}
}

impl TaskSettings {
	/// Resolves the effective subagent policy from the process console context.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self {
			max_recursion_depth: i16::try_from(SV_TASK_MAX_RECURSION_DEPTH.get(ctx))
				.unwrap_or(i16::MAX),
			max_concurrency: usize::try_from(SV_TASK_MAX_CONCURRENCY.get(ctx)).unwrap_or(usize::MAX),
			max_runtime_ms: span_millis(SV_TASK_MAX_RUNTIME.get(ctx)),
			soft_request_budget: SV_TASK_SOFT_REQUEST_BUDGET.get(ctx),
			soft_request_budget_notice: SV_TASK_SOFT_REQUEST_BUDGET_NOTICE.get(ctx),
			max_effort: SV_TASK_MAX_EFFORT.get(ctx),
			eager: SV_TASK_EAGER.get(ctx),
			enable_lsp: SV_TASK_ENABLE_LSP.get(ctx),
			agent_idle_ttl_ms: span_millis(SV_TASK_AGENT_IDLE_TTL.get(ctx)),
			disabled_agents: SV_TASK_DISABLED_AGENTS.get(ctx),
			agent_model_overrides: string_map(SV_TASK_AGENT_MODEL_OVERRIDES.get(ctx)),
			agent_prewalk: string_map(SV_TASK_AGENT_PREWALK.get(ctx)),
			agent_advisor: string_map(SV_TASK_AGENT_ADVISOR.get(ctx)),
			isolation: TaskIsolationSettings {
				mode:  SV_TASK_ISOLATION_MODE.get(ctx),
				apply: SV_TASK_ISOLATION_APPLY.get(ctx),
				merge: SV_TASK_ISOLATION_MERGE.get(ctx),
			},
			show_agent_badge: CL_TASK_SHOW_AGENT_BADGE.get(ctx),
			show_resolved_model_badge: CL_TASK_SHOW_RESOLVED_MODEL_BADGE.get(ctx),
		}
	}
}

fn span_millis(span: omp_con::Span) -> u64 {
	span
		.to_std()
		.map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
		.unwrap_or(0)
}

fn string_map(values: Kv) -> BTreeMap<Str, Str> {
	values
		.0
		.into_iter()
		.filter_map(|(name, value)| match value {
			Value::Str(value) | Value::Enum(value) => Some((name, value)),
			_ => None,
		})
		.collect()
}

/// Creates a child console context from the parent's effective inherited
/// values, then executes `config.cfg`, `subagent.cfg`, and `<agent>.cfg`.
pub fn child_ctx(
	parent: &Ctx,
	loader: &dyn CfgLoader,
	agent: &str,
) -> Result<Ctx, omp_con::ConError> {
	let seed = parent.seed_child();
	let child = Ctx::new();
	for (name, value) in seed.iter() {
		child.set_value(name.as_str(), value.clone(), omp_con::SetSource::Code)?;
	}
	let outcome = child.exec_configs(loader, Some(agent));
	if outcome.failed > 0 {
		tracing::warn!(
			agent,
			failed = outcome.failed,
			"child cfg contained statements this build does not understand; they were skipped"
		);
	}
	Ok(child)
}
