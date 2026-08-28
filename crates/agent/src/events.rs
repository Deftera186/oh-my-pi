//! Ordered, nonblocking fan-out for agent lifecycle events.

use std::sync::{
	Arc,
	atomic::{AtomicU8, AtomicU64, Ordering},
};

use bytes::Bytes;
use flume::Receiver;
use omp_core::{Str, ToolPath};
use omp_inference::TurnId;
use omp_proto::{
	inference::{
		v1,
		v1::{TurnEvent, turn_event},
	},
	thread::v1::Item,
};
use omp_storage::transcript::TitleSource;
use omp_tool::{Rev, ToolIdentity};
use parking_lot::Mutex;

use crate::{Receipt, state::AgentSnapshot};

/// Observable phase of the agent loop.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AgentPhase {
	/// Waiting for work.
	#[default]
	Idle,
	/// Rebuilding canonical thread state from the journal.
	Projecting,
	/// Streaming or recovering an inference turn.
	Turning,
	/// Executing a committed batch of tool calls.
	ToolBatch,
}

/// Host-visible run state used by terminal titles and retained UI.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum AgentRunState {
	/// No turn is currently active.
	#[default]
	Idle,
	/// A turn or tool batch is making progress.
	Working,
	/// The run stopped and needs user attention.
	Attention,
}

impl AgentRunState {
	const fn encode(self) -> u8 {
		self as u8
	}

	const fn decode(encoded: u8) -> Self {
		match encoded {
			1 => Self::Working,
			2 => Self::Attention,
			_ => Self::Idle,
		}
	}
}

/// Visibility assigned by the loop to a host-observable event.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum EventVisibility {
	/// User-facing turn activity.
	#[default]
	User,
	/// Runtime coordination that headless hosts must not project as
	/// conversation.
	Internal,
}

/// Typed origin of one host-observable event.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum EventProvenance {
	/// Authored by the model in the active turn.
	#[default]
	Model,
	/// Emitted by the core loop itself.
	Runtime,
	/// Emitted by a supervised extension.
	Extension,
	/// Emitted by a child agent.
	Subagent,
}

/// Typed planning state projected to protocol hosts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum PlanState {
	/// Planning is inactive.
	#[default]
	Inactive,
	/// Planning is active and mutations remain prohibited.
	Active,
	/// Planning is active with one explicitly authorized transition.
	Yolo,
}

impl AgentPhase {
	const fn encode(self) -> u8 {
		match self {
			Self::Idle => 0,
			Self::Projecting => 1,
			Self::Turning => 2,
			Self::ToolBatch => 3,
		}
	}

	const fn decode(encoded: u8) -> Self {
		match encoded {
			1 => Self::Projecting,
			2 => Self::Turning,
			3 => Self::ToolBatch,
			_ => Self::Idle,
		}
	}
}

/// Display-only peer traffic projected to the main session UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerRelayObservation {
	/// Stable message id used for end-to-end deduplication.
	pub id:      Str,
	/// Stable sender identity.
	pub from:    Str,
	/// Resolved recipient identity.
	pub to:      Str,
	/// Exact peer-visible body.
	pub text:    Str,
	/// Delivery state shown beside the body.
	pub outcome: Receipt,
}

/// One immutable observation emitted by the agent loop.
#[derive(Clone, Debug)]
pub enum AgentEvent {
	/// A newly published authoritative agent snapshot.
	Snapshot(Arc<AgentSnapshot>),
	/// The sole journal writer committed a new session title.
	TitleChanged {
		/// Accepted title.
		title:  Str,
		/// Authority that assigned it.
		source: TitleSource,
	},
	/// Terminal/UI run-state transition.
	RunStateChanged {
		/// State exited.
		from: AgentRunState,
		/// State entered.
		to:   AgentRunState,
	},
	/// A lifecycle transition between loop phases.
	PhaseChanged {
		/// Phase exited by the loop.
		from: AgentPhase,
		/// Phase entered by the loop.
		to:   AgentPhase,
	},
	/// The append-only agent roster changed; consumers re-project from
	/// `AgentTree`.
	RosterChanged {
		/// Monotonic generation returned by `AgentTree::roster_generation`.
		generation: u64,
	},
	/// Display-only peer message; never appended to the model journal.
	PeerRelay(Arc<PeerRelayObservation>),
	/// One canonical input item was durably staged for the next model turn.
	///
	/// Presentation consumers use message metadata to update turn-time anchors
	/// at actual delivery rather than when a queued prompt was submitted.
	InputStaged {
		/// Exact staged item, including its original local creation time.
		item: Item,
	},
	/// An inference event, preserved without lossy adaptation.
	Turn {
		/// Logical turn that emitted the event.
		turn_id: TurnId,
		/// Canonical turn protocol event.
		event:   Box<TurnEvent>,
	},
	/// Typed metadata for a tool invocation. This precedes `ToolOpened` for the
	/// same call in every loop-produced stream.
	ToolObserved {
		/// Stable call identifier.
		call_id:            Str,
		/// Exact selected tool revision.
		identity:           ToolIdentity,
		/// Typed device/sub-tool path when the model-facing name is a valid path.
		path:               Option<ToolPath>,
		/// Host presentation visibility.
		visibility:         EventVisibility,
		/// Authenticated event origin.
		provenance:         EventProvenance,
		/// Active session incarnation.
		session_generation: u64,
	},
	/// A typed planning-state transition.
	PlanStateChanged {
		/// Previous state.
		from:               PlanState,
		/// New state.
		to:                 PlanState,
		/// Active session incarnation.
		session_generation: u64,
	},
	/// A speculative tool invocation was opened.
	ToolOpened {
		/// Stable call identifier.
		call_id: Str,
		/// Model-facing tool name.
		name:    Str,
		/// Tool argument and rendering revision.
		rev:     Rev,
	},
	/// A raw model-authored argument fragment arrived.
	ToolArgs {
		/// Stable call identifier.
		call_id:  Str,
		/// Unparsed argument bytes in arrival order.
		fragment: Bytes,
		/// Loop-owned best-effort view of all argument fragments so far.
		view:     omp_slopjson::Value,
	},
	/// A tool emitted an ephemeral update that must not enter the thread.
	ToolUpdate {
		/// Stable call identifier.
		call_id: Str,
		/// Raw structured update bytes.
		json:    Bytes,
	},
	/// A tool completed and lowered to a canonical thread item.
	ToolFinished {
		/// Stable call identifier.
		call_id: Str,
		/// Canonical result item staged for the next delta.
		item:    Item,
		/// Cumulative session usage observed before this tool boundary.
		usage:   v1::Usage,
	},
	/// A detached job began settlement tracking.
	JobRegistered {
		/// Stable detached-job identifier.
		job_id: Str,
	},
	/// A detached job reached a terminal settlement.
	JobSettled {
		/// Stable detached-job identifier.
		job_id: Str,
	},
	/// Durable history was rewritten (rewind or reset) and journal-derived
	/// state was reconciled to the new live prefix.
	HistoryRewritten {
		/// Retained physical event; `None` when rewound to the root.
		to:            Option<u64>,
		/// New journal head after the rewrite marker.
		head:          u64,
		/// Loop-owned job ids the JobBoard could not cancel itself; the
		/// composition layer escalates these to its session supervisor.
		escalate_jobs: Vec<Str>,
	},
	/// The loop reached an error that is visible to hosts.
	Failed {
		/// Logical turn involved, when failure occurred within a turn.
		turn_id: Option<TurnId>,
		/// Stable human-readable failure description.
		message: Str,
	},
}
/// Explicit collaboration visibility assigned by the EventBus projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum CollabEventVisibility {
	/// Canonical user-visible turn content.
	PublicTranscript,
	/// Credential-free lifecycle or registry presentation state.
	PublicPresentation,
}

/// One allowlisted collaboration projection.
///
/// Construction is private to [`EventBus`], so an arbitrary internal
/// [`AgentEvent`] cannot be smuggled into the collaboration stream.
#[derive(Clone, Debug)]
pub struct CollabEvent {
	visibility: CollabEventVisibility,
	event:      Arc<AgentEvent>,
}

impl CollabEvent {
	/// Returns the explicit peer visibility class.
	pub const fn visibility(&self) -> CollabEventVisibility {
		self.visibility
	}

	/// Returns the allowlisted immutable agent event.
	pub fn event(&self) -> &AgentEvent {
		&self.event
	}
}

#[derive(Debug)]
struct LossySender {
	tx:      flume::Sender<Arc<AgentEvent>>,
	dropped: Arc<AtomicU64>,
}

#[derive(Debug)]
struct CollabSender {
	tx:      flume::Sender<Arc<CollabEvent>>,
	dropped: Arc<AtomicU64>,
}

#[derive(Debug, Default)]
struct Subscribers {
	lossless: Vec<flume::Sender<Arc<AgentEvent>>>,
	lossy:    Vec<LossySender>,
	collab:   Vec<CollabSender>,
}

#[derive(Debug, Default)]
struct EventBusInner {
	subscribers:        Mutex<Subscribers>,
	dropped_lossy:      AtomicU64,
	phase:              AtomicU8,
	run_state:          AtomicU8,
	session_generation: AtomicU64,
}

/// Cloneable ordered fan-out for immutable shared agent events.
///
/// Publication never waits for a consumer: journal subscribers use unbounded
/// channels, while bounded UI subscribers drop on saturation and account for
/// each loss. One mutex establishes the same concurrent publication order for
/// every subscriber.
#[derive(Clone, Debug, Default)]
pub struct EventBus {
	inner: Arc<EventBusInner>,
}

impl EventBus {
	/// Creates an event bus with no subscribers.
	pub fn new() -> Self {
		Self::default()
	}

	/// Adds an unbounded, lossless subscriber suitable for journaling.
	pub fn subscribe_lossless(&self) -> EventSubscription {
		let (tx, rx) = flume::unbounded();
		self.inner.subscribers.lock().lossless.push(tx);
		EventSubscription { rx }
	}

	/// Adds a bounded, lossy subscriber suitable for UI presentation.
	///
	/// A zero capacity is valid and acts as a pure best-effort rendezvous.
	pub fn subscribe_ui(&self, capacity: usize) -> LossyEventSubscription {
		let (tx, rx) = flume::bounded(capacity);
		let dropped = Arc::new(AtomicU64::new(0));
		self
			.inner
			.subscribers
			.lock()
			.lossy
			.push(LossySender { tx, dropped: dropped.clone() });
		LossyEventSubscription { rx, dropped }
	}

	/// Publishes an owned event and returns its shared representation.
	pub fn publish(&self, event: AgentEvent) -> Arc<AgentEvent> {
		self.publish_shared(Arc::new(event))
	}

	/// Adds a bounded subscriber receiving only explicitly allowlisted peer
	/// facts.
	pub fn subscribe_collab(&self, capacity: usize) -> CollabEventSubscription {
		let (tx, rx) = flume::bounded(capacity);
		let dropped = Arc::new(AtomicU64::new(0));
		self
			.inner
			.subscribers
			.lock()
			.collab
			.push(CollabSender { tx, dropped: dropped.clone() });
		CollabEventSubscription { rx, dropped }
	}

	/// Publishes an already shared event without another event allocation.
	pub fn publish_shared(&self, event: Arc<AgentEvent>) -> Arc<AgentEvent> {
		let mut subscribers = self.inner.subscribers.lock();
		subscribers
			.lossless
			.retain(|tx| tx.try_send(event.clone()).is_ok());
		subscribers
			.lossy
			.retain(|subscriber| match subscriber.tx.try_send(event.clone()) {
				Ok(()) => true,
				Err(flume::TrySendError::Full(_)) => {
					subscriber.dropped.fetch_add(1, Ordering::Relaxed);
					self.inner.dropped_lossy.fetch_add(1, Ordering::Relaxed);
					true
				},
				Err(flume::TrySendError::Disconnected(_)) => false,
			});
		if !subscribers.collab.is_empty()
			&& let Some(visibility) = collab_visibility(&event)
		{
			let projection = Arc::new(CollabEvent { visibility, event: event.clone() });
			subscribers
				.collab
				.retain(|subscriber| match subscriber.tx.try_send(projection.clone()) {
					Ok(()) => true,
					Err(flume::TrySendError::Full(_)) => {
						subscriber.dropped.fetch_add(1, Ordering::Relaxed);
						self.inner.dropped_lossy.fetch_add(1, Ordering::Relaxed);
						true
					},
					Err(flume::TrySendError::Disconnected(_)) => false,
				});
		}
		event
	}

	/// Publishes a phase transition after updating the allocation-free phase
	/// snapshot.
	pub fn transition(&self, from: AgentPhase, to: AgentPhase) -> Arc<AgentEvent> {
		self.inner.phase.store(to.encode(), Ordering::Release);
		let run_state = if matches!(to, AgentPhase::Idle) {
			AgentRunState::Idle
		} else {
			AgentRunState::Working
		};
		if self.run_state() != run_state {
			self.run_transition(run_state);
		}
		self.publish(AgentEvent::PhaseChanged { from, to })
	}

	/// Returns the latest phase without subscribing or allocating.
	pub fn phase(&self) -> AgentPhase {
		AgentPhase::decode(self.inner.phase.load(Ordering::Acquire))
	}

	/// Publishes a title only after the journal/index owner accepted it.
	pub fn title_changed(&self, title: Str, source: TitleSource) -> Arc<AgentEvent> {
		self.publish(AgentEvent::TitleChanged { title, source })
	}

	/// Updates and publishes the terminal/UI run state.
	pub fn run_transition(&self, to: AgentRunState) -> Arc<AgentEvent> {
		let from = AgentRunState::decode(self.inner.run_state.swap(to.encode(), Ordering::AcqRel));
		self.publish(AgentEvent::RunStateChanged { from, to })
	}

	/// Returns the latest terminal/UI run state.
	pub fn run_state(&self) -> AgentRunState {
		AgentRunState::decode(self.inner.run_state.load(Ordering::Acquire))
	}

	/// Replaces the session-generation stamp attached to subsequent typed
	/// observations.
	pub fn set_session_generation(&self, generation: u64) {
		self
			.inner
			.session_generation
			.store(generation, Ordering::Release);
	}

	/// Returns the generation stamped onto newly published typed observations.
	pub fn session_generation(&self) -> u64 {
		self.inner.session_generation.load(Ordering::Acquire)
	}

	/// Publishes a typed planning-state transition.
	pub fn plan_transition(&self, from: PlanState, to: PlanState) -> Arc<AgentEvent> {
		self.publish(AgentEvent::PlanStateChanged {
			from,
			to,
			session_generation: self.session_generation(),
		})
	}

	/// Returns the cumulative number of events dropped by all lossy subscribers.
	pub fn dropped_lossy(&self) -> u64 {
		self.inner.dropped_lossy.load(Ordering::Relaxed)
	}
}

fn collab_visibility(event: &AgentEvent) -> Option<CollabEventVisibility> {
	match event {
		AgentEvent::TitleChanged { .. }
		| AgentEvent::RunStateChanged { .. }
		| AgentEvent::PhaseChanged { .. }
		| AgentEvent::RosterChanged { .. }
		| AgentEvent::PlanStateChanged { .. }
		| AgentEvent::JobRegistered { .. }
		| AgentEvent::JobSettled { .. } => Some(CollabEventVisibility::PublicPresentation),
		AgentEvent::ToolObserved { visibility: EventVisibility::User, .. } => {
			Some(CollabEventVisibility::PublicTranscript)
		},
		AgentEvent::Turn { event, .. }
			if matches!(
				event.event.as_ref(),
				Some(
					turn_event::Event::PartStart(_)
						| turn_event::Event::PartDelta(_)
						| turn_event::Event::PartEnd(_)
						| turn_event::Event::Outcome(_)
				)
			) =>
		{
			Some(CollabEventVisibility::PublicTranscript)
		},
		_ => None,
	}
}

/// Receiving half of an ordered lossless event subscription.
pub struct EventSubscription {
	rx: Receiver<Arc<AgentEvent>>,
}

impl EventSubscription {
	/// Receives the next event asynchronously.
	pub async fn recv(&self) -> Result<Arc<AgentEvent>, flume::RecvError> {
		self.rx.recv_async().await
	}

	/// Attempts to receive the next event without blocking.
	pub fn try_recv(&self) -> Result<Arc<AgentEvent>, flume::TryRecvError> {
		self.rx.try_recv()
	}

	/// Returns the number of events currently buffered for this subscriber.
	pub fn len(&self) -> usize {
		self.rx.len()
	}

	/// Returns whether this subscriber currently has no buffered events.
	pub fn is_empty(&self) -> bool {
		self.rx.is_empty()
	}
}

/// Receiving half of an ordered bounded collaboration projection.
pub struct CollabEventSubscription {
	rx:      Receiver<Arc<CollabEvent>>,
	dropped: Arc<AtomicU64>,
}

impl CollabEventSubscription {
	/// Receives the next retained allowlisted event asynchronously.
	pub async fn recv(&self) -> Result<Arc<CollabEvent>, flume::RecvError> {
		self.rx.recv_async().await
	}

	/// Attempts to receive the next retained allowlisted event without blocking.
	pub fn try_recv(&self) -> Result<Arc<CollabEvent>, flume::TryRecvError> {
		self.rx.try_recv()
	}

	/// Returns the cumulative number of collaboration events dropped for this
	/// subscriber.
	pub fn dropped(&self) -> u64 {
		self.dropped.load(Ordering::Relaxed)
	}

	/// Returns the number of retained collaboration events currently buffered.
	pub fn len(&self) -> usize {
		self.rx.len()
	}

	/// Returns whether this subscriber currently has no buffered events.
	pub fn is_empty(&self) -> bool {
		self.rx.is_empty()
	}
}

/// Receiving half of an ordered bounded UI event subscription.
pub struct LossyEventSubscription {
	rx:      Receiver<Arc<AgentEvent>>,
	dropped: Arc<AtomicU64>,
}

impl LossyEventSubscription {
	/// Receives the next retained event asynchronously.
	pub async fn recv(&self) -> Result<Arc<AgentEvent>, flume::RecvError> {
		self.rx.recv_async().await
	}

	/// Attempts to receive the next retained event without blocking.
	pub fn try_recv(&self) -> Result<Arc<AgentEvent>, flume::TryRecvError> {
		self.rx.try_recv()
	}

	/// Returns the cumulative number of events dropped for this subscriber.
	pub fn dropped(&self) -> u64 {
		self.dropped.load(Ordering::Relaxed)
	}

	/// Returns the number of retained events currently buffered.
	pub fn len(&self) -> usize {
		self.rx.len()
	}

	/// Returns whether this subscriber currently has no buffered events.
	pub fn is_empty(&self) -> bool {
		self.rx.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::{AgentEvent, AgentPhase, EventBus, PlanState};

	#[test]
	fn phase_snapshot_tracks_transitions_across_clones() {
		let bus = EventBus::new();
		let clone = bus.clone();
		let events = bus.subscribe_lossless();

		assert_eq!(bus.phase(), AgentPhase::Idle);
		clone.transition(AgentPhase::Idle, AgentPhase::Projecting);
		assert_eq!(bus.phase(), AgentPhase::Projecting);

		let mut saw_phase_change = false;
		while let Ok(event) = events.try_recv() {
			if matches!(event.as_ref(), AgentEvent::PhaseChanged {
				from: AgentPhase::Idle,
				to:   AgentPhase::Projecting,
			}) {
				saw_phase_change = true;
				break;
			}
		}
		assert!(saw_phase_change, "transition must remain observable");
		assert_eq!(clone.phase(), AgentPhase::Projecting);

		bus.transition(AgentPhase::Projecting, AgentPhase::Turning);
		assert_eq!(clone.phase(), AgentPhase::Turning);
	}

	#[test]
	fn plan_events_preserve_order_and_session_generation() {
		let bus = EventBus::new();
		bus.set_session_generation(17);
		let events = bus.subscribe_lossless();
		bus.plan_transition(PlanState::Inactive, PlanState::Active);
		bus.plan_transition(PlanState::Active, PlanState::Yolo);

		let first = events.try_recv().expect("first plan event");
		let second = events.try_recv().expect("second plan event");
		assert!(matches!(first.as_ref(), AgentEvent::PlanStateChanged {
			from:               PlanState::Inactive,
			to:                 PlanState::Active,
			session_generation: 17,
		}));
		assert!(matches!(second.as_ref(), AgentEvent::PlanStateChanged {
			from:               PlanState::Active,
			to:                 PlanState::Yolo,
			session_generation: 17,
		}));
	}
}
