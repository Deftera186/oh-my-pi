//! Single runtime-owner command and presence authority for live collaboration.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	time::Duration,
};

use bytes::Bytes;
use flume::{Receiver, Sender};
use omp_agent::{MailboxSender, ReplicationSubscription};
use omp_collab::{
	codec::RelayRoute,
	crypto::{CryptoError, RoomKey},
	guest::{
		GuestReplicaError, GuestReplicaHandle, GuestStateEffects, GuestStateMirror,
		SNAPSHOT_RECORD_MAX,
	},
	host::{
		AuthenticatedPeer, HostAdmission, HostAgentClass, HostAgentRuntime, HostUiAnswer,
		HostUiDispatcher, RemoteOperation, VisibilityClass as HostVisibilityClass, bus_visibility,
		read_transcript_chunk, route_agent_command,
	},
	link::{CollabLink, HostedRoom, RelayEndpoint, WebEndpoint},
	presence::{ConnectionState, PresenceFacts},
	relay::{Handshake, RelayClient, RelayError, RelayInbound, RelayRole, SendDisposition},
};
use omp_core::{RemotePrincipal, Str};
use omp_proto::collab::{
	v1,
	v1::{Bye, Hello, PromptRequest, Welcome, collab_frame},
};
use thiserror::Error;
use tokio::{sync::watch, task::JoinHandle, time, time::error::Elapsed};

use super::{
	host_bridge::{
		HostBridgeError, HostJournalBridge, HostReplicationEvent, SNAPSHOT_CHUNK_SOFT_BYTES,
	},
	remote_admission::enqueue_prompt,
};
const COMMAND_CAPACITY: usize = 16;
const LIVE_PRESENTATION_CAPACITY: usize = 64;
const HOST_OPERATION_CAPACITY: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const WELCOME_TIMEOUT: Duration = Duration::from_secs(30);
const SNAPSHOT_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);
const SNAPSHOT_ENTRY_CHUNK: usize = 256;

/// One visible host agent plus its host-local transcript location.
#[derive(Clone, Debug, PartialEq)]
pub struct HostAgentProjection {
	/// Public registry row sent to guests.
	pub summary:         v1::AgentSummary,
	/// Host-local transcript path used for bounded guest reads.
	pub transcript_path: Option<PathBuf>,
}

/// Coalesced visible registry input for the collaboration owner.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostRegistryUpdate {
	/// Public registry snapshot sent to guests.
	pub snapshot: v1::RegistrySnapshot,
	/// Host-local routing metadata keyed by public agent id.
	pub agents:   HashMap<Str, HostAgentProjection>,
}

impl HostRegistryUpdate {
	/// Builds an update from a public snapshot without transcript locations.
	pub fn public_only(snapshot: v1::RegistrySnapshot) -> Self {
		let agents = snapshot
			.agents
			.iter()
			.cloned()
			.map(|summary| {
				(Str::new(&summary.id), HostAgentProjection { summary, transcript_path: None })
			})
			.collect();
		Self { snapshot, agents }
	}
}

/// One admitted host-owned effect delivered to the interactive app owner.
#[derive(Clone, Debug)]
pub enum HostOperation {
	/// Interrupt the active host turn.
	Abort {
		/// Authenticated guest requesting the interrupt.
		principal: RemotePrincipal,
		/// Guest-supplied audit reason.
		reason:    Str,
	},
	/// Chat with a visible main or subagent.
	AgentChat {
		/// Authenticated guest requesting the operation.
		principal: RemotePrincipal,
		/// Public agent id.
		agent_id:  Str,
		/// Trimmed chat text.
		text:      Str,
	},
	/// Kill a visible main or subagent.
	AgentKill {
		/// Authenticated guest requesting the operation.
		principal: RemotePrincipal,
		/// Public agent id.
		agent_id:  Str,
	},
	/// Revive a visible main or subagent.
	AgentRevive {
		/// Authenticated guest requesting the operation.
		principal: RemotePrincipal,
		/// Public agent id.
		agent_id:  Str,
	},
	/// First accepted answer to a host-owned UI request.
	UiAnswer {
		/// Authenticated guest supplying the answer.
		principal: RemotePrincipal,
		/// Settled request and selected value.
		answer:    HostUiAnswer,
	},
}

/// Single-consumer stream of admitted host effects.
pub struct HostOperationReceiver {
	operations: Receiver<HostOperation>,
}

impl HostOperationReceiver {
	/// Receives the next admitted operation with backpressure.
	pub async fn recv(&self) -> Result<HostOperation, flume::RecvError> {
		self.operations.recv_async().await
	}
}

enum HostPresentationInput {
	Stream(v1::StreamEvent),
	Bus(v1::BusEvent),
	BeginUi { request: v1::UiRequest, reply: Sender<Result<u32, HostLiveError>> },
	CancelUi(u32),
}

/// Clone-cheap producer for bounded live host presentation.
#[derive(Clone)]
pub struct HostLiveHandle {
	state:        watch::Sender<v1::SessionStateUpdate>,
	registry:     watch::Sender<HostRegistryUpdate>,
	presentation: Sender<HostPresentationInput>,
}

impl HostLiveHandle {
	/// Replaces the pending state projection; slow consumers observe the latest.
	pub fn publish_state(&self, state: v1::SessionStateUpdate) {
		self.state.send_replace(state);
	}

	/// Replaces the pending visible registry and transcript routing metadata.
	pub fn publish_registry(&self, registry: HostRegistryUpdate) {
		self.registry.send_replace(registry);
	}

	/// Delivers one ordered stream event through the bounded presentation lane.
	pub async fn send_stream(&self, event: v1::StreamEvent) -> Result<(), HostLiveError> {
		self
			.presentation
			.send_async(HostPresentationInput::Stream(event))
			.await
			.map_err(|_| HostLiveError::Stopped)
	}

	/// Delivers one ordered public task-bus event through the bounded lane.
	pub async fn send_bus(&self, event: v1::BusEvent) -> Result<(), HostLiveError> {
		if bus_visibility(event.channel) != HostVisibilityClass::PublicPresentation {
			return Err(HostLiveError::PrivateBusChannel);
		}
		self
			.presentation
			.send_async(HostPresentationInput::Bus(event))
			.await
			.map_err(|_| HostLiveError::Stopped)
	}

	/// Begins a host-owned UI request and returns its collaboration request id.
	pub async fn begin_ui(&self, request: v1::UiRequest) -> Result<u32, HostLiveError> {
		let (reply, response) = flume::bounded(1);
		self
			.presentation
			.send_async(HostPresentationInput::BeginUi { request, reply })
			.await
			.map_err(|_| HostLiveError::Stopped)?;
		response
			.recv_async()
			.await
			.map_err(|_| HostLiveError::Stopped)?
	}

	/// Cancels one active host-owned UI request.
	pub async fn cancel_ui(&self, request_id: u32) -> Result<(), HostLiveError> {
		self
			.presentation
			.send_async(HostPresentationInput::CancelUi(request_id))
			.await
			.map_err(|_| HostLiveError::Stopped)
	}
}

/// Host live-input failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum HostLiveError {
	/// The collaboration owner has stopped.
	#[error("collaboration live owner has stopped")]
	Stopped,
	/// A UI request had no writable guest recipient.
	#[error("collaboration UI request has no writable guest")]
	NoWritableGuest,
	/// A host-local or unknown bus channel cannot enter peer presentation.
	#[error("collaboration bus channel is not public")]
	PrivateBusChannel,
}

/// Agent-owned services required by an authoritative hosted room.
pub struct HostRuntime {
	bridge:             HostJournalBridge,
	history:            Vec<v1::JournalRecord>,
	snapshot_watermark: u64,
	header:             v1::SessionHeader,
	state:              v1::SessionStateUpdate,
	agents:             v1::RegistrySnapshot,
	agent_routes:       HashMap<Str, HostAgentProjection>,
	mailbox:            MailboxSender,
	state_updates:      watch::Receiver<v1::SessionStateUpdate>,
	registry_updates:   watch::Receiver<HostRegistryUpdate>,
	presentation:       Receiver<HostPresentationInput>,
	operations:         Sender<HostOperation>,
	ui:                 HostUiDispatcher,
}

/// App-facing endpoints paired with one host runtime.
pub struct HostRuntimePorts {
	/// Producer for coalesced state/registry and bounded presentation events.
	pub live:       HostLiveHandle,
	/// Single-consumer stream of authenticated host operations.
	pub operations: HostOperationReceiver,
}

impl HostRuntime {
	/// Captures the race-free journal snapshot and creates bounded live ports.
	pub fn new(
		subscription: ReplicationSubscription,
		header: v1::SessionHeader,
		state: v1::SessionStateUpdate,
		agents: v1::RegistrySnapshot,
		mailbox: MailboxSender,
	) -> Result<(Self, HostRuntimePorts), HostBridgeError> {
		let mut bridge = HostJournalBridge::from_subscription(subscription);
		let chunks = bridge.snapshot_chunks()?;
		let snapshot_watermark = chunks
			.last()
			.map_or(0, |chunk| chunk.host_revision_watermark);
		let history = chunks.into_iter().flat_map(|chunk| chunk.entries).collect();
		let registry = HostRegistryUpdate::public_only(agents.clone());
		let agent_routes = registry.agents.clone();
		let (state_sender, state_updates) = watch::channel(state.clone());
		let (registry_sender, registry_updates) = watch::channel(registry);
		let (presentation_sender, presentation) = flume::bounded(LIVE_PRESENTATION_CAPACITY);
		let (operations, operation_receiver) = flume::bounded(HOST_OPERATION_CAPACITY);
		let runtime = Self {
			bridge,
			history,
			snapshot_watermark,
			header,
			state,
			agents,
			agent_routes,
			mailbox,
			state_updates,
			registry_updates,
			presentation,
			operations,
			ui: HostUiDispatcher::default(),
		};
		let ports = HostRuntimePorts {
			live:       HostLiveHandle {
				state:        state_sender,
				registry:     registry_sender,
				presentation: presentation_sender,
			},
			operations: HostOperationReceiver { operations: operation_receiver },
		};
		Ok((runtime, ports))
	}
}

/// Validated options for starting an authoritative room.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostOptions {
	/// relay origin.
	pub relay: RelayEndpoint,
	/// Browser UI origin used only to render fragment links.
	pub web:   WebEndpoint,
}

/// One operation serialized through the sole live collaboration owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollabOwnerCommand {
	/// Start hosting a writable room.
	Start(HostOptions),
	/// Render the read-only link for an existing hosted room.
	View,
	/// Return current role, connection, and participant facts.
	Status,
	/// End an authoritative hosted room.
	Stop,
	/// Join a parsed room link under the resolved local identity.
	Join {
		/// Strictly parsed room link and credentials.
		link:         CollabLink,
		/// Trimmed setting/OS/fallback participant name.
		display_name: Str,
	},
	/// Submit a writable guest prompt through the host authority.
	Prompt {
		/// Prompt text after expanding staged text attachments.
		text:   Str,
		/// Bounded staged image attachments.
		images: Vec<RemoteImage>,
	},
	/// Interrupt the active host turn through a writable link.
	Abort {
		/// Guest-supplied audit reason.
		reason: Str,
	},
	/// Control one visible host agent through a writable link.
	AgentCommand {
		/// Requested chat, kill, or revive operation.
		command:  v1::agent_command::Command,
		/// Public agent id.
		agent_id: Str,
		/// Chat text; absent for kill and revive.
		text:     Option<Str>,
	},
	/// Answer one active host-owned UI request.
	UiResponse {
		/// Host-assigned request id.
		request_id: u32,
		/// Selected/editor value; `None` is a genuine cancel.
		value:      Option<Str>,
	},
	/// Fetch a bounded transcript increment for one visible agent.
	TranscriptRequest {
		/// Guest request correlation id.
		request_id: u32,
		/// Public agent id.
		agent_id:   Str,
		/// First byte not yet present locally.
		from_byte:  u64,
	},
	/// Leave a replica and restore the prior local session.
	Leave,
}

/// One remote prompt image loaded by the guest UI boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteImage {
	/// Exact image bytes.
	pub data:      Bytes,
	/// Detected media type.
	pub mime_type: Str,
}

/// Owner-produced result rendered by slash-command adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollabCommandResult {
	/// Current presence facts, absent after stop/leave.
	pub presence:      Option<PresenceFacts>,
	/// Writable compact room link when hosting.
	pub full_link:     Option<Str>,
	/// Read-only compact room link when hosting.
	pub view_link:     Option<Str>,
	/// Writable browser deep link when hosting.
	pub web_link:      Option<Str>,
	/// Read-only browser deep link when hosting.
	pub web_view_link: Option<Str>,
}

impl CollabCommandResult {
	/// Constructs an inactive result after stop or leave.
	pub const fn inactive() -> Self {
		Self {
			presence:      None,
			full_link:     None,
			view_link:     None,
			web_link:      None,
			web_view_link: None,
		}
	}
}

struct OwnerRequest {
	command: CollabOwnerCommand,
	reply:   Sender<Result<CollabCommandResult, CollabCommandFault>>,
}
/// Latest coalesced state and registry consumed from host frames.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuestLiveProjection {
	/// Latest authoritative host state.
	pub state:   Option<v1::SessionStateUpdate>,
	/// Latest visible-agent registry.
	pub agents:  v1::RegistrySnapshot,
	/// UI effects derived while consuming the latest state.
	pub effects: Option<GuestStateEffects>,
}

/// One ordered non-coalescible presentation frame consumed by the guest owner.
#[derive(Clone, Debug, PartialEq)]
pub enum GuestPresentationEvent {
	/// A welcome began a new snapshot; dismiss all prior transient UI.
	Resync,
	/// Incremental host stream presentation.
	Stream(v1::StreamEvent),
	/// Host-owned UI request to present.
	UiRequest(v1::UiRequest),
	/// Host-owned UI request to dismiss.
	UiRequestEnd(v1::UiRequestEnd),
	/// Bounded transcript read response.
	Transcript(v1::TranscriptChunk),
	/// Public task-bus lifecycle or progress event.
	Bus(v1::BusEvent),
	/// Targeted host protocol error.
	Error(v1::ErrorMessage),
}

/// Single-UI-owner receiver for ordered guest presentation frames.
#[derive(Clone)]
pub struct GuestPresentationReceiver {
	events: Receiver<GuestPresentationEvent>,
}

impl GuestPresentationReceiver {
	/// Receives the next ordered presentation frame with backpressure.
	pub async fn recv(&self) -> Result<GuestPresentationEvent, flume::RecvError> {
		self.events.recv_async().await
	}
}

mod command_handle {
	use tokio::sync::watch::Receiver;

	use super::*;

	/// Clone-cheap command/presence handle installed only when the production
	/// collaboration owner is constructed.
	#[derive(Clone)]
	pub struct CollabCommandHandle {
		pub(super) commands:     Sender<OwnerRequest>,
		pub(super) presence:     Receiver<Option<PresenceFacts>>,
		pub(super) replica:      Option<GuestReplicaHandle>,
		pub(super) guest_live:   Receiver<GuestLiveProjection>,
		pub(super) guest_events: GuestPresentationReceiver,
	}

	impl CollabCommandHandle {
		/// Requests a serialized owner operation and awaits its settled result.
		pub async fn request(
			&self,
			command: CollabOwnerCommand,
		) -> Result<CollabCommandResult, CollabCommandFault> {
			let (reply, result) = flume::bounded(1);
			self
				.commands
				.send_async(OwnerRequest { command, reply })
				.await
				.map_err(|_| CollabCommandFault::OwnerStopped)?;
			result
				.recv_async()
				.await
				.map_err(|_| CollabCommandFault::OwnerStopped)?
		}

		/// Returns the most recently published role/connection/participant facts.
		pub fn presence(&self) -> Option<PresenceFacts> {
			*self.presence.borrow()
		}

		/// Subscribes to role and presence changes for command filtering and
		/// status rendering.
		pub fn subscribe_presence(&self) -> Receiver<Option<PresenceFacts>> {
			self.presence.clone()
		}

		/// Returns the guest transcript projection handle when replica storage
		/// was attached to this owner.
		pub fn guest_replica(&self) -> Option<GuestReplicaHandle> {
			self.replica.clone()
		}

		/// Returns the latest coalesced host state and visible registry.
		pub fn guest_live(&self) -> GuestLiveProjection {
			self.guest_live.borrow().clone()
		}

		/// Subscribes to coalesced host state and registry updates.
		pub fn subscribe_guest_live(&self) -> Receiver<GuestLiveProjection> {
			self.guest_live.clone()
		}

		/// Returns the ordered presentation stream for the single guest UI owner.
		pub fn guest_presentation(&self) -> GuestPresentationReceiver {
			self.guest_events.clone()
		}
	}
}

pub use command_handle::CollabCommandHandle;

/// Receiving half retained by the production host/guest lifecycle owner.
pub struct CollabSessionAuthority {
	commands:         Receiver<OwnerRequest>,
	presence:         watch::Sender<Option<PresenceFacts>>,
	replica:          Option<GuestReplicaHandle>,
	host:             Option<HostRuntime>,
	guest_mirror:     GuestStateMirror,
	guest_live:       watch::Sender<GuestLiveProjection>,
	guest_projection: GuestLiveProjection,
	guest_events:     Sender<GuestPresentationEvent>,
}

impl CollabSessionAuthority {
	/// Constructs the sole authority and its clone-cheap UI handle.
	pub fn new() -> (Self, CollabCommandHandle) {
		Self::with_guest_replica(None)
	}

	/// Constructs the sole authority with guest replica storage attached.
	pub fn with_guest_replica(replica: Option<GuestReplicaHandle>) -> (Self, CollabCommandHandle) {
		Self::with_runtimes(replica, None)
	}

	/// Constructs the sole authority with guest storage and host services.
	pub fn with_runtimes(
		replica: Option<GuestReplicaHandle>,
		host: Option<HostRuntime>,
	) -> (Self, CollabCommandHandle) {
		let (commands, requests) = flume::bounded(COMMAND_CAPACITY);
		let (presence, observed_presence) = watch::channel(None);
		let guest_projection = GuestLiveProjection::default();
		let (guest_live, observed_guest_live) = watch::channel(guest_projection.clone());
		let (guest_events, observed_guest_events) = flume::bounded(LIVE_PRESENTATION_CAPACITY);
		(
			Self {
				commands: requests,
				presence,
				replica: replica.clone(),
				host,
				guest_mirror: GuestStateMirror::default(),
				guest_live,
				guest_projection,
				guest_events,
			},
			CollabCommandHandle {
				commands,
				presence: observed_presence,
				replica,
				guest_live: observed_guest_live,
				guest_events: GuestPresentationReceiver { events: observed_guest_events },
			},
		)
	}

	/// Receives the next serialized owner request.
	pub async fn recv(&self) -> Result<CollabOwnerRequest, CollabCommandFault> {
		let request = self
			.commands
			.recv_async()
			.await
			.map_err(|_| CollabCommandFault::OwnerStopped)?;
		Ok(CollabOwnerRequest { command: request.command, reply: Some(request.reply) })
	}

	/// Atomically publishes role/connection/participant changes.
	pub fn publish_presence(&self, facts: Option<PresenceFacts>) {
		self.presence.send_replace(facts);
	}
}
/// Starts the native relay-backed command owner.
///
/// The returned task owns every active relay socket. Dropping all command
/// handles ends the loop and closes the current room.
pub fn spawn_session_owner(authority: CollabSessionAuthority) -> JoinHandle<()> {
	tokio::spawn(authority.run())
}

enum ActiveSession {
	Host {
		relay:     RelayClient,
		admission: HostAdmission,
		peers:     HashMap<u32, AuthenticatedPeer>,
		sequence:  u64,
		result:    CollabCommandResult,
	},
	Guest {
		relay:             RelayClient,
		sequence:          u64,
		hello:             v1::CollabFrame,
		room_id:           Str,
		replica:           GuestReplicaHandle,
		result:            CollabCommandResult,
		snapshot_deadline: Option<time::Instant>,
	},
}

impl ActiveSession {
	fn result(&self) -> &CollabCommandResult {
		match self {
			Self::Host { result, .. } | Self::Guest { result, .. } => result,
		}
	}

	async fn close(&mut self, reason: &'static str) -> Result<(), CollabCommandFault> {
		let relay = match self {
			Self::Host { relay, .. } | Self::Guest { relay, .. } => relay,
		};
		let frame = v1::CollabFrame {
			protocol_revision: omp_collab::PROTOCOL_REVISION,
			sequence: 1,
			payload: Some(collab_frame::Payload::Bye(Bye { reason: reason.to_owned() })),
			..Default::default()
		};
		let _ = relay.send(RelayRoute { peer_id: 0 }, &frame).await?;
		relay.close().await?;
		Ok(())
	}
}

impl CollabSessionAuthority {
	async fn run(mut self) {
		let mut active = None;
		let mut state_updates = self.host.as_ref().map(|host| host.state_updates.clone());
		let mut registry_updates = self.host.as_ref().map(|host| host.registry_updates.clone());
		let presentation = self.host.as_ref().map(|host| host.presentation.clone());
		loop {
			enum Input {
				Request(Result<CollabOwnerRequest, CollabCommandFault>),
				Inbound(Result<Option<RelayInbound>, RelayError>),
				Replication(Result<HostReplicationEvent, HostBridgeError>),
				State(v1::SessionStateUpdate),
				Registry(HostRegistryUpdate),
				Presentation(HostPresentationInput),
				SnapshotTimeout,
			}
			let input = match active.as_mut() {
				Some(ActiveSession::Guest { relay, snapshot_deadline, .. }) => tokio::select! {
					request = self.recv() => Input::Request(request),
					inbound = relay.receive() => Input::Inbound(inbound),
					replication = recv_replication(self.host.as_ref()) => Input::Replication(replication),
					state = recv_state(&mut state_updates) => Input::State(state),
					registry = recv_registry(&mut registry_updates) => Input::Registry(registry),
					presentation = recv_presentation(presentation.as_ref()) => Input::Presentation(presentation),
					() = wait_snapshot_deadline(*snapshot_deadline) => Input::SnapshotTimeout,
				},
				Some(ActiveSession::Host { relay, .. }) => tokio::select! {
					request = self.recv() => Input::Request(request),
					inbound = relay.receive() => Input::Inbound(inbound),
					replication = recv_replication(self.host.as_ref()) => Input::Replication(replication),
					state = recv_state(&mut state_updates) => Input::State(state),
					registry = recv_registry(&mut registry_updates) => Input::Registry(registry),
					presentation = recv_presentation(presentation.as_ref()) => Input::Presentation(presentation),
				},
				None => tokio::select! {
					request = self.recv() => Input::Request(request),
					replication = recv_replication(self.host.as_ref()) => Input::Replication(replication),
					state = recv_state(&mut state_updates) => Input::State(state),
					registry = recv_registry(&mut registry_updates) => Input::Registry(registry),
					presentation = recv_presentation(presentation.as_ref()) => Input::Presentation(presentation),
				},
			};
			match input {
				Input::Request(Ok(request)) => {
					let clears_presence = matches!(
						request.command(),
						CollabOwnerCommand::Start(_) | CollabOwnerCommand::Join { .. }
					);
					let result = self.apply(request.command(), &mut active).await;
					if clears_presence && result.is_err() {
						self.publish_presence(None);
					}
					let _ = request.settle(result);
				},
				Input::Request(Err(_)) => break,
				Input::Inbound(Ok(inbound)) => {
					if self.apply_inbound(inbound, &mut active).await.is_err() {
						self.publish_presence(None);
						active = None;
					}
				},
				Input::Inbound(Err(_)) => {
					self.publish_presence(None);
					active = None;
				},
				Input::Replication(Ok(event)) => {
					if self.apply_replication(event, &mut active).await.is_err() {
						self.publish_presence(None);
						active = None;
					}
				},
				Input::Replication(Err(_)) => {
					self.host = None;
					if matches!(active, Some(ActiveSession::Host { .. })) {
						self.publish_presence(None);
						active = None;
					}
				},
				Input::State(state) => {
					if self.apply_host_state(state, &mut active).await.is_err() {
						self.publish_presence(None);
						active = None;
					}
				},
				Input::Registry(registry) => {
					if self
						.apply_host_registry(registry, &mut active)
						.await
						.is_err()
					{
						self.publish_presence(None);
						active = None;
					}
				},
				Input::Presentation(presentation) => {
					if self
						.apply_host_presentation(presentation, &mut active)
						.await
						.is_err()
					{
						self.publish_presence(None);
						active = None;
					}
				},
				Input::SnapshotTimeout => {
					if self.restart_guest_snapshot(&mut active).await.is_err() {
						self.publish_presence(None);
						active = None;
					}
				},
			}
		}
		if let Some(mut session) = active {
			let _ = session.close("runtime stopped").await;
		}
	}

	async fn apply_inbound(
		&mut self,
		inbound: Option<RelayInbound>,
		active: &mut Option<ActiveSession>,
	) -> Result<(), CollabCommandFault> {
		match active {
			Some(ActiveSession::Host { .. }) => self.apply_host_inbound(inbound, active).await,
			Some(ActiveSession::Guest { .. }) => self.apply_guest_inbound(inbound, active).await,
			None => Ok(()),
		}
	}

	async fn apply_guest_inbound(
		&mut self,
		inbound: Option<RelayInbound>,
		active: &mut Option<ActiveSession>,
	) -> Result<(), CollabCommandFault> {
		let Some(ActiveSession::Guest {
			relay,
			hello,
			room_id,
			replica,
			result,
			snapshot_deadline,
			..
		}) = active.as_mut()
		else {
			return Ok(());
		};
		let Some(inbound) = inbound else {
			let read_only = result.presence.is_some_and(PresenceFacts::read_only);
			let presence = PresenceFacts::guest(ConnectionState::Reconnecting, 0, read_only);
			result.presence = Some(presence);
			*snapshot_deadline = None;
			self.publish_presence(Some(presence));
			reconnect(relay).await?;
			return send_required(relay, RelayRoute { peer_id: 0 }, hello).await;
		};
		let RelayInbound::Frame(frame) = inbound else {
			return Ok(());
		};
		match frame.frame.payload {
			Some(collab_frame::Payload::Welcome(welcome)) => {
				let read_only = welcome.read_only;
				self
					.guest_events
					.send_async(GuestPresentationEvent::Resync)
					.await
					.map_err(|_| CollabCommandFault::GuestPresentationStopped)?;
				begin_guest_snapshot(replica, room_id.clone(), &welcome).await?;
				if let Some(state) = welcome.initial_state.clone() {
					self.consume_guest_state(state);
				}
				if let Some(agents) = welcome.initial_agents.clone() {
					self.consume_guest_registry(agents);
				}
				let participant_count = welcome
					.initial_state
					.as_ref()
					.map_or(1, |state| state.participants.len().max(1));
				let presence =
					PresenceFacts::guest(ConnectionState::Connecting, participant_count, read_only);
				result.presence = Some(presence);
				*snapshot_deadline = Some(time::Instant::now() + SNAPSHOT_PROGRESS_TIMEOUT);
				self.publish_presence(Some(presence));
			},
			Some(collab_frame::Payload::SnapshotChunk(chunk)) => {
				let projection = replica.push_snapshot_chunk(chunk).await?;
				if projection.ready && !projection.gap {
					*snapshot_deadline = None;
					let read_only = result.presence.is_some_and(PresenceFacts::read_only);
					let participants = result.presence.map_or(1, PresenceFacts::participant_count);
					let presence =
						PresenceFacts::guest(ConnectionState::Connected, participants, read_only);
					result.presence = Some(presence);
					self.publish_presence(Some(presence));
				} else {
					*snapshot_deadline = Some(time::Instant::now() + SNAPSHOT_PROGRESS_TIMEOUT);
				}
			},
			Some(collab_frame::Payload::JournalRecord(record)) => {
				if replica.append_live(record).await.is_err() {
					*snapshot_deadline = None;
					return send_required(relay, RelayRoute { peer_id: 0 }, hello).await;
				}
			},
			Some(collab_frame::Payload::State(state)) => {
				let read_only = result.presence.is_some_and(PresenceFacts::read_only);
				self.consume_guest_state(state.clone());
				let connection = result
					.presence
					.map_or(ConnectionState::Connecting, PresenceFacts::connection);
				let presence =
					PresenceFacts::guest(connection, state.participants.len().max(1), read_only);
				result.presence = Some(presence);
				self.publish_presence(Some(presence));
			},
			Some(collab_frame::Payload::Agents(agents)) => {
				self.consume_guest_registry(agents);
			},
			Some(collab_frame::Payload::Event(event)) => {
				self
					.guest_events
					.send_async(GuestPresentationEvent::Stream(event))
					.await
					.map_err(|_| CollabCommandFault::GuestPresentationStopped)?;
			},
			Some(collab_frame::Payload::UiRequest(request)) => {
				self
					.guest_events
					.send_async(GuestPresentationEvent::UiRequest(request))
					.await
					.map_err(|_| CollabCommandFault::GuestPresentationStopped)?;
			},
			Some(collab_frame::Payload::UiRequestEnd(end)) => {
				self
					.guest_events
					.send_async(GuestPresentationEvent::UiRequestEnd(end))
					.await
					.map_err(|_| CollabCommandFault::GuestPresentationStopped)?;
			},
			Some(collab_frame::Payload::Transcript(transcript)) => {
				self
					.guest_events
					.send_async(GuestPresentationEvent::Transcript(transcript))
					.await
					.map_err(|_| CollabCommandFault::GuestPresentationStopped)?;
			},
			Some(collab_frame::Payload::BusEvent(event)) => {
				self
					.guest_events
					.send_async(GuestPresentationEvent::Bus(event))
					.await
					.map_err(|_| CollabCommandFault::GuestPresentationStopped)?;
			},
			Some(collab_frame::Payload::Error(error)) => {
				self
					.guest_events
					.send_async(GuestPresentationEvent::Error(error))
					.await
					.map_err(|_| CollabCommandFault::GuestPresentationStopped)?;
			},
			Some(collab_frame::Payload::Bye(_)) => {
				self.publish_presence(None);
				result.presence = None;
				relay.close().await?;
			},
			_ => {},
		}
		Ok(())
	}

	fn consume_guest_state(&mut self, state: v1::SessionStateUpdate) {
		let effects = self.guest_mirror.apply_state(state.clone());
		self.guest_projection.state = Some(state);
		self.guest_projection.effects = Some(effects);
		self.guest_live.send_replace(self.guest_projection.clone());
	}

	fn consume_guest_registry(&mut self, agents: v1::RegistrySnapshot) {
		self.guest_mirror.apply_registry(agents.clone());
		self.guest_projection.agents = agents;
		self.guest_live.send_replace(self.guest_projection.clone());
	}

	async fn apply_host_inbound(
		&mut self,
		inbound: Option<RelayInbound>,
		active: &mut Option<ActiveSession>,
	) -> Result<(), CollabCommandFault> {
		let Some(ActiveSession::Host { relay, admission, peers, sequence, result }) = active.as_mut()
		else {
			return Ok(());
		};
		let Some(inbound) = inbound else {
			peers.clear();
			let state = {
				let runtime = self
					.host
					.as_mut()
					.ok_or(CollabCommandFault::HostUnavailable)?;
				runtime.state.participants = host_participants(&runtime.state, peers);
				runtime.state.clone()
			};
			let reconnecting = PresenceFacts::host(ConnectionState::Reconnecting, 0);
			result.presence = Some(reconnecting);
			self.publish_presence(Some(reconnecting));
			reconnect(relay).await?;
			*sequence = sequence.saturating_add(1);
			send_required(
				relay,
				RelayRoute { peer_id: 0 },
				&payload_frame(*sequence, collab_frame::Payload::State(state)),
			)
			.await?;
			let connected = PresenceFacts::host(ConnectionState::Connected, 0);
			result.presence = Some(connected);
			self.publish_presence(Some(connected));
			return Ok(());
		};
		match inbound {
			RelayInbound::PeerJoined(_) => {},
			RelayInbound::PeerLeft(peer) => {
				peers.remove(&peer.peer_id);
				let runtime = self
					.host
					.as_mut()
					.ok_or(CollabCommandFault::HostUnavailable)?;
				runtime.state.participants = host_participants(&runtime.state, peers);
				*sequence = sequence.saturating_add(1);
				send_required(
					relay,
					RelayRoute { peer_id: 0 },
					&payload_frame(*sequence, collab_frame::Payload::State(runtime.state.clone())),
				)
				.await?;
				let presence = PresenceFacts::host(ConnectionState::Connected, peers.len());
				result.presence = Some(presence);
				self.publish_presence(Some(presence));
			},
			RelayInbound::Frame(routed) => {
				let peer_id = routed.route.peer_id;
				let Some(payload) = routed.frame.payload else {
					return Ok(());
				};
				if let collab_frame::Payload::Hello(hello) = &payload {
					let peer = match admission.authenticate(peer_id, hello) {
						Ok(peer) => peer,
						Err(error) => {
							*sequence = sequence.saturating_add(1);
							send_required(
								relay,
								RelayRoute { peer_id },
								&error_frame(*sequence, error.to_string()),
							)
							.await?;
							return Ok(());
						},
					};
					peers.insert(peer_id, peer.clone());
					let runtime = self
						.host
						.as_mut()
						.ok_or(CollabCommandFault::HostUnavailable)?;
					runtime.state.participants = host_participants(&runtime.state, peers);
					let header = runtime.header.clone();
					let state = runtime.state.clone();
					let agents = runtime.agents.clone();
					let snapshot_watermark = runtime.snapshot_watermark;
					let pending_ui = runtime.ui.replay_for_join(peer_id, &peer);
					let history = &runtime.history;
					if history.len() > SNAPSHOT_RECORD_MAX {
						return Err(CollabCommandFault::SnapshotTooLarge);
					}
					let welcome = Welcome {
						protocol_revision: omp_collab::PROTOCOL_REVISION,
						header:            Some(header),
						initial_state:     Some(state.clone()),
						initial_agents:    Some(agents),
						total_entry_count: u32::try_from(history.len())
							.map_err(|_| CollabCommandFault::SnapshotTooLarge)?,
						read_only:         peer.read_only(),
					};
					*sequence = sequence.saturating_add(1);
					send_required(
						relay,
						RelayRoute { peer_id },
						&Handshake::welcome(*sequence, welcome),
					)
					.await?;
					let chunks = snapshot_entries(&history);
					if chunks.is_empty() {
						*sequence = sequence.saturating_add(1);
						send_required(
							relay,
							RelayRoute { peer_id },
							&snapshot_frame(*sequence, Vec::new(), true, snapshot_watermark),
						)
						.await?;
					} else {
						let chunk_count = chunks.len();
						for (index, entries) in chunks.into_iter().enumerate() {
							*sequence = sequence.saturating_add(1);
							send_required(
								relay,
								RelayRoute { peer_id },
								&snapshot_frame(
									*sequence,
									entries,
									index + 1 == chunk_count,
									snapshot_watermark,
								),
							)
							.await?;
						}
					}
					for targeted in pending_ui {
						*sequence = sequence.saturating_add(1);
						let mut frame = targeted.frame;
						frame.sequence = *sequence;
						send_required(relay, RelayRoute { peer_id: targeted.peer_id }, &frame).await?;
					}
					*sequence = sequence.saturating_add(1);
					send_required(
						relay,
						RelayRoute { peer_id: 0 },
						&payload_frame(*sequence, collab_frame::Payload::State(state)),
					)
					.await?;
					let presence = PresenceFacts::host(ConnectionState::Connected, peers.len());
					result.presence = Some(presence);
					self.publish_presence(Some(presence));
					return Ok(());
				}
				let peer = match peers.get(&peer_id) {
					Some(peer) => peer,
					None => {
						*sequence = sequence.saturating_add(1);
						send_required(
							relay,
							RelayRoute { peer_id },
							&error_frame(*sequence, "authenticate before sending operations".to_owned()),
						)
						.await?;
						return Ok(());
					},
				};
				if let collab_frame::Payload::TranscriptRequest(request) = &payload {
					let runtime = self
						.host
						.as_ref()
						.ok_or(CollabCommandFault::HostUnavailable)?;
					let transcript = runtime
						.agent_routes
						.get(request.agent_id.as_str())
						.and_then(|agent| agent.transcript_path.as_deref())
						.map_or_else(
							|| v1::TranscriptChunk {
								request_id: request.request_id,
								text_utf8:  Bytes::new(),
								new_size:   request.from_byte,
								error:      Some("no transcript available".to_owned()),
							},
							|path| transcript_response(path, request),
						);
					*sequence = sequence.saturating_add(1);
					send_required(
						relay,
						RelayRoute { peer_id },
						&payload_frame(*sequence, collab_frame::Payload::Transcript(transcript)),
					)
					.await?;
					return Ok(());
				}
				let mutation = match admission.admit_mutation(peer, &payload) {
					Ok(mutation) => mutation,
					Err(error) => {
						*sequence = sequence.saturating_add(1);
						send_required(
							relay,
							RelayRoute { peer_id },
							&error_frame(*sequence, error.to_string()),
						)
						.await?;
						return Ok(());
					},
				};
				let operation_result = match &mutation.operation {
					RemoteOperation::Prompt(_) => enqueue_prompt(
						&self
							.host
							.as_ref()
							.ok_or(CollabCommandFault::HostUnavailable)?
							.mailbox,
						omp_agent::InterruptClass::Immediate,
						mutation.clone(),
					)
					.map_err(|error| error.to_string()),
					RemoteOperation::Abort(request) => self
						.host
						.as_ref()
						.ok_or(CollabCommandFault::HostUnavailable)?
						.operations
						.try_send(HostOperation::Abort {
							principal: mutation.principal.clone(),
							reason:    Str::new(request.reason.trim()),
						})
						.map_err(|error| HostOperationRouteError::from(error).to_string()),
					RemoteOperation::AgentCommand(command) => {
						let runtime = self
							.host
							.as_ref()
							.ok_or(CollabCommandFault::HostUnavailable)?;
						let router = HostOperationRouter {
							agents:     &runtime.agent_routes,
							operations: &runtime.operations,
							principal:  mutation.principal.clone(),
						};
						route_agent_command(peer, command, &router).map_err(|error| error.to_string())
					},
					RemoteOperation::UiResponse(response) => {
						async {
							let runtime = self
								.host
								.as_mut()
								.ok_or_else(|| CollabCommandFault::HostUnavailable.to_string())?;
							let settled = runtime
								.ui
								.answer(
									peer_id,
									peer,
									(**response).clone(),
									peers.iter().map(|(id, peer)| (*id, peer)),
								)
								.map_err(|error| error.to_string())?;
							let Some((answer, cleanup)) = settled else {
								return Ok(());
							};
							runtime
								.operations
								.send_async(HostOperation::UiAnswer {
									principal: mutation.principal.clone(),
									answer,
								})
								.await
								.map_err(|error| error.to_string())?;
							for targeted in cleanup {
								*sequence = sequence.saturating_add(1);
								let mut frame = targeted.frame;
								frame.sequence = *sequence;
								send_required(relay, RelayRoute { peer_id: targeted.peer_id }, &frame)
									.await
									.map_err(|error| error.to_string())?;
							}
							Ok(())
						}
						.await
					},
				};
				if let Err(error) = operation_result {
					*sequence = sequence.saturating_add(1);
					send_required(relay, RelayRoute { peer_id }, &error_frame(*sequence, error)).await?;
				}
			},
		}
		Ok(())
	}

	async fn apply_host_state(
		&mut self,
		state: v1::SessionStateUpdate,
		active: &mut Option<ActiveSession>,
	) -> Result<(), CollabCommandFault> {
		let runtime = self
			.host
			.as_mut()
			.ok_or(CollabCommandFault::HostUnavailable)?;
		let state = match active {
			Some(ActiveSession::Host { peers, .. }) => {
				let mut state = state;
				state.participants = host_participants(&state, peers);
				state
			},
			_ => state,
		};
		runtime.state = state.clone();
		if let Some(ActiveSession::Host { relay, sequence, .. }) = active {
			*sequence = sequence.saturating_add(1);
			send_required(
				relay,
				RelayRoute { peer_id: 0 },
				&payload_frame(*sequence, collab_frame::Payload::State(state)),
			)
			.await?;
		}
		Ok(())
	}

	async fn apply_host_registry(
		&mut self,
		registry: HostRegistryUpdate,
		active: &mut Option<ActiveSession>,
	) -> Result<(), CollabCommandFault> {
		let runtime = self
			.host
			.as_mut()
			.ok_or(CollabCommandFault::HostUnavailable)?;
		runtime.agents = registry.snapshot.clone();
		runtime.agent_routes = registry
			.snapshot
			.agents
			.iter()
			.filter_map(|summary| {
				registry
					.agents
					.get(summary.id.as_str())
					.filter(|projection| &projection.summary == summary)
					.cloned()
					.map(|projection| (Str::new(&summary.id), projection))
			})
			.collect();
		if let Some(ActiveSession::Host { relay, sequence, .. }) = active {
			*sequence = sequence.saturating_add(1);
			send_required(
				relay,
				RelayRoute { peer_id: 0 },
				&payload_frame(*sequence, collab_frame::Payload::Agents(registry.snapshot)),
			)
			.await?;
		}
		Ok(())
	}

	async fn apply_host_presentation(
		&mut self,
		input: HostPresentationInput,
		active: &mut Option<ActiveSession>,
	) -> Result<(), CollabCommandFault> {
		match input {
			HostPresentationInput::Stream(event) => {
				if let Some(ActiveSession::Host { relay, sequence, .. }) = active {
					*sequence = sequence.saturating_add(1);
					send_required(
						relay,
						RelayRoute { peer_id: 0 },
						&payload_frame(*sequence, collab_frame::Payload::Event(event)),
					)
					.await?;
				}
			},
			HostPresentationInput::Bus(event) => {
				if let Some(ActiveSession::Host { relay, sequence, .. }) = active {
					*sequence = sequence.saturating_add(1);
					send_required(
						relay,
						RelayRoute { peer_id: 0 },
						&payload_frame(*sequence, collab_frame::Payload::BusEvent(event)),
					)
					.await?;
				}
			},
			HostPresentationInput::BeginUi { request, reply } => {
				let Some(ActiveSession::Host { relay, peers, sequence, .. }) = active else {
					let _ = reply.send(Err(HostLiveError::NoWritableGuest));
					return Ok(());
				};
				let runtime = self
					.host
					.as_mut()
					.ok_or(CollabCommandFault::HostUnavailable)?;
				let Some(frames) = runtime
					.ui
					.begin(request, peers.iter().map(|(id, peer)| (*id, peer)))
				else {
					let _ = reply.send(Err(HostLiveError::NoWritableGuest));
					return Ok(());
				};
				let request_id = frames
					.first()
					.and_then(|targeted| targeted.frame.payload.as_ref())
					.and_then(|payload| match payload {
						collab_frame::Payload::UiRequest(request) => Some(request.request_id),
						_ => None,
					})
					.expect("host UI dispatcher emits UI request frames");
				for targeted in frames {
					*sequence = sequence.saturating_add(1);
					let mut frame = targeted.frame;
					frame.sequence = *sequence;
					send_required(relay, RelayRoute { peer_id: targeted.peer_id }, &frame).await?;
				}
				let _ = reply.send(Ok(request_id));
			},
			HostPresentationInput::CancelUi(request_id) => {
				let Some(ActiveSession::Host { relay, peers, sequence, .. }) = active else {
					return Ok(());
				};
				let runtime = self
					.host
					.as_mut()
					.ok_or(CollabCommandFault::HostUnavailable)?;
				for targeted in runtime
					.ui
					.cancel(request_id, peers.iter().map(|(id, peer)| (*id, peer)))
				{
					*sequence = sequence.saturating_add(1);
					let mut frame = targeted.frame;
					frame.sequence = *sequence;
					send_required(relay, RelayRoute { peer_id: targeted.peer_id }, &frame).await?;
				}
			},
		}
		Ok(())
	}

	async fn apply_replication(
		&mut self,
		event: HostReplicationEvent,
		active: &mut Option<ActiveSession>,
	) -> Result<(), CollabCommandFault> {
		let runtime = self
			.host
			.as_mut()
			.ok_or(CollabCommandFault::HostUnavailable)?;
		match event {
			HostReplicationEvent::Record(record) => {
				let expected = runtime.snapshot_watermark.saturating_add(1);
				if record.revision != expected {
					return Err(CollabCommandFault::ReplicationGap {
						expected,
						actual: record.revision,
					});
				}
				runtime.history.push(record.clone());
				runtime.snapshot_watermark = record.revision;
				if let Some(ActiveSession::Host { relay, sequence, .. }) = active {
					*sequence = sequence.saturating_add(1);
					let frame = v1::CollabFrame {
						protocol_revision: omp_collab::PROTOCOL_REVISION,
						sequence: *sequence,
						payload: Some(collab_frame::Payload::JournalRecord(record)),
						..Default::default()
					};
					send_required(relay, RelayRoute { peer_id: 0 }, &frame).await?;
				}
			},
			HostReplicationEvent::Terminal(_) => return Err(CollabCommandFault::ReplicationEnded),
		}
		Ok(())
	}

	async fn restart_guest_snapshot(
		&self,
		active: &mut Option<ActiveSession>,
	) -> Result<(), CollabCommandFault> {
		let Some(ActiveSession::Guest { relay, hello, result, snapshot_deadline, .. }) = active
		else {
			return Ok(());
		};
		let read_only = result.presence.is_some_and(PresenceFacts::read_only);
		let presence = PresenceFacts::guest(ConnectionState::Reconnecting, 0, read_only);
		result.presence = Some(presence);
		*snapshot_deadline = None;
		self.publish_presence(Some(presence));
		reconnect(relay).await?;
		send_required(relay, RelayRoute { peer_id: 0 }, hello).await
	}

	async fn apply(
		&self,
		command: &CollabOwnerCommand,
		active: &mut Option<ActiveSession>,
	) -> Result<CollabCommandResult, CollabCommandFault> {
		match command {
			CollabOwnerCommand::Start(options) => {
				if active.is_some() {
					return Err(CollabCommandFault::AlreadyActive);
				}
				if self.host.is_none() {
					return Err(CollabCommandFault::HostUnavailable);
				}
				self.publish_presence(Some(PresenceFacts::host(ConnectionState::Connecting, 0)));
				let room = HostedRoom::generate(options.relay.clone())?;
				let full_link = Str::from(room.full.compact());
				let view_link = Str::from(room.view.compact());
				let web_link = Str::from(room.full.browser(&options.web));
				let web_view_link = Str::from(room.view.browser(&options.web));
				let mut relay = RelayClient::new(room.full.room_url(), RelayRole::Host, room.room_key)?;
				connect(&mut relay).await?;
				let room_id = Str::from(
					omp_core::base64_url::encode_raw(room.full.room_id().as_bytes()).into_string(),
				);
				let admission = HostAdmission::new(room_id, room.write_token);
				let presence = PresenceFacts::host(ConnectionState::Connected, 0);
				let result = CollabCommandResult {
					presence:      Some(presence),
					full_link:     Some(full_link),
					view_link:     Some(view_link),
					web_link:      Some(web_link),
					web_view_link: Some(web_view_link),
				};
				*active = Some(ActiveSession::Host {
					relay,
					admission,
					peers: HashMap::new(),
					sequence: 0,
					result: result.clone(),
				});
				self.publish_presence(Some(presence));
				Ok(result)
			},
			CollabOwnerCommand::View => match active {
				Some(ActiveSession::Host { result, .. }) => Ok(result.clone()),
				Some(ActiveSession::Guest { .. }) | None => Err(CollabCommandFault::NotHosting),
			},
			CollabOwnerCommand::Status => Ok(active
				.as_ref()
				.map_or_else(CollabCommandResult::inactive, |session| session.result().clone())),
			CollabOwnerCommand::Stop => {
				let Some(ActiveSession::Host { .. }) = active else {
					return Err(CollabCommandFault::NotHosting);
				};
				let mut session = active.take().expect("host matched above");
				session.close("host stopped").await?;
				self.publish_presence(None);
				Ok(CollabCommandResult::inactive())
			},
			CollabOwnerCommand::Join { link, display_name } => {
				if active.is_some() {
					return Err(CollabCommandFault::AlreadyActive);
				}
				let replica = self
					.replica
					.clone()
					.ok_or(CollabCommandFault::ReplicaUnavailable)?;
				self.publish_presence(Some(PresenceFacts::guest(
					ConnectionState::Connecting,
					0,
					link.credentials().is_read_only(),
				)));
				let key = RoomKey::from_bytes(*link.credentials().key())?;
				let write_token = link
					.credentials()
					.write_token()
					.map(|token| Bytes::copy_from_slice(token.as_bytes()));
				let mut relay = RelayClient::new(link.room_url(), RelayRole::Guest, key)?;
				connect(&mut relay).await?;
				let hello = Handshake::hello(1, Hello {
					protocol_revision: omp_collab::PROTOCOL_REVISION,
					display_name: display_name.to_string(),
					write_token,
					client_version: env!("CARGO_PKG_VERSION").to_owned(),
				});
				send_required(&mut relay, RelayRoute { peer_id: 0 }, &hello).await?;
				let inbound = time::timeout(WELCOME_TIMEOUT, relay.receive())
					.await
					.map_err(|source| CollabCommandFault::WelcomeTimeout { source })??
					.ok_or(CollabCommandFault::UnexpectedWelcome)?;
				let RelayInbound::Frame(frame) = inbound else {
					return Err(CollabCommandFault::UnexpectedWelcome);
				};
				let mut handshake = Handshake::new(RelayRole::Guest);
				handshake.accept(&frame.frame)?;
				let Some(collab_frame::Payload::Welcome(welcome)) = frame.frame.payload else {
					return Err(CollabCommandFault::UnexpectedWelcome);
				};
				if welcome.read_only != link.credentials().is_read_only() {
					return Err(CollabCommandFault::CredentialTierMismatch);
				}
				let room_id =
					Str::from(omp_core::base64_url::encode_raw(link.room_id().as_bytes()).into_string());
				begin_guest_snapshot(&replica, room_id.clone(), &welcome).await?;
				let participant_count = welcome
					.initial_state
					.as_ref()
					.map_or(1, |state| state.participants.len().max(1));
				let presence = PresenceFacts::guest(
					ConnectionState::Connecting,
					participant_count,
					welcome.read_only,
				);
				let result =
					CollabCommandResult { presence: Some(presence), ..CollabCommandResult::inactive() };
				*active = Some(ActiveSession::Guest {
					relay,
					sequence: 1,
					hello,
					room_id,
					replica,
					result: result.clone(),
					snapshot_deadline: Some(time::Instant::now() + SNAPSHOT_PROGRESS_TIMEOUT),
				});
				self.publish_presence(Some(presence));
				Ok(result)
			},
			CollabOwnerCommand::Prompt { text, images } => {
				let payload = collab_frame::Payload::Prompt(PromptRequest {
					text:   text.to_string(),
					images: images
						.iter()
						.map(|image| v1::ImageAttachment {
							data:      image.data.clone(),
							mime_type: image.mime_type.to_string(),
						})
						.collect(),
				});
				send_guest_operation(active, payload, true).await
			},
			CollabOwnerCommand::Abort { reason } => {
				send_guest_operation(
					active,
					collab_frame::Payload::Abort(v1::AbortRequest { reason: reason.to_string() }),
					true,
				)
				.await
			},
			CollabOwnerCommand::AgentCommand { command, agent_id, text } => {
				send_guest_operation(
					active,
					collab_frame::Payload::AgentCommand(v1::AgentCommand {
						command:  *command as i32,
						agent_id: agent_id.to_string(),
						text:     text.as_ref().map(ToString::to_string),
					}),
					true,
				)
				.await
			},
			CollabOwnerCommand::UiResponse { request_id, value } => {
				send_guest_operation(
					active,
					collab_frame::Payload::UiResponse(v1::UiResponse {
						request_id: *request_id,
						value:      value.as_ref().map(ToString::to_string),
					}),
					true,
				)
				.await
			},
			CollabOwnerCommand::TranscriptRequest { request_id, agent_id, from_byte } => {
				send_guest_operation(
					active,
					collab_frame::Payload::TranscriptRequest(v1::TranscriptRequest {
						request_id: *request_id,
						agent_id:   agent_id.to_string(),
						from_byte:  *from_byte,
					}),
					false,
				)
				.await
			},
			CollabOwnerCommand::Leave => {
				let Some(ActiveSession::Guest { .. }) = active else {
					return Err(CollabCommandFault::NotGuest);
				};
				let mut session = active.take().expect("guest matched above");
				session.close("guest left").await?;
				self.publish_presence(None);
				Ok(CollabCommandResult::inactive())
			},
		}
	}
}

async fn send_guest_operation(
	active: &mut Option<ActiveSession>,
	payload: collab_frame::Payload,
	requires_write: bool,
) -> Result<CollabCommandResult, CollabCommandFault> {
	let Some(ActiveSession::Guest { relay, sequence, result, .. }) = active else {
		return Err(CollabCommandFault::NotGuest);
	};
	if requires_write && result.presence.is_some_and(PresenceFacts::read_only) {
		return Err(CollabCommandFault::ReadOnly);
	}
	if result.presence.map(PresenceFacts::connection) != Some(ConnectionState::Connected) {
		return Err(CollabCommandFault::SnapshotInProgress);
	}
	*sequence = sequence.saturating_add(1);
	send_required(relay, RelayRoute { peer_id: 0 }, &payload_frame(*sequence, payload)).await?;
	Ok(result.clone())
}

async fn recv_replication(
	host: Option<&HostRuntime>,
) -> Result<HostReplicationEvent, HostBridgeError> {
	match host {
		Some(host) => host.bridge.recv().await,
		None => std::future::pending().await,
	}
}
async fn recv_state(
	receiver: &mut Option<watch::Receiver<v1::SessionStateUpdate>>,
) -> v1::SessionStateUpdate {
	let Some(receiver) = receiver else {
		return std::future::pending().await;
	};
	if receiver.changed().await.is_err() {
		return std::future::pending().await;
	}
	let state = receiver.borrow_and_update().clone();
	state
}

async fn recv_registry(
	receiver: &mut Option<watch::Receiver<HostRegistryUpdate>>,
) -> HostRegistryUpdate {
	let Some(receiver) = receiver else {
		return std::future::pending().await;
	};
	if receiver.changed().await.is_err() {
		return std::future::pending().await;
	}
	let registry = receiver.borrow_and_update().clone();
	registry
}

async fn recv_presentation(
	receiver: Option<&Receiver<HostPresentationInput>>,
) -> HostPresentationInput {
	let Some(receiver) = receiver else {
		return std::future::pending().await;
	};
	match receiver.recv_async().await {
		Ok(input) => input,
		Err(_) => std::future::pending().await,
	}
}

async fn wait_snapshot_deadline(deadline: Option<time::Instant>) {
	match deadline {
		Some(deadline) => time::sleep_until(deadline).await,
		None => std::future::pending().await,
	}
}

async fn send_required(
	relay: &mut RelayClient,
	route: RelayRoute,
	frame: &v1::CollabFrame,
) -> Result<(), CollabCommandFault> {
	if relay.send(route, frame).await? == SendDisposition::Sent {
		Ok(())
	} else {
		Err(CollabCommandFault::OutboundQueued)
	}
}

fn snapshot_frame(
	sequence: u64,
	entries: Vec<v1::JournalRecord>,
	r#final: bool,
	host_revision_watermark: u64,
) -> v1::CollabFrame {
	v1::CollabFrame {
		protocol_revision: omp_collab::PROTOCOL_REVISION,
		sequence,
		payload: Some(collab_frame::Payload::SnapshotChunk(v1::SnapshotChunk {
			entries,
			r#final,
			host_revision_watermark,
		})),
		..Default::default()
	}
}

fn payload_frame(sequence: u64, payload: collab_frame::Payload) -> v1::CollabFrame {
	v1::CollabFrame {
		protocol_revision: omp_collab::PROTOCOL_REVISION,
		sequence,
		payload: Some(payload),
		..Default::default()
	}
}

fn transcript_response(path: &Path, request: &v1::TranscriptRequest) -> v1::TranscriptChunk {
	match read_transcript_chunk(path, request.request_id, request.from_byte) {
		Ok(chunk) => chunk,
		Err(error) => v1::TranscriptChunk {
			request_id: request.request_id,
			text_utf8:  Bytes::new(),
			new_size:   request.from_byte,
			error:      Some(error.to_string()),
		},
	}
}

fn host_participants(
	state: &v1::SessionStateUpdate,
	peers: &HashMap<u32, AuthenticatedPeer>,
) -> Vec<v1::Participant> {
	let mut participants = state
		.participants
		.iter()
		.filter(|participant| participant.is_host)
		.cloned()
		.collect::<Vec<_>>();
	participants.extend(peers.iter().map(|(peer_id, peer)| v1::Participant {
		display_name: peer.principal().display_name().to_owned(),
		is_host:      false,
		read_only:    peer.read_only(),
		peer_id:      *peer_id,
	}));
	participants.sort_by_key(|participant| participant.peer_id);
	participants
}

fn snapshot_entries(records: &[v1::JournalRecord]) -> Vec<Vec<v1::JournalRecord>> {
	let mut chunks = Vec::new();
	let mut entries = Vec::new();
	let mut bytes = 0_usize;
	for record in records {
		let record_bytes = record.transcript_v4_json.len().saturating_add(128);
		if !entries.is_empty()
			&& (entries.len() >= SNAPSHOT_ENTRY_CHUNK
				|| bytes.saturating_add(record_bytes) > SNAPSHOT_CHUNK_SOFT_BYTES)
		{
			chunks.push(std::mem::take(&mut entries));
			bytes = 0;
		}
		bytes = bytes.saturating_add(record_bytes);
		entries.push(record.clone());
	}
	if !entries.is_empty() {
		chunks.push(entries);
	}
	chunks
}

fn error_frame(sequence: u64, message: String) -> v1::CollabFrame {
	v1::CollabFrame {
		protocol_revision: omp_collab::PROTOCOL_REVISION,
		sequence,
		payload: Some(collab_frame::Payload::Error(v1::ErrorMessage {
			code: "mutation-refused".to_owned(),
			message,
		})),
		..Default::default()
	}
}

async fn connect(relay: &mut RelayClient) -> Result<(), CollabCommandFault> {
	time::timeout(CONNECT_TIMEOUT, relay.connect())
		.await
		.map_err(|source| CollabCommandFault::ConnectTimeout { source })??;
	Ok(())
}

async fn reconnect(relay: &mut RelayClient) -> Result<(), CollabCommandFault> {
	loop {
		time::sleep(relay.reconnect_delay()?).await;
		match time::timeout(CONNECT_TIMEOUT, relay.connect()).await {
			Ok(Ok(())) => return Ok(()),
			Ok(Err(RelayError::Socket(_))) | Err(_) => {},
			Ok(Err(error)) => return Err(error.into()),
		}
	}
}

async fn begin_guest_snapshot(
	replica: &GuestReplicaHandle,
	room_id: Str,
	welcome: &Welcome,
) -> Result<(), CollabCommandFault> {
	let header = welcome
		.header
		.clone()
		.ok_or(CollabCommandFault::MissingWelcomeHeader)?;
	replica
		.begin_snapshot(
			room_id,
			header,
			usize::try_from(welcome.total_entry_count)
				.expect("protobuf u32 record count fits in usize"),
		)
		.await?;
	Ok(())
}

struct HostOperationRouter<'a> {
	agents:     &'a HashMap<Str, HostAgentProjection>,
	operations: &'a Sender<HostOperation>,
	principal:  RemotePrincipal,
}

impl HostAgentRuntime for HostOperationRouter<'_> {
	type Error = HostOperationRouteError;

	fn class(&self, agent_id: &str) -> Option<HostAgentClass> {
		let agent = self.agents.get(agent_id)?;
		match v1::agent_summary::Kind::try_from(agent.summary.kind).ok()? {
			v1::agent_summary::Kind::Main => Some(HostAgentClass::Main),
			v1::agent_summary::Kind::Sub => Some(HostAgentClass::Subagent),
		}
	}

	fn chat(&self, agent_id: &str, text: &str) -> Result<(), Self::Error> {
		self
			.operations
			.try_send(HostOperation::AgentChat {
				principal: self.principal.clone(),
				agent_id:  Str::new(agent_id),
				text:      Str::new(text),
			})
			.map_err(HostOperationRouteError::from)
	}

	fn kill(&self, agent_id: &str) -> Result<(), Self::Error> {
		self
			.operations
			.try_send(HostOperation::AgentKill {
				principal: self.principal.clone(),
				agent_id:  Str::new(agent_id),
			})
			.map_err(HostOperationRouteError::from)
	}

	fn revive(&self, agent_id: &str) -> Result<(), Self::Error> {
		self
			.operations
			.try_send(HostOperation::AgentRevive {
				principal: self.principal.clone(),
				agent_id:  Str::new(agent_id),
			})
			.map_err(HostOperationRouteError::from)
	}
}

#[derive(Clone, Copy, Debug, Error)]
enum HostOperationRouteError {
	#[error("collaboration host operation queue is full")]
	Full,
	#[error("collaboration host operation owner has stopped")]
	Stopped,
}

impl<T> From<flume::TrySendError<T>> for HostOperationRouteError {
	fn from(error: flume::TrySendError<T>) -> Self {
		match error {
			flume::TrySendError::Full(_) => Self::Full,
			flume::TrySendError::Disconnected(_) => Self::Stopped,
		}
	}
}

/// One owner request that must settle exactly once.
pub struct CollabOwnerRequest {
	command: CollabOwnerCommand,
	reply:   Option<Sender<Result<CollabCommandResult, CollabCommandFault>>>,
}

impl CollabOwnerRequest {
	/// Returns the requested operation.
	pub const fn command(&self) -> &CollabOwnerCommand {
		&self.command
	}

	/// Settles the waiting slash-command adapter.
	pub fn settle(
		mut self,
		result: Result<CollabCommandResult, CollabCommandFault>,
	) -> Result<(), CollabCommandFault> {
		self
			.reply
			.take()
			.expect("collaboration request reply is present until settlement")
			.send(result)
			.map_err(|_| CollabCommandFault::CallerStopped)
	}
}

/// Collaboration command authority failure.
#[derive(Debug, Error)]
pub enum CollabCommandFault {
	/// Hosting was requested without an attached agent journal and mailbox.
	#[error("collaboration host runtime is unavailable")]
	HostUnavailable,
	/// The host journal cannot fit its entry count into the wire contract.
	#[error("collaboration host snapshot has too many entries")]
	SnapshotTooLarge,
	/// Guest mutations remain disabled until the snapshot commits.
	#[error("collaboration snapshot is still in progress")]
	SnapshotInProgress,
	/// The authoritative journal replication stream ended.
	#[error("collaboration journal replication ended")]
	ReplicationEnded,
	/// Live journal replication skipped or repeated a physical revision.
	#[error("collaboration replication expected revision {expected}, received {actual}")]
	ReplicationGap {
		/// Next required physical revision.
		expected: u64,
		/// Received physical revision.
		actual:   u64,
	},
	/// Production collaboration owner has stopped.
	#[error("collaboration runtime owner has stopped")]
	OwnerStopped,
	/// The single guest presentation owner has stopped consuming frames.
	#[error("collaboration guest presentation owner has stopped")]
	GuestPresentationStopped,
	/// The requesting command surface disappeared before settlement.
	#[error("collaboration command caller has stopped")]
	CallerStopped,
	/// A host-only operation was requested while not hosting.
	#[error("no collaboration room is being hosted")]
	NotHosting,
	/// A leave operation was requested while not joined as a guest.
	#[error("not joined to a collaboration room")]
	NotGuest,
	/// A second room cannot replace an active host or guest implicitly.
	#[error("a collaboration room is already active")]
	AlreadyActive,
	/// Room cryptographic material could not be created or imported.
	#[error(transparent)]
	Crypto(#[from] CryptoError),
	/// Native relay transport failed.
	#[error(transparent)]
	Relay(#[from] RelayError),
	/// Initial relay connection exceeded the host/guest deadline.
	#[error("collaboration relay connection timed out")]
	ConnectTimeout {
		/// Timeout source.
		#[source]
		source: Elapsed,
	},
	/// Guest welcome progress exceeded its deadline.
	#[error("collaboration host welcome timed out")]
	WelcomeTimeout {
		/// Timeout source.
		#[source]
		source: Elapsed,
	},
	/// The relay produced a non-welcome item during guest handshake.
	#[error("collaboration host did not send the expected welcome")]
	UnexpectedWelcome,
	/// A connected outbound operation unexpectedly entered reconnect buffering.
	#[error("collaboration operation could not be sent on the connected relay")]
	OutboundQueued,
	/// Host welcome access tier disagreed with the supplied credential width.
	#[error("collaboration host returned a mismatched credential tier")]
	CredentialTierMismatch,
	/// Viewer credentials cannot submit prompts.
	#[error("this collaboration link is read-only")]
	ReadOnly,
	/// Guest join was composed without a durable transcript replica.
	#[error("collaboration guest replica storage is unavailable")]
	ReplicaUnavailable,
	/// Host welcome omitted the required session header.
	#[error("collaboration host welcome omitted its session header")]
	MissingWelcomeHeader,
	/// Guest transcript projection failed.
	#[error(transparent)]
	GuestReplica(#[from] GuestReplicaError),
}

#[cfg(test)]
mod tests {
	use omp_collab::presence::{ConnectionState, PresenceFacts};

	use super::*;

	#[tokio::test]
	async fn owner_request_settles_one_waiting_caller() {
		let (owner, handle) = CollabSessionAuthority::new();
		let caller = tokio::spawn({
			let handle = handle.clone();
			async move { handle.request(CollabOwnerCommand::Status).await }
		});
		let request = owner.recv().await.expect("request");
		assert!(matches!(request.command(), CollabOwnerCommand::Status));
		request
			.settle(Ok(CollabCommandResult::inactive()))
			.expect("settle");
		assert_eq!(
			caller.await.expect("caller task").expect("command"),
			CollabCommandResult::inactive(),
		);
	}

	#[test]
	fn presence_watch_is_authoritative() {
		let (owner, handle) = CollabSessionAuthority::new();
		let facts = PresenceFacts::host(ConnectionState::Connected, 2);
		owner.publish_presence(Some(facts));
		assert_eq!(handle.presence(), Some(facts));
	}
	#[test]
	fn snapshot_frame_preserves_physical_omissions_and_authoritative_watermark() {
		let records = vec![
			v1::JournalRecord {
				revision:           1,
				transcript_v4_json: Bytes::from_static(br#"{"ts":1,"k":"reset"}"#),
				visibility_class:   v1::VisibilityClass::PublicTranscript as i32,
			},
			v1::JournalRecord {
				revision:           2,
				transcript_v4_json: Bytes::from_static(
					br#"{"ts":2,"k":"collab_omitted","rev":"host_local.v1"}"#,
				),
				visibility_class:   v1::VisibilityClass::HostLocalOmitted as i32,
			},
			v1::JournalRecord {
				revision:           3,
				transcript_v4_json: Bytes::from_static(br#"{"ts":3,"k":"reset"}"#),
				visibility_class:   v1::VisibilityClass::PublicTranscript as i32,
			},
		];
		let chunks = snapshot_entries(&records);
		assert_eq!(chunks.concat(), records);
		let frame = snapshot_frame(7, chunks.into_iter().next().expect("chunk"), true, 3);
		let Some(collab_frame::Payload::SnapshotChunk(chunk)) = frame.payload else {
			panic!("snapshot payload");
		};
		assert_eq!(chunk.host_revision_watermark, 3);
		assert_eq!(chunk.entries.len(), 3);
		assert_eq!(chunk.entries[1].visibility_class, v1::VisibilityClass::HostLocalOmitted as i32,);
	}

	#[test]
	fn guest_owner_consumes_state_and_registry_frames_into_live_projection() {
		let (mut owner, handle) = CollabSessionAuthority::new();
		owner.consume_guest_state(v1::SessionStateUpdate {
			is_streaming: true,
			session_name: "remote".to_owned(),
			participants: vec![v1::Participant {
				display_name: "host".to_owned(),
				is_host:      true,
				read_only:    false,
				peer_id:      0,
			}],
			..Default::default()
		});
		owner.consume_guest_registry(v1::RegistrySnapshot {
			agents: vec![v1::AgentSummary {
				id: "agent-1".to_owned(),
				display_name: "worker".to_owned(),
				kind: v1::agent_summary::Kind::Sub as i32,
				..Default::default()
			}],
		});
		let projection = handle.guest_live();
		assert!(projection.state.is_some_and(|state| state.is_streaming));
		assert_eq!(projection.agents.agents[0].id, "agent-1");
		assert_eq!(
			projection.effects.expect("state effects").activity,
			omp_collab::guest::GuestActivityTransition::Started,
		);
	}
}
