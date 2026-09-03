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
	/// First line of the first user prompt, when the session has one.
	pub title:      Option<Str>,
}

impl StoredSession {
	/// Human label for pickers and the welcome box: the first prompt's first
	/// line, else the session id.
	#[must_use]
	pub fn display_name(&self) -> Str {
		self.title.clone().unwrap_or_else(|| self.id.clone())
	}
}

/// First non-empty line of the earliest `msg.user@1` entry, control
/// characters stripped.
fn first_prompt_title(entries: &[omp_journal::Entry]) -> Option<Str> {
	let user = omp_journal::Kind::known(omp_journal::KindName::MsgUser);
	entries
		.iter()
		.filter(|entry| entry.kind == user)
		.find_map(|entry| {
			let payload: omp_journal::data::MsgUser = serde_json::from_str(entry.data.as_str()).ok()?;
			let line = payload.text.lines().next()?;
			let clean = line
				.chars()
				.filter(|character| !character.is_control())
				.collect::<String>();
			let clean = clean.trim();
			(!clean.is_empty()).then(|| Str::new(clean))
		})
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
			let title = first_prompt_title(&entries);
			rows.insert(Str::new(stem), StoredSession {
				id: Str::new(stem),
				path,
				cwd: payload.cwd,
				created: payload.created,
				updated_ms,
				title,
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

	/// The newest `limit` sessions other than the journal at `exclude`, for
	/// the welcome box's recent-session rows.
	#[must_use]
	pub fn recent(&self, exclude: Option<&Path>, limit: usize) -> Vec<StoredSession> {
		let mut rows = self.list();
		rows.retain(|row| exclude.is_none_or(|exclude| row.path != exclude));
		rows.truncate(limit);
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

#[cfg(test)]
mod tests {
	use std::time::{Duration, SystemTime};

	use omp_journal::{EntryDraft, Journal, Kind, KindName};

	use super::*;

	fn write_journal(root: &Path, stem: &str, prompt: Option<&str>, age: Duration) -> PathBuf {
		let path = root.join(format!("{stem}.{}", omp_journal::FILE_EXTENSION));
		let mut journal = Journal::create(&path).expect("create journal");
		let genesis = journal
			.append(EntryDraft {
				kind:  Kind::known(KindName::Journal),
				by:    None,
				prior: None,
				label: None,
				data:  Str::new(r#"{"version":1,"cwd":"/w","created":"2026-01-01T00:00:00Z"}"#),
			})
			.expect("genesis");
		if let Some(prompt) = prompt {
			let payload = serde_json::json!({ "text": prompt }).to_string();
			journal
				.append(EntryDraft {
					kind:  Kind::known(KindName::MsgUser),
					by:    Some(genesis.id),
					prior: None,
					label: None,
					data:  Str::new(payload),
				})
				.expect("prompt");
		}
		drop(journal);
		let modified = SystemTime::now() - age;
		fs::File::options()
			.write(true)
			.open(&path)
			.expect("reopen")
			.set_modified(modified)
			.expect("set mtime");
		path
	}

	#[test]
	fn recent_orders_newest_first_and_excludes_the_open_journal() {
		let scratch = tempfile::tempdir().expect("tempdir");
		let root = scratch.path();
		let oldest = write_journal(root, "old", Some("  Fix the parser\nsecond line"), Duration::from_secs(300));
		let current = write_journal(root, "current", Some("live prompt"), Duration::from_secs(0));
		let middle = write_journal(root, "mid", None, Duration::from_secs(60));
		let control = write_journal(root, "ctl", Some("\u{7}\t\n"), Duration::from_secs(120));

		let index = SessionIndex::open(root).expect("index");
		let recent = index.recent(Some(&current), 2);
		assert_eq!(
			recent.iter().map(|row| row.path.clone()).collect::<Vec<_>>(),
			[middle.clone(), control.clone()]
		);
		assert_eq!(recent[0].display_name().as_str(), "mid", "no prompt falls back to the id");
		assert_eq!(recent[1].display_name().as_str(), "ctl", "control-only prompt falls back to the id");

		let all = index.recent(Some(&current), 8);
		assert_eq!(all.iter().map(|row| row.path.clone()).collect::<Vec<_>>(), [middle, control, oldest]);
		assert_eq!(all[2].display_name().as_str(), "Fix the parser");
		assert!(index.recent(None, 8).iter().any(|row| row.path == current));
	}
}
