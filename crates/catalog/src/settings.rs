//! Runtime-owned model, thinking, provider, and wire settings projections.

#![allow(missing_docs, reason = "strum IntoStaticStr emits undocumented inherent methods")]

use std::{
	collections::BTreeMap,
	path::{Component, Path, PathBuf},
	sync,
	time::Duration,
};

use omp_core::Str;
use omp_settings::{
	DomainRegistration, FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError,
};
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

use crate::{
	capability::{ProviderFamily, ServiceTier, TierAudience},
	id::WireModelId,
	provider::TransportKind,
	thinking::{ThinkingEffort, ThinkingPolicy},
};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

/// Token budgets associated with portable reasoning effort levels.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ThinkingBudgets {
	/// Minimal-effort token ceiling.
	pub minimal: u64,
	/// Low-effort token ceiling.
	pub low:     u64,
	/// Medium-effort token ceiling.
	pub medium:  u64,
	/// High-effort token ceiling.
	pub high:    u64,
	/// Extra-high-effort token ceiling.
	pub xhigh:   u64,
	/// Maximum-effort token ceiling.
	pub max:     u64,
}

impl Default for ThinkingBudgets {
	fn default() -> Self {
		Self {
			minimal: 1_024,
			low:     2_048,
			medium:  8_192,
			high:    16_384,
			xhigh:   32_768,
			max:     32_768,
		}
	}
}

impl ThinkingBudgets {
	/// Returns the configured budget for a concrete effort.
	pub const fn for_effort(self, effort: ThinkingEffort) -> Option<u64> {
		match effort {
			ThinkingEffort::Off => None,
			ThinkingEffort::Minimal => Some(self.minimal),
			ThinkingEffort::Low => Some(self.low),
			ThinkingEffort::Medium => Some(self.medium),
			ThinkingEffort::High => Some(self.high),
			ThinkingEffort::XHigh => Some(self.xhigh),
			ThinkingEffort::Max => Some(self.max),
		}
	}
}

/// Portable service-tier selection persisted without provider credentials.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TierSetting {
	/// Omit a service tier.
	#[default]
	None,
	/// Inherit the root session tier.
	Inherit,
	/// Provider standard tier.
	Standard,
	/// Provider flex tier.
	Flex,
	/// Provider priority tier.
	Priority,
}

impl TierSetting {
	fn resolve(&self, family: ProviderFamily, parent: Option<&ServiceTier>) -> Option<ServiceTier> {
		match self {
			Self::None => None,
			Self::Inherit => parent.cloned(),
			Self::Standard => Some(ServiceTier { name: Str::new_static("standard"), priority: 0 }),
			Self::Flex if family == ProviderFamily::OpenAi => {
				Some(ServiceTier { name: Str::new_static("flex"), priority: -10 })
			},
			Self::Flex => None,
			Self::Priority => {
				Some(ServiceTier { name: Str::new_static("priority"), priority: 10 })
			},
		}
	}
}

/// Default OpenRouter routing suffix.
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
#[strum(serialize_all = "lowercase", ascii_case_insensitive, const_into_str)]
pub enum OpenRouterVariant {
	/// Do not append a routing suffix.
	#[default]
	Default,
	/// Prefer throughput and latency.
	Nitro,
	/// Prefer lowest price.
	Floor,
	/// Enable OpenRouter online routing.
	Online,
	/// Use OpenRouter's curated exacto route.
	Exacto,
}

/// Tri-state wire feature selection.
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
#[strum(serialize_all = "lowercase", ascii_case_insensitive, const_into_str)]
pub enum WireToggle {
	/// Follow catalog policy.
	#[default]
	Auto,
	/// Disable the feature.
	Off,
	/// Require the feature.
	On,
}

/// Kimi provider API format.
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
#[strum(serialize_all = "lowercase", ascii_case_insensitive, const_into_str)]
pub enum KimiApiFormat {
	/// Follow live catalog metadata.
	#[default]
	Auto,
	/// Require an OpenAI-compatible route.
	OpenAi,
	/// Require an Anthropic-compatible route.
	Anthropic,
}

/// Prompt-cache retention selection.
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
#[strum(serialize_all = "lowercase", ascii_case_insensitive, const_into_str)]
pub enum CacheRetentionSetting {
	/// Preserve request intent and catalog defaults.
	#[default]
	Auto,
	/// Disable prompt caching.
	None,
	/// Request short retention.
	Short,
	/// Request long retention.
	Long,
}

/// Persistence scope for configured model role assignments.
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
#[strum(serialize_all = "lowercase", ascii_case_insensitive, const_into_str)]
pub enum ModelRoleStorage {
	/// Persist role assignments in the active global profile.
	#[default]
	Global,
	/// Persist role assignments in project settings with global fallback.
	Project,
}

/// Presentation metadata for one configured model role.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelTag {
	/// Human-readable role label.
	pub name:   Str,
	/// Optional presentation color.
	#[serde(default)]
	pub color:  Option<Str>,
	/// Whether the role is functional but omitted from selectors.
	#[serde(default)]
	pub hidden: bool,
}

/// Model selector assignments keyed by role name.
pub type ModelRoles = BTreeMap<Str, Str>;

/// Presentation metadata keyed by role name.
pub type ModelTags = BTreeMap<Str, ModelTag>;

/// Catalog-owned model and provider policy projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ModelSettings {
	/// Model selector assignments keyed by role name.
	pub roles:                    ModelRoles,
	/// Persistence scope for model role assignments.
	pub role_storage:             ModelRoleStorage,
	/// Presentation metadata keyed by model role.
	pub tags:                     ModelTags,
	/// Role names in quick-cycle order.
	pub cycle_order:              ArcStrList,
	/// Optional canonical model selector allow-list.
	pub enabled_models:           PathScopedStrList,
	/// Provider ids excluded from discovery, selection, and routing.
	pub disabled_providers:       PathScopedStrList,
	/// Default thinking effort used when a caller leaves effort unset.
	pub default_thinking:         ThinkingEffort,
	/// Universal configured reasoning ceiling.
	pub thinking_ceiling:         ThinkingEffort,
	/// Per-effort reasoning token budgets.
	pub thinking_budgets:         ThinkingBudgets,
	/// Provider ids in preferred routing order.
	pub provider_order:           ArcStrList,
	/// OpenAI-family service tier.
	pub tier_openai:              TierSetting,
	/// Anthropic-family service tier.
	pub tier_anthropic:           TierSetting,
	/// Google-family service tier.
	pub tier_google:              TierSetting,
	/// Fireworks serving tier.
	pub tier_fireworks:           TierSetting,
	/// Spawned-agent tier override.
	pub tier_subagent:            TierSetting,
	/// Advisor tier override.
	pub tier_advisor:             TierSetting,
	/// Prompt-cache retention policy.
	pub cache_retention:          CacheRetentionSetting,
	/// OpenAI Codex websocket preference.
	pub openai_websockets:        WireToggle,
	/// Default OpenRouter routing suffix.
	pub openrouter_variant:       OpenRouterVariant,
	/// Kimi wire format preference.
	pub kimi_api_format:          KimiApiFormat,
	/// Model selector for tiny/title work.
	pub tiny_selector:            Str,
	/// Model selector for memory inference.
	pub memory_selector:          Str,
	/// Model selector for automatic-thinking classification.
	pub auto_thinking_selector:   Str,
	/// Model selector for unexpected-stop classification.
	pub unexpected_stop_selector: Str,
}

/// Clone-cheap string sequence.
pub type ArcStrList = sync::Arc<[Str]>;

/// One string or a string sequence in path-scoped settings syntax.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum OneOrManyStr {
	/// One value.
	One(Str),
	/// Multiple values.
	Many(Box<[Str]>),
}

/// One mixed bare or path-scoped string-list entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PathScopedStringEntry {
	/// A value active in every working directory.
	Bare(Str),
	/// Values active below at least one configured path prefix.
	Scoped(PathScopedStringValues),
}

/// Path predicates and values for one scoped list entry.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PathScopedStringValues {
	/// Singular path prefix.
	pub path:          Option<OneOrManyStr>,
	/// Path-prefix sequence.
	pub paths:         Option<OneOrManyStr>,
	/// Singular legacy path-prefix spelling.
	pub path_prefix:   Option<OneOrManyStr>,
	/// Legacy path-prefix sequence spelling.
	pub path_prefixes: Option<OneOrManyStr>,
	/// Generic values.
	pub values:        Option<OneOrManyStr>,
	/// Generic item spelling.
	pub items:         Option<OneOrManyStr>,
	/// Model selectors used by enabled-model entries.
	pub models:        Option<OneOrManyStr>,
	/// Provider ids used by disabled-provider entries.
	pub providers:     Option<OneOrManyStr>,
}

/// Clone-cheap mixed global/path-scoped string-list source.
pub type PathScopedStrList = sync::Arc<[PathScopedStringEntry]>;

impl Default for ModelSettings {
	fn default() -> Self {
		Self {
			roles:                    BTreeMap::new(),
			role_storage:             ModelRoleStorage::Global,
			tags:                     BTreeMap::new(),
			cycle_order:              sync::Arc::from([
				Str::new_static("smol"),
				Str::new_static("default"),
				Str::new_static("slow"),
			]),
			enabled_models:           sync::Arc::from([]),
			disabled_providers:       sync::Arc::from([]),
			default_thinking:         ThinkingEffort::Medium,
			thinking_ceiling:         ThinkingEffort::Max,
			thinking_budgets:         ThinkingBudgets::default(),
			provider_order:           sync::Arc::from([]),
			tier_openai:              TierSetting::None,
			tier_anthropic:           TierSetting::None,
			tier_google:              TierSetting::None,
			tier_fireworks:           TierSetting::None,
			tier_subagent:            TierSetting::Inherit,
			tier_advisor:             TierSetting::None,
			cache_retention:          CacheRetentionSetting::Auto,
			openai_websockets:        WireToggle::Auto,
			openrouter_variant:       OpenRouterVariant::Default,
			kimi_api_format:          KimiApiFormat::Auto,
			tiny_selector:            Str::new_static("@tiny"),
			memory_selector:          Str::new_static("@tiny"),
			auto_thinking_selector:   Str::new_static("@tiny"),
			unexpected_stop_selector: Str::new_static("@tiny"),
		}
	}
}

impl ModelSettings {
	/// Applies configured effort budgets and the configured default to one model
	/// policy.
	pub fn apply_thinking_policy(&self, policy: &mut ThinkingPolicy) {
		policy.default_level = Some(self.default_thinking)
			.filter(|effort| *effort != ThinkingEffort::Off && policy.efforts.contains(effort));
		for effort in policy.efforts.iter().copied() {
			if let Some(budget) = self.thinking_budgets.for_effort(effort) {
				policy.effort_budgets.insert(effort, budget);
			}
		}
	}

	/// Returns a stable provider preference rank; unlisted providers follow
	/// listed ones.
	pub fn provider_rank(&self, provider: &str) -> usize {
		self
			.provider_order
			.iter()
			.position(|item| item == provider)
			.unwrap_or(usize::MAX)
	}

	/// Returns the configured selector for one role.
	pub fn role_selector(&self, role: &str) -> Option<&Str> {
		self.roles.get(role)
	}

	/// Returns presentation metadata for one role.
	pub fn role_tag(&self, role: &str) -> Option<&ModelTag> {
		self.tags.get(role)
	}

	/// Returns a role's quick-cycle rank; unlisted roles follow configured ones.
	pub fn cycle_rank(&self, role: &str) -> usize {
		self
			.cycle_order
			.iter()
			.position(|configured| configured == role)
			.unwrap_or(usize::MAX)
	}

	/// Resolves enabled-model entries for an exact working directory.
	pub fn resolved_enabled_models(&self, cwd: &Path, home: &Path) -> ArcStrList {
		resolve_path_scoped(&self.enabled_models, cwd, home, ScopedValueKind::Models)
	}

	/// Resolves disabled-provider entries for an exact working directory.
	pub fn resolved_disabled_providers(&self, cwd: &Path, home: &Path) -> ArcStrList {
		resolve_path_scoped(&self.disabled_providers, cwd, home, ScopedValueKind::Providers)
	}

	/// Clones these settings into one frozen working-directory projection.
	///
	/// The returned enabled-model and disabled-provider lists contain only bare
	/// entries, so downstream routing, inference, and discovery do not retain
	/// filesystem context.
	pub fn resolve_path_scopes(&self, cwd: &Path, home: &Path) -> Self {
		let mut resolved = self.clone();
		resolved.enabled_models = self
			.resolved_enabled_models(cwd, home)
			.iter()
			.cloned()
			.map(PathScopedStringEntry::Bare)
			.collect::<Vec<_>>()
			.into();
		resolved.disabled_providers = self
			.resolved_disabled_providers(cwd, home)
			.iter()
			.cloned()
			.map(PathScopedStringEntry::Bare)
			.collect::<Vec<_>>()
			.into();
		resolved
	}

	/// Reports whether a provider remains eligible using bare global entries.
	pub fn provider_allowed(&self, provider: &str) -> bool {
		!self
			.disabled_providers
			.iter()
			.any(|entry| matches!(entry, PathScopedStringEntry::Bare(value) if value == provider))
	}

	/// Reports whether a provider remains eligible at an exact working
	/// directory.
	pub fn provider_allowed_at(&self, cwd: &Path, home: &Path, provider: &str) -> bool {
		!self
			.resolved_disabled_providers(cwd, home)
			.iter()
			.any(|disabled| disabled == provider)
	}

	/// Reports whether a canonical identity is inside the bare global model
	/// scope.
	pub fn model_allowed(&self, provider: &str, model: &str) -> bool {
		let patterns = self.enabled_models.iter().filter_map(|entry| match entry {
			PathScopedStringEntry::Bare(value) => Some(value),
			PathScopedStringEntry::Scoped(_) => None,
		});
		self.provider_allowed(provider) && model_matches(patterns, provider, model)
	}

	/// Appends a persistently selected canonical model to a non-empty model
	/// scope.
	///
	/// The first configured occurrence wins case-insensitively. Empty scopes
	/// remain empty so persisting a default never creates a new restriction.
	pub fn insert_persisted_default(&mut self, canonical: &str) -> bool {
		if self.enabled_models.is_empty()
			|| self.enabled_models.iter().any(|entry| match entry {
				PathScopedStringEntry::Bare(value) => value.eq_ignore_ascii_case(canonical),
				PathScopedStringEntry::Scoped(source) => scoped_values(source, ScopedValueKind::Models)
					.iter()
					.any(|value| value.eq_ignore_ascii_case(canonical)),
			}) {
			return false;
		}
		let mut enabled = self.enabled_models.iter().cloned().collect::<Vec<_>>();
		enabled.push(PathScopedStringEntry::Bare(Str::new(canonical)));
		self.enabled_models = enabled.into();
		true
	}

	/// Reports whether a canonical identity is inside the resolved
	/// working-directory scope.
	pub fn model_allowed_at(&self, cwd: &Path, home: &Path, provider: &str, model: &str) -> bool {
		let patterns = self.resolved_enabled_models(cwd, home);
		self.provider_allowed_at(cwd, home, provider)
			&& model_matches(patterns.iter(), provider, model)
	}

	/// Returns the stable routing rank for an eligible model.
	pub fn model_rank(&self, provider: &str, model: &str) -> Option<usize> {
		self
			.model_allowed(provider, model)
			.then(|| self.provider_rank(provider))
	}

	/// Returns the stable routing rank in a resolved working-directory scope.
	pub fn model_rank_at(
		&self,
		cwd: &Path,
		home: &Path,
		provider: &str,
		model: &str,
	) -> Option<usize> {
		self
			.model_allowed_at(cwd, home, provider, model)
			.then(|| self.provider_rank(provider))
	}

	/// Resolves route family and provider-specific tier policy.
	pub fn service_tier_for_route(
		&self,
		provider: &str,
		model: Option<&str>,
		audience: TierAudience,
		parent: Option<&ServiceTier>,
	) -> Option<ServiceTier> {
		if provider.contains("fireworks") {
			return self.tier_fireworks.resolve(ProviderFamily::Other, parent);
		}
		self.service_tier(provider_family(provider, model), audience, parent)
	}

	/// Resolves a family/audience service tier into the concrete wire value.
	pub fn service_tier(
		&self,
		family: ProviderFamily,
		audience: TierAudience,
		parent: Option<&ServiceTier>,
	) -> Option<ServiceTier> {
		let audience_setting = match audience {
			TierAudience::Session => None,
			TierAudience::Subagent => Some(&self.tier_subagent),
			TierAudience::Advisor => Some(&self.tier_advisor),
		};
		if let Some(setting) = audience_setting
			&& !matches!(setting, TierSetting::Inherit)
		{
			return setting.resolve(family, parent);
		}
		let family_setting = match family {
			ProviderFamily::OpenAi => &self.tier_openai,
			ProviderFamily::Anthropic => &self.tier_anthropic,
			ProviderFamily::Google => &self.tier_google,
			ProviderFamily::Other => return None,
		};
		family_setting.resolve(family, parent)
	}

	/// Reports whether a concrete route satisfies configured wire preferences.
	pub fn wire_route_allowed(&self, provider: &str, codec: &str, transport: TransportKind) -> bool {
		let openai_route = provider.contains("openai") || provider.contains("codex");
		let websocket_allowed = !openai_route
			|| match self.openai_websockets {
				WireToggle::Auto => true,
				WireToggle::Off => transport != TransportKind::Websocket,
				WireToggle::On => transport == TransportKind::Websocket,
			};
		let kimi_route = provider.contains("kimi") || provider.contains("moonshot");
		let kimi_allowed = !kimi_route
			|| match self.kimi_api_format {
				KimiApiFormat::Auto => true,
				KimiApiFormat::OpenAi => codec.starts_with("openai-"),
				KimiApiFormat::Anthropic => codec == "anthropic",
			};
		websocket_allowed && kimi_allowed
	}

	/// Applies the configured OpenRouter suffix only when the model has no
	/// explicit variant.
	pub fn openrouter_wire_model(&self, provider: &str, model: &WireModelId<str>) -> WireModelId {
		if provider != "openrouter"
			|| self.openrouter_variant == OpenRouterVariant::Default
			|| model
				.rsplit('/')
				.next()
				.is_some_and(|tail| tail.contains(':'))
		{
			return model.to_owned();
		}
		Str::from(format!("{}:{}", model, <&'static str>::from(self.openrouter_variant))).into()
	}

	/// Selects the configured model for one harness-owned auxiliary purpose.
	pub const fn special_selector(&self, purpose: SpecialModelPurpose) -> &Str {
		match purpose {
			SpecialModelPurpose::Tiny => &self.tiny_selector,
			SpecialModelPurpose::Memory => &self.memory_selector,
			SpecialModelPurpose::AutoThinking => &self.auto_thinking_selector,
			SpecialModelPurpose::UnexpectedStop => &self.unexpected_stop_selector,
		}
	}

	/// Returns a bounded first-event timeout derived from provider settings.
	pub const fn plan_ttl(&self) -> Duration {
		Duration::from_secs(30)
	}
}

/// Harness-owned auxiliary model use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecialModelPurpose {
	/// Session titles and cheap transforms.
	Tiny,
	/// Memory extraction and consolidation.
	Memory,
	/// Automatic thinking classifier.
	AutoThinking,
	/// Unexpected-stop classifier.
	UnexpectedStop,
}

impl SettingsDomain for ModelSettings {
	const DOMAIN: &'static str = "model";
	const FIELDS: &'static [FieldDescriptor] = &[
		field("model.roles", "Model Roles", SettingKind::Table, 1),
		field(
			"model.role_storage",
			"Model Role Storage",
			SettingKind::Enum(&["global", "project"]),
			2,
		),
		field("model.tags", "Model Role Tags", SettingKind::Table, 3),
		field("model.cycle_order", "Model Cycle Order", SettingKind::Array, 4),
		field("model.enabled_models", "Enabled Models", SettingKind::Array, 5),
		field("model.disabled_providers", "Disabled Providers", SettingKind::Array, 6),
		field(
			"model.default_thinking",
			"Default Thinking",
			SettingKind::Enum(&["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
			10,
		),
		field(
			"model.thinking_ceiling",
			"Thinking Ceiling",
			SettingKind::Enum(&["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
			20,
		),
		field("model.thinking_budgets", "Thinking Budgets", SettingKind::Table, 30),
		field("model.provider_order", "Provider Priority", SettingKind::Array, 40),
		field(
			"model.tier_openai",
			"OpenAI Tier",
			SettingKind::Enum(&["none", "standard", "flex", "priority"]),
			50,
		),
		field(
			"model.tier_anthropic",
			"Anthropic Tier",
			SettingKind::Enum(&["none", "standard", "priority"]),
			60,
		),
		field(
			"model.tier_google",
			"Google Tier",
			SettingKind::Enum(&["none", "standard", "priority"]),
			70,
		),
		field(
			"model.tier_fireworks",
			"Fireworks Tier",
			SettingKind::Enum(&["none", "standard", "priority"]),
			80,
		),
		field(
			"model.tier_subagent",
			"Subagent Tier",
			SettingKind::Enum(&["none", "inherit", "standard", "flex", "priority"]),
			90,
		),
		field(
			"model.tier_advisor",
			"Advisor Tier",
			SettingKind::Enum(&["none", "inherit", "standard", "flex", "priority"]),
			100,
		),
		field(
			"model.cache_retention",
			"Cache Retention",
			SettingKind::Enum(&["auto", "none", "short", "long"]),
			100,
		),
		field(
			"model.openai_websockets",
			"OpenAI WebSockets",
			SettingKind::Enum(&["auto", "off", "on"]),
			110,
		),
		field(
			"model.openrouter_variant",
			"OpenRouter Variant",
			SettingKind::Enum(&["default", "nitro", "floor", "online", "exacto"]),
			120,
		),
		field(
			"model.kimi_api_format",
			"Kimi API Format",
			SettingKind::Enum(&["auto", "openai", "anthropic"]),
			130,
		),
		field("model.tiny_selector", "Tiny Model", SettingKind::String, 140),
		field("model.memory_selector", "Memory Model", SettingKind::String, 150),
		field("model.auto_thinking_selector", "Auto-Thinking Model", SettingKind::String, 160),
		field("model.unexpected_stop_selector", "Unexpected-Stop Model", SettingKind::String, 170),
	];

	fn validate(&self) -> Result<(), ValidationError> {
		let budgets = self.thinking_budgets;
		let ordered =
			[budgets.minimal, budgets.low, budgets.medium, budgets.high, budgets.xhigh, budgets.max];
		let selectors_valid = [
			&self.tiny_selector,
			&self.memory_selector,
			&self.auto_thinking_selector,
			&self.unexpected_stop_selector,
		]
		.into_iter()
		.all(|value| !value.trim().is_empty());
		let lists_valid = unique_nonempty(&self.provider_order)
			&& unique_nonempty(&self.cycle_order)
			&& scoped_entries_valid(&self.enabled_models, ScopedValueKind::Models)
			&& scoped_entries_valid(&self.disabled_providers, ScopedValueKind::Providers);
		let roles_valid = self
			.roles
			.iter()
			.all(|(role, selector)| !role.trim().is_empty() && !selector.trim().is_empty());
		let tags_valid = self
			.tags
			.iter()
			.all(|(role, tag)| !role.trim().is_empty() && !tag.name.trim().is_empty());
		if ordered.iter().all(|value| *value > 0)
			&& ordered.windows(2).all(|pair| pair[0] <= pair[1])
			&& selectors_valid
			&& lists_valid
			&& roles_valid
			&& tags_valid
		{
			Ok(())
		} else {
			Err(ValidationError::DomainInvariant { domain: Self::DOMAIN })
		}
	}
}

const fn field(
	path: &'static str,
	label: &'static str,
	kind: SettingKind,
	order: u16,
) -> FieldDescriptor {
	FieldDescriptor {
		path,
		label,
		description: "Runtime-owned model and provider policy.",
		kind,
		scopes: PERSISTED,
		order,
		options: None,
		condition: None,
		secret: false,
	}
}

impl OneOrManyStr {
	fn as_slice(&self) -> &[Str] {
		match self {
			Self::One(value) => std::slice::from_ref(value),
			Self::Many(values) => values,
		}
	}
}

#[derive(Clone, Copy)]
enum ScopedValueKind {
	Models,
	Providers,
}

fn scoped_values(source: &PathScopedStringValues, kind: ScopedValueKind) -> Vec<&Str> {
	match kind {
		ScopedValueKind::Models => source.models.iter(),
		ScopedValueKind::Providers => source.providers.iter(),
	}
	.chain(source.values.iter())
	.chain(source.items.iter())
	.flat_map(OneOrManyStr::as_slice)
	.collect()
}

fn scoped_prefixes(source: &PathScopedStringValues) -> impl Iterator<Item = &Str> {
	source
		.path
		.iter()
		.chain(source.paths.iter())
		.chain(source.path_prefix.iter())
		.chain(source.path_prefixes.iter())
		.flat_map(OneOrManyStr::as_slice)
}

fn resolve_path_scoped(
	entries: &[PathScopedStringEntry],
	cwd: &Path,
	home: &Path,
	kind: ScopedValueKind,
) -> ArcStrList {
	let cwd = normalize_path(cwd, cwd, home);
	let mut resolved = Vec::new();
	for entry in entries {
		match entry {
			PathScopedStringEntry::Bare(value) => resolved.push(value.clone()),
			PathScopedStringEntry::Scoped(source)
				if scoped_prefixes(source).any(|prefix| {
					cwd.starts_with(normalize_path(Path::new(prefix.as_str()), &cwd, home))
				}) =>
			{
				resolved.extend(scoped_values(source, kind).into_iter().cloned());
			},
			PathScopedStringEntry::Scoped(_) => {},
		}
	}
	resolved.into()
}

fn normalize_path(path: &Path, cwd: &Path, home: &Path) -> PathBuf {
	let expanded = path.to_str().map_or_else(
		|| path.to_owned(),
		|text| {
			if text == "~" {
				home.to_owned()
			} else if let Some(relative) = text.strip_prefix("~/") {
				home.join(relative)
			} else if path.is_absolute() {
				path.to_owned()
			} else {
				cwd.join(path)
			}
		},
	);
	let mut normalized = PathBuf::new();
	for component in expanded.components() {
		match component {
			Component::CurDir => {},
			Component::ParentDir => {
				normalized.pop();
			},
			Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
			Component::RootDir => normalized.push(Path::new("/")),
			Component::Normal(value) => normalized.push(value),
		}
	}
	normalized
}

fn model_matches<'a>(patterns: impl Iterator<Item = &'a Str>, provider: &str, model: &str) -> bool {
	let mut configured = false;
	let mut matched = false;
	for pattern in patterns {
		configured = true;
		matched |= model_pattern_matches(pattern, provider, model);
	}
	!configured || matched
}

/// Reports whether one configured model-scope pattern matches a provider and
/// model identity.
///
/// Matching is ASCII case-insensitive and supports `*`, `?`, and glob character
/// classes. A valid trailing thinking effort is ignored for admission, except
/// when the complete pattern exactly names a colon-bearing model id.
pub fn model_pattern_matches(pattern: &str, provider: &str, model: &str) -> bool {
	let logical_id = model
		.split_once('/')
		.map_or(model, |(_, logical_id)| logical_id);
	if exact_pattern_matches(pattern, provider, logical_id) {
		return true;
	}
	let pattern = pattern
		.rsplit_once(':')
		.filter(|(_, suffix)| suffix.parse::<ThinkingEffort>().is_ok())
		.map_or(pattern, |(pattern, _)| pattern);
	pattern.split_once('/').map_or_else(
		|| glob_matches(pattern.as_bytes(), logical_id.as_bytes()),
		|(provider_pattern, model_pattern)| {
			glob_matches(provider_pattern.as_bytes(), provider.as_bytes())
				&& glob_matches(model_pattern.as_bytes(), logical_id.as_bytes())
		},
	)
}

fn exact_pattern_matches(pattern: &str, provider: &str, model: &str) -> bool {
	pattern.split_once('/').map_or_else(
		|| pattern.eq_ignore_ascii_case(model),
		|(pattern_provider, pattern_model)| {
			pattern_provider.eq_ignore_ascii_case(provider)
				&& pattern_model.eq_ignore_ascii_case(model)
		},
	)
}

fn scoped_entries_valid(entries: &[PathScopedStringEntry], kind: ScopedValueKind) -> bool {
	entries.iter().all(|entry| match entry {
		PathScopedStringEntry::Bare(value) => !value.trim().is_empty(),
		PathScopedStringEntry::Scoped(source) => {
			let prefixes = scoped_prefixes(source).collect::<Vec<_>>();
			let values = scoped_values(source, kind);
			!prefixes.is_empty()
				&& !values.is_empty()
				&& prefixes.iter().all(|value| !value.trim().is_empty())
				&& values.iter().all(|value| !value.trim().is_empty())
		},
	})
}

fn unique_nonempty(values: &[Str]) -> bool {
	values.iter().enumerate().all(|(index, value)| {
		!value.trim().is_empty() && values[..index].iter().all(|prior| prior != value)
	})
}

fn glob_matches(pattern: &[u8], value: &[u8]) -> bool {
	let (mut pattern_index, mut value_index) = (0, 0);
	let (mut star, mut retry_value) = (None, 0);
	while value_index < value.len() {
		if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
			star = Some(pattern_index);
			pattern_index += 1;
			retry_value = value_index;
			continue;
		}
		let token = glob_token_matches(pattern, pattern_index, value[value_index]);
		if let Some((true, next_pattern)) = token {
			pattern_index = next_pattern;
			value_index += 1;
		} else if let Some(star_index) = star {
			retry_value += 1;
			value_index = retry_value;
			pattern_index = star_index + 1;
		} else {
			return false;
		}
	}
	while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
		pattern_index += 1;
	}
	pattern_index == pattern.len()
}

fn glob_token_matches(pattern: &[u8], index: usize, value: u8) -> Option<(bool, usize)> {
	let token = *pattern.get(index)?;
	if token == b'?' {
		return Some((true, index + 1));
	}
	if token != b'[' {
		return Some((token.eq_ignore_ascii_case(&value), index + 1));
	}
	character_class_matches(pattern, index, value)
		.or(Some((b'['.eq_ignore_ascii_case(&value), index + 1)))
}

fn character_class_matches(pattern: &[u8], start: usize, value: u8) -> Option<(bool, usize)> {
	let mut index = start + 1;
	let negated = matches!(pattern.get(index), Some(b'!' | b'^'));
	index += usize::from(negated);
	let mut matched = false;
	let mut populated = false;
	if pattern.get(index) == Some(&b']') {
		matched = value == b']';
		populated = true;
		index += 1;
	}
	while let Some(&current) = pattern.get(index) {
		if current == b']' && populated {
			return Some(((matched && !negated) || (!matched && negated), index + 1));
		}
		populated = true;
		if pattern.get(index + 1) == Some(&b'-')
			&& let Some(&end) = pattern.get(index + 2)
			&& end != b']'
		{
			let value = value.to_ascii_lowercase();
			let first = current.to_ascii_lowercase();
			let last = end.to_ascii_lowercase();
			matched |= first.min(last) <= value && value <= first.max(last);
			index += 3;
		} else {
			matched |= current.eq_ignore_ascii_case(&value);
			index += 1;
		}
	}
	None
}

omp_settings::inventory::submit! { DomainRegistration::of::<ModelSettings>() }

/// Resolves provider family from canonical route and model identities.
pub fn provider_family(provider: &str, model: Option<&str>) -> ProviderFamily {
	let model = model.unwrap_or_default();
	if provider.contains("anthropic")
		|| provider.contains("claude")
		|| model.contains("anthropic/")
		|| model.contains("claude")
	{
		ProviderFamily::Anthropic
	} else if provider.contains("google")
		|| provider.contains("gemini")
		|| model.contains("google/")
		|| model.contains("gemini")
	{
		ProviderFamily::Google
	} else if provider.contains("openai")
		|| provider == "openrouter"
		|| provider == "azure"
		|| model.contains("openai/")
	{
		ProviderFamily::OpenAi
	} else {
		ProviderFamily::Other
	}
}

/// Exact configured model fallback chains keyed by model id or `provider/*`.
pub type FallbackChains = BTreeMap<Str, Vec<Str>>;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn role_metadata_and_canonical_paths_round_trip() {
		let mut settings = ModelSettings::default();
		settings
			.roles
			.insert(Str::new_static("smol"), Str::new_static("openai/gpt-5-mini"));
		settings.tags.insert(Str::new_static("smol"), ModelTag {
			name:   Str::new_static("Small"),
			color:  Some(Str::new_static("cyan")),
			hidden: false,
		});
		settings.role_storage = ModelRoleStorage::Project;
		assert_eq!(settings.role_selector("smol").map(Str::as_str), Some("openai/gpt-5-mini"));
		assert_eq!(settings.role_tag("smol").map(|tag| tag.name.as_str()), Some("Small"));
		assert_eq!(settings.cycle_rank("smol"), 0);
		assert_eq!(settings.cycle_rank("other"), usize::MAX);
		let encoded = serde_json::to_value(&settings).expect("settings serialize");
		let decoded: ModelSettings = serde_json::from_value(encoded).expect("settings deserialize");
		assert_eq!(decoded, settings);
		for path in [
			"model.roles",
			"model.role_storage",
			"model.tags",
			"model.cycle_order",
			"model.enabled_models",
			"model.disabled_providers",
		] {
			assert!(ModelSettings::FIELDS.iter().any(|field| field.path == path), "{path}");
		}
	}

	#[test]
	fn model_scope_filters_before_provider_ranking() {
		let mut settings = ModelSettings::default();
		settings.provider_order =
			sync::Arc::from([Str::new_static("openai"), Str::new_static("anthropic")]);
		settings.enabled_models = sync::Arc::from([
			PathScopedStringEntry::Bare(Str::new_static("openai/gpt-5.*")),
			PathScopedStringEntry::Bare(Str::new_static("claude-*")),
		]);
		settings.disabled_providers =
			sync::Arc::from([PathScopedStringEntry::Bare(Str::new_static("anthropic"))]);
		assert_eq!(settings.model_rank("openai", "gpt-5.6"), Some(0));
		assert_eq!(settings.model_rank("openai", "openai/gpt-5.6"), Some(0));
		assert_eq!(settings.model_rank("openai", "gpt-4.1"), None);
		assert_eq!(settings.model_rank("anthropic", "claude-opus-4-6"), None);
		assert!(settings.model_allowed("openrouter", "claude-sonnet-4-6"));
		assert!(model_pattern_matches("OPENAI/GPT-5.[4-7]:HIGH", "openai", "gpt-5.6"));
		assert!(model_pattern_matches("openrouter/model:exacto", "OPENROUTER", "MODEL:EXACTO"));
		assert!(!model_pattern_matches("openai/gpt-5.[!4-7]", "openai", "gpt-5.6"));
		assert!(settings.validate().is_ok());
		settings.cycle_order =
			sync::Arc::from([Str::new_static("default"), Str::new_static("default")]);
		assert!(settings.validate().is_err());
	}

	#[test]
	fn persisted_default_extends_only_an_existing_scope() {
		let mut settings = ModelSettings::default();
		assert!(!settings.insert_persisted_default("openai/gpt-5.6"));
		settings.enabled_models =
			sync::Arc::from([PathScopedStringEntry::Bare(Str::new_static("anthropic/*"))]);
		assert!(settings.insert_persisted_default("openai/gpt-5.6"));
		assert!(!settings.insert_persisted_default("OPENAI/GPT-5.6"));
		assert_eq!(settings.enabled_models.len(), 2);
		assert!(matches!(
			settings.enabled_models.last(),
			Some(PathScopedStringEntry::Bare(value)) if value == "openai/gpt-5.6"
		));
	}

	#[test]
	fn mixed_path_scoped_lists_resolve_against_exact_cwd_and_home() {
		let settings: ModelSettings = serde_json::from_value(serde_json::json!({
			"enabled_models": [
				"openai/gpt-5.*",
				{
					"pathPrefix": "/work/project",
					"models": ["anthropic/claude-*"],
					"items": "openrouter/*"
				},
				{
					"paths": ["~/private"],
					"values": "google/gemini-*"
				}
			],
			"disabled_providers": [
				"legacy",
				{
					"pathPrefixes": ["/work/project", "/other"],
					"providers": ["anthropic"]
				}
			]
		}))
		.expect("mixed scoped settings");
		let cwd = Path::new("/work/project/subdir");
		let home = Path::new("/Users/test");
		assert_eq!(settings.resolved_enabled_models(cwd, home).as_ref(), &[
			Str::new_static("openai/gpt-5.*"),
			Str::new_static("anthropic/claude-*"),
			Str::new_static("openrouter/*"),
		]);
		assert_eq!(settings.resolved_disabled_providers(cwd, home).as_ref(), &[
			Str::new_static("legacy"),
			Str::new_static("anthropic")
		]);
		assert!(settings.model_allowed_at(cwd, home, "openai", "gpt-5.6"));
		assert!(!settings.model_allowed_at(cwd, home, "anthropic", "claude-opus-4-6"));
		let frozen = settings.resolve_path_scopes(cwd, home);
		assert!(
			frozen
				.enabled_models
				.iter()
				.all(|entry| matches!(entry, PathScopedStringEntry::Bare(_)))
		);
		assert!(frozen.model_allowed("openai", "gpt-5.6"));
		assert!(!frozen.model_allowed("anthropic", "claude-opus-4-6"));
		assert_eq!(
			settings
				.resolved_enabled_models(Path::new("/Users/test/private/repo"), home)
				.as_ref(),
			&[Str::new_static("openai/gpt-5.*"), Str::new_static("google/gemini-*")]
		);
	}
}
