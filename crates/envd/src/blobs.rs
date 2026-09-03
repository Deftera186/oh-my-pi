//! Content-addressed env blob storage and hash-only result references.

use std::{
	fs, io,
	path::Path,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use omp_core::{Hash32, Str, sf};
use omp_journal::blob::{self, BlobRef, BlobStage, BlobStore};
use omp_proto::blob::v1 as blob_pb;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use tokio::task;

/// Stable content identity returned by blob host operations.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BlobId {
	/// Raw SHA-256 content digest.
	pub hash: [u8; 32],
	/// Exact byte length of the content.
	pub size: u64,
}

impl From<BlobRef> for BlobId {
	fn from(reference: BlobRef) -> Self {
		Self { hash: reference.hash.into_bytes(), size: reference.size }
	}
}

impl From<BlobId> for BlobRef {
	fn from(id: BlobId) -> Self {
		Self { hash: Hash32::new(id.hash), size: id.size }
	}
}

/// A complete or ranged blob read without text encoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobRead {
	/// Identity of the complete stored content.
	pub id:   BlobId,
	/// Requested complete or ranged content bytes.
	pub data: Bytes,
}

/// A blob request or backing-store operation failed.
#[derive(Debug, Error)]
pub enum BlobError {
	/// Backing blob storage error.
	#[error(transparent)]
	Store(#[from] blob::Error),
	/// Blocking blob finalization task failed.
	#[error("blob finalization task failed: {0}")]
	FinalizeTask(#[from] task::JoinError),
	/// Blob hash format was not 32 bytes.
	#[error("blob hash must be exactly 32 bytes")]
	InvalidHash,
	/// Blob bytes did not match the expected digest.
	#[error("uploaded blob digest differs from the expected digest")]
	HashMismatch,
	/// Blob byte count differed from expected.
	#[error("uploaded blob size differs from expected {expected} bytes (received {actual})")]
	SizeMismatch {
		/// Expected byte count.
		expected: u64,
		/// Received byte count.
		actual:   u64,
	},
	/// Requested range started beyond content bounds.
	#[error("blob range starts after the end of the content")]
	InvalidRange,
	/// Content length exceeded host address limits.
	#[error("blob length cannot be represented on this host")]
	LengthOverflow,
	/// Underlying filesystem removal operation failed.
	#[error("blob removal failed: {0}")]
	Remove(#[source] io::Error),
	/// Durable artifact metadata database failed.
	#[error("artifact metadata operation failed: {0}")]
	ArtifactMetadata(#[from] rusqlite::Error),
	/// The host clock cannot produce an artifact creation time.
	#[error("artifact creation time is before the Unix epoch")]
	ArtifactClock,
}
/// JSON-facing metadata complement to the content/retention catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactMetadata {
	/// Monotonic catalog identity.
	pub catalog_id:  u64,
	/// Media type supplied at adoption, defaulted once by the authority.
	pub media_type:  Str,
	/// Optional user-facing description.
	pub description: Option<Str>,
	/// Core-authenticated source identity.
	pub source:      Str,
	/// Durable creation time.
	pub created_ms:  u64,
}

/// Durable owner of artifact metadata not represented by the GC roots table.
#[derive(Clone)]
pub struct ArtifactMetadataStore {
	connection: Arc<Mutex<Connection>>,
}

impl ArtifactMetadataStore {
	/// Opens the metadata table beside the authoritative blob catalog.
	pub fn open(store: &BlobStore) -> Result<Self, BlobError> {
		let connection = Connection::open(store.root().join("artifact-metadata.sqlite3"))?;
		connection.pragma_update(None, "journal_mode", "WAL")?;
		connection.pragma_update(None, "synchronous", "FULL")?;
		connection.execute_batch(
			"CREATE TABLE IF NOT EXISTS artifact_metadata (
			 catalog_id INTEGER PRIMARY KEY,
			 media_type TEXT NOT NULL,
			 description TEXT,
			 source TEXT NOT NULL,
			 created_ms INTEGER NOT NULL
			 ) WITHOUT ROWID;",
		)?;
		Ok(Self { connection: Arc::new(Mutex::new(connection)) })
	}

	/// Records metadata exactly once so retries cannot rewrite provenance.
	pub fn record(
		&self,
		catalog_id: u64,
		media_type: Option<&str>,
		description: Option<&str>,
		source: &str,
	) -> Result<ArtifactMetadata, BlobError> {
		let created_ms = u64::try_from(
			SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.map_err(|_| BlobError::ArtifactClock)?
				.as_millis(),
		)
		.unwrap_or(u64::MAX);
		let media_type = media_type.unwrap_or("application/octet-stream");
		let connection = self.connection.lock();
		connection.execute(
			"INSERT OR IGNORE INTO artifact_metadata(
			 catalog_id, media_type, description, source, created_ms
			 ) VALUES (?1, ?2, ?3, ?4, ?5)",
			params![catalog_id, media_type, description, source, created_ms],
		)?;
		drop(connection);
		self
			.get(catalog_id)?
			.ok_or_else(|| BlobError::ArtifactMetadata(rusqlite::Error::QueryReturnedNoRows))
	}

	/// Loads metadata for one catalog record.
	pub fn get(&self, catalog_id: u64) -> Result<Option<ArtifactMetadata>, BlobError> {
		self
			.connection
			.lock()
			.query_row(
				"SELECT catalog_id, media_type, description, source, created_ms
				 FROM artifact_metadata WHERE catalog_id = ?1",
				[catalog_id],
				|row| {
					Ok(ArtifactMetadata {
						catalog_id:  row.get(0)?,
						media_type:  Str::new(row.get::<_, String>(1)?),
						description: row.get::<_, Option<String>>(2)?.map(Str::new),
						source:      Str::new(row.get::<_, String>(3)?),
						created_ms:  row.get(4)?,
					})
				},
			)
			.optional()
			.map_err(BlobError::from)
	}
}

/// Concrete env-side owner of a filesystem-backed content-addressed store.
#[derive(Clone, Debug)]
pub struct BlobHost {
	store: BlobStore,
}

impl BlobHost {
	/// Opens or creates a content-addressed store beneath `root`.
	pub fn open(root: impl AsRef<Path>) -> Result<Self, BlobError> {
		Ok(Self { store: BlobStore::open(root.as_ref())? })
	}

	/// Takes ownership of an already-open store.
	pub const fn from_store(store: BlobStore) -> Self {
		Self { store }
	}

	/// Borrows the single blob authority for metadata and retention operations.
	pub(crate) const fn store(&self) -> &BlobStore {
		&self.store
	}

	/// Opens the single staged minting path shared by every blob producer.
	pub(crate) fn begin_spill(&self) -> Result<BlobStage, BlobError> {
		self.store.begin_put().map_err(BlobError::from)
	}

	/// Stores exact bytes and returns their SHA-256-derived identity.
	pub fn put(&self, data: &[u8]) -> Result<BlobId, BlobError> {
		self
			.store
			.put(data)
			.map(BlobId::from)
			.map_err(BlobError::from)
	}

	/// Stores bytes while validating optional upload-stream preconditions.
	pub fn put_checked(
		&self,
		data: &[u8],
		expected_hash: Option<&[u8]>,
		expected_size: Option<u64>,
	) -> Result<BlobId, BlobError> {
		let expected_hash = expected_hash.map(parse_hash).transpose()?;
		let actual_size = u64::try_from(data.len()).map_err(|_| BlobError::LengthOverflow)?;
		if let Some(expected) = expected_size
			&& expected != actual_size
		{
			return Err(BlobError::SizeMismatch { expected, actual: actual_size });
		}
		if expected_hash.is_some_and(|expected| expected != Hash32::sum(data).into_bytes()) {
			return Err(BlobError::HashMismatch);
		}
		self.put(data)
	}

	/// Stores exact bytes and returns the env wire response.
	pub fn put_response(&self, data: &[u8]) -> Result<blob_pb::PutResponse, BlobError> {
		let id = self.put(data)?;
		Ok(blob_pb::PutResponse { hash: Bytes::copy_from_slice(&id.hash), size: id.size })
	}

	/// Returns presence and size for a raw SHA-256 digest.
	pub fn stat(&self, hash: &[u8]) -> Result<blob_pb::StatResponse, BlobError> {
		let hash = parse_hash(hash)?;
		let probe = BlobRef { hash: Hash32::new(hash), size: 0 };
		match fs::metadata(self.store.path(&probe)) {
			Ok(metadata) if metadata.is_file() => {
				Ok(blob_pb::StatResponse { present: true, size: metadata.len() })
			},
			Ok(_) => Ok(blob_pb::StatResponse { present: false, size: 0 }),
			Err(error) if error.kind() == io::ErrorKind::NotFound => {
				Ok(blob_pb::StatResponse { present: false, size: 0 })
			},
			Err(error) => Err(BlobError::Store(error.into())),
		}
	}

	/// Reads a complete blob by content identity.
	pub fn get(&self, id: BlobId) -> Result<Bytes, BlobError> {
		self.store.get(&id.into()).map_err(BlobError::from)
	}

	/// Reads the env wire range without base64 or another text projection.
	pub fn get_request(&self, request: &blob_pb::GetRequest) -> Result<BlobRead, BlobError> {
		let hash = parse_hash(&request.hash)?;
		let stat = self.stat(&request.hash)?;
		if !stat.present {
			return Err(BlobError::Store(blob::Error::NotFound));
		}
		if request.offset > stat.size {
			return Err(BlobError::InvalidRange);
		}
		let available = stat.size - request.offset;
		let length = if request.length == 0 {
			available
		} else {
			request.length.min(available)
		};
		let end = request
			.offset
			.checked_add(length)
			.ok_or(BlobError::InvalidRange)?;
		let start = usize::try_from(request.offset).map_err(|_| BlobError::LengthOverflow)?;
		let end = usize::try_from(end).map_err(|_| BlobError::LengthOverflow)?;
		let id = BlobId { hash, size: stat.size };
		let data = self.get(id)?.slice(start..end);
		Ok(BlobRead { id, data })
	}

	/// Removes a raw digest and reports whether content existed.
	pub fn delete(&self, hash: &[u8]) -> Result<blob_pb::DeleteResponse, BlobError> {
		let hash = parse_hash(hash)?;
		let probe = BlobRef { hash: Hash32::new(hash), size: 0 };
		match fs::remove_file(self.store.path(&probe)) {
			Ok(()) => Ok(blob_pb::DeleteResponse { deleted: true }),
			Err(error) if error.kind() == io::ErrorKind::NotFound => {
				Ok(blob_pb::DeleteResponse { deleted: false })
			},
			Err(error) => Err(BlobError::Remove(error)),
		}
	}
}

mod references {
	use bytes::Bytes;
	use omp_core::Str;
	use omp_proto::thread::v1::{self as thread_pb, blob};

	use super::{BlobError, BlobHost, BlobId};

	impl BlobHost {
		/// Creates the canonical hash-only media/result shape used by thread
		/// parts.
		pub fn reference(&self, id: BlobId, mime: Str, detail: blob::Detail) -> thread_pb::Blob {
			thread_pb::Blob {
				hash:   Bytes::copy_from_slice(&id.hash),
				mime:   mime.into(),
				size:   id.size,
				inline: Bytes::new(),
				detail: detail.into(),
			}
		}

		/// Stores media/result bytes and returns their canonical hash-only shape.
		pub fn put_reference(
			&self,
			data: &[u8],
			mime: Str,
			detail: blob::Detail,
		) -> Result<thread_pb::Blob, BlobError> {
			let id = self.put(data)?;
			Ok(self.reference(id, mime, detail))
		}
	}
}

impl omp_tool::CallOutcomeSpill for BlobHost {
	type Error = BlobError;
	type Stage<'a> = BlobStage;

	fn open(&self) -> Result<Self::Stage<'_>, Self::Error> {
		self.begin_spill()
	}

	async fn finish<'a>(&'a self, stage: Self::Stage<'a>) -> Result<omp_tool::BlobRef, Self::Error> {
		let reference = task::spawn_blocking(move || stage.finish()).await??;
		Ok(call_outcome_reference(reference))
	}
}

fn call_outcome_reference(reference: BlobRef) -> omp_tool::BlobRef {
	let hash = reference.hash.to_hex();
	omp_tool::BlobRef {
		hash:       Str::from(hash.as_str()),
		media_type: sf!("application/json"),
		byte_len:   reference.size,
	}
}

fn parse_hash(hash: &[u8]) -> Result<[u8; 32], BlobError> {
	hash.try_into().map_err(|_| BlobError::InvalidHash)
}

#[cfg(test)]
mod tests {
	use std::{
		fs,
		io::Write as _,
		path::{Path, PathBuf},
	};

	use omp_core::{Hash32, Str};
	use omp_tool::{CallOutcome, CallOutcomeDetails, CallOutcomeDetailsError, call_outcome_details};
	use tempfile::TempDir;

	use super::{BlobError, BlobHost, BlobId};

	fn open_host() -> (TempDir, BlobHost) {
		let root = tempfile::tempdir().expect("temporary blob root");
		let host = BlobHost::open(root.path()).expect("open blob host");
		(root, host)
	}

	fn tmp_dir(root: &Path) -> PathBuf {
		root.join("tmp")
	}

	fn poison_tmp_dir(root: &Path) {
		let tmp = tmp_dir(root);
		fs::remove_dir(&tmp).expect("remove empty temporary directory");
		fs::File::create(tmp).expect("replace temporary directory with a file");
	}

	#[tokio::test]
	async fn inline_outcome_never_opens_a_blob_stage() {
		let (root, host) = open_host();
		poison_tmp_dir(root.path());
		let outcome = CallOutcome::<u8, u8>::Ok(7);

		let details = call_outcome_details(&outcome, 1_024, &host)
			.await
			.expect("inline serialization must not touch poisoned blob staging");

		assert!(matches!(details, CallOutcomeDetails::Inline { .. }));
	}

	#[tokio::test]
	async fn spilled_outcome_retains_exact_bytes_digest_and_size() {
		let (_root, host) = open_host();
		let outcome = CallOutcome::<Str, Str>::Ok(omp_core::sf!("payload beyond the inline limit",));
		let expected = serde_json::to_vec(&outcome).expect("serialize expected outcome");
		let expected_hash = Hash32::sum(&expected);

		let details = call_outcome_details(&outcome, 1, &host)
			.await
			.expect("spill outcome");
		let CallOutcomeDetails::Spilled { blob, byte_len } = details else {
			panic!("outcome larger than one byte must spill");
		};

		assert_eq!(byte_len, expected.len() as u64);
		assert_eq!(blob.byte_len, expected.len() as u64);
		assert_eq!(blob.media_type.as_str(), "application/json");
		assert_eq!(blob.hash.as_str(), expected_hash.to_hex().as_str());
		assert_eq!(
			host
				.get(BlobId { hash: expected_hash.into(), size: expected.len() as u64 })
				.expect("read spilled outcome")
				.as_ref(),
			expected.as_slice()
		);
	}

	#[test]
	fn dropping_an_unfinished_spill_removes_its_temporary_file() {
		let (root, host) = open_host();
		let mut stage = host.begin_spill().expect("begin staged spill");
		stage
			.write_all(b"cancelled bytes")
			.expect("write staged bytes");
		assert_eq!(
			fs::read_dir(tmp_dir(root.path()))
				.expect("read tmp")
				.count(),
			1
		);

		drop(stage);

		assert_eq!(
			fs::read_dir(tmp_dir(root.path()))
				.expect("read tmp")
				.count(),
			0
		);
	}

	#[tokio::test]
	async fn storage_open_errors_remain_typed() {
		let (root, host) = open_host();
		poison_tmp_dir(root.path());
		let outcome = CallOutcome::<u8, u8>::Ok(7);

		let error = call_outcome_details(&outcome, 0, &host)
			.await
			.expect_err("poisoned staging directory must fail");

		assert!(matches!(error, CallOutcomeDetailsError::SpillOpen(BlobError::Store(_))));
	}

	#[test]
	fn blob_host_is_clone_send_and_sync() {
		fn assert_traits<T: Clone + Send + Sync>() {}
		assert_traits::<BlobHost>();
	}
}
