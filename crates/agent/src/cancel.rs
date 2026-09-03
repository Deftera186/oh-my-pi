//! Hierarchical cancellation for sessions, turns, and tool executions.
//!
//! Turn interruption never reaches an in-flight foreground mutation. Only
//! session cancellation can stop that scope, preventing partially applied
//! mutations from being reported as a clean turn interrupt.

use tokio_util::sync::CancellationToken;

/// Root of a session cancellation hierarchy.
#[derive(Clone, Debug)]
pub struct CancelTree {
	session: CancellationToken,
}

impl CancelTree {
	/// Creates a live session cancellation tree.
	#[must_use]
	pub fn new() -> Self {
		Self { session: CancellationToken::new() }
	}

	/// Cancels the session and every current or future descendant.
	pub fn cancel_session(&self) {
		self.session.cancel();
	}

	/// Reports whether the session has been cancelled.
	#[must_use]
	pub fn is_session_cancelled(&self) -> bool {
		self.session.is_cancelled()
	}

	/// Starts one cancellation scope beneath the session root.
	#[must_use]
	pub fn begin_turn(&self) -> TurnCancellation {
		TurnCancellation { session: self.session.clone(), turn: self.session.child_token() }
	}
}

impl Default for CancelTree {
	fn default() -> Self {
		Self::new()
	}
}

/// Cancellation scope for one turn.
#[derive(Clone, Debug)]
pub struct TurnCancellation {
	session: CancellationToken,
	turn:    CancellationToken,
}

impl TurnCancellation {
	/// Interrupts this turn and its interruptible tools, but not an in-flight
	/// foreground mutation.
	pub fn cancel_turn(&self) {
		self.turn.cancel();
	}

	/// Reports whether this turn was interrupted, including session
	/// cancellation.
	#[must_use]
	pub fn is_turn_cancelled(&self) -> bool {
		self.turn.is_cancelled()
	}

	/// Issues the session-only scope available to foreground mutations.
	#[must_use]
	pub fn foreground_mutation(&self) -> ForegroundMutationCancellation {
		ForegroundMutationCancellation { token: self.session.clone() }
	}

	/// Issues a turn-scoped child token for a read-only tool.
	#[must_use]
	pub fn read_only_tool(&self) -> ReadOnlyToolCancellation {
		ReadOnlyToolCancellation { token: self.turn.child_token() }
	}

	/// Issues a turn-scoped child token for background work.
	#[must_use]
	pub fn background_tool(&self) -> BackgroundToolCancellation {
		BackgroundToolCancellation { token: self.turn.child_token() }
	}
}

/// Session-only cancellation issued to a foreground mutating tool.
#[derive(Clone, Debug)]
pub struct ForegroundMutationCancellation {
	token: CancellationToken,
}

impl ForegroundMutationCancellation {
	/// Returns the session cancellation token.
	#[must_use]
	pub fn token(&self) -> CancellationToken {
		self.token.clone()
	}

	/// Reports whether the owning session was cancelled.
	#[must_use]
	pub fn is_cancelled(&self) -> bool {
		self.token.is_cancelled()
	}
}

/// Turn/tool cancellation issued to a read-only tool.
#[derive(Clone, Debug)]
pub struct ReadOnlyToolCancellation {
	token: CancellationToken,
}

impl ReadOnlyToolCancellation {
	/// Returns the cancellation token.
	#[must_use]
	pub fn token(&self) -> CancellationToken {
		self.token.clone()
	}

	/// Cancels this tool without cancelling its turn or session.
	pub fn cancel_tool(&self) {
		self.token.cancel();
	}

	/// Reports whether this tool, its turn, or its session was cancelled.
	#[must_use]
	pub fn is_cancelled(&self) -> bool {
		self.token.is_cancelled()
	}
}

/// Turn/tool cancellation issued to background work.
#[derive(Clone, Debug)]
pub struct BackgroundToolCancellation {
	token: CancellationToken,
}

impl BackgroundToolCancellation {
	/// Adopts a host-supervised cancellation token for a session-owned job.
	#[must_use]
	pub const fn from_token_for_host(token: CancellationToken) -> Self {
		Self { token }
	}

	pub(crate) const fn from_token(token: CancellationToken) -> Self {
		Self { token }
	}

	/// Returns the cancellation token.
	#[must_use]
	pub fn token(&self) -> CancellationToken {
		self.token.clone()
	}

	/// Cancels this tool without cancelling its turn or session.
	pub fn cancel_tool(&self) {
		self.token.cancel();
	}

	/// Reports whether this tool, its turn, or its session was cancelled.
	#[must_use]
	pub fn is_cancelled(&self) -> bool {
		self.token.is_cancelled()
	}
}
