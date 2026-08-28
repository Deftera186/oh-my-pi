//! Owner-local session breadcrumbs, selector resolution, and journal moves.
//!
//! These operate on the session index and journal files inside a project state
//! directory; the state directory itself is addressed by
//! [`omp_env::project_state`].

use std::{
	fs, io,
	path::{Path, PathBuf},
};

use omp_core::{Hash32, Str, encoding::hex};
use omp_storage::{atomic, index::SessionInfo, transcript::SessionId};
use thiserror::Error;

/// Journal relocation failure.
#[derive(Debug, Error)]
pub enum RelocateError {
	/// Destination already exists and must never be overwritten.
	#[error("session journal destination already exists: {0}")]
	DestinationExists(PathBuf),
	/// A filesystem operation failed.
	#[error("failed to relocate session journal from {source_path} to {destination_path}")]
	Io {
		/// Existing journal path.
		source_path:      PathBuf,
		/// New journal path.
		destination_path: PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source:           io::Error,
	},
	/// The pre-move journal snapshot could not be read.
	#[error("failed to snapshot session journal before relocation: {path}")]
	Snapshot {
		/// Existing journal path.
		path:   PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source: io::Error,
	},
	/// The captured journal boundary and header could not be restored after a
	/// failed move.
	#[error("failed to restore session journal after relocation rollback: {path}")]
	Restore {
		/// Original journal path.
		path:   PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source: io::Error,
	},
}

/// Failure to persist or resolve owner-local session breadcrumbs.
#[derive(Debug, Error)]
pub enum SessionResolveError {
	/// Breadcrumb storage failed.
	#[error("failed to update terminal session breadcrumb")]
	Breadcrumb(#[from] atomic::Error),
	/// Breadcrumb directory setup or reading failed.
	#[error("terminal session breadcrumb I/O failed")]
	Io(#[from] io::Error),
	/// No indexed session matched the selector.
	#[error("no session matches selector {selector}")]
	NotFound {
		/// Rejected selector.
		selector: Str,
	},
	/// More than one indexed session matched a UUID fragment or prefix.
	#[error("session selector {selector} is ambiguous")]
	Ambiguous {
		/// Ambiguous selector.
		selector: Str,
		/// Matching stable session identifiers.
		matches:  Vec<SessionId>,
	},
}

/// Owner-local per-terminal pointer used by interactive `--continue`.
pub struct TerminalBreadcrumbs {
	directory: PathBuf,
}

impl TerminalBreadcrumbs {
	/// Creates a breadcrumb store below the owner's private data directory.
	///
	/// # Errors
	///
	/// Fails when the private directory cannot be created or restricted.
	pub fn new(data_dir: &Path) -> Result<Self, SessionResolveError> {
		let directory = data_dir.join("terminals");
		fs::create_dir_all(&directory)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt as _;
			fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
		}
		Ok(Self { directory })
	}

	fn path(&self, terminal: &str) -> PathBuf {
		let digest = Hash32::sum(terminal.as_bytes());
		let short: &[u8; 16] = digest.as_bytes()[..16]
			.try_into()
			.expect("a Blake3 digest contains 16 prefix bytes");
		self.directory.join(hex::encode_n(short).as_str())
	}

	/// Atomically points `terminal` at the newly active session.
	///
	/// # Errors
	///
	/// Fails when the breadcrumb cannot be committed.
	pub fn restamp(&self, terminal: &str, session: &SessionId) -> Result<(), SessionResolveError> {
		atomic::commit(&self.path(terminal), session.0.as_bytes(), || true)?;
		Ok(())
	}

	/// Reads the active session previously stamped for `terminal`.
	///
	/// # Errors
	///
	/// Fails when the breadcrumb exists but cannot be read.
	pub fn read(&self, terminal: &str) -> Result<Option<SessionId>, SessionResolveError> {
		match fs::read_to_string(self.path(terminal)) {
			Ok(value) => Ok(Some(SessionId(Str::from(value.trim())))),
			Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(error) => Err(error.into()),
		}
	}
}

/// Resolves `@latest`, exact IDs, unique title prefixes, and unique UUID
/// fragments against an already project-filtered newest-first index page.
///
/// # Errors
///
/// Fails when no session matches or the selector is ambiguous.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		session_count = sessions.len(),
		latest = selector == "@latest",
		selector_len = selector.len(),
	)
)]
pub fn resolve_session_selector(
	sessions: &[SessionInfo],
	selector: &str,
) -> Result<SessionId, SessionResolveError> {
	if selector == "@latest" {
		return sessions
			.first()
			.map(|session| session.id.clone())
			.ok_or_else(|| SessionResolveError::NotFound { selector: Str::from(selector) });
	}
	if let Some(exact) = sessions
		.iter()
		.find(|session| session.id.0.as_str() == selector)
	{
		return Ok(exact.id.clone());
	}
	let mut matches = sessions
		.iter()
		.filter(|session| {
			session.id.0.as_str().contains(selector)
				|| session
					.title
					.as_ref()
					.is_some_and(|title| title.as_str().starts_with(selector))
		})
		.map(|session| session.id.clone());
	let Some(first) = matches.next() else {
		return Err(SessionResolveError::NotFound { selector: Str::from(selector) });
	};
	let Some(second) = matches.next() else {
		return Ok(first);
	};
	let mut ambiguous = vec![first, second];
	ambiguous.extend(matches);
	Err(SessionResolveError::Ambiguous { selector: Str::from(selector), matches: ambiguous })
}

/// A journal rename paired with its pre-move header and append boundary.
///
/// Call [`Self::rollback`] if any operation after the rename fails. Rollback
/// renames the journal back, truncates appended workspace mutations, and
/// restores the captured header.
pub struct JournalRelocation {
	source:      PathBuf,
	destination: PathBuf,
	snapshot:    Option<JournalSnapshot>,
	moved:       bool,
}

struct JournalSnapshot {
	len:    u64,
	header: Box<[u8]>,
}

impl JournalRelocation {
	/// Captures the journal and relocates it without rewriting its contents.
	///
	/// A fileless untouched session remains fileless and returns a transaction
	/// whose [`Self::moved`] value is false.
	///
	/// # Errors
	///
	/// Fails when the source cannot be snapshotted, the destination exists, or
	/// the rename cannot be performed.
	pub fn begin(source: &Path, destination: &Path) -> Result<Self, RelocateError> {
		use std::io::BufRead as _;

		let snapshot = match fs::File::open(source) {
			Ok(file) => {
				let len = file
					.metadata()
					.map_err(|error| RelocateError::Snapshot { path: source.to_owned(), source: error })?
					.len();
				let mut reader = io::BufReader::new(file);
				let mut header = Vec::new();
				reader
					.read_until(b'\n', &mut header)
					.map_err(|error| RelocateError::Snapshot { path: source.to_owned(), source: error })?;
				Some(JournalSnapshot { len, header: header.into_boxed_slice() })
			},
			Err(error) if error.kind() == io::ErrorKind::NotFound => None,
			Err(source_error) => {
				return Err(RelocateError::Snapshot {
					path:   source.to_owned(),
					source: source_error,
				});
			},
		};
		if snapshot.is_none() {
			return Ok(Self {
				source: source.to_owned(),
				destination: destination.to_owned(),
				snapshot,
				moved: false,
			});
		}
		match fs::metadata(destination) {
			Ok(_) => return Err(RelocateError::DestinationExists(destination.to_owned())),
			Err(error) if error.kind() == io::ErrorKind::NotFound => {},
			Err(source_error) => {
				return Err(RelocateError::Io {
					source_path:      source.to_owned(),
					destination_path: destination.to_owned(),
					source:           source_error,
				});
			},
		}
		if let Some(parent) = destination.parent() {
			fs::create_dir_all(parent).map_err(|source_error| RelocateError::Io {
				source_path:      source.to_owned(),
				destination_path: destination.to_owned(),
				source:           source_error,
			})?;
		}
		fs::rename(source, destination).map_err(|source_error| RelocateError::Io {
			source_path:      source.to_owned(),
			destination_path: destination.to_owned(),
			source:           source_error,
		})?;
		Ok(Self {
			source: source.to_owned(),
			destination: destination.to_owned(),
			snapshot,
			moved: true,
		})
	}

	/// Returns whether an existing journal was renamed.
	pub const fn moved(&self) -> bool {
		self.moved
	}

	/// Renames the journal back and restores its pre-move header and length.
	///
	/// # Errors
	///
	/// Fails without overwriting a recreated source path, or when the rename or
	/// snapshot restoration fails.
	pub fn rollback(self) -> Result<(), RelocateError> {
		if !self.moved {
			return Ok(());
		}
		if self.source.exists() {
			return Err(RelocateError::DestinationExists(self.source));
		}
		fs::rename(&self.destination, &self.source).map_err(|source_error| RelocateError::Io {
			source_path:      self.destination.clone(),
			destination_path: self.source.clone(),
			source:           source_error,
		})?;
		let snapshot = self
			.snapshot
			.expect("a moved journal has a captured snapshot");
		restore_journal_snapshot(&self.source, &snapshot)
			.map_err(|source| RelocateError::Restore { path: self.source, source })
	}
}

fn restore_journal_snapshot(path: &Path, snapshot: &JournalSnapshot) -> io::Result<()> {
	use std::io::{Seek as _, Write as _};

	let mut file = fs::OpenOptions::new().write(true).open(path)?;
	file.set_len(snapshot.len)?;
	file.seek(io::SeekFrom::Start(0))?;
	file.write_all(&snapshot.header)?;
	file.sync_all()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn failed_relocation_restores_pre_move_journal_snapshot() {
		let directory = tempfile::tempdir().unwrap();
		let source = directory.path().join("source.jsonl");
		let destination = directory.path().join("destination").join("session.jsonl");
		let original = concat!(
			"{\"v\":4,\"id\":\"01J00000000000000000000000\",\"cwd\":\"/source\"}\n",
			"{\"ts\":1,\"k\":\"workspace_move\",\"root\":\"/source\"}\n",
		)
		.as_bytes();
		fs::write(&source, original).unwrap();

		let relocation = JournalRelocation::begin(&source, &destination).unwrap();
		assert!(relocation.moved());
		use std::io::Write as _;
		fs::OpenOptions::new()
			.append(true)
			.open(&destination)
			.unwrap()
			.write_all(b"{\"ts\":2,\"k\":\"workspace_move\",\"root\":\"/target\"}\n")
			.unwrap();

		relocation.rollback().unwrap();
		assert_eq!(fs::read(&source).unwrap(), original);
		assert!(!destination.exists());
	}
	#[test]
	fn same_path_move_rollback_truncates_appended_workspace_metadata() {
		use std::io::Write as _;

		let directory = tempfile::tempdir().unwrap();
		let source = directory.path().join("session.jsonl");
		let original = b"{\"v\":4,\"id\":\"01J00000000000000000000000\",\"cwd\":\"/source\"}\n";
		fs::write(&source, original).unwrap();

		let relocation = JournalRelocation::begin(&source, &source).unwrap();
		assert!(!relocation.moved());
		fs::OpenOptions::new()
			.append(true)
			.open(&source)
			.unwrap()
			.write_all(b"{\"ts\":2,\"k\":\"workspace_move\",\"root\":\"/target\"}\n")
			.unwrap();

		relocation.rollback().unwrap();
		assert_eq!(fs::read(&source).unwrap(), original);
	}
}
