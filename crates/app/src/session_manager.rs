//! Owner-local session UI state that is deliberately excluded from journals and
//! replication.

use std::{
	io,
	path::{Path, PathBuf},
};

use omp_core::encoding::hex;
use omp_storage::{atomic, transcript::SessionId};
use thiserror::Error;

/// Draft persistence failure.
#[derive(Debug, Error)]
pub enum DraftError {
	/// Draft directory or file access failed.
	#[error("session draft I/O failed")]
	Io(#[from] io::Error),
	/// Atomic draft publication failed.
	#[error("failed to publish session draft")]
	Atomic(#[from] atomic::Error),
}
use std::fs;

pub use omp_driver::chat::{PinError, PinStore};
/// Private, owner-local unsent composer buffers keyed by session identity.
pub struct DraftStore {
	directory: PathBuf,
}

impl DraftStore {
	/// Opens the private draft directory below the owner's application data
	/// root.
	pub fn new(data_dir: &Path) -> Result<Self, DraftError> {
		let directory = data_dir.join("drafts");
		fs::create_dir_all(&directory)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt as _;
			fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
		}
		Ok(Self { directory })
	}

	fn path(&self, session: &SessionId) -> PathBuf {
		let digest = omp_core::Hash32::sum(session.0.as_bytes());
		let short: &[u8; 16] = digest.as_bytes()[..16]
			.try_into()
			.expect("a SHA-256 digest contains 16 prefix bytes");
		self.directory.join(hex::encode_n(short).as_str())
	}

	/// Atomically saves the current unsent composer text, or removes an empty
	/// draft.
	pub fn save(&self, session: &SessionId, draft: &str) -> Result<(), DraftError> {
		let path = self.path(session);
		if draft.is_empty() {
			match fs::remove_file(path) {
				Ok(()) => {},
				Err(error) if error.kind() == io::ErrorKind::NotFound => {},
				Err(error) => return Err(error.into()),
			}
			return Ok(());
		}
		atomic::commit(&path, draft.as_bytes(), || true)?;
		Ok(())
	}

	/// Takes a saved draft exactly once after restart or session switch.
	pub fn consume(&self, session: &SessionId) -> Result<Option<String>, DraftError> {
		let path = self.path(session);
		let claimed = path.with_extension(format!("claimed-{}", omp_core::Ulid::generate()));
		match fs::rename(&path, &claimed) {
			Ok(()) => {},
			Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
			Err(error) => return Err(error.into()),
		}
		let result = fs::read_to_string(&claimed);
		let removal = fs::remove_file(&claimed);
		match (result, removal) {
			(Ok(draft), Ok(())) => Ok(Some(draft)),
			(Err(error), _) | (Ok(_), Err(error)) => Err(error.into()),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use omp_core::Str;
	use tempfile::tempdir;

	use super::*;

	#[test]
	fn draft_is_private_and_consumed_once() {
		let temp = tempdir().expect("tempdir");
		let store = DraftStore::new(temp.path()).expect("draft store");
		let session = SessionId(Str::from("session-one"));
		store
			.save(&session, "unfinished prompt")
			.expect("save draft");
		assert_eq!(store.consume(&session).expect("consume"), Some("unfinished prompt".to_owned()));
		assert_eq!(store.consume(&session).expect("consume again"), None);
	}
	#[test]
	fn pins_toggle_and_persist_across_reopen() {
		let temp = tempdir().expect("tempdir");
		let first = SessionId(Str::from("session-one"));
		let second = SessionId(Str::from("session-two"));
		let store = PinStore::new(temp.path());

		assert!(store.toggle(&first).expect("pin first"));
		assert!(store.toggle(&second).expect("pin second"));
		let reopened = PinStore::new(temp.path());
		assert_eq!(
			reopened.load().expect("reload pins"),
			BTreeSet::from([first.0.clone(), second.0.clone()])
		);
		assert!(!reopened.toggle(&first).expect("unpin first"));
		assert_eq!(reopened.load().expect("reload unpin"), BTreeSet::from([second.0]));
	}
	#[test]
	fn corrupt_pin_metadata_does_not_break_session_listing() {
		let temp = tempdir().expect("tempdir");
		fs::write(temp.path().join("session-pins.json"), b"{broken").expect("corrupt fixture");
		assert!(
			PinStore::new(temp.path())
				.load()
				.expect("recover pins")
				.is_empty()
		);
	}
}
