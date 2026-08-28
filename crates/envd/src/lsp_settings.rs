//! Typed settings owned by the project LSP runtime.

use std::path::Path;

use omp_settings::{
	FieldDescriptor, SettingKind, SettingScope, SettingsCatalog, SettingsDomain,
	manager::{SettingsManager, SettingsManagerError, SettingsPaths},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

/// Layered language-server enablement and mutation feedback policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct LspSettings {
	/// Enables language-server bindings and the model-facing LSP surface.
	pub enabled:                 bool,
	/// Defers language-server startup until the first matching operation.
	pub lazy:                    bool,
	/// Formats supported files after a write transaction.
	pub format_on_write:         bool,
	/// Returns diagnostics after write transactions.
	pub diagnostics_on_write:    bool,
	/// Returns diagnostics after edit transactions.
	pub diagnostics_on_edit:     bool,
	/// Suppresses unchanged diagnostics already surfaced in this session.
	pub diagnostics_deduplicate: bool,
}

impl Default for LspSettings {
	fn default() -> Self {
		Self {
			enabled:                 true,
			lazy:                    true,
			format_on_write:         false,
			diagnostics_on_write:    true,
			diagnostics_on_edit:     false,
			diagnostics_deduplicate: true,
		}
	}
}

impl SettingsDomain for LspSettings {
	const DOMAIN: &'static str = "lsp";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "lsp.enabled",
			label:       "Language Servers",
			description: "Enable project language-server bindings and tools.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "lsp.lazy",
			label:       "Lazy LSP Startup",
			description: "Start matching language servers on first use.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "lsp.formatOnWrite",
			label:       "Format on Write",
			description: "Format supported files after write transactions.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       30,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "lsp.diagnosticsOnWrite",
			label:       "Diagnostics on Write",
			description: "Return language-server diagnostics after writes.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       40,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "lsp.diagnosticsOnEdit",
			label:       "Diagnostics on Edit",
			description: "Return language-server diagnostics after edits.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       50,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "lsp.diagnosticsDeduplicate",
			label:       "Deduplicate Diagnostics",
			description: "Suppress unchanged diagnostics already shown for a file.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       60,
			options:     None,
			condition:   None,
			secret:      false,
		},
	];
}

/// Loading the immutable LSP projection from native settings failed.
#[derive(Debug, Error)]
pub enum LspSettingsError {
	/// Native settings layers could not be read or composed.
	#[error(transparent)]
	Manager(#[from] SettingsManagerError),
	/// The layered LSP table did not decode as its owning Rust type.
	#[error(transparent)]
	Projection(#[from] omp_settings::SnapshotError),
}

/// Loads one immutable global/project/overlay LSP policy snapshot.
pub fn load(
	data_dir: &Path,
	project_root: &Path,
	catalog: SettingsCatalog,
) -> Result<LspSettings, LspSettingsError> {
	let manager = SettingsManager::open_read_only(
		SettingsPaths::discover(data_dir, Some(project_root)),
		catalog,
	)?;
	Ok(manager.snapshot().project::<LspSettings>()?.get().clone())
}

#[cfg(test)]
mod tests {
	use omp_settings::SettingsSnapshot;

	use super::*;

	#[test]
	fn defaults_match_pi_policy_without_shared_toggle() {
		assert_eq!(LspSettings::default(), LspSettings {
			enabled:                 true,
			lazy:                    true,
			format_on_write:         false,
			diagnostics_on_write:    true,
			diagnostics_on_edit:     false,
			diagnostics_deduplicate: true,
		});
		assert!(
			!LspSettings::FIELDS
				.iter()
				.any(|field| field.path == "lsp.shared")
		);
	}

	#[test]
	fn isolated_projection_is_typed() {
		let expected =
			LspSettings { enabled: false, diagnostics_on_edit: true, ..LspSettings::default() };
		let snapshot = SettingsSnapshot::isolated(expected.clone(), crate::TEST_SETTINGS_CATALOG)
			.expect("isolated LSP snapshot");
		assert_eq!(
			snapshot
				.project::<LspSettings>()
				.expect("LSP projection")
				.get(),
			&expected
		);
	}

	#[test]
	fn removed_shared_toggle_is_rejected_by_the_live_domain() {
		let document = toml::toml! {
			[lsp]
			shared = true
		};
		let snapshot = SettingsSnapshot::read_only(document, crate::TEST_SETTINGS_CATALOG);
		assert!(snapshot.project::<LspSettings>().is_err());
	}
}
