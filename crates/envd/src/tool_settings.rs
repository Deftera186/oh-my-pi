//! Typed settings owned by production tool admission and registry composition.

use std::{collections::BTreeMap, path::PathBuf};

use omp_core::{Duration, Str};
use omp_settings::{FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError};
use omp_tool::Effects;
use omp_tools::edit::FormatPolicy;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Runtime posture for automatic tool-admission decisions.
pub use super::admission::ApprovalMode;
use super::admission::{ApprovalPolicy, ResolvedApproval, resolve_approval};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const APPROVAL_MODES: &[&str] = &["always-ask", "write", "yolo"];

/// Tool exposure, timeout, and approval policy resolved from native settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSettings {
	/// Explicit per-tool enablement overrides; absent names remain enabled.
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub enabled:              BTreeMap<Str, bool>,
	/// Global ceiling for tool deadlines.
	#[serde(skip_serializing_if = "Option::is_none", with = "optional_duration")]
	pub max_timeout:          Option<Duration>,
	/// Optional pinned edit revision (`rep.1` or `hl.1`) for this client.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub edit_dialect:         Option<Str>,
	/// Optional JSONL destination for edit black-box diagnostics.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub edit_blackbox_path:   Option<PathBuf>,
	/// Repair newly introduced syntax parse errors before commit after validated
	/// reparse and non-revert checks.
	pub edit_auto_repair:     bool,
	/// Abort a streaming turn as soon as the edit guard proves it invalid.
	pub edit_streaming_abort: bool,
	/// Permit HTTP(S) URL dispatch from read.
	pub fetch_enabled:        bool,
	/// Convert supported documents to Markdown.
	pub render_markdown:      bool,
	/// Normalize images to model pixel/output bounds.
	pub auto_resize_images:   bool,
	/// Formatter requirement for write/edit transactions.
	pub format_policy:        FormatPolicy,
	/// Capture one final diagnostics batch after write.
	pub diagnostics_on_write: bool,
	/// Capture one final diagnostics batch after edit.
	pub diagnostics_on_edit:  bool,
	/// Collapse identical final diagnostics across server bindings.
	pub diagnostic_dedup:     bool,
	/// Default approval posture, applied after effect-tier resolution.
	pub approval_mode:        ApprovalMode,
	/// Authoritative per-tool approval policy overrides.
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub approval:             BTreeMap<Str, ApprovalPolicy>,
	/// Permit fuzzy edit matching when exact anchors are unavailable.
	pub edit_fuzzy:           bool,
	/// Require files to have been read before mutation.
	pub edit_require_seen:    bool,
	/// Refuse generated-file edits unless explicitly requested.
	pub edit_guard_generated: bool,
	/// Maximum bytes returned by one read call before spill/summarization.
	pub read_max_bytes:       u64,
	/// Summarize supported oversized documents.
	pub read_summarize:       bool,
	/// Include source line numbers in text reads.
	pub read_line_numbers:    bool,
	/// Default context lines around grep matches.
	pub grep_context_lines:   u16,
	/// Named eval interpreter command overrides.
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub eval_interpreters:    BTreeMap<Str, Str>,
	/// Bytes retained inline before tool output spills.
	pub output_spill_bytes:   u64,
	/// Hard byte ceiling for one materialized tool output.
	pub output_max_bytes:     u64,
	/// Include tool-intent decisions in diagnostic tracing.
	pub intent_tracing:       bool,
	/// Maximum repeated equivalent tool calls before the loop guard trips.
	pub loop_guard_limit:     u32,
}

impl Default for ToolSettings {
	fn default() -> Self {
		Self {
			enabled:              BTreeMap::new(),
			max_timeout:          None,
			edit_dialect:         None,
			edit_blackbox_path:   None,
			edit_auto_repair:     false,
			edit_streaming_abort: false,
			fetch_enabled:        true,
			render_markdown:      true,
			auto_resize_images:   true,
			format_policy:        FormatPolicy::BestEffort,
			diagnostics_on_write: true,
			diagnostics_on_edit:  true,
			diagnostic_dedup:     true,
			approval_mode:        ApprovalMode::Yolo,
			approval:             BTreeMap::new(),
			edit_fuzzy:           true,
			edit_require_seen:    true,
			edit_guard_generated: true,
			read_max_bytes:       1024 * 1024,
			read_summarize:       true,
			read_line_numbers:    true,
			grep_context_lines:   2,
			eval_interpreters:    BTreeMap::new(),
			output_spill_bytes:   64 * 1024,
			output_max_bytes:     16 * 1024 * 1024,
			intent_tracing:       false,
			loop_guard_limit:     8,
		}
	}
}

impl ToolSettings {
	/// Returns a session-local copy with an explicit approval-mode override.
	///
	/// The source settings are unchanged, so callers can apply an invocation
	/// override without persisting it.
	#[must_use]
	pub fn with_approval_mode_override(mut self, approval_mode: Option<ApprovalMode>) -> Self {
		if let Some(approval_mode) = approval_mode {
			self.approval_mode = approval_mode;
		}
		self
	}

	/// Whether a named tool is available after applying the default-enabled
	/// rule.
	pub fn enabled(&self, name: &str) -> bool {
		self.enabled.get(name).copied().unwrap_or(true)
	}

	/// Resolves and receipts one invocation against its live declared effects.
	pub fn approval_for(
		&self,
		invocation_id: impl Into<Str>,
		tool_name: impl Into<Str>,
		effects: &Effects,
	) -> ResolvedApproval {
		let tool_name = tool_name.into();
		resolve_approval(
			invocation_id,
			tool_name.clone(),
			effects,
			self.approval_mode,
			self.approval.get(&tool_name).copied(),
		)
	}
}

impl SettingsDomain for ToolSettings {
	const DOMAIN: &'static str = "tools";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "tools.enabled",
			label:       "Enabled tools",
			description: "Per-tool availability overrides.",
			kind:        SettingKind::Table,
			scopes:      PERSISTED,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "tools.max_timeout",
			label:       "Maximum tool timeout",
			description: "Global ceiling for tool execution deadlines.",
			kind:        SettingKind::Duration,
			scopes:      PERSISTED,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "tools.edit_dialect",
			label:       "Edit dialect",
			description: "Pinned edit tool revision.",
			kind:        SettingKind::String,
			scopes:      PERSISTED,
			order:       30,
			options:     None,
			condition:   None,
			secret:      false,
		},
		field(
			"tools.edit_blackbox_path",
			"Edit Black-box Path",
			"Optional JSONL destination for edit black-box diagnostics.",
			SettingKind::Path,
			31,
		),
		field(
			"tools.edit_auto_repair",
			"Edit Auto-repair",
			"Repair newly introduced syntax parse errors before commit after validated reparse and \
			 non-revert checks (up to two attempts).",
			SettingKind::Boolean,
			32,
		),
		field(
			"tools.edit_streaming_abort",
			"Streaming Edit Abort",
			"Abort a turn as soon as streamed edit validation fails.",
			SettingKind::Boolean,
			33,
		),
		FieldDescriptor {
			path:        "tools.approval_mode",
			label:       "Tool approval",
			description: "Default effect tier approved without confirmation.",
			kind:        SettingKind::Enum(APPROVAL_MODES),
			scopes:      PERSISTED,
			order:       40,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "tools.approval",
			label:       "Tool approval policies",
			description: "Per-tool allow, prompt, or deny overrides.",
			kind:        SettingKind::Table,
			scopes:      PERSISTED,
			order:       50,
			options:     None,
			condition:   None,
			secret:      false,
		},
		field(
			"tools.edit_fuzzy",
			"Fuzzy Edit",
			"Permit fuzzy edit anchor matching.",
			SettingKind::Boolean,
			60,
		),
		field(
			"tools.edit_require_seen",
			"Seen-line Edit Guard",
			"Require a prior read before mutation.",
			SettingKind::Boolean,
			70,
		),
		field(
			"tools.edit_guard_generated",
			"Generated-file Guard",
			"Refuse incidental generated-file edits.",
			SettingKind::Boolean,
			80,
		),
		field(
			"tools.read_max_bytes",
			"Read Byte Limit",
			"Maximum bytes returned by one read call.",
			SettingKind::Integer,
			90,
		),
		field(
			"tools.read_summarize",
			"Read Summarization",
			"Summarize supported oversized documents.",
			SettingKind::Boolean,
			100,
		),
		field(
			"tools.read_line_numbers",
			"Read Line Numbers",
			"Include line numbers in text reads.",
			SettingKind::Boolean,
			110,
		),
		field(
			"tools.grep_context_lines",
			"Grep Context",
			"Default context lines around matches.",
			SettingKind::Integer,
			140,
		),
		field(
			"tools.eval_interpreters",
			"Eval Interpreters",
			"Named eval interpreter overrides.",
			SettingKind::Table,
			150,
		),
		field(
			"tools.output_spill_bytes",
			"Output Spill Threshold",
			"Bytes retained inline before spill.",
			SettingKind::Integer,
			160,
		),
		field(
			"tools.output_max_bytes",
			"Output Byte Limit",
			"Hard materialized output ceiling.",
			SettingKind::Integer,
			170,
		),
		field(
			"tools.intent_tracing",
			"Intent Tracing",
			"Trace tool intent decisions.",
			SettingKind::Boolean,
			200,
		),
		field(
			"tools.loop_guard_limit",
			"Loop Guard",
			"Repeated equivalent calls before interruption.",
			SettingKind::Integer,
			210,
		),
	];

	fn validate(&self) -> Result<(), ValidationError> {
		if self
			.approval
			.keys()
			.chain(self.enabled.keys())
			.any(|name| name.trim().is_empty())
			|| self.read_max_bytes == 0
			|| self.output_spill_bytes == 0
			|| self.output_max_bytes < self.output_spill_bytes
			|| self.loop_guard_limit == 0
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
	kind: SettingKind,
	order: u16,
) -> FieldDescriptor {
	FieldDescriptor {
		path,
		label,
		description,
		kind,
		scopes: PERSISTED,
		order,
		options: None,
		condition: None,
		secret: false,
	}
}

mod optional_duration {

	use super::*;

	pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		match value {
			Some(value) => serializer.serialize_some(&value.to_string()),
			None => serializer.serialize_none(),
		}
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
	where
		D: Deserializer<'de>,
	{
		Option::<String>::deserialize(deserializer)?
			.map(|value| value.parse().map_err(de::Error::custom))
			.transpose()
	}
}

#[cfg(test)]
mod tests {
	use omp_core::sf;
	use omp_settings::SettingsSnapshot;
	use omp_tool::{Effects, ExecEffects};

	use super::*;
	use crate::admission::{ApprovalPolicy, ApprovalSource, ApprovalTier};

	#[test]
	fn typed_projection_round_trips() {
		let expected = ToolSettings {
			approval_mode: ApprovalMode::Write,
			approval: BTreeMap::from([(sf!("bash"), ApprovalPolicy::Deny)]),
			..ToolSettings::default()
		};
		let snapshot = SettingsSnapshot::isolated(expected.clone(), crate::TEST_SETTINGS_CATALOG)
			.expect("isolated settings");
		let projected = snapshot
			.project::<ToolSettings>()
			.expect("typed projection");
		assert_eq!(projected.get(), &expected);
	}

	#[test]
	fn override_is_applied_to_declared_effect_tier() {
		let settings = ToolSettings {
			approval: BTreeMap::from([(sf!("bash"), ApprovalPolicy::Deny)]),
			..ToolSettings::default()
		};
		let effects = Effects {
			exec: Some(ExecEffects { commands: [sf!("*")].into(), network: true }),
			..Effects::empty()
		};
		let decision = settings.approval_for("call-1", "bash", &effects);
		assert_eq!(decision.tier, ApprovalTier::Exec);
		assert_eq!(decision.policy, ApprovalPolicy::Deny);
		assert_eq!(decision.source, ApprovalSource::User);
	}

	#[test]
	fn approval_mode_override_is_session_local_and_precedes_persisted_mode() {
		let persisted =
			ToolSettings { approval_mode: ApprovalMode::AlwaysAsk, ..ToolSettings::default() };
		let effects = Effects {
			exec: Some(ExecEffects { commands: [sf!("*")].into(), network: true }),
			..Effects::empty()
		};

		let overridden = persisted
			.clone()
			.with_approval_mode_override(Some(ApprovalMode::Yolo));
		assert_eq!(
			overridden.approval_for("override", "bash", &effects).policy,
			ApprovalPolicy::Allow
		);
		assert_eq!(
			persisted.approval_for("persisted", "bash", &effects).policy,
			ApprovalPolicy::Prompt
		);

		let unchanged = persisted.clone().with_approval_mode_override(None);
		assert_eq!(unchanged, persisted);
	}

	#[test]
	fn empty_override_key_is_rejected() {
		let settings = ToolSettings {
			approval: BTreeMap::from([(Str::default(), ApprovalPolicy::Prompt)]),
			..ToolSettings::default()
		};
		assert!(settings.validate().is_err());
	}
}
