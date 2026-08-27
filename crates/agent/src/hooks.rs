//! Subscription masks and the per-invocation hook decision procedure.

use std::sync::atomic::{AtomicU64, Ordering};

use bytes::{Bytes, BytesMut};
use flume::Receiver;
use omp_core::{Str, sf};
use omp_proto::toolhost::v1::HookEventId;
use parking_lot::Mutex;
use smallvec::SmallVec;

use crate::{ApprovalSpec, HookDecision, HookPhase};

/// The number of atomic words needed by the stable hook catalog through ordinal
/// 127.
const MASK_WORDS: usize = 2;
/// A transform phase may make exactly one ordered pass.
pub const MODIFY_ROUNDS: u8 = 1;
/// No event may have more than this many observe-only handlers.
pub const OBSERVE_HANDLER_CAP: usize = 64;

/// The composition rule for one mutable event field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Composition {
	/// Later ordered writes replace the preceding value.
	Replace   = 1,
	/// Values are appended in subscription order.
	Append    = 2,
	/// Values are narrowed by intersection.
	Intersect = 3,
}

/// Failure policy declared by a hook subscription.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum OnFailure {
	/// A failed host abstains.
	Defer = 1,
	/// A failed host is represented by a synthetic denial.
	Deny  = 2,
}

/// Data-only filter evaluated by Core before dispatching a subscription.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct When {
	/// Tool names accepted by this subscription; empty accepts every name.
	pub names:   Vec<Str>,
	/// Target names accepted by this subscription; empty accepts every target.
	pub targets: Vec<Str>,
}

impl When {
	/// Returns whether this filter accepts the supplied target and name.
	pub fn matches(&self, target: &str, name: &str) -> bool {
		(self.names.is_empty() || self.names.iter().any(|value| value.as_str() == name))
			&& (self.targets.is_empty() || self.targets.iter().any(|value| value.as_str() == target))
	}
}

/// Authenticated extension provenance used to order and journal domain replies.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceRef {
	/// Install-layer ordering key.
	pub layer:        u32,
	/// Publisher identity within the layer.
	pub publisher:    Str,
	/// Stable extension identity within the publisher.
	pub extension_id: Str,
}

/// One registered handler and the Core-validated ordering facts it declared.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscription {
	/// Authenticated host identity.
	pub host:       Str,
	/// Authenticated extension provenance for deterministic domain ordering.
	pub source:     SourceRef,
	/// Per-host subscription identity.
	pub id:         u32,
	/// Stable catalog event.
	pub event:      HookEventId,
	/// Stage in which the subscription participates.
	pub phase:      HookPhase,
	/// Stable total ordering key used only by TRANSFORM.
	pub order:      i32,
	/// Host-loss behavior cross-checked at activation.
	pub on_failure: OnFailure,
	/// Core-side data-only applicability predicate.
	pub when:       When,
}

/// A gateable admission event. Its two mutable fields use REPLACE composition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateEvent {
	/// Environment-fixed requested call target.
	pub requested_target:    Str,
	/// Canonical requested argument bytes.
	pub requested_args:      Bytes,
	/// Effective target after accepted transforms.
	pub effective_target:    Str,
	/// Effective arguments after accepted transforms.
	pub effective_args:      Bytes,
	/// Incremented after each accepted transform so later phases derive fresh
	/// facts.
	pub derived_ir_revision: u32,
}

impl GateEvent {
	/// Creates an unmodified gate event from canonical requested facts.
	pub fn new(target: Str, args: Bytes) -> Self {
		Self {
			requested_target:    target.clone(),
			requested_args:      args.clone(),
			effective_target:    target,
			effective_args:      args,
			derived_ir_revision: 0,
		}
	}
}

/// A decoded domain-specific hook result.
///
/// Domain events are fail-open: malformed, absent, and failed host replies
/// return [`DomainReturn::fail_open`] rather than becoming an admission denial.
pub trait DomainReturn: Sized + Clone {
	/// Decodes `HookDecision.domain` bytes emitted by a host.
	fn decode_domain(bytes: &[u8]) -> Option<Self>;
	/// Returns this family's specified fail-open result.
	fn fail_open() -> Self;
	/// Combines replies in deterministic subscription order.
	fn merge_domain(self, next: Self) -> Self {
		let _ = self;
		next
	}
}

impl DomainReturn for () {
	fn decode_domain(_: &[u8]) -> Option<Self> {
		Some(())
	}

	fn fail_open() -> Self {}
}

/// Domain result for `agent_settled`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentSettled {
	/// Continue the agent loop with another turn.
	Continue,
	/// Settle the current agent invocation.
	Settle,
}

impl DomainReturn for AgentSettled {
	fn decode_domain(bytes: &[u8]) -> Option<Self> {
		match bytes {
			b"continue" => Some(Self::Continue),
			b"settle" => Some(Self::Settle),
			_ => None,
		}
	}

	fn fail_open() -> Self {
		Self::Settle
	}
}

/// Domain result for `provider_error`.
///
/// The wire payload is a JSON array of model-route names in failover order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderFailover(pub Bytes);

impl ProviderFailover {
	/// Decodes the validated failover chain.
	pub fn routes(&self) -> Vec<Str> {
		serde_json::from_slice(self.0.as_ref()).unwrap_or_default()
	}
}

impl DomainReturn for ProviderFailover {
	fn decode_domain(bytes: &[u8]) -> Option<Self> {
		let routes = serde_json::from_slice::<Vec<Str>>(bytes).ok()?;
		(!routes.is_empty()).then(|| Self(Bytes::copy_from_slice(bytes)))
	}

	fn fail_open() -> Self {
		Self(Bytes::new())
	}
}

/// Hook payload emitted after a retryable provider failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderErrorEvent {
	/// Stable classified failure code.
	pub code:    Str,
	/// Durable turn that failed.
	pub turn_id: Str,
}

impl HookEvent for ProviderErrorEvent {
	type Return = ProviderFailover;

	const ID: HookEventId = HookEventId::HookEventProviderError;
	const REV: u32 = 1;

	fn encode_into(&self, out: &mut BytesMut) {
		out.extend_from_slice(self.code.as_bytes());
		out.extend_from_slice(b"\n");
		out.extend_from_slice(self.turn_id.as_bytes());
	}

	fn apply(&mut self, _: &HookPatch) -> Result<(), GateError> {
		Ok(())
	}
}

/// Domain result for `thread_projection`; validation is owned by `context`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextPatch(pub Bytes);

impl DomainReturn for ContextPatch {
	fn decode_domain(bytes: &[u8]) -> Option<Self> {
		Some(Self(Bytes::copy_from_slice(bytes)))
	}

	fn fail_open() -> Self {
		Self(Bytes::new())
	}
}

/// A hook event with a stable wire identity and reversible patch application.
pub trait HookEvent {
	/// Stable dense catalog identity.
	const ID: HookEventId;
	/// Schema revision of the encoded payload.
	const REV: u32;
	/// Domain return family, or `()` for ordinary five-arm decisions.
	type Return: DomainReturn;
	/// Encodes the event payload into the supplied reusable buffer.
	fn encode_into(&self, out: &mut BytesMut);
	/// Applies an accepted transform under the event's fixed composition table.
	fn apply(&mut self, patch: &HookPatch) -> Result<(), GateError>;
}

impl HookEvent for GateEvent {
	type Return = ();

	const ID: HookEventId = HookEventId::HookEventToolCall;
	const REV: u32 = 1;

	fn encode_into(&self, out: &mut BytesMut) {
		out.extend_from_slice(self.effective_target.as_bytes());
		out.extend_from_slice(b"\n");
		out.extend_from_slice(&self.effective_args);
	}

	fn apply(&mut self, patch: &HookPatch) -> Result<(), GateError> {
		if let Some(target) = &patch.target {
			self.effective_target = target.clone();
		}
		if let Some(args) = &patch.args {
			self.effective_args = args.clone();
		}
		self.derived_ir_revision = self.derived_ir_revision.saturating_add(1);
		Ok(())
	}
}

/// One accepted mutation returned by a TRANSFORM subscription.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HookPatch {
	/// Replacement target, subject to the caller's capability bounds.
	pub target: Option<Str>,
	/// Replacement canonical argument bytes.
	pub args:   Option<Bytes>,
}

/// A decision response returned by a host or a synthetic failure stub.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GateDecision {
	/// An affirmative vote.
	Allow,
	/// A terminal refusal and stable reason.
	Deny(Str),
	/// A legal TRANSFORM patch.
	Modify(HookPatch),
	/// No opinion.
	Defer,
	/// A domain-family payload encoded in the existing wire `domain` field.
	Domain(Bytes),
	/// A legal APPROVAL requirement.
	RequireApproval(ApprovalSpec),
}

impl GateDecision {
	const fn arm(&self) -> Option<HookDecision> {
		Some(match self {
			Self::Allow => HookDecision::Allow,
			Self::Deny(_) => HookDecision::Deny,
			Self::Modify(_) => HookDecision::Modify,
			Self::Defer => HookDecision::Defer,
			Self::RequireApproval(_) => HookDecision::RequireApproval,
			Self::Domain(_) => return None,
		})
	}
}

/// One dispatch offered to a host. A receiver responds through
/// [`HookGate::answer`].
#[derive(Debug)]
pub struct HookDispatch {
	/// Correlates the host reply with a pending decision.
	pub dispatch_id:   u64,
	/// Event identity.
	pub event:         HookEventId,
	/// Event revision.
	pub rev:           u32,
	/// Current decision stage.
	pub phase:         HookPhase,
	/// Subscriptions selected for this host and stage.
	pub subscriptions: Vec<Subscription>,
	/// Reusable encoded event payload.
	pub payload:       Bytes,
}

/// A durable record of one winning transform overwrite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransformTrail {
	/// Subscription that supplied the mutation.
	pub subscription_id:  u32,
	/// Previous effective target.
	pub previous_target:  Str,
	/// Previous effective argument bytes.
	pub previous_args:    Bytes,
	/// Resulting effective target.
	pub effective_target: Str,
	/// Resulting effective argument bytes.
	pub effective_args:   Bytes,
}

/// The completed per-invocation decision result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GateOutcome {
	/// No handler denied and no human ticket is required.
	Allow {
		/// Effective event after all accepted transforms.
		event: GateEvent,
		/// Ordered transform overwrite audit trail.
		trail: Vec<TransformTrail>,
	},
	/// A handler or synthetic stub refused the invocation.
	Deny {
		/// Effective event at the point of refusal.
		event:  GateEvent,
		/// Stable refusal reason.
		reason: Str,
		/// Ordered transform overwrite audit trail before refusal.
		trail:  Vec<TransformTrail>,
	},
	/// Approval requirements are merged by the Core-owned ticket book.
	Approval {
		/// Effective event awaiting human or external approval.
		event: GateEvent,
		/// All approval requirements returned during APPROVAL.
		specs: Vec<ApprovalSpec>,
		/// Ordered transform overwrite audit trail.
		trail: Vec<TransformTrail>,
	},
}

/// Domain gate result retaining each valid responder's authenticated
/// provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DomainOutcome<R> {
	/// Family-composed result, or the family's fail-open default.
	pub winner:        R,
	/// Decoded contributions in deterministic `(layer, publisher, extension_id)`
	/// order.
	pub contributions: SmallVec<(SourceRef, R), 2>,
}

/// Invalid dispatch input or illegal host decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GateError {
	/// A decision arm was not legal for the phase which reported it.
	IllegalDecision,
	/// The response did not correspond to a pending dispatch.
	UnknownDispatch,
}

struct Pending {
	response: flume::Sender<Vec<(u32, GateDecision)>>,
}

/// Core-owned subscription bitmap, dispatch queue, and pending reply table.
///
/// Unsubscribed emission performs only one relaxed load, one bit-and, and a
/// branch; it does not construct a payload or frame.
pub struct HookGate {
	mask:             [AtomicU64; MASK_WORDS],
	dispatch:         flume::Sender<HookDispatch>,
	pending:          Mutex<omp_core::SparseMap<u64, Pending>>,
	next_id:          AtomicU64,
	subscriptions:    Mutex<Vec<Subscription>>,
	dropped_notifies: AtomicU64,
}

impl HookGate {
	/// Creates a gate and the bounded lossy observer-dispatch receiver.
	pub fn channel() -> (Self, Receiver<HookDispatch>) {
		let (dispatch, receive) = flume::bounded(OBSERVE_HANDLER_CAP);
		(
			Self {
				mask: [const { AtomicU64::new(0) }; MASK_WORDS],
				dispatch,
				pending: Mutex::new(omp_core::SparseMap::new()),
				next_id: AtomicU64::new(1),
				subscriptions: Mutex::new(Vec::new()),
				dropped_notifies: AtomicU64::new(0),
			},
			receive,
		)
	}

	/// Replaces one host's subscriptions and publishes their event bits.
	pub fn subscribe(
		&self,
		host: &str,
		subscriptions: impl IntoIterator<Item = Subscription>,
	) -> Result<(), GateError> {
		let mut subscriptions = subscriptions.into_iter().collect::<Vec<_>>();
		for subscription in &mut subscriptions {
			if subscription.host.as_str() != host {
				return Err(GateError::UnknownDispatch);
			}
		}
		let observe = subscriptions
			.iter()
			.filter(|value| value.phase == HookPhase::Observe)
			.count();
		if observe > OBSERVE_HANDLER_CAP {
			return Err(GateError::IllegalDecision);
		}
		let mut registered = self.subscriptions.lock();
		registered.retain(|value| value.host.as_str() != host);
		registered.extend(subscriptions);
		let mut mask = [0_u64; MASK_WORDS];
		for subscription in registered.iter() {
			let (word, bit) = event_position(subscription.event);
			mask[word] |= bit;
		}
		for (word, value) in self.mask.iter().zip(mask) {
			word.store(value, Ordering::Release);
		}
		Ok(())
	}

	/// Returns whether an event has any subscribed or fail-closed stub bit.
	#[inline]
	pub fn subscribed(&self, event: HookEventId) -> bool {
		let (word, bit) = event_position(event);
		self.mask[word].load(Ordering::Relaxed) & bit != 0
	}

	/// Emits an observation without waiting; a full queue is accounted and
	/// dropped.
	pub fn notify<E: HookEvent>(&self, event: &E) {
		if !self.subscribed(E::ID) {
			return;
		}
		let mut encoded = BytesMut::new();
		event.encode_into(&mut encoded);
		let dispatch = HookDispatch {
			dispatch_id:   self.next_id.fetch_add(1, Ordering::Relaxed),
			event:         E::ID,
			rev:           E::REV,
			phase:         HookPhase::Observe,
			subscriptions: self.selected(E::ID, HookPhase::Observe, "", ""),
			payload:       encoded.freeze(),
		};
		if self.dispatch.try_send(dispatch).is_err() {
			self.dropped_notifies.fetch_add(1, Ordering::Relaxed);
		}
	}

	/// Returns the number of observer frames dropped due to bounded
	/// backpressure.
	pub fn dropped_notifies(&self) -> u64 {
		self.dropped_notifies.load(Ordering::Relaxed)
	}

	/// Resolves one host dispatch after validating the subscription ids and
	/// phase.
	pub fn answer(
		&self,
		dispatch_id: u64,
		decisions: Vec<(u32, GateDecision)>,
	) -> Result<(), GateError> {
		let pending = self
			.pending
			.lock()
			.remove(dispatch_id)
			.ok_or(GateError::UnknownDispatch)?;
		pending
			.response
			.send(decisions)
			.map_err(|_| GateError::UnknownDispatch)
	}

	/// Runs the phase-ordered decision procedure without boxing its future.
	pub async fn gate(&self, mut event: GateEvent) -> GateOutcome {
		let mut trail = Vec::new();
		let mut approvals = Vec::new();
		for phase in
			[HookPhase::Precheck, HookPhase::Transform, HookPhase::Review, HookPhase::Approval]
		{
			let replies = self.dispatch_phase(&event, phase).await;
			for (subscription, decision) in replies {
				if decision.arm().is_none_or(|arm| !arm.is_legal_in(phase)) {
					return GateOutcome::Deny { event, reason: sf!("illegal hook decision"), trail };
				}
				match decision {
					GateDecision::Deny(reason) => return GateOutcome::Deny { event, reason, trail },
					GateDecision::Modify(patch) => {
						let previous_target = event.effective_target.clone();
						let previous_args = event.effective_args.clone();
						let _ = event.apply(&patch);
						trail.push(TransformTrail {
							subscription_id: subscription.id,
							previous_target,
							previous_args,
							effective_target: event.effective_target.clone(),
							effective_args: event.effective_args.clone(),
						});
					},
					GateDecision::RequireApproval(spec) => approvals.push(spec),
					GateDecision::Allow | GateDecision::Defer | GateDecision::Domain(_) => {},
				}
			}
		}
		if approvals.is_empty() {
			GateOutcome::Allow { event, trail }
		} else {
			GateOutcome::Approval { event, specs: approvals, trail }
		}
	}

	/// Dispatches a domain-return event and preserves each valid responder.
	///
	/// Contributions are ordered by `(layer, publisher, extension_id)` so
	/// callers can select a winner and journal deterministic losers. Missing,
	/// malformed, and failed replies retain the family's fail-open default.
	pub async fn gate_domain<'gate, E: HookEvent>(
		&'gate self,
		event: &'gate E,
	) -> DomainOutcome<E::Return> {
		let mut result = DomainReturn::fail_open();
		let mut contributions: SmallVec<(SourceRef, E::Return), 2> = SmallVec::new();
		if !self.subscribed(E::ID) {
			return DomainOutcome { winner: result, contributions };
		}
		let mut payload = BytesMut::new();
		event.encode_into(&mut payload);
		let payload = payload.freeze();
		for phase in HookPhase::ALL {
			let mut subscriptions = self.selected(E::ID, phase, "", "");
			subscriptions.sort_by_key(|subscription| subscription.source.clone());
			for subscription in subscriptions {
				let dispatch_id = self.next_id.fetch_add(1, Ordering::Relaxed);
				let (reply, receive) = flume::bounded(1);
				self
					.pending
					.lock()
					.insert(dispatch_id, Pending { response: reply });
				let dispatch = HookDispatch {
					dispatch_id,
					event: E::ID,
					rev: E::REV,
					phase,
					subscriptions: vec![subscription.clone()],
					payload: payload.clone(),
				};
				if self.dispatch.send_async(dispatch).await.is_err() {
					self.pending.lock().remove(dispatch_id);
					continue;
				}
				let Ok(decisions) = receive.recv_async().await else {
					continue;
				};
				for (reported_id, decision) in decisions {
					if reported_id != subscription.id {
						continue;
					}
					if let GateDecision::Domain(bytes) = decision
						&& let Some(next) = <E::Return as DomainReturn>::decode_domain(&bytes)
					{
						result = result.merge_domain(next.clone());
						contributions.push((subscription.source.clone(), next));
					}
				}
			}
		}
		DomainOutcome { winner: result, contributions }
	}

	async fn dispatch_phase(
		&self,
		event: &GateEvent,
		phase: HookPhase,
	) -> Vec<(Subscription, GateDecision)> {
		let mut selected = self.selected(
			HookEventId::HookEventToolCall,
			phase,
			event.effective_target.as_str(),
			event.effective_target.as_str(),
		);
		if phase == HookPhase::Transform {
			selected.sort_by_key(|subscription| {
				(subscription.order, subscription.host.clone(), subscription.id)
			});
		}
		let mut replies = Vec::new();
		for subscription in selected {
			let id = self.next_id.fetch_add(1, Ordering::Relaxed);
			let (reply, receive) = flume::bounded(1);
			self.pending.lock().insert(id, Pending { response: reply });
			let mut payload = BytesMut::new();
			event.encode_into(&mut payload);
			let dispatch = HookDispatch {
				dispatch_id: id,
				event: HookEventId::HookEventToolCall,
				rev: GateEvent::REV,
				phase,
				subscriptions: vec![subscription.clone()],
				payload: payload.freeze(),
			};
			if self.dispatch.send_async(dispatch).await.is_err() {
				self.pending.lock().remove(id);
				if subscription.on_failure == OnFailure::Deny {
					replies
						.push((subscription, GateDecision::Deny(sf!("required hook host unavailable"))));
				}
				continue;
			}
			match receive.recv_async().await {
				Ok(decisions) => {
					for (reported, decision) in decisions {
						if reported == subscription.id {
							replies.push((subscription.clone(), decision));
						} else {
							replies.push((
								subscription.clone(),
								GateDecision::Deny(sf!("hook reported a different subscription",)),
							));
						}
					}
				},
				Err(_) if subscription.on_failure == OnFailure::Deny => {
					replies.push((subscription, GateDecision::Deny(sf!("required hook host failed"))));
				},
				Err(_) => {},
			}
		}
		replies
	}

	fn selected(
		&self,
		event: HookEventId,
		phase: HookPhase,
		target: &str,
		name: &str,
	) -> Vec<Subscription> {
		self
			.subscriptions
			.lock()
			.iter()
			.filter(|subscription| {
				subscription.event == event
					&& subscription.phase == phase
					&& subscription.when.matches(target, name)
			})
			.cloned()
			.collect()
	}
}

const fn event_position(event: HookEventId) -> (usize, u64) {
	let ordinal = event as usize;
	(ordinal / 64, 1_u64 << (ordinal % 64))
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use omp_proto::toolhost::v1::HookEventId;

	use super::{
		AgentSettled, DomainReturn, GateDecision, GateError, GateEvent, GateOutcome, HookEvent,
		HookGate, HookPatch, HookPhase, OnFailure, ProviderFailover, SourceRef, Subscription, When,
	};

	#[test]
	fn provider_failover_requires_a_nonempty_typed_route_chain() {
		let failover =
			ProviderFailover::decode_domain(br#"["model-a","model-b"]"#).expect("valid route chain");
		assert_eq!(failover.routes(), vec![sf!("model-a"), sf!("model-b")]);
		assert!(ProviderFailover::decode_domain(b"[]").is_none());
		assert!(ProviderFailover::decode_domain(b"not-json").is_none());
	}

	fn subscription(phase: HookPhase, id: u32) -> Subscription {
		Subscription {
			host: sf!("test"),
			source: SourceRef {
				layer:        0,
				publisher:    sf!("test"),
				extension_id: sf!("test"),
			},
			id,
			event: HookEventId::HookEventToolCall,
			phase,
			order: 0,
			on_failure: OnFailure::Defer,
			when: When::default(),
		}
	}

	#[test]
	fn mask_fast_path_stays_empty_until_subscription() {
		let (gate, _) = HookGate::channel();
		assert!(!gate.subscribed(HookEventId::HookEventToolCall));
		assert!(!gate.subscribed(HookEventId::HookEventMcpNotification));
		let tool = subscription(HookPhase::Observe, 1);
		let mut mcp = subscription(HookPhase::Observe, 2);
		mcp.event = HookEventId::HookEventMcpNotification;
		gate.subscribe("test", [tool, mcp]).unwrap();
		assert!(gate.subscribed(HookEventId::HookEventToolCall));
		assert!(gate.subscribed(HookEventId::HookEventMcpNotification));
	}

	#[tokio::test]
	async fn transform_is_ordered_and_trails_every_overwrite() {
		let (gate, receiver) = HookGate::channel();
		let mut first = subscription(HookPhase::Transform, 1);
		first.order = 1;
		let mut second = subscription(HookPhase::Transform, 2);
		second.order = 2;
		gate.subscribe("test", [first, second]).unwrap();
		let gate_future = gate.gate(GateEvent::new(sf!("bash"), Bytes::from_static(b"{}")));
		let driver = async {
			for expected in [1, 2] {
				let dispatch = receiver.recv_async().await.unwrap();
				let patch = HookPatch {
					target: None,
					args:   Some(Bytes::from(format!("{{\"n\":{expected}}}"))),
				};
				gate
					.answer(dispatch.dispatch_id, vec![(expected, GateDecision::Modify(patch))])
					.unwrap();
			}
		};
		let (outcome, ()) = tokio::join!(gate_future, driver);
		let GateOutcome::Allow { event, trail } = outcome else {
			panic!("expected allow");
		};
		assert_eq!(event.effective_args, Bytes::from_static(b"{\"n\":2}"));
		assert_eq!(trail.len(), 2);
	}

	#[tokio::test]
	async fn deny_short_circuits_later_phases() {
		let (gate, rx) = HookGate::channel();
		gate
			.subscribe("test", [
				subscription(HookPhase::Precheck, 1),
				subscription(HookPhase::Review, 2),
			])
			.unwrap();
		let work = gate.gate(GateEvent::new(sf!("bash"), Bytes::from_static(b"{}")));
		let driver = async {
			let dispatch = rx.recv_async().await.unwrap();
			gate
				.answer(dispatch.dispatch_id, vec![(1, GateDecision::Deny(sf!("no")))])
				.unwrap();
		};
		let (outcome, ()) = tokio::join!(work, driver);
		assert!(matches!(outcome, GateOutcome::Deny { .. }));
		assert!(rx.try_recv().is_err());
	}
	#[tokio::test]
	async fn fail_closed_subscription_synthesizes_deny_when_host_is_gone() {
		let (gate, receiver) = HookGate::channel();
		let mut required = subscription(HookPhase::Precheck, 3);
		required.on_failure = OnFailure::Deny;
		gate.subscribe("test", [required]).unwrap();
		drop(receiver);
		assert!(matches!(
			gate
				.gate(GateEvent::new(sf!("bash"), Bytes::from_static(b"{}")))
				.await,
			super::GateOutcome::Deny { .. }
		));
	}
	#[derive(Clone)]
	struct SettledEvent;
	impl HookEvent for SettledEvent {
		type Return = AgentSettled;

		const ID: HookEventId = HookEventId::HookEventAgentSettled;
		const REV: u32 = 1;

		fn encode_into(&self, _: &mut bytes::BytesMut) {}

		fn apply(&mut self, _: &HookPatch) -> Result<(), GateError> {
			Ok(())
		}
	}

	#[tokio::test]
	async fn domain_return_decodes_existing_domain_arm() {
		let (gate, rx) = HookGate::channel();
		let mut domain_subscription = subscription(HookPhase::Review, 9);
		domain_subscription.event = HookEventId::HookEventAgentSettled;
		gate.subscribe("test", [domain_subscription]).unwrap();
		let gate_future = gate.gate_domain(&SettledEvent);
		let driver = async {
			let dispatch = rx.recv_async().await.unwrap();
			gate
				.answer(dispatch.dispatch_id, vec![(
					9,
					GateDecision::Domain(Bytes::from_static(b"continue")),
				)])
				.unwrap();
		};
		let (outcome, ()) = tokio::join!(gate_future, driver);
		assert_eq!(outcome.winner, super::AgentSettled::Continue);
		assert_eq!(outcome.contributions.len(), 1);
	}
}
