//! Typed sleep-prevention settings owned by the power runtime.

use std::{env, path::Path};

use omp_settings::{
	FieldDescriptor, OptionProvider, SettingKind, SettingOption, SettingScope, SettingsDomain,
	manager::{SettingsManager, SettingsManagerError, SettingsPaths},
};
use serde::{Deserialize, Serialize};

use super::SleepPrevention;

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const VALUES: &[&str] = &["off", "idle", "display", "system"];
const OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "off",
		label:       "Off",
		description: Some("Do not prevent host sleep."),
	},
	SettingOption {
		value:       "idle",
		label:       "Idle Sleep",
		description: Some("Prevent idle system sleep during active runs."),
	},
	SettingOption {
		value:       "display",
		label:       "Display Sleep",
		description: Some("Prevent display sleep during active runs."),
	},
	SettingOption {
		value:       "system",
		label:       "System Sleep",
		description: Some("Prevent system sleep during active runs."),
	},
];

/// Power behavior projected once into the active agent runtime.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PowerSettings {
	/// Assertion strength acquired only while an agent run is active.
	pub sleep_prevention: SleepPrevention,
}

impl SettingsDomain for PowerSettings {
	const DOMAIN: &'static str = "power";
	const FIELDS: &'static [FieldDescriptor] = &[FieldDescriptor {
		path:        "power.sleep_prevention",
		label:       "Sleep Prevention",
		description: "Keep the Mac awake only while an agent run is active.",
		kind:        SettingKind::Enum(VALUES),
		scopes:      PERSISTED,
		order:       10,
		options:     Some(OptionProvider::Static(OPTIONS)),
		condition:   None,
		secret:      false,
	}];
}

/// Loads the current power projection through the application settings
/// authority.
pub fn current(data_dir: &Path) -> Result<PowerSettings, SettingsManagerError> {
	let project = env::current_dir().ok();
	let manager = SettingsManager::open(
		SettingsPaths::discover(data_dir, project.as_deref()),
		crate::SETTINGS_CATALOG,
	)?;
	let projection = manager
		.snapshot()
		.project::<PowerSettings>()
		.map_err(|source| SettingsManagerError::Projection { source })?;
	Ok(projection.get().clone())
}

#[cfg(test)]
mod tests {
	use omp_settings::{SettingsCatalog, SettingsSnapshot};

	use super::*;

	const CATALOG: SettingsCatalog =
		SettingsCatalog::new(&[&omp_settings::SETTINGS_CONTRIBUTION, &crate::SETTINGS_CONTRIBUTION]);

	#[test]
	fn power_projection_round_trips() {
		assert_eq!(PowerSettings::default().sleep_prevention, SleepPrevention::Idle);
		let expected = PowerSettings { sleep_prevention: SleepPrevention::Display };
		let snapshot =
			SettingsSnapshot::isolated(expected.clone(), CATALOG).expect("isolated snapshot");
		assert_eq!(
			snapshot
				.project::<PowerSettings>()
				.expect("projection")
				.get(),
			&expected
		);
	}
}
