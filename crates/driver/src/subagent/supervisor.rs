//! Durable session supervisor for owned subagent loops.

use std::{
	collections::HashMap,
	future::Future,
	marker,
	pin::Pin,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use flume::Receiver;
use omp_agent::{
	AbortHandle, Agent, AgentError, AgentEvent, AgentNode, AgentRunSummary, AgentStatus, AgentTree,
	AgentTreeLimits, HookEvent, HookGate, Interrupt, InterruptClass, InterruptSource, JobBoard,
	MailboxSender, SubagentActivity, SubagentActivityKind, SubagentDisposition, SubagentLifecycle,
	SubagentProgressSnapshot, SubagentRunState, SubagentStateError, SubagentTerminalKind,
	SubagentTerminalStatus, TurnClient, TurnId,
};
use omp_core::{Str, sf};
use omp_proto::{
	inference::v1::{Outcome, turn_event},
	thread::v1::{self as thread, Item, item},
};
use omp_tool::{ArtifactLifetime, ExpectedArtifact, JobKind, JobMetadata, JobOwner, JobRef};
use omp_tools::yield_tool::YieldType;
use parking_lot::RwLock;
use serde_json::Value;
use thiserror::Error;
use tokio::{sync::Notify, time, time::Instant};
use tokio_util::sync::CancellationToken;

use super::settings::TaskSettings;

/// Cold revival future. This allocation occurs only after memory parking, not
/// on a request, token, or tool-call path.
pub type RevivalFuture<C> =
	Pin<Box<dyn Future<Output = Result<SupervisedRuntime<C>, SupervisorError>> + Send + 'static>>;

/// Reconstructs an equivalent child loop from its durable journal and
/// snapshots.
pub trait ChildReviver<C: TurnClient + Clone>: Send + Sync + 'static {
	/// Rebuilds the live runtime after memory parking.
	fn revive(&self) -> RevivalFuture<C>;
}

/// Opaque live resources retained for exactly as long as a child loop.
pub trait ChildResource: Send + 'static {}

impl<T: Send + 'static> ChildResource for T {}

/// Live child loop plus application bindings owned by the supervisor actor.
pub struct SupervisedRuntime<C: TurnClient + Clone> {
	agent:     Agent<C>,
	resources: Vec<Box<dyn ChildResource>>,
}

impl<C: TurnClient + Clone> SupervisedRuntime<C> {
	/// Creates a supervised runtime around a fully configured durable loop.
	pub fn new(agent: Agent<C>) -> Self {
		Self { agent, resources: Vec::new() }
	}

	/// Retains an environment, control lease, hub attachment, or other binding.
	pub fn retain(&mut self, resource: impl ChildResource) {
		self.resources.push(Box::new(resource));
	}

	/// Returns the live child loop before it is registered with a supervisor.
	pub const fn agent(&self) -> &Agent<C> {
		&self.agent
	}
}

struct ChildHandle {
	commands:      flume::Sender<ChildCommand>,
	abort:         Arc<RwLock<Option<AbortHandle>>>,
	cancellations: Arc<RwLock<Vec<Arc<CancellationToken>>>>,
	mailbox:       Arc<RwLock<Option<MailboxSender>>>,
	state:         Arc<SubagentRunState>,
	metadata:      RwLock<Option<serde_json::Value>>,
	result:        RwLock<Option<serde_json::Value>>,
	cancel_reason: RwLock<Option<Str>>,
}

struct RunCommand {
	items:    Vec<Item>,
	turn_id:  TurnId,
	settings: Arc<TaskSettings>,
	cancel:   Arc<CancellationToken>,
	reply:    flume::Sender<Result<AgentRunSummary, SupervisorError>>,
}

#[derive(Default)]
struct SessionAdmission {
	active: AtomicUsize,
	queued: AtomicUsize,
	notify: Notify,
}

impl SessionAdmission {
	async fn acquire(
		self: &Arc<Self>,
		limit: usize,
		cancel: &CancellationToken,
	) -> Result<SessionPermit, SupervisorError> {
		let mut queued = false;
		loop {
			if cancel.is_cancelled() {
				if queued {
					self.queued.fetch_sub(1, Ordering::AcqRel);
				}
				return Err(SupervisorError::CancelledBeforeStart);
			}
			let active = self.active.load(Ordering::Acquire);
			if limit == 0 || active < limit {
				if self
					.active
					.compare_exchange_weak(
						active,
						active.saturating_add(1),
						Ordering::AcqRel,
						Ordering::Acquire,
					)
					.is_ok()
				{
					if queued {
						self.queued.fetch_sub(1, Ordering::AcqRel);
					}
					return Ok(SessionPermit { admission: Arc::clone(self) });
				}
				continue;
			}
			if !queued {
				self.queued.fetch_add(1, Ordering::AcqRel);
				queued = true;
			}
			tokio::select! {
				() = self.notify.notified() => {},
				() = cancel.cancelled() => {
					self.queued.fetch_sub(1, Ordering::AcqRel);
					return Err(SupervisorError::CancelledBeforeStart);
				},
			}
		}
	}
}

struct SessionPermit {
	admission: Arc<SessionAdmission>,
}

impl Drop for SessionPermit {
	fn drop(&mut self) {
		self.admission.active.fetch_sub(1, Ordering::AcqRel);
		self.admission.notify.notify_waiters();
	}
}

struct JobRegisteredHookEvent {
	payload: bytes::Bytes,
}

impl HookEvent for JobRegisteredHookEvent {
	type Return = ();

	const ID: omp_proto::toolhost::v1::HookEventId =
		omp_proto::toolhost::v1::HookEventId::HookEventJobRegistered;
	const REV: u32 = 1;

	fn encode_into(&self, out: &mut bytes::BytesMut) {
		out.extend_from_slice(&self.payload);
	}

	fn apply(&mut self, _: &omp_agent::HookPatch) -> Result<(), omp_agent::GateError> {
		Ok(())
	}
}

struct JobSettledHookEvent {
	payload: bytes::Bytes,
}

impl HookEvent for JobSettledHookEvent {
	type Return = ();

	const ID: omp_proto::toolhost::v1::HookEventId =
		omp_proto::toolhost::v1::HookEventId::HookEventJobSettled;
	const REV: u32 = 1;

	fn encode_into(&self, out: &mut bytes::BytesMut) {
		out.extend_from_slice(&self.payload);
	}

	fn apply(&mut self, _: &omp_agent::HookPatch) -> Result<(), omp_agent::GateError> {
		Ok(())
	}
}

fn notify_job_registered(gate: Option<&HookGate>, job: &JobRef) {
	let Some(gate) = gate else {
		return;
	};
	if !gate.subscribed(JobRegisteredHookEvent::ID) {
		return;
	}
	let owner = match &job.owner {
		JobOwner::NamedProcess { name, .. } => name.as_str(),
		JobOwner::AgentLoop { agent_id } => agent_id.as_str(),
	};
	let payload = serde_json::json!({
		"job_id": job.id,
		"owner": owner,
		"call_id": Value::Null,
		"lifetime": job.artifact.lifetime.to_string(),
		"expected_artifact": Value::Null,
	});
	if let Ok(payload) = serde_json::to_vec(&payload) {
		gate.notify(&JobRegisteredHookEvent { payload: bytes::Bytes::from(payload) });
	}
}

fn notify_job_settled(gate: Option<&HookGate>, job: &JobRef, failed: bool, duration_ms: u64) {
	let Some(gate) = gate else {
		return;
	};
	if !gate.subscribed(JobSettledHookEvent::ID) {
		return;
	}
	let owner = match &job.owner {
		JobOwner::NamedProcess { name, .. } => name.as_str(),
		JobOwner::AgentLoop { agent_id } => agent_id.as_str(),
	};
	let payload = serde_json::json!({
		"job_id": job.id,
		"owner": owner,
		"artifact": Value::Null,
		"failed": failed,
		"duration": format!("{duration_ms}ms"),
	});
	if let Ok(payload) = serde_json::to_vec(&payload) {
		gate.notify(&JobSettledHookEvent { payload: bytes::Bytes::from(payload) });
	}
}

enum ChildCommand {
	Run(RunCommand),
	Revive(flume::Sender<Result<u64, SupervisorError>>),
	Park(ParkReason, flume::Sender<Result<(), SupervisorError>>),
	Teardown(flume::Sender<()>),
}
#[derive(Clone, Copy, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum ParkReason {
	Parked,
	Stop,
}

/// Session-owned durable child-loop authority.
pub struct SessionSupervisor<C: TurnClient + Clone + Send + 'static> {
	tree:        Arc<AgentTree>,
	admission:   Arc<SessionAdmission>,
	children:    RwLock<HashMap<Str, ChildHandle>>,
	settings:    RwLock<Arc<TaskSettings>>,
	parent_jobs: RwLock<Option<Arc<JobBoard>>>,
	hook_gate:   RwLock<Option<Arc<HookGate>>>,
	_marker:     marker::PhantomData<fn() -> C>,
}

impl<C: TurnClient + Clone + Send + 'static> SessionSupervisor<C> {
	/// Creates one supervisor for a session's complete child roster.
	pub fn new(tree: Arc<AgentTree>) -> Self {
		Self {
			tree,
			admission: Arc::new(SessionAdmission::default()),
			children: RwLock::new(HashMap::new()),
			settings: RwLock::new(Arc::new(TaskSettings::default())),
			parent_jobs: RwLock::new(None),
			hook_gate: RwLock::new(None),
			_marker: marker::PhantomData,
		}
	}

	/// Replaces the live settings snapshot used by later runs.
	pub fn apply_settings(&self, settings: Arc<TaskSettings>) {
		*self.settings.write() = settings;
	}

	/// Binds the parent agent's authoritative detached-job board.
	pub fn bind_parent_jobs(&self, jobs: Arc<JobBoard>) {
		*self.parent_jobs.write() = Some(jobs);
	}

	/// Binds the observation gate used by driver-owned job lifecycle seams.
	pub fn bind_hook_gate(&self, gate: Arc<HookGate>) {
		*self.hook_gate.write() = Some(gate);
	}

	/// Returns the parent board used for self-delivering durable child turns.
	pub fn parent_jobs(&self) -> Option<Arc<JobBoard>> {
		self.parent_jobs.read().clone()
	}

	/// Returns a coherent snapshot of configured admission limits and current
	/// occupancy.
	pub fn limits(&self) -> AgentTreeLimits {
		let mut limits = self.tree.limits();
		let configured = self.settings.read().max_recursion_depth;
		if configured >= 0 {
			limits.max_depth = limits
				.max_depth
				.min(u16::try_from(configured).unwrap_or_default());
		}
		limits.max_concurrency = self.settings.read().max_concurrency;
		limits.active = self.admission.active.load(Ordering::Acquire);
		limits.queued = self.admission.queued.load(Ordering::Acquire);
		limits
	}

	/// Resolves a stable id, `agent://` URL, generation handle, or
	/// case-insensitive session-local name to a supervised identity.
	pub fn resolve(&self, reference: &str) -> Option<Str> {
		let reference = reference.strip_prefix("agent://").unwrap_or(reference);
		let stable = reference.rsplit_once('#').map_or(reference, |(id, _)| id);
		let children = self.children.read();
		if children.contains_key(stable) {
			return Some(Str::new(stable));
		}
		let node = self.tree.named(stable)?;
		children
			.contains_key(node.id.as_str())
			.then(|| node.id.clone())
	}

	/// Returns retained handle metadata rewritten for the current generation.
	pub fn resolved_metadata(&self, reference: &str) -> Option<serde_json::Value> {
		let id = self.resolve(reference)?;
		let child = self.children.read();
		let child = child.get(id.as_str())?;
		let mut metadata = child.metadata.read().clone()?;
		if let Some(object) = metadata.as_object_mut() {
			object.insert(
				"run_id".to_owned(),
				serde_json::Value::String(format!("{id}#{}", child.state.generation().0)),
			);
		}
		Some(metadata)
	}

	/// Registers and starts ownership of one configured child loop.
	pub fn register(
		&self,
		node: Arc<AgentNode>,
		runtime: SupervisedRuntime<C>,
		reviver: Option<Arc<dyn ChildReviver<C>>>,
	) -> Result<Arc<SubagentRunState>, SupervisorError> {
		let id = node.id.clone();
		let mut children = self.children.write();
		if children.contains_key(&id) {
			return Err(SupervisorError::AlreadyRegistered { id });
		}
		let state = Arc::new(SubagentRunState::new(id.clone()));
		let abort = Arc::new(RwLock::new(Some(runtime.agent.abort_handle())));
		let cancellations = Arc::new(RwLock::new(Vec::new()));
		let mailbox = Arc::new(RwLock::new(Some(runtime.agent.mailbox())));
		let (commands, receiver) = flume::unbounded();
		let admission = Arc::clone(&self.admission);
		let loop_state = Arc::clone(&state);
		tokio::spawn(child_loop(
			node,
			admission,
			Some(runtime),
			reviver,
			Arc::clone(&abort),
			Arc::clone(&cancellations),
			Arc::clone(&mailbox),
			loop_state,
			receiver,
		));
		children.insert(id, ChildHandle {
			commands,
			abort,
			cancellations,
			mailbox,
			state: Arc::clone(&state),
			metadata: RwLock::new(None),
			result: RwLock::new(None),
			cancel_reason: RwLock::new(None),
		});
		Ok(state)
	}

	/// Registers a journal-recovered identity without constructing live
	/// resources.
	pub fn register_parked(
		&self,
		node: Arc<AgentNode>,
		reviver: Arc<dyn ChildReviver<C>>,
	) -> Result<Arc<SubagentRunState>, SupervisorError> {
		let id = node.id.clone();
		let mut children = self.children.write();
		if children.contains_key(&id) {
			return Err(SupervisorError::AlreadyRegistered { id });
		}
		let state = Arc::new(SubagentRunState::new(id.clone()));
		state.transition(SubagentLifecycle::Settled)?;
		state.transition(SubagentLifecycle::Parked)?;
		let abort = Arc::new(RwLock::new(None));
		let cancellations = Arc::new(RwLock::new(Vec::new()));
		let mailbox = Arc::new(RwLock::new(None));
		let (commands, receiver) = flume::unbounded();
		let admission = Arc::clone(&self.admission);
		let loop_state = Arc::clone(&state);
		tokio::spawn(child_loop(
			node,
			admission,
			None,
			Some(reviver),
			Arc::clone(&abort),
			Arc::clone(&cancellations),
			Arc::clone(&mailbox),
			loop_state,
			receiver,
		));
		children.insert(id, ChildHandle {
			commands,
			abort,
			cancellations,
			mailbox,
			state: Arc::clone(&state),
			metadata: RwLock::new(None),
			result: RwLock::new(None),
			cancel_reason: RwLock::new(None),
		});
		Ok(state)
	}

	/// Runs a first turn or serialized follow-up on a retained child loop.
	pub async fn run(
		&self,
		id: &str,
		items: Vec<Item>,
		turn_id: TurnId,
	) -> Result<AgentRunSummary, SupervisorError> {
		let (commands, cancellations) = self
			.children
			.read()
			.get(id)
			.map(|child| (child.commands.clone(), Arc::clone(&child.cancellations)))
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		let (reply, response) = flume::bounded(1);
		let settings = Arc::clone(&self.settings.read());
		let cancel = Arc::new(CancellationToken::new());
		cancellations.write().push(Arc::clone(&cancel));
		commands
			.send_async(ChildCommand::Run(RunCommand { items, turn_id, settings, cancel, reply }))
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?;
		response
			.recv_async()
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?
	}

	/// Starts one background child run registered through the parent JobBoard.
	///
	/// The returned job reference is process-local; the durable agent identity
	/// remains the `AgentLoop` owner and survives job-row retention.
	pub async fn run_detached(
		&self,
		id: &str,
		items: Vec<Item>,
		turn_id: TurnId,
	) -> Result<JobRef, SupervisorError> {
		let (commands, cancellations) = self
			.children
			.read()
			.get(id)
			.map(|child| (child.commands.clone(), Arc::clone(&child.cancellations)))
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		let board = self
			.parent_jobs
			.read()
			.clone()
			.ok_or(SupervisorError::JobBoardUnavailable)?;
		let now = now_ms();
		let job = JobRef {
			id:       board.next_id(),
			owner:    JobOwner::AgentLoop { agent_id: Str::new(id) },
			metadata: Arc::new(JobMetadata::running(JobKind::Task, sf!("subagent:{}", id), now)),
			artifact: ExpectedArtifact {
				description: sf!("durable subagent result"),
				media_type:  Some(sf!("application/vnd.omp.subagent-result+json")),
				lifetime:    ArtifactLifetime::Durable,
			},
		};
		if !board
			.try_register(job.clone())
			.map_err(|_| SupervisorError::JobCapacity)?
		{
			return Err(SupervisorError::DuplicateJob { id: job.id.clone() });
		}
		let hook_gate = self.hook_gate.read().clone();
		notify_job_registered(hook_gate.as_deref(), &job);
		let (reply, response) = flume::bounded(1);
		let settings = Arc::clone(&self.settings.read());
		let cancel = Arc::new(CancellationToken::new());
		cancellations.write().push(Arc::clone(&cancel));
		commands
			.send_async(ChildCommand::Run(RunCommand { items, turn_id, settings, cancel, reply }))
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?;
		let settlement_board = board;
		let settlement_id = job.id.clone();
		let settlement_job = job.clone();
		tokio::spawn(async move {
			let (text, failed) = match response.recv_async().await {
				Ok(Ok(summary)) => (
					summary
						.final_assistant()
						.map_or_else(|| sf!("subagent completed"), Str::new),
					false,
				),
				Ok(Err(_)) => (sf!("subagent failed; inspect its durable history for details"), true),
				Err(_) => (sf!("subagent supervisor stopped before settlement"), true),
			};
			if settlement_board
				.settle(settlement_id.as_str(), system_item(text))
				.is_ok_and(|accepted| accepted)
			{
				notify_job_settled(
					hook_gate.as_deref(),
					&settlement_job,
					failed,
					now_ms().saturating_sub(now),
				);
			}
		});
		Ok(job)
	}

	/// Cancels the active generation without destroying its durable identity.
	pub fn cancel(&self, id: &str) -> Result<(), SupervisorError> {
		let children = self.children.read();
		let child = children
			.get(id)
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		if let Some(abort) = child.abort.read().as_ref() {
			abort.abort();
		}
		for cancellation in child.cancellations.read().iter() {
			cancellation.cancel();
		}
		Ok(())
	}

	/// Requests cooperative cancellation with the caller's reason, then
	/// escalates to the generation abort handle after the courtesy grace.
	pub async fn cancel_with_grace(
		&self,
		id: &str,
		generation: u64,
		reason: Str,
		grace: Duration,
	) -> Result<(), SupervisorError> {
		self.state_at_generation(id, generation)?;
		let (state, mailbox, abort) = {
			let children = self.children.read();
			let child = children
				.get(id)
				.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::new(id) })?;
			*child.cancel_reason.write() = Some(reason.clone());
			(Arc::clone(&child.state), child.mailbox.read().clone(), Arc::clone(&child.abort))
		};
		if let Some(mailbox) = mailbox {
			let _ = mailbox.try_enqueue(Interrupt {
				class:  InterruptClass::Immediate,
				item:   system_item(reason),
				source: InterruptSource::Producer(sf!("cancel")),
			});
		}
		if !grace.is_zero() {
			time::sleep(grace).await;
		}
		if !matches!(state.lifecycle(), SubagentLifecycle::Settled | SubagentLifecycle::Parked)
			&& let Some(abort) = abort.read().as_ref()
		{
			abort.abort();
		}
		Ok(())
	}

	/// Returns the authoritative cancellation reason recorded for the current
	/// retained generation.
	pub fn cancellation_reason(&self, id: &str) -> Option<Str> {
		self
			.children
			.read()
			.get(id)
			.and_then(|child| child.cancel_reason.read().clone())
	}

	/// Installs the durable public handle projection after admission.
	pub fn set_metadata(
		&self,
		id: &str,
		metadata: serde_json::Value,
	) -> Result<(), SupervisorError> {
		let children = self.children.read();
		let child = children
			.get(id)
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		*child.metadata.write() = Some(metadata);
		Ok(())
	}

	/// Returns the public handle projection retained by the child owner.
	pub fn metadata(&self, id: &str) -> Option<serde_json::Value> {
		self
			.children
			.read()
			.get(id)
			.and_then(|child| child.metadata.read().clone())
	}

	/// Retains the completed public result emitted by the child application.
	pub fn set_result(&self, id: &str, result: serde_json::Value) -> Result<(), SupervisorError> {
		let children = self.children.read();
		let child = children
			.get(id)
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		*child.result.write() = Some(result);
		Ok(())
	}

	/// Returns the completed application result for the current generation.
	pub fn result(&self, id: &str) -> Option<serde_json::Value> {
		self
			.children
			.read()
			.get(id)
			.and_then(|child| child.result.read().clone())
	}

	/// Cancels only the generation named by an opaque CONTROL handle.
	pub fn cancel_at_generation(&self, id: &str, generation: u64) -> Result<(), SupervisorError> {
		self.state_at_generation(id, generation)?;
		self.cancel(id)
	}

	/// Releases one idle loop's live resources while retaining its state and
	/// reviver.
	pub async fn park(&self, id: &str) -> Result<(), SupervisorError> {
		self.park_with_reason(id, ParkReason::Parked).await
	}

	/// Releases a cancelled loop within the stop lifecycle.
	pub async fn park_stopped(&self, id: &str) -> Result<(), SupervisorError> {
		self.park_with_reason(id, ParkReason::Stop).await
	}

	async fn park_with_reason(&self, id: &str, reason: ParkReason) -> Result<(), SupervisorError> {
		let commands = self
			.children
			.read()
			.get(id)
			.map(|child| child.commands.clone())
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		let (reply, response) = flume::bounded(1);
		commands
			.send_async(ChildCommand::Park(reason, reply))
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?;
		response
			.recv_async()
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?
	}

	/// Returns retained state without requiring a live listener or child loop.
	pub fn state(&self, id: &str) -> Option<Arc<SubagentRunState>> {
		self
			.children
			.read()
			.get(id)
			.map(|child| Arc::clone(&child.state))
	}

	/// Aggregates the latest retained progress for one child and every
	/// descendant using the authoritative tree lineage.
	pub fn subtree_progress(&self, id: &str) -> Option<SubagentProgressSnapshot> {
		self.tree.node(id)?;
		let children = self.children.read();
		let mut total = SubagentProgressSnapshot::default();
		for node in self.tree.roster() {
			let mut current = Some(Arc::clone(node));
			let included = loop {
				let Some(candidate) = current else {
					break false;
				};
				if candidate.id == id {
					break true;
				}
				current = candidate
					.parent
					.as_ref()
					.and_then(|parent| self.tree.node(parent));
			};
			if !included {
				continue;
			}
			let Some(state) = children.get(node.id.as_str()) else {
				continue;
			};
			let progress = state.state.progress();
			total.requests = total.requests.saturating_add(progress.requests);
			total.tool_calls = total.tool_calls.saturating_add(progress.tool_calls);
			total.input_tokens = total.input_tokens.saturating_add(progress.input_tokens);
			total.output_tokens = total.output_tokens.saturating_add(progress.output_tokens);
			total.cost_micros = total.cost_micros.saturating_add(progress.cost_micros);
			total.context_tokens = total.context_tokens.max(progress.context_tokens);
		}
		Some(total)
	}

	/// Returns retained state only when the caller's run generation is current.
	///
	/// CONTROL handles must use this fence before reading, steering, cancelling,
	/// waiting, or releasing a child. A stable agent id alone is insufficient
	/// because cold revival starts a new generation under the same journal.
	pub fn state_at_generation(
		&self,
		id: &str,
		generation: u64,
	) -> Result<Arc<SubagentRunState>, SupervisorError> {
		let state = self
			.state(id)
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		let current = state.generation().0;
		if current != generation {
			return Err(SupervisorError::StaleGeneration {
				id: Str::from(id),
				expected: generation,
				current,
			});
		}
		Ok(state)
	}

	/// Cold-reconstructs a parked child and returns its new fenced generation.
	pub async fn revive(&self, reference: &str) -> Result<u64, SupervisorError> {
		let id = self
			.resolve(reference)
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::new(reference) })?;
		let commands = self
			.children
			.read()
			.get(id.as_str())
			.map(|child| child.commands.clone())
			.ok_or_else(|| SupervisorError::UnknownAgent { id: id.clone() })?;
		let (reply, response) = flume::bounded(1);
		commands
			.send_async(ChildCommand::Revive(reply))
			.await
			.map_err(|_| SupervisorError::Stopped { id: id.clone() })?;
		response
			.recv_async()
			.await
			.map_err(|_| SupervisorError::Stopped { id })?
	}

	/// Releases live resources only when the caller still owns this generation.
	pub async fn park_at_generation(
		&self,
		id: &str,
		generation: u64,
	) -> Result<(), SupervisorError> {
		self.state_at_generation(id, generation)?;
		self.park(id).await
	}

	/// Relinquishes structural supervision without cancelling the generation.
	///
	/// A running actor finishes its current generation before processing the
	/// queued teardown; the caller returns as soon as ownership is transferred.
	pub async fn release_at_generation(
		&self,
		id: &str,
		generation: u64,
	) -> Result<(), SupervisorError> {
		let state = self.state_at_generation(id, generation)?;
		let active =
			!matches!(state.lifecycle(), SubagentLifecycle::Settled | SubagentLifecycle::Parked);
		let child = self
			.children
			.write()
			.remove(id)
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::new(id) })?;
		let (reply, response) = flume::bounded(1);
		child
			.commands
			.send_async(ChildCommand::Teardown(reply))
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::new(id) })?;
		if active {
			tokio::spawn(async move {
				let _ = response.recv_async().await;
			});
			Ok(())
		} else {
			response
				.recv_async()
				.await
				.map_err(|_| SupervisorError::Stopped { id: Str::new(id) })
		}
	}

	/// Cancels and tears down one live actor at session shutdown.
	pub async fn teardown(&self, id: &str) -> Result<(), SupervisorError> {
		let child = self
			.children
			.write()
			.remove(id)
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		if let Some(abort) = child.abort.read().as_ref() {
			abort.abort();
		}
		let (reply, response) = flume::bounded(1);
		child
			.commands
			.send_async(ChildCommand::Teardown(reply))
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?;
		response
			.recv_async()
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })
	}
}

impl<C: TurnClient + Clone + Send + 'static> Drop for SessionSupervisor<C> {
	fn drop(&mut self) {
		for (_, child) in self.children.get_mut().drain() {
			if let Some(abort) = child.abort.read().as_ref() {
				abort.abort();
			}
			let (reply, _) = flume::bounded(1);
			let _ = child.commands.send(ChildCommand::Teardown(reply));
		}
	}
}

async fn child_loop<C: TurnClient + Clone + Send + 'static>(
	node: Arc<AgentNode>,
	admission: Arc<SessionAdmission>,
	mut runtime: Option<SupervisedRuntime<C>>,
	reviver: Option<Arc<dyn ChildReviver<C>>>,
	abort: Arc<RwLock<Option<AbortHandle>>>,
	cancellations: Arc<RwLock<Vec<Arc<CancellationToken>>>>,
	mailbox: Arc<RwLock<Option<MailboxSender>>>,
	state: Arc<SubagentRunState>,
	commands: Receiver<ChildCommand>,
) {
	while let Ok(command) = commands.recv_async().await {
		match command {
			ChildCommand::Run(command) => {
				let result = run_child(
					&node,
					&admission,
					&state,
					&mut runtime,
					reviver.as_ref(),
					&abort,
					&mailbox,
					&command.cancel,
					command.items,
					command.turn_id,
					&command.settings,
				)
				.await;
				cancellations
					.write()
					.retain(|candidate| !Arc::ptr_eq(candidate, &command.cancel));
				let _ = command.reply.send(result);
			},
			ChildCommand::Revive(reply) => {
				let result =
					revive_child(&node, &state, &mut runtime, reviver.as_ref(), &abort, &mailbox).await;
				let _ = reply.send(result);
			},
			ChildCommand::Park(reason, reply) => {
				let result = if reviver.is_none() {
					Err(SupervisorError::RevivalUnavailable { id: node.id.clone() })
				} else if state.lifecycle() != SubagentLifecycle::Settled {
					Err(SupervisorError::NotIdle { id: node.id.clone() })
				} else {
					let journaled = runtime.as_mut().map_or(Ok(()), |runtime| {
						record_lifecycle(runtime, &state, &node.id, <&'static str>::from(reason), None)
					});
					journaled.and_then(|()| {
						runtime = None;
						*abort.write() = None;
						*mailbox.write() = None;
						node.set_status(AgentStatus::Settled);
						state
							.transition(SubagentLifecycle::Parked)
							.map_err(SupervisorError::State)
					})
				};
				let _ = reply.send(result);
			},
			ChildCommand::Teardown(reply) => {
				drop(runtime.take());
				let _ = reply.send(());
				break;
			},
		}
	}
}

async fn revive_child<C: TurnClient + Clone + Send + 'static>(
	node: &AgentNode,
	state: &SubagentRunState,
	runtime: &mut Option<SupervisedRuntime<C>>,
	reviver: Option<&Arc<dyn ChildReviver<C>>>,
	abort: &RwLock<Option<AbortHandle>>,
	mailbox: &RwLock<Option<MailboxSender>>,
) -> Result<u64, SupervisorError> {
	if state.lifecycle() != SubagentLifecycle::Parked {
		return Err(SupervisorError::NotParked {
			id:        node.id.clone(),
			lifecycle: state.lifecycle(),
		});
	}
	let factory =
		reviver.ok_or_else(|| SupervisorError::RevivalUnavailable { id: node.id.clone() })?;
	state.begin_generation()?;
	let mut restored = match factory.revive().await {
		Ok(restored) => restored,
		Err(error) => {
			state.transition(SubagentLifecycle::Settled)?;
			return Err(error);
		},
	};
	record_lifecycle(&mut restored, state, &node.id, "reopen", None)?;
	*abort.write() = Some(restored.agent.abort_handle());
	*mailbox.write() = Some(restored.agent.mailbox());
	*runtime = Some(restored);
	state.transition(SubagentLifecycle::Settled)?;
	node.set_status(AgentStatus::Settled);
	Ok(state.generation().0)
}

async fn run_child<C: TurnClient + Clone + Send + 'static>(
	node: &AgentNode,
	admission: &Arc<SessionAdmission>,
	state: &SubagentRunState,
	runtime: &mut Option<SupervisedRuntime<C>>,
	reviver: Option<&Arc<dyn ChildReviver<C>>>,
	abort: &RwLock<Option<AbortHandle>>,
	mailbox: &RwLock<Option<MailboxSender>>,
	cancel: &CancellationToken,
	items: Vec<Item>,
	turn_id: TurnId,
	settings: &TaskSettings,
) -> Result<AgentRunSummary, SupervisorError> {
	let first_turn = state.lifecycle() == SubagentLifecycle::Created;
	let reopening = state.lifecycle() == SubagentLifecycle::Parked;
	match state.lifecycle() {
		SubagentLifecycle::Created => state.transition(SubagentLifecycle::Starting)?,
		SubagentLifecycle::Settled | SubagentLifecycle::Parked => {
			state.begin_generation()?;
		},
		lifecycle => return Err(SupervisorError::NotIdleState { id: node.id.clone(), lifecycle }),
	}
	let permit = match admission.acquire(settings.max_concurrency, cancel).await {
		Ok(permit) => permit,
		Err(SupervisorError::CancelledBeforeStart) => {
			settle(state, SubagentTerminalKind::Cancelled)?;
			node.set_status(AgentStatus::Cancelled);
			return Err(SupervisorError::CancelledBeforeStart);
		},
		Err(error) => return Err(error),
	};
	if runtime.is_none() {
		let Some(factory) = reviver else {
			rollback_pre_run(state, reopening)?;
			return Err(SupervisorError::RevivalUnavailable { id: node.id.clone() });
		};
		*runtime = match factory.revive().await {
			Ok(runtime) => Some(runtime),
			Err(error) => {
				rollback_pre_run(state, reopening)?;
				return Err(error);
			},
		};
		*abort.write() = Some(
			runtime
				.as_ref()
				.expect("reviver produced a live runtime")
				.agent
				.abort_handle(),
		);
		*mailbox.write() = Some(
			runtime
				.as_ref()
				.expect("reviver produced a live runtime")
				.agent
				.mailbox(),
		);
	}
	let runtime = runtime
		.as_mut()
		.expect("runtime was restored before lifecycle publication");
	let publication = record_lifecycle(
		runtime,
		state,
		&node.id,
		if first_turn {
			"spawn"
		} else if reopening {
			"reopen"
		} else {
			"turn-started"
		},
		None,
	)
	.and_then(|()| {
		if first_turn || reopening {
			record_lifecycle(runtime, state, &node.id, "turn-started", None)
		} else {
			Ok(())
		}
	});
	if let Err(error) = publication {
		rollback_pre_run(state, reopening)?;
		return Err(error);
	}
	state.transition(SubagentLifecycle::Running)?;
	state.record_activity(SubagentActivity {
		kind: Some(if first_turn {
			SubagentActivityKind::FirstTurn
		} else {
			SubagentActivityKind::FollowUp
		}),
		detail: sf!(if first_turn {
			"first turn"
		} else {
			"follow-up turn"
		}),
		..SubagentActivity::default()
	})?;
	node.set_status(AgentStatus::Running);
	let result = supervised_submit(state, runtime, abort, items, turn_id, settings).await;
	drop(permit);
	match result {
		Ok(summary) => {
			let kind = if summary.interrupted {
				SubagentTerminalKind::Cancelled
			} else {
				SubagentTerminalKind::Succeeded
			};
			settle(state, kind)?;
			record_lifecycle(runtime, state, &node.id, "turn-settled", Some(kind))?;
			node.set_status(if summary.interrupted {
				AgentStatus::Cancelled
			} else {
				AgentStatus::Completed
			});
			Ok(summary)
		},
		Err(SupervisorError::RuntimeLimit { .. }) => {
			settle(state, SubagentTerminalKind::RuntimeLimit)?;
			record_lifecycle(
				runtime,
				state,
				&node.id,
				"turn-settled",
				Some(SubagentTerminalKind::RuntimeLimit),
			)?;
			node.set_status(AgentStatus::Exhausted);
			result
		},
		Err(SupervisorError::RequestBudget { .. }) => {
			settle(state, SubagentTerminalKind::Failed)?;
			record_lifecycle(
				runtime,
				state,
				&node.id,
				"turn-settled",
				Some(SubagentTerminalKind::Failed),
			)?;
			node.set_status(AgentStatus::Exhausted);
			result
		},
		Err(SupervisorError::Agent(AgentError::Interrupted)) => {
			settle(state, SubagentTerminalKind::Cancelled)?;
			record_lifecycle(
				runtime,
				state,
				&node.id,
				"turn-settled",
				Some(SubagentTerminalKind::Cancelled),
			)?;
			node.set_status(AgentStatus::Cancelled);
			Err(SupervisorError::Agent(AgentError::Interrupted))
		},
		Err(error) => {
			settle(state, SubagentTerminalKind::Failed)?;
			record_lifecycle(
				runtime,
				state,
				&node.id,
				"turn-settled",
				Some(SubagentTerminalKind::Failed),
			)?;
			node.set_status(AgentStatus::Failed);
			Err(error)
		},
	}
}

fn record_lifecycle<C: TurnClient + Clone>(
	runtime: &mut SupervisedRuntime<C>,
	state: &SubagentRunState,
	child_id: &Str,
	lifecycle: &'static str,
	terminal: Option<SubagentTerminalKind>,
) -> Result<(), SupervisorError> {
	runtime.agent.record_child_lifecycle(
		now_ms(),
		omp_storage::transcript::ChildLifecycleEntry {
			child_id:        child_id.clone(),
			generation:      state.generation().0,
			init_event:      0,
			lifecycle:       Str::new(lifecycle),
			terminal_status: terminal.map(|kind| Str::from(kind.to_string())),
		},
	)?;
	Ok(())
}

async fn supervised_submit<C: TurnClient + Clone + Send + 'static>(
	state: &SubagentRunState,
	runtime: &mut SupervisedRuntime<C>,
	abort: &RwLock<Option<AbortHandle>>,
	items: Vec<Item>,
	turn_id: TurnId,
	settings: &TaskSettings,
) -> Result<AgentRunSummary, SupervisorError> {
	let events = runtime.agent.events().subscribe_lossless();
	let mailbox = runtime.agent.mailbox();
	let submission = runtime.agent.submit(items, turn_id);
	tokio::pin!(submission);
	let deadline = Instant::now() + Duration::from_millis(settings.max_runtime_ms.max(1));
	let mut runtime_limit_active = settings.max_runtime_ms != 0;
	let mut observed_outcomes = 0;
	let mut pending_terminal_yield = None;
	let mut committed_terminal_yield = None;
	loop {
		tokio::select! {
			biased;
			event = events.recv() => {
				let Ok(event) = event else {
					continue;
				};
				if handle_event(
					state,
					event.as_ref(),
					&mailbox,
					settings,
					abort,
					&mut observed_outcomes,
					&mut pending_terminal_yield,
					&mut committed_terminal_yield,
				)? {
					return Err(SupervisorError::RequestBudget {
						requests: state.progress().requests,
						budget: settings.soft_request_budget,
					});
				}
				if state.yield_committed() {
					runtime_limit_active = false;
				}
			},
			result = &mut submission => {
				return preserve_committed_yield(result, committed_terminal_yield)
					.map_err(SupervisorError::Agent);
			},
			() = tokio::time::sleep_until(deadline), if runtime_limit_active => {
				if !runtime_limit_should_abort(state) {
					runtime_limit_active = false;
					continue;
				}
				if let Some(abort) = abort.read().as_ref() {
					abort.abort();
				}
				let _ = tokio::time::timeout(Duration::from_secs(5), &mut submission).await;
				return Err(SupervisorError::RuntimeLimit {
					max_runtime_ms: settings.max_runtime_ms,
				});
			},
		}
	}
}

struct PendingTerminalYield {
	call_id:         Str,
	outcome:         Outcome,
	committed_turns: u32,
}

/// Keeps a tool-confirmed terminal yield authoritative over later teardown
/// failures while preserving an explicit caller abort.
fn preserve_committed_yield(
	result: Result<AgentRunSummary, AgentError>,
	committed_terminal_yield: Option<AgentRunSummary>,
) -> Result<AgentRunSummary, AgentError> {
	match (result, committed_terminal_yield) {
		(Err(AgentError::Interrupted), _) => Err(AgentError::Interrupted),
		(Err(_), Some(summary)) => Ok(summary),
		(result, _) => result,
	}
}

fn handle_event(
	state: &SubagentRunState,
	event: &AgentEvent,
	mailbox: &omp_agent::MailboxSender,
	settings: &TaskSettings,
	abort: &RwLock<Option<AbortHandle>>,
	observed_outcomes: &mut u32,
	pending_terminal_yield: &mut Option<PendingTerminalYield>,
	committed_terminal_yield: &mut Option<AgentRunSummary>,
) -> Result<bool, SupervisorError> {
	match event {
		AgentEvent::Turn { event, .. } => match event.event.as_ref() {
			Some(turn_event::Event::Accepted(accepted)) if !accepted.replay => {
				state.record_activity(SubagentActivity {
					kind: Some(SubagentActivityKind::Request),
					detail: sf!("assistant request"),
					..SubagentActivity::default()
				})?;
				let requests = state.progress().requests;
				let budget = settings.soft_request_budget;
				if budget == 0 {
					return Ok(false);
				}
				let stop = u64::from(budget).saturating_mul(3).saturating_add(1) / 2;
				if settings.soft_request_budget_notice && requests == budget {
					steer(
						mailbox,
						sf!(
							"[budget notice] {} requests used (soft budget {}). Finish the current step \
							 and yield the final result.",
							requests,
							budget
						),
					);
				}
				if u64::from(requests) == stop {
					steer(
						mailbox,
						sf!(
							"Soft request budget reached its force threshold. Call yield now with all \
							 partial work; do not start another task."
						),
					);
				}
				if u64::from(requests) >= stop.saturating_add(5) {
					if let Some(abort) = abort.read().as_ref() {
						abort.abort();
					}
					return Ok(true);
				}
			},
			Some(turn_event::Event::Outcome(outcome)) => {
				*observed_outcomes = observed_outcomes.saturating_add(1);
				if let Some(call_id) = outcome.output.iter().find_map(terminal_yield_call) {
					*pending_terminal_yield = Some(PendingTerminalYield {
						call_id,
						outcome: outcome.clone(),
						committed_turns: *observed_outcomes,
					});
				}
				let usage = outcome.usage.as_ref();
				state.record_activity(SubagentActivity {
					kind:           Some(SubagentActivityKind::Usage),
					detail:         sf!("usage receipt"),
					serving_model:  (!outcome.model.is_empty()).then(|| Str::new(&outcome.model)),
					input_tokens:   usage.map_or(0, |usage| usage.input_tokens),
					output_tokens:  usage.map_or(0, |usage| usage.output_tokens),
					cost_micros:    outcome
						.cost
						.as_ref()
						.map_or(0, |cost| cost.nanos_usd / 1_000),
					context_tokens: usage
						.and_then(|usage| usage.context_tokens)
						.unwrap_or_default(),
				})?;
			},
			Some(turn_event::Event::Error(error)) => {
				state.record_activity(SubagentActivity {
					kind: Some(SubagentActivityKind::ProviderWait),
					detail: Str::new(&error.detail),
					..SubagentActivity::default()
				})?;
			},
			_ => {},
		},
		AgentEvent::ToolFinished { call_id, item, .. } => {
			if pending_terminal_yield
				.as_ref()
				.is_some_and(|pending| pending.call_id.as_str() == call_id.as_str())
			{
				let succeeded = matches!(
					item.kind.as_ref(),
					Some(omp_proto::thread::v1::item::Kind::ToolResult(result))
						if result.name == "yield" && !result.is_error
				);
				let pending = pending_terminal_yield.take();
				if succeeded {
					let pending = pending.expect("matching terminal yield is retained");
					state.commit_yield();
					*committed_terminal_yield =
						Some(AgentRunSummary::settled(pending.outcome, pending.committed_turns, false));
				}
			}
		},
		AgentEvent::ToolOpened { name, .. } => {
			state.record_activity(SubagentActivity {
				kind: Some(SubagentActivityKind::Tool),
				detail: name.clone(),
				..SubagentActivity::default()
			})?;
		},
		AgentEvent::PhaseChanged { to: omp_agent::AgentPhase::Turning, .. } => {
			state.record_activity(SubagentActivity {
				kind: Some(SubagentActivityKind::ProviderWait),
				detail: sf!("provider admission"),
				..SubagentActivity::default()
			})?;
		},
		_ => {},
	}
	Ok(false)
}

fn terminal_yield_call(item: &Item) -> Option<Str> {
	let Some(item::Kind::ToolCall(call)) = item.kind.as_ref() else {
		return None;
	};
	if call.name != "yield" {
		return None;
	}
	let params = serde_json::from_slice::<omp_tools::yield_tool::Params>(&call.args_json).ok()?;
	let terminal = match params.kind {
		Some(YieldType::Terminal(_)) => true,
		Some(YieldType::Sections(_)) => false,
		None => params.result.is_some(),
	};
	terminal.then(|| Str::new(&call.id))
}

fn steer(mailbox: &omp_agent::MailboxSender, text: Str) {
	let item = system_item(text);
	let _ = mailbox.try_enqueue(Interrupt {
		class: InterruptClass::TurnBoundary,
		item,
		source: InterruptSource::Producer(sf!("subagent supervisor")),
	});
}

fn system_item(text: Str) -> Item {
	Item {
		seq:           0,
		created_at_ms: now_ms(),
		kind:          Some(item::Kind::Message(thread::Message {
			role:            thread::Role::System as i32,
			parts:           vec![omp_proto::thread::v1::Part {
				kind: Some(omp_proto::thread::v1::part::Kind::Text(text.to_string())),
			}],
			synthetic:       None,
			user_initiated:  None,
			completed_at_ms: None,
			usage:           None,
		})),
		props:         None,
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

fn runtime_limit_should_abort(state: &SubagentRunState) -> bool {
	matches!(
		state.lifecycle(),
		SubagentLifecycle::Starting | SubagentLifecycle::Running | SubagentLifecycle::Waiting
	) && !state.yield_committed()
}

fn settle(state: &SubagentRunState, kind: SubagentTerminalKind) -> Result<(), SupervisorError> {
	let summary = match kind {
		SubagentTerminalKind::Succeeded => sf!("completed"),
		SubagentTerminalKind::Cancelled => sf!("cancelled with bounded salvage"),
		SubagentTerminalKind::SchemaInvalid => sf!("structured output schema validation failed"),
		SubagentTerminalKind::RuntimeLimit => sf!("runtime limit reached with bounded salvage"),
		SubagentTerminalKind::Failed => sf!("subagent run failed"),
	};
	state.settle(SubagentTerminalStatus {
		kind,
		summary,
		disposition: SubagentDisposition::default(),
	})?;
	Ok(())
}

fn rollback_pre_run(state: &SubagentRunState, reopening: bool) -> Result<(), SupervisorError> {
	settle(state, SubagentTerminalKind::Failed)?;
	if reopening {
		state.transition(SubagentLifecycle::Parked)?;
	}
	Ok(())
}

/// Durable supervisor operation failure.
#[derive(Debug, Error)]
pub enum SupervisorError {
	/// Agent loop failure.
	#[error(transparent)]
	Agent(#[from] AgentError),
	/// Durable lifecycle publication failed.
	#[error(transparent)]
	Journal(#[from] omp_agent::JournalError),
	/// Core-owned lifecycle mutation failed.
	#[error(transparent)]
	State(#[from] SubagentStateError),
	/// Admission failed.
	#[error(transparent)]
	Admission(#[from] omp_agent::SpawnRefusal),
	/// Cancellation won while this run was waiting for its parent-local slot.
	#[error("subagent run was cancelled before execution")]
	CancelledBeforeStart,
	/// Configured wall-clock limit stopped this generation.
	#[error("subagent runtime limit reached after {max_runtime_ms}ms")]
	RuntimeLimit {
		/// Configured limit.
		max_runtime_ms: u64,
	},
	/// The child ignored forced-yield steering beyond its request budget.
	#[error("subagent request budget exhausted ({requests} requests; budget {budget})")]
	RequestBudget {
		/// Requests observed.
		requests: u32,
		/// Configured soft budget.
		budget:   u32,
	},
	/// No root JobBoard has been bound yet.
	#[error("parent detached-job board is unavailable")]
	JobBoardUnavailable,
	/// Parent JobBoard capacity rejected this child.
	#[error("parent detached-job capacity is exhausted")]
	JobCapacity,
	/// A generated process-local JobBoard identifier collided.
	#[error("subagent job {id} is already registered")]
	DuplicateJob {
		/// Conflicting job identifier.
		id: Str,
	},
	/// Stable ID is already owned by this session supervisor.
	#[error("subagent {id} is already registered")]
	AlreadyRegistered {
		/// Stable ID.
		id: Str,
	},
	/// Stable ID is unknown in this session.
	#[error("subagent {id} is not registered")]
	UnknownAgent {
		/// Stable ID.
		id: Str,
	},
	/// A handle addressed an older or not-yet-current run generation.
	#[error("subagent {id} generation is stale (expected {expected}, current {current})")]
	StaleGeneration {
		/// Stable child identity.
		id:       Str,
		/// Generation carried by the handle.
		expected: u64,
		/// Current retained generation.
		current:  u64,
	},
	/// Child actor stopped before replying.
	#[error("subagent supervisor for {id} stopped")]
	Stopped {
		/// Stable ID.
		id: Str,
	},
	/// The child is executing or transitioning and cannot accept another turn.
	#[error("subagent {id} is not idle ({lifecycle})")]
	NotIdleState {
		/// Stable ID.
		id:        Str,
		/// Current lifecycle.
		lifecycle: SubagentLifecycle,
	},
	/// The child is not settled and cannot be parked.
	#[error("subagent {id} is not idle")]
	NotIdle {
		/// Stable ID.
		id: Str,
	},
	/// Only a memory-parked identity can be cold revived.
	#[error("subagent {id} cannot be revived from lifecycle {lifecycle}")]
	NotParked {
		/// Stable ID.
		id:        Str,
		/// Current lifecycle.
		lifecycle: SubagentLifecycle,
	},
	/// Memory parking is unavailable without an equivalent cold reviver.
	#[error("subagent {id} has no cold-revival factory")]
	RevivalUnavailable {
		/// Stable ID.
		id: Str,
	},
	/// The application could not reconstruct an equivalent parked loop.
	#[error("subagent {id} cold revival failed")]
	RevivalFailed {
		/// Stable ID.
		id: Str,
	},
}
#[cfg(test)]
mod tests {
	use super::*;

	fn running_state() -> SubagentRunState {
		let state = SubagentRunState::new(sf!("race-child"));
		state.transition(SubagentLifecycle::Starting).unwrap();
		state.transition(SubagentLifecycle::Running).unwrap();
		state
	}

	fn yield_call(args: &'static [u8]) -> Item {
		Item {
			kind: Some(item::Kind::ToolCall(thread::ToolCall {
				id: sf!("yield-call").to_string(),
				name: sf!("yield").to_string(),
				args_json: bytes::Bytes::from_static(args),
				..Default::default()
			})),
			..Default::default()
		}
	}

	#[test]
	fn job_registered_and_settled_observations_preserve_payloads() {
		let (gate, dispatches) = HookGate::channel();
		let subscription = |event, id| omp_agent::Subscription {
			host: sf!("test"),
			source: omp_agent::SourceRef {
				layer:        0,
				publisher:    sf!("test"),
				extension_id: sf!("jobs"),
			},
			id,
			event,
			phase: omp_agent::HookPhase::Observe,
			order: 0,
			on_failure: omp_agent::OnFailure::Defer,
			when: omp_agent::When::default(),
		};
		gate
			.subscribe("test", [
				subscription(omp_proto::toolhost::v1::HookEventId::HookEventJobRegistered, 52),
				subscription(omp_proto::toolhost::v1::HookEventId::HookEventJobSettled, 53),
			])
			.expect("job subscriptions");
		let job = JobRef {
			id:       sf!("job-1"),
			owner:    JobOwner::AgentLoop { agent_id: sf!("worker-1") },
			metadata: Arc::new(JobMetadata::running(JobKind::Task, sf!("task"), 10)),
			artifact: ExpectedArtifact {
				description: sf!("result"),
				media_type:  None,
				lifetime:    ArtifactLifetime::Durable,
			},
		};
		notify_job_registered(Some(&gate), &job);
		notify_job_settled(Some(&gate), &job, true, 25);
		let registered = dispatches.try_recv().expect("job registered frame");
		let settled = dispatches.try_recv().expect("job settled frame");
		let registered: Value =
			serde_json::from_slice(&registered.payload).expect("registered payload");
		let settled: Value = serde_json::from_slice(&settled.payload).expect("settled payload");
		assert_eq!(registered["job_id"], "job-1");
		assert_eq!(registered["lifetime"], "durable");
		assert_eq!(settled["failed"], true);
		assert_eq!(settled["duration"], "25ms");
	}

	#[test]
	fn only_structurally_valid_terminal_yields_can_commit_an_outcome() {
		assert_eq!(
			terminal_yield_call(&yield_call(br#"{"result":{"data":{"ok":true}}}"#)),
			Some(sf!("yield-call"))
		);
		assert!(
			terminal_yield_call(&yield_call(br#"{"type":["findings"],"result":{"data":[1]}}"#))
				.is_none()
		);
		assert!(terminal_yield_call(&yield_call(br#"{}"#)).is_none());
	}

	#[test]
	fn post_yield_failure_does_not_replace_committed_outcome() {
		let summary = AgentRunSummary::settled(Outcome::default(), 3, false);
		let preserved = preserve_committed_yield(
			Err(AgentError::Protocol("post-run cleanup failed")),
			Some(summary),
		)
		.expect("committed yield is authoritative");
		assert_eq!(preserved.committed_turns, 3);
		assert!(!preserved.interrupted);

		assert!(matches!(
			preserve_committed_yield(Err(AgentError::Protocol("run failed before yielding")), None,),
			Err(AgentError::Protocol("run failed before yielding"))
		));
		assert!(matches!(
			preserve_committed_yield(
				Err(AgentError::Interrupted),
				Some(AgentRunSummary::settled(Outcome::default(), 3, false)),
			),
			Err(AgentError::Interrupted)
		));
	}

	#[test]
	fn late_runtime_limit_does_not_replace_committed_outcomes() {
		let yielded = running_state();
		yielded.commit_yield();
		assert!(!runtime_limit_should_abort(&yielded));
		settle(&yielded, SubagentTerminalKind::Succeeded).unwrap();
		assert_eq!(
			yielded.terminal().map(|terminal| terminal.kind),
			Some(SubagentTerminalKind::Succeeded)
		);
		assert!(!runtime_limit_should_abort(&yielded));

		let budget_killed = running_state();
		settle(&budget_killed, SubagentTerminalKind::Failed).unwrap();
		assert!(!runtime_limit_should_abort(&budget_killed));
		assert_eq!(
			budget_killed.terminal().map(|terminal| terminal.kind),
			Some(SubagentTerminalKind::Failed)
		);
	}

	#[tokio::test]
	async fn parent_local_admission_is_cancellable_without_consuming_capacity() {
		let admission = Arc::new(SessionAdmission::default());
		let first = admission
			.acquire(1, &CancellationToken::new())
			.await
			.expect("first permit");
		let nested_session = Arc::new(SessionAdmission::default());
		let nested = nested_session
			.acquire(1, &CancellationToken::new())
			.await
			.expect("nested session has independent sibling admission");
		drop(nested);
		let cancel = CancellationToken::new();
		let waiting = {
			let admission = Arc::clone(&admission);
			let cancel = cancel.clone();
			tokio::spawn(async move { admission.acquire(1, &cancel).await })
		};
		tokio::task::yield_now().await;
		assert_eq!(admission.queued.load(Ordering::Acquire), 1);
		cancel.cancel();
		assert!(matches!(
			waiting.await.expect("waiter joined"),
			Err(SupervisorError::CancelledBeforeStart)
		));
		assert_eq!(admission.queued.load(Ordering::Acquire), 0);
		drop(first);
		assert_eq!(admission.active.load(Ordering::Acquire), 0);
	}

	#[test]
	fn failed_cold_revival_returns_parked_generation_to_retryable_state() {
		let state = SubagentRunState::new(sf!("parked-child"));
		state
			.transition(SubagentLifecycle::Settled)
			.expect("settled");
		state.transition(SubagentLifecycle::Parked).expect("parked");
		state.begin_generation().expect("begin revival");
		rollback_pre_run(&state, true).expect("rollback");
		assert_eq!(state.lifecycle(), SubagentLifecycle::Parked);
		state.begin_generation().expect("retry revival");
	}
}
