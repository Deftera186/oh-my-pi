//! Bounded advisor history delivery and interruption accounting.

use std::{
	collections::VecDeque,
	iter, mem,
	sync::Arc,
	time::{Duration, Instant},
};

use flume::Receiver;
use omp_core::Str;
use omp_secrets::{obfuscator::SecretObfuscator, replacement::bun_wyhash};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Maximum primary entries retained for advisor catch-up by default.
pub const DEFAULT_HISTORY_ENTRY_LIMIT: usize = 256;
/// Maximum approximate payload bytes retained for advisor catch-up by default.
pub const DEFAULT_HISTORY_BYTE_LIMIT: usize = 512 * 1024;
/// Maximum late-arrival coalescing passes before dispatch yields.
pub const MAX_DELTA_COALESCE_ROUNDS: usize = 3;
/// Fingerprint chunk size used to compare advisor context projections.
pub const ADVISOR_FINGERPRINT_CHUNK_BYTES: usize = 4 * 1024;

/// Severity requested by an advisor.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Eq,
	JsonSchema,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
	strum::Display,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AdviceSeverity {
	/// Non-interrupting cleanup or optional improvement.
	#[default]
	Nit,
	/// Material risk which should steer when policy permits.
	Concern,
	/// Broken work which may wake an otherwise completed turn.
	Blocker,
}

/// Primary-mailbox delivery selected for one accepted note.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AdviceDelivery {
	/// Batch into the next primary step boundary.
	Aside,
	/// Interrupt or trigger a primary turn.
	Steer,
	/// Preserve as a visible card without waking the primary.
	Preserve,
}

/// Current primary-loop facts used to evaluate advisor delivery.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeliveryContext {
	/// A primary turn is currently streaming.
	pub streaming:              bool,
	/// The stopped turn ended with a terminal text answer.
	pub terminal_answer:        bool,
	/// Work remains queued after the current boundary.
	pub queued_work:            bool,
	/// The user or another external authority deliberately stopped the run.
	pub externally_interrupted: bool,
	/// Plan mode forbids advisor-driven turns.
	pub plan_mode:              bool,
	/// The client cannot represent an idle advisor-driven turn.
	pub deferred_client_turns:  bool,
	/// The advisor is reviewing a partial primary update.
	pub update_in_progress:     bool,
}

/// Monotonic immune-window projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImmuneTurnAccount {
	configured:        u32,
	remaining:         u32,
	last_completed_id: Option<u64>,
}

impl ImmuneTurnAccount {
	/// Creates accounting with the configured number of completed primary turns.
	pub const fn new(configured: u32) -> Self {
		Self { configured, remaining: 0, last_completed_id: None }
	}

	/// Arms the full immune window after a note actually used the steering
	/// channel.
	pub const fn record_steer(&mut self) {
		self.remaining = self.configured;
	}

	/// Accounts one newly completed primary turn.
	///
	/// Repeated settlement notifications for the same or an older turn id do not
	/// consume the window, which keeps retries and replay idempotent.
	pub fn record_primary_completion(&mut self, turn_id: u64) {
		if self.last_completed_id.is_some_and(|last| turn_id <= last) {
			return;
		}
		self.last_completed_id = Some(turn_id);
		self.remaining = self.remaining.saturating_sub(1);
	}

	/// Remaining primary completions before interrupting advice is enabled.
	pub const fn remaining(&self) -> u32 {
		self.remaining
	}

	/// Chooses the delivery route without mutating accounting.
	pub fn evaluate(&self, severity: AdviceSeverity, context: DeliveryContext) -> AdviceDelivery {
		if severity == AdviceSeverity::Nit {
			return AdviceDelivery::Aside;
		}
		if context.externally_interrupted || context.plan_mode {
			return AdviceDelivery::Preserve;
		}
		if context.update_in_progress && severity != AdviceSeverity::Blocker {
			return AdviceDelivery::Aside;
		}
		if self.remaining > 0 {
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
}

impl Default for ImmuneTurnAccount {
	fn default() -> Self {
		Self::new(3)
	}
}

/// One retained primary-history entry with an absolute cursor and bounded size.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorHistoryEntry<T> {
	/// Absolute monotonically increasing source cursor.
	pub cursor: u64,
	/// Approximate rendered bytes charged against the history bound.
	pub bytes:  usize,
	/// Immutable entry payload.
	pub value:  T,
}

/// A bounded advisor delta returned to one runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorHistoryDelta<T> {
	/// Cursor after the last returned entry.
	pub next_cursor: u64,
	/// Whether the requested cursor predates retained history and requires
	/// re-prime.
	pub reset:       bool,
	/// Oldest-to-newest retained entries.
	pub entries:     Arc<[AdvisorHistoryEntry<T>]>,
}

/// Bounded append-only primary-history window shared by advisor runtimes.
#[derive(Clone, Debug)]
pub struct BoundedAdvisorHistory<T> {
	entries:        VecDeque<AdvisorHistoryEntry<T>>,
	entry_limit:    usize,
	byte_limit:     usize,
	retained_bytes: usize,
	next_cursor:    u64,
}

impl<T> BoundedAdvisorHistory<T> {
	/// Creates a history window. Zero bounds retain no entries but cursors still
	/// advance.
	pub fn new(entry_limit: usize, byte_limit: usize) -> Self {
		Self { entries: VecDeque::new(), entry_limit, byte_limit, retained_bytes: 0, next_cursor: 0 }
	}

	/// Appends one immutable source entry and returns its absolute cursor.
	pub fn push(&mut self, value: T, bytes: usize) -> u64 {
		let cursor = self.next_cursor;
		self.next_cursor = self.next_cursor.saturating_add(1);
		self.retained_bytes = self.retained_bytes.saturating_add(bytes);
		self
			.entries
			.push_back(AdvisorHistoryEntry { cursor, bytes, value });
		while self.entries.len() > self.entry_limit
			|| self.retained_bytes > self.byte_limit
			|| (self.entry_limit == 0 || self.byte_limit == 0) && !self.entries.is_empty()
		{
			if let Some(removed) = self.entries.pop_front() {
				self.retained_bytes = self.retained_bytes.saturating_sub(removed.bytes);
			}
		}
		cursor
	}

	/// Cursor after the newest observed source entry.
	pub const fn next_cursor(&self) -> u64 {
		self.next_cursor
	}

	/// Clears retained entries after a primary-history rewrite without rewinding
	/// absolute cursors.
	pub fn reset(&mut self) {
		self.entries.clear();
		self.retained_bytes = 0;
		self.next_cursor = self.next_cursor.saturating_add(1);
	}
}

impl<T: Clone> BoundedAdvisorHistory<T> {
	/// Returns entries at or after `cursor`, signaling re-prime if that cursor
	/// was evicted.
	pub fn delta_after(&self, cursor: u64) -> AdvisorHistoryDelta<T> {
		let oldest = self
			.entries
			.front()
			.map_or(self.next_cursor, |entry| entry.cursor);
		let reset = cursor < oldest || cursor > self.next_cursor;
		let start = if reset { oldest } else { cursor };
		let entries = self
			.entries
			.iter()
			.filter(|entry| entry.cursor >= start)
			.cloned()
			.collect::<Vec<_>>()
			.into();
		AdvisorHistoryDelta { next_cursor: self.next_cursor, reset, entries }
	}
}

impl<T> Default for BoundedAdvisorHistory<T> {
	fn default() -> Self {
		Self::new(DEFAULT_HISTORY_ENTRY_LIMIT, DEFAULT_HISTORY_BYTE_LIMIT)
	}
}

/// Durable advisor child identity and private-context cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorRuntimeState {
	/// Stable child id.
	pub id:             Str,
	/// Owning primary session id.
	pub parent_id:      Str,
	/// Durable display label.
	pub display_name:   Str,
	/// Next primary history cursor to consume.
	pub history_cursor: u64,
	/// Separate advisor usage totals.
	pub input_tokens:   u64,
	/// Separate advisor usage totals.
	pub output_tokens:  u64,
}
/// One accepted note routed to a primary-loop delivery channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutedAdvice {
	/// Source advisor identity.
	pub advisor_id: Str,
	/// Concrete note.
	pub note:       Str,
	/// Effective severity.
	pub severity:   AdviceSeverity,
}

/// Delivery-channel setup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AdvisorRouteError {
	/// The live steering receiver has stopped.
	#[error("advisor steering channel is closed")]
	SteeringClosed,
	/// The idle-preserve receiver has stopped.
	#[error("advisor preserve channel is closed")]
	PreserveClosed,
}

/// Runtime router spanning aside, live-steering, and idle-preserve delivery.
pub struct AdvisorDeliveryRouter {
	asides:   VecDeque<RoutedAdvice>,
	steering: flume::Sender<RoutedAdvice>,
	preserve: flume::Sender<RoutedAdvice>,
	immunity: ImmuneTurnAccount,
}

impl AdvisorDeliveryRouter {
	/// Creates a router and its two externally consumed channels.
	pub fn channel(immune_turns: u32) -> (Self, Receiver<RoutedAdvice>, Receiver<RoutedAdvice>) {
		let (steering, steering_rx) = flume::unbounded();
		let (preserve, preserve_rx) = flume::unbounded();
		(
			Self {
				asides: VecDeque::new(),
				steering,
				preserve,
				immunity: ImmuneTurnAccount::new(immune_turns),
			},
			steering_rx,
			preserve_rx,
		)
	}

	/// Routes one note using current primary-loop facts.
	pub fn route(
		&mut self,
		advice: RoutedAdvice,
		context: DeliveryContext,
	) -> Result<AdviceDelivery, AdvisorRouteError> {
		let delivery = self.immunity.evaluate(advice.severity, context);
		match delivery {
			AdviceDelivery::Aside => self.asides.push_back(advice),
			AdviceDelivery::Steer => {
				self
					.steering
					.send(advice)
					.map_err(|_| AdvisorRouteError::SteeringClosed)?;
				self.immunity.record_steer();
			},
			AdviceDelivery::Preserve => {
				self
					.preserve
					.send(advice)
					.map_err(|_| AdvisorRouteError::PreserveClosed)?;
			},
		}
		Ok(delivery)
	}

	/// Accounts a completed primary turn once for interrupt immunity.
	pub fn record_primary_completion(&mut self, turn_id: u64) {
		self.immunity.record_primary_completion(turn_id);
	}

	/// Drains oldest-to-newest asides at the next primary step boundary.
	pub fn drain_asides(&mut self) -> impl Iterator<Item = RoutedAdvice> + '_ {
		self.asides.drain(..)
	}

	/// Remaining post-steer immune completions.
	pub const fn immune_turns_remaining(&self) -> u32 {
		self.immunity.remaining()
	}
}

/// One obfuscated, fingerprinted advisor-context chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorDeltaChunk {
	/// Absolute primary-history cursor.
	pub cursor:      u64,
	/// Provider-safe projected text.
	pub text:        Str,
	/// Bun-compatible Wyhash of this exact chunk.
	pub fingerprint: u64,
}

/// Coalesced delta ready for one advisor update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorDeltaBatch {
	/// Projection generation; changes whenever primary history is rewritten.
	pub revision:    u64,
	/// Whether the advisor must discard its old context before applying chunks.
	pub reprime:     bool,
	/// Cursor after the newest consumed primary entry.
	pub next_cursor: u64,
	/// Provider-safe oldest-to-newest chunks.
	pub chunks:      Arc<[AdvisorDeltaChunk]>,
}

/// Primary-to-advisor delta synchronization coordinator.
pub struct AdvisorDeltaSync {
	pending:              VecDeque<(u64, Str)>,
	next_cursor:          u64,
	revision:             u64,
	reprime:              bool,
	last_maintenance:     Instant,
	maintenance_interval: Duration,
	obfuscator:           Option<Arc<Mutex<SecretObfuscator>>>,
}

impl AdvisorDeltaSync {
	/// Creates a coordinator with optional session-secret obfuscation.
	pub fn new(
		maintenance_interval: Duration,
		obfuscator: Option<Arc<Mutex<SecretObfuscator>>>,
	) -> Self {
		Self {
			pending: VecDeque::new(),
			next_cursor: 0,
			revision: 0,
			reprime: false,
			last_maintenance: Instant::now(),
			maintenance_interval,
			obfuscator,
		}
	}

	/// Queues one primary projection without dispatching a partial batch.
	pub fn push(&mut self, text: impl Into<Str>) -> u64 {
		let cursor = self.next_cursor;
		self.next_cursor = self.next_cursor.saturating_add(1);
		self.pending.push_back((cursor, text.into()));
		cursor
	}

	/// Invalidates advisor context after rewind, branch, or compaction.
	pub fn history_rewritten(&mut self) {
		self.pending.clear();
		self.revision = self.revision.saturating_add(1);
		self.reprime = true;
		self.next_cursor = self.next_cursor.saturating_add(1);
	}

	/// Reports whether proactive provider-context maintenance is due.
	pub fn maintenance_due(&self, now: Instant) -> bool {
		now.saturating_duration_since(self.last_maintenance) >= self.maintenance_interval
	}

	/// Records successful provider-context maintenance.
	pub fn record_maintenance(&mut self, now: Instant) {
		self.last_maintenance = now;
	}

	/// Coalesces pending arrivals into a bounded number of oldest-first rounds.
	///
	/// Each source entry is split on UTF-8 boundaries, obfuscated, then
	/// fingerprinted. Items arriving after the third round remain queued for the
	/// next advisor update so a hot primary cannot starve dispatch.
	pub fn drain_coalesced(&mut self) -> Option<AdvisorDeltaBatch> {
		if self.pending.is_empty() && !self.reprime {
			return None;
		}
		let mut chunks = Vec::new();
		let mut rounds = 0;
		while rounds < MAX_DELTA_COALESCE_ROUNDS && !self.pending.is_empty() {
			let round_len = self.pending.len();
			for _ in 0..round_len {
				let Some((cursor, text)) = self.pending.pop_front() else {
					break;
				};
				let safe = self.obfuscator.as_ref().map_or_else(
					|| text.to_string(),
					|obfuscator| obfuscator.lock().obfuscate(text.as_str()),
				);
				for chunk in utf8_chunks(&safe, ADVISOR_FINGERPRINT_CHUNK_BYTES) {
					chunks.push(AdvisorDeltaChunk {
						cursor,
						fingerprint: bun_wyhash(chunk.as_bytes()),
						text: Str::new(chunk),
					});
				}
			}
			rounds += 1;
		}
		let reprime = mem::take(&mut self.reprime);
		Some(AdvisorDeltaBatch {
			revision: self.revision,
			reprime,
			next_cursor: self.next_cursor,
			chunks: chunks.into(),
		})
	}
}

impl Default for AdvisorDeltaSync {
	fn default() -> Self {
		Self::new(Duration::from_secs(60), None)
	}
}

fn utf8_chunks(text: &str, limit: usize) -> impl Iterator<Item = &str> {
	let mut start = 0;
	iter::from_fn(move || {
		if start >= text.len() {
			return None;
		}
		let mut end = start.saturating_add(limit.max(1)).min(text.len());
		while !text.is_char_boundary(end) {
			end -= 1;
		}
		let chunk = &text[start..end];
		start = end;
		Some(chunk)
	})
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn immune_account_counts_each_completed_turn_once() {
		let mut account = ImmuneTurnAccount::new(3);
		account.record_steer();
		account.record_primary_completion(10);
		account.record_primary_completion(10);
		assert_eq!(account.remaining(), 2);
		assert_eq!(
			account.evaluate(AdviceSeverity::Concern, DeliveryContext {
				streaming: true,
				..Default::default()
			}),
			AdviceDelivery::Aside
		);
		account.record_primary_completion(11);
		account.record_primary_completion(12);
		assert_eq!(
			account.evaluate(AdviceSeverity::Concern, DeliveryContext {
				streaming: true,
				..Default::default()
			}),
			AdviceDelivery::Steer
		);
	}

	#[test]
	fn bounded_history_requires_reprime_after_eviction_and_rewrite() {
		let mut history = BoundedAdvisorHistory::new(2, 16);
		history.push("one", 3);
		history.push("two", 3);
		history.push("three", 5);
		let evicted = history.delta_after(0);
		assert!(evicted.reset);
		assert_eq!(evicted.entries.len(), 2);

		let cursor = evicted.next_cursor;
		history.reset();
		history.push("replacement", 11);
		let rewritten = history.delta_after(cursor);
		assert!(rewritten.reset);
		assert_eq!(rewritten.entries[0].value, "replacement");
	}
	#[test]
	fn external_interrupt_preserves_advice_after_immunity_expires() {
		let account = ImmuneTurnAccount::new(0);
		let context = DeliveryContext {
			streaming: true,
			externally_interrupted: true,
			..DeliveryContext::default()
		};
		assert_eq!(account.evaluate(AdviceSeverity::Concern, context), AdviceDelivery::Preserve);
		assert_eq!(account.evaluate(AdviceSeverity::Blocker, context), AdviceDelivery::Preserve);
	}
}
