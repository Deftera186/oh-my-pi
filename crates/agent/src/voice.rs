//! Core bridge between provider-neutral realtime delegation and durable agent
//! turns.
//!
//! The bridge only prepares ordinary [`crate::Agent::submit`] inputs and
//! projects its existing event stream. It never owns a second turn loop,
//! transcript, or journal.

use std::{collections::BTreeMap, iter, string::FromUtf8Error, sync::LazyLock, time::SystemTime};

use omp_core::Str;
use omp_inference::{
	RealtimeContextAppend, RealtimeContextChannel, RealtimeContextTarget, RealtimeDelegation,
	RealtimeDelegationReceipt, RealtimeDelegationStatus, RealtimeInput, TurnId,
};
use omp_proto::{
	inference::v1::{Outcome, part_start, turn_event},
	thread::v1::{Item, Message, Part, Role, item, part},
};
use omp_scribe::{Props, Template};
use strum::IntoStaticStr;
use thiserror::Error;

use crate::{AgentEvent, AgentRunSummary, broker::now_ms, prompt_engine, prompt_keys};

/// Voice-friendly provider instruction for the realtime half of the unified
/// assistant. Placeholders are substituted by [`render_live_instructions`].
pub const LIVE_VOICE_INSTRUCTIONS: &str = r#"You: omp Live, realtime voice surface of one unified coding assistant for {{ first_name }} (OS account: {{ username }}).

RFC 2119: MUST, REQUIRED, SHOULD, RECOMMENDED, MAY, OPTIONAL. NEVER means MUST NOT.

You and the omp coding agent are one assistant, not separate agents. You MUST delegate repository work, coding, tool use, commands, investigation, and verification to the client backend. You MUST keep conversation natural while it works.

Respond directly, briefly, conversationally, with speech-friendly phrasing. NEVER use markdown, code blocks, long lists, or read implementation detail aloud unless requested.

The client backend is your execution surface with repository context, the normal durable agent session, tools, and coding model. Promptly create a complete plain-language delegation for execution work, including relevant conversational context. A new request during active work creates a new delegation steering the same backend session.

Treat delegated context as your own internal progress and results. NEVER describe the backend as another assistant. MAY briefly acknowledge active work; NEVER claim changes, findings, or verification before the backend reports. Commentary context is silent progress and MUST NOT be recited. Context beginning with \"Agent Final Message\": is the backend's final visible answer; present its useful result naturally as your own without mentioning the label, protocol, delegation, or backend.

Greetings, clarification, and ordinary conversation needing no repository or tools MUST be answered directly without delegation. Ask one concise clarifying question only when an execution request is genuinely underspecified.

MUST preserve one-assistant continuity: converse here, delegate execution, and communicate returned results as your own."#;

/// Stable prefix separating a delegated agent's final answer from commentary.
pub const LIVE_FINAL_MESSAGE_PREFIX: &str = "\"Agent Final Message\":\n\n";

/// Renders live instructions with bounded user/account identity substitution.
pub fn render_live_instructions(first_name: &str, username: &str) -> Str {
	static TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
		prompt_engine::engine()
			.compile("voice/live-instructions", LIVE_VOICE_INSTRUCTIONS)
			.expect("embedded voice instructions template")
	});
	let mut props = Props::new();
	props.set(prompt_keys::FIRST_NAME, first_name.trim().chars().take(64).collect::<String>());
	props.set(prompt_keys::USERNAME, username.trim().chars().take(64).collect::<String>());
	TEMPLATE
		.render_str(prompt_engine::engine(), &props)
		.expect("typed voice props satisfy embedded template")
}

/// Wraps one canonical delegated final answer for the realtime peer.
pub fn live_final_message(message: &str) -> Str {
	let mut output = String::with_capacity(LIVE_FINAL_MESSAGE_PREFIX.len() + message.len());
	output.push_str(LIVE_FINAL_MESSAGE_PREFIX);
	output.push_str(message.trim());
	Str::from(output)
}

/// Observable state of the live delegation bridge.
#[derive(Clone, Copy, Debug, Default, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "lowercase")]
pub enum LiveAgentState {
	/// No delegated turn is active.
	#[default]
	Idle,
	/// An ordinary durable agent submission is running.
	Working,
	/// Cancellation was requested and settlement is pending.
	Cancelling,
}

/// Failure to start, observe, or settle delegated work.
#[derive(Debug, Error)]
pub enum LiveAgentBridgeError {
	/// A second delegation arrived while one ordinary agent submission is
	/// active.
	#[error("delegation {requested} arrived while delegation {active} is active")]
	Busy {
		/// Active delegation identity.
		active:    Str,
		/// Newly requested delegation identity.
		requested: Str,
	},
	/// A delegation identity was empty.
	#[error("realtime delegation identity is empty")]
	EmptyDelegationId,
	/// A delegation request contained no non-whitespace text.
	#[error("realtime delegation request is empty")]
	EmptyRequest,
	/// Cancellation targeted a delegation other than the active one.
	#[error("cancellation for delegation {requested} does not match active delegation {active}")]
	WrongDelegation {
		/// Active delegation identity.
		active:    Str,
		/// Cancellation target identity.
		requested: Str,
	},
	/// Canonical assistant text deltas did not form UTF-8.
	#[error("delegated agent commentary is not valid UTF-8")]
	InvalidCommentary {
		/// UTF-8 conversion failure.
		#[source]
		source: FromUtf8Error,
	},
}

/// Ordinary durable submission prepared for a realtime delegation.
#[derive(Clone, Debug)]
pub struct LiveAgentSubmission {
	/// Stable root turn identity passed to [`crate::Agent::submit`].
	pub turn_id:    TurnId,
	/// Canonical user item appended through the existing agent journal.
	pub item:       Item,
	/// Realtime delegation identity associated with the turn.
	pub delegation: Str,
}

/// Exactly-once terminal projection of delegated work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveAgentSettlement {
	/// Optional final speakable context for the realtime peer.
	pub final_context: Option<RealtimeContextAppend>,
	/// Terminal delegated-turn receipt.
	pub receipt:       RealtimeDelegationReceipt,
}

impl LiveAgentSettlement {
	/// Converts settlement evidence into provider-neutral realtime inputs.
	pub fn into_inputs(self) -> impl Iterator<Item = RealtimeInput> {
		self
			.final_context
			.into_iter()
			.map(RealtimeInput::AppendContext)
			.chain(iter::once(RealtimeInput::SettleDelegation(self.receipt)))
	}
}

#[derive(Debug)]
struct ActiveDelegation {
	id:         Str,
	state:      LiveAgentState,
	replaying:  bool,
	text_parts: BTreeMap<u32, Vec<u8>>,
}

/// Serial state machine projecting one realtime delegation onto the existing
/// durable agent authority.
///
/// A host starts the returned [`LiveAgentSubmission`] with
/// [`crate::Agent::submit`], feeds the bridge that agent's lossless events, and
/// settles it with the resulting [`AgentRunSummary`]. A
/// [`RealtimeInput::CancelDelegation`] returned by [`Self::cancel`] is the
/// host's instruction to call its existing [`crate::AbortHandle`]; the bridge
/// then waits for the ordinary submission summary before emitting settlement.
#[derive(Debug, Default)]
pub struct LiveAgentBridge {
	active: Option<ActiveDelegation>,
}

impl LiveAgentBridge {
	/// Creates an idle bridge.
	pub const fn new() -> Self {
		Self { active: None }
	}

	/// Returns the current delegated-turn state.
	pub fn state(&self) -> LiveAgentState {
		self
			.active
			.as_ref()
			.map_or(LiveAgentState::Idle, |active| active.state)
	}

	/// Starts one delegation as an ordinary canonical user turn.
	pub fn begin(
		&mut self,
		delegation: RealtimeDelegation,
	) -> Result<LiveAgentSubmission, LiveAgentBridgeError> {
		if delegation.id.is_empty() {
			return Err(LiveAgentBridgeError::EmptyDelegationId);
		}
		let request = delegation.request.trim();
		if request.is_empty() {
			return Err(LiveAgentBridgeError::EmptyRequest);
		}
		if let Some(active) = self.active.as_ref() {
			return Err(LiveAgentBridgeError::Busy {
				active:    active.id.clone(),
				requested: delegation.id,
			});
		}

		let turn_id = TurnId::new(omp_core::Ulid::generate().to_string());
		let item = Item {
			seq:           0,
			created_at_ms: now_ms(),
			kind:          Some(item::Kind::Message(Message {
				role:            i32::from(Role::User),
				parts:           vec![Part {
					kind: Some(part::Kind::Text(request.as_str().to_owned())),
				}],
				synthetic:       None,
				user_initiated:  None,
				completed_at_ms: None,
				usage:           None,
			})),
			props:         None,
		};
		let id = delegation.id;
		self.active = Some(ActiveDelegation {
			id:         id.clone(),
			state:      LiveAgentState::Working,
			replaying:  false,
			text_parts: BTreeMap::new(),
		});
		Ok(LiveAgentSubmission { turn_id, item, delegation: id })
	}

	/// Projects one existing agent event into commentary for the active
	/// delegation.
	///
	/// Commentary is emitted only when a canonical turn outcome commits a tool
	/// call, matching the normal agent's progress boundary. Provider replay is
	/// ignored so recovered turns cannot duplicate spoken progress.
	pub fn observe(
		&mut self,
		event: &AgentEvent,
	) -> Result<Option<RealtimeInput>, LiveAgentBridgeError> {
		let Some(active) = self.active.as_mut() else {
			return Ok(None);
		};
		let AgentEvent::Turn { event, .. } = event else {
			return Ok(None);
		};
		match event.event.as_ref() {
			Some(turn_event::Event::Accepted(accepted)) => {
				active.replaying = accepted.replay;
			},
			Some(turn_event::Event::PartStart(start))
				if start.kind() == part_start::Kind::Text && !active.replaying =>
			{
				active.text_parts.entry(start.index).or_default();
			},
			Some(turn_event::Event::PartDelta(delta)) if !active.replaying => {
				if let Some(bytes) = active.text_parts.get_mut(&delta.index) {
					bytes.extend_from_slice(&delta.chunk);
				}
			},
			Some(turn_event::Event::Outcome(outcome)) => {
				if active.replaying {
					active.replaying = false;
					active.text_parts.clear();
					return Ok(None);
				}
				let has_tool_call = outcome
					.output
					.iter()
					.any(|item| matches!(item.kind.as_ref(), Some(item::Kind::ToolCall(_))));
				if !has_tool_call {
					active.text_parts.clear();
					return Ok(None);
				}
				let mut joined = Vec::new();
				for bytes in active.text_parts.values() {
					joined.extend_from_slice(bytes);
				}
				active.text_parts.clear();
				let text = String::from_utf8(joined)
					.map_err(|source| LiveAgentBridgeError::InvalidCommentary { source })?;
				let text = text.trim();
				if text.is_empty() {
					return Ok(None);
				}
				return Ok(Some(RealtimeInput::AppendContext(RealtimeContextAppend {
					target:  RealtimeContextTarget::Delegation { id: active.id.clone() },
					channel: RealtimeContextChannel::Commentary,
					text:    Str::from(text),
				})));
			},
			_ => {},
		}
		Ok(None)
	}

	/// Requests cancellation of the active ordinary agent submission.
	///
	/// The first call transitions to [`LiveAgentState::Cancelling`] and returns
	/// a provider-neutral cancellation input. Repeated requests are idempotent.
	pub fn cancel(
		&mut self,
		delegation_id: &str,
	) -> Result<Option<RealtimeInput>, LiveAgentBridgeError> {
		let Some(active) = self.active.as_mut() else {
			return Ok(None);
		};
		if active.id.as_str() != delegation_id {
			return Err(LiveAgentBridgeError::WrongDelegation {
				active:    active.id.clone(),
				requested: Str::from(delegation_id),
			});
		}
		if active.state == LiveAgentState::Cancelling {
			return Ok(None);
		}
		active.state = LiveAgentState::Cancelling;
		Ok(Some(RealtimeInput::CancelDelegation { id: active.id.clone() }))
	}

	/// Settles the active delegation from the authoritative ordinary agent run
	/// summary. A second call returns `None`, guaranteeing one terminal receipt.
	pub fn settle(&mut self, summary: &AgentRunSummary) -> Option<LiveAgentSettlement> {
		let active = self.active.take()?;
		let cancelled = summary.interrupted || active.state == LiveAgentState::Cancelling;
		let status = if cancelled {
			RealtimeDelegationStatus::Cancelled
		} else {
			RealtimeDelegationStatus::Completed
		};
		let final_context = (!cancelled)
			.then(|| final_response(summary.outcome.as_ref()))
			.flatten()
			.map(|text| RealtimeContextAppend {
				target: RealtimeContextTarget::Delegation { id: active.id.clone() },
				channel: RealtimeContextChannel::Speakable,
				text,
			});
		Some(LiveAgentSettlement {
			final_context,
			receipt: RealtimeDelegationReceipt {
				delegation_id: active.id,
				status,
				settled_at: SystemTime::now(),
			},
		})
	}

	/// Settles an active delegation that failed outside a completed agent run.
	/// A second call returns `None`, guaranteeing one terminal receipt.
	pub fn settle_failed(&mut self) -> Option<LiveAgentSettlement> {
		let active = self.active.take()?;
		let status = if active.state == LiveAgentState::Cancelling {
			RealtimeDelegationStatus::Cancelled
		} else {
			RealtimeDelegationStatus::Failed
		};
		Some(LiveAgentSettlement {
			final_context: None,
			receipt:       RealtimeDelegationReceipt {
				delegation_id: active.id,
				status,
				settled_at: SystemTime::now(),
			},
		})
	}
}

fn final_response(outcome: Option<&Outcome>) -> Option<Str> {
	let outcome = outcome?;
	for candidate in outcome.output.iter().rev() {
		let Some(item::Kind::Message(message)) = candidate.kind.as_ref() else {
			continue;
		};
		if message.role != i32::from(Role::Assistant) {
			continue;
		}
		let mut text = String::new();
		for part in &message.parts {
			if let Some(part::Kind::Text(fragment)) = part.kind.as_ref() {
				text.push_str(fragment);
			}
		}
		let text = text.trim();
		if !text.is_empty() {
			return Some(Str::from(text));
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use omp_proto::inference::v1::{self, Accepted, PartDelta, PartStart, TurnEvent};

	use super::*;

	fn delegation() -> RealtimeDelegation {
		RealtimeDelegation { id: Str::from("delegation-1"), request: Str::from("fix the build") }
	}

	fn summary(interrupted: bool, text: Option<&str>) -> AgentRunSummary {
		let output = text.map_or_else(Vec::new, |text| {
			vec![Item {
				kind: Some(item::Kind::Message(Message {
					role: i32::from(Role::Assistant),
					parts: vec![Part { kind: Some(part::Kind::Text(text.to_owned())) }],
					..Default::default()
				})),
				..Item::default()
			}]
		});
		AgentRunSummary::settled(
			Outcome { output, stop: v1::StopReason::StopEndTurn as i32, ..Outcome::default() },
			1,
			interrupted,
		)
	}

	#[test]
	fn cancellation_transitions_and_settles_once() {
		let mut bridge = LiveAgentBridge::new();
		let submission = bridge.begin(delegation()).unwrap();
		assert_eq!(submission.delegation.as_str(), "delegation-1");
		assert_eq!(bridge.state(), LiveAgentState::Working);

		assert!(matches!(
			bridge.cancel("delegation-1").unwrap(),
			Some(RealtimeInput::CancelDelegation { id }) if id.as_str() == "delegation-1"
		));
		assert_eq!(bridge.state(), LiveAgentState::Cancelling);
		assert!(bridge.cancel("delegation-1").unwrap().is_none());

		let settlement = bridge.settle(&summary(true, None)).unwrap();
		assert_eq!(settlement.receipt.status, RealtimeDelegationStatus::Cancelled);
		assert!(settlement.final_context.is_none());
		assert_eq!(bridge.state(), LiveAgentState::Idle);
		assert!(bridge.settle(&summary(true, None)).is_none());
	}

	#[test]
	fn tool_boundary_emits_commentary_and_final_settlement() {
		let mut bridge = LiveAgentBridge::new();
		bridge.begin(delegation()).unwrap();
		let turn_id = TurnId::new("turn-1");
		let accepted = AgentEvent::Turn {
			turn_id: turn_id.clone(),
			event:   Box::new(TurnEvent {
				event: Some(turn_event::Event::Accepted(Accepted {
					replay: false,
					..Accepted::default()
				})),
			}),
		};
		bridge.observe(&accepted).unwrap();
		let start = AgentEvent::Turn {
			turn_id: turn_id.clone(),
			event:   Box::new(TurnEvent {
				event: Some(turn_event::Event::PartStart(PartStart {
					index: 7,
					kind: part_start::Kind::Text as i32,
					..PartStart::default()
				})),
			}),
		};
		bridge.observe(&start).unwrap();
		let delta = AgentEvent::Turn {
			turn_id: turn_id.clone(),
			event:   Box::new(TurnEvent {
				event: Some(turn_event::Event::PartDelta(PartDelta {
					index: 7,
					chunk: b"Checking the build".to_vec().into(),
				})),
			}),
		};
		bridge.observe(&delta).unwrap();
		let outcome = AgentEvent::Turn {
			turn_id,
			event: Box::new(TurnEvent {
				event: Some(turn_event::Event::Outcome(Outcome {
					output: vec![Item {
						kind: Some(item::Kind::ToolCall(Default::default())),
						..Item::default()
					}],
					..Outcome::default()
				})),
			}),
		};
		assert!(matches!(
			bridge.observe(&outcome).unwrap(),
			Some(RealtimeInput::AppendContext(RealtimeContextAppend {
				channel: RealtimeContextChannel::Commentary,
				ref text,
				..
			})) if text.as_str() == "Checking the build"
		));

		let settlement = bridge.settle(&summary(false, Some("Build fixed"))).unwrap();
		assert_eq!(settlement.receipt.status, RealtimeDelegationStatus::Completed);
		assert_eq!(
			settlement
				.final_context
				.as_ref()
				.map(|context| context.text.as_str()),
			Some("Build fixed")
		);
		assert!(bridge.settle_failed().is_none());
	}
}
