//! Consent-gated local statistics database.
//!
//! The schema intentionally has no column capable of storing raw prompts.
//! User-message rows contain derived counters only.

use std::{
	fmt, fs, io,
	path::{Path, PathBuf},
};

use omp_core::Str;
use omp_observability::{sentiment::UserSentimentMetrics, stats::LocalAnalyticsConsent};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use thiserror::Error;

const SCHEMA_VERSION: i64 = 1;

/// One prompt-free message fact extracted from a durable transcript entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageFact<'a> {
	/// Canonical journal path.
	pub session_file:  &'a Path,
	/// Stable event identity within the journal.
	pub entry_id:      &'a str,
	/// Event time in epoch milliseconds when present.
	pub timestamp_ms:  Option<u64>,
	/// Serving provider when present.
	pub provider:      Option<&'a str>,
	/// Serving model when present.
	pub model:         Option<&'a str>,
	/// Transcript role/category, never message content.
	pub role:          &'a str,
	/// `main`, `subagent`, or `advisor`.
	pub agent_type:    AgentType,
	/// Input token count when present.
	pub input_tokens:  Option<u64>,
	/// Output token count when present.
	pub output_tokens: Option<u64>,
}

/// One prompt-free tool invocation fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCallFact<'a> {
	/// Canonical journal path.
	pub session_file: &'a Path,
	/// Stable event identity within the journal.
	pub entry_id:     &'a str,
	/// Tool name.
	pub tool_name:    &'a str,
	/// Settled result category.
	pub outcome:      Option<&'a str>,
	/// Execution duration when present.
	pub duration_ms:  Option<u64>,
	/// Agent role owning the call.
	pub agent_type:   AgentType,
}

/// Transcript agent classification retained by the local index.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AgentType {
	/// Primary interactive/headless agent.
	#[default]
	Main,
	/// Task/subagent child.
	Subagent,
	/// Read-only advisor/reviewer child.
	Advisor,
}

/// Statistics storage failure.
#[derive(Debug, Error)]
pub enum StatsDbError {
	/// Database parent directory could not be created.
	#[error("failed to create statistics database directory")]
	Directory(#[source] io::Error),
	/// SQLite operation failed.
	#[error("statistics database operation failed")]
	Database(#[from] rusqlite::Error),
	/// A Rust counter exceeds SQLite's signed integer range.
	#[error("statistics counter exceeds SQLite range: {field}")]
	IntegerRange {
		/// Field being converted.
		field: &'static str,
	},
}

/// WAL-backed local derived-counter store.
pub struct StatsDb {
	connection: Mutex<Connection>,
	consent:    LocalAnalyticsConsent,
	path:       PathBuf,
}

impl fmt::Debug for StatsDb {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("StatsDb")
			.field("path", &self.path)
			.field("consent", &self.consent)
			.finish_non_exhaustive()
	}
}

impl StatsDb {
	/// Opens `~/.omp/stats.db` below an already-resolved home directory.
	pub fn open_default(home: &Path, consent: LocalAnalyticsConsent) -> Result<Self, StatsDbError> {
		Self::open(home.join(".omp/stats.db"), consent)
	}

	/// Opens a statistics store, enables WAL, and applies monotonic migrations.
	#[tracing::instrument(
		name = "stats_store_open",
		level = "debug",
		skip_all,
		fields(path = %path.as_ref().display())
	)]
	pub fn open(
		path: impl AsRef<Path>,
		consent: LocalAnalyticsConsent,
	) -> Result<Self, StatsDbError> {
		let path = path.as_ref();
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).map_err(StatsDbError::Directory)?;
		}
		let mut connection = Connection::open(path)?;
		connection.pragma_update(None, "journal_mode", "WAL")?;
		connection.pragma_update(None, "foreign_keys", "ON")?;
		connection.pragma_update(None, "busy_timeout", 5_000_i64)?;
		migrate(&mut connection)?;
		Ok(Self { connection: Mutex::new(connection), consent, path: path.to_owned() })
	}

	/// Returns the database path.
	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Returns whether derived user counters may be ingested.
	pub const fn consent(&self) -> LocalAnalyticsConsent {
		self.consent
	}

	/// Inserts structural message metadata. Raw prompt text is not accepted by
	/// this API or representable in the schema.
	pub fn insert_message(&self, fact: &MessageFact<'_>) -> Result<(), StatsDbError> {
		if self.consent == LocalAnalyticsConsent::Disabled {
			return Ok(());
		}
		self.connection.lock().execute(
			"INSERT INTO \
			 messages(session_file,entry_id,timestamp_ms,provider,model,role,agent_type,input_tokens,\
			 output_tokens)
			 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
			 ON CONFLICT(session_file,entry_id) DO UPDATE SET
			 timestamp_ms=excluded.timestamp_ms,provider=excluded.provider,model=excluded.model,
			 role=excluded.role,agent_type=excluded.agent_type,input_tokens=excluded.input_tokens,
			 output_tokens=excluded.output_tokens",
			params![
				fact.session_file.to_string_lossy(),
				fact.entry_id,
				optional_sql_u64(fact.timestamp_ms, "timestamp_ms")?,
				fact.provider,
				fact.model,
				fact.role,
				fact.agent_type.to_string(),
				optional_sql_u64(fact.input_tokens, "input_tokens")?,
				optional_sql_u64(fact.output_tokens, "output_tokens")?,
			],
		)?;
		Ok(())
	}

	/// Inserts behavioral counters derived transiently from one user prompt.
	pub fn insert_user_metrics(
		&self,
		session_file: &Path,
		entry_id: &str,
		metrics: UserSentimentMetrics,
	) -> Result<(), StatsDbError> {
		if self.consent == LocalAnalyticsConsent::Disabled {
			return Ok(());
		}
		self.connection.lock().execute(
			"INSERT INTO \
			 user_messages(session_file,entry_id,chars,words,yelling,profanity,anguish,negation,\
			 repetition,blame)
			 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
			 ON CONFLICT(session_file,entry_id) DO UPDATE SET chars=excluded.chars,words=excluded.words,
			 yelling=excluded.yelling,profanity=excluded.profanity,anguish=excluded.anguish,
			 negation=excluded.negation,repetition=excluded.repetition,blame=excluded.blame",
			params![
				session_file.to_string_lossy(),
				entry_id,
				sql_u64(metrics.chars, "chars")?,
				sql_u64(metrics.words, "words")?,
				sql_u64(metrics.yelling, "yelling")?,
				sql_u64(metrics.profanity, "profanity")?,
				sql_u64(metrics.anguish, "anguish")?,
				sql_u64(metrics.negation, "negation")?,
				sql_u64(metrics.repetition, "repetition")?,
				sql_u64(metrics.blame, "blame")?,
			],
		)?;
		Ok(())
	}

	/// Inserts one tool-call outcome without arguments or output payloads.
	pub fn insert_tool_call(&self, fact: &ToolCallFact<'_>) -> Result<(), StatsDbError> {
		if self.consent == LocalAnalyticsConsent::Disabled {
			return Ok(());
		}
		self.connection.lock().execute(
			"INSERT INTO tool_calls(session_file,entry_id,tool_name,outcome,duration_ms,agent_type)
			 VALUES(?1,?2,?3,?4,?5,?6)
			 ON CONFLICT(session_file,entry_id) DO UPDATE SET tool_name=excluded.tool_name,
			 outcome=excluded.outcome,duration_ms=excluded.duration_ms,agent_type=excluded.agent_type",
			params![
				fact.session_file.to_string_lossy(),
				fact.entry_id,
				fact.tool_name,
				fact.outcome,
				optional_sql_u64(fact.duration_ms, "duration_ms")?,
				fact.agent_type.to_string(),
			],
		)?;
		Ok(())
	}

	/// Returns the complete-line byte watermark for one journal.
	pub fn file_offset(&self, path: &Path) -> Result<u64, StatsDbError> {
		let value = self
			.connection
			.lock()
			.query_row(
				"SELECT byte_offset FROM file_offsets WHERE path=?1",
				[path.to_string_lossy().as_ref()],
				|row| row.get::<_, i64>(0),
			)
			.optional()?;
		Ok(value.unwrap_or(0).max(0) as u64)
	}

	/// Advances a journal watermark after all complete lines were committed.
	pub fn set_file_offset(
		&self,
		path: &Path,
		byte_offset: u64,
		modified_ns: Option<u64>,
	) -> Result<(), StatsDbError> {
		self.connection.lock().execute(
			"INSERT INTO file_offsets(path,byte_offset,modified_ns) VALUES(?1,?2,?3)
			 ON CONFLICT(path) DO UPDATE SET \
			 byte_offset=excluded.byte_offset,modified_ns=excluded.modified_ns",
			params![
				path.to_string_lossy(),
				sql_u64(byte_offset, "byte_offset")?,
				optional_sql_u64(modified_ns, "modified_ns")?
			],
		)?;
		Ok(())
	}

	/// Stores a non-secret ingestion metadata value.
	pub fn set_meta(&self, key: &str, value: &str) -> Result<(), StatsDbError> {
		self.connection.lock().execute(
			"INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET \
			 value=excluded.value",
			[key, value],
		)?;
		Ok(())
	}

	/// Reads a non-secret ingestion metadata value.
	pub fn meta(&self, key: &str) -> Result<Option<Str>, StatsDbError> {
		Ok(self
			.connection
			.lock()
			.query_row("SELECT value FROM meta WHERE key=?1", [key], |row| row.get::<_, String>(0))
			.optional()?
			.map(Str::new))
	}
}

#[tracing::instrument(
	name = "storage_migration",
	level = "debug",
	skip_all,
	fields(database = "stats", target_version = SCHEMA_VERSION)
)]
fn migrate(connection: &mut Connection) -> Result<(), rusqlite::Error> {
	let version = connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
	if version >= SCHEMA_VERSION {
		return Ok(());
	}
	let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
	transaction.execute_batch(
		"CREATE TABLE IF NOT EXISTS messages(
		 session_file TEXT NOT NULL, entry_id TEXT NOT NULL, timestamp_ms INTEGER,
		 provider TEXT, model TEXT, role TEXT NOT NULL, agent_type TEXT NOT NULL,
		 input_tokens INTEGER, output_tokens INTEGER,
		 PRIMARY KEY(session_file,entry_id));
		 CREATE TABLE IF NOT EXISTS user_messages(
		 session_file TEXT NOT NULL, entry_id TEXT NOT NULL, chars INTEGER NOT NULL,
		 words INTEGER NOT NULL, yelling INTEGER NOT NULL, profanity INTEGER NOT NULL,
		 anguish INTEGER NOT NULL, negation INTEGER NOT NULL, repetition INTEGER NOT NULL,
		 blame INTEGER NOT NULL, PRIMARY KEY(session_file,entry_id));
		 CREATE TABLE IF NOT EXISTS tool_calls(
		 session_file TEXT NOT NULL, entry_id TEXT NOT NULL, tool_name TEXT NOT NULL,
		 outcome TEXT, duration_ms INTEGER, agent_type TEXT NOT NULL,
		 PRIMARY KEY(session_file,entry_id));
		 CREATE TABLE IF NOT EXISTS file_offsets(
		 path TEXT PRIMARY KEY, byte_offset INTEGER NOT NULL, modified_ns INTEGER);
		 CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
		 CREATE INDEX IF NOT EXISTS messages_timestamp ON messages(timestamp_ms);
		 CREATE INDEX IF NOT EXISTS tool_calls_name ON tool_calls(tool_name);",
	)?;
	transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
	transaction.commit()?;
	tracing::info!(
		database = "stats",
		from_version = version,
		to_version = SCHEMA_VERSION,
		"storage migration completed"
	);
	Ok(())
}

fn sql_u64(value: u64, field: &'static str) -> Result<i64, StatsDbError> {
	i64::try_from(value).map_err(|_| StatsDbError::IntegerRange { field })
}

fn optional_sql_u64(value: Option<u64>, field: &'static str) -> Result<Option<i64>, StatsDbError> {
	value.map(|value| sql_u64(value, field)).transpose()
}
