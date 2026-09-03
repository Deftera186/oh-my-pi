//! One upward mailbox for steering and cancellation.

use omp_core::Str;
use omp_dom::{Handle, KnownTag, NodeSpec, Op, PropId, Tag, Txn, Value};
use omp_session::{Session, SessionError};

pub(crate) const EMPTY_OUTPUT_RETRY_CAP: u8 = 3;
const EMPTY_OUTPUT_CAP_NOTICE: &str =
	"Assistant returned no final output after retry cap; try switching models";

/// Control sent to a running kernel turn.
#[derive(Clone, Debug)]
pub enum Up {
	/// Adds a user steering aside at the next safe point.
	Steer(Str),
	/// Interrupts the current inference/tool turn while preserving mutations.
	Interrupt,
	/// Cancels the whole session and every execution scope.
	Cancel,
	/// Delivers an environment observation or host-authority request.
	Env(crate::EnvEvent),
	/// Resolves a journal-backed approval prompt.
	Approve {
		/// Stable prompt identity.
		id:       Str,
		/// Idempotent first decision.
		decision: crate::ApprovalDecision,
	},
}

pub(crate) fn append_steering(
	session: &mut Session,
	turn: Handle,
	text: Str,
) -> Result<(), SessionError> {
	let steering = session
		.dom()
		.children(session.dom().queues())
		.iter()
		.copied()
		.find(|handle| {
			session
				.dom()
				.get(*handle)
				.is_some_and(|node| node.tag == Tag::Known(KnownTag::Steering))
		})
		.ok_or(SessionError::NoActiveTurn)?;
	let cause = session.head().ok_or(SessionError::NoActiveTurn)?;
	session.patch(Txn {
		cause,
		label: Some(Str::new_static("steering.safe-point")),
		ops: vec![
			Op::Ins {
				parent: steering,
				after:  session.dom().children(steering).last().copied(),
				node:   NodeSpec::new(KnownTag::User).with_content(text.clone()),
			},
			Op::Ins {
				parent: turn,
				after:  session.dom().children(turn).last().copied(),
				node:   NodeSpec::new(KnownTag::Developer).with_content(text),
			},
		],
	})?;
	Ok(())
}

pub(crate) fn append_notice(
	session: &mut Session,
	turn: Handle,
	text: Str,
) -> Result<(), SessionError> {
	append_notice_with_kind(session, turn, text, Str::new_static("info"))
}

/// Appends a `<notice kind=error>` describing why the turn failed.
pub(crate) fn append_error_notice(
	session: &mut Session,
	turn: Handle,
	text: Str,
) -> Result<(), SessionError> {
	append_notice_with_kind(session, turn, text, Str::new_static("error"))
}

pub(crate) fn append_empty_output_retry(
	session: &mut Session,
	turn: Handle,
	attempt: u8,
) -> Result<(), SessionError> {
	append_turn_child(
		session,
		turn,
		NodeSpec::new(KnownTag::Developer).with_content(Str::new(format!(
			"<system-injection>\nStopped without actionable output; task incomplete. Continue with a \
			 user-visible final answer or the next required tool call.\nAttempt \
			 #{attempt}/{EMPTY_OUTPUT_RETRY_CAP}\n</system-injection>"
		))),
		Str::new_static("kernel.empty-output-retry"),
	)
}

pub(crate) fn append_empty_output_cap_notice(
	session: &mut Session,
	turn: Handle,
) -> Result<(), SessionError> {
	append_notice_with_kind(
		session,
		turn,
		Str::new_static(EMPTY_OUTPUT_CAP_NOTICE),
		Str::new_static("error"),
	)
}

fn append_notice_with_kind(
	session: &mut Session,
	turn: Handle,
	text: Str,
	kind: Str,
) -> Result<(), SessionError> {
	append_turn_child(
		session,
		turn,
		NodeSpec::new(KnownTag::Notice)
			.with_prop(PropId::Kind, Value::Str(kind))
			.with_content(text),
		Str::new_static("kernel.notice"),
	)
}

fn append_turn_child(
	session: &mut Session,
	turn: Handle,
	node: NodeSpec,
	label: Str,
) -> Result<(), SessionError> {
	session.patch(Txn {
		cause: session.head().ok_or(SessionError::NoActiveTurn)?,
		label: Some(label),
		ops:   vec![Op::Ins {
			parent: turn,
			after: session.dom().children(turn).last().copied(),
			node,
		}],
	})?;
	Ok(())
}
