//! Goal-directed swarm orchestration exposed as one dynamic device.

use std::{
	collections::{BTreeMap, VecDeque},
	error,
	fmt::{self, Display},
	sync::{
		Arc, LazyLock, Weak,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use omp_agent::{AgentStatus, RegistryStatus};
use omp_core::{Str, sf};
use omp_envd::eval::ParentSessionHost as _;
use omp_proto::thread::v1::{Item, Message, Part as ThreadPart, Role, item};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, ArtifactLifetime, CommitError, Constraint, Effects, Ev,
	ExpectedArtifact, IncomingParams, JobKind, JobMetadata, JobOwner, JobRef, ParamError, Part,
	PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use parking_lot::{Mutex, RwLock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{sync::Notify, time, time::MissedTickBehavior};

use crate::{chat::ChatParentHost, modes::RegimeHandle};

/// A worker requested in a vibe-mode spawn wave.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveEntry {
	/// Complete delegated brief.
	pub brief: Str,
	/// Optional roster label.
	pub label: Option<Str>,
	/// Worker tier: `fast` selects sonic; `good` selects task.
	#[serde(default)]
	pub tier:  WorkerTier,
}

/// Worker tier available to a vibe spawn wave.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTier {
	/// Mechanical, low-reasoning work.
	Fast,
	/// General-purpose implementation and analysis.
	#[default]
	Good,
}

/// The five operations accepted by the single vibe device.
/// One completed action in a worker's current-turn activity trace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VibeActivityTrace {
	/// Epoch-millisecond activity time.
	pub at_ms:   u64,
	/// One-line activity label.
	pub summary: Str,
}

/// Live per-worker screen projected into TV-wall presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VibeScreenSnapshot {
	/// Stable worker id.
	pub id:               Str,
	/// Roster label.
	pub label:            Str,
	/// Worker tier.
	pub tier:             WorkerTier,
	/// Lifecycle state.
	pub state:            Str,
	/// Completed turns.
	pub turns:            u64,
	/// In-flight turn start.
	pub turn_started_ms:  Option<u64>,
	/// Gist of the message starting the current turn.
	pub turn_message:     Option<Str>,
	/// Current tool label.
	pub current_tool:     Option<Str>,
	/// Oldest-to-newest bounded activity tail.
	pub trace:            Arc<[VibeActivityTrace]>,
	/// Oldest-to-newest streamed output tail.
	pub output_tail:      Arc<[Str]>,
	/// Latest activity time.
	pub last_activity_ms: u64,
	/// Output tokens attributed to this worker.
	pub output_tokens:    u64,
}

/// One dynamically sized TV-wall cell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VibeTvWallFrame {
	/// Worker shown by this cell.
	pub screen: Str,
	/// Zero-based wall row.
	pub row:    u16,
	/// Zero-based wall column.
	pub column: u16,
	/// Width allocated to this cell.
	pub width:  u16,
}

/// Aggregate monitor projection for the current swarm.
#[derive(Clone, Debug, PartialEq)]
pub struct VibeMonitorSnapshot {
	/// Spawn-order live screens.
	pub screens:           Arc<[VibeScreenSnapshot]>,
	/// Aggregate output throughput since monitoring began.
	pub tokens_per_second: f64,
}

#[derive(Clone, Debug)]
struct LiveVibeScreen {
	snapshot:    VibeScreenSnapshot,
	trace:       VecDeque<VibeActivityTrace>,
	output_tail: VecDeque<Str>,
}

/// Bounded swarm monitor used by retained status and TV-wall surfaces.
#[derive(Debug)]
pub struct VibeSwarmMonitor {
	screens: BTreeMap<Str, LiveVibeScreen>,
	order:   Vec<Str>,
	started: Instant,
}

impl VibeSwarmMonitor {
	const OUTPUT_CAP: usize = 12;
	const TRACE_CAP: usize = 40;

	/// Creates an empty monitor.
	pub fn new() -> Self {
		Self { screens: BTreeMap::new(), order: Vec::new(), started: Instant::now() }
	}

	/// Registers or restarts one worker screen.
	pub fn begin_turn(&mut self, id: Str, label: Str, tier: WorkerTier, message: &str) {
		let now = now_ms();
		let screen = self.screens.entry(id.clone()).or_insert_with(|| {
			self.order.push(id.clone());
			LiveVibeScreen {
				snapshot:    VibeScreenSnapshot {
					id,
					label,
					tier,
					state: sf!("running"),
					turns: 0,
					turn_started_ms: Some(now),
					turn_message: Some(one_line(message, 120)),
					current_tool: None,
					trace: Arc::from([]),
					output_tail: Arc::from([]),
					last_activity_ms: now,
					output_tokens: 0,
				},
				trace:       VecDeque::new(),
				output_tail: VecDeque::new(),
			}
		});
		screen.snapshot.state = sf!("running");
		screen.snapshot.turn_started_ms = Some(now);
		screen.snapshot.turn_message = Some(one_line(message, 120));
		screen.snapshot.last_activity_ms = now;
		screen
			.trace
			.push_back(VibeActivityTrace { at_ms: now, summary: sf!("turn started") });
	}

	/// Records one tool or lifecycle activity.
	pub fn record_activity(&mut self, id: &str, summary: &str) {
		let Some(screen) = self.screens.get_mut(id) else {
			return;
		};
		let at_ms = now_ms();
		screen
			.trace
			.push_back(VibeActivityTrace { at_ms, summary: one_line(summary, 120) });
		while screen.trace.len() > Self::TRACE_CAP {
			screen.trace.pop_front();
		}
		screen.snapshot.last_activity_ms = at_ms;
	}

	/// Replaces the current tool label without formatting during paint.
	pub fn set_current_tool(&mut self, id: &str, tool: Option<Str>) {
		let Some(screen) = self.screens.get_mut(id) else {
			return;
		};
		screen.snapshot.current_tool = tool;
		screen.snapshot.last_activity_ms = now_ms();
	}

	/// Appends a sanitized streamed output line.
	pub fn push_output(&mut self, id: &str, text: &str) {
		let Some(screen) = self.screens.get_mut(id) else {
			return;
		};
		for line in text.lines().filter(|line| !line.trim().is_empty()) {
			screen.output_tail.push_back(one_line(line, 120));
			while screen.output_tail.len() > Self::OUTPUT_CAP {
				screen.output_tail.pop_front();
			}
		}
		screen.snapshot.last_activity_ms = now_ms();
	}

	/// Adds output-token usage for aggregate throughput.
	pub fn record_usage(&mut self, id: &str, output_tokens: u64) {
		if let Some(screen) = self.screens.get_mut(id) {
			screen.snapshot.output_tokens =
				screen.snapshot.output_tokens.saturating_add(output_tokens);
		}
	}

	/// Settles one worker screen.
	pub fn settle(&mut self, id: &str, failed: bool) {
		let Some(screen) = self.screens.get_mut(id) else {
			return;
		};
		screen.snapshot.state = if failed { sf!("failed") } else { sf!("idle") };
		screen.snapshot.turn_started_ms = None;
		screen.snapshot.current_tool = None;
		screen.snapshot.turns = screen.snapshot.turns.saturating_add(1);
		screen.snapshot.last_activity_ms = now_ms();
	}

	/// Returns a spawn-order snapshot and aggregate output throughput.
	pub fn snapshot(&self, now: Instant) -> VibeMonitorSnapshot {
		let screens = self
			.order
			.iter()
			.filter_map(|id| self.screens.get(id))
			.map(|live| {
				let mut screen = live.snapshot.clone();
				screen.trace = live.trace.iter().cloned().collect::<Vec<_>>().into();
				screen.output_tail = live.output_tail.iter().cloned().collect::<Vec<_>>().into();
				screen
			})
			.collect::<Vec<_>>();
		let tokens = screens
			.iter()
			.map(|screen| screen.output_tokens)
			.sum::<u64>();
		let elapsed = now.saturating_duration_since(self.started).as_secs_f64();
		VibeMonitorSnapshot {
			screens:           screens.into(),
			tokens_per_second: if elapsed > 0.0 {
				tokens as f64 / elapsed
			} else {
				0.0
			},
		}
	}

	/// Computes dynamic TV-wall cells for the supplied viewport width.
	pub fn tv_wall_frames(&self, viewport_width: u16) -> Vec<VibeTvWallFrame> {
		let count = self.order.len();
		if count == 0 {
			return Vec::new();
		}
		let columns = ((count as f64).sqrt().ceil() as u16)
			.min(count as u16)
			.max(1);
		let width = (viewport_width / columns).max(1);
		self
			.order
			.iter()
			.enumerate()
			.map(|(index, id)| VibeTvWallFrame {
				screen: id.clone(),
				row: u16::try_from(index).unwrap_or(u16::MAX) / columns,
				column: u16::try_from(index).unwrap_or(u16::MAX) % columns,
				width,
			})
			.collect()
	}
}

impl Default for VibeSwarmMonitor {
	fn default() -> Self {
		Self::new()
	}
}

fn one_line(text: &str, max_chars: usize) -> Str {
	let mut output = text.split_whitespace().collect::<Vec<_>>().join(" ");
	let cutoff = output.char_indices().nth(max_chars).map(|(index, _)| index);
	if let Some(cutoff) = cutoff {
		output.truncate(cutoff);
	}
	Str::new(output)
}

/// The five operations accepted by the single vibe device.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
	/// Launch one concurrent worker wave.
	Spawn,
	/// Inspect worker lifecycle state.
	Status,
	/// Deliver a steering message to a running worker.
	Steer,
	/// Wait for and return worker results.
	Collect,
	/// Cancel running workers.
	Stop,
}

/// Arguments accepted by the vibe device.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// Operation to execute.
	pub op:         Operation,
	/// Worker briefs for `spawn`.
	#[serde(default)]
	pub wave:       Vec<WaveEntry>,
	/// Worker identifiers for `status`, `collect`, or `stop`; empty means all.
	#[serde(default)]
	pub ids:        Vec<Str>,
	/// One worker identifier for `steer`.
	pub id:         Option<Str>,
	/// Steering text for `steer`.
	pub message:    Option<Str>,
	/// Maximum wait for `collect`; omitted waits until every selected worker
	/// settles.
	pub timeout_ms: Option<u64>,
}

impl Params {
	fn validate(&self) -> Result<(), Fault> {
		match self.op {
			Operation::Spawn if self.wave.is_empty() => {
				Err(Fault::new("spawn requires a non-empty wave"))
			},
			Operation::Spawn
				if self
					.wave
					.iter()
					.any(|worker| worker.brief.trim().is_empty()) =>
			{
				Err(Fault::new("worker briefs must not be empty"))
			},
			Operation::Steer if self.id.as_ref().is_none_or(|id| id.trim().is_empty()) => {
				Err(Fault::new("steer requires id"))
			},
			Operation::Steer
				if self
					.message
					.as_ref()
					.is_none_or(|text| text.trim().is_empty()) =>
			{
				Err(Fault::new("steer requires a non-empty message"))
			},
			_ => Ok(()),
		}
	}
}

/// JSON result returned by the device.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Payload {
	/// Structured operation result.
	pub result: Value,
}

/// Vibe operations do not stream intermediate updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// A rejected or failed vibe operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fault {
	message: Str,
}

impl Fault {
	fn new(message: impl Into<Str>) -> Self {
		Self { message: message.into() }
	}
}

impl Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}

impl error::Error for Fault {}

#[async_trait]
trait VibeBackend: Send + Sync {
	async fn execute(&self, params: Params) -> Result<Value, Fault>;
}

static BACKEND: LazyLock<RwLock<Option<Arc<dyn VibeBackend>>>> =
	LazyLock::new(|| RwLock::new(None));

/// Restores the preceding chat-scoped vibe backend when dropped.
#[must_use]
pub struct Attachment {
	previous: Option<Arc<dyn VibeBackend>>,
}

impl Drop for Attachment {
	fn drop(&mut self) {
		*BACKEND.write() = self.previous.take();
	}
}

fn attach(backend: Arc<dyn VibeBackend>) -> Attachment {
	let previous = BACKEND.write().replace(backend);
	Attachment { previous }
}

/// The native implementation mounted under the dynamic-device catalog.
pub struct Vibe {
	spec: ToolSpec,
}

/// Creates the single five-verb vibe device.
pub fn tool() -> Vibe {
	Vibe {
		spec: ToolSpec {
			name:            sf!("vibe"),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(
				"Runs a goal-directed worker swarm through one device. Use op=spawn with a wave of \
				 briefs, op=status to inspect workers, op=steer with id/message, op=collect to return \
				 settled results, and op=stop to cancel workers. ids omitted means all workers in \
				 this wave.",
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects { subagents: u32::MAX, ..Effects::empty() },
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("vibe.rs"),
			)
			.into_bytes(),
		},
	}
}

impl Tool for Vibe {
	type Fault = Fault;
	type Params = Params;
	type Payload = Payload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, Payload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<Params>().await {
				Ok(value) => value,
				Err(error) => { yield param_event(error); return; }
			};
			if let Err(error) = params.validate() {
				yield Ev::Done(ToolTerminal::Done { result: Err(error), useless: true });
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			let Some(backend) = BACKEND.read().clone() else {
				yield Ev::Done(ToolTerminal::Done {
					result: Err(Fault::new("vibe is unavailable outside an attached chat session")),
					useless: true,
				});
				return;
			};
			match backend.execute(params).await {
				Ok(result) => yield Ev::Done(ToolTerminal::Done { result: Ok(Payload { result }), useless: false }),
				Err(error) => yield Ev::Done(ToolTerminal::Done { result: Err(error), useless: false }),
			}
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		let text = match view {
			Ok(payload) => serde_json::to_string_pretty(&payload.result)
				.unwrap_or_else(|_| "{\"error\":\"vibe result serialization failed\"}".to_owned()),
			Err(fault) => fault.to_string(),
		};
		vec![Part::Text { text: Str::from(text) }]
	}
}

#[derive(Clone)]
enum WorkerOutcome {
	Done(Value),
	Failed(Str),
}

struct Worker {
	label:      Str,
	tier:       WorkerTier,
	generation: u64,
	running:    bool,
	stopped:    bool,
	job_id:     Option<Str>,
	outcome:    Option<WorkerOutcome>,
	notify:     Arc<Notify>,
}

/// Chat-scoped wave runner backed by durable registered agent loops.
pub(crate) struct ChatVibeBackend<C: omp_agent::TurnClient + Clone + Send + 'static> {
	parent:      Arc<ChatParentHost<C>>,
	modes:       Arc<RegimeHandle>,
	workers:     Arc<Mutex<BTreeMap<Str, Worker>>>,
	monitor:     Arc<Mutex<VibeSwarmMonitor>>,
	seen_active: AtomicBool,
}

impl<C: omp_agent::TurnClient + Clone + Send + 'static> ChatVibeBackend<C> {
	/// Creates a wave runner and its app-owned TTL/mode-exit scheduler.
	pub(crate) fn new(parent: Arc<ChatParentHost<C>>, modes: Arc<RegimeHandle>) -> Arc<Self> {
		let backend = Arc::new(Self {
			parent,
			modes,
			workers: Arc::new(Mutex::new(BTreeMap::new())),
			monitor: Arc::new(Mutex::new(VibeSwarmMonitor::new())),
			seen_active: AtomicBool::new(false),
		});
		Self::start_scheduler(Arc::downgrade(&backend));
		backend
	}

	fn start_scheduler(backend: Weak<Self>) {
		drop(tokio::spawn(async move {
			let mut tick = time::interval(Duration::from_secs(1));
			tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
			loop {
				tick.tick().await;
				let Some(backend) = backend.upgrade() else {
					break;
				};
				if backend.modes.holds_mode("vibe") {
					backend.seen_active.store(true, Ordering::Release);
					let ttl_ms = backend.parent.task_settings().agent_idle_ttl_ms;
					if ttl_ms != 0 {
						backend
							.parent
							.park_expired_children(Duration::from_millis(ttl_ms))
							.await;
					}
				} else if backend.seen_active.swap(false, Ordering::AcqRel) {
					backend.release_scope().await;
				}
			}
		}));
	}

	fn selected_ids(&self, ids: &[Str]) -> Vec<Str> {
		if ids.is_empty() {
			self.workers.lock().keys().cloned().collect()
		} else {
			ids.to_vec()
		}
	}

	async fn spawn(&self, wave: Vec<WaveEntry>) -> Result<Value, Fault> {
		if self.parent.job_board().is_none() {
			return Err(Fault::new("vibe requires the session async job manager"));
		}
		let mut launched = Vec::with_capacity(wave.len());
		for entry in wave {
			let id = Str::from(omp_core::Ulid::generate().to_string());
			let label = entry.label.unwrap_or_else(|| id.clone());
			self.workers.lock().insert(id.clone(), Worker {
				label:      label.clone(),
				tier:       entry.tier,
				generation: 1,
				running:    true,
				stopped:    false,
				job_id:     None,
				outcome:    None,
				notify:     Arc::new(Notify::new()),
			});
			self.monitor.lock().begin_turn(
				id.clone(),
				label.clone(),
				entry.tier,
				entry.brief.as_str(),
			);
			if let Err(error) = self.launch_turn(id.clone(), label.clone(), entry.tier, entry.brief, 1)
			{
				self.workers.lock().remove(&id);
				return Err(error);
			}
			launched.push(json!({ "id": id, "label": label, "status": "running" }));
		}
		Ok(json!({ "wave": launched }))
	}

	fn launch_turn(
		&self,
		id: Str,
		label: Str,
		tier: WorkerTier,
		prompt: Str,
		generation: u64,
	) -> Result<(), Fault> {
		let board = self
			.parent
			.job_board()
			.ok_or_else(|| Fault::new("vibe requires the session async job manager"))?;
		let job_id = board.next_id();
		let job = JobRef {
			id:       job_id.clone(),
			owner:    JobOwner::AgentLoop { agent_id: id.clone() },
			metadata: Arc::new(JobMetadata::running(JobKind::Task, sf!("vibe:{}", label), now_ms())),
			artifact: ExpectedArtifact {
				description: sf!("durable vibe worker result"),
				media_type:  Some(sf!("application/vnd.omp.vibe-result+json")),
				lifetime:    ArtifactLifetime::Durable,
			},
		};
		if !board
			.try_register(job)
			.map_err(|error| Fault::new(format!("vibe job admission failed: {error}")))?
		{
			return Err(Fault::new("vibe job identifier collision"));
		}
		{
			let mut workers = self.workers.lock();
			let Some(worker) = workers.get_mut(&id) else {
				return Err(Fault::new("vibe worker disappeared before launch"));
			};
			if worker.generation != generation || !worker.running {
				return Err(Fault::new("vibe worker generation changed before launch"));
			}
			worker.job_id = Some(job_id.clone());
		}
		let parent = Arc::clone(&self.parent);
		let workers = Arc::clone(&self.workers);
		let monitor = Arc::clone(&self.monitor);
		drop(tokio::spawn(async move {
			let kind = match tier {
				WorkerTier::Fast => "sonic",
				WorkerTier::Good => "task",
			};
			let mut args = json!({
				"prompt": prompt,
				"agent": kind,
				"stableId": id,
				"enableLsp": true,
			});
			if valid_worker_name(label.as_str()) {
				args["name"] = json!(label);
			}
			let result = parent
				.agent(args, &omp_envd::eval::NoopBridgeProgress)
				.await;
			let outcome = match result {
				Ok(value) => WorkerOutcome::Done(value),
				Err(error) => WorkerOutcome::Failed(Str::from(error.to_string())),
			};
			let delivery = delivery_text(&id, &outcome);
			let mut workers = workers.lock();
			let Some(worker) = workers.get_mut(&id) else {
				drop(workers);
				let _ = board.settle(job_id.as_str(), system_item(delivery));
				return;
			};
			if !completion_is_current(worker, generation, &job_id) {
				drop(workers);
				let _ = board.settle(job_id.as_str(), system_item(delivery));
				return;
			}
			{
				let mut monitor = monitor.lock();
				monitor.push_output(id.as_str(), delivery.as_str());
				monitor.settle(id.as_str(), matches!(outcome, WorkerOutcome::Failed(_)));
			}
			let _ = board.settle(job_id.as_str(), system_item(delivery));
			worker.running = false;
			worker.stopped = false;
			worker.job_id = None;
			worker.outcome = Some(outcome);
			worker.notify.notify_waiters();
		}));
		Ok(())
	}

	fn status(&self, ids: &[Str]) -> Result<Value, Fault> {
		let ids = self.selected_ids(ids);
		let workers = self.workers.lock();
		let mut rows = Vec::with_capacity(ids.len());
		for id in ids {
			let worker = workers
				.get(&id)
				.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
			let status = if worker.stopped {
				"stopped"
			} else if worker.running {
				"running"
			} else {
				match worker.outcome.as_ref() {
					Some(WorkerOutcome::Done(_)) => "idle",
					Some(WorkerOutcome::Failed(_)) => "failed",
					None => "parked",
				}
			};
			rows.push(json!({
				"id": id,
				"label": worker.label,
				"tier": worker.tier,
				"status": status,
				"generation": worker.generation,
			}));
		}
		drop(workers);
		let monitor = self.monitor.lock().snapshot(Instant::now());
		let screens = monitor
			.screens
			.iter()
			.map(|screen| {
				json!({
					"id": screen.id,
					"label": screen.label,
					"tier": screen.tier,
					"state": screen.state,
					"turns": screen.turns,
					"turn_started_ms": screen.turn_started_ms,
					"turn_message": screen.turn_message,
					"current_tool": screen.current_tool,
					"trace": screen.trace.iter().map(|entry| entry.summary.as_str()).collect::<Vec<_>>(),
					"output_tail": screen.output_tail,
					"last_activity_ms": screen.last_activity_ms,
					"output_tokens": screen.output_tokens,
				})
			})
			.collect::<Vec<_>>();
		Ok(json!({
			"workers": rows,
			"screens": screens,
			"tokens_per_second": monitor.tokens_per_second,
		}))
	}

	async fn steer(&self, id: Str, message: Str) -> Result<Value, Fault> {
		let running = self
			.workers
			.lock()
			.get(&id)
			.map(|worker| worker.running)
			.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
		if running && self.parent.child_registry_status(id.as_str()) == Some(RegistryStatus::Running)
		{
			let session_id = self.parent.session_id();
			let receipts = self
				.parent
				.broker()
				.send(omp_agent::PeerMessage {
					id: Str::from(omp_core::Ulid::generate().to_string()),
					from: session_id.clone(),
					to: id.clone(),
					text: message,
					mode: omp_agent::DeliveryMode::Steer,
					reply_to: None,
					sent_ms: now_ms(),
					session_id,
					expects_reply: false,
				})
				.map_err(|error| Fault::new(error.to_string()))?;
			return Ok(json!({
				"id": id,
				"receipts": receipts.iter().map(ToString::to_string).collect::<Vec<_>>(),
			}));
		}

		let (label, tier, generation) = {
			let mut workers = self.workers.lock();
			let worker = workers
				.get_mut(&id)
				.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
			worker.generation = worker.generation.saturating_add(1);
			worker.running = true;
			worker.stopped = false;
			worker.outcome = None;
			(worker.label.clone(), worker.tier, worker.generation)
		};
		self
			.monitor
			.lock()
			.begin_turn(id.clone(), label.clone(), tier, message.as_str());
		if let Err(error) = self.launch_turn(id.clone(), label, tier, message, generation) {
			if let Some(worker) = self.workers.lock().get_mut(&id) {
				worker.running = false;
				worker.outcome = Some(WorkerOutcome::Failed(error.message.clone()));
				worker.notify.notify_waiters();
			}
			return Err(error);
		}
		Ok(json!({ "id": id, "status": "running", "generation": generation }))
	}

	async fn collect(&self, ids: &[Str], timeout_ms: Option<u64>) -> Result<Value, Fault> {
		let ids = self.selected_ids(ids);
		let mut rows = Vec::with_capacity(ids.len());
		for id in ids {
			let notify = {
				let workers = self.workers.lock();
				let worker = workers
					.get(&id)
					.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
				worker.notify.clone()
			};
			let notified = notify.notified();
			let waiting = self
				.workers
				.lock()
				.get(&id)
				.is_some_and(|worker| worker.running);
			if waiting {
				if let Some(limit) = timeout_ms {
					if time::timeout(Duration::from_millis(limit), notified)
						.await
						.is_err()
					{
						rows.push(json!({ "id": id, "status": "running" }));
						continue;
					}
				} else {
					notified.await;
				}
			}
			let workers = self.workers.lock();
			let worker = workers
				.get(&id)
				.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
			match worker.outcome.as_ref() {
				Some(WorkerOutcome::Done(value)) => {
					rows.push(json!({ "id": id, "result": value }));
				},
				Some(WorkerOutcome::Failed(error)) => {
					rows.push(json!({ "id": id, "error": error }));
				},
				None => rows.push(json!({
					"id": id,
					"status": if worker.stopped { "stopped" } else { "parked" },
				})),
			}
		}
		Ok(json!({ "workers": rows }))
	}

	async fn stop(&self, ids: &[Str]) -> Result<Value, Fault> {
		let ids = self.selected_ids(ids);
		let tree = self.parent.tree();
		for id in &ids {
			let mut workers = self.workers.lock();
			let worker = workers
				.get_mut(id)
				.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
			worker.generation = worker.generation.saturating_add(1);
			worker.running = false;
			worker.stopped = true;
			let job_id = worker.job_id.take();
			worker.outcome = None;
			worker.notify.notify_waiters();
			drop(workers);
			if let Some(job_id) = job_id {
				let board = self
					.parent
					.job_board()
					.ok_or_else(|| Fault::new("vibe job manager disappeared during stop"))?;
				let mut watch = board.watch(Some(std::slice::from_ref(&job_id)));
				drop(tokio::spawn(async move {
					if let Some(settlement) = watch.next().await {
						let _ = settlement.lease.claim();
					}
				}));
			}
			self.parent.cancel_child(id.as_str());
			if let Some(node) = tree.node(id.as_str()) {
				node.set_status(AgentStatus::Cancelled);
			}
		}
		let release = async {
			for id in &ids {
				self.parent.release_child(id.as_str()).await;
			}
		};
		let _ = time::timeout(Duration::from_secs(5), release).await;
		Ok(json!({ "stopped": ids }))
	}

	async fn release_scope(&self) {
		let ids = self.selected_ids(&[]);
		let _ = self.stop(&ids).await;
	}
}

fn delivery_text(id: &str, outcome: &WorkerOutcome) -> Str {
	let (status, text) = match outcome {
		WorkerOutcome::Done(value) => (
			"settled",
			value
				.get("text")
				.and_then(Value::as_str)
				.map_or_else(|| value.to_string(), str::to_owned),
		),
		WorkerOutcome::Failed(error) => ("failed", error.to_string()),
	};
	let mut preview = text.chars().take(6_000).collect::<String>();
	if text.chars().count() > 6_000 {
		preview.push_str("\n[preview truncated]");
	}
	Str::from(format!("Vibe worker {id} {status}:\n{preview}\n\nFull output: agent://{id}"))
}
fn completion_is_current(worker: &Worker, generation: u64, job_id: &Str) -> bool {
	worker.generation == generation && worker.job_id.as_ref() == Some(job_id)
}

fn valid_worker_name(name: &str) -> bool {
	if name.len() > 32 {
		return false;
	}
	let mut bytes = name.bytes();
	bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
		&& bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn system_item(text: Str) -> Item {
	Item {
		seq:           0,
		created_at_ms: now_ms(),
		kind:          Some(item::Kind::Message(Message {
			role:            i32::from(Role::System),
			parts:           vec![ThreadPart {
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
		.map_or(0, |duration| duration.as_millis() as u64)
}

#[async_trait]
impl<C: omp_agent::TurnClient + Clone + Send + 'static> VibeBackend for ChatVibeBackend<C> {
	async fn execute(&self, params: Params) -> Result<Value, Fault> {
		if !self.modes.holds_mode("vibe") {
			return Err(Fault::new("vibe device requires /vibe on"));
		}
		match params.op {
			Operation::Spawn => self.spawn(params.wave).await,
			Operation::Status => self.status(&params.ids),
			Operation::Steer => {
				self
					.steer(
						params.id.expect("validated steer id"),
						params.message.expect("validated steer message"),
					)
					.await
			},
			Operation::Collect => self.collect(&params.ids, params.timeout_ms).await,
			Operation::Stop => self.stop(&params.ids).await,
		}
	}
}

/// Attaches the vibe device to one chat session.
pub fn attach_chat<C: omp_agent::TurnClient + Clone + Send + 'static>(
	parent: Arc<ChatParentHost<C>>,
	modes: Arc<RegimeHandle>,
) -> Attachment {
	attach(ChatVibeBackend::new(parent, modes))
}

fn param_event(error: ParamError) -> Ev<Update, Payload, Fault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn commit_event(error: CommitError) -> Ev<Update, Payload, Fault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed vibe operation object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"op":"status"}}"#)),
		found:    Some(message),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn one_closed_schema_covers_all_five_verbs() {
		for op in ["spawn", "status", "steer", "collect", "stop"] {
			let mut value = json!({ "op": op });
			if op == "spawn" {
				value["wave"] = json!([{ "brief": "inspect", "tier": "fast" }]);
			}
			if op == "steer" {
				value["id"] = json!("worker");
				value["message"] = json!("focus on errors");
			}
			let params: Params = serde_json::from_value(value).expect("valid verb shape");
			params.validate().expect("valid operation");
		}
		assert!(serde_json::from_value::<Params>(json!({ "op": "status", "extra": true })).is_err());
	}

	#[test]
	fn spawn_and_steer_reject_incomplete_shapes() {
		let spawn: Params = serde_json::from_value(json!({ "op": "spawn" })).expect("shape");
		assert!(spawn.validate().is_err());
		let steer: Params =
			serde_json::from_value(json!({ "op": "steer", "id": "worker" })).expect("shape");
		assert!(steer.validate().is_err());
	}

	#[test]
	fn stale_vibe_generation_cannot_publish_its_job() {
		let job_id = sf!("job-1");
		let worker = Worker {
			label:      sf!("worker"),
			tier:       WorkerTier::Fast,
			generation: 2,
			running:    false,
			stopped:    true,
			job_id:     None,
			outcome:    None,
			notify:     Arc::new(Notify::new()),
		};
		assert!(!completion_is_current(&worker, 1, &job_id));
	}
}
