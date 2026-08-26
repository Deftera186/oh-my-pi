//! Integration tests for durable artifact garbage collection.

use std::{
	fs::{self, File},
	io::Write as _,
	path::{Path, PathBuf},
	time::Duration,
};

use omp_core::{ArtifactUrl, Str};
use omp_storage::{
	blob::{BlobRef, BlobStore},
	gc::{
		self, ArtifactCatalog, ArtifactLifetime, ArtifactRequest, DurableRoots, Error,
		MAX_ARTIFACT_PAGE, SessionRoots, SweepReport,
	},
	transcript::SessionId,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn fixture() -> (TempDir, BlobStore) {
	let directory = tempfile::tempdir().expect("create fixture");
	let store = BlobStore::open(directory.path()).expect("open blob store");
	(directory, store)
}

fn session(name: &str) -> SessionId {
	SessionId(Str::from(name))
}

const fn request<'a>(
	session: &'a SessionId,
	key: &'a str,
	host_generation: u64,
) -> ArtifactRequest<'a> {
	ArtifactRequest {
		principal: "principal",
		extension: "publisher.extension",
		idempotency_key: key,
		session,
		host_generation,
		session_generation: 7,
	}
}

fn journal(root: &Path, id: &str, events: impl IntoIterator<Item = Value>) -> PathBuf {
	let sessions = root.join("sessions");
	fs::create_dir_all(&sessions).expect("create sessions directory");
	let path = sessions.join(format!("{id}.jsonl"));
	let mut file = File::create(&path).expect("create journal");
	writeln!(file, "{}", json!({"v": 4, "id": id, "created": 1, "cwd": root}))
		.expect("write header");
	for event in events {
		writeln!(file, "{event}").expect("write event");
	}
	path
}

fn sweep(
	store: &BlobStore,
	_expected_roots: &[SessionId],
	min_age: Duration,
) -> Result<SweepReport, Error> {
	let roots = SessionRoots::discover(store, &[])?;
	gc::sweep(store, &roots, min_age)
}

fn anchor(store: &BlobStore) {
	journal(store.root(), "gc-anchor", []);
}

fn artifact(reference: &BlobRef, lifetime: &str) -> Value {
	json!({
		"ts": 1,
		"k": "custom",
		"kind": "artifact",
		"data": {
			"lifetime": lifetime,
			"ref": {"h": reference.to_hex().as_str(), "n": reference.size}
		},
		"context": null,
		"display": false
	})
}

#[test]
fn grace_window_closes_the_put_before_append_race() {
	let (_directory, store) = fixture();
	anchor(&store);
	let first = store.put(b"first race payload").expect("put first");
	let second = store.put(b"second race payload").expect("put second");

	let protected = sweep(&store, &[], Duration::from_secs(60)).expect("grace sweep");
	assert_eq!(protected, SweepReport {
		examined_count: 2,
		examined_bytes: first.size + second.size,
		..SweepReport::default()
	});
	assert!(store.has(&first));
	assert!(store.has(&second));

	let reclaimed = sweep(&store, &[], Duration::ZERO).expect("ungraced sweep");
	assert_eq!(reclaimed.examined_count, 2);
	assert_eq!(reclaimed.examined_bytes, first.size + second.size);
	assert_eq!(reclaimed.reclaimed_count, 2);
	assert_eq!(reclaimed.reclaimed_bytes, first.size + second.size);
	assert!(!store.has(&first));
	assert!(!store.has(&second));
}

#[test]
fn rewind_restores_an_ephemeral_reference_to_the_live_chain() {
	let (directory, store) = fixture();
	let reference = store.put(b"rewound artifact").expect("put artifact");
	journal(directory.path(), "rewind", [
		artifact(&reference, "ephemeral"),
		json!({"ts": 2, "k": "rewind", "to": null}),
		json!({"ts": 3, "k": "rewind", "to": 0}),
	]);

	let report = sweep(&store, &[session("rewind")], Duration::ZERO).expect("sweep");
	assert_eq!(report.reachable_count, 1);
	assert_eq!(report.reclaimed_count, 0);
	assert!(store.has(&reference));
}

#[test]
fn discard_and_compaction_consume_only_ephemeral_artifacts() {
	let (directory, store) = fixture();
	let discarded = store.put(b"discarded ephemeral").expect("put discarded");
	let compacted = store.put(b"compacted ephemeral").expect("put compacted");
	let session_blob = store.put(b"session retention").expect("put session");

	journal(directory.path(), "discard", [
		artifact(&discarded, "ephemeral"),
		json!({"ts": 2, "k": "rewind", "to": null}),
	]);
	journal(directory.path(), "compact", [
		artifact(&compacted, "ephemeral"),
		artifact(&session_blob, "session"),
		json!({"ts": 3, "k": "title", "title": "kept", "source": "user"}),
		json!({
			"ts": 4,
			"k": "compact",
			"summary": "summary",
			"short": null,
			"first_kept": 2,
			"tokens_before": 10,
			"warning": null
		}),
	]);

	let report =
		sweep(&store, &[session("discard"), session("compact")], Duration::ZERO).expect("sweep");
	assert_eq!(report.examined_count, 3);
	assert_eq!(report.reclaimed_count, 2);
	assert_eq!(report.reclaimed_bytes, discarded.size + compacted.size);
	assert!(!store.has(&discarded));
	assert!(!store.has(&compacted));
	assert!(store.has(&session_blob));
}

#[test]
fn durable_pin_outlives_every_session_root() {
	let (_directory, store) = fixture();
	anchor(&store);
	let reference = store
		.put(b"month-later export")
		.expect("put durable artifact");
	let mut roots = DurableRoots::open(&store).expect("open durable roots");
	roots.pin(&reference).expect("pin durable artifact");
	drop(roots);

	let protected = sweep(&store, &[], Duration::ZERO).expect("pinned sweep");
	assert_eq!(protected.reachable_count, 1);
	assert_eq!(protected.reclaimed_count, 0);
	assert!(store.has(&reference));

	let mut roots = DurableRoots::open(&store).expect("reopen durable roots");
	roots.unpin(&reference).expect("remove pin");
	drop(roots);
	let reclaimed = sweep(&store, &[], Duration::ZERO).expect("unpinned sweep");
	assert_eq!(reclaimed.reclaimed_count, 1);
	assert_eq!(reclaimed.reclaimed_bytes, reference.size);
	assert!(!store.has(&reference));
}

#[test]
fn corrupt_references_fail_closed_when_the_hash_is_recoverable() {
	let (directory, store) = fixture();
	let recoverable = store
		.put(b"recoverable corrupt ref")
		.expect("put recoverable");
	let garbage = store.put(b"unreferenced").expect("put garbage");
	let mut malformed_length = artifact(&recoverable, "session");
	malformed_length["data"]["ref"]["n"] = json!("not-a-length");
	let malformed_hash = json!({
		"ts": 2,
		"k": "custom",
		"kind": "artifact",
		"data": {"lifetime": "session", "ref": {"h": "not-a-hash", "n": 4}},
		"context": null,
		"display": false
	});
	journal(directory.path(), "corrupt", [malformed_length, malformed_hash]);

	let report = sweep(&store, &[session("corrupt")], Duration::ZERO).expect("sweep");
	assert_eq!(report.corrupt_references, 2);
	assert_eq!(report.reachable_count, 1);
	assert_eq!(report.reclaimed_count, 1);
	assert_eq!(report.reclaimed_bytes, garbage.size);
	assert!(store.has(&recoverable));
	assert!(!store.has(&garbage));
}

#[test]
fn catalog_rejects_peer_size_and_missing_content_before_adoption() {
	let (_directory, store) = fixture();
	let reference = store.put(b"authoritative length").expect("put artifact");
	let mut catalog = ArtifactCatalog::open(&store).expect("open artifact catalog");

	let mismatch = catalog
		.adopt(
			&session("authority"),
			reference.hash.into_bytes(),
			Some(reference.size + 1),
			ArtifactLifetime::Session,
		)
		.expect_err("peer size must not override the store");
	assert!(matches!(
		mismatch,
		Error::SizeClaim { claimed, actual }
			if claimed == reference.size + 1 && actual == reference.size
	));
	let missing = catalog
		.adopt(&session("authority"), [0x5a; 32], None, ArtifactLifetime::Session)
		.expect_err("missing content must not be catalogued");
	assert!(matches!(missing, Error::Blob(omp_storage::blob::Error::NotFound)));
	let adopted = catalog
		.adopt(
			&session("authority"),
			reference.hash.into_bytes(),
			Some(reference.size),
			ArtifactLifetime::Session,
		)
		.expect("adopt authoritative artifact");
	fs::remove_file(store.path(&reference)).expect("remove backing blob");
	let missing_stat = catalog
		.stat_url(&session("authority"), &adopted.url())
		.expect_err("stat must recheck blob authority");
	assert!(matches!(missing_stat, Error::Blob(omp_storage::blob::Error::NotFound)));
}

#[test]
fn catalog_pin_is_monotonic_and_updates_durable_roots_atomically() {
	let (_directory, store) = fixture();
	anchor(&store);
	let reference = store.put(b"catalog pin").expect("put artifact");
	let mut catalog = ArtifactCatalog::open(&store).expect("open artifact catalog");
	let adopted = catalog
		.adopt(
			&session("pin"),
			reference.hash.into_bytes(),
			Some(reference.size),
			ArtifactLifetime::Session,
		)
		.expect("adopt artifact");
	let downgrade = catalog
		.pin(adopted.catalog_id, ArtifactLifetime::Ephemeral)
		.expect_err("retention promises cannot be lowered");
	assert!(matches!(downgrade, Error::LifetimeDowngrade { .. }));
	let durable = catalog
		.pin(adopted.catalog_id, ArtifactLifetime::Durable)
		.expect("promote durable");
	assert!(durable.pinned);
	assert_eq!(
		catalog
			.stat_digest(reference.hash.into_bytes())
			.expect("stat durable digest"),
		durable
	);
	drop(catalog);

	let report = sweep(&store, &[], Duration::ZERO).expect("sweep pinned catalog artifact");
	assert_eq!(report.reclaimed_count, 0);
	assert!(store.has(&reference));
}

#[test]
fn catalog_persists_exact_idempotency_results_and_rejects_conflicts() {
	let (_directory, store) = fixture();
	let first = store.put(b"idempotent first").expect("put first");
	let second = store.put(b"idempotent second").expect("put second");
	let owner = session("idempotency");
	let mut catalog = ArtifactCatalog::open(&store).expect("open artifact catalog");
	let stamp = request(&owner, "adopt-1", 3);
	let adopted = catalog
		.adopt_once(stamp, first.hash.into_bytes(), Some(first.size), ArtifactLifetime::Session)
		.expect("first adopt");
	assert_eq!(
		catalog
			.adopt_once(stamp, first.hash.into_bytes(), Some(first.size), ArtifactLifetime::Session)
			.expect("exact replay"),
		adopted
	);
	let conflict = catalog
		.adopt_once(stamp, second.hash.into_bytes(), Some(second.size), ArtifactLifetime::Session)
		.expect_err("same key with another hash must conflict");
	assert!(matches!(conflict, Error::IdempotencyConflict(_)));
	let operation_conflict = catalog
		.pin_once(stamp, adopted.catalog_id, ArtifactLifetime::Durable)
		.expect_err("same key with another operation must conflict");
	assert!(matches!(operation_conflict, Error::IdempotencyConflict(_)));

	let pin_stamp = request(&owner, "pin-1", 3);
	let pinned = catalog
		.pin_once(pin_stamp, adopted.catalog_id, ArtifactLifetime::Durable)
		.expect("durable pin");
	assert_eq!(
		catalog
			.pin_once(pin_stamp, adopted.catalog_id, ArtifactLifetime::Durable)
			.expect("pin replay"),
		pinned
	);
	assert_eq!(
		catalog
			.adopt_once(stamp, first.hash.into_bytes(), Some(first.size), ArtifactLifetime::Session)
			.expect("adopt replay keeps recorded result")
			.lifetime,
		ArtifactLifetime::Session
	);
	let next_generation = catalog
		.adopt_once(
			request(&owner, "adopt-1", 4),
			second.hash.into_bytes(),
			Some(second.size),
			ArtifactLifetime::Session,
		)
		.expect("generation participates in request identity");
	assert_eq!(next_generation.reference, second);
}

#[test]
fn catalog_list_is_bounded_and_uses_a_stable_keyset_cursor() {
	let (_directory, store) = fixture();
	let mut catalog = ArtifactCatalog::open(&store).expect("open artifact catalog");
	for byte in 0..=MAX_ARTIFACT_PAGE {
		let reference = store.put(&byte.to_le_bytes()).expect("put listed artifact");
		catalog
			.adopt(&session("list"), reference.hash.into_bytes(), None, ArtifactLifetime::Session)
			.expect("adopt listed artifact");
	}

	let first = catalog
		.list(Some(&session("list")), None, u32::MAX)
		.expect("first page");
	assert_eq!(first.records.len(), MAX_ARTIFACT_PAGE as usize);
	let second = catalog
		.list(Some(&session("list")), first.next_cursor, u32::MAX)
		.expect("second page");
	assert_eq!(second.records.len(), 1);
	assert_eq!(second.next_cursor, None);
	assert!(first.records.last().unwrap().catalog_id < second.records[0].catalog_id);
}

#[test]
fn journal_artifact_ordinals_mark_catalog_content_reachable() {
	let (directory, store) = fixture();
	let referenced = store
		.put(b"catalog journal reference")
		.expect("put referenced");
	let unreferenced = store
		.put(b"catalog without journal reference")
		.expect("put unreferenced");
	let mut catalog = ArtifactCatalog::open(&store).expect("open artifact catalog");
	let record = catalog
		.adopt(
			&session("catalog-journal"),
			referenced.hash.into_bytes(),
			None,
			ArtifactLifetime::Session,
		)
		.expect("adopt referenced");
	catalog
		.adopt(
			&session("catalog-journal"),
			unreferenced.hash.into_bytes(),
			None,
			ArtifactLifetime::Session,
		)
		.expect("adopt unreferenced");
	journal(directory.path(), "catalog-journal", [json!({
		"ts": 1,
		"k": "custom",
		"kind": "artifact-link",
		"data": {"url": record.url().as_str()},
		"context": null,
		"display": true
	})]);
	drop(catalog);

	let report = sweep(&store, &[session("catalog-journal")], Duration::ZERO).expect("sweep");
	assert!(store.has(&referenced));
	assert!(!store.has(&unreferenced));
	assert_eq!(report.reclaimed_count, 1);
	assert_eq!(report.reclaimed_bytes, unreferenced.size);
}

#[test]
fn artifact_urls_are_session_local_until_durable_digest_promotion() {
	let (_directory, store) = fixture();
	let reference = store.put(b"cross-session artifact").expect("put artifact");
	let mut catalog = ArtifactCatalog::open(&store).expect("open artifact catalog");
	let local = catalog
		.adopt(&session("source"), reference.hash.into_bytes(), None, ArtifactLifetime::Session)
		.expect("adopt local artifact");
	assert_eq!(local.url(), ArtifactUrl::from_ordinal(0));
	assert_eq!(local.url_for(&session("other")), None);
	assert!(matches!(
		catalog.stat_url(&session("other"), &local.url()),
		Err(Error::ArtifactNotFound)
	));

	let durable = catalog
		.pin_url(&session("source"), &local.url(), ArtifactLifetime::Durable)
		.expect("promote by URL");
	let digest_url = durable.durable_url().expect("durable digest URL");
	assert_eq!(durable.url_for(&session("other")), Some(digest_url.clone()));
	assert_eq!(
		catalog
			.stat_url(&session("other"), &digest_url)
			.expect("cross-session durable stat"),
		durable
	);
	let adopted = catalog
		.adopt_url(&session("other"), &digest_url, Some(reference.size), ArtifactLifetime::Session)
		.expect("adopt durable source into another session");
	assert_eq!(adopted.session, session("other"));
	assert_eq!(adopted.url(), ArtifactUrl::from_ordinal(0));
}

#[test]
fn profile_sweep_discovers_project_and_custom_session_authorities() {
	let (directory, store) = fixture();
	let project_blob = store.put(b"project attachment").expect("put project blob");
	let custom_blob = store.put(b"custom attachment").expect("put custom blob");
	let orphan = store.put(b"unreachable attachment").expect("put orphan");

	fs::create_dir_all(directory.path().join("sessions")).expect("create empty legacy store");
	journal(&directory.path().join("projects/project-a"), "project-session", [artifact(
		&project_blob,
		"session",
	)]);
	let custom = tempfile::tempdir().expect("create custom store");
	journal(custom.path(), "custom-session", [artifact(&custom_blob, "session")]);
	let custom_sessions = custom.path().join("sessions");

	let roots =
		SessionRoots::discover(&store, &[custom_sessions]).expect("discover every session authority");
	assert_eq!(roots.store_count(), 3);
	assert_eq!(roots.journal_count(), 2);
	let report = gc::sweep(&store, &roots, Duration::ZERO).expect("profile sweep");

	assert!(store.has(&project_blob));
	assert!(store.has(&custom_blob));
	assert!(!store.has(&orphan));
	assert_eq!(report.reclaimed_count, 1);
}

#[test]
fn empty_session_authorities_refuse_profile_wide_deletion() {
	let (directory, store) = fixture();
	fs::create_dir_all(directory.path().join("sessions")).expect("create empty session authority");
	let orphan = store
		.put(b"must survive incomplete roots")
		.expect("put orphan");

	let error = SessionRoots::discover(&store, &[]).expect_err("empty roots must fail closed");
	assert!(matches!(error, Error::NoSessionJournals));
	assert!(store.has(&orphan));
}

#[test]
fn changed_journal_inventory_refuses_profile_wide_deletion() {
	let (directory, store) = fixture();
	journal(directory.path(), "known", []);
	let roots = SessionRoots::discover(&store, &[]).expect("discover roots");
	let orphan = store
		.put(b"must survive stale discovery")
		.expect("put orphan");
	journal(directory.path(), "arrived-late", []);

	let error =
		gc::sweep(&store, &roots, Duration::ZERO).expect_err("stale root discovery must fail closed");
	assert!(matches!(error, Error::SessionRootsChanged));
	assert!(store.has(&orphan));
}

#[cfg(unix)]
#[test]
fn interrupted_sweep_returns_exact_cancellation_safe_accounting() {
	use std::os::unix::fs::PermissionsExt as _;

	let (_directory, store) = fixture();
	anchor(&store);
	let reference = store.put(b"cannot remove yet").expect("put artifact");
	let artifact_path = store.path(&reference);
	let shard = artifact_path.parent().expect("second shard");
	let original = fs::metadata(shard).expect("shard metadata").permissions();
	let mut locked = original.clone();
	locked.set_mode(0o500);
	fs::set_permissions(shard, locked).expect("lock shard");

	let error = sweep(&store, &[], Duration::ZERO).expect_err("removal must be interrupted");
	fs::set_permissions(shard, original).expect("restore shard");
	let Error::Interrupted { report, .. } = error else {
		panic!("unexpected error: {error}");
	};
	assert_eq!(report, SweepReport {
		examined_count: 1,
		examined_bytes: reference.size,
		..SweepReport::default()
	});
	assert!(store.has(&reference));
}
