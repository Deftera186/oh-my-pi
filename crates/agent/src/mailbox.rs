//! Single-channel interrupt mailbox with point-specific draining.

use std::collections::VecDeque;

use flume::Receiver;
use omp_core::{RemotePrincipal, Str, sf};
use omp_proto::{
	inference::v1::{self as inference, value},
	thread::v1::{self as thread, Item, item},
};

/// Durable item property identifying a deferred-diagnostics document.
pub const DEFERRED_DIAGNOSTIC_DOCUMENT_PROP: &str = "omp/deferred-diagnostic-document";
/// Durable item property fencing the document revision of deferred diagnostics.
pub const DEFERRED_DIAGNOSTIC_REVISION_PROP: &str = "omp/deferred-diagnostic-revision";
/// Durable item property fencing the language-server generation.
pub const DEFERRED_DIAGNOSTIC_GENERATION_PROP: &str = "omp/deferred-diagnostic-generation";

/// Earliest loop point at which an interrupt may be observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptClass {
	/// Between tool completions while a batch is running.
	Immediate,
	/// After a committed turn outcome and before the next submission.
	TurnBoundary,
	/// When the loop would otherwise become idle.
	Idle,
}

impl InterruptClass {
	const fn index(self) -> usize {
		match self {
			Self::Immediate => 0,
			Self::TurnBoundary => 1,
			Self::Idle => 2,
		}
	}
}

/// A loop location at which queued interrupts are drained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainPoint {
	/// The completion boundary between tools in a batch.
	Immediate,
	/// The boundary following a committed turn outcome.
	TurnBoundary,
	/// The point at which the loop would otherwise stop.
	Idle,
}

impl DrainPoint {
	const fn highest_class(self) -> usize {
		match self {
			Self::Immediate => InterruptClass::Immediate.index(),
			Self::TurnBoundary => InterruptClass::TurnBoundary.index(),
			Self::Idle => InterruptClass::Idle.index(),
		}
	}
}

/// Typed attribution for an interrupt producer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InterruptSource {
	/// Settlement notification for one detached job.
	Job {
		/// Stable detached-job identifier.
		id: Str,
	},
	/// A continuation accepted at a settled boundary.
	Continuation {
		/// Extension that won the settled-boundary decision.
		owner: Str,
	},
	/// A durable schedule firing.
	Schedule {
		/// Stable schedule identifier.
		id: Str,
	},
	/// A message sent by a peer agent or session.
	Peer {
		/// Stable sender identity.
		from: Str,
	},
	/// Authenticated mutation admitted from a writable collaboration peer.
	Remote {
		/// Immutable room, peer, and credential-tier provenance.
		principal: RemotePrincipal,
	},
	/// Revision-fenced diagnostics completed after the inline write budget.
	DeferredDiagnostics {
		/// Stable document identity supplied by document authority.
		document:          Str,
		/// Exact committed document revision.
		revision:          u64,
		/// Language-server generation that produced the diagnostics.
		server_generation: u64,
	},
	/// Named producer without a more specific structured source.
	Producer(Str),
}

/// Durable item property marking a host-admitted collaboration mutation.
pub const REMOTE_PRINCIPAL_PROP: &str = "omp/collab/remote";
/// Durable item property retaining the relay peer id for audit linkage.
pub const REMOTE_PEER_ID_PROP: &str = "omp/collab/peer-id";
/// Durable item property retaining the sanitized guest display name.
pub const REMOTE_DISPLAY_NAME_PROP: &str = "omp/collab/display-name";
/// Durable item property retaining the secret-free collaboration room identity.
pub const REMOTE_ROOM_ID_PROP: &str = "omp/collab/room-id";

/// Canonical thread input delivered asynchronously to the agent loop.
#[derive(Clone, Debug, PartialEq)]
pub struct Interrupt {
	/// Earliest point at which this input may interrupt the loop.
	pub class:  InterruptClass,
	/// Canonical thread item to append on delivery.
	pub item:   Item,
	/// Typed attribution for the producer of this input.
	pub source: InterruptSource,
}

/// Builds a Core-mailbox interrupt stamped with authenticated remote
/// provenance.
///
/// The write token and its audit digest are intentionally absent from durable
/// item properties. Tool effects produced by this input still traverse the
/// normal Environment grant and durable approval path.
pub fn remote_principal_interrupt(
	mut item: Item,
	class: InterruptClass,
	principal: RemotePrincipal,
) -> Interrupt {
	let props = item.props.get_or_insert_with(inference::ValueMap::default);
	props
		.fields
		.insert(REMOTE_PRINCIPAL_PROP.to_owned(), inference::Value {
			kind: Some(value::Kind::Bool(true)),
		});
	props
		.fields
		.insert(REMOTE_PEER_ID_PROP.to_owned(), inference::Value {
			kind: Some(value::Kind::Uint(u64::from(principal.peer_id()))),
		});
	props
		.fields
		.insert(REMOTE_DISPLAY_NAME_PROP.to_owned(), inference::Value {
			kind: Some(value::Kind::String(principal.display_name().to_owned())),
		});
	props
		.fields
		.insert(REMOTE_ROOM_ID_PROP.to_owned(), inference::Value {
			kind: Some(value::Kind::String(principal.room_id().to_owned())),
		});
	Interrupt { class, item, source: InterruptSource::Remote { principal } }
}

/// Builds the deferred catalog-change interrupt used for device availability.
///
/// Availability is deliberately visible only at the turn boundary: changing a
/// device catalog cannot preempt an in-flight tool batch.
pub fn device_availability_interrupt(item: Item) -> Interrupt {
	Interrupt {
		class: InterruptClass::TurnBoundary,
		item,
		source: InterruptSource::Producer(sf!("device availability")),
	}
}

/// Builds a durable, revision-fenced deferred-diagnostics delivery.
///
/// Attribution is copied into item properties before enqueue, so journal replay
/// preserves the source and fences even though the live mailbox source is not
/// itself persisted. Delivery is a system item restricted to a turn boundary
/// and therefore cannot split or reorder an already emitted tool batch.
pub fn deferred_diagnostics_interrupt(
	text: Str,
	document: Str,
	revision: u64,
	server_generation: u64,
) -> Interrupt {
	let mut props = inference::ValueMap::default();
	props
		.fields
		.insert(DEFERRED_DIAGNOSTIC_DOCUMENT_PROP.to_owned(), inference::Value {
			kind: Some(value::Kind::String(document.to_string())),
		});
	props
		.fields
		.insert(DEFERRED_DIAGNOSTIC_REVISION_PROP.to_owned(), inference::Value {
			kind: Some(value::Kind::Uint(revision)),
		});
	props
		.fields
		.insert(DEFERRED_DIAGNOSTIC_GENERATION_PROP.to_owned(), inference::Value {
			kind: Some(value::Kind::Uint(server_generation)),
		});
	let item = Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(item::Kind::Message(thread::Message {
			role:  thread::Role::System as i32,
			parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text.to_string())) }],
		})),
		props:         Some(props),
	};
	Interrupt {
		class: InterruptClass::TurnBoundary,
		item,
		source: InterruptSource::DeferredDiagnostics { document, revision, server_generation },
	}
}

/// Deferred local effect selected by the interactive composer.
#[derive(
	Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
pub enum DeferredCommandKind {
	/// Environment-mediated shell command.
	Shell,
	/// Environment-mediated evaluator cell.
	Eval,
}

/// Whether an evaluator cell may receive the active conversation context.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum DeferredContext {
	/// Exclude conversation context (`$$`).
	Excluded,
	/// Include the active context (`$`).
	Included,
}

/// One ordered shell/eval item queued while a turn is active.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DeferredCommand {
	/// Stable producer identity used by durable settlement.
	pub id:      Str,
	/// Monotonic queue order assigned at enqueue.
	pub order:   u64,
	/// Effect family.
	pub kind:    DeferredCommandKind,
	/// Source text without the local execution prefix.
	pub source:  Str,
	/// Evaluator context policy; shell items always use `Excluded`.
	pub context: DeferredContext,
}

/// Terminal classification projected durably after a deferred command starts.
#[derive(
	Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
pub enum DeferredSettlementStatus {
	/// The Environment operation completed successfully.
	Succeeded,
	/// The Environment operation returned a typed failure.
	Failed,
	/// The user cancelled after execution started.
	Cancelled,
}

/// Durable, non-context settlement projection for one started local command.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DeferredSettlement {
	/// Stable command identity.
	pub id:             Str,
	/// Original queue order.
	pub order:          u64,
	/// Effect family.
	pub kind:           DeferredCommandKind,
	/// Terminal status.
	pub status:         DeferredSettlementStatus,
	/// Bounded user-visible output preview.
	pub output_preview: Str,
	/// Environment artifact URI containing full output, when spilled.
	pub artifact:       Option<Str>,
	/// Journal-clock start timestamp.
	pub started_at_ms:  u64,
	/// Journal-clock settlement timestamp.
	pub settled_at_ms:  u64,
}

/// Ordered, UI-observable deferred-command queue.
///
/// Taking an item does not mark it started: callers may restore it at the front
/// if cancellation wins before Environment admission. Once admission begins,
/// only a [`DeferredSettlement`] may return it to transcript projection.
#[derive(Default)]
pub struct DeferredCommands {
	next_order: u64,
	pending:    VecDeque<DeferredCommand>,
}

impl DeferredCommands {
	/// Creates an empty queue.
	pub const fn new() -> Self {
		Self { next_order: 0, pending: VecDeque::new() }
	}

	/// Enqueues one command at the FIFO tail and returns its assigned order.
	pub fn enqueue(
		&mut self,
		id: Str,
		kind: DeferredCommandKind,
		source: Str,
		context: DeferredContext,
	) -> u64 {
		let order = self.next_order;
		self.next_order = self.next_order.saturating_add(1);
		self.pending.push_back(DeferredCommand {
			id,
			order,
			kind,
			source,
			context: if kind == DeferredCommandKind::Shell {
				DeferredContext::Excluded
			} else {
				context
			},
		});
		order
	}

	/// Takes the oldest command for a start attempt.
	pub fn take_next(&mut self) -> Option<DeferredCommand> {
		self.pending.pop_front()
	}

	/// Restores a command whose start attempt lost to cancellation.
	pub fn restore_before_start(&mut self, command: DeferredCommand) {
		self.pending.push_front(command);
	}

	/// Dequeues the newest unstarted item back into the composer.
	pub fn take_newest_unstarted(&mut self) -> Option<DeferredCommand> {
		self.pending.pop_back()
	}

	/// Returns pending commands in exact execution order.
	pub fn pending(&self) -> impl ExactSizeIterator<Item = &DeferredCommand> {
		self.pending.iter()
	}

	/// Returns the unstarted command count.
	pub fn len(&self) -> usize {
		self.pending.len()
	}

	/// Reports whether no commands await admission.
	pub fn is_empty(&self) -> bool {
		self.pending.is_empty()
	}
}

/// Cloneable nonblocking producer for the agent's sole command mailbox.
#[derive(Clone, Debug)]
pub struct MailboxSender {
	tx:       flume::Sender<Interrupt>,
	commands: flume::Sender<MailboxCommand>,
}

impl MailboxSender {
	/// Enqueues an interrupt without blocking the producer.
	///
	/// The mailbox is unbounded, so this fails only after its receiver closes.
	pub fn try_enqueue(
		&self,
		interrupt: Interrupt,
	) -> Result<(), Box<flume::TrySendError<Interrupt>>> {
		self.tx.try_send(interrupt).map_err(Box::new)
	}

	/// Returns whether the receiving mailbox has closed.
	pub fn is_disconnected(&self) -> bool {
		self.tx.is_disconnected()
	}

	/// Removes every producer-authored interrupt that has not reached a drain
	/// point yet, returning the number removed.
	pub async fn take_unstarted_producers(&self) -> Option<usize> {
		let (reply, removed) = flume::bounded(1);
		self
			.commands
			.send_async(MailboxCommand::TakeProducer { reply })
			.await
			.ok()?;
		removed.recv_async().await.ok()
	}
}

#[derive(Debug)]
enum MailboxCommand {
	TakeProducer { reply: flume::Sender<usize> },
}

/// Single-consumer interrupt mailbox with an ordered backlog.
///
/// Shutdown is deliberately absent: the owner races [`Self::wait`] against a
/// `tokio::watch` receiver, so selecting shutdown never consumes an interrupt.
pub struct Mailbox {
	tx:          flume::Sender<Interrupt>,
	rx:          Receiver<Interrupt>,
	commands_tx: flume::Sender<MailboxCommand>,
	commands_rx: Receiver<MailboxCommand>,
	backlog:     VecDeque<Interrupt>,
}
impl Default for Mailbox {
	fn default() -> Self {
		Self::new()
	}
}

impl Mailbox {
	/// Creates an empty unbounded mailbox.
	pub fn new() -> Self {
		let (tx, rx) = flume::unbounded();
		let (commands_tx, commands_rx) = flume::unbounded();
		Self { tx, rx, commands_tx: commands_tx.clone(), commands_rx, backlog: VecDeque::new() }
	}

	/// Returns a cloneable producer for this mailbox.
	pub fn sender(&self) -> MailboxSender {
		MailboxSender { tx: self.tx.clone(), commands: self.commands_tx.clone() }
	}

	/// Returns the number of interrupts waiting for a future drain point.
	///
	/// Pending channel items are first folded into the receiver-owned backlog so
	/// deduplication uses the same rules as an ordinary drain.
	pub fn pending_len(&mut self) -> usize {
		self.service_commands();
		self.pump(false);
		self.backlog.len()
	}

	/// Waits until one interrupt is retained in the local backlog.
	///
	/// Cancelling this future leaves the channel unchanged. Once it completes,
	/// the received value remains owned by the mailbox until a matching drain.
	pub async fn wait(&mut self) -> Result<(), flume::RecvError> {
		loop {
			tokio::select! {
				interrupt = self.rx.recv_async() => {
					if self.push_back(interrupt?) {
						return Ok(());
					}
				},
				command = self.commands_rx.recv_async() => {
					let command = command?;
					self.handle_command(command);
				},
			}
		}
	}

	/// Drains every interrupt eligible at `point` in class-precedence order.
	///
	/// FIFO is preserved within each class. When `defer_interrupts` is set,
	/// queued immediate interrupts are permanently demoted to the turn boundary
	/// before eligibility is evaluated, so an immediate-point drain retains
	/// them.
	pub fn drain(&mut self, point: DrainPoint, defer_interrupts: bool) -> Vec<Interrupt> {
		self.drain_steering(point, defer_interrupts, usize::MAX)
	}

	/// Drains eligible interrupts while admitting at most `steering_limit`
	/// queued user/peer steering messages.
	///
	/// Non-steering continuations and durable settlements are never throttled by
	/// presentation queue mode.
	pub fn drain_steering(
		&mut self,
		point: DrainPoint,
		defer_interrupts: bool,
		steering_limit: usize,
	) -> Vec<Interrupt> {
		self.service_commands();
		self.pump(defer_interrupts);
		if defer_interrupts {
			self.demote_immediate();
		}

		let mut drained = Vec::new();
		let mut steering = 0usize;
		for class in 0..=point.highest_class() {
			let queued = self.backlog.len();
			for _ in 0..queued {
				let Some(interrupt) = self.backlog.pop_front() else {
					break;
				};
				if interrupt.class.index() == class
					&& (!is_steering(&interrupt) || steering < steering_limit)
				{
					if is_steering(&interrupt) {
						steering = steering.saturating_add(1);
					}
					drained.push(interrupt);
				} else {
					self.backlog.push_back(interrupt);
				}
			}
		}
		drained
	}

	/// Restores previously drained interrupts ahead of newer inputs.
	///
	/// This is the rollback operation for a drain whose surrounding loop action
	/// aborts before the items are staged into a thread delta.
	pub fn requeue_front(&mut self, interrupts: Vec<Interrupt>) {
		for interrupt in interrupts.into_iter().rev() {
			self.backlog.push_front(interrupt);
		}
	}

	/// Discards queued producer steering while preserving detached-job
	/// settlements, whose durable facts remain valid across a history rewind.
	pub(crate) fn discard_producer_interrupts(&mut self) {
		self.pump(false);
		self
			.backlog
			.retain(|interrupt| matches!(interrupt.source, InterruptSource::Job { .. }));
	}

	/// Returns the number of interrupts retained locally and in the channel.
	pub fn len(&self) -> usize {
		self.backlog.len() + self.rx.len()
	}

	/// Returns whether no interrupts are currently queued.
	pub fn is_empty(&self) -> bool {
		self.backlog.is_empty() && self.rx.is_empty()
	}

	fn pump(&mut self, defer_interrupts: bool) {
		while let Ok(mut interrupt) = self.rx.try_recv() {
			if defer_interrupts && interrupt.class == InterruptClass::Immediate {
				interrupt.class = InterruptClass::TurnBoundary;
			}
			self.push_back(interrupt);
		}
	}

	fn service_commands(&mut self) {
		while let Ok(command) = self.commands_rx.try_recv() {
			self.handle_command(command);
		}
	}

	fn handle_command(&mut self, command: MailboxCommand) {
		match command {
			MailboxCommand::TakeProducer { reply } => {
				self.pump(false);
				let before = self.backlog.len();
				self
					.backlog
					.retain(|interrupt| !matches!(interrupt.source, InterruptSource::Producer(_)));
				let _ = reply.send(before.saturating_sub(self.backlog.len()));
			},
		}
	}

	fn push_back(&mut self, interrupt: Interrupt) -> bool {
		let InterruptSource::DeferredDiagnostics { document, revision, server_generation } =
			&interrupt.source
		else {
			self.backlog.push_back(interrupt);
			return true;
		};

		let stale = self.backlog.iter().any(|queued| {
			let InterruptSource::DeferredDiagnostics {
				document: queued_document,
				revision: queued_revision,
				server_generation: queued_generation,
			} = &queued.source
			else {
				return false;
			};
			queued_document == document
				&& (*queued_generation > *server_generation
					|| (*queued_generation == *server_generation && *queued_revision >= *revision))
		});
		if stale {
			return false;
		}
		self.backlog.retain(|queued| {
			let InterruptSource::DeferredDiagnostics {
				document: queued_document,
				revision: queued_revision,
				server_generation: queued_generation,
			} = &queued.source
			else {
				return true;
			};
			queued_document != document
				|| *queued_generation > *server_generation
				|| (*queued_generation == *server_generation && *queued_revision > *revision)
		});
		self.backlog.push_back(interrupt);
		true
	}

	fn demote_immediate(&mut self) {
		for interrupt in &mut self.backlog {
			if interrupt.class == InterruptClass::Immediate {
				interrupt.class = InterruptClass::TurnBoundary;
			}
		}
	}
}
fn is_steering(interrupt: &Interrupt) -> bool {
	let user_message = matches!(
		interrupt.item.kind.as_ref(),
		Some(item::Kind::Message(message)) if message.role == thread::Role::User as i32
	);
	user_message
		&& matches!(
			&interrupt.source,
			InterruptSource::Producer(_)
				| InterruptSource::Peer { .. }
				| InterruptSource::Remote { .. }
		)
}

#[cfg(test)]
mod tests {
	use omp_proto::thread::v1::{Item, Message, Part, Role, part};

	use super::*;

	fn item() -> Item {
		Item {
			seq:           0,
			created_at_ms: 0,
			kind:          Some(item::Kind::Message(Message {
				role:  Role::User as i32,
				parts: vec![Part { kind: Some(part::Kind::Text("continue".to_owned())) }],
			})),
			props:         None,
		}
	}

	#[test]
	fn one_at_a_time_throttles_only_steering_messages() {
		let mut mailbox = Mailbox::new();
		let sender = mailbox.sender();
		for source in [
			InterruptSource::Producer(sf!("first")),
			InterruptSource::Continuation { owner: sf!("system") },
			InterruptSource::Producer(sf!("second")),
		] {
			sender
				.try_enqueue(Interrupt { class: InterruptClass::Immediate, item: item(), source })
				.expect("enqueue interrupt");
		}
		let first = mailbox.drain_steering(DrainPoint::Immediate, false, 1);
		assert_eq!(first.len(), 2, "continuations are not throttled");
		assert!(first.iter().any(|interrupt| {
			matches!(&interrupt.source, InterruptSource::Producer(name) if name.as_str() == "first")
		}));
		let second = mailbox.drain_steering(DrainPoint::Immediate, false, 1);
		assert_eq!(second.len(), 1);
		assert!(matches!(
			&second[0].source,
			InterruptSource::Producer(name) if name.as_str() == "second"
		));
	}

	#[test]
	fn deferred_commands_restore_before_start_and_dequeue_newest() {
		let mut commands = DeferredCommands::new();
		commands.enqueue(
			sf!("shell"),
			DeferredCommandKind::Shell,
			sf!("git status"),
			DeferredContext::Included,
		);
		commands.enqueue(
			sf!("eval"),
			DeferredCommandKind::Eval,
			sf!("display(1)"),
			DeferredContext::Included,
		);
		let first = commands.take_next().expect("oldest command");
		assert_eq!(first.id, "shell");
		assert_eq!(first.context, DeferredContext::Excluded);
		commands.restore_before_start(first);
		assert_eq!(commands.pending().next().map(|item| item.id.as_str()), Some("shell"));
		let newest = commands.take_newest_unstarted().expect("newest command");
		assert_eq!(newest.id, "eval");
		assert_eq!(commands.len(), 1);
	}

	#[test]
	fn deferred_continuation_waits_for_the_turn_boundary() {
		let mut mailbox = Mailbox::new();
		mailbox
			.sender()
			.try_enqueue(Interrupt {
				class:  InterruptClass::Immediate,
				item:   item(),
				source: InterruptSource::Continuation { owner: sf!("goal") },
			})
			.unwrap();
		assert!(mailbox.drain(DrainPoint::Immediate, true).is_empty());
		assert_eq!(mailbox.drain(DrainPoint::TurnBoundary, true).len(), 1);
	}

	#[test]
	fn pending_len_counts_receiver_and_backlog_without_consuming() {
		let mut mailbox = Mailbox::new();
		let sender = mailbox.sender();
		for name in ["first", "second"] {
			sender
				.try_enqueue(Interrupt {
					class:  InterruptClass::TurnBoundary,
					item:   item(),
					source: InterruptSource::Producer(Str::new(name)),
				})
				.expect("enqueue");
		}
		assert_eq!(mailbox.pending_len(), 2);
		assert_eq!(mailbox.pending_len(), 2);
		assert_eq!(mailbox.drain(DrainPoint::TurnBoundary, false).len(), 2);
		assert_eq!(mailbox.pending_len(), 0);
	}
}
