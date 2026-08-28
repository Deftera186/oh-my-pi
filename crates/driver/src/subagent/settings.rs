//! Typed settings owned and consumed by the subagent runtime.

use std::{collections::BTreeMap, sync::Arc};

use omp_agent::AgentTree;
use omp_core::Str;
use omp_settings::{
	FieldDescriptor, OptionProvider, SettingKind, SettingOption, SettingScope, SettingsDomain,
	ValidationError,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const EAGER_VALUES: &[&str] = &["default", "preferred", "always"];
const EAGER_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "default",
		label:       "Default",
		description: Some("Model decides when to delegate"),
	},
	SettingOption {
		value:       "preferred",
		label:       "Preferred",
		description: Some("Add delegation guidance"),
	},
	SettingOption {
		value:       "always",
		label:       "Always",
		description: Some("Require first-turn delegation"),
	},
];
const EFFORT_VALUES: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];
const ISOLATION_VALUES: &[&str] = &[
	"none",
	"auto",
	"apfs",
	"btrfs",
	"zfs",
	"reflink",
	"overlayfs",
	"projfs",
	"block-clone",
	"rcopy",
];
const MERGE_VALUES: &[&str] = &["patch", "branch"];

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

/// Normalizes legacy boolean per-agent role overrides within one settings
/// layer before that layer participates in precedence merging.
pub(crate) fn normalize_persisted_agent_overrides(document: &mut toml::Table) {
	let Some(task) = document.get_mut("task").and_then(toml::Value::as_table_mut) else {
		return;
	};
	for key in ["agentPrewalk", "agentAdvisor"] {
		let Some(overrides) = task.get_mut(key).and_then(toml::Value::as_table_mut) else {
			continue;
		};
		for (_, value) in overrides.iter_mut() {
			if let toml::Value::Boolean(enabled) = value {
				*value = toml::Value::String(if *enabled { "on" } else { "off" }.to_owned());
			}
		}
	}
}

impl SettingsDomain for TaskSettings {
	const DOMAIN: &'static str = "task";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"task.maxRecursionDepth",
			"Max Task Recursion",
			"Maximum recursive subagent depth; -1 is unlimited.",
			SettingKind::Integer,
			10,
		),
		field(
			"task.maxConcurrency",
			"Max Concurrent Tasks",
			"Maximum active subagent runs; 0 is unlimited.",
			SettingKind::Integer,
			20,
		),
		field(
			"task.maxRuntimeMs",
			"Max Subagent Runtime",
			"Per-run wall-clock cap in milliseconds; 0 disables it.",
			SettingKind::Integer,
			30,
		),
		field(
			"task.softRequestBudget",
			"Soft Request Budget",
			"Assistant-request budget before bounded wrap-up.",
			SettingKind::Integer,
			40,
		),
		field(
			"task.softRequestBudgetNotice",
			"Soft Budget Notice",
			"Ask a child to wrap up after crossing its soft budget.",
			SettingKind::Boolean,
			50,
		),
		FieldDescriptor {
			path:        "task.maxEffort",
			label:       "Maximum Per-Spawn Effort",
			description: "Clamp explicit and inherited child effort to this ceiling.",
			kind:        SettingKind::Enum(EFFORT_VALUES),
			scopes:      PERSISTED,
			order:       60,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "task.eager",
			label:       "Prefer Task Delegation",
			description: "How strongly prompts encourage task delegation.",
			kind:        SettingKind::Enum(EAGER_VALUES),
			scopes:      PERSISTED,
			order:       70,
			options:     Some(OptionProvider::Static(EAGER_OPTIONS)),
			condition:   None,
			secret:      false,
		},
		field(
			"task.enableLsp",
			"LSP in Subagents",
			"Explicitly grant LSP capability to new child spawns.",
			SettingKind::Boolean,
			80,
		),
		field(
			"task.agentIdleTtlMs",
			"Agent Idle TTL",
			"Milliseconds before an idle loop is parked; 0 keeps it loaded.",
			SettingKind::Integer,
			90,
		),
		field(
			"task.disabledAgents",
			"Disabled Agents",
			"Agent definitions excluded from spawn resolution.",
			SettingKind::Array,
			100,
		),
		field(
			"task.agentModelOverrides",
			"Agent Model Overrides",
			"Definition-specific model-role overrides.",
			SettingKind::Table,
			110,
		),
		field(
			"task.agentPrewalk",
			"Agent Prewalk Overrides",
			"Definition-specific prewalk-role overrides.",
			SettingKind::Table,
			120,
		),
		field(
			"task.agentAdvisor",
			"Agent Advisor Overrides",
			"Definition-specific advisor-role overrides.",
			SettingKind::Table,
			130,
		),
		FieldDescriptor {
			path:        "task.isolation.mode",
			label:       "Isolation Mode",
			description: "Environment backend used for isolated child workspaces.",
			kind:        SettingKind::Enum(ISOLATION_VALUES),
			scopes:      PERSISTED,
			order:       140,
			options:     None,
			condition:   None,
			secret:      false,
		},
		field(
			"task.isolation.apply",
			"Apply Isolated Changes",
			"Apply successful isolated workspace changes.",
			SettingKind::Boolean,
			150,
		),
		FieldDescriptor {
			path:        "task.isolation.merge",
			label:       "Isolation Merge Strategy",
			description: "Apply a patch or merge a retained branch.",
			kind:        SettingKind::Enum(MERGE_VALUES),
			scopes:      PERSISTED,
			order:       160,
			options:     None,
			condition:   None,
			secret:      false,
		},
		field(
			"task.showAgentBadge",
			"Show Agent Badge",
			"Show the selected definition in task output.",
			SettingKind::Boolean,
			170,
		),
		field(
			"task.showResolvedModelBadge",
			"Show Resolved Model Badge",
			"Show the actual serving model in task output.",
			SettingKind::Boolean,
			180,
		),
	];

	fn validate(&self) -> Result<(), ValidationError> {
		if self.max_recursion_depth < -1 {
			return Err(ValidationError::DomainInvariant { domain: Self::DOMAIN });
		}
		Ok(())
	}
}

const fn field(
	path: &'static str,
	label: &'static str,
	description: &'static str,
	kind: SettingKind,
	order: u16,
) -> FieldDescriptor {
	FieldDescriptor {
		path,
		label,
		description,
		kind,
		scopes: PERSISTED,
		order,
		options: None,
		condition: None,
		secret: false,
	}
}

/// Typed IRC wait and visibility settings owned by subagent supervision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct IrcSettings {
	/// Default message-wait timeout in milliseconds; `0` disables it.
	pub timeout_ms:       u64,
	/// Relay peer-to-peer bodies once to the main transcript.
	pub relay_to_main_ui: bool,
	/// Show delivery-state badges beside relayed messages.
	pub show_badges:      bool,
}

impl Default for IrcSettings {
	fn default() -> Self {
		Self { timeout_ms: 120_000, relay_to_main_ui: true, show_badges: true }
	}
}

impl SettingsDomain for IrcSettings {
	const DOMAIN: &'static str = "irc";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"irc.timeoutMs",
			"IRC Timeout",
			"Default wait timeout in milliseconds; 0 disables it.",
			SettingKind::Integer,
			10,
		),
		field(
			"irc.relayToMainUi",
			"Relay Peer Messages",
			"Relay agent-to-agent message bodies once to the main transcript.",
			SettingKind::Boolean,
			20,
		),
		field(
			"irc.showBadges",
			"IRC Delivery Badges",
			"Show delivery-state badges for peer messages.",
			SettingKind::Boolean,
			30,
		),
	];
}

/// Atomically replaceable settings projection read once by each new spawn.
#[derive(Clone)]
pub struct LiveTaskSettings {
	current: Arc<RwLock<Arc<TaskSettings>>>,
	tree:    Arc<AgentTree>,
}

impl LiveTaskSettings {
	/// Installs an initial typed projection and its concurrency ceiling.
	pub fn new(initial: Arc<TaskSettings>, tree: Arc<AgentTree>) -> Self {
		tree.resize_concurrency(initial.max_concurrency);
		Self { current: Arc::new(RwLock::new(initial)), tree }
	}

	/// Returns the immutable projection to capture for one new spawn.
	pub fn snapshot(&self) -> Arc<TaskSettings> {
		Arc::clone(&self.current.read())
	}

	/// Atomically applies a reloaded projection to later spawns and live
	/// admission.
	pub fn apply(&self, settings: Arc<TaskSettings>) {
		self.tree.resize_concurrency(settings.max_concurrency);
		*self.current.write() = settings;
	}
}

#[cfg(test)]
mod tests {
	use omp_settings::{SettingsCatalog, SettingsSnapshot};

	use super::*;

	const CATALOG: SettingsCatalog =
		SettingsCatalog::new(&[&omp_settings::SETTINGS_CONTRIBUTION, &crate::SETTINGS_CONTRIBUTION]);

	#[test]
	fn defaults_match_pi_task_contract() {
		let settings = TaskSettings::default();
		assert_eq!(settings.max_recursion_depth, 2);
		assert_eq!(settings.max_concurrency, 32);
		assert_eq!(settings.max_runtime_ms, 0);
		assert_eq!(settings.soft_request_budget, 200);
		assert!(settings.soft_request_budget_notice);
		assert_eq!(settings.max_effort, TaskEffortCeiling::Max);
		assert!(!settings.enable_lsp);
		assert_eq!(settings.agent_idle_ttl_ms, 420_000);
	}

	#[test]
	fn typed_projection_and_validation_are_linked() {
		let expected = TaskSettings { max_concurrency: 0, ..TaskSettings::default() };
		let snapshot =
			SettingsSnapshot::isolated(expected.clone(), CATALOG).expect("isolated task settings");
		assert_eq!(
			snapshot
				.project::<TaskSettings>()
				.expect("task projection")
				.get(),
			&expected
		);
		assert!(
			TaskSettings { max_recursion_depth: -2, ..TaskSettings::default() }
				.validate()
				.is_err()
		);
	}
	#[test]
	fn legacy_agent_overrides_are_normalized_before_layering() {
		let mut document = toml::Table::from_iter([(
			"task".to_owned(),
			toml::Value::Table(toml::Table::from_iter([(
				"agentPrewalk".to_owned(),
				toml::Value::Table(toml::Table::from_iter([(
					"scout".to_owned(),
					toml::Value::Boolean(true),
				)])),
			)])),
		)]);
		CATALOG.normalize(&mut document);
		assert_eq!(document["task"]["agentPrewalk"]["scout"].as_str(), Some("on"));
	}

	#[tokio::test]
	async fn live_reload_resizes_finite_and_unlimited_admission() {
		let tree = Arc::new(AgentTree::new(2, 1, 4));
		let live = LiveTaskSettings::new(
			Arc::new(TaskSettings { max_concurrency: 1, ..TaskSettings::default() }),
			Arc::clone(&tree),
		);
		let first = tree.admit(1).await.unwrap();
		let waiting_tree = Arc::clone(&tree);
		let waiting = tokio::spawn(async move { waiting_tree.admit(1).await.unwrap() });
		use tokio::task;
		task::yield_now().await;
		assert!(!waiting.is_finished());
		live.apply(Arc::new(TaskSettings {
			max_concurrency: 0,
			enable_lsp: true,
			..TaskSettings::default()
		}));
		let second = waiting.await.unwrap();
		assert_eq!(tree.max_concurrency(), 0);
		assert!(live.snapshot().enable_lsp);
		drop((first, second));
	}
}
