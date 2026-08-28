//! App-owned advisor model fallback, retry, cooldown, and quota policy.

use std::{
	collections::{BTreeMap, VecDeque},
	sync::Arc,
	time::{Duration, Instant},
};

use omp_agent::{
	ControlError, ControlSender, Next, Regime, RegimeContext, RegimeError, RegimeLifetime,
	RegimeSpec, RegimeStateError, StartOptions,
	advisor::{AdviceDelivery, AdviceSeverity, DeliveryContext, RoutedAdvice},
	broker_now_ms,
};
use omp_core::{Point, PointSet, Str};
use omp_proto::thread::v1::{self as thread, Item, item, part};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Provider failure class relevant to advisor recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AdvisorFailureClass {
	/// Transient transport or provider failure.
	Transient,
	/// Provider quota is exhausted and must not be retried automatically.
	Quota,
	/// The model or request shape is permanently unsupported.
	Permanent,
}

/// One explicitly ordered advisor model fallback chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorFallbackChain {
	selectors: Arc<[Str]>,
}

impl AdvisorFallbackChain {
	/// Builds a non-empty, stable, duplicate-free selector chain.
	pub fn new(selectors: impl IntoIterator<Item = Str>) -> Result<Self, AdvisorResilienceError> {
		let mut retained = Vec::new();
		for selector in selectors {
			let selector = selector.trim();
			if selector.is_empty() {
				return Err(AdvisorResilienceError::EmptySelector);
			}
			if !retained.iter().any(|existing: &Str| *existing == selector) {
				retained.push(Str::new(selector));
			}
		}
		if retained.is_empty() {
			return Err(AdvisorResilienceError::EmptyChain);
		}
		Ok(Self { selectors: retained.into() })
	}

	/// Borrows selectors in exact fallback order.
	pub fn selectors(&self) -> &[Str] {
		&self.selectors
	}
}

/// Retry decision for one advisor update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdvisorRetryDecision {
	/// Attempt this selector immediately.
	Attempt {
		/// Provider/model selector chosen from the ordered fallback chain.
		selector: Str,
		/// One-based attempt number for this selector.
		attempt:  u32,
	},
	/// Wait until the cooldown expires, then ask again.
	Cooldown {
		/// Monotonic deadline after which another attempt may be selected.
		until: Instant,
	},
	/// Quota is hard-latched until an explicit reset or credential refresh.
	QuotaLatched,
	/// Every retry and fallback candidate was exhausted.
	Exhausted,
	/// The current failure is permanent for the configured chain.
	Permanent,
}

/// Invalid resilience configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AdvisorResilienceError {
	/// No fallback selector was supplied.
	#[error("advisor fallback chain must not be empty")]
	EmptyChain,
	/// One selector was empty after trimming.
	#[error("advisor fallback selector must not be empty")]
	EmptySelector,
	/// A retry budget of zero cannot execute an update.
	#[error("advisor retry budget must be positive")]
	ZeroRetryBudget,
}
/// Typed terminal reason retained after an advisor regime is muted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorMuteReason {
	/// Repeated unsafe advisor turns exhausted the quarantine bound.
	QuarantineExhausted {
		/// Classification attached to the final quarantined turn.
		reason: Str,
	},
}

/// Result of offering one guarded note to the advisor regime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdvisorRegimeSubmission {
	/// The note was accepted for delivery at the named loop boundary.
	Accepted(AdviceDelivery),
	/// The advisor was already muted and cannot enqueue more notes.
	Muted(AdvisorMuteReason),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PendingAdvice {
	advisor_id: Str,
	note:       Str,
	severity:   AdviceSeverity,
}

impl From<RoutedAdvice> for PendingAdvice {
	fn from(advice: RoutedAdvice) -> Self {
		Self { advisor_id: advice.advisor_id, note: advice.note, severity: advice.severity }
	}
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct AdvisorRegimeState {
	context_or_idle:  VecDeque<PendingAdvice>,
	turn_end:         VecDeque<PendingAdvice>,
	idle:             VecDeque<PendingAdvice>,
	last_delivery_ms: Option<u64>,
	quarantines:      u32,
	muted:            Option<AdvisorMuteReason>,
}

/// App-owned handle feeding one active advisor's delivery regime.
#[derive(Clone)]
pub struct AdvisorRegimeHandle {
	state:            Arc<Mutex<AdvisorRegimeState>>,
	quarantine_bound: u32,
}

impl AdvisorRegimeHandle {
	/// Maps and queues one delivery decision for a later arbiter fold.
	pub fn submit(&self, advice: RoutedAdvice, context: DeliveryContext) -> AdvisorRegimeSubmission {
		let mut state = self.state.lock();
		if let Some(reason) = state.muted.clone() {
			return AdvisorRegimeSubmission::Muted(reason);
		}
		let delivery = advisor_delivery(advice.severity, context);
		let pending = PendingAdvice::from(advice);
		match delivery {
			AdviceDelivery::Aside => state.context_or_idle.push_back(pending),
			AdviceDelivery::Steer => state.turn_end.push_back(pending),
			AdviceDelivery::Preserve => state.idle.push_back(pending),
		}
		AdvisorRegimeSubmission::Accepted(delivery)
	}

	/// Advances the regime's durable quarantine bound and returns a typed
	/// mute reason when its finite bound exhausts.
	pub async fn record_quarantine(
		&self,
		control: &ControlSender,
		activation: &str,
		reason: impl Into<Str>,
	) -> Result<Option<AdvisorMuteReason>, ControlError> {
		let mut next_state = self.state.lock().clone();
		if let Some(reason) = next_state.muted.clone() {
			return Ok(Some(reason));
		}
		next_state.quarantines = next_state.quarantines.saturating_add(1);
		let muted = if next_state.quarantines >= self.quarantine_bound {
			let muted = AdvisorMuteReason::QuarantineExhausted { reason: reason.into() };
			next_state.muted = Some(muted.clone());
			next_state.context_or_idle.clear();
			next_state.turn_end.clear();
			next_state.idle.clear();
			Some(muted)
		} else {
			None
		};
		let payload = serde_json::to_vec(&next_state)
			.expect("advisor regime state has infallible JSON serialization");
		control
			.update_regime_state(Str::new(activation), payload.into())
			.await?;
		*self.state.lock() = next_state;
		Ok(muted)
	}

	/// Returns the terminal mute reason, when this advisor exhausted policy.
	pub fn muted_reason(&self) -> Option<AdvisorMuteReason> {
		self.state.lock().muted.clone()
	}
}

/// One regime handler staging advisor notes as ordered context effects.
pub struct AdvisorDeliveryRegime {
	state:       Arc<Mutex<AdvisorRegimeState>>,
	immunity_ms: u64,
}

/// Lifecycle owner for one active advisor delivery regime.
pub struct ActiveAdvisorRegime {
	control:    ControlSender,
	activation: Str,
	handle:     AdvisorRegimeHandle,
}

impl ActiveAdvisorRegime {
	/// Starts exactly one delivery regime when an advisor child starts.
	pub async fn start(
		control: ControlSender,
		advisor_id: &str,
		immunity: Duration,
		quarantine_bound: u32,
	) -> Result<Self, ControlError> {
		let (spec, regime, handle) =
			AdvisorDeliveryRegime::new(advisor_id, immunity, quarantine_bound);
		let receipt = control
			.start_regime(spec, Box::new(regime), StartOptions {
				now_ms: broker_now_ms(),
				queue:  false,
			})
			.await?;
		Ok(Self { control, activation: receipt.activation, handle })
	}

	/// Returns the feeding handle for guarded advisor notes.
	pub const fn handle(&self) -> &AdvisorRegimeHandle {
		&self.handle
	}

	/// Advances this advisor's durable quarantine bound.
	pub async fn record_quarantine(
		&self,
		reason: impl Into<Str>,
	) -> Result<Option<AdvisorMuteReason>, ControlError> {
		self
			.handle
			.record_quarantine(&self.control, self.activation.as_str(), reason)
			.await
	}

	/// Stops the regime when the advisor child stops.
	pub async fn stop(&self) -> Result<bool, ControlError> {
		self.control.stop_regime(self.activation.clone()).await
	}

	/// Returns the durable activation identity.
	pub fn activation(&self) -> &str {
		self.activation.as_str()
	}
}

impl AdvisorDeliveryRegime {
	/// Builds the session-scoped declaration, handler, and feeding handle for
	/// one active advisor.
	pub fn new(
		advisor_id: &str,
		immunity: Duration,
		quarantine_bound: u32,
	) -> (Arc<RegimeSpec>, Self, AdvisorRegimeHandle) {
		let immunity_ms = u64::try_from(immunity.as_millis()).unwrap_or(u64::MAX);
		let quarantine_bound = quarantine_bound.max(2);
		let state = Arc::new(Mutex::new(AdvisorRegimeState::default()));
		let spec = Arc::new(RegimeSpec {
			id: Str::from(format!("advisor-delivery/{advisor_id}")),
			events: PointSet::EMPTY
				.with(Point::Context)
				.with(Point::TurnEnd)
				.with(Point::Idle),
			precedence: 40,
			max_steps: None,
			committed_step_interval_ms: None,
			on_limit: false,
			lifetime: RegimeLifetime::Session,
			family_rev: Str::new_static("dev.omp.app.advisor-delivery@1"),
			when: None,
			owns: Arc::from([]),
			sets: Arc::from([]),
			minimum_duration_ms: Some(immunity_ms),
		});
		let regime = Self { state: Arc::clone(&state), immunity_ms };
		let handle = AdvisorRegimeHandle { state, quarantine_bound };
		(spec, regime, handle)
	}

	fn drain(&self, point: Point, now_ms: u64) -> Vec<Item> {
		let mut state = self.state.lock();
		if state.muted.is_some()
			|| state
				.last_delivery_ms
				.is_some_and(|last| now_ms < last.saturating_add(self.immunity_ms))
		{
			return Vec::new();
		}
		let mut pending = Vec::new();
		match point {
			Point::Context => pending.extend(state.context_or_idle.drain(..)),
			Point::TurnEnd => pending.extend(state.turn_end.drain(..)),
			Point::Idle => {
				pending.extend(state.context_or_idle.drain(..));
				pending.extend(state.idle.drain(..));
			},
			_ => return Vec::new(),
		}
		let items = pending.into_iter().map(advisor_item).collect::<Vec<_>>();
		if !items.is_empty() {
			state.last_delivery_ms = Some(now_ms);
		}
		items
	}
}

impl Regime for AdvisorDeliveryRegime {
	fn apply(&mut self, ctx: &mut RegimeContext<'_>, _next: Next<'_>) -> Result<(), RegimeError> {
		let items = self.drain(ctx.point(), ctx.facts().now_ms);
		if !items.is_empty() {
			ctx.append_context(items);
			ctx.replace_state(self.state());
		}
		Ok(())
	}

	fn state(&self) -> Str {
		serde_json::to_string(&*self.state.lock()).map_or_else(|_| Str::new_static("{}"), Str::from)
	}

	fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
		let restored = serde_json::from_str(payload).map_err(|_| RegimeStateError::InvalidPayload)?;
		*self.state.lock() = restored;
		Ok(())
	}
}

fn advisor_delivery(severity: AdviceSeverity, context: DeliveryContext) -> AdviceDelivery {
	if severity == AdviceSeverity::Nit {
		return AdviceDelivery::Aside;
	}
	if context.externally_interrupted || context.plan_mode {
		return AdviceDelivery::Preserve;
	}
	if context.update_in_progress && severity != AdviceSeverity::Blocker {
		return AdviceDelivery::Aside;
	}
	if context.streaming {
		return AdviceDelivery::Steer;
	}
	if context.deferred_client_turns {
		return AdviceDelivery::Preserve;
	}
	if context.terminal_answer && !context.queued_work && severity == AdviceSeverity::Concern {
		return AdviceDelivery::Preserve;
	}
	AdviceDelivery::Steer
}

fn advisor_item(advice: PendingAdvice) -> Item {
	let severity: &'static str = advice.severity.into();
	let mut text =
		String::with_capacity(advice.advisor_id.len() + advice.note.len() + severity.len() + 16);
	text.push_str("[Advisor ");
	text.push_str(advice.advisor_id.as_str());
	text.push_str(" (");
	text.push_str(severity);
	text.push_str(")]\n");
	text.push_str(advice.note.as_str());
	Item {
		kind: Some(item::Kind::Message(thread::Message {
			role:            thread::Role::System as i32,
			parts:           vec![thread::Part { kind: Some(part::Kind::Text(text)) }],
			synthetic:       None,
			user_initiated:  None,
			completed_at_ms: None,
			usage:           None,
		})),
		..Item::default()
	}
}

#[derive(Clone, Debug)]
struct AdvisorBudgetState {
	selectors:      Vec<Str>,
	candidate:      usize,
	attempts:       u32,
	cooldown_until: Option<Instant>,
	quota_latched:  bool,
}

/// Per-advisor retry budget manager owned by production composition.
pub struct AdvisorRetryManager {
	chain:              AdvisorFallbackChain,
	owned_chains:       BTreeMap<Str, AdvisorFallbackChain>,
	attempts_per_model: u32,
	initial_backoff:    Duration,
	max_backoff:        Duration,
	states:             BTreeMap<Str, AdvisorBudgetState>,
}

impl AdvisorRetryManager {
	/// Creates a manager with bounded exponential cooldowns.
	pub fn new(
		chain: AdvisorFallbackChain,
		attempts_per_model: u32,
		initial_backoff: Duration,
		max_backoff: Duration,
	) -> Result<Self, AdvisorResilienceError> {
		if attempts_per_model == 0 {
			return Err(AdvisorResilienceError::ZeroRetryBudget);
		}
		Ok(Self {
			chain,
			owned_chains: BTreeMap::new(),
			attempts_per_model,
			initial_backoff,
			max_backoff: max_backoff.max(initial_backoff),
			states: BTreeMap::new(),
		})
	}

	/// Installs chains owned by selectors that can occur at the end of the
	/// primary advisor chain.
	///
	/// When an advisor exhausts the pinned chain on such a selector, that
	/// selector's own chain is appended once. Existing selectors are skipped, so
	/// mutually-referential chains remain finite.
	pub fn with_owned_chains(mut self, chains: BTreeMap<Str, AdvisorFallbackChain>) -> Self {
		self.owned_chains = chains;
		self
	}

	/// Selects the next permitted attempt for one stable advisor id.
	pub fn next(&mut self, advisor_id: &str, now: Instant) -> AdvisorRetryDecision {
		let initial = self.chain.selectors().to_vec();
		let state = self
			.states
			.entry(Str::new(advisor_id))
			.or_insert(AdvisorBudgetState {
				selectors:      initial,
				candidate:      0,
				attempts:       0,
				cooldown_until: None,
				quota_latched:  false,
			});
		if state.quota_latched {
			return AdvisorRetryDecision::QuotaLatched;
		}
		if let Some(until) = state.cooldown_until {
			if now < until {
				return AdvisorRetryDecision::Cooldown { until };
			}
			state.cooldown_until = None;
		}
		let Some(selector) = state.selectors.get(state.candidate) else {
			return AdvisorRetryDecision::Exhausted;
		};
		AdvisorRetryDecision::Attempt {
			selector: selector.clone(),
			attempt:  state.attempts.saturating_add(1),
		}
	}

	/// Records a failed attempt and advances retry/fallback policy.
	pub fn record_failure(
		&mut self,
		advisor_id: &str,
		class: AdvisorFailureClass,
		now: Instant,
	) -> AdvisorRetryDecision {
		let initial = self.chain.selectors().to_vec();
		let state = self
			.states
			.entry(Str::new(advisor_id))
			.or_insert(AdvisorBudgetState {
				selectors:      initial,
				candidate:      0,
				attempts:       0,
				cooldown_until: None,
				quota_latched:  false,
			});
		match class {
			AdvisorFailureClass::Quota => {
				state.quota_latched = true;
				AdvisorRetryDecision::QuotaLatched
			},
			AdvisorFailureClass::Permanent => {
				state.candidate = state.selectors.len();
				AdvisorRetryDecision::Permanent
			},
			AdvisorFailureClass::Transient => {
				state.attempts = state.attempts.saturating_add(1);
				if state.attempts >= self.attempts_per_model {
					state.candidate = state.candidate.saturating_add(1);
					state.attempts = 0;
				}
				if state.candidate >= state.selectors.len()
					&& let Some(current) = state.selectors.last()
					&& let Some(chain) = self.owned_chains.get(current)
				{
					for selector in chain.selectors() {
						if !state.selectors.contains(selector) {
							state.selectors.push(selector.clone());
						}
					}
				}
				if state.candidate >= state.selectors.len() {
					return AdvisorRetryDecision::Exhausted;
				}
				let exponent = state.attempts.min(31);
				let factor = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
				let backoff = self
					.initial_backoff
					.saturating_mul(factor)
					.min(self.max_backoff);
				let until = now + backoff;
				state.cooldown_until = Some(until);
				AdvisorRetryDecision::Cooldown { until }
			},
		}
	}

	/// Clears retry/cooldown state after one successful update.
	pub fn record_success(&mut self, advisor_id: &str) {
		self.states.remove(advisor_id);
	}

	/// Releases only the quota hard latch after credential refresh or user
	/// reset.
	pub fn reset_quota_latch(&mut self, advisor_id: &str) {
		if let Some(state) = self.states.get_mut(advisor_id) {
			state.quota_latched = false;
			state.cooldown_until = None;
		}
	}
}
#[cfg(test)]
mod tests {
	use super::*;

	fn advice(note: &'static str) -> RoutedAdvice {
		RoutedAdvice {
			advisor_id: Str::new_static("watchdog"),
			note:       Str::new_static(note),
			severity:   AdviceSeverity::Nit,
		}
	}

	#[test]
	fn immunity_window_blocks_delivery_until_elapsed() {
		let (spec, regime, handle) =
			AdvisorDeliveryRegime::new("watchdog", Duration::from_millis(25), 2);
		assert_eq!(spec.minimum_duration_ms, Some(25));
		assert_eq!(spec.committed_step_interval_ms, None);
		assert_eq!(
			handle.submit(advice("first"), DeliveryContext::default()),
			AdvisorRegimeSubmission::Accepted(AdviceDelivery::Aside)
		);
		assert_eq!(regime.drain(Point::Context, 100).len(), 1);
		assert_eq!(
			handle.submit(advice("second"), DeliveryContext::default()),
			AdvisorRegimeSubmission::Accepted(AdviceDelivery::Aside)
		);
		assert!(regime.drain(Point::Idle, 124).is_empty());
		assert_eq!(regime.drain(Point::Idle, 125).len(), 1);
	}
	#[test]
	fn external_interrupt_preserves_streaming_advice_without_steering() {
		let context = DeliveryContext {
			streaming: true,
			externally_interrupted: true,
			..DeliveryContext::default()
		};
		assert_eq!(advisor_delivery(AdviceSeverity::Concern, context), AdviceDelivery::Preserve);
		assert_eq!(advisor_delivery(AdviceSeverity::Blocker, context), AdviceDelivery::Preserve);
	}

	#[test]
	fn advisor_retry_reaches_chain_owned_by_last_fallback() {
		let primary =
			AdvisorFallbackChain::new([Str::new_static("provider/a"), Str::new_static("provider/b")])
				.expect("primary chain");
		let owned =
			AdvisorFallbackChain::new([Str::new_static("provider/b"), Str::new_static("provider/c")])
				.expect("fallback-owned chain");
		let mut manager = AdvisorRetryManager::new(primary, 1, Duration::ZERO, Duration::ZERO)
			.expect("retry manager")
			.with_owned_chains(BTreeMap::from([(Str::new_static("provider/b"), owned)]));
		let now = Instant::now();

		assert_eq!(manager.next("watchdog", now), AdvisorRetryDecision::Attempt {
			selector: Str::new_static("provider/a"),
			attempt:  1,
		});
		assert!(matches!(
			manager.record_failure("watchdog", AdvisorFailureClass::Transient, now),
			AdvisorRetryDecision::Cooldown { .. }
		));
		assert_eq!(manager.next("watchdog", now), AdvisorRetryDecision::Attempt {
			selector: Str::new_static("provider/b"),
			attempt:  1,
		});
		assert!(matches!(
			manager.record_failure("watchdog", AdvisorFailureClass::Transient, now),
			AdvisorRetryDecision::Cooldown { .. }
		));
		assert_eq!(manager.next("watchdog", now), AdvisorRetryDecision::Attempt {
			selector: Str::new_static("provider/c"),
			attempt:  1,
		});
	}
}
