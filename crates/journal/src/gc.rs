//! Atomic pruning of abandoned journal branches.

use std::{
	fs::{self, File, OpenOptions},
	io::{self, Write as _},
	path::{Path, PathBuf},
	time::{SystemTime, UNIX_EPOCH},
};

use thiserror::Error;

use crate::{Journal, JournalError, live_chain, sse};

/// Result of pruning one journal to its selected live chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GcReport {
	/// Committed entries before pruning.
	pub entries_before: usize,
	/// Live entries retained by the rewrite.
	pub entries_after:  usize,
	/// File bytes before pruning.
	pub bytes_before:   u64,
	/// File bytes after pruning.
	pub bytes_after:    u64,
}

impl GcReport {
	/// Number of abandoned entries removed.
	#[must_use]
	pub const fn entries_pruned(self) -> usize {
		self.entries_before.saturating_sub(self.entries_after)
	}

	/// Number of journal bytes reclaimed.
	#[must_use]
	pub const fn bytes_reclaimed(self) -> u64 {
		self.bytes_before.saturating_sub(self.bytes_after)
	}
}

/// Failure to open, encode, or atomically replace a journal.
#[derive(Debug, Error)]
pub enum GcError {
	/// Existing journal validation or recovery failed.
	#[error("journal could not be opened for pruning")]
	Journal(#[from] JournalError),
	/// A retained entry could not be encoded.
	#[error("retained journal entry could not be encoded")]
	Encode(#[from] sse::SseError),
	/// A filesystem operation failed.
	#[error("journal pruning I/O failed")]
	Io(#[from] io::Error),
	/// System time cannot be represented for a unique staging name.
	#[error("system clock is before the Unix epoch")]
	Clock(#[from] std::time::SystemTimeError),
}

/// Rewrites `path` atomically so it contains only the tail-selected live chain.
///
/// The replacement is fully encoded and synced beside the journal before the
/// atomic rename. A crash therefore leaves either the old complete journal or
/// the new complete journal at `path`; an unreferenced staging file is
/// harmless. Blob references remain embedded in retained entries and are not
/// rewritten.
///
/// # Errors
///
/// Returns a typed error if the source journal is invalid, a retained frame
/// cannot be encoded, or staging/sync/replacement fails.
pub fn prune_abandoned(path: impl AsRef<Path>) -> Result<GcReport, GcError> {
	let path = path.as_ref();
	let bytes_before = fs::metadata(path)?.len();
	let (journal, entries) = Journal::open(path)?;
	let retained: Vec<_> = live_chain(&entries).cloned().collect();
	let entries_before = entries.len();
	let entries_after = retained.len();

	if entries_before == entries_after {
		return Ok(GcReport {
			entries_before,
			entries_after,
			bytes_before,
			bytes_after: bytes_before,
		});
	}

	let mut encoded = Vec::new();
	for entry in &retained {
		sse::encode(entry, &mut encoded)?;
	}
	let bytes_after =
		u64::try_from(encoded.len()).map_err(|_| io::Error::other("journal is too large"))?;
	let staging = staging_path(path)?;
	let permissions = fs::metadata(path)?.permissions();
	let mut staged = OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(&staging)?;
	let result = (|| -> Result<(), io::Error> {
		staged.set_permissions(permissions)?;
		staged.write_all(&encoded)?;
		staged.sync_all()?;
		drop(journal);
		fs::rename(&staging, path)?;
		if let Some(parent) = path
			.parent()
			.filter(|parent| !parent.as_os_str().is_empty())
		{
			File::open(parent)?.sync_all()?;
		}
		Ok(())
	})();
	if result.is_err() {
		let _ = fs::remove_file(&staging);
	}
	result?;

	Ok(GcReport { entries_before, entries_after, bytes_before, bytes_after })
}

fn staging_path(path: &Path) -> Result<PathBuf, GcError> {
	let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
	let name = path.file_name().unwrap_or_default().to_string_lossy();
	Ok(path.with_file_name(format!(".{name}.gc-{}-{nonce}.tmp", std::process::id())))
}
