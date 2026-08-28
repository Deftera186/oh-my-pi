//! Append-only transcript file writer.
//!
//! Error paths summarize affected physical indexes as [`IndexRun`] because
//! appends assign consecutive indexes in write order.

use std::{
	fs,
	fs::{File, OpenOptions},
	io,
	io::{Seek, SeekFrom, Write},
	iter::FusedIterator,
	mem::size_of,
	path::{Path, PathBuf},
	slice,
};

use bytes::BytesMut;
use smallvec::SmallVec;
use thiserror::Error as ThisError;

use super::{
	Entry, Reader,
	codec::{Error, Header, write_atomic_group, write_header, write_line},
	event::{Event, Kind},
};
use crate::atomic;

/// Maximum number of events in one atomic transcript append.
pub const MAX_ATOMIC_ENTRIES: usize = 1_024;

/// A compact contiguous run of durable event indexes carried by errors.
///
/// Writer failures always identify either a proven prefix or a possibly written
/// atomic group. Both are contiguous because physical indexes are assigned in
/// append order, so retaining every index would only duplicate that fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexRun {
	first: u64,
	count: u64,
}

impl IndexRun {
	/// Creates a run from contiguous durable event indexes.
	///
	/// Debug builds assert that every adjacent pair is consecutive. An empty
	/// slice produces an empty run.
	pub fn from_contiguous(indexes: &[u64]) -> Self {
		debug_assert!(
			indexes
				.windows(2)
				.all(|pair| { pair[0].checked_add(1).is_some_and(|next| pair[1] == next) })
		);
		Self {
			first: indexes.first().copied().unwrap_or_default(),
			count: u64::try_from(indexes.len()).expect("index count fits in u64"),
		}
	}

	/// Returns the first durable event index, if this run is non-empty.
	pub const fn first(self) -> Option<u64> {
		if self.count == 0 {
			None
		} else {
			Some(self.first)
		}
	}

	/// Returns the number of durable event indexes in this run.
	pub fn len(self) -> usize {
		usize::try_from(self.count).expect("index count fits in usize")
	}

	/// Returns whether this run contains no durable event indexes.
	pub const fn is_empty(self) -> bool {
		self.count == 0
	}
}

/// Iterator over the durable event indexes in an [`IndexRun`].
#[derive(Debug, Clone)]
pub struct IndexRunIter {
	next:      u64,
	remaining: u64,
}

impl Iterator for IndexRunIter {
	type Item = u64;

	fn next(&mut self) -> Option<Self::Item> {
		if self.remaining == 0 {
			return None;
		}

		let index = self.next;
		self.remaining -= 1;
		if self.remaining != 0 {
			self.next = self
				.next
				.checked_add(1)
				.expect("contiguous index run must not overflow");
		}
		Some(index)
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		match usize::try_from(self.remaining) {
			Ok(remaining) => (remaining, Some(remaining)),
			Err(_) => (usize::MAX, None),
		}
	}
}

impl FusedIterator for IndexRunIter {}

impl IntoIterator for IndexRun {
	type IntoIter = IndexRunIter;
	type Item = u64;

	/// Iterates over every durable event index in the run.
	fn into_iter(self) -> Self::IntoIter {
		IndexRunIter { next: self.first, remaining: self.count }
	}
}

/// A transcript writer that owns the single header and appends event lines.
pub struct Writer {
	file:       Option<File>,
	pending:    Option<(PathBuf, Header)>,
	next_index: u64,
	line:       BytesMut,
	poisoned:   bool,
}

trait AppendTarget: Write {
	fn append_len(&self) -> io::Result<u64>;
	fn rollback_to(&mut self, len: u64) -> io::Result<()>;
	fn sync_data(&mut self) -> io::Result<()>;
}

impl AppendTarget for File {
	fn append_len(&self) -> io::Result<u64> {
		Ok(self.metadata()?.len())
	}

	fn rollback_to(&mut self, len: u64) -> io::Result<()> {
		self.set_len(len)?;
		self.seek(SeekFrom::Start(len))?;
		Self::sync_data(self)
	}

	fn sync_data(&mut self) -> io::Result<()> {
		Self::sync_data(self)
	}
}

/// A journal write failure with a proven or indeterminate outcome.
#[derive(Debug, ThisError)]
pub enum JournalError {
	/// The attempted append was rolled back completely.
	#[error("journal append failed and was rolled back: {source}")]
	RolledBack {
		/// Encoding, write, or durability failure.
		#[source]
		source: Error,
	},
	/// The attempted append could not be rolled back and the writer is halted.
	#[error(transparent)]
	Indeterminate(#[from] JournalIndeterminate),
	/// The requested atomic group exceeded [`MAX_ATOMIC_ENTRIES`].
	#[error("atomic journal append has {entries} entries; maximum is {maximum}")]
	TooManyEntries {
		/// Requested event count.
		entries: usize,
		/// Supported event count ceiling.
		maximum: usize,
	},
}

/// A journal failure whose durable outcome cannot be proven.
#[derive(Debug, ThisError)]
#[error("journal durability is indeterminate and the writer is halted: {source}")]
pub struct JournalIndeterminate {
	/// Write and rollback failure pair.
	#[source]
	pub source:   Error,
	/// Physical indexes whose bytes may have been written.
	pub written:  IndexRun,
	/// Whether the single-owner writer refuses subsequent appends.
	pub poisoned: bool,
}

/// Failure from a non-atomic multi-event append.
///
/// `appended` contains exactly the physical indexes proven to remain in the
/// transcript. The indeterminate variant reports potentially written indexes
/// separately through [`JournalIndeterminate::written`].
#[derive(Debug, ThisError)]
#[error("transcript batch append failed after {count} events: {source}", count = .appended.len())]
pub struct AppendManyError {
	/// Proven or indeterminate journal outcome.
	#[source]
	pub source:   JournalError,
	/// Physical indexes proven to remain appended by this operation.
	pub appended: IndexRun,
}

const _: () = assert!(size_of::<IndexRun>() == 16, "IndexRun must stay compact");
const _: () = assert!(size_of::<JournalError>() <= 64, "JournalError must stay compact");
const _: () = assert!(size_of::<AppendManyError>() <= 80, "AppendManyError must stay compact");

fn rollback_error(target: &mut impl AppendTarget, original_len: u64, write: io::Error) -> Error {
	match target.rollback_to(original_len) {
		Ok(()) => Error::Io(write),
		Err(rollback) => Error::AppendRollback { write, rollback },
	}
}

fn append_all(
	target: &mut impl AppendTarget,
	original_len: u64,
	bytes: &[u8],
) -> Result<(), Error> {
	target
		.write_all(bytes)
		.map_err(|write| rollback_error(target, original_len, write))
}

fn commit(target: &mut impl AppendTarget, original_len: u64) -> Result<(), Error> {
	target
		.sync_data()
		.map_err(|write| rollback_error(target, original_len, write))
}

fn validate_event(event: &Event) -> Result<(), Error> {
	if let Kind::Infer { thinking, model, tier, cred_pin } = &event.kind
		&& thinking.is_unchanged()
		&& model.is_unchanged()
		&& tier.is_unchanged()
		&& cred_pin.is_unchanged()
	{
		return Err(Error::EmptyInfer);
	}
	if let Kind::EntryUndecodable(entry) = &event.kind
		&& serde_json::from_str::<Header>(entry.raw.get()).is_ok()
	{
		return Err(Error::DuplicateHeader);
	}
	Ok(())
}

fn halted_error() -> JournalError {
	JournalIndeterminate {
		source:   Error::Io(io::Error::other(
			"transcript writer is halted after an indeterminate append",
		)),
		written:  IndexRun::default(),
		poisoned: true,
	}
	.into()
}

impl Writer {
	/// Creates a new transcript and writes its line-zero header.
	///
	/// Creation fails when the path already exists so an append-only journal is
	/// never overwritten.
	pub fn create(path: &Path, header: &Header) -> Result<Self, Error> {
		if header.v != 4 {
			return Err(Error::InvalidHeaderVersion(header.v));
		}
		let mut file = OpenOptions::new()
			.read(true)
			.write(true)
			.create_new(true)
			.open(path)?;
		let mut line = BytesMut::new();
		write_header(header, &mut line)?;
		line.extend_from_slice(b"\n");
		file.write_all(&line)?;
		file.sync_data()?;
		line.clear();
		Ok(Self { file: Some(file), pending: None, next_index: 0, line, poisoned: false })
	}

	/// Opens an existing transcript for append and removes only an unterminated
	/// Creates a fileless writer whose first append atomically publishes the
	/// header and first durable event group.
	///
	/// Dropping this writer before an append leaves no filesystem entry.
	pub fn create_lazy(path: &Path, header: &Header) -> Result<Self, Error> {
		if header.v != 4 {
			return Err(Error::InvalidHeaderVersion(header.v));
		}
		if path.exists() {
			return Err(Error::Io(io::Error::new(
				io::ErrorKind::AlreadyExists,
				"transcript path already exists",
			)));
		}
		Ok(Self {
			file:       None,
			pending:    Some((path.to_owned(), header.clone())),
			next_index: 0,
			line:       BytesMut::new(),
			poisoned:   false,
		})
	}

	/// trailing record.
	///
	/// Every newline-terminated malformed record remains a stable tombstone.
	/// Scanning is bounded by [`super::reader::READ_BUFFER_BYTES`].
	#[tracing::instrument(
		name = "journal_open_append",
		level = "debug",
		skip_all,
		fields(path = %path.display())
	)]
	pub fn open_append(path: &Path) -> Result<Self, Error> {
		let reader = Reader::open(path)?;
		let next_index = reader.next_index();
		let append_offset = reader.append_offset();
		for index in 0..next_index {
			if let Some(Entry::Tombstone(raw)) = reader.log().get(index)
				&& let Ok(source) = serde_json::from_str::<String>(raw.get())
				&& serde_json::from_str::<Header>(&source).is_ok()
			{
				return Err(Error::DuplicateHeader);
			}
		}
		let header_terminated = append_offset != 0
			&& fs::File::open(path)
				.and_then(|mut file| {
					file.seek(SeekFrom::Start(append_offset.saturating_sub(1)))?;
					let mut byte = [0_u8; 1];
					io::Read::read_exact(&mut file, &mut byte)?;
					Ok(byte[0] == b'\n')
				})
				.unwrap_or(false);
		drop(reader);
		let mut file = OpenOptions::new().read(true).write(true).open(path)?;
		let original_len = file.metadata()?.len();
		if !header_terminated && next_index == 0 && original_len == append_offset {
			file.seek(SeekFrom::End(0))?;
			file.write_all(b"\n")?;
			file.sync_data()?;
			tracing::info!("journal header terminator repaired");
		} else if original_len > append_offset {
			file.set_len(append_offset)?;
			file.sync_data()?;
			tracing::info!(
				removed_bytes = original_len.saturating_sub(append_offset),
				"journal torn tail repaired"
			);
		}
		file.seek(SeekFrom::End(0))?;
		tracing::debug!(next_event_index = next_index, "journal opened for append");
		Ok(Self {
			file: Some(file),
			pending: None,
			next_index,
			line: BytesMut::new(),
			poisoned: false,
		})
	}

	/// Rejects an attempt to write another header to this transcript.
	pub const fn write_header(&mut self, _header: &Header) -> Result<(), Error> {
		Err(Error::DuplicateHeader)
	}

	/// Appends an event and returns its assigned event index.
	///
	/// The event index is its position in expanded durable event order. Empty
	/// inference patches are rejected because they encode no state transition.
	pub fn append(&mut self, event: &Event) -> Result<u64, Error> {
		match self.append_atomic(slice::from_ref(event)) {
			Ok(mut indexes) => Ok(indexes.pop().expect("one event has one physical index")),
			Err(JournalError::RolledBack { source }) => Err(source),
			Err(JournalError::Indeterminate(indeterminate)) => Err(indeterminate.source),
			Err(JournalError::TooManyEntries { .. }) => {
				unreachable!("a single-event append cannot exceed the atomic group limit")
			},
		}
	}

	/// Appends events in order with one final durability point.
	///
	/// This operation is deliberately not transactional. A failure while
	/// writing a later line leaves the successfully written prefix in place and
	/// reports those physical indexes in [`AppendManyError::appended`].
	pub fn append_many(&mut self, events: &[Event]) -> Result<SmallVec<u64, 8>, AppendManyError> {
		if self.poisoned {
			return Err(AppendManyError { source: halted_error(), appended: IndexRun::default() });
		}
		let ends = self.stage(events).map_err(|source| AppendManyError {
			source:   JournalError::RolledBack { source },
			appended: IndexRun::default(),
		})?;
		if ends.is_empty() {
			return Ok(SmallVec::new());
		}

		if self.file.is_none() {
			return self.materialize_many(events);
		}
		let original_len = self
			.file
			.as_mut()
			.expect("pending writer materialized above")
			.append_len()
			.map_err(|source| AppendManyError {
				source:   JournalError::RolledBack { source: Error::Io(source) },
				appended: IndexRun::default(),
			})?;
		let first_index = self.next_index;
		let mut indexes = SmallVec::with_capacity(ends.len());
		let mut start = 0;
		for end in ends {
			let index = self.next_index;
			let line_start = match self
				.file
				.as_mut()
				.expect("writer file is materialized")
				.append_len()
			{
				Ok(line_start) => line_start,
				Err(error) => {
					let appended = IndexRun::from_contiguous(&indexes);
					let (source, appended) = if appended.is_empty() {
						(JournalError::RolledBack { source: Error::Io(error) }, appended)
					} else {
						match self
							.file
							.as_mut()
							.expect("writer file is materialized")
							.sync_data()
						{
							Ok(()) => (JournalError::RolledBack { source: Error::Io(error) }, appended),
							Err(write) => {
								self.next_index = first_index;
								let rollback = rollback_error(
									self.file.as_mut().expect("writer file is materialized"),
									original_len,
									write,
								);
								(self.classify(rollback, appended), IndexRun::default())
							},
						}
					};
					self.line.clear();
					return Err(AppendManyError { source, appended });
				},
			};
			let result = append_all(
				self.file.as_mut().expect("writer file is materialized"),
				line_start,
				&self.line[start..end],
			);
			if let Err(source) = result {
				let journal_error = self.finish_failed_many(source, &indexes, index);
				self.line.clear();
				return Err(journal_error);
			}
			indexes.push(index);
			self.next_index = self.next_index.saturating_add(1);
			start = end;
		}

		if let Err(source) =
			commit(self.file.as_mut().expect("writer file is materialized"), original_len)
		{
			self.next_index = first_index;
			let source = self.classify(source, IndexRun::from_contiguous(&indexes));
			self.line.clear();
			return Err(AppendManyError { source, appended: IndexRun::default() });
		}
		self.line.clear();
		Ok(indexes)
	}

	/// Atomically appends an event group with contiguous physical indexes.
	///
	/// Every event is encoded into one committed newline-delimited envelope
	/// before the file is touched, then synchronized at one durability point.
	/// Recovery publishes none of the group unless the whole canonical envelope
	/// is present. A clean failure
	/// restores and synchronizes the original length. If rollback itself fails,
	/// the returned [`JournalIndeterminate`] poisons this writer permanently.
	pub fn append_atomic(&mut self, events: &[Event]) -> Result<SmallVec<u64, 8>, JournalError> {
		if self.poisoned {
			return Err(halted_error());
		}
		if events.len() > MAX_ATOMIC_ENTRIES {
			return Err(JournalError::TooManyEntries {
				entries: events.len(),
				maximum: MAX_ATOMIC_ENTRIES,
			});
		}
		self
			.stage_atomic(events)
			.map_err(|source| JournalError::RolledBack { source })?;
		if events.is_empty() {
			return Ok(SmallVec::new());
		}
		if self.file.is_none() {
			return self.materialize_atomic(events);
		}
		let original_len = self
			.file
			.as_mut()
			.expect("writer file is materialized")
			.append_len()
			.map_err(|source| JournalError::RolledBack { source: Error::Io(source) })?;
		let first_index = self.next_index;
		let indexes = (0..events.len())
			.map(|offset| {
				first_index.saturating_add(u64::try_from(offset).expect("event count fits in u64"))
			})
			.collect::<SmallVec<u64, 8>>();
		let result = append_all(
			self.file.as_mut().expect("writer file is materialized"),
			original_len,
			&self.line,
		)
		.and_then(|()| {
			commit(self.file.as_mut().expect("writer file is materialized"), original_len)
		});
		self.line.clear();
		match result {
			Ok(()) => {
				self.next_index = first_index
					.saturating_add(u64::try_from(events.len()).expect("event count fits in u64"));
				Ok(indexes)
			},
			Err(source) => Err(self.classify(source, IndexRun::from_contiguous(&indexes))),
		}
	}

	/// Returns whether an indeterminate rollback has halted this writer.
	pub const fn is_poisoned(&self) -> bool {
		self.poisoned
	}

	/// Returns the byte offset immediately after the last complete durable line.
	///
	/// The watermark is allocation-free and may be persisted beside the last
	/// assigned physical index. An indeterminate writer fails closed because a
	/// torn suffix has no trustworthy complete-line boundary.
	pub fn byte_watermark(&self) -> Result<u64, Error> {
		if self.poisoned {
			return Err(Error::Io(io::Error::other(
				"transcript byte watermark is unknown after an indeterminate append",
			)));
		}
		match &self.file {
			Some(file) => Ok(file.metadata()?.len()),
			None => Ok(0),
		}
	}

	fn materialize_atomic(&mut self, events: &[Event]) -> Result<SmallVec<u64, 8>, JournalError> {
		let Some((path, header)) = self.pending.take() else {
			unreachable!("only pending writers materialize")
		};
		let mut bytes = BytesMut::new();
		if let Err(source) = write_header(&header, &mut bytes) {
			self.pending = Some((path, header));
			self.line.clear();
			return Err(JournalError::RolledBack { source });
		}
		bytes.extend_from_slice(b"\n");
		bytes.extend_from_slice(&self.line);
		if let Err(source) = atomic::commit(&path, &bytes, || !path.exists()) {
			self.pending = Some((path, header));
			self.line.clear();
			return Err(JournalError::RolledBack { source: Error::Io(io::Error::other(source)) });
		}
		let mut file = match OpenOptions::new().read(true).write(true).open(&path) {
			Ok(file) => file,
			Err(source) => {
				self.poisoned = true;
				self.line.clear();
				return Err(
					JournalIndeterminate {
						source:   Error::Io(source),
						written:  IndexRun {
							first: 0,
							count: u64::try_from(events.len()).expect("event count fits in u64"),
						},
						poisoned: true,
					}
					.into(),
				);
			},
		};
		if let Err(source) = file.seek(SeekFrom::End(0)) {
			self.poisoned = true;
			self.line.clear();
			return Err(
				JournalIndeterminate {
					source:   Error::Io(source),
					written:  IndexRun {
						first: 0,
						count: u64::try_from(events.len()).expect("event count fits in u64"),
					},
					poisoned: true,
				}
				.into(),
			);
		}
		self.file = Some(file);
		let indexes = (0..events.len())
			.map(|index| u64::try_from(index).expect("event count fits in u64"))
			.collect();
		self.next_index = u64::try_from(events.len()).expect("event count fits in u64");
		self.line.clear();
		Ok(indexes)
	}

	fn materialize_many(&mut self, events: &[Event]) -> Result<SmallVec<u64, 8>, AppendManyError> {
		self
			.materialize_atomic(events)
			.map_err(|source| AppendManyError { source, appended: IndexRun::default() })
	}

	fn stage(&mut self, events: &[Event]) -> Result<SmallVec<usize, 8>, Error> {
		self.line.clear();
		let mut ends = SmallVec::with_capacity(events.len());
		for event in events {
			validate_event(event)?;
			if matches!(event.kind, Kind::Msg(_)) {
				let mut bounded = event.clone();
				if let Kind::Msg(message) = &mut bounded.kind {
					message.truncate_for_persistence();
				}
				write_line(&bounded, &mut self.line)?;
			} else {
				write_line(event, &mut self.line)?;
			}
			self.line.extend_from_slice(b"\n");
			ends.push(self.line.len());
		}
		Ok(ends)
	}

	fn stage_atomic(&mut self, events: &[Event]) -> Result<(), Error> {
		self.line.clear();
		if events.is_empty() {
			return Ok(());
		}
		for event in events {
			validate_event(event)?;
		}
		write_atomic_group(events, &mut self.line)?;
		self.line.extend_from_slice(b"\n");
		Ok(())
	}

	fn classify(&mut self, source: Error, written: IndexRun) -> JournalError {
		if matches!(source, Error::AppendRollback { .. }) {
			self.poisoned = true;
			JournalIndeterminate { source, written, poisoned: true }.into()
		} else {
			JournalError::RolledBack { source }
		}
	}

	fn finish_failed_many(
		&mut self,
		source: Error,
		indexes: &SmallVec<u64, 8>,
		failed_index: u64,
	) -> AppendManyError {
		if matches!(source, Error::AppendRollback { .. }) {
			return AppendManyError {
				source:   self.classify(source, run_including_next(indexes, failed_index)),
				appended: IndexRun::default(),
			};
		}
		AppendManyError {
			source:   JournalError::RolledBack { source },
			appended: IndexRun::from_contiguous(indexes),
		}
	}
}

fn run_including_next(indexes: &[u64], next: u64) -> IndexRun {
	let prefix = IndexRun::from_contiguous(indexes);
	debug_assert!(prefix.first().is_none_or(|first| {
		first
			.checked_add(u64::try_from(prefix.len()).expect("index count fits in u64"))
			.is_some_and(|expected| next == expected)
	}));
	IndexRun {
		first: prefix.first().unwrap_or(next),
		count: prefix
			.count
			.checked_add(1)
			.expect("index count fits in u64"),
	}
}

#[cfg(test)]
mod tests {
	use std::{
		fs::OpenOptions,
		io::{self, Write},
	};

	use bytes::BytesMut;
	use tempfile::tempdir;

	use super::{AppendTarget, Error, IndexRun, JournalError, Writer, append_all, commit};

	struct FaultTarget {
		bytes:         Vec<u8>,
		write_left:    Option<usize>,
		rollback_fail: bool,
		sync_failures: usize,
		syncs:         usize,
	}

	impl Write for FaultTarget {
		fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
			let Some(left) = self.write_left else {
				self.bytes.extend_from_slice(bytes);
				return Ok(bytes.len());
			};
			if left == 0 {
				return Err(io::Error::new(io::ErrorKind::StorageFull, "injected full device"));
			}
			let written = left.min(bytes.len());
			self.bytes.extend_from_slice(&bytes[..written]);
			self.write_left = Some(left - written);
			Ok(written)
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	impl AppendTarget for FaultTarget {
		fn append_len(&self) -> io::Result<u64> {
			Ok(u64::try_from(self.bytes.len()).expect("test buffer length fits in u64"))
		}

		fn rollback_to(&mut self, len: u64) -> io::Result<()> {
			if self.rollback_fail {
				return Err(io::Error::new(
					io::ErrorKind::PermissionDenied,
					"injected rollback failure",
				));
			}
			self
				.bytes
				.truncate(usize::try_from(len).expect("test buffer length fits in usize"));
			self.sync_data()
		}

		fn sync_data(&mut self) -> io::Result<()> {
			self.syncs += 1;
			if self.sync_failures > 0 {
				self.sync_failures -= 1;
				return Err(io::Error::other("injected sync failure"));
			}
			Ok(())
		}
	}

	fn target() -> FaultTarget {
		FaultTarget {
			bytes:         b"complete\n".to_vec(),
			write_left:    None,
			rollback_fail: false,
			sync_failures: 0,
			syncs:         0,
		}
	}

	#[test]
	fn index_run_preserves_contiguous_indexes() {
		let run = IndexRun::from_contiguous(&[7, 8, 9]);
		assert_eq!(run.first(), Some(7));
		assert_eq!(run.len(), 3);
		assert!(!run.is_empty());
		assert_eq!(run.into_iter().collect::<Vec<_>>(), [7, 8, 9]);
		assert!(IndexRun::from_contiguous(&[]).is_empty());
	}

	#[test]
	fn partial_append_rolls_back_and_target_remains_retryable() {
		let mut target = target();
		target.write_left = Some(5);
		let original_len = target.append_len().expect("length");
		assert!(append_all(&mut target, original_len, b"{\"torn\":true}\n").is_err());
		assert_eq!(target.bytes, b"complete\n");
		assert_eq!(target.syncs, 1, "rollback is made durable");

		target.write_left = None;
		let original_len = target.append_len().expect("length");
		append_all(&mut target, original_len, b"{\"complete\":true}\n").expect("retry succeeds");
		assert_eq!(target.bytes, b"complete\n{\"complete\":true}\n");
	}

	#[test]
	fn rollback_failure_reports_indeterminate_bytes() {
		let mut target = target();
		target.write_left = Some(5);
		target.rollback_fail = true;
		let original_len = target.append_len().expect("length");
		assert!(matches!(
			append_all(&mut target, original_len, b"{\"torn\":true}\n"),
			Err(Error::AppendRollback { .. })
		));
		assert_eq!(target.bytes, b"complete\n{\"tor");
	}

	#[test]
	fn durability_failure_with_clean_rollback_removes_the_group() {
		let mut target = target();
		let original_len = target.append_len().expect("length");
		append_all(&mut target, original_len, b"one\ntwo\n").expect("staged write");
		target.sync_failures = 1;
		assert!(matches!(commit(&mut target, original_len), Err(Error::Io(_))));
		assert_eq!(target.bytes, b"complete\n");
		assert_eq!(target.syncs, 2, "failed commit and durable rollback each synchronize");
	}

	#[test]
	fn durability_failure_never_makes_a_false_rollback_claim() {
		let mut target = target();
		let original_len = target.append_len().expect("length");
		append_all(&mut target, original_len, b"one\ntwo\n").expect("staged write");
		target.sync_failures = 2;
		assert!(matches!(commit(&mut target, original_len), Err(Error::AppendRollback { .. })));
		assert_eq!(
			target.bytes, b"complete\n",
			"truncate happened but its durability is indeterminate when rollback sync fails"
		);
	}

	#[test]
	fn indeterminate_failure_poison_halts_the_writer() {
		let directory = tempdir().expect("temporary directory");
		let file = OpenOptions::new()
			.read(true)
			.write(true)
			.create_new(true)
			.open(directory.path().join("poisoned.jsonl"))
			.expect("create target");
		let mut writer = Writer {
			file:       Some(file),
			pending:    None,
			next_index: 7,
			line:       BytesMut::new(),
			poisoned:   false,
		};
		let failure = writer.classify(
			Error::AppendRollback {
				write:    io::Error::new(io::ErrorKind::StorageFull, "injected write failure"),
				rollback: io::Error::new(io::ErrorKind::PermissionDenied, "injected rollback failure"),
			},
			IndexRun::from_contiguous(&[7]),
		);

		assert!(matches!(
			&failure,
			JournalError::Indeterminate(state)
				if state.written.into_iter().eq([7]) && state.poisoned
		));
		assert!(writer.is_poisoned());
		assert!(matches!(
			writer.append_atomic(&[]),
			Err(JournalError::Indeterminate(state))
				if state.written.is_empty() && state.poisoned
		));
	}
}
