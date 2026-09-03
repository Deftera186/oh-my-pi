//! Typed settings owned by production tool admission and registry composition.

use std::{collections::BTreeMap, path::PathBuf};

use omp_con::{Ctx, Kv, Span, Value};
use omp_core::{Duration, Str};
use omp_tool::Effects;
use omp_tools::edit::FormatPolicy;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Runtime posture for automatic tool-admission decisions.
pub use super::admission::ApprovalMode;
use super::admission::{ApprovalPolicy, ResolvedApproval, resolve_approval};

omp_con::con_enum!(ApprovalMode);

omp_con::var! {
	/// Per-tool availability overrides.
	pub static SV_TOOLS_ENABLED = sv_tools_enabled: Kv {
		default: Kv::new(),
		validate: |_ctx, values| validate_bool_map(values),
		flags: archive | inherit,
	};
	/// Global ceiling for tool execution deadlines.
	pub static SV_TOOLS_MAX_TIMEOUT = sv_tools_max_timeout: Span {
		default: Span::NEVER,
		flags: archive | inherit,
	};
	/// Pinned edit tool revision; empty selects the route default.
	pub static SV_TOOLS_EDIT_DIALECT = sv_tools_edit_dialect: Str {
		default: Str::default(),
		flags: archive | inherit,
	};
	/// Optional JSONL destination for edit black-box diagnostics.
	pub static SV_TOOLS_EDIT_BLACKBOX_PATH = sv_tools_edit_blackbox_path: Str {
		default: Str::default(),
		flags: archive | inherit,
	};
	/// Repair newly introduced syntax parse errors before commit.
	pub static SV_TOOLS_EDIT_AUTO_REPAIR = sv_tools_edit_auto_repair: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Abort a turn as soon as streamed edit validation fails.
	pub static SV_TOOLS_EDIT_STREAMING_ABORT = sv_tools_edit_streaming_abort: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Default effect tier approved without confirmation.
	pub static SV_TOOLS_APPROVAL_MODE = sv_tools_approval_mode: ApprovalMode {
		default: ApprovalMode::Yolo,
		flags: archive | inherit,
	};
	/// Per-tool allow, prompt, or deny overrides.
	pub static SV_TOOLS_APPROVAL = sv_tools_approval: Kv {
		default: Kv::new(),
		validate: |_ctx, values| validate_approval_map(values),
		flags: archive | inherit,
	};
	/// Permit fuzzy edit anchor matching.
	pub static SV_TOOLS_EDIT_FUZZY = sv_tools_edit_fuzzy: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Require a prior read before mutation.
	pub static SV_TOOLS_EDIT_REQUIRE_SEEN = sv_tools_edit_require_seen: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Refuse incidental generated-file edits.
	pub static SV_TOOLS_EDIT_GUARD_GENERATED = sv_tools_edit_guard_generated: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Maximum bytes returned by one read call.
	pub static SV_TOOLS_READ_MAX_BYTES = sv_tools_read_max_bytes: i64 {
		default: 1024 * 1024,
		min: 1,
		flags: archive | inherit,
	};
	/// Summarize supported oversized documents.
	pub static SV_TOOLS_READ_SUMMARIZE = sv_tools_read_summarize: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Include line numbers in text reads.
	pub static SV_TOOLS_READ_LINE_NUMBERS = sv_tools_read_line_numbers: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Default context lines around grep matches.
	pub static SV_TOOLS_GREP_CONTEXT_LINES = sv_tools_grep_context_lines: u16 {
		default: 2,
		flags: archive | inherit,
	};
	/// Named eval interpreter command overrides.
	pub static SV_TOOLS_EVAL_INTERPRETERS = sv_tools_eval_interpreters: Kv {
		default: Kv::new(),
		validate: |_ctx, values| validate_string_map(values),
		flags: archive | inherit,
	};
	/// Bytes retained inline before tool output spills.
	pub static SV_TOOLS_OUTPUT_SPILL_BYTES = sv_tools_output_spill_bytes: i64 {
		default: 64 * 1024,
		min: 1,
		validate: |ctx, value| {
			if *value <= SV_TOOLS_OUTPUT_MAX_BYTES.get(ctx) {
				Ok(())
			} else {
				Err(Str::new_static("spill threshold exceeds output ceiling"))
			}
		},
		flags: archive | inherit,
	};
	/// Hard byte ceiling for one materialized tool output.
	pub static SV_TOOLS_OUTPUT_MAX_BYTES = sv_tools_output_max_bytes: i64 {
		default: 16 * 1024 * 1024,
		min: 1,
		validate: |ctx, value| {
			if *value >= SV_TOOLS_OUTPUT_SPILL_BYTES.get(ctx) {
				Ok(())
			} else {
				Err(Str::new_static("output ceiling is below spill threshold"))
			}
		},
		flags: archive | inherit,
	};
	/// Include tool-intent decisions in diagnostic tracing.
	pub static SV_TOOLS_INTENT_TRACING = sv_tools_intent_tracing: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Maximum repeated equivalent calls before interruption.
	pub static SV_TOOLS_LOOP_GUARD_LIMIT = sv_tools_loop_guard_limit: u32 {
		default: 8,
		min: 1,
		flags: archive | inherit,
	};
}

/// Tool exposure, timeout, and approval policy resolved from the control
/// context.
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
	/// Resolves tool exposure and admission policy from the process control
	/// context.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		let defaults = Self::default();
		Self {
			enabled:              bool_map(SV_TOOLS_ENABLED.get(ctx)),
			max_timeout:          SV_TOOLS_MAX_TIMEOUT.get(ctx).as_finite(),
			edit_dialect:         nonempty_str(SV_TOOLS_EDIT_DIALECT.get(ctx)),
			edit_blackbox_path:   nonempty_str(SV_TOOLS_EDIT_BLACKBOX_PATH.get(ctx))
				.map(|value| PathBuf::from(value.as_str())),
			edit_auto_repair:     SV_TOOLS_EDIT_AUTO_REPAIR.get(ctx),
			edit_streaming_abort: SV_TOOLS_EDIT_STREAMING_ABORT.get(ctx),
			fetch_enabled:        defaults.fetch_enabled,
			render_markdown:      defaults.render_markdown,
			auto_resize_images:   defaults.auto_resize_images,
			format_policy:        defaults.format_policy,
			diagnostics_on_write: defaults.diagnostics_on_write,
			diagnostics_on_edit:  defaults.diagnostics_on_edit,
			diagnostic_dedup:     defaults.diagnostic_dedup,
			approval_mode:        SV_TOOLS_APPROVAL_MODE.get(ctx),
			approval:             approval_map(SV_TOOLS_APPROVAL.get(ctx)),
			edit_fuzzy:           SV_TOOLS_EDIT_FUZZY.get(ctx),
			edit_require_seen:    SV_TOOLS_EDIT_REQUIRE_SEEN.get(ctx),
			edit_guard_generated: SV_TOOLS_EDIT_GUARD_GENERATED.get(ctx),
			read_max_bytes:       SV_TOOLS_READ_MAX_BYTES.get(ctx) as u64,
			read_summarize:       SV_TOOLS_READ_SUMMARIZE.get(ctx),
			read_line_numbers:    SV_TOOLS_READ_LINE_NUMBERS.get(ctx),
			grep_context_lines:   SV_TOOLS_GREP_CONTEXT_LINES.get(ctx),
			eval_interpreters:    string_map(SV_TOOLS_EVAL_INTERPRETERS.get(ctx)),
			output_spill_bytes:   SV_TOOLS_OUTPUT_SPILL_BYTES.get(ctx) as u64,
			output_max_bytes:     SV_TOOLS_OUTPUT_MAX_BYTES.get(ctx) as u64,
			intent_tracing:       SV_TOOLS_INTENT_TRACING.get(ctx),
			loop_guard_limit:     SV_TOOLS_LOOP_GUARD_LIMIT.get(ctx),
		}
	}

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

fn nonempty_str(value: Str) -> Option<Str> {
	(!value.is_empty()).then_some(value)
}

fn validate_bool_map(values: &Kv) -> Result<(), Str> {
	if values
		.iter()
		.all(|(name, value)| !name.trim().is_empty() && matches!(value, Value::Bool(_)))
	{
		Ok(())
	} else {
		Err(Str::new_static("tool names must be nonempty and values must be booleans"))
	}
}

fn validate_string_map(values: &Kv) -> Result<(), Str> {
	if values
		.iter()
		.all(|(name, value)| !name.trim().is_empty() && matches!(value, Value::Str(_)))
	{
		Ok(())
	} else {
		Err(Str::new_static("names must be nonempty and values must be strings"))
	}
}

fn validate_approval_map(values: &Kv) -> Result<(), Str> {
	if values.iter().all(|(name, value)| {
		!name.trim().is_empty()
			&& value
				.as_str()
				.is_some_and(|value| value.parse::<ApprovalPolicy>().is_ok())
	}) {
		Ok(())
	} else {
		Err(Str::new_static("approval values must be allow, deny, or prompt"))
	}
}

fn bool_map(values: Kv) -> BTreeMap<Str, bool> {
	values
		.0
		.into_iter()
		.filter_map(|(name, value)| Some((name, value.as_bool()?)))
		.collect()
}

fn string_map(values: Kv) -> BTreeMap<Str, Str> {
	values
		.0
		.into_iter()
		.filter_map(|(name, value)| Some((name, Str::new(value.as_str()?))))
		.collect()
}

fn approval_map(values: Kv) -> BTreeMap<Str, ApprovalPolicy> {
	values
		.0
		.into_iter()
		.filter_map(|(name, value)| Some((name, value.as_str()?.parse().ok()?)))
		.collect()
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
	use omp_tool::{Effects, ExecEffects};

	use super::*;
	use crate::admission::{ApprovalPolicy, ApprovalSource, ApprovalTier};

	#[test]
	fn typed_con_projection_round_trips() {
		let ctx = Ctx::new();
		SV_TOOLS_APPROVAL_MODE
			.set(&ctx, ApprovalMode::Write)
			.expect("set mode");
		SV_TOOLS_APPROVAL
			.set(&ctx, Kv(vec![(sf!("bash"), Value::Str(Str::new_static("deny")))]))
			.expect("set approval");
		assert_eq!(ToolSettings::from_con(&ctx), ToolSettings {
			approval_mode: ApprovalMode::Write,
			approval: BTreeMap::from([(sf!("bash"), ApprovalPolicy::Deny)]),
			..ToolSettings::default()
		});
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
		let ctx = Ctx::new();
		assert!(
			SV_TOOLS_APPROVAL
				.set(&ctx, Kv(vec![(Str::default(), Value::Str(Str::new_static("prompt")),)]))
				.is_err()
		);
	}
}
