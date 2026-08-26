//! Locked, merge-preserving native TOML persistence.

use std::{
	fs::{self, File, OpenOptions},
	io::{self, Write as _},
	path::{Path, PathBuf},
	sync::atomic::{AtomicU64, Ordering},
	time::{SystemTime, UNIX_EPOCH},
};

use toml::{de, ser};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A corrupt source moved out of the writer's way without losing its bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantineDiagnostic {
	/// Original native settings path.
	pub path:        PathBuf,
	/// Unique durable backup containing the corrupt source bytes.
	pub backup_path: PathBuf,
	/// Parser location when available.
	pub line:        Option<usize>,
	/// Parser column when available.
	pub column:      Option<usize>,
}

/// Result of reading a writable native TOML document.
#[derive(Debug, Default)]
pub struct ReadDocument {
	/// Parsed mapping, or an empty mapping when absent/corrupt.
	pub document:   toml::Table,
	/// Structured notice when corrupt input was quarantined.
	pub quarantine: Option<QuarantineDiagnostic>,
}

/// One reflected path mutation applied after re-reading under the file lock.
#[derive(Clone, Debug, PartialEq)]
pub enum DocumentMutation {
	/// Replace one whole reflected value.
	Set {
		/// Dotted reflected path.
		path:  &'static str,
		/// Typed TOML value.
		value: toml::Value,
	},
	/// Remove one whole reflected value.
	Unset {
		/// Dotted reflected path.
		path: &'static str,
	},
}

/// Reads native TOML. Corruption is reported without changing the source.
pub fn read_document(path: &Path) -> Result<toml::Table, SettingsIoError> {
	match fs::read_to_string(path) {
		Ok(source) => parse_table(path, &source),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(toml::Table::new()),
		Err(source) => Err(SettingsIoError::Read { path: path.to_owned(), source }),
	}
}

/// Locks and reads native TOML, quarantining malformed input before returning.
pub fn read_or_quarantine(path: &Path) -> Result<ReadDocument, SettingsIoError> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)
			.map_err(|source| SettingsIoError::CreateDirectory { path: parent.to_owned(), source })?;
	}
	let _lock = FileLock::acquire(path)?;
	read_or_quarantine_locked(path)
}

/// Locks, re-reads, applies whole-path mutations, and atomically replaces the
/// native file. Unrelated concurrent edits are preserved.
pub fn mutate_document(
	path: &Path,
	mutations: &[DocumentMutation],
) -> Result<ReadDocument, SettingsIoError> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)
			.map_err(|source| SettingsIoError::CreateDirectory { path: parent.to_owned(), source })?;
	}
	let _lock = FileLock::acquire(path)?;
	let mut read = read_or_quarantine_locked(path)?;
	for mutation in mutations {
		match mutation {
			DocumentMutation::Set { path, value } => set_path(&mut read.document, path, value.clone()),
			DocumentMutation::Unset { path } => unset_path(&mut read.document, path),
		}
	}
	atomic_replace(path, &toml::to_string_pretty(&read.document)?)?;
	Ok(read)
}

/// Atomically replaces a native settings file with PID-isolated temporary
/// storage and a rollback path for Windows rename semantics.
pub fn atomic_replace(path: &Path, content: &str) -> Result<(), SettingsIoError> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)
			.map_err(|source| SettingsIoError::CreateDirectory { path: parent.to_owned(), source })?;
	}
	let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
	let name = path
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or("config.toml");
	let temporary = path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
	let mut options = OpenOptions::new();
	options.write(true).create_new(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt as _;
		options.mode(0o600);
	}
	let mut file = options
		.open(&temporary)
		.map_err(|source| SettingsIoError::CreateTemporary { path: temporary.clone(), source })?;
	let result = (|| {
		file
			.write_all(content.as_bytes())
			.map_err(|source| SettingsIoError::WriteTemporary { path: temporary.clone(), source })?;
		file
			.sync_all()
			.map_err(|source| SettingsIoError::SyncTemporary { path: temporary.clone(), source })?;
		drop(file);
		replace_path(&temporary, path)?;
		#[cfg(unix)]
		if let Some(parent) = path.parent() {
			File::open(parent)
				.and_then(|directory| directory.sync_all())
				.map_err(|source| SettingsIoError::SyncDirectory { path: parent.to_owned(), source })?;
		}
		Ok(())
	})();
	if result.is_err() {
		let _ = fs::remove_file(&temporary);
	}
	result
}

fn parse_table(path: &Path, source: &str) -> Result<toml::Table, SettingsIoError> {
	if path
		.extension()
		.and_then(|extension| extension.to_str())
		.is_some_and(|extension| {
			extension.eq_ignore_ascii_case("yml") || extension.eq_ignore_ascii_case("yaml")
		}) {
		serde_yaml::from_str(source)
			.map_err(|source| SettingsIoError::ParseYaml { path: path.to_owned(), source })
	} else {
		toml::from_str(source)
			.map_err(|source| SettingsIoError::Parse { path: path.to_owned(), source })
	}
}

fn read_or_quarantine_locked(path: &Path) -> Result<ReadDocument, SettingsIoError> {
	let source = match fs::read_to_string(path) {
		Ok(source) => source,
		Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ReadDocument::default()),
		Err(source) => return Err(SettingsIoError::Read { path: path.to_owned(), source }),
	};
	match toml::from_str(&source) {
		Ok(document) => Ok(ReadDocument { document, quarantine: None }),
		Err(error) => {
			let span = error.span();
			let (line, column) = span
				.map(|span| byte_line_column(&source, span.start))
				.map_or((None, None), |(line, column)| (Some(line), Some(column)));
			let backup_path = unique_quarantine_path(path);
			fs::rename(path, &backup_path).map_err(|source| SettingsIoError::Quarantine {
				path: path.to_owned(),
				backup_path: backup_path.clone(),
				source,
			})?;
			Ok(ReadDocument {
				document:   toml::Table::new(),
				quarantine: Some(QuarantineDiagnostic {
					path: path.to_owned(),
					backup_path,
					line,
					column,
				}),
			})
		},
	}
}

fn unique_quarantine_path(path: &Path) -> PathBuf {
	let millis = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis();
	let name = path
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or("config.toml");
	path.with_file_name(format!("{name}.broken-{millis}-{}", std::process::id()))
}

fn byte_line_column(source: &str, offset: usize) -> (usize, usize) {
	let prefix = &source[..offset.min(source.len())];
	let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
	let column = prefix
		.rsplit_once('\n')
		.map_or(prefix.len(), |(_, tail)| tail.len())
		+ 1;
	(line, column)
}

fn set_path(document: &mut toml::Table, path: &str, value: toml::Value) {
	let mut segments = path.split('.').peekable();
	let mut current = document;
	while let Some(segment) = segments.next() {
		if segments.peek().is_none() {
			current.insert(segment.to_owned(), value);
			return;
		}
		let entry = current
			.entry(segment.to_owned())
			.or_insert_with(|| toml::Value::Table(toml::Table::new()));
		if !entry.is_table() {
			*entry = toml::Value::Table(toml::Table::new());
		}
		current = entry.as_table_mut().expect("table established above");
	}
}

fn unset_path(document: &mut toml::Table, path: &str) {
	fn remove(table: &mut toml::Table, segments: &[&str]) -> bool {
		if segments.len() == 1 {
			table.remove(segments[0]);
			return table.is_empty();
		}
		if let Some(child) = table
			.get_mut(segments[0])
			.and_then(toml::Value::as_table_mut)
			&& remove(child, &segments[1..])
		{
			table.remove(segments[0]);
		}
		table.is_empty()
	}
	let segments = path.split('.').collect::<Vec<_>>();
	if !segments.is_empty() {
		remove(document, &segments);
	}
}

fn replace_path(temporary: &Path, path: &Path) -> Result<(), SettingsIoError> {
	match fs::rename(temporary, path) {
		Ok(()) => Ok(()),
		#[cfg(windows)]
		Err(error) if error.raw_os_error() == Some(5) => replace_after_eperm(temporary, path, error),
		Err(source) => Err(SettingsIoError::Replace { path: path.to_owned(), source }),
	}
}

#[cfg(windows)]
fn replace_after_eperm(
	temporary: &Path,
	path: &Path,
	original: io::Error,
) -> Result<(), SettingsIoError> {
	let backup = path.with_extension(format!("{}.bak", std::process::id()));
	match fs::rename(path, &backup) {
		Ok(()) => {},
		Err(error) if error.kind() == io::ErrorKind::NotFound => {
			return fs::rename(temporary, path)
				.map_err(|source| SettingsIoError::Replace { path: path.to_owned(), source });
		},
		Err(_) => return Err(SettingsIoError::Replace { path: path.to_owned(), source: original }),
	}
	if let Err(source) = fs::rename(temporary, path) {
		if let Err(rollback) = fs::rename(&backup, path) {
			return Err(SettingsIoError::Rollback { path: path.to_owned(), source: rollback });
		}
		return Err(SettingsIoError::Replace { path: path.to_owned(), source });
	}
	let _ = fs::remove_file(backup);
	Ok(())
}

#[must_use]
struct FileLock {
	file: File,
}

impl FileLock {
	fn acquire(path: &Path) -> Result<Self, SettingsIoError> {
		let lock_path = path.with_extension("toml.lock");
		let file = OpenOptions::new()
			.read(true)
			.write(true)
			.create(true)
			.truncate(false)
			.open(&lock_path)
			.map_err(|source| SettingsIoError::Lock { path: lock_path.clone(), source })?;
		lock_exclusive(&file).map_err(|source| SettingsIoError::Lock { path: lock_path, source })?;
		Ok(Self { file })
	}
}

impl Drop for FileLock {
	fn drop(&mut self) {
		let _ = unlock(&self.file);
	}
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> io::Result<()> {
	use std::os::fd::AsRawFd as _;
	// SAFETY: `file` owns a valid descriptor for the duration of the call.
	let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
	(result == 0)
		.then_some(())
		.ok_or_else(io::Error::last_os_error)
}

#[cfg(unix)]
fn unlock(file: &File) -> io::Result<()> {
	use std::os::fd::AsRawFd as _;
	// SAFETY: `file` owns a valid descriptor for the duration of the call.
	let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
	(result == 0)
		.then_some(())
		.ok_or_else(io::Error::last_os_error)
}

#[cfg(windows)]
fn lock_exclusive(file: &File) -> io::Result<()> {
	use std::os::windows::io::AsRawHandle as _;

	use windows_sys::Win32::Storage::FileSystem;
	// SAFETY: OVERLAPPED is a plain C record for synchronous range locking.
	let mut overlapped = unsafe { mem::zeroed() };
	// SAFETY: the handle and OVERLAPPED pointer remain valid for this call.
	let result = unsafe {
		FileSystem::LockFileEx(
			file.as_raw_handle() as _,
			FileSystem::LOCKFILE_EXCLUSIVE_LOCK,
			0,
			u32::MAX,
			u32::MAX,
			&mut overlapped,
		)
	};
	(result != 0)
		.then_some(())
		.ok_or_else(io::Error::last_os_error)
}

#[cfg(windows)]
fn unlock(file: &File) -> io::Result<()> {
	use std::os::windows::io::AsRawHandle as _;

	use windows_sys::Win32::Storage::FileSystem;
	// SAFETY: OVERLAPPED is a plain C record for synchronous range unlocking.
	let mut overlapped = unsafe { mem::zeroed() };
	// SAFETY: the handle and OVERLAPPED pointer remain valid for this call.
	let result = unsafe {
		FileSystem::UnlockFileEx(file.as_raw_handle() as _, 0, u32::MAX, u32::MAX, &mut overlapped)
	};
	(result != 0)
		.then_some(())
		.ok_or_else(io::Error::last_os_error)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn atomic_merge_preserves_unrelated_external_changes() {
		let directory = tempfile::tempdir().expect("directory");
		let path = directory.path().join("config.toml");
		atomic_replace(&path, "external = \"kept\"\n[worktree]\nbase = \"old\"\n")
			.expect("initial replace");
		let result = mutate_document(&path, &[DocumentMutation::Set {
			path:  "worktree.base",
			value: toml::Value::String("new".to_owned()),
		}])
		.expect("mutation");
		assert!(result.quarantine.is_none());
		assert_eq!(result.document["external"].as_str(), Some("kept"));
		assert_eq!(result.document["worktree"]["base"].as_str(), Some("new"));
		let parsed = read_document(&path).expect("read");
		assert_eq!(parsed["external"].as_str(), Some("kept"));
		assert_eq!(parsed["worktree"]["base"].as_str(), Some("new"));
	}

	#[test]
	fn unset_prunes_empty_parent_tables() {
		let directory = tempfile::tempdir().expect("directory");
		let path = directory.path().join("config.toml");
		atomic_replace(&path, "[worktree]\nbase = \"old\"\n").expect("initial replace");
		let result = mutate_document(&path, &[DocumentMutation::Unset { path: "worktree.base" }])
			.expect("mutation");
		assert!(!result.document.contains_key("worktree"));
	}
}

/// Native settings persistence failure.
#[derive(Debug, thiserror::Error)]
pub enum SettingsIoError {
	/// A parent directory could not be created.
	#[error("failed to create settings directory {path}")]
	CreateDirectory {
		/// Parent directory required to persist the settings file.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while creating the parent directory.
		source: io::Error,
	},
	/// The settings lock could not be acquired.
	#[error("failed to lock settings file {path}")]
	Lock {
		/// Sidecar lock file used to serialize settings updates.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while opening or locking the sidecar file.
		source: io::Error,
	},
	/// A settings source could not be read.
	#[error("failed to read settings file {path}")]
	Read {
		/// Native settings file being loaded.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while reading the settings file.
		source: io::Error,
	},
	/// A settings source was malformed.
	#[error("failed to parse settings file {path}")]
	Parse {
		/// Native settings file containing malformed TOML.
		path:   PathBuf,
		#[source]
		/// TOML failure returned while parsing the settings file.
		source: de::Error,
	},
	/// A YAML compatibility source was malformed.
	#[error("failed to parse YAML settings file {path}")]
	ParseYaml {
		/// YAML compatibility file containing malformed settings.
		path:   PathBuf,
		#[source]
		/// YAML failure returned while parsing the settings file.
		source: serde_yaml::Error,
	},
	/// Corrupt settings could not be moved aside.
	#[error("refusing to overwrite corrupt settings file {path}; quarantine {backup_path} failed")]
	Quarantine {
		/// Corrupt native settings file being moved aside.
		path:        PathBuf,
		/// Unique backup destination chosen for the corrupt file.
		backup_path: PathBuf,
		#[source]
		/// I/O failure returned while renaming the corrupt file.
		source:      io::Error,
	},
	/// A temporary file could not be created.
	#[error("failed to create temporary settings file {path}")]
	CreateTemporary {
		/// PID-isolated temporary file used for atomic persistence.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while creating the temporary file.
		source: io::Error,
	},
	/// A temporary file could not be written.
	#[error("failed to write temporary settings file {path}")]
	WriteTemporary {
		/// Temporary settings file receiving the encoded TOML.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while writing the encoded settings.
		source: io::Error,
	},
	/// A temporary file could not be synced.
	#[error("failed to sync temporary settings file {path}")]
	SyncTemporary {
		/// Temporary settings file being flushed before replacement.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while syncing the temporary file.
		source: io::Error,
	},
	/// TOML encoding failed.
	#[error(transparent)]
	Encode(#[from] ser::Error),
	/// The containing directory could not be synced after replacement.
	#[error("failed to sync settings directory {path}")]
	SyncDirectory {
		/// Parent directory containing the atomically replaced settings file.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while syncing the parent directory.
		source: io::Error,
	},
	/// Atomic replacement failed.
	#[error("failed to atomically replace settings file {path}")]
	Replace {
		/// Native settings file targeted by the atomic replacement.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while renaming the temporary file into place.
		source: io::Error,
	},
	/// Windows rollback after replacement failure also failed.
	#[error("failed to roll back settings file {path} after replacement failure")]
	Rollback {
		/// Native settings file whose failed replacement is being restored.
		path:   PathBuf,
		#[source]
		/// I/O failure returned while restoring the Windows backup.
		source: io::Error,
	},
}
