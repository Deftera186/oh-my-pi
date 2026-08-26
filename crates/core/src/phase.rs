//! Shared ordered protocol phase vocabularies.
#![allow(missing_docs, reason = "strum IntoStaticStr emits undocumented inherent methods")]

use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

/// Canonical ordered state of one tool invocation.
/// One fixed decision point in the closed agent loop.
///
/// Discriminants are stable bit positions in [`PointSet`].
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
)]
#[repr(u8)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE", const_into_str)]
pub enum Point {
	/// Context projection and prompt injection.
	Context    = 0,
	/// Resolution of the next required tool.
	ToolChoice = 1,
	/// Final gate before model sampling.
	PreModel   = 2,
	/// Streaming output inspection.
	Stream     = 3,
	/// Per-invocation admission.
	Admission  = 4,
	/// In-flight tool batch supervision.
	///
	/// Resolved twice per committed batch: before execution with
	/// `delivered: false` (admission-side supervision) and after settlement
	/// with `delivered: true` (safe boundary for injecting items ahead of the
	/// staged tool results).
	Batch      = 5,
	/// Observation after a committed turn.
	TurnEnd    = 6,
	/// Settlement and continuation arbitration.
	Settle     = 7,
	/// Idle mailbox wake arbitration.
	Idle       = 8,
}

impl Point {
	/// Every decision point in loop order.
	pub const ALL: [Self; 9] = [
		Self::Context,
		Self::ToolChoice,
		Self::PreModel,
		Self::Stream,
		Self::Admission,
		Self::Batch,
		Self::TurnEnd,
		Self::Settle,
		Self::Idle,
	];

	/// Returns this point's stable bit position.
	pub const fn ordinal(self) -> u8 {
		self as u8
	}

	/// Returns the singleton set containing this point.
	pub const fn set(self) -> PointSet {
		PointSet(1_u16 << self.ordinal())
	}
}

/// Compact subscription mask over the closed [`Point`] vocabulary.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PointSet(u16);

impl PointSet {
	/// Every currently defined decision point.
	pub const ALL: Self = Self((1_u16 << Point::ALL.len()) - 1);
	/// The empty point set.
	pub const EMPTY: Self = Self(0);

	/// Creates a set from raw bits, discarding bits outside the vocabulary.
	pub const fn from_bits(bits: u16) -> Self {
		Self(bits & Self::ALL.0)
	}

	/// Returns the raw stable bit mask.
	pub const fn bits(self) -> u16 {
		self.0
	}

	/// Returns whether the set contains `point`.
	pub const fn contains(self, point: Point) -> bool {
		self.0 & point.set().0 != 0
	}

	/// Returns the union with `other`.
	pub const fn union(self, other: Self) -> Self {
		Self(self.0 | other.0)
	}

	/// Returns the set with `point` inserted.
	pub const fn with(self, point: Point) -> Self {
		self.union(point.set())
	}
}

impl From<Point> for PointSet {
	fn from(point: Point) -> Self {
		point.set()
	}
}

///
/// Each transition fixes additional durable facts. Discriminants are stable
/// protocol vocabulary and therefore match the state-machine order.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
)]
#[repr(u8)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE", const_into_str)]
pub enum InvocationPhase {
	/// Streaming has named the target, but argument emission remains open.
	Open              = 0,
	/// The requested target and canonical requested arguments are fixed.
	ArgsFinalized     = 1,
	/// Admission policy is evaluating the finalized request.
	Admission         = 2,
	/// Policy has fixed the effective arguments and admission receipt.
	Admitted          = 3,
	/// The assistant item containing this invocation is durable.
	AssistantItemCommitted = 4,
	/// Core has issued the invocation's scoped effect token.
	EffectsAuthorized = 5,
	/// The single durable call outcome is fixed.
	Settled           = 6,
}

impl InvocationPhase {
	/// Every invocation phase in canonical transition order.
	pub const ALL: [Self; 7] = [
		Self::Open,
		Self::ArgsFinalized,
		Self::Admission,
		Self::Admitted,
		Self::AssistantItemCommitted,
		Self::EffectsAuthorized,
		Self::Settled,
	];

	/// Returns the stable zero-based protocol discriminant.
	pub const fn ordinal(self) -> u8 {
		self as u8
	}

	/// Returns whether this phase is terminal.
	pub const fn is_terminal(self) -> bool {
		matches!(self, Self::Settled)
	}

	/// Returns whether a direct transition from `self` to `next` is legal.
	pub const fn can_transition_to(self, next: Self) -> bool {
		self.ordinal() + 1 == next.ordinal()
	}

	/// Returns whether this invocation has reached `required`.
	pub const fn has_reached(self, required: Self) -> bool {
		self.ordinal() >= required.ordinal()
	}

	/// Returns whether an operation with `minimum` phase may run now.
	///
	/// Settled invocations cannot start new work even when they reached the
	/// operation's minimum phase earlier.
	pub const fn allows_operation(self, minimum: Self) -> bool {
		!self.is_terminal() && self.has_reached(minimum)
	}
}

/// Ordered lifecycle state of an extension declaration.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
)]
#[repr(u8)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE", const_into_str)]
pub enum LifecyclePhase {
	/// Extension declarations are being collected.
	Declared = 0,
	/// The declaration registry is immutable.
	Frozen   = 1,
	/// Frozen declarations match their authoritative manifest.
	Verified = 2,
	/// The verified extension may receive dispatches.
	Active   = 3,
	/// The extension remains known but must not receive dispatches.
	Degraded = 4,
}

impl LifecyclePhase {
	/// Every lifecycle phase in stable vocabulary order.
	pub const ALL: [Self; 5] =
		[Self::Declared, Self::Frozen, Self::Verified, Self::Active, Self::Degraded];

	/// Returns the stable zero-based vocabulary position.
	pub const fn ordinal(self) -> u8 {
		self as u8
	}
}

/// Coarse reason an extension is being activated.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", const_into_str)]
pub enum ActivateReason {
	/// A declared lazy surface was reached for the first time this session.
	FirstReach = 0,
	/// The host was respawned after a crash or retirement.
	Restart    = 1,
	/// The extension was reloaded in place.
	HotReload  = 2,
}

/// Supervisor-owned cause of an extension-host restart.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", const_into_str)]
pub enum RestartReason {
	/// The child exited or violated protocol.
	Crash            = 0,
	/// A reload request or watched source change restarted the child.
	HotReload        = 1,
	/// Stage three of the cancellation ladder restarted the child.
	CancelEscalation = 2,
	/// The child could not honor a required protocol frame.
	ProtocolError    = 3,
	/// The operating-system memory limiter killed the child.
	Oom              = 4,
	/// The child missed health probes beyond the health timeout.
	HealthTimeout    = 5,
}

impl RestartReason {
	/// Returns the coarse activation reason exposed to extension handlers.
	pub const fn activate_reason(self) -> ActivateReason {
		match self {
			Self::HotReload => ActivateReason::HotReload,
			Self::Crash
			| Self::CancelEscalation
			| Self::ProtocolError
			| Self::Oom
			| Self::HealthTimeout => ActivateReason::Restart,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{ActivateReason, InvocationPhase, LifecyclePhase, Point, PointSet, RestartReason};
	#[test]
	fn point_set_is_a_stable_nine_bit_mask() {
		assert_eq!(PointSet::ALL.bits(), 0x01ff);
		for (ordinal, point) in Point::ALL.into_iter().enumerate() {
			assert_eq!(usize::from(point.ordinal()), ordinal);
			assert!(PointSet::ALL.contains(point));
			assert_eq!(PointSet::from(point).bits(), 1 << ordinal);
		}
		assert_eq!(PointSet::from_bits(u16::MAX), PointSet::ALL);
	}

	#[test]
	fn discriminants_and_transitions_are_canonical() {
		for (ordinal, phase) in InvocationPhase::ALL.into_iter().enumerate() {
			assert_eq!(usize::from(phase.ordinal()), ordinal);
			assert_eq!(phase.is_terminal(), phase == InvocationPhase::Settled);
		}
		for pair in InvocationPhase::ALL.windows(2) {
			assert!(pair[0].can_transition_to(pair[1]));
		}
		assert!(!InvocationPhase::Open.can_transition_to(InvocationPhase::Admission));
		assert!(!InvocationPhase::Settled.can_transition_to(InvocationPhase::Settled));
	}

	#[test]
	fn operation_gate_requires_minimum_and_nonterminal_phase() {
		assert!(!InvocationPhase::Admitted.allows_operation(InvocationPhase::EffectsAuthorized));
		assert!(
			InvocationPhase::EffectsAuthorized.allows_operation(InvocationPhase::EffectsAuthorized)
		);
		assert!(!InvocationPhase::Settled.allows_operation(InvocationPhase::Open));
	}
	#[test]
	fn lifecycle_and_reason_vocabularies_are_exact() {
		for (ordinal, (phase, name)) in LifecyclePhase::ALL
			.into_iter()
			.zip(["DECLARED", "FROZEN", "VERIFIED", "ACTIVE", "DEGRADED"])
			.enumerate()
		{
			assert_eq!(usize::from(phase.ordinal()), ordinal);
			assert_eq!(phase.to_string(), name);
			assert_eq!(name.parse::<LifecyclePhase>(), Ok(phase));
		}
		for (reason, name) in [
			(ActivateReason::FirstReach, "first_reach"),
			(ActivateReason::Restart, "restart"),
			(ActivateReason::HotReload, "hot_reload"),
		] {
			assert_eq!(reason.to_string(), name);
			assert_eq!(name.parse::<ActivateReason>(), Ok(reason));
		}
		for (reason, name) in [
			(RestartReason::Crash, "crash"),
			(RestartReason::HotReload, "hot_reload"),
			(RestartReason::CancelEscalation, "cancel_escalation"),
			(RestartReason::ProtocolError, "protocol_error"),
			(RestartReason::Oom, "oom"),
			(RestartReason::HealthTimeout, "health_timeout"),
		] {
			assert_eq!(reason.to_string(), name);
			assert_eq!(name.parse::<RestartReason>(), Ok(reason));
			let expected = if reason == RestartReason::HotReload {
				ActivateReason::HotReload
			} else {
				ActivateReason::Restart
			};
			assert_eq!(reason.activate_reason(), expected);
		}
	}
}
