//! Crash-tolerant session journal and content-addressed blob storage.
//!
//! Each session is one flat raw-SSE `.oms` file. A blank line commits an
//! entry; opening a journal truncates only bytes after the last commit point.
//! Branches are `prior` links, so rewinding never destroys abandoned history.

pub mod blob;
mod chain;
pub mod data;
mod entry;
pub mod gc;
pub mod kind;
pub mod sse;
pub mod ulid;

use std::{
	fs::{self, File, OpenOptions},
	io::{self, Write as _},
	path::{Path, PathBuf},
};

pub use chain::{abandoned, live_chain};
pub use entry::{Entry, EntryDraft, EntryId};
pub use kind::{Kind, KindError, KindName};
use omp_core::{FastHashSet, Str, Ulid};
use thiserror::Error;

use crate::{
	kind::JOURNAL,
	sse::{Scanner, SseError},
	ulid::{MonotonicUlid, UlidGenerationError},
};

/// Conventional journal file extension.
pub const FILE_EXTENSION: &str = "oms";

/// The single writer for one session journal.
#[derive(Debug)]
pub struct Journal {
	path:                 PathBuf,
	file:                 File,
	generator:            MonotonicUlid,
	ids:                  FastHashSet<EntryId>,
	entry_count:          usize,
	recovered_tail_bytes: u64,
}

impl Journal {
	/// Creates a new empty journal at `path`.
	///
	/// The first appended entry must be the `journal@1` genesis.
	///
	/// # Errors
	///
	/// Returns [`JournalError::Io`] if the parent cannot be created or the file
	/// already exists.
	pub fn create(path: impl AsRef<Path>) -> Result<Self, JournalError> {
		let path = path.as_ref().to_path_buf();
		if let Some(parent) = path
			.parent()
			.filter(|parent| !parent.as_os_str().is_empty())
		{
			fs::create_dir_all(parent)?;
		}
		let file = OpenOptions::new()
			.create_new(true)
			.append(true)
			.read(true)
			.open(&path)?;
		Ok(Self {
			path,
			file,
			generator: MonotonicUlid::default(),
			ids: FastHashSet::default(),
			entry_count: 0,
			recovered_tail_bytes: 0,
		})
	}

	/// Opens an existing journal, returning its committed entries.
	///
	/// Bytes after the last committing blank line are physically truncated.
	/// The number removed is available through [`Self::recovered_tail_bytes`].
	///
	/// # Errors
	///
	/// Returns a typed error for I/O, malformed complete frames, invalid journal
	/// structure, or invalid branch links.
	pub fn open(path: impl AsRef<Path>) -> Result<(Self, Vec<Entry>), JournalError> {
		let path = path.as_ref().to_path_buf();
		let bytes = fs::read(&path)?;
		let mut scanner = Scanner::new(&bytes);
		let mut entries = Vec::new();
		while let Some(frame) = scanner.next() {
			entries.push(
				frame
					.map_err(|source| JournalError::Frame { source })?
					.entry,
			);
		}
		let clean_len = scanner.offset();
		let truncated = bytes.len().saturating_sub(clean_len);
		validate_history(&entries)?;

		let file = OpenOptions::new()
			.append(true)
			.read(true)
			.write(true)
			.open(&path)?;
		if truncated != 0 {
			file.set_len(u64::try_from(clean_len).map_err(|_| JournalError::FileTooLarge)?)?;
			file.sync_data()?;
		}
		let mut ids = FastHashSet::default();
		let mut floor = None;
		for entry in &entries {
			ids.insert(entry.id);
			let id = entry.id.as_ulid();
			floor = Some(floor.map_or(id, |prior: Ulid| prior.max(id)));
		}
		Ok((
			Self {
				path,
				file,
				generator: MonotonicUlid::seeded(floor),
				ids,
				entry_count: entries.len(),
				recovered_tail_bytes: truncated as u64,
			},
			entries,
		))
	}

	/// Appends and durably commits one entry.
	///
	/// # Errors
	///
	/// Returns a typed error when the draft violates genesis/cause/branch rules,
	/// its payload is not single-line JSON or exceeds one mebibyte, identity
	/// generation is exhausted, or the durable write fails.
	pub fn append(&mut self, draft: EntryDraft) -> Result<Entry, JournalError> {
		validate_draft(&draft, self.entry_count, &self.ids)?;
		let id = EntryId::from(self.generator.generate()?);
		let entry = Entry {
			id,
			kind: draft.kind,
			by: draft.by,
			prior: draft.prior,
			label: draft.label,
			data: draft.data,
		};
		let mut encoded = Vec::with_capacity(entry.data.len() + 160);
		sse::encode(&entry, &mut encoded).map_err(map_sse_write_error)?;
		self.file.write_all(&encoded)?;
		self.file.sync_data()?;
		self.ids.insert(id);
		self.entry_count += 1;
		Ok(entry)
	}

	/// Returns the journal file path.
	#[must_use]
	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Returns the number of torn-tail bytes removed by [`Self::open`].
	#[must_use]
	pub const fn recovered_tail_bytes(&self) -> u64 {
		self.recovered_tail_bytes
	}
}

/// Journal creation, recovery, validation, and append failure.
#[derive(Debug, Error)]
pub enum JournalError {
	/// A filesystem operation failed.
	#[error("journal I/O failed")]
	Io(#[from] io::Error),
	/// A complete SSE frame is malformed.
	#[error("journal contains a malformed complete frame")]
	Frame {
		/// SSE codec failure.
		#[source]
		source: SseError,
	},
	/// A kind is outside the closed revision-1 vocabulary.
	#[error("journal kind {kind} is not in the closed revision-1 vocabulary")]
	UnknownKind {
		/// Unsupported versioned kind.
		kind: Kind,
	},
	/// A non-genesis entry has no causal `by` link.
	#[error("non-genesis entry {kind} requires a `by` cause")]
	MissingCause {
		/// Offending versioned kind.
		kind: Kind,
	},
	/// A non-genesis entry appeared before the genesis.
	#[error("the first journal entry must be journal@1 genesis")]
	GenesisMustBeFirst,
	/// A genesis entry appeared after the first position.
	#[error("journal@1 genesis may appear only as the first entry")]
	GenesisOnlyFirst,
	/// An explicit `prior` link does not name an earlier entry.
	#[error("journal prior entry {id} does not exist earlier in the file")]
	UnknownPrior {
		/// Missing branch target.
		id: EntryId,
	},
	/// An opened file repeats an entry identity.
	#[error("journal entry id {id} is duplicated")]
	DuplicateId {
		/// Repeated identity.
		id: EntryId,
	},
	/// The JSON payload exceeds one mebibyte.
	#[error("journal data payload is {len} bytes; maximum is 1048576")]
	DataTooLarge {
		/// Payload byte length.
		len: usize,
	},
	/// The JSON payload contains a physical line break.
	#[error("journal data payload must occupy one physical line")]
	MultilineData,
	/// The optional operation label contains a physical line break.
	#[error("journal operation label must occupy one physical line")]
	MultilineLabel,
	/// The JSON payload is malformed.
	#[error("journal data payload is invalid JSON")]
	InvalidData {
		/// JSON decoder failure.
		#[source]
		source: serde_json::Error,
	},
	/// The platform cannot represent the journal file length.
	#[error("journal file length cannot be represented")]
	FileTooLarge,
	/// No larger ULID can be generated.
	#[error(transparent)]
	Ulid(#[from] UlidGenerationError),
}

fn validate_history(entries: &[Entry]) -> Result<(), JournalError> {
	let mut ids = FastHashSet::default();
	for (index, entry) in entries.iter().enumerate() {
		validate_entry(entry, index, &ids)?;
		if !ids.insert(entry.id) {
			return Err(JournalError::DuplicateId { id: entry.id });
		}
	}
	Ok(())
}

fn validate_draft(
	draft: &EntryDraft,
	index: usize,
	ids: &FastHashSet<EntryId>,
) -> Result<(), JournalError> {
	validate_fields(&draft.kind, draft.by, draft.prior, &draft.label, &draft.data, index, ids)
}

fn validate_entry(
	entry: &Entry,
	index: usize,
	ids: &FastHashSet<EntryId>,
) -> Result<(), JournalError> {
	validate_fields(&entry.kind, entry.by, entry.prior, &entry.label, &entry.data, index, ids)
}

fn validate_fields(
	kind: &Kind,
	by: Option<EntryId>,
	prior: Option<EntryId>,
	label: &Option<Str>,
	data: &Str,
	index: usize,
	ids: &FastHashSet<EntryId>,
) -> Result<(), JournalError> {
	if !kind.is_known() {
		return Err(JournalError::UnknownKind { kind: kind.clone() });
	}
	let genesis = kind.name.as_str() == JOURNAL && kind.rev == 1;
	if !genesis && by.is_none() {
		return Err(JournalError::MissingCause { kind: kind.clone() });
	}
	if index == 0 && !genesis {
		return Err(JournalError::GenesisMustBeFirst);
	}
	if index != 0 && genesis {
		return Err(JournalError::GenesisOnlyFirst);
	}
	if let Some(prior) = prior
		&& !ids.contains(&prior)
	{
		return Err(JournalError::UnknownPrior { id: prior });
	}
	if data.len() > sse::DATA_HARD_CAP {
		return Err(JournalError::DataTooLarge { len: data.len() });
	}
	if data.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
		return Err(JournalError::MultilineData);
	}
	if label
		.as_ref()
		.is_some_and(|value| value.bytes().any(|byte| matches!(byte, b'\r' | b'\n')))
	{
		return Err(JournalError::MultilineLabel);
	}
	serde_json::from_str::<serde::de::IgnoredAny>(data)
		.map(|_| ())
		.map_err(|source| JournalError::InvalidData { source })
}

fn map_sse_write_error(source: SseError) -> JournalError {
	match source {
		SseError::DataTooLarge { len } => JournalError::DataTooLarge { len },
		SseError::MultilineData => JournalError::MultilineData,
		SseError::MultilineLabel => JournalError::MultilineLabel,
		SseError::InvalidData { source } => JournalError::InvalidData { source },
		other => JournalError::Frame { source: other },
	}
}
