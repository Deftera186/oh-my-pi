//! Runtime-backed inference retry, fallback, sampling, admission, and timeout
//! settings.

#![allow(missing_docs, reason = "strum IntoStaticStr emits undocumented inherent methods")]

use std::{collections::BTreeMap, sync, sync::LazyLock, time::Duration};

use omp_catalog::{
	ModelKey, ProviderId,
	settings::{CacheRetentionSetting, FallbackChains},
};
use omp_core::Str;
use omp_settings::{
	DomainRegistration, FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

use crate::{
	Call,
	call::{CacheRetention, ChatRequest, OperationCall, Setting, TextVerbosity},
	layer::retry::RetryBackoff,
	receipt::ExecutionBudget,
};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

/// Behavior after a fallback route succeeds.
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
#[strum(serialize_all = "kebab-case", ascii_case_insensitive, const_into_str)]
pub enum FallbackRevertPolicy {
	/// Retry the primary after its suppression window expires.
	#[default]
	CooldownExpiry,
	/// Keep the fallback until the caller explicitly changes selection.
	Never,
}

/// Policy when every metered account is inside the configured usage reserve.
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
#[strum(serialize_all = "kebab-case", ascii_case_insensitive, const_into_str)]
pub enum UsageReservePolicy {
	/// Interactive callers confirm; unattended callers use fallback.
	#[default]
	Confirm,
	/// Automatically use an eligible fallback.
	Auto,
	/// Refuse to spend the reserve and do not fall back.
	FailClosed,
}

/// Replay-safe retry and explicitly authorized fallback policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct RetrySettings {
	/// Enables transport and model fallback recovery.
	pub enabled:              bool,
	/// Maximum retries after the first attempt.
	pub max_retries:          u32,
	/// First exponential retry ceiling in milliseconds.
	pub base_delay_ms:        u64,
	/// Largest accepted retry delay in milliseconds; `0` disables the cap.
	pub max_delay_ms:         u64,
	/// Enables model fallback candidates.
	pub model_fallback:       bool,
	/// Enables quota-aware preflight fallback.
	pub usage_aware_fallback: bool,
	/// Remaining quota percentage held in reserve.
	pub usage_reserve_pct:    u8,
	/// Action when every account is inside the reserve.
	pub usage_reserve_policy: UsageReservePolicy,
	/// Exact model/provider fallback chains.
	pub fallback_chains:      FallbackChains,
	/// Primary reversion behavior after fallback.
	pub fallback_revert:      FallbackRevertPolicy,
	/// Enables the explicit Anthropic server-side safety fallback header.
	pub server_side_fallback: bool,
}

impl Default for RetrySettings {
	fn default() -> Self {
		Self {
			enabled:              true,
			max_retries:          10,
			base_delay_ms:        500,
			max_delay_ms:         300_000,
			model_fallback:       true,
			usage_aware_fallback: false,
			usage_reserve_pct:    10,
			usage_reserve_policy: UsageReservePolicy::Confirm,
			fallback_chains:      BTreeMap::new(),
			fallback_revert:      FallbackRevertPolicy::CooldownExpiry,
			server_side_fallback: false,
		}
	}
}

static ACTIVE_FALLBACKS: LazyLock<Mutex<BTreeMap<ModelKey, ModelKey>>> =
	LazyLock::new(Default::default);

pub(crate) fn record_fallback(primary: &ModelKey<str>, fallback: &ModelKey<str>) {
	ACTIVE_FALLBACKS
		.lock()
		.insert(primary.to_owned(), fallback.to_owned());
}

pub(crate) fn active_fallback(primary: &ModelKey<str>) -> Option<ModelKey> {
	ACTIVE_FALLBACKS.lock().get(primary).cloned()
}

impl RetrySettings {
	/// Returns the total attempt bound installed on calls that retain defaults.
	pub const fn max_attempts(&self) -> u32 {
		if self.enabled {
			self.max_retries.saturating_add(1)
		} else {
			1
		}
	}

	/// Returns the retry middleware policy.
	pub const fn backoff(&self) -> RetryBackoff {
		RetryBackoff {
			base:    Duration::from_millis(self.base_delay_ms),
			maximum: Duration::from_millis(self.max_delay_ms),
		}
	}

	/// Applies retry defaults without weakening tighter caller limits.
	pub fn apply_budget(&self, budget: &mut ExecutionBudget) {
		let configured = self.max_attempts();
		budget.max_attempts = if budget.max_attempts == ExecutionBudget::default().max_attempts {
			configured
		} else {
			budget.max_attempts.min(configured).max(1)
		};
	}

	/// Resolves the configured chain for an exact model, then its provider
	/// wildcard.
	pub fn fallback_selectors<'a>(
		&'a self,
		model: &ModelKey<str>,
		provider: Option<&ProviderId<str>>,
	) -> impl Iterator<Item = &'a Str> + 'a {
		let exact = self
			.fallback_chains
			.get(model.as_str())
			.into_iter()
			.flatten();
		let wildcard = provider
			.and_then(|provider| {
				self
					.fallback_chains
					.get(&Str::from(format!("{}/*", provider)))
			})
			.into_iter()
			.flatten();
		exact.chain(wildcard)
	}

	/// Expands the configured chain and then the chain owned by its last
	/// reachable fallback.
	///
	/// The walk is bounded by the caller's remaining attempt budget and keeps
	/// the first occurrence of each model. This makes a fallback that is itself
	/// a chain key reachable without allowing cyclic chains to grow forever.
	pub fn fallback_walk(
		&self,
		primary: &ModelKey<str>,
		primary_provider: Option<&ProviderId<str>>,
		max_fallbacks: usize,
		mut provider_for: impl FnMut(&ModelKey<str>) -> Option<ProviderId>,
	) -> Vec<ModelKey> {
		let mut selected = Vec::new();
		let mut current = primary.to_owned();
		let mut provider = primary_provider.map(ToOwned::to_owned);
		while selected.len() < max_fallbacks {
			let remaining = max_fallbacks - selected.len();
			let next = self
				.fallback_selectors(&current, provider.as_deref())
				.map(|selector| ModelKey::from(selector.clone()))
				.filter(|candidate| candidate != primary && !selected.contains(candidate))
				.filter_map(|candidate| provider_for(&candidate).map(|provider| (candidate, provider)))
				.take(remaining)
				.collect::<Vec<_>>();
			let Some((last, last_provider)) = next.last().cloned() else {
				break;
			};
			selected.extend(next.into_iter().map(|(candidate, _)| candidate));
			current = last;
			provider = Some(last_provider);
		}
		selected
	}
}

impl SettingsDomain for RetrySettings {
	const DOMAIN: &'static str = "retry";
	const FIELDS: &'static [FieldDescriptor] = &[
		field("retry.enabled", "Retry Enabled", SettingKind::Boolean, 10),
		field("retry.max_retries", "Maximum Retries", SettingKind::Integer, 20),
		field("retry.base_delay_ms", "Base Retry Delay", SettingKind::Integer, 30),
		FieldDescriptor {
			path:        "retry.max_delay_ms",
			label:       "Maximum Retry Delay",
			description: "Largest retry wait in milliseconds; 0 disables the cap.",
			kind:        SettingKind::Integer,
			scopes:      PERSISTED,
			order:       40,
			options:     None,
			condition:   None,
			secret:      false,
		},
		field("retry.model_fallback", "Model Fallback", SettingKind::Boolean, 50),
		field("retry.usage_aware_fallback", "Usage-Aware Fallback", SettingKind::Boolean, 60),
		field("retry.usage_reserve_pct", "Usage Reserve", SettingKind::Integer, 70),
		field(
			"retry.usage_reserve_policy",
			"Usage Reserve Policy",
			SettingKind::Enum(&["confirm", "auto", "fail-closed"]),
			80,
		),
		field("retry.fallback_chains", "Fallback Chains", SettingKind::Table, 90),
		field(
			"retry.fallback_revert",
			"Fallback Revert",
			SettingKind::Enum(&["cooldown-expiry", "never"]),
			100,
		),
		field("retry.server_side_fallback", "Server-Side Fallback", SettingKind::Boolean, 110),
	];

	fn validate(&self) -> Result<(), ValidationError> {
		let chains_valid = self.fallback_chains.iter().all(|(key, values)| {
			!key.is_empty()
				&& !values.is_empty()
				&& values.iter().enumerate().all(|(index, value)| {
					!value.is_empty() && values[..index].iter().all(|prior| prior != value)
				})
		});
		if self.max_retries <= 100
			&& (self.max_delay_ms == 0 || self.base_delay_ms <= self.max_delay_ms)
			&& self.max_delay_ms <= 3_600_000
			&& self.usage_reserve_pct <= 100
			&& chains_valid
		{
			Ok(())
		} else {
			Err(ValidationError::DomainInvariant { domain: Self::DOMAIN })
		}
	}
}

/// Defaults for chat sampling and output shaping.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct SamplingSettings {
	/// Temperature; negative preserves the provider default.
	pub temperature:        f32,
	/// Nucleus cutoff; negative preserves the provider default.
	pub top_p:              f32,
	/// Top-k bound; negative preserves the provider default.
	pub top_k:              i32,
	/// Minimum probability cutoff; negative preserves the provider default.
	pub min_p:              f32,
	/// Presence penalty; negative preserves the provider default.
	pub presence_penalty:   f32,
	/// Frequency penalty; negative preserves the provider default.
	pub frequency_penalty:  f32,
	/// Repetition penalty; negative preserves the provider default.
	pub repetition_penalty: f32,
	/// Default response verbosity.
	pub verbosity:          TextVerbositySetting,
}

impl Default for SamplingSettings {
	fn default() -> Self {
		Self {
			temperature:        -1.0,
			top_p:              -1.0,
			top_k:              -1,
			min_p:              -1.0,
			presence_penalty:   -1.0,
			frequency_penalty:  -1.0,
			repetition_penalty: -1.0,
			verbosity:          TextVerbositySetting::Medium,
		}
	}
}

/// Configured default response verbosity.
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
pub enum TextVerbositySetting {
	/// Concise output.
	Low,
	/// Balanced output.
	#[default]
	Medium,
	/// Detailed output.
	High,
}

impl SamplingSettings {
	/// Installs defaults on a chat request while preserving every
	/// caller-explicit value.
	pub fn apply(
		&self,
		request: &mut ChatRequest,
		top_k: bool,
		penalties: bool,
		extended: bool,
		verbosity: bool,
	) {
		request.sampling.temperature = request
			.sampling
			.temperature
			.or_else(|| nonnegative(self.temperature));
		request.sampling.top_p = request.sampling.top_p.or_else(|| nonnegative(self.top_p));
		if top_k {
			request.sampling.top_k = request
				.sampling
				.top_k
				.or_else(|| u32::try_from(self.top_k).ok());
		}
		if extended {
			request.sampling.min_p = request.sampling.min_p.or_else(|| nonnegative(self.min_p));
			request.sampling.repetition_penalty = request
				.sampling
				.repetition_penalty
				.or_else(|| nonnegative(self.repetition_penalty));
		}
		if penalties {
			request.sampling.presence_penalty = request
				.sampling
				.presence_penalty
				.or_else(|| nonnegative(self.presence_penalty));
			request.sampling.frequency_penalty = request
				.sampling
				.frequency_penalty
				.or_else(|| nonnegative(self.frequency_penalty));
		}
		if verbosity && matches!(request.verbosity, Setting::Unset) {
			request.verbosity = Setting::Prefer(match self.verbosity {
				TextVerbositySetting::Low => TextVerbosity::Low,
				TextVerbositySetting::Medium => TextVerbosity::Medium,
				TextVerbositySetting::High => TextVerbosity::High,
			});
		}
	}
}

impl SettingsDomain for SamplingSettings {
	const DOMAIN: &'static str = "sampling";
	const FIELDS: &'static [FieldDescriptor] = &[
		field("sampling.temperature", "Temperature", SettingKind::Number, 10),
		field("sampling.top_p", "Top P", SettingKind::Number, 20),
		field("sampling.top_k", "Top K", SettingKind::Integer, 30),
		field("sampling.min_p", "Min P", SettingKind::Number, 40),
		field("sampling.presence_penalty", "Presence Penalty", SettingKind::Number, 50),
		field("sampling.frequency_penalty", "Frequency Penalty", SettingKind::Number, 60),
		field("sampling.repetition_penalty", "Repetition Penalty", SettingKind::Number, 70),
		field(
			"sampling.verbosity",
			"Text Verbosity",
			SettingKind::Enum(&["low", "medium", "high"]),
			80,
		),
	];

	fn validate(&self) -> Result<(), ValidationError> {
		let probability = |value: f32| value == -1.0 || (0.0..=1.0).contains(&value);
		let finite =
			[self.temperature, self.presence_penalty, self.frequency_penalty, self.repetition_penalty]
				.into_iter()
				.all(f32::is_finite);
		if finite
			&& self.temperature >= -1.0
			&& probability(self.top_p)
			&& probability(self.min_p)
			&& self.top_k >= -1
		{
			Ok(())
		} else {
			Err(ValidationError::DomainInvariant { domain: Self::DOMAIN })
		}
	}
}

/// Provider admission and request timeout policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ProviderRuntimeSettings {
	/// Maximum concurrent requests keyed by provider id; absent or zero is
	/// unlimited.
	pub max_in_flight:        BTreeMap<Str, usize>,
	/// Maximum queued callers per provider before backpressure fails fast.
	pub max_queued:           usize,
	/// Per-transport-attempt timeout in seconds.
	pub timeout_seconds:      u64,
	/// Overall logical-call timeout in seconds; zero leaves caller deadlines
	/// authoritative.
	pub call_timeout_seconds: u64,
	/// Bedrock guardrail policy keyed by provider id.
	pub bedrock_guardrails:   BTreeMap<Str, crate::codec::bedrock::BedrockGuardrail>,
}

impl Default for ProviderRuntimeSettings {
	fn default() -> Self {
		Self {
			max_in_flight:        BTreeMap::new(),
			max_queued:           64,
			timeout_seconds:      300,
			call_timeout_seconds: 0,
			bedrock_guardrails:   BTreeMap::new(),
		}
	}
}

impl ProviderRuntimeSettings {
	/// Resolves a provider concurrency limit; zero and absent entries are
	/// unlimited.
	pub fn in_flight_limit(&self, provider: &ProviderId<str>) -> Option<usize> {
		self
			.max_in_flight
			.get(provider.as_str())
			.copied()
			.filter(|limit| *limit > 0)
	}

	/// Applies the configured logical timeout without weakening a tighter caller
	/// timeout.
	pub fn apply_budget(&self, budget: &mut ExecutionBudget) {
		if self.call_timeout_seconds == 0 {
			return;
		}
		let configured = Duration::from_secs(self.call_timeout_seconds);
		budget.max_elapsed = Some(
			budget
				.max_elapsed
				.map_or(configured, |current| current.min(configured)),
		);
	}
}

impl SettingsDomain for ProviderRuntimeSettings {
	const DOMAIN: &'static str = "provider_runtime";
	const FIELDS: &'static [FieldDescriptor] = &[
		field("provider_runtime.max_in_flight", "Maximum In-Flight Requests", SettingKind::Table, 10),
		field("provider_runtime.max_queued", "Maximum Queued Requests", SettingKind::Integer, 20),
		field("provider_runtime.timeout_seconds", "Transport Timeout", SettingKind::Integer, 30),
		field("provider_runtime.call_timeout_seconds", "Call Timeout", SettingKind::Integer, 40),
		field("provider_runtime.bedrock_guardrails", "Bedrock Guardrails", SettingKind::Table, 50),
	];

	fn validate(&self) -> Result<(), ValidationError> {
		if self.max_queued <= 100_000
			&& self.timeout_seconds > 0
			&& self.timeout_seconds <= 3_600
			&& self.call_timeout_seconds <= 86_400
			&& self
				.max_in_flight
				.iter()
				.all(|(provider, limit)| !provider.is_empty() && *limit <= 100_000)
			&& self.bedrock_guardrails.iter().all(|(provider, guardrail)| {
				!provider.trim().is_empty()
					&& !guardrail.identifier.trim().is_empty()
					&& !guardrail.version.trim().is_empty()
			}) {
			Ok(())
		} else {
			Err(ValidationError::DomainInvariant { domain: Self::DOMAIN })
		}
	}
}

/// Immutable projection installed into constructed inference services.
#[derive(Clone, Debug, Default)]
pub struct InferenceSettings {
	/// Retry and fallback policy.
	pub retry:     RetrySettings,
	/// Chat sampling defaults.
	pub sampling:  SamplingSettings,
	/// Provider admission and timeout policy.
	pub providers: ProviderRuntimeSettings,
	/// Catalog/model policy.
	pub model:     omp_catalog::settings::ModelSettings,
}

impl InferenceSettings {
	/// Applies budget projections before side-effect-free planning.
	pub fn apply_planning_call(&self, call: &mut Call) {
		self.retry.apply_budget(&mut call.budget);
		self.providers.apply_budget(&mut call.budget);
	}

	/// Applies request-level projections after the immutable plan is selected.
	pub fn apply_call(&self, call: &mut Call) {
		let codec = call
			.execution
			.as_ref()
			.map(|execution| execution.codec.as_str());
		let service_tier = call.execution.as_ref().and_then(|execution| {
			self.model.service_tier_for_route(
				execution.provider.as_str(),
				execution.model.as_ref().map(|model| model.as_str()),
				omp_catalog::TierAudience::Session,
				None,
			)
		});
		if let OperationCall::Chat(chat) = &mut call.operation {
			let chat = sync::Arc::make_mut(chat);
			let openai_chat = codec == Some("openai-chat");
			let openai_responses = codec == Some("openai-responses");
			let top_k =
				openai_chat || matches!(codec, Some("anthropic" | "gemini" | "ollama" | "devin"));
			let penalties = openai_chat || openai_responses;
			self
				.sampling
				.apply(chat, top_k, penalties, openai_chat, openai_responses);
			if matches!(chat.cache_retention, Setting::Unset) {
				chat.cache_retention = match self.model.cache_retention {
					CacheRetentionSetting::Auto => Setting::Unset,
					CacheRetentionSetting::None => Setting::Require(CacheRetention::Request),
					CacheRetentionSetting::Short => Setting::Prefer(CacheRetention::Short),
					CacheRetentionSetting::Long => Setting::Prefer(CacheRetention::Long),
				};
			}
			if matches!(chat.service_tier, Setting::Unset)
				&& let Some(tier) = service_tier
			{
				chat.service_tier = Setting::Prefer(tier);
			}
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
		description: "Runtime-owned inference policy.",
		kind,
		scopes: PERSISTED,
		order,
		options: None,
		condition: None,
		secret: false,
	}
}

fn nonnegative(value: f32) -> Option<f32> {
	(value >= 0.0).then_some(value)
}

/// Settings domains owned by the inference crate.
pub const SETTINGS_CONTRIBUTION: omp_settings::SettingsContribution =
	omp_settings::SettingsContribution {
		domains:     &[
			DomainRegistration::of::<RetrySettings>(),
			DomainRegistration::of::<SamplingSettings>(),
			DomainRegistration::of::<ProviderRuntimeSettings>(),
			DomainRegistration::of::<crate::search_settings::WebSearchSettings>(),
		],
		normalizers: &[],
	};

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn zero_max_retry_delay_is_a_valid_uncapped_sentinel() {
		let settings =
			RetrySettings { base_delay_ms: 500, max_delay_ms: 0, ..RetrySettings::default() };
		assert!(settings.validate().is_ok());
		assert_eq!(settings.backoff().maximum, Duration::ZERO);
	}
	#[test]
	fn planning_projects_retry_budget_onto_the_real_call_once() {
		let mut call = Call::new(
			crate::call::CallMeta {
				id:             crate::id::RequestId::from("settings-budget"),
				target:         crate::call::Target::ProviderService(ProviderId::from("provider")),
				deadline:       None,
				budget:         ExecutionBudget::default(),
				session:        None,
				response_hooks: Default::default(),
			},
			OperationCall::Auth(sync::Arc::new(crate::call::AuthRequest::ListAccounts {
				provider: None,
			})),
		);
		let settings = InferenceSettings::default();
		settings.apply_planning_call(&mut call);
		assert_eq!(call.budget.max_attempts, settings.retry.max_attempts());
		let planned_budget = call.budget.clone();
		settings.apply_call(&mut call);
		assert_eq!(call.budget, planned_budget, "late request projection cannot mutate budget");
	}

	#[test]
	fn fallback_walk_reaches_chain_owned_by_last_fallback_within_budget() {
		let settings = RetrySettings {
			fallback_chains: BTreeMap::from([
				(Str::new_static("provider/a"), vec![Str::new_static("provider/b")]),
				(Str::new_static("provider/b"), vec![Str::new_static("provider/c")]),
			]),
			..RetrySettings::default()
		};
		let walked = settings.fallback_walk(
			ModelKey::from_ref("provider/a"),
			Some(ProviderId::from_ref("provider")),
			2,
			|model| {
				matches!(model.as_str(), "provider/a" | "provider/b" | "provider/c")
					.then(|| ProviderId::from("provider"))
			},
		);
		assert_eq!(walked, [ModelKey::from("provider/b"), ModelKey::from("provider/c"),]);
	}

	#[test]
	fn fallback_walk_deduplicates_cycles_and_obeys_attempt_bound() {
		let settings = RetrySettings {
			fallback_chains: BTreeMap::from([
				(Str::new_static("provider/a"), vec![Str::new_static("provider/b")]),
				(Str::new_static("provider/b"), vec![Str::new_static("provider/a")]),
			]),
			..RetrySettings::default()
		};
		assert_eq!(
			settings.fallback_walk(
				ModelKey::from_ref("provider/a"),
				Some(ProviderId::from_ref("provider")),
				10,
				|_| Some(ProviderId::from("provider")),
			),
			[ModelKey::from("provider/b")]
		);
	}
}
