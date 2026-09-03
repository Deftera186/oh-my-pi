//! Runtime-backed inference retry, fallback, sampling, admission, and timeout
//! settings.

#![allow(missing_docs, reason = "strum IntoStaticStr emits undocumented inherent methods")]

use std::{collections::BTreeMap, sync, sync::LazyLock, time::Duration};

use omp_catalog::{
	ModelKey, ProviderId,
	settings::{CacheRetentionSetting, FallbackChains},
};
use omp_con::{Ctx, Kv, Value};
use omp_core::Str;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use strum::{Display, EnumString, IntoStaticStr, VariantNames};

use crate::{
	Call,
	call::{CacheRetention, ChatRequest, OperationCall, Setting, TextVerbosity},
	layer::retry::RetryBackoff,
	receipt::ExecutionBudget,
};

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
	VariantNames,
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
	VariantNames,
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

impl RetrySettings {
	/// Projects retry and fallback policy from the control plane.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self {
			enabled:              AI_RETRY_ENABLED.get(ctx),
			max_retries:          AI_RETRY_MAX_RETRIES.get(ctx),
			base_delay_ms:        u64::from(AI_RETRY_BASE_DELAY_MS.get(ctx)),
			max_delay_ms:         u64::from(AI_RETRY_MAX_DELAY_MS.get(ctx)),
			model_fallback:       AI_RETRY_MODEL_FALLBACK.get(ctx),
			usage_aware_fallback: AI_RETRY_USAGE_AWARE_FALLBACK.get(ctx),
			usage_reserve_pct:    AI_RETRY_USAGE_RESERVE_PCT.get(ctx),
			usage_reserve_policy: AI_RETRY_USAGE_RESERVE_POLICY.get(ctx),
			fallback_chains:      deserialize_table(AI_RETRY_FALLBACK_CHAINS.get(ctx)),
			fallback_revert:      AI_RETRY_FALLBACK_REVERT.get(ctx),
			server_side_fallback: AI_RETRY_SERVER_SIDE_FALLBACK.get(ctx),
		}
	}

	/// Reports whether all cross-variable retry invariants hold.
	#[must_use]
	pub fn validate(&self) -> bool {
		let chains_valid = self.fallback_chains.iter().all(|(key, values)| {
			!key.is_empty()
				&& !values.is_empty()
				&& values.iter().enumerate().all(|(index, value)| {
					!value.is_empty() && values[..index].iter().all(|prior| prior != value)
				})
		});
		self.max_retries <= 100
			&& (self.max_delay_ms == 0 || self.base_delay_ms <= self.max_delay_ms)
			&& self.max_delay_ms <= 3_600_000
			&& self.usage_reserve_pct <= 100
			&& chains_valid
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
	VariantNames,
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

omp_con::con_enum!(FallbackRevertPolicy);
omp_con::con_enum!(UsageReservePolicy);
omp_con::con_enum!(TextVerbositySetting);

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

impl SamplingSettings {
	/// Projects sampling defaults from the control plane.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self {
			temperature:        AI_SAMPLING_TEMPERATURE.get(ctx),
			top_p:              AI_SAMPLING_TOP_P.get(ctx),
			top_k:              AI_SAMPLING_TOP_K.get(ctx),
			min_p:              AI_SAMPLING_MIN_P.get(ctx),
			presence_penalty:   AI_SAMPLING_PRESENCE_PENALTY.get(ctx),
			frequency_penalty:  AI_SAMPLING_FREQUENCY_PENALTY.get(ctx),
			repetition_penalty: AI_SAMPLING_REPETITION_PENALTY.get(ctx),
			verbosity:          AI_SAMPLING_VERBOSITY.get(ctx),
		}
	}

	/// Reports whether all cross-variable sampling invariants hold.
	#[must_use]
	pub fn validate(&self) -> bool {
		let probability = |value: f32| value == -1.0 || (0.0..=1.0).contains(&value);
		let finite =
			[self.temperature, self.presence_penalty, self.frequency_penalty, self.repetition_penalty]
				.into_iter()
				.all(f32::is_finite);
		finite
			&& self.temperature >= -1.0
			&& probability(self.top_p)
			&& probability(self.min_p)
			&& self.top_k >= -1
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

impl ProviderRuntimeSettings {
	/// Projects provider admission and timeout policy from the control plane.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self {
			max_in_flight:        deserialize_table(AI_PROVIDER_MAX_IN_FLIGHT.get(ctx)),
			max_queued:           AI_PROVIDER_MAX_QUEUED.get(ctx) as usize,
			timeout_seconds:      u64::from(AI_PROVIDER_TIMEOUT_SECONDS.get(ctx)),
			call_timeout_seconds: u64::from(AI_PROVIDER_CALL_TIMEOUT_SECONDS.get(ctx)),
			bedrock_guardrails:   deserialize_table(AI_PROVIDER_BEDROCK_GUARDRAILS.get(ctx)),
		}
	}

	/// Reports whether all cross-variable provider runtime invariants hold.
	#[must_use]
	pub fn validate(&self) -> bool {
		self.max_queued <= 100_000
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
			})
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
	/// Projects the complete inference policy from the control plane.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self {
			retry:     RetrySettings::from_con(ctx),
			sampling:  SamplingSettings::from_con(ctx),
			providers: ProviderRuntimeSettings::from_con(ctx),
			model:     omp_catalog::settings::ModelSettings::from_con(ctx),
		}
	}

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

fn nonnegative(value: f32) -> Option<f32> {
	(value >= 0.0).then_some(value)
}

fn json_to_con(value: serde_json::Value) -> Option<Value> {
	match value {
		serde_json::Value::Null => None,
		serde_json::Value::Bool(value) => Some(Value::Bool(value)),
		serde_json::Value::Number(value) => value
			.as_i64()
			.map(Value::Int)
			.or_else(|| value.as_f64().map(Value::Float)),
		serde_json::Value::String(value) => Some(Value::Str(Str::from(value))),
		serde_json::Value::Array(values) => values
			.into_iter()
			.map(json_to_con)
			.collect::<Option<Vec<_>>>()
			.map(Value::List),
		serde_json::Value::Object(values) => values
			.into_iter()
			.map(|(key, value)| Some((Str::from(key), json_to_con(value)?)))
			.collect::<Option<Vec<_>>>()
			.map(|values| Value::Kv(Kv(values))),
	}
}

fn con_to_json(value: Value) -> serde_json::Value {
	match value {
		Value::Bool(value) => serde_json::Value::Bool(value),
		Value::Int(value) => serde_json::Value::Number(value.into()),
		Value::Float(value) => serde_json::Number::from_f64(value)
			.map_or(serde_json::Value::Null, serde_json::Value::Number),
		Value::Str(value) | Value::Enum(value) => serde_json::Value::String(value.into()),
		Value::Duration(value) => serde_json::Value::String(value.to_string()),
		Value::List(values) => {
			serde_json::Value::Array(values.into_iter().map(con_to_json).collect())
		},
		Value::Kv(values) => serde_json::Value::Object(
			values
				.0
				.into_iter()
				.map(|(key, value)| (key.into(), con_to_json(value)))
				.collect(),
		),
	}
}

fn serialize_table<T: Serialize>(value: &T) -> Kv {
	match json_to_con(serde_json::to_value(value).expect("settings table serializes")) {
		Some(Value::Kv(value)) => value,
		_ => panic!("settings table must serialize as an object"),
	}
}

fn try_deserialize_table<T: DeserializeOwned>(value: Kv) -> Option<T> {
	serde_json::from_value(con_to_json(Value::Kv(value))).ok()
}

fn deserialize_table<T: DeserializeOwned>(value: Kv) -> T {
	try_deserialize_table(value).expect("convar table was validated before commit")
}

fn invalid(reason: &'static str) -> Result<(), Str> {
	Err(Str::new_static(reason))
}

fn validate_retry_chains(_: &Ctx, value: &Kv) -> Result<(), Str> {
	let Some(chains) = try_deserialize_table::<FallbackChains>(value.clone()) else {
		return invalid("fallback chains must map selectors to non-empty selector lists");
	};
	if chains.iter().all(|(key, values)| {
		!key.is_empty()
			&& !values.is_empty()
			&& values.iter().enumerate().all(|(index, value)| {
				!value.is_empty() && values[..index].iter().all(|prior| prior != value)
			})
	}) {
		Ok(())
	} else {
		invalid("fallback chains must map selectors to non-empty unique selector lists")
	}
}

fn validate_max_in_flight(_: &Ctx, value: &Kv) -> Result<(), Str> {
	let Some(limits) = try_deserialize_table::<BTreeMap<Str, usize>>(value.clone()) else {
		return invalid("provider limits must map provider names to integers");
	};
	if limits
		.iter()
		.all(|(provider, limit)| !provider.is_empty() && *limit <= 100_000)
	{
		Ok(())
	} else {
		invalid("provider limits require non-empty names and values at most 100000")
	}
}

fn validate_bedrock_guardrails(_: &Ctx, value: &Kv) -> Result<(), Str> {
	let Some(guardrails) = try_deserialize_table::<
		BTreeMap<Str, crate::codec::bedrock::BedrockGuardrail>,
	>(value.clone()) else {
		return invalid("Bedrock guardrails must be keyed configuration blocks");
	};
	if guardrails.iter().all(|(provider, guardrail)| {
		!provider.trim().is_empty()
			&& !guardrail.identifier.trim().is_empty()
			&& !guardrail.version.trim().is_empty()
	}) {
		Ok(())
	} else {
		invalid("Bedrock guardrails require non-empty provider, identifier, and version")
	}
}

fn validate_finite(_: &Ctx, value: &f32) -> Result<(), Str> {
	if value.is_finite() {
		Ok(())
	} else {
		invalid("sampling value must be finite")
	}
}

fn validate_retry_base(ctx: &Ctx, value: &u32) -> Result<(), Str> {
	let maximum = AI_RETRY_MAX_DELAY_MS.get(ctx);
	if maximum == 0 || *value <= maximum {
		Ok(())
	} else {
		invalid("base retry delay must not exceed the maximum retry delay")
	}
}

fn validate_retry_max(ctx: &Ctx, value: &u32) -> Result<(), Str> {
	if *value == 0 || AI_RETRY_BASE_DELAY_MS.get(ctx) <= *value {
		Ok(())
	} else {
		invalid("maximum retry delay must be zero or at least the base retry delay")
	}
}

omp_con::var! {
	/// Enables transport and model fallback recovery.
	pub static AI_RETRY_ENABLED = ai_retry_enabled: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Maximum retries after the first attempt.
	pub static AI_RETRY_MAX_RETRIES = ai_retry_max_retries: u32 {
		default: 10,
		min: 0,
		max: 100,
		flags: archive | inherit,
	};
	/// First exponential retry ceiling in milliseconds.
	pub static AI_RETRY_BASE_DELAY_MS = ai_retry_base_delay_ms: u32 {
		default: 500,
		min: 0,
		max: 3_600_000,
		validate: validate_retry_base,
		flags: archive | inherit,
	};
	/// Largest accepted retry delay in milliseconds; zero disables the cap.
	pub static AI_RETRY_MAX_DELAY_MS = ai_retry_max_delay_ms: u32 {
		default: 300_000,
		min: 0,
		max: 3_600_000,
		validate: validate_retry_max,
		flags: archive | inherit,
	};
	/// Enables model fallback candidates.
	pub static AI_RETRY_MODEL_FALLBACK = ai_retry_model_fallback: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Enables quota-aware preflight fallback.
	pub static AI_RETRY_USAGE_AWARE_FALLBACK = ai_retry_usage_aware_fallback: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Remaining quota percentage held in reserve.
	pub static AI_RETRY_USAGE_RESERVE_PCT = ai_retry_usage_reserve_pct: u8 {
		default: 10,
		min: 0,
		max: 100,
		flags: archive | inherit,
	};
	/// Action when every account is inside the reserve.
	pub static AI_RETRY_USAGE_RESERVE_POLICY = ai_retry_usage_reserve_policy: UsageReservePolicy {
		default: UsageReservePolicy::Confirm,
		flags: archive | inherit,
	};
	/// Exact model/provider fallback chains.
	pub static AI_RETRY_FALLBACK_CHAINS = ai_retry_fallback_chains: Kv {
		default: serialize_table(&FallbackChains::new()),
		validate: validate_retry_chains,
		flags: archive | inherit,
	};
	/// Primary reversion behavior after fallback.
	pub static AI_RETRY_FALLBACK_REVERT = ai_retry_fallback_revert: FallbackRevertPolicy {
		default: FallbackRevertPolicy::CooldownExpiry,
		flags: archive | inherit,
	};
	/// Enables the explicit Anthropic server-side safety fallback header.
	pub static AI_RETRY_SERVER_SIDE_FALLBACK = ai_retry_server_side_fallback: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Default sampling temperature; negative preserves provider default.
	pub static AI_SAMPLING_TEMPERATURE = ai_sampling_temperature: f32 {
		default: -1.0,
		min: -1.0,
		validate: validate_finite,
		flags: archive | inherit,
	};
	/// Default nucleus cutoff; negative preserves provider default.
	pub static AI_SAMPLING_TOP_P = ai_sampling_top_p: f32 {
		default: -1.0,
		min: -1.0,
		max: 1.0,
		validate: validate_finite,
		flags: archive | inherit,
	};
	/// Default top-k bound; negative preserves provider default.
	pub static AI_SAMPLING_TOP_K = ai_sampling_top_k: i32 {
		default: -1,
		min: -1,
		flags: archive | inherit,
	};
	/// Default minimum probability cutoff; negative preserves provider default.
	pub static AI_SAMPLING_MIN_P = ai_sampling_min_p: f32 {
		default: -1.0,
		min: -1.0,
		max: 1.0,
		validate: validate_finite,
		flags: archive | inherit,
	};
	/// Default presence penalty; negative preserves provider default.
	pub static AI_SAMPLING_PRESENCE_PENALTY = ai_sampling_presence_penalty: f32 {
		default: -1.0,
		validate: validate_finite,
		flags: archive | inherit,
	};
	/// Default frequency penalty; negative preserves provider default.
	pub static AI_SAMPLING_FREQUENCY_PENALTY = ai_sampling_frequency_penalty: f32 {
		default: -1.0,
		validate: validate_finite,
		flags: archive | inherit,
	};
	/// Default repetition penalty; negative preserves provider default.
	pub static AI_SAMPLING_REPETITION_PENALTY = ai_sampling_repetition_penalty: f32 {
		default: -1.0,
		validate: validate_finite,
		flags: archive | inherit,
	};
	/// Default response verbosity.
	pub static AI_SAMPLING_VERBOSITY = ai_sampling_verbosity: TextVerbositySetting {
		default: TextVerbositySetting::Medium,
		flags: archive | inherit,
	};
	/// Maximum concurrent requests keyed by provider id.
	pub static AI_PROVIDER_MAX_IN_FLIGHT = ai_provider_max_in_flight: Kv {
		default: serialize_table(&BTreeMap::<Str, usize>::new()),
		validate: validate_max_in_flight,
		flags: archive | inherit,
	};
	/// Maximum queued callers per provider before backpressure fails fast.
	pub static AI_PROVIDER_MAX_QUEUED = ai_provider_max_queued: u32 {
		default: 64,
		min: 0,
		max: 100_000,
		flags: archive | inherit,
	};
	/// Per-transport-attempt timeout in seconds.
	pub static AI_PROVIDER_TIMEOUT_SECONDS = ai_provider_timeout_seconds: u32 {
		default: 300,
		min: 1,
		max: 3_600,
		flags: archive | inherit,
	};
	/// Overall logical-call timeout in seconds; zero preserves caller deadlines.
	pub static AI_PROVIDER_CALL_TIMEOUT_SECONDS = ai_provider_call_timeout_seconds: u32 {
		default: 0,
		min: 0,
		max: 86_400,
		flags: archive | inherit,
	};
	/// Bedrock guardrail policy keyed by provider id.
	pub static AI_PROVIDER_BEDROCK_GUARDRAILS = ai_provider_bedrock_guardrails: Kv {
		default: serialize_table(&BTreeMap::<Str, crate::codec::bedrock::BedrockGuardrail>::new()),
		validate: validate_bedrock_guardrails,
		flags: archive | inherit,
	};
}

/// One-shot migration map from reflected TOML paths to convar names.
pub const LEGACY_CONVAR_MAPPINGS: &[(&str, &str)] = &[
	("retry.enabled", "ai_retry_enabled"),
	("retry.max_retries", "ai_retry_max_retries"),
	("retry.base_delay_ms", "ai_retry_base_delay_ms"),
	("retry.max_delay_ms", "ai_retry_max_delay_ms"),
	("retry.model_fallback", "ai_retry_model_fallback"),
	("retry.usage_aware_fallback", "ai_retry_usage_aware_fallback"),
	("retry.usage_reserve_pct", "ai_retry_usage_reserve_pct"),
	("retry.usage_reserve_policy", "ai_retry_usage_reserve_policy"),
	("retry.fallback_chains", "ai_retry_fallback_chains"),
	("retry.fallback_revert", "ai_retry_fallback_revert"),
	("retry.server_side_fallback", "ai_retry_server_side_fallback"),
	("sampling.temperature", "ai_sampling_temperature"),
	("sampling.top_p", "ai_sampling_top_p"),
	("sampling.top_k", "ai_sampling_top_k"),
	("sampling.min_p", "ai_sampling_min_p"),
	("sampling.presence_penalty", "ai_sampling_presence_penalty"),
	("sampling.frequency_penalty", "ai_sampling_frequency_penalty"),
	("sampling.repetition_penalty", "ai_sampling_repetition_penalty"),
	("sampling.verbosity", "ai_sampling_verbosity"),
	("provider_runtime.max_in_flight", "ai_provider_max_in_flight"),
	("provider_runtime.max_queued", "ai_provider_max_queued"),
	("provider_runtime.timeout_seconds", "ai_provider_timeout_seconds"),
	("provider_runtime.call_timeout_seconds", "ai_provider_call_timeout_seconds"),
	("provider_runtime.bedrock_guardrails", "ai_provider_bedrock_guardrails"),
	("web_search.order", "ai_search_order"),
	("web_search.exclusions", "ai_search_exclusions"),
	("web_search.timeout_seconds", "ai_search_timeout_seconds"),
	("web_search.searxng_endpoint", "ai_search_searxng_endpoint"),
	("web_search.gemini_model", "ai_search_gemini_model"),
	("web_search.antigravity_mode", "ai_search_antigravity_mode"),
	("web_search.perplexity_responses", "ai_search_perplexity_responses"),
];

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn zero_max_retry_delay_is_a_valid_uncapped_sentinel() {
		let settings =
			RetrySettings { base_delay_ms: 500, max_delay_ms: 0, ..RetrySettings::default() };
		assert!(settings.validate());
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
	fn from_con_projects_typed_overrides() {
		let ctx = Ctx::new();
		AI_RETRY_MAX_RETRIES.set(&ctx, 3).expect("set retry limit");
		AI_SAMPLING_VERBOSITY
			.set(&ctx, TextVerbositySetting::High)
			.expect("set verbosity");
		let settings = InferenceSettings::from_con(&ctx);
		assert_eq!(settings.retry.max_retries, 3);
		assert_eq!(settings.sampling.verbosity, TextVerbositySetting::High);
		assert!(settings.retry.validate());
		assert!(settings.sampling.validate());
		assert!(settings.providers.validate());
	}

	#[test]
	fn vars_declare_every_former_schema_field() {
		use crate::search_settings::*;

		let old_fields = [
			"retry.enabled",
			"retry.max_retries",
			"retry.base_delay_ms",
			"retry.max_delay_ms",
			"retry.model_fallback",
			"retry.usage_aware_fallback",
			"retry.usage_reserve_pct",
			"retry.usage_reserve_policy",
			"retry.fallback_chains",
			"retry.fallback_revert",
			"retry.server_side_fallback",
			"sampling.temperature",
			"sampling.top_p",
			"sampling.top_k",
			"sampling.min_p",
			"sampling.presence_penalty",
			"sampling.frequency_penalty",
			"sampling.repetition_penalty",
			"sampling.verbosity",
			"provider_runtime.max_in_flight",
			"provider_runtime.max_queued",
			"provider_runtime.timeout_seconds",
			"provider_runtime.call_timeout_seconds",
			"provider_runtime.bedrock_guardrails",
			"web_search.order",
			"web_search.exclusions",
			"web_search.timeout_seconds",
			"web_search.searxng_endpoint",
			"web_search.gemini_model",
			"web_search.antigravity_mode",
			"web_search.perplexity_responses",
		];
		let vars = [
			AI_RETRY_ENABLED.name(),
			AI_RETRY_MAX_RETRIES.name(),
			AI_RETRY_BASE_DELAY_MS.name(),
			AI_RETRY_MAX_DELAY_MS.name(),
			AI_RETRY_MODEL_FALLBACK.name(),
			AI_RETRY_USAGE_AWARE_FALLBACK.name(),
			AI_RETRY_USAGE_RESERVE_PCT.name(),
			AI_RETRY_USAGE_RESERVE_POLICY.name(),
			AI_RETRY_FALLBACK_CHAINS.name(),
			AI_RETRY_FALLBACK_REVERT.name(),
			AI_RETRY_SERVER_SIDE_FALLBACK.name(),
			AI_SAMPLING_TEMPERATURE.name(),
			AI_SAMPLING_TOP_P.name(),
			AI_SAMPLING_TOP_K.name(),
			AI_SAMPLING_MIN_P.name(),
			AI_SAMPLING_PRESENCE_PENALTY.name(),
			AI_SAMPLING_FREQUENCY_PENALTY.name(),
			AI_SAMPLING_REPETITION_PENALTY.name(),
			AI_SAMPLING_VERBOSITY.name(),
			AI_PROVIDER_MAX_IN_FLIGHT.name(),
			AI_PROVIDER_MAX_QUEUED.name(),
			AI_PROVIDER_TIMEOUT_SECONDS.name(),
			AI_PROVIDER_CALL_TIMEOUT_SECONDS.name(),
			AI_PROVIDER_BEDROCK_GUARDRAILS.name(),
			AI_SEARCH_ORDER.name(),
			AI_SEARCH_EXCLUSIONS.name(),
			AI_SEARCH_TIMEOUT_SECONDS.name(),
			AI_SEARCH_SEARXNG_ENDPOINT.name(),
			AI_SEARCH_GEMINI_MODEL.name(),
			AI_SEARCH_ANTIGRAVITY_MODE.name(),
			AI_SEARCH_PERPLEXITY_RESPONSES.name(),
		];
		assert_eq!(
			LEGACY_CONVAR_MAPPINGS,
			old_fields
				.into_iter()
				.zip(vars)
				.collect::<Vec<_>>()
				.as_slice()
		);
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
