//! Core-owned durable approval ticket state.

use std::{
	collections::BTreeMap,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use flume::Receiver;
use omp_core::{Str, sf};
use parking_lot::Mutex;
use tokio::{sync::oneshot, time};

/// One requirement merged into an invocation's single approval ticket.
#[derive(Clone, Debug, Eq, PartialEq)]
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
	/// Optional timeout default; Core never invents one.
	pub default:       Option<bool>,
	/// Requested approver route.
	pub route:         Str,
	/// Optional named external approver.
	pub approver:      Option<Str>,
	/// Maximum wait in milliseconds.
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

/// Durable state of an approval ticket.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum TicketState {
	/// Awaiting a single idempotent answer.
	Pending,
	/// Answered exactly once.
	Decided,
	/// Invocation ended before an answer.
	Withdrawn,
}

/// The source that supplied an approval result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum ApprovalSource {
	/// A local user answered.
	User,
	/// An authenticated external approver answered.
	External,
	/// A parent agent answered.
	Forwarded,
	/// The frozen turn configuration pre-answered the ticket.
	Config,
	/// An authorized policy extension answered.
	Extension,
	/// An explicit timeout default answered.
	Timeout,
	/// An unreachable-route policy answered.
	Unavailable,
}

/// One idempotent durable approval result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalDecision {
	/// Whether all merged reasons are approved.
	pub approved:   bool,
	/// Granted policy scope.
	pub scope:      Str,
	/// Source of the answer.
	pub source:     ApprovalSource,
	/// Optional authenticated decider.
	pub decided_by: Option<Str>,
	/// Optional user-visible rationale.
	pub reason:     Option<Str>,
	/// Whether a fail-open result was durably audited.
	pub audited:    bool,
}

/// Core-owned ticket, independent of an extension coroutine lifetime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalTicket {
	/// Stable idempotency key for approvers.
	pub ticket_id:     Str,
	/// Invocation this ticket blocks, if any.
	pub invocation_id: Option<Str>,
	/// Every unresolved hook requirement in filing order.
	pub reasons:       Vec<ApprovalSpec>,
	/// Current durable state.
	pub state:         TicketState,
	/// Set only once `state` becomes `Decided`.
	pub decision:      Option<ApprovalDecision>,
	/// Journal-clock epoch milliseconds at filing.
	pub created_at_ms: u64,
}

impl ApprovalTicket {
	/// Converts this ticket to the typed transcript payload filed on creation or
	/// merge.
	pub fn filed_record(&self) -> omp_storage::transcript::ApprovalTicketFiled {
		omp_storage::transcript::ApprovalTicketFiled {
			ticket_id:     self.ticket_id.clone(),
			invocation_id: self.invocation_id.clone(),
			reasons:       self
				.reasons
				.iter()
				.map(|reason| omp_storage::transcript::ApprovalReason {
					title:         reason.title.clone(),
					body:          reason.body.clone(),
					subject:       reason.subject.clone(),
					kind:          reason.kind.clone(),
					scopes:        reason.scopes.clone(),
					default:       reason.default,
					route:         reason.route.clone(),
					approver:      reason.approver.clone(),
					timeout_ms:    reason.timeout_ms,
					unreachable:   reason.unreachable.clone(),
					require_human: reason.require_human,
					pattern:       reason.pattern.clone(),
					evidence:      reason.evidence.clone(),
				})
				.collect(),
			created_at_ms: self.created_at_ms,
		}
	}

	/// Converts a terminal decision or withdrawal to its typed transcript
	/// payload.
	pub fn decision_record(&self) -> Option<omp_storage::transcript::ApprovalDecided> {
		let state = match self.state {
			TicketState::Pending => return None,
			state => sf!(<&'static str>::from(state)),
		};
		let decision = self.decision.as_ref();
		Some(omp_storage::transcript::ApprovalDecided {
			ticket_id: self.ticket_id.clone(),
			state,
			approved: decision.map(|value| value.approved),
			scope: decision.map(|value| value.scope.clone()),
			source: decision.map(|value| sf!(<&'static str>::from(value.source))),
			decided_by: decision.and_then(|value| value.decided_by.clone()),
			reason: decision.and_then(|value| value.reason.clone()),
			audited: decision.is_some_and(|value| value.audited),
		})
	}
}

fn approval_source_from_name(source: &str) -> Option<ApprovalSource> {
	Some(match source {
		"user" => ApprovalSource::User,
		"external" => ApprovalSource::External,
		"forwarded" => ApprovalSource::Forwarded,
		"config" => ApprovalSource::Config,
		"extension" => ApprovalSource::Extension,
		"timeout" => ApprovalSource::Timeout,
		"unavailable" => ApprovalSource::Unavailable,
		_ => return None,
	})
}

/// In-memory index reconstructed from `ApprovalTicketFiled` and
/// `ApprovalDecided` journal entries.
pub struct ApprovalBook {
	next_id:       AtomicU64,
	ticket_prefix: Str,
	tickets:       Mutex<BTreeMap<Str, ApprovalTicket>>,
	by_invocation: Mutex<BTreeMap<Str, Str>>,
}

/// Awaitable host dispatch for durable approval tickets.
///
/// Each request owns a one-shot response. Dropping the host inbox, dropping a
/// received request without answering, or timing out resolves through the
/// ticket's declared policy instead of leaving the invocation suspended.
#[derive(Clone)]
pub struct ApprovalRoute {
	book: Arc<ApprovalBook>,
	tx:   flume::Sender<ApprovalRequest>,
}

/// Host-facing receiving half of an [`ApprovalRoute`].
pub struct ApprovalInbox {
	rx: Receiver<ApprovalRequest>,
}

/// One pending approval delivered to a host.
pub struct ApprovalRequest {
	/// Durable ticket awaiting a decision.
	pub ticket: ApprovalTicket,
	reply:      oneshot::Sender<ApprovalDecision>,
}

impl ApprovalRequest {
	/// Answers this request. The route's durable first-decision rule remains
	/// authoritative if a timeout or another host has already settled it.
	pub fn respond(self, decision: ApprovalDecision) -> Result<(), ApprovalDecision> {
		self.reply.send(decision)
	}
}

impl ApprovalInbox {
	/// Receives the next pending approval request.
	pub async fn recv(&self) -> Result<ApprovalRequest, flume::RecvError> {
		self.rx.recv_async().await
	}

	/// Attempts to receive a pending request without waiting.
	pub fn try_recv(&self) -> Result<ApprovalRequest, flume::TryRecvError> {
		self.rx.try_recv()
	}
}

impl ApprovalRoute {
	/// Creates a route and its single host inbox.
	pub fn new(book: Arc<ApprovalBook>) -> (Self, ApprovalInbox) {
		let (tx, rx) = flume::unbounded();
		(Self { book, tx }, ApprovalInbox { rx })
	}

	/// Files, dispatches, and awaits one durable approval ticket.
	///
	/// Cancellation withdraws the pending ticket. An unreachable host denies
	/// by default and only approves when every merged requirement explicitly
	/// declares a fail-open unreachable policy. Timeout defaults are honored
	/// only when every requirement supplies the same default.
	pub async fn request(
		&self,
		invocation_id: Option<Str>,
		reasons: Vec<ApprovalSpec>,
		created_at_ms: u64,
	) -> ApprovalTicket {
		let ticket = self.book.file(invocation_id, reasons, created_at_ms);
		if ticket.state != TicketState::Pending {
			return ticket;
		}
		let _guard = self
			.book
			.guard(ticket.ticket_id.as_str())
			.expect("newly filed approval ticket exists");
		let (reply, response) = oneshot::channel();
		let timeout_ms = ticket
			.reasons
			.iter()
			.map(|reason| reason.timeout_ms)
			.filter(|timeout| *timeout != 0)
			.min();
		if self
			.tx
			.send(ApprovalRequest { ticket: ticket.clone(), reply })
			.is_err()
		{
			return self.resolve_unreachable(&ticket, "approval host disconnected");
		}
		let decision = match timeout_ms {
			Some(timeout_ms) => {
				match time::timeout(Duration::from_millis(timeout_ms), response).await {
					Ok(Ok(decision)) => decision,
					Ok(Err(_)) => {
						return self.resolve_unreachable(&ticket, "approval host became unreachable");
					},
					Err(_) => timeout_decision(&ticket),
				}
			},
			None => match response.await {
				Ok(decision) => decision,
				Err(_) => {
					return self.resolve_unreachable(&ticket, "approval host became unreachable");
				},
			},
		};
		self
			.book
			.decide(ticket.ticket_id.as_str(), decision)
			.expect("dispatched approval ticket exists")
	}

	fn resolve_unreachable(&self, ticket: &ApprovalTicket, reason: &'static str) -> ApprovalTicket {
		let approved = !ticket.reasons.is_empty()
			&& ticket
				.reasons
				.iter()
				.all(|spec| matches!(spec.unreachable.as_str(), "allow" | "approve" | "fail_open"));
		let decision = ApprovalDecision {
			approved,
			scope: sf!("once"),
			source: ApprovalSource::Unavailable,
			decided_by: None,
			reason: Some(sf!(reason)),
			audited: approved,
		};
		self
			.book
			.decide(ticket.ticket_id.as_str(), decision)
			.expect("dispatched approval ticket exists")
	}
}

fn timeout_decision(ticket: &ApprovalTicket) -> ApprovalDecision {
	let mut defaults = ticket.reasons.iter().map(|reason| reason.default);
	let first = defaults.next().flatten();
	let approved = first.is_some() && defaults.all(|value| value == first) && first == Some(true);
	ApprovalDecision {
		approved,
		scope: sf!("once"),
		source: ApprovalSource::Timeout,
		decided_by: None,
		reason: Some(sf!("approval request timed out")),
		audited: approved,
	}
}
/// Invocation-owned guard that withdraws an unanswered ticket on drop.
#[must_use]
pub struct ApprovalGuard<'a> {
	book:      &'a ApprovalBook,
	ticket_id: Str,
}

impl Drop for ApprovalGuard<'_> {
	fn drop(&mut self) {
		let _ = self.book.withdraw(self.ticket_id.as_str());
	}
}

impl ApprovalBook {
	/// Creates an empty Core ticket index.
	pub const fn new() -> Self {
		Self {
			next_id:       AtomicU64::new(1),
			ticket_prefix: Str::new_static("approval"),
			tickets:       Mutex::new(BTreeMap::new()),
			by_invocation: Mutex::new(BTreeMap::new()),
		}
	}

	/// Creates an empty Core ticket index with a disjoint durable id namespace.
	///
	/// This is reserved for Core-owned approval families that share a session
	/// journal with ordinary invocation tickets.
	pub fn with_prefix(prefix: impl Into<Str>) -> Self {
		Self {
			next_id:       AtomicU64::new(1),
			ticket_prefix: prefix.into(),
			tickets:       Mutex::new(BTreeMap::new()),
			by_invocation: Mutex::new(BTreeMap::new()),
		}
	}

	/// Files or merges requirements into the one ticket for an invocation.
	pub fn file(
		&self,
		invocation_id: Option<Str>,
		reasons: Vec<ApprovalSpec>,
		created_at_ms: u64,
	) -> ApprovalTicket {
		if let Some(invocation_id) = &invocation_id
			&& let Some(ticket_id) = self.by_invocation.lock().get(invocation_id).cloned()
		{
			let mut tickets = self.tickets.lock();
			let ticket = tickets
				.get_mut(&ticket_id)
				.expect("invocation ticket index stays coherent");
			if ticket.state == TicketState::Pending {
				ticket.reasons.extend(reasons);
			}
			return ticket.clone();
		}
		let ticket_id =
			sf!("{}-{}", self.ticket_prefix, self.next_id.fetch_add(1, Ordering::Relaxed));
		let ticket = ApprovalTicket {
			ticket_id: ticket_id.clone(),
			invocation_id: invocation_id.clone(),
			reasons,
			state: TicketState::Pending,
			decision: None,
			created_at_ms,
		};
		if let Some(invocation_id) = invocation_id {
			self
				.by_invocation
				.lock()
				.insert(invocation_id, ticket_id.clone());
		}
		self.tickets.lock().insert(ticket_id, ticket.clone());
		ticket
	}

	/// Applies an idempotent answer. The first answer wins permanently.
	pub fn decide(&self, ticket_id: &str, decision: ApprovalDecision) -> Option<ApprovalTicket> {
		let mut tickets = self.tickets.lock();
		let ticket = tickets.get_mut(ticket_id)?;
		if ticket.state == TicketState::Pending {
			ticket.state = TicketState::Decided;
			ticket.decision = Some(decision);
		}
		Some(ticket.clone())
	}

	/// Marks an unanswered ticket withdrawn when its invocation guard drops.
	pub fn withdraw(&self, ticket_id: &str) -> Option<ApprovalTicket> {
		let mut tickets = self.tickets.lock();
		let ticket = tickets.get_mut(ticket_id)?;
		if ticket.state == TicketState::Pending {
			ticket.state = TicketState::Withdrawn;
		}
		Some(ticket.clone())
	}

	/// Returns pending tickets in filing order.
	/// Returns one ticket by its authenticated durable identity.
	pub fn ticket(&self, ticket_id: &str) -> Option<ApprovalTicket> {
		self.tickets.lock().get(ticket_id).cloned()
	}

	/// Returns pending tickets in filing order.
	pub fn pending(&self) -> Vec<ApprovalTicket> {
		self
			.tickets
			.lock()
			.values()
			.filter(|ticket| ticket.state == TicketState::Pending)
			.cloned()
			.collect()
	}

	/// Restores a filed ticket from its typed durable record during session
	/// replay.
	pub fn restore_filed(&self, filed: omp_storage::transcript::ApprovalTicketFiled) {
		let ticket = ApprovalTicket {
			ticket_id:     filed.ticket_id.clone(),
			invocation_id: filed.invocation_id.clone(),
			reasons:       filed
				.reasons
				.into_iter()
				.map(|reason| ApprovalSpec {
					title:         reason.title,
					body:          reason.body,
					subject:       reason.subject,
					kind:          reason.kind,
					scopes:        reason.scopes,
					default:       reason.default,
					route:         reason.route,
					approver:      reason.approver,
					timeout_ms:    reason.timeout_ms,
					unreachable:   reason.unreachable,
					require_human: reason.require_human,
					pattern:       reason.pattern,
					evidence:      reason.evidence,
				})
				.collect(),
			state:         TicketState::Pending,
			decision:      None,
			created_at_ms: filed.created_at_ms,
		};
		if let Some(invocation_id) = ticket.invocation_id.clone() {
			self
				.by_invocation
				.lock()
				.insert(invocation_id, ticket.ticket_id.clone());
		}
		if let Some(sequence) = ticket
			.ticket_id
			.as_str()
			.strip_prefix(self.ticket_prefix.as_str())
			.and_then(|value| value.strip_prefix('-'))
			.and_then(|value| value.parse::<u64>().ok())
		{
			self
				.next_id
				.fetch_max(sequence.saturating_add(1), Ordering::Relaxed);
		}
		self.tickets.lock().insert(ticket.ticket_id.clone(), ticket);
	}

	/// Restores a terminal ticket decision or withdrawal during session replay.
	pub fn restore_decision(&self, decided: omp_storage::transcript::ApprovalDecided) {
		let mut tickets = self.tickets.lock();
		let Some(ticket) = tickets.get_mut(decided.ticket_id.as_str()) else {
			return;
		};
		if decided.state.as_str() == "withdrawn" {
			ticket.state = TicketState::Withdrawn;
			return;
		}
		let Some((approved, scope, source)) = decided
			.approved
			.zip(decided.scope)
			.zip(decided.source)
			.and_then(|((approved, scope), source)| {
				approval_source_from_name(source.as_str()).map(|source| (approved, scope, source))
			})
		else {
			return;
		};
		ticket.state = TicketState::Decided;
		ticket.decision = Some(ApprovalDecision {
			approved,
			scope,
			source,
			decided_by: decided.decided_by,
			reason: decided.reason,
			audited: decided.audited,
		});
	}

	/// Returns a guard which withdraws this ticket unless it is decided first.
	pub fn guard(&self, ticket_id: &str) -> Option<ApprovalGuard<'_>> {
		self
			.tickets
			.lock()
			.contains_key(ticket_id)
			.then(|| ApprovalGuard { book: self, ticket_id: Str::new(ticket_id) })
	}
}

impl Default for ApprovalBook {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use super::{
		ApprovalBook, ApprovalDecision, ApprovalRoute, ApprovalSource, ApprovalSpec, TicketState,
	};
	fn spec() -> ApprovalSpec {
		ApprovalSpec {
			title:         sf!("Run"),
			body:          sf!("run"),
			subject:       sf!("cmd"),
			kind:          sf!("exec"),
			scopes:        vec![sf!("once")],
			default:       None,
			route:         sf!("local"),
			approver:      None,
			timeout_ms:    1,
			unreachable:   sf!("fail_closed"),
			require_human: false,
			pattern:       None,
			evidence:      Vec::new(),
		}
	}
	#[test]
	fn scoped_ticket_prefixes_do_not_collide_with_invocation_tickets() {
		let ordinary = ApprovalBook::new().file(None, vec![spec()], 1);
		let extension = ApprovalBook::with_prefix("extension-approval").file(None, vec![spec()], 1);
		assert_eq!(ordinary.ticket_id.as_str(), "approval-1");
		assert_eq!(extension.ticket_id.as_str(), "extension-approval-1");
		let restored = ApprovalBook::with_prefix("extension-approval");
		restored.restore_filed(extension.filed_record());
		assert_eq!(restored.file(None, vec![spec()], 2).ticket_id.as_str(), "extension-approval-2");
	}

	#[test]
	fn tickets_merge_answer_idempotently_and_withdraw() {
		let book = ApprovalBook::new();
		let ticket = book.file(Some(sf!("i")), vec![spec()], 1);
		assert_eq!(book.file(Some(sf!("i")), vec![spec()], 2).reasons.len(), 2);
		let decision = ApprovalDecision {
			approved:   true,
			scope:      sf!("once"),
			source:     ApprovalSource::User,
			decided_by: None,
			reason:     None,
			audited:    false,
		};
		assert_eq!(
			book
				.decide(ticket.ticket_id.as_str(), decision.clone())
				.unwrap()
				.decision,
			Some(decision)
		);
		assert_eq!(book.withdraw(ticket.ticket_id.as_str()).unwrap().state, TicketState::Decided);
		let withdrawn = book.file(Some(sf!("j")), vec![spec()], 3);
		assert_eq!(
			book.withdraw(withdrawn.ticket_id.as_str()).unwrap().state,
			TicketState::Withdrawn
		);
	}
	#[test]
	fn guard_withdraws_unanswered_ticket() {
		let book = ApprovalBook::new();
		let ticket = book.file(Some(sf!("guarded")), vec![spec()], 1);
		{
			let _guard = book.guard(ticket.ticket_id.as_str()).unwrap();
		}
		assert!(book.pending().is_empty());
	}

	#[tokio::test]
	async fn route_suspends_then_resumes_with_first_decision() {
		let book = Arc::new(ApprovalBook::new());
		let (route, inbox) = ApprovalRoute::new(Arc::clone(&book));
		let request = route.request(Some(sf!("routed")), vec![spec()], 1);
		let answer = async {
			let pending = inbox.recv().await.unwrap();
			pending
				.respond(ApprovalDecision {
					approved:   true,
					scope:      sf!("once"),
					source:     ApprovalSource::User,
					decided_by: None,
					reason:     None,
					audited:    false,
				})
				.unwrap();
		};
		let (ticket, ()) = tokio::join!(request, answer);
		assert_eq!(ticket.state, TicketState::Decided);
		assert!(ticket.decision.unwrap().approved);
		assert!(book.pending().is_empty());
	}

	#[tokio::test]
	async fn route_fails_closed_when_host_is_lost() {
		let book = Arc::new(ApprovalBook::new());
		let (route, inbox) = ApprovalRoute::new(book);
		drop(inbox);
		let ticket = route.request(Some(sf!("lost")), vec![spec()], 1).await;
		let decision = ticket.decision.unwrap();
		assert!(!decision.approved);
		assert_eq!(decision.source, ApprovalSource::Unavailable);
		assert!(decision.reason.unwrap().contains("disconnected"));
	}
}
