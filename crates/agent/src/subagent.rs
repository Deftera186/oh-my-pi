//! Retained subagent run state and lifecycle events.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use omp_core::{AppendVec, Str};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum retained bytes for one progress activity label.
pub const MAX_PROGRESS_ACTIVITY_BYTES: usize = 512;
/// Maximum retained bytes for one terminal summary.
pub const MAX_TERMINAL_SUMMARY_BYTES: usize = 5_000;
/// Maximum retained bytes for a caller-visible disposition preview.
pub const MAX_DISPOSITION_PREVIEW_BYTES: usize = 5_000;

/// Monotonic incarnation of one stable subagent identity.
#[derive(
	Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct SubagentGeneration(pub u64);

/// Lifecycle of the currently loaded subagent loop.
#[repr(u8)]
#[derive(
	Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum SubagentLifecycle {
	/// Identity exists but no child loop has started.
	Created,
	/// A first turn or cold revival is being constructed.
	Starting,
	/// The child loop is executing a turn.
	Running,
	/// The loop released its run permit while waiting on owned children.
	Waiting,
	/// Live resources were released while the durable identity remains
	/// addressable.
	Parked,
	/// This generation reached a terminal outcome and remains follow-up-capable.
	Settled,
}

impl SubagentLifecycle {
	const fn allows(self, next: Self) -> bool {
		matches!(
			(self, next),
			(Self::Created, Self::Starting | Self::Settled)
				| (Self::Starting, Self::Running | Self::Parked | Self::Settled)
				| (Self::Running, Self::Waiting | Self::Parked | Self::Settled)
				| (Self::Waiting, Self::Running | Self::Parked | Self::Settled)
				| (Self::Parked, Self::Starting)
				| (Self::Settled, Self::Parked)
		)
	}
}

/// Bounded latest progress facts retained independently of UI listeners.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentProgressSnapshot {
	/// Short current activity label.
	pub activity:       Str,
	/// Assistant requests committed in this generation.
	pub requests:       u32,
	/// Tool calls committed in this generation.
	pub tool_calls:     u32,
	/// Input tokens attributed by durable receipts.
	pub input_tokens:   u64,
	/// Output tokens attributed by durable receipts.
	pub output_tokens:  u64,
	/// Durable receipt cost in micro-USD.
	pub cost_micros:    u64,
	/// Latest provider context size.
	pub context_tokens: u64,
	/// Model which actually served the latest request.
	pub serving_model:  Option<Str>,
	/// Whether the activity label was byte-truncated.
	pub truncated:      bool,
}

impl SubagentProgressSnapshot {
	/// Constructs a snapshot while enforcing the retained activity bound.
	pub fn bounded(mut self) -> Self {
		let original_len = self.activity.len();
		truncate_utf8(&mut self.activity, MAX_PROGRESS_ACTIVITY_BYTES);
		self.truncated |= self.activity.len() != original_len;
		self
	}
}

/// Durable isolated-workspace disposition attached to a terminal outcome.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentDisposition {
	/// Durable artifact URI containing the full outcome, when one was emitted.
	pub artifact_uri: Option<Str>,
	/// Bounded caller-visible preview or cancellation salvage.
	pub preview:      Option<Str>,
	/// Whether the preview omits bytes from the full outcome.
	pub truncated:    bool,
	/// Durable isolated workspace identity needed by later merge or revival.
	pub workspace:    Option<Str>,
}

impl SubagentDisposition {
	/// Enforces caller-visible preview bounds without copying heap-backed text.
	pub fn bounded(mut self) -> Self {
		if let Some(preview) = self.preview.as_mut() {
			let original_len = preview.len();
			truncate_utf8(preview, MAX_DISPOSITION_PREVIEW_BYTES);
			self.truncated |= preview.len() != original_len;
		}
		self
	}
}

/// Structured terminal classification for a subagent generation.
#[derive(
	Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum SubagentTerminalKind {
	/// The child completed with a caller-visible result.
	Succeeded,
	/// The caller cancelled the generation; bounded salvage may remain.
	Cancelled,
	/// Strict output schema validation failed.
	SchemaInvalid,
	/// The configured wall-clock limit stopped the generation.
	RuntimeLimit,
	/// The child loop failed before producing a successful result.
	Failed,
}

/// Terminal outcome retained by core for UI, artifact, and revival projections.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentTerminalStatus {
	/// Machine-readable terminal classification.
	pub kind:        SubagentTerminalKind,
	/// Bounded human-readable terminal summary.
	pub summary:     Str,
	/// Structured bounded artifact/workspace disposition.
	pub disposition: SubagentDisposition,
}

impl SubagentTerminalStatus {
	/// Enforces terminal summary and disposition bounds.
	pub fn bounded(mut self) -> Self {
		truncate_utf8(&mut self.summary, MAX_TERMINAL_SUMMARY_BYTES);
		self.disposition = self.disposition.bounded();
		self
	}
}

/// Fine-grained supervised child activity.
#[derive(
	Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum SubagentActivityKind {
	/// Initial delegated turn.
	FirstTurn,
	/// Serialized follow-up turn.
	FollowUp,
	/// IRC message woke the loop.
	IrcWake,
	/// Child is awaiting provider admission or stream progress.
	ProviderWait,
	/// One assistant request began.
	Request,
	/// One tool invocation began.
	Tool,
	/// Receipt usage and cost changed.
	Usage,
	/// Context window facts changed.
	Context,
}

/// Bounded raw/progress fact retained for UI and telemetry projection.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentActivity {
	/// Typed activity class.
	pub kind:           Option<SubagentActivityKind>,
	/// Bounded tool/model/activity detail.
	pub detail:         Str,
	/// Serving model when known.
	pub serving_model:  Option<Str>,
	/// Receipt input tokens.
	pub input_tokens:   u64,
	/// Receipt output tokens.
	pub output_tokens:  u64,
	/// Receipt cost in micro-USD.
	pub cost_micros:    u64,
	/// Latest context size.
	pub context_tokens: u64,
}

impl SubagentActivity {
	fn bounded(mut self) -> Self {
		truncate_utf8(&mut self.detail, MAX_PROGRESS_ACTIVITY_BYTES);
		self
	}
}

/// Retained payload published for one subagent run transition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "data")]
pub enum SubagentRunEventKind {
	/// Fine-grained raw/progress activity.
	Activity(SubagentActivity),
	/// Lifecycle transition.
	Lifecycle(SubagentLifecycle),
	/// Replacement bounded progress snapshot.
	Progress(SubagentProgressSnapshot),
	/// Structured terminal outcome.
	Terminal(SubagentTerminalStatus),
}

/// Monotonic retained subagent event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentRunEvent {
	/// Stable subagent identity.
	pub agent_id:   Str,
	/// Incarnation to which this event belongs.
	pub generation: SubagentGeneration,
	/// Per-identity monotonic event sequence.
	pub sequence:   u64,
	/// Event payload.
	pub event:      SubagentRunEventKind,
}

/// Invalid mutation of core-owned subagent run state.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum SubagentStateError {
	/// The requested lifecycle edge is not valid.
	#[error("invalid subagent lifecycle transition from {from} to {to}")]
	InvalidTransition {
		/// Current lifecycle.
		from: SubagentLifecycle,
		/// Requested lifecycle.
		to:   SubagentLifecycle,
	},
	/// Progress was published while no turn could be active.
	#[error("subagent progress is invalid while lifecycle is {lifecycle}")]
	ProgressWhileInactive {
		/// Current lifecycle.
		lifecycle: SubagentLifecycle,
	},
	/// A generation was restarted before the previous generation settled or
	/// parked.
	#[error("subagent generation cannot restart while lifecycle is {lifecycle}")]
	GenerationStillActive {
		/// Current lifecycle.
		lifecycle: SubagentLifecycle,
	},
}

/// Sole mutable owner of retained state for one stable subagent identity.
pub struct SubagentRunState {
	agent_id:        Str,
	generation:      AtomicU64,
	sequence:        AtomicU64,
	lifecycle:       AtomicU8,
	yield_committed: AtomicBool,
	progress:        Mutex<SubagentProgressSnapshot>,
	terminal:        Mutex<Option<SubagentTerminalStatus>>,
	events:          AppendVec<SubagentRunEvent>,
}

impl SubagentRunState {
	/// Creates retained state for a new durable identity.
	pub fn new(agent_id: Str) -> Self {
		let state = Self {
			agent_id,
			generation: AtomicU64::new(0),
			sequence: AtomicU64::new(0),
			lifecycle: AtomicU8::new(SubagentLifecycle::Created as u8),
			yield_committed: AtomicBool::new(false),
			progress: Mutex::new(SubagentProgressSnapshot::default()),
			terminal: Mutex::new(None),
			events: AppendVec::new(),
		};
		state.append(SubagentRunEventKind::Lifecycle(SubagentLifecycle::Created));
		state
	}

	/// Stable identity shared by every generation.
	pub const fn agent_id(&self) -> &Str {
		&self.agent_id
	}

	/// Current generation.
	pub fn generation(&self) -> SubagentGeneration {
		SubagentGeneration(self.generation.load(Ordering::Acquire))
	}

	/// Current lifecycle.
	pub fn lifecycle(&self) -> SubagentLifecycle {
		decode_lifecycle(self.lifecycle.load(Ordering::Acquire))
	}

	/// Latest bounded progress snapshot.
	pub fn progress(&self) -> SubagentProgressSnapshot {
		self.progress.lock().clone()
	}

	/// Terminal status of the current generation, if settled.
	pub fn terminal(&self) -> Option<SubagentTerminalStatus> {
		self.terminal.lock().clone()
	}

	/// Records that this generation committed a terminal `yield` call.
	pub fn commit_yield(&self) {
		self.yield_committed.store(true, Ordering::Release);
	}

	/// Whether this generation committed a terminal `yield` call.
	pub fn yield_committed(&self) -> bool {
		self.yield_committed.load(Ordering::Acquire)
	}

	/// Iterates all retained events in publication order.
	pub fn events(&self) -> impl Iterator<Item = &SubagentRunEvent> {
		self.events.iter()
	}

	/// Retains one fine-grained activity and advances its bounded progress
	/// projection.
	pub fn record_activity(&self, activity: SubagentActivity) -> Result<(), SubagentStateError> {
		let lifecycle = self.lifecycle();
		if !matches!(
			lifecycle,
			SubagentLifecycle::Starting | SubagentLifecycle::Running | SubagentLifecycle::Waiting
		) {
			return Err(SubagentStateError::ProgressWhileInactive { lifecycle });
		}
		let activity = activity.bounded();
		let mut progress = self.progress.lock();
		progress.activity = activity.detail.clone();
		let usage_receipt = matches!(activity.kind, Some(SubagentActivityKind::Usage));
		match activity.kind {
			Some(SubagentActivityKind::Request) => {
				progress.requests = progress.requests.saturating_add(1);
			},
			Some(SubagentActivityKind::Tool) => {
				progress.tool_calls = progress.tool_calls.saturating_add(1);
			},
			_ => {},
		}
		progress.input_tokens = if usage_receipt {
			progress.input_tokens.saturating_add(activity.input_tokens)
		} else {
			progress.input_tokens.max(activity.input_tokens)
		};
		progress.output_tokens = if usage_receipt {
			progress
				.output_tokens
				.saturating_add(activity.output_tokens)
		} else {
			progress.output_tokens.max(activity.output_tokens)
		};
		progress.cost_micros = if usage_receipt {
			progress.cost_micros.saturating_add(activity.cost_micros)
		} else {
			progress.cost_micros.max(activity.cost_micros)
		};
		progress.context_tokens = progress.context_tokens.max(activity.context_tokens);
		if activity.serving_model.is_some() {
			progress.serving_model.clone_from(&activity.serving_model);
		}
		drop(progress);
		self.append(SubagentRunEventKind::Activity(activity));
		Ok(())
	}

	/// Applies a valid lifecycle transition and retains it.
	pub fn transition(&self, next: SubagentLifecycle) -> Result<(), SubagentStateError> {
		let current = self.lifecycle();
		if !current.allows(next) {
			return Err(SubagentStateError::InvalidTransition { from: current, to: next });
		}
		self.lifecycle.store(next as u8, Ordering::Release);
		self.append(SubagentRunEventKind::Lifecycle(next));
		tracing::debug!(
			agent_id = %self.agent_id,
			generation = self.generation().0,
			from = %current,
			to = %next,
			"subagent lifecycle changed"
		);
		Ok(())
	}

	/// Replaces and retains the bounded progress snapshot.
	pub fn record_progress(
		&self,
		progress: SubagentProgressSnapshot,
	) -> Result<(), SubagentStateError> {
		let lifecycle = self.lifecycle();
		if !matches!(
			lifecycle,
			SubagentLifecycle::Starting | SubagentLifecycle::Running | SubagentLifecycle::Waiting
		) {
			return Err(SubagentStateError::ProgressWhileInactive { lifecycle });
		}
		let progress = progress.bounded();
		*self.progress.lock() = progress.clone();
		self.append(SubagentRunEventKind::Progress(progress));
		Ok(())
	}

	/// Settles the current generation with a structured bounded outcome.
	pub fn settle(&self, terminal: SubagentTerminalStatus) -> Result<(), SubagentStateError> {
		self.transition(SubagentLifecycle::Settled)?;
		let terminal = terminal.bounded();
		*self.terminal.lock() = Some(terminal.clone());
		self.append(SubagentRunEventKind::Terminal(terminal));
		Ok(())
	}

	/// Begins a follow-up or cold-revival generation for this durable identity.
	pub fn begin_generation(&self) -> Result<SubagentGeneration, SubagentStateError> {
		let lifecycle = self.lifecycle();
		if !matches!(lifecycle, SubagentLifecycle::Settled | SubagentLifecycle::Parked) {
			return Err(SubagentStateError::GenerationStillActive { lifecycle });
		}
		let generation = self
			.generation
			.fetch_add(1, Ordering::AcqRel)
			.wrapping_add(1);
		self.yield_committed.store(false, Ordering::Release);
		*self.progress.lock() = SubagentProgressSnapshot::default();
		*self.terminal.lock() = None;
		self
			.lifecycle
			.store(SubagentLifecycle::Starting as u8, Ordering::Release);
		self.append(SubagentRunEventKind::Lifecycle(SubagentLifecycle::Starting));
		tracing::debug!(
			agent_id = %self.agent_id,
			generation,
			from = %lifecycle,
			to = %SubagentLifecycle::Starting,
			"subagent generation started"
		);
		Ok(SubagentGeneration(generation))
	}

	fn append(&self, event: SubagentRunEventKind) {
		let sequence = self.sequence.fetch_add(1, Ordering::AcqRel);
		self.events.push(SubagentRunEvent {
			agent_id: self.agent_id.clone(),
			generation: self.generation(),
			sequence,
			event,
		});
	}
}

fn decode_lifecycle(value: u8) -> SubagentLifecycle {
	match value {
		value if value == SubagentLifecycle::Created as u8 => SubagentLifecycle::Created,
		value if value == SubagentLifecycle::Starting as u8 => SubagentLifecycle::Starting,
		value if value == SubagentLifecycle::Running as u8 => SubagentLifecycle::Running,
		value if value == SubagentLifecycle::Waiting as u8 => SubagentLifecycle::Waiting,
		value if value == SubagentLifecycle::Parked as u8 => SubagentLifecycle::Parked,
		_ => SubagentLifecycle::Settled,
	}
}

fn truncate_utf8(value: &mut Str, max_bytes: usize) {
	if value.len() <= max_bytes {
		return;
	}
	let mut end = max_bytes;
	while !value.as_str().is_char_boundary(end) {
		end -= 1;
	}
	value.truncate(end);
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn retained_events_reconstruct_lifecycle_after_listeners_detach() {
		let state = SubagentRunState::new(sf!("agent-1"));
		state.transition(SubagentLifecycle::Starting).unwrap();
		state.transition(SubagentLifecycle::Running).unwrap();
		state
			.record_progress(SubagentProgressSnapshot {
				activity: sf!("reading"),
				requests: 1,
				..SubagentProgressSnapshot::default()
			})
			.unwrap();
		state.transition(SubagentLifecycle::Waiting).unwrap();
		state.transition(SubagentLifecycle::Running).unwrap();
		state
			.settle(SubagentTerminalStatus {
				kind:        SubagentTerminalKind::Succeeded,
				summary:     sf!("done"),
				disposition: SubagentDisposition::default(),
			})
			.unwrap();

		let events = state.events().cloned().collect::<Vec<_>>();
		assert_eq!(events.first().unwrap().sequence, 0);
		assert!(matches!(events.last().unwrap().event, SubagentRunEventKind::Terminal(_)));
		assert_eq!(state.lifecycle(), SubagentLifecycle::Settled);
		assert_eq!(state.progress().activity, "reading");
	}

	#[test]
	fn lifecycle_rejects_invalid_edges_and_generations_are_monotonic() {
		let state = SubagentRunState::new(sf!("agent-2"));
		assert!(matches!(
			state.transition(SubagentLifecycle::Running),
			Err(SubagentStateError::InvalidTransition {
				from: SubagentLifecycle::Created,
				to:   SubagentLifecycle::Running,
			})
		));
		state.transition(SubagentLifecycle::Starting).unwrap();
		state.transition(SubagentLifecycle::Running).unwrap();
		state
			.settle(SubagentTerminalStatus {
				kind:        SubagentTerminalKind::Cancelled,
				summary:     sf!("cancelled"),
				disposition: SubagentDisposition::default(),
			})
			.unwrap();
		assert_eq!(state.begin_generation().unwrap(), SubagentGeneration(1));
		assert_eq!(state.lifecycle(), SubagentLifecycle::Starting);
		assert!(state.terminal().is_none());
	}

	#[test]
	fn progress_and_terminal_payloads_are_bounded_on_utf8_boundaries() {
		let state = SubagentRunState::new(sf!("agent-3"));
		state.transition(SubagentLifecycle::Starting).unwrap();
		state.transition(SubagentLifecycle::Running).unwrap();
		state
			.record_progress(SubagentProgressSnapshot {
				activity: Str::from("é".repeat(MAX_PROGRESS_ACTIVITY_BYTES)),
				..SubagentProgressSnapshot::default()
			})
			.unwrap();
		assert!(state.progress().activity.len() <= MAX_PROGRESS_ACTIVITY_BYTES);
		assert!(state.progress().truncated);
	}

	#[test]
	fn usage_receipts_accumulate_across_a_generation() {
		let state = SubagentRunState::new(sf!("agent-usage"));
		state.transition(SubagentLifecycle::Starting).unwrap();
		state.transition(SubagentLifecycle::Running).unwrap();
		for output_tokens in [1_234, 2_345] {
			state
				.record_activity(SubagentActivity {
					kind: Some(SubagentActivityKind::Usage),
					output_tokens,
					..SubagentActivity::default()
				})
				.unwrap();
		}
		assert_eq!(state.progress().output_tokens, 3_579);
	}
}
