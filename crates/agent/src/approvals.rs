//! Durable approval prompts backed by the authoritative session DOM.

use std::{
	str::FromStr,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use omp_core::{Str, sf};
use omp_dom::{Handle, KnownTag, NodeSpec, Op, PropId, PropKey, Tag, Txn, Value};
use omp_session::{Session, SessionError, components::prompts::prompts_handle};
use parking_lot::Mutex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use tokio::time;

/// One requirement merged into an invocation's approval prompt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalSpec {
	/// Short user-visible description.
	pub title:         Str,
	/// TML-safe explanatory text.
	pub body:          Str,
	/// Exact command, path, or device subject.
	pub subject:       Str,
	/// Presentation and configuration vocabulary such as `exec` or `write`.
	pub kind:          Str,
	/// Offered grant scopes in strictness order.
	pub scopes:        Vec<Str>,
	/// Optional timeout default; the kernel never invents one.
	pub default:       Option<bool>,
	/// Requested approver route.
	pub route:         Str,
	/// Optional named external approver.
	pub approver:      Option<Str>,
	/// Maximum wait in milliseconds; zero means no timeout.
	pub timeout_ms:    u64,
	/// Unreachable-route behavior.
	pub unreachable:   Str,
	/// Forbids extension-sourced decisions.
	pub require_human: bool,
	/// Scope-bearing approval pattern.
	pub pattern:       Option<Str>,
	/// Rule and derived-fact evidence.
	pub evidence:      Vec<Str>,
}

/// Granted lifetime of an approval decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalScope {
	/// This operation only.
	Once,
	/// This call only.
	Call,
	/// The current turn.
	Turn,
	/// The current session.
	Session,
	/// Persisted policy.
	Persist,
	/// A forward-compatible host-defined scope.
	Custom(Str),
}

impl ApprovalScope {
	/// Returns the stable wire spelling.
	#[must_use]
	pub fn as_str(&self) -> &str {
		match self {
			Self::Once => "once",
			Self::Call => "call",
			Self::Turn => "turn",
			Self::Session => "session",
			Self::Persist => "persist",
			Self::Custom(value) => value.as_str(),
		}
	}
}

impl FromStr for ApprovalScope {
	type Err = core::convert::Infallible;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		Ok(match value {
			"once" => Self::Once,
			"call" => Self::Call,
			"turn" => Self::Turn,
			"session" => Self::Session,
			"persist" => Self::Persist,
			other => Self::Custom(Str::new(other)),
		})
	}
}

impl Serialize for ApprovalScope {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.serialize_str(self.as_str())
	}
}

impl<'de> Deserialize<'de> for ApprovalScope {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let value = Str::deserialize(deserializer)?;
		Ok(Self::from_str(value.as_str()).expect("approval scope parsing is infallible"))
	}
}

/// Durable state of an approval prompt.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TicketState {
	/// Awaiting one idempotent answer.
	Pending,
	/// Answered exactly once.
	Decided,
	/// Invocation ended before an answer.
	Withdrawn,
}

/// Source that supplied an approval result.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ApprovalSource {
	/// A local user answered.
	User,
	/// An authenticated external approver answered.
	External,
	/// A parent agent answered.
	Forwarded,
	/// Frozen configuration pre-answered the prompt.
	Config,
	/// An authorized extension answered.
	Extension,
	/// An explicit timeout default answered.
	Timeout,
	/// The requested route was unavailable.
	Unavailable,
}

/// One idempotent approval result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalDecision {
	/// Whether every merged requirement is approved.
	pub approved:   bool,
	/// Granted lifetime.
	pub scope:      ApprovalScope,
	/// Source of the answer.
	pub source:     ApprovalSource,
	/// Optional authenticated decider.
	pub decided_by: Option<Str>,
	/// Optional user-visible rationale.
	pub reason:     Option<Str>,
	/// Whether a fail-open result was durably audited.
	pub audited:    bool,
}

/// Journal-derived approval prompt projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalTicket {
	/// Stable idempotency key.
	pub ticket_id:     Str,
	/// Invocation this prompt blocks, if any.
	pub invocation_id: Option<Str>,
	/// Every unresolved requirement in filing order.
	pub reasons:       Vec<ApprovalSpec>,
	/// Current durable state.
	pub state:         TicketState,
	/// Present only after a decision.
	pub decision:      Option<ApprovalDecision>,
	/// Journal-clock epoch milliseconds at filing.
	pub created_at_ms: u64,
}

/// Approval DOM operation failure.
#[derive(Debug, Error)]
pub enum ApprovalError {
	/// Session persistence or DOM mutation failed.
	#[error(transparent)]
	Session(#[from] SessionError),
	/// The canonical prompts subtree is absent.
	#[error("session prompts component is absent")]
	MissingPrompts,
	/// The requested ticket does not exist.
	#[error("approval ticket {id} does not exist")]
	UnknownTicket {
		/// Missing ticket id.
		id: Str,
	},
	/// Stored prompt JSON is malformed.
	#[error("approval prompt state is malformed")]
	Malformed(#[from] serde_json::Error),
}

/// Stateless runtime index over `<queues><prompts>`.
///
/// Every lookup is rebuilt from the DOM; this value owns only an id prefix and
/// can never disagree with replayed session state.
pub struct ApprovalBook {
	prefix: Str,
}

impl ApprovalBook {
	/// Creates the ordinary approval family.
	#[must_use]
	pub const fn new() -> Self {
		Self { prefix: Str::new_static("approval") }
	}

	/// Creates a disjoint approval id family.
	#[must_use]
	pub fn with_prefix(prefix: impl Into<Str>) -> Self {
		Self { prefix: prefix.into() }
	}

	/// Opens one prompt in the session tree.
	pub fn open(
		&self,
		session: &mut Session,
		spec: ApprovalSpec,
	) -> Result<ApprovalTicket, ApprovalError> {
		self.open_for(session, None, vec![spec], epoch_millis())
	}

	/// Opens or merges the prompt for an invocation.
	pub fn open_for(
		&self,
		session: &mut Session,
		invocation_id: Option<Str>,
		reasons: Vec<ApprovalSpec>,
		created_at_ms: u64,
	) -> Result<ApprovalTicket, ApprovalError> {
		let existing = invocation_id.as_deref().and_then(|invocation_id| {
			tickets(session).find(|(_, ticket)| {
				ticket.invocation_id.as_deref() == Some(invocation_id)
					&& ticket.state == TicketState::Pending
			})
		});
		if let Some((handle, mut ticket)) = existing {
			ticket.reasons.extend(reasons);
			let encoded = serde_json::to_string(&ticket)?;
			session.patch(Txn {
				cause: session.head().ok_or(ApprovalError::MissingPrompts)?,
				label: Some(Str::new_static("approval.merge")),
				ops:   vec![Op::Set {
					h:     handle,
					prop:  custom("ticket"),
					value: Value::Str(Str::new(encoded)),
				}],
			})?;
			return Ok(ticket);
		}

		let prompts = prompts_handle(session.dom()).ok_or(ApprovalError::MissingPrompts)?;
		let ticket_id = sf!("{}-{}", self.prefix, session.dom().high_water().saturating_add(1));
		let ticket = ApprovalTicket {
			ticket_id: ticket_id.clone(),
			invocation_id,
			reasons,
			state: TicketState::Pending,
			decision: None,
			created_at_ms,
		};
		let encoded = serde_json::to_string(&ticket)?;
		let first = ticket.reasons.first();
		let mut node = NodeSpec::new(KnownTag::Prompt)
			.with_prop(PropId::Kind, Value::Str(Str::new_static("approval")))
			.with_prop(PropId::Id, Value::Str(ticket_id))
			.with_prop(PropId::Status, Value::Str(Str::new_static("pending")))
			.with_prop(custom("ticket"), Value::Str(Str::new(encoded)));
		if let Some(spec) = first {
			node = node
				.with_prop(PropId::Label, Value::Str(spec.title.clone()))
				.with_prop(PropId::Detail, Value::Str(spec.body.clone()))
				.with_prop(custom("subject"), Value::Str(spec.subject.clone()))
				.with_prop(
					custom("scope"),
					Value::Str(spec.scopes.first().cloned().unwrap_or_else(|| sf!("once"))),
				)
				.with_prop(
					custom("timeout-ms"),
					Value::Int(i64::try_from(spec.timeout_ms).unwrap_or(i64::MAX)),
				);
		}
		session.patch(Txn {
			cause: session.head().ok_or(ApprovalError::MissingPrompts)?,
			label: Some(Str::new_static("approval.open")),
			ops:   vec![Op::Ins {
				parent: prompts,
				after: session.dom().children(prompts).last().copied(),
				node,
			}],
		})?;
		Ok(ticket)
	}

	/// Applies an idempotent first decision to a prompt.
	pub fn decide(
		&self,
		session: &mut Session,
		ticket_id: &str,
		decision: ApprovalDecision,
	) -> Result<ApprovalTicket, ApprovalError> {
		let (handle, mut ticket) = find_ticket(session, ticket_id)?;
		if ticket.state == TicketState::Pending {
			ticket.state = TicketState::Decided;
			ticket.decision = Some(decision.clone());
			let encoded = serde_json::to_string(&ticket)?;
			let decision_json = serde_json::to_string(&decision)?;
			session.patch(Txn {
				cause: session.head().ok_or(ApprovalError::MissingPrompts)?,
				label: Some(Str::new_static("approval.decide")),
				ops:   vec![
					Op::Set {
						h:     handle,
						prop:  PropId::Status.into(),
						value: Value::Str(sf!("decided")),
					},
					Op::Set {
						h:     handle,
						prop:  custom("ticket"),
						value: Value::Str(Str::new(encoded)),
					},
					Op::Set {
						h:     handle,
						prop:  custom("decision"),
						value: Value::Str(Str::new(decision_json)),
					},
					Op::Set {
						h:     handle,
						prop:  custom("approved"),
						value: Value::Bool(decision.approved),
					},
					Op::Set {
						h:     handle,
						prop:  custom("scope"),
						value: Value::Str(Str::new(decision.scope.as_str())),
					},
					Op::Set {
						h:     handle,
						prop:  custom("source"),
						value: Value::Str(sf!(<&'static str>::from(decision.source))),
					},
				],
			})?;
		}
		Ok(ticket)
	}

	/// Withdraws an unanswered prompt.
	pub fn withdraw(
		&self,
		session: &mut Session,
		ticket_id: &str,
	) -> Result<ApprovalTicket, ApprovalError> {
		let (handle, mut ticket) = find_ticket(session, ticket_id)?;
		if ticket.state == TicketState::Pending {
			ticket.state = TicketState::Withdrawn;
			let encoded = serde_json::to_string(&ticket)?;
			session.patch(Txn {
				cause: session.head().ok_or(ApprovalError::MissingPrompts)?,
				label: Some(Str::new_static("approval.withdraw")),
				ops:   vec![
					Op::Set {
						h:     handle,
						prop:  PropId::Status.into(),
						value: Value::Str(sf!("withdrawn")),
					},
					Op::Set {
						h:     handle,
						prop:  custom("ticket"),
						value: Value::Str(Str::new(encoded)),
					},
				],
			})?;
		}
		Ok(ticket)
	}

	/// Rebuilds one ticket from the authoritative DOM.
	pub fn ticket(&self, session: &Session, ticket_id: &str) -> Option<ApprovalTicket> {
		tickets(session)
			.find_map(|(_, ticket)| (ticket.ticket_id.as_str() == ticket_id).then_some(ticket))
	}

	/// Rebuilds pending tickets in tree order.
	pub fn pending(&self, session: &Session) -> Vec<ApprovalTicket> {
		tickets(session)
			.filter_map(|(_, ticket)| (ticket.state == TicketState::Pending).then_some(ticket))
			.collect()
	}
}

impl Default for ApprovalBook {
	fn default() -> Self {
		Self::new()
	}
}

fn custom(name: &'static str) -> PropKey {
	PropKey::Custom(Str::new_static(name))
}

fn tickets(session: &Session) -> impl Iterator<Item = (Handle, ApprovalTicket)> + '_ {
	let prompts = prompts_handle(session.dom());
	prompts
		.into_iter()
		.flat_map(|prompts| session.dom().children(prompts).iter().copied())
		.filter_map(|handle| {
			let node = session.dom().get(handle)?;
			if node.tag != Tag::Known(KnownTag::Prompt)
				|| node
					.prop(&PropKey::from(PropId::Kind))
					.and_then(Value::as_str)
					!= Some("approval")
			{
				return None;
			}
			let encoded = node.prop(&custom("ticket")).and_then(Value::as_str)?;
			serde_json::from_str(encoded)
				.ok()
				.map(|ticket| (handle, ticket))
		})
}

fn find_ticket(
	session: &Session,
	ticket_id: &str,
) -> Result<(Handle, ApprovalTicket), ApprovalError> {
	tickets(session)
		.find(|(_, ticket)| ticket.ticket_id.as_str() == ticket_id)
		.ok_or_else(|| ApprovalError::UnknownTicket { id: Str::new(ticket_id) })
}

fn epoch_millis() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

/// Cloneable request/reply route between environment policy and the host actor.
#[derive(Clone)]
pub struct ApprovalRoute {
	inner: Arc<RouteInner>,
}

struct RouteInner {
	next_id:   AtomicU64,
	tx:        flume::Sender<ApprovalRequest>,
	pending:   Mutex<std::collections::BTreeMap<Str, PendingRequest>>,
	#[allow(dead_code, reason = "hook dispatch is wired by the extension registrar")]
	hook_gate: Option<Arc<crate::HookGate>>,
}

#[derive(Clone)]
struct PendingRequest {
	ticket: ApprovalTicket,
	reply:  flume::Sender<ApprovalDecision>,
}

/// Host-facing receiving half of an approval route.
pub struct ApprovalInbox {
	rx: flume::Receiver<ApprovalRequest>,
}

/// One pending approval delivered to the host actor.
pub struct ApprovalRequest {
	/// Prompt awaiting a decision.
	pub ticket: ApprovalTicket,
	reply:      flume::Sender<ApprovalDecision>,
}

impl ApprovalRequest {
	/// Answers the request. The first response wins.
	pub fn respond(self, decision: ApprovalDecision) -> Result<(), ApprovalDecision> {
		self
			.reply
			.try_send(decision)
			.map_err(|error| error.into_inner())
	}
}

impl ApprovalInbox {
	/// Receives the next pending request.
	pub async fn recv(&self) -> Result<ApprovalRequest, flume::RecvError> {
		self.rx.recv_async().await
	}

	/// Attempts to receive a request without waiting.
	pub fn try_recv(&self) -> Result<ApprovalRequest, flume::TryRecvError> {
		self.rx.try_recv()
	}
}

impl ApprovalRoute {
	/// Creates a route and its single host inbox.
	#[must_use]
	pub fn new(
		_book: Arc<ApprovalBook>,
		hook_gate: Option<Arc<crate::HookGate>>,
	) -> (Self, ApprovalInbox) {
		let (tx, rx) = flume::unbounded();
		(
			Self {
				inner: Arc::new(RouteInner {
					next_id: AtomicU64::new(1),
					tx,
					pending: Mutex::new(std::collections::BTreeMap::new()),
					hook_gate,
				}),
			},
			ApprovalInbox { rx },
		)
	}

	/// Files, dispatches, and awaits one approval prompt.
	pub async fn request(
		&self,
		invocation_id: Option<Str>,
		reasons: Vec<ApprovalSpec>,
		created_at_ms: u64,
	) -> ApprovalTicket {
		let ticket_id = sf!("approval-{}", self.inner.next_id.fetch_add(1, Ordering::Relaxed));
		let mut ticket = ApprovalTicket {
			ticket_id: ticket_id.clone(),
			invocation_id,
			reasons,
			state: TicketState::Pending,
			decision: None,
			created_at_ms,
		};
		let (reply, response) = flume::bounded(1);
		self
			.inner
			.pending
			.lock()
			.insert(ticket_id.clone(), PendingRequest {
				ticket: ticket.clone(),
				reply:  reply.clone(),
			});
		let timeout_ms = ticket
			.reasons
			.iter()
			.map(|reason| reason.timeout_ms)
			.filter(|value| *value != 0)
			.min();
		if self
			.inner
			.tx
			.send(ApprovalRequest { ticket: ticket.clone(), reply })
			.is_err()
		{
			let decision = unreachable_decision(&ticket, "approval host disconnected");
			ticket.state = TicketState::Decided;
			ticket.decision = Some(decision);
			self.inner.pending.lock().remove(&ticket_id);
			return ticket;
		}
		let decision = match timeout_ms {
			Some(timeout_ms) => {
				match time::timeout(Duration::from_millis(timeout_ms), response.recv_async()).await {
					Ok(Ok(decision)) => decision,
					Ok(Err(_)) => unreachable_decision(&ticket, "approval host became unreachable"),
					Err(_) => timeout_decision(&ticket),
				}
			},
			None => response
				.recv_async()
				.await
				.unwrap_or_else(|_| unreachable_decision(&ticket, "approval host became unreachable")),
		};
		self.inner.pending.lock().remove(&ticket_id);
		ticket.state = TicketState::Decided;
		ticket.decision = Some(decision);
		ticket
	}

	/// Returns a currently dispatched prompt.
	#[must_use]
	pub fn ticket(&self, ticket_id: &str) -> Option<ApprovalTicket> {
		self
			.inner
			.pending
			.lock()
			.get(ticket_id)
			.map(|pending| pending.ticket.clone())
	}

	/// Returns every currently dispatched prompt in filing order.
	#[must_use]
	pub fn pending(&self) -> Vec<ApprovalTicket> {
		self
			.inner
			.pending
			.lock()
			.values()
			.map(|request| request.ticket.clone())
			.filter(|ticket| ticket.state == TicketState::Pending)
			.collect()
	}

	/// Sends a decision to a currently dispatched prompt.
	pub fn decide(&self, ticket_id: &str, decision: ApprovalDecision) -> Option<ApprovalTicket> {
		let mut pending = self.inner.pending.lock();
		let request = pending.get_mut(ticket_id)?;
		if request.reply.try_send(decision.clone()).is_err() {
			return Some(request.ticket.clone());
		}
		request.ticket.state = TicketState::Decided;
		request.ticket.decision = Some(decision);
		Some(request.ticket.clone())
	}
}

fn timeout_decision(ticket: &ApprovalTicket) -> ApprovalDecision {
	let mut defaults = ticket.reasons.iter().map(|reason| reason.default);
	let first = defaults.next().flatten();
	let approved = first.is_some() && defaults.all(|value| value == first) && first == Some(true);
	ApprovalDecision {
		approved,
		scope: ApprovalScope::Once,
		source: ApprovalSource::Timeout,
		decided_by: None,
		reason: Some(sf!("approval request timed out")),
		audited: approved,
	}
}

fn unreachable_decision(ticket: &ApprovalTicket, reason: &'static str) -> ApprovalDecision {
	let approved = !ticket.reasons.is_empty()
		&& ticket
			.reasons
			.iter()
			.all(|spec| matches!(spec.unreachable.as_str(), "allow" | "approve" | "fail_open"));
	ApprovalDecision {
		approved,
		scope: ApprovalScope::Once,
		source: ApprovalSource::Unavailable,
		decided_by: None,
		reason: Some(Str::new_static(reason)),
		audited: approved,
	}
}
