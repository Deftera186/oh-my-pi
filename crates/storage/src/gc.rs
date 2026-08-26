//! Reachability-based garbage collection for the content-addressed blob store.
//!
//! Blob placement deliberately completes before the journal append that makes a
//! blob reachable. [`sweep`] therefore never removes a blob younger than its
//! `min_age` grace window. Callers must choose a window longer than the maximum
//! put-before-append interval; a zero window is appropriate only while writers
//! are stopped.
//!
//! Session roots are transcript-v4 journals discovered beneath every standard
//! project store and each explicitly configured custom store. Session artifacts
//! remain reachable from every physical journal entry. Ephemeral
//! artifacts remain reachable only while their referencing entry belongs to the
//! reconstructed live chain, so compaction/discard can consume them while a
//! rewind that restores the entry keeps them alive. Durable artifacts are
//! pinned independently in the authoritative durable-roots table.

use std::{
	collections::HashSet,
	fs::{self, File},
	io::{self, BufRead as _, BufReader},
	path::{Path, PathBuf},
	str,
	time::{Duration, SystemTime},
};

use omp_core::{ArtifactAddress, ArtifactUrl, Hash32, Str, hash32::Hasher};
pub use omp_tool::ArtifactLifetime;
use rusqlite::{Connection, OptionalExtension as _, Row, TransactionBehavior, params};
use serde_json::{Map, Value};
use thiserror::Error as ThisError;

use crate::{
	blob::{self, BlobRef, BlobStore},
	transcript::{self, Reader, SessionId, read_header},
};

const DURABLE_ROOTS_FILE: &str = "durable-roots.sqlite3";
const PROJECTS_DIRECTORY: &str = "projects";
const SESSIONS_DIRECTORY: &str = "sessions";

/// A complete, validated snapshot of the journals that root one profile-wide
/// blob sweep.
///
/// Construct this only through [`SessionRoots::discover`]. Keeping the fields
/// private prevents destructive callers from substituting an arbitrary or
/// accidentally empty session-id list for authoritative journal discovery.
#[derive(Debug)]
pub struct SessionRoots {
	blob_root:     PathBuf,
	custom_stores: Vec<PathBuf>,
	journals:      Vec<(SessionId, PathBuf)>,
	stores:        Vec<PathBuf>,
}

impl SessionRoots {
	/// Discovers every standard project session store and every explicitly
	/// configured custom store for `store`.
	///
	/// Standard discovery includes `<profile>/projects/*/sessions` and the
	/// legacy `<profile>/sessions` location. Each custom path must name an
	/// existing session-store directory. Every transcript-v4 journal is parsed
	/// through its line-zero header; unreadable or malformed journals abort
	/// discovery rather than being ignored.
	///
	/// # Errors
	///
	/// Returns [`Error::NoSessionStores`] when no authoritative store exists and
	/// [`Error::NoSessionJournals`] when all discovered stores are empty.
	pub fn discover(store: &BlobStore, custom_stores: &[PathBuf]) -> Result<Self, Error> {
		let blob_root = fs::canonicalize(store.root())?;
		let mut canonical_custom_stores = Vec::with_capacity(custom_stores.len());
		for custom in custom_stores {
			add_session_store(custom, &mut canonical_custom_stores)?;
		}
		canonical_custom_stores.sort_unstable();
		let stores = discover_session_stores(store, &canonical_custom_stores)?;
		if stores.is_empty() {
			return Err(Error::NoSessionStores);
		}

		let mut journals = Vec::new();
		for directory in &stores {
			discover_journals(directory, &mut journals)?;
		}
		if journals.is_empty() {
			return Err(Error::NoSessionJournals);
		}
		journals.sort_unstable_by(|left, right| left.1.cmp(&right.1));
		Ok(Self { blob_root, custom_stores: canonical_custom_stores, journals, stores })
	}

	/// Number of distinct authoritative session-store directories discovered.
	#[must_use]
	pub fn store_count(&self) -> usize {
		self.stores.len()
	}

	/// Number of physical transcript journals that will be marked.
	#[must_use]
	pub fn journal_count(&self) -> usize {
		self.journals.len()
	}
}

/// Exact accounting for one completed or interrupted sweep.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SweepReport {
	/// Blob files whose metadata was examined.
	pub examined_count:     u64,
	/// Total stored bytes represented by examined blob files.
	pub examined_bytes:     u64,
	/// Unreachable, old blob files successfully removed.
	pub reclaimed_count:    u64,
	/// Total bytes successfully reclaimed.
	pub reclaimed_bytes:    u64,
	/// Distinct content hashes protected by journals or durable roots.
	pub reachable_count:    u64,
	/// Malformed blob-reference occurrences observed in retained journals.
	pub corrupt_references: u64,
}

#[derive(Default)]
struct ArtifactUses {
	physical: HashSet<(Str, u64)>,
	live:     HashSet<(Str, u64)>,
}

/// Garbage-collection and durable-root failures.
#[derive(Debug, ThisError)]
pub enum Error {
	/// A filesystem operation outside the destructive sweep failed.
	#[error(transparent)]
	Io(#[from] io::Error),
	/// A retained journal could not be decoded.
	#[error(transparent)]
	Journal(#[from] transcript::Error),
	/// The durable-roots table could not be read or updated.
	#[error(transparent)]
	Database(#[from] rusqlite::Error),
	/// A blob supplied to the durable-roots API was unavailable or invalid.
	#[error(transparent)]
	Blob(#[from] blob::Error),
	/// No standard or explicit session authority was available for a profile
	/// sweep.
	#[error("profile blob sweep requires at least one authoritative session store")]
	NoSessionStores,
	/// Session stores were discovered but contained no authoritative journals.
	#[error("profile blob sweep refused because all authoritative session stores are empty")]
	NoSessionJournals,
	/// A standard or explicit session-store path was not a directory.
	#[error("invalid session store: {}", .0.display())]
	InvalidSessionStore(PathBuf),
	/// A session-store symlink could hide a journal from bounded discovery.
	#[error("session store contains unsupported symlink: {}", .0.display())]
	UnsupportedSessionSymlink(PathBuf),
	/// Roots were discovered for a different profile-wide blob store.
	#[error("session roots belong to a different blob store")]
	SessionRootMismatch,
	/// The journal inventory changed after authoritative root discovery.
	#[error("session journal inventory changed after root discovery")]
	SessionRootsChanged,
	/// The durable-roots table contained a hash that was not a SHA-256 digest.
	#[error("durable-roots table contains an invalid blob hash")]
	CorruptDurableRoot,
	/// A caller-reported byte length disagreed with authoritative blob metadata.
	#[error("artifact size claim was {claimed} bytes, authoritative size is {actual} bytes")]
	SizeClaim {
		/// Untrusted caller-reported length.
		claimed: u64,
		/// Length reported by the blob store.
		actual:  u64,
	},
	/// No catalog record matched the supplied artifact address.
	#[error("artifact catalog record not found")]
	ArtifactNotFound,
	/// An artifact retention promise cannot be lowered.
	#[error("artifact lifetime cannot be lowered from {current} to {requested}")]
	LifetimeDowngrade {
		/// Existing minimum retention.
		current:   ArtifactLifetime,
		/// Requested weaker retention.
		requested: ArtifactLifetime,
	},
	/// A stored artifact row contained invalid typed metadata.
	#[error("artifact catalog contains an invalid row")]
	CorruptArtifactCatalog,
	/// An idempotency key was replayed with a different artifact request.
	#[error("artifact idempotency replay for `{0}` differs from durable truth")]
	IdempotencyConflict(Str),
	/// Sweeping stopped after a filesystem failure; the report counts only
	/// metadata observed and removals completed before the failure.
	#[error("blob sweep stopped after {report:?}: {source}")]
	Interrupted {
		/// Exact work completed before interruption.
		report: SweepReport,
		/// Filesystem failure that stopped the sweep.
		#[source]
		source: io::Error,
	},
}

/// The independent SQLite authority for blobs retained beyond session lifetime.
///
/// Pin and unpin operations take an immediate transaction, the same lock held
/// by [`sweep`] while it snapshots roots and removes files. A successful
/// concurrent pin therefore either precedes marking or observes that the blob
/// was already reclaimed; it can never publish a root to a missing blob.
pub struct DurableRoots {
	store:      BlobStore,
	connection: Connection,
}

impl DurableRoots {
	/// Opens the durable-roots table belonging to `store`, creating it when
	/// absent.
	pub fn open(store: &BlobStore) -> Result<Self, Error> {
		let connection = open_catalog_connection(store)?;
		Ok(Self { store: store.clone(), connection })
	}

	/// Pins `reference` independently of every session journal.
	///
	/// The referenced blob must exist at its declared length when the pin
	/// transaction commits.
	pub fn pin(&mut self, reference: &BlobRef) -> Result<(), Error> {
		let transaction = self
			.connection
			.transaction_with_behavior(TransactionBehavior::Immediate)?;
		let actual = match fs::metadata(self.store.path(reference)) {
			Ok(metadata) => metadata.len(),
			Err(error) if error.kind() == io::ErrorKind::NotFound => {
				return Err(blob::Error::NotFound.into());
			},
			Err(error) => return Err(error.into()),
		};
		if actual != reference.size {
			return Err(blob::Error::Corrupt { expected: reference.size, actual }.into());
		}
		transaction.execute(
			"INSERT INTO durable_roots(hash) VALUES (?1) ON CONFLICT(hash) DO NOTHING",
			params![reference.hash.as_bytes()],
		)?;
		transaction.commit()?;
		Ok(())
	}

	/// Removes an independent durable pin.
	///
	/// This is idempotent. The blob remains protected by retained journals and
	/// by the sweep grace window after the pin is removed.
	pub fn unpin(&mut self, reference: &BlobRef) -> Result<(), Error> {
		let transaction = self
			.connection
			.transaction_with_behavior(TransactionBehavior::Immediate)?;
		transaction.execute("DELETE FROM durable_roots WHERE hash = ?1", params![
			reference.hash.as_bytes()
		])?;
		transaction.commit()?;
		Ok(())
	}
}

/// Maximum number of artifact rows returned by one catalog page.
pub const MAX_ARTIFACT_PAGE: u32 = 200;

/// One authoritative artifact identity and retention record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactRecord {
	/// Monotonic catalog cursor.
	pub catalog_id: u64,
	/// Session-local namespace that minted the short ordinal.
	pub session:    SessionId,
	/// Short session-local `artifact://` ordinal.
	pub ordinal:    u64,
	/// Content identity with authoritative stored length.
	pub reference:  BlobRef,
	/// Minimum retention promise.
	pub lifetime:   ArtifactLifetime,
	/// Whether an independent durable root protects the content.
	pub pinned:     bool,
}

/// One bounded, keyset-paginated artifact catalog result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactPage {
	/// Rows in ascending catalog order.
	pub records:     Vec<ArtifactRecord>,
	/// Cursor to supply for the next page, absent at the end.
	pub next_cursor: Option<u64>,
}

impl ArtifactRecord {
	/// Returns the short session-local address for this artifact.
	pub fn url(&self) -> ArtifactUrl {
		ArtifactUrl::from_ordinal(self.ordinal)
	}

	/// Returns the cross-session digest address when retention is durable.
	pub fn durable_url(&self) -> Option<ArtifactUrl> {
		(self.lifetime == ArtifactLifetime::Durable)
			.then(|| ArtifactUrl::from_digest(self.reference.hash.into_bytes()))
	}

	/// Returns an address valid from `session`.
	///
	/// The owning session receives the short ordinal. Other sessions receive the
	/// durable digest form; non-durable records have no cross-session address.
	pub fn url_for(&self, session: &SessionId) -> Option<ArtifactUrl> {
		if &self.session == session {
			Some(self.url())
		} else {
			self.durable_url()
		}
	}
}

/// Authoritative artifact metadata catalog backed by the GC roots database.
///
/// The catalog never stores bytes and never trusts a caller-reported length.
/// Adoption stats the [`BlobStore`] path and records that length. Durable
/// promotion and metadata update share one SQLite transaction with the
/// durable-roots table.
pub struct ArtifactCatalog {
	store:      BlobStore,
	connection: Connection,
}

/// Authenticated durable-request identity for artifact adoption and pinning.
#[derive(Clone, Copy)]
pub struct ArtifactRequest<'a> {
	/// Stable authenticated principal identifier.
	pub principal:          &'a str,
	/// Authenticated extension identifier.
	pub extension:          &'a str,
	/// Stable key reused by retries of one logical request.
	pub idempotency_key:    &'a str,
	/// Session whose artifact namespace receives the operation.
	pub session:            &'a SessionId,
	/// Accepted extension-host generation.
	pub host_generation:    u64,
	/// Accepted session generation.
	pub session_generation: u64,
}

impl ArtifactCatalog {
	/// Opens the artifact catalog belonging to `store`.
	pub fn open(store: &BlobStore) -> Result<Self, Error> {
		Ok(Self { store: store.clone(), connection: open_catalog_connection(store)? })
	}

	/// Adopts blob content into a session-local artifact namespace.
	///
	/// `claimed_size` is checked but never persisted as authority. Re-adopting
	/// the same hash in the same session is idempotent and may raise, but never
	/// lower, its retention promise.
	pub fn adopt(
		&mut self,
		session: &SessionId,
		hash: [u8; 32],
		claimed_size: Option<u64>,
		lifetime: ArtifactLifetime,
	) -> Result<ArtifactRecord, Error> {
		let transaction = self
			.connection
			.transaction_with_behavior(TransactionBehavior::Immediate)?;
		let record =
			adopt_in_transaction(&self.store, &transaction, session, hash, claimed_size, lifetime)?;
		transaction.commit()?;
		Ok(record)
	}

	/// Adopts a blob with persistent, authenticated idempotency replay.
	///
	/// An exact retry returns the originally recorded artifact. Reusing the same
	/// authenticated key for another payload returns
	/// [`Error::IdempotencyConflict`].
	pub fn adopt_once(
		&mut self,
		request: ArtifactRequest<'_>,
		hash: [u8; 32],
		claimed_size: Option<u64>,
		lifetime: ArtifactLifetime,
	) -> Result<ArtifactRecord, Error> {
		let fingerprint = adopt_fingerprint(hash, claimed_size, lifetime);
		let transaction = self
			.connection
			.transaction_with_behavior(TransactionBehavior::Immediate)?;
		if let Some(record) = replay_record(&transaction, request, fingerprint)? {
			validate_record(&self.store, &record)?;
			transaction.commit()?;
			return Ok(record);
		}
		let record = adopt_in_transaction(
			&self.store,
			&transaction,
			request.session,
			hash,
			claimed_size,
			lifetime,
		)?;
		record_request(&transaction, request, fingerprint, &record)?;
		transaction.commit()?;
		Ok(record)
	}

	/// Adopts content addressed by an existing artifact URL.
	///
	/// Session ordinals resolve only inside `session`. Digest URLs resolve only
	/// records already promoted to durable retention. The resulting identity is
	/// assigned or reuses an ordinal in `session`.
	pub fn adopt_url(
		&mut self,
		session: &SessionId,
		source: &ArtifactUrl,
		claimed_size: Option<u64>,
		lifetime: ArtifactLifetime,
	) -> Result<ArtifactRecord, Error> {
		let source = self.stat_url(session, source)?;
		self.adopt(session, source.reference.hash.into_bytes(), claimed_size, lifetime)
	}

	/// Adopts an artifact URL with persistent authenticated replay.
	pub fn adopt_url_once(
		&mut self,
		request: ArtifactRequest<'_>,
		source: &ArtifactUrl,
		claimed_size: Option<u64>,
		lifetime: ArtifactLifetime,
	) -> Result<ArtifactRecord, Error> {
		let source = self.stat_url(request.session, source)?;
		self.adopt_once(request, source.reference.hash.into_bytes(), claimed_size, lifetime)
	}

	/// Resolves a typed artifact URL with authoritative blob metadata.
	///
	/// Ordinal form is session-local. Digest form is accepted only for artifacts
	/// already retained durably.
	pub fn stat_url(&self, session: &SessionId, url: &ArtifactUrl) -> Result<ArtifactRecord, Error> {
		match url.address() {
			ArtifactAddress::Ordinal(ordinal) => self.stat_ordinal(session, ordinal),
			ArtifactAddress::Digest(hash) => {
				let reference = BlobRef::parse_hex(hash, 0)?;
				self.stat_digest(reference.hash.into_bytes())
			},
		}
	}

	/// Looks up one session-local artifact ordinal.
	pub fn stat_ordinal(&self, session: &SessionId, ordinal: u64) -> Result<ArtifactRecord, Error> {
		let encoded = self
			.connection
			.query_row(
				"SELECT id, session, ordinal, hash, size, lifetime FROM artifacts WHERE session = ?1 \
				 AND ordinal = ?2",
				params![session.0.as_str(), ordinal],
				artifact_row,
			)
			.optional()?
			.ok_or(Error::ArtifactNotFound)?;
		let record = decode_artifact(encoded)?;
		validate_record(&self.store, &record)?;
		Ok(record)
	}

	/// Looks up a durable artifact by cross-session content digest.
	pub fn stat_digest(&self, hash: [u8; 32]) -> Result<ArtifactRecord, Error> {
		let encoded = self
			.connection
			.query_row(
				"SELECT id, session, ordinal, hash, size, lifetime FROM artifacts WHERE hash = ?1 AND \
				 lifetime = 'durable' ORDER BY id LIMIT 1",
				params![hash.as_slice()],
				artifact_row,
			)
			.optional()?
			.ok_or(Error::ArtifactNotFound)?;
		let record = decode_artifact(encoded)?;
		validate_record(&self.store, &record)?;
		Ok(record)
	}

	/// Returns a bounded artifact page after `cursor`.
	///
	/// `session` restricts results to one journal namespace. Limits above
	/// [`MAX_ARTIFACT_PAGE`] are clamped.
	pub fn list(
		&self,
		session: Option<&SessionId>,
		cursor: Option<u64>,
		limit: u32,
	) -> Result<ArtifactPage, Error> {
		let limit = limit.clamp(1, MAX_ARTIFACT_PAGE);
		let fetch = u64::from(limit) + 1;
		let cursor = cursor.unwrap_or(0);
		let mut records = Vec::with_capacity(usize::try_from(fetch).expect("page bound fits usize"));
		if let Some(session) = session {
			let mut statement = self.connection.prepare(
				"SELECT id, session, ordinal, hash, size, lifetime FROM artifacts WHERE id > ?1 AND \
				 session = ?2 ORDER BY id LIMIT ?3",
			)?;
			let mut rows = statement.query(params![cursor, session.0.as_str(), fetch])?;
			while let Some(row) = rows.next()? {
				let record = decode_artifact(artifact_row(row)?)?;
				validate_record(&self.store, &record)?;
				records.push(record);
			}
		} else {
			let mut statement = self.connection.prepare(
				"SELECT id, session, ordinal, hash, size, lifetime FROM artifacts WHERE id > ?1 ORDER \
				 BY id LIMIT ?2",
			)?;
			let mut rows = statement.query(params![cursor, fetch])?;
			while let Some(row) = rows.next()? {
				let record = decode_artifact(artifact_row(row)?)?;
				validate_record(&self.store, &record)?;
				records.push(record);
			}
		}
		let has_more = records.len() > usize::try_from(limit).expect("page bound fits usize");
		if has_more {
			records.pop();
		}
		let next_cursor = has_more.then(|| records.last().expect("nonempty bounded page").catalog_id);
		Ok(ArtifactPage { records, next_cursor })
	}

	/// Raises the retention promise for a catalog record.
	///
	/// Durable promotion atomically inserts the independent GC root. Equal
	/// lifetimes are idempotent; downgrades fail.
	pub fn pin(
		&mut self,
		catalog_id: u64,
		lifetime: ArtifactLifetime,
	) -> Result<ArtifactRecord, Error> {
		let transaction = self
			.connection
			.transaction_with_behavior(TransactionBehavior::Immediate)?;
		let record = pin_in_transaction(&self.store, &transaction, catalog_id, lifetime)?;
		transaction.commit()?;
		Ok(record)
	}

	/// Raises retention with persistent authenticated idempotency replay.
	pub fn pin_once(
		&mut self,
		request: ArtifactRequest<'_>,
		catalog_id: u64,
		lifetime: ArtifactLifetime,
	) -> Result<ArtifactRecord, Error> {
		let fingerprint = pin_fingerprint(catalog_id, lifetime);
		let transaction = self
			.connection
			.transaction_with_behavior(TransactionBehavior::Immediate)?;
		if let Some(record) = replay_record(&transaction, request, fingerprint)? {
			validate_record(&self.store, &record)?;
			transaction.commit()?;
			return Ok(record);
		}
		let record = pin_in_transaction(&self.store, &transaction, catalog_id, lifetime)?;
		record_request(&transaction, request, fingerprint, &record)?;
		transaction.commit()?;
		Ok(record)
	}

	/// Raises retention for the artifact addressed by `url`.
	///
	/// Session ordinals require their owning `session`; digest addresses already
	/// imply durable retention.
	pub fn pin_url(
		&mut self,
		session: &SessionId,
		url: &ArtifactUrl,
		lifetime: ArtifactLifetime,
	) -> Result<ArtifactRecord, Error> {
		let record = self.stat_url(session, url)?;
		self.pin(record.catalog_id, lifetime)
	}

	/// Raises URL-addressed retention with persistent authenticated replay.
	pub fn pin_url_once(
		&mut self,
		request: ArtifactRequest<'_>,
		url: &ArtifactUrl,
		lifetime: ArtifactLifetime,
	) -> Result<ArtifactRecord, Error> {
		let record = self.stat_url(request.session, url)?;
		self.pin_once(request, record.catalog_id, lifetime)
	}
}

type EncodedArtifact = (u64, String, u64, Vec<u8>, u64, String);

fn adopt_in_transaction(
	store: &BlobStore,
	transaction: &rusqlite::Transaction<'_>,
	session: &SessionId,
	hash: [u8; 32],
	claimed_size: Option<u64>,
	lifetime: ArtifactLifetime,
) -> Result<ArtifactRecord, Error> {
	let actual = authoritative_size(store, hash)?;
	if let Some(claimed) = claimed_size
		&& claimed != actual
	{
		return Err(Error::SizeClaim { claimed, actual });
	}
	let existing = transaction
		.query_row(
			"SELECT id, session, ordinal, hash, size, lifetime FROM artifacts WHERE session = ?1 AND \
			 hash = ?2",
			params![session.0.as_str(), hash.as_slice()],
			artifact_row,
		)
		.optional()?;
	if let Some(existing) = existing {
		let mut record = decode_artifact(existing)?;
		validate_record(store, &record)?;
		promote_record(transaction, &record, lifetime)?;
		record.lifetime = lifetime;
		record.pinned = lifetime == ArtifactLifetime::Durable;
		return Ok(record);
	}
	let ordinal: u64 = transaction.query_row(
		"SELECT COALESCE(MAX(ordinal), -1) + 1 FROM artifacts WHERE session = ?1",
		params![session.0.as_str()],
		|row| row.get(0),
	)?;
	let lifetime_text: &'static str = lifetime.into();
	transaction.execute(
		"INSERT INTO artifacts(session, ordinal, hash, size, lifetime) VALUES (?1, ?2, ?3, ?4, ?5)",
		params![session.0.as_str(), ordinal, hash.as_slice(), actual, lifetime_text],
	)?;
	if lifetime == ArtifactLifetime::Durable {
		insert_durable_root(transaction, hash)?;
	}
	let catalog_id =
		u64::try_from(transaction.last_insert_rowid()).map_err(|_| Error::CorruptArtifactCatalog)?;
	Ok(ArtifactRecord {
		catalog_id,
		session: session.clone(),
		ordinal,
		reference: BlobRef { hash: Hash32::new(hash), size: actual },
		lifetime,
		pinned: lifetime == ArtifactLifetime::Durable,
	})
}

fn pin_in_transaction(
	store: &BlobStore,
	transaction: &rusqlite::Transaction<'_>,
	catalog_id: u64,
	lifetime: ArtifactLifetime,
) -> Result<ArtifactRecord, Error> {
	let encoded = transaction
		.query_row(
			"SELECT id, session, ordinal, hash, size, lifetime FROM artifacts WHERE id = ?1",
			params![catalog_id],
			artifact_row,
		)
		.optional()?
		.ok_or(Error::ArtifactNotFound)?;
	let mut record = decode_artifact(encoded)?;
	validate_record(store, &record)?;
	promote_record(transaction, &record, lifetime)?;
	record.lifetime = lifetime;
	record.pinned = lifetime == ArtifactLifetime::Durable;
	Ok(record)
}

fn replay_record(
	transaction: &rusqlite::Transaction<'_>,
	request: ArtifactRequest<'_>,
	fingerprint: [u8; 32],
) -> Result<Option<ArtifactRecord>, Error> {
	let replay: Option<(Vec<u8>, EncodedArtifact)> = transaction
		.query_row(
			"SELECT request_hash, artifact_id, result_session, result_ordinal, \
			 result_hash,result_size, result_lifetime FROM artifact_requests WHERE principal = ?1 \
			 AND extension = ?2 AND idempotency_key = ?3 AND session = ?4 AND host_generation = ?5 \
			 AND session_generation = ?6",
			params![
				request.principal,
				request.extension,
				request.idempotency_key,
				request.session.0.as_str(),
				request.host_generation,
				request.session_generation,
			],
			|row| {
				Ok((
					row.get(0)?,
					(row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?),
				))
			},
		)
		.optional()?;
	let Some((recorded, encoded)) = replay else {
		return Ok(None);
	};
	if recorded.as_slice() != fingerprint.as_slice() {
		return Err(Error::IdempotencyConflict(Str::new(request.idempotency_key)));
	}
	decode_artifact(encoded).map(Some)
}

fn record_request(
	transaction: &rusqlite::Transaction<'_>,
	request: ArtifactRequest<'_>,
	fingerprint: [u8; 32],
	record: &ArtifactRecord,
) -> Result<(), Error> {
	transaction.execute(
		"INSERT INTO artifact_requests(principal, extension, idempotency_key, session, \
		 host_generation, session_generation,request_hash, artifact_id, result_session, \
		 result_ordinal, result_hash, result_size,result_lifetime) VALUES (?1, ?2, ?3, ?4, ?5, ?6, \
		 ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
		params![
			request.principal,
			request.extension,
			request.idempotency_key,
			request.session.0.as_str(),
			request.host_generation,
			request.session_generation,
			fingerprint.as_slice(),
			record.catalog_id,
			record.session.0.as_str(),
			record.ordinal,
			record.reference.hash.as_bytes(),
			record.reference.size,
			Into::<&'static str>::into(record.lifetime),
		],
	)?;
	Ok(())
}

fn adopt_fingerprint(
	hash: [u8; 32],
	claimed_size: Option<u64>,
	lifetime: ArtifactLifetime,
) -> [u8; 32] {
	let mut hasher = Hash32::hasher();
	hash_fingerprint_field(&mut hasher, b"adopt");
	hash_fingerprint_field(&mut hasher, &hash);
	match claimed_size {
		Some(size) => {
			hash_fingerprint_field(&mut hasher, b"some");
			hash_fingerprint_field(&mut hasher, &size.to_le_bytes());
		},
		None => hash_fingerprint_field(&mut hasher, b"none"),
	}
	hash_fingerprint_field(&mut hasher, &[retention_rank(lifetime)]);
	hasher.finalize().into_bytes()
}

fn pin_fingerprint(catalog_id: u64, lifetime: ArtifactLifetime) -> [u8; 32] {
	let mut hasher = Hash32::hasher();
	hash_fingerprint_field(&mut hasher, b"pin");
	hash_fingerprint_field(&mut hasher, &catalog_id.to_le_bytes());
	hash_fingerprint_field(&mut hasher, &[retention_rank(lifetime)]);
	hasher.finalize().into_bytes()
}

fn hash_fingerprint_field(hasher: &mut Hasher, value: &[u8]) {
	hasher.update(
		u64::try_from(value.len())
			.expect("fingerprint field length fits u64")
			.to_le_bytes(),
	);
	hasher.update(value);
}

fn open_catalog_connection(store: &BlobStore) -> Result<Connection, Error> {
	let connection = Connection::open(store.root().join(DURABLE_ROOTS_FILE))?;
	connection.busy_timeout(Duration::from_secs(30))?;
	connection.execute_batch(
		"PRAGMA journal_mode = WAL;PRAGMA synchronous = FULL;CREATE TABLE IF NOT EXISTS \
		 durable_roots (hash BLOB PRIMARY KEY NOT NULL CHECK(length(hash) = 32)) STRICT;CREATE \
		 TABLE IF NOT EXISTS artifacts (id INTEGER PRIMARY KEY,session TEXT NOT NULL,ordinal \
		 INTEGER NOT NULL,hash BLOB NOT NULL CHECK(length(hash) = 32),size INTEGER NOT \
		 NULL,lifetime TEXT NOT NULL CHECK(lifetime IN ('ephemeral', 'session', \
		 'durable')),UNIQUE(session, ordinal),UNIQUE(session, hash)) STRICT;CREATE TABLE IF NOT \
		 EXISTS artifact_requests (principal TEXT NOT NULL,extension TEXT NOT NULL,idempotency_key \
		 TEXT NOT NULL,session TEXT NOT NULL,host_generation INTEGER NOT NULL,session_generation \
		 INTEGER NOT NULL,request_hash BLOB NOT NULL CHECK(length(request_hash) = 32),artifact_id \
		 INTEGER NOT NULL REFERENCES artifacts(id),result_session TEXT NOT NULL,result_ordinal \
		 INTEGER NOT NULL,result_hash BLOB NOT NULL CHECK(length(result_hash) = 32),result_size \
		 INTEGER NOT NULL,result_lifetime TEXT NOT NULL CHECK(result_lifetime IN ('ephemeral', \
		 'session', 'durable')),PRIMARY KEY(principal, extension, idempotency_key, session, \
		 host_generation, session_generation)) STRICT;",
	)?;
	Ok(connection)
}

fn artifact_row(row: &Row<'_>) -> rusqlite::Result<EncodedArtifact> {
	Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
}

fn decode_artifact(encoded: EncodedArtifact) -> Result<ArtifactRecord, Error> {
	let (catalog_id, session, ordinal, hash, size, lifetime) = encoded;
	let hash: [u8; 32] = hash.try_into().map_err(|_| Error::CorruptArtifactCatalog)?;
	let lifetime = lifetime
		.parse::<ArtifactLifetime>()
		.map_err(|_| Error::CorruptArtifactCatalog)?;
	Ok(ArtifactRecord {
		catalog_id,
		session: SessionId(Str::new(session)),
		ordinal,
		reference: BlobRef { hash: Hash32::new(hash), size },
		lifetime,
		pinned: lifetime == ArtifactLifetime::Durable,
	})
}

fn authoritative_size(store: &BlobStore, hash: [u8; 32]) -> Result<u64, Error> {
	let probe = BlobRef { hash: Hash32::new(hash), size: 0 };
	match fs::metadata(store.path(&probe)) {
		Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
		Ok(_) => Err(blob::Error::NotFound.into()),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Err(blob::Error::NotFound.into()),
		Err(error) => Err(error.into()),
	}
}

fn validate_record(store: &BlobStore, record: &ArtifactRecord) -> Result<(), Error> {
	let actual = authoritative_size(store, record.reference.hash.into_bytes())?;
	if actual == record.reference.size {
		Ok(())
	} else {
		Err(blob::Error::Corrupt { expected: record.reference.size, actual }.into())
	}
}

fn promote_record(
	transaction: &rusqlite::Transaction<'_>,
	record: &ArtifactRecord,
	requested: ArtifactLifetime,
) -> Result<(), Error> {
	if retention_rank(requested) < retention_rank(record.lifetime) {
		return Err(Error::LifetimeDowngrade { current: record.lifetime, requested });
	}
	if requested == record.lifetime {
		if requested == ArtifactLifetime::Durable {
			insert_durable_root(transaction, record.reference.hash.into_bytes())?;
		}
		return Ok(());
	}
	let lifetime: &'static str = requested.into();
	transaction.execute("UPDATE artifacts SET lifetime = ?1 WHERE id = ?2", params![
		lifetime,
		record.catalog_id
	])?;
	if requested == ArtifactLifetime::Durable {
		insert_durable_root(transaction, record.reference.hash.into_bytes())?;
	}
	Ok(())
}

fn insert_durable_root(
	transaction: &rusqlite::Transaction<'_>,
	hash: [u8; 32],
) -> Result<(), Error> {
	transaction.execute(
		"INSERT INTO durable_roots(hash) VALUES (?1) ON CONFLICT(hash) DO NOTHING",
		params![hash.as_slice()],
	)?;
	Ok(())
}

const fn retention_rank(lifetime: ArtifactLifetime) -> u8 {
	match lifetime {
		ArtifactLifetime::Ephemeral => 0,
		ArtifactLifetime::Session => 1,
		ArtifactLifetime::Durable => 2,
	}
}

/// Removes old blobs unreachable from every journal in `roots` and from durable
/// roots.
///
/// `roots` must come from [`SessionRoots::discover`] for this same profile-wide
/// blob store. The complete journal inventory is re-read before deletion, so a
/// missing, added, or replaced journal fails closed. Blob bodies are never
/// opened: the sweep uses only shard names, metadata, and root hashes.
/// `min_age` is the grace period for the intentional put-before-append race.
///
/// # Errors
///
/// Returns [`Error::SessionRootMismatch`] when `roots` belongs to another blob
/// store. Root revalidation, transcript parse, and database failures all happen
/// before deletion. A filesystem failure during blob traversal or removal
/// returns [`Error::Interrupted`] with exact partial accounting.
pub fn sweep(
	store: &BlobStore,
	roots: &SessionRoots,
	min_age: Duration,
) -> Result<SweepReport, Error> {
	if fs::canonicalize(store.root())? != roots.blob_root {
		return Err(Error::SessionRootMismatch);
	}
	let current_stores = discover_session_stores(store, &roots.custom_stores)?;
	if current_stores != roots.stores {
		return Err(Error::SessionRootsChanged);
	}
	let mut current = Vec::new();
	for directory in &current_stores {
		discover_journals(directory, &mut current)?;
	}
	current.sort_unstable_by(|left, right| left.1.cmp(&right.1));
	if current != roots.journals {
		return Err(Error::SessionRootsChanged);
	}

	let mut reachable = HashSet::new();
	let mut artifact_uses = ArtifactUses::default();
	let mut report = SweepReport::default();
	mark_session_roots(roots, &mut reachable, &mut artifact_uses, &mut report)?;

	let mut durable = DurableRoots::open(store)?;
	let transaction = durable
		.connection
		.transaction_with_behavior(TransactionBehavior::Immediate)?;
	mark_catalog_uses(&transaction, &artifact_uses, &mut reachable, &mut report)?;
	{
		let mut statement = transaction.prepare("SELECT hash FROM durable_roots")?;
		let mut rows = statement.query([])?;
		while let Some(row) = rows.next()? {
			let hash: Vec<u8> = row.get(0)?;
			let hash: [u8; 32] = hash.try_into().map_err(|_| Error::CorruptDurableRoot)?;
			reachable.insert(Hash32::new(hash));
		}
	}
	report.reachable_count = u64::try_from(reachable.len()).expect("blob root counts fit in u64");

	if let Err(source) = sweep_blob_directory(store, &reachable, min_age, &mut report) {
		return Err(Error::Interrupted { report, source });
	}
	transaction.commit()?;
	Ok(report)
}

fn mark_session_roots(
	roots: &SessionRoots,
	reachable: &mut HashSet<Hash32>,
	artifact_uses: &mut ArtifactUses,
	report: &mut SweepReport,
) -> Result<(), Error> {
	for (session, path) in &roots.journals {
		mark_journal(path, session, reachable, artifact_uses, report)?;
	}
	Ok(())
}

fn discover_session_stores(
	store: &BlobStore,
	custom_stores: &[PathBuf],
) -> Result<Vec<PathBuf>, Error> {
	let mut stores = Vec::new();
	add_session_store_if_present(&store.root().join(SESSIONS_DIRECTORY), &mut stores)?;

	let projects = store.root().join(PROJECTS_DIRECTORY);
	match fs::metadata(&projects) {
		Ok(metadata) if metadata.is_dir() => {
			for entry in fs::read_dir(&projects)? {
				let entry = entry?;
				if fs::metadata(entry.path())?.is_dir() {
					add_session_store_if_present(&entry.path().join(SESSIONS_DIRECTORY), &mut stores)?;
				}
			}
		},
		Ok(_) => return Err(Error::InvalidSessionStore(projects)),
		Err(error) if error.kind() == io::ErrorKind::NotFound => {},
		Err(error) => return Err(error.into()),
	}
	for custom in custom_stores {
		add_session_store(custom, &mut stores)?;
	}
	stores.sort_unstable();
	Ok(stores)
}

fn add_session_store_if_present(path: &Path, stores: &mut Vec<PathBuf>) -> Result<(), Error> {
	match fs::metadata(path) {
		Ok(_) => add_session_store(path, stores),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
		Err(error) => Err(error.into()),
	}
}

fn add_session_store(path: &Path, stores: &mut Vec<PathBuf>) -> Result<(), Error> {
	let metadata = fs::metadata(path).map_err(|error| {
		if error.kind() == io::ErrorKind::NotFound {
			Error::InvalidSessionStore(path.to_owned())
		} else {
			Error::Io(error)
		}
	})?;
	if !metadata.is_dir() {
		return Err(Error::InvalidSessionStore(path.to_owned()));
	}
	let canonical = fs::canonicalize(path)?;
	if !stores.contains(&canonical) {
		stores.push(canonical);
	}
	Ok(())
}

fn discover_journals(
	directory: &Path,
	journals: &mut Vec<(SessionId, PathBuf)>,
) -> Result<(), Error> {
	for entry in fs::read_dir(directory)? {
		let entry = entry?;
		let file_type = entry.file_type()?;
		if file_type.is_symlink() {
			return Err(Error::UnsupportedSessionSymlink(entry.path()));
		}
		if file_type.is_dir() {
			discover_journals(&entry.path(), journals)?;
		} else if file_type.is_file()
			&& entry
				.path()
				.extension()
				.is_some_and(|extension| extension == "jsonl")
		{
			let path = entry.path();
			journals.push((journal_session_id(&path)?, path));
		}
	}
	Ok(())
}

fn journal_session_id(path: &Path) -> Result<SessionId, Error> {
	let file = File::open(path)?;
	let mut reader = BufReader::new(file);
	let mut line = Vec::new();
	if reader.read_until(b'\n', &mut line)? == 0 {
		return Err(transcript::Error::MissingHeader.into());
	}
	trim_line_end(&mut line);
	Ok(read_header(&line)?.id)
}

fn mark_journal(
	path: &Path,
	session: &SessionId,
	reachable: &mut HashSet<Hash32>,
	artifact_uses: &mut ArtifactUses,
	report: &mut SweepReport,
) -> Result<(), Error> {
	let transcript = Reader::open(path)?;
	let event_count = u64::try_from(transcript.log().len()).expect("event counts fit in u64");
	let file = File::open(path)?;
	let mut reader = BufReader::new(file);
	let mut line = Vec::new();
	reader.read_until(b'\n', &mut line)?;
	line.clear();
	let mut index = 0_u64;
	while reader.read_until(b'\n', &mut line)? != 0 {
		trim_line_end(&mut line);
		let live = index >= event_count || transcript.live().contains(index);
		mark_line(&line, session, live, reachable, artifact_uses, report);
		index = index.saturating_add(1);
		line.clear();
	}
	Ok(())
}

fn mark_line(
	line: &[u8],
	session: &SessionId,
	live: bool,
	reachable: &mut HashSet<Hash32>,
	artifact_uses: &mut ArtifactUses,
	report: &mut SweepReport,
) {
	match serde_json::from_slice::<Value>(line) {
		Ok(value) => mark_value(
			&value,
			session,
			live,
			ArtifactLifetime::Session,
			reachable,
			artifact_uses,
			report,
		),
		Err(_) => mark_corrupt_line(line, session, live, reachable, artifact_uses, report),
	}
}

fn mark_value(
	value: &Value,
	session: &SessionId,
	live: bool,
	inherited_lifetime: ArtifactLifetime,
	reachable: &mut HashSet<Hash32>,
	artifact_uses: &mut ArtifactUses,
	report: &mut SweepReport,
) {
	match value {
		Value::Array(values) => {
			for value in values {
				mark_value(value, session, live, inherited_lifetime, reachable, artifact_uses, report);
			}
		},
		Value::Object(object) => {
			let lifetime = object_lifetime(object, inherited_lifetime, report);
			if let Some(hash) = object_blob_hash(object, report)
				&& (lifetime != ArtifactLifetime::Ephemeral || live)
			{
				reachable.insert(hash);
			}
			for child in object.values() {
				mark_value(child, session, live, lifetime, reachable, artifact_uses, report);
			}
		},
		Value::String(text) => {
			mark_artifact_urls(text, session, live, reachable, artifact_uses);
		},
		_ => {},
	}
}

fn mark_artifact_urls(
	text: &str,
	session: &SessionId,
	live: bool,
	reachable: &mut HashSet<Hash32>,
	artifact_uses: &mut ArtifactUses,
) {
	let mut rest = text;
	while let Some(start) = rest.find("artifact://") {
		rest = &rest[start..];
		let end = rest
			.find(|character: char| {
				character == '"'
					|| character.is_whitespace()
					|| matches!(character, ',' | '.' | ';' | '!' | '?' | '}' | ']' | ')' | '>')
			})
			.unwrap_or(rest.len());
		mark_artifact_url(&rest[..end], session, live, reachable, artifact_uses);
		rest = &rest[end..];
	}
}

fn mark_artifact_url(
	text: &str,
	session: &SessionId,
	live: bool,
	reachable: &mut HashSet<Hash32>,
	artifact_uses: &mut ArtifactUses,
) {
	if !text.starts_with("artifact://") {
		return;
	}
	let Ok(url) = ArtifactUrl::new(Str::new(text)) else {
		return;
	};
	match url.address() {
		ArtifactAddress::Ordinal(ordinal) => {
			let key = (session.0.clone(), ordinal);
			artifact_uses.physical.insert(key.clone());
			if live {
				artifact_uses.live.insert(key);
			}
		},
		ArtifactAddress::Digest(hash) => {
			if let Ok(reference) = BlobRef::parse_hex(hash, 0) {
				reachable.insert(reference.hash);
			}
		},
	}
}

fn mark_catalog_uses(
	transaction: &rusqlite::Transaction<'_>,
	uses: &ArtifactUses,
	reachable: &mut HashSet<Hash32>,
	report: &mut SweepReport,
) -> Result<(), Error> {
	let mut statement = transaction
		.prepare("SELECT hash, lifetime FROM artifacts WHERE session = ?1 AND ordinal = ?2")?;
	for (session, ordinal) in &uses.physical {
		let encoded: Option<(Vec<u8>, String)> = statement
			.query_row(params![session.as_str(), ordinal], |row| Ok((row.get(0)?, row.get(1)?)))
			.optional()?;
		let Some((hash, lifetime)) = encoded else {
			report.corrupt_references = report.corrupt_references.saturating_add(1);
			continue;
		};
		let hash: [u8; 32] = hash.try_into().map_err(|_| Error::CorruptArtifactCatalog)?;
		let lifetime = lifetime
			.parse::<ArtifactLifetime>()
			.map_err(|_| Error::CorruptArtifactCatalog)?;
		if lifetime != ArtifactLifetime::Ephemeral || uses.live.contains(&(session.clone(), *ordinal))
		{
			reachable.insert(Hash32::new(hash));
		}
	}
	Ok(())
}

fn object_lifetime(
	object: &Map<String, Value>,
	inherited: ArtifactLifetime,
	report: &mut SweepReport,
) -> ArtifactLifetime {
	let Some(value) = object.get("lifetime") else {
		return inherited;
	};
	let Some(text) = value.as_str() else {
		report.corrupt_references = report.corrupt_references.saturating_add(1);
		return ArtifactLifetime::Session;
	};
	text.parse::<ArtifactLifetime>().unwrap_or_else(|_| {
		report.corrupt_references = report.corrupt_references.saturating_add(1);
		ArtifactLifetime::Session
	})
}

fn object_blob_hash(object: &Map<String, Value>, report: &mut SweepReport) -> Option<Hash32> {
	let (hash, length) = if let Some(hash) = object.get("h") {
		(hash, object.get("n"))
	} else if object.contains_key("byte_len") {
		(object.get("hash")?, object.get("byte_len"))
	} else {
		return None;
	};
	let length_valid = length.and_then(Value::as_u64).is_some();
	let parsed = hash
		.as_str()
		.and_then(|hash| BlobRef::parse_hex(hash, 0).ok())
		.map(|reference| reference.hash);
	if parsed.is_none() || !length_valid {
		report.corrupt_references = report.corrupt_references.saturating_add(1);
	}
	parsed
}

fn mark_corrupt_line(
	line: &[u8],
	session: &SessionId,
	live: bool,
	reachable: &mut HashSet<Hash32>,
	artifact_uses: &mut ArtifactUses,
	report: &mut SweepReport,
) {
	for key in [b"\"h\"".as_slice(), b"\"hash\"".as_slice()] {
		let mut rest = line;
		while let Some(position) = find_bytes(rest, key) {
			rest = &rest[position + key.len()..];
			let Some(colon) = rest.iter().position(|byte| *byte == b':') else {
				break;
			};
			rest = &rest[colon + 1..];
			let value = rest
				.iter()
				.position(|byte| !byte.is_ascii_whitespace())
				.unwrap_or(rest.len());
			if rest.get(value) != Some(&b'\"') || rest.len() < value + 66 {
				report.corrupt_references = report.corrupt_references.saturating_add(1);
				continue;
			}
			let hash = &rest[value + 1..value + 65];
			if rest[value + 65] != b'\"' {
				report.corrupt_references = report.corrupt_references.saturating_add(1);
				continue;
			}
			match str::from_utf8(hash)
				.ok()
				.and_then(|hash| BlobRef::parse_hex(hash, 0).ok())
			{
				Some(reference) => {
					reachable.insert(reference.hash);
				},
				None => {
					report.corrupt_references = report.corrupt_references.saturating_add(1);
				},
			}
		}
	}
	if let Ok(text) = str::from_utf8(line) {
		mark_artifact_urls(text, session, live, reachable, artifact_uses);
	}
}

fn sweep_blob_directory(
	store: &BlobStore,
	reachable: &HashSet<Hash32>,
	min_age: Duration,
	report: &mut SweepReport,
) -> io::Result<()> {
	let blobs = store.root().join("blobs");
	let now = SystemTime::now();
	for first in fs::read_dir(blobs)? {
		let first = first?;
		if !first.file_type()?.is_dir() {
			continue;
		}
		for second in fs::read_dir(first.path())? {
			let second = second?;
			if !second.file_type()?.is_dir() {
				continue;
			}
			for candidate in fs::read_dir(second.path())? {
				let candidate = candidate?;
				if !candidate.file_type()?.is_file() {
					continue;
				}
				let Some(hash) = candidate_hash(&candidate.path()) else {
					continue;
				};
				let metadata = candidate.metadata()?;
				let size = metadata.len();
				report.examined_count = report.examined_count.saturating_add(1);
				report.examined_bytes = report.examined_bytes.saturating_add(size);
				if reachable.contains(&hash) || is_recent(&metadata, now, min_age)? {
					continue;
				}
				match fs::remove_file(candidate.path()) {
					Ok(()) => {
						report.reclaimed_count = report.reclaimed_count.saturating_add(1);
						report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(size);
					},
					Err(error) if error.kind() == io::ErrorKind::NotFound => {},
					Err(error) => return Err(error),
				}
			}
		}
	}
	Ok(())
}

fn candidate_hash(path: &Path) -> Option<Hash32> {
	let name = path.file_name()?.to_str()?;
	let reference = BlobRef::parse_hex(name, 0).ok()?;
	let second = path.parent()?.file_name()?.to_str()?;
	let first = path.parent()?.parent()?.file_name()?.to_str()?;
	if first == &name[..2] && second == &name[2..4] {
		Some(reference.hash)
	} else {
		None
	}
}

fn is_recent(metadata: &fs::Metadata, now: SystemTime, min_age: Duration) -> io::Result<bool> {
	let modified = metadata.modified()?;
	Ok(now
		.duration_since(modified)
		.map_or(true, |age| age < min_age))
}

fn trim_line_end(line: &mut Vec<u8>) {
	if line.last() == Some(&b'\n') {
		line.pop();
	}
	if line.last() == Some(&b'\r') {
		line.pop();
	}
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack
		.windows(needle.len())
		.position(|window| window == needle)
}
