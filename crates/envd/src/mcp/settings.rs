//! Typed settings owned by the Environment MCP runtime.

use omp_settings::{FieldDescriptor, SettingKind, SettingScope, SettingsDomain};
use serde::{Deserialize, Serialize};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

/// Native MCP discovery policy.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpSettings {
	/// Whether project `.omp/mcp.json` and root `.mcp.json` sources participate.
	pub enable_project_config: bool,
}

impl Default for McpSettings {
	fn default() -> Self {
		Self { enable_project_config: true }
	}
}

impl SettingsDomain for McpSettings {
	const DOMAIN: &'static str = "mcp";
	const FIELDS: &'static [FieldDescriptor] = &[FieldDescriptor {
		path:        "mcp.enableProjectConfig",
		label:       "Project MCP configuration",
		description: "Load native project MCP server configuration.",
		kind:        SettingKind::Boolean,
		scopes:      PERSISTED,
		order:       10,
		options:     None,
		condition:   None,
		secret:      false,
	}];

	fn validate(&self) -> Result<(), omp_settings::ValidationError> {
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use omp_settings::SettingsSnapshot;

	use super::*;

	#[test]
	fn projection_defaults_enabled() {
		let snapshot =
			SettingsSnapshot::isolated(McpSettings::default(), crate::TEST_SETTINGS_CATALOG)
				.expect("snapshot");
		assert!(
			snapshot
				.project::<McpSettings>()
				.expect("projection")
				.get()
				.enable_project_config
		);
	}
}
