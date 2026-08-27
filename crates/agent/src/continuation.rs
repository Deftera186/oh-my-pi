//! Settled-boundary continuation decisions and recursive ledger accounting.

use std::{future::Future, pin::Pin, time::Duration};

use bytes::BytesMut;
use omp_core::Str;
use omp_proto::{thread::v1::Item, toolhost::v1::HookEventId};

use crate::{
	hooks::{AgentSettled, GateError, HookEvent, HookPatch},
	mailbox::InterruptSource,
};

/// Committed recovery evidence offered to a cold OAuth redemption authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedemptionEvidence {
	/// Preserve a committed partial generation.
	Salvage {
		/// Durable turn identity.
		turn_id: Str,
	},
	/// Restore a turn that produced no usable output.
	Restore {
		/// Durable turn identity.
		turn_id: Str,
	},
	/// A compaction replaced provider history at this journal epoch.
	PostCompaction {
		/// Compaction journal event.
		epoch: u64,
	},
}

/// Cold future allocated only around real OAuth redemption I/O.
pub type RedemptionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// App-owned authority bridging loop evidence to provider redemption.
///
/// This is deliberately the cold dynamic boundary sanctioned for real network
/// I/O; the hot arbiter fold remains allocation-free.
pub trait RedemptionAuthority: Send + Sync + 'static {
	/// Attempts redemption for committed typed evidence.
	fn redeem(&self, evidence: RedemptionEvidence) -> RedemptionFuture<'_, bool>;

	/// Reseeds provider-native history after a successful redemption.
	fn reseed_history(&self) -> RedemptionFuture<'_, ()>;
}

/// Consecutive-continuation accounting projected from durable journal facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContinuationLedger {
	/// Accepted continuations since the last real user item.
	pub consecutive: u32,
	/// Total accepted continuations over the agent lifetime.
	pub total:       u64,
	/// Effective cap after policy and ancestor clamping.
	pub cap:         u32,
	/// Epoch milliseconds of the last accepted continuation.
	pub last_ms:     u64,
	/// Count of explicit refusals, which callers must journal rather than drop.
	pub refusals:    u32,
	/// Extension that won the latest continuation decision.
	pub owner:       Option<Str>,
}
/// Per-owner bounds applied before the session-wide continuation ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContinuationPolicy {
	/// Maximum consecutive continuations since real user input.
	pub max_consecutive:  u32,
	/// Optional lifetime continuation ceiling.
	pub max_total:        Option<u64>,
	/// Minimum spacing between accepted continuations.
	pub min_interval:     Duration,
	/// Whether exhaustion should produce a user-visible notification.
	pub notify_exhausted: bool,
}

impl Default for ContinuationPolicy {
	fn default() -> Self {
		Self {
			max_consecutive:  8,
			max_total:        None,
			min_interval:     Duration::ZERO,
			notify_exhausted: true,
		}
	}
}

/// Core-owned repetition and progress evidence consumed by autonomous modes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoopSignal {
	/// Consecutive turns with the same committed tool-call digest.
	pub repeats:              u32,
	/// Stable digest of the latest committed tool-call shape.
	pub digest:               Option<Str>,
	/// Consecutive turns without an environment effect.
	pub no_progress_turns:    u32,
	/// Empty-output retries already spent by the core.
	pub empty_output_retries: u8,
	/// Conservative composite used to stop autonomous continuation.
	pub stalled:              bool,
}

impl LoopSignal {
	/// Folds one committed turn into bounded loop evidence.
	pub fn observe(
		&mut self,
		digest: Option<Str>,
		made_environment_effect: bool,
		empty_output_retries: u8,
	) {
		self.repeats = if digest.is_some() && digest == self.digest {
			self.repeats.saturating_add(1)
		} else {
			u32::from(digest.is_some())
		};
		self.digest = digest;
		self.no_progress_turns = if made_environment_effect {
			0
		} else {
			self.no_progress_turns.saturating_add(1)
		};
		self.empty_output_retries = empty_output_retries.min(3);
		self.stalled =
			self.repeats >= 3 || self.no_progress_turns >= 3 || self.empty_output_retries >= 3;
	}
}
/// Application-owned autonomous-mode decision consumed only at the settled
/// boundary.
pub trait ContinuationSource: Send + Sync {
	/// Returns a candidate and its owner policy from Core loop evidence.
	fn decide(&self, signal: &LoopSignal, now_ms: u64) -> (Continuation, ContinuationPolicy);
	/// Refreshes application projections after an automatic core regime
	/// transition.
	fn sync_regimes(&self, _regimes: &crate::RegimeSet) {}
}
/// Built-in participant lanes evaluated by the SETTLE arbiter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettledParticipant {
	/// Application-owned autonomous continuation source.
	ContinuationSource,
	/// Stateless `AgentSettled` domain hook.
	AgentSettled,
}

/// Degenerate built-in SETTLE fold preceding regime resolution.
pub struct SettledFold {
	candidate: Continuation,
	policy:    ContinuationPolicy,
	winner:    Option<SettledParticipant>,
}

impl Default for SettledFold {
	fn default() -> Self {
		Self {
			candidate: Continuation::Settle,
			policy:    ContinuationPolicy::default(),
			winner:    None,
		}
	}
}

impl SettledFold {
	/// Creates an empty settle fold.
	pub fn new() -> Self {
		Self::default()
	}

	/// Considers a participant in priority order; the first continuation wins.
	pub fn consider(
		&mut self,
		participant: SettledParticipant,
		candidate: Continuation,
		policy: ContinuationPolicy,
	) {
		if matches!(self.candidate, Continuation::Settle)
			&& matches!(candidate, Continuation::Continue { .. })
		{
			self.candidate = candidate;
			self.policy = policy;
			self.winner = Some(participant);
		}
	}

	/// Applies an explicit settled-hook veto to every earlier continuation.
	pub fn veto(&mut self) {
		self.candidate = Continuation::Settle;
		self.policy = ContinuationPolicy::default();
		self.winner = None;
	}

	/// Returns the winning participant, if a lane vetoed settlement.
	pub const fn winner(&self) -> Option<SettledParticipant> {
		self.winner
	}

	/// Consumes the fold into the candidate and its owner policy.
	pub fn into_parts(self) -> (Continuation, ContinuationPolicy) {
		(self.candidate, self.policy)
	}
}

impl ContinuationLedger {
	/// Creates a zeroed ledger with an already-clamped cap.
	pub const fn new(cap: u32) -> Self {
		Self { consecutive: 0, total: 0, cap, last_ms: 0, refusals: 0, owner: None }
	}

	/// Resets the consecutive count after a real user item.
	pub const fn reset_for_user(&mut self) {
		self.consecutive = 0;
	}

	/// Applies one candidate decision, returning a refusal that must be
	/// journaled.
	pub fn decide(&mut self, candidate: Continuation, now_ms: u64) -> Continuation {
		match candidate {
			Continuation::Continue { .. } if self.consecutive >= self.cap => {
				self.refusals = self.refusals.saturating_add(1);
				Continuation::Refused { cap: self.cap }
			},
			Continuation::Continue { owner, item, label, collapse_prior } => {
				self.consecutive = self.consecutive.saturating_add(1);
				self.total = self.total.saturating_add(1);
				self.last_ms = now_ms;
				self.owner = Some(owner.clone());
				Continuation::Continue { owner, item, label, collapse_prior }
			},
			other => other,
		}
	}

	/// Applies one candidate under both an owner policy and the session cap.
	pub fn decide_with_policy(
		&mut self,
		candidate: Continuation,
		now_ms: u64,
		policy: ContinuationPolicy,
	) -> Continuation {
		let effective_cap = self.cap.min(policy.max_consecutive);
		let exhausted = self.consecutive >= effective_cap
			|| policy
				.max_total
				.is_some_and(|maximum| self.total >= maximum)
			|| (self.last_ms != 0
				&& now_ms.saturating_sub(self.last_ms)
					< u64::try_from(policy.min_interval.as_millis()).unwrap_or(u64::MAX));
		if matches!(candidate, Continuation::Continue { .. }) && exhausted {
			self.refusals = self.refusals.saturating_add(1);
			return Continuation::Refused { cap: effective_cap };
		}
		self.decide(candidate, now_ms)
	}
}

/// What the settled boundary decided for the next loop action.
#[derive(Clone, Debug, PartialEq)]
pub enum Continuation {
	/// Leave the agent settled.
	Settle,
	/// Start another turn with a canonical item after deferred-interrupt
	/// handling.
	Continue {
		/// Extension that requested the continuation.
		owner:          Str,
		/// Canonical item appended through the normal mailbox path.
		item:           Item,
		/// Optional telemetry and journal label.
		label:          Option<Str>,
		/// Whether an earlier continuation item is replaced.
		collapse_prior: bool,
	},
	/// A cap refusal that is retained as a durable ledger fact.
	Refused {
		/// Effective cap that rejected the candidate.
		cap: u32,
	},
}

/// Read-only actionable todo reference carried by the settled hook.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TodoRef {
	/// Stable phase label.
	pub phase:  Str,
	/// User-visible task text.
	pub text:   Str,
	/// Actionable status (`pending` or `in_progress`).
	pub status: Str,
}

/// Hook payload emitted at the settled boundary.
#[derive(Clone, Debug)]
pub struct AgentSettledEvent {
	/// Stable agent identity.
	pub agent_id:         Str,
	/// The terminal turn id that reached the boundary.
	pub turn_id:          Str,
	/// Ordered actionable built-in todo snapshot.
	pub incomplete_todos: Box<[TodoRef]>,
}

impl HookEvent for AgentSettledEvent {
	type Return = AgentSettled;

	const ID: HookEventId = HookEventId::HookEventAgentSettled;
	const REV: u32 = 2;

	fn encode_into(&self, out: &mut BytesMut) {
		let payload = serde_json::json!({
			"agent_id": self.agent_id,
			"turn_id": self.turn_id,
			"incomplete_todos": self.incomplete_todos,
		});
		if let Ok(encoded) = serde_json::to_vec(&payload) {
			out.extend_from_slice(&encoded);
		}
	}

	fn apply(&mut self, _: &HookPatch) -> Result<(), GateError> {
		// Domain events never accept transforms.
		Ok(())
	}
}

/// Converts a hook's fail-open settled result into a loop continuation.
pub fn from_hook(result: AgentSettled, owner: Str, item: Item) -> Continuation {
	match result {
		AgentSettled::Continue => {
			Continuation::Continue { owner, item, label: None, collapse_prior: false }
		},
		AgentSettled::Settle => Continuation::Settle,
	}
}

/// Returns whether an interrupt source is permitted to start another loop turn.
///
/// Detached job settlement is deliberately excluded: job facts are next-turn
/// data, not an autonomous-loop signal.
pub const fn continues_loop(source: &InterruptSource) -> bool {
	matches!(
		source,
		InterruptSource::Producer(_)
			| InterruptSource::Continuation { .. }
			| InterruptSource::Schedule { .. }
			| InterruptSource::Peer { .. }
			| InterruptSource::Remote { .. }
			| InterruptSource::DeferredDiagnostics { .. }
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn owner_policy_clamps_and_spaces_continuations() {
		let mut ledger = ContinuationLedger::new(8);
		let policy = ContinuationPolicy {
			max_consecutive:  2,
			max_total:        Some(4),
			min_interval:     Duration::from_millis(10),
			notify_exhausted: true,
		};
		let candidate = || Continuation::Continue {
			owner:          sf!("goal"),
			item:           Item::default(),
			label:          None,
			collapse_prior: true,
		};
		assert!(matches!(
			ledger.decide_with_policy(candidate(), 100, policy),
			Continuation::Continue { .. }
		));
		assert_eq!(ledger.decide_with_policy(candidate(), 105, policy), Continuation::Refused {
			cap: 2,
		});
		assert!(matches!(
			ledger.decide_with_policy(candidate(), 110, policy),
			Continuation::Continue { .. }
		));
		assert_eq!(ledger.decide_with_policy(candidate(), 120, policy), Continuation::Refused {
			cap: 2,
		});
	}

	#[test]
	fn settled_revision_two_encodes_ordered_actionable_todos() {
		let event = AgentSettledEvent {
			agent_id:         sf!("agent"),
			turn_id:          sf!("turn"),
			incomplete_todos: vec![
				TodoRef { phase: sf!("Build"), text: sf!("compile"), status: sf!("in_progress") },
				TodoRef { phase: sf!("Ship"), text: sf!("publish"), status: sf!("pending") },
			]
			.into_boxed_slice(),
		};
		assert_eq!(<AgentSettledEvent as HookEvent>::REV, 2);
		let mut encoded = BytesMut::new();
		event.encode_into(&mut encoded);
		let payload: serde_json::Value = serde_json::from_slice(&encoded).expect("settled JSON");
		assert_eq!(payload["incomplete_todos"][0]["phase"], "Build");
		assert_eq!(payload["incomplete_todos"][0]["status"], "in_progress");
		assert_eq!(payload["incomplete_todos"][1]["text"], "publish");
	}

	#[test]
	fn loop_signal_detects_repetition_and_no_progress() {
		let mut signal = LoopSignal::default();
		for _ in 0..3 {
			signal.observe(Some(sf!("same")), false, 0);
		}
		assert_eq!(signal.repeats, 3);
		assert_eq!(signal.no_progress_turns, 3);
		assert!(signal.stalled);
	}

	#[test]
	fn deferable_continuation_source_continues_the_loop() {
		assert!(continues_loop(&InterruptSource::Continuation { owner: sf!("goal") }));
	}

	#[test]
	fn schedule_source_continues_the_loop() {
		assert!(continues_loop(&InterruptSource::Schedule { id: sf!("nightly") }));
	}

	#[test]
	fn peer_source_continues_the_loop() {
		assert!(continues_loop(&InterruptSource::Peer { from: sf!("reviewer") }));
	}
}
