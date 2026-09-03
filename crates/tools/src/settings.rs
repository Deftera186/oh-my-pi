//! Typed convars owned by the file-tool runtime.

use omp_con::Ctx;
use serde::{Deserialize, Serialize};

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

impl FetchSettings {
	/// Projects the current fetch policy from the control plane.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self { enabled: SV_FETCH_ENABLED.get(ctx) }
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

impl ImageSettings {
	/// Projects the current image policy from the control plane.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self { auto_resize: SV_IMAGES_AUTO_RESIZE.get(ctx) }
	}
}

/// Text presentation policy applied by read.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReadSettings {
	/// Whether Markdown reads carry rendered-Markdown presentation metadata.
	pub render_markdown: bool,
}

impl ReadSettings {
	/// Projects the current read presentation policy from the control plane.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self { render_markdown: CL_READ_RENDER_MARKDOWN.get(ctx) }
	}
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

impl LspFileSettings {
	/// Projects the current LSP file policy from the control plane.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self {
			format_on_write:              SV_LSP_FORMAT_ON_WRITE.get(ctx),
			diagnostics_on_write:         SV_LSP_DIAGNOSTICS_ON_WRITE.get(ctx),
			diagnostics_on_edit:          SV_LSP_DIAGNOSTICS_ON_EDIT.get(ctx),
			diagnostics_deduplicate:      SV_LSP_DIAGNOSTICS_DEDUPLICATE.get(ctx),
			diagnostics_history_capacity: SV_LSP_DIAGNOSTICS_HISTORY_CAPACITY.get(ctx) as usize,
			max_diagnostics_per_batch:    SV_LSP_MAX_DIAGNOSTICS_PER_BATCH.get(ctx) as usize,
		}
	}

	/// Reports whether all LSP policy bounds hold.
	#[must_use]
	pub fn validate(&self) -> bool {
		(1..=MAX_DIAGNOSTIC_HISTORY_CAPACITY).contains(&self.diagnostics_history_capacity)
			&& (1..=MAX_DIAGNOSTICS_PER_BATCH).contains(&self.max_diagnostics_per_batch)
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

impl FileToolSettings {
	/// Projects all file-tool policy from the control plane.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self {
			fetch:  FetchSettings::from_con(ctx),
			images: ImageSettings::from_con(ctx),
			read:   ReadSettings::from_con(ctx),
			lsp:    LspFileSettings::from_con(ctx),
		}
	}
}

omp_con::var! {
	/// Allow read to fetch HTTP(S) resources.
	pub static SV_FETCH_ENABLED = sv_fetch_enabled: bool {
		default: true,
		flags: archive | inherit | replicated,
	};
	/// Resize oversized images before model delivery.
	pub static SV_IMAGES_AUTO_RESIZE = sv_images_auto_resize: bool {
		default: true,
		flags: archive | session | inherit | replicated,
	};
	/// Present Markdown reads as rendered Markdown.
	pub static CL_READ_RENDER_MARKDOWN = cl_read_render_markdown: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Format supported documents after a whole-file write.
	pub static SV_LSP_FORMAT_ON_WRITE = sv_lsp_format_on_write: bool {
		default: false,
		flags: archive | inherit | replicated,
	};
	/// Return diagnostics bound to the committed write revision.
	pub static SV_LSP_DIAGNOSTICS_ON_WRITE = sv_lsp_diagnostics_on_write: bool {
		default: true,
		flags: archive | inherit | replicated,
	};
	/// Return diagnostics bound to the committed edit revision.
	pub static SV_LSP_DIAGNOSTICS_ON_EDIT = sv_lsp_diagnostics_on_edit: bool {
		default: false,
		flags: archive | inherit | replicated,
	};
	/// Suppress diagnostics already surfaced for the same file.
	pub static SV_LSP_DIAGNOSTICS_DEDUPLICATE = sv_lsp_diagnostics_deduplicate: bool {
		default: true,
		flags: archive | inherit | replicated,
	};
	/// Bound the per-runtime diagnostic identity history.
	pub static SV_LSP_DIAGNOSTICS_HISTORY_CAPACITY = sv_lsp_diagnostics_history_capacity: u32 {
		default: DEFAULT_DIAGNOSTIC_HISTORY_CAPACITY as u32,
		min: 1,
		max: MAX_DIAGNOSTIC_HISTORY_CAPACITY as u32,
		flags: archive | inherit | replicated,
	};
	/// Bound diagnostics attached to one committed revision.
	pub static SV_LSP_MAX_DIAGNOSTICS_PER_BATCH = sv_lsp_max_diagnostics_per_batch: u32 {
		default: DEFAULT_DIAGNOSTICS_PER_BATCH as u32,
		min: 1,
		max: MAX_DIAGNOSTICS_PER_BATCH as u32,
		flags: archive | inherit | replicated,
	};
}

/// One-shot migration map from reflected TOML paths to convar names.
pub const LEGACY_CONVAR_MAPPINGS: &[(&str, &str)] = &[
	("fetch.enabled", "sv_fetch_enabled"),
	("images.autoResize", "sv_images_auto_resize"),
	("read.renderMarkdown", "cl_read_render_markdown"),
	("lsp.formatOnWrite", "sv_lsp_format_on_write"),
	("lsp.diagnosticsOnWrite", "sv_lsp_diagnostics_on_write"),
	("lsp.diagnosticsOnEdit", "sv_lsp_diagnostics_on_edit"),
	("lsp.diagnosticsDeduplicate", "sv_lsp_diagnostics_deduplicate"),
	("lsp.diagnosticsHistoryCapacity", "sv_lsp_diagnostics_history_capacity"),
	("lsp.maxDiagnosticsPerBatch", "sv_lsp_max_diagnostics_per_batch"),
];

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn projects_defaults_and_ctx_override() {
		let ctx = Ctx::new();
		SV_FETCH_ENABLED.set(&ctx, false).expect("set fetch policy");
		let projection = FileToolSettings::from_con(&ctx);
		assert!(!projection.fetch.enabled);
		assert!(projection.lsp.validate());
	}

	#[test]
	fn rejects_unbounded_diagnostic_policy() {
		let settings =
			LspFileSettings { diagnostics_history_capacity: 0, ..LspFileSettings::default() };
		assert!(!settings.validate());
	}

	#[test]
	fn vars_declare_every_former_schema_field() {
		let old_fields = [
			"fetch.enabled",
			"images.autoResize",
			"read.renderMarkdown",
			"lsp.formatOnWrite",
			"lsp.diagnosticsOnWrite",
			"lsp.diagnosticsOnEdit",
			"lsp.diagnosticsDeduplicate",
			"lsp.diagnosticsHistoryCapacity",
			"lsp.maxDiagnosticsPerBatch",
		];
		let vars = [
			SV_FETCH_ENABLED.name(),
			SV_IMAGES_AUTO_RESIZE.name(),
			CL_READ_RENDER_MARKDOWN.name(),
			SV_LSP_FORMAT_ON_WRITE.name(),
			SV_LSP_DIAGNOSTICS_ON_WRITE.name(),
			SV_LSP_DIAGNOSTICS_ON_EDIT.name(),
			SV_LSP_DIAGNOSTICS_DEDUPLICATE.name(),
			SV_LSP_DIAGNOSTICS_HISTORY_CAPACITY.name(),
			SV_LSP_MAX_DIAGNOSTICS_PER_BATCH.name(),
		];
		assert_eq!(
			LEGACY_CONVAR_MAPPINGS,
			old_fields
				.into_iter()
				.zip(vars)
				.collect::<Vec<_>>()
				.as_slice()
		);
	}
}
