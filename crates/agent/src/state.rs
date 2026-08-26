//! Immutable, watch-published configuration for agent turns.

use std::{
	fmt,
	num::NonZeroU32,
	sync::Arc,
	time::{Duration, Instant},
};

use omp_core::Str;
use omp_scribe::Props;
use omp_tool::Registry;
use thiserror::Error;
use tokio::sync::watch::{self, Receiver};

use crate::{
	CompactionMethodOrder, InterruptedReasoningDialect, TurnOptions,
	prompt::{CanonicalPromptSource, PromptError, PromptSource, RenderedPrompt, render_prompt},
};

/// Automatic recovery policy for unexpectedly terminated assistant turns.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	serde::Serialize,
	serde::Deserialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum UnexpectedStopMode {
	/// Disable automatic unexpected-stop recovery.
	None,
	/// Retry terminal turns that produced no visible message or tool call.
	#[default]
	Mechanical,
	/// Add a small-model classifier for text-only terminal turns.
	Smart,
}
/// Delivery cardinality for queued steering messages.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	serde::Serialize,
	serde::Deserialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum SteeringMode {
	/// Deliver one queued steering message at each injection boundary.
	#[default]
	OneAtATime,
	/// Deliver every queued steering message at the same injection boundary.
	All,
}

impl SteeringMode {
	/// Maximum steering messages admitted by one mailbox drain.
	pub const fn delivery_limit(self) -> usize {
		match self {
			Self::OneAtATime => 1,
			Self::All => usize::MAX,
		}
	}
}

/// Context-overflow promotion configured for the active model route.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextPromotionPolicy {
	/// Whether promotion may run before context compaction.
	pub enabled: bool,
	/// Eligible larger-context route selected by the catalog owner.
	pub target:  Option<Str>,
}

/// Synchronous compaction check performed only between tool-loop requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidTurnCompactionPolicy {
	/// Whether safe-boundary checks are enabled.
	pub enabled:          bool,
	/// Context occupancy that triggers compaction.
	pub threshold_tokens: u64,
}

impl Default for MidTurnCompactionPolicy {
	fn default() -> Self {
		Self { enabled: true, threshold_tokens: u64::MAX }
	}
}

/// Bounded loop-level retry policy for recoverable turn failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
	max_attempts:    NonZeroU32,
	initial_backoff: Duration,
	max_backoff:     Duration,
}

impl RetryPolicy {
	/// Creates a bounded retry policy.
	///
	/// `max_attempts` includes the initial submission. The maximum backoff must
	/// not be shorter than the initial backoff.
	pub fn new(
		max_attempts: NonZeroU32,
		initial_backoff: Duration,
		max_backoff: Duration,
	) -> Result<Self, RetryPolicyError> {
		if initial_backoff > max_backoff {
			return Err(RetryPolicyError::BackoffOrder);
		}
		Ok(Self { max_attempts, initial_backoff, max_backoff })
	}

	/// Maximum submissions of one stable turn identity, including the first.
	#[inline]
	pub const fn max_attempts(self) -> NonZeroU32 {
		self.max_attempts
	}

	/// Backoff used for the first retry.
	#[inline]
	pub const fn initial_backoff(self) -> Duration {
		self.initial_backoff
	}

	/// Upper bound applied to retry backoff.
	#[inline]
	pub const fn max_backoff(self) -> Duration {
		self.max_backoff
	}
}

impl Default for RetryPolicy {
	fn default() -> Self {
		Self {
			max_attempts:    NonZeroU32::new(3).expect("three is non-zero"),
			initial_backoff: Duration::from_millis(250),
			max_backoff:     Duration::from_secs(4),
		}
	}
}

/// Invalid retry-policy configuration.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RetryPolicyError {
	/// The initial delay exceeded its declared upper bound.
	#[error("initial retry backoff exceeds maximum backoff")]
	BackoffOrder,
}

/// Immutable authoritative configuration consumed by one agent turn.
///
/// A loop reads a fresh snapshot before every logical turn. Cloning a snapshot
/// shares its registry, prompt source, tool names, workspace bytes, and context
/// files.
#[derive(Clone)]
pub struct AgentSnapshot {
	/// Per-turn arbiter options.
	pub turn:                TurnOptions,
	/// Names of tools enabled for this turn, in stable publication order.
	pub enabled_tools:       Arc<[Str]>,
	/// Live revisioned tools used for advertisement, projection, and execution.
	pub registry:            Arc<Registry>,
	/// Immutable workspace and context-file input.
	pub props:               Props,
	/// Synchronous source used to construct the canonical prompt head.
	pub prompt_source:       Arc<dyn PromptSource>,
	/// Dialect policy governing hidden continuity after interrupted reasoning.
	pub reasoning_dialect:   InterruptedReasoningDialect,
	/// Whether immediate interrupts are demoted to turn-boundary interrupts.
	pub defer_interrupts:    bool,
	/// Queued steering delivery cardinality.
	pub steering_mode:       SteeringMode,
	/// Absolute deadline for the active logical turn, when bounded by the host.
	pub deadline:            Option<Instant>,
	/// Bounded loop-level recovery policy.
	pub retry:               RetryPolicy,
	/// Ordered context-overflow recovery ladder; empty disables automatic
	/// recovery compaction.
	pub compaction:          CompactionMethodOrder,
	/// Larger-context model promotion attempted before overflow compaction.
	pub context_promotion:   ContextPromotionPolicy,
	/// Safe tool-loop-boundary compaction threshold policy.
	pub mid_turn_compaction: MidTurnCompactionPolicy,
	/// Unexpected assistant-stop recovery policy.
	pub unexpected_stop:     UnexpectedStopMode,
}

impl AgentSnapshot {
	/// Creates a snapshot with one live registry and the deterministic workspace
	/// prompt source.
	pub fn new(turn: TurnOptions, props: Props, registry: Arc<Registry>) -> Self {
		Self {
			turn,
			enabled_tools: Arc::from([]),
			registry,
			props,
			prompt_source: Arc::new(CanonicalPromptSource),
			reasoning_dialect: InterruptedReasoningDialect::Other,
			defer_interrupts: false,
			steering_mode: SteeringMode::default(),
			deadline: None,
			retry: RetryPolicy::default(),
			compaction: CompactionMethodOrder::default(),
			context_promotion: ContextPromotionPolicy::default(),
			mid_turn_compaction: MidTurnCompactionPolicy::default(),
			unexpected_stop: UnexpectedStopMode::Mechanical,
		}
	}

	/// Renders the prompt twice and returns only a deterministic canonical head.
	#[inline]
	pub fn render_prompt(&self) -> Result<RenderedPrompt, PromptError> {
		render_prompt(self.prompt_source.as_ref(), &self.props)
	}
}

impl Default for AgentSnapshot {
	fn default() -> Self {
		Self::new(TurnOptions::default(), Props::default(), Arc::new(Registry::new()))
	}
}

impl fmt::Debug for AgentSnapshot {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("AgentSnapshot")
			.field("turn", &self.turn)
			.field("enabled_tools", &self.enabled_tools)
			.field("registry_hash", &self.registry.slot_hash())
			.field("props", &self.props)
			.field("prompt_source", &format_args!("<dyn PromptSource>"))
			.field("reasoning_dialect", &self.reasoning_dialect)
			.field("defer_interrupts", &self.defer_interrupts)
			.field("steering_mode", &self.steering_mode)
			.field("deadline", &self.deadline)
			.field("retry", &self.retry)
			.field("compaction", &self.compaction)
			.field("context_promotion", &self.context_promotion)
			.field("mid_turn_compaction", &self.mid_turn_compaction)
			.field("unexpected_stop", &self.unexpected_stop)
			.finish()
	}
}

/// Authoritative agent configuration published as immutable snapshots.
///
/// Readers clone the current [`Arc`] directly from a Tokio watch value. An
/// update clones the prior snapshot, applies one synchronous mutation, and
/// atomically replaces the published pointer while holding the watch slot.
#[derive(Clone, Debug)]
pub struct AgentState {
	sender: watch::Sender<Arc<AgentSnapshot>>,
}

impl AgentState {
	/// Creates state with one initially published snapshot.
	pub fn new(initial: AgentSnapshot) -> Self {
		let (sender, _receiver) = watch::channel(Arc::new(initial));
		Self { sender }
	}

	/// Returns the currently published immutable snapshot.
	#[inline]
	pub fn snapshot(&self) -> Arc<AgentSnapshot> {
		self.sender.borrow().clone()
	}

	/// Subscribes to future snapshot publications.
	///
	/// The receiver's current value is the snapshot published at subscription
	/// time; lagging readers observe the newest value without an update queue.
	#[inline]
	pub fn subscribe(&self) -> Receiver<Arc<AgentSnapshot>> {
		self.sender.subscribe()
	}

	/// Atomically derives and publishes a new snapshot from the current value.
	///
	/// Concurrent callers are serialized by the watch slot, so each closure sees
	/// the snapshot published by the preceding update rather than losing writes.
	pub fn update(&self, update: impl FnOnce(&mut AgentSnapshot)) -> Arc<AgentSnapshot> {
		let mut update = Some(update);
		let mut published = None;
		self.sender.send_modify(|current| {
			let mut next = (**current).clone();
			update.take().expect("watch invokes update once")(&mut next);
			let next = Arc::new(next);
			published = Some(next.clone());
			*current = next;
		});
		published.expect("watch invokes update once")
	}

	/// Atomically replaces and returns the previously published snapshot.
	#[inline]
	pub fn replace(&self, next: AgentSnapshot) -> Arc<AgentSnapshot> {
		self.sender.send_replace(Arc::new(next))
	}
}

impl Default for AgentState {
	fn default() -> Self {
		Self::new(AgentSnapshot::default())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn runtime_policy_defaults_match_pi() {
		let snapshot = AgentSnapshot::default();
		assert_eq!(snapshot.steering_mode, SteeringMode::OneAtATime);
		assert!(!snapshot.context_promotion.enabled);
		assert!(snapshot.mid_turn_compaction.enabled);
	}

	#[test]
	fn update_publishes_a_new_immutable_snapshot() {
		let state = AgentState::default();
		let old = state.snapshot();
		let receiver = state.subscribe();
		let published = state.update(|snapshot| snapshot.defer_interrupts = true);

		assert!(!old.defer_interrupts);
		assert!(published.defer_interrupts);
		assert!(Arc::ptr_eq(&published, &state.snapshot()));
		assert!(receiver.has_changed().expect("sender remains alive"));
	}

	#[test]
	fn sequential_updates_derive_from_latest_publication() {
		let state = AgentState::default();
		let original_registry = state.snapshot().registry.clone();
		let replacement_registry = Arc::new(Registry::new());
		state.update(|snapshot| {
			snapshot.enabled_tools = Arc::from([sf!("read")]);
			snapshot.registry = replacement_registry.clone();
		});
		let published = state.update(|snapshot| snapshot.defer_interrupts = true);

		assert_eq!(published.enabled_tools.as_ref(), &[sf!("read")]);
		assert!(!Arc::ptr_eq(&original_registry, &published.registry));
		assert!(Arc::ptr_eq(&replacement_registry, &published.registry));
		assert!(published.defer_interrupts);
	}
}
