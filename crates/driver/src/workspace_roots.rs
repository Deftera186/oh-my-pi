//! Journal/Environment reconciliation for immutable workspace-root snapshots.

#[cfg(unix)]
use std::ffi::CString;
use std::{
	collections::BTreeMap,
	fs, io,
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_agent::{Journal, JournalError, WorkspaceRootInput, WorkspaceRootsInput};
use omp_core::Str;
use omp_proto::{SCHEMA_REV, env::v1 as pb};
use thiserror::Error;
use url::Url;

/// Failure to prove that a project directory can be entered safely.
#[derive(Debug, Error)]
pub enum DirectoryEnterabilityError {
	/// Directory metadata could not be read.
	#[error("project directory cannot be inspected: {path}")]
	Metadata {
		/// Project directory being checked.
		path:   PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source: io::Error,
	},
	/// The existing path is not a directory.
	#[error("project path is not a directory: {path}")]
	NotDirectory {
		/// Rejected project path.
		path: PathBuf,
	},
	/// The directory lacks search permission or cannot otherwise be entered.
	#[error("project directory is not enterable: {path}")]
	Search {
		/// Rejected project directory.
		path:   PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source: io::Error,
	},
}

impl DirectoryEnterabilityError {
	/// Returns whether the path disappeared before it could be adopted.
	pub fn is_missing(&self) -> bool {
		match self {
			Self::Metadata { source, .. } | Self::Search { source, .. } => {
				matches!(source.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
			},
			Self::NotDirectory { .. } => true,
		}
	}
}

/// Proves that `path` exists, is a directory, and has search permission.
///
/// Metadata alone is insufficient on POSIX: it can succeed for a directory
/// whose own execute/search bit is denied.
pub fn ensure_directory_enterable(path: &Path) -> Result<(), DirectoryEnterabilityError> {
	let metadata = fs::metadata(path)
		.map_err(|source| DirectoryEnterabilityError::Metadata { path: path.to_owned(), source })?;
	if !metadata.is_dir() {
		return Err(DirectoryEnterabilityError::NotDirectory { path: path.to_owned() });
	}
	ensure_search_permission(path)
}

/// Returns whether `path` can safely be adopted as a working directory.
pub fn directory_is_enterable(path: &Path) -> bool {
	ensure_directory_enterable(path).is_ok()
}

#[cfg(unix)]
fn ensure_search_permission(path: &Path) -> Result<(), DirectoryEnterabilityError> {
	use std::os::unix::ffi::OsStrExt as _;

	let encoded = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
		DirectoryEnterabilityError::Search {
			path:   path.to_owned(),
			source: io::Error::new(io::ErrorKind::InvalidInput, "project path contains a NUL byte"),
		}
	})?;
	// SAFETY: `encoded` is a live NUL-terminated path and `access` does not retain
	// it.
	if unsafe { libc::access(encoded.as_ptr(), libc::X_OK) } == 0 {
		Ok(())
	} else {
		Err(DirectoryEnterabilityError::Search {
			path:   path.to_owned(),
			source: io::Error::last_os_error(),
		})
	}
}

#[cfg(not(unix))]
fn ensure_search_permission(path: &Path) -> Result<(), DirectoryEnterabilityError> {
	fs::read_dir(path)
		.map(drop)
		.map_err(|source| DirectoryEnterabilityError::Search { path: path.to_owned(), source })
}

/// Root-authority drift retained with a prompt snapshot instead of hidden.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceRootDiagnostic {
	/// A journal root is no longer granted by Environment.
	JournalRootNotGranted(PathBuf),
	/// Environment's primary differs from the session's immutable primary.
	PrimaryMismatch {
		/// Primary path fixed by the session journal.
		journal:     PathBuf,
		/// Primary path reported by Environment.
		environment: PathBuf,
	},
}

/// Immutable root snapshot plus reconciliation diagnostics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceRootSnapshot {
	/// Journal/grant intersection.
	pub roots:       WorkspaceRootsInput,
	/// Structured authority drift.
	pub diagnostics: Arc<[WorkspaceRootDiagnostic]>,
}

/// Root mutation or Environment snapshot failure.
#[derive(Debug, Error)]
pub enum WorkspaceRootError {
	/// Environment returned a root-set revision from another wire schema.
	#[error("Environment workspace-root wire revision {actual} does not match {SCHEMA_REV}")]
	WireRevision {
		/// Returned schema revision.
		actual: u32,
	},
	/// Environment omitted its singular primary grant.
	#[error("Environment workspace-root snapshot has no primary grant")]
	MissingPrimary,
	/// Environment returned a malformed root URI.
	#[error("Environment workspace-root URI is invalid: {uri}")]
	InvalidUri {
		/// Rejected canonical URI.
		uri:    Str,
		/// URL parser failure.
		#[source]
		source: url::ParseError,
	},
	/// Environment returned a non-file root URI.
	#[error("Environment workspace-root URI is not a local file URI: {uri}")]
	NonFileUri {
		/// Rejected canonical URI.
		uri: Str,
	},
	/// A requested root could not be canonicalized.
	#[error("workspace root cannot be canonicalized: {path}")]
	Canonicalize {
		/// Requested path.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: io::Error,
	},
	/// A mutation names a root outside Environment authority.
	#[error("workspace root is not granted by Environment: {path}")]
	NotGranted {
		/// Canonical rejected path.
		path: PathBuf,
	},
	/// The immutable session primary cannot be removed.
	#[error("the session primary workspace root cannot be removed: {path}")]
	PrimaryRemoval {
		/// Canonical primary path.
		path: PathBuf,
	},
	/// Durable journal operation failed.
	#[error(transparent)]
	Journal(#[from] JournalError),
}

struct Grant {
	path:       PathBuf,
	provenance: WorkspaceRootInput,
}

/// Parsed Environment grant set used to validate mutations and freeze roots.
pub struct WorkspaceRootGuard {
	revision: u64,
	primary:  Grant,
	granted:  Vec<Grant>,
}

impl WorkspaceRootGuard {
	/// Parses an ordered canonical Environment root snapshot.
	pub fn from_environment(snapshot: pb::WorkspaceRootSet) -> Result<Self, WorkspaceRootError> {
		if snapshot.wire_revision != SCHEMA_REV {
			return Err(WorkspaceRootError::WireRevision { actual: snapshot.wire_revision });
		}
		let primary = snapshot.primary.ok_or(WorkspaceRootError::MissingPrimary)?;
		let primary = parse_grant(primary)?;
		let mut by_path = BTreeMap::new();
		let mut granted = Vec::with_capacity(snapshot.granted.len());
		for wire in snapshot.granted {
			let grant = parse_grant(wire)?;
			if by_path.insert(grant.path.clone(), ()).is_none() {
				granted.push(grant);
			}
		}
		if !by_path.contains_key(&primary.path) {
			granted.insert(0, Grant {
				path:       primary.path.clone(),
				provenance: primary.provenance.clone(),
			});
		}
		Ok(Self { revision: snapshot.revision, primary, granted })
	}

	/// Canonicalizes and validates ordered root additions before journaling.
	pub async fn add(
		&self,
		journal: &mut Journal,
		ts: u64,
		requested: &[PathBuf],
	) -> Result<u64, WorkspaceRootError> {
		let roots = self.validate(requested).await?;
		Ok(journal.append_workspace_dirs(ts, roots)?)
	}

	/// Canonicalizes and validates ordered root removals before journaling.
	pub async fn remove(
		&self,
		journal: &mut Journal,
		ts: u64,
		requested: &[PathBuf],
	) -> Result<u64, WorkspaceRootError> {
		let roots = self.validate(requested).await?;
		if roots.iter().any(|root| root == &self.primary.path) {
			return Err(WorkspaceRootError::PrimaryRemoval { path: self.primary.path.clone() });
		}
		Ok(journal.remove_workspace_dirs(ts, roots)?)
	}

	/// Freezes only the append-only journal/Environment-grant intersection.
	pub fn snapshot(
		&self,
		journal: &Journal,
		journal_primary: &Path,
	) -> Result<WorkspaceRootSnapshot, WorkspaceRootError> {
		let journal_roots = journal.workspace_roots(journal_primary)?;
		let mut diagnostics = Vec::new();
		if self.primary.path != journal_primary {
			diagnostics.push(WorkspaceRootDiagnostic::PrimaryMismatch {
				journal:     journal_primary.to_path_buf(),
				environment: self.primary.path.clone(),
			});
		}
		for root in journal_roots.iter() {
			if !self.granted.iter().any(|grant| grant.path == root) {
				diagnostics.push(WorkspaceRootDiagnostic::JournalRootNotGranted(root.to_path_buf()));
			}
		}

		let roots = self
			.granted
			.iter()
			.filter(|grant| journal_roots.iter().any(|root| root == grant.path))
			.map(|grant| grant.provenance.clone())
			.collect::<Vec<_>>();
		let primary = (self.primary.path == journal_primary).then(|| self.primary.provenance.clone());
		Ok(WorkspaceRootSnapshot {
			roots:       WorkspaceRootsInput { revision: self.revision, primary, roots: roots.into() },
			diagnostics: diagnostics.into(),
		})
	}

	async fn validate(&self, requested: &[PathBuf]) -> Result<Vec<PathBuf>, WorkspaceRootError> {
		let mut accepted = Vec::with_capacity(requested.len());
		for path in requested {
			let canonical = tokio::fs::canonicalize(path)
				.await
				.map_err(|source| WorkspaceRootError::Canonicalize { path: path.clone(), source })?;
			if !self.granted.iter().any(|grant| grant.path == canonical) {
				return Err(WorkspaceRootError::NotGranted { path: canonical });
			}
			if !accepted.contains(&canonical) {
				accepted.push(canonical);
			}
		}
		Ok(accepted)
	}
}

fn parse_grant(root: pb::WorkspaceRoot) -> Result<Grant, WorkspaceRootError> {
	let uri = Str::from(root.canonical_uri);
	let parsed = Url::parse(uri.as_str())
		.map_err(|source| WorkspaceRootError::InvalidUri { uri: uri.clone(), source })?;
	let path = parsed
		.to_file_path()
		.map_err(|()| WorkspaceRootError::NonFileUri { uri: uri.clone() })?;
	Ok(Grant { path, provenance: WorkspaceRootInput::new(uri, root.grant_id) })
}

#[cfg(test)]
mod tests {
	#[cfg(unix)]
	use std::os::unix::fs::PermissionsExt as _;
	use std::{fs, slice};

	use omp_storage::transcript::{Header, SessionId};

	use super::*;

	fn wire_root(path: &Path, id: &'static [u8]) -> pb::WorkspaceRoot {
		pb::WorkspaceRoot {
			canonical_uri: Url::from_file_path(path).unwrap().to_string(),
			grant_id:      id.into(),
		}
	}

	#[cfg(unix)]
	#[test]
	fn enterability_rejects_directory_without_search_permission() {
		let directory = tempfile::tempdir().unwrap();
		let denied = directory.path().join("denied");
		fs::create_dir(&denied).unwrap();
		fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
		assert!(!directory_is_enterable(&denied));
		assert!(matches!(
			ensure_directory_enterable(&denied),
			Err(DirectoryEnterabilityError::Search { .. })
		));
		fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
	}

	#[tokio::test]
	async fn ungranted_and_removed_roots_never_enter_snapshot() {
		let directory = tempfile::tempdir().unwrap();
		let primary = directory.path().join("primary");
		let secondary = directory.path().join("secondary");
		let ungranted = directory.path().join("ungranted");
		for path in [&primary, &secondary, &ungranted] {
			fs::create_dir(path).unwrap();
		}
		let primary = fs::canonicalize(primary).unwrap();
		let secondary = fs::canonicalize(secondary).unwrap();
		let ungranted = fs::canonicalize(ungranted).unwrap();
		let journal_path = directory.path().join("session.jsonl");
		let mut journal = Journal::create(&journal_path, &Header {
			v:       4,
			id:      SessionId(Str::from("roots")),
			created: 1,
			cwd:     primary.clone(),
		})
		.unwrap();
		let guard = WorkspaceRootGuard::from_environment(pb::WorkspaceRootSet {
			revision:      7,
			primary:       Some(wire_root(&primary, b"p")),
			granted:       vec![wire_root(&primary, b"p"), wire_root(&secondary, b"s")],
			wire_revision: SCHEMA_REV,
		})
		.unwrap();
		assert!(matches!(
			guard
				.add(&mut journal, 2, slice::from_ref(&ungranted))
				.await,
			Err(WorkspaceRootError::NotGranted { .. })
		));
		guard
			.add(&mut journal, 3, slice::from_ref(&secondary))
			.await
			.unwrap();
		assert_eq!(
			guard
				.snapshot(&journal, &primary)
				.unwrap()
				.roots
				.roots
				.len(),
			2
		);
		guard
			.remove(&mut journal, 4, slice::from_ref(&secondary))
			.await
			.unwrap();
		let snapshot = guard.snapshot(&journal, &primary).unwrap();
		assert_eq!(snapshot.roots.revision, 7);
		assert_eq!(snapshot.roots.roots.len(), 1);
		assert!(snapshot.diagnostics.is_empty());
	}
}
