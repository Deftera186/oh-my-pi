//! Versioned read-only statistics API over the authoritative session index.

use std::{
	collections::BTreeMap,
	fs::{self, OpenOptions},
	path::PathBuf,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::Full;
use hyper::body::Incoming;
use omp_core::Str;
use omp_proto::omp::inference::v1;
use omp_storage::{
	index::{
		SessionFilter, SessionIndex, SessionStatus, UsageBucket, UsageBucketWidth, UsageDimension,
		UsageQuery,
	},
	transcript::SessionId,
};
use serde_json::{Value, json};
use smallvec::SmallVec;
/// Production historical telemetry backend over the append-only side file and
/// its SQLite index.
pub mod telemetry_backend {
	use std::{
		cmp,
		collections::{BTreeMap, BTreeSet},
		fmt::Display,
		fs::File,
		io::{Read as _, Seek as _, SeekFrom},
		sync::Arc,
		time::Instant,
	};

	use omp_core::Str;
	use omp_observability::authority::{
		DurableTelemetryQuery, DurableTelemetryRow, DurableTelemetryRows, TelemetryAuthorityError,
		TelemetryAuthorityIdentity,
	};
	use omp_storage::telemetry_index::{QueryGuard, TelemetryIndex};
	use serde_json::{Value, json};

	const QUERY_LIMIT_MAX: usize = 10_000;
	const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

	/// Real-index query adapter. The session inventory defines project scope;
	/// its first entry is the active session used by session scope.
	pub struct TelemetryIndexQuery {
		indexes: Arc<BTreeMap<Str, Arc<TelemetryIndex>>>,
		active:  Str,
	}

	impl TelemetryIndexQuery {
		#[must_use]
		/// Creates a query inventory containing one active session telemetry
		/// index.
		pub fn new(index: Arc<TelemetryIndex>, session: impl Into<Str>) -> Self {
			let active = session.into();
			Self { indexes: Arc::new(BTreeMap::from([(active.clone(), index)])), active }
		}

		/// Creates a project inventory and requires `active` to name one supplied
		/// session.
		pub fn with_sessions(
			active: impl Into<Str>,
			sessions: impl IntoIterator<Item = (Str, Arc<TelemetryIndex>)>,
		) -> Result<Self, TelemetryAuthorityError> {
			let active = active.into();
			let indexes: BTreeMap<_, _> = sessions.into_iter().collect();
			if !indexes.contains_key(active.as_str()) {
				return Err(invalid("telemetry query requires a session"));
			}
			Ok(Self { indexes: Arc::new(indexes), active })
		}

		fn scan(
			&self,
			identity: &TelemetryAuthorityIdentity,
			spec: &QuerySpec,
		) -> Result<DurableTelemetryRows, TelemetryAuthorityError> {
			let started = Instant::now();
			let sessions: Vec<_> = if spec.scope == "session" {
				self
					.indexes
					.get_key_value(self.active.as_str())
					.into_iter()
					.collect()
			} else {
				self.indexes.iter().collect()
			};
			let mut selected = Vec::new();
			let mut scanned_events = 0usize;
			let mut floored = false;
			let mut backfilled = false;
			for (session, index) in &sessions {
				if !spec.sessions.is_empty() && !spec.sessions.contains(session.as_str()) {
					continue;
				}
				let guard = QueryGuard::new();
				let result = index
					.query(session.as_str(), None, None, &guard)
					.map_err(owner)?;
				scanned_events = scanned_events.saturating_add(result.rows.len());
				for row in result.rows {
					if row.occurred_at_ms < identity.installed_at_ms {
						floored = true;
						continue;
					}
					if row.occurred_at_ms < spec.since
						|| spec.until.is_some_and(|until| row.occurred_at_ms > until)
						|| spec.cursor.is_some_and(|cursor| row.offset.0 <= cursor)
						|| !spec.kinds.is_empty() && !spec.kinds.contains(row.kind.as_str())
					{
						continue;
					}
					let mut file = File::open(index.side_path()).map_err(owner)?;
					let event = read_event(&mut file, row.offset.0)?;
					if !spec
						.predicates
						.iter()
						.all(|(path, expected)| value_at(&event, path).is_some_and(|v| v == expected))
					{
						continue;
					}
					let mut bindings = BTreeMap::new();
					if let Some(name) = &spec.binding {
						bindings.insert(name.clone(), event.clone());
					}
					let mut values = BTreeMap::from([
						(Str::new_static("offset"), Value::from(row.offset.0)),
						(Str::new_static("kind"), Value::String(row.kind.to_string())),
						(Str::new_static("occurred_at_ms"), Value::from(row.occurred_at_ms)),
					]);
					for path in &spec.select {
						if let Some(value) = value_at(&event, path) {
							values.insert(path.clone(), value.clone());
						}
					}
					backfilled |= row.backfilled;
					selected.push(DurableTelemetryRow {
						session: row.session_id,
						turn: event.get("turn").and_then(Value::as_u64).unwrap_or(0),
						offset: row.offset.0,
						kind: row.kind,
						occurred_at_ms: row.occurred_at_ms,
						backfilled: row.backfilled,
						events: vec![event],
						bindings,
						values,
					});
				}
			}
			selected.sort_by_key(|row| (row.occurred_at_ms, row.offset));
			let total = selected.len();
			let truncated = total > spec.limit;
			selected.truncate(spec.limit);
			let cursor = truncated
				.then(|| selected.last().map(|row| Str::new(row.offset.to_string())))
				.flatten();
			Ok(DurableTelemetryRows {
				rows: selected,
				total,
				cursor,
				truncated,
				scanned_sessions: sessions.len(),
				scanned_events,
				backfilled,
				floored,
				elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
			})
		}
	}

	impl DurableTelemetryQuery for TelemetryIndexQuery {
		fn query(
			&self,
			identity: &TelemetryAuthorityIdentity,
			query: &Value,
		) -> Result<Value, TelemetryAuthorityError> {
			self.scan(identity, &QuerySpec::parse(query)?)?.into_value()
		}

		fn rev_metrics(
			&self,
			identity: &TelemetryAuthorityIdentity,
			tool: &str,
			family: Option<&str>,
			since: Option<&Value>,
			scope: &str,
		) -> Result<Value, TelemetryAuthorityError> {
			if tool.is_empty() {
				return Err(invalid("tool must be non-empty"));
			}
			let spec = QuerySpec {
				scope:      parse_scope(scope)?,
				sessions:   BTreeSet::new(),
				kinds:      BTreeSet::new(),
				predicates: vec![(Str::new_static("tool"), Value::String(tool.to_owned()))],
				binding:    None,
				select:     Vec::new(),
				since:      since
					.map(parse_time)
					.transpose()?
					.unwrap_or(identity.installed_at_ms),
				until:      None,
				cursor:     None,
				limit:      usize::MAX,
			};
			let mut grouped = BTreeMap::<(Str, u64), (u64, u64, BTreeSet<Str>, u64, u64)>::new();
			for row in self.scan(identity, &spec)?.rows {
				let Some((rev_family, rev_n)) = revision(&row.events[0]) else {
					continue;
				};
				if family.is_some_and(|family| family != rev_family.as_str()) {
					continue;
				}
				let metric = grouped.entry((rev_family, rev_n)).or_insert((
					row.occurred_at_ms,
					row.occurred_at_ms,
					BTreeSet::new(),
					0,
					0,
				));
				metric.0 = metric.0.min(row.occurred_at_ms);
				metric.1 = metric.1.max(row.occurred_at_ms);
				metric.2.insert(row.session);
				metric.3 += 1;
				if row.events[0]
					.get("status")
					.and_then(Value::as_str)
					.unwrap_or("ok")
					== "ok"
				{
					metric.4 += 1;
				}
			}
			let mut rows: Vec<_> = grouped
				.into_iter()
				.map(|((family, n), (first, last, sessions, calls, ok))| {
					json!({
						"rev": { "family": family, "n": n },
						"first_seen_ms": first, "last_seen_ms": last,
						"sessions": sessions.len(), "calls": calls, "ok": ok,
						"faults": calls - ok, "blocked": 0, "timeouts": 0, "aborted": 0,
						"skipped": 0, "postcondition_rejected": 0, "abandoned": 0,
						"fault_codes": {}, "repaired_calls": 0, "repair_paths": {},
						"retry_rate": 0.0, "p50_latency_ms": 0.0, "p95_latency_ms": 0.0,
						"p99_latency_ms": 0.0, "p50_speculation_ms": 0.0,
						"p50_prompt_bytes": 0.0, "p95_prompt_bytes": 0.0,
						"spills": 0, "issues": 0
					})
				})
				.collect();
			rows.sort_by_key(|row| cmp::Reverse(row["rev"]["n"].as_u64().unwrap_or(0)));
			Ok(Value::Array(rows))
		}
	}

	struct QuerySpec {
		scope:      &'static str,
		sessions:   BTreeSet<Str>,
		kinds:      BTreeSet<Str>,
		predicates: Vec<(Str, Value)>,
		binding:    Option<Str>,
		select:     Vec<Str>,
		since:      u64,
		until:      Option<u64>,
		cursor:     Option<u64>,
		limit:      usize,
	}

	impl QuerySpec {
		fn parse(value: &Value) -> Result<Self, TelemetryAuthorityError> {
			let object = value
				.as_object()
				.ok_or_else(|| invalid("query must be an object"))?;
			let steps = object
				.get("match")
				.and_then(Value::as_array)
				.filter(|steps| steps.len() == 1)
				.ok_or_else(|| invalid("query requires exactly one match step"))?;
			let step = steps[0]
				.as_object()
				.ok_or_else(|| invalid("match step must be an object"))?;
			let mut predicates = Vec::new();
			for field in ["tool", "target", "rev"] {
				if let Some(value) = step.get(field).filter(|value| !value.is_null()) {
					predicates.push((Str::new_static(field), value.clone()));
				}
			}
			if let Some(where_) = step.get("where").and_then(Value::as_object) {
				for (path, predicate) in where_ {
					let value = predicate
						.as_object()
						.filter(|p| p.get("op").and_then(Value::as_str) == Some("eq"))
						.and_then(|p| p.get("value"))
						.ok_or_else(|| invalid("only exact telemetry predicates are supported"))?;
					predicates.push((Str::new(path), value.clone()));
				}
			}
			let limit_u64 = object.get("limit").and_then(Value::as_u64).unwrap_or(1_000);
			let limit = usize::try_from(limit_u64).map_err(|_| invalid("query limit is too large"))?;
			if !(1..=QUERY_LIMIT_MAX).contains(&limit) {
				return Err(invalid("query limit must be in 1..=10000"));
			}
			Ok(Self {
				scope: parse_scope(
					object
						.get("scope")
						.and_then(Value::as_str)
						.unwrap_or("project"),
				)?,
				sessions: strings(object.get("sessions"), "sessions")?,
				kinds: strings(step.get("kinds"), "kinds")?,
				predicates,
				binding: step.get("name").and_then(Value::as_str).map(Str::new),
				select: strings(object.get("select"), "select")?
					.into_iter()
					.collect(),
				since: object
					.get("since")
					.filter(|v| !v.is_null())
					.map(parse_time)
					.transpose()?
					.unwrap_or(0),
				until: object
					.get("until")
					.filter(|v| !v.is_null())
					.map(parse_time)
					.transpose()?,
				cursor: object
					.get("cursor")
					.and_then(Value::as_str)
					.map(|v| v.parse().map_err(|_| invalid("invalid cursor")))
					.transpose()?,
				limit,
			})
		}
	}

	fn strings(value: Option<&Value>, name: &str) -> Result<BTreeSet<Str>, TelemetryAuthorityError> {
		let Some(value) = value else {
			return Ok(BTreeSet::new());
		};
		value
			.as_array()
			.ok_or_else(|| invalid(format!("{name} must be an array")))?
			.iter()
			.map(|value| {
				value
					.as_str()
					.map(Str::new)
					.ok_or_else(|| invalid(format!("{name} entries must be strings")))
			})
			.collect()
	}

	fn read_event(file: &mut File, offset: u64) -> Result<Value, TelemetryAuthorityError> {
		file.seek(SeekFrom::Start(offset)).map_err(owner)?;
		let mut length = [0; 4];
		file.read_exact(&mut length).map_err(owner)?;
		let length = u32::from_le_bytes(length) as usize;
		if length > MAX_EVENT_BYTES {
			return Err(invalid("telemetry frame exceeds 16 MiB"));
		}
		let mut body = vec![0; length];
		file.read_exact(&mut body).map_err(owner)?;
		serde_json::from_slice(&body).map_err(owner)
	}

	fn value_at<'a>(event: &'a Value, path: &str) -> Option<&'a Value> {
		path
			.split('.')
			.try_fold(event, |value, part| value.get(part))
	}

	fn revision(event: &Value) -> Option<(Str, u64)> {
		let revision = event.get("rev")?;
		if let Some(value) = revision.as_str() {
			let (family, n) = value.rsplit_once('@')?;
			return Some((Str::new(family), n.parse().ok()?));
		}
		Some((Str::new(revision.get("family")?.as_str()?), revision.get("n")?.as_u64()?))
	}

	fn parse_time(value: &Value) -> Result<u64, TelemetryAuthorityError> {
		value
			.as_u64()
			.or_else(|| value.as_str()?.parse().ok())
			.ok_or_else(|| invalid("time must be Unix milliseconds"))
	}

	fn parse_scope(scope: &str) -> Result<&'static str, TelemetryAuthorityError> {
		match scope {
			"session" => Ok("session"),
			"self" | "tree" => Ok("session"),
			"project" => Ok("project"),
			_ => Err(invalid("scope must be session or project")),
		}
	}

	fn invalid(message: impl Into<Str>) -> TelemetryAuthorityError {
		TelemetryAuthorityError::Invalid(message.into())
	}

	fn owner(error: impl Display) -> TelemetryAuthorityError {
		TelemetryAuthorityError::Owner(Str::new(error.to_string()))
	}
}

/// Host-owned prompt projection and detached-job authority.
pub mod job_authority {
	use std::{
		collections::BTreeSet,
		sync::Arc,
		time::{Duration, Instant, SystemTime, UNIX_EPOCH},
	};

	use async_trait::async_trait;
	use omp_agent::JobBoard;
	use omp_core::{InvocationPhase, Str};
	use omp_envd::exthost::{
		CallbackConcurrency,
		control::{
			self, ControlConnectionIdentity, ControlDispatch, ControlInvocationAuthority,
			ControlProtocolError,
		},
		dispatch::CallbackDispatcher,
	};
	use omp_tool::{
		ArtifactLifetime, ExpectedArtifact, JobKind, JobMetadata, JobOwner, JobRef, JobStatus,
	};
	use serde::{Deserialize, Serialize};
	use serde_json::Value;
	use thiserror::Error;
	use tokio::time;

	/// Maximum wall time allowed for a pure prompt projection callback.
	pub const PROMPT_PROJECTION_DEADLINE: Duration = Duration::from_millis(50);

	/// Authenticated extension incarnation allowed to use one job owner.
	#[derive(Clone, Debug, Eq, PartialEq)]
	pub struct JobAuthorityIdentity {
		/// Stable authenticated principal spelling.
		pub principal:          Str,
		/// Declaring extension identifier.
		pub extension:          Str,
		/// Verified extension artifact digest.
		pub artifact_digest:    Str,
		/// Active child incarnation.
		pub host_generation:    u64,
		/// Active session incarnation.
		pub session_generation: u64,
		/// Durable session receiving job settlement.
		pub session:            Str,
		/// Exact durable capability grants.
		pub capabilities:       Arc<BTreeSet<Str>>,
	}

	/// Core-authored authority for one job operation.
	#[derive(Clone, Copy, Debug)]
	pub struct JobCallContext<'a> {
		/// Authenticated connection identity.
		pub identity:   &'a JobAuthorityIdentity,
		/// Current invocation phase.
		pub phase:      InvocationPhase,
		/// Whether cancellation has already won.
		pub cancelled:  bool,
		/// Exact host-issued callback authority, when projection calls back into
		/// Python.
		pub invocation: Option<&'a ControlInvocationAuthority>,
	}

	/// Structured job-owner failure.
	#[derive(Clone, Debug, Error, Eq, PartialEq)]
	pub enum JobAuthorityError {
		/// The request belongs to a stale or foreign connection.
		#[error("job request belongs to a stale or foreign connection")]
		Identity,
		/// The owning invocation was cancelled or settled.
		#[error("job request was cancelled")]
		Cancelled,
		/// The operation is illegal in the current invocation phase.
		#[error("job request is not legal in the current invocation phase")]
		Phase,
		/// The descriptor is not backed by a scoped Environment resource.
		#[error("invalid detached job: {0}")]
		InvalidJob(Str),
		/// The stable job id names a different durable descriptor.
		#[error("detached job id `{0}` is already bound to another descriptor")]
		JobConflict(Str),
		/// The authoritative board rejected admission.
		#[error("detached job admission failed: {0}")]
		JobAdmission(Str),
		/// The callback exceeded its host-owned deadline.
		#[error("verdict projection timed out")]
		ProjectionTimeout,
		/// The callback host rejected or lost the projection.
		#[error("verdict projection failed: {0}")]
		Projection(Str),
	}

	/// CONTROL-safe detached-job descriptor.
	///
	/// Only named Environment process generations are accepted. Extensions
	/// never receive a process handle or ambient process-listing authority.
	#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
	pub struct JobRegistration {
		/// Stable durable job identity.
		pub id:               Str,
		/// Environment process name.
		pub owner_name:       Str,
		/// Exact Environment process generation.
		pub owner_generation: u64,
		/// Human-readable expected artifact role.
		pub description:      Str,
		/// Expected artifact media type.
		pub media_type:       Option<Str>,
		/// Minimum artifact lifetime.
		pub lifetime:         ArtifactLifetime,
	}

	impl JobRegistration {
		fn into_job(self, session: Str) -> Result<JobRef, JobAuthorityError> {
			if self.id.is_empty() || self.owner_name.is_empty() || self.owner_generation == 0 {
				return Err(JobAuthorityError::InvalidJob(Str::new_static(
					"id, owner name, and owner generation must be present",
				)));
			}
			let now = now_ms();
			let mut metadata = JobMetadata::running(JobKind::Eval, self.description.clone(), now);
			metadata.status = JobStatus::Running;
			metadata.owner_session = Some(session);
			Ok(JobRef {
				id:       self.id,
				owner:    JobOwner::NamedProcess {
					name:       self.owner_name,
					generation: self.owner_generation,
				},
				metadata: Arc::new(metadata),
				artifact: ExpectedArtifact {
					description: self.description,
					media_type:  self.media_type,
					lifetime:    self.lifetime,
				},
			})
		}
	}

	/// Agent-owned durable registration boundary. Implementations must journal
	/// the descriptor before attaching it to the live board.
	#[async_trait]
	pub trait DurableJobRegistrar: Send + Sync + 'static {
		/// Persists and installs one exact descriptor idempotently.
		async fn register(&self, job: JobRef) -> Result<JobRef, JobAuthorityError>;
	}

	/// Production registrar routed to the sole mutable Agent/journal owner.
	pub struct AgentDurableJobRegistrar {
		control: omp_agent::AgentHostControl,
	}

	impl AgentDurableJobRegistrar {
		/// Binds the cloneable Agent actor control handle.
		#[must_use]
		pub fn new(control: omp_agent::AgentHostControl) -> Self {
			Self { control }
		}
	}

	#[async_trait]
	impl DurableJobRegistrar for AgentDurableJobRegistrar {
		async fn register(&self, job: JobRef) -> Result<JobRef, JobAuthorityError> {
			let value = serde_json::to_value(&job)
				.map_err(|error| JobAuthorityError::JobAdmission(Str::new(error.to_string())))?;
			let result = self
				.control
				.request("omp.jobs.register", serde_json::Map::from_iter([("job".to_owned(), value)]))
				.await
				.map_err(JobAuthorityError::JobAdmission)?;
			serde_json::from_value(result)
				.map_err(|error| JobAuthorityError::JobAdmission(Str::new(error.to_string())))
		}
	}

	/// Pure prompt projection request sent back to the declaring extension.
	#[derive(Clone, Debug, Serialize)]
	pub struct PromptProjectionRequest {
		/// Exact registered wire name.
		pub name:        Str,
		/// Exact revision family.
		pub family:      Str,
		/// Exact monotonic revision.
		pub revision:    u16,
		/// Canonical durable verdict body.
		pub verdict:     Value,
		/// Host-sealed prompt projection budget and dialect.
		pub prompt_caps: Value,
	}

	/// Callback transport used by Core to invoke the extension projector.
	#[async_trait]
	pub trait PromptProjectionDispatcher: Send + Sync + 'static {
		/// Dispatches to the exact authenticated worker generation.
		async fn project(
			&self,
			identity: Arc<JobAuthorityIdentity>,
			invocation: ControlInvocationAuthority,
			request: PromptProjectionRequest,
		) -> Result<Value, JobAuthorityError>;
	}

	/// CONTROL-backed projection dispatcher for the exact authenticated worker.
	pub struct ControlPromptProjectionDispatcher {
		target:     Arc<ControlConnectionIdentity>,
		dispatcher: Arc<dyn CallbackDispatcher>,
	}

	impl ControlPromptProjectionDispatcher {
		/// Binds a live supervisor callback dispatcher to one worker generation.
		#[must_use]
		pub fn new(
			target: Arc<ControlConnectionIdentity>,
			dispatcher: Arc<dyn CallbackDispatcher>,
		) -> Self {
			Self { target, dispatcher }
		}
	}

	#[async_trait]
	impl PromptProjectionDispatcher for ControlPromptProjectionDispatcher {
		async fn project(
			&self,
			identity: Arc<JobAuthorityIdentity>,
			invocation: ControlInvocationAuthority,
			request: PromptProjectionRequest,
		) -> Result<Value, JobAuthorityError> {
			if self.target.principal.id() != identity.principal.as_str()
				|| self.target.extension != identity.extension
				|| self.target.artifact_digest != identity.artifact_digest
				|| self.target.host_generation != identity.host_generation
				|| self.target.session_generation != identity.session_generation
			{
				return Err(JobAuthorityError::Identity);
			}
			let Value::Object(arguments) = serde_json::to_value(request)
				.map_err(|error| JobAuthorityError::Projection(Str::new(error.to_string())))?
			else {
				return Err(JobAuthorityError::Projection(Str::new_static(
					"projection request did not serialize as an object",
				)));
			};
			self
				.dispatcher
				.dispatch(self.target.clone(), ControlDispatch {
					operation: Str::new_static("omp.jobs.project"),
					arguments,
					authority: invocation,
					policy: CallbackConcurrency::Serialized,
					deadline: omp_envd::exthost::EventDeadline {
						at: Instant::now() + PROMPT_PROJECTION_DEADLINE,
					},
				})
				.await
				.map_err(|error| JobAuthorityError::Projection(Str::new(error.to_string())))
		}
	}

	/// Identity-fenced owner for durable detached jobs and prompt projections.
	pub struct JobAuthority {
		identity:   Arc<JobAuthorityIdentity>,
		jobs:       JobBoard,
		registrar:  Arc<dyn DurableJobRegistrar>,
		dispatcher: Arc<dyn PromptProjectionDispatcher>,
	}

	impl JobAuthority {
		/// Binds one authority to an authenticated connection and session board.
		pub fn new(
			identity: Arc<JobAuthorityIdentity>,
			jobs: JobBoard,
			registrar: Arc<dyn DurableJobRegistrar>,
			dispatcher: Arc<dyn PromptProjectionDispatcher>,
		) -> Self {
			Self { identity, jobs, registrar, dispatcher }
		}

		fn authorize(&self, context: JobCallContext<'_>) -> Result<(), JobAuthorityError> {
			if context.identity != self.identity.as_ref() {
				return Err(JobAuthorityError::Identity);
			}
			if context.cancelled || context.phase.is_terminal() {
				return Err(JobAuthorityError::Cancelled);
			}
			if !context
				.phase
				.allows_operation(InvocationPhase::EffectsAuthorized)
			{
				return Err(JobAuthorityError::Phase);
			}
			Ok(())
		}

		/// Idempotently installs one Environment-owned descriptor on the
		/// authoritative session job board.
		pub async fn register_job(
			&self,
			context: JobCallContext<'_>,
			registration: JobRegistration,
		) -> Result<JobRef, JobAuthorityError> {
			self.authorize(context)?;
			let job = registration.into_job(self.identity.session.clone())?;
			if let Some(existing) = self
				.jobs
				.snapshot()
				.into_iter()
				.find(|existing| existing.id == job.id)
			{
				return if same_registration(&existing, &job) {
					Ok(existing)
				} else {
					Err(JobAuthorityError::JobConflict(job.id))
				};
			}
			self.registrar.register(job).await
		}

		/// Dispatches one exact-revision prompt projection under the host
		/// deadline.
		pub async fn project_prompt(
			&self,
			context: JobCallContext<'_>,
			request: PromptProjectionRequest,
		) -> Result<Value, JobAuthorityError> {
			if context.identity != self.identity.as_ref() {
				return Err(JobAuthorityError::Identity);
			}
			if context.cancelled {
				return Err(JobAuthorityError::Cancelled);
			}
			if context.phase != InvocationPhase::Settled {
				return Err(JobAuthorityError::Phase);
			}
			if request.name.is_empty() {
				return Err(JobAuthorityError::Projection(Str::new_static(
					"projection requires an exact device wire name",
				)));
			}
			let invocation = context
				.invocation
				.cloned()
				.ok_or(JobAuthorityError::Phase)?;
			time::timeout(
				PROMPT_PROJECTION_DEADLINE,
				self
					.dispatcher
					.project(self.identity.clone(), invocation, request),
			)
			.await
			.map_err(|_| JobAuthorityError::ProjectionTimeout)?
		}
	}

	#[async_trait]
	impl omp_envd::exthost::JobsControlOwner for JobAuthority {
		async fn register_job(
			&self,
			context: control::ControlRequestContext,
			mut arguments: serde_json::Map<String, Value>,
		) -> Result<Value, ControlProtocolError> {
			let descriptor = arguments
				.remove("job")
				.unwrap_or_else(|| Value::Object(arguments));
			let descriptor = descriptor.as_object().ok_or_else(|| {
				ControlProtocolError::new("InvalidJob", "job descriptor must be an object")
			})?;
			let owner_kind = descriptor
				.get("owner_kind")
				.and_then(Value::as_str)
				.unwrap_or_default();
			if owner_kind != "named_process" {
				return Err(ControlProtocolError::new(
					"JobOwnerDenied",
					"extensions may register only Environment-owned named process generations",
				));
			}
			let string = |name: &'static str| {
				descriptor
					.get(name)
					.and_then(Value::as_str)
					.filter(|value| !value.is_empty())
					.map(Str::new)
					.ok_or_else(|| {
						ControlProtocolError::new(
							"InvalidJob",
							format!("{name} must be a non-empty string"),
						)
					})
			};
			let lifetime = descriptor
				.get("lifetime")
				.and_then(Value::as_str)
				.unwrap_or("session")
				.parse::<ArtifactLifetime>()
				.map_err(|_| {
					ControlProtocolError::new(
						"InvalidJob",
						"lifetime must be ephemeral, session, or durable",
					)
				})?;
			let registration = JobRegistration {
				id: string("id")?,
				owner_name: string("owner_name")?,
				owner_generation: descriptor
					.get("owner_generation")
					.and_then(Value::as_u64)
					.filter(|generation| *generation != 0)
					.ok_or_else(|| {
						ControlProtocolError::new(
							"InvalidJob",
							"owner_generation must be a positive integer",
						)
					})?,
				description: string("description")?,
				media_type: descriptor
					.get("media_type")
					.and_then(Value::as_str)
					.map(Str::new),
				lifetime,
			};
			let phase = context
				.invocation
				.as_ref()
				.map(|invocation| invocation.phase)
				.ok_or_else(|| {
					ControlProtocolError::new(
						"InvalidPhase",
						"job registration requires a live invocation",
					)
				})?;
			let call = JobCallContext {
				identity: self.identity.as_ref(),
				phase,
				cancelled: phase.is_terminal(),
				invocation: context.invocation.as_ref(),
			};
			let job = JobAuthority::register_job(self, call, registration)
				.await
				.map_err(control_error)?;
			let (owner_name, owner_generation) = match &job.owner {
				JobOwner::NamedProcess { name, generation } => (name.as_str(), *generation),
				JobOwner::AgentLoop { .. } => {
					return Err(ControlProtocolError::new(
						"JobOwnerDenied",
						"job owner escaped the named-process authority",
					));
				},
			};
			Ok(serde_json::json!({
				"id": job.id.as_str(),
				"owner_kind": "named_process",
				"owner_name": owner_name,
				"owner_generation": owner_generation,
				"description": job.artifact.description.as_str(),
				"media_type": job.artifact.media_type.as_deref(),
				"lifetime": job.artifact.lifetime.to_string(),
			}))
		}
	}

	fn control_error(error: JobAuthorityError) -> ControlProtocolError {
		let code = match &error {
			JobAuthorityError::Identity => "StaleGeneration",
			JobAuthorityError::Cancelled => "Cancelled",
			JobAuthorityError::Phase => "InvalidPhase",
			JobAuthorityError::InvalidJob(_) => "InvalidJob",
			JobAuthorityError::JobConflict(_) => "JobConflict",
			JobAuthorityError::JobAdmission(_) => "JobAdmissionDenied",
			JobAuthorityError::ProjectionTimeout => "ProjectionTimeout",
			JobAuthorityError::Projection(_) => "ProjectionFailed",
		};
		ControlProtocolError::new(code, error.to_string())
	}

	fn same_registration(left: &JobRef, right: &JobRef) -> bool {
		left.id == right.id
			&& left.owner == right.owner
			&& left.artifact == right.artifact
			&& left.metadata.kind == right.metadata.kind
			&& left.metadata.label == right.metadata.label
			&& left.metadata.owner_session == right.metadata.owner_session
	}

	fn now_ms() -> u64 {
		SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis() as u64
	}
}

/// Stable media type and schema revision returned by every JSON route.
pub const API_VERSION: &str = "omp.stats.v1";
/// Concrete response body used by the stats HTTP service.
pub type Body = Full<Bytes>;

/// Shared API dependencies.
pub struct StatsApi {
	index:     Arc<SessionIndex>,
	sync_lock: PathBuf,
}

impl StatsApi {
	/// Creates an API backed by the authoritative receipt index.
	pub fn new(index: Arc<SessionIndex>, sync_lock: PathBuf) -> Self {
		Self { index, sync_lock }
	}

	/// Produces the same overview envelope used by the HTTP route.
	pub fn overview_document(&self, range: &str) -> Result<Value, String> {
		let range = Range::parse(Some(&format!("range={range}"))).map_err(str::to_owned)?;
		let data = self.overview(range)?;
		Ok(json!({"version": API_VERSION, "data": data, "meta": {"range": range.label}}))
	}

	/// Routes one versioned API request.
	pub fn handle(&self, request: &Request<Incoming>) -> Response<Body> {
		let path = request.uri().path();
		if path == "/api/version" && request.method() == Method::GET {
			return json_response(StatusCode::OK, json!({"version": API_VERSION}));
		}
		if path == "/api/v1/stats/sync" && request.method() == Method::POST {
			return self.sync();
		}
		if request.method() != Method::GET || !path.starts_with("/api/v1/stats/") {
			return error_response(StatusCode::NOT_FOUND, "route_not_found", "unknown stats route");
		}
		let range = match Range::parse(request.uri().query()) {
			Ok(range) => range,
			Err(message) => return error_response(StatusCode::BAD_REQUEST, "invalid_range", message),
		};
		let route = &path[14..];
		let result = match route {
			"overview" => self.overview(range),
			"models" => self.grouped(range, UsageDimension::Model),
			"providers" => self.grouped(range, UsageDimension::Provider),
			"folders" => self.grouped(range, UsageDimension::Project),
			"costs" => self.costs(range),
			"timeseries" => self.timeseries(range),
			"recent" => self.recent(range, false),
			"errors" => self.recent(range, true),
			"tools" => self.tools(range),
			"behavior" | "gain" => {
				return error_response(
					StatusCode::NOT_IMPLEMENTED,
					"query_unavailable",
					"this statistics projection has no authoritative index",
				);
			},
			_ if route.starts_with("request/") => self.request(&route[8..]),
			_ => {
				return error_response(StatusCode::NOT_FOUND, "route_not_found", "unknown stats route");
			},
		};
		match result {
			Ok(data) => envelope_response(data, range),
			Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "query_failed", &error),
		}
	}

	fn overview(&self, range: Range) -> Result<Value, String> {
		let rows = self.query(range, SmallVec::new(), UsageBucketWidth::None)?;
		let total = rows.iter().fold(Totals::default(), |mut total, row| {
			total.add(row);
			total
		});
		Ok(json!({"overall": total.value()}))
	}

	fn grouped(&self, range: Range, dimension: UsageDimension) -> Result<Value, String> {
		let rows = self.query(range, SmallVec::from_buf([dimension]), UsageBucketWidth::None)?;
		Ok(json!({"rows": rows.iter().map(bucket_value).collect::<Vec<_>>() }))
	}

	fn costs(&self, range: Range) -> Result<Value, String> {
		let rows =
			self.query(range, SmallVec::from_buf([UsageDimension::Model]), UsageBucketWidth::None)?;
		Ok(json!({"rows": rows.iter().map(|row| json!({
			"model": key(row, UsageDimension::Model),
			"cost_nanos_usd": row.cost.nanos_usd,
			"cost_usd": row.cost.nanos_usd as f64 / 1_000_000_000.0,
			"estimated": row.cost.estimated,
		})).collect::<Vec<_>>() }))
	}

	fn timeseries(&self, range: Range) -> Result<Value, String> {
		let rows = self.query(range, SmallVec::new(), range.bucket)?;
		Ok(json!({"rows": rows.iter().map(bucket_value).collect::<Vec<_>>() }))
	}

	fn recent(&self, range: Range, errors_only: bool) -> Result<Value, String> {
		let page = self
			.index
			.list(&SessionFilter {
				since_ms: range.since_ms,
				until_ms: range.until_ms,
				limit: 100,
				..SessionFilter::default()
			})
			.map_err(|error| error.to_string())?;
		let rows = page
			.sessions
			.into_iter()
			.filter_map(|session| {
				if errors_only && session.status != SessionStatus::Error {
					return None;
				}
				Some(json!({
					"session_id": session.id.0.as_str(), "title": session.title.as_deref(),
					"project": session.project.as_str(), "kind": session.kind.to_string(),
					"status": session.status.to_string(), "updated_ms": session.updated_ms,
					"turns": session.turns, "entries": session.entries,
				}))
			})
			.collect::<Vec<_>>();
		Ok(json!({"rows": rows}))
	}

	fn tools(&self, range: Range) -> Result<Value, String> {
		let page = self
			.index
			.list(&SessionFilter {
				since_ms: range.since_ms,
				until_ms: range.until_ms,
				limit: 200,
				..SessionFilter::default()
			})
			.map_err(|error| error.to_string())?;
		let mut calls = 0_u64;
		let mut results = 0_u64;
		let mut errors = 0_u64;
		for session in page.sessions {
			let stats = self
				.index
				.session_statistics(&session.id, false)
				.map_err(|error| error.to_string())?;
			calls = calls.saturating_add(stats.tool_calls);
			results = results.saturating_add(stats.tool_results);
			errors = errors.saturating_add(stats.tool_errors);
		}
		Ok(json!({"rows": [{
			"tool": "all", "calls": calls, "results": results, "errors": errors,
		}]}))
	}

	fn request(&self, id: &str) -> Result<Value, String> {
		let Some((session, event)) = id.rsplit_once(':') else {
			return Err("request id must be SESSION:EVENT".to_owned());
		};
		let event_index = event
			.parse::<u64>()
			.map_err(|_| "request event is not an integer".to_owned())?;
		let receipt = self
			.index
			.receipt(&SessionId(Str::new(session)), event_index)
			.map_err(|error| error.to_string())?;
		Ok(receipt.map_or(Value::Null, |receipt| {
			json!({
				"id": id, "session_id": session, "event_index": event_index,
				"usage": usage_value(&receipt.usage), "cost_nanos_usd": receipt.cost.nanos_usd,
				"redacted": true,
			})
		}))
	}

	fn query(
		&self,
		range: Range,
		group_by: SmallVec<UsageDimension, 3>,
		bucket: UsageBucketWidth,
	) -> Result<Vec<UsageBucket>, String> {
		self
			.index
			.usage(&UsageQuery {
				since_ms: range.since_ms,
				until_ms: range.until_ms,
				group_by,
				bucket,
				include_subagents: true,
				..UsageQuery::default()
			})
			.map_err(|error| error.to_string())
	}

	/// Serializes manual synchronization through the same cross-process lock.
	pub fn sync_document(&self) -> Result<Value, &'static str> {
		let file = OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&self.sync_lock)
			.map_err(|_| "another process is synchronizing statistics")?;
		drop(file);
		let _ = fs::remove_file(&self.sync_lock);
		Ok(json!({"version": API_VERSION, "data": {"processed": 0, "source": "write_time_index"}}))
	}

	fn sync(&self) -> Response<Body> {
		match self.sync_document() {
			Ok(document) => json_response(StatusCode::OK, document),
			Err(message) => error_response(StatusCode::CONFLICT, "sync_busy", message),
		}
	}
}

#[derive(Clone, Copy)]
struct Range {
	since_ms: Option<u64>,
	until_ms: Option<u64>,
	bucket:   UsageBucketWidth,
	label:    &'static str,
}

impl Range {
	fn parse(query: Option<&str>) -> Result<Self, &'static str> {
		let mut params = BTreeMap::new();
		for pair in query
			.unwrap_or_default()
			.split('&')
			.filter(|pair| !pair.is_empty())
		{
			let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
			params.insert(key, value);
		}
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis() as u64;
		let named = params.get("range").copied().unwrap_or("30d");
		let (mut since_ms, mut bucket, label) = match named {
			"24h" => (Some(now.saturating_sub(86_400_000)), UsageBucketWidth::Hour, "24h"),
			"7d" => (Some(now.saturating_sub(604_800_000)), UsageBucketWidth::Day, "7d"),
			"30d" => (Some(now.saturating_sub(2_592_000_000)), UsageBucketWidth::Day, "30d"),
			"90d" => (Some(now.saturating_sub(7_776_000_000)), UsageBucketWidth::Week, "90d"),
			"all" => (None, UsageBucketWidth::Month, "all"),
			_ => return Err("range must be 24h, 7d, 30d, 90d, or all"),
		};
		let mut until_ms = Some(now);
		if let Some(value) = params.get("since") {
			since_ms = Some(
				value
					.parse()
					.map_err(|_| "since must be epoch milliseconds")?,
			);
		}
		if let Some(value) = params.get("until") {
			until_ms = Some(
				value
					.parse()
					.map_err(|_| "until must be epoch milliseconds")?,
			);
		}
		if since_ms
			.zip(until_ms)
			.is_some_and(|(since, until)| since > until)
		{
			return Err("since must not be later than until");
		}
		if let Some(value) = params.get("bucket") {
			bucket = match *value {
				"none" => UsageBucketWidth::None,
				"hour" => UsageBucketWidth::Hour,
				"day" => UsageBucketWidth::Day,
				"week" => UsageBucketWidth::Week,
				"month" => UsageBucketWidth::Month,
				_ => return Err("bucket must be none, hour, day, week, or month"),
			};
		}
		Ok(Self { since_ms, until_ms, bucket, label })
	}
}

#[derive(Default)]
struct Totals {
	requests:    u64,
	errors:      u64,
	input:       u64,
	output:      u64,
	cache_read:  u64,
	cache_write: u64,
	premium:     u64,
	cost:        u64,
	duration:    u64,
	sessions:    u64,
}

impl Totals {
	fn add(&mut self, row: &UsageBucket) {
		self.requests = self.requests.saturating_add(row.requests);
		self.errors = self.errors.saturating_add(row.errors);
		self.input = self.input.saturating_add(row.usage.input_tokens);
		self.output = self.output.saturating_add(row.usage.output_tokens);
		self.cache_read = self.cache_read.saturating_add(row.usage.cache_read_tokens);
		self.cache_write = self
			.cache_write
			.saturating_add(row.usage.cache_write_tokens);
		self.premium = self
			.premium
			.saturating_add(row.usage.premium_requests.unwrap_or_default());
		self.cost = self.cost.saturating_add(row.cost.nanos_usd);
		self.duration = self.duration.saturating_add(row.duration_ms);
		self.sessions = self.sessions.saturating_add(row.sessions);
	}

	fn value(&self) -> Value {
		json!({
			"requests": self.requests, "errors": self.errors,
			"input_tokens": self.input, "output_tokens": self.output,
			"cache_read_tokens": self.cache_read, "cache_write_tokens": self.cache_write,
			"premium_requests": self.premium, "cost_nanos_usd": self.cost,
			"cost_usd": self.cost as f64 / 1_000_000_000.0,
			"duration_ms": self.duration, "sessions": self.sessions,
		})
	}
}

fn key(row: &UsageBucket, dimension: UsageDimension) -> Option<&str> {
	row.key
		.iter()
		.find_map(|(candidate, value)| (*candidate == dimension).then_some(value.as_str()))
}
fn usage_value(usage: &v1::Usage) -> Value {
	json!({
		"input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens,
		"cache_read_tokens": usage.cache_read_tokens, "cache_write_tokens": usage.cache_write_tokens,
		"total_tokens": usage.total_tokens, "context_tokens": usage.context_tokens,
		"premium_requests": usage.premium_requests, "reasoning_tokens": usage.reasoning_tokens,
	})
}
fn bucket_value(row: &UsageBucket) -> Value {
	json!({
		"key": row.key.iter().map(|(dimension, value)| (dimension.to_string(), value.as_str())).collect::<BTreeMap<_, _>>(),
		"start_ms": row.start_ms, "requests": row.requests, "errors": row.errors,
		"duration_ms": row.duration_ms, "sessions": row.sessions,
		"usage": usage_value(&row.usage), "cost_nanos_usd": row.cost.nanos_usd,
	})
}
fn envelope_response(data: Value, range: Range) -> Response<Body> {
	json_response(
		StatusCode::OK,
		json!({"version": API_VERSION, "data": data, "meta": {"range": range.label}}),
	)
}
fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
	json_response(
		status,
		json!({"version": API_VERSION, "error": {"code": code, "message": message}}),
	)
}
fn json_response(status: StatusCode, value: Value) -> Response<Body> {
	let bytes = serde_json::to_vec(&value)
		.unwrap_or_else(|_| b"{\"error\":{\"code\":\"serialization_failed\"}}".to_vec());
	Response::builder()
		.status(status)
		.header(header::CONTENT_TYPE, "application/json; charset=utf-8")
		.body(Full::new(Bytes::from(bytes)))
		.unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}
