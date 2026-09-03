//! Driver-owned optional capabilities injected into the environment host.

use std::{path::Path, sync::Arc};

/// Inference service binding retained by compositions that enable search.
#[derive(Default)]
pub struct InferenceBridge;

/// Session goal-control binding.
#[derive(Clone, Default)]
pub struct AgentGoalControl;

/// Builds the baseline environment bridges for one project.
///
/// Core tools, Python registrations, and session routing are installed by the
/// environment and kernel composition directly; this helper carries only the
/// optional host-resource authority.
#[must_use]
pub fn builtin(
	_root: &Path,
	_search: Arc<InferenceBridge>,
	_goal_control: AgentGoalControl,
	host_resources: Option<Arc<dyn omp_envd::HostResources>>,
) -> omp_envd::RegistryBridges {
	omp_envd::RegistryBridges { host_resources, ..omp_envd::RegistryBridges::default() }
}
