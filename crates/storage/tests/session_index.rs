//! Integration tests for the durable session index.

use std::{
	collections::BTreeMap,
	io, slice,
	sync::{
		Arc, Barrier, Mutex,
		atomic::{AtomicBool, Ordering},
	},
	thread,
};

use omp_core::{Str, sf};
use omp_proto::{
	inference::v1::{self as pb, usage, value},
	thread::v1::{self as thread_pb, item},
};
use omp_storage::{
	index::{
		self, EventProjection, IndexAuthority, IndexedEvent, IndexedWriteError, JournalPosition,
		NewSession, RepairRecord, SessionFilter, SessionIndex, SessionKind, SessionRenameObserver,
		UsageBucketWidth, UsageDimension, UsageQuery,
	},
	maintenance::MaintenanceMode,
	transcript::{SessionId, TitleSource},
};
use rusqlite::Connection;
use smallvec::smallvec;
use tempfile::tempdir;

fn session_id(value: &str) -> SessionId {
	SessionId(Str::from(value))
}

fn create(index: &SessionIndex, id: &SessionId) {
	create_after_journal(index, id, || {});
}

fn create_child(index: &SessionIndex, id: &SessionId, parent: &SessionId) {
	index
		.create_session(
			&NewSession {
				id,
				cwd: "/workspace/project",
				project: "/workspace/project",
				created_ms: 2_000,
				kind: SessionKind::Interactive,
				parent: Some(parent),
				remote: false,
			},
			|| Ok::<_, io::Error>(((), 64)),
		)
		.expect("write child header and session index row");
}

fn create_after_journal(index: &SessionIndex, id: &SessionId, after_journal: impl FnOnce()) {
	index
		.create_session(
			&NewSession {
				id,
				cwd: "/workspace/project",
				project: "/workspace/project",
				created_ms: 1_000,
				kind: SessionKind::Interactive,
				parent: None,
				remote: false,
			},
			|| {
				after_journal();
				Ok::<_, io::Error>(((), 64))
			},
		)
		.expect("write header and session index row");
}

fn create_kind(index: &SessionIndex, id: &SessionId, kind: SessionKind) {
	index
		.create_session(
			&NewSession {
				id,
				cwd: "/workspace/project",
				project: "/workspace/project",
				created_ms: 1_000,
				kind,
				parent: None,
				remote: false,
			},
			|| Ok::<_, io::Error>(((), 64)),
		)
		.expect("write header and session index row");
}

fn append_prompt(
	index: &SessionIndex,
	session: &SessionId,
	event_index: u64,
	ts_ms: u64,
	prompt: &str,
) {
	index
		.append(
			&IndexedEvent {
				session,
				ts_ms,
				kind: "prompt",
				projection: EventProjection::Prompt { text: prompt },
			},
			|| {
				Ok::<_, io::Error>(((), JournalPosition {
					event_index,
					byte_watermark: 128 + event_index,
				}))
			},
		)
		.expect("append prompt");
}

#[derive(Default)]
struct RenameObserver(Mutex<Vec<(Str, Option<Str>)>>);

impl SessionRenameObserver for RenameObserver {
	fn renamed(&self, session: &SessionId, name: Option<&str>) {
		self
			.0
			.lock()
			.expect("rename observer lock")
			.push((session.0.clone(), name.map(Str::new)));
	}
}

#[test]
fn committed_ui_and_extension_renames_emit_once_while_create_and_resume_stay_silent() {
	let directory = tempdir().expect("temporary index");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("index");
	let observer = Arc::new(RenameObserver::default());
	index.bind_rename_observer(observer.clone());
	let session = session_id("live");
	create(&index, &session);
	assert!(observer.0.lock().expect("rename observer lock").is_empty());
	let _resumed = index
		.get(&session)
		.expect("resume lookup")
		.expect("live session");
	assert!(observer.0.lock().expect("rename observer lock").is_empty());

	for (event_index, title) in [(0, "  UI title  "), (1, "Extension title"), (2, "   ")] {
		index
			.append(
				&IndexedEvent {
					session:    &session,
					ts_ms:      2_000 + event_index,
					kind:       "title",
					projection: EventProjection::Title { title, source: TitleSource::User },
				},
				|| {
					Ok::<_, io::Error>(((), JournalPosition {
						event_index,
						byte_watermark: 128 + event_index,
					}))
				},
			)
			.expect("commit rename");
	}
	let observed = observer.0.lock().expect("rename observer lock");
	assert_eq!(observed.as_slice(), [
		(session.0.clone(), Some(sf!("UI title"))),
		(session.0.clone(), Some(sf!("Extension title"))),
		(session.0.clone(), None),
	]);
}

#[test]
fn prompt_history_lists_unique_prompts_newest_first() {
	let directory = tempdir().expect("temporary index");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("index");
	let first = session_id("first");
	let second = session_id("second");
	create(&index, &first);
	create(&index, &second);

	append_prompt(&index, &first, 0, 2_000, "older prompt");
	append_prompt(&index, &first, 1, 3_000, "duplicate prompt");
	append_prompt(&index, &second, 0, 4_000, "duplicate prompt");
	append_prompt(&index, &second, 1, 5_000, "newest prompt");

	assert_eq!(index.prompt_history("", 10).expect("recent prompt history"), vec![
		index::PromptHistoryEntry { prompt: Str::from("newest prompt"), ts_ms: Some(5_000) },
		index::PromptHistoryEntry { prompt: Str::from("duplicate prompt"), ts_ms: Some(4_000) },
		index::PromptHistoryEntry { prompt: Str::from("older prompt"), ts_ms: Some(2_000) },
	]);
}

#[test]
fn prompt_history_combines_prefix_and_token_and_substring_search() {
	let directory = tempdir().expect("temporary index");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("index");
	let session = session_id("search");
	create(&index, &session);
	append_prompt(&index, &session, 0, 2_000, "parser tests");
	append_prompt(&index, &session, 1, 3_000, "commit changes");
	append_prompt(&index, &session, 2, 4_000, "parser only");
	append_prompt(&index, &session, 3, 5_000, "tests only");

	assert_eq!(index.prompt_history("par tes", 10).expect("prefix search"), vec![
		index::PromptHistoryEntry { prompt: Str::from("parser tests"), ts_ms: Some(2_000) }
	]);
	assert_eq!(index.prompt_history("mit", 10).expect("infix search"), vec![
		index::PromptHistoryEntry { prompt: Str::from("commit changes"), ts_ms: Some(3_000) }
	]);
	assert!(
		index
			.prompt_history("parser changes", 10)
			.expect("token-AND search")
			.is_empty()
	);
}

#[test]
fn prompt_history_excludes_non_interactive_sessions() {
	let directory = tempdir().expect("temporary index");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("index");
	let interactive = session_id("interactive");
	let subagent = session_id("subagent");
	create_kind(&index, &interactive, SessionKind::Interactive);
	create_kind(&index, &subagent, SessionKind::Subagent);
	append_prompt(&index, &interactive, 0, 2_000, "visible prompt");
	append_prompt(&index, &subagent, 0, 3_000, "hidden prompt");

	assert_eq!(index.prompt_history("", 10).expect("interactive history"), vec![
		index::PromptHistoryEntry { prompt: Str::from("visible prompt"), ts_ms: Some(2_000) }
	]);
	assert!(
		index
			.prompt_history("hidden", 10)
			.expect("filtered prompt search")
			.is_empty()
	);
}

#[test]
fn opening_v4_index_migrates_prompt_history_without_fabricating_timestamps() {
	let directory = tempdir().expect("temporary index");
	let path = directory.path().join("sessions.sqlite3");
	let connection = Connection::open(&path).expect("open v4 database");
	connection
		.execute_batch(
			"CREATE TABLE index_meta (
			    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
			    schema_version INTEGER NOT NULL
			 );
			 INSERT INTO index_meta(singleton, schema_version) VALUES (1, 4);
			 CREATE TABLE sessions (
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
			 INSERT INTO sessions(
			    id, cwd, project, created_ms, updated_ms, status, kind, remote, journal_watermark
			 ) VALUES (
			    'legacy', '/workspace/project', '/workspace/project', 1000, 2000,
			    'complete', 'interactive', 0, 128
			 );
			 CREATE VIRTUAL TABLE prompts_fts USING fts5(
			    session_id UNINDEXED,
			    event_index UNINDEXED,
			    prompt,
			    tokenize = 'unicode61'
			 );
			 INSERT INTO prompts_fts(session_id, event_index, prompt)
			 VALUES ('legacy', 0, 'preserved prompt');",
		)
		.expect("create v4 schema");
	drop(connection);

	let index = SessionIndex::open(&path).expect("migrate v4 index");
	let migrated = Connection::open(&path).expect("inspect migrated database");
	let schema_version = migrated
		.query_row("SELECT schema_version FROM index_meta WHERE singleton = 1", [], |row| {
			row.get::<_, i64>(0)
		})
		.expect("read migrated schema version");
	assert_eq!(schema_version, 5);
	assert_eq!(
		index
			.prompt_history("", 10)
			.expect("migrated prompt history"),
		vec![index::PromptHistoryEntry { prompt: Str::from("preserved prompt"), ts_ms: None }]
	);
}

fn outcome(input: u64, output: u64) -> pb::Outcome {
	pb::Outcome {
		provider: "anthropic".to_owned(),
		model: "claude-opus".to_owned(),
		duration_ms: Some(250),
		usage: Some(pb::Usage {
			input_tokens:       input,
			output_tokens:      output,
			cache_read_tokens:  3,
			cache_write_tokens: 4,
			accuracy:           usage::Accuracy::Exact as i32,
			detail:             Some(pb::ValueMap {
				fields: [("anthropic/service_tier".to_owned(), pb::Value {
					kind: Some(value::Kind::String("standard".to_owned())),
				})]
				.into(),
			}),
			total_tokens:       Some(input + output + 7),
			context_tokens:     Some(8_192),
			orchestration:      Some(pb::OrchestrationUsage {
				input_tokens:      Some(5),
				cache_read_tokens: Some(6),
				output_tokens:     Some(7),
			}),
			premium_requests:   Some(1),
			reasoning_tokens:   Some(2),
			cache_ttl:          Some(pb::CacheTtlUsage {
				ephemeral_5m_tokens: Some(8),
				ephemeral_1h_tokens: Some(9),
			}),
			server_tools:       Some(pb::ServerToolUsage {
				web_search_requests: Some(10),
				web_fetch_requests:  Some(11),
			}),
		}),
		cost: Some(pb::Cost {
			nanos_usd:             1_000,
			estimated:             false,
			input_nanos_usd:       Some(400),
			output_nanos_usd:      Some(500),
			cache_read_nanos_usd:  Some(40),
			cache_write_nanos_usd: Some(60),
		}),
		..pb::Outcome::default()
	}
}

const fn receipt_event<'a>(
	id: &'a SessionId,
	outcome: &'a pb::Outcome,
	ts_ms: u64,
) -> IndexedEvent<'a> {
	IndexedEvent {
		session: id,
		ts_ms,
		kind: "omp.turn_receipt",
		projection: EventProjection::TurnReceipt { outcome, failed: false },
	}
}
fn append_projection(
	index: &SessionIndex,
	session: &SessionId,
	event_index: u64,
	kind: &str,
	projection: EventProjection<'_>,
) {
	index
		.append(
			&IndexedEvent { session, ts_ms: 3_000_u64.saturating_add(event_index), kind, projection },
			|| {
				Ok::<_, io::Error>(((), JournalPosition {
					event_index,
					byte_watermark: 128_u64.saturating_add(event_index.saturating_mul(64)),
				}))
			},
		)
		.expect("append indexed projection");
}

#[test]
fn lineage_preserves_exact_fork_checkpoint() {
	let directory = tempdir().expect("temporary directory");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("open index");
	let root = session_id("root");
	let child = session_id("child");
	create(&index, &root);
	create_child(&index, &child, &root);
	append_projection(&index, &child, 0, "forked_from", EventProjection::Fork {
		parent: &root,
		at:     Some(17),
	});

	let lineage = index.lineage(&child).expect("load exact lineage");
	assert_eq!(lineage.len(), 2);
	assert_eq!(lineage[0].id, root);
	assert_eq!(lineage[0].parent, None);
	assert_eq!(lineage[0].at, None);
	assert_eq!(lineage[1].id, child);
	assert_eq!(lineage[1].parent.as_ref(), Some(&root));
	assert_eq!(lineage[1].at, Some(17));
}

#[test]
fn command_usage_accumulates_and_survives_reopen() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("sessions.sqlite3");
	let expected =
		BTreeMap::from([(Str::new_static("model"), 2_u64), (Str::new_static("skill:review"), 1_u64)]);
	{
		let index = SessionIndex::open(&path).expect("open index");
		index
			.record_command_usage("model", 1_000)
			.expect("record model use");
		index
			.record_command_usage("model", 2_000)
			.expect("record second model use");
		index
			.record_command_usage("skill:review", 3_000)
			.expect("record skill use");
		assert_eq!(index.command_usage().expect("list command use"), expected);
	}

	let reopened = SessionIndex::open(&path).expect("reopen index");
	assert_eq!(
		reopened
			.command_usage()
			.expect("list command use after restart"),
		expected
	);
	reopened
		.record_command_usage("model", 4_000)
		.expect("record after restart");
	assert_eq!(reopened.command_usage().expect("list updated command use")["model"], 3);
}

#[test]
fn failed_journal_header_does_not_publish_a_session_row() {
	let directory = tempdir().expect("temporary directory");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("open index");
	let id = session_id("failed-header");
	let result = index.create_session(
		&NewSession {
			id:         &id,
			cwd:        "/workspace",
			project:    "/workspace",
			created_ms: 1,
			kind:       SessionKind::Interactive,
			parent:     None,
			remote:     false,
		},
		|| Err::<((), u64), _>(io::Error::other("disk full")),
	);
	assert!(matches!(result, Err(IndexedWriteError::Journal(_))));
	assert!(
		index
			.list(&SessionFilter::default())
			.expect("list index only")
			.sessions
			.is_empty()
	);
}

#[test]
fn failed_receipt_append_rolls_back_every_index_projection() {
	let directory = tempdir().expect("temporary directory");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("open index");
	let id = session_id("failed-receipt");
	create(&index, &id);
	let outcome = outcome(13, 21);
	let result = index.append(&receipt_event(&id, &outcome, 2_000), || {
		Err::<((), JournalPosition), _>(io::Error::other("journal write failed"))
	});
	assert!(matches!(result, Err(IndexedWriteError::Journal(_))));

	let page = index
		.list(&SessionFilter::default())
		.expect("list index only");
	assert_eq!(page.sessions.len(), 1);
	assert_eq!(page.sessions[0].entries, 0);
	assert_eq!(page.sessions[0].turns, 0);
	assert_eq!(page.sessions[0].journal_watermark, 64);
	assert_eq!(index.receipt(&id, 0).expect("query receipt"), None);
}

#[test]
fn receipt_persists_canonical_usage_and_sql_groups_every_scalar_field() {
	let directory = tempdir().expect("temporary directory");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("open index");
	let id = session_id("accounting");
	create(&index, &id);
	let first = outcome(13, 21);
	let second = outcome(34, 55);
	index
		.append(&receipt_event(&id, &first, 3_600_001), || {
			Ok::<_, io::Error>(((), JournalPosition { event_index: 0, byte_watermark: 256 }))
		})
		.expect("append first receipt");
	index
		.append(&receipt_event(&id, &second, 3_600_002), || {
			Ok::<_, io::Error>(((), JournalPosition { event_index: 1, byte_watermark: 512 }))
		})
		.expect("append second receipt");

	let exact = index
		.receipt(&id, 0)
		.expect("query exact receipt")
		.expect("receipt row");
	assert_eq!(exact.usage, first.usage.expect("fixture usage"));
	assert_eq!(exact.cost, first.cost.expect("fixture cost"));
	let sessions = index
		.list(&SessionFilter::default())
		.expect("list from index rows");
	assert_eq!(sessions.sessions[0].usage.input_tokens, 47);
	assert_eq!(sessions.sessions[0].cost.nanos_usd, 2_000);
	assert_eq!(sessions.sessions[0].models.as_slice(), [sf!("anthropic/claude-opus")]);

	let buckets = index
		.usage(&UsageQuery {
			group_by: smallvec![UsageDimension::Provider, UsageDimension::Model],
			bucket: UsageBucketWidth::Hour,
			..UsageQuery::default()
		})
		.expect("aggregate from SQLite rows");
	assert_eq!(buckets.len(), 1);
	let bucket = &buckets[0];
	assert_eq!(bucket.start_ms, Some(3_600_000));
	assert_eq!(bucket.requests, 2);
	assert_eq!(bucket.sessions, 1);
	assert_eq!(bucket.duration_ms, 500);
	assert_eq!(bucket.usage.input_tokens, 47);
	assert_eq!(bucket.usage.output_tokens, 76);
	assert_eq!(bucket.usage.cache_read_tokens, 6);
	assert_eq!(bucket.usage.cache_write_tokens, 8);
	assert_eq!(bucket.usage.premium_requests, Some(2));
	assert_eq!(bucket.usage.reasoning_tokens, Some(4));
	assert!(matches!(
		bucket
			.usage
			.detail
			.as_ref()
			.and_then(|detail| detail.fields.get("anthropic/service_tier"))
			.and_then(|value| value.kind.as_ref()),
		Some(value::Kind::String(value)) if value == "standard"
	));
	assert_eq!(
		bucket
			.usage
			.orchestration
			.as_ref()
			.and_then(|usage| usage.input_tokens),
		Some(10)
	);
	assert_eq!(
		bucket
			.usage
			.cache_ttl
			.as_ref()
			.and_then(|usage| usage.ephemeral_1h_tokens),
		Some(18)
	);
	assert_eq!(
		bucket
			.usage
			.server_tools
			.as_ref()
			.and_then(|usage| usage.web_fetch_requests),
		Some(22)
	);
	assert_eq!(bucket.cost.nanos_usd, 2_000);
	assert_eq!(bucket.cost.input_nanos_usd, Some(800));
}

#[test]
fn index_failure_after_journal_rolls_back_partial_sql_transaction() {
	let directory = tempdir().expect("temporary directory");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("open index");
	let id = session_id("missing-usage");
	create(&index, &id);
	let outcome = pb::Outcome {
		provider: "provider".to_owned(),
		model: "model".to_owned(),
		..pb::Outcome::default()
	};
	let event = receipt_event(&id, &outcome, 2_000);
	let result = index.append(&event, || {
		Ok::<_, io::Error>(("durable", JournalPosition { event_index: 0, byte_watermark: 128 }))
	});
	assert!(matches!(
		result,
		Err(IndexedWriteError::IndexAfterJournal {
			written:  "durable",
			position: Some(JournalPosition { event_index: 0, byte_watermark: 128 }),
			source:   index::Error::MissingUsage,
		})
	));
	let page = index
		.list(&SessionFilter::default())
		.expect("list rolled-back index");
	assert_eq!(page.sessions[0].entries, 0);
	assert_eq!(page.sessions[0].turns, 0);
	assert!(
		index
			.receipt(&id, 0)
			.expect("query rolled-back receipt")
			.is_none()
	);
}

#[test]
fn title_and_contains_kind_are_updated_only_after_journal_success() {
	let directory = tempdir().expect("temporary directory");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("open index");
	let id = session_id("title");
	create(&index, &id);
	let failed = IndexedEvent {
		session:    &id,
		ts_ms:      2_000,
		kind:       "omp.title",
		projection: EventProjection::Title { title: "Not durable", source: TitleSource::Assistant },
	};
	assert!(matches!(
		index.append(&failed, || Err::<((), JournalPosition), _>(io::Error::other("write failed"))),
		Err(IndexedWriteError::Journal(_))
	));
	let durable = IndexedEvent {
		projection: EventProjection::Title { title: "Durable", source: TitleSource::User },
		..failed
	};
	index
		.append(&durable, || {
			Ok::<_, io::Error>(((), JournalPosition { event_index: 0, byte_watermark: 128 }))
		})
		.expect("append durable title");

	let page = index
		.list(&SessionFilter { contains_kind: Some(sf!("omp.title")), ..SessionFilter::default() })
		.expect("query indexed kind");
	assert_eq!(page.sessions.len(), 1);
	assert_eq!(page.sessions[0].title.as_deref(), Some("Durable"));
	assert_eq!(page.sessions[0].title_source, Some(TitleSource::User));
}

#[test]
fn stale_position_is_reported_after_journal_without_advancing_index_watermarks() {
	let directory = tempdir().expect("temporary directory");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("open index");
	let id = session_id("watermark");
	create(&index, &id);
	let outcome = outcome(1, 2);
	let event = receipt_event(&id, &outcome, 2_000);
	index
		.append(&event, || {
			Ok::<_, io::Error>(((), JournalPosition { event_index: 0, byte_watermark: 128 }))
		})
		.expect("first append");
	let journal_ran = AtomicBool::new(false);
	let stale = index.append(&event, || {
		journal_ran.store(true, Ordering::SeqCst);
		Ok::<_, io::Error>(((), JournalPosition { event_index: 0, byte_watermark: 128 }))
	});
	assert!(journal_ran.load(Ordering::SeqCst));
	assert!(matches!(
		stale,
		Err(IndexedWriteError::IndexAfterJournal {
			written: (),
			position: Some(JournalPosition { event_index: 0, byte_watermark: 128 }),
			..
		})
	));
	let page = index.list(&SessionFilter::default()).expect("list index");
	assert_eq!(page.sessions[0].entries, 1);
	assert_eq!(page.sessions[0].turns, 1);
	assert_eq!(page.sessions[0].journal_watermark, 128);
}

#[test]
fn explicit_repair_has_a_separate_monotonic_watermark() {
	let directory = tempdir().expect("temporary directory");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("open index");
	let id = session_id("foreign");
	create(&index, &id);
	index
		.repair(&id, 256, [RepairRecord {
			event_index:    0,
			byte_watermark: 256,
			ts_ms:          5_000,
			kind:           "omp.title",
			projection:     EventProjection::Title {
				title:  "Imported",
				source: TitleSource::Imported,
			},
		}])
		.expect("explicit legacy repair");
	assert!(index.repair(&id, 256, []).is_err());
	let page = index
		.list(&SessionFilter::default())
		.expect("normal listing");
	assert_eq!(page.sessions[0].title.as_deref(), Some("Imported"));
}

#[test]
fn offline_thin_client_cache_is_stale_labeled_and_read_only() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("sessions.sqlite3");
	let id = session_id("remote-authority");
	{
		let index = SessionIndex::open(&path).expect("open authoritative index");
		create(&index, &id);
	}
	let cache = SessionIndex::open_offline_cache(&path, 9_999).expect("open offline cache");
	let page = cache
		.list(&SessionFilter::default())
		.expect("offline listing");
	assert_eq!(page.authority, IndexAuthority::OfflineCache { cached_at_ms: 9_999 });
	assert_eq!(page.sessions.len(), 1);

	let journal_ran = Arc::new(AtomicBool::new(false));
	let observed = Arc::clone(&journal_ran);
	let result = cache.append(
		&IndexedEvent {
			session:    &id,
			ts_ms:      2_000,
			kind:       "omp.title",
			projection: EventProjection::Title { title: "local fork", source: TitleSource::User },
		},
		move || {
			observed.store(true, Ordering::SeqCst);
			Ok::<_, io::Error>(((), JournalPosition { event_index: 0, byte_watermark: 128 }))
		},
	);
	assert!(matches!(result, Err(IndexedWriteError::IndexBeforeJournal(_))));
	assert!(!journal_ran.load(Ordering::SeqCst));
}

#[test]
fn remote_authority_reader_bootstraps_before_writer_and_remains_query_only() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("sessions.sqlite3");
	let reader = SessionIndex::open_authoritative_reader(&path).expect("bootstrap authority reader");
	assert_eq!(
		reader
			.list(&SessionFilter::default())
			.expect("empty authority listing")
			.authority,
		IndexAuthority::Authoritative
	);

	let writer = SessionIndex::open(&path).expect("writer opens while reader remains live");
	let id = session_id("remote-reader");
	create(&writer, &id);
	assert_eq!(
		reader
			.list(&SessionFilter::default())
			.expect("live authority listing")
			.sessions
			.len(),
		1
	);

	let journal_ran = Arc::new(AtomicBool::new(false));
	let observed = Arc::clone(&journal_ran);
	let result = reader.append(
		&IndexedEvent {
			session:    &id,
			ts_ms:      2_000,
			kind:       "omp.title",
			projection: EventProjection::Title { title: "forbidden", source: TitleSource::User },
		},
		move || {
			observed.store(true, Ordering::SeqCst);
			Ok::<_, io::Error>(((), JournalPosition { event_index: 0, byte_watermark: 128 }))
		},
	);
	assert!(matches!(
		result,
		Err(IndexedWriteError::IndexBeforeJournal(index::Error::ReadOnlyAuthority))
	));
	assert!(!journal_ran.load(Ordering::SeqCst));
}

#[test]
fn two_writer_connections_serialize_distinct_session_commits() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("sessions.sqlite3");
	let first = SessionIndex::open(&path).expect("first writer connection");
	let second = SessionIndex::open(&path).expect("second writer connection");
	let gate = Arc::new(Barrier::new(2));
	let first_gate = Arc::clone(&gate);
	let first_writer = thread::spawn(move || {
		let id = session_id("first-writer");
		create_after_journal(&first, &id, || {
			first_gate.wait();
		});
	});
	let second_gate = Arc::clone(&gate);
	let second_writer = thread::spawn(move || {
		let id = session_id("second-writer");
		create_after_journal(&second, &id, || {
			second_gate.wait();
		});
	});
	first_writer.join().expect("first writer thread");
	second_writer.join().expect("second writer thread");

	let reader = SessionIndex::open_authoritative_reader(&path).expect("authority reader");
	let page = reader
		.list(&SessionFilter::default())
		.expect("serialized writer rows");
	assert_eq!(page.sessions.len(), 2);
	assert!(
		page
			.sessions
			.iter()
			.any(|session| session.id == session_id("first-writer"))
	);
	assert!(
		page
			.sessions
			.iter()
			.any(|session| session.id == session_id("second-writer"))
	);
}
#[test]
fn relocate_session_moves_complete_projection_and_updates_workspace_identity() {
	let directory = tempdir().expect("temporary directory");
	let source =
		SessionIndex::open(directory.path().join("source.sqlite3")).expect("open source index");
	let destination =
		SessionIndex::open(directory.path().join("destination.sqlite3")).expect("open destination");
	let moved = session_id("move-me");
	create(&source, &moved);
	let receipt = outcome(41, 42);
	append_projection(&source, &moved, 0, "omp.turn_receipt", EventProjection::TurnReceipt {
		outcome: &receipt,
		failed:  false,
	});
	let item = thread_pb::Item {
		kind: Some(item::Kind::Message(thread_pb::Message {
			role:  thread_pb::Role::User as i32,
			parts: Vec::new(),
		})),
		..thread_pb::Item::default()
	};
	append_projection(&source, &moved, 1, "omp.item", EventProjection::ThreadItem {
		item:    &item,
		prompt:  Some("relocated projection prompt"),
		context: None,
	});

	assert!(
		source
			.relocate_session(&destination, &moved, "/workspace/destination", "/workspace/destination",)
			.expect("relocate session projection")
	);
	assert!(
		source
			.list(&SessionFilter { limit: 10, ..SessionFilter::default() })
			.expect("list source")
			.sessions
			.is_empty()
	);
	let destination_page = destination
		.list(&SessionFilter { limit: 10, ..SessionFilter::default() })
		.expect("list destination");
	assert_eq!(destination_page.sessions.len(), 1);
	assert_eq!(destination_page.sessions[0].id, moved);
	assert_eq!(destination_page.sessions[0].cwd.as_str(), "/workspace/destination");
	assert_eq!(destination_page.sessions[0].project.as_str(), "/workspace/destination");
	assert_eq!(
		destination
			.receipt(&moved, 0)
			.expect("query relocated receipt")
			.expect("relocated receipt")
			.usage
			.input_tokens,
		41
	);
	assert_eq!(
		destination
			.search_prompts("relocated projection prompt", 5)
			.expect("query relocated prompt")[0]
			.session,
		moved
	);
	assert!(
		!source
			.relocate_session(&destination, &moved, "/workspace/destination", "/workspace/destination",)
			.expect("repeat absent relocation")
	);
}

#[test]
fn delete_session_removes_the_projection_and_every_derived_row_atomically() {
	let directory = tempdir().expect("temporary directory");
	let index = SessionIndex::open(directory.path().join("sessions.sqlite3")).expect("open index");
	let deleted = session_id("delete-me");
	let child = session_id("surviving-child");
	create(&index, &deleted);
	create_child(&index, &child, &deleted);
	let receipt = outcome(7, 8);
	append_projection(&index, &deleted, 0, "omp.turn_receipt", EventProjection::TurnReceipt {
		outcome: &receipt,
		failed:  false,
	});
	let item = thread_pb::Item {
		kind: Some(item::Kind::Message(thread_pb::Message {
			role:  thread_pb::Role::User as i32,
			parts: Vec::new(),
		})),
		..thread_pb::Item::default()
	};
	append_projection(&index, &deleted, 1, "omp.item", EventProjection::ThreadItem {
		item:    &item,
		prompt:  Some("delete projection prompt"),
		context: None,
	});

	assert!(
		index
			.delete_session(&deleted)
			.expect("delete session projection")
	);
	let remaining = index
		.list(&SessionFilter { limit: 10, ..SessionFilter::default() })
		.expect("list after deletion");
	assert_eq!(remaining.sessions.len(), 1);
	assert_eq!(remaining.sessions[0].id, child);
	assert_eq!(remaining.sessions[0].parent.as_ref(), Some(&deleted));
	assert!(
		index
			.receipt(&deleted, 0)
			.expect("query deleted receipt")
			.is_none()
	);
	assert!(
		index
			.search_prompts("delete projection prompt", 5)
			.expect("query deleted prompt")
			.is_empty()
	);
	assert!(!index.delete_session(&deleted).expect("repeat deletion"));
}

#[test]
fn lineage_rekey_moves_all_projections_retains_collision_winners_and_rolls_back_dry_run() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("sessions.sqlite3");
	let index = SessionIndex::open(&path).expect("open index");
	let concurrent_writer = SessionIndex::open(&path).expect("open second WAL writer");
	let retained = session_id("retained-child");
	let archived = session_id("archived-parent");
	create(&index, &archived);
	create_child(&index, &retained, &archived);

	let retained_receipt = outcome(10, 11);
	let archived_collision_receipt = outcome(20, 21);
	let archived_unique_receipt = outcome(30, 31);
	let retained_item = thread_pb::Item {
		kind: Some(item::Kind::Message(thread_pb::Message {
			role:  thread_pb::Role::User as i32,
			parts: Vec::new(),
		})),
		..thread_pb::Item::default()
	};
	let archived_collision_item = retained_item.clone();
	let archived_unique_item = retained_item.clone();
	append_projection(&index, &retained, 0, "omp.turn_receipt", EventProjection::TurnReceipt {
		outcome: &retained_receipt,
		failed:  false,
	});
	append_projection(&index, &retained, 1, "omp.item", EventProjection::ThreadItem {
		item:    &retained_item,
		prompt:  Some("retained collision prompt"),
		context: None,
	});
	append_projection(&index, &archived, 0, "omp.turn_receipt", EventProjection::TurnReceipt {
		outcome: &archived_collision_receipt,
		failed:  false,
	});
	append_projection(&index, &archived, 1, "omp.item", EventProjection::ThreadItem {
		item:    &archived_collision_item,
		prompt:  Some("archived collision prompt"),
		context: None,
	});
	append_projection(&index, &archived, 2, "omp.turn_receipt", EventProjection::TurnReceipt {
		outcome: &archived_unique_receipt,
		failed:  false,
	});
	append_projection(&index, &archived, 3, "omp.archived_item", EventProjection::ThreadItem {
		item:    &archived_unique_item,
		prompt:  Some("archived unique prompt"),
		context: None,
	});

	let dry_run = index
		.rekey_archived_lineage(&retained, slice::from_ref(&archived), MaintenanceMode::DryRun)
		.expect("measure lineage transfer");
	assert_eq!(dry_run.receipts.transferred, 1);
	assert_eq!(dry_run.receipts.collisions, 1);
	assert_eq!(dry_run.item_outcomes.transferred, 1);
	assert_eq!(dry_run.item_outcomes.collisions, 1);
	assert_eq!(dry_run.model_performance.transferred, 1);
	assert_eq!(dry_run.model_performance.collisions, 1);
	assert_eq!(dry_run.entry_kinds.transferred, 1);
	assert_eq!(dry_run.entry_kinds.collisions, 2);
	assert_eq!(dry_run.prompts_fts.transferred, 1);
	assert_eq!(dry_run.prompts_fts.collisions, 1);
	assert_eq!(dry_run.archived_sessions, 1);
	assert!(
		index
			.receipt(&archived, 2)
			.expect("query archived receipt after dry run")
			.is_some()
	);
	assert_eq!(
		index
			.search_prompts("archived unique prompt", 5)
			.expect("query archived prompt after dry run")[0]
			.session,
		archived
	);

	let applied = index
		.rekey_archived_lineage(&retained, slice::from_ref(&archived), MaintenanceMode::Apply)
		.expect("apply lineage transfer");
	assert_eq!(applied, dry_run);
	assert_eq!(
		index
			.receipt(&retained, 0)
			.expect("query retained collision receipt")
			.expect("retained collision receipt")
			.usage
			.input_tokens,
		10
	);
	assert_eq!(
		index
			.receipt(&retained, 2)
			.expect("query transferred receipt")
			.expect("transferred receipt")
			.usage
			.input_tokens,
		30
	);
	assert!(
		index
			.receipt(&archived, 2)
			.expect("query removed archived receipt")
			.is_none()
	);
	let retained_prompt = index
		.search_prompts("retained collision prompt", 5)
		.expect("query retained collision prompt");
	assert_eq!(retained_prompt.len(), 1);
	assert_eq!(retained_prompt[0].session, retained);
	assert!(
		index
			.search_prompts("archived collision prompt", 5)
			.expect("query discarded collision prompt")
			.is_empty()
	);
	let transferred_prompt = index
		.search_prompts("archived unique prompt", 5)
		.expect("query transferred prompt");
	assert_eq!(transferred_prompt.len(), 1);
	assert_eq!(transferred_prompt[0].session, retained);
	let statistics = index
		.session_statistics(&retained, false)
		.expect("query transferred statistics");
	assert_eq!(statistics.requests, 2);
	assert_eq!(statistics.user_messages, 2);
	let page = index
		.list(&SessionFilter {
			contains_kind: Some(sf!("omp.archived_item")),
			..SessionFilter::default()
		})
		.expect("query transferred entry kind");
	assert_eq!(page.sessions.len(), 1);
	assert_eq!(page.sessions[0].id, retained);
	assert_eq!(page.sessions[0].turns, 2);

	let post_maintenance = session_id("post-maintenance-writer");
	create(&concurrent_writer, &post_maintenance);
	assert_eq!(
		concurrent_writer
			.list(&SessionFilter::default())
			.expect("query after maintenance writer")
			.sessions
			.len(),
		2
	);
}
