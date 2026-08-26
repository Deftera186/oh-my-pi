//! Explicit extension upgrade, rollback, pin, uninstall, and generation GC.

use std::{
	cmp,
	collections::BTreeSet,
	fs, io,
	path::{Path, PathBuf},
};

use omp_core::Str;
use serde::{Deserialize, Serialize};

use super::{
	ExtensionCode, ExtensionError, Layer,
	lock::{InstalledRecord, LockFile, atomic_toml},
};

/// Durable exact-version pins used only by explicit resolver operations.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PinsFile {
	/// File format version.
	#[serde(default = "one")]
	pub version: u32,
	/// Exact extension pins.
	#[serde(default, rename = "pin")]
	pub pins:    Vec<Pin>,
}

/// One extension version pin.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Pin {
	/// Extension identity.
	pub id:      Str,
	/// Exact pinned version.
	pub version: Str,
}

impl PinsFile {
	/// Reads an absent pin file as an empty set.
	pub fn read(path: &Path) -> Result<Self, ExtensionError> {
		if !path.exists() {
			return Ok(Self { version: 1, pins: Vec::new() });
		}
		let value = fs::read_to_string(path)
			.map_err(|error| ExtensionError::new(ExtensionCode::EIntegrity, error.to_string()))?;
		toml::from_str(&value)
			.map_err(|error| ExtensionError::new(ExtensionCode::EIntegrity, error.to_string()))
	}

	/// Sets or replaces one exact pin and persists atomically.
	pub fn set(&mut self, path: &Path, id: Str, version: Str) -> io::Result<()> {
		if let Some(pin) = self.pins.iter_mut().find(|pin| pin.id == id) {
			pin.version = version;
		} else {
			self.pins.push(Pin { id, version });
		}
		self.pins.sort_by(|left, right| left.id.cmp(&right.id));
		atomic_toml(path, self)
	}

	/// Removes one pin and persists atomically.
	pub fn remove(&mut self, path: &Path, id: &str) -> io::Result<bool> {
		let before = self.pins.len();
		self.pins.retain(|pin| pin.id != id);
		if self.pins.len() == before {
			return Ok(false);
		}
		atomic_toml(path, self)?;
		Ok(true)
	}
}

const fn one() -> u32 {
	1
}

/// Dry-run description of records affected by an uninstall.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UninstallPlan {
	/// Installed identities that will be removed.
	pub installed: Vec<Str>,
	/// Lock identities that will be removed.
	pub locked:    Vec<Str>,
	/// Requested identities not present in either authority.
	pub missing:   Vec<Str>,
}

/// Computes uninstall effects without mutating either state file.
pub fn plan_uninstall(
	installed: &InstalledRecord,
	lock: &LockFile,
	ids: impl IntoIterator<Item = Str>,
	keep_lock: bool,
) -> UninstallPlan {
	let mut plan = UninstallPlan::default();
	for id in ids {
		let present_installed = installed.extensions.iter().any(|entry| entry.id == id);
		let present_locked = lock.extensions.iter().any(|entry| entry.id == id);
		if present_installed {
			plan.installed.push(id.clone());
		}
		if present_locked && !keep_lock {
			plan.locked.push(id.clone());
		}
		if !present_installed && !present_locked {
			plan.missing.push(id);
		}
	}
	plan
}

/// Applies a previously reviewed uninstall plan in memory.
pub fn apply_uninstall(installed: &mut InstalledRecord, lock: &mut LockFile, plan: &UninstallPlan) {
	let installed_ids: BTreeSet<&Str> = plan.installed.iter().collect();
	installed
		.extensions
		.retain(|entry| !installed_ids.contains(&entry.id));
	let locked_ids: BTreeSet<&Str> = plan.locked.iter().collect();
	lock
		.extensions
		.retain(|entry| !locked_ids.contains(&entry.id));
	lock.packages.retain_mut(|package| {
		package.requested_by.retain(|id| !locked_ids.contains(id));
		!package.requested_by.is_empty()
	});
}

/// Enables or disables exactly one installed extension.
pub fn set_enabled(
	installed: &mut InstalledRecord,
	id: &str,
	enabled: bool,
) -> Result<(), ExtensionError> {
	let entry = installed
		.extensions
		.iter_mut()
		.find(|entry| entry.id == id)
		.ok_or_else(|| {
			ExtensionError::new(ExtensionCode::ENoManifest, "extension is not installed")
		})?;
	entry.enabled = enabled;
	Ok(())
}

/// Verified replacement state staged for one explicit upgrade or rollback.
#[derive(Clone, Debug)]
pub struct Generation {
	/// Reproducible lock state.
	pub lock:      LockFile,
	/// Local enabled/link selection state.
	pub installed: InstalledRecord,
}

/// Writes a verified generation while retaining a restorable copy of the prior
/// generation. Verification must happen before calling this function.
pub fn commit_generation(
	lock_path: &Path,
	installed_path: &Path,
	generation_root: &Path,
	generation_id: &str,
	generation: &Generation,
) -> Result<PathBuf, ExtensionError> {
	generation.lock.validate_for(generation.lock.layer)?;
	validate_generation_id(generation_id)?;
	fs::create_dir_all(generation_root).map_err(integrity)?;
	let stage_id = format!("{generation_id}.staging");
	let stage = contained_generation_path(generation_root, &stage_id, false)?;
	let committed = contained_generation_path(generation_root, generation_id, false)?;
	remove_contained_directory(generation_root, &stage)?;
	fs::create_dir(&stage).map_err(integrity)?;
	generation
		.lock
		.write(&stage.join("omp.lock"))
		.map_err(integrity)?;
	generation
		.installed
		.write(&stage.join("installed.toml"))
		.map_err(integrity)?;
	remove_contained_directory(generation_root, &committed)?;
	fs::rename(&stage, &committed).map_err(integrity)?;

	let old_lock = fs::read(lock_path).ok();
	let old_installed = fs::read(installed_path).ok();
	if let Err(error) = generation.lock.write(lock_path) {
		return Err(integrity(error));
	}
	if let Err(error) = generation.installed.write(installed_path) {
		restore(lock_path, old_lock.as_deref());
		restore(installed_path, old_installed.as_deref());
		return Err(integrity(error));
	}
	Ok(committed)
}

/// Loads an immutable prior generation for an explicit rollback.
pub fn load_generation(
	generation_root: &Path,
	generation_id: &str,
	layer: Layer,
) -> Result<Generation, ExtensionError> {
	validate_generation_id(generation_id)?;
	let root = contained_generation_path(generation_root, generation_id, true)?;
	Ok(Generation {
		lock:      LockFile::read(&root.join("omp.lock"), layer)?,
		installed: InstalledRecord::read(&root.join("installed.toml"))?,
	})
}

/// GC report. Collection is a dry run unless `apply` is true.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
	/// Unreachable generation directories.
	pub generations: Vec<PathBuf>,
	/// Total bytes reachable beneath those directories.
	pub bytes:       u64,
}

/// Retains the newest `keep` immutable generations and reports or removes the
/// remainder. Active lock/install files are outside this cache and cannot be
/// collected.
pub fn gc_generations(root: &Path, keep: usize, apply: bool) -> Result<GcReport, ExtensionError> {
	if !root.exists() {
		return Ok(GcReport::default());
	}
	let mut entries = fs::read_dir(root)
		.map_err(integrity)?
		.filter_map(Result::ok)
		.filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
		.collect::<Vec<_>>();
	entries.sort_by_key(|entry| cmp::Reverse(entry.file_name()));
	let mut report = GcReport::default();
	for entry in entries.into_iter().skip(keep) {
		let path = entry.path();
		report.bytes = report.bytes.saturating_add(directory_bytes(&path)?);
		report.generations.push(path.clone());
		if apply {
			fs::remove_dir_all(path).map_err(integrity)?;
		}
	}
	Ok(report)
}

fn directory_bytes(root: &Path) -> Result<u64, ExtensionError> {
	let mut total = 0_u64;
	let mut pending = vec![root.to_path_buf()];
	while let Some(path) = pending.pop() {
		for entry in fs::read_dir(path).map_err(integrity)? {
			let entry = entry.map_err(integrity)?;
			let metadata = entry.metadata().map_err(integrity)?;
			if metadata.is_dir() {
				pending.push(entry.path());
			} else {
				total = total.saturating_add(metadata.len());
			}
		}
	}
	Ok(total)
}

fn validate_generation_id(generation_id: &str) -> Result<(), ExtensionError> {
	if generation_id.is_empty()
		|| matches!(generation_id, "." | "..")
		|| generation_id.len() > 128
		|| !generation_id
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
	{
		return Err(ExtensionError::new(ExtensionCode::EIntegrity, "invalid generation id"));
	}
	Ok(())
}

fn contained_generation_path(
	generation_root: &Path,
	generation_id: &str,
	must_exist: bool,
) -> Result<PathBuf, ExtensionError> {
	validate_generation_id(generation_id)?;
	let canonical_root = generation_root.canonicalize().map_err(integrity)?;
	let candidate = canonical_root.join(generation_id);
	if must_exist {
		let metadata = fs::symlink_metadata(&candidate).map_err(integrity)?;
		if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
			return Err(ExtensionError::new(
				ExtensionCode::EIntegrity,
				"generation is not an owned directory",
			));
		}
		let canonical_candidate = candidate.canonicalize().map_err(integrity)?;
		if canonical_candidate.parent() != Some(canonical_root.as_path()) {
			return Err(ExtensionError::new(
				ExtensionCode::EIntegrity,
				"generation escapes the generation root",
			));
		}
		return Ok(canonical_candidate);
	}
	if candidate.parent() != Some(canonical_root.as_path()) {
		return Err(ExtensionError::new(
			ExtensionCode::EIntegrity,
			"generation escapes the generation root",
		));
	}
	Ok(candidate)
}

fn remove_contained_directory(generation_root: &Path, path: &Path) -> Result<(), ExtensionError> {
	let Ok(metadata) = fs::symlink_metadata(path) else {
		return Ok(());
	};
	if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
		return Err(ExtensionError::new(
			ExtensionCode::EIntegrity,
			"generation cache entry is not an owned directory",
		));
	}
	let canonical_root = generation_root.canonicalize().map_err(integrity)?;
	let canonical_path = path.canonicalize().map_err(integrity)?;
	if canonical_path.parent() != Some(canonical_root.as_path()) {
		return Err(ExtensionError::new(
			ExtensionCode::EIntegrity,
			"generation cache entry escapes the generation root",
		));
	}
	fs::remove_dir_all(canonical_path).map_err(integrity)
}

fn restore(path: &Path, bytes: Option<&[u8]>) {
	match bytes {
		Some(bytes) => {
			let _ = fs::write(path, bytes);
		},
		None => {
			let _ = fs::remove_file(path);
		},
	}
}

fn integrity(error: io::Error) -> ExtensionError {
	ExtensionError::new(ExtensionCode::EIntegrity, error.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn generation_ids_are_single_safe_components() {
		for invalid in ["", ".", "..", "../outside", "a/b", "a\\b", "white space"] {
			assert!(validate_generation_id(invalid).is_err(), "{invalid:?}");
		}
		for valid in ["01J6FZB5QNF3J1XW7TG6QY7A4V", "plugin-1.2.3", "rollback_2"] {
			assert!(validate_generation_id(valid).is_ok(), "{valid:?}");
		}
	}

	#[cfg(unix)]
	#[test]
	fn generation_load_rejects_symlink_escape() {
		use std::os::unix::fs::symlink;

		let temporary = tempfile::tempdir().unwrap();
		let root = temporary.path().join("generations");
		let outside = temporary.path().join("outside");
		fs::create_dir_all(&root).unwrap();
		fs::create_dir_all(&outside).unwrap();
		symlink(&outside, root.join("escaped")).unwrap();

		let error = load_generation(&root, "escaped", Layer::Client).unwrap_err();
		assert_eq!(error.code, ExtensionCode::EIntegrity);
	}
}
