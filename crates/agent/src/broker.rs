//! Process-global agent registry and project-scoped IRC routing.

use std::{
	collections::{HashMap, VecDeque},
	ffi, fs, io,
	io::{BufRead as _, Read as _},
	path::{Path, PathBuf},
	sync::{
		Arc, LazyLock, Weak,
		atomic::{AtomicU64, Ordering},
	},
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use omp_core::Str;
use omp_proto::thread::v1::{Item, Message as ThreadMessage, Part, Role, item, part};
use parking_lot::Mutex;
use smallvec::SmallVec;
use thiserror::Error;
use tokio::sync::{Notify, broadcast, watch, watch::Receiver};

use crate::{
	AgentKind, AgentNode, Interrupt, InterruptClass, InterruptSource, MailboxSender,
	SubagentDisposition, SubagentTerminalStatus, prompt_assets::render_parent_irc,
};

const MAILBOX_CAPACITY: usize = 100;
const ACTIVITY_MAX_CHARS: usize = 80;
const DISCOVERY_DIAGNOSTIC_CAPACITY: usize = 128;
const DELIVERY_DEDUP_CAPACITY: usize = 1_024;
const PREFIX_MAX_LINES: usize = 64;
const PREFIX_MAX_BYTES: usize = 256 * 1_024;
const TASK_SUMMARY_MAX_CHARS: usize = 160;
const QUERY_MAX_BYTES: usize = 4 * 1_024 * 1_024;
const QUERY_MAX_CHARS: usize = 4_096;
const QUERY_MAX_DEPTH: usize = 64;
const QUERY_MAX_DURATION: Duration = Duration::from_millis(100);
const MAX_PERSISTED_ROSTER_LATCHES: usize = 32;

/// Delivery boundary requested by a peer message.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum DeliveryMode {
	/// Deliver at a tool-completion boundary without cancelling work.
	Aside,
	/// Deliver as an immediate steer interrupt.
	Steer,
	/// Deliver only before the next turn.
	NextTurn,
}

/// Fire-and-forget delivery outcome for one recipient.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum Receipt {
	/// Injected into a running recipient at its requested boundary.
	Injected,
	/// Injected into an idle live recipient, which may begin a turn.
	Woken,
	/// Accepted by the recipient's cold-revival transport.
	Revived,
	/// No live or revivable target accepted the message.
	Failed,
}

/// Result of atomically settling one recipient turn against newly queued IRC
/// work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnEndDisposition {
	/// No IRC work was queued at settlement; the turn is terminal.
	Terminal,
	/// IRC arrived after the loop's final drain and requires a continuation.
	ContinuationPending,
}

/// Process-global lifecycle state retained after a live loop is detached.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum RegistryStatus {
	/// A turn is active.
	Running = 0,
	/// A live in-memory session is waiting for work.
	Idle    = 1,
	/// Live resources were evicted; the durable journal can restart the loop.
	Parked  = 2,
}

/// Classification for a bounded transcript-prefix discovery diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum DiscoveryDiagnosticKind {
	/// The v4 header or a prefix event was malformed.
	Corrupt,
	/// The bounded prefix ended before durable child initialization appeared.
	Incomplete,
}

/// A retained diagnostic for an on-disk journal that could not be registered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryDiagnostic {
	/// Journal path that was inspected.
	pub path: PathBuf,
	/// Stable machine-readable classification.
	pub kind: DiscoveryDiagnosticKind,
}

/// Generation-fenced request to detach one idle live runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkLease {
	/// Parked durable registry projection.
	pub record:   AgentRecord,
	/// Revision that must still match before live resources are detached.
	pub revision: u64,
}

/// Historical, non-secret metrics retained after an agent parks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentHistory {
	/// Provider requests completed by this agent.
	pub requests:      u64,
	/// Metered input tokens.
	pub input_tokens:  u64,
	/// Metered output and reasoning tokens.
	pub output_tokens: u64,
	/// Durable receipt cost in micro-USD.
	pub usd_micros:    u64,
	/// Total active duration in milliseconds.
	pub duration_ms:   u64,
	/// Markdown or structured output artifact.
	pub output_path:   Option<PathBuf>,
	/// Preserved patch artifact.
	pub patch_path:    Option<PathBuf>,
	/// Preserved branch name.
	pub branch:        Option<Str>,
	/// Historical terminal outcome; cancellation and failure never destroy
	/// identity.
	pub terminal:      Option<SubagentTerminalStatus>,
}

/// Clone-cheap process-global roster projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRecord {
	/// Stable process identity.
	pub id:               Str,
	/// Human-readable routing name.
	pub name:             Str,
	/// Main, subagent, or advisor classification.
	pub kind:             AgentKind,
	/// Parent agent identity.
	pub parent:           Option<Str>,
	/// Owning durable session identity.
	pub session:          Str,
	/// Recursion depth.
	pub depth:            u16,
	/// Current process-global lifecycle state.
	pub status:           RegistryStatus,
	/// Sanitized one-line activity gist.
	pub activity:         Str,
	/// Last lifecycle or activity change in epoch milliseconds.
	pub last_activity_ms: u64,
	/// Read-only transcript backing history and cold revival.
	pub transcript:       Option<PathBuf>,
	/// Agent definition name used to create the session.
	pub definition:       Option<Str>,
	/// Requested model role or selector retained from child initialization.
	pub model:            Option<Str>,
	/// Actual serving model most recently observed in the bounded prefix.
	pub serving_model:    Option<Str>,
	/// Normalized task summary for historical rosters.
	pub task:             Option<Str>,
	/// Historical execution and merge facts.
	pub history:          AgentHistory,
}

/// Credential-free registry row safe for collaboration presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollabAgentRecord {
	/// Stable process identity.
	pub id:               Str,
	/// Sanitized display name.
	pub name:             Str,
	/// Main or task-subagent classification; advisors are never representable.
	pub kind:             CollabAgentKind,
	/// Visible parent agent identity.
	pub parent:           Option<Str>,
	/// Current lifecycle state.
	pub status:           RegistryStatus,
	/// Whether a bounded transcript fetch may be requested.
	pub has_transcript:   bool,
	/// Last activity change in epoch milliseconds.
	pub last_activity_ms: u64,
}

/// Agent kinds permitted in collaboration registry snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum CollabAgentKind {
	/// Main session agent.
	Main,
	/// User-visible task subagent.
	Sub,
}

/// Generation-fenced collaboration registry snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollabRegistrySnapshot {
	/// Monotonic process-global registry generation.
	pub generation: u64,
	/// Deterministically ordered public registry rows.
	pub agents:     Arc<[CollabAgentRecord]>,
}

/// Registry compare-and-swap or persistence failure.
#[derive(Debug, Error)]
pub enum RegistryError {
	/// The stable id was not registered.
	#[error("agent {0} is not registered")]
	NotFound(Str),
	/// The expected record revision did not match.
	#[error("agent {id} registry revision changed (expected {expected}, actual {actual})")]
	Revision {
		/// Stable agent id.
		id:       Str,
		/// Revision supplied by the caller.
		expected: u64,
		/// Current revision.
		actual:   u64,
	},
	/// A recovered display alias remains reserved by another stable identity.
	#[error("agent display name remains reserved: {0}")]
	NameReserved(Str),
	/// The requested agent or history artifact does not exist.
	#[error("agent resource was not found: {0}")]
	ResourceNotFound(Str),
	/// Agent output was not valid JSON.
	#[error("agent output is not valid JSON")]
	InvalidJson(#[source] serde_json::Error),
	/// The jq program could not be loaded or compiled.
	#[error("agent output query is invalid")]
	InvalidQuery,
	/// The jq program exceeded its query, input, output, depth, or time bound.
	#[error("agent output query exceeded a safety limit")]
	QueryLimit,
	/// The jq program emitted no value.
	#[error("agent output query emitted no value")]
	QueryEmpty,
	/// The jq program emitted more than one value.
	#[error("agent output query emitted more than one value")]
	QueryMultiple,
	/// A transcript or artifact could not be read.
	#[error("agent resource I/O failed: {0}")]
	Io(#[from] io::Error),
}

struct RegistryEntry {
	record:       AgentRecord,
	revision:     u64,
	live_history: Option<Arc<[u8]>>,
}

#[derive(Clone, Debug, Default)]
struct PersistedRosterLatch {
	owned: HashMap<Str, PathBuf>,
}

struct RegistryInner {
	records:        Mutex<HashMap<Str, RegistryEntry>>,
	diagnostics:    Mutex<VecDeque<DiscoveryDiagnostic>>,
	restored_roots: Mutex<HashMap<PathBuf, PersistedRosterLatch>>,
	roster_scan:    Mutex<()>,
	generation:     watch::Sender<u64>,
}

/// Process-global CAS registry for live, parked, and disk-recovered agents.
#[derive(Clone)]
pub struct AgentRegistry {
	inner: Arc<RegistryInner>,
}

impl Default for AgentRegistry {
	fn default() -> Self {
		Self::new()
	}
}

impl AgentRegistry {
	/// Returns the one process-global registry.
	pub fn global() -> &'static Self {
		static GLOBAL: LazyLock<AgentRegistry> = LazyLock::new(AgentRegistry::new);
		&GLOBAL
	}

	/// Creates an independent registry, primarily for an isolated daemon or
	/// test.
	pub fn new() -> Self {
		let (generation, _) = watch::channel(0_u64);
		Self {
			inner: Arc::new(RegistryInner {
				records: Mutex::new(HashMap::new()),
				diagnostics: Mutex::new(VecDeque::with_capacity(DISCOVERY_DIAGNOSTIC_CAPACITY)),
				restored_roots: Mutex::new(HashMap::new()),
				roster_scan: Mutex::new(()),
				generation,
			}),
		}
	}

	/// Subscribes to every registration, lifecycle, activity, and history
	/// change.
	pub fn subscribe(&self) -> Receiver<u64> {
		self.inner.generation.subscribe()
	}

	/// Returns the current process-global generation.
	pub fn generation(&self) -> u64 {
		*self.inner.generation.borrow()
	}

	/// Registers `record` iff `expected` matches the current revision. `None`
	/// requires the id to be absent.
	pub fn compare_and_register(
		&self,
		mut record: AgentRecord,
		expected: Option<u64>,
	) -> Result<u64, RegistryError> {
		let mut records = self.inner.records.lock();
		let existing_key = records
			.keys()
			.find(|id| id.as_str().eq_ignore_ascii_case(record.id.as_str()))
			.cloned();
		let previous = existing_key.as_ref().and_then(|id| records.get(id));
		match previous {
			Some(entry) if expected != Some(entry.revision) => {
				return Err(RegistryError::Revision {
					id:       entry.record.id.clone(),
					expected: expected.unwrap_or(0),
					actual:   entry.revision,
				});
			},
			None if expected.is_some() => return Err(RegistryError::NotFound(record.id)),
			_ => {},
		}
		if records.iter().any(|(id, entry)| {
			existing_key.as_ref() != Some(id)
				&& entry
					.record
					.name
					.as_str()
					.eq_ignore_ascii_case(record.name.as_str())
		}) {
			return Err(RegistryError::NameReserved(record.name));
		}
		let key = existing_key.unwrap_or_else(|| record.id.clone());
		record.id = key.clone();
		record.activity = sanitize_activity(record.activity.as_str());
		let previous = records.get(&key);
		let revision = previous.map_or(1, |entry| entry.revision.saturating_add(1));
		let live_history = previous.and_then(|entry| entry.live_history.clone());
		records.insert(key, RegistryEntry { record, revision, live_history });
		drop(records);
		self.bump_generation();
		Ok(revision)
	}

	/// Registers a live tree node while preserving its durable identity.
	pub fn register_node(
		&self,
		node: &AgentNode,
		status: RegistryStatus,
		transcript: Option<PathBuf>,
	) -> Result<u64, RegistryError> {
		let previous = self.revision(node.id.as_str());
		self.compare_and_register(
			AgentRecord {
				id: node.id.clone(),
				name: node.name.clone(),
				kind: node.kind,
				parent: node.parent.clone(),
				session: node.session.clone(),
				depth: node.depth,
				status,
				activity: node.activity(),
				last_activity_ms: now_ms(),
				transcript,
				definition: None,
				model: None,
				serving_model: None,
				task: None,
				history: AgentHistory::default(),
			},
			previous,
		)
	}

	/// Returns one record and its CAS revision.
	pub fn record(&self, id: &str) -> Option<(AgentRecord, u64)> {
		let records = self.inner.records.lock();
		let (_, entry) = find_record(&records, id)?;
		Some((entry.record.clone(), entry.revision))
	}

	/// Lists the roster deterministically, optionally retaining advisors.
	pub fn roster(&self, include_advisors: bool) -> Vec<AgentRecord> {
		let mut records = self
			.inner
			.records
			.lock()
			.values()
			.filter(|entry| include_advisors || entry.record.kind != AgentKind::Advisor)
			.map(|entry| entry.record.clone())
			.collect::<Vec<_>>();
		records.sort_by(|left, right| {
			left
				.last_activity_ms
				.cmp(&right.last_activity_ms)
				.then_with(|| left.id.cmp(&right.id))
		});
		records
	}

	/// Returns a generation-fenced, credential-free collaboration roster.
	///
	/// Advisor identities, transcript paths, workspace/session ids, models,
	/// activity text, and historical artifacts remain host-local.
	pub fn collab_snapshot(&self) -> CollabRegistrySnapshot {
		let generation = self.generation();
		let agents = self
			.roster(false)
			.into_iter()
			.filter_map(|record| {
				let kind = match record.kind {
					AgentKind::Main => CollabAgentKind::Main,
					AgentKind::Subagent => CollabAgentKind::Sub,
					AgentKind::Advisor => return None,
				};
				Some(CollabAgentRecord {
					id: record.id,
					name: record.name,
					kind,
					parent: record.parent,
					status: record.status,
					has_transcript: record.transcript.is_some(),
					last_activity_ms: record.last_activity_ms,
				})
			})
			.collect::<Vec<_>>()
			.into();
		CollabRegistrySnapshot { generation, agents }
	}

	/// Lists only agents whose retained parent chain belongs to `root`.
	///
	/// This prevents parked records discovered for another root session from
	/// leaking into a caller's roster while preserving live descendants at any
	/// nesting depth.
	pub fn roster_for_root(&self, root: &str, include_advisors: bool) -> Vec<AgentRecord> {
		let records = self.inner.records.lock();
		let mut roster = records
			.values()
			.filter(|entry| include_advisors || entry.record.kind != AgentKind::Advisor)
			.filter(|entry| record_belongs_to_root(&records, &entry.record, root))
			.map(|entry| entry.record.clone())
			.collect::<Vec<_>>();
		roster.sort_by(|left, right| {
			left
				.last_activity_ms
				.cmp(&right.last_activity_ms)
				.then_with(|| left.id.cmp(&right.id))
		});
		roster
	}

	/// CAS-updates one lifecycle state.
	pub fn set_status(
		&self,
		id: &str,
		expected: Option<u64>,
		status: RegistryStatus,
	) -> Result<u64, RegistryError> {
		self.update(id, expected, |record| {
			record.status = status;
			Ok(())
		})
	}

	/// Replaces the sanitized activity gist and refreshes idle TTL accounting.
	pub fn set_activity(&self, id: &str, activity: &str) -> Result<u64, RegistryError> {
		let activity = sanitize_activity(activity);
		self.update(id, None, |record| {
			record.activity = activity;
			Ok(())
		})
	}

	/// Removes one record by id, freeing its display alias for a successor.
	///
	/// Returns whether a record existed. Live routing state is separate and is
	/// removed through [`Broker::unregister`].
	pub fn remove(&self, id: &str) -> bool {
		let removed = {
			let mut records = self.inner.records.lock();
			let key = records
				.keys()
				.find(|key| key.as_str().eq_ignore_ascii_case(id))
				.cloned();
			key.is_some_and(|key| records.remove(&key).is_some())
		};
		if removed {
			self.bump_generation();
		}
		removed
	}

	/// Replaces durable transcript, model, task, and historical result facts.
	pub fn set_history(
		&self,
		id: &str,
		transcript: Option<PathBuf>,
		model: Option<Str>,
		task: Option<Str>,
		history: AgentHistory,
	) -> Result<u64, RegistryError> {
		self.update(id, None, |record| {
			record.transcript = transcript;
			record.model = model;
			record.task = task;
			record.history = history;
			Ok(())
		})
	}

	/// Retains a terminal generation outcome without changing durable identity.
	pub fn set_terminal(
		&self,
		id: &str,
		terminal: SubagentTerminalStatus,
	) -> Result<u64, RegistryError> {
		self.update(id, None, |record| {
			record.history.terminal = Some(terminal.bounded());
			Ok(())
		})
	}

	/// Parks idle records whose TTL elapsed and returns records whose owners
	/// should dispose their live sessions.
	pub fn park_expired(&self, now: u64, ttl: Duration) -> Vec<ParkLease> {
		if ttl.is_zero() {
			return Vec::new();
		}
		let ttl = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
		let mut records = self.inner.records.lock();
		let mut parked = Vec::new();
		for entry in records.values_mut() {
			if entry.record.status == RegistryStatus::Idle
				&& now.saturating_sub(entry.record.last_activity_ms) >= ttl
			{
				entry.record.status = RegistryStatus::Parked;
				entry.record.last_activity_ms = now;
				entry.revision = entry.revision.saturating_add(1);
				parked.push(ParkLease { record: entry.record.clone(), revision: entry.revision });
			}
		}
		drop(records);
		if !parked.is_empty() {
			self.bump_generation();
		}
		parked
	}

	/// Imports bounded valid transcript prefixes as parked records.
	///
	/// This unscoped entry point is retained for explicit administrative
	/// discovery. Model-facing callers should use
	/// [`Self::restore_transcripts_once`], which filters records to one root
	/// session.
	pub fn discover_transcripts(&self, directory: &Path) -> Result<usize, RegistryError> {
		let records = self.scan_transcripts(directory)?;
		Ok(self.import_transcripts(records).0)
	}

	/// Restores parked transcripts at most once for one canonical root session
	/// file.
	///
	/// Root lookup and transcript discovery failures are warned and remain
	/// retryable instead of failing the caller's roster request. Distinct roots
	/// serialize scan bodies because shared child ids mutate one process-global
	/// registry. Settled latches are reused only while every id still points at
	/// the transcript restored by that scan.
	pub fn restore_transcripts_once(&self, root_file: &Path, directory: &Path) -> usize {
		let root = match fs::canonicalize(root_file) {
			Ok(root) => root,
			Err(error) => {
				tracing::warn!(
					path = %root_file.display(),
					%error,
					"failed to resolve persisted agent roster root"
				);
				return 0;
			},
		};
		if self.restored_root_is_valid(&root) {
			return 0;
		}

		let _scan = self.inner.roster_scan.lock();
		if self.restored_root_is_valid(&root) {
			return 0;
		}
		self.inner.restored_roots.lock().remove(&root);

		let root_id = match root_session_id(&root) {
			Ok(root_id) => root_id,
			Err(error) => {
				tracing::warn!(
					root = %root.display(),
					%error,
					"failed to read persisted agent roster root"
				);
				return 0;
			},
		};
		let restored = self.scan_transcripts(directory).map(|records| {
			let records = records_for_scanned_root(records, root_id.as_str());
			self.import_transcripts(records)
		});
		match restored {
			Ok((imported, owned)) => {
				let mut latches = self.inner.restored_roots.lock();
				if latches.len() >= MAX_PERSISTED_ROSTER_LATCHES
					&& let Some(expired) = latches.keys().next().cloned()
				{
					latches.remove(&expired);
				}
				latches.insert(root, PersistedRosterLatch { owned });
				imported
			},
			Err(error) => {
				tracing::warn!(
					root = %root.display(),
					path = %directory.display(),
					%error,
					"failed to restore persisted agent roster"
				);
				0
			},
		}
	}

	fn restored_root_is_valid(&self, root: &Path) -> bool {
		let latches = self.inner.restored_roots.lock();
		let Some(latch) = latches.get(root) else {
			return false;
		};
		let records = self.inner.records.lock();
		latch.owned.iter().all(|(id, transcript)| {
			find_record(&records, id.as_str()).is_some_and(|(_, entry)| {
				entry.record.transcript.as_deref() == Some(transcript.as_path())
			})
		})
	}

	fn scan_transcripts(&self, directory: &Path) -> Result<Vec<AgentRecord>, RegistryError> {
		let mut records = Vec::new();
		for entry in fs::read_dir(directory)? {
			let path = entry?.path();
			if path.extension().and_then(ffi::OsStr::to_str) != Some("jsonl") {
				continue;
			}
			match cold_record(&path)? {
				ColdScan::Record(record) => records.push(record),
				ColdScan::Skipped(kind) => self.record_discovery_diagnostic(path, kind),
			}
		}
		Ok(records)
	}

	fn import_transcripts(&self, records: Vec<AgentRecord>) -> (usize, HashMap<Str, PathBuf>) {
		let mut imported = 0;
		let mut owned = HashMap::new();
		for record in records {
			let id = record.id.clone();
			let transcript = record.transcript.clone();
			let existing = self.record(id.as_str());
			let replaceable = existing.as_ref().is_none_or(|(current, _)| {
				current.status == RegistryStatus::Parked && current.transcript.is_some()
			});
			if replaceable {
				let expected = existing.map(|(_, revision)| revision);
				if self.compare_and_register(record, expected).is_ok() {
					imported += 1;
				}
			}
			if let Some(transcript) = transcript
				&& self.registry_transcript_matches(id.as_str(), &transcript)
			{
				owned.insert(id, transcript);
			}
		}
		(imported, owned)
	}

	fn registry_transcript_matches(&self, id: &str, transcript: &Path) -> bool {
		self
			.record(id)
			.is_some_and(|(record, _)| record.transcript.as_deref() == Some(transcript))
	}

	/// Returns retained bounded-prefix diagnostics, oldest first.
	pub fn discovery_diagnostics(&self) -> Vec<DiscoveryDiagnostic> {
		self.inner.diagnostics.lock().iter().cloned().collect()
	}

	/// Resolves `agent://<id>` or `agent://<id>/<child>` to immutable artifact
	/// bytes. Child names become dot-separated artifact stems.
	pub fn resolve_agent(&self, resource: &str) -> Result<Vec<u8>, RegistryError> {
		Ok(fs::read(self.agent_path(resource)?)?)
	}

	/// Resolves an agent artifact with the caller root taking precedence over
	/// the process-global registry.
	pub fn resolve_agent_from(
		&self,
		resource: &str,
		preferred_directory: &Path,
	) -> Result<Vec<u8>, RegistryError> {
		Ok(fs::read(self.agent_path_from(resource, preferred_directory)?)?)
	}

	/// Resolves and queries an agent artifact with the caller root taking
	/// precedence over the process-global registry.
	pub fn resolve_agent_query_from(
		&self,
		resource: &str,
		query: &str,
		preferred_directory: &Path,
	) -> Result<Vec<u8>, RegistryError> {
		let path = self.agent_path_from(resource, preferred_directory)?;
		if fs::metadata(&path)?.len() > QUERY_MAX_BYTES as u64 {
			return Err(RegistryError::QueryLimit);
		}
		let bytes = fs::read(path)?;
		bounded_json_query(&bytes, query)
	}

	/// Resolves an output and applies one bounded jq-compatible expression.
	pub fn resolve_agent_query(
		&self,
		resource: &str,
		query: &str,
	) -> Result<Vec<u8>, RegistryError> {
		let path = self.agent_path(resource)?;
		if fs::metadata(&path)?.len() > QUERY_MAX_BYTES as u64 {
			return Err(RegistryError::QueryLimit);
		}
		let bytes = fs::read(path)?;
		bounded_json_query(&bytes, query)
	}

	/// Resolves an agent artifact path with the caller root taking precedence
	/// over the process-global registry.
	pub fn agent_path_from(
		&self,
		resource: &str,
		preferred_directory: &Path,
	) -> Result<PathBuf, RegistryError> {
		let resource = resource.trim_start_matches('/');
		let (id, child) = resource.split_once('/').unwrap_or((resource, ""));
		let suffix = if child.is_empty() {
			String::new()
		} else {
			format!(".{}", child.replace('/', "."))
		};
		if valid_artifact_component(id)
			&& (suffix.is_empty() || valid_artifact_component(&suffix[1..]))
		{
			let preferred = preferred_directory.join(format!("{id}{suffix}.md"));
			if preferred.is_file() {
				return Ok(preferred);
			}
		}
		self.agent_path(resource)
	}

	fn agent_path(&self, resource: &str) -> Result<PathBuf, RegistryError> {
		let resource = resource.trim_start_matches('/');
		let (id, child) = resource.split_once('/').unwrap_or((resource, ""));
		let (record, _) = self
			.record(id)
			.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(id)))?;
		if child.is_empty() {
			return record
				.history
				.output_path
				.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(resource)));
		}
		let child = child.replace('/', ".");
		if !valid_artifact_component(&child) {
			return Err(RegistryError::ResourceNotFound(Str::new(resource)));
		}
		let parent = record
			.history
			.output_path
			.as_ref()
			.and_then(|path| path.parent())
			.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(resource)))?;
		Ok(parent.join(format!("{}.{}.md", record.id, child)))
	}

	/// Replaces the live in-memory transcript projection for one session.
	///
	/// Returns whether a matching live registry entry was present.
	pub fn set_live_history(&self, session: &str, history: Vec<u8>) -> bool {
		let mut records = self.inner.records.lock();
		let Some(entry) = records
			.values_mut()
			.find(|entry| entry.record.session == session || entry.record.id == session)
		else {
			return false;
		};
		entry.live_history = Some(Arc::from(history));
		true
	}

	/// Resolves history with the caller's transcript directory taking
	/// precedence over a colliding process-global registry entry.
	pub fn resolve_history_from(
		&self,
		resource: &str,
		preferred_directory: &Path,
	) -> Result<Vec<u8>, RegistryError> {
		let id = resource.trim_matches('/');
		if id.is_empty() {
			return Ok(self.history_index().into_bytes());
		}
		if id.starts_with("__advisor") {
			return Err(RegistryError::ResourceNotFound(Str::new(id)));
		}
		if valid_artifact_component(id)
			&& let Ok(entries) = fs::read_dir(preferred_directory)
		{
			for entry in entries {
				let entry = entry?;
				let path = entry.path();
				if path.extension().and_then(ffi::OsStr::to_str) == Some("jsonl")
					&& path
						.file_stem()
						.and_then(ffi::OsStr::to_str)
						.is_some_and(|stem| stem.eq_ignore_ascii_case(id))
				{
					return Ok(fs::read(path)?);
				}
			}
		}
		if self
			.record(id)
			.is_some_and(|(record, _)| record.kind == AgentKind::Advisor)
		{
			return Err(RegistryError::ResourceNotFound(Str::new(id)));
		}
		self.resolve_history(resource)
	}

	/// Resolves `history://` to a roster index and `history://<id>` to immutable
	/// transcript bytes.
	pub fn resolve_history(&self, resource: &str) -> Result<Vec<u8>, RegistryError> {
		let id = resource.trim_matches('/');
		if id.is_empty() {
			return Ok(self.history_index().into_bytes());
		}
		let (live_history, path) = {
			let records = self.inner.records.lock();
			let (_, entry) = find_record(&records, id)
				.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(id)))?;
			(entry.live_history.clone(), entry.record.transcript.clone())
		};
		if let Some(history) = live_history {
			return Ok(history.to_vec());
		}
		let path = path.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(id)))?;
		Ok(fs::read(path)?)
	}

	/// Renders the live/parked/disk transcript index used by `history://`.
	pub fn history_index(&self) -> String {
		let mut output = String::from(
			"| id | name | kind | status | parent/depth | definition | model → serving | task | last \
			 active |\n",
		);
		output.push_str("|---|---|---|---|---|---|---|---|---:|\n");
		let now = now_ms();
		for record in self.roster(false) {
			let age = now.saturating_sub(record.last_activity_ms) / 1_000;
			output.push_str(&format!(
				"| {} | {} | {} | {} | {}/{} | {} | {} → {} | {} | {}s |\n",
				record.id,
				record.name,
				record.kind,
				record.status,
				record.parent.as_deref().unwrap_or("-"),
				record.depth,
				record.definition.as_deref().unwrap_or("-"),
				record.model.as_deref().unwrap_or("-"),
				record.serving_model.as_deref().unwrap_or("-"),
				record.task.as_deref().unwrap_or("-"),
				age,
			));
		}
		output
	}

	/// Renders the `history://` index scoped to one caller root session.
	pub fn history_index_for_root(&self, root: &str) -> String {
		let mut output = String::from(
			"| id | name | kind | status | parent/depth | definition | model → serving | task | last \
			 active |\n",
		);
		output.push_str("|---|---|---|---|---|---|---|---|---:|\n");
		let now = now_ms();
		for record in self.roster_for_root(root, false) {
			let age = now.saturating_sub(record.last_activity_ms) / 1_000;
			output.push_str(&format!(
				"| {} | {} | {} | {} | {}/{} | {} | {} → {} | {} | {}s |\n",
				record.id,
				record.name,
				record.kind,
				record.status,
				record.parent.as_deref().unwrap_or("-"),
				record.depth,
				record.definition.as_deref().unwrap_or("-"),
				record.model.as_deref().unwrap_or("-"),
				record.serving_model.as_deref().unwrap_or("-"),
				record.task.as_deref().unwrap_or("-"),
				age,
			));
		}
		output
	}

	fn record_discovery_diagnostic(&self, path: PathBuf, kind: DiscoveryDiagnosticKind) {
		let mut diagnostics = self.inner.diagnostics.lock();
		if diagnostics.len() == DISCOVERY_DIAGNOSTIC_CAPACITY {
			diagnostics.pop_front();
		}
		diagnostics.push_back(DiscoveryDiagnostic { path, kind });
		drop(diagnostics);
		self.bump_generation();
	}

	fn revision(&self, id: &str) -> Option<u64> {
		self.record(id).map(|(_, revision)| revision)
	}

	fn update(
		&self,
		id: &str,
		expected: Option<u64>,
		change: impl FnOnce(&mut AgentRecord) -> Result<(), RegistryError>,
	) -> Result<u64, RegistryError> {
		let mut records = self.inner.records.lock();
		let key = records
			.keys()
			.find(|candidate| candidate.as_str().eq_ignore_ascii_case(id))
			.cloned()
			.ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		let entry = records.get_mut(&key).expect("key selected from same map");
		if let Some(expected) = expected
			&& expected != entry.revision
		{
			return Err(RegistryError::Revision { id: key, expected, actual: entry.revision });
		}
		change(&mut entry.record)?;
		entry.record.last_activity_ms = now_ms();
		entry.revision = entry.revision.saturating_add(1);
		let revision = entry.revision;
		drop(records);
		self.bump_generation();
		Ok(revision)
	}

	fn bump_generation(&self) {
		self
			.inner
			.generation
			.send_modify(|generation| *generation = generation.wrapping_add(1));
	}
}

/// Metadata projected alongside a canonical peer thread item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerMessage {
	/// Stable message identity.
	pub id:            Str,
	/// Stable sender agent identity.
	pub from:          Str,
	/// Address supplied by the sender.
	pub to:            Str,
	/// Plain-prose coordination text.
	pub text:          Str,
	/// Delivery boundary.
	pub mode:          DeliveryMode,
	/// Optional prior message being answered.
	pub reply_to:      Option<Str>,
	/// Sender wall-clock timestamp.
	pub sent_ms:       u64,
	/// Sender session identity.
	pub session_id:    Str,
	/// Whether the sender is synchronously awaiting a reply.
	pub expects_reply: bool,
}

/// One message leg's stable delivery state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryReceipt {
	/// Resolved recipient identity or unresolved requested address.
	pub to:          Str,
	/// How the message reached the recipient.
	pub outcome:     Receipt,
	/// Read-only journal pointer supplied when a known recipient cannot run.
	pub history_uri: Option<Str>,
}

/// Routed event published once for a non-deduplicated delivery leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutedEvent {
	/// Message with its stable id and exact visible body.
	pub message:       PeerMessage,
	/// Result for the resolved delivery leg.
	pub delivery:      DeliveryReceipt,
	/// Whether the main UI should show this body as a display-only observation.
	pub relay_to_main: bool,
}

/// A message accepted by a cold-revival owner.
#[derive(Clone, Debug)]
pub struct RevivalRequest {
	/// Parked recipient identity.
	pub recipient:         Str,
	/// Registry revision that selected this parked generation.
	pub registry_revision: u64,
	/// First message to inject after reconstruction.
	pub message:           PeerMessage,
}

/// Broker routing failure independent of per-recipient receipts.
#[derive(Debug, Error)]
pub enum BrokerError {
	/// Empty addresses are never broadcast implicitly.
	#[error("broker address is empty")]
	EmptyAddress,
}

struct WaitFilter {
	generation: u64,
	sender:     Option<Str>,
	reply_to:   Option<Str>,
}

struct InboxState {
	queue:       Mutex<VecDeque<PeerMessage>>,
	waiter:      Mutex<Option<WaitFilter>>,
	next_waiter: AtomicU64,
	notify:      Notify,
}

impl InboxState {
	fn new() -> Self {
		Self {
			queue:       Mutex::new(VecDeque::with_capacity(MAILBOX_CAPACITY)),
			waiter:      Mutex::new(None),
			next_waiter: AtomicU64::new(1),
			notify:      Default::default(),
		}
	}

	fn push(&self, message: PeerMessage) {
		let mut queue = self.queue.lock();
		if queue.len() == MAILBOX_CAPACITY {
			queue.pop_front();
		}
		queue.push_back(message);
		drop(queue);
		self.notify.notify_waiters();
	}

	fn register_waiter(
		self: &Arc<Self>,
		sender: Option<&str>,
		reply_to: Option<&str>,
	) -> WaitRegistration {
		let generation = self.next_waiter.fetch_add(1, Ordering::Relaxed);
		*self.waiter.lock() = Some(WaitFilter {
			generation,
			sender: sender.map(Str::new),
			reply_to: reply_to.map(Str::new),
		});
		WaitRegistration { state: Arc::clone(self), generation }
	}

	fn deliver_waiter(&self, message: &PeerMessage) -> bool {
		let matches = self.waiter.lock().as_ref().is_some_and(|waiter| {
			waiter
				.sender
				.as_deref()
				.is_none_or(|sender| sender.eq_ignore_ascii_case(message.from.as_str()))
				&& waiter
					.reply_to
					.as_deref()
					.is_none_or(|reply| message.reply_to.as_deref() == Some(reply))
		});
		if matches {
			self.push(message.clone());
		}
		matches
	}

	fn matching(&self, sender: Option<&str>, reply_to: Option<&str>) -> Option<PeerMessage> {
		let mut queue = self.queue.lock();
		let index = queue.iter().position(|message| {
			sender.is_none_or(|sender| sender.eq_ignore_ascii_case(message.from.as_str()))
				&& reply_to.is_none_or(|reply| message.reply_to.as_deref() == Some(reply))
		})?;
		queue.remove(index)
	}

	fn read(&self, peek: bool) -> Vec<PeerMessage> {
		let mut queue = self.queue.lock();
		if peek {
			queue.iter().cloned().collect()
		} else {
			queue.drain(..).collect()
		}
	}
}

#[must_use]
struct WaitRegistration {
	state:      Arc<InboxState>,
	generation: u64,
}

impl Drop for WaitRegistration {
	fn drop(&mut self) {
		let mut waiter = self.state.waiter.lock();
		if waiter
			.as_ref()
			.is_some_and(|waiter| waiter.generation == self.generation)
		{
			*waiter = None;
		}
	}
}

struct RegisteredNode {
	name:            Str,
	session:         Str,
	mailbox:         Option<MailboxSender>,
	inbox:           Arc<InboxState>,
	revival:         Option<flume::Sender<RevivalRequest>>,
	revival_pending: bool,
	idle:            bool,
	terminal_turn:   u64,
}

struct DeliveryCache {
	entries: HashMap<(Str, Str), DeliveryReceipt>,
	order:   VecDeque<(Str, Str)>,
}

impl DeliveryCache {
	fn new() -> Self {
		Self {
			entries: HashMap::with_capacity(DELIVERY_DEDUP_CAPACITY),
			order:   VecDeque::with_capacity(DELIVERY_DEDUP_CAPACITY),
		}
	}

	fn get(&self, message: &str, recipient: &str) -> Option<DeliveryReceipt> {
		self
			.entries
			.iter()
			.find(|((cached_message, cached_recipient), _)| {
				cached_message == message && cached_recipient.eq_ignore_ascii_case(recipient)
			})
			.map(|(_, delivery)| delivery.clone())
	}

	fn insert(&mut self, message: &str, delivery: DeliveryReceipt) {
		let key = (Str::new(message), delivery.to.clone());
		if self.entries.contains_key(&key) {
			return;
		}
		if self.order.len() == DELIVERY_DEDUP_CAPACITY
			&& let Some(expired) = self.order.pop_front()
		{
			self.entries.remove(&expired);
		}
		self.order.push_back(key.clone());
		self.entries.insert(key, delivery);
	}
}

struct BrokerInner {
	project:    Str,
	nodes:      Mutex<HashMap<Str, RegisteredNode>>,
	deliveries: Mutex<DeliveryCache>,
	events:     broadcast::Sender<RoutedEvent>,
	generation: watch::Sender<u64>,
	registry:   AgentRegistry,
}

/// Core-owned project routing table backed by the process-global registry.
#[derive(Clone)]
pub struct Broker {
	inner: Arc<BrokerInner>,
}

impl Broker {
	/// Creates a broker using the process-global lifecycle registry.
	pub fn new(project: Str) -> Self {
		Self::with_registry(project, AgentRegistry::global().clone())
	}

	/// Creates a broker with an explicit registry.
	pub fn with_registry(project: Str, registry: AgentRegistry) -> Self {
		let (generation, _) = watch::channel(0_u64);
		let (events, _) = broadcast::channel(MAILBOX_CAPACITY);
		Self {
			inner: Arc::new(BrokerInner {
				project,
				nodes: Mutex::new(HashMap::new()),
				deliveries: Mutex::new(DeliveryCache::new()),
				events,
				generation,
				registry,
			}),
		}
	}

	/// Returns the registry shared with URL resolvers and roster projections.
	pub fn registry(&self) -> &AgentRegistry {
		&self.inner.registry
	}

	/// Registers a messageable live node and returns its bounded inbox.
	///
	/// A main-kind node supersedes any previous main incarnation holding the
	/// same display alias: the retired record and its routing entry are
	/// removed so session switches (`/new`, resume) re-register cleanly.
	pub fn register(
		&self,
		node: &AgentNode,
		mailbox: MailboxSender,
	) -> Result<BrokerInbox, RegistryError> {
		if node.kind == AgentKind::Main
			&& let Some((previous, _)) = self.inner.registry.record(node.name.as_str())
			&& previous.kind == AgentKind::Main
			&& !previous.id.as_str().eq_ignore_ascii_case(node.id.as_str())
		{
			self.unregister(previous.id.as_str());
			self.inner.registry.remove(previous.id.as_str());
		}
		self
			.inner
			.registry
			.register_node(node, RegistryStatus::Idle, None)?;
		let inbox = Arc::new(InboxState::new());
		self
			.inner
			.nodes
			.lock()
			.insert(node.id.clone(), RegisteredNode {
				name:            node.name.clone(),
				session:         node.session.clone(),
				mailbox:         Some(mailbox),
				inbox:           Arc::clone(&inbox),
				revival:         None,
				revival_pending: false,
				idle:            true,
				terminal_turn:   0,
			});
		self.bump_generation();
		Ok(BrokerInbox {
			owner:     node.id.clone(),
			state:     inbox,
			broker:    Arc::downgrade(&self.inner),
			roster:    self.inner.generation.subscribe(),
			lifecycle: self.inner.registry.subscribe(),
		})
	}

	/// Registers a parked record with a nonblocking cold-revival transport.
	pub fn register_parked(
		&self,
		mut record: AgentRecord,
		revival: flume::Sender<RevivalRequest>,
	) -> Result<(), RegistryError> {
		record.status = RegistryStatus::Parked;
		let expected = self.inner.registry.revision(record.id.as_str());
		self
			.inner
			.registry
			.compare_and_register(record.clone(), expected)?;
		self.inner.nodes.lock().insert(record.id, RegisteredNode {
			name:            record.name,
			session:         record.session,
			mailbox:         None,
			inbox:           Arc::new(InboxState::new()),
			revival:         Some(revival),
			revival_pending: false,
			idle:            true,
			terminal_turn:   0,
		});
		self.bump_generation();
		Ok(())
	}

	/// Attaches a reconstructed live mailbox without replacing historical data.
	pub fn attach_live(
		&self,
		id: &str,
		expected_revision: u64,
		mailbox: MailboxSender,
	) -> Result<BrokerInbox, RegistryError> {
		let mut nodes = self.inner.nodes.lock();
		let (_, node) =
			find_node_mut(&mut nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		self
			.inner
			.registry
			.set_status(id, Some(expected_revision), RegistryStatus::Idle)?;
		node.mailbox = Some(mailbox);
		node.revival_pending = false;
		node.idle = true;
		let state = Arc::clone(&node.inbox);
		let owner = nodes
			.keys()
			.find(|key| key.as_str().eq_ignore_ascii_case(id))
			.cloned()
			.ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		drop(nodes);
		self.bump_generation();
		Ok(BrokerInbox {
			owner,
			state,
			broker: Arc::downgrade(&self.inner),
			roster: self.inner.generation.subscribe(),
			lifecycle: self.inner.registry.subscribe(),
		})
	}

	/// Removes a terminal node from routing while retaining registry history.
	pub fn unregister(&self, id: &str) {
		let removed = {
			let mut nodes = self.inner.nodes.lock();
			let key = nodes
				.keys()
				.find(|key| key.as_str().eq_ignore_ascii_case(id))
				.cloned();
			key.is_some_and(|key| nodes.remove(&key).is_some())
		};
		if removed {
			self.bump_generation();
		}
	}

	/// Marks a live session parked and detaches its mailbox.
	pub fn park(&self, id: &str, expected_revision: u64) -> Result<(), RegistryError> {
		let mut nodes = self.inner.nodes.lock();
		let (_, node) =
			find_node_mut(&mut nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		self
			.inner
			.registry
			.set_status(id, Some(expected_revision), RegistryStatus::Parked)?;
		node.mailbox = None;
		node.revival_pending = false;
		node.idle = true;
		drop(nodes);
		self.bump_generation();
		Ok(())
	}

	/// Sets whether a routed node is currently idle.
	pub fn set_idle(&self, id: &str, idle: bool) -> Result<(), RegistryError> {
		let mut nodes = self.inner.nodes.lock();
		let (_, node) =
			find_node_mut(&mut nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		node.idle = idle;
		drop(nodes);
		self.inner.registry.set_status(
			id,
			None,
			if idle {
				RegistryStatus::Idle
			} else {
				RegistryStatus::Running
			},
		)?;
		Ok(())
	}

	/// Atomically completes one fully unwound turn unless IRC work arrived after
	/// the loop's final drain.
	///
	/// A pending interrupt keeps the node running and requires the supervisor to
	/// issue an empty-input continuation. Only a truly quiescent boundary bumps
	/// the terminal-turn generation observed by awaited sends.
	pub fn finish_turn(&self, id: &str) -> Result<TurnEndDisposition, RegistryError> {
		let mut nodes = self.inner.nodes.lock();
		let (_, node) =
			find_node_mut(&mut nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		if node
			.mailbox
			.as_ref()
			.is_some_and(MailboxSender::has_pending)
		{
			node.idle = false;
			drop(nodes);
			self
				.inner
				.registry
				.set_status(id, None, RegistryStatus::Running)?;
			return Ok(TurnEndDisposition::ContinuationPending);
		}
		node.idle = true;
		node.terminal_turn = node.terminal_turn.wrapping_add(1);
		drop(nodes);
		self
			.inner
			.registry
			.set_status(id, None, RegistryStatus::Idle)?;
		self.bump_generation();
		Ok(TurnEndDisposition::Terminal)
	}

	/// Marks a failed turn terminal even when queued work cannot be continued.
	///
	/// Failure callers use this only after an attempted IRC wake continuation
	/// also failed, ensuring awaited senders are not stranded behind work the
	/// recipient can no longer execute.
	pub fn finish_failed_turn(&self, id: &str) -> Result<(), RegistryError> {
		let mut nodes = self.inner.nodes.lock();
		let (_, node) =
			find_node_mut(&mut nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		node.idle = true;
		node.terminal_turn = node.terminal_turn.wrapping_add(1);
		drop(nodes);
		self
			.inner
			.registry
			.set_status(id, None, RegistryStatus::Idle)?;
		self.bump_generation();
		Ok(())
	}

	/// Routes one message to every address match without waiting for recipient
	/// turns. Each match publishes exactly one message-id-bearing event.
	pub fn route(&self, message: PeerMessage) -> Result<SmallVec<DeliveryReceipt, 4>, BrokerError> {
		if message.to.is_empty() {
			return Err(BrokerError::EmptyAddress);
		}
		let mut deliveries = SmallVec::new();
		let mut lifecycle = SmallVec::<(Str, RegistryStatus), 4>::new();
		let mut events = SmallVec::<RoutedEvent, 4>::new();
		let mut nodes = self.inner.nodes.lock();
		let sender_is_main = self
			.inner
			.registry
			.record(message.from.as_str())
			.is_some_and(|(record, _)| record.kind == AgentKind::Main);
		let broadcast_has_main = is_broadcast(message.to.as_str())
			&& nodes.iter().any(|(id, node)| {
				matches_address(&self.inner.project, &message.to, id, node)
					&& self
						.inner
						.registry
						.record(id)
						.is_some_and(|(record, _)| record.kind == AgentKind::Main)
			});
		for (id, node) in nodes
			.iter_mut()
			.filter(|(id, node)| matches_address(&self.inner.project, &message.to, id, node))
		{
			if let Some(cached) = self.inner.deliveries.lock().get(message.id.as_str(), id) {
				deliveries.push(cached);
				continue;
			}
			let outcome = if node.inbox.deliver_waiter(&message) {
				Receipt::Injected
			} else if let Some(mailbox) = node.mailbox.as_ref() {
				let interrupt = Interrupt {
					class:  class(message.mode),
					item:   peer_item(&message),
					source: InterruptSource::Peer { from: message.from.clone() },
				};
				if mailbox.try_enqueue(interrupt).is_ok() {
					node.inbox.push(message.clone());
					if node.idle {
						Receipt::Woken
					} else {
						Receipt::Injected
					}
				} else {
					node.mailbox = None;
					node.inbox.push(message.clone());
					Receipt::Failed
				}
			} else if node.revival_pending {
				node.inbox.push(message.clone());
				Receipt::Revived
			} else if node.revival.as_ref().is_some_and(|revival| {
				self
					.inner
					.registry
					.record(id)
					.is_some_and(|(_, registry_revision)| {
						revival
							.try_send(RevivalRequest {
								recipient: id.clone(),
								registry_revision,
								message: message.clone(),
							})
							.is_ok()
					})
			}) {
				node.revival_pending = true;
				Receipt::Revived
			} else {
				node.inbox.push(message.clone());
				Receipt::Failed
			};
			if outcome != Receipt::Failed && outcome != Receipt::Revived {
				lifecycle.push((
					id.clone(),
					if outcome == Receipt::Woken || !node.idle {
						RegistryStatus::Running
					} else {
						RegistryStatus::Idle
					},
				));
			}
			let history_uri = (outcome == Receipt::Failed)
				.then(|| history_uri(&self.inner.registry, id))
				.flatten();
			let delivery = DeliveryReceipt { to: id.clone(), outcome, history_uri };
			self
				.inner
				.deliveries
				.lock()
				.insert(message.id.as_str(), delivery.clone());
			let recipient_is_main = self
				.inner
				.registry
				.record(id)
				.is_some_and(|(record, _)| record.kind == AgentKind::Main);
			events.push(RoutedEvent {
				message:       message.clone(),
				delivery:      delivery.clone(),
				relay_to_main: outcome != Receipt::Failed
					&& !sender_is_main
					&& !recipient_is_main
					&& !broadcast_has_main,
			});
			deliveries.push(delivery);
		}
		drop(nodes);
		for (id, status) in lifecycle {
			let _ = self.inner.registry.set_status(id.as_str(), None, status);
		}
		if deliveries.is_empty() {
			let history_uri = history_uri(&self.inner.registry, message.to.as_str());
			let delivery =
				DeliveryReceipt { to: message.to.clone(), outcome: Receipt::Failed, history_uri };
			let cached = self
				.inner
				.deliveries
				.lock()
				.get(message.id.as_str(), delivery.to.as_str());
			if let Some(cached) = cached {
				deliveries.push(cached);
			} else {
				self
					.inner
					.deliveries
					.lock()
					.insert(message.id.as_str(), delivery.clone());
				events.push(RoutedEvent {
					message:       message.clone(),
					delivery:      delivery.clone(),
					relay_to_main: false,
				});
				deliveries.push(delivery);
			}
		}
		for event in events {
			let _ = self.inner.events.send(event);
		}
		Ok(deliveries)
	}

	/// Routes a message and returns its compact outcome vocabulary.
	pub fn send(&self, message: PeerMessage) -> Result<SmallVec<Receipt, 4>, BrokerError> {
		Ok(self
			.route(message)?
			.into_iter()
			.map(|delivery| delivery.outcome)
			.collect())
	}

	/// Drains or peeks at one agent's bounded FIFO inbox.
	pub fn inbox(&self, id: &str, peek: bool) -> Result<Vec<PeerMessage>, RegistryError> {
		let nodes = self.inner.nodes.lock();
		let (_, node) = find_node(&nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		Ok(node.inbox.read(peek))
	}

	/// Returns one agent's unread bounded-inbox count.
	pub fn unread_count(&self, id: &str) -> Result<usize, RegistryError> {
		let nodes = self.inner.nodes.lock();
		let (_, node) = find_node(&nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		Ok(node.inbox.queue.lock().len())
	}

	/// Lists currently messageable node identities for the project or a session.
	pub fn peers(&self, session: Option<&str>) -> SmallVec<Str, 4> {
		self
			.inner
			.nodes
			.lock()
			.iter()
			.filter(|&(_id, node)| session.is_none() || session == Some(node.session.as_str()))
			.map(|(id, _node)| id.clone())
			.collect()
	}

	/// Returns the terminal-turn generation for one currently routed peer.
	pub fn terminal_turn(&self, id: &str) -> Option<u64> {
		let nodes = self.inner.nodes.lock();
		find_node(&nodes, id).map(|(_, node)| node.terminal_turn)
	}

	fn bump_generation(&self) {
		self
			.inner
			.generation
			.send_modify(|generation| *generation = generation.wrapping_add(1));
	}
}

mod route_subscription {
	use tokio::sync::broadcast::Receiver;

	use super::{Broker, RoutedEvent};

	impl Broker {
		/// Subscribes to message-id-bearing delivery events.
		pub fn subscribe_routes(&self) -> Receiver<RoutedEvent> {
			self.inner.events.subscribe()
		}
	}
}

/// Why a blocking wait ended without a matching message.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum WaitError {
	/// The requested deadline elapsed.
	#[error("IRC wait timed out")]
	Timeout,
	/// An awaited-send recipient completed a terminal turn without replying.
	#[error("awaited peer {peer} stopped without replying")]
	AwaitTargetStopped {
		/// Stable recipient identity.
		peer: Str,
	},
	/// The awaited peer died or no other live peers remain.
	#[error("IRC wait aborted because the peer is no longer live")]
	PeerDead,
	/// The owning broker was dropped.
	#[error("IRC broker is no longer available")]
	BrokerGone,
}

/// Receiver used for bounded inbox access and liveness-aware waits.
pub struct BrokerInbox {
	owner:     Str,
	state:     Arc<InboxState>,
	broker:    Weak<BrokerInner>,
	roster:    Receiver<u64>,
	lifecycle: Receiver<u64>,
}

impl BrokerInbox {
	/// Waits indefinitely for a matching delivery or liveness abort.
	pub async fn wait_for(
		&mut self,
		sender: Option<&str>,
		reply_to: Option<&str>,
	) -> Option<PeerMessage> {
		self
			.wait_for_timeout(sender, reply_to, None)
			.await
			.ok()
			.flatten()
	}

	/// Waits for a matching delivery with an optional deadline. Unmatched
	/// messages remain FIFO-ordered for later inbox reads or waits.
	pub async fn wait_for_timeout(
		&mut self,
		sender: Option<&str>,
		reply_to: Option<&str>,
		timeout: Option<Duration>,
	) -> Result<Option<PeerMessage>, WaitError> {
		self
			.wait_for_timeout_inner(sender, reply_to, None, timeout)
			.await
	}

	/// Waits for one threaded reply and settles when the recipient advances past
	/// the terminal-turn generation observed before delivery.
	pub async fn wait_for_reply_timeout(
		&mut self,
		sender: &str,
		reply_to: &str,
		observed_terminal_turn: u64,
		timeout: Option<Duration>,
	) -> Result<Option<PeerMessage>, WaitError> {
		self
			.wait_for_timeout_inner(
				Some(sender),
				Some(reply_to),
				Some(observed_terminal_turn),
				timeout,
			)
			.await
	}

	async fn wait_for_timeout_inner(
		&mut self,
		sender: Option<&str>,
		reply_to: Option<&str>,
		observed_terminal_turn: Option<u64>,
		timeout: Option<Duration>,
	) -> Result<Option<PeerMessage>, WaitError> {
		use tokio::time::{self, Instant};
		if let Some(message) = self.state.matching(sender, reply_to) {
			return Ok(Some(message));
		}
		let _registration = self.state.register_waiter(sender, reply_to);
		let deadline = timeout.map(|duration| Instant::now() + duration);
		loop {
			let notified = self.state.notify.notified();
			if let Some(message) = self.state.matching(sender, reply_to) {
				return Ok(Some(message));
			}
			let broker = self.broker.upgrade().ok_or(WaitError::BrokerGone)?;
			if observed_terminal_turn.is_some_and(|observed| {
				find_node(&broker.nodes.lock(), sender.unwrap_or_default())
					.is_some_and(|(_, node)| node.terminal_turn != observed)
			}) {
				return Err(WaitError::AwaitTargetStopped {
					peer: Str::new(sender.unwrap_or_default()),
				});
			}
			if !peer_is_live(&broker, self.owner.as_str(), sender) {
				return Err(WaitError::PeerDead);
			}
			if let Some(deadline) = deadline {
				tokio::select! {
					() = notified => {},
					changed = self.roster.changed() => {
						if changed.is_err() {
							return Err(WaitError::BrokerGone);
						}
					},
					changed = self.lifecycle.changed() => {
						if changed.is_err() {
							return Err(WaitError::BrokerGone);
						}
					},
					() = time::sleep_until(deadline) => return Err(WaitError::Timeout),
				}
			} else {
				tokio::select! {
					() = notified => {},
					changed = self.roster.changed() => {
						if changed.is_err() {
							return Err(WaitError::BrokerGone);
						}
					},
					changed = self.lifecycle.changed() => {
						if changed.is_err() {
							return Err(WaitError::BrokerGone);
						}
					},
				}
			}
		}
	}

	/// Drains or peeks at every unread message without double delivery.
	pub fn inbox(&self, peek: bool) -> Vec<PeerMessage> {
		self.state.read(peek)
	}

	/// Returns the unread FIFO count.
	pub fn unread_count(&self) -> usize {
		self.state.queue.lock().len()
	}
}

fn find_record<'a>(
	records: &'a HashMap<Str, RegistryEntry>,
	id: &str,
) -> Option<(&'a Str, &'a RegistryEntry)> {
	records.iter().find(|(candidate, entry)| {
		candidate.as_str().eq_ignore_ascii_case(id)
			|| entry.record.name.as_str().eq_ignore_ascii_case(id)
	})
}
fn record_belongs_to_root(
	records: &HashMap<Str, RegistryEntry>,
	record: &AgentRecord,
	root: &str,
) -> bool {
	if record.id.as_str().eq_ignore_ascii_case(root) {
		return true;
	}
	let mut parent = record.parent.as_deref();
	let mut remaining = records.len();
	while let Some(id) = parent {
		if id.eq_ignore_ascii_case(root) {
			return true;
		}
		if remaining == 0 {
			return false;
		}
		remaining -= 1;
		parent = find_record(records, id).and_then(|(_, entry)| entry.record.parent.as_deref());
	}
	false
}

fn find_node<'a>(
	nodes: &'a HashMap<Str, RegisteredNode>,
	id: &str,
) -> Option<(&'a Str, &'a RegisteredNode)> {
	nodes
		.iter()
		.find(|(candidate, _)| candidate.as_str().eq_ignore_ascii_case(id))
}

fn find_node_mut<'a>(
	nodes: &'a mut HashMap<Str, RegisteredNode>,
	id: &str,
) -> Option<(&'a Str, &'a mut RegisteredNode)> {
	nodes
		.iter_mut()
		.find(|(candidate, _)| candidate.as_str().eq_ignore_ascii_case(id))
}

fn is_broadcast(address: &str) -> bool {
	address == "all" || address == "project:all" || address.starts_with("session:")
}

fn matches_address(project: &str, address: &str, id: &str, node: &RegisteredNode) -> bool {
	address.eq_ignore_ascii_case(id)
		|| address.eq_ignore_ascii_case(node.name.as_str())
		|| address == "all"
		|| (address == "project:all" && !project.is_empty())
		|| address
			.strip_prefix("session:")
			.is_some_and(|session| session == node.session.as_str())
}

fn peer_is_live(broker: &BrokerInner, owner: &str, sender: Option<&str>) -> bool {
	let nodes = broker.nodes.lock();
	match sender {
		Some(sender) => find_node(&nodes, sender).is_some(),
		None => nodes.keys().any(|id| id.as_str() != owner),
	}
}

fn history_uri(registry: &AgentRegistry, id: &str) -> Option<Str> {
	registry
		.record(id)
		.filter(|(record, _)| record.transcript.is_some())
		.map(|(record, _)| omp_core::sf!("history://{}", record.id))
}

const fn class(mode: DeliveryMode) -> InterruptClass {
	match mode {
		DeliveryMode::Aside | DeliveryMode::Steer => InterruptClass::Immediate,
		DeliveryMode::NextTurn => InterruptClass::TurnBoundary,
	}
}

/// Encodes a peer message as the canonical thread item journaled by the loop.
pub fn peer_item(message: &PeerMessage) -> Item {
	let mut text = String::new();
	render_parent_irc(&mut text, message.from.as_str(), message.text.as_str());
	Item {
		seq:           0,
		created_at_ms: message.sent_ms,
		kind:          Some(item::Kind::Message(ThreadMessage {
			role:            Role::User as i32,
			parts:           vec![Part { kind: Some(part::Kind::Text(text)) }],
			synthetic:       None,
			user_initiated:  None,
			completed_at_ms: None,
			usage:           None,
		})),
		props:         None,
	}
}

/// Returns the current epoch milliseconds for caller-created messages.
pub fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

fn sanitize_activity(activity: &str) -> Str {
	let mut sanitized = String::with_capacity(activity.len().min(ACTIVITY_MAX_CHARS));
	for character in activity.chars() {
		if sanitized.chars().count() == ACTIVITY_MAX_CHARS {
			break;
		}
		if character == '\n' || character == '\r' || character.is_control() {
			if !sanitized.ends_with(' ') {
				sanitized.push(' ');
			}
		} else {
			sanitized.push(character);
		}
	}
	Str::new(sanitized.trim())
}

enum ColdScan {
	Record(AgentRecord),
	Skipped(DiscoveryDiagnosticKind),
}
fn root_session_id(path: &Path) -> Result<Str, RegistryError> {
	path
		.file_stem()
		.and_then(ffi::OsStr::to_str)
		.filter(|id| !id.is_empty())
		.map(Str::new)
		.ok_or_else(|| {
			RegistryError::Io(io::Error::new(
				io::ErrorKind::InvalidData,
				"persisted roster root has no UTF-8 session id",
			))
		})
}

fn records_for_scanned_root(records: Vec<AgentRecord>, root: &str) -> Vec<AgentRecord> {
	let parents = records
		.iter()
		.map(|record| (record.id.clone(), record.parent.clone()))
		.collect::<HashMap<_, _>>();
	records
		.into_iter()
		.filter(|record| {
			let mut parent = record.parent.as_deref();
			let mut remaining = parents.len();
			while let Some(id) = parent {
				if id.eq_ignore_ascii_case(root) {
					return true;
				}
				if remaining == 0 {
					return false;
				}
				remaining -= 1;
				parent = parents.get(id).and_then(Option::as_deref);
			}
			false
		})
		.collect()
}

fn cold_record(path: &Path) -> Result<ColdScan, RegistryError> {
	use omp_storage::transcript::{Kind, Msg, Patch, UserBlock};

	let file = fs::File::open(path)?;
	let mut reader =
		io::BufReader::new(file).take(u64::try_from(PREFIX_MAX_BYTES).unwrap_or(u64::MAX));
	let mut line = String::new();
	if reader.read_line(&mut line)? == 0 {
		return Ok(ColdScan::Skipped(DiscoveryDiagnosticKind::Incomplete));
	}
	let Ok(header) = omp_storage::transcript::read_header(line.as_bytes()) else {
		return Ok(ColdScan::Skipped(if reader.limit() == 0 {
			DiscoveryDiagnosticKind::Incomplete
		} else {
			DiscoveryDiagnosticKind::Corrupt
		}));
	};

	let mut definition = None;
	let mut parent = None;
	let mut display_name = None;
	let mut depth = 1;
	let mut model = None;
	let mut serving_model = None;
	let mut task = None;
	let mut history = AgentHistory::default();
	let mut last_activity_ms = header.created;
	let mut saw_revival = false;

	for _ in 0..PREFIX_MAX_LINES {
		line.clear();
		if reader.read_line(&mut line)? == 0 {
			break;
		}
		let Ok(event) = omp_storage::transcript::read_line(line.as_bytes()) else {
			return Ok(ColdScan::Skipped(if reader.limit() == 0 {
				DiscoveryDiagnosticKind::Incomplete
			} else {
				DiscoveryDiagnosticKind::Corrupt
			}));
		};
		last_activity_ms = last_activity_ms.max(event.ts);
		match event.kind {
			Kind::Init { agent, revival: Some(revival), .. } => {
				saw_revival = true;
				parent = agent;
				if !revival.parent_id.is_empty() {
					parent = Some(revival.parent_id);
				}
				if !revival.display_name.is_empty() {
					display_name = Some(revival.display_name);
				}
				depth = revival.depth;
				definition = Some(revival.definition);
				model = Some(revival.model_role);
				serving_model = revival.serving_model.as_ref().map(model_label);
			},
			Kind::Msg(Msg::User { content, synthetic: false, .. }) if task.is_none() => {
				task = content.into_iter().find_map(|block| match block {
					UserBlock::Text { text } if !text.trim().is_empty() => {
						Some(normalize_task_summary(text.as_str()))
					},
					UserBlock::Text { .. } | UserBlock::Image { .. } => None,
				});
			},
			Kind::Item(record) if task.is_none() => {
				task = task_summary_from_item(&record.item);
			},
			Kind::TurnInput(input) if task.is_none() => {
				task = task_summary_from_item(&input.item);
			},
			Kind::Msg(Msg::Assistant { model: served, usage, timing, .. }) => {
				history.requests = history.requests.saturating_add(1);
				history.input_tokens = history
					.input_tokens
					.saturating_add(usage.input)
					.saturating_add(usage.cache_read);
				history.output_tokens = history.output_tokens.saturating_add(usage.output);
				history.duration_ms = history.duration_ms.saturating_add(timing.duration_ms);
				serving_model = Some(model_label(&served));
			},
			Kind::Infer { model: Patch::Set(change), .. } => {
				model = Some(model_label(&change.model));
			},
			Kind::ChildLifecycle(lifecycle) if lifecycle.child_id == header.id.0 => {
				if let Some(kind) = lifecycle
					.terminal_status
					.as_deref()
					.and_then(|status| status.parse().ok())
				{
					history.terminal = Some(SubagentTerminalStatus {
						kind,
						summary: lifecycle.terminal_status.unwrap_or_default(),
						disposition: SubagentDisposition::default(),
					});
				}
			},
			Kind::EntryUndecodable(_) => {
				return Ok(ColdScan::Skipped(DiscoveryDiagnosticKind::Corrupt));
			},
			_ => {},
		}
	}
	if !saw_revival {
		return Ok(ColdScan::Skipped(DiscoveryDiagnosticKind::Incomplete));
	}

	let id = header.id.0.clone();
	let name = path
		.file_stem()
		.and_then(ffi::OsStr::to_str)
		.map_or_else(|| id.clone(), Str::new);
	let name = display_name.unwrap_or(name);
	Ok(ColdScan::Record(AgentRecord {
		id,
		name,
		kind: AgentKind::Subagent,
		parent,
		session: header.id.0,
		depth,
		status: RegistryStatus::Parked,
		activity: Default::default(),
		last_activity_ms,
		transcript: Some(path.to_path_buf()),
		definition,
		model,
		serving_model,
		task,
		history,
	}))
}

fn task_summary_from_item(item: &Item) -> Option<Str> {
	let item::Kind::Message(message) = item.kind.as_ref()? else {
		return None;
	};
	if message.role != Role::User as i32 {
		return None;
	}
	message
		.parts
		.iter()
		.find_map(|part| match part.kind.as_ref()? {
			part::Kind::Text(text) if !text.trim().is_empty() => Some(normalize_task_summary(text)),
			_ => None,
		})
}

fn model_label(model: &omp_storage::transcript::ModelRef) -> Str {
	omp_core::sf!("{}/{}", model.provider.0, model.model.0)
}

fn normalize_task_summary(task: &str) -> Str {
	let mut summary = String::with_capacity(task.len().min(TASK_SUMMARY_MAX_CHARS));
	for character in task.chars().take(TASK_SUMMARY_MAX_CHARS) {
		if character == '\n' || character == '\r' || character.is_control() {
			if !summary.ends_with(' ') {
				summary.push(' ');
			}
		} else {
			summary.push(character);
		}
	}
	Str::new(summary.trim())
}

fn bounded_json_query(bytes: &[u8], query: &str) -> Result<Vec<u8>, RegistryError> {
	use hifijson::token::Lex as _;
	use jaq_core::{
		Ctx, RcIter,
		compile::Compiler,
		load::{Arena, File, Loader},
	};
	use jaq_json::Val;

	if bytes.len() > QUERY_MAX_BYTES
		|| query.chars().count() > QUERY_MAX_CHARS
		|| !query_is_safe(query)
	{
		return Err(RegistryError::QueryLimit);
	}
	serde_json::from_slice::<serde_json::Value>(bytes).map_err(RegistryError::InvalidJson)?;
	let arena = Arena::default();
	let loader = Loader::new(jaq_std::defs().chain(jaq_json::defs()));
	let modules = loader
		.load(&arena, File { path: (), code: query })
		.map_err(|_| RegistryError::InvalidQuery)?;
	let filter = Compiler::default()
		.with_funs(jaq_std::funs().chain(jaq_json::funs()))
		.compile(modules)
		.map_err(|_| RegistryError::InvalidQuery)?;

	let mut lexer = hifijson::SliceLexer::new(bytes);
	let token = lexer
		.ws_token()
		.expect("serde-validated JSON has one token");
	let input = Val::parse(token, &mut lexer).map_err(|_| RegistryError::QueryLimit)?;
	let empty = Box::new(core::iter::empty()) as Box<dyn Iterator<Item = Result<Val, String>>>;
	let inputs = RcIter::new(empty);
	let ctx = Ctx::new(Vec::new(), &inputs);
	let started = Instant::now();
	let mut values = filter.run((ctx, input));
	let first = values
		.next()
		.ok_or(RegistryError::QueryEmpty)?
		.map_err(|_| RegistryError::InvalidQuery)?;
	if started.elapsed() > QUERY_MAX_DURATION {
		return Err(RegistryError::QueryLimit);
	}
	if values.next().is_some() {
		return Err(RegistryError::QueryMultiple);
	}
	let output = first.to_string().into_bytes();
	if output.len() > QUERY_MAX_BYTES || started.elapsed() > QUERY_MAX_DURATION {
		return Err(RegistryError::QueryLimit);
	}
	let value =
		serde_json::from_slice::<serde_json::Value>(&output).map_err(RegistryError::InvalidJson)?;
	if json_depth(&value, 0) > QUERY_MAX_DEPTH {
		return Err(RegistryError::QueryLimit);
	}
	Ok(output)
}

fn query_is_safe(query: &str) -> bool {
	const DENIED: &[&str] = &[
		"debug",
		"env",
		"foreach",
		"gsub",
		"halt",
		"halt_error",
		"input",
		"inputs",
		"match",
		"range",
		"recurse",
		"reduce",
		"repeat",
		"scan",
		"stderr",
		"sub",
		"test",
		"until",
		"while",
	];
	let mut quoted = false;
	let mut escaped = false;
	let mut field = false;
	let mut previous = None;
	let mut token = String::new();
	for character in query.chars().chain(core::iter::once(' ')) {
		if quoted {
			if escaped {
				escaped = false;
			} else if character == '\\' {
				escaped = true;
			} else if character == '"' {
				quoted = false;
			}
			continue;
		}
		if character == '"' {
			quoted = true;
			token.clear();
		} else if character.is_ascii_alphanumeric() || character == '_' {
			if token.is_empty() {
				field = previous == Some('.');
			}
			token.push(character);
		} else {
			if !field && DENIED.contains(&token.as_str()) {
				return false;
			}
			token.clear();
			if !character.is_whitespace() {
				previous = Some(character);
			}
		}
	}
	!quoted
}

fn json_depth(value: &serde_json::Value, depth: usize) -> usize {
	match value {
		serde_json::Value::Array(values) => values
			.iter()
			.map(|value| json_depth(value, depth.saturating_add(1)))
			.max()
			.unwrap_or(depth),
		serde_json::Value::Object(values) => values
			.values()
			.map(|value| json_depth(value, depth.saturating_add(1)))
			.max()
			.unwrap_or(depth),
		_ => depth,
	}
}

fn valid_artifact_component(value: &str) -> bool {
	!value.is_empty()
		&& value
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
	use std::fs;

	use omp_core::sf;

	use super::*;
	use crate::{AgentTree, Budget, DrainPoint, Mailbox};

	fn node(tree: &AgentTree, id: &str, name: &str) -> Arc<AgentNode> {
		tree
			.register(
				id.into(),
				name.into(),
				AgentKind::Subagent,
				None,
				"session".into(),
				Budget::default(),
			)
			.expect("node")
	}

	fn message(from: &str, to: &str, index: usize) -> PeerMessage {
		PeerMessage {
			id:            sf!("message-{index}"),
			from:          from.into(),
			to:            to.into(),
			text:          sf!("body-{index}"),
			mode:          DeliveryMode::Aside,
			reply_to:      None,
			sent_ms:       now_ms(),
			session_id:    "session".into(),
			expects_reply: false,
		}
	}
	#[test]
	fn caller_root_wins_agent_and_history_id_collisions() {
		let registry = AgentRegistry::new();
		let tree = AgentTree::standard(2);
		let worker = node(&tree, "Worker", "Worker");
		registry
			.register_node(&worker, RegistryStatus::Parked, None)
			.expect("register global worker");
		let root = tempfile::tempdir().expect("caller root");
		let preferred_artifacts = root.path().join("caller");
		fs::create_dir_all(&preferred_artifacts).expect("preferred artifacts");
		let preferred_history = root.path().join("sessions");
		fs::create_dir_all(&preferred_history).expect("preferred histories");
		let global_output = root.path().join("global.md");
		let global_history = root.path().join("global.jsonl");
		fs::write(&global_output, b"GLOBAL OUTPUT").expect("global output");
		fs::write(&global_history, b"GLOBAL HISTORY").expect("global history");
		fs::write(preferred_artifacts.join("Worker.md"), b"CALLER OUTPUT").expect("caller output");
		fs::write(preferred_history.join("Worker.jsonl"), b"CALLER HISTORY").expect("caller history");
		registry
			.set_history("Worker", Some(global_history), None, None, AgentHistory {
				output_path: Some(global_output),
				..AgentHistory::default()
			})
			.expect("global history");

		assert_eq!(
			registry
				.resolve_agent_from("Worker", &preferred_artifacts)
				.expect("preferred output"),
			b"CALLER OUTPUT"
		);
		assert_eq!(
			registry
				.resolve_history_from("Worker", &preferred_history)
				.expect("preferred transcript"),
			b"CALLER HISTORY"
		);
	}

	#[test]
	fn registry_cas_ttl_and_parking_preserve_identity() {
		let registry = AgentRegistry::new();
		let tree = AgentTree::standard(2);
		let node = node(&tree, "worker", "Worker");
		let revision = registry
			.register_node(&node, RegistryStatus::Idle, None)
			.expect("register");
		assert!(matches!(
			registry.set_status("worker", Some(revision + 1), RegistryStatus::Running),
			Err(RegistryError::Revision { .. })
		));
		let parked = registry.park_expired(now_ms() + 10_000, Duration::from_secs(1));
		assert_eq!(parked.len(), 1);
		assert_eq!(parked[0].record.status, RegistryStatus::Parked);
		registry
			.register_node(&node, RegistryStatus::Idle, None)
			.expect("revive parked identity");
		assert_eq!(
			registry
				.record("Worker")
				.expect("alias remains reserved")
				.0
				.id,
			node.id
		);
	}

	#[test]
	fn main_succession_reclaims_alias_and_routing() {
		let registry = AgentRegistry::new();
		let broker = Broker::with_registry("project".into(), registry.clone());
		let tree = AgentTree::standard(2);
		let first = tree
			.register(
				"session-a".into(),
				"Main".into(),
				AgentKind::Main,
				None,
				"session-a".into(),
				Budget::default(),
			)
			.expect("first main");
		let first_mailbox = Mailbox::new();
		let first_inbox = broker
			.register(&first, first_mailbox.sender())
			.expect("register first main");
		let second = tree
			.register(
				"session-b".into(),
				"Main".into(),
				AgentKind::Main,
				None,
				"session-b".into(),
				Budget::default(),
			)
			.expect("second main");
		let second_mailbox = Mailbox::new();
		let second_inbox = broker
			.register(&second, second_mailbox.sender())
			.expect("superseding main reclaims the alias");
		assert!(registry.record("session-a").is_none());
		assert_eq!(
			registry
				.record("Main")
				.expect("alias follows successor")
				.0
				.id
				.as_str(),
			"session-b"
		);
		assert_eq!(
			broker
				.send(message("peer", "Main", 0))
				.expect("route")
				.as_slice(),
			[Receipt::Woken]
		);
		assert_eq!(second_inbox.unread_count(), 1);
		assert_eq!(first_inbox.unread_count(), 0);
	}

	#[test]
	fn mailbox_is_fifo_capped_and_receipts_are_fire_and_forget() {
		let registry = AgentRegistry::new();
		let broker = Broker::with_registry("project".into(), registry);
		let tree = AgentTree::standard(2);
		let worker = node(&tree, "worker", "Worker");
		let mailbox = Mailbox::new();
		let inbox = broker
			.register(&worker, mailbox.sender())
			.expect("register");
		for index in 0..105 {
			assert_eq!(
				broker
					.send(message("Main", "worker", index))
					.expect("send")
					.as_slice(),
				[Receipt::Woken]
			);
		}
		assert_eq!(inbox.unread_count(), MAILBOX_CAPACITY);
		let messages = inbox.inbox(false);
		assert_eq!(messages.first().expect("first retained").id.as_str(), "message-5");
		assert_eq!(messages.last().expect("last retained").id.as_str(), "message-104");
		assert_eq!(inbox.unread_count(), 0);
	}

	#[tokio::test]
	async fn wait_preserves_unmatched_messages_and_aborts_on_peer_death() {
		let registry = AgentRegistry::new();
		let broker = Broker::with_registry("project".into(), registry.clone());
		let tree = AgentTree::standard(2);
		let owner = node(&tree, "owner", "Owner");
		let peer = node(&tree, "peer", "Peer");
		let owner_mailbox = Mailbox::new();
		let peer_mailbox = Mailbox::new();
		let mut inbox = broker
			.register(&owner, owner_mailbox.sender())
			.expect("owner");
		broker.register(&peer, peer_mailbox.sender()).expect("peer");
		broker
			.send(message("other", "owner", 0))
			.expect("unmatched");
		broker.send(message("peer", "owner", 1)).expect("matched");
		let matched = inbox
			.wait_for_timeout(Some("peer"), None, Some(Duration::from_secs(1)))
			.await
			.expect("wait")
			.expect("message");
		assert_eq!(matched.id.as_str(), "message-1");
		assert_eq!(inbox.inbox(true)[0].id.as_str(), "message-0");
		broker.unregister("peer");
		assert_eq!(
			inbox
				.wait_for_timeout(Some("peer"), None, Some(Duration::from_secs(1)))
				.await,
			Err(WaitError::PeerDead)
		);
	}
	#[tokio::test]
	async fn awaited_reply_settles_when_recipient_turn_ends_without_reply() {
		let registry = AgentRegistry::new();
		let broker = Broker::with_registry("project".into(), registry);
		let tree = AgentTree::standard(2);
		let owner = node(&tree, "owner", "Owner");
		let peer = node(&tree, "peer", "Peer");
		let owner_mailbox = Mailbox::new();
		let mut peer_mailbox = Mailbox::new();
		let mut inbox = broker
			.register(&owner, owner_mailbox.sender())
			.expect("owner");
		broker.register(&peer, peer_mailbox.sender()).expect("peer");
		let observed = broker.terminal_turn("peer").expect("terminal generation");
		let mut outbound = message("owner", "peer", 7);
		outbound.expects_reply = true;
		let reply_to = outbound.id.clone();
		broker.route(outbound).expect("route awaited message");
		assert_eq!(
			peer_mailbox.drain(DrainPoint::Idle, false).len(),
			1,
			"recipient consumed the awaited message"
		);
		assert_eq!(
			broker.finish_turn("peer").expect("settle peer turn"),
			TurnEndDisposition::Terminal
		);

		let result = tokio::time::timeout(
			Duration::from_millis(100),
			inbox.wait_for_reply_timeout(
				"peer",
				reply_to.as_str(),
				observed,
				Some(Duration::from_secs(30)),
			),
		)
		.await
		.expect("terminal monitor settles promptly");
		assert_eq!(result, Err(WaitError::AwaitTargetStopped { peer: "peer".into() }));
	}

	#[tokio::test]
	async fn queued_irc_continuation_does_not_emit_terminal_turn_end() {
		let registry = AgentRegistry::new();
		let broker = Broker::with_registry("project".into(), registry);
		let tree = AgentTree::standard(2);
		let owner = node(&tree, "owner", "Owner");
		let peer = node(&tree, "peer", "Peer");
		let owner_mailbox = Mailbox::new();
		let peer_mailbox = Mailbox::new();
		let mut inbox = broker
			.register(&owner, owner_mailbox.sender())
			.expect("owner");
		broker.register(&peer, peer_mailbox.sender()).expect("peer");
		let observed = broker.terminal_turn("peer").expect("terminal generation");
		let outbound = message("owner", "peer", 8);
		let reply_to = outbound.id.clone();
		broker.route(outbound).expect("queue tail IRC");
		assert_eq!(
			broker.finish_turn("peer").expect("settle peer turn"),
			TurnEndDisposition::ContinuationPending
		);
		broker
			.set_idle("peer", true)
			.expect("mid-turn continuation idle flip");
		assert_eq!(broker.terminal_turn("peer"), Some(observed));

		assert!(
			tokio::time::timeout(
				Duration::from_millis(20),
				inbox.wait_for_reply_timeout(
					"peer",
					reply_to.as_str(),
					observed,
					Some(Duration::from_secs(30)),
				),
			)
			.await
			.is_err(),
			"non-terminal continuation must keep the reply waiter armed"
		);
	}

	#[test]
	fn persisted_roster_restore_latches_per_root_file() {
		let scratch = tempfile::tempdir().expect("temporary directory");
		let transcripts = scratch.path().join("eval-agents");
		fs::create_dir(&transcripts).expect("transcript directory");
		let first = scratch.path().join("first.jsonl");
		let second = scratch.path().join("second.jsonl");
		fs::write(&first, b"").expect("first root");
		fs::write(&second, b"").expect("second root");
		let registry = AgentRegistry::new();

		assert_eq!(registry.restore_transcripts_once(&first, &transcripts), 0);
		assert_eq!(registry.restore_transcripts_once(&first, &transcripts), 0);
		assert_eq!(registry.restore_transcripts_once(&second, &transcripts), 0);
		assert_eq!(registry.inner.restored_roots.lock().len(), 2);
	}

	#[test]
	fn persisted_roster_latch_refreshes_after_owned_reference_is_superseded() {
		let scratch = tempfile::tempdir().expect("temporary directory");
		let transcripts = scratch.path().join("eval-agents");
		fs::create_dir(&transcripts).expect("transcript directory");
		let root_file = scratch.path().join("root.jsonl");
		fs::write(&root_file, b"").expect("root transcript");
		let first_transcript = transcripts.join("first.jsonl");
		let replacement_transcript = transcripts.join("replacement.jsonl");
		let registry = AgentRegistry::new();
		let record = |transcript: PathBuf| AgentRecord {
			id:               "shared".into(),
			name:             "Shared".into(),
			kind:             AgentKind::Subagent,
			parent:           Some("root".into()),
			session:          "shared".into(),
			depth:            1,
			status:           RegistryStatus::Parked,
			activity:         Str::default(),
			last_activity_ms: 0,
			transcript:       Some(transcript),
			definition:       None,
			model:            None,
			serving_model:    None,
			task:             None,
			history:          AgentHistory::default(),
		};
		let revision = registry
			.compare_and_register(record(first_transcript.clone()), None)
			.expect("first parked ref");
		let root = fs::canonicalize(&root_file).expect("canonical root");
		registry
			.inner
			.restored_roots
			.lock()
			.insert(root.clone(), PersistedRosterLatch {
				owned: HashMap::from([("shared".into(), first_transcript)]),
			});
		registry
			.compare_and_register(record(replacement_transcript), Some(revision))
			.expect("supersede parked ref");

		assert_eq!(registry.restore_transcripts_once(&root_file, &transcripts), 0);
		assert!(
			registry
				.inner
				.restored_roots
				.lock()
				.get(&root)
				.expect("refreshed latch")
				.owned
				.is_empty(),
			"stale owned identity was discarded by the refresh"
		);
	}

	#[test]
	fn persisted_roster_scan_failure_does_not_poison_retry() {
		let scratch = tempfile::tempdir().expect("temporary directory");
		let root_file = scratch.path().join("root.jsonl");
		let transcripts = scratch.path().join("eval-agents");
		fs::write(&root_file, b"").expect("root transcript");
		let root = fs::canonicalize(&root_file).expect("canonical root");
		let registry = AgentRegistry::new();

		assert_eq!(registry.restore_transcripts_once(&root_file, &transcripts), 0);
		assert!(!registry.inner.restored_roots.lock().contains_key(&root));
		fs::create_dir(&transcripts).expect("transcript directory");
		assert_eq!(registry.restore_transcripts_once(&root_file, &transcripts), 0);
		assert!(registry.inner.restored_roots.lock().contains_key(&root));
	}
}
