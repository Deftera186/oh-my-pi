//! Persisted application settings and layered extension configuration.

use std::path::{Path, PathBuf};

use omp_agent::{CompactionMethodOrder, CompactionTier};
use omp_core::{Str, sf};
use omp_inference::{Difficulty, DifficultyBackend};
mod domains;
pub use domains::{
	AppearanceSettings, CompletionSettings, CredentialKeySourceSetting, DisplaySettings,
	ErrorNotificationSettings, HyperlinkMode, InteractionSettings, LifecycleSettings,
	MarketplaceUpdateMode, NotifyToggle, RecapSettings, ResizeScrollbackMode, RootDisplaySettings,
	ShareSettings, ShareStore, ShimmerMode, SteeringMode, TitleSettings, TtsrContextMode,
	TtsrInterruptMode, TtsrSettings, TuiSettings, UnexpectedStopMode,
};
pub use omp_memory::config::{AutolearnSettings, MemorySettings, MnemopiSettings};
impl PromptSettings {
	/// Applies CLI overrides supplied through a composition-layer conversion.
	///
	/// The owning application implements the conversion for its CLI argument
	/// type, keeping command-line parsing out of the driver crate.
	pub fn with_cli<'a, T>(self, cli: &'a T) -> Self
	where
		PromptOverrides: From<&'a T>,
	{
		self.with_overrides(&PromptOverrides::from(cli))
	}
}
use serde::{Deserialize, Serialize};

/// Persisted composer chrome style selected independently of a TUI renderer.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumIter,
	strum::EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ComposerStyle {
	/// Rounded frame with status embedded in the top edge.
	Box,
	/// Full-width rules with prompt gutter and status chip.
	Claude,
	/// Rounded frame with a prompt gutter and scrollbar.
	Pi,
	/// Unboxed prompt with a single curved left cue and a status strip above it.
	#[default]
	Borderless,
	/// One status-bearing rule above the input.
	Rule,
	/// Filled input surface with accented end caps.
	Field,
	/// Filled input surface anchored by an accented left rail.
	Rail,
}

use std::env;

use omp_collab::link::{RelayEndpoint, WebEndpoint};
use omp_envd::{
	host_settings::{RuntimeDurations, WorktreeSettings},
	tool_settings::ToolSettings,
};
use omp_ext::config::{ExtensionOverlay, Scope, ScopedOverlay};
use omp_settings::manager::{SettingsManager, SettingsManagerError, SettingsPaths};

use crate::prompt_prep::settings::{PromptOverrides, PromptSettings};

const PERSISTED_SCOPES: &[omp_settings::SettingScope] = &[
	omp_settings::SettingScope::Global,
	omp_settings::SettingScope::Project,
	omp_settings::SettingScope::Runtime,
];

const CORE_FIELDS: &[omp_settings::FieldDescriptor] = &[
	omp_settings::FieldDescriptor {
		path:        "runtime.interrupt_grace",
		label:       "Interrupt grace",
		description: "Courtesy interval before forced interruption.",
		kind:        omp_settings::SettingKind::Duration,
		scopes:      PERSISTED_SCOPES,
		order:       20,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.enabled",
		label:       "Automatic compaction",
		description: "Enable automatic context compaction.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       40,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.mid_turn_enabled",
		label:       "Mid-turn compaction",
		description: "Check compaction thresholds at safe tool-loop boundaries.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       41,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "compaction.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.async_enabled",
		label:       "Speculative compaction",
		description: "Allow latency-bearing methods to run speculatively.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       42,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "compaction.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.method_order",
		label:       "Compaction methods",
		description: "Ordered automatic compaction fallback ladder.",
		kind:        omp_settings::SettingKind::Array,
		scopes:      PERSISTED_SCOPES,
		order:       43,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "compaction.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.threshold_fraction",
		label:       "Compaction threshold",
		description: "Usable-context fraction that triggers compaction.",
		kind:        omp_settings::SettingKind::Number,
		scopes:      PERSISTED_SCOPES,
		order:       44,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "compaction.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.keep_recent_tokens",
		label:       "Recent tokens",
		description: "Recent-context growth retained around speculative summaries.",
		kind:        omp_settings::SettingKind::Integer,
		scopes:      PERSISTED_SCOPES,
		order:       45,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "compaction.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "context_promotion.enabled",
		label:       "Context promotion",
		description: "Promote to an eligible larger-context model before compaction.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       49,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "auto_thinking.backend",
		label:       "Auto-thinking backend",
		description: "Classifier backend.",
		kind:        omp_settings::SettingKind::Enum(&["online", "local"]),
		scopes:      PERSISTED_SCOPES,
		order:       50,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "auto_thinking.provisional",
		label:       "Provisional effort",
		description: "Effort used while classification settles.",
		kind:        omp_settings::SettingKind::Enum(&[
			"off", "minimal", "low", "medium", "high", "max",
		]),
		scopes:      PERSISTED_SCOPES,
		order:       51,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "auto_thinking.ceiling",
		label:       "Effort ceiling",
		description: "Maximum auto-classified effort.",
		kind:        omp_settings::SettingKind::Enum(&[
			"off", "minimal", "low", "medium", "high", "max",
		]),
		scopes:      PERSISTED_SCOPES,
		order:       52,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "auto_thinking.allow_max",
		label:       "Allow maximum effort",
		description: "Allow the online classifier to choose maximum effort.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       53,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "memory.backend",
		label:       "Memory backend",
		description: "Default-off durable memory backend.",
		kind:        omp_settings::SettingKind::Enum(&["off", "mnemopi"]),
		scopes:      PERSISTED_SCOPES,
		order:       54,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "mnemopi.scoping",
		label:       "Mnemopi bank scope",
		description: "Canonical-project and shared-bank recall policy.",
		kind:        omp_settings::SettingKind::Enum(&[
			"global",
			"per-project",
			"per-project-tagged",
		]),
		scopes:      PERSISTED_SCOPES,
		order:       55,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "memory.backend", equals: "mnemopi" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "autolearn.enabled",
		label:       "Automatic learning",
		description: "Enable managed-skill guidance and capture eligibility.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       56,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "autolearn.auto_continue",
		label:       "Auto-run learning capture",
		description: "Run one private managed-skill/lesson capture after a substantive turn.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       57,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "autolearn.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "autolearn.min_tool_calls",
		label:       "Automatic learning tool threshold",
		description: "Minimum settled tool executions required in one primary turn.",
		kind:        omp_settings::SettingKind::Integer,
		scopes:      PERSISTED_SCOPES,
		order:       58,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "autolearn.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "plan.enabled",
		label:       "Plan mode",
		description: "Enable the planning execution mode.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       60,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "plan.default_on_startup",
		label:       "Start in plan mode",
		description: "Enter plan mode at the start of a fresh interactive session.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       61,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "plan.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "worktree.base",
		label:       "Worktree base",
		description: "Base directory for Environment-owned isolated worktrees.",
		kind:        omp_settings::SettingKind::Path,
		scopes:      PERSISTED_SCOPES,
		order:       60,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "composer.shape",
		label:       "Composer shape",
		description: "Interactive composer chrome.",
		kind:        omp_settings::SettingKind::Enum(&[
			"box",
			"claude",
			"pi",
			"borderless",
			"rule",
			"field",
			"rail",
		]),
		scopes:      PERSISTED_SCOPES,
		order:       70,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "spelling.typo_detection",
		label:       "Typo detection",
		description: "Underline spelling mistakes in the composer.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       71,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "spelling.autocomplete",
		label:       "Spelling autocomplete",
		description: "Offer platform spelling completions.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       72,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "spelling.autocorrect",
		label:       "Spelling autocorrect",
		description: "Apply platform spelling corrections automatically.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       73,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "extensions",
		label:       "Extensions",
		description: "Client-scope extension overlay.",
		kind:        omp_settings::SettingKind::Table,
		scopes:      PERSISTED_SCOPES,
		order:       80,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "images.autoResize",
		label:       "Auto-resize images",
		description: "Resize large prompt images to 2000x2000 while preserving format.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       81,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "images.describeForTextModels",
		label:       "Describe images for text models",
		description: "Describe attached images when the selected model lacks vision support.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       82,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "secrets.enabled",
		label:       "Provider secret obfuscation",
		description: "Obfuscate configured secrets in provider-bound projections.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       82,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "export.shareRedactSecrets",
		label:       "Share secret redaction",
		description: "Irreversibly redact configured secrets from share snapshots.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       83,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "security.enabled",
		label:       "Local security review",
		description: "Register the restricted local security reviewer and command.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       84,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "collab.relayUrl",
		label:       "Collaboration relay",
		description: "OMP-v1 relay origin used by live collaboration rooms.",
		kind:        omp_settings::SettingKind::String,
		scopes:      PERSISTED_SCOPES,
		order:       85,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "collab.webUrl",
		label:       "Collaboration web UI",
		description: "Optional browser UI origin used for fragment-only room links.",
		kind:        omp_settings::SettingKind::String,
		scopes:      PERSISTED_SCOPES,
		order:       86,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "collab.displayName",
		label:       "Collaboration display name",
		description: "Name shown to live room participants; defaults to the OS username.",
		kind:        omp_settings::SettingKind::String,
		scopes:      PERSISTED_SCOPES,
		order:       87,
		options:     None,
		condition:   None,
		secret:      false,
	},
];

const fn default_true() -> bool {
	true
}

const fn default_compaction_threshold() -> f64 {
	0.8
}

const fn default_keep_recent_tokens() -> u64 {
	20_000
}

fn default_compaction_method_order() -> Vec<CompactionTier> {
	CompactionTier::ALL.to_vec()
}

/// Context-overflow model-promotion policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextPromotionSettings {
	/// Promote to a larger-context eligible model before compacting.
	#[serde(default)]
	pub enabled: bool,
}

/// Persisted automatic context-maintenance policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionSettings {
	/// Whether automatic context compaction is enabled.
	#[serde(default = "default_true")]
	pub enabled:            bool,
	/// Whether to check compaction thresholds between tool-loop requests.
	#[serde(default = "default_true")]
	pub mid_turn_enabled:   bool,
	/// Whether latency-bearing methods may run speculatively in the background.
	#[serde(default = "default_true")]
	pub async_enabled:      bool,
	/// Ordered enabled ladder methods. Omitted methods are disabled.
	#[serde(default = "default_compaction_method_order")]
	pub method_order:       Vec<CompactionTier>,
	/// Fraction of usable context that triggers automatic compaction.
	#[serde(default = "default_compaction_threshold")]
	pub threshold_fraction: f64,
	/// Recent-context growth allowed before an armed summary is refreshed.
	#[serde(default = "default_keep_recent_tokens")]
	pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
	fn default() -> Self {
		Self {
			enabled:            true,
			mid_turn_enabled:   true,
			async_enabled:      true,
			method_order:       default_compaction_method_order(),
			threshold_fraction: default_compaction_threshold(),
			keep_recent_tokens: default_keep_recent_tokens(),
		}
	}
}

impl CompactionSettings {
	/// Resolves duplicates while preserving the user's first-occurrence order.
	/// Disabled automatic compaction resolves to an empty ladder.
	pub fn method_order(&self) -> CompactionMethodOrder {
		if self.enabled {
			CompactionMethodOrder::resolve(&self.method_order)
		} else {
			CompactionMethodOrder::resolve(&[])
		}
	}

	/// Returns speculation options consumed by the agent coordinator.
	pub const fn speculation_options(&self) -> omp_agent::CompactionSpeculationOptions {
		omp_agent::CompactionSpeculationOptions {
			enabled:            self.async_enabled,
			keep_recent_tokens: self.keep_recent_tokens,
		}
	}
}

/// Persisted automatic per-turn reasoning classifier policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AutoThinkingSettings {
	/// Backend used for the constrained-output classifier.
	#[serde(default = "default_difficulty_backend")]
	pub backend:     DifficultyBackend,
	/// Provisional `auto` level while classification settles.
	#[serde(default)]
	pub provisional: Difficulty,
	/// Session-wide effort ceiling applied after classification.
	#[serde(default = "default_difficulty_ceiling")]
	pub ceiling:     Difficulty,
	/// Whether the online five-rung ladder may choose `max`.
	#[serde(default)]
	pub allow_max:   bool,
}

const fn default_difficulty_backend() -> DifficultyBackend {
	DifficultyBackend::Online
}

const fn default_difficulty_ceiling() -> Difficulty {
	Difficulty::Max
}

impl Default for AutoThinkingSettings {
	fn default() -> Self {
		Self {
			backend:     default_difficulty_backend(),
			provisional: Difficulty::default(),
			ceiling:     default_difficulty_ceiling(),
			allow_max:   false,
		}
	}
}

impl AutoThinkingSettings {
	/// Builds immutable classifier inputs for an ordinary turn.
	pub const fn for_turn(self) -> omp_inference::AutoDifficulty {
		omp_inference::AutoDifficulty {
			provisional:  self.provisional,
			ceiling:      self.ceiling,
			allow_max:    self.allow_max,
			prewalk_noop: false,
		}
	}

	/// Builds classifier inputs for a prewalk turn and applies its no-op hook.
	pub fn for_prewalk_turn(self, reason_to_execute: Option<&str>) -> omp_inference::AutoDifficulty {
		self.for_turn().with_prewalk_reason(reason_to_execute)
	}
}

/// Persisted planning-mode defaults.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanSettings {
	/// Whether plan mode is available.
	#[serde(default = "default_plan_enabled")]
	pub enabled:            bool,
	/// Whether fresh interactive sessions begin in plan mode.
	#[serde(default)]
	pub default_on_startup: bool,
}

const fn default_plan_enabled() -> bool {
	true
}

impl Default for PlanSettings {
	fn default() -> Self {
		Self { enabled: true, default_on_startup: false }
	}
}

/// Persisted appearance options for the interactive composer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComposerSettings {
	/// Built-in chrome rendered around the interactive input.
	#[serde(default)]
	pub shape: ComposerStyle,
}
/// Platform spelling assistance for the interactive composer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpellingSettings {
	/// Whether misspellings are detected and underlined.
	#[serde(default = "default_true")]
	pub typo_detection: bool,
	/// Whether platform spelling completions are offered.
	#[serde(default = "default_true")]
	pub autocomplete:   bool,
	/// Whether platform spelling corrections are applied automatically.
	#[serde(default)]
	pub autocorrect:    bool,
}

impl Default for SpellingSettings {
	fn default() -> Self {
		Self { typo_detection: true, autocomplete: true, autocorrect: false }
	}
}

/// Prompt image attachment policy.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImageSettings {
	/// Resize dimensions above the provider-compatible ceiling.
	#[serde(default = "default_true")]
	pub auto_resize:              bool,
	/// Describe images through a vision-capable model when the selected model
	/// cannot consume them.
	pub describe_for_text_models: bool,
}

impl Default for ImageSettings {
	fn default() -> Self {
		Self { auto_resize: true, describe_for_text_models: true }
	}
}

/// Reversible provider-bound secret policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SecretsSettings {
	/// Whether complete bidirectional provider obfuscation is enabled.
	#[serde(default)]
	pub enabled: bool,
}

/// Default-off registration policy for the minimal local reviewer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SecuritySettings {
	/// Whether the reviewer profile, prompt contribution, and slash command are
	/// registered.
	#[serde(default)]
	pub enabled: bool,
}

/// Irreversible export-boundary policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExportSettings {
	/// Whether share snapshots are irreversibly redacted before leaving Core.
	#[serde(default = "default_true", rename = "shareRedactSecrets")]
	pub share_redact_secrets: bool,
}

impl Default for ExportSettings {
	fn default() -> Self {
		Self { share_redact_secrets: true }
	}
}
/// Live collaboration identity and endpoint settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CollabSettings {
	/// OMP-v1 relay origin.
	#[serde(default = "default_collab_relay_url", rename = "relayUrl")]
	pub relay_url:    String,
	/// Optional browser UI origin. Empty derives an HTTP(S) origin from the
	/// relay.
	#[serde(default, rename = "webUrl")]
	pub web_url:      String,
	/// Optional user-facing participant name.
	#[serde(default, rename = "displayName")]
	pub display_name: String,
}

impl Default for CollabSettings {
	fn default() -> Self {
		Self {
			relay_url:    default_collab_relay_url(),
			web_url:      String::new(),
			display_name: String::new(),
		}
	}
}

impl CollabSettings {
	/// Validates and normalizes the relay origin.
	pub fn relay_endpoint(&self) -> Result<RelayEndpoint, omp_collab::link::EndpointError> {
		RelayEndpoint::parse(&self.relay_url)
	}

	/// Validates the configured browser origin or derives it from the relay.
	pub fn web_endpoint(&self) -> Result<WebEndpoint, omp_collab::link::EndpointError> {
		let relay = self.relay_endpoint()?;
		if self.web_url.trim().is_empty() {
			Ok(WebEndpoint::from_relay(&relay))
		} else {
			WebEndpoint::parse(&self.web_url)
		}
	}

	/// Resolves a trimmed participant name as setting → OS account →
	/// `anonymous`.
	pub fn resolved_display_name(&self) -> Str {
		let os_name = os_username();
		resolve_collab_display_name(&self.display_name, os_name.as_deref())
	}
}

fn resolve_collab_display_name(configured: &str, os_name: Option<&str>) -> Str {
	let configured = configured.trim();
	if !configured.is_empty() {
		return Str::from(configured);
	}
	os_name
		.map(str::trim)
		.filter(|name| !name.is_empty())
		.map_or_else(|| sf!("anonymous"), Str::from)
}

fn default_collab_relay_url() -> String {
	omp_collab::link::DEFAULT_RELAY_URL.to_owned()
}

#[cfg(unix)]
fn os_username() -> Option<String> {
	use std::{ffi::CStr, mem::MaybeUninit, ptr};

	let mut account = MaybeUninit::<libc::passwd>::uninit();
	let mut resolved = ptr::null_mut();
	let mut buffer = vec![0_u8; 16 * 1024];
	// SAFETY: `account`, `resolved`, and the writable buffer live through the
	// call. A non-null result points into `account`/`buffer` and is consumed
	// before either is dropped.
	let status = unsafe {
		libc::getpwuid_r(
			libc::geteuid(),
			account.as_mut_ptr(),
			buffer.as_mut_ptr().cast(),
			buffer.len(),
			&mut resolved,
		)
	};
	if status != 0 || resolved.is_null() {
		return None;
	}
	// SAFETY: successful `getpwuid_r` initialized `account` and its `pw_name`
	// points at a NUL-terminated string within the live buffer.
	let account = unsafe { account.assume_init() };
	let name = unsafe { CStr::from_ptr(account.pw_name) };
	name.to_str().ok().map(ToOwned::to_owned)
}

#[cfg(windows)]
fn os_username() -> Option<String> {
	use std::env;
	env::var("USERNAME").ok()
}

#[cfg(not(any(unix, windows)))]
fn os_username() -> Option<String> {
	use std::env;
	env::var("USER").ok()
}

/// Persisted client-scope preferences under `<data_dir>/config.toml`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
	/// Runtime timeout and cancellation settings.
	#[serde(default)]
	pub runtime:           RuntimeDurations,
	/// Built-in tool exposure and execution timeout policy.
	#[serde(default)]
	pub tools:             ToolSettings,
	/// Context-overflow model-promotion policy.
	#[serde(default)]
	pub context_promotion: ContextPromotionSettings,
	/// Automatic context-maintenance options.
	#[serde(default)]
	pub compaction:        CompactionSettings,
	/// Automatic per-turn reasoning classification.
	#[serde(default)]
	pub auto_thinking:     AutoThinkingSettings,
	/// Planning-mode availability and startup policy.
	#[serde(default)]
	pub plan:              PlanSettings,
	/// Default-off memory backend selector.
	#[serde(default)]
	pub memory:            MemorySettings,
	/// Mnemopi-specific durable bank and lifecycle settings.
	#[serde(default)]
	pub mnemopi:           MnemopiSettings,
	/// Automatic-learning capture settings.
	#[serde(default)]
	pub autolearn:         AutolearnSettings,
	/// Isolated worktree placement policy.
	#[serde(default)]
	pub worktree:          WorktreeSettings,
	/// Interactive composer appearance.
	#[serde(default)]
	pub composer:          ComposerSettings,
	/// Platform spelling assistance for the interactive composer.
	#[serde(default)]
	pub spelling:          SpellingSettings,
	/// Terminal display and rendering behavior.
	#[serde(default)]
	pub display:           DisplaySettings,
	/// Pi-compatible TUI rendering and input settings.
	#[serde(default)]
	pub tui:               TuiSettings,
	/// Root-level Pi-compatible display switches.
	#[serde(flatten)]
	pub root_display:      RootDisplaySettings,
	/// Theme, status-line, and icon choices.
	#[serde(default)]
	pub appearance:        AppearanceSettings,
	/// Notifications, voice, loops, and input behavior.
	#[serde(default)]
	pub interaction:       InteractionSettings,
	/// Successful-turn notification policy.
	#[serde(default)]
	pub completion:        CompletionSettings,
	/// Failed-turn notification policy.
	#[serde(default)]
	pub error:             ErrorNotificationSettings,
	/// Time-traveling stream-rule policy.
	#[serde(default)]
	pub ttsr:              TtsrSettings,
	/// Miscellaneous startup, retention, and workspace policy.
	#[serde(default)]
	pub lifecycle:         LifecycleSettings,
	/// Encrypted session-sharing endpoint and backing store.
	#[serde(default)]
	pub share:             ShareSettings,
	/// Session title generation policy.
	#[serde(default)]
	pub title:             TitleSettings,
	/// Idle recap generation policy.
	#[serde(default)]
	pub recap:             RecapSettings,
	/// Client-scope extension overlay.
	#[serde(default)]
	pub extensions:        ExtensionOverlay,
	/// Prompt image attachment policy.
	#[serde(default)]
	pub images:            ImageSettings,
	/// Reversible provider-bound secret policy.
	#[serde(default)]
	pub secrets:           SecretsSettings,
	/// Default-off local security-review registration.
	#[serde(default)]
	pub security:          SecuritySettings,
	/// Irreversible export-boundary policy.
	#[serde(default)]
	pub export:            ExportSettings,
	/// Live collaboration endpoints and participant identity.
	#[serde(default)]
	pub collab:            CollabSettings,
}

impl omp_settings::SettingsDomain for Settings {
	const DOMAIN: &'static str = "app-core";
	const FIELDS: &'static [omp_settings::FieldDescriptor] = CORE_FIELDS;
	const PREFIX: Option<&'static str> = None;

	fn validate(&self) -> Result<(), omp_settings::ValidationError> {
		if self.extensions.validate(Scope::Client).is_err()
			|| !(0.0..=1.0).contains(&self.compaction.threshold_fraction)
			|| self.compaction.threshold_fraction == 0.0
			|| self.collab.relay_endpoint().is_err()
			|| self.collab.web_endpoint().is_err()
		{
			return Err(omp_settings::ValidationError::DomainInvariant { domain: Self::DOMAIN });
		}
		Ok(())
	}
}

omp_settings::inventory::submit! {
	omp_settings::DomainRegistration::of::<Settings>()
}

/// Loads the current typed projection for the process working directory.
pub fn current(data_dir: &Path) -> Result<Settings, SettingsManagerError> {
	current_with_overlays(data_dir, &[])
}

/// Loads settings with ordered invocation-local native TOML overlays.
pub fn current_with_overlays(
	data_dir: &Path,
	overlays: &[PathBuf],
) -> Result<Settings, SettingsManagerError> {
	let project = env::current_dir().ok();
	current_for_project_with_overlays(data_dir, project.as_deref(), overlays)
}

/// Loads settings for exactly `project`, without walking ancestor `.omp`
/// directories.
pub fn current_for_project(
	data_dir: &Path,
	project: &Path,
) -> Result<Settings, SettingsManagerError> {
	current_for_project_with_overlays(data_dir, Some(project), &[])
}

/// Loads settings for exactly `project` plus ordered invocation-local TOML or
/// YAML overlays.
pub fn current_for_project_with_overlays(
	data_dir: &Path,
	project: Option<&Path>,
	overlays: &[PathBuf],
) -> Result<Settings, SettingsManagerError> {
	let mut paths = SettingsPaths::discover(data_dir, project);
	paths.overlays.extend_from_slice(overlays);
	let manager = SettingsManager::open(paths)?;
	let projection = manager
		.snapshot()
		.project::<Settings>()
		.map_err(|error| SettingsManagerError::Projection { source: error })?;
	let mut settings = projection.get().clone();
	settings.mnemopi = settings.mnemopi.normalize();
	Ok(settings)
}

impl Settings {
	/// Returns the resolved runtime durations.
	pub const fn runtime_durations(&self) -> RuntimeDurations {
		self.runtime
	}

	/// Constructs the ordered P1 client → workspace overlay list and validates
	/// each scope's security invariants.
	pub fn extension_scopes(
		&self,
		workspace: Option<ExtensionOverlay>,
	) -> Result<Vec<ScopedOverlay>, omp_ext::ExtensionError> {
		self.extensions.validate(Scope::Client)?;
		let mut scopes =
			vec![ScopedOverlay { scope: Scope::Client, overlay: self.extensions.clone() }];
		if let Some(workspace) = workspace {
			workspace.validate(Scope::Workspace)?;
			scopes.push(ScopedOverlay { scope: Scope::Workspace, overlay: workspace });
		}
		Ok(scopes)
	}
}

#[cfg(test)]
mod tests {
	use std::fs;

	use omp_core::DurationUnit;

	use super::*;
	#[test]
	fn isolated_snapshot_round_trip() {
		let settings = Settings::default();
		let snapshot = omp_settings::SettingsSnapshot::isolated(settings.clone()).expect("snapshot");
		let loaded = snapshot.project::<Settings>().expect("projection");
		assert_eq!(
			loaded.get().runtime_durations().interrupt_grace,
			settings.runtime_durations().interrupt_grace,
		);
		assert_eq!(
			loaded.get().runtime_durations().interrupt_grace.unit(),
			DurationUnit::Milliseconds,
		);
	}

	#[test]
	fn tool_settings_default_enabled_and_parse_timeout() {
		let settings: Settings = toml::from_str(
			"[tools]\nmax_timeout = \"30s\"\nedit_dialect = \"rep.1\"\n[tools.enabled]\nask = false",
		)
		.expect("tool settings parse");
		assert!(!settings.tools.enabled("ask"));
		assert!(settings.tools.enabled("todo"));
		assert_eq!(
			settings.tools.max_timeout,
			Some(omp_core::Duration::new(30, DurationUnit::Seconds))
		);
		assert_eq!(settings.tools.edit_dialect.as_deref(), Some("rep.1"));
	}

	#[test]
	fn compaction_defaults_to_the_current_ladder() {
		let settings: Settings = toml::from_str("").expect("defaults parse");
		assert_eq!(settings.compaction.method_order().as_slice(), &CompactionTier::ALL);
		assert!(settings.compaction.speculation_options().enabled);
		assert!(settings.compaction.mid_turn_enabled);
		assert!(!settings.context_promotion.enabled);
		assert_eq!(settings.interaction.steering_mode, SteeringMode::OneAtATime);
		assert_eq!(settings.compaction.keep_recent_tokens, 20_000);
	}

	#[test]
	fn compaction_method_order_is_user_ordered_and_deduplicated() {
		let settings: Settings = toml::from_str(
			"[compaction]\nmethod_order = [\"remote\", \"local\", \"remote\", \
			 \"handoff\"]\nasync_enabled = false",
		)
		.expect("compaction settings parse");
		assert_eq!(settings.compaction.method_order().as_slice(), &[
			CompactionTier::Remote,
			CompactionTier::Local,
			CompactionTier::Handoff
		],);
		assert!(!settings.compaction.speculation_options().enabled);
		let mut disabled = settings.compaction.clone();
		disabled.enabled = false;
		assert!(disabled.method_order().as_slice().is_empty());
	}

	#[test]
	fn auto_thinking_backend_and_ceiling_are_configurable() {
		let settings: Settings = toml::from_str(
			"[auto_thinking]\nbackend = \"local\"\nprovisional = \"high\"\nceiling = \
			 \"medium\"\nallow_max = true",
		)
		.expect("auto thinking settings parse");
		assert_eq!(settings.auto_thinking.backend, DifficultyBackend::Local);
		assert_eq!(settings.auto_thinking.provisional, Difficulty::High);
		assert_eq!(settings.auto_thinking.ceiling, Difficulty::Medium);
		assert!(settings.auto_thinking.allow_max);
		assert!(!settings.auto_thinking.for_turn().prewalk_noop);
		assert!(settings.auto_thinking.for_prewalk_turn(None).prewalk_noop);
	}

	#[test]
	fn plan_settings_use_owned_nested_keys() {
		let settings: Settings = toml::from_str("[plan]\nenabled = true\ndefault_on_startup = true")
			.expect("plan settings parse");
		assert!(settings.plan.enabled);
		assert!(settings.plan.default_on_startup);
		let encoded = toml::to_string(&settings).expect("plan settings serialize");
		assert!(encoded.contains("[plan]"));
		assert!(encoded.contains("default_on_startup = true"));
	}

	#[test]
	fn composer_shape_uses_nested_appearance_setting() {
		assert_eq!(Settings::default().composer.shape, ComposerStyle::Borderless);
		let settings: Settings =
			toml::from_str("[composer]\nshape = \"rail\"").expect("composer settings parse");
		assert_eq!(settings.composer.shape, ComposerStyle::Rail);
		let encoded = toml::to_string(&settings).expect("composer settings serialize");
		assert!(encoded.contains("[composer]"));
		assert!(encoded.contains("shape = \"rail\""));
	}
	#[test]
	fn unexpected_stop_and_spelling_defaults_are_explicit() {
		let defaults: Settings = toml::from_str("").expect("defaults parse");
		assert_eq!(defaults.interaction.unexpected_stop_detection, UnexpectedStopMode::Mechanical,);
		assert!(defaults.spelling.typo_detection);
		assert!(defaults.spelling.autocomplete);
		assert!(!defaults.spelling.autocorrect);

		let configured: Settings = toml::from_str(
			"[interaction]\nunexpectedStopDetection = \"smart\"\n[spelling]\ntypo_detection = \
			 false\nautocomplete = false\nautocorrect = true",
		)
		.expect("unexpected-stop and spelling settings parse");
		assert_eq!(configured.interaction.unexpected_stop_detection, UnexpectedStopMode::Smart,);
		assert!(!configured.spelling.typo_detection);
		assert!(!configured.spelling.autocomplete);
		assert!(configured.spelling.autocorrect);
	}

	#[test]
	fn configured_runtime_duration_precedes_default() {
		let settings: Settings = toml::from_str("[runtime]\ninterrupt_grace = \"375ms\"")
			.expect("configured duration parses");

		assert_eq!(
			settings.runtime_durations().interrupt_grace,
			omp_core::Duration::new(375, DurationUnit::Milliseconds),
		);
		assert_eq!(settings.runtime_durations().interrupt_grace.to_string(), "375ms");
	}

	#[test]
	fn missing_runtime_duration_uses_explicit_unit_default() {
		let settings: Settings = toml::from_str("").expect("defaults parse");

		assert_eq!(settings.runtime_durations().interrupt_grace, omp_tool::DEFAULT_INTERRUPT_GRACE,);
		assert_eq!(settings.runtime_durations().interrupt_grace.to_string(), "150ms");
	}

	#[test]
	fn collab_settings_parse_camel_case_and_trim_identity() {
		let settings: Settings = toml::from_str(
			"[collab]\nrelayUrl = \"https://relay.example\"\nwebUrl = \
			 \"https://collab.example\"\ndisplayName = \"  Ada  \"",
		)
		.expect("collab settings parse");
		assert_eq!(
			settings
				.collab
				.relay_endpoint()
				.expect("relay")
				.as_url()
				.scheme(),
			"wss"
		);
		assert_eq!(settings.collab.resolved_display_name().as_str(), "Ada");
		assert!(omp_settings::SettingsDomain::validate(&settings).is_ok());
	}

	#[test]
	fn collab_display_name_precedence_is_deterministic() {
		assert_eq!(
			resolve_collab_display_name(" configured ", Some("os-user")).as_str(),
			"configured"
		);
		assert_eq!(resolve_collab_display_name(" ", Some(" os-user ")).as_str(), "os-user");
		assert_eq!(resolve_collab_display_name("", None).as_str(), "anonymous");
		assert_eq!(resolve_collab_display_name("", Some(" ")).as_str(), "anonymous");
	}
	#[test]
	fn collab_insecure_endpoints_are_loopback_only() {
		let mut settings = Settings::default();
		settings.collab.relay_url = "ws://relay.example".to_owned();
		assert!(omp_settings::SettingsDomain::validate(&settings).is_err());
		settings.collab.relay_url = "ws://localhost:9070".to_owned();
		settings.collab.web_url = "http://127.0.0.1:9071".to_owned();
		assert!(omp_settings::SettingsDomain::validate(&settings).is_ok());
	}
	#[test]
	fn corrupt_settings_are_quarantined_with_diagnostics() {
		let data_dir = tempfile::tempdir().expect("create temporary data directory");
		let path = data_dir.path().join("config.toml");
		fs::write(&path, "not valid toml").expect("write corrupt settings");
		let manager = SettingsManager::open(SettingsPaths {
			global:   path.clone(),
			project:  None,
			overlays: Vec::new(),
		})
		.expect("manager");
		let diagnostics = manager.diagnostics();
		assert_eq!(diagnostics.len(), 1);
		assert_eq!(diagnostics[0].path, path);
		assert!(diagnostics[0].backup_path.exists());
		assert!(!path.exists());
	}
	#[test]
	fn custom_legacy_theme_migrates_to_schema_valid_scalar_before_marker() {
		let data_dir = tempfile::tempdir().expect("data");
		fs::write(
			data_dir.path().join("settings.json"),
			r#"{"theme":"solarized","defaultThinkingLevel":"high"}"#,
		)
		.expect("legacy settings");
		let paths = SettingsPaths::discover(data_dir.path(), None);
		let manager = SettingsManager::open(paths.clone()).expect("first startup");
		assert_eq!(
			manager
				.snapshot()
				.project::<AppearanceSettings>()
				.expect("appearance")
				.get()
				.theme
				.as_str(),
			"solarized",
		);
		assert_eq!(
			manager
				.snapshot()
				.project::<omp_catalog::settings::ModelSettings>()
				.expect("model")
				.get()
				.default_thinking,
			omp_catalog::ThinkingEffort::High,
		);
		let persisted =
			fs::read_to_string(data_dir.path().join("config.toml")).expect("native config");
		let parsed: toml::Table = toml::from_str(&persisted).expect("schema-shaped TOML");
		assert_eq!(
			parsed
				.get("appearance")
				.and_then(toml::Value::as_table)
				.and_then(|appearance| appearance.get("theme"))
				.and_then(toml::Value::as_str),
			Some("solarized"),
		);
		assert!(data_dir.path().join(".settings-migration-v2").is_file());
		SettingsManager::open(paths).expect("subsequent startup");
	}
}
