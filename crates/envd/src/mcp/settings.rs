//! Typed settings owned by the Environment MCP runtime.

use omp_con::Ctx;
use serde::{Deserialize, Serialize};

omp_con::var! {
	/// Load native project MCP server configuration.
	pub static SV_MCP_ENABLE_PROJECT_CONFIG = sv_mcp_enable_project_config: bool {
		default: true,
		flags: archive | inherit,
	};
}

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

impl McpSettings {
	/// Resolves MCP discovery policy from the process control context.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self { enable_project_config: SV_MCP_ENABLE_PROJECT_CONFIG.get(ctx) }
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn con_defaults_enabled() {
		assert!(McpSettings::from_con(&Ctx::new()).enable_project_config);
	}
}
