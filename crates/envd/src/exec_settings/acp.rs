//! Native ACP exec-backend routing settings.

use omp_settings::{
	FieldDescriptor, OptionProvider, SettingKind, SettingOption, SettingScope, SettingsDomain,
};
use serde::{Deserialize, Serialize};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const ROUTING_VALUES: &[&str] = &["auto", "never"];
const ROUTING_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "auto",
		label:       "Automatic",
		description: Some(
			"Prefer a capable ACP terminal and fall back to the normal Environment backend.",
		),
	},
	SettingOption {
		value:       "never",
		label:       "Never",
		description: Some("Always use the normal Environment backend."),
	},
];

/// Routing policy for capability-advertised ACP terminal execution.
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
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AcpRouting {
	/// Prefer ACP for eligible calls only when the Environment advertises it.
	#[default]
	Auto,
	/// Never route shell execution through ACP.
	Never,
}

/// ACP execution settings consumed by shell backend selection.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AcpSettings {
	/// Capability-gated terminal routing policy.
	pub routing: AcpRouting,
}

impl SettingsDomain for AcpSettings {
	const DOMAIN: &'static str = "acp";
	const FIELDS: &'static [FieldDescriptor] = &[FieldDescriptor {
		path:        "acp.routing",
		label:       "ACP Shell Routing",
		description: "Choose whether eligible shell calls prefer a capable ACP terminal backend.",
		kind:        SettingKind::Enum(ROUTING_VALUES),
		scopes:      PERSISTED,
		order:       10,
		options:     Some(OptionProvider::Static(ROUTING_OPTIONS)),
		condition:   None,
		secret:      false,
	}];
}

#[cfg(test)]
mod tests {
	use omp_settings::SettingsSnapshot;

	use super::*;

	#[test]
	fn acp_projection_round_trips() {
		let expected = AcpSettings { routing: AcpRouting::Never };
		let snapshot = SettingsSnapshot::isolated(expected.clone(), crate::TEST_SETTINGS_CATALOG)
			.expect("isolated snapshot");
		assert_eq!(snapshot.project::<AcpSettings>().expect("projection").get(), &expected);
	}
}
