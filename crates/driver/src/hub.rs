//! Chat-owned composition for the unified hub tool.

use std::{
	collections::{BTreeMap, BTreeSet},
	iter, mem,
	sync::{Arc, LazyLock},
	time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use omp_agent::{
	AgentEvent, AgentRecord, Broker, CancelOutcome, DeliveryMode, EventBus, JobBoard, JobError,
	PeerMessage, PeerRelayObservation, RegistryStatus, TurnClient,
};
use omp_core::{Duration, DurationUnit, Str, sf};
use omp_env::{EnvClient, ProcessAttachmentEvent};
use omp_proto::{
	env::{
		v1,
		v1::{
			AttachOutput, EnvironmentDelta, ListProcesses, ProcessSpec, PtySpec, ReadyLog, ReadyProbe,
			ReadyTcp, RestartSpec, Script, SendInput, SignalProcess, StartProcess, StopProcess,
			ready_probe, send_input,
		},
	},
	inference::v1::{Value, ValueMap, value},
};
use omp_tool::{ArtifactLifetime, ExpectedArtifact, JobKind, JobMetadata, JobOwner, JobRef, Tool};
use omp_tools::hub::{
	DEFAULT_LIST_LIMIT, Fault, HubBackend, HubRouter, ListStatus, Op, Params, Request, Response,
	RestartPolicy,
};
use regex::Regex;
use serde_json::json;
use tokio::{sync::broadcast::error::RecvError, task::JoinHandle, time};

use crate::subagent::supervisor::SessionSupervisor;

static ROUTER: LazyLock<HubRouter<ChatHubBackend>> = LazyLock::new(HubRouter::new);
const DEFAULT_ROUTE: &str = "*";
const HUB_TERMINAL_COLUMNS: usize = 120;
const HUB_TERMINAL_ROWS: usize = 40;
const HUB_WAIT_MATCH_BYTES: usize = 256 * 1024;
#[derive(Debug, Eq, PartialEq, serde::Serialize)]
struct RosterCounts {
	running:   usize,
	idle:      usize,
	parked:    usize,
	shown:     usize,
	truncated: usize,
}

struct RosterPage {
	peers:  Vec<serde_json::Value>,
	counts: RosterCounts,
}

#[derive(Default)]
enum TerminalParseState {
	#[default]
	Text,
	Escape,
	Csi(String),
	Osc,
	OscEscape,
}

struct TerminalRowReplay {
	cells:  Vec<Vec<char>>,
	row:    usize,
	column: usize,
	state:  TerminalParseState,
}

impl TerminalRowReplay {
	fn new() -> Self {
		Self {
			cells:  vec![vec![' '; HUB_TERMINAL_COLUMNS]; HUB_TERMINAL_ROWS],
			row:    0,
			column: 0,
			state:  TerminalParseState::Text,
		}
	}

	fn process(&mut self, data: &[u8]) {
		for character in String::from_utf8_lossy(data).chars() {
			let state = mem::take(&mut self.state);
			match state {
				TerminalParseState::Text => self.text(character),
				TerminalParseState::Escape if character == '[' => {
					self.state = TerminalParseState::Csi(String::new());
				},
				TerminalParseState::Escape if character == ']' => {
					self.state = TerminalParseState::Osc;
				},
				TerminalParseState::Escape => {},
				TerminalParseState::Csi(parameters) if ('\u{40}'..='\u{7e}').contains(&character) => {
					self.csi(&parameters, character);
				},
				TerminalParseState::Csi(mut parameters) => {
					if parameters.len() < 64 {
						parameters.push(character);
					}
					self.state = TerminalParseState::Csi(parameters);
				},
				TerminalParseState::Osc if character == '\u{7}' => {},
				TerminalParseState::Osc if character == '\u{1b}' => {
					self.state = TerminalParseState::OscEscape;
				},
				TerminalParseState::Osc => self.state = TerminalParseState::Osc,
				TerminalParseState::OscEscape if character == '\\' => {},
				TerminalParseState::OscEscape => self.state = TerminalParseState::Osc,
			}
		}
	}

	fn text(&mut self, character: char) {
		match character {
			'\u{1b}' => self.state = TerminalParseState::Escape,
			'\r' => self.column = 0,
			'\n' => self.newline(),
			'\u{8}' => self.column = self.column.saturating_sub(1),
			'\t' => self.column = (self.column + 8).min(HUB_TERMINAL_COLUMNS) & !7,
			character if character.is_control() => {},
			character => {
				let width = xutf::width_char(character);
				if width == 0 {
					return;
				}
				if self.column >= HUB_TERMINAL_COLUMNS {
					self.newline();
				}
				self.cells[self.row][self.column] = character;
				for offset in 1..width {
					if self.column + offset < HUB_TERMINAL_COLUMNS {
						self.cells[self.row][self.column + offset] = '\0';
					}
				}
				self.column = (self.column + width).min(HUB_TERMINAL_COLUMNS);
			},
		}
	}

	fn newline(&mut self) {
		self.column = 0;
		if self.row + 1 < HUB_TERMINAL_ROWS {
			self.row += 1;
		} else {
			self.cells.rotate_left(1);
			self.cells[HUB_TERMINAL_ROWS - 1].fill(' ');
		}
	}

	fn csi(&mut self, parameters: &str, command: char) {
		let mut values = parameters
			.trim_start_matches('?')
			.split(';')
			.map(|value| value.parse::<usize>().unwrap_or(0));
		let first = values.next().unwrap_or(0);
		let amount = first.max(1);
		match command {
			'A' => self.row = self.row.saturating_sub(amount),
			'B' => self.row = (self.row + amount).min(HUB_TERMINAL_ROWS - 1),
			'C' => self.column = (self.column + amount).min(HUB_TERMINAL_COLUMNS),
			'D' => self.column = self.column.saturating_sub(amount),
			'G' => self.column = amount.saturating_sub(1).min(HUB_TERMINAL_COLUMNS - 1),
			'd' => self.row = amount.saturating_sub(1).min(HUB_TERMINAL_ROWS - 1),
			'H' | 'f' => {
				self.row = amount.saturating_sub(1).min(HUB_TERMINAL_ROWS - 1);
				self.column = values
					.next()
					.unwrap_or(1)
					.saturating_sub(1)
					.min(HUB_TERMINAL_COLUMNS - 1);
			},
			'J' if first == 2 || first == 3 => {
				for row in &mut self.cells {
					row.fill(' ');
				}
				self.row = 0;
				self.column = 0;
			},
			'K' if first == 1 => {
				let end = self.column.min(HUB_TERMINAL_COLUMNS - 1);
				self.cells[self.row][..=end].fill(' ');
			},
			'K' if first == 2 => self.cells[self.row].fill(' '),
			'K' => self.cells[self.row][self.column..].fill(' '),
			_ => {},
		}
	}

	fn into_lines(self) -> Vec<String> {
		self
			.cells
			.into_iter()
			.map(|row| {
				row.into_iter()
					.filter(|character| *character != '\0')
					.collect::<String>()
					.trim_end()
					.to_owned()
			})
			.collect()
	}
}

/// Produces the one process-global hub tool registered in the env registry.
pub fn tool() -> impl Tool {
	omp_tools::hub::tool(ChatHubRoute)
}

/// Installs the main live chat composition, restoring the prior one on drop.
pub fn attach(backend: Arc<ChatHubBackend>) -> HubAttachment {
	attach_for(sf!(DEFAULT_ROUTE), backend)
}

/// Installs one agent-addressed chat composition without replacing Main.
pub fn attach_for(owner: Str, backend: Arc<ChatHubBackend>) -> HubAttachment {
	let previous = ROUTER.attach(owner.clone(), backend);
	HubAttachment { owner, previous }
}

/// Scoped agent-addressed hub attachment that restores the prior route when
/// dropped.
#[must_use]
pub struct HubAttachment {
	owner:    Str,
	previous: Option<Arc<ChatHubBackend>>,
}

impl Drop for HubAttachment {
	fn drop(&mut self) {
		ROUTER.detach(&self.owner);
		if let Some(previous) = self.previous.take() {
			ROUTER.attach(self.owner.clone(), previous);
		}
	}
}

#[derive(Clone, Copy)]
struct ChatHubRoute;

impl HubBackend for ChatHubRoute {
	async fn execute<'a>(
		&'a self,
		_caller_id: &'a str,
		request: Request,
		updates: &'a flume::Sender<Response>,
	) -> Result<Response, Fault> {
		// Child invocations route by stable agent identity. Calls without a
		// child attachment belong to the main session composition.
		let owner = if ROUTER.contains(_caller_id) {
			_caller_id
		} else {
			DEFAULT_ROUTE
		};
		ROUTER.execute(owner, request, updates).await
	}
}

/// Cancellation authority used by the hub's agent-addressed cancel operation.
pub trait AgentCancellation: Send + Sync {
	/// Cancels the live agent identified by its stable hub id.
	fn cancel(&self, id: &str) -> Result<(), Str>;
}

impl<C: TurnClient + Clone + Send + 'static> AgentCancellation for SessionSupervisor<C> {
	fn cancel(&self, id: &str) -> Result<(), Str> {
		SessionSupervisor::cancel(self, id).map_err(|error| Str::from(error.to_string()))
	}
}

/// Shared mailbox ownership used by the hub tool and authenticated CONTROL.
mod shared_inbox {
	use std::sync::Arc;

	use omp_agent::BrokerInbox;
	use tokio::sync::Mutex;

	/// Shared mailbox ownership used by the hub tool and authenticated CONTROL.
	pub type SharedBrokerInbox = Arc<Mutex<BrokerInbox>>;

	/// Promotes the broker's single-consumer inbox into a shared serialized
	/// owner.
	pub fn share_inbox(inbox: BrokerInbox) -> SharedBrokerInbox {
		Arc::new(Mutex::new(inbox))
	}
}
pub use shared_inbox::{SharedBrokerInbox, share_inbox};

/// Session-scoped hub composition for peer messaging, jobs, and supervised
/// processes.
pub struct ChatHubBackend {
	broker:     Broker,
	inbox:      SharedBrokerInbox,
	jobs:       Arc<JobBoard>,
	env:        EnvClient,
	agent_id:   Str,
	session:    Str,
	relay_task: Option<JoinHandle<()>>,
	canceller:  Option<Arc<dyn AgentCancellation>>,
}

impl ChatHubBackend {
	/// Binds hub operations to one agent identity and optionally relays peer
	/// events.
	pub fn new(
		broker: Broker,
		inbox: SharedBrokerInbox,
		jobs: Arc<JobBoard>,
		env: EnvClient,
		agent_id: Str,
		session: Str,
		relay_bus: Option<EventBus>,
		canceller: Option<Arc<dyn AgentCancellation>>,
	) -> Self {
		let relay_task = relay_bus.map(|bus| {
			let mut routes = broker.subscribe_routes();
			tokio::spawn(async move {
				loop {
					match routes.recv().await {
						Ok(event) if event.relay_to_main => {
							bus.publish(AgentEvent::PeerRelay(Arc::new(PeerRelayObservation {
								id:      event.message.id,
								from:    event.message.from,
								to:      event.delivery.to,
								text:    event.message.text,
								outcome: event.delivery.outcome,
							})));
						},
						Ok(_) | Err(RecvError::Lagged(_)) => {},
						Err(RecvError::Closed) => return,
					}
				}
			})
		});
		Self { broker, inbox, jobs, env, agent_id, session, relay_task, canceller }
	}

	fn response(value: serde_json::Value) -> Result<Response, Fault> {
		Self::response_with(value, false)
	}

	fn response_with(value: serde_json::Value, useless: bool) -> Result<Response, Fault> {
		serde_json::to_string_pretty(&value)
			.map(|text| Response { text: Str::from(text), useless })
			.map_err(|error| fault(error.to_string()))
	}

	fn roster_page(&self, status: Option<ListStatus>, limit: Option<u16>) -> RosterPage {
		self.restore_persisted_roster();
		let (records, counts) = select_roster(
			self.broker.registry().roster(false),
			self.agent_id.as_str(),
			status,
			limit.map_or(DEFAULT_LIST_LIMIT, usize::from),
		);
		let peers = records
			.into_iter()
			.map(|record| {
				let unread = self.broker.unread_count(&record.id).unwrap_or(0);
				json!({
					"id": record.id,
					"name": record.name,
					"kind": record.kind.to_string(),
					"status": record.status.to_string(),
					"parent": record.parent,
					"session": record.session,
					"depth": record.depth,
					"activity": record.activity,
					"lastActivityMs": record.last_activity_ms,
					"unread": unread,
					"definition": record.definition,
					"model": record.model,
					"servingModel": record.serving_model,
					"task": record.task,
					"terminal": record.history.terminal.map(|terminal| format!("{terminal:?}")),
					"output": record.history.output_path.map(|path| path.display().to_string()),
					"patch": record.history.patch_path.map(|path| path.display().to_string()),
				})
			})
			.collect();
		RosterPage { peers, counts }
	}

	fn restore_persisted_roster(&self) {
		let registry = self.broker.registry();
		let mut current = self.agent_id.clone();
		let mut root_file = None;
		for _ in 0..64 {
			let Some((record, _)) = registry.record(current.as_str()) else {
				break;
			};
			if record.kind == omp_agent::AgentKind::Main {
				root_file = record.transcript;
				break;
			}
			let Some(parent) = record.parent else {
				root_file = record.transcript;
				break;
			};
			current = parent;
		}
		let Some(root_file) = root_file else {
			return;
		};
		let Some(sessions) = root_file.parent() else {
			return;
		};
		let directory = sessions.join("eval-agents");
		if directory.is_dir() {
			registry.restore_transcripts_once(&root_file, &directory);
		}
	}

	async fn process_generation(&self, name: &str) -> Result<u64, Fault> {
		self
			.env
			.list_processes(ListProcesses { props: None })
			.await
			.map_err(|error| fault(error.to_string()))?
			.processes
			.into_iter()
			.find(|process| process.name == name)
			.map(|process| process.generation)
			.ok_or_else(|| fault(format!("process {name:?} was not found")))
	}

	async fn peer_send(&self, params: &Params) -> Result<Response, Fault> {
		let to = params
			.to
			.clone()
			.ok_or_else(|| fault("peer recipient is required"))?;
		if to.eq_ignore_ascii_case(self.agent_id.as_str()) {
			return Err(fault("cannot send a hub message to the calling agent"));
		}
		if params.await_reply && (to == "all" || to == "project:all" || to.starts_with("session:")) {
			return Err(fault("awaited hub sends require one direct recipient"));
		}
		let id = Str::from(omp_core::Ulid::generate().to_string());
		let message = PeerMessage {
			id:            id.clone(),
			from:          self.agent_id.clone(),
			to:            to.clone(),
			text:          params.message.clone().unwrap_or_default(),
			mode:          DeliveryMode::Aside,
			reply_to:      params.reply_to.clone(),
			sent_ms:       now_ms(),
			session_id:    self.session.clone(),
			expects_reply: params.await_reply,
		};
		let deliveries = self
			.broker
			.route(message)
			.map_err(|error| fault(error.to_string()))?;
		if params.await_reply {
			if deliveries
				.iter()
				.all(|delivery| delivery.outcome == omp_agent::Receipt::Failed)
			{
				return Self::response(json!({ "deliveries": deliveries_json(&deliveries) }));
			}
			let recipient = deliveries
				.first()
				.map(|delivery| delivery.to.clone())
				.unwrap_or(to);
			let timeout = wait_timeout(params.timeout_ms);
			let reply = self
				.inbox
				.lock()
				.await
				.wait_for_timeout(Some(recipient.as_str()), Some(id.as_str()), timeout)
				.await
				.map_err(|error| fault(error.to_string()))?;
			return Self::response(json!({
				"deliveries": deliveries_json(&deliveries),
				"reply": reply.map(message_json),
			}));
		}
		Self::response(json!({
			  "id": id,
			  "deliveries": deliveries_json(&deliveries),
		}))
	}

	async fn wait(
		&self,
		params: &Params,
		updates: &flume::Sender<Response>,
	) -> Result<Response, Fault> {
		if params.name.is_some() {
			return self.process_wait(params).await;
		}
		let timeout = match params.timeout_ms {
			None => Some(self.jobs.next_smart_wait(&self.agent_id)),
			Some(0) => None,
			Some(milliseconds) => Some(StdDuration::from_millis(milliseconds)),
		};
		let started = now_ms();
		let progress_jobs = Arc::clone(&self.jobs);
		let progress_ids = params.ids.clone();
		let progress_updates = updates.clone();
		tokio::spawn(async move {
			loop {
				time::sleep(StdDuration::from_millis(500)).await;
				let jobs = progress_jobs
					.snapshot()
					.into_iter()
					.filter(|job| {
						progress_ids
							.as_ref()
							.is_none_or(|ids| ids.contains(&job.id))
					})
					.map(job_json)
					.collect::<Vec<_>>();
				let Ok(response) = ChatHubBackend::response_with(
					json!({ "waitingMs": now_ms().saturating_sub(started), "jobs": jobs }),
					true,
				) else {
					return;
				};
				match progress_updates.try_send(response) {
					Ok(()) | Err(flume::TrySendError::Full(_)) => {},
					Err(flume::TrySendError::Disconnected(_)) => return,
				}
			}
		});
		let mut jobs = self.jobs.watch(params.ids.as_deref());
		if jobs.is_empty() {
			let message = self
				.inbox
				.lock()
				.await
				.wait_for_timeout(params.from_peer.as_deref(), None, timeout)
				.await
				.map_err(|error| fault(error.to_string()))?;
			if params.timeout_ms.is_none() {
				self.jobs.record_smart_wait_end(&self.agent_id);
			}
			let useless = message.is_none();
			return Self::response_with(json!({ "message": message.map(message_json) }), useless);
		}
		let mut inbox = self.inbox.lock().await;
		let result = async {
			tokio::select! {
				  biased;
				  peer = inbox.wait_for_timeout(params.from_peer.as_deref(), None, timeout) => {
						 peer.map(|message| (message.map(message_json), None)).map_err(|error| fault(error.to_string()))
				  },
				  settlement = jobs.next() => Ok((None, settlement)),
			}
		};
		let (peer, settlement) = if let Some(timeout) = timeout {
			match time::timeout(timeout, result).await {
				Ok(result) => result?,
				Err(_) => {
					if params.timeout_ms.is_none() {
						self.jobs.record_smart_wait_end(&self.agent_id);
					}
					return Self::response_with(
						json!({ "timeout": true, "waitedMs": now_ms().saturating_sub(started) }),
						true,
					);
				},
			}
		} else {
			result.await?
		};
		if params.timeout_ms.is_none() {
			self.jobs.record_smart_wait_end(&self.agent_id);
		}
		if let Some(settlement) = settlement {
			let id = settlement.job.id.clone();
			let item = format!("{:?}", settlement.item);
			settlement
				.lease
				.claim()
				.map_err(|error| fault(error.to_string()))?;
			return Self::response(json!({ "job": id, "settled": true, "item": item }));
		}
		Self::response_with(json!({ "message": peer }), peer.is_none())
	}

	async fn cancel_jobs(&self, params: &Params) -> Result<Response, Fault> {
		let grace = Duration::new(5, DurationUnit::Seconds);
		let mut outcomes = BTreeMap::new();
		for id in params.ids.as_deref().unwrap_or_default() {
			let outcome = match self.jobs.cancel(id, grace).await {
				Ok(CancelOutcome::Accepted) => json!({ "status": "accepted" }),
				Ok(CancelOutcome::AlreadySettled) => json!({ "status": "already_settled" }),
				Err(JobError::AgentLoopCancellation { agent_id }) => self.cancel_agent(&agent_id),
				Err(error) => return Err(fault(error.to_string())),
				Ok(CancelOutcome::Missing) => self.cancel_agent(id),
			};
			outcomes.insert(id.as_str(), outcome);
		}
		Self::response(json!({ "jobs": outcomes }))
	}

	fn cancel_agent(&self, id: &str) -> serde_json::Value {
		let Some((record, _)) = self.broker.registry().record(id) else {
			return json!({ "status": "missing" });
		};
		let salvage = json!({
			"output": record.history.output_path.map(|path| path.display().to_string()),
			"patch": record.history.patch_path.map(|path| path.display().to_string()),
			"branch": record.history.branch,
		});
		match record.status {
			omp_agent::RegistryStatus::Running | omp_agent::RegistryStatus::Idle => {
				match self
					.canceller
					.as_ref()
					.map(|canceller| canceller.cancel(&record.id))
				{
					Some(Ok(())) => {
						json!({ "status": "accepted", "agent": record.id, "salvage": salvage })
					},
					Some(Err(error)) => {
						json!({ "status": "failed", "agent": record.id, "error": error, "salvage": salvage })
					},
					None => json!({ "status": "unavailable", "agent": record.id, "salvage": salvage }),
				}
			},
			omp_agent::RegistryStatus::Parked => {
				json!({ "status": "not_running", "agent": record.id, "salvage": salvage })
			},
		}
	}

	async fn start(&self, params: &Params) -> Result<Response, Fault> {
		let name = params
			.name
			.as_ref()
			.ok_or_else(|| fault("process name is required"))?;
		let command = command_text(
			params
				.application
				.as_deref()
				.ok_or_else(|| fault("application is required"))?,
			params.args.as_deref().unwrap_or_default(),
		);
		let ready = params.ready.as_ref().map_or_else(Vec::new, |ready| {
			let timeout_ms = ready.timeout.unwrap_or(30.0).mul_add(1_000.0, 0.0) as u64;
			let mut probes = Vec::new();
			if let Some(pattern) = &ready.log {
				probes.push(ReadyProbe {
					probe: Some(ready_probe::Probe::Log(ReadyLog {
						pattern: pattern.to_string(),
						props:   None,
					})),
					timeout_ms,
					props: None,
				});
			}
			if let Some(port) = ready.port {
				probes.push(ReadyProbe {
					probe: Some(ready_probe::Probe::Tcp(ReadyTcp {
						host:  ready.host.as_deref().unwrap_or("127.0.0.1").to_owned(),
						port:  u32::from(port),
						props: None,
					})),
					timeout_ms,
					props: None,
				});
			}
			probes
		});
		let restart = match params.restart.unwrap_or(RestartPolicy::No) {
			RestartPolicy::No => v1::RestartPolicy::Never,
			RestartPolicy::OnFailure => v1::RestartPolicy::OnFailure,
			RestartPolicy::Always => v1::RestartPolicy::Always,
		};
		let cwd_uri = params.cwd.as_ref().map_or_else(
			|| {
				self
					.env
					.info()
					.map_or_else(String::new, |info| info.root_uri)
			},
			ToString::to_string,
		);
		let cwd =
			omp_core::EnvPath::new(Str::from(cwd_uri)).map_err(|error| fault(error.to_string()))?;
		let started = self
			.env
			.start_process(&cwd, StartProcess {
				name: name.to_string(),
				spec: Some(ProcessSpec {
					source:     Some(Script { text: command, props: None }),
					cwd_uri:    String::new(),
					env_delta:  Some(EnvironmentDelta {
						set:   params
							.env
							.clone()
							.unwrap_or_default()
							.into_iter()
							.map(|(key, value)| (key.to_string(), value.to_string()))
							.collect(),
						unset: Vec::new(),
						props: None,
					}),
					pty:        params
						.pty
						.unwrap_or(true)
						.then(|| PtySpec { terminal: "xterm-256color".to_owned(), ..Default::default() }),
					restart:    Some(RestartSpec { policy: restart as i32, ..Default::default() }),
					timeout_ms: None,
					detached:   params.detached,
					persist:    params.persist,
					props:      Some(owner_process_props(&self.session, &self.agent_id)),
				}),
				ready,
				props: None,
			})
			.await
			.map_err(|error| fault(error.to_string()))?;
		let lifetime = if params.detached {
			ArtifactLifetime::Durable
		} else {
			ArtifactLifetime::Session
		};
		let label = Str::from(format!("completion of named process {}", started.name));
		let started_at_ms = now_ms();
		let mut metadata = JobMetadata::running(JobKind::Shell, label.clone(), started_at_ms);
		metadata.owner_session = Some(self.session.clone());
		let job = JobRef {
			id:       Str::from(format!("process:{}:{}", started.name, started.generation)),
			owner:    JobOwner::NamedProcess {
				name:       Str::from(started.name.clone()),
				generation: started.generation,
			},
			metadata: Arc::new(metadata),
			artifact: ExpectedArtifact { description: label, media_type: None, lifetime },
		};
		if let Err(error) = self.jobs.try_register(job) {
			let _ = self
				.env
				.stop_process(StopProcess {
					name:       started.name.clone(),
					grace_ms:   250,
					generation: started.generation,
					props:      None,
				})
				.await;
			return Err(fault(error.to_string()));
		}
		Self::response(
			json!({ "name": started.name, "generation": started.generation, "ready": true }),
		)
	}

	async fn ensure_owned_process_jobs(&self) -> Result<(), Fault> {
		let processes = self
			.env
			.list_processes(ListProcesses { props: None })
			.await
			.map_err(|error| fault(error.to_string()))?
			.processes;
		let mut live = BTreeSet::new();
		for process in processes {
			if process_owner(process.props.as_ref())
				!= Some((self.session.as_str(), self.agent_id.as_str()))
			{
				continue;
			}
			live.insert((Str::from(process.name.clone()), process.generation));
			let label = Str::from(format!("completion of named process {}", process.name));
			let mut metadata = JobMetadata::running(JobKind::Shell, label.clone(), now_ms());
			metadata.owner_session = Some(self.session.clone());
			let job = JobRef {
				id:       Str::from(format!("process:{}:{}", process.name, process.generation)),
				owner:    JobOwner::NamedProcess {
					name:       Str::from(process.name.clone()),
					generation: process.generation,
				},
				metadata: Arc::new(metadata),
				artifact: ExpectedArtifact {
					description: label,
					media_type:  None,
					lifetime:    ArtifactLifetime::Session,
				},
			};
			self
				.jobs
				.reattach(job)
				.map_err(|error| fault(error.to_string()))?;
		}
		self
			.jobs
			.release_missing_process_leases(&self.session, &live);
		Ok(())
	}

	async fn process_wait(&self, params: &Params) -> Result<Response, Fault> {
		let name = params
			.name
			.as_deref()
			.ok_or_else(|| fault("process name is required"))?;
		let generation = self.process_generation(name).await?;
		let pattern = compile_regex(params.pattern.as_deref(), "wait pattern")?;
		let mut attachment = self
			.env
			.attach_output(AttachOutput {
				name: name.to_owned(),
				after_sequence: params.cursor.unwrap_or(0),
				generation,
				max_bytes: 64 * 1024,
				terminal_text: false,
				terminal_columns: 0,
				terminal_rows: 0,
				props: None,
			})
			.await
			.map_err(|error| fault(error.to_string()))?;
		let deadline = StdDuration::from_secs_f64(params.timeout.unwrap_or(30.0));
		let event = time::timeout(deadline, async {
			let mut accumulated = String::new();
			while let Some(event) = attachment
				.next_event()
				.await
				.map_err(|error| fault(error.to_string()))?
			{
				match event {
					ProcessAttachmentEvent::Output(output) => {
						let text = String::from_utf8_lossy(&output.data);
						if pattern.is_none() {
							return Ok(Some(json!({
								"name": output.name,
								"cursor": output.sequence,
								"output": text,
							})));
						}
						append_bounded(&mut accumulated, &text, HUB_WAIT_MATCH_BYTES);
						if pattern
							.as_ref()
							.is_some_and(|pattern| pattern.is_match(&accumulated))
						{
							return Ok(Some(json!({
								"name": output.name,
								"cursor": output.sequence,
								"output": accumulated,
							})));
						}
					},
					ProcessAttachmentEvent::State(state) => {
						let target = params.wait_for.as_deref().unwrap_or("exit");
						let process_state = v1::ProcessState::try_from(
							state.process.as_ref().map_or(0, |process| process.state),
						)
						.ok();
						if (target == "ready"
							&& matches!(
								process_state,
								Some(
									omp_proto::env::v1::ProcessState::Ready
										| omp_proto::env::v1::ProcessState::Running
								)
							)) || (target == "exit"
							&& matches!(
								process_state,
								Some(
									omp_proto::env::v1::ProcessState::Exited
										| omp_proto::env::v1::ProcessState::Stopped
										| omp_proto::env::v1::ProcessState::Failed
								)
							)) {
							return Ok(Some(
								json!({ "name": name, "state": format!("{process_state:?}") }),
							));
						}
					},
					_ => {},
				}
			}
			Ok::<_, Fault>(None)
		})
		.await
		.map_err(|_| fault("process wait timed out"))??;
		Self::response(json!({ "event": event }))
	}

	async fn logs(&self, params: &Params) -> Result<Response, Fault> {
		let name = params
			.name
			.as_deref()
			.ok_or_else(|| fault("process name is required"))?;
		let generation = self.process_generation(name).await?;
		let grep = compile_regex(params.grep.as_deref(), "log filter")?;
		let mut attachment = self
			.env
			.attach_output(AttachOutput {
				name: name.to_owned(),
				after_sequence: params.cursor.unwrap_or(0),
				generation,
				max_bytes: 2 * 1024 * 1024,
				terminal_text: params.render_terminal_rows,
				terminal_columns: u32::from(params.render_terminal_rows) * HUB_TERMINAL_COLUMNS as u32,
				terminal_rows: u32::from(params.render_terminal_rows) * HUB_TERMINAL_ROWS as u32,
				props: None,
			})
			.await
			.map_err(|error| fault(error.to_string()))?;
		let limit = usize::from(params.lines.unwrap_or(100));
		let mut lines = Vec::new();
		let mut terminal_bytes = Vec::new();
		let mut cursor = params.cursor.unwrap_or(0);
		let mut timed_out = false;
		let idle = if params.follow {
			StdDuration::from_secs_f64(params.timeout.unwrap_or(30.0))
		} else {
			StdDuration::from_millis(20)
		};
		loop {
			match time::timeout(idle, attachment.next_event()).await {
				Ok(Ok(Some(ProcessAttachmentEvent::Output(output)))) => {
					cursor = cursor.max(output.sequence);
					if params.render_terminal_rows {
						terminal_bytes.extend_from_slice(&output.data);
					} else {
						for line in String::from_utf8_lossy(&output.data).lines() {
							if grep.as_ref().is_none_or(|pattern| pattern.is_match(line)) {
								lines.push(line.to_owned());
							}
						}
					}
					if params.follow && (!lines.is_empty() || !terminal_bytes.is_empty()) {
						break;
					}
				},
				Ok(Ok(Some(ProcessAttachmentEvent::State(_)) | None)) => break,
				Err(_) => {
					timed_out = params.follow;
					break;
				},
				Ok(Ok(Some(ProcessAttachmentEvent::Attached(_)))) => {},
				Ok(Err(error)) => return Err(fault(error.to_string())),
			}
		}
		if params.render_terminal_rows {
			let mut replay = TerminalRowReplay::new();
			replay.process(&terminal_bytes);
			lines = replay
				.into_lines()
				.into_iter()
				.filter(|line| grep.as_ref().is_none_or(|pattern| pattern.is_match(line)))
				.collect();
		}
		if !params.head && lines.len() > limit {
			lines.drain(..lines.len() - limit);
		} else {
			lines.truncate(limit);
		}
		Self::response(json!({
			"name": name,
			"lines": lines,
			"cursor": cursor,
			"timedOut": timed_out,
		}))
	}

	async fn process_send(&self, params: &Params) -> Result<Response, Fault> {
		let name = params
			.name
			.as_deref()
			.ok_or_else(|| fault("process name is required"))?;
		let generation = self.process_generation(name).await?;
		if let Some(signal) = params.signal {
			self
				.env
				.signal_process(SignalProcess {
					name: name.to_owned(),
					signal: format!("{signal:?}").to_uppercase(),
					generation,
					props: None,
				})
				.await
				.map_err(|error| fault(error.to_string()))?;
		}
		let mut data = params
			.text
			.as_deref()
			.unwrap_or_default()
			.as_bytes()
			.to_vec();
		for key in params.keys.as_deref().unwrap_or_default() {
			append_key(&mut data, key);
		}
		if params.enter.unwrap_or(true) && params.text.is_some() {
			data.push(b'\n');
		}
		if !data.is_empty() {
			self
				.env
				.send_process_input(SendInput {
					name: name.to_owned(),
					input: Some(send_input::Input::Data(Bytes::from(data))),
					generation,
					props: None,
				})
				.await
				.map_err(|error| fault(error.to_string()))?;
		}
		Self::response(json!({ "name": name, "accepted": true }))
	}
}

impl Drop for ChatHubBackend {
	fn drop(&mut self) {
		if let Some(task) = self.relay_task.take() {
			task.abort();
		}
	}
}

impl HubBackend for ChatHubBackend {
	async fn execute<'a>(
		&'a self,
		_caller_id: &'a str,
		request: Request,
		updates: &'a flume::Sender<Response>,
	) -> Result<Response, Fault> {
		let params = request.params;
		if matches!(
			params.op,
			Op::Jobs
				| Op::Wait
				| Op::Cancel
				| Op::Ps | Op::Logs
				| Op::Stop
				| Op::Restart
				| Op::Describe
		) || (params.op == Op::Send && params.name.is_some())
		{
			self.ensure_owned_process_jobs().await?;
		}
		match params.op {
			Op::Send if params.to.is_some() => self.peer_send(&params).await,
			Op::Send => self.process_send(&params).await,
			Op::Wait => self.wait(&params, updates).await,
			Op::Inbox => Self::response(
				json!({ "messages": self.inbox.lock().await.inbox(params.peek).into_iter().map(message_json).collect::<Vec<_>>() }),
			),
			Op::List => {
				let page = self.roster_page(params.status, params.limit);
				Self::response(json!({ "peers": page.peers, "counts": page.counts }))
			},
			Op::Jobs => {
				let roster = self.roster_page(None, None);
				Self::response(json!({
					"jobs": self.jobs.snapshot_consuming().into_iter().map(job_json).collect::<Vec<_>>(),
					"agents": roster.peers,
				}))
			},
			Op::Cancel => self.cancel_jobs(&params).await,
			Op::Start => self.start(&params).await,
			Op::Ps => {
				let list = self
					.env
					.list_processes(ListProcesses { props: None })
					.await
					.map_err(|error| fault(error.to_string()))?;
				Self::response(json!({
					"processes": list.processes.into_iter().map(process_json).collect::<Vec<_>>()
				}))
			},
			Op::Logs => self.logs(&params).await,
			Op::Stop => {
				let name = params
					.name
					.as_deref()
					.ok_or_else(|| fault("process name is required"))?;
				let grace_ms = params.timeout.unwrap_or(5.0).mul_add(1_000.0, 0.0) as u64;
				let generation = self.process_generation(name).await?;
				self
					.env
					.stop_process(StopProcess {
						name: name.to_owned(),
						grace_ms,
						generation,
						props: None,
					})
					.await
					.map_err(|error| fault(error.to_string()))?;
				Self::response(json!({ "name": name, "stopping": true }))
			},
			Op::Restart => {
				let name = params
					.name
					.as_deref()
					.ok_or_else(|| fault("process name is required"))?;
				let generation = self.process_generation(name).await?;
				let restarted = self
					.env
					.restart_process(v1::RestartProcess {
						name: name.to_owned(),
						generation,
						wire_revision: omp_proto::SCHEMA_REV,
						props: None,
					})
					.await
					.map_err(|error| fault(error.to_string()))?;
				self.ensure_owned_process_jobs().await?;
				Self::response(json!({
					"name": restarted.name,
					"generation": restarted.generation,
					"restarted": true,
				}))
			},
			Op::Describe => {
				let name = params
					.name
					.as_deref()
					.ok_or_else(|| fault("process name is required"))?;
				let list = self
					.env
					.list_processes(ListProcesses { props: None })
					.await
					.map_err(|error| fault(error.to_string()))?;
				let process = list
					.processes
					.into_iter()
					.find(|process| process.name == name);
				Self::response(json!({
					"name": name,
					"retained": process.is_some(),
					"process": process.map(process_json),
				}))
			},
		}
	}
}

const fn wait_timeout(timeout_ms: Option<u64>) -> Option<StdDuration> {
	match timeout_ms {
		Some(0) | None => None,
		Some(timeout) => Some(StdDuration::from_millis(timeout)),
	}
}

fn process_json(process: v1::ProcessInfo) -> serde_json::Value {
	let pid = process.identity.as_ref().map(|identity| identity.pid);
	let started_at_ms = process
		.identity
		.as_ref()
		.map(|identity| identity.started_at_ms);
	let uptime_ms = started_at_ms.map(|started| now_ms().saturating_sub(started));
	json!({
		"name": process.name,
		"generation": process.generation,
		"state": omp_proto::env::v1::ProcessState::try_from(process.state)
			.map_or_else(|_| format!("state_{}", process.state), |state| format!("{state:?}").to_lowercase()),
		"pid": pid,
		"startedAtMs": started_at_ms,
		"uptimeMs": uptime_ms,
		"status": process.status.map(|status| json!({
			"outcome": omp_proto::env::v1::ExecOutcome::try_from(status.outcome)
				.map_or_else(|_| format!("outcome_{}", status.outcome), |outcome| format!("{outcome:?}").to_lowercase()),
			"exitCode": status.exit_code,
			"signal": status.signal,
			"wallClockMs": status.wall_clock_ms,
		})),
		"logStart": process.log_start_offset,
		"logEnd": process.log_end_offset,
		"restartCount": process.restart_count,
		"consecutiveFailures": process.consecutive_failures,
	})
}

fn owner_process_props(session: &str, agent: &str) -> ValueMap {
	ValueMap {
		fields: BTreeMap::from([
			(String::from("omp/owner-session"), Value {
				kind: Some(value::Kind::String(session.to_owned())),
			}),
			(String::from("omp/owner-agent"), Value {
				kind: Some(value::Kind::String(agent.to_owned())),
			}),
		]),
	}
}

fn process_owner(props: Option<&ValueMap>) -> Option<(&str, &str)> {
	let props = props?;
	let string = |key| {
		props
			.fields
			.get(key)?
			.kind
			.as_ref()
			.and_then(|kind| match kind {
				value::Kind::String(value) => Some(value.as_str()),
				_ => None,
			})
	};
	Some((string("omp/owner-session")?, string("omp/owner-agent")?))
}

fn job_json(job: JobRef) -> serde_json::Value {
	let now = now_ms();
	let owner = match &job.owner {
		JobOwner::NamedProcess { name, generation } => {
			json!({ "kind": "named_process", "name": name, "generation": generation })
		},
		JobOwner::AgentLoop { agent_id } => json!({ "kind": "agent_loop", "agent": agent_id }),
	};
	json!({
		"id": job.id,
		"kind": job.metadata.kind.to_string(),
		"status": job.metadata.status.to_string(),
		"label": job.metadata.label,
		"owner": owner,
		"createdAtMs": job.metadata.created_at_ms,
		"startedAtMs": job.metadata.started_at_ms,
		"settledAtMs": job.metadata.settled_at_ms,
		"ownerSession": job.metadata.owner_session,
		"model": job.metadata.model,
		"result": job.metadata.result,
		"error": job.metadata.error,
		"durationMs": job.metadata.started_at_ms.map(|started| {
			job.metadata.settled_at_ms.unwrap_or(now).saturating_sub(started)
		}),
		"artifact": {
			"description": job.artifact.description,
			"mediaType": job.artifact.media_type,
			"lifetime": job.artifact.lifetime.to_string(),
		},
	})
}

fn message_json(message: PeerMessage) -> serde_json::Value {
	let mode = match message.mode {
		DeliveryMode::Aside => "aside",
		DeliveryMode::Steer => "steer",
		DeliveryMode::NextTurn => "next_turn",
	};
	json!({
		"id": message.id,
		"from": message.from,
		"to": message.to,
		"message": message.text,
		"mode": mode,
		"replyTo": message.reply_to,
		"sentMs": message.sent_ms,
		"sessionId": message.session_id,
	})
}

fn deliveries_json(deliveries: &[omp_agent::DeliveryReceipt]) -> Vec<serde_json::Value> {
	deliveries
		.iter()
		.map(|delivery| {
			json!({
				"to": delivery.to,
				"outcome": delivery.outcome.to_string(),
				"history": delivery.history_uri,
			})
		})
		.collect()
}

fn command_text(application: &str, args: &[Str]) -> String {
	iter::once(application)
		.chain(args.iter().map(Str::as_str))
		.map(shell_word)
		.collect::<Vec<_>>()
		.join(" ")
}
fn compile_regex(pattern: Option<&str>, label: &str) -> Result<Option<Regex>, Fault> {
	pattern
		.map(|pattern| {
			Regex::new(pattern)
				.map_err(|error| fault(format!("invalid {label} regex `{pattern}`: {error}")))
		})
		.transpose()
}

fn append_bounded(buffer: &mut String, text: &str, limit: usize) {
	buffer.push_str(text);
	if buffer.len() <= limit {
		return;
	}
	let mut start = buffer.len().saturating_sub(limit);
	while !buffer.is_char_boundary(start) {
		start = start.saturating_add(1);
	}
	buffer.drain(..start);
}

fn shell_word(word: &str) -> String {
	if !word.is_empty()
		&& word
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || b"-_./:".contains(&byte))
	{
		return word.to_owned();
	}
	format!("'{}'", word.replace('\'', "'\\''"))
}

fn append_key(data: &mut Vec<u8>, key: &str) {
	match key.to_ascii_uppercase().as_str() {
		"ENTER" => data.push(b'\n'),
		"TAB" => data.push(b'\t'),
		"ESCAPE" => data.push(0x1b),
		"CTRL_C" => data.push(0x03),
		"CTRL_D" => data.push(0x04),
		"UP" => data.extend_from_slice(b"\x1b[A"),
		"DOWN" => data.extend_from_slice(b"\x1b[B"),
		"RIGHT" => data.extend_from_slice(b"\x1b[C"),
		"LEFT" => data.extend_from_slice(b"\x1b[D"),
		_ => {},
	}
}

fn select_roster(
	mut records: Vec<AgentRecord>,
	self_id: &str,
	status: Option<ListStatus>,
	limit: usize,
) -> (Vec<AgentRecord>, RosterCounts) {
	records.retain(|record| record.id != self_id);
	let mut counts = RosterCounts {
		running:   records
			.iter()
			.filter(|record| record.status == RegistryStatus::Running)
			.count(),
		idle:      records
			.iter()
			.filter(|record| record.status == RegistryStatus::Idle)
			.count(),
		parked:    records
			.iter()
			.filter(|record| record.status == RegistryStatus::Parked)
			.count(),
		shown:     0,
		truncated: 0,
	};
	records.retain(|record| match status {
		Some(ListStatus::Running) => record.status == RegistryStatus::Running,
		Some(ListStatus::Idle) => record.status == RegistryStatus::Idle,
		Some(ListStatus::Parked) => record.status == RegistryStatus::Parked,
		None => matches!(record.status, RegistryStatus::Running | RegistryStatus::Idle),
	});
	records.sort_by(|left, right| {
		roster_status_order(left.status)
			.cmp(&roster_status_order(right.status))
			.then_with(|| right.last_activity_ms.cmp(&left.last_activity_ms))
			.then_with(|| left.id.cmp(&right.id))
	});
	counts.truncated = records.len().saturating_sub(limit);
	records.truncate(limit);
	counts.shown = records.len();
	(records, counts)
}

const fn roster_status_order(status: RegistryStatus) -> u8 {
	match status {
		RegistryStatus::Running => 0,
		RegistryStatus::Idle => 1,
		RegistryStatus::Parked => 2,
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

fn fault(message: impl Into<Str>) -> Fault {
	Fault { message: message.into() }
}
#[cfg(test)]
mod tests {
	use omp_agent::{AgentHistory, AgentKind, AgentRecord, RegistryStatus};
	use omp_core::{Str, sf};
	use omp_tools::hub::ListStatus;

	use super::{
		DEFAULT_LIST_LIMIT, HUB_WAIT_MATCH_BYTES, TerminalRowReplay, append_bounded, compile_regex,
		select_roster,
	};

	#[test]
	fn terminal_row_replay_applies_cursor_motion_and_carriage_return() {
		let mut replay = TerminalRowReplay::new();
		replay.process(b"one\rTWO\nthree\x1b[1A\rX");
		let lines = replay.into_lines();
		assert_eq!(lines[0], "XWO");
		assert_eq!(lines[1], "three");
	}
	#[test]
	fn hub_patterns_are_validated_regexes_and_wait_buffers_cross_chunk_matches() {
		let pattern = compile_regex(Some(r"ready\s+\d+"), "wait pattern")
			.expect("valid regex")
			.expect("pattern");
		let mut accumulated = String::new();
		append_bounded(&mut accumulated, "rea", HUB_WAIT_MATCH_BYTES);
		assert!(!pattern.is_match(&accumulated));
		append_bounded(&mut accumulated, "dy 42", HUB_WAIT_MATCH_BYTES);
		assert!(pattern.is_match(&accumulated));
		assert!(compile_regex(Some("("), "log filter").is_err());

		let mut bounded = String::new();
		append_bounded(&mut bounded, &"x".repeat(HUB_WAIT_MATCH_BYTES + 10), HUB_WAIT_MATCH_BYTES);
		assert_eq!(bounded.len(), HUB_WAIT_MATCH_BYTES);
	}
	#[test]
	fn default_roster_is_live_bounded_and_reports_parked_count() {
		let mut records = (0..40)
			.map(|index| record(index, RegistryStatus::Running))
			.collect::<Vec<_>>();
		records.extend((40..45).map(|index| record(index, RegistryStatus::Parked)));

		let (shown, counts) = select_roster(records, "self", None, DEFAULT_LIST_LIMIT);

		assert_eq!(shown.len(), DEFAULT_LIST_LIMIT);
		assert!(
			shown
				.iter()
				.all(|record| record.status == RegistryStatus::Running)
		);
		assert_eq!(counts.running, 40);
		assert_eq!(counts.idle, 0);
		assert_eq!(counts.parked, 5);
		assert_eq!(counts.shown, DEFAULT_LIST_LIMIT);
		assert_eq!(counts.truncated, 8);
	}

	#[test]
	fn parked_roster_requires_explicit_filter() {
		let records = vec![record(1, RegistryStatus::Running), record(2, RegistryStatus::Parked)];
		let (shown, counts) = select_roster(records, "self", Some(ListStatus::Parked), 100);
		assert_eq!(shown.len(), 1);
		assert_eq!(shown[0].status, RegistryStatus::Parked);
		assert_eq!(counts.shown, 1);
		assert_eq!(counts.truncated, 0);
	}

	fn record(index: u64, status: RegistryStatus) -> AgentRecord {
		AgentRecord {
			id: sf!("agent-{index}"),
			name: sf!("Agent{index}"),
			kind: AgentKind::Subagent,
			parent: Some(sf!("self")),
			session: sf!("session"),
			depth: 1,
			status,
			activity: Str::default(),
			last_activity_ms: index,
			transcript: None,
			definition: None,
			model: None,
			serving_model: None,
			task: None,
			history: AgentHistory::default(),
		}
	}
}
