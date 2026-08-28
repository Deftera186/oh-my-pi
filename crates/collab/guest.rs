//! Guest-side snapshot reseed and live transcript-v4 append fencing.

use std::{
	collections::BTreeMap,
	fs, io,
	path::{Path, PathBuf},
};

use flume::{Receiver, Sender};
use omp_core::{Str, sf};
use omp_proto::collab::{
	v1,
	v1::{
		AgentSummary, ContextUsage, JournalRecord, ModelMetadata, RegistrySnapshot, SessionHeader,
		SessionStateUpdate, SnapshotChunk, UiRequest, VisibilityClass, agent_summary, ui_request,
	},
};
use omp_storage::transcript::{
	SessionId,
	replica::{
		RemoteProvenance, RemoteRecord, Replica, ReplicaError, ReplicaFence, ReplicaVisibility,
	},
};
use thiserror::Error;
use tokio::sync::watch;

/// Commands that remain local while rendering a remote collaboration replica.
///
/// Every command outside this closed set is host-owned and must be refused
/// before command dispatch.
pub fn guest_command_allowed(command: &str) -> bool {
	let name = command
		.trim()
		.strip_prefix('/')
		.unwrap_or(command.trim())
		.split_ascii_whitespace()
		.next()
		.unwrap_or_default();
	matches!(
		name,
		"dump"
			| "export"
			| "copy"
			| "help"
			| "hotkeys"
			| "theme"
			| "settings"
			| "leave"
			| "collab"
			| "exit"
			| "quit"
	)
}

/// Applies the guest's local pre-send gates.
pub fn admit_guest_input(
	input: &str,
	read_only: bool,
) -> Result<GuestInputDisposition, GuestInputError> {
	if input.trim_start().starts_with('/') {
		if guest_command_allowed(input) {
			Ok(GuestInputDisposition::LocalCommand)
		} else {
			Err(GuestInputError::HostCommand)
		}
	} else if read_only {
		Err(GuestInputError::ReadOnly)
	} else {
		Ok(GuestInputDisposition::RemotePrompt)
	}
}

/// Accepted route for one guest composer submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestInputDisposition {
	/// Execute through the guest-local command registry.
	LocalCommand,
	/// Send an authenticated prompt request to the host.
	RemotePrompt,
}

/// Guest composer input rejected before local or remote dispatch.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum GuestInputError {
	/// This slash command belongs to the authoritative host session.
	#[error("command is unavailable while joined to a collaboration")]
	HostCommand,
	/// Viewer credentials cannot prompt or mutate the host.
	#[error("this collaboration link is read-only")]
	ReadOnly,
}
/// Host activity edge applied to the guest's local spinner and activity meter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestActivityTransition {
	/// The host became active; start the local activity meter and loader.
	Started,
	/// The host became idle; stop the local activity meter and every transient
	/// loader.
	Stopped,
	/// The host activity state did not change.
	Unchanged,
}

/// Canonical guest footer facts projected from the latest host state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestFooterFacts {
	/// Number of participants including the host and this guest.
	pub participants:    usize,
	/// Number of prompts queued on the host.
	pub queued_messages: u32,
	/// Whether the host is currently aborting a turn.
	pub aborting:        bool,
}

/// Effects a UI owner applies after one state update.
#[derive(Clone, Debug, PartialEq)]
pub struct GuestStateEffects {
	/// Activity edge for loader/meter reconciliation.
	pub activity:       GuestActivityTransition,
	/// Host-authored terminal title; this never changes the guest cwd.
	pub terminal_title: Str,
	/// Footer facts for the collaboration status segment.
	pub footer:         GuestFooterFacts,
	/// Provider-real context estimate reported by the host.
	pub context:        Option<ContextUsage>,
}

/// Guest-local mirror of host presentation state and visible agents.
///
/// The mirror deliberately contains no relay credentials, host filesystem
/// authority, agent paths, model credentials, or advisor rows.
#[derive(Default)]
pub struct GuestStateMirror {
	state:          Option<SessionStateUpdate>,
	models:         BTreeMap<Str, ModelMetadata>,
	agents:         BTreeMap<Str, AgentSummary>,
	host_streaming: bool,
}

impl GuestStateMirror {
	/// Applies one authoritative host-state snapshot and returns UI effects.
	pub fn apply_state(&mut self, mut state: SessionStateUpdate) -> GuestStateEffects {
		if let Some(context) = state.context_usage.as_mut() {
			context.percent = if context.context_window == 0 {
				0.0
			} else {
				(context.tokens as f64 * 100.0 / context.context_window as f64) as f32
			};
		}
		if let Some(model) = state.model.as_ref() {
			self.models.insert(Str::new(&model.id), model.clone());
		}
		let activity = match (self.host_streaming, state.is_streaming) {
			(false, true) => GuestActivityTransition::Started,
			(true, false) => GuestActivityTransition::Stopped,
			_ => GuestActivityTransition::Unchanged,
		};
		self.host_streaming = state.is_streaming;
		let terminal_title = if state.session_name.trim().is_empty() {
			sf!("OMP collaboration")
		} else {
			Str::new(state.session_name.trim())
		};
		let footer = GuestFooterFacts {
			participants:    state.participants.len().max(1),
			queued_messages: state.queued_message_count,
			aborting:        state.is_aborting,
		};
		let context = state.context_usage.clone();
		self.state = Some(state);
		GuestStateEffects { activity, terminal_title, footer, context }
	}

	/// Replaces the visible subagent registry with the host snapshot.
	pub fn apply_registry(&mut self, snapshot: RegistrySnapshot) {
		self.agents.clear();
		for agent in snapshot.agents {
			if agent_summary::Kind::try_from(agent.kind).is_ok() {
				self.agents.insert(Str::new(&agent.id), agent);
			}
		}
	}

	/// Returns the latest host state.
	pub const fn state(&self) -> Option<&SessionStateUpdate> {
		self.state.as_ref()
	}

	/// Returns the effective reasoning effort reported by the host.
	pub fn reasoning_effort(&self) -> Option<&str> {
		self.state.as_ref()?.thinking_level.as_deref()
	}

	/// Iterates the model catalog learned from host state updates.
	pub fn models(&self) -> impl ExactSizeIterator<Item = &ModelMetadata> + DoubleEndedIterator {
		self.models.values()
	}

	/// Iterates the latest visible agent registry mirror.
	pub fn agents(&self) -> impl ExactSizeIterator<Item = &AgentSummary> + DoubleEndedIterator {
		self.agents.values()
	}
}

/// Guest UI presentation hook implemented by the interactive app boundary.
pub trait GuestUiHooks {
	/// Presents one host-owned select dialog.
	fn present_select(&mut self, request_id: u32, title: &str, spec: &v1::SelectSpec);
	/// Presents one host-owned editor dialog.
	fn present_editor(&mut self, request_id: u32, title: &str, spec: &v1::EditorSpec);
	/// Dismisses a presented dialog without answering the host.
	fn dismiss(&mut self, request_id: u32);
}

/// Ordered guest dialog owner. Resync and leave dismiss newest-first.
#[derive(Default)]
pub struct GuestUiRequests {
	pending: BTreeMap<u32, UiRequest>,
	order:   Vec<u32>,
}

impl GuestUiRequests {
	/// Presents a valid host request through the matching UI hook.
	pub fn present(
		&mut self,
		request: UiRequest,
		hooks: &mut impl GuestUiHooks,
	) -> Result<(), GuestUiError> {
		let spec = request.spec.as_ref().ok_or(GuestUiError::MissingSpec)?;
		if self.pending.contains_key(&request.request_id) {
			hooks.dismiss(request.request_id);
			self.order.retain(|id| *id != request.request_id);
		}
		match spec {
			ui_request::Spec::Select(spec) => {
				hooks.present_select(request.request_id, &request.title, spec);
			},
			ui_request::Spec::Editor(spec) => {
				hooks.present_editor(request.request_id, &request.title, spec);
			},
		}
		self.order.push(request.request_id);
		self.pending.insert(request.request_id, request);
		Ok(())
	}

	/// Dismisses one request after `ui_request_end`.
	pub fn end(&mut self, request_id: u32, hooks: &mut impl GuestUiHooks) -> bool {
		let existed = self.pending.remove(&request_id).is_some();
		if existed {
			self.order.retain(|id| *id != request_id);
			hooks.dismiss(request_id);
		}
		existed
	}

	/// Dismisses all requests in reverse presentation order on resync or leave.
	pub fn dismiss_all(&mut self, hooks: &mut impl GuestUiHooks) {
		for request_id in self.order.drain(..).rev() {
			hooks.dismiss(request_id);
		}
		self.pending.clear();
	}

	/// Returns whether a request is still presented.
	pub fn contains(&self, request_id: u32) -> bool {
		self.pending.contains_key(&request_id)
	}
}

/// Local session destination restored after leaving a replica.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalSessionRestore {
	/// Resume the exact prior local transcript.
	Saved(PathBuf),
	/// Return to a fresh unsaved local session.
	Unsaved,
}

/// Exactly-once guest session restoration owner.
#[derive(Default)]
pub struct GuestSessionRestore {
	return_to: Option<LocalSessionRestore>,
	active:    bool,
}

impl GuestSessionRestore {
	/// Captures the local session before switching to the remote replica.
	pub fn begin(&mut self, session_file: Option<&Path>) {
		self.return_to = Some(session_file.map_or(LocalSessionRestore::Unsaved, |path| {
			LocalSessionRestore::Saved(path.to_path_buf())
		}));
		self.active = true;
	}

	/// Restores after intentional leave or a terminal disconnect.
	///
	/// A reconnecting relay does not call this method; callers invoke it only
	/// after reconnect is exhausted.
	pub fn take(&mut self) -> Option<LocalSessionRestore> {
		if !self.active {
			return None;
		}
		self.active = false;
		self.return_to.take()
	}
}

/// Guest dialog protocol failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum GuestUiError {
	/// A request had neither a select nor editor specification.
	#[error("collaboration UI request has no presentation specification")]
	MissingSpec,
}

/// Maximum physical records retained in one in-flight snapshot accumulator.
pub const SNAPSHOT_RECORD_MAX: usize = 1_000_000;

struct PendingSnapshot {
	fence:            ReplicaFence,
	expected_records: usize,
	records:          Vec<RemoteRecord>,
}

/// Guest-owned collaboration replica that survives reconnect reseeds in place.
pub struct GuestReplica {
	replica: Replica,
	active:  ReplicaFence,
	pending: Option<PendingSnapshot>,
	ready:   bool,
}

impl GuestReplica {
	/// Creates an empty replica using only guest-local cwd and secret-free host
	/// provenance.
	pub fn create(
		path: &Path,
		id: SessionId,
		created: u64,
		local_cwd: PathBuf,
		remote: RemoteProvenance,
	) -> Result<Self, GuestReplicaError> {
		let mut replica = Replica::create(path, id, created, local_cwd, remote)?;
		let active = replica.begin_reseed();
		Ok(Self { replica, active, pending: None, ready: false })
	}

	/// Reopens a replica without adopting host cwd or credentials.
	pub fn open(path: &Path, expected_room_id: &str) -> Result<Self, GuestReplicaError> {
		let mut replica = Replica::open(path, expected_room_id)?;
		let active = replica.begin_reseed();
		Ok(Self { replica, active, pending: None, ready: false })
	}

	/// Returns the durable replica.
	pub const fn replica(&self) -> &Replica {
		&self.replica
	}

	/// Starts a reconnect or initial snapshot, fencing all older live frames.
	pub fn begin_snapshot(&mut self, expected_records: usize) -> Result<(), GuestReplicaError> {
		if expected_records > SNAPSHOT_RECORD_MAX {
			return Err(GuestReplicaError::SnapshotTooLarge {
				actual:  expected_records,
				maximum: SNAPSHOT_RECORD_MAX,
			});
		}
		let fence = self.replica.begin_reseed();
		self.ready = false;
		self.active = fence;
		self.pending = Some(PendingSnapshot {
			fence,
			expected_records,
			records: Vec::with_capacity(expected_records.min(4096)),
		});
		Ok(())
	}

	/// Applies one ordered snapshot chunk and atomically publishes on `final`.
	///
	/// Returns `true` only after the final chunk is durably reseeded.
	pub fn push_snapshot_chunk(&mut self, chunk: SnapshotChunk) -> Result<bool, GuestReplicaError> {
		let pending = self
			.pending
			.as_mut()
			.ok_or(GuestReplicaError::OrphanSnapshotChunk)?;
		if pending.records.len().saturating_add(chunk.entries.len()) > pending.expected_records
			|| pending.records.len().saturating_add(chunk.entries.len()) > SNAPSHOT_RECORD_MAX
		{
			return Err(GuestReplicaError::SnapshotEntryOverflow {
				expected: pending.expected_records,
			});
		}
		for record in chunk.entries {
			pending.records.push(convert_record(record)?);
		}
		if !chunk.r#final {
			return Ok(false);
		}
		if pending.records.len() != pending.expected_records {
			return Err(GuestReplicaError::SnapshotCountMismatch {
				expected: pending.expected_records,
				actual:   pending.records.len(),
			});
		}
		let pending = self.pending.take().expect("pending snapshot checked above");
		self
			.replica
			.commit_reseed(pending.fence, chunk.host_revision_watermark, &pending.records)?;
		self.ready = true;
		Ok(true)
	}

	/// Appends one live record only for the post-snapshot active generation.
	pub fn append_live(&mut self, record: JournalRecord) -> Result<u64, GuestReplicaError> {
		if self.pending.is_some() {
			return Err(GuestReplicaError::SnapshotInProgress);
		}
		if !self.ready {
			return Err(GuestReplicaError::SnapshotRequired);
		}
		let record = convert_record(record)?;
		Ok(self.replica.append_live(self.active, &record)?)
	}
}

const REPLICA_MAILBOX_CAPACITY: usize = 64;

/// Latest durable state published by the ordered guest replica pump.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GuestReplicaProjection {
	/// Durable replica path once the first welcome has identified the room.
	pub path:       Option<PathBuf>,
	/// Highest physical host revision durably applied.
	pub watermark:  u64,
	/// Reseed generation, incremented for every welcome.
	pub generation: u64,
	/// Whether a complete snapshot currently backs the projection.
	pub ready:      bool,
	/// Whether a live revision gap requires a fresh host snapshot.
	pub gap:        bool,
}

enum GuestReplicaCommand {
	Begin {
		room_id:          Str,
		header:           SessionHeader,
		expected_records: usize,
		reply:            Sender<Result<GuestReplicaProjection, GuestReplicaError>>,
	},
	Snapshot {
		chunk: SnapshotChunk,
		reply: Sender<Result<GuestReplicaProjection, GuestReplicaError>>,
	},
	Live {
		record: JournalRecord,
		reply:  Sender<Result<GuestReplicaProjection, GuestReplicaError>>,
	},
	Stop,
}

mod replica_handle {
	use tokio::sync::watch::Receiver;

	use super::*;

	/// Clone-cheap producer and projection observer for [`GuestRelayPump`].
	#[derive(Clone)]
	pub struct GuestReplicaHandle {
		pub(super) commands: Sender<GuestReplicaCommand>,
		pub(super) updates:  Receiver<GuestReplicaProjection>,
	}

	impl GuestReplicaHandle {
		/// Starts an initial or reconnect snapshot for one authenticated room.
		#[tracing::instrument(
			level = "debug",
			name = "collaboration_sync",
			skip_all,
			fields(expected_records)
		)]
		pub async fn begin_snapshot(
			&self,
			room_id: Str,
			header: SessionHeader,
			expected_records: usize,
		) -> Result<GuestReplicaProjection, GuestReplicaError> {
			let (reply, response) = flume::bounded(1);
			self
				.commands
				.send_async(GuestReplicaCommand::Begin { room_id, header, expected_records, reply })
				.await
				.map_err(|_| GuestReplicaError::PumpStopped)?;
			response
				.recv_async()
				.await
				.map_err(|_| GuestReplicaError::PumpStopped)?
		}

		/// Applies one ordered snapshot chunk.
		pub async fn push_snapshot_chunk(
			&self,
			chunk: SnapshotChunk,
		) -> Result<GuestReplicaProjection, GuestReplicaError> {
			let (reply, response) = flume::bounded(1);
			self
				.commands
				.send_async(GuestReplicaCommand::Snapshot { chunk, reply })
				.await
				.map_err(|_| GuestReplicaError::PumpStopped)?;
			response
				.recv_async()
				.await
				.map_err(|_| GuestReplicaError::PumpStopped)?
		}

		/// Applies one ordered live record.
		pub async fn append_live(
			&self,
			record: JournalRecord,
		) -> Result<GuestReplicaProjection, GuestReplicaError> {
			let (reply, response) = flume::bounded(1);
			self
				.commands
				.send_async(GuestReplicaCommand::Live { record, reply })
				.await
				.map_err(|_| GuestReplicaError::PumpStopped)?;
			response
				.recv_async()
				.await
				.map_err(|_| GuestReplicaError::PumpStopped)?
		}

		/// Returns the most recently published durable projection state.
		pub fn projection(&self) -> GuestReplicaProjection {
			self.updates.borrow().clone()
		}

		/// Subscribes to coalesced durable projection updates.
		pub fn subscribe(&self) -> Receiver<GuestReplicaProjection> {
			self.updates.clone()
		}

		/// Stops the pump after the collaboration owner has detached.
		pub async fn stop(&self) {
			let _ = self.commands.send_async(GuestReplicaCommand::Stop).await;
		}
	}
}

pub use replica_handle::GuestReplicaHandle;

/// Ordered guest snapshot/live actor.
///
/// The actor has one bounded flume mailbox. A malformed or gapped record
/// settles only that request; the loop remains alive so a reconnect welcome
/// can reseed the same durable replica.
pub struct GuestRelayPump {
	root:       PathBuf,
	local_cwd:  PathBuf,
	created_ms: u64,
	commands:   Receiver<GuestReplicaCommand>,
	updates:    watch::Sender<GuestReplicaProjection>,
	replica:    Option<GuestReplica>,
	room_id:    Option<Str>,
	projection: GuestReplicaProjection,
}

impl GuestRelayPump {
	/// Creates an idle pump rooted in guest-local state.
	pub fn new(root: PathBuf, local_cwd: PathBuf, created_ms: u64) -> (Self, GuestReplicaHandle) {
		let (commands, receiver) = flume::bounded(REPLICA_MAILBOX_CAPACITY);
		let projection = GuestReplicaProjection::default();
		let (updates, observed) = watch::channel(projection.clone());
		(
			Self {
				root,
				local_cwd,
				created_ms,
				commands: receiver,
				updates,
				replica: None,
				room_id: None,
				projection,
			},
			GuestReplicaHandle { commands, updates: observed },
		)
	}

	/// Runs until the owner explicitly stops or every producer is dropped.
	pub async fn run(mut self) {
		while let Ok(command) = self.commands.recv_async().await {
			match command {
				GuestReplicaCommand::Begin { room_id, header, expected_records, reply } => {
					let result = self.begin(room_id, header, expected_records);
					let _ = reply.send(result);
				},
				GuestReplicaCommand::Snapshot { chunk, reply } => {
					let result = self.snapshot(chunk);
					let _ = reply.send(result);
				},
				GuestReplicaCommand::Live { record, reply } => {
					let result = self.live(record);
					let _ = reply.send(result);
				},
				GuestReplicaCommand::Stop => break,
			}
		}
	}

	fn begin(
		&mut self,
		room_id: Str,
		header: SessionHeader,
		expected_records: usize,
	) -> Result<GuestReplicaProjection, GuestReplicaError> {
		if self.room_id.as_ref() != Some(&room_id) {
			fs::create_dir_all(&self.root).map_err(|source| GuestReplicaError::CreateDirectory {
				path: self.root.clone(),
				source,
			})?;
			let path = self.root.join(format!("{room_id}.jsonl"));
			let replica = if path.exists() {
				GuestReplica::open(&path, room_id.as_str())?
			} else {
				GuestReplica::create(
					&path,
					SessionId(Str::from(omp_core::Ulid::generate().to_string())),
					self.created_ms,
					self.local_cwd.clone(),
					RemoteProvenance {
						host_session: SessionId(Str::new(&header.session_id)),
						room_id:      room_id.clone(),
						host_created: header.created_at_ms,
					},
				)?
			};
			self.replica = Some(replica);
			self.room_id = Some(room_id);
			self.projection.path = Some(path);
		}
		let replica = self
			.replica
			.as_mut()
			.ok_or(GuestReplicaError::SnapshotRequired)?;
		replica.begin_snapshot(expected_records)?;
		self.projection.generation = self.projection.generation.saturating_add(1);
		self.projection.ready = false;
		self.projection.gap = false;
		self.publish();
		tracing::info!(
			generation = self.projection.generation,
			expected_records,
			"collaboration synchronization started"
		);
		Ok(self.projection.clone())
	}

	fn snapshot(
		&mut self,
		chunk: SnapshotChunk,
	) -> Result<GuestReplicaProjection, GuestReplicaError> {
		let replica = self
			.replica
			.as_mut()
			.ok_or(GuestReplicaError::SnapshotRequired)?;
		if replica.push_snapshot_chunk(chunk)? {
			self.projection.watermark = replica.replica().host_revision_watermark();
			self.projection.ready = true;
			self.projection.gap = false;
			self.publish();
			tracing::info!(
				generation = self.projection.generation,
				watermark = self.projection.watermark,
				"collaboration synchronization completed"
			);
		}
		Ok(self.projection.clone())
	}

	fn live(&mut self, record: JournalRecord) -> Result<GuestReplicaProjection, GuestReplicaError> {
		let replica = self
			.replica
			.as_mut()
			.ok_or(GuestReplicaError::SnapshotRequired)?;
		match replica.append_live(record) {
			Ok(watermark) => {
				self.projection.watermark = watermark;
				self.publish();
				Ok(self.projection.clone())
			},
			Err(error @ GuestReplicaError::Replica(ReplicaError::Revision { .. })) => {
				self.projection.ready = false;
				self.projection.gap = true;
				self.publish();
				tracing::warn!(
					generation = self.projection.generation,
					%error,
					"collaboration replica revision gap; resynchronization required"
				);
				Err(error)
			},
			Err(error) => Err(error),
		}
	}

	fn publish(&self) {
		self.updates.send_replace(self.projection.clone());
	}
}

/// Guest replica protocol or storage failure.
#[derive(Debug, Error)]
pub enum GuestReplicaError {
	/// Durable replica operation failed.
	#[error(transparent)]
	Replica(#[from] ReplicaError),
	/// Welcome advertised an unreasonable physical record count.
	#[error("collaboration snapshot has {actual} records; maximum is {maximum}")]
	SnapshotTooLarge {
		/// Advertised count.
		actual:  usize,
		/// Hard accumulator ceiling.
		maximum: usize,
	},
	/// A chunk arrived before a welcome began a snapshot.
	#[error("collaboration snapshot chunk arrived without an active snapshot")]
	OrphanSnapshotChunk,
	/// Chunks exceeded the welcome's exact entry count.
	#[error("collaboration snapshot exceeded its expected {expected} records")]
	SnapshotEntryOverflow {
		/// Welcome-advertised count.
		expected: usize,
	},
	/// A final chunk arrived before all advertised physical records.
	#[error("collaboration snapshot expected {expected} records, received {actual}")]
	SnapshotCountMismatch {
		/// Welcome-advertised count.
		expected: usize,
		/// Received count.
		actual:   usize,
	},
	/// Live traffic arrived before the active snapshot was durably published.
	#[error("collaboration live record arrived while a snapshot is in progress")]
	SnapshotInProgress,
	/// Live traffic arrived before any complete snapshot established the fence.
	#[error("collaboration live record arrived before a complete snapshot")]
	SnapshotRequired,
	/// The ordered replica actor is no longer running.
	#[error("collaboration guest replica pump has stopped")]
	PumpStopped,
	/// The guest-local replica directory could not be created.
	#[error("collaboration guest replica directory could not be created at {path}")]
	CreateDirectory {
		/// Guest-local replica directory.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: io::Error,
	},
	/// A journal record carried an unknown visibility value.
	#[error("collaboration journal record has an unknown visibility class")]
	UnknownVisibility,
}

fn convert_record(record: JournalRecord) -> Result<RemoteRecord, GuestReplicaError> {
	let visibility = match VisibilityClass::try_from(record.visibility_class) {
		Ok(VisibilityClass::PublicTranscript) => ReplicaVisibility::PublicTranscript,
		Ok(VisibilityClass::PublicPresentation) => ReplicaVisibility::PublicPresentation,
		Ok(VisibilityClass::HostLocalOmitted) => ReplicaVisibility::HostLocalOmitted,
		Ok(VisibilityClass::Unspecified) | Err(_) => {
			return Err(GuestReplicaError::UnknownVisibility);
		},
	};
	Ok(RemoteRecord { revision: record.revision, visibility, json: record.transcript_v4_json })
}
#[cfg(test)]
mod tests {

	use super::*;

	#[test]
	fn guest_command_filter_is_closed() {
		assert!(guest_command_allowed("/export transcript.html"));
		assert!(guest_command_allowed(" /leave "));
		assert!(!guest_command_allowed("/model anthropic/example"));
		assert!(!guest_command_allowed("/agents"));
	}

	#[test]
	fn read_only_gate_precedes_remote_prompt_send() {
		assert_eq!(admit_guest_input("hello", true), Err(GuestInputError::ReadOnly));
		assert_eq!(admit_guest_input("/help", true), Ok(GuestInputDisposition::LocalCommand),);
		assert_eq!(admit_guest_input("hello", false), Ok(GuestInputDisposition::RemotePrompt),);
	}
	#[test]
	fn state_mirror_reconciles_activity_catalog_registry_and_context() {
		let mut mirror = GuestStateMirror::default();
		let effects = mirror.apply_state(SessionStateUpdate {
			is_streaming: true,
			queued_message_count: 2,
			session_name: "remote".to_owned(),
			model: Some(ModelMetadata {
				id:             "model-1".to_owned(),
				name:           "Model".to_owned(),
				provider:       "provider".to_owned(),
				context_window: 100,
			}),
			thinking_level: Some("high".to_owned()),
			context_usage: Some(ContextUsage {
				tokens:         25,
				context_window: 100,
				percent:        0.0,
			}),
			participants: vec![Default::default(), Default::default()],
			..SessionStateUpdate::default()
		});
		assert_eq!(effects.activity, GuestActivityTransition::Started);
		assert_eq!(effects.footer.participants, 2);
		assert_eq!(effects.context.expect("context").percent, 25.0);
		assert_eq!(mirror.reasoning_effort(), Some("high"));
		assert_eq!(mirror.models().len(), 1);

		mirror.apply_registry(RegistrySnapshot {
			agents: vec![AgentSummary { id: "agent-1".to_owned(), ..AgentSummary::default() }],
		});
		assert_eq!(mirror.agents().len(), 1);
		let effects =
			mirror.apply_state(SessionStateUpdate { is_streaming: false, ..Default::default() });
		assert_eq!(effects.activity, GuestActivityTransition::Stopped);
	}

	#[test]
	fn local_session_restore_is_exactly_once() {
		let mut restore = GuestSessionRestore::default();
		restore.begin(Some(Path::new("/tmp/session.jsonl")));
		assert_eq!(
			restore.take(),
			Some(LocalSessionRestore::Saved(PathBuf::from("/tmp/session.jsonl")))
		);
		assert_eq!(restore.take(), None);
	}

	fn wire_record(revision: u64) -> JournalRecord {
		JournalRecord {
			revision,
			transcript_v4_json: bytes::Bytes::from(format!(r#"{{"ts":{revision},"k":"reset"}}"#)),
			visibility_class: VisibilityClass::PublicTranscript as i32,
		}
	}

	fn header() -> SessionHeader {
		SessionHeader {
			session_id:    "host-session".to_owned(),
			title:         "Remote".to_owned(),
			created_at_ms: 7,
			host_cwd:      "/host/secret".to_owned(),
		}
	}

	#[tokio::test]
	async fn relay_pump_applies_snapshot_then_live_records_in_order() {
		let directory = tempfile::tempdir().expect("temporary replica directory");
		let (pump, handle) =
			GuestRelayPump::new(directory.path().to_path_buf(), PathBuf::from("/guest"), 11);
		let task = tokio::spawn(pump.run());
		handle
			.begin_snapshot(sf!("room"), header(), 2)
			.await
			.expect("begin snapshot");
		let snapshot = handle
			.push_snapshot_chunk(SnapshotChunk {
				entries:                 vec![wire_record(1), wire_record(2)],
				r#final:                 true,
				host_revision_watermark: 2,
			})
			.await
			.expect("commit snapshot");
		assert!(snapshot.ready);
		assert_eq!(snapshot.watermark, 2);
		let live = handle
			.append_live(wire_record(3))
			.await
			.expect("append live");
		assert_eq!(live.watermark, 3);

		let path = live.path.expect("replica path");
		let log = omp_storage::transcript::load(&path).expect("load replica");
		assert_eq!(log.len(), 3);
		handle.stop().await;
		task.await.expect("pump task");
	}

	#[tokio::test]
	async fn relay_pump_survives_a_live_gap_until_reseed() {
		let directory = tempfile::tempdir().expect("temporary replica directory");
		let (pump, handle) =
			GuestRelayPump::new(directory.path().to_path_buf(), PathBuf::from("/guest"), 11);
		let task = tokio::spawn(pump.run());
		handle
			.begin_snapshot(sf!("room"), header(), 1)
			.await
			.expect("begin snapshot");
		handle
			.push_snapshot_chunk(SnapshotChunk {
				entries:                 vec![wire_record(1)],
				r#final:                 true,
				host_revision_watermark: 1,
			})
			.await
			.expect("commit snapshot");

		assert!(matches!(
			handle.append_live(wire_record(3)).await,
			Err(GuestReplicaError::Replica(ReplicaError::Revision { expected: 2, actual: 3 }))
		));
		assert!(handle.projection().gap);

		handle
			.begin_snapshot(sf!("room"), header(), 3)
			.await
			.expect("begin recovery snapshot");
		let recovered = handle
			.push_snapshot_chunk(SnapshotChunk {
				entries:                 vec![wire_record(1), wire_record(2), wire_record(3)],
				r#final:                 true,
				host_revision_watermark: 3,
			})
			.await
			.expect("commit recovery snapshot");
		assert!(recovered.ready);
		assert!(!recovered.gap);
		assert_eq!(
			handle
				.append_live(wire_record(4))
				.await
				.expect("append after reseed")
				.watermark,
			4,
		);
		handle.stop().await;
		task.await.expect("pump task");
	}
}
