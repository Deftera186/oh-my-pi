//! Small convar projections used by surviving standalone driver services.

use std::{
	fs, io,
	path::{Path, PathBuf},
};

use omp_con::Ctx;
use omp_core::Str;
pub use omp_envd::host_settings::SV_WORKTREE_BASE;
use serde::{Deserialize, Serialize};

/// One-shot legacy TOML paths mapped into surviving convars.
pub const LEGACY_CONVAR_MAPPINGS: &[(&str, &str)] = &[
	("worktree.base", "sv_worktree_base"),
	("share.serverUrl", "sv_share_server"),
	("share.store", "sv_share_store"),
	("export.shareRedactSecrets", "sv_share_redact_secrets"),
	("collab.displayName", "cl_collab_display_name"),
];

/// Standalone sharing backend.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Deserialize,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
	strum::VariantNames,
)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum ShareStore {
	/// Encrypted HTTP object store.
	#[default]
	Http,
	/// Authenticated GitHub gist.
	Gist,
}
omp_con::con_enum!(ShareStore);

omp_con::var! {
	/// Share service origin.
	pub static SV_SHARE_SERVER = sv_share_server: Str {
		default: Str::new_static("https://share.omp.dev"),
		flags: archive,
	};
	/// Share storage backend.
	pub static SV_SHARE_STORE = sv_share_store: ShareStore {
		default: ShareStore::Http,
		flags: archive,
	};
	/// Redact discovered secrets before encryption.
	pub static SV_SHARE_REDACT_SECRETS = sv_share_redact_secrets: bool {
		default: true,
		flags: archive,
	};
	/// Collaboration display name.
	pub static CL_COLLAB_DISPLAY_NAME = cl_collab_display_name: Str {
		default: Str::default(),
		flags: archive,
	};
}

/// Share redaction projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportSettings {
	/// Remove secret values before sealing.
	pub share_redact_secrets: bool,
}

/// Share transport projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShareSettings {
	/// Service endpoint.
	pub server_url: Str,
	/// Storage backend.
	pub store:      ShareStore,
}

/// Collaboration identity projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollabSettings {
	display_name: Str,
}

impl CollabSettings {
	/// Returns the configured name, or the local login name.
	#[must_use]
	pub fn resolved_display_name(&self) -> Str {
		if self.display_name.is_empty() {
			std::env::var("USER").map_or_else(|_| Str::new_static("guest"), Str::new)
		} else {
			self.display_name.clone()
		}
	}
}

/// Environment-owned worktree placement.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorktreeSettings {
	/// Optional base directory.
	pub base: Option<PathBuf>,
}

/// Complete projection required by standalone commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settings {
	/// Collaboration settings.
	pub collab:   CollabSettings,
	/// Worktree placement.
	pub worktree: WorktreeSettings,
	/// Share redaction policy.
	pub export:   ExportSettings,
	/// Share transport policy.
	pub share:    ShareSettings,
}

impl Settings {
	/// Projects settings from the effective convar context.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		let worktree = {
			let value = SV_WORKTREE_BASE.get(ctx);
			(!value.is_empty()).then(|| PathBuf::from(value.as_str()))
		};
		Self {
			collab:   CollabSettings { display_name: CL_COLLAB_DISPLAY_NAME.get(ctx) },
			worktree: WorktreeSettings { base: worktree },
			export:   ExportSettings { share_redact_secrets: SV_SHARE_REDACT_SECRETS.get(ctx) },
			share:    ShareSettings {
				server_url: SV_SHARE_SERVER.get(ctx),
				store:      SV_SHARE_STORE.get(ctx),
			},
		}
	}

	/// Resolves extension overlays. Extension configuration itself owns the
	/// overlay schema; this projection contributes no hidden settings layer.
	pub fn extension_scopes(
		&self,
		overlays: Vec<omp_ext::config::ScopedOverlay>,
	) -> Result<Vec<omp_ext::config::ScopedOverlay>, io::Error> {
		Ok(overlays)
	}
}

/// Loads the user `config.cfg` into a convar context and projects settings.
pub fn current(data_dir: &Path) -> Result<Settings, io::Error> {
	let ctx = Ctx::new();
	let path = data_dir.join("config.cfg");
	if path.exists() {
		let source = fs::read_to_string(path)?;
		ctx.run(&source).map_err(io::Error::other)?;
	}
	Ok(Settings::from_con(&ctx))
}

/// Loads settings for one project, with project cfg layered after user cfg.
pub fn current_for_project(data_dir: &Path, project: &Path) -> Result<Settings, io::Error> {
	let ctx = Ctx::new();
	for path in [data_dir.join("config.cfg"), project.join(".omp/config.cfg")] {
		if path.exists() {
			ctx.run(&fs::read_to_string(path)?)
				.map_err(io::Error::other)?;
		}
	}
	Ok(Settings::from_con(&ctx))
}

/// Reads the workspace extension overlay without introducing a settings schema.
pub fn workspace_extension_overlay(
	_project: &Path,
) -> Result<Vec<omp_ext::config::ScopedOverlay>, io::Error> {
	Ok(Vec::new())
}
