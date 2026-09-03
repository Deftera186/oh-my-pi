//! Host-owned disposable routing index for live session kernels.

use std::{
	fs, io,
	path::{Path, PathBuf},
	sync::Arc,
	time::UNIX_EPOCH,
};

use omp_agent::{SessionAuthority, SessionEndpoint, Up};
use omp_core::{FastHashMap, Str};
use omp_dom::Snapshot;
use parking_lot::RwLock;

omp_core::string_id!(
	/// Stable live-session identifier.
	SessionId
);

/// Cloneable endpoint retained by the process composition for one live kernel.
#[derive(Clone)]
pub struct KernelHandle {
	/// Stable session identity.
	pub id:       SessionId,
	/// Display and routing name.
	pub name:     Str,
	/// The kernel's sole upward mailbox.
	pub up:       flume::Sender<Up>,
	/// Latest detached DOM projection.
	pub snapshot: Arc<RwLock<Snapshot>>,
}

impl KernelHandle {
	/// Refreshes the detached projection after the controller advances.
	pub fn refresh(&self, session: &omp_session::Session) {
		*self.snapshot.write() = session.dom().snapshot();
	}

	fn endpoint(&self) -> SessionEndpoint {
		SessionEndpoint {
			id:       Str::new(self.id.as_str()),
			name:     self.name.clone(),
			up:       self.up.clone(),
			snapshot: Arc::clone(&self.snapshot),
		}
	}
}

#[derive(Default)]
struct RegistryState {
	by_id:   FastHashMap<SessionId, KernelHandle>,
	by_name: FastHashMap<Str, SessionId>,
}

/// Thread-safe process-local index of live session controllers.
///
/// This is routing state only. It is never persisted and every projected
/// session fact remains owned by the journal-backed controller.
#[derive(Default)]
pub struct SessionRegistry {
	state: RwLock<RegistryState>,
}

impl SessionRegistry {
	/// Creates an empty live-session registry.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Registers or replaces one live kernel endpoint.
	pub fn register(&self, name: Str, mut handle: KernelHandle) -> Option<KernelHandle> {
		handle.name = name.clone();
		let mut state = self.state.write();
		if let Some(previous_id) = state.by_name.insert(name, handle.id.clone())
			&& previous_id != handle.id
		{
			state.by_id.remove(&previous_id);
		}
		let id = handle.id.clone();
		let current_name = handle.name.clone();
		let previous = state.by_id.insert(id, handle);
		if let Some(previous) = &previous
			&& previous.name != current_name
		{
			state.by_name.remove(&previous.name);
		}
		previous
	}

	/// Removes one retired session.
	pub fn remove(&self, id: &SessionId<str>) -> Option<KernelHandle> {
		let mut state = self.state.write();
		let handle = state.by_id.remove(id)?;
		state.by_name.remove(&handle.name);
		Some(handle)
	}

	/// Looks up a live kernel by stable session id.
	#[must_use]
	pub fn lookup(&self, id: &SessionId<str>) -> Option<KernelHandle> {
		self.state.read().by_id.get(id).cloned()
	}

	/// Looks up a live kernel by its routing name.
	#[must_use]
	pub fn lookup_name(&self, name: &str) -> Option<KernelHandle> {
		let state = self.state.read();
		let id = state.by_name.get(name)?;
		state.by_id.get(id).cloned()
	}

	/// Lists every addressable live kernel.
	#[must_use]
	pub fn list(&self) -> Vec<KernelHandle> {
		self.state.read().by_id.values().cloned().collect()
	}
}

/// Journal-derived metadata for one durable session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSession {
	/// Session selector, conventionally the `.oms` file stem.
	pub id:         Str,
	/// Canonical journal path.
	pub path:       PathBuf,
	/// Working directory recorded by the genesis frame.
	pub cwd:        Str,
	/// Genesis creation value.
	pub created:    Str,
	/// Journal file modification time in Unix milliseconds.
	pub updated_ms: u64,
}

/// Disposable in-memory lookup rebuilt by scanning `.oms` genesis frames.
#[derive(Default)]
pub struct SessionIndex {
	by_id: RwLock<FastHashMap<Str, StoredSession>>,
}

impl SessionIndex {
	/// Scans `root` and builds a disposable index from authoritative journals.
	pub fn open(root: impl AsRef<Path>) -> Result<Self, io::Error> {
		let index = Self::default();
		index.refresh(root)?;
		Ok(index)
	}

	/// Replaces every cached row from the current journal directory contents.
	pub fn refresh(&self, root: impl AsRef<Path>) -> Result<(), io::Error> {
		let mut paths = Vec::new();
		collect_journals(root.as_ref(), &mut paths)?;
		let mut rows = FastHashMap::default();
		for path in paths {
			let (_, entries) = match omp_journal::Journal::open(&path) {
				Ok(opened) => opened,
				Err(error) => {
					tracing::warn!(journal = %path.display(), %error, "skipping invalid session journal");
					continue;
				},
			};
			let Some(genesis) = entries.first() else {
				continue;
			};
			let payload: omp_journal::data::Genesis = match serde_json::from_str(genesis.data.as_str())
			{
				Ok(payload) => payload,
				Err(error) => {
					tracing::warn!(journal = %path.display(), %error, "skipping journal with invalid genesis");
					continue;
				},
			};
			let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
				continue;
			};
			let updated_ms = fs::metadata(&path)?
				.modified()?
				.duration_since(UNIX_EPOCH)
				.unwrap_or_default()
				.as_millis()
				.try_into()
				.unwrap_or(u64::MAX);
			rows.insert(Str::new(stem), StoredSession {
				id: Str::new(stem),
				path,
				cwd: payload.cwd,
				created: payload.created,
				updated_ms,
			});
		}
		*self.by_id.write() = rows;
		Ok(())
	}

	/// Looks up one derived durable-session row.
	#[must_use]
	pub fn get(&self, id: &str) -> Option<StoredSession> {
		self.by_id.read().get(id).cloned()
	}

	/// Lists derived sessions newest first.
	#[must_use]
	pub fn list(&self) -> Vec<StoredSession> {
		let mut rows: Vec<_> = self.by_id.read().values().cloned().collect();
		rows.sort_unstable_by(|left, right| {
			right
				.updated_ms
				.cmp(&left.updated_ms)
				.then_with(|| left.id.cmp(&right.id))
		});
		rows
	}
}

fn collect_journals(directory: &Path, output: &mut Vec<PathBuf>) -> Result<(), io::Error> {
	let entries = match fs::read_dir(directory) {
		Ok(entries) => entries,
		Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
		Err(error) => return Err(error),
	};
	for entry in entries {
		let path = entry?.path();
		if path.is_dir() {
			collect_journals(&path, output)?;
		} else if path.extension().and_then(|value| value.to_str())
			== Some(omp_journal::FILE_EXTENSION)
		{
			output.push(path);
		}
	}
	Ok(())
}

impl SessionAuthority for SessionRegistry {
	fn lookup(&self, id_or_name: &str) -> Option<SessionEndpoint> {
		self
			.lookup(SessionId::from_ref(id_or_name))
			.or_else(|| self.lookup_name(id_or_name))
			.map(|handle| handle.endpoint())
	}

	fn list(&self) -> Vec<SessionEndpoint> {
		SessionRegistry::list(self)
			.into_iter()
			.map(|handle| handle.endpoint())
			.collect()
	}
}
