//! Canonical native storage roots and the owner's private data directory.
//!
//! Configuration lives in the owner's `~/.o2` directory (`OMP_CONFIG_DIR`
//! overrides it; see [`config_dir`]); mutable application data is split by
//! XDG purpose so caches can be discarded without losing durable state.
//!
//! ```no_run
//! let data = omp_core::dirs::data_dir(None)?;
//! # Ok::<_, omp_core::dirs::DataDirError>(())
//! ```

use std::{
	env,
	path::{Path, PathBuf},
	sync::OnceLock,
};

use thiserror::Error;

use crate::Str;

/// Canonical native storage roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeDirectories {
	/// Durable application data.
	pub data:  PathBuf,
	/// Durable runtime state.
	pub state: PathBuf,
	/// Re-creatable downloaded and derived data.
	pub cache: PathBuf,
}

/// Resolves the canonical native data, state, and cache roots.
///
/// Explicit OMP variables win. Otherwise the corresponding XDG variable is
/// used, followed by the XDG home-relative default.
#[must_use]
pub fn native_directories(home: &Path) -> NativeDirectories {
	fn root(omp: &str, xdg: &str, fallback: &Path) -> PathBuf {
		env::var_os(omp)
			.filter(|value| !value.is_empty())
			.map(PathBuf::from)
			.or_else(|| {
				env::var_os(xdg)
					.filter(|value| !value.is_empty())
					.map(|value| PathBuf::from(value).join("omp"))
			})
			.unwrap_or_else(|| fallback.join("omp"))
	}
	NativeDirectories {
		data:  root("OMP_DATA_DIR", "XDG_DATA_HOME", &home.join(".local/share")),
		state: root("OMP_STATE_DIR", "XDG_STATE_HOME", &home.join(".local/state")),
		cache: root("OMP_CACHE_DIR", "XDG_CACHE_HOME", &home.join(".cache")),
	}
}

static SELECTED_PROFILE: OnceLock<Option<Str>> = OnceLock::new();
/// Returns the process owner's home directory from the platform environment.
///
/// `HOME` is preferred for Unix-compatible environments. `USERPROFILE`
/// provides the native Windows fallback.
pub fn home_dir() -> Option<PathBuf> {
	env::var_os("HOME")
		.filter(|value| !value.is_empty())
		.or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
		.map(PathBuf::from)
}

/// Publishes the bootstrap-selected profile without mutating the environment.
///
/// The first call wins; later calls are ignored so every consumer observes one
/// immutable process-wide selection.
pub fn set_selected_profile(profile: Option<Str>) {
	let _ = SELECTED_PROFILE.set(profile);
}

/// Returns the bootstrap-selected profile, if one was published.
#[must_use]
pub fn selected_profile() -> Option<&'static str> {
	SELECTED_PROFILE
		.get()
		.and_then(|profile| profile.as_deref())
}

/// Default configuration directory name under the owner's home.
///
/// Pinned by the owner: user configuration (`config.cfg`, agent assets, cfg
/// scripts) lives in `~/.o2`, never in the data or XDG config roots.
pub const CONFIG_DIR_NAME: &str = ".o2";

/// Resolves the owner's configuration directory.
///
/// `OMP_CONFIG_DIR` wins when set and non-empty; otherwise `<home>/.o2`.
#[must_use]
pub fn config_dir(home: &Path) -> PathBuf {
	env::var_os("OMP_CONFIG_DIR")
		.filter(|value| !value.is_empty())
		.map_or_else(|| home.join(CONFIG_DIR_NAME), PathBuf::from)
}

/// Failure to resolve the owner's private data directory.
#[derive(Clone, Copy, Debug, Error)]
pub enum DataDirError {
	/// Neither an explicit path, `OMP_DATA_DIR`, `HOME`, nor `USERPROFILE` was
	/// available.
	#[error("HOME, USERPROFILE, or OMP_DATA_DIR must be set")]
	HomeUnset,
}

/// Resolves the owner's private data directory, honoring the selected profile.
///
/// An `explicit` path is used verbatim. Otherwise `OMP_DATA_DIR` wins, then the
/// XDG data root; a selected profile appends `profiles/<profile>`.
///
/// # Errors
///
/// Returns [`DataDirError::HomeUnset`] when no root can be derived.
pub fn data_dir(explicit: Option<PathBuf>) -> Result<PathBuf, DataDirError> {
	if let Some(path) = explicit {
		return Ok(path);
	}
	let base = if let Some(path) = env::var_os("OMP_DATA_DIR").filter(|value| !value.is_empty()) {
		PathBuf::from(path)
	} else {
		let home = home_dir().ok_or(DataDirError::HomeUnset)?;
		native_directories(&home).data
	};
	Ok(selected_profile().map_or(base.clone(), |profile| base.join("profiles").join(profile)))
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::{CONFIG_DIR_NAME, config_dir};

	#[test]
	fn config_dir_defaults_to_dot_o2_under_home() {
		// The env override is process-global; this test only asserts the
		// pinned default when it is absent.
		if std::env::var_os("OMP_CONFIG_DIR").is_some_and(|value| !value.is_empty()) {
			return;
		}
		assert_eq!(CONFIG_DIR_NAME, ".o2");
		assert_eq!(config_dir(Path::new("/home/owner")), Path::new("/home/owner/.o2"));
	}
}
