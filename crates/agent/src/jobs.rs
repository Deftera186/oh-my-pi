//! Detached-job registration and authoritative settlement delivery.

use std::{
	collections::{BTreeMap, BTreeSet, btree_map::Entry},
	mem,
	sync::{Arc, Weak, atomic},
	time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use omp_core::{Duration, Str};
use omp_env::{ClientError, EnvClient, ProcessAttachmentEvent};
use omp_proto::{
	blob::v1::Chunk,
	env::v1::{
		AttachOutput, ExecStatusMsg, ListProcesses, ProcessInfo, ProcessOutput, ProcessState,
		StopProcess,
	},
	thread::v1::{self as thread, item},
};
use omp_tool::{ArtifactLifetime, JobOwner, JobRef, JobStatus};
use parking_lot::{Mutex, MutexGuard};
use serde::Serialize;
use tokio::{
	runtime,
	sync::{watch, watch::Receiver},
	task::AbortHandle,
	time,
};

use crate::{
	events::{AgentEvent, EventBus},
	mailbox::{Interrupt, InterruptClass, InterruptSource, MailboxSender},
};

const SETTLEMENT_MEDIA_TYPE: &str = "application/vnd.omp.process-settlement+json";
const UPLOAD_CHUNK_BYTES: usize = 16 * 1024;
const DEFAULT_RETENTION: StdDuration = StdDuration::from_secs(5 * 60);
const SMART_WAIT_LADDER: [StdDuration; 5] = [
	StdDuration::from_secs(5),
	StdDuration::from_secs(10),
	StdDuration::from_secs(30),
	StdDuration::from_secs(60),
	StdDuration::from_secs(300),
];
const SMART_WAIT_RESET: StdDuration = StdDuration::from_secs(60);
const DELIVERY_RETRY_BASE: StdDuration = StdDuration::from_millis(500);
const DELIVERY_RETRY_MAX: StdDuration = StdDuration::from_secs(30);
const DELIVERY_RETRY_LIMIT: u8 = 8;
const DEAD_LETTER_LIMIT: usize = 64;

/// Thread-safe registry and structural supervisor for detached jobs.
///
/// The environment remains the resource owner. Each registration starts one
/// attachment watcher; dropping the last board handle aborts every watcher.
#[derive(Clone)]
pub struct JobBoard {
	inner: Arc<JobBoardInner>,
}

struct JobBoardInner {
	env:          EnvClient,
	mailbox:      MailboxSender,
	events:       Option<EventBus>,
	pending:      Mutex<BTreeMap<Str, JobEntry>>,
	watchers:     Mutex<BTreeMap<Str, AbortHandle>>,
	settled:      Mutex<BTreeSet<Str>>,
	recent:       Mutex<BTreeMap<Str, JobRef>>,
	max_running:  usize,
	retention:    StdDuration,
	next_id:      atomic::AtomicU64,
	poll:         Mutex<BTreeMap<Str, PollState>>,
	dead_letters: Mutex<BTreeMap<Str, JobRef>>,
	generation:   watch::Sender<u64>,
}

#[derive(Clone, Copy)]
struct PollState {
	level:    usize,
	last_end: Instant,
}

struct JobEntry {
	job:               JobRef,
	settlement:        Option<thread::Item>,
	suppressions:      usize,
	leased:            bool,
	delivery_queued:   bool,
	delivery_attempts: u8,
}

impl Drop for JobBoardInner {
	fn drop(&mut self) {
		for (_, watcher) in mem::take(self.watchers.get_mut()) {
			watcher.abort();
		}
	}
}

impl JobBoard {
	/// Creates an empty board over the authoritative environment client.
	pub fn new(env: EnvClient, mailbox: MailboxSender) -> Self {
		Self::with_limits_and_events(env, mailbox, None, 15, DEFAULT_RETENTION)
	}

	/// Creates an Agent-owned board that publishes terminal job lifecycle
	/// events.
	pub(crate) fn with_events(env: EnvClient, mailbox: MailboxSender, events: EventBus) -> Self {
		Self::with_limits_and_events(env, mailbox, Some(events), 15, DEFAULT_RETENTION)
	}

	/// Creates a board with an explicit running capacity and terminal retention.
	///
	/// A capacity of zero is unlimited.
	pub fn with_limits(
		env: EnvClient,
		mailbox: MailboxSender,
		max_running: usize,
		retention: StdDuration,
	) -> Self {
		Self::with_limits_and_events(env, mailbox, None, max_running, retention)
	}

	fn with_limits_and_events(
		env: EnvClient,
		mailbox: MailboxSender,
		events: Option<EventBus>,
		max_running: usize,
		retention: StdDuration,
	) -> Self {
		Self {
			inner: Arc::new(JobBoardInner {
				env,
				mailbox,
				events,
				pending: Mutex::new(BTreeMap::new()),
				watchers: Mutex::new(BTreeMap::new()),
				settled: Mutex::new(BTreeSet::new()),
				recent: Mutex::new(BTreeMap::new()),
				max_running,
				retention,
				next_id: atomic::AtomicU64::new(1),
				poll: Mutex::new(BTreeMap::new()),
				dead_letters: Mutex::new(BTreeMap::new()),
				generation: watch::channel(0).0,
			}),
		}
	}

	/// Returns a process-local sequential job identifier.
	pub fn next_id(&self) -> Str {
		Str::from(
			self
				.inner
				.next_id
				.fetch_add(1, atomic::Ordering::Relaxed)
				.to_string(),
		)
	}

	/// Registers and starts watching one detached job.
	///
	/// Returns `true` when inserted. An exact or conflicting duplicate stable ID
	/// returns `false` without replacing the first descriptor or watcher. This
	/// method must be called from a Tokio runtime.
	pub fn register(&self, job: JobRef) -> bool {
		self.try_register(job).unwrap_or(false)
	}

	/// Registers work after atomically checking the authoritative running cap.
	pub fn try_register(&self, job: JobRef) -> Result<bool, JobAdmissionError> {
		self.register_inner(job, true)
	}

	/// Re-registers an already-running authoritative process without consuming
	/// a new admission slot.
	pub fn reattach(&self, job: JobRef) -> Result<bool, JobAdmissionError> {
		self.register_inner(job, false)
	}

	fn register_inner(
		&self,
		job: JobRef,
		enforce_capacity: bool,
	) -> Result<bool, JobAdmissionError> {
		let mut pending = self.inner.pending.lock();
		if self.inner.settled.lock().contains(job.id.as_str())
			|| self.inner.recent.lock().contains_key(job.id.as_str())
		{
			return Ok(false);
		}
		if enforce_capacity
			&& job.metadata.status == JobStatus::Running
			&& self.inner.max_running != 0
			&& pending
				.values()
				.filter(|entry| {
					entry.settlement.is_none() && entry.job.metadata.status == JobStatus::Running
				})
				.count() >= self.inner.max_running
		{
			return Err(JobAdmissionError::Capacity { limit: self.inner.max_running });
		}
		match pending.entry(job.id.clone()) {
			Entry::Vacant(entry) => {
				entry.insert(JobEntry {
					job:               job.clone(),
					settlement:        None,
					suppressions:      0,
					leased:            false,
					delivery_queued:   false,
					delivery_attempts: 0,
				});
			},
			Entry::Occupied(_) => return Ok(false),
		}

		if let JobOwner::NamedProcess { name, generation } = &job.owner {
			let name = name.clone();
			let generation = *generation;
			let id = job.id.clone();
			let registration_id = id.clone();
			let weak = Arc::downgrade(&self.inner);
			let env = self.inner.env.clone();
			let watcher = tokio::spawn(async move {
				let item = match watch_job(&env, &job, &name, generation).await {
					Ok(item) => item,
					Err(reason) => settlement_error_item(&job, &reason),
				};
				if let Some(inner) = weak.upgrade() {
					if inner.complete(&id, item).is_err() {
						schedule_delivery_retry(Arc::downgrade(&inner), id.clone());
					}
					schedule_retention(Arc::downgrade(&inner), id.clone(), inner.retention);
					inner.watchers.lock().remove(&id);
				}
			})
			.abort_handle();
			self.inner.watchers.lock().insert(registration_id, watcher);
		}
		drop(pending);
		Ok(true)
	}

	/// Settles a pending job with a caller-supplied canonical item.
	///
	/// This idempotent seam is used by authoritative settlement recovery and
	/// tests. Normal named-process settlement is produced by the board's
	/// watcher.
	pub fn settle(
		&self,
		job_id: &str,
		item: thread::Item,
	) -> Result<bool, Box<flume::TrySendError<Interrupt>>> {
		let accepted = match self.inner.complete(job_id, item) {
			Ok(accepted) => accepted,
			Err(_) => {
				schedule_delivery_retry(Arc::downgrade(&self.inner), Str::new(job_id));
				true
			},
		};
		if accepted {
			schedule_retention(Arc::downgrade(&self.inner), Str::new(job_id), self.inner.retention);
		}
		if accepted && let Some(watcher) = self.inner.watchers.lock().remove(job_id) {
			watcher.abort();
		}
		Ok(accepted)
	}

	/// Copies pending descriptors in stable job-identifier order.
	pub fn snapshot(&self) -> Vec<JobRef> {
		self.inner.prune_recent();
		let pending = self.inner.pending.lock();
		let recent = self.inner.recent.lock();
		pending
			.values()
			.map(|entry| entry.job.clone())
			.chain(recent.values().cloned())
			.collect()
	}

	/// Copies all job rows while atomically consuming each currently recoverable
	/// terminal body.
	///
	/// A body claimed here is removed from automatic delivery. Later snapshots
	/// retain the terminal row but omit its already-consumed body.
	pub fn snapshot_consuming(&self) -> Vec<JobRef> {
		self.inner.prune_recent();
		let mut pending = self.inner.pending.lock();
		let mut snapshots = Vec::with_capacity(pending.len());
		let recoverable = pending
			.iter()
			.filter_map(|(id, entry)| {
				if entry.settlement.is_some() && !entry.leased {
					Some(id.clone())
				} else {
					snapshots.push(entry.job.clone());
					None
				}
			})
			.collect::<Vec<_>>();
		for id in recoverable {
			let Some(entry) = pending.remove(&id) else {
				continue;
			};
			let mut recovered = entry.job.clone();
			if let Some(item) = entry.settlement.as_ref()
				&& let Some(body) = settlement_body(item)
			{
				let mut metadata = (*recovered.metadata).clone();
				if metadata.status == JobStatus::Failed {
					metadata.error = Some(body);
				} else {
					metadata.result = Some(body);
				}
				recovered.metadata = Arc::new(metadata);
			}
			snapshots.push(recovered);
			self.inner.finish_consumed(id, entry.job);
		}
		drop(pending);
		let seen = snapshots
			.iter()
			.map(|job| job.id.clone())
			.collect::<BTreeSet<_>>();
		let recent = self.inner.recent.lock();
		snapshots.extend(
			recent
				.values()
				.filter(|job| !seen.contains(job.id.as_str()))
				.cloned(),
		);
		snapshots.sort_by(|left, right| left.id.cmp(&right.id));
		self.inner.bump();
		snapshots
	}

	/// Acquires the queued automatic delivery immediately before its durable
	/// session insertion.
	///
	/// `None` means a foreground snapshot or another consumer already claimed
	/// the body. Dropping the returned lease retries normal delivery.
	pub fn lease_delivery(&self, job_id: &str) -> Option<JobSettlement> {
		let mut pending = self.inner.pending.lock();
		let entry = pending.get_mut(job_id)?;
		if entry.settlement.is_none() || entry.leased || !entry.delivery_queued {
			return None;
		}
		entry.leased = true;
		entry.delivery_queued = false;
		Some(JobSettlement {
			job:   entry.job.clone(),
			item:  entry.settlement.clone()?,
			lease: SettlementLease {
				inner:   Arc::downgrade(&self.inner),
				job_id:  Str::new(job_id),
				claimed: false,
			},
		})
	}

	/// Releases provisional delivery leases for this session when an
	/// authoritative process listing proves the owned generation is absent.
	pub fn release_missing_process_leases(&self, owner_session: &str, live: &BTreeSet<(Str, u64)>) {
		let mut pending = self.inner.pending.lock();
		let mut changed = false;
		for entry in pending.values_mut() {
			let JobOwner::NamedProcess { name, generation } = &entry.job.owner else {
				continue;
			};
			if entry.job.metadata.owner_session.as_deref() == Some(owner_session)
				&& !live.contains(&(name.clone(), *generation))
				&& entry.leased
			{
				entry.leased = false;
				changed = true;
			}
		}
		drop(pending);
		if changed {
			self.inner.bump();
		}
	}

	/// Moves queued work into the running state after acquiring a capacity slot.
	pub fn mark_running(&self, id: &str, started_at_ms: u64) -> Result<bool, JobAdmissionError> {
		let mut pending = self.inner.pending.lock();
		let Some(entry) = pending.get(id) else {
			return Ok(false);
		};
		if entry.job.metadata.status != JobStatus::Queued {
			return Ok(false);
		}
		if self.inner.max_running != 0
			&& pending
				.values()
				.filter(|candidate| {
					candidate.settlement.is_none() && candidate.job.metadata.status == JobStatus::Running
				})
				.count() >= self.inner.max_running
		{
			return Err(JobAdmissionError::Capacity { limit: self.inner.max_running });
		}
		let entry = pending.get_mut(id).expect("entry retained under lock");
		let mut metadata = (*entry.job.metadata).clone();
		metadata.status = JobStatus::Running;
		metadata.started_at_ms = Some(started_at_ms);
		entry.job.metadata = Arc::new(metadata);
		drop(pending);
		self.inner.bump();
		Ok(true)
	}

	/// Copies bounded terminal deliveries that exhausted their retry budget.
	pub fn dead_letters(&self) -> Vec<JobRef> {
		self.inner.dead_letters.lock().values().cloned().collect()
	}

	/// Returns whether a terminal body was delivered, recovered, or discarded
	/// and therefore must not be replayed.
	pub fn is_result_consumed(&self, id: &str) -> bool {
		self.inner.settled.lock().contains(id)
	}

	/// Suppresses automatic delivery for selected jobs until a settlement is
	/// claimed or the returned watch is dropped.
	pub fn watch(&self, ids: Option<&[Str]>) -> JobWatch {
		let mut pending = self.inner.pending.lock();
		let selected = match ids {
			Some(ids) => ids
				.iter()
				.filter(|id| pending.contains_key(id.as_str()))
				.cloned()
				.collect::<BTreeSet<_>>(),
			None => pending.keys().cloned().collect(),
		};
		for id in &selected {
			if let Some(entry) = pending.get_mut(id) {
				entry.suppressions = entry.suppressions.saturating_add(1);
			}
		}
		drop(pending);
		JobWatch {
			inner:      Arc::clone(&self.inner),
			ids:        selected,
			generation: self.inner.generation.subscribe(),
		}
	}

	/// Stops the verified named process that owns a pending job.
	pub async fn cancel(&self, id: &str, grace: Duration) -> Result<CancelOutcome, JobError> {
		let job = {
			let pending = self.inner.pending.lock();
			let Some(entry) = pending.get(id) else {
				return Ok(if self.inner.settled.lock().contains(id) {
					CancelOutcome::AlreadySettled
				} else {
					CancelOutcome::Missing
				});
			};
			if entry.settlement.is_some() {
				return Ok(CancelOutcome::AlreadySettled);
			}
			entry.job.clone()
		};
		let (name, generation) = match &job.owner {
			JobOwner::NamedProcess { name, generation } => (name, generation),
			JobOwner::AgentLoop { agent_id } => {
				return Err(JobError::AgentLoopCancellation { agent_id: agent_id.clone() });
			},
		};
		let processes = self
			.inner
			.env
			.list_processes(ListProcesses { props: None })
			.await
			.map_err(|error| JobError::Environment(Str::new(error.to_string())))?;
		let Some(process) = processes
			.processes
			.iter()
			.find(|process| process.name == name.as_str() && process.generation == *generation)
		else {
			return Ok(CancelOutcome::AlreadySettled);
		};
		if matches!(
			ProcessState::try_from(process.state),
			Ok(ProcessState::Exited | ProcessState::Stopped | ProcessState::Failed)
		) {
			return Ok(CancelOutcome::AlreadySettled);
		}
		let grace_ms = grace
			.to_std()
			.map_err(|error| JobError::InvalidGrace(Str::new(error.to_string())))?
			.as_millis()
			.try_into()
			.unwrap_or(u64::MAX);
		self
			.inner
			.env
			.stop_process(StopProcess {
				name: name.to_string(),
				grace_ms,
				generation: *generation,
				props: None,
			})
			.await
			.map_err(|error| JobError::Environment(Str::new(error.to_string())))?;
		Ok(CancelOutcome::Accepted)
	}

	/// Borrows unsettled and not-yet-consumed jobs in stable identifier order
	/// without allocating.
	pub fn pending(&self) -> PendingJobs<'_> {
		PendingJobs { guard: self.inner.pending.lock() }
	}

	/// Returns the number of unsettled or not-yet-consumed jobs.
	pub fn len(&self) -> usize {
		self.inner.pending.lock().len()
	}

	/// Returns whether no unsettled or not-yet-consumed jobs remain.
	pub fn is_empty(&self) -> bool {
		self.inner.pending.lock().is_empty()
	}

	/// Selects the next adaptive smart-wait deadline for one owner.
	pub fn next_smart_wait(&self, owner: &str) -> StdDuration {
		let now = Instant::now();
		let mut states = self.inner.poll.lock();
		match states.entry(Str::new(owner)) {
			Entry::Vacant(entry) => {
				entry.insert(PollState { level: 0, last_end: now });
				SMART_WAIT_LADDER[0]
			},
			Entry::Occupied(mut entry) => {
				let state = entry.get_mut();
				if now.duration_since(state.last_end) >= SMART_WAIT_RESET {
					state.level = 0;
				} else {
					state.level = (state.level + 1).min(SMART_WAIT_LADDER.len() - 1);
				}
				SMART_WAIT_LADDER[state.level]
			},
		}
	}

	/// Records the end of an adaptive owner wait.
	pub fn record_smart_wait_end(&self, owner: &str) {
		let mut states = self.inner.poll.lock();
		let state = states
			.entry(Str::new(owner))
			.or_insert(PollState { level: 0, last_end: Instant::now() });
		state.last_end = Instant::now();
	}

	/// Cancels owner-scoped named processes and waits through actual settlement.
	///
	/// Returned identifiers are the bounded salvage set still live at timeout.
	pub async fn cancel_and_reap_owner(
		&self,
		owner: &str,
		grace: Duration,
		timeout: StdDuration,
	) -> Vec<Str> {
		let ids = self
			.inner
			.pending
			.lock()
			.values()
			.filter(|entry| entry.settlement.is_none() && job_owner_id(&entry.job.owner) == owner)
			.map(|entry| entry.job.id.clone())
			.collect::<Vec<_>>();
		for id in &ids {
			let _ = self.cancel(id, grace).await;
		}
		if self.drain_owner(owner, Some(timeout)).await {
			Vec::new()
		} else {
			self
				.inner
				.pending
				.lock()
				.values()
				.filter(|entry| entry.settlement.is_none() && job_owner_id(&entry.job.owner) == owner)
				.map(|entry| entry.job.id.clone())
				.collect()
		}
	}

	/// Waits until no unsettled work owned by `owner` remains.
	pub async fn drain_owner(&self, owner: &str, timeout: Option<StdDuration>) -> bool {
		let wait = async {
			let mut changed = self.inner.generation.subscribe();
			loop {
				let pending =
					self.inner.pending.lock().values().any(|entry| {
						entry.settlement.is_none() && job_owner_id(&entry.job.owner) == owner
					});
				if !pending {
					return true;
				}
				if changed.changed().await.is_err() {
					return false;
				}
			}
		};
		match timeout {
			Some(timeout) => time::timeout(timeout, wait).await.unwrap_or(false),
			None => wait.await,
		}
	}
}

impl JobBoardInner {
	fn complete(
		&self,
		job_id: &str,
		item: thread::Item,
	) -> Result<bool, Box<flume::TrySendError<Interrupt>>> {
		let mut pending = self.pending.lock();
		let Some(entry) = pending.get_mut(job_id) else {
			return Ok(false);
		};
		if entry.settlement.is_some() {
			return Ok(false);
		}
		entry.settlement = Some(item);
		let mut metadata = (*entry.job.metadata).clone();
		metadata.status = JobStatus::Completed;
		metadata.settled_at_ms = Some(now_ms());
		entry.job.metadata = Arc::new(metadata);
		if let Some(events) = &self.events {
			events.publish(AgentEvent::JobSettled { job_id: Str::new(job_id) });
		}
		self.flush_locked(job_id, &mut pending)?;
		self.bump();
		Ok(true)
	}

	fn flush_locked(
		&self,
		job_id: &str,
		pending: &mut BTreeMap<Str, JobEntry>,
	) -> Result<(), Box<flume::TrySendError<Interrupt>>> {
		let Some(entry) = pending.get(job_id) else {
			return Ok(());
		};
		if entry.suppressions != 0 || entry.leased || entry.delivery_queued {
			return Ok(());
		}
		let Some(item) = entry.settlement.clone() else {
			return Ok(());
		};
		let id = entry.job.id.clone();
		self.mailbox.try_enqueue(Interrupt {
			class: InterruptClass::TurnBoundary,
			item,
			source: InterruptSource::Job { id: id.clone() },
		})?;
		pending
			.get_mut(job_id)
			.expect("queued job retained under lock")
			.delivery_queued = true;
		Ok(())
	}

	fn claim(&self, job_id: &str) -> Result<(), JobClaimError> {
		let mut pending = self.pending.lock();
		let Some(entry) = pending.get(job_id) else {
			return Err(JobClaimError::AlreadyConsumed);
		};
		if !entry.leased || entry.settlement.is_none() {
			return Err(JobClaimError::AlreadyConsumed);
		}
		let id = entry.job.id.clone();
		let completed = entry.job.clone();
		pending.remove(job_id);
		self.finish_consumed(id, completed);
		drop(pending);
		self.bump();
		Ok(())
	}

	fn finish_consumed(&self, id: Str, completed: JobRef) {
		self.settled.lock().insert(id.clone());
		if !self.retention.is_zero() {
			self.recent.lock().insert(id, completed);
		}
	}

	fn release_lease(&self, job_id: &str) {
		let mut pending = self.pending.lock();
		if let Some(entry) = pending.get_mut(job_id) {
			entry.leased = false;
			entry.delivery_queued = false;
		}
		let _ = self.flush_locked(job_id, &mut pending);
		drop(pending);
		self.bump();
	}

	fn release_watch(&self, ids: &BTreeSet<Str>) {
		let mut pending = self.pending.lock();
		for id in ids {
			if let Some(entry) = pending.get_mut(id) {
				entry.suppressions = entry.suppressions.saturating_sub(1);
			}
			let _ = self.flush_locked(id, &mut pending);
		}
		drop(pending);
		self.bump();
	}

	fn prune_recent(&self) {
		let retention_ms = self.retention.as_millis().try_into().unwrap_or(u64::MAX);
		let cutoff = now_ms().saturating_sub(retention_ms);
		self.recent.lock().retain(|_, job| {
			job.metadata
				.settled_at_ms
				.is_none_or(|settled| settled > cutoff)
		});
	}

	fn bump(&self) {
		let next = (*self.generation.borrow()).wrapping_add(1);
		self.generation.send_replace(next);
	}
}

/// Locked, allocation-free view of unsettled and not-yet-consumed jobs.
pub struct PendingJobs<'a> {
	guard: MutexGuard<'a, BTreeMap<Str, JobEntry>>,
}

impl PendingJobs<'_> {
	/// Iterates descriptors in stable job-identifier order.
	pub fn iter(&self) -> impl DoubleEndedIterator<Item = &JobRef> + ExactSizeIterator + Clone + '_ {
		self.guard.values().map(|entry| &entry.job)
	}

	/// Returns the number of jobs in this view.
	pub fn len(&self) -> usize {
		self.guard.len()
	}

	/// Returns whether this view contains no jobs.
	pub fn is_empty(&self) -> bool {
		self.guard.is_empty()
	}
}

fn job_owner_id(owner: &JobOwner) -> &str {
	match owner {
		JobOwner::NamedProcess { name, .. } => name,
		JobOwner::AgentLoop { agent_id } => agent_id,
	}
}

/// Capacity rejection from the authoritative detached-job board.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum JobAdmissionError {
	/// The configured number of active execution slots is occupied.
	#[error("background job limit reached ({limit})")]
	Capacity {
		/// Configured active-job ceiling.
		limit: usize,
	},
}

/// Result of requesting cancellation for a detached job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelOutcome {
	/// No pending or settled job has this identifier.
	Missing,
	/// The job has already produced a terminal settlement.
	AlreadySettled,
	/// The authoritative environment accepted the stop request.
	Accepted,
}

/// Failure to inspect or stop a detached job.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum JobError {
	/// Cancellation must be routed to the journal-backed agent-loop authority.
	#[error("agent loop {agent_id:?} must be cancelled through its loop authority")]
	AgentLoopCancellation {
		/// Stable agent identifier requiring cancellation.
		agent_id: Str,
	},
	/// The configured courtesy grace cannot be represented by the runtime.
	#[error("invalid job cancellation grace: {0}")]
	InvalidGrace(Str),
	/// The environment rejected a process operation.
	#[error("job process operation failed: {0}")]
	Environment(Str),
}

/// Failure to atomically consume a watched settlement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum JobClaimError {
	/// Another consumer already delivered or claimed the settlement.
	#[error("job settlement was already consumed")]
	AlreadyConsumed,
}

/// Failure while observing and recording a named-process settlement.
#[derive(Debug, thiserror::Error)]
enum JobSettlementError {
	#[error("could not attach to named process: {0}")]
	Attach(#[source] ClientError),
	#[error("named-process attachment failed: {0}")]
	Attachment(#[source] ClientError),
	#[error("could not open settlement artifact upload: {0}")]
	OpenArtifact(#[source] ClientError),
	#[error("could not commit settlement artifact: {0}")]
	CommitArtifact(#[source] ClientError),
	#[error("could not stream settlement artifact: {0}")]
	StreamArtifact(#[source] ClientError),
	#[error("could not encode settlement header: {0}")]
	EncodeHeader(#[source] serde_json::Error),
	#[error("could not encode process output: {0}")]
	EncodeOutput(#[source] serde_json::Error),
	#[error("could not encode terminal process state: {0}")]
	EncodeTerminalState(#[source] serde_json::Error),
	#[error("named-process attachment omitted acknowledgement")]
	AttachmentOmittedAcknowledgement,
	#[error("named-process attachment closed before acknowledgement")]
	AttachmentClosedBeforeAcknowledgement,
	#[error("named-process attachment closed before terminal state")]
	AttachmentClosedBeforeTerminalState,
	#[error("named-process attachment repeated acknowledgement")]
	AttachmentRepeatedAcknowledgement,
	#[error("named-process state omitted process info")]
	MissingProcessInfo,
	#[error("settlement header was not a JSON object")]
	NonObjectHeader,
	#[error(
		"named-process attachment generation mismatch: expected {name}@{expected}, got \
		 {actual_name}@{got}"
	)]
	AttachmentGenerationMismatch {
		name:        Str,
		expected:    u64,
		actual_name: Str,
		got:         u64,
	},
	#[error(
		"named-process output generation mismatch: expected {name}@{expected}, got \
		 {actual_name}@{got}"
	)]
	OutputGenerationMismatch {
		name:        Str,
		expected:    u64,
		actual_name: Str,
		got:         u64,
	},
	#[error(
		"named-process state generation mismatch: expected {name}@{expected}, got \
		 {actual_name}@{got}"
	)]
	StateGenerationMismatch {
		name:        Str,
		expected:    u64,
		actual_name: Str,
		got:         u64,
	},
}

/// One watched terminal settlement and its exclusive delivery lease.
pub struct JobSettlement {
	/// Stable detached-job descriptor.
	pub job:   JobRef,
	/// Canonical thread item produced by the settlement watcher.
	pub item:  thread::Item,
	/// Lease controlling whether normal mailbox delivery resumes.
	pub lease: SettlementLease,
}

/// Exclusive claim on one settlement held outside the board lock.
#[must_use]
pub struct SettlementLease {
	inner:   Weak<JobBoardInner>,
	job_id:  Str,
	claimed: bool,
}

impl SettlementLease {
	/// Atomically consumes the settlement without mailbox auto-delivery.
	pub fn claim(mut self) -> Result<(), JobClaimError> {
		let inner = self.inner.upgrade().ok_or(JobClaimError::AlreadyConsumed)?;
		inner.claim(self.job_id.as_str())?;
		self.claimed = true;
		Ok(())
	}
}

impl Drop for SettlementLease {
	fn drop(&mut self) {
		if !self.claimed
			&& let Some(inner) = self.inner.upgrade()
		{
			inner.release_lease(self.job_id.as_str());
		}
	}
}

/// Settlement subscription which temporarily suppresses normal delivery.
#[must_use]
pub struct JobWatch {
	inner:      Arc<JobBoardInner>,
	ids:        BTreeSet<Str>,
	generation: Receiver<u64>,
}

impl JobWatch {
	/// Returns whether no selected pending job remains.
	pub fn is_empty(&self) -> bool {
		self.ids.is_empty()
	}

	/// Waits for the next selected settlement, retaining unrelated jobs.
	pub async fn next(&mut self) -> Option<JobSettlement> {
		loop {
			let selected = {
				let mut pending = self.inner.pending.lock();
				let id = self.ids.iter().find_map(|id| {
					pending
						.get(id)
						.filter(|entry| entry.settlement.is_some() && !entry.leased)
						.map(|_| id.clone())
				});
				id.and_then(|id| {
					let entry = pending.get_mut(&id)?;
					entry.leased = true;
					entry.delivery_queued = false;
					entry.suppressions = entry.suppressions.saturating_sub(1);
					Some((id, entry.job.clone(), entry.settlement.clone()?))
				})
			};
			if let Some((id, job, item)) = selected {
				self.ids.remove(&id);
				return Some(JobSettlement {
					job,
					item,
					lease: SettlementLease {
						inner:   Arc::downgrade(&self.inner),
						job_id:  id,
						claimed: false,
					},
				});
			}
			self
				.ids
				.retain(|id| self.inner.pending.lock().contains_key(id));
			if self.ids.is_empty() || self.generation.changed().await.is_err() {
				return None;
			}
		}
	}
}

impl Drop for JobWatch {
	fn drop(&mut self) {
		self.inner.release_watch(&self.ids);
	}
}

fn schedule_retention(inner: Weak<JobBoardInner>, job_id: Str, retention: StdDuration) {
	if retention.is_zero() {
		if let Some(inner) = inner.upgrade() {
			inner.recent.lock().remove(&job_id);
		}
		return;
	}
	let Ok(runtime) = runtime::Handle::try_current() else {
		return;
	};
	runtime.spawn(async move {
		time::sleep(retention).await;
		let Some(inner) = inner.upgrade() else { return };
		inner.recent.lock().remove(&job_id);
		let removed = {
			let mut pending = inner.pending.lock();
			pending
				.get(&job_id)
				.is_some_and(|entry| entry.settlement.is_some() && !entry.leased)
				.then(|| pending.remove(&job_id))
				.flatten()
		};
		if removed.is_some() {
			inner.settled.lock().insert(job_id);
			inner.bump();
		}
	});
}

fn schedule_delivery_retry(inner: Weak<JobBoardInner>, job_id: Str) {
	let Some(inner) = inner.upgrade() else { return };
	let attempt = {
		let mut pending = inner.pending.lock();
		let Some(entry) = pending.get_mut(&job_id) else {
			return;
		};
		if entry.suppressions != 0
			|| entry.leased
			|| entry.delivery_queued
			|| entry.settlement.is_none()
		{
			return;
		}
		if entry.delivery_attempts >= DELIVERY_RETRY_LIMIT {
			let job = entry.job.clone();
			pending.remove(&job_id);
			inner.settled.lock().insert(job_id.clone());
			if !inner.retention.is_zero() {
				inner.recent.lock().insert(job_id.clone(), job.clone());
			}
			let mut dead = inner.dead_letters.lock();
			dead.insert(job_id.clone(), job);
			while dead.len() > DEAD_LETTER_LIMIT {
				if let Some(oldest) = dead.keys().next().cloned() {
					dead.remove(&oldest);
				}
			}
			inner.bump();
			return;
		}
		entry.delivery_attempts += 1;
		entry.delivery_attempts
	};
	let shift = u32::from(attempt.saturating_sub(1)).min(16);
	let exponential = DELIVERY_RETRY_BASE
		.checked_mul(1_u32 << shift)
		.unwrap_or(DELIVERY_RETRY_MAX)
		.min(DELIVERY_RETRY_MAX);
	let jitter = StdDuration::from_millis(
		job_id
			.as_bytes()
			.iter()
			.fold(0_u64, |sum, byte| sum.wrapping_add(u64::from(*byte)))
			% 200,
	);
	let weak = Arc::downgrade(&inner);
	tokio::spawn(async move {
		time::sleep(exponential.saturating_add(jitter)).await;
		let Some(inner) = weak.upgrade() else { return };
		let mut pending = inner.pending.lock();
		if inner.flush_locked(&job_id, &mut pending).is_err() {
			drop(pending);
			schedule_delivery_retry(Arc::downgrade(&inner), job_id);
		}
	});
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

async fn watch_job(
	env: &EnvClient,
	job: &JobRef,
	name: &Str,
	generation: u64,
) -> Result<thread::Item, JobSettlementError> {
	let mut attachment = env
		.attach_output(AttachOutput {
			name: name.to_string(),
			after_sequence: 0,
			generation,
			max_bytes: 16 * 1024 * 1024,
			terminal_text: false,
			terminal_columns: 0,
			terminal_rows: 0,
			props: None,
		})
		.await
		.map_err(JobSettlementError::Attach)?;
	let attached = match attachment
		.next_event()
		.await
		.map_err(JobSettlementError::Attachment)?
	{
		Some(ProcessAttachmentEvent::Attached(attached)) => attached,
		Some(_) => return Err(JobSettlementError::AttachmentOmittedAcknowledgement),
		None => return Err(JobSettlementError::AttachmentClosedBeforeAcknowledgement),
	};
	if attached.name != name.as_str() || attached.generation != generation {
		return Err(JobSettlementError::AttachmentGenerationMismatch {
			name:        name.clone(),
			expected:    generation,
			actual_name: Str::from(attached.name),
			got:         attached.generation,
		});
	}

	let upload = env.blob_put().map_err(JobSettlementError::OpenArtifact)?;
	let mut header = serde_json::to_vec(&ArtifactHeader {
		job_id:            job.id.as_str(),
		owner:             OwnerRecord { name: name.as_str(), generation },
		expected_artifact: ExpectedArtifactRecord {
			description: job.artifact.description.as_str(),
			media_type:  job.artifact.media_type.as_deref(),
			lifetime:    job.artifact.lifetime,
		},
	})
	.map_err(JobSettlementError::EncodeHeader)?;
	if header.pop() != Some(b'}') {
		return Err(JobSettlementError::NonObjectHeader);
	}
	header.extend_from_slice(b",\"output\":[");
	upload_bytes(&upload, &header).await?;
	let mut first_output = true;

	loop {
		let event = attachment
			.next_event()
			.await
			.map_err(JobSettlementError::Attachment)?
			.ok_or(JobSettlementError::AttachmentClosedBeforeTerminalState)?;
		match event {
			ProcessAttachmentEvent::Attached(_) => {
				return Err(JobSettlementError::AttachmentRepeatedAcknowledgement);
			},
			ProcessAttachmentEvent::Output(output) => {
				validate_output(&output, name, generation)?;
				let mut encoded = serde_json::to_vec(&OutputRecord {
					sequence: output.sequence,
					channel:  output.channel,
					data:     &output.data,
				})
				.map_err(JobSettlementError::EncodeOutput)?;
				if !first_output {
					encoded.insert(0, b',');
				}
				first_output = false;
				upload_bytes(&upload, &encoded).await?;
			},
			ProcessAttachmentEvent::State(state) => {
				let info = state
					.process
					.ok_or(JobSettlementError::MissingProcessInfo)?;
				validate_state(&info, name, generation)?;
				if terminal_state(&info) {
					return finish_settlement(upload, job, info).await;
				}
			},
		}
	}
}

async fn finish_settlement(
	upload: omp_env::BlobUpload,
	job: &JobRef,
	info: ProcessInfo,
) -> Result<thread::Item, JobSettlementError> {
	let mut suffix = Vec::from(&b"],\"state\":"[..]);
	serde_json::to_writer(&mut suffix, &StateRecord::from(&info))
		.map_err(JobSettlementError::EncodeTerminalState)?;
	suffix.push(b'}');
	upload_bytes(&upload, &suffix).await?;
	let stored = upload
		.commit()
		.await
		.map_err(JobSettlementError::CommitArtifact)?;
	let state = ProcessState::try_from(info.state)
		.map_or_else(|_| format!("state {}", info.state), |state| format!("{state:?}"));
	let text = format!("Detached job {} settled: {}.", job.id, state.to_lowercase());
	let mime = SETTLEMENT_MEDIA_TYPE.to_owned();
	Ok(system_item(vec![
		thread::Part { kind: Some(thread::part::Kind::Text(text)) },
		thread::Part {
			kind: Some(thread::part::Kind::Blob(thread::Blob {
				hash: stored.hash,
				mime,
				size: stored.size,
				inline: Bytes::new(),
				detail: thread::blob::Detail::Auto as i32,
			})),
		},
	]))
}

async fn upload_bytes(
	upload: &omp_env::BlobUpload,
	bytes: &[u8],
) -> Result<(), JobSettlementError> {
	for data in bytes.chunks(UPLOAD_CHUNK_BYTES) {
		upload
			.send_chunk(Chunk { data: Bytes::copy_from_slice(data), hash: Bytes::new(), size: None })
			.await
			.map_err(JobSettlementError::StreamArtifact)?;
	}
	Ok(())
}

fn validate_output(
	output: &ProcessOutput,
	name: &str,
	generation: u64,
) -> Result<(), JobSettlementError> {
	if output.name == name && output.generation == generation {
		Ok(())
	} else {
		Err(JobSettlementError::OutputGenerationMismatch {
			name:        Str::from(name),
			expected:    generation,
			actual_name: Str::from(output.name.as_str()),
			got:         output.generation,
		})
	}
}

fn validate_state(
	info: &ProcessInfo,
	name: &str,
	generation: u64,
) -> Result<(), JobSettlementError> {
	if info.name == name && info.generation == generation {
		Ok(())
	} else {
		Err(JobSettlementError::StateGenerationMismatch {
			name:        Str::from(name),
			expected:    generation,
			actual_name: Str::from(info.name.as_str()),
			got:         info.generation,
		})
	}
}

fn terminal_state(info: &ProcessInfo) -> bool {
	matches!(
		ProcessState::try_from(info.state).ok(),
		Some(ProcessState::Exited | ProcessState::Stopped | ProcessState::Failed)
	)
}

fn settlement_error_item(job: &JobRef, reason: &JobSettlementError) -> thread::Item {
	system_item(vec![thread::Part {
		kind: Some(thread::part::Kind::Text(format!(
			"Detached job {} could not be observed to settlement: {reason}",
			job.id
		))),
	}])
}
fn settlement_body(item: &thread::Item) -> Option<Str> {
	let parts = match item.kind.as_ref()? {
		item::Kind::Message(message) => message.parts.as_slice(),
		item::Kind::ToolResult(result) => result.parts.as_slice(),
		item::Kind::ToolCall(_) => return None,
	};
	let mut body = String::new();
	for text in parts.iter().filter_map(|part| match part.kind.as_ref() {
		Some(thread::part::Kind::Text(text)) => Some(text.as_str()),
		_ => None,
	}) {
		if !body.is_empty() {
			body.push('\n');
		}
		body.push_str(text);
	}
	(!body.is_empty()).then(|| Str::from(body))
}

const fn system_item(parts: Vec<thread::Part>) -> thread::Item {
	thread::Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(item::Kind::Message(thread::Message {
			role: thread::Role::System as i32,
			parts,
		})),
		props:         None,
	}
}

#[derive(Serialize)]
struct ArtifactHeader<'a> {
	job_id:            &'a str,
	owner:             OwnerRecord<'a>,
	expected_artifact: ExpectedArtifactRecord<'a>,
}

#[derive(Serialize)]
struct OwnerRecord<'a> {
	name:       &'a str,
	generation: u64,
}

#[derive(Serialize)]
struct ExpectedArtifactRecord<'a> {
	description: &'a str,
	media_type:  Option<&'a str>,
	lifetime:    ArtifactLifetime,
}

#[derive(Serialize)]
struct OutputRecord<'a> {
	sequence: u64,
	channel:  i32,
	data:     &'a [u8],
}

#[derive(Serialize)]
struct StateRecord<'a> {
	state:  i32,
	status: Option<StatusRecord<'a>>,
}

impl<'a> From<&'a ProcessInfo> for StateRecord<'a> {
	fn from(info: &'a ProcessInfo) -> Self {
		Self { state: info.state, status: info.status.as_ref().map(StatusRecord::from) }
	}
}

#[derive(Serialize)]
struct StatusRecord<'a> {
	outcome:       i32,
	exit_code:     Option<i32>,
	signal:        &'a str,
	wall_clock_ms: u64,
	aborted:       bool,
}

impl<'a> From<&'a ExecStatusMsg> for StatusRecord<'a> {
	fn from(status: &'a ExecStatusMsg) -> Self {
		Self {
			outcome:       status.outcome,
			exit_code:     status.exit_code,
			signal:        status.signal.as_str(),
			wall_clock_ms: status.wall_clock_ms,
			aborted:       status.aborted,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{
		sync,
		sync::atomic::{AtomicUsize, Ordering},
		thread as std_thread,
	};

	use omp_core::sf;
	use omp_tool::{ArtifactLifetime, ExpectedArtifact};

	use super::*;
	use crate::mailbox::{DrainPoint, Mailbox};

	fn job(id: &str, lifetime: ArtifactLifetime) -> JobRef {
		JobRef {
			id:       Str::new(id),
			owner:    JobOwner::NamedProcess { name: Str::new(id), generation: 1 },
			metadata: sync::Arc::default(),
			artifact: ExpectedArtifact {
				description: sf!("detached output"),
				media_type: None,
				lifetime,
			},
		}
	}

	#[tokio::test]
	async fn pending_view_is_stable_and_duplicates_preserve_the_first_descriptor() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::new(env, mailbox.sender());
		assert!(board.register(job("job-b", ArtifactLifetime::Durable)));
		assert!(board.register(job("job-a", ArtifactLifetime::Session)));
		assert!(!board.register(job("job-a", ArtifactLifetime::Ephemeral)));

		let pending = board.pending();
		assert_eq!(pending.len(), 2);
		let mut jobs = pending.iter();
		assert_eq!(jobs.next().unwrap().id, "job-a");
		assert_eq!(jobs.next().unwrap().id, "job-b");
		assert_eq!(jobs.next(), None);
		assert_eq!(pending.iter().next().unwrap().artifact.lifetime, ArtifactLifetime::Session);
	}

	#[tokio::test]
	async fn agent_board_publishes_terminal_transition_exactly_once() {
		let mailbox = Mailbox::new();
		let events = EventBus::new();
		let subscription = events.subscribe_lossless();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::with_events(env, mailbox.sender(), events);
		assert!(board.register(job("job-event", ArtifactLifetime::Session)));
		assert!(board.settle("job-event", thread::Item::default()).unwrap());
		assert!(!board.settle("job-event", thread::Item::default()).unwrap());
		let event = subscription.try_recv().expect("terminal event");
		assert!(matches!(
			event.as_ref(),
			AgentEvent::JobSettled { job_id } if job_id == "job-event"
		));
		assert!(subscription.try_recv().is_err(), "terminal event is exactly once");
	}

	#[tokio::test]
	async fn concurrent_settlement_enqueues_once_and_retains_until_commit() {
		let mut mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::new(env, mailbox.sender());
		assert!(board.register(job("job-1", ArtifactLifetime::Session)));
		assert!(!board.settle("unknown", thread::Item::default()).unwrap());
		let settled = AtomicUsize::new(0);
		std_thread::scope(|scope| {
			for seq in 0..8 {
				let board = &board;
				let settled = &settled;
				scope.spawn(move || {
					if board
						.settle("job-1", thread::Item { seq, ..thread::Item::default() })
						.unwrap()
					{
						settled.fetch_add(1, Ordering::Relaxed);
					}
				});
			}
		});

		assert_eq!(settled.load(Ordering::Relaxed), 1);
		assert!(!board.is_empty(), "queued delivery remains recoverable before commit");
		assert_eq!(mailbox.len(), 1);
		let interrupts = mailbox.drain(DrainPoint::TurnBoundary, false);
		assert_eq!(interrupts.len(), 1);
		assert_eq!(interrupts[0].class, InterruptClass::TurnBoundary);
		assert_eq!(interrupts[0].source, InterruptSource::Job { id: sf!("job-1") });
		let settlement = board
			.lease_delivery("job-1")
			.expect("queued delivery lease");
		settlement.lease.claim().expect("committed delivery");
		assert!(board.is_empty());
		assert!(!board.settle("job-1", thread::Item::default()).unwrap());
		assert!(mailbox.is_empty());
	}

	#[tokio::test]
	async fn later_deliveries_enqueue_while_an_earlier_receipt_is_pending() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::new(env, mailbox.sender());
		assert!(board.register(job("first", ArtifactLifetime::Session)));
		assert!(board.register(job("second", ArtifactLifetime::Session)));

		assert!(board.settle("first", thread::Item::default()).unwrap());
		assert!(board.settle("second", thread::Item::default()).unwrap());

		assert_eq!(mailbox.len(), 2);
		assert_eq!(board.snapshot().len(), 2);
	}

	#[tokio::test]
	async fn capacity_counts_only_running_and_zero_is_unlimited() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::with_limits(env, mailbox.sender(), 1, DEFAULT_RETENTION);
		assert!(
			board
				.try_register(job("running", ArtifactLifetime::Session))
				.unwrap()
		);
		assert_eq!(
			board.try_register(job("denied", ArtifactLifetime::Session)),
			Err(JobAdmissionError::Capacity { limit: 1 })
		);
		let mut queued = job("queued", ArtifactLifetime::Session);
		let mut metadata = omp_tool::JobMetadata::default();
		metadata.status = JobStatus::Queued;
		queued.metadata = Arc::new(metadata);
		assert!(board.try_register(queued).unwrap());
		assert_eq!(board.mark_running("queued", 10), Err(JobAdmissionError::Capacity { limit: 1 }));

		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let unlimited = JobBoard::with_limits(env, mailbox.sender(), 0, DEFAULT_RETENTION);
		for id in 0..32 {
			assert!(
				unlimited
					.try_register(job(&format!("task-{id}"), ArtifactLifetime::Session))
					.unwrap()
			);
		}
	}

	#[tokio::test]
	async fn consuming_snapshot_returns_a_result_body_at_most_once() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::new(env, mailbox.sender());
		assert!(board.register(job("once", ArtifactLifetime::Session)));
		assert!(
			board
				.settle(
					"once",
					system_item(vec![thread::Part {
						kind: Some(thread::part::Kind::Text("complete body".to_owned())),
					}]),
				)
				.unwrap()
		);

		let first = board.snapshot_consuming();
		assert_eq!(first.len(), 1);
		assert_eq!(first[0].metadata.result.as_deref(), Some("complete body"));
		assert!(board.is_result_consumed("once"));
		let second = board.snapshot_consuming();
		assert_eq!(second.len(), 1);
		assert_eq!(second[0].metadata.result, None);
		assert_eq!(second[0].metadata.error, None);
		assert!(
			board.lease_delivery("once").is_none(),
			"stale queued injection cannot replay a recovered body"
		);
	}

	#[tokio::test]
	async fn smart_wait_climbs_and_retained_terminal_rows_expire() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::with_limits(env, mailbox.sender(), 1, StdDuration::from_millis(20));
		assert_eq!(board.next_smart_wait("owner"), StdDuration::from_secs(5));
		board.record_smart_wait_end("owner");
		assert_eq!(board.next_smart_wait("owner"), StdDuration::from_secs(10));
		assert!(board.register(job("recent", ArtifactLifetime::Session)));
		assert!(board.settle("recent", thread::Item::default()).unwrap());
		assert_eq!(board.snapshot().len(), 1);
		time::sleep(StdDuration::from_millis(30)).await;
		assert!(board.snapshot().is_empty());
	}
}

#[cfg(test)]
mod watch_tests {
	use std::sync;

	use omp_core::sf;
	use omp_tool::{ArtifactLifetime, ExpectedArtifact};

	use super::*;
	use crate::mailbox::Mailbox;

	fn watched_job(id: &str) -> JobRef {
		JobRef {
			id:       Str::new(id),
			owner:    JobOwner::NamedProcess { name: Str::new(id), generation: 1 },
			metadata: sync::Arc::default(),
			artifact: ExpectedArtifact {
				description: sf!("detached output"),
				media_type:  None,
				lifetime:    ArtifactLifetime::Session,
			},
		}
	}

	#[tokio::test]
	async fn claimed_watch_settlement_suppresses_mailbox_delivery() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::new(env, mailbox.sender());
		assert!(board.register(watched_job("claimed")));
		let mut watch = board.watch(None);
		assert!(board.settle("claimed", thread::Item::default()).unwrap());
		assert!(mailbox.is_empty());
		let settlement = watch.next().await.expect("watched settlement");
		settlement.lease.claim().expect("exclusive claim");
		assert!(board.is_empty());
		assert!(mailbox.is_empty());
	}

	#[tokio::test]
	async fn dropping_watch_resumes_normal_delivery() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::new(env, mailbox.sender());
		assert!(board.register(watched_job("released")));
		let watch = board.watch(None);
		assert!(board.settle("released", thread::Item::default()).unwrap());
		assert!(mailbox.is_empty());
		drop(watch);
		assert_eq!(mailbox.len(), 1);
		assert!(!board.is_empty(), "mailbox receipt is not a committed delivery");
		let settlement = board
			.lease_delivery("released")
			.expect("queued delivery lease");
		settlement.lease.claim().expect("commit queued delivery");
		assert!(board.is_empty());
	}
}
