//! SQLite query projection for autoresearch journal facts.
//!
//! The v4 transcript journal is authoritative. This module never invents a
//! lifecycle mutation: callers append a [`JournalFact`] first and synchronously
//! project that exact fact through [`Storage::append_and_project`]. A
//! projection can be rebuilt from journal sequence zero at any time.

use std::{
	env,
	error::Error as StdError,
	ffi::OsStr,
	fs, io,
	path::{Path, PathBuf},
	time::Duration,
};

use omp_core::Str;
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use super::{
	helpers::Measurement,
	types::{
		DispositionIntent, ExperimentStatus, JournalFact, MetricDirection, RunCompletion,
		SessionConfig,
	},
};

const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS projection_entries (
	journal_seq INTEGER PRIMARY KEY,
	kind TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
	id INTEGER PRIMARY KEY,
	name TEXT NOT NULL,
	goal TEXT,
	primary_metric TEXT NOT NULL,
	metric_unit TEXT NOT NULL,
	direction TEXT NOT NULL,
	branch TEXT,
	baseline_commit TEXT,
	current_segment INTEGER NOT NULL,
	max_iterations INTEGER,
	scope_paths_json TEXT NOT NULL,
	off_limits_json TEXT NOT NULL,
	constraints_json TEXT NOT NULL,
	secondary_metrics_json TEXT NOT NULL,
	notes TEXT NOT NULL,
	created_at INTEGER NOT NULL,
	updated_at INTEGER NOT NULL,
	closed_at INTEGER
);
CREATE TABLE IF NOT EXISTS runs (
	id INTEGER PRIMARY KEY,
	session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
	segment INTEGER NOT NULL,
	command TEXT NOT NULL,
	started_at INTEGER NOT NULL,
	completed_at INTEGER,
	duration_ms INTEGER,
	exit_code INTEGER,
	timed_out INTEGER NOT NULL DEFAULT 0,
	parsed_primary REAL,
	parsed_metrics_json TEXT,
	parsed_asi_json TEXT,
	pre_run_head TEXT,
	pre_dirty_paths_json TEXT NOT NULL,
	artifact_dir TEXT NOT NULL,
	status TEXT,
	description TEXT,
	metric REAL,
	metrics_json TEXT,
	asi_json TEXT,
	commit_hash TEXT,
	confidence REAL,
	tracked_paths_json TEXT,
	untracked_paths_json TEXT,
	scope_deviations_json TEXT,
	justification TEXT,
	disposition_started_at INTEGER,
	settled_at INTEGER,
	flagged INTEGER NOT NULL DEFAULT 0,
	flagged_reason TEXT,
	abandoned_at INTEGER
);
CREATE TABLE IF NOT EXISTS artifacts (
	id INTEGER PRIMARY KEY,
	run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
	kind TEXT NOT NULL,
	uri TEXT NOT NULL,
	bytes INTEGER NOT NULL,
	created_at INTEGER NOT NULL,
	UNIQUE(run_id, kind, uri)
);
CREATE INDEX IF NOT EXISTS sessions_active_branch_idx ON sessions(branch, closed_at, id);
CREATE INDEX IF NOT EXISTS runs_session_segment_idx ON runs(session_id, segment, id);
CREATE INDEX IF NOT EXISTS runs_pending_idx ON runs(session_id, settled_at, abandoned_at, id);
CREATE INDEX IF NOT EXISTS artifacts_run_idx ON artifacts(run_id, id);
"#;

/// Appends typed autoresearch facts to the owning v4 transcript journal.
pub trait JournalAppender {
	/// Concrete journal failure.
	type Error: StdError + Send + Sync + 'static;

	/// Durably appends `fact` and returns its physical journal sequence.
	fn append_autoresearch(&mut self, fact: &JournalFact) -> Result<u64, Self::Error>;
}

/// Failure while appending and projecting one fact.
#[derive(Debug, thiserror::Error)]
pub enum RecordError<E: StdError + 'static> {
	/// The authoritative journal append failed, so no projection was attempted.
	#[error("autoresearch journal append failed")]
	Journal(#[source] E),
	/// The journal append succeeded but its rebuildable query projection failed.
	#[error("autoresearch journal append succeeded but SQLite projection failed")]
	Projection(#[source] StorageError),
}

/// Autoresearch projection failure.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
	/// Projection directory creation failed.
	#[error("failed to create autoresearch projection directory {path:?}")]
	CreateDirectory {
		/// Directory being created.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: io::Error,
	},
	/// SQLite open, schema, query, or transaction failed.
	#[error("autoresearch SQLite projection failed")]
	Sqlite(#[from] rusqlite::Error),
	/// A journal payload could not be represented as canonical JSON.
	#[error("autoresearch journal projection JSON encoding failed")]
	Json(#[from] serde_json::Error),
	/// The data directory contract is unavailable.
	#[error("HOME, OMP_DATA_DIR, or OMP_AUTORESEARCH_DB_DIR must be set")]
	MissingDataDirectory,
}

/// Filesystem layout for one repository's autoresearch projection and
/// artifacts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoragePaths {
	/// SQLite projection file.
	pub database:    PathBuf,
	/// Per-project artifact directory.
	pub project_dir: PathBuf,
}

impl StoragePaths {
	/// Resolves the per-project layout and `OMP_AUTORESEARCH_DB_DIR` override.
	pub fn resolve(repository_root: &Path) -> Result<Self, StorageError> {
		let encoded = encode_project_key(repository_root.as_os_str());
		if let Some(root) = env::var_os("OMP_AUTORESEARCH_DB_DIR").filter(|value| !value.is_empty()) {
			let root = PathBuf::from(root);
			return Ok(Self {
				database:    root.join(format!("{encoded}.db")),
				project_dir: root.join(encoded),
			});
		}
		let root = if let Some(root) = env::var_os("OMP_DATA_DIR").filter(|value| !value.is_empty()) {
			PathBuf::from(root)
		} else {
			let home = env::var_os("HOME").ok_or(StorageError::MissingDataDirectory)?;
			PathBuf::from(home).join(".local/share/omp")
		};
		let root = root.join("autoresearch");
		Ok(Self { database: root.join(format!("{encoded}.db")), project_dir: root.join(encoded) })
	}
}

/// Open SQLite projection handle.
pub struct Storage {
	connection: Connection,
	paths:      StoragePaths,
}

impl Storage {
	/// Opens or creates one WAL-mode projection with a five-second busy timeout.
	pub fn open(paths: StoragePaths) -> Result<Self, StorageError> {
		let parent = paths.database.parent().unwrap_or(Path::new("."));
		fs::create_dir_all(parent)
			.map_err(|source| StorageError::CreateDirectory { path: parent.to_path_buf(), source })?;
		fs::create_dir_all(&paths.project_dir).map_err(|source| StorageError::CreateDirectory {
			path: paths.project_dir.clone(),
			source,
		})?;
		let connection = Connection::open(&paths.database)?;
		connection.busy_timeout(BUSY_TIMEOUT)?;
		connection.execute_batch(SCHEMA)?;
		connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
		Ok(Self { connection, paths })
	}

	/// Returns the projection and artifact layout.
	pub const fn paths(&self) -> &StoragePaths {
		&self.paths
	}

	/// Allocates the next session id while the single journal owner is
	/// serialized.
	pub fn next_session_id(&self) -> Result<i64, StorageError> {
		Ok(self
			.connection
			.query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM sessions", [], |row| row.get(0))?)
	}

	/// Allocates the next run id while the single journal owner is serialized.
	pub fn next_run_id(&self) -> Result<i64, StorageError> {
		Ok(self
			.connection
			.query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM runs", [], |row| row.get(0))?)
	}

	/// Appends to journal truth, then synchronously updates the SQLite query
	/// projection.
	pub fn append_and_project<J: JournalAppender>(
		&mut self,
		journal: &mut J,
		fact: &JournalFact,
	) -> Result<u64, RecordError<J::Error>> {
		let sequence = journal
			.append_autoresearch(fact)
			.map_err(RecordError::Journal)?;
		self
			.project(sequence, fact)
			.map_err(RecordError::Projection)?;
		Ok(sequence)
	}

	/// Idempotently projects one already-durable journal fact.
	pub fn project(&mut self, sequence: u64, fact: &JournalFact) -> Result<(), StorageError> {
		let tx = self.connection.transaction()?;
		let inserted = tx.execute(
			"INSERT OR IGNORE INTO projection_entries(journal_seq, kind) VALUES (?1, ?2)",
			params![sequence, fact_kind(fact)],
		)?;
		if inserted == 0 {
			tx.commit()?;
			return Ok(());
		}
		project_fact(&tx, fact)?;
		tx.commit()?;
		Ok(())
	}

	/// Rebuilds the complete query projection from journal order.
	pub fn rebuild<'a>(
		&mut self,
		facts: impl IntoIterator<Item = (u64, &'a JournalFact)>,
	) -> Result<(), StorageError> {
		let tx = self.connection.transaction()?;
		tx.execute_batch(
			"DELETE FROM artifacts; DELETE FROM runs; DELETE FROM sessions; DELETE FROM \
			 projection_entries;",
		)?;
		tx.commit()?;
		for (sequence, fact) in facts {
			self.project(sequence, fact)?;
		}
		Ok(())
	}

	/// Returns the latest active session for `branch`.
	pub fn active_session(
		&self,
		branch: Option<&str>,
	) -> Result<Option<ProjectedSession>, StorageError> {
		let mut statement = self.connection.prepare(
			"SELECT id, config_json FROM (SELECT id, \
			 json_object('name',name,'goal',goal,'primary_metric',primary_metric,'metric_unit',\
			 metric_unit,'direction',direction,'branch',branch,'baseline_commit',baseline_commit,'\
			 segment',current_segment,'max_iterations',max_iterations,'scope_paths',\
			 json(scope_paths_json),'off_limits',json(off_limits_json),'constraints',\
			 json(constraints_json),'secondary_metrics',json(secondary_metrics_json),'notes',notes) \
			 AS config_json FROM sessions WHERE closed_at IS NULL AND branch IS ?1 ORDER BY id DESC \
			 LIMIT 1)",
		)?;
		let row = statement
			.query_row(params![branch], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
			.optional()?;
		row.map(|(id, json)| Ok(ProjectedSession { id, config: serde_json::from_str(&json)? }))
			.transpose()
	}

	/// Returns the newest unsettled disposition on the current branch.
	pub fn pending_disposition(
		&self,
		branch: Option<&str>,
	) -> Result<Option<DispositionIntent>, StorageError> {
		let json = self
			.connection
			.query_row(
				"SELECT json_object(
				 'run_id',runs.id,'status',runs.status,'description',runs.description,
				 'metric',runs.metric,'metrics',json(runs.metrics_json),'asi',json(runs.asi_json),
				 'delta',json_object(
				  'tracked',json(runs.tracked_paths_json),
				  'untracked',json(runs.untracked_paths_json),
				  'deviations',json(runs.scope_deviations_json)
				 ),
				 'justification',runs.justification,'rollback_head',runs.pre_run_head,
				 'started_at_ms',runs.disposition_started_at
				 )
				 FROM runs JOIN sessions ON sessions.id=runs.session_id
				 WHERE sessions.branch IS ?1 AND runs.disposition_started_at IS NOT NULL
				  AND runs.settled_at IS NULL AND runs.abandoned_at IS NULL
				 ORDER BY runs.id DESC LIMIT 1",
				[branch],
				|row| row.get::<_, String>(0),
			)
			.optional()?;
		json
			.map(|json| serde_json::from_str(&json).map_err(StorageError::from))
			.transpose()
	}

	/// Counts settled, unflagged runs in one segment.
	pub fn segment_run_count(&self, session_id: i64, segment: u32) -> Result<u32, StorageError> {
		let count: i64 = self.connection.query_row(
			"SELECT COUNT(*) FROM runs WHERE session_id=?1 AND segment=?2 AND settled_at IS NOT NULL \
			 AND flagged=0",
			params![session_id, segment],
			|row| row.get(0),
		)?;
		Ok(count.try_into().unwrap_or(u32::MAX))
	}

	/// Returns the newest run awaiting disposition.
	pub fn pending_run(&self, session_id: i64) -> Result<Option<PendingRun>, StorageError> {
		let mut statement = self.connection.prepare(
			"SELECT id,segment,pre_run_head,pre_dirty_paths_json,parsed_primary,parsed_metrics_json,\
			 parsed_asi_json,completed_at,duration_ms,exit_code,timed_out,artifact_dir FROM runs \
			 WHERE session_id=?1 AND completed_at IS NOT NULL AND disposition_started_at IS NULL AND \
			 abandoned_at IS NULL ORDER BY id DESC LIMIT 1",
		)?;
		let row = statement
			.query_row([session_id], |row| {
				Ok((
					row.get::<_, i64>(0)?,
					row.get::<_, u32>(1)?,
					row.get::<_, Option<String>>(2)?,
					row.get::<_, String>(3)?,
					row.get::<_, Option<f64>>(4)?,
					row.get::<_, String>(5)?,
					row.get::<_, String>(6)?,
					row.get::<_, i64>(7)?,
					row.get::<_, i64>(8)?,
					row.get::<_, Option<i32>>(9)?,
					row.get::<_, bool>(10)?,
					row.get::<_, String>(11)?,
				))
			})
			.optional()?;
		row.map(|row| {
			Ok(PendingRun {
				id:              row.0,
				segment:         row.1,
				pre_run_head:    row.2.map(Str::from),
				pre_dirty_paths: serde_json::from_str(&row.3)?,
				completion:      RunCompletion {
					run_id:          row.0,
					completed_at_ms: row.7,
					duration_ms:     row.8,
					exit_code:       row.9,
					timed_out:       row.10,
					parsed_primary:  row.4,
					parsed_metrics:  serde_json::from_str(&row.5)?,
					parsed_asi:      serde_json::from_str(&row.6)?,
				},
				artifact_dir:    Str::from(row.11),
			})
		})
		.transpose()
	}

	/// Returns the newest process that started but never journaled completion.
	pub fn incomplete_run(&self, session_id: i64) -> Result<Option<IncompleteRun>, StorageError> {
		let row = self
			.connection
			.query_row(
				"SELECT id,pre_run_head,pre_dirty_paths_json FROM runs WHERE session_id=?1 AND \
				 completed_at IS NULL AND abandoned_at IS NULL ORDER BY id DESC LIMIT 1",
				[session_id],
				|row| {
					Ok((
						row.get::<_, i64>(0)?,
						row.get::<_, Option<String>>(1)?,
						row.get::<_, String>(2)?,
					))
				},
			)
			.optional()?;
		row.map(|(id, head, paths)| {
			Ok(IncompleteRun {
				id,
				pre_run_head: head.map(Str::from),
				pre_dirty_paths: serde_json::from_str(&paths)?,
			})
		})
		.transpose()
	}

	/// Returns settled measurements used for segment baseline and MAD math.
	pub fn measurements(&self, session_id: i64) -> Result<Vec<Measurement>, StorageError> {
		let mut statement = self.connection.prepare(
			"SELECT metric,status,segment,flagged FROM runs WHERE session_id=?1 AND settled_at IS \
			 NOT NULL AND metric IS NOT NULL ORDER BY id",
		)?;
		let rows = statement.query_map([session_id], |row| {
			let status = match row.get::<_, String>(1)?.as_str() {
				"keep" => ExperimentStatus::Keep,
				"discard" => ExperimentStatus::Discard,
				"crash" => ExperimentStatus::Crash,
				_ => ExperimentStatus::ChecksFailed,
			};
			Ok(Measurement { metric: row.get(0)?, status, segment: row.get(2)?, flagged: row.get(3)? })
		})?;
		Ok(rows.collect::<Result<_, _>>()?)
	}
}
/// Completed run waiting for `log_experiment`.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingRun {
	/// Stable run id.
	pub id:              i64,
	/// Segment at launch.
	pub segment:         u32,
	/// HEAD used for exact rollback.
	pub pre_run_head:    Option<Str>,
	/// Dirty user paths excluded from the run delta.
	pub pre_dirty_paths: Vec<Str>,
	/// Parsed process completion.
	pub completion:      RunCompletion,
	/// Run artifact directory.
	pub artifact_dir:    Str,
}
/// Started harness process that did not journal completion before a crash.
#[derive(Clone, Debug, PartialEq)]
pub struct IncompleteRun {
	/// Stable run id.
	pub id:              i64,
	/// HEAD captured before the run.
	pub pre_run_head:    Option<Str>,
	/// Dirty user paths present before the run.
	pub pre_dirty_paths: Vec<Str>,
}

/// One active session decoded from its projection.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedSession {
	/// Stable session id.
	pub id:     i64,
	/// Complete session configuration.
	pub config: SessionConfig,
}
fn encode_project_key(path: &OsStr) -> String {
	let text = path.to_string_lossy();
	let text = text.trim_start_matches(['/', '\\']);
	let mut encoded = String::with_capacity(text.len() + 4);
	encoded.push_str("--");
	for character in text.chars() {
		if matches!(character, '/' | '\\' | ':') {
			encoded.push('-');
		} else {
			encoded.push(character);
		}
	}
	encoded.push_str("--");
	encoded
}

fn fact_kind(fact: &JournalFact) -> &'static str {
	match fact {
		JournalFact::SessionOpened { .. } => "session_opened",
		JournalFact::SessionUpdated { .. } => "session_updated",
		JournalFact::SessionClosed { .. } => "session_closed",
		JournalFact::NotesUpdated { .. } => "notes_updated",
		JournalFact::RunStarted { .. } => "run_started",
		JournalFact::RunCompleted(_) => "run_completed",
		JournalFact::RunAbandoned { .. } => "run_abandoned",
		JournalFact::DispositionStarted(_) => "disposition_started",
		JournalFact::DispositionSettled(_) => "disposition_settled",
		JournalFact::RunFlagged { .. } => "run_flagged",
		JournalFact::ArtifactRecorded { .. } => "artifact_recorded",
	}
}

fn project_fact(tx: &Transaction<'_>, fact: &JournalFact) -> Result<(), StorageError> {
	match fact {
		JournalFact::SessionOpened { id, config, at_ms } => {
			upsert_session(tx, *id, config, *at_ms, true)?
		},
		JournalFact::SessionUpdated { id, config, at_ms } => {
			upsert_session(tx, *id, config, *at_ms, false)?
		},
		JournalFact::SessionClosed { id, at_ms } => {
			tx.execute("UPDATE sessions SET closed_at=?2,updated_at=?2 WHERE id=?1", params![
				id, at_ms
			])?;
		},
		JournalFact::NotesUpdated { id, notes, at_ms } => {
			tx.execute("UPDATE sessions SET notes=?2,updated_at=?3 WHERE id=?1", params![
				id,
				notes.as_str(),
				at_ms
			])?;
		},
		JournalFact::RunStarted { id, start } => {
			tx.execute(
				"INSERT OR REPLACE INTO \
				 runs(id,session_id,segment,command,started_at,pre_run_head,pre_dirty_paths_json,\
				 artifact_dir) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
				params![
					id,
					start.session_id,
					start.segment,
					start.command.as_str(),
					start.started_at_ms,
					start.pre_run_head.as_deref(),
					serde_json::to_string(&start.pre_dirty_paths)?,
					start.artifact_dir.as_str()
				],
			)?;
		},
		JournalFact::RunCompleted(completion) => {
			tx.execute(
				"UPDATE runs SET \
				 completed_at=?2,duration_ms=?3,exit_code=?4,timed_out=?5,parsed_primary=?6,\
				 parsed_metrics_json=?7,parsed_asi_json=?8 WHERE id=?1",
				params![
					completion.run_id,
					completion.completed_at_ms,
					completion.duration_ms,
					completion.exit_code,
					completion.timed_out,
					completion.parsed_primary,
					serde_json::to_string(&completion.parsed_metrics)?,
					serde_json::to_string(&completion.parsed_asi)?
				],
			)?;
		},
		JournalFact::RunAbandoned { run_id, at_ms } => {
			tx.execute(
				"UPDATE runs SET abandoned_at=?2 WHERE id=?1 AND settled_at IS NULL",
				params![run_id, at_ms],
			)?;
		},
		JournalFact::DispositionStarted(intent) => project_intent(tx, intent)?,
		JournalFact::DispositionSettled(settled) => {
			tx.execute(
				"UPDATE runs SET commit_hash=?2,confidence=?3,settled_at=?4 WHERE id=?1",
				params![
					settled.run_id,
					settled.commit.as_deref(),
					settled.confidence,
					settled.settled_at_ms
				],
			)?;
		},
		JournalFact::RunFlagged { run_id, reason, .. } => {
			tx.execute("UPDATE runs SET flagged=1,flagged_reason=?2 WHERE id=?1", params![
				run_id,
				reason.as_str()
			])?;
		},
		JournalFact::ArtifactRecorded { run_id, kind, uri, bytes, at_ms } => {
			tx.execute(
				"INSERT OR IGNORE INTO artifacts(run_id,kind,uri,bytes,created_at) \
				 VALUES(?1,?2,?3,?4,?5)",
				params![run_id, kind.as_str(), uri.as_str(), bytes, at_ms],
			)?;
		},
	}
	Ok(())
}

fn upsert_session(
	tx: &Transaction<'_>,
	id: i64,
	config: &SessionConfig,
	at_ms: i64,
	opening: bool,
) -> Result<(), StorageError> {
	let created_at = if opening {
		at_ms
	} else {
		tx.query_row("SELECT created_at FROM sessions WHERE id=?1", [id], |row| row.get(0))
			.optional()?
			.unwrap_or(at_ms)
	};
	tx.execute(
		"INSERT INTO \
		 sessions(id,name,goal,primary_metric,metric_unit,direction,branch,baseline_commit,\
		 current_segment,max_iterations,scope_paths_json,off_limits_json,constraints_json,\
		 secondary_metrics_json,notes,created_at,updated_at,closed_at) \
		 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,NULL) ON CONFLICT(id) DO \
		 UPDATE SET \
		 name=excluded.name,goal=excluded.goal,primary_metric=excluded.primary_metric,\
		 metric_unit=excluded.metric_unit,direction=excluded.direction,branch=excluded.branch,\
		 baseline_commit=excluded.baseline_commit,current_segment=excluded.current_segment,\
		 max_iterations=excluded.max_iterations,scope_paths_json=excluded.scope_paths_json,\
		 off_limits_json=excluded.off_limits_json,constraints_json=excluded.constraints_json,\
		 secondary_metrics_json=excluded.secondary_metrics_json,notes=excluded.notes,\
		 updated_at=excluded.updated_at,closed_at=NULL",
		params![
			id,
			config.name.as_str(),
			config.goal.as_deref(),
			config.primary_metric.as_str(),
			config.metric_unit.as_str(),
			direction(config.direction),
			config.branch.as_deref(),
			config.baseline_commit.as_deref(),
			config.segment,
			config.max_iterations,
			serde_json::to_string(&config.scope_paths)?,
			serde_json::to_string(&config.off_limits)?,
			serde_json::to_string(&config.constraints)?,
			serde_json::to_string(&config.secondary_metrics)?,
			config.notes.as_str(),
			created_at,
			at_ms
		],
	)?;
	Ok(())
}

fn project_intent(tx: &Transaction<'_>, intent: &DispositionIntent) -> Result<(), StorageError> {
	tx.execute(
		"UPDATE runs SET \
		 status=?2,description=?3,metric=?4,metrics_json=?5,asi_json=?6,tracked_paths_json=?7,\
		 untracked_paths_json=?8,scope_deviations_json=?9,justification=?10,disposition_started_at=?\
		 11 WHERE id=?1",
		params![
			intent.run_id,
			status(intent.status),
			intent.description.as_str(),
			intent.metric,
			serde_json::to_string(&intent.metrics)?,
			serde_json::to_string(&intent.asi)?,
			serde_json::to_string(&intent.delta.tracked)?,
			serde_json::to_string(&intent.delta.untracked)?,
			serde_json::to_string(&intent.delta.deviations)?,
			intent.justification.as_deref(),
			intent.started_at_ms
		],
	)?;
	Ok(())
}

fn direction(direction: MetricDirection) -> &'static str {
	match direction {
		MetricDirection::Lower => "lower",
		MetricDirection::Higher => "higher",
	}
}

fn status(status: ExperimentStatus) -> &'static str {
	match status {
		ExperimentStatus::Keep => "keep",
		ExperimentStatus::Discard => "discard",
		ExperimentStatus::Crash => "crash",
		ExperimentStatus::ChecksFailed => "checks_failed",
	}
}

#[cfg(test)]
mod tests {
	use tempfile::TempDir;

	use super::*;
	use crate::autoresearch::types::{JournalFact, MetricDirection, SessionConfig};

	#[test]
	fn projection_is_wal_busy_bounded_and_idempotent() {
		let directory = TempDir::new().expect("tempdir");
		let paths = StoragePaths {
			database:    directory.path().join("state.db"),
			project_dir: directory.path().join("project"),
		};
		let mut storage = Storage::open(paths).expect("open");
		let fact = JournalFact::SessionOpened {
			id:     1,
			config: SessionConfig {
				name:              "latency".into(),
				goal:              None,
				primary_metric:    "latency_ms".into(),
				metric_unit:       "ms".into(),
				direction:         MetricDirection::Lower,
				branch:            Some("autoresearch/latency-20260822".into()),
				baseline_commit:   Some("abc".into()),
				segment:           0,
				max_iterations:    Some(5),
				scope_paths:       vec!["src".into()],
				off_limits:        Vec::new(),
				constraints:       Vec::new(),
				secondary_metrics: Vec::new(),
				notes:             Str::default(),
			},
			at_ms:  1,
		};
		storage.project(7, &fact).expect("project");
		storage.project(7, &fact).expect("project twice");
		assert_eq!(storage.next_session_id().expect("next"), 2);
		let mode: String = storage
			.connection
			.pragma_query_value(None, "journal_mode", |row| row.get(0))
			.expect("mode");
		assert_eq!(mode.to_ascii_lowercase(), "wal");
	}
}
