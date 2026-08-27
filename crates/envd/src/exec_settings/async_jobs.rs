//! Native async-job capacity, retention, and wait settings.

use omp_settings::{
	DomainRegistration, FieldDescriptor, OptionProvider, SettingKind, SettingOption, SettingScope,
	SettingsDomain,
};
use serde::{Deserialize, Serialize};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const POLL_WAIT_VALUES: &[&str] = &["5s", "10s", "30s", "1m", "5m", "smart"];
const POLL_WAIT_OPTIONS: &[SettingOption] = &[
	SettingOption { value: "5s", label: "5 seconds", description: None },
	SettingOption { value: "10s", label: "10 seconds", description: None },
	SettingOption { value: "30s", label: "30 seconds", description: None },
	SettingOption { value: "1m", label: "1 minute", description: None },
	SettingOption { value: "5m", label: "5 minutes", description: None },
	SettingOption {
		value:       "smart",
		label:       "Smart",
		description: Some("Adaptive 5-second to 5-minute wait with idle reset."),
	},
];

/// Maximum duration used by an implicit background-job wait.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Eq,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
pub enum PollWaitDuration {
	/// Wait five seconds.
	#[serde(rename = "5s")]
	#[strum(to_string = "5s")]
	Seconds5,
	/// Wait ten seconds.
	#[serde(rename = "10s")]
	#[strum(to_string = "10s")]
	Seconds10,
	/// Wait thirty seconds.
	#[serde(rename = "30s")]
	#[strum(to_string = "30s")]
	Seconds30,
	/// Wait one minute.
	#[serde(rename = "1m")]
	#[strum(to_string = "1m")]
	Minute1,
	/// Wait five minutes.
	#[serde(rename = "5m")]
	#[strum(to_string = "5m")]
	Minutes5,
	/// Apply the adaptive wait ladder.
	#[default]
	#[serde(rename = "smart")]
	#[strum(to_string = "smart")]
	Smart,
}

/// Settings consumed by the authoritative async job board.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsyncJobSettings {
	/// Whether detached execution is available.
	pub enabled:            bool,
	/// Concurrent running-job capacity; zero means unlimited.
	pub max_jobs:           u32,
	/// Duration terminal rows remain observable after settlement.
	pub retention_ms:       u64,
	/// Implicit wait duration or adaptive policy.
	pub poll_wait_duration: PollWaitDuration,
}

impl Default for AsyncJobSettings {
	fn default() -> Self {
		Self {
			enabled:            true,
			max_jobs:           100,
			retention_ms:       5 * 60 * 1_000,
			poll_wait_duration: PollWaitDuration::Smart,
		}
	}
}

impl SettingsDomain for AsyncJobSettings {
	const DOMAIN: &'static str = "async";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "async.enabled",
			label:       "Async Execution",
			description: "Enable detached shell, task, and evaluation jobs.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "async.max_jobs",
			label:       "Async Job Capacity",
			description: "Maximum running detached jobs; zero removes the capacity ceiling.",
			kind:        SettingKind::Integer,
			scopes:      PERSISTED,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "async.retention_ms",
			label:       "Async Job Retention",
			description: "Milliseconds to retain terminal job rows for observation.",
			kind:        SettingKind::Integer,
			scopes:      PERSISTED,
			order:       30,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "async.poll_wait_duration",
			label:       "Maximum Poll Time",
			description: "Maximum implicit wait, or the adaptive wait ladder.",
			kind:        SettingKind::Enum(POLL_WAIT_VALUES),
			scopes:      PERSISTED,
			order:       40,
			options:     Some(OptionProvider::Static(POLL_WAIT_OPTIONS)),
			condition:   None,
			secret:      false,
		},
	];
}

omp_settings::inventory::submit! {
	DomainRegistration::of::<AsyncJobSettings>()
}

#[cfg(test)]
mod tests {
	use omp_settings::{SettingsSnapshot, registered_domains};

	use super::*;

	#[test]
	fn async_projection_round_trips_and_is_registered() {
		let expected = AsyncJobSettings {
			enabled:            true,
			max_jobs:           0,
			retention_ms:       42_000,
			poll_wait_duration: PollWaitDuration::Seconds30,
		};
		let snapshot = SettingsSnapshot::isolated(expected.clone()).expect("isolated snapshot");
		assert_eq!(
			snapshot
				.project::<AsyncJobSettings>()
				.expect("projection")
				.get(),
			&expected
		);
		assert!(
			registered_domains()
				.iter()
				.any(|domain| domain.name == AsyncJobSettings::DOMAIN)
		);
	}
}
