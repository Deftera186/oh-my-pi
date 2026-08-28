//! Environment-host projection of the persisted settings tables.
//!
//! The host enforces a narrow slice of configuration: its own tool policy plus
//! the runtime, worktree, memory, and autolearn knobs it owns. Projecting a
//! dedicated read-only domain keeps the environment independent of the
//! client-side aggregate while both read the same merged tables.
//!
//! ```ignore
//! let settings = host_settings::load(state_dir, workspace.root(), catalog)?;
//! let grace = settings.runtime.interrupt_grace;
//! ```

use std::{
	fmt,
	path::{Path, PathBuf},
};

use omp_core::{Duration, DurationError};
use omp_memory::config::{AutolearnSettings, MemorySettings, MnemopiSettings};
use omp_settings::{
	SettingsCatalog,
	manager::{SettingsManager, SettingsManagerError, SettingsPaths},
};
use omp_tool::DEFAULT_INTERRUPT_GRACE;
use serde::{
	Deserialize, Deserializer, Serialize, Serializer,
	de::{self, Visitor},
};

use super::tool_settings::ToolSettings;

/// Runtime durations shared by the agent, eval, and extension-host control
/// planes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeDurations {
	/// Courtesy interval between cooperative cancellation and forced
	/// interruption.
	#[serde(with = "nonzero_duration")]
	pub interrupt_grace: Duration,
}

impl Default for RuntimeDurations {
	fn default() -> Self {
		Self { interrupt_grace: DEFAULT_INTERRUPT_GRACE }
	}
}

/// Placement policy for Environment-owned isolated worktrees.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorktreeSettings {
	/// Optional base directory. `OMP_WORKTREE_DIR` takes precedence.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub base: Option<PathBuf>,
}

/// Settings the environment host reads without owning the client aggregate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostSettings {
	/// Model key selected as the default, used to pin the edit dialect.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub default_model: Option<String>,
	/// Runtime timeout and cancellation settings.
	#[serde(default)]
	pub runtime:       RuntimeDurations,
	/// Built-in tool exposure and execution timeout policy.
	#[serde(default)]
	pub tools:         ToolSettings,
	/// Default-off memory backend selector.
	#[serde(default)]
	pub memory:        MemorySettings,
	/// Mnemopi-specific durable bank and lifecycle settings.
	#[serde(default)]
	pub mnemopi:       MnemopiSettings,
	/// Automatic-learning capture settings.
	#[serde(default)]
	pub autolearn:     AutolearnSettings,
	/// Isolated worktree placement policy.
	#[serde(default)]
	pub worktree:      WorktreeSettings,
}

impl omp_settings::SettingsDomain for HostSettings {
	const DOMAIN: &'static str = "envd-host";
	const FIELDS: &'static [omp_settings::FieldDescriptor] = &[];
	const PREFIX: Option<&'static str> = None;
}

/// Loads the host projection layered for `project_root`.
///
/// # Errors
///
/// Fails when the settings tables cannot be opened or do not project onto
/// [`HostSettings`].
pub fn load(
	data_dir: &Path,
	project_root: &Path,
	catalog: SettingsCatalog,
) -> Result<HostSettings, SettingsManagerError> {
	let manager =
		SettingsManager::open(SettingsPaths::discover(data_dir, Some(project_root)), catalog)?;
	let mut settings = manager
		.snapshot()
		.project::<HostSettings>()
		.map_err(|source| SettingsManagerError::Projection { source })?
		.get()
		.clone();
	settings.mnemopi = settings.mnemopi.normalize();
	Ok(settings)
}

mod nonzero_duration {
	use super::*;

	pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.collect_str(value)
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
	where
		D: Deserializer<'de>,
	{
		deserializer.deserialize_str(DurationVisitor)
	}

	struct DurationVisitor;

	impl Visitor<'_> for DurationVisitor {
		type Value = Duration;

		fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
			formatter.write_str("a positive integer duration with an explicit ns/us/ms/s/m/h unit")
		}

		fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
		where
			E: de::Error,
		{
			let duration = value.parse::<Duration>().map_err(E::custom)?;
			if duration.value() == 0 {
				return Err(E::custom("duration must be greater than zero"));
			}
			let standard = duration.to_std().map_err(|error| match error {
				DurationError::Overflow => E::custom("duration is too large"),
				other => E::custom(other),
			})?;
			i64::try_from(standard.as_nanos())
				.map_err(|_| E::custom("duration is too large for telemetry serialization"))?;
			Ok(duration)
		}
	}
}
