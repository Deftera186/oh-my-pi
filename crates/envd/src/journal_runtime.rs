//! Authenticated external CONTROL routing for session index and durable state.

use std::{
	collections::{BTreeMap, BTreeSet},
	fmt::Display,
	path::{Path, PathBuf},
	str,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use omp_agent::{
	AgentHostControl, JournalAuthor, JournalCustomEntry, JournalOperation, JournalQuery,
	JournalRequest, JournalRequestStamp, PendingCustomEntry, SessionStateWatchEvent,
	control::ControlSender,
};
use omp_core::{ArtifactUrl, Provenance, Str, sf};
use omp_proto::{
	inference::v1::{self, usage, value},
	toolhost::v1::{
		ArtifactRow, JournalHostEnvelope, SessionRow, StateChanged as WireStateChanged,
		StateValue as WireStateValue, UsageReport, journal_host_envelope,
	},
};
use omp_storage::{
	blob::BlobRef,
	gc::{ArtifactCatalog, ArtifactRecord, ArtifactRequest, Error as ArtifactError},
	index::{
		SessionFilter, SessionIndex, SessionInfo, SessionKind, SessionStatus, UsageBucket,
		UsageBucketWidth, UsageDimension, UsageQuery,
	},
	state::{
		DurableRequest, GenerationFence, StateAuthority, StateChange, StateEntry, StateRevision,
		StateScope, StateStore,
	},
	transcript,
	transcript::SessionId,
};
use omp_tool::ArtifactLifetime;
use parking_lot::Mutex;
use serde_json::{Map, Value, json, value::RawValue};

use super::{
	blobs::{ArtifactMetadata, ArtifactMetadataStore, BlobHost, BlobId},
	exthost::{
		context::LiveContextControlOwner,
		control::{
			ControlAuthority, ControlAuthorityFactory, ControlCompositionError,
			ControlConnectionIdentity, ControlEffect, ControlProtocolError, ControlRequestContext,
			ExternalJournalRequest, JournalConnectionIdentity, artifact_rows, journal_rows,
			session_rows, usage_rows,
		},
	},
	schedules::{
		DurableScheduleError, DurableScheduleHandle, ScheduleCaller, ScheduleDeliveryBackend,
		open_durable_scheduler_unbound,
	},
	server::EnvdError,
	worker::{ExternalJournalCall, WorkerError},
};

#[derive(Clone)]
struct AgentBinding {
	id:     u64,
	sender: ControlSender,
	host:   Option<AgentHostControl>,
}

/// Environment-owned endpoint for external Journal CONTROL requests.
#[derive(Clone)]
pub struct ExternalJournalActor {
	sender:        flume::Sender<ExternalJournalCall>,
	agent:         Arc<Mutex<Option<AgentBinding>>>,
	sessions:      Arc<SessionIndex>,
	state:         Option<Arc<StateStore>>,
	catalog:       Arc<Mutex<ArtifactCatalog>>,
	artifact_meta: ArtifactMetadataStore,
	blobs:         BlobHost,
	session_id:    Str,
	project_scope: Str,
	project_path:  Str,
	state_dir:     PathBuf,
	sessions_dir:  PathBuf,
	schedules:     DurableScheduleHandle,
}

impl ExternalJournalActor {
	/// Starts one actor over the Environment's authoritative storage handles.
	///
	/// # Errors
	///
	/// Fails if the shared artifact catalog cannot be opened.
	pub(crate) fn spawn(
		sessions: Arc<SessionIndex>,
		state: Option<Arc<StateStore>>,
		blobs: BlobHost,
		session_id: Str,
		project_scope: Str,
		project_path: Str,
	) -> Result<Self, EnvdError> {
		let catalog = Arc::new(Mutex::new(
			ArtifactCatalog::open(blobs.store())
				.map_err(|error| EnvdError::Blob(Str::from(error.to_string())))?,
		));
		let state_dir = blobs
			.store()
			.root()
			.parent()
			.unwrap_or_else(|| blobs.store().root())
			.to_path_buf();
		let sessions_dir = state_dir.join("sessions");
		let artifact_meta = ArtifactMetadataStore::open(blobs.store())
			.map_err(|error| EnvdError::Blob(Str::from(error.to_string())))?;
		let schedules = open_durable_scheduler_unbound(&state_dir.join("agent-schedules.sqlite"))?;
		let (sender, receiver) = flume::unbounded::<ExternalJournalCall>();
		let agent = Arc::new(Mutex::new(None));
		let actor_agent = Arc::clone(&agent);
		let actor_sessions = Arc::clone(&sessions);
		let actor_state = state.clone();
		let actor_catalog = Arc::clone(&catalog);
		let actor_blobs = blobs.clone();
		let actor_session_id = session_id.clone();
		let actor_project_scope = project_scope.clone();
		let actor_project_path = project_path.clone();
		tokio::spawn(async move {
			while let Ok(call) = receiver.recv_async().await {
				let sessions = Arc::clone(&sessions);
				let state = state.clone();
				let session_id = session_id.clone();
				let project_scope = project_scope.clone();
				let project_path = project_path.clone();
				let catalog = Arc::clone(&catalog);
				let agent = Arc::clone(&actor_agent);
				tokio::spawn(async move {
					let reply = call.reply.clone();
					if let Err(error) = dispatch(
						call.request,
						call.identity,
						&sessions,
						state.as_deref(),
						&session_id,
						&project_scope,
						&project_path,
						&catalog,
						&agent,
						&reply,
					)
					.await
					{
						let _ = reply.send(Err(error));
					}
				});
			}
		});
		Ok(Self {
			sender,
			agent,
			sessions: actor_sessions,
			state: actor_state,
			catalog: actor_catalog,
			artifact_meta,
			blobs: actor_blobs,
			session_id: actor_session_id,
			project_scope: actor_project_scope,
			project_path: actor_project_path,
			state_dir,
			sessions_dir,
			schedules,
		})
	}

	/// Returns the endpoint installed into each authenticated extension host.
	pub(crate) fn sender(&self) -> flume::Sender<ExternalJournalCall> {
		self.sender.clone()
	}

	/// Installs the sole active Agent Journal mailbox before child activation.
	///
	/// # Errors
	///
	/// Refuses a second binding rather than transferring session authority.
	#[cfg(test)]
	pub(crate) fn bind_agent(&self, id: u64, sender: ControlSender) -> Result<(), EnvdError> {
		self.bind_agent_with_host(id, sender, None)
	}

	pub(crate) fn bind_agent_with_host(
		&self,
		id: u64,
		sender: ControlSender,
		host: Option<AgentHostControl>,
	) -> Result<(), EnvdError> {
		let mut agent = self.agent.lock();
		if agent.is_some() {
			return Err(WorkerError::Protocol(sf!("agent journal CONTROL is already bound",)).into());
		}
		*agent = Some(AgentBinding { id, sender, host });
		Ok(())
	}

	/// Installs the host owner that attaches or starts agents for scheduled
	/// delivery.
	pub(crate) async fn bind_schedule_delivery(
		&self,
		backend: Arc<dyn ScheduleDeliveryBackend>,
	) -> Result<(), EnvdError> {
		Ok(self.schedules.bind_delivery(backend).await?)
	}

	pub(crate) fn unbind_agent(&self, id: u64) {
		let mut agent = self.agent.lock();
		if agent.as_ref().is_some_and(|binding| binding.id == id) {
			*agent = None;
			let schedules = self.schedules.clone();
			tokio::spawn(async move {
				if let Err(error) = schedules.expire_session().await {
					tracing::warn!(%error, "session schedule expiration failed");
				}
			});
		}
	}
}
/// Factory for connection-scoped JSON persistence authorities.
pub struct PersistenceControlFactory {
	actor:       ExternalJournalActor,
	provenances: Arc<BTreeMap<(Str, Str, Str), Provenance>>,
}

impl PersistenceControlFactory {
	/// Binds the actor to the admitted manifest provenance table.
	pub fn new(
		actor: ExternalJournalActor,
		provenances: Arc<BTreeMap<(Str, Str, Str), Provenance>>,
	) -> Self {
		Self { actor, provenances }
	}
}

impl ControlAuthorityFactory for PersistenceControlFactory {
	fn bind(
		&self,
		identity: Arc<ControlConnectionIdentity>,
	) -> Result<Arc<dyn ControlAuthority>, ControlCompositionError> {
		let provenance = self
			.provenances
			.get(&(identity.layer.clone(), identity.tier.clone(), identity.extension.clone()))
			.cloned()
			.ok_or_else(|| {
				ControlCompositionError::unavailable(
					"persistence",
					"authenticated extension provenance is absent",
				)
			})?;
		let context = self
			.actor
			.agent
			.lock()
			.clone()
			.and_then(|binding| binding.host)
			.map(|host| {
				LiveContextControlOwner::new(Arc::clone(&identity), self.actor.session_id.clone(), host)
			});
		Ok(Arc::new(PersistenceControlOwner {
			actor: self.actor.clone(),
			identity: Arc::clone(&identity),
			journal_identity: JournalConnectionIdentity {
				principal: identity.principal.clone(),
				provenance,
				host_generation: identity.host_generation,
				session_generation: identity.session_generation,
			},
			context,
		}))
	}
}

struct PersistenceControlOwner {
	actor:            ExternalJournalActor,
	identity:         Arc<ControlConnectionIdentity>,
	journal_identity: JournalConnectionIdentity,
	context:          Option<LiveContextControlOwner>,
}

impl PersistenceControlOwner {
	fn validate(&self, context: &ControlRequestContext) -> Result<(), ControlProtocolError> {
		let actual = &context.connection;
		if self.identity.extension == actual.extension
			&& self.identity.principal == actual.principal
			&& self.identity.artifact_digest == actual.artifact_digest
			&& self.identity.layer == actual.layer
			&& self.identity.tier == actual.tier
			&& self.identity.host_generation == actual.host_generation
			&& self.identity.session_generation == actual.session_generation
		{
			Ok(())
		} else {
			Err(ControlProtocolError::new(
				"StaleGeneration",
				"persistence authority belongs to a replaced CONTROL connection",
			))
		}
	}

	fn sender(&self) -> Result<ControlSender, ControlProtocolError> {
		bound_agent(&self.actor.agent).map_err(protocol_error)
	}

	fn state_authority(&self) -> Result<StateAuthority, ControlProtocolError> {
		state_authority(
			self.journal_identity.clone(),
			self.identity.extension.clone(),
			&self.actor.session_id,
			&self.actor.project_scope,
		)
		.map_err(protocol_error)
	}

	fn schedule_caller(
		&self,
		context: &ControlRequestContext,
	) -> Result<ScheduleCaller, ControlProtocolError> {
		let owner = context
			.invocation
			.as_ref()
			.map(|invocation| invocation.session.clone())
			.ok_or_else(|| {
				ControlProtocolError::new(
					"PhaseConflict",
					"schedule operation requires a live Agent invocation",
				)
			})?;
		Ok(ScheduleCaller {
			owner,
			extension_owner: context.connection.extension.clone(),
			principal: Str::from(context.connection.principal.id()),
			artifact_digest: context.connection.artifact_digest.clone(),
			host_generation: context.connection.host_generation,
			session_generation: context.connection.session_generation,
		})
	}

	async fn schedule_request(
		&self,
		context: ControlRequestContext,
		operation: Str,
		arguments: Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		let caller = self.schedule_caller(&context)?;
		self
			.actor
			.schedules
			.request(caller, operation, arguments)
			.await
			.map_err(schedule_protocol_error)
	}
}

#[async_trait::async_trait]
impl ControlAuthority for PersistenceControlOwner {
	fn handles(&self, operation: &str) -> bool {
		operation.starts_with("omp.context.")
			|| operation.starts_with("omp.journal.")
			|| operation.starts_with("omp.state.")
			|| operation == "omp.state_dir"
			|| (operation.starts_with("omp.sessions.") && operation != "omp.sessions.create")
			|| operation.starts_with("omp.artifacts.")
			|| operation.starts_with("omp.agents.schedule")
			|| operation == "omp.agents.schedules"
			|| operation == "omp.agents.unschedule"
	}

	fn authorize(
		&self,
		context: &ControlRequestContext,
		operation: &str,
		arguments: &Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		if operation.starts_with("omp.context.") {
			return self
				.context
				.as_ref()
				.ok_or_else(|| {
					ControlProtocolError::new(
						"ControlOwnerUnavailable",
						"active Agent context owner is not bound",
					)
					.retryable(true)
				})?
				.authorize(context, operation, arguments);
		}
		self.validate(context)
	}

	async fn request(
		&self,
		context: ControlRequestContext,
		operation: Str,
		arguments: Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		if operation.as_str().starts_with("omp.context.") {
			return self
				.context
				.as_ref()
				.ok_or_else(|| {
					ControlProtocolError::new(
						"ControlOwnerUnavailable",
						"active Agent context owner is not bound",
					)
					.retryable(true)
				})?
				.request(context, operation, arguments)
				.await;
		}
		self.validate(&context)?;
		if operation.as_str().starts_with("omp.agents.schedule")
			|| operation.as_str() == "omp.agents.schedules"
			|| operation.as_str() == "omp.agents.unschedule"
		{
			return self.schedule_request(context, operation, arguments).await;
		}
		match operation.as_str() {
			"omp.journal.append"
			| "omp.journal.append_many"
			| "omp.journal.append_atomic"
			| "omp.journal.label" => {
				self
					.journal_mutation(context.request_id, operation.as_str(), &arguments)
					.await
			},
			"omp.journal.entries" | "omp.journal.latest" => {
				self.journal_query(operation.as_str(), &arguments).await
			},
			"omp.state.append" | "omp.state.entries" | "omp.state.latest" | "omp.state.cas_put"
			| "omp.state.cas_get" => {
				self
					.state_request(context.request_id, operation.as_str(), &arguments)
					.await
			},
			"omp.sessions.list"
			| "omp.sessions.get"
			| "omp.sessions.lineage"
			| "omp.sessions.usage" => self.sessions_request(operation.as_str(), &arguments),
			"omp.sessions.rename" => self.rename_session(&arguments).await,
			"omp.artifacts.adopt"
			| "omp.artifacts.stat"
			| "omp.artifacts.list"
			| "omp.artifacts.pin" => {
				self.artifact_request(context.request_id, operation.as_str(), &arguments)
			},
			"omp.state_dir" => Ok(Value::String(self.actor.state_dir.to_string_lossy().into_owned())),
			"omp.sessions.resume" | "omp.sessions.delete" => Err(ControlProtocolError::new(
				"ControlOwnerUnavailable",
				format!("{operation} requires a historical journal lifecycle owner"),
			)),
			_ => Err(ControlProtocolError::new(
				"unhandled_operation",
				format!("unhandled persistence operation: {operation}"),
			)),
		}
	}

	async fn effect(
		&self,
		context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		self.validate(&context)?;
		Err(ControlProtocolError::new(
			"InvalidEffect",
			"persistence authorities do not own child observations",
		))
	}
}
impl PersistenceControlOwner {
	async fn rename_session(
		&self,
		arguments: &Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		let session_id = required_string(arguments, "session_id")?;
		if session_id != self.actor.session_id.as_str() {
			return Err(ControlProtocolError::new(
				"ControlOwnerUnavailable",
				"only the live session may be renamed",
			));
		}
		let title = required_string(arguments, "title")?.trim();
		let host = self
			.actor
			.agent
			.lock()
			.as_ref()
			.and_then(|binding| binding.host.clone())
			.ok_or_else(|| {
				ControlProtocolError::new("ControlOwnerUnavailable", "Agent owner is not bound")
			})?;
		let mut request = Map::new();
		request.insert(String::from("title"), Value::String(title.to_owned()));
		host
			.request("omp.sessions.rename", request)
			.await
			.map_err(protocol_error)?;
		let mut get = Map::new();
		get.insert(String::from("session_id"), Value::String(session_id.to_owned()));
		self.sessions_request("omp.sessions.get", &get)
	}

	async fn journal_mutation(
		&self,
		request_id: u64,
		operation: &str,
		arguments: &Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		if let Some(expected) = arguments
			.get("expected_context_epoch")
			.and_then(Value::as_u64)
		{
			let host = self
				.actor
				.agent
				.lock()
				.as_ref()
				.and_then(|binding| binding.host.clone())
				.ok_or_else(|| {
					ControlProtocolError::new("ControlOwnerUnavailable", "Agent owner is not bound")
				})?;
			let actual = host
				.request("omp.context.epoch", Map::new())
				.await
				.map_err(protocol_error)?
				.as_u64()
				.ok_or_else(|| {
					ControlProtocolError::new("InvalidContext", "Agent returned an invalid epoch")
				})?;
			if expected != actual {
				return Err(
					ControlProtocolError::new(
						"StaleEpoch",
						format!("expected context epoch {expected}, current epoch is {actual}"),
					)
					.with_details(json!({"expected": expected, "actual": actual})),
				);
			}
		}
		let stamp = JournalRequestStamp {
			request_id:         Str::from(request_id.to_string()),
			idempotency_key:    Str::from(required_string(arguments, "idempotency_key")?),
			host_generation:    self.identity.host_generation,
			session_generation: self.identity.session_generation,
		};
		let author = JournalAuthor {
			principal:  self.journal_identity.principal.clone(),
			provenance: self.journal_identity.provenance.clone(),
		};
		let requested = match operation {
			"omp.journal.append" => JournalOperation::Append(pending_control_entry(
				arguments
					.get("entry")
					.and_then(Value::as_object)
					.ok_or_else(|| invalid_argument("entry", "must be an object"))?,
				arguments.get("display").and_then(Value::as_bool),
			)?),
			"omp.journal.append_many" | "omp.journal.append_atomic" => {
				let entries = arguments
					.get("entries")
					.and_then(Value::as_array)
					.ok_or_else(|| invalid_argument("entries", "must be an array"))?
					.iter()
					.map(|entry| {
						pending_control_entry(
							entry
								.as_object()
								.ok_or_else(|| invalid_argument("entries", "rows must be objects"))?,
							None,
						)
					})
					.collect::<Result<Vec<_>, _>>()?;
				if operation == "omp.journal.append_many" {
					JournalOperation::AppendMany(entries)
				} else {
					JournalOperation::AppendAtomic(entries)
				}
			},
			"omp.journal.label" => {
				let (session, index) = parse_entry_id(required_string(arguments, "target")?)?;
				if session != self.actor.session_id.as_str() {
					return Err(ControlProtocolError::new(
						"EntryAccessDenied",
						"journal label target belongs to another session",
					));
				}
				JournalOperation::Label {
					target: index,
					label:  arguments
						.get("label")
						.and_then(Value::as_str)
						.map(Str::from),
				}
			},
			_ => unreachable!(),
		};
		let reply = self
			.sender()?
			.journal(JournalRequest {
				ts: epoch_millis().map_err(protocol_error)?,
				stamp,
				author,
				operation: requested,
			})
			.await
			.map_err(|error| protocol_error(Str::from(error.to_string())))?;
		let mut ids = reply
			.indexes
			.into_iter()
			.map(|index| json!({"session": self.actor.session_id.as_str(), "index": index}))
			.collect::<Vec<_>>();
		let result = if matches!(operation, "omp.journal.append" | "omp.journal.label") {
			ids.pop().unwrap_or(Value::Null)
		} else {
			Value::Array(ids)
		};
		Ok(json!({"schema": format!("{operation}.v1"), "result": result}))
	}

	async fn journal_query(
		&self,
		operation: &str,
		arguments: &Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		let since = arguments
			.get("since")
			.and_then(Value::as_str)
			.map(parse_entry_id)
			.transpose()?
			.map(|(session, index)| {
				if session == self.actor.session_id.as_str() {
					Ok(index)
				} else {
					Err(ControlProtocolError::new(
						"EntryAccessDenied",
						"journal cursor belongs to another session",
					))
				}
			})
			.transpose()?;
		let rows = self
			.sender()?
			.query(vec![JournalQuery {
				caller_extension: self.identity.extension.clone(),
				granted_extensions: Vec::new(),
				kind: arguments.get("kind").and_then(Value::as_str).map(Str::from),
				rev: arguments.get("rev").and_then(Value::as_str).map(Str::from),
				since,
				limit: if operation == "omp.journal.latest" {
					Some(1)
				} else {
					arguments
						.get("limit")
						.and_then(Value::as_u64)
						.map(|limit| usize::try_from(limit).unwrap_or(usize::MAX))
				},
				live: arguments
					.get("live")
					.and_then(Value::as_bool)
					.unwrap_or(true),
			}])
			.await
			.map_err(|error| protocol_error(Str::from(error.to_string())))?;
		let mut values = rows
			.iter()
			.map(|row| journal_entry_json(&self.actor.session_id, row))
			.collect::<Result<Vec<_>, _>>()?;
		let result = if operation == "omp.journal.latest" {
			values.pop().unwrap_or(Value::Null)
		} else {
			Value::Array(values)
		};
		Ok(json!({"schema": format!("{operation}.v1"), "result": result}))
	}
}
impl PersistenceControlOwner {
	async fn state_request(
		&self,
		request_id: u64,
		operation: &str,
		arguments: &Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		let scope = parse_control_scope(
			arguments
				.get("scope")
				.ok_or_else(|| invalid_argument("scope", "is required"))?,
		)?;
		let authority = self.state_authority()?;
		let request = DurableRequest::new(
			request_id.to_string(),
			arguments
				.get("idempotency_key")
				.and_then(Value::as_str)
				.map(Str::from),
			GenerationFence {
				host:    self.identity.host_generation,
				session: self.identity.session_generation,
			},
		)
		.map_err(protocol_error)?;
		match operation {
			"omp.state.append" => {
				let entry = arguments
					.get("entry")
					.and_then(Value::as_object)
					.ok_or_else(|| invalid_argument("entry", "must be an object"))?;
				let pending = pending_control_entry(entry, None)?;
				let data = pending
					.data
					.as_ref()
					.map_or_else(|| b"null".to_vec(), |raw| raw.get().as_bytes().to_vec());
				let index = if scope == StateScope::Session {
					self
						.sender()?
						.journal(JournalRequest {
							ts:        epoch_millis().map_err(protocol_error)?,
							stamp:     JournalRequestStamp {
								request_id:         Str::from(request_id.to_string()),
								idempotency_key:    request.idempotency_key().map(Str::from).ok_or_else(
									|| {
										ControlProtocolError::new(
											"MissingIdempotencyKey",
											"SESSION state append requires an idempotency key",
										)
									},
								)?,
								host_generation:    self.identity.host_generation,
								session_generation: self.identity.session_generation,
							},
							author:    JournalAuthor {
								principal:  self.journal_identity.principal.clone(),
								provenance: self.journal_identity.provenance.clone(),
							},
							operation: JournalOperation::Append(pending),
						})
						.await
						.map_err(|error| protocol_error(Str::from(error.to_string())))?
						.indexes
						.into_iter()
						.next()
						.ok_or_else(|| {
							ControlProtocolError::new(
								"PersistenceError",
								"SESSION state append returned no index",
							)
						})?
				} else {
					state_owner(self.actor.state.as_deref())
						.map_err(protocol_error)?
						.append(&authority, scope, pending.kind, pending.rev, &data, &request)
						.map_err(protocol_error)?
						.revision()
						.get()
				};
				Ok(json!({"scope": scope_instance(&authority, scope), "index": index}))
			},
			"omp.state.entries" | "omp.state.latest" => {
				let kind = required_string(arguments, "kind")?;
				let since = arguments
					.get("since")
					.and_then(|value| {
						value
							.as_u64()
							.or_else(|| value.as_object()?.get("index")?.as_u64())
					})
					.map(StateRevision::new);
				let limit = if operation == "omp.state.latest" {
					Some(1)
				} else {
					arguments
						.get("limit")
						.and_then(Value::as_u64)
						.map(|value| usize::try_from(value).unwrap_or(usize::MAX))
				};
				let mut rows = if scope == StateScope::Session {
					self
						.sender()?
						.query(vec![JournalQuery {
							caller_extension: self.identity.extension.clone(),
							granted_extensions: Vec::new(),
							kind: Some(Str::from(kind)),
							rev: None,
							since: since.map(StateRevision::get),
							limit,
							live: true,
						}])
						.await
						.map_err(|error| protocol_error(Str::from(error.to_string())))?
						.iter()
						.map(|row| state_journal_entry_json(&self.actor.session_id, row))
						.collect::<Result<Vec<_>, _>>()?
				} else {
					state_owner(self.actor.state.as_deref())
						.map_err(protocol_error)?
						.entries(&authority, scope, authority.namespace(), kind, since, limit)
						.map_err(protocol_error)?
						.map(state_entry_json)
						.collect::<Result<Vec<_>, _>>()?
				};
				Ok(if operation == "omp.state.latest" {
					rows.pop().unwrap_or(Value::Null)
				} else {
					Value::Array(rows)
				})
			},
			"omp.state.cas_put" => {
				let data = control_bytes(
					arguments
						.get("data")
						.ok_or_else(|| invalid_argument("data", "is required"))?,
				)?;
				if scope == StateScope::Session {
					let reference = BlobRef::from(self.actor.blobs.put(&data).map_err(protocol_error)?);
					let root = self
						.sender()?
						.session_state_root_content(
							epoch_millis().map_err(protocol_error)?,
							authority,
							reference,
							request,
						)
						.await
						.map_err(|error| protocol_error(Str::from(error.to_string())))?;
					return Ok(json!({
						"hash": root.reference.hash.to_hex().as_str(),
						"size": root.reference.size,
					}));
				}
				let root = state_owner(self.actor.state.as_deref())
					.map_err(protocol_error)?
					.put_content(&authority, scope, &data, &request)
					.map_err(protocol_error)?;
				Ok(json!({
					"hash": root.reference.hash.to_hex().as_str(),
					"size": root.reference.size,
				}))
			},
			"omp.state.cas_get" => {
				let reference = parse_control_blob(
					arguments
						.get("ref")
						.and_then(Value::as_object)
						.ok_or_else(|| invalid_argument("ref", "must be an object"))?,
				)?;
				if scope == StateScope::Session {
					let rooted = self
						.sender()?
						.session_state_content_is_rooted(authority, reference)
						.await
						.map_err(|error| protocol_error(Str::from(error.to_string())))?;
					if !rooted {
						return Err(ControlProtocolError::new(
							"ContentNotRooted",
							"blob is not rooted by the live SESSION journal",
						));
					}
					let bytes = self
						.actor
						.blobs
						.get(BlobId::from(reference))
						.map_err(protocol_error)?;
					return Ok(json!({"$bytes": omp_core::base64::encode(bytes.as_ref())}));
				}
				let bytes = state_owner(self.actor.state.as_deref())
					.map_err(protocol_error)?
					.get_content(&authority, scope, authority.namespace(), &reference)
					.map_err(protocol_error)?;
				Ok(json!({"$bytes": omp_core::base64::encode(bytes.as_ref())}))
			},
			_ => unreachable!(),
		}
	}
}
impl PersistenceControlOwner {
	fn sessions_request(
		&self,
		operation: &str,
		arguments: &Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		match operation {
			"omp.sessions.list" => {
				let filter = arguments.get("filter").and_then(Value::as_object);
				let filter = parse_session_filter(filter)?;
				let page = self.actor.sessions.list(&filter).map_err(protocol_error)?;
				Ok(Value::Array(
					page
						.sessions
						.iter()
						.map(session_info_json)
						.collect::<Vec<_>>(),
				))
			},
			"omp.sessions.get" => {
				let session = SessionId(Str::from(required_string(arguments, "session_id")?));
				let row = self
					.actor
					.sessions
					.get(&session)
					.map_err(protocol_error)?
					.ok_or_else(|| {
						ControlProtocolError::new(
							"SessionNotFound",
							format!("session {} is not indexed", session.0),
						)
					})?;
				Ok(session_info_json(&row))
			},
			"omp.sessions.lineage" => {
				let session = SessionId(Str::from(required_string(arguments, "session_id")?));
				let links = self
					.actor
					.sessions
					.lineage(&session)
					.map_err(protocol_error)?;
				if links.is_empty() {
					return Err(ControlProtocolError::new(
						"SessionNotFound",
						format!("session {} is not indexed", session.0),
					));
				}
				Ok(Value::Array(
					links
						.into_iter()
						.map(|link| {
							json!({
								"id": link.id.0.as_str(),
								"parent": link.parent.map(|parent| parent.0),
								"at": link.at,
							})
						})
						.collect(),
				))
			},
			"omp.sessions.usage" => {
				let query = arguments
					.get("query")
					.and_then(Value::as_object)
					.ok_or_else(|| invalid_argument("query", "must be an object"))?;
				let usage_query = parse_usage_query(query)?;
				let groups = self
					.actor
					.sessions
					.usage(&usage_query)
					.map_err(protocol_error)?;
				let mut total_query = usage_query.clone();
				total_query.group_by.clear();
				total_query.bucket = UsageBucketWidth::None;
				let total = self
					.actor
					.sessions
					.usage(&total_query)
					.map_err(protocol_error)?
					.into_iter()
					.next()
					.map_or_else(empty_usage_bucket_json, |bucket| usage_bucket_json(&bucket));
				let sessions = groups
					.iter()
					.map(|bucket| bucket.sessions)
					.max()
					.unwrap_or(0);
				let (series, groups): (Vec<_>, Vec<_>) = groups
					.iter()
					.map(usage_bucket_json)
					.partition(|row| row.get("start_ms").is_some_and(|value| !value.is_null()));
				Ok(json!({
					"total": total,
					"groups": groups,
					"series": series,
					"sessions": sessions,
					"truncated": false,
				}))
			},
			"omp.sessions.journal" => {
				let session = SessionId(Str::from(required_string(arguments, "session_id")?));
				let info = self
					.actor
					.sessions
					.get(&session)
					.map_err(protocol_error)?
					.ok_or_else(|| {
						ControlProtocolError::new(
							"SessionNotFound",
							format!("session {} is not indexed", session.0),
						)
					})?;
				if info.project != self.actor.project_path {
					return Err(ControlProtocolError::new(
						"SessionAccessDenied",
						"historical session belongs to another project authority",
					));
				}
				historical_journal_page(&self.actor.sessions_dir, &session, arguments)
			},
			_ => unreachable!(),
		}
	}
}
impl PersistenceControlOwner {
	fn artifact_request(
		&self,
		_request_id: u64,
		operation: &str,
		arguments: &Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		let session = SessionId(self.actor.session_id.clone());
		match operation {
			"omp.artifacts.adopt" => {
				let blob = parse_control_blob(
					arguments
						.get("blob")
						.and_then(Value::as_object)
						.ok_or_else(|| invalid_argument("blob", "must be an object"))?,
				)?;
				let lifetime = parse_lifetime(
					arguments
						.get("lifetime")
						.and_then(Value::as_str)
						.unwrap_or("session"),
				)
				.map_err(protocol_error)?;
				let record = self
					.actor
					.catalog
					.lock()
					.adopt(&session, blob.hash.into_bytes(), Some(blob.size), lifetime)
					.map_err(|error| protocol_error(artifact_error(error)))?;
				let metadata = self
					.actor
					.artifact_meta
					.record(
						record.catalog_id,
						arguments.get("media_type").and_then(Value::as_str),
						arguments.get("description").and_then(Value::as_str),
						self.identity.extension.as_str(),
					)
					.map_err(protocol_error)?;
				Ok(artifact_ref_json(&record, &metadata))
			},
			"omp.artifacts.stat" => {
				let requested = arguments
					.get("ref")
					.and_then(Value::as_object)
					.ok_or_else(|| invalid_argument("ref", "must be an object"))?;
				let record = self.stat_control_artifact(requested, &session)?;
				let metadata = self
					.actor
					.artifact_meta
					.get(record.catalog_id)
					.map_err(protocol_error)?
					.ok_or_else(|| {
						ControlProtocolError::new("ArtifactCorrupt", "artifact metadata is missing")
					})?;
				Ok(artifact_stat_json(&record, &metadata, &session))
			},
			"omp.artifacts.list" => {
				let requested = arguments
					.get("session")
					.and_then(Value::as_str)
					.filter(|value| !value.is_empty())
					.unwrap_or(self.actor.session_id.as_str());
				if requested != self.actor.session_id.as_str() {
					return Err(ControlProtocolError::new(
						"SessionAccessDenied",
						"artifact listing is restricted to the active session",
					));
				}
				let page = self
					.actor
					.catalog
					.lock()
					.list(
						Some(&session),
						None,
						arguments
							.get("limit")
							.and_then(Value::as_u64)
							.and_then(|limit| u32::try_from(limit).ok())
							.unwrap_or(200),
					)
					.map_err(|error| protocol_error(artifact_error(error)))?;
				page
					.records
					.iter()
					.map(|record| {
						let metadata = self
							.actor
							.artifact_meta
							.get(record.catalog_id)
							.map_err(protocol_error)?
							.ok_or_else(|| {
								ControlProtocolError::new("ArtifactCorrupt", "artifact metadata is missing")
							})?;
						Ok(artifact_stat_json(record, &metadata, &session))
					})
					.collect::<Result<Vec<_>, _>>()
					.map(Value::Array)
			},
			"omp.artifacts.pin" => {
				let requested = arguments
					.get("ref")
					.and_then(Value::as_object)
					.ok_or_else(|| invalid_argument("ref", "must be an object"))?;
				let record = self.stat_control_artifact(requested, &session)?;
				let lifetime =
					parse_lifetime(required_string(arguments, "lifetime")?).map_err(protocol_error)?;
				self
					.actor
					.catalog
					.lock()
					.pin(record.catalog_id, lifetime)
					.map_err(|error| protocol_error(artifact_error(error)))?;
				Ok(Value::Null)
			},
			_ => unreachable!(),
		}
	}

	fn stat_control_artifact(
		&self,
		requested: &Map<String, Value>,
		session: &SessionId,
	) -> Result<ArtifactRecord, ControlProtocolError> {
		let id = required_string(requested, "id")?;
		let url = ArtifactUrl::new(format!("artifact://{id}")).map_err(protocol_error)?;
		let record = self
			.actor
			.catalog
			.lock()
			.stat_url(session, &url)
			.map_err(|error| protocol_error(artifact_error(error)))?;
		let claimed = parse_control_blob(requested)?;
		if claimed != record.reference {
			return Err(ControlProtocolError::new(
				"ArtifactCorrupt",
				"artifact reference disagrees with authoritative content identity",
			));
		}
		Ok(record)
	}
}

#[allow(
	clippy::too_many_arguments,
	reason = "the actor carries distinct authenticated authorities"
)]
async fn dispatch(
	request: ExternalJournalRequest,
	identity: JournalConnectionIdentity,
	sessions: &SessionIndex,
	state: Option<&StateStore>,
	session_id: &Str,
	project_scope: &Str,
	project_path: &Str,
	catalog: &Mutex<ArtifactCatalog>,
	agent: &Mutex<Option<AgentBinding>>,
	reply: &flume::Sender<Result<JournalHostEnvelope, Str>>,
) -> Result<(), Str> {
	match request {
		ExternalJournalRequest::Query { extension, query, .. } => {
			if query.session != session_id.as_str() {
				return unsupported("cross-session journal query");
			}
			let sender = bound_agent(agent)?;
			let kinds = if query.kinds.is_empty() {
				vec![None]
			} else {
				query
					.kinds
					.into_iter()
					.map(|kind| Some(Str::from(kind)))
					.collect()
			};
			let limit = query.limit.map(|limit| limit as usize);
			let queries = kinds
				.into_iter()
				.map(|kind| JournalQuery {
					caller_extension: extension.clone(),
					granted_extensions: Vec::new(),
					kind,
					rev: None,
					since: query.since_index,
					limit,
					live: query.live_only,
				})
				.collect();
			let rows = sender.query(queries).await.map_err(display_error)?;
			emit(reply, journal_rows(&rows));
		},
		ExternalJournalRequest::ListSessions { query, .. } => {
			if query.cursor.is_some() {
				return unsupported("sessions cursor pagination");
			}
			let page = sessions
				.list(&SessionFilter {
					project: Some(project_path.clone()),
					limit: query.limit.unwrap_or(100),
					..SessionFilter::default()
				})
				.map_err(display_error)?;
			let rows = page.sessions.into_iter().map(|row| SessionRow {
				session:       row.id.0.to_string(),
				cwd_uri:       row.cwd.to_string(),
				updated_at_ms: row.updated_ms,
				terminal:      false,
				props:         None,
			});
			emit(reply, session_rows(rows));
		},
		ExternalJournalRequest::QueryUsage { query, .. } => {
			let session = query.session.map(|session| SessionId(Str::from(session)));
			let usage = sessions
				.usage(&UsageQuery {
					project: session.is_none().then(|| project_path.clone()),
					session,
					include_subagents: query.include_tree,
					..UsageQuery::default()
				})
				.map_err(display_error)?;
			emit(
				reply,
				usage_rows(usage.into_iter().map(|bucket| UsageReport {
					usage:    Some(bucket.usage),
					terminal: false,
					props:    None,
				})),
			);
		},
		ExternalJournalRequest::StateGet { extension, request, .. } => {
			let authority = state_authority(identity, extension, session_id, project_scope)?;
			let scope = state_scope(request.scope)?;
			let namespace = requested_namespace(&request.namespace, authority.namespace());
			let value = if scope == StateScope::Session {
				require_own_namespace(namespace, authority.namespace())?;
				bound_agent(agent)?
					.session_state_get(authority, Str::from(request.key))
					.await
					.map_err(display_error)?
					.map(|value| {
						(value.revision.get(), Bytes::copy_from_slice(value.value.get().as_bytes()))
					})
			} else {
				state_owner(state)?
					.value(&authority, scope, namespace, &request.key)
					.map_err(display_error)?
					.map(|value| (value.revision.get(), Bytes::copy_from_slice(value.value.as_ref())))
			};
			let _ = reply.send(Ok(state_value(value)));
		},
		ExternalJournalRequest::StateCas { stamp, author, request, .. } => {
			let authority = state_authority(
				identity,
				Str::from(author.provenance.extension_id()),
				session_id,
				project_scope,
			)?;
			require_own_namespace(
				requested_namespace(&request.namespace, authority.namespace()),
				authority.namespace(),
			)?;
			let scope = state_scope(request.scope)?;
			let expected =
				(request.expected_revision != 0).then(|| StateRevision::new(request.expected_revision));
			let durable =
				DurableRequest::new(stamp.request_id, Some(stamp.idempotency_key), GenerationFence {
					host:    stamp.host_generation,
					session: stamp.session_generation,
				})
				.map_err(display_error)?;
			let value = if scope == StateScope::Session {
				let raw = serde_json::from_slice(&request.value_json).map_err(display_error)?;
				let value = bound_agent(agent)?
					.session_state_compare_exchange(
						epoch_millis()?,
						authority,
						Str::from(request.key),
						expected,
						raw,
						durable,
					)
					.await
					.map_err(display_error)?;
				(value.revision.get(), Bytes::copy_from_slice(value.value.get().as_bytes()))
			} else {
				let value = state_owner(state)?
					.compare_exchange(
						&authority,
						scope,
						Str::from(request.key),
						expected,
						&request.value_json,
						&durable,
					)
					.map_err(display_error)?;
				(value.revision.get(), Bytes::copy_from_slice(value.value.as_ref()))
			};
			let _ = reply.send(Ok(state_value(Some(value))));
		},
		ExternalJournalRequest::StateWatch { extension, request, .. } => {
			let authority = state_authority(identity, extension, session_id, project_scope)?;
			let scope = state_scope(request.scope)?;
			let namespace = requested_namespace(&request.namespace, authority.namespace());
			let since =
				(request.after_revision != 0).then(|| StateRevision::new(request.after_revision));
			if scope == StateScope::Session {
				require_own_namespace(namespace, authority.namespace())?;
				let events = bound_agent(agent)?
					.session_state_watch(authority, Str::from(request.key), since)
					.await
					.map_err(display_error)?;
				loop {
					tokio::select! {
						event = events.recv_async() => {
							let Ok(event) = event else { break };
							match event {
								SessionStateWatchEvent::Value(value) => {
									if reply
										.send(Ok(state_changed(
											value.revision.get(),
											Bytes::copy_from_slice(value.value.get().as_bytes()),
										)))
										.is_err()
									{
										break;
									}
								},
								SessionStateWatchEvent::Terminal(_) => break,
							}
						},
						() = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
							if reply.is_disconnected() {
								break;
							}
						},
					}
				}
			} else {
				let watcher = state_owner(state)?
					.watch(&authority, scope, namespace, since)
					.map_err(display_error)?;
				loop {
					tokio::select! {
						change = watcher.recv_async() => {
							let Ok(change) = change else { break };
							if let StateChange::Value(value) = change
								&& value.key == request.key
								&& reply
									.send(Ok(state_changed(
										value.revision.get(),
										Bytes::copy_from_slice(value.value.as_ref()),
									)))
									.is_err()
							{
								break;
							}
						},
						() = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
							if reply.is_disconnected() {
								break;
							}
						},
					}
				}
			}
		},
		ExternalJournalRequest::AdoptArtifact { stamp, author, request, .. } => {
			let lifetime = parse_lifetime(&request.lifetime)?;
			let current_session = SessionId(session_id.clone());
			let durable_request = ArtifactRequest {
				principal:          author.principal.id(),
				extension:          author.provenance.extension_id(),
				idempotency_key:    stamp.idempotency_key.as_str(),
				session:            &current_session,
				host_generation:    stamp.host_generation,
				session_generation: stamp.session_generation,
			};
			let record = if request.source_url.starts_with("artifact://") {
				let source = parse_artifact_url(&request.source_url)?;
				catalog
					.lock()
					.adopt_url_once(durable_request, &source, None, lifetime)
					.map_err(artifact_error)?
			} else {
				let (hash, claimed_size) = parse_blob_source(&request.source_url)?;
				catalog
					.lock()
					.adopt_once(durable_request, hash, claimed_size, lifetime)
					.map_err(artifact_error)?
			};
			let row = artifact_row(&record, &current_session)
				.ok_or_else(|| sf!("artifact is not visible from this session"))?;
			emit(reply, artifact_rows([row]));
		},
		ExternalJournalRequest::StatArtifact { request, .. } => {
			let url = parse_artifact_url(&request.url)?;
			let current_session = SessionId(session_id.clone());
			let record = catalog
				.lock()
				.stat_url(&current_session, &url)
				.map_err(display_error)?;
			let row = artifact_row(&record, &current_session)
				.ok_or_else(|| sf!("artifact is not visible from this session"))?;
			emit(reply, artifact_rows([row]));
		},
		ExternalJournalRequest::ListArtifacts { request, .. } => {
			let requested_session = request
				.session
				.filter(|session| !session.is_empty())
				.map(Str::from);
			let requested_session = requested_session
				.as_ref()
				.map_or_else(|| SessionId(session_id.clone()), |session| SessionId(session.clone()));
			if requested_session.0.as_str() != session_id.as_str() {
				return Err(
					sf!("Denied: non-durable cross-session artifact listing is not permitted",),
				);
			}
			let cursor = request
				.cursor
				.as_deref()
				.map(str::parse::<u64>)
				.transpose()
				.map_err(display_error)?;
			let page = catalog
				.lock()
				.list(Some(&requested_session), cursor, request.limit.unwrap_or(200))
				.map_err(display_error)?;
			let current_session = SessionId(session_id.clone());
			let mut rows = page
				.records
				.iter()
				.filter_map(|record| artifact_row(record, &current_session))
				.collect::<Vec<_>>();
			if let Some(cursor) = page.next_cursor
				&& let Some(last) = rows.last_mut()
			{
				last.props = Some(cursor_props(cursor));
			}
			emit(reply, artifact_rows(rows));
		},
		ExternalJournalRequest::PinArtifact { stamp, author, request, .. } => {
			let url = parse_artifact_url(&request.url)?;
			let lifetime = parse_lifetime(&request.lifetime)?;
			let current_session = SessionId(session_id.clone());
			let durable_request = ArtifactRequest {
				principal:          author.principal.id(),
				extension:          author.provenance.extension_id(),
				idempotency_key:    stamp.idempotency_key.as_str(),
				session:            &current_session,
				host_generation:    stamp.host_generation,
				session_generation: stamp.session_generation,
			};
			let record = catalog
				.lock()
				.pin_url_once(durable_request, &url, lifetime)
				.map_err(artifact_error)?;
			let row = artifact_row(&record, &current_session)
				.ok_or_else(|| sf!("artifact is not visible from this session"))?;
			emit(reply, artifact_rows([row]));
		},
	}
	Ok(())
}
fn parse_blob_source(source: &str) -> Result<([u8; 32], Option<u64>), Str> {
	let source = source
		.strip_prefix("blob://")
		.ok_or_else(|| sf!("invalid blob source URL"))?;
	let (resource, query) = source
		.split_once('?')
		.map_or((source, None), |(resource, query)| (resource, Some(query)));
	let digest = resource
		.strip_prefix("sha256/")
		.ok_or_else(|| sf!("invalid blob source URL"))?;
	let claimed_size = query
		.and_then(|query| {
			query
				.split('&')
				.find_map(|field| field.strip_prefix("size="))
		})
		.map(str::parse::<u64>)
		.transpose()
		.map_err(display_error)?;
	BlobRef::parse_hex(digest, claimed_size.unwrap_or_default())
		.map(|reference| (reference.hash.into(), claimed_size))
		.map_err(display_error)
}

fn parse_artifact_url(value: &str) -> Result<ArtifactUrl, Str> {
	let url = ArtifactUrl::new(value).map_err(display_error)?;
	if url.selector().is_some() {
		return Err(sf!("artifact CONTROL URLs cannot carry read selectors"));
	}
	Ok(url)
}

fn parse_lifetime(value: &str) -> Result<ArtifactLifetime, Str> {
	if value.is_empty() {
		Ok(ArtifactLifetime::Session)
	} else {
		value.parse().map_err(display_error)
	}
}

fn artifact_row(record: &ArtifactRecord, session: &SessionId) -> Option<ArtifactRow> {
	let lifetime: &'static str = record.lifetime.into();
	Some(ArtifactRow {
		url:      record.url_for(session)?.as_str().to_owned(),
		hash:     Bytes::copy_from_slice(record.reference.hash.as_bytes()),
		size:     record.reference.size,
		lifetime: lifetime.to_owned(),
		pinned:   record.pinned,
		terminal: false,
		props:    None,
	})
}

fn cursor_props(cursor: u64) -> v1::ValueMap {
	v1::ValueMap {
		fields: BTreeMap::from([("next_cursor".to_owned(), v1::Value {
			kind: Some(value::Kind::Uint(cursor)),
		})]),
	}
}

fn emit(
	reply: &flume::Sender<Result<JournalHostEnvelope, Str>>,
	rows: impl Iterator<Item = JournalHostEnvelope>,
) {
	for row in rows {
		if reply.send(Ok(row)).is_err() {
			break;
		}
	}
}

fn state_authority(
	identity: JournalConnectionIdentity,
	extension: Str,
	session_id: &Str,
	project_id: &Str,
) -> Result<StateAuthority, Str> {
	let generation =
		GenerationFence { host: identity.host_generation, session: identity.session_generation };
	StateAuthority::new_core(
		identity.principal,
		identity.provenance,
		extension,
		session_id.clone(),
		project_id.clone(),
		generation,
	)
	.map_err(display_error)
}

fn state_scope(scope: i32) -> Result<StateScope, Str> {
	use omp_proto::toolhost::v1;
	match v1::StateScope::try_from(scope) {
		Ok(v1::StateScope::Session) => Ok(StateScope::Session),
		Ok(v1::StateScope::Project) => Ok(StateScope::Project),
		Ok(v1::StateScope::User) => Ok(StateScope::User),
		Ok(v1::StateScope::Organization) => Ok(StateScope::Organization),
		_ => Err(sf!("Unsupported: invalid durable state scope")),
	}
}

const fn requested_namespace<'a>(requested: &'a str, own: &'a str) -> &'a str {
	if requested.is_empty() { own } else { requested }
}

fn require_own_namespace(requested: &str, own: &str) -> Result<(), Str> {
	if requested == own {
		Ok(())
	} else {
		Err(sf!("Denied: state writes and SESSION state reads require the authenticated namespace",))
	}
}

fn state_owner(state: Option<&StateStore>) -> Result<&StateStore, Str> {
	state.ok_or_else(|| sf!("Unsupported: this environment is not the durable state authority"))
}

fn bound_agent(agent: &Mutex<Option<AgentBinding>>) -> Result<ControlSender, Str> {
	agent
		.lock()
		.as_ref()
		.map(|binding| binding.sender.clone())
		.ok_or_else(|| sf!("agent journal CONTROL is not bound"))
}

fn state_value(value: Option<(u64, Bytes)>) -> JournalHostEnvelope {
	let (revision, value_json, present) =
		value.map_or((0, Bytes::new(), false), |(revision, value)| (revision, value, true));
	JournalHostEnvelope {
		body:  Some(journal_host_envelope::Body::StateValue(WireStateValue {
			revision,
			value_json,
			present,
			props: None,
		})),
		props: None,
	}
}

const fn state_changed(revision: u64, value_json: Bytes) -> JournalHostEnvelope {
	JournalHostEnvelope {
		body:  Some(journal_host_envelope::Body::StateChanged(WireStateChanged {
			revision,
			value_json,
			props: None,
		})),
		props: None,
	}
}

fn epoch_millis() -> Result<u64, Str> {
	let millis = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_err(display_error)?
		.as_millis();
	u64::try_from(millis).map_err(display_error)
}

fn unsupported<T>(operation: &'static str) -> Result<T, Str> {
	Err(Str::from(format!("Unsupported: {operation} is not available")))
}

fn artifact_error(error: ArtifactError) -> Str {
	match error {
		ArtifactError::IdempotencyConflict(key) => Str::from(format!("IdempotencyConflict: {key}")),
		error => display_error(error),
	}
}

fn display_error(error: impl Display) -> Str {
	Str::from(error.to_string())
}
fn required_string<'a>(
	arguments: &'a Map<String, Value>,
	name: &'static str,
) -> Result<&'a str, ControlProtocolError> {
	arguments
		.get(name)
		.and_then(Value::as_str)
		.filter(|value| !value.is_empty())
		.ok_or_else(|| invalid_argument(name, "must be a non-empty string"))
}

fn invalid_argument(name: &'static str, reason: &'static str) -> ControlProtocolError {
	ControlProtocolError::new("InvalidArgument", format!("{name} {reason}"))
		.with_details(json!({"field": name}))
}

fn protocol_error(message: impl Display) -> ControlProtocolError {
	let message = message.to_string();
	let code = message
		.split_once(':')
		.map_or("PersistenceError", |(code, _)| code);
	ControlProtocolError::new(Str::from(code), Str::from(message))
}
fn schedule_protocol_error(error: DurableScheduleError) -> ControlProtocolError {
	let details = match &error {
		DurableScheduleError::Invalid { field, reason } => {
			json!({"field": field, "reason": reason})
		},
		DurableScheduleError::NotFound => json!({"reason": "schedule was not found"}),
		DurableScheduleError::NotOwned => json!({"reason": "schedule is not owned by caller"}),
		_ => Value::Null,
	};
	let (code, retryable) = match &error {
		DurableScheduleError::StaleGeneration { .. } => ("StaleGeneration", true),
		DurableScheduleError::Closed | DurableScheduleError::Delivery(_) => {
			("ScheduleUnavailable", true)
		},
		DurableScheduleError::Storage(_) => ("ScheduleUnavailable", false),
		DurableScheduleError::NotFound
		| DurableScheduleError::NotOwned
		| DurableScheduleError::Invalid { .. } => ("ScheduleRejected", false),
	};
	ControlProtocolError::new(code, error.to_string())
		.retryable(retryable)
		.with_details(details)
}

fn parse_entry_id(value: &str) -> Result<(&str, u64), ControlProtocolError> {
	let (session, index) = value
		.rsplit_once(':')
		.ok_or_else(|| invalid_argument("entry_id", "must use <session>:<index>"))?;
	if session.is_empty() {
		return Err(invalid_argument("entry_id", "has an empty session"));
	}
	let index = index
		.parse()
		.map_err(|_| invalid_argument("entry_id", "has an invalid index"))?;
	Ok((session, index))
}

fn pending_control_entry(
	entry: &Map<String, Value>,
	display_override: Option<bool>,
) -> Result<PendingCustomEntry, ControlProtocolError> {
	if entry.get("schema").and_then(Value::as_str) != Some("omp.journal.entry.v1") {
		return Err(invalid_argument("entry.schema", "is not omp.journal.entry.v1"));
	}
	let data = required_string(entry, "data")?;
	let value: Value =
		serde_json::from_str(data).map_err(|_| invalid_argument("entry.data", "is not JSON"))?;
	let canonical = serde_json::to_string(&value)
		.map_err(|error| protocol_error(Str::from(error.to_string())))?;
	if canonical != data {
		return Err(ControlProtocolError::new(
			"EntryUndecodable",
			"journal entry data is not canonical JSON",
		));
	}
	let raw = serde_json::from_str::<Box<RawValue>>(data)
		.map_err(|_| invalid_argument("entry.data", "is not canonical JSON"))?;
	Ok(PendingCustomEntry {
		kind:    Str::from(required_string(entry, "kind")?),
		rev:     Str::from(required_string(entry, "rev")?),
		data:    Some(raw),
		context: None,
		display: display_override.or_else(|| entry.get("display").and_then(Value::as_bool)),
	})
}

fn journal_entry_json(
	session: &Str,
	row: &JournalCustomEntry,
) -> Result<Value, ControlProtocolError> {
	let entry = &row.entry;
	let raw = entry.data().map_or("null", |raw| raw.get());
	let value = serde_json::from_str::<Value>(raw)
		.map_err(|_| ControlProtocolError::new("EntryUndecodable", "stored entry JSON is invalid"))?;
	let principal = entry.principal();
	let provenance = entry.provenance();
	Ok(json!({
		"id": {"session": session.as_str(), "index": row.index},
		"kind": entry.kind(),
		"rev": entry.rev().unwrap_or_default(),
		"ts": row.ts,
		"principal": {"$principal": {
			"id": principal.id(),
			"display": principal.display(),
		}},
		"provenance": {"$provenance": {
			"publisher": provenance.publisher(),
			"extension_id": provenance.extension_id(),
			"version": provenance.version(),
			"artifact_digest": provenance.artifact_digest().to_string(),
			"layer": provenance.layer(),
			"tier": provenance.tier(),
			"generation": provenance.generation(),
		}},
		"value": value,
		"raw": raw,
		"display": entry.display(),
		"in_context": entry.context().is_some(),
		"artifact": Value::Null,
	}))
}
fn parse_control_scope(value: &Value) -> Result<StateScope, ControlProtocolError> {
	let value = value
		.as_str()
		.ok_or_else(|| invalid_argument("scope", "must be a string"))?
		.to_ascii_uppercase();
	match value.as_str() {
		"SESSION" => Ok(StateScope::Session),
		"PROJECT" | "WORKSPACE" => Ok(StateScope::Project),
		"USER" => Ok(StateScope::User),
		"ORGANIZATION" => Ok(StateScope::Organization),
		_ => Err(invalid_argument("scope", "is unknown")),
	}
}

fn scope_instance(authority: &StateAuthority, scope: StateScope) -> String {
	format!("{scope}:{}", authority.namespace())
}

fn state_journal_entry_json(
	session: &Str,
	row: &JournalCustomEntry,
) -> Result<Value, ControlProtocolError> {
	let mut value = journal_entry_json(session, row)?;
	let object = value
		.as_object_mut()
		.expect("journal entry JSON is an object");
	object.insert("id".to_owned(), json!({"scope": session.as_str(), "index": row.index}));
	object.remove("display");
	object.remove("in_context");
	Ok(value)
}

fn state_entry_json(entry: StateEntry) -> Result<Value, ControlProtocolError> {
	let raw = str::from_utf8(entry.raw.as_ref())
		.map_err(|_| ControlProtocolError::new("EntryUndecodable", "state entry is not UTF-8"))?;
	let value = serde_json::from_str::<Value>(raw)
		.map_err(|_| ControlProtocolError::new("EntryUndecodable", "state entry JSON is invalid"))?;
	let provenance = &entry.provenance;
	Ok(json!({
		"id": {
			"scope": format!(
				"{}:{}",
				entry.id.scope().scope(),
				entry.id.scope().authority()
			),
			"index": entry.id.revision().get(),
		},
		"kind": entry.kind,
		"rev": entry.schema_rev,
		"ts": entry.timestamp,
		"principal": {"$principal": {
			"id": entry.principal.id(),
			"display": entry.principal.display(),
		}},
		"provenance": {"$provenance": {
			"publisher": provenance.publisher(),
			"extension_id": provenance.extension_id(),
			"version": provenance.version(),
			"artifact_digest": provenance.artifact_digest().to_string(),
			"layer": provenance.layer(),
			"tier": provenance.tier(),
			"generation": provenance.generation(),
		}},
		"value": value,
		"raw": raw,
		"artifact": Value::Null,
	}))
}

fn control_bytes(value: &Value) -> Result<Vec<u8>, ControlProtocolError> {
	let encoded = value
		.as_object()
		.and_then(|value| value.get("$bytes"))
		.and_then(Value::as_str)
		.ok_or_else(|| invalid_argument("data", "must be a CONTROL bytes value"))?;
	omp_core::base64::decode(encoded)
		.into_vec()
		.map_err(|_| invalid_argument("data", "contains invalid base64"))
}

fn parse_control_blob(value: &Map<String, Value>) -> Result<BlobRef, ControlProtocolError> {
	let hash = required_string(value, "hash")?;
	let size = value
		.get("size")
		.or_else(|| value.get("byte_len"))
		.and_then(Value::as_u64)
		.ok_or_else(|| invalid_argument("size", "must be a non-negative integer"))?;
	BlobRef::parse_hex(hash, size).map_err(protocol_error)
}

fn parse_session_filter(
	filter: Option<&Map<String, Value>>,
) -> Result<SessionFilter, ControlProtocolError> {
	let mut parsed = SessionFilter::default();
	let Some(filter) = filter else {
		return Ok(parsed);
	};
	parsed.project = filter.get("project").and_then(Value::as_str).map(Str::from);
	parsed.since_ms = filter.get("since_ms").and_then(Value::as_u64);
	parsed.until_ms = filter.get("until_ms").and_then(Value::as_u64);
	parsed.contains_kind = filter
		.get("contains_kind")
		.and_then(Value::as_str)
		.map(Str::from);
	parsed.limit = filter
		.get("limit")
		.and_then(Value::as_u64)
		.and_then(|limit| u32::try_from(limit).ok())
		.unwrap_or(200);
	if let Some(statuses) = filter.get("status").and_then(Value::as_array) {
		for status in statuses {
			parsed.statuses.push(
				status
					.as_str()
					.ok_or_else(|| invalid_argument("filter.status", "must contain strings"))?
					.parse::<SessionStatus>()
					.map_err(protocol_error)?,
			);
		}
	}
	if let Some(kinds) = filter.get("kind").and_then(Value::as_array) {
		for kind in kinds {
			parsed.kinds.push(
				kind
					.as_str()
					.ok_or_else(|| invalid_argument("filter.kind", "must contain strings"))?
					.parse::<SessionKind>()
					.map_err(protocol_error)?,
			);
		}
	}
	Ok(parsed)
}

fn parse_usage_query(query: &Map<String, Value>) -> Result<UsageQuery, ControlProtocolError> {
	let mut parsed = UsageQuery {
		since_ms: query.get("since_ms").and_then(Value::as_u64),
		until_ms: query.get("until_ms").and_then(Value::as_u64),
		include_subagents: query
			.get("include_subagents")
			.and_then(Value::as_bool)
			.unwrap_or(true),
		..UsageQuery::default()
	};
	parsed.group_by.clear();
	if let Some(dimensions) = query.get("group_by").and_then(Value::as_array) {
		for dimension in dimensions {
			let dimension = match dimension.as_str() {
				Some("model") => UsageDimension::Model,
				Some("provider") => UsageDimension::Provider,
				Some("project") => UsageDimension::Project,
				Some("session") => UsageDimension::SessionId,
				Some("kind") => UsageDimension::SessionKind,
				_ => return Err(invalid_argument("query.group_by", "contains an unknown value")),
			};
			parsed.group_by.push(dimension);
		}
	}
	parsed.bucket = match query
		.get("bucket")
		.and_then(Value::as_str)
		.unwrap_or("none")
	{
		"none" => UsageBucketWidth::None,
		"hour" => UsageBucketWidth::Hour,
		"day" => UsageBucketWidth::Day,
		"week" => UsageBucketWidth::Week,
		"month" => UsageBucketWidth::Month,
		_ => return Err(invalid_argument("query.bucket", "is unknown")),
	};
	if let Some(filter) = query.get("filter").and_then(Value::as_object) {
		let filter = parse_session_filter(Some(filter))?;
		parsed.project = filter.project;
		parsed.since_ms = parsed.since_ms.or(filter.since_ms);
		parsed.until_ms = parsed.until_ms.or(filter.until_ms);
	}
	Ok(parsed)
}

fn session_info_json(info: &SessionInfo) -> Value {
	json!({
		"id": info.id.0.as_str(),
		"title": info.title,
		"title_source": info
			.title_source
			.map_or_else(|| "system".to_owned(), |source| source.to_string()),
		"cwd": info.cwd,
		"project": info.project,
		"created_ms": info.created_ms,
		"updated_ms": info.updated_ms,
		"status": info.status.to_string(),
		"kind": info.kind.to_string(),
		"parent": info.parent.as_ref().map(|parent| parent.0.as_str()),
		"entries": info.entries,
		"turns": info.turns,
		"usage": usage_json(&info.usage),
		"cost": cost_json(&info.cost),
		"models": info.models,
		"remote": info.remote,
	})
}

fn usage_json(usage: &v1::Usage) -> Value {
	let accuracy = match usage::Accuracy::try_from(usage.accuracy) {
		Ok(usage::Accuracy::Exact) => "exact",
		Ok(usage::Accuracy::Estimated) => "estimated",
		Ok(usage::Accuracy::Mixed) => "mixed",
		_ => "exact",
	};
	json!({
		"input": usage.input_tokens,
		"output": usage.output_tokens,
		"cache_read": usage.cache_read_tokens,
		"cache_write": usage.cache_write_tokens,
		"reasoning": usage.reasoning_tokens.unwrap_or(0),
		"premium_requests": usage.premium_requests.unwrap_or(0),
		"context": usage.context_tokens,
		"total": usage.total_tokens.unwrap_or_else(|| usage.input_tokens
			.saturating_add(usage.output_tokens)
			.saturating_add(usage.cache_read_tokens)
			.saturating_add(usage.cache_write_tokens)),
		"accuracy": accuracy,
		"detail": {},
	})
}

fn cost_json(cost: &v1::Cost) -> Value {
	json!({
		"nanos_usd": cost.nanos_usd,
		"estimated": cost.estimated,
		"input_nanos_usd": cost.input_nanos_usd,
		"output_nanos_usd": cost.output_nanos_usd,
	})
}

fn usage_bucket_json(bucket: &UsageBucket) -> Value {
	let key = bucket
		.key
		.iter()
		.map(|(dimension, value)| {
			let name = match dimension {
				UsageDimension::Project => "project",
				UsageDimension::Provider => "provider",
				UsageDimension::Model => "model",
				UsageDimension::SessionId => "session",
				UsageDimension::SessionKind => "kind",
			};
			(name.to_owned(), Value::String(value.to_string()))
		})
		.collect::<Map<_, _>>();
	json!({
		"key": key,
		"start_ms": bucket.start_ms,
		"usage": usage_json(&bucket.usage),
		"cost": cost_json(&bucket.cost),
		"requests": bucket.requests,
		"errors": bucket.errors,
		"duration": {"value": bucket.duration_ms, "unit": "ms"},
	})
}

fn empty_usage_bucket_json() -> Value {
	json!({
		"key": {},
		"start_ms": Value::Null,
		"usage": usage_json(&Default::default()),
		"cost": cost_json(&Default::default()),
		"requests": 0,
		"errors": 0,
		"duration": {"value": 0, "unit": "ms"},
	})
}

fn artifact_ref_json(record: &ArtifactRecord, metadata: &ArtifactMetadata) -> Value {
	json!({
		"id": record.ordinal.to_string(),
		"hash": record.reference.hash.to_hex().as_str(),
		"media_type": metadata.media_type,
		"byte_len": record.reference.size,
	})
}

fn artifact_stat_json(
	record: &ArtifactRecord,
	metadata: &ArtifactMetadata,
	session: &SessionId,
) -> Value {
	let lifetime: &'static str = record.lifetime.into();
	json!({
		"ref": artifact_ref_json(record, metadata),
		"url": record.url_for(session).map(|url| url.to_string()),
		"media_type": metadata.media_type,
		"byte_len": record.reference.size,
		"description": metadata.description,
		"lifetime": lifetime,
		"created_ms": metadata.created_ms,
		"source": metadata.source,
		"reachable_from": [],
		"lines": Value::Null,
	})
}
fn historical_journal_page(
	sessions_dir: &Path,
	session: &SessionId,
	arguments: &Map<String, Value>,
) -> Result<Value, ControlProtocolError> {
	let path = sessions_dir.join(format!("{}.jsonl", session.0));
	let reader = transcript::Reader::open(&path).map_err(protocol_error)?;
	let since = arguments.get("since").and_then(Value::as_u64);
	let cursor = arguments
		.get("cursor")
		.and_then(Value::as_str)
		.map(str::parse::<u64>)
		.transpose()
		.map_err(protocol_error)?;
	let after = cursor.or(since);
	let until = arguments.get("until").and_then(Value::as_u64);
	let live = arguments
		.get("live")
		.and_then(Value::as_bool)
		.unwrap_or(true);
	let kinds = arguments
		.get("kinds")
		.and_then(Value::as_array)
		.map(|values| {
			values
				.iter()
				.filter_map(Value::as_str)
				.collect::<BTreeSet<_>>()
		})
		.unwrap_or_default();
	let mut rows = Vec::with_capacity(201);
	for index in 0..u64::try_from(reader.log().len()).unwrap_or(u64::MAX) {
		if after.is_some_and(|after| index <= after)
			|| until.is_some_and(|until| index > until)
			|| live && !reader.live().contains(index)
		{
			continue;
		}
		let Some(transcript::Entry::Ok(event)) = reader.log().get(index) else {
			continue;
		};
		let transcript::Kind::Custom(custom) = &event.kind else {
			continue;
		};
		if custom.kind().starts_with("omp.state.session.")
			|| !kinds.is_empty() && !kinds.contains(custom.kind())
		{
			continue;
		}
		let row = JournalCustomEntry { index, ts: event.ts, entry: custom.clone() };
		let mut value = journal_entry_json(&session.0, &row)?;
		let object = value
			.as_object_mut()
			.expect("journal entry JSON is an object");
		let raw = object
			.get("raw")
			.and_then(Value::as_str)
			.unwrap_or("null")
			.as_bytes();
		object.insert("raw".to_owned(), Value::String(omp_core::base64::encode(raw).into_string()));
		rows.push(value);
		if rows.len() == 201 {
			break;
		}
	}
	let has_more = rows.len() > 200;
	if has_more {
		rows.pop();
	}
	let next = has_more.then(|| {
		rows
			.last()
			.and_then(|row| row.get("id"))
			.and_then(Value::as_object)
			.and_then(|id| id.get("index"))
			.and_then(Value::as_u64)
			.unwrap_or_default()
			.to_string()
	});
	Ok(json!({"entries": rows, "cursor": next, "done": !has_more}))
}

#[cfg(test)]
mod tests {
	use omp_agent::{Journal, JournalAuthor, JournalRequestStamp, control::ControlMailboxEvent};
	use omp_core::{ArtifactDigest, Principal, Provenance};
	use omp_proto::toolhost::v1::{AdoptArtifact, PinArtifact, QueryJournal, StatArtifact};
	use omp_storage::transcript::{Header, SessionId};

	use super::*;

	fn identity() -> JournalConnectionIdentity {
		JournalConnectionIdentity {
			principal:          Principal::new(sf!("os:test"), sf!("Test User")),
			provenance:         Provenance::new(
				sf!("publisher"),
				sf!("dev.example"),
				sf!("1.0.0"),
				ArtifactDigest::new([7; 32]),
				sf!("workspace"),
				sf!("trusted"),
				1,
			),
			host_generation:    1,
			session_generation: 1,
		}
	}

	fn query() -> ExternalJournalRequest {
		ExternalJournalRequest::Query {
			request_id: 1,
			extension:  sf!("dev.example"),
			query:      QueryJournal { session: "session".to_owned(), ..QueryJournal::default() },
		}
	}

	fn stamp(request_id: &str) -> JournalRequestStamp {
		JournalRequestStamp {
			request_id:         Str::from(request_id),
			idempotency_key:    Str::from(request_id),
			host_generation:    1,
			session_generation: 1,
		}
	}

	fn author() -> JournalAuthor {
		let identity = identity();
		JournalAuthor { principal: identity.principal, provenance: identity.provenance }
	}

	async fn call(
		actor: &ExternalJournalActor,
		request: ExternalJournalRequest,
	) -> Result<JournalHostEnvelope, Str> {
		let (reply, replies) = flume::unbounded();
		actor
			.sender()
			.send(ExternalJournalCall { request, identity: identity(), reply })
			.expect("send external request");
		replies.recv_async().await.expect("external response")
	}

	#[tokio::test]
	async fn current_session_query_fails_unbound_then_routes_to_agent_owner() {
		let state_dir = tempfile::tempdir().expect("state directory");
		let sessions = Arc::new(
			SessionIndex::open(state_dir.path().join("sessions.sqlite3")).expect("sessions index"),
		);
		let state = Arc::new(StateStore::open(state_dir.path().join("state")).expect("state store"));
		let blobs = BlobHost::open(state_dir.path().join("blobs")).expect("blob store");
		let actor = ExternalJournalActor::spawn(
			sessions,
			Some(state),
			blobs,
			sf!("session"),
			sf!("project"),
			sf!("/project"),
		)
		.expect("external actor");

		let (reply, replies) = flume::unbounded();
		actor
			.sender()
			.send(ExternalJournalCall { request: query(), identity: identity(), reply })
			.expect("send unbound query");
		assert!(
			replies
				.recv_async()
				.await
				.expect("unbound response")
				.expect_err("unbound query must fail")
				.contains("not bound")
		);

		let journal_path = state_dir.path().join("session.jsonl");
		let mut journal = Journal::create(&journal_path, &Header {
			v:       4,
			id:      SessionId(sf!("session")),
			created: 1,
			cwd:     state_dir.path().to_path_buf(),
		})
		.expect("journal");
		let (control, mailbox) = omp_agent::control::channel();
		actor.bind_agent(1, control).expect("bind agent owner");
		let owner = tokio::spawn(async move {
			loop {
				match mailbox.handle_next(&mut journal).await {
					ControlMailboxEvent::Closed => break,
					ControlMailboxEvent::JournalHandled | ControlMailboxEvent::Rewind(_) => {},
					ControlMailboxEvent::Regime(command) => {
						command.reject_unavailable();
					},
					ControlMailboxEvent::ProjectThread { .. } => {},
				}
			}
		});

		let (reply, replies) = flume::unbounded();
		actor
			.sender()
			.send(ExternalJournalCall { request: query(), identity: identity(), reply })
			.expect("send bound query");
		let response = replies
			.recv_async()
			.await
			.expect("bound response")
			.expect("bound query succeeds");
		assert!(matches!(
			response.body,
			Some(journal_host_envelope::Body::JournalRow(row)) if row.terminal
		));
		let (contender, _contender_mailbox) = omp_agent::control::channel();
		assert!(
			actor.bind_agent(2, contender).is_err(),
			"a concurrent owner must not replace the live binding"
		);
		owner.abort();
		actor.unbind_agent(1);

		let (successor, _successor_mailbox) = omp_agent::control::channel();
		actor
			.bind_agent(2, successor)
			.expect("bind successor after release");
		actor.unbind_agent(1);
		let (third, _third_mailbox) = omp_agent::control::channel();
		assert!(
			actor.bind_agent(3, third.clone()).is_err(),
			"a stale release must not clear the successor"
		);
		actor.unbind_agent(2);
		actor
			.bind_agent(3, third)
			.expect("bind after successor release");
	}

	#[tokio::test]
	async fn artifact_adoption_uses_authoritative_size_and_pin_is_durable() {
		let state_dir = tempfile::tempdir().expect("state directory");
		let sessions = Arc::new(
			SessionIndex::open(state_dir.path().join("sessions.sqlite3")).expect("sessions index"),
		);
		let state = Arc::new(StateStore::open(state_dir.path().join("state")).expect("state store"));
		let blobs = BlobHost::open(state_dir.path().join("blobs")).expect("blob store");
		let stored = blobs.put(b"authoritative bytes").expect("put blob");
		let digest = omp_core::hex::encode(&stored.hash).to_string();
		let actor = ExternalJournalActor::spawn(
			sessions,
			Some(state),
			blobs,
			sf!("session"),
			sf!("project"),
			sf!("/project"),
		)
		.expect("external actor");

		let wrong_size = call(&actor, ExternalJournalRequest::AdoptArtifact {
			request_id: 2,
			stamp:      stamp("wrong-size"),
			author:     author(),
			request:    AdoptArtifact {
				source_url: format!("blob://sha256/{digest}?size={}", stored.size + 1),
				lifetime: "session".to_owned(),
				..AdoptArtifact::default()
			},
		})
		.await
		.expect_err("peer size mismatch must fail");
		assert!(wrong_size.contains("length") || wrong_size.contains("size"));

		let missing = omp_core::hex::encode(&[9; 32]).to_string();
		let not_found = call(&actor, ExternalJournalRequest::AdoptArtifact {
			request_id: 3,
			stamp:      stamp("not-found"),
			author:     author(),
			request:    AdoptArtifact {
				source_url: format!("blob://sha256/{missing}"),
				lifetime: "session".to_owned(),
				..AdoptArtifact::default()
			},
		})
		.await
		.expect_err("missing blob must fail");
		assert!(not_found.contains("not found"));

		let adopted = call(&actor, ExternalJournalRequest::AdoptArtifact {
			request_id: 4,
			stamp:      stamp("adopt"),
			author:     author(),
			request:    AdoptArtifact {
				source_url: format!("blob://sha256/{digest}?size={}", stored.size),
				lifetime: "session".to_owned(),
				..AdoptArtifact::default()
			},
		})
		.await
		.expect("adopt blob");
		let Some(journal_host_envelope::Body::ArtifactRow(adopted)) = adopted.body else {
			panic!("adopt response must be an artifact row");
		};
		assert_eq!(adopted.size, stored.size);
		assert!(!adopted.pinned);

		let replayed = call(&actor, ExternalJournalRequest::AdoptArtifact {
			request_id: 41,
			stamp:      stamp("adopt"),
			author:     author(),
			request:    AdoptArtifact {
				source_url: format!("blob://sha256/{digest}?size={}", stored.size),
				lifetime: "session".to_owned(),
				..AdoptArtifact::default()
			},
		})
		.await
		.expect("exact artifact replay");
		assert!(matches!(
			replayed.body,
			Some(journal_host_envelope::Body::ArtifactRow(row)) if row.url == adopted.url
		));
		let conflict = call(&actor, ExternalJournalRequest::AdoptArtifact {
			request_id: 42,
			stamp:      stamp("adopt"),
			author:     author(),
			request:    AdoptArtifact {
				source_url: format!("blob://sha256/{digest}?size={}", stored.size),
				lifetime: "durable".to_owned(),
				..AdoptArtifact::default()
			},
		})
		.await
		.expect_err("changed artifact replay must conflict");
		assert!(conflict.starts_with("IdempotencyConflict:"));

		let pinned = call(&actor, ExternalJournalRequest::PinArtifact {
			request_id: 5,
			stamp:      stamp("pin"),
			author:     author(),
			request:    PinArtifact {
				url: adopted.url.clone(),
				lifetime: "durable".to_owned(),
				..PinArtifact::default()
			},
		})
		.await
		.expect("pin artifact");
		assert!(matches!(
			pinned.body,
			Some(journal_host_envelope::Body::ArtifactRow(row))
				if row.pinned && row.lifetime == "durable"
		));

		let durable = call(&actor, ExternalJournalRequest::StatArtifact {
			request_id: 6,
			request:    StatArtifact {
				url: format!("artifact://sha256/{digest}"),
				..StatArtifact::default()
			},
		})
		.await
		.expect("stat durable digest");
		assert!(matches!(
			durable.body,
			Some(journal_host_envelope::Body::ArtifactRow(row)) if row.pinned
		));
	}
}
