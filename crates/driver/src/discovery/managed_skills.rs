//! Dead-last discovery source for isolated model-generated skills.

use std::path::{Path, PathBuf};

use super::{
	manifest::SourceScope,
	skills::{SkillDiscovery, SkillDiscoverySettings, SkillSource, SkillSourceKind, discover},
};

/// Returns the isolated managed-skills directory beneath the active agent root.
pub fn root(agent_root: &Path) -> PathBuf {
	agent_root.join("managed-skills")
}

use std::fs;

/// Discovers generated skills independently of autolearn enablement.
///
/// Callers append this provider after every authored source. The final
/// first-wins merge therefore makes generated skills dead-last without giving
/// the managed directory any way to override authored content.
pub fn discover_dead_last(agent_root: &Path, settings: &SkillDiscoverySettings) -> SkillDiscovery {
	let managed_root = root(agent_root);
	let Ok(metadata) = fs::symlink_metadata(&managed_root) else {
		return SkillDiscovery::default();
	};
	if metadata.file_type().is_symlink() || !metadata.is_dir() {
		return SkillDiscovery::default();
	}
	discover(
		&[SkillSource {
			id:                  omp_envd::managed_skills_domain::PROVIDER_ID.into(),
			root:                managed_root.clone(),
			scope:               SourceScope::User,
			include_root:        false,
			require_description: true,
			contain_root:        Some(managed_root),
			read_only:           false,
			kind:                SkillSourceKind::Native,
		}],
		settings,
	)
}
