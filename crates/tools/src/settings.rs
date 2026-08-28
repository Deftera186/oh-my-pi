//! Typed settings owned by the file-tool runtime.

use omp_settings::{
	DomainRegistration, FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError,
};
use serde::{Deserialize, Serialize};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
/// Scope set matching the app root `images.*` registration
/// (runtime-overridable).
const PERSISTED_RUNTIME: &[SettingScope] =
	&[SettingScope::Global, SettingScope::Project, SettingScope::Runtime];
/// Default number of prior diagnostic identities retained for deduplication.
pub const DEFAULT_DIAGNOSTIC_HISTORY_CAPACITY: usize = 1_024;
/// Default maximum diagnostics retained in one committed batch.
pub const DEFAULT_DIAGNOSTICS_PER_BATCH: usize = 256;
/// Hard upper bound for the diagnostic identity ledger.
pub const MAX_DIAGNOSTIC_HISTORY_CAPACITY: usize = 16_384;
/// Hard upper bound for one committed diagnostic batch.
pub const MAX_DIAGNOSTICS_PER_BATCH: usize = 4_096;

/// URL-fetch policy applied before read dispatch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct FetchSettings {
	/// Whether read may perform HTTP(S) fetches.
	pub enabled: bool,
}

impl Default for FetchSettings {
	fn default() -> Self {
		Self { enabled: true }
	}
}

/// Image handling policy applied by read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImageSettings {
	/// Whether oversized images are decoded and resized for model compatibility.
	pub auto_resize: bool,
}

impl Default for ImageSettings {
	fn default() -> Self {
		Self { auto_resize: true }
	}
}

/// Text presentation policy applied by read.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReadSettings {
	/// Whether Markdown reads carry rendered-Markdown presentation metadata.
	pub render_markdown: bool,
}

/// LSP policy captured once for a file-tool invocation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LspFileSettings {
	/// Whether whole-file writes request formatter execution.
	pub format_on_write:              bool,
	/// Whether whole-file writes request revision-bound diagnostics.
	pub diagnostics_on_write:         bool,
	/// Whether edit transactions request revision-bound diagnostics.
	pub diagnostics_on_edit:          bool,
	/// Whether diagnostics already surfaced for a file are suppressed.
	pub diagnostics_deduplicate:      bool,
	/// Maximum prior diagnostic identities retained by the deduplication ledger.
	pub diagnostics_history_capacity: usize,
	/// Maximum diagnostics retained in one committed batch.
	pub max_diagnostics_per_batch:    usize,
}

impl Default for LspFileSettings {
	fn default() -> Self {
		Self {
			format_on_write:              false,
			diagnostics_on_write:         true,
			diagnostics_on_edit:          false,
			diagnostics_deduplicate:      true,
			diagnostics_history_capacity: DEFAULT_DIAGNOSTIC_HISTORY_CAPACITY,
			max_diagnostics_per_batch:    DEFAULT_DIAGNOSTICS_PER_BATCH,
		}
	}
}

/// Complete immutable file-tool policy projection.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct FileToolSettings {
	/// Fetch settings.
	pub fetch:  FetchSettings,
	/// Image settings.
	pub images: ImageSettings,
	/// Read presentation settings.
	pub read:   ReadSettings,
	/// LSP mutation settings.
	pub lsp:    LspFileSettings,
}

impl SettingsDomain for FetchSettings {
	const DOMAIN: &'static str = "fetch";
	const FIELDS: &'static [FieldDescriptor] =
		&[field("fetch.enabled", "Read URLs", "Allow read to fetch HTTP(S) resources.", 10)];
}

impl SettingsDomain for ImageSettings {
	const DOMAIN: &'static str = "images";
	const FIELDS: &'static [FieldDescriptor] = &[FieldDescriptor {
		scopes: PERSISTED_RUNTIME,
		..field(
			"images.autoResize",
			"Auto-resize images",
			"Resize oversized images before model delivery.",
			10,
		)
	}];
}

impl SettingsDomain for ReadSettings {
	const DOMAIN: &'static str = "read";
	const FIELDS: &'static [FieldDescriptor] = &[field(
		"read.renderMarkdown",
		"Render Markdown",
		"Present Markdown reads as rendered Markdown.",
		10,
	)];
}

impl SettingsDomain for LspFileSettings {
	const DOMAIN: &'static str = "lsp";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"lsp.formatOnWrite",
			"Format on write",
			"Format supported documents after a whole-file write.",
			10,
		),
		field(
			"lsp.diagnosticsOnWrite",
			"Diagnostics on write",
			"Return diagnostics bound to the committed write revision.",
			20,
		),
		field(
			"lsp.diagnosticsOnEdit",
			"Diagnostics on edit",
			"Return diagnostics bound to the committed edit revision.",
			30,
		),
		field(
			"lsp.diagnosticsDeduplicate",
			"Deduplicate diagnostics",
			"Suppress diagnostics already surfaced for the same file.",
			40,
		),
		integer_field(
			"lsp.diagnosticsHistoryCapacity",
			"Diagnostic history capacity",
			"Bound the per-runtime diagnostic identity history.",
			50,
		),
		integer_field(
			"lsp.maxDiagnosticsPerBatch",
			"Maximum diagnostics per batch",
			"Bound diagnostics attached to one committed revision.",
			60,
		),
	];

	fn validate(&self) -> Result<(), ValidationError> {
		if !(1..=MAX_DIAGNOSTIC_HISTORY_CAPACITY).contains(&self.diagnostics_history_capacity)
			|| !(1..=MAX_DIAGNOSTICS_PER_BATCH).contains(&self.max_diagnostics_per_batch)
		{
			return Err(ValidationError::DomainInvariant { domain: Self::DOMAIN });
		}
		Ok(())
	}
}

const fn field(
	path: &'static str,
	label: &'static str,
	description: &'static str,
	order: u16,
) -> FieldDescriptor {
	FieldDescriptor {
		path,
		label,
		description,
		kind: SettingKind::Boolean,
		scopes: PERSISTED,
		order,
		options: None,
		condition: None,
		secret: false,
	}
}

const fn integer_field(
	path: &'static str,
	label: &'static str,
	description: &'static str,
	order: u16,
) -> FieldDescriptor {
	FieldDescriptor { kind: SettingKind::Integer, ..field(path, label, description, order) }
}

/// Settings domains owned by the tools crate.
pub const SETTINGS_CONTRIBUTION: omp_settings::SettingsContribution =
	omp_settings::SettingsContribution {
		domains:     &[
			DomainRegistration::of::<FetchSettings>(),
			DomainRegistration::of::<ImageSettings>(),
			DomainRegistration::of::<ReadSettings>(),
			DomainRegistration::of::<LspFileSettings>(),
		],
		normalizers: &[],
	};

#[cfg(test)]
mod tests {
	use omp_settings::{SettingsCatalog, SettingsSnapshot};

	use super::*;

	const CATALOG: SettingsCatalog =
		SettingsCatalog::new(&[&omp_settings::SETTINGS_CONTRIBUTION, &crate::SETTINGS_CONTRIBUTION]);

	#[test]
	fn projects_defaults_and_links_registration() {
		let snapshot =
			SettingsSnapshot::isolated(FetchSettings::default(), CATALOG).expect("snapshot");
		let projection = snapshot.project::<FetchSettings>().expect("projection");
		assert!(projection.get().enabled);
		assert_eq!(
			SETTINGS_CONTRIBUTION
				.domains
				.iter()
				.map(|domain| domain.descriptor().name)
				.collect::<Vec<_>>(),
			["fetch", "images", "read", "lsp"],
		);
	}

	#[test]
	fn rejects_unbounded_diagnostic_policy() {
		let settings =
			LspFileSettings { diagnostics_history_capacity: 0, ..LspFileSettings::default() };
		assert!(settings.validate().is_err());
	}
}
