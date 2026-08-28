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

/// Relocates exact journal bytes without rewriting historical workspace state.
///
/// A fileless untouched session remains fileless and reports `Ok(false)`.
/// Existing journals are renamed on the same filesystem; their v4 header and
/// every historical workspace event remain byte-identical.
///
/// # Errors
///
/// Fails when the destination exists or the rename cannot be performed.
pub fn relocate_journal(source: &Path, destination: &Path) -> Result<bool, RelocateError> {
	if !source.exists() {
		return Ok(false);
	}
	if destination.exists() {
		return Err(RelocateError::DestinationExists(destination.to_owned()));
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
	Ok(true)
}
