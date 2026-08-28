//! Write-time SQLite index for session discovery and usage accounting.
//!
//! The journal remains durable truth. This module deliberately does not scan
//! journals on normal reads: each journal owner records its committed appends
//! through SQLite's transaction-serialized WAL writer path. Foreign and legacy
//! journals enter through the explicitly named [`SessionIndex::repair`] API.

use std::{
	collections::BTreeMap,
	error,
	fmt::{self, Display},
	path::Path,
	str::FromStr,
	sync::Arc,
	time::Duration,
};

use omp_core::Str;
use omp_proto::{
	inference::v1 as pb,
	prost::Message as _,
	thread::v1::{self as thread_pb, item},
};
use parking_lot::{Mutex, RwLock};
use rusqlite::{
	Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
	params_from_iter, types, types::Value,
};
use smallvec::SmallVec;
use strum::{Display, EnumString, IntoStaticStr};
use thiserror::Error;

use crate::transcript::{SessionId, TitleSource};

const SCHEMA_VERSION: i64 = 5;

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS index_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL
);
INSERT OR IGNORE INTO index_meta(singleton, schema_version) VALUES (1, 5);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    title TEXT,
    title_source TEXT,
    cwd TEXT NOT NULL,
    project TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    status TEXT NOT NULL,
    kind TEXT NOT NULL,
    parent TEXT,
    parent_checkpoint INTEGER,
    entries INTEGER NOT NULL DEFAULT 0,
    turns INTEGER NOT NULL DEFAULT 0,
    remote INTEGER NOT NULL,
    journal_watermark INTEGER NOT NULL,
    last_event_index INTEGER,
    repair_watermark INTEGER NOT NULL DEFAULT 0,
    serving_provider TEXT,
    serving_model TEXT,
    context_anchor INTEGER,
    context_revision INTEGER NOT NULL DEFAULT 0,
    compaction_epoch INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS sessions_recent ON sessions(project, updated_ms DESC, id);
CREATE INDEX IF NOT EXISTS sessions_parent ON sessions(parent);
CREATE TABLE IF NOT EXISTS session_create_receipts (
    idempotency_key TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS session_entry_kinds (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    PRIMARY KEY(session_id, kind)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS receipts (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    event_index INTEGER NOT NULL,
    journal_watermark INTEGER NOT NULL,
    ts_ms INTEGER NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    failed INTEGER NOT NULL,
    duration_ms INTEGER,
    usage BLOB NOT NULL,
    cost BLOB NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    accuracy INTEGER NOT NULL,
    total_tokens INTEGER,
    context_tokens INTEGER,
    orchestration_input_tokens INTEGER,
    orchestration_cache_read_tokens INTEGER,
    orchestration_output_tokens INTEGER,
    premium_requests INTEGER,
    reasoning_tokens INTEGER,
    cache_ephemeral_5m_tokens INTEGER,
    cache_ephemeral_1h_tokens INTEGER,
    web_search_requests INTEGER,
    web_fetch_requests INTEGER,
    cost_nanos_usd INTEGER NOT NULL,
    cost_estimated INTEGER NOT NULL,
    input_nanos_usd INTEGER,
    output_nanos_usd INTEGER,
    cache_read_nanos_usd INTEGER,
    cache_write_nanos_usd INTEGER,
    PRIMARY KEY(session_id, event_index)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS receipts_time ON receipts(ts_ms, session_id);
CREATE INDEX IF NOT EXISTS receipts_model ON receipts(provider, model, ts_ms);

CREATE TABLE IF NOT EXISTS item_outcomes (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    event_index INTEGER NOT NULL,
    user_messages INTEGER NOT NULL DEFAULT 0,
    assistant_messages INTEGER NOT NULL DEFAULT 0,
    system_messages INTEGER NOT NULL DEFAULT 0,
    tool_calls INTEGER NOT NULL DEFAULT 0,
    tool_results INTEGER NOT NULL DEFAULT 0,
    tool_errors INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(session_id, event_index)
) WITHOUT ROWID;

CREATE VIRTUAL TABLE IF NOT EXISTS prompts_fts USING fts5(
    session_id UNINDEXED,
    event_index UNINDEXED,
    prompt,
    ts_ms UNINDEXED,
    tokenize = 'unicode61'
);

CREATE TABLE IF NOT EXISTS model_performance (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    event_index INTEGER NOT NULL,
    ts_ms INTEGER NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    ttft_ms INTEGER,
    duration_ms INTEGER,
    output_tokens INTEGER NOT NULL,
    PRIMARY KEY(session_id, event_index)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS model_performance_model
    ON model_performance(provider, model, ts_ms);

CREATE TABLE IF NOT EXISTS command_usage (
    name TEXT PRIMARY KEY,
    count INTEGER NOT NULL DEFAULT 0,
    last_used_at INTEGER NOT NULL
) WITHOUT ROWID;
";

/// Whether an index is writable authority or a stale offline projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexAuthority {
	/// The index belongs to the Agent Core and is authoritative.
	Authoritative,
	/// A thin-client snapshot opened read-only for offline listing.
	OfflineCache {
		/// Epoch-millisecond time at which the cache was copied from authority.
		cached_at_ms: u64,
	},
}

/// Session terminal disposition stored by the index.
#[derive(Debug, Clone, Copy, Display, EnumString, IntoStaticStr, PartialEq, Eq)]
#[strum(serialize_all = "snake_case")]
pub enum SessionStatus {
	/// The most recent turn has a durable receipt.
	Complete,
	/// A person interrupted the most recent turn.
	Interrupted,
	/// The most recent turn was durably aborted.
	Aborted,
	/// The most recent request failed.
	Error,
	/// A turn has started without a terminal event.
	Pending,
	/// No terminal disposition can be derived.
	Unknown,
}

/// Session role used by listing and accounting filters.
#[derive(Debug, Clone, Copy, Display, EnumString, IntoStaticStr, PartialEq, Eq)]
#[strum(serialize_all = "snake_case")]
pub enum SessionKind {
	/// A user-facing interactive session.
	Interactive,
	/// A delegated agent session.
	Subagent,
	/// An advisor session.
	Advisor,
}

/// One session's immutable creation metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession<'a> {
	/// Stable transcript identifier.
	pub id:         &'a SessionId,
	/// Environment-qualified working-directory display text.
	pub cwd:        &'a str,
	/// Normalized project root.
	pub project:    &'a str,
	/// Epoch-millisecond creation time.
	pub created_ms: u64,
	/// Session role.
	pub kind:       SessionKind,
	/// Immediate lineage parent, when any.
	pub parent:     Option<&'a SessionId>,
	/// Whether the session's environment is remote.
	pub remote:     bool,
}

/// Committed physical and byte position returned by the journal closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalPosition {
	/// Zero-based durable transcript event index.
	pub event_index:    u64,
	/// Complete-line byte watermark after this event is appended.
	pub byte_watermark: u64,
}

/// Index projection attached to one journal event.
#[derive(Debug, Clone, Copy)]
pub enum EventProjection<'a> {
	/// The event only affects recency, entry count, and kind membership.
	Plain,
	/// Private prompt text admitted to the local FTS projection.
	Prompt {
		/// Prompt text indexed for owner-local search.
		text: &'a str,
	},
	/// Monotonic context position attached to a durable boundary.
	Context {
		/// Prompt anchor event, when one exists.
		anchor:   Option<u64>,
		/// Monotonic context revision.
		revision: u64,
		/// Monotonic compaction epoch.
		epoch:    u64,
	},
	/// The event changes the session title.
	Title {
		/// Assigned title.
		title:  &'a str,
		/// Source that assigned the title.
		source: TitleSource,
	},
	/// One canonical thread item used for message and tool-outcome counts.
	ThreadItem {
		/// Durable item, counted exactly once at its physical journal event.
		item:    &'a thread_pb::Item,
		/// Owner-private user prompt text admitted to FTS, when this is a user
		/// message.
		prompt:  Option<&'a str>,
		/// Monotonic context position when this item is a durable message
		/// boundary.
		context: Option<ContextPosition>,
	},
	/// The event is an inference receipt with canonical rich accounting.
	TurnReceipt {
		/// Complete gateway outcome; its 13-field usage is stored losslessly.
		outcome: &'a pb::Outcome,
		/// Whether the request is accounted as failed.
		failed:  bool,
	},
	/// The event changes only terminal disposition.
	Status(SessionStatus),
	/// The event fixes the exact durable parent checkpoint for this child.
	Fork {
		/// Immediate parent session.
		parent: &'a SessionId,
		/// Parent event checkpoint inherited by the child.
		at:     Option<u64>,
	},
}

/// One event about to be durably appended and indexed.
#[derive(Debug, Clone, Copy)]
pub struct IndexedEvent<'a> {
	/// Session receiving the event.
	pub session:    &'a SessionId,
	/// Epoch-millisecond event timestamp.
	pub ts_ms:      u64,
	/// Declared journal kind name, used by `contains_kind` filtering.
	pub kind:       &'a str,
	/// Query projection for the event.
	pub projection: EventProjection<'a>,
}

/// Result of a write closure and its index update.
#[derive(Debug)]
pub enum IndexedWriteError<T, E> {
	/// The index rejected the operation before calling the journal closure.
	IndexBeforeJournal(Error),
	/// The journal write failed; no index transaction was committed.
	Journal(E),
	/// The journal committed but its rebuildable index did not.
	IndexAfterJournal {
		/// Value proving what the journal durably committed.
		written:  T,
		/// Committed journal position, absent only for the line-zero header.
		position: Option<JournalPosition>,
		/// Index failure that prevented publication.
		source:   Error,
	},
}
/// Idempotent outcome of one atomic seeded-session publication.
#[derive(Debug)]
pub enum CreateSessionWrite<T> {
	/// This request published the supplied staged journal.
	Created(T),
	/// The same logical request already published this session.
	Existing(SessionId),
}

impl<T, E: Display> Display for IndexedWriteError<T, E> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::IndexBeforeJournal(error) => {
				write!(formatter, "sessions index rejected write: {error}")
			},
			Self::Journal(error) => write!(formatter, "journal write failed: {error}"),
			Self::IndexAfterJournal { source, .. } => {
				write!(formatter, "journal committed but sessions index update failed: {source}")
			},
		}
	}
}

impl<T, E> error::Error for IndexedWriteError<T, E>
where
	T: fmt::Debug,
	E: error::Error + 'static,
{
	fn source(&self) -> Option<&(dyn error::Error + 'static)> {
		match self {
			Self::IndexBeforeJournal(error) => Some(error),
			Self::Journal(error) => Some(error),
			Self::IndexAfterJournal { source, .. } => Some(source),
		}
	}
}

/// Sessions-index failure.
#[derive(Debug, Error)]
pub enum Error {
	/// SQLite rejected an operation, including a write that exceeded the busy
	/// timeout.
	#[error("sessions index database operation failed")]
	Database(#[from] rusqlite::Error),
	/// A write was attempted through the authority's read-only query handle.
	#[error("authoritative sessions-index reader is read-only")]
	ReadOnlyAuthority,
	/// A write was attempted through an offline thin-client cache.
	#[error("offline sessions cache is read-only")]
	ReadOnlyCache,
	/// A durable turn receipt omitted canonical inference usage.
	#[error("turn receipt is missing canonical inference usage")]
	MissingUsage,
	/// A journal event did not advance both physical and byte watermarks.
	#[error("journal watermark did not advance monotonically")]
	NonMonotonicWatermark,
	/// A row contains an unknown stored vocabulary value.
	#[error("sessions index contains unknown {field} value `{value}`")]
	UnknownVocabulary {
		/// Column whose value was invalid.
		field: &'static str,
		/// Invalid stored value.
		value: Str,
	},
	/// A stored canonical protobuf value could not be decoded.
	#[error("sessions index contains invalid canonical accounting bytes")]
	InvalidAccounting(#[from] omp_proto::prost::DecodeError),
	/// A Rust counter does not fit SQLite's signed integer domain.
	#[error("{field} exceeds the SQLite integer range")]
	IntegerRange {
		/// Counter being converted.
		field: &'static str,
	},
	/// The retained lineage target is also among the sessions being archived.
	#[error("retained lineage target is also marked for archive")]
	RetainedSessionArchived {
		/// Conflicting session identifier.
		session: SessionId,
	},
	/// A lineage maintenance request named a session absent from the index.
	#[error("lineage maintenance session is absent from the index")]
	MissingMaintenanceSession {
		/// Missing session identifier.
		session: SessionId,
	},
	/// Cross-authority relocation requires indexes backed by SQLite files.
	#[error("session relocation requires file-backed source and destination indexes")]
	RelocationRequiresFileBackedIndexes,
}

/// Filters applied directly by `sessions.list` SQL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFilter {
	/// Project root, or `None` for all projects.
	pub project:       Option<Str>,
	/// Inclusive lower activity bound.
	pub since_ms:      Option<u64>,
	/// Inclusive upper activity bound.
	pub until_ms:      Option<u64>,
	/// Allowed terminal dispositions; empty means all.
	pub statuses:      SmallVec<SessionStatus, 3>,
	/// Allowed roles; empty means interactive only.
	pub kinds:         SmallVec<SessionKind, 3>,
	/// Required journal kind membership.
	pub contains_kind: Option<Str>,
	/// Maximum rows returned.
	pub limit:         u32,
}

/// Indexed metadata returned by `sessions.list`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
	/// Stable session identifier.
	pub id:                SessionId,
	/// Most recently assigned title.
	pub title:             Option<Str>,
	/// Source of the most recently assigned title.
	pub title_source:      Option<TitleSource>,
	/// Environment-qualified working-directory display text.
	pub cwd:               Str,
	/// Normalized project root.
	pub project:           Str,
	/// Creation time in epoch milliseconds.
	pub created_ms:        u64,
	/// Last indexed append time in epoch milliseconds.
	pub updated_ms:        u64,
	/// Terminal disposition.
	pub status:            SessionStatus,
	/// Session role.
	pub kind:              SessionKind,
	/// Immediate lineage parent.
	pub parent:            Option<SessionId>,
	/// Durable event count.
	pub entries:           u64,
	/// Completed receipt count.
	pub turns:             u64,
	/// Rolled-up canonical inference usage.
	pub usage:             pb::Usage,
	/// Rolled-up canonical cost.
	pub cost:              pb::Cost,
	/// Distinct `provider/model` serving models in lexical order.
	pub models:            SmallVec<Str, 4>,
	/// Complete-line journal byte watermark.
	pub journal_watermark: u64,
	/// Most recent durable event index.
	pub last_event_index:  Option<u64>,
	/// Whether the session environment is remote.
	pub remote:            bool,
}

/// A listing response with explicit authority/cache freshness labeling.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPage {
	/// Rows ordered by descending activity.
	pub sessions:  Vec<SessionInfo>,
	/// Source authority of these rows.
	pub authority: IndexAuthority,
}
/// One durable parent edge in a session lineage, ordered root first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLink {
	/// Session at this point in the lineage.
	pub id:     SessionId,
	/// Immediate parent, absent for a lineage root.
	pub parent: Option<SessionId>,
	/// Parent checkpoint inherited by this child when indexed, when known.
	pub at:     Option<u64>,
}

/// SQL grouping dimension for usage queries.
#[derive(Debug, Clone, Copy, Display, EnumString, IntoStaticStr, PartialEq, Eq, Hash)]
#[strum(serialize_all = "snake_case")]
pub enum UsageDimension {
	/// Normalized project root.
	Project,
	/// Serving provider.
	Provider,
	/// Fully qualified `provider/model` serving model.
	Model,
	/// Stable session identifier.
	SessionId,
	/// Interactive, subagent, or advisor role.
	SessionKind,
}

/// Time-bucket width for usage series.
#[derive(Debug, Clone, Copy, Display, EnumString, IntoStaticStr, PartialEq, Eq)]
#[strum(serialize_all = "snake_case")]
pub enum UsageBucketWidth {
	/// Do not group by time.
	None,
	/// One UTC hour.
	Hour,
	/// One UTC day.
	Day,
	/// Seven UTC days from the Unix epoch.
	Week,
	/// One UTC calendar month.
	Month,
}

/// Usage query executed against receipt rows, never transcript files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageQuery {
	/// Inclusive lower receipt-time bound.
	pub since_ms:          Option<u64>,
	/// Inclusive upper receipt-time bound.
	pub until_ms:          Option<u64>,
	/// Optional exact session scope.
	pub session:           Option<SessionId>,
	/// Optional exact project scope.
	pub project:           Option<Str>,
	/// Ordered grouping dimensions.
	pub group_by:          SmallVec<UsageDimension, 3>,
	/// Optional time grouping.
	pub bucket:            UsageBucketWidth,
	/// Whether subagent rows are included.
	pub include_subagents: bool,
}

impl Default for UsageQuery {
	fn default() -> Self {
		Self {
			since_ms:          None,
			until_ms:          None,
			session:           None,
			project:           None,
			group_by:          SmallVec::from_buf([UsageDimension::Model]),
			bucket:            UsageBucketWidth::None,
			include_subagents: true,
		}
	}
}

/// One SQL `GROUP BY` accounting row.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageBucket {
	/// Ordered grouping values corresponding to the query dimensions.
	pub key:         SmallVec<(UsageDimension, Str), 3>,
	/// UTC bucket start, absent when no time bucket was requested.
	pub start_ms:    Option<u64>,
	/// Canonical rich usage aggregate.
	pub usage:       pb::Usage,
	/// Canonical cost aggregate.
	pub cost:        pb::Cost,
	/// Inference receipt count.
	pub requests:    u64,
	/// Failed request count.
	pub errors:      u64,
	/// Summed provider duration.
	pub duration_ms: u64,
	/// Number of distinct contributing sessions.
	pub sessions:    u64,
}

/// Exact accounting stored for one receipt.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiptAccounting {
	/// Canonical 13-field inference usage.
	pub usage: pb::Usage,
	/// Canonical nano-USD cost.
	pub cost:  pb::Cost,
}

/// Aggregate message, tool, request, token, and cost statistics for one session
/// tree.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatistics {
	/// User message items.
	pub user_messages:      u64,
	/// Assistant message items.
	pub assistant_messages: u64,
	/// System message items.
	pub system_messages:    u64,
	/// Tool call items.
	pub tool_calls:         u64,
	/// Settled tool result items.
	pub tool_results:       u64,
	/// Tool results carrying an error outcome.
	pub tool_errors:        u64,
	/// Canonical rich usage across every selected receipt.
	pub usage:              pb::Usage,
	/// Canonical cost, including every component field.
	pub cost:               pb::Cost,
	/// Durable inference receipt count.
	pub requests:           u64,
	/// Failed inference receipt count.
	pub request_errors:     u64,
	/// Number of distinct sessions included in the rollup.
	pub sessions:           u64,
}

/// One owner-local prompt search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHit {
	/// Session containing the prompt.
	pub session:     SessionId,
	/// Stable physical event index.
	pub event_index: u64,
	/// Exact indexed prompt text.
	pub prompt:      Str,
}

/// One unique prompt from the owner-local history projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHistoryEntry {
	/// Exact prompt text of the most recent submission.
	pub prompt: Str,
	/// Epoch-millisecond submission time of the most recent occurrence, when
	/// recorded.
	pub ts_ms:  Option<u64>,
}

/// Monotonic context position projected from durable boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPosition {
	/// Prompt anchor event, when set.
	pub anchor:   Option<u64>,
	/// Context revision.
	pub revision: u64,
	/// Compaction epoch.
	pub epoch:    u64,
}

/// One settled serving-model performance sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPerformanceSample {
	/// Session contributing the sample.
	pub session:       SessionId,
	/// Physical receipt event index.
	pub event_index:   u64,
	/// Epoch-millisecond settlement time.
	pub ts_ms:         u64,
	/// Serving provider.
	pub provider:      Str,
	/// Serving model.
	pub model:         Str,
	/// Time to first token.
	pub ttft_ms:       Option<u64>,
	/// Total request duration.
	pub duration_ms:   Option<u64>,
	/// Output token count.
	pub output_tokens: u64,
}

/// Bounded recency-decayed model performance projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPerformanceEstimate {
	/// Weighted time to first token.
	pub ttft_ms:       Option<u64>,
	/// Weighted total request duration.
	pub duration_ms:   Option<u64>,
	/// Weighted output token count.
	pub output_tokens: u64,
	/// Samples admitted from the trailing 90-day window.
	pub samples:       u32,
}

/// One parser-produced record admitted only through explicit legacy repair.
#[derive(Debug, Clone, Copy)]
pub struct RepairRecord<'a> {
	/// Physical event index recovered by the parser.
	pub event_index:    u64,
	/// Complete-line byte watermark through this event.
	pub byte_watermark: u64,
	/// Event timestamp.
	pub ts_ms:          u64,
	/// Declared event kind.
	pub kind:           &'a str,
	/// Optional query projection recoverable from the legacy journal.
	pub projection:     EventProjection<'a>,
}

/// SQLite WAL sessions index with transaction-serialized writers.
pub struct SessionIndex {
	pub(crate) connection: Mutex<Connection>,
	authority:             IndexAuthority,
	writable:              bool,
	rename_observer:       RwLock<Option<Arc<dyn SessionRenameObserver>>>,
}
/// Non-blocking observer for committed live-session rename projections.
pub trait SessionRenameObserver: Send + Sync + 'static {
	/// Offers one normalized committed session name; `None` means cleared.
	fn renamed(&self, session: &SessionId, name: Option<&str>);
}

impl SessionIndex {
	/// Opens or creates the authoritative write-time index.
	#[tracing::instrument(
		name = "session_index_open",
		level = "debug",
		skip_all,
		fields(path = %path.as_ref().display(), mode = "writer")
	)]
	pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
		let connection = Connection::open(path)?;
		connection.busy_timeout(Duration::from_secs(5))?;
		connection.pragma_update(None, "journal_mode", "WAL")?;
		connection.pragma_update(None, "synchronous", "FULL")?;
		connection.pragma_update(None, "foreign_keys", "ON")?;
		connection.execute_batch(SCHEMA)?;
		migrate_schema(&connection)?;
		check_schema(&connection)?;
		Ok(Self {
			connection:      Mutex::new(connection),
			authority:       IndexAuthority::Authoritative,
			writable:        true,
			rename_observer: RwLock::new(None),
		})
	}

	/// Opens the remote authority's live query handle without participating in
	/// write transactions.
	///
	/// A first-start authority initializes the empty database before switching
	/// this connection into SQLite's query-only mode. This lets the project
	/// owner become ready before journal-owning apps open their independent
	/// transaction-serialized writer connections.
	#[tracing::instrument(
		name = "session_index_open",
		level = "debug",
		skip_all,
		fields(path = %path.as_ref().display(), mode = "authoritative_reader")
	)]
	pub fn open_authoritative_reader(path: impl AsRef<Path>) -> Result<Self, Error> {
		let connection = Connection::open(path)?;
		connection.busy_timeout(Duration::from_secs(5))?;
		let initialized = connection.query_row(
			"SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'index_meta')",
			[],
			|row| row.get::<_, bool>(0),
		)?;
		connection.pragma_update(None, "foreign_keys", "ON")?;
		if !initialized {
			connection.pragma_update(None, "journal_mode", "WAL")?;
			connection.pragma_update(None, "synchronous", "FULL")?;
			connection.execute_batch(SCHEMA)?;
		} else {
			migrate_schema(&connection)?;
		}
		check_schema(&connection)?;
		connection.pragma_update(None, "query_only", true)?;
		Ok(Self {
			connection:      Mutex::new(connection),
			authority:       IndexAuthority::Authoritative,
			writable:        false,
			rename_observer: RwLock::new(None),
		})
	}

	/// Opens a thin-client cache read-only for explicitly stale offline listing.
	#[tracing::instrument(
		name = "session_index_open",
		level = "debug",
		skip_all,
		fields(path = %path.as_ref().display(), mode = "offline_cache")
	)]
	pub fn open_offline_cache(path: impl AsRef<Path>, cached_at_ms: u64) -> Result<Self, Error> {
		let connection = Connection::open_with_flags(
			path,
			OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
		)?;
		check_schema(&connection)?;
		Ok(Self {
			connection:      Mutex::new(connection),
			authority:       IndexAuthority::OfflineCache { cached_at_ms },
			writable:        false,
			rename_observer: RwLock::new(None),
		})
	}

	/// Returns whether this handle is authoritative or an offline cache.
	pub const fn authority(&self) -> IndexAuthority {
		self.authority
	}

	/// Installs the environment-owned committed rename observer.
	pub fn bind_rename_observer(&self, observer: Arc<dyn SessionRenameObserver>) {
		*self.rename_observer.write() = Some(observer);
	}

	/// Runs the journal header write first, then publishes the session row in an
	/// immediate transaction serialized with every other process writer.
	///
	/// A failed journal closure never starts an index transaction. Once the
	/// closure succeeds, an index error is reported as
	/// [`IndexedWriteError::IndexAfterJournal`] so callers can halt and
	/// schedule explicit repair rather than claim the index committed.
	pub fn create_session<T, E>(
		&self,
		session: &NewSession<'_>,
		write_journal_header: impl FnOnce() -> Result<(T, u64), E>,
	) -> Result<T, IndexedWriteError<T, E>> {
		if let Err(error) = self.require_writer() {
			return Err(IndexedWriteError::IndexBeforeJournal(error));
		}
		let mut connection = self.connection.lock();
		let (written, journal_watermark) =
			write_journal_header().map_err(IndexedWriteError::Journal)?;
		let result = (|| {
			let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
			transaction.execute(
				"INSERT INTO sessions(
				 id, cwd, project, created_ms, updated_ms, status, kind, parent,
				 remote, journal_watermark
				 ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9)",
				params![
					session.id.0.as_str(),
					session.cwd,
					session.project,
					sql_u64(session.created_ms, "created_ms")?,
					<&'static str>::from(SessionStatus::Unknown),
					<&'static str>::from(session.kind),
					session.parent.map(|parent| parent.0.as_str()),
					session.remote,
					sql_u64(journal_watermark, "journal_watermark")?,
				],
			)?;
			transaction.commit()?;
			Ok(())
		})();
		if let Err(source) = result {
			return Err(IndexedWriteError::IndexAfterJournal { written, position: None, source });
		}
		Ok(written)
	}

	/// Publishes a fully staged journal and its seed projection exactly once.
	pub fn create_seeded_session<T, E>(
		&self,
		session: &NewSession<'_>,
		idempotency_key: &str,
		title: Option<&str>,
		entry_count: u64,
		entry_kinds: &[Str],
		write_journal: impl FnOnce() -> Result<(T, u64), E>,
	) -> Result<CreateSessionWrite<T>, IndexedWriteError<T, E>> {
		if let Err(error) = self.require_writer() {
			return Err(IndexedWriteError::IndexBeforeJournal(error));
		}
		let mut connection = self.connection.lock();
		let existing = connection
			.query_row(
				"SELECT session_id FROM session_create_receipts WHERE idempotency_key=?1",
				[idempotency_key],
				|row| row.get::<_, String>(0),
			)
			.optional()
			.map_err(Error::from)
			.map_err(IndexedWriteError::IndexBeforeJournal)?;
		if let Some(existing) = existing {
			return Ok(CreateSessionWrite::Existing(SessionId(Str::from(existing))));
		}
		let (written, journal_watermark) = write_journal().map_err(IndexedWriteError::Journal)?;
		let result = (|| {
			let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
			transaction.execute(
				"INSERT INTO sessions(
				 id, title, title_source, cwd, project, created_ms, updated_ms, status, kind,
				 parent, entries, remote, journal_watermark, last_event_index
				 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
				params![
					session.id.0.as_str(),
					title,
					title.map(|_| <&'static str>::from(TitleSource::User)),
					session.cwd,
					session.project,
					sql_u64(session.created_ms, "created_ms")?,
					<&'static str>::from(SessionStatus::Unknown),
					<&'static str>::from(session.kind),
					session.parent.map(|parent| parent.0.as_str()),
					sql_u64(entry_count, "entries")?,
					session.remote,
					sql_u64(journal_watermark, "journal_watermark")?,
					entry_count
						.checked_sub(1)
						.map(|index| sql_u64(index, "last_event_index"))
						.transpose()?,
				],
			)?;
			for kind in entry_kinds {
				transaction.execute(
					"INSERT OR IGNORE INTO session_entry_kinds(session_id, kind) VALUES (?1, ?2)",
					params![session.id.0.as_str(), kind.as_str()],
				)?;
			}
			transaction.execute(
				"INSERT INTO session_create_receipts(idempotency_key, session_id) VALUES (?1, ?2)",
				params![idempotency_key, session.id.0.as_str()],
			)?;
			transaction.commit()?;
			Ok(())
		})();
		if let Err(source) = result {
			return Err(IndexedWriteError::IndexAfterJournal { written, position: None, source });
		}
		Ok(CreateSessionWrite::Created(written))
	}

	/// Runs one journal append first, then updates its index projection in an
	/// immediate transaction serialized with every other process writer.
	pub fn append<T, E>(
		&self,
		event: &IndexedEvent<'_>,
		write_journal_event: impl FnOnce() -> Result<(T, JournalPosition), E>,
	) -> Result<T, IndexedWriteError<T, E>> {
		if let Err(error) = self.require_writer() {
			return Err(IndexedWriteError::IndexBeforeJournal(error));
		}
		let mut connection = self.connection.lock();
		let (written, position) = write_journal_event().map_err(IndexedWriteError::Journal)?;
		let result = (|| {
			let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
			validate_position(&transaction, event.session, position)?;
			index_event(&transaction, event, position)?;
			transaction.commit()?;
			Ok(())
		})();
		if let Err(source) = result {
			return Err(IndexedWriteError::IndexAfterJournal {
				written,
				position: Some(position),
				source,
			});
		}
		if let EventProjection::Title { title, .. } = event.projection
			&& let Some(observer) = self.rename_observer.read().as_ref()
		{
			observer.renamed(event.session, normalize_session_name(title));
		}
		if event.kind == "compact"
			&& let EventProjection::Context { epoch, .. } = event.projection
		{
			tracing::info!(
				session_id = %event.session.0,
				event_index = position.event_index,
				compaction_epoch = epoch,
				"session compaction committed"
			);
		}
		Ok(written)
	}

	/// Explicitly indexes parser-produced records from a foreign or legacy
	/// journal, advancing a monotonic repair watermark in one transaction.
	/// Normal listing and accounting never call this method.
	pub fn repair<'a>(
		&self,
		session: &SessionId,
		through_watermark: u64,
		records: impl IntoIterator<Item = RepairRecord<'a>>,
	) -> Result<(), Error> {
		self.require_writer()?;
		let mut connection = self.connection.lock();
		let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
		let prior = transaction.query_row(
			"SELECT repair_watermark FROM sessions WHERE id = ?1",
			[session.0.as_str()],
			|row| row.get::<_, u64>(0),
		)?;
		if through_watermark <= prior {
			return Err(Error::NonMonotonicWatermark);
		}
		let mut cursor = prior;
		let mut event_cursor = None;
		for record in records {
			if record.byte_watermark <= cursor || record.byte_watermark > through_watermark {
				return Err(Error::NonMonotonicWatermark);
			}
			if event_cursor.is_some_and(|index| record.event_index <= index) {
				return Err(Error::NonMonotonicWatermark);
			}
			cursor = record.byte_watermark;
			event_cursor = Some(record.event_index);
			let event = IndexedEvent {
				session,
				ts_ms: record.ts_ms,
				kind: record.kind,
				projection: record.projection,
			};
			index_event_repair(&transaction, &event, JournalPosition {
				event_index:    record.event_index,
				byte_watermark: record.byte_watermark,
			})?;
		}
		transaction.execute("UPDATE sessions SET repair_watermark = ?2 WHERE id = ?1", params![
			session.0.as_str(),
			sql_u64(through_watermark, "repair_watermark")?
		])?;
		transaction.commit()?;
		Ok(())
	}

	/// Lists indexed sessions newest first without opening any transcript.
	pub fn list(&self, filter: &SessionFilter) -> Result<SessionPage, Error> {
		let mut sql = String::from(
			"SELECT id, title, title_source, cwd, project, created_ms, updated_ms, status, \
			 kind,parent, entries, turns, journal_watermark, last_event_index, remote FROM sessions \
			 WHERE 1=1",
		);
		let mut values = Vec::new();
		if let Some(project) = &filter.project {
			push_filter(&mut sql, &mut values, " AND project = ?", Value::Text(project.to_string()));
		}
		if let Some(since) = filter.since_ms {
			push_filter(&mut sql, &mut values, " AND updated_ms >= ?", sql_value(since, "since_ms")?);
		}
		if let Some(until) = filter.until_ms {
			push_filter(&mut sql, &mut values, " AND updated_ms <= ?", sql_value(until, "until_ms")?);
		}
		push_enum_filter(&mut sql, &mut values, "status", &filter.statuses);
		if filter.kinds.is_empty() {
			push_filter(
				&mut sql,
				&mut values,
				" AND kind = ?",
				Value::Text(<&'static str>::from(SessionKind::Interactive).to_owned()),
			);
		} else {
			push_enum_filter(&mut sql, &mut values, "kind", &filter.kinds);
		}
		if let Some(kind) = &filter.contains_kind {
			push_filter(
				&mut sql,
				&mut values,
				" AND EXISTS (SELECT 1 FROM session_entry_kinds ek WHERE ek.session_id = sessions.id \
				 AND ek.kind = ?)",
				Value::Text(kind.to_string()),
			);
		}
		sql.push_str(" ORDER BY updated_ms DESC, id LIMIT ?");
		values.push(Value::Integer(i64::from(if filter.limit == 0 { 200 } else { filter.limit })));

		let connection = self.connection.lock();
		let mut statement = connection.prepare(&sql)?;
		let rows = statement.query_map(params_from_iter(values), decode_session)?;
		let mut sessions = Vec::new();
		for row in rows {
			sessions.push(row?);
		}
		drop(statement);
		for session in &mut sessions {
			let (usage, cost, models) = session_accounting(&connection, &session.id)?;
			session.usage = usage;
			session.cost = cost;
			session.models = models;
		}
		Ok(SessionPage { sessions, authority: self.authority })
	}

	/// Returns one root and every durable lineage descendant in parent-before-
	/// child order.
	pub fn subagent_tree(&self, root: &SessionId) -> Result<Vec<SessionInfo>, Error> {
		let connection = self.connection.lock();
		let mut statement = connection.prepare(
			"WITH RECURSIVE lineage(id, depth) AS (
			   SELECT id, 0 FROM sessions WHERE id = ?1
			   UNION ALL
			   SELECT child.id, lineage.depth + 1
			   FROM sessions child JOIN lineage ON child.parent = lineage.id
			 )
			 SELECT s.id, s.title, s.title_source, s.cwd, s.project, s.created_ms, s.updated_ms,
			        s.status, s.kind, s.parent, s.entries, s.turns, s.journal_watermark,
			        s.last_event_index, s.remote
			 FROM lineage JOIN sessions s ON s.id = lineage.id
			 ORDER BY lineage.depth, s.created_ms, s.id",
		)?;
		let rows = statement.query_map([root.0.as_str()], decode_session)?;
		let mut sessions = Vec::new();
		for row in rows {
			sessions.push(row?);
		}
		drop(statement);
		for session in &mut sessions {
			let (usage, cost, models) = session_accounting(&connection, &session.id)?;
			session.usage = usage;
			session.cost = cost;
			session.models = models;
		}
		Ok(sessions)
	}

	/// Returns one exact authoritative session row without scanning a journal.
	///
	/// The row is absent when the session is not indexed. Canonical accounting
	/// is loaded from receipt rows exactly as it is for [`Self::list`].
	pub fn get(&self, session: &SessionId) -> Result<Option<SessionInfo>, Error> {
		let connection = self.connection.lock();
		let mut info = connection
			.query_row(
				"SELECT id, title, title_source, cwd, project, created_ms, updated_ms, status,
				        kind, parent, entries, turns, journal_watermark, last_event_index, remote
				 FROM sessions WHERE id = ?1",
				[session.0.as_str()],
				decode_session,
			)
			.optional()?;
		if let Some(info) = &mut info {
			let (usage, cost, models) = session_accounting(&connection, &info.id)?;
			info.usage = usage;
			info.cost = cost;
			info.models = models;
		}
		Ok(info)
	}

	/// Returns the durable ancestry ending at `session`, ordered root first.
	///
	/// Parent checkpoints are copied from the child's durable `ForkedFrom`
	/// event; an absent value remains absent rather than being inferred.
	pub fn lineage(&self, session: &SessionId) -> Result<Vec<SessionLink>, Error> {
		let connection = self.connection.lock();
		let mut statement = connection.prepare(
			"WITH RECURSIVE ancestry(id, parent, parent_checkpoint, depth) AS (
			   SELECT id, parent, parent_checkpoint, 0 FROM sessions WHERE id = ?1
			   UNION ALL
			   SELECT parent.id, parent.parent, parent.parent_checkpoint, ancestry.depth + 1
			   FROM sessions parent JOIN ancestry ON parent.id = ancestry.parent
			 )
			 SELECT id, parent, parent_checkpoint FROM ancestry ORDER BY depth DESC",
		)?;
		let rows = statement.query_map([session.0.as_str()], |row| {
			Ok(SessionLink {
				id:     SessionId(Str::new(row.get::<_, String>(0)?)),
				parent: row
					.get::<_, Option<String>>(1)?
					.map(|value| SessionId(Str::new(value))),
				at:     row.get(2)?,
			})
		})?;
		let mut links = Vec::new();
		for row in rows {
			links.push(row?);
		}
		Ok(links)
	}

	/// Executes SQL aggregation over write-time receipt rows.
	pub fn usage(&self, query: &UsageQuery) -> Result<Vec<UsageBucket>, Error> {
		let (sql, values) = usage_sql(query)?;
		let connection = self.connection.lock();
		let mut statement = connection.prepare(&sql)?;
		let rows =
			statement.query_map(params_from_iter(values), |row| decode_usage_bucket(row, query))?;
		let mut buckets = Vec::new();
		for row in rows {
			buckets.push(row?);
		}
		drop(statement);
		merge_bucket_details(&connection, query, &mut buckets)?;
		Ok(buckets)
	}

	/// Aggregates one session and, when requested, every lineage descendant.
	///
	/// Recursive traversal uses distinct durable session IDs and joins canonical
	/// receipt/item rows, so parent-embedded task summaries never duplicate
	/// child accounting.
	pub fn session_statistics(
		&self,
		session: &SessionId,
		recursive: bool,
	) -> Result<SessionStatistics, Error> {
		let connection = self.connection.lock();
		session_statistics(&connection, session, recursive)
	}

	/// Loads exact canonical accounting for one receipt without parsing its
	/// journal.
	pub fn receipt(
		&self,
		session: &SessionId,
		event_index: u64,
	) -> Result<Option<ReceiptAccounting>, Error> {
		let connection = self.connection.lock();
		let encoded = connection
			.query_row(
				"SELECT usage, cost FROM receipts WHERE session_id = ?1 AND event_index = ?2",
				params![session.0.as_str(), sql_u64(event_index, "event_index")?],
				|row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
			)
			.optional()?;
		encoded
			.map(|(usage, cost)| {
				Ok(ReceiptAccounting {
					usage: pb::Usage::decode(usage.as_slice())?,
					cost:  pb::Cost::decode(cost.as_slice())?,
				})
			})
			.transpose()
	}

	/// Searches private prompt rows with FTS5 and deterministic Unicode
	/// substring fallback.
	pub fn search_prompts(&self, query: &str, limit: u32) -> Result<Vec<PromptHit>, Error> {
		let connection = self.connection.lock();
		let phrase = format!("\"{}\"", query.replace('"', "\"\""));
		let mut hits = Vec::new();
		{
			let mut statement = connection.prepare(
				"SELECT session_id, event_index, prompt FROM prompts_fts
				 WHERE prompts_fts MATCH ?1 ORDER BY rank LIMIT ?2",
			)?;
			let rows = statement.query_map(params![phrase, i64::from(limit)], |row| {
				Ok(PromptHit {
					session:     SessionId(Str::new(row.get::<_, String>(0)?)),
					event_index: row.get(1)?,
					prompt:      Str::new(row.get::<_, String>(2)?),
				})
			})?;
			for row in rows {
				hits.push(row?);
			}
		}
		if hits.len() < usize::try_from(limit).expect("u32 fits in usize") {
			let remaining = usize::try_from(limit)
				.expect("u32 fits in usize")
				.saturating_sub(hits.len());
			let mut statement = connection.prepare(
				"SELECT session_id, event_index, prompt FROM prompts_fts
				 WHERE instr(lower(prompt), lower(?1)) > 0
				 ORDER BY rowid DESC LIMIT ?2",
			)?;
			let rows = statement.query_map(
				params![query, i64::try_from(remaining).expect("search limit fits in i64")],
				|row| {
					Ok(PromptHit {
						session:     SessionId(Str::new(row.get::<_, String>(0)?)),
						event_index: row.get(1)?,
						prompt:      Str::new(row.get::<_, String>(2)?),
					})
				},
			)?;
			for row in rows {
				let hit = row?;
				if !hits
					.iter()
					.any(|known| known.session == hit.session && known.event_index == hit.event_index)
				{
					hits.push(hit);
				}
			}
		}
		hits.truncate(usize::try_from(limit).expect("u32 fits in usize"));
		Ok(hits)
	}

	/// Returns unique, newest-first prompts from interactive sessions only.
	///
	/// Non-empty queries combine token-prefix FTS5 matching with token-AND
	/// substring fallback. Empty, whitespace-only, and punctuation-only queries
	/// return the most recent prompts.
	pub fn prompt_history(&self, query: &str, limit: u32) -> Result<Vec<PromptHistoryEntry>, Error> {
		let limit = limit.min(1_000);
		if limit == 0 {
			return Ok(Vec::new());
		}
		let connection = self.connection.lock();
		let lower_query = query.to_lowercase();
		let tokens = lower_query
			.split(|character: char| !character.is_alphanumeric())
			.filter(|token| !token.is_empty())
			.collect::<Vec<_>>();

		if tokens.is_empty() {
			let mut statement = connection.prepare(
				"SELECT prompt, ts_ms, MAX(prompts_fts.rowid)
				 FROM prompts_fts
				 JOIN sessions
				   ON sessions.id = prompts_fts.session_id
				  AND sessions.kind = 'interactive'
				 GROUP BY prompt
				 ORDER BY MAX(prompts_fts.rowid) DESC
				 LIMIT ?1",
			)?;
			// With exactly one MAX aggregate, SQLite takes bare columns from the
			// row containing that maximum, so `ts_ms` belongs to the newest prompt.
			let rows = statement.query_map([i64::from(limit)], |row| {
				Ok(PromptHistoryEntry {
					prompt: Str::new(row.get::<_, String>(0)?),
					ts_ms:  row.get(1)?,
				})
			})?;
			return rows.collect::<Result<Vec<_>, _>>().map_err(Error::from);
		}

		let mut match_expression = String::with_capacity(
			lower_query
				.len()
				.saturating_add(tokens.len().saturating_mul(4)),
		);
		for token in &tokens {
			if !match_expression.is_empty() {
				match_expression.push(' ');
			}
			match_expression.push('"');
			for character in token.chars() {
				if character == '"' {
					match_expression.push('"');
				}
				match_expression.push(character);
			}
			match_expression.push_str("\"*");
		}
		let mut merged = BTreeMap::<Str, (Option<u64>, i64)>::new();

		let fts_rows = (|| -> rusqlite::Result<Vec<(Str, Option<u64>, i64)>> {
			let mut statement = connection.prepare(
				"SELECT prompt, ts_ms, MAX(prompts_fts.rowid)
				 FROM prompts_fts
				 JOIN sessions
				   ON sessions.id = prompts_fts.session_id
				  AND sessions.kind = 'interactive'
				 WHERE prompts_fts MATCH ?1
				 GROUP BY prompt
				 ORDER BY MAX(prompts_fts.rowid) DESC
				 LIMIT ?2",
			)?;
			let rows = statement.query_map(params![match_expression, i64::from(limit)], |row| {
				Ok((Str::new(row.get::<_, String>(0)?), row.get(1)?, row.get(2)?))
			})?;
			rows.collect()
		})();
		// A malformed FTS expression must not prevent the deterministic fallback.
		if let Ok(rows) = fts_rows {
			for (prompt, ts_ms, rowid) in rows {
				merged.insert(prompt, (ts_ms, rowid));
			}
		}

		let mut substring_sql = String::from(
			"SELECT prompt, ts_ms, MAX(prompts_fts.rowid)
			 FROM prompts_fts
			 JOIN sessions
			   ON sessions.id = prompts_fts.session_id
			  AND sessions.kind = 'interactive'
			 WHERE 1 = 1",
		);
		let mut values = Vec::with_capacity(tokens.len().saturating_add(1));
		for token in &tokens {
			substring_sql.push_str(" AND instr(lower(prompt), ?) > 0");
			values.push(Value::Text((*token).to_owned()));
		}
		substring_sql.push_str(
			" GROUP BY prompt
			  ORDER BY MAX(prompts_fts.rowid) DESC
			  LIMIT ?",
		);
		values.push(Value::Integer(i64::from(limit)));
		let mut statement = connection.prepare(&substring_sql)?;
		let rows = statement.query_map(params_from_iter(values), |row| {
			Ok((Str::new(row.get::<_, String>(0)?), row.get(1)?, row.get(2)?))
		})?;
		for row in rows {
			let (prompt, ts_ms, rowid) = row?;
			merged.entry(prompt).or_insert((ts_ms, rowid));
		}

		let mut history = merged
			.into_iter()
			.map(|(prompt, (ts_ms, rowid))| (PromptHistoryEntry { prompt, ts_ms }, rowid))
			.collect::<Vec<_>>();
		history.sort_unstable_by(|left, right| right.1.cmp(&left.1));
		history.truncate(usize::try_from(limit).expect("u32 fits in usize"));
		Ok(history.into_iter().map(|(entry, _)| entry).collect())
	}

	/// Loads the latest monotonic context position for one session.
	pub fn context_position(&self, session: &SessionId) -> Result<Option<ContextPosition>, Error> {
		let connection = self.connection.lock();
		connection
			.query_row(
				"SELECT context_anchor, context_revision, compaction_epoch
				 FROM sessions WHERE id = ?1",
				[session.0.as_str()],
				|row| {
					Ok(ContextPosition {
						anchor:   row.get(0)?,
						revision: row.get(1)?,
						epoch:    row.get(2)?,
					})
				},
			)
			.optional()
			.map_err(Error::from)
	}

	/// Lists settled serving-model samples newest first.
	pub fn model_performance(
		&self,
		provider: &str,
		model: &str,
		since_ms: u64,
		limit: u32,
	) -> Result<Vec<ModelPerformanceSample>, Error> {
		let connection = self.connection.lock();
		let mut statement = connection.prepare(
			"SELECT session_id, event_index, ts_ms, provider, model, ttft_ms,
			 duration_ms, output_tokens FROM model_performance
			 WHERE provider = ?1 AND model = ?2 AND ts_ms >= ?3
			 ORDER BY ts_ms DESC LIMIT ?4",
		)?;
		let rows = statement.query_map(
			params![provider, model, sql_u64(since_ms, "since_ms")?, i64::from(limit),],
			|row| {
				Ok(ModelPerformanceSample {
					session:       SessionId(Str::new(row.get::<_, String>(0)?)),
					event_index:   row.get(1)?,
					ts_ms:         row.get(2)?,
					provider:      Str::new(row.get::<_, String>(3)?),
					model:         Str::new(row.get::<_, String>(4)?),
					ttft_ms:       row.get(5)?,
					duration_ms:   row.get(6)?,
					output_tokens: row.get(7)?,
				})
			},
		)?;
		let mut samples = Vec::new();
		for row in rows {
			samples.push(row?);
		}
		Ok(samples)
	}

	/// Computes a bounded integer exponential-decay estimate over the trailing
	/// 90-day sample window.
	pub fn model_performance_estimate(
		&self,
		provider: &str,
		model: &str,
		now_ms: u64,
	) -> Result<Option<ModelPerformanceEstimate>, Error> {
		const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
		const WINDOW_MS: u64 = 90 * DAY_MS;
		let samples =
			self.model_performance(provider, model, now_ms.saturating_sub(WINDOW_MS), 4_096)?;
		if samples.is_empty() {
			return Ok(None);
		}
		let mut total_weight = 0_u128;
		let mut output = 0_u128;
		let mut ttft = (0_u128, 0_u128);
		let mut duration = (0_u128, 0_u128);
		for sample in &samples {
			let age_weeks = now_ms.saturating_sub(sample.ts_ms) / (7 * DAY_MS);
			let shift = u32::try_from(age_weeks.min(12)).expect("bounded decay shift");
			let weight = u128::from(1_u16 << (12 - shift));
			total_weight = total_weight.saturating_add(weight);
			output = output.saturating_add(u128::from(sample.output_tokens) * weight);
			if let Some(value) = sample.ttft_ms {
				ttft.0 = ttft.0.saturating_add(u128::from(value) * weight);
				ttft.1 = ttft.1.saturating_add(weight);
			}
			if let Some(value) = sample.duration_ms {
				duration.0 = duration.0.saturating_add(u128::from(value) * weight);
				duration.1 = duration.1.saturating_add(weight);
			}
		}
		let weighted = |sum: u128, weight: u128| {
			(weight != 0).then(|| u64::try_from(sum / weight).unwrap_or(u64::MAX))
		};
		Ok(Some(ModelPerformanceEstimate {
			ttft_ms:       weighted(ttft.0, ttft.1),
			duration_ms:   weighted(duration.0, duration.1),
			output_tokens: weighted(output, total_weight).unwrap_or_default(),
			samples:       u32::try_from(samples.len()).expect("query limit fits u32"),
		}))
	}

	/// Records one invocation of a canonical slash-command name.
	pub fn record_command_usage(&self, name: &str, now_ms: u64) -> Result<(), Error> {
		self.require_writer()?;
		let connection = self.connection.lock();
		connection.execute(
			"INSERT INTO command_usage(name, count, last_used_at) VALUES (?1, 1, ?2)
			 ON CONFLICT(name) DO UPDATE SET
			     count = command_usage.count + 1,
			     last_used_at = excluded.last_used_at",
			params![name, sql_u64(now_ms, "command_usage.last_used_at")?],
		)?;
		Ok(())
	}

	/// Returns persisted slash-command invocation counts keyed by canonical
	/// name.
	pub fn command_usage(&self) -> Result<BTreeMap<Str, u64>, Error> {
		let connection = self.connection.lock();
		let mut statement = connection.prepare("SELECT name, count FROM command_usage")?;
		let rows = statement
			.query_map([], |row| Ok((Str::new(row.get::<_, String>(0)?), row.get::<_, u64>(1)?)))?;
		let mut counts = BTreeMap::new();
		for row in rows {
			let (name, count) = row?;
			counts.insert(name, count);
		}
		Ok(counts)
	}

	/// Backfills at most `limit` legacy receipt samples from the bounded
	/// trailing 90-day window. Existing samples are left untouched.
	pub fn backfill_model_performance(&self, now_ms: u64, limit: u32) -> Result<u64, Error> {
		self.require_writer()?;
		const NINETY_DAYS_MS: u64 = 90 * 24 * 60 * 60 * 1_000;
		let since_ms = now_ms.saturating_sub(NINETY_DAYS_MS);
		let connection = self.connection.lock();
		let changed = connection.execute(
			"INSERT OR IGNORE INTO model_performance(
			 session_id, event_index, ts_ms, provider, model, ttft_ms, duration_ms, output_tokens
			 )
			 SELECT session_id, event_index, ts_ms, provider, model, NULL, duration_ms,
			        output_tokens
			 FROM receipts
			 WHERE ts_ms >= ?1
			 ORDER BY ts_ms DESC
			 LIMIT ?2",
			params![sql_u64(since_ms, "since_ms")?, i64::from(limit)],
		)?;
		Ok(u64::try_from(changed).expect("SQLite changed-row count fits u64"))
	}

	pub(crate) const fn require_writer(&self) -> Result<(), Error> {
		match (self.authority, self.writable) {
			(IndexAuthority::Authoritative, true) => Ok(()),
			(IndexAuthority::Authoritative, false) => Err(Error::ReadOnlyAuthority),
			(IndexAuthority::OfflineCache { .. }, _) => Err(Error::ReadOnlyCache),
		}
	}
}

#[tracing::instrument(
	name = "storage_migration",
	level = "debug",
	skip_all,
	fields(database = "session_index", target_version = SCHEMA_VERSION)
)]
fn migrate_schema(connection: &Connection) -> Result<(), Error> {
	let mut version = connection.query_row(
		"SELECT schema_version FROM index_meta WHERE singleton = 1",
		[],
		|row| row.get::<_, i64>(0),
	)?;
	let initial_version = version;
	if version == 1 {
		connection.execute_batch(
			"ALTER TABLE sessions ADD COLUMN serving_provider TEXT;
			 ALTER TABLE sessions ADD COLUMN serving_model TEXT;
			 ALTER TABLE sessions ADD COLUMN context_anchor INTEGER;
			 ALTER TABLE sessions ADD COLUMN context_revision INTEGER NOT NULL DEFAULT 0;
			 ALTER TABLE sessions ADD COLUMN compaction_epoch INTEGER NOT NULL DEFAULT 0;
			 UPDATE index_meta SET schema_version = 2 WHERE singleton = 1;",
		)?;
		version = 2;
	}
	if version == 2 {
		connection.execute_batch(
			"CREATE TABLE IF NOT EXISTS item_outcomes (
			    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
			    event_index INTEGER NOT NULL,
			    user_messages INTEGER NOT NULL DEFAULT 0,
			    assistant_messages INTEGER NOT NULL DEFAULT 0,
			    system_messages INTEGER NOT NULL DEFAULT 0,
			    tool_calls INTEGER NOT NULL DEFAULT 0,
			    tool_results INTEGER NOT NULL DEFAULT 0,
			    tool_errors INTEGER NOT NULL DEFAULT 0,
			    PRIMARY KEY(session_id, event_index)
			 ) WITHOUT ROWID;
			 UPDATE index_meta SET schema_version = 3 WHERE singleton = 1;",
		)?;
		version = 3;
	}
	if version == 3 {
		connection.execute_batch(
			"ALTER TABLE sessions ADD COLUMN parent_checkpoint INTEGER;
			 UPDATE index_meta SET schema_version = 4 WHERE singleton = 1;",
		)?;
		version = 4;
	}
	if version == 4 {
		connection.execute_batch(
			"CREATE VIRTUAL TABLE prompts_fts_v5 USING fts5(
			    session_id UNINDEXED,
			    event_index UNINDEXED,
			    prompt,
			    ts_ms UNINDEXED,
			    tokenize = 'unicode61'
			 );
			 INSERT INTO prompts_fts_v5(session_id, event_index, prompt, ts_ms)
			 SELECT session_id, event_index, prompt, NULL FROM prompts_fts;
			 DROP TABLE prompts_fts;
			 ALTER TABLE prompts_fts_v5 RENAME TO prompts_fts;
			 UPDATE index_meta SET schema_version = 5 WHERE singleton = 1;",
		)?;
		version = 5;
	}
	if version != initial_version {
		tracing::info!(
			database = "session_index",
			from_version = initial_version,
			to_version = version,
			"storage migration completed"
		);
	}
	Ok(())
}

fn check_schema(connection: &Connection) -> Result<(), Error> {
	let version = connection.query_row(
		"SELECT schema_version FROM index_meta WHERE singleton = 1",
		[],
		|row| row.get::<_, i64>(0),
	)?;
	if version != SCHEMA_VERSION {
		return Err(Error::Database(rusqlite::Error::InvalidQuery));
	}
	Ok(())
}

fn validate_position(
	connection: &Transaction<'_>,
	session: &SessionId,
	position: JournalPosition,
) -> Result<(), Error> {
	let prior = connection.query_row(
		"SELECT last_event_index, journal_watermark FROM sessions WHERE id = ?1",
		[session.0.as_str()],
		|row| Ok((row.get::<_, Option<u64>>(0)?, row.get::<_, u64>(1)?)),
	)?;
	if prior.0.is_some_and(|index| position.event_index <= index)
		|| position.byte_watermark <= prior.1
	{
		return Err(Error::NonMonotonicWatermark);
	}
	Ok(())
}

fn index_event(
	transaction: &Transaction<'_>,
	event: &IndexedEvent<'_>,
	position: JournalPosition,
) -> Result<(), Error> {
	index_event_inner(transaction, event, position, false)
}

fn normalize_session_name(value: &str) -> Option<&str> {
	let value = value.trim();
	(!value.is_empty()).then_some(value)
}

fn index_event_repair(
	transaction: &Transaction<'_>,
	event: &IndexedEvent<'_>,
	position: JournalPosition,
) -> Result<(), Error> {
	index_event_inner(transaction, event, position, true)
}

fn index_event_inner(
	transaction: &Transaction<'_>,
	event: &IndexedEvent<'_>,
	position: JournalPosition,
	repair: bool,
) -> Result<(), Error> {
	if !repair {
		let prior = transaction.query_row(
			"SELECT last_event_index, journal_watermark FROM sessions WHERE id = ?1",
			[event.session.0.as_str()],
			|row| Ok((row.get::<_, Option<u64>>(0)?, row.get::<_, u64>(1)?)),
		)?;
		if prior.0.is_some_and(|index| position.event_index <= index)
			|| position.byte_watermark <= prior.1
		{
			return Err(Error::NonMonotonicWatermark);
		}
	}

	transaction.execute(
		"INSERT OR IGNORE INTO session_entry_kinds(session_id, kind) VALUES (?1, ?2)",
		params![event.session.0.as_str(), event.kind],
	)?;
	transaction.execute(
		"UPDATE sessions SET updated_ms = MAX(updated_ms, ?2), entries = MAX(entries, ?3),
		 journal_watermark = MAX(journal_watermark, ?4),
		 last_event_index = CASE WHEN last_event_index IS NULL OR last_event_index < ?5 THEN ?5 ELSE \
		 last_event_index END
		 WHERE id = ?1",
		params![
			event.session.0.as_str(),
			sql_u64(event.ts_ms, "ts_ms")?,
			sql_u64(position.event_index.saturating_add(1), "entries")?,
			sql_u64(position.byte_watermark, "journal_watermark")?,
			sql_u64(position.event_index, "event_index")?,
		],
	)?;
	match event.projection {
		EventProjection::Plain => {},
		EventProjection::Title { title, source } => {
			let title = normalize_session_name(title);
			transaction.execute(
				"UPDATE sessions SET title = ?2, title_source = ?3 WHERE id = ?1",
				params![event.session.0.as_str(), title, title.map(|_| <&'static str>::from(source))],
			)?;
		},
		EventProjection::Prompt { text } => {
			transaction.execute(
				"INSERT INTO prompts_fts(session_id, event_index, prompt, ts_ms) VALUES (?1, ?2, ?3, \
				 ?4)",
				params![
					event.session.0.as_str(),
					sql_u64(position.event_index, "event_index")?,
					text,
					sql_u64(event.ts_ms, "ts_ms")?,
				],
			)?;
		},
		EventProjection::Context { anchor, revision, epoch } => {
			update_context_position(transaction, event.session, ContextPosition {
				anchor,
				revision,
				epoch,
			})?;
		},
		EventProjection::ThreadItem { item, prompt, context } => {
			insert_item_outcome(transaction, event, position, item)?;
			if let Some(prompt) = prompt {
				transaction.execute(
					"INSERT INTO prompts_fts(session_id, event_index, prompt, ts_ms) VALUES (?1, ?2, \
					 ?3, ?4)",
					params![
						event.session.0.as_str(),
						sql_u64(position.event_index, "event_index")?,
						prompt,
						sql_u64(event.ts_ms, "ts_ms")?,
					],
				)?;
			}
			if let Some(context) = context {
				update_context_position(transaction, event.session, context)?;
			}
		},
		EventProjection::TurnReceipt { outcome, failed } => {
			insert_receipt(transaction, event, position, outcome, failed)?;
			transaction.execute(
				"UPDATE sessions SET serving_provider = ?2, serving_model = ?3 WHERE id = ?1",
				params![event.session.0.as_str(), outcome.provider.as_str(), outcome.model.as_str()],
			)?;
			let usage = outcome.usage.as_ref().ok_or(Error::MissingUsage)?;
			transaction.execute(
				"INSERT OR REPLACE INTO model_performance(
				 session_id, event_index, ts_ms, provider, model, ttft_ms, duration_ms, output_tokens
				 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
				params![
					event.session.0.as_str(),
					sql_u64(position.event_index, "event_index")?,
					sql_u64(event.ts_ms, "ts_ms")?,
					outcome.provider.as_str(),
					outcome.model.as_str(),
					outcome.ttft_ms,
					outcome.duration_ms,
					usage.output_tokens,
				],
			)?;
			let status = if failed {
				SessionStatus::Error
			} else {
				SessionStatus::Complete
			};
			transaction.execute(
				"UPDATE sessions SET turns = turns + 1, status = ?2 WHERE id = ?1",
				params![event.session.0.as_str(), <&'static str>::from(status)],
			)?;
		},
		EventProjection::Status(status) => {
			transaction.execute("UPDATE sessions SET status = ?2 WHERE id = ?1", params![
				event.session.0.as_str(),
				<&'static str>::from(status)
			])?;
		},
		EventProjection::Fork { parent, at } => {
			let at = at
				.map(|value| sql_u64(value, "parent_checkpoint"))
				.transpose()?;
			transaction.execute(
				"UPDATE sessions SET parent = ?2, parent_checkpoint = ?3 WHERE id = ?1",
				params![event.session.0.as_str(), parent.0.as_str(), at],
			)?;
		},
	}
	Ok(())
}

fn update_context_position(
	transaction: &Transaction<'_>,
	session: &SessionId,
	context: ContextPosition,
) -> Result<(), Error> {
	let changed = transaction.execute(
		"UPDATE sessions SET context_anchor = ?2, context_revision = ?3,
		 compaction_epoch = ?4
		 WHERE id = ?1 AND context_revision <= ?3 AND compaction_epoch <= ?4",
		params![
			session.0.as_str(),
			context
				.anchor
				.map(|anchor| sql_u64(anchor, "context_anchor"))
				.transpose()?,
			sql_u64(context.revision, "context_revision")?,
			sql_u64(context.epoch, "compaction_epoch")?,
		],
	)?;
	if changed != 1 {
		return Err(Error::NonMonotonicWatermark);
	}
	Ok(())
}

fn insert_item_outcome(
	transaction: &Transaction<'_>,
	event: &IndexedEvent<'_>,
	position: JournalPosition,
	item: &thread_pb::Item,
) -> Result<(), Error> {
	let mut user_messages = 0_u8;
	let mut assistant_messages = 0_u8;
	let mut system_messages = 0_u8;
	let mut tool_calls = 0_u8;
	let mut tool_results = 0_u8;
	let mut tool_errors = 0_u8;
	match item.kind.as_ref() {
		Some(item::Kind::Message(message)) => {
			match thread_pb::Role::try_from(message.role).unwrap_or(thread_pb::Role::Unspecified) {
				thread_pb::Role::User => user_messages = 1,
				thread_pb::Role::Assistant => assistant_messages = 1,
				thread_pb::Role::System => system_messages = 1,
				thread_pb::Role::Unspecified => {},
			}
		},
		Some(item::Kind::ToolCall(_)) => tool_calls = 1,
		Some(item::Kind::ToolResult(result)) => {
			tool_results = 1;
			tool_errors = u8::from(result.is_error);
		},
		None => {},
	}
	transaction.execute(
		"INSERT INTO item_outcomes(
		 session_id, event_index, user_messages, assistant_messages, system_messages,
		 tool_calls, tool_results, tool_errors
		 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
		params![
			event.session.0.as_str(),
			sql_u64(position.event_index, "event_index")?,
			user_messages,
			assistant_messages,
			system_messages,
			tool_calls,
			tool_results,
			tool_errors,
		],
	)?;
	Ok(())
}

fn insert_receipt(
	transaction: &Transaction<'_>,
	event: &IndexedEvent<'_>,
	position: JournalPosition,
	outcome: &pb::Outcome,
	failed: bool,
) -> Result<(), Error> {
	let usage = outcome.usage.as_ref().ok_or(Error::MissingUsage)?;
	let cost = outcome.cost.unwrap_or_default();
	let orchestration = usage.orchestration.as_ref();
	let cache_ttl = usage.cache_ttl.as_ref();
	let server_tools = usage.server_tools.as_ref();
	transaction.execute(
		"INSERT INTO receipts(
		 session_id, event_index, journal_watermark, ts_ms, provider, model, failed, duration_ms,
		 usage, cost, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, accuracy,
		 total_tokens, context_tokens, orchestration_input_tokens, orchestration_cache_read_tokens,
		 orchestration_output_tokens, premium_requests, reasoning_tokens, cache_ephemeral_5m_tokens,
		 cache_ephemeral_1h_tokens, web_search_requests, web_fetch_requests, cost_nanos_usd,
		 cost_estimated, input_nanos_usd, output_nanos_usd, cache_read_nanos_usd,
		 cache_write_nanos_usd
		 ) VALUES (
		 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
		 ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32
		 )",
		params![
			event.session.0.as_str(),
			sql_u64(position.event_index, "event_index")?,
			sql_u64(position.byte_watermark, "journal_watermark")?,
			sql_u64(event.ts_ms, "ts_ms")?,
			outcome.provider.as_str(),
			outcome.model.as_str(),
			failed,
			outcome.duration_ms,
			usage.encode_to_vec(),
			cost.encode_to_vec(),
			usage.input_tokens,
			usage.output_tokens,
			usage.cache_read_tokens,
			usage.cache_write_tokens,
			usage.accuracy,
			usage.total_tokens,
			usage.context_tokens,
			orchestration.and_then(|value| value.input_tokens),
			orchestration.and_then(|value| value.cache_read_tokens),
			orchestration.and_then(|value| value.output_tokens),
			usage.premium_requests,
			usage.reasoning_tokens,
			cache_ttl.and_then(|value| value.ephemeral_5m_tokens),
			cache_ttl.and_then(|value| value.ephemeral_1h_tokens),
			server_tools.and_then(|value| value.web_search_requests),
			server_tools.and_then(|value| value.web_fetch_requests),
			cost.nanos_usd,
			cost.estimated,
			cost.input_nanos_usd,
			cost.output_nanos_usd,
			cost.cache_read_nanos_usd,
			cost.cache_write_nanos_usd,
		],
	)?;
	Ok(())
}

fn decode_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionInfo> {
	let status_text = row.get::<_, String>(7)?;
	let kind_text = row.get::<_, String>(8)?;
	let title_source = row
		.get::<_, Option<String>>(2)?
		.map(|value| {
			TitleSource::from_str(&value).map_err(|_| {
				rusqlite::Error::FromSqlConversionFailure(
					2,
					types::Type::Text,
					Box::new(Error::UnknownVocabulary {
						field: "title_source",
						value: Str::from(value),
					}),
				)
			})
		})
		.transpose()?;
	let status = SessionStatus::from_str(&status_text).map_err(|_| {
		rusqlite::Error::FromSqlConversionFailure(
			7,
			types::Type::Text,
			Box::new(Error::UnknownVocabulary { field: "status", value: Str::from(status_text) }),
		)
	})?;
	let kind = SessionKind::from_str(&kind_text).map_err(|_| {
		rusqlite::Error::FromSqlConversionFailure(
			8,
			types::Type::Text,
			Box::new(Error::UnknownVocabulary { field: "kind", value: Str::from(kind_text) }),
		)
	})?;
	Ok(SessionInfo {
		id: SessionId(Str::from(row.get::<_, String>(0)?)),
		title: row.get::<_, Option<String>>(1)?.map(Str::from),
		title_source,
		cwd: Str::from(row.get::<_, String>(3)?),
		project: Str::from(row.get::<_, String>(4)?),
		created_ms: row.get(5)?,
		updated_ms: row.get(6)?,
		status,
		kind,
		parent: row
			.get::<_, Option<String>>(9)?
			.map(|value| SessionId(Str::from(value))),
		entries: row.get(10)?,
		turns: row.get(11)?,
		journal_watermark: row.get(12)?,
		usage: pb::Usage::default(),
		cost: pb::Cost::default(),
		models: SmallVec::new(),
		last_event_index: row.get(13)?,
		remote: row.get(14)?,
	})
}

fn session_statistics(
	connection: &Connection,
	session: &SessionId,
	recursive: bool,
) -> Result<SessionStatistics, Error> {
	let scope = if recursive {
		"WITH RECURSIVE subtree(id) AS (
		    SELECT ?1
		    UNION
		    SELECT sessions.id FROM sessions JOIN subtree ON sessions.parent = subtree.id
		 )"
	} else {
		"WITH subtree(id) AS (SELECT ?1)"
	};
	let query = UsageQuery { group_by: SmallVec::new(), ..UsageQuery::default() };
	let mut usage_query = String::from(scope);
	usage_query.push_str(
		", scoped AS (
		    SELECT receipts.* FROM receipts JOIN subtree ON subtree.id = receipts.session_id
		 ) SELECT ",
	);
	usage_query.push_str(USAGE_AGGREGATE_COLUMNS);
	usage_query.push_str(" FROM scoped HAVING COUNT(*) > 0");
	let aggregate = connection
		.query_row(&usage_query, [session.0.as_str()], |row| decode_usage_bucket(row, &query))
		.optional()?;
	let (mut usage, cost, requests, request_errors) = aggregate.map_or_else(
		|| (pb::Usage::default(), pb::Cost::default(), 0, 0),
		|bucket| (bucket.usage, bucket.cost, bucket.requests, bucket.errors),
	);

	let mut detail_query = String::from(scope);
	detail_query.push_str(
		" SELECT receipts.usage FROM receipts
		  JOIN subtree ON subtree.id = receipts.session_id
		  ORDER BY receipts.session_id, receipts.event_index",
	);
	let mut statement = connection.prepare(&detail_query)?;
	let encoded = statement.query_map([session.0.as_str()], |row| row.get::<_, Vec<u8>>(0))?;
	for row in encoded {
		let receipt = pb::Usage::decode(row?.as_slice())?;
		merge_detail(&mut usage.detail, receipt.detail);
	}
	drop(statement);

	let mut outcome_query = String::from(scope);
	outcome_query.push_str(
		" SELECT
		    COALESCE(SUM(user_messages), 0),
		    COALESCE(SUM(assistant_messages), 0),
		    COALESCE(SUM(system_messages), 0),
		    COALESCE(SUM(tool_calls), 0),
		    COALESCE(SUM(tool_results), 0),
		    COALESCE(SUM(tool_errors), 0)
		  FROM item_outcomes JOIN subtree ON subtree.id = item_outcomes.session_id",
	);
	let (user_messages, assistant_messages, system_messages, tool_calls, tool_results, tool_errors) =
		connection.query_row(&outcome_query, [session.0.as_str()], |row| {
			Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
		})?;

	let mut session_count_query = String::from(scope);
	session_count_query.push_str(" SELECT COUNT(*) FROM subtree");
	let sessions =
		connection.query_row(&session_count_query, [session.0.as_str()], |row| row.get(0))?;

	Ok(SessionStatistics {
		user_messages,
		assistant_messages,
		system_messages,
		tool_calls,
		tool_results,
		tool_errors,
		usage,
		cost,
		requests,
		request_errors,
		sessions,
	})
}

fn session_accounting(
	connection: &Connection,
	session: &SessionId,
) -> Result<(pb::Usage, pb::Cost, SmallVec<Str, 4>), Error> {
	let query = UsageQuery {
		session: Some(session.clone()),
		group_by: SmallVec::new(),
		..UsageQuery::default()
	};
	let (sql, values) = usage_sql(&query)?;
	let mut statement = connection.prepare(&sql)?;
	let mut rows =
		statement.query_map(params_from_iter(values), |row| decode_usage_bucket(row, &query))?;
	let (mut usage, cost) = match rows.next() {
		Some(row) => {
			let bucket = row?;
			(bucket.usage, bucket.cost)
		},
		None => (pb::Usage::default(), pb::Cost::default()),
	};
	drop(rows);
	drop(statement);

	let mut statement = connection
		.prepare("SELECT usage FROM receipts WHERE session_id = ?1 ORDER BY event_index")?;
	let encoded = statement.query_map([session.0.as_str()], |row| row.get::<_, Vec<u8>>(0))?;
	for row in encoded {
		let receipt = pb::Usage::decode(row?.as_slice())?;
		merge_detail(&mut usage.detail, receipt.detail);
	}
	drop(statement);

	let mut statement = connection.prepare(
		"SELECT DISTINCT provider || '/' || model FROM receipts
		 WHERE session_id = ?1 ORDER BY provider || '/' || model",
	)?;
	let rows = statement.query_map([session.0.as_str()], |row| row.get::<_, String>(0))?;
	let mut models = SmallVec::new();
	for row in rows {
		models.push(Str::from(row?));
	}
	Ok((usage, cost, models))
}

fn merge_detail(target: &mut Option<pb::ValueMap>, source: Option<pb::ValueMap>) {
	let Some(source) = source else {
		return;
	};
	let target = target.get_or_insert_with(pb::ValueMap::default);
	for (key, incoming) in source.fields {
		use std::collections::btree_map::Entry;
		match target.fields.entry(key) {
			Entry::Vacant(entry) => {
				entry.insert(incoming);
			},
			Entry::Occupied(mut entry) => {
				if let Some(merged) = merge_detail_value(entry.get(), &incoming) {
					*entry.get_mut() = merged;
				} else {
					entry.remove();
				}
			},
		}
	}
}

fn merge_detail_value(existing: &pb::Value, incoming: &pb::Value) -> Option<pb::Value> {
	use pb::value::Kind;
	let kind = match (existing.kind.as_ref(), incoming.kind.as_ref()) {
		(Some(Kind::Int(left)), Some(Kind::Int(right))) => Kind::Int(left.checked_add(*right)?),
		(Some(Kind::Uint(left)), Some(Kind::Uint(right))) => Kind::Uint(left.checked_add(*right)?),
		_ if existing == incoming => return Some(existing.clone()),
		_ => return None,
	};
	Some(pb::Value { kind: Some(kind) })
}

fn push_filter(sql: &mut String, values: &mut Vec<Value>, clause: &str, value: Value) {
	sql.push_str(clause);
	values.push(value);
}

fn push_enum_filter<T>(sql: &mut String, values: &mut Vec<Value>, column: &str, selected: &[T])
where
	T: Copy,
	&'static str: From<T>,
{
	if selected.is_empty() {
		return;
	}
	sql.push_str(" AND ");
	sql.push_str(column);
	sql.push_str(" IN (");
	for (index, value) in selected.iter().copied().enumerate() {
		if index != 0 {
			sql.push(',');
		}
		sql.push('?');
		values.push(Value::Text(<&'static str>::from(value).to_owned()));
	}
	sql.push(')');
}

const USAGE_AGGREGATE_COLUMNS: &str = "
	SUM(input_tokens), SUM(output_tokens), SUM(cache_read_tokens), SUM(cache_write_tokens),
	CASE WHEN MIN(accuracy) = MAX(accuracy) THEN MIN(accuracy) ELSE 3 END,
	SUM(COALESCE(total_tokens, input_tokens + output_tokens + cache_read_tokens +
	cache_write_tokens)),
	SUM(context_tokens), SUM(orchestration_input_tokens), SUM(orchestration_cache_read_tokens),
	SUM(orchestration_output_tokens), SUM(premium_requests), SUM(reasoning_tokens),
	SUM(cache_ephemeral_5m_tokens), SUM(cache_ephemeral_1h_tokens), SUM(web_search_requests),
	SUM(web_fetch_requests), SUM(cost_nanos_usd), MAX(cost_estimated), SUM(input_nanos_usd),
	SUM(output_nanos_usd), SUM(cache_read_nanos_usd), SUM(cache_write_nanos_usd), COUNT(*),
	SUM(failed), SUM(COALESCE(duration_ms, 0)), COUNT(DISTINCT session_id)";

fn usage_sql(query: &UsageQuery) -> Result<(String, Vec<Value>), Error> {
	let mut sql = String::from(
		"WITH scoped AS (SELECT r.*, s.project, s.kind AS session_kind FROM receipts r JOIN \
		 sessions s ON s.id = r.session_id WHERE 1=1",
	);
	let mut values = Vec::new();
	if let Some(since) = query.since_ms {
		push_filter(&mut sql, &mut values, " AND r.ts_ms >= ?", sql_value(since, "since_ms")?);
	}
	if let Some(until) = query.until_ms {
		push_filter(&mut sql, &mut values, " AND r.ts_ms <= ?", sql_value(until, "until_ms")?);
	}
	if let Some(session) = &query.session {
		push_filter(
			&mut sql,
			&mut values,
			" AND r.session_id = ?",
			Value::Text(session.0.to_string()),
		);
	}
	if let Some(project) = &query.project {
		push_filter(&mut sql, &mut values, " AND s.project = ?", Value::Text(project.to_string()));
	}
	if !query.include_subagents {
		push_filter(
			&mut sql,
			&mut values,
			" AND s.kind != ?",
			Value::Text(<&'static str>::from(SessionKind::Subagent).to_owned()),
		);
	}
	sql.push_str(") SELECT ");
	let mut group_expressions: SmallVec<&'static str, 4> = SmallVec::new();
	for dimension in &query.group_by {
		let expression = match dimension {
			UsageDimension::Project => "project",
			UsageDimension::Provider => "provider",
			UsageDimension::Model => "provider || '/' || model",
			UsageDimension::SessionId => "session_id",
			UsageDimension::SessionKind => "session_kind",
		};
		group_expressions.push(expression);
		sql.push_str(expression);
		sql.push_str(", ");
	}
	if let Some(expression) = bucket_expression(query.bucket) {
		group_expressions.push(expression);
		sql.push_str(expression);
		sql.push_str(" AS bucket_start_ms, ");
	}
	sql.push_str(USAGE_AGGREGATE_COLUMNS);
	sql.push_str(" FROM scoped");
	if group_expressions.is_empty() {
		sql.push_str(" HAVING COUNT(*) > 0");
	} else {
		sql.push_str(" GROUP BY ");
		for (index, expression) in group_expressions.iter().enumerate() {
			if index != 0 {
				sql.push_str(", ");
			}
			sql.push_str(expression);
		}
		sql.push_str(" HAVING COUNT(*) > 0");
		sql.push_str(" ORDER BY ");
		for (index, expression) in group_expressions.iter().enumerate() {
			if index != 0 {
				sql.push_str(", ");
			}
			sql.push_str(expression);
		}
	}
	Ok((sql, values))
}

fn merge_bucket_details(
	connection: &Connection,
	query: &UsageQuery,
	buckets: &mut [UsageBucket],
) -> Result<(), Error> {
	if buckets.is_empty() {
		return Ok(());
	}
	let (sql, values) = usage_detail_sql(query)?;
	let mut statement = connection.prepare(&sql)?;
	let rows = statement.query_map(params_from_iter(values), |row| {
		let mut offset = 0;
		let mut key: SmallVec<(UsageDimension, Str), 3> = SmallVec::new();
		for dimension in &query.group_by {
			key.push((*dimension, Str::from(row.get::<_, String>(offset)?)));
			offset += 1;
		}
		let start_ms = if query.bucket == UsageBucketWidth::None {
			None
		} else {
			let value = row.get(offset)?;
			offset += 1;
			Some(value)
		};
		Ok((key, start_ms, row.get::<_, Vec<u8>>(offset)?))
	})?;
	for row in rows {
		let (key, start_ms, encoded) = row?;
		if let Some(bucket) = buckets
			.iter_mut()
			.find(|bucket| bucket.key == key && bucket.start_ms == start_ms)
		{
			let usage = pb::Usage::decode(encoded.as_slice())?;
			merge_detail(&mut bucket.usage.detail, usage.detail);
		}
	}
	Ok(())
}

fn usage_detail_sql(query: &UsageQuery) -> Result<(String, Vec<Value>), Error> {
	let mut sql = String::from(
		"WITH scoped AS (SELECT r.*, s.project, s.kind AS session_kind FROM receipts r
		 JOIN sessions s ON s.id = r.session_id WHERE 1=1",
	);
	let mut values = Vec::new();
	if let Some(since) = query.since_ms {
		push_filter(&mut sql, &mut values, " AND r.ts_ms >= ?", sql_value(since, "since_ms")?);
	}
	if let Some(until) = query.until_ms {
		push_filter(&mut sql, &mut values, " AND r.ts_ms <= ?", sql_value(until, "until_ms")?);
	}
	if let Some(session) = &query.session {
		push_filter(
			&mut sql,
			&mut values,
			" AND r.session_id = ?",
			Value::Text(session.0.to_string()),
		);
	}
	if let Some(project) = &query.project {
		push_filter(&mut sql, &mut values, " AND s.project = ?", Value::Text(project.to_string()));
	}
	if !query.include_subagents {
		push_filter(
			&mut sql,
			&mut values,
			" AND s.kind != ?",
			Value::Text(<&'static str>::from(SessionKind::Subagent).to_owned()),
		);
	}
	sql.push_str(") SELECT ");
	for dimension in &query.group_by {
		let expression = match dimension {
			UsageDimension::Project => "project",
			UsageDimension::Provider => "provider",
			UsageDimension::Model => "provider || '/' || model",
			UsageDimension::SessionId => "session_id",
			UsageDimension::SessionKind => "session_kind",
		};
		sql.push_str(expression);
		sql.push_str(", ");
	}
	if let Some(expression) = bucket_expression(query.bucket) {
		sql.push_str(expression);
		sql.push_str(", ");
	}
	sql.push_str("usage FROM scoped ORDER BY event_index");
	Ok((sql, values))
}

const fn bucket_expression(bucket: UsageBucketWidth) -> Option<&'static str> {
	match bucket {
		UsageBucketWidth::None => None,
		UsageBucketWidth::Hour => Some("(ts_ms / 3600000) * 3600000"),
		UsageBucketWidth::Day => Some("(ts_ms / 86400000) * 86400000"),
		UsageBucketWidth::Week => Some("(ts_ms / 604800000) * 604800000"),
		UsageBucketWidth::Month => {
			Some("CAST(strftime('%s', ts_ms / 1000, 'unixepoch', 'start of month') AS INTEGER) * 1000")
		},
	}
}

fn decode_usage_bucket(
	row: &rusqlite::Row<'_>,
	query: &UsageQuery,
) -> rusqlite::Result<UsageBucket> {
	let mut offset = 0;
	let mut key = SmallVec::new();
	for dimension in &query.group_by {
		key.push((*dimension, Str::from(row.get::<_, String>(offset)?)));
		offset += 1;
	}
	let start_ms = if query.bucket == UsageBucketWidth::None {
		None
	} else {
		let value = row.get(offset)?;
		offset += 1;
		Some(value)
	};
	let orchestration = pb::OrchestrationUsage {
		input_tokens:      row.get(offset + 7)?,
		cache_read_tokens: row.get(offset + 8)?,
		output_tokens:     row.get(offset + 9)?,
	};
	let cache_ttl = pb::CacheTtlUsage {
		ephemeral_5m_tokens: row.get(offset + 12)?,
		ephemeral_1h_tokens: row.get(offset + 13)?,
	};
	let server_tools = pb::ServerToolUsage {
		web_search_requests: row.get(offset + 14)?,
		web_fetch_requests:  row.get(offset + 15)?,
	};
	let usage = pb::Usage {
		input_tokens:       row.get(offset)?,
		output_tokens:      row.get(offset + 1)?,
		cache_read_tokens:  row.get(offset + 2)?,
		cache_write_tokens: row.get(offset + 3)?,
		accuracy:           row.get(offset + 4)?,
		detail:             None,
		total_tokens:       row.get(offset + 5)?,
		context_tokens:     row.get(offset + 6)?,
		orchestration:      Some(orchestration),
		premium_requests:   row.get(offset + 10)?,
		reasoning_tokens:   row.get(offset + 11)?,
		cache_ttl:          Some(cache_ttl),
		server_tools:       Some(server_tools),
	};
	let cost = pb::Cost {
		nanos_usd:             row.get(offset + 16)?,
		estimated:             row.get(offset + 17)?,
		input_nanos_usd:       row.get(offset + 18)?,
		output_nanos_usd:      row.get(offset + 19)?,
		cache_read_nanos_usd:  row.get(offset + 20)?,
		cache_write_nanos_usd: row.get(offset + 21)?,
	};
	Ok(UsageBucket {
		key,
		start_ms,
		usage,
		cost,
		requests: row.get(offset + 22)?,
		errors: row.get(offset + 23)?,
		duration_ms: row.get(offset + 24)?,
		sessions: row.get(offset + 25)?,
	})
}

fn sql_u64(value: u64, field: &'static str) -> Result<i64, Error> {
	i64::try_from(value).map_err(|_| Error::IntegerRange { field })
}

fn sql_value(value: u64, field: &'static str) -> Result<Value, Error> {
	Ok(Value::Integer(sql_u64(value, field)?))
}
