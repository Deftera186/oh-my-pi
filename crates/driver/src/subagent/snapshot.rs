//! Monotonic child snapshot attenuation (plus the hidden `yield` grant) and
//! reasoning-effort resolution.

use std::{ffi, path::Path, sync::Arc};

use omp_agent::{AgentDefinition, AgentSnapshot};
use omp_catalog::GrammarBits;
use omp_core::{Str, sf};
use omp_envd::eval::spawn::SpawnEffort;
use omp_inference::ToolInputConstraint;
use omp_proto::inference::v1::{Effort, Reasoning, ToolDef, tool_def};
use omp_tool::{LoweringCaps, Registry};

use super::settings::{TaskEffortCeiling, TaskSettings};

/// Source which won the child thinking-precedence cascade.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum EffortSource {
	/// Spawn caller parameter.
	Caller,
	/// Suffix on the resolved child model selector.
	ModelSuffix,
	/// Agent-definition frontmatter.
	Frontmatter,
	/// Inherited model pattern suffix.
	Pattern,
	/// No source selected an effort.
	None,
}

/// Auditable requested and effective child effort.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffortResolution {
	/// Winning precedence source.
	pub source:    EffortSource,
	/// Value before settings and model ceilings.
	pub requested: Option<SpawnEffort>,
	/// Value after both ceilings.
	pub effective: Option<SpawnEffort>,
	/// Whether either ceiling attenuated the requested value.
	pub clamped:   bool,
}

/// Inputs which may only attenuate a cloned parent snapshot.
pub struct ChildSnapshotOptions<'a> {
	/// Resolved agent definition.
	pub definition:        &'a AgentDefinition,
	/// Live task settings captured once for this spawn.
	pub settings:          &'a TaskSettings,
	/// Effective child working directory.
	pub cwd:               &'a Path,
	/// Exact selected model selector or role.
	pub selected_model:    Option<&'a str>,
	/// Stable inference role (`subagent:<id>`) for attribution and fallback.
	pub inference_role:    Option<&'a str>,
	/// Parent/session fallback pattern used only for effort precedence.
	pub inherited_pattern: Option<&'a str>,
	/// Caller effort, highest precedence.
	pub caller_effort:     Option<SpawnEffort>,
	/// Selected model's advertised effort ceiling.
	pub model_ceiling:     Option<SpawnEffort>,
	/// Whether the parent is in read-only plan mode.
	pub plan_mode:         bool,
	/// Per-spawn LSP request; the task setting must also permit it.
	pub enable_lsp:        bool,
	/// Whether the prewalk gate is currently active.
	pub prewalk_gate:      bool,
}

/// Clones a parent runtime snapshot while applying child attenuation and the
/// hidden `yield` grant.
pub fn child_snapshot(parent: &AgentSnapshot, options: ChildSnapshotOptions<'_>) -> AgentSnapshot {
	let mut child = parent.clone();
	child
		.props
		.set(omp_agent::prompt_keys::CWD, options.cwd.to_string_lossy().into_owned());
	if let Some(omp_scribe::Value::List(files)) =
		child.props.get(omp_agent::prompt_keys::CONTEXT_FILES)
	{
		let filtered = files
			.iter()
			.filter(|entry| {
				let omp_scribe::Value::Map(entry) = entry else {
					return false;
				};
				entry
					.get(omp_agent::prompt_keys::PATH)
					.and_then(omp_scribe::Value::as_str)
					.is_some_and(|path| context_applies(Path::new(path), options.cwd))
			})
			.cloned()
			.collect::<omp_scribe::Value>();
		child
			.props
			.set(omp_agent::prompt_keys::CONTEXT_FILES, filtered);
	}
	if let Some(model) = options.selected_model {
		child.turn.params.model = strip_effort_suffix(model).to_owned();
		let mut fields = match child.props.get(omp_agent::prompt_keys::MODEL) {
			Some(omp_scribe::Value::Map(fields)) => fields.clone(),
			_ => Default::default(),
		};
		fields.insert(
			Str::new_static(omp_agent::prompt_keys::IDENTIFIER),
			omp_scribe::Value::from(child.turn.params.model.clone()),
		);
		child
			.props
			.set(omp_agent::prompt_keys::MODEL, omp_scribe::Value::Map(fields));
	}
	if let Some(role) = options.inference_role {
		let meta = child.turn.params.meta.get_or_insert_default();
		meta.initiator = role.to_owned();
	}
	let effort = resolve_effort(
		options.caller_effort,
		options.selected_model,
		options.definition.thinking_level.as_deref(),
		options.inherited_pattern,
		options.settings.max_effort,
		options.model_ceiling,
	);
	if let Some(effective) = effort.effective {
		child.turn.params.thinking =
			Some(Reasoning { effort: effort_proto(effective) as i32, ..Reasoning::default() });
	}
	let mut enabled = child
		.enabled_tools
		.iter()
		.filter(|name| tool_allowed(name.as_str(), &options))
		.cloned()
		.collect::<Vec<_>>();
	child
		.turn
		.params
		.tools
		.retain(|tool| tool_allowed(tool.name.as_str(), &options));
	// Every child finalizes through the hidden `yield` tool, which the
	// top-level agent never advertises; widening the cloned set here is the
	// one non-attenuating grant for all task children.
	if !enabled.iter().any(|name| name == "yield")
		&& let Some(tool) = lowered_yield(&child.registry)
	{
		enabled.push(sf!("yield"));
		child.turn.params.tools.push(tool);
	}
	child.enabled_tools = Arc::from(enabled);
	child
		.props
		.set(omp_agent::prompt_keys::TOOLS, child.enabled_tools.iter().cloned().collect::<Vec<_>>());
	child
}
/// Lowers the hidden `yield` declaration into a child's protocol tool list.
///
/// Returns `None` when the registry does not host `yield` (tool disabled or
/// bare test registries); such a child terminates through its last assistant
/// turn instead.
fn lowered_yield(registry: &Registry) -> Option<ToolDef> {
	let caps = LoweringCaps {
		strict_schema:  true,
		grammar:        GrammarBits::ALL,
		maximum_tools:  None,
		maximum_strict: None,
	};
	let tool = registry
		.advertise_selected(caps, &[sf!("yield")])
		.ok()?
		.into_iter()
		.next()?;
	let ToolInputConstraint::JsonSchema { parameters, strict } = tool.definition.input else {
		return None;
	};
	Some(ToolDef {
		name:        tool.definition.name.to_string(),
		description: tool
			.definition
			.description
			.map_or_else(String::new, |value| value.to_string()),
		input:       Some(tool_def::Input::JsonSchema(tool_def::JsonSchema {
			schema_json: serde_json::to_vec(parameters.as_value()).ok()?.into(),
			strict:      Some(strict),
		})),
	})
}

/// Resolves caller → model suffix → frontmatter → pattern and clamps every
/// source to both configured and advertised ceilings.
pub fn resolve_effort(
	caller: Option<SpawnEffort>,
	model: Option<&str>,
	frontmatter: Option<&str>,
	pattern: Option<&str>,
	settings_ceiling: TaskEffortCeiling,
	model_ceiling: Option<SpawnEffort>,
) -> EffortResolution {
	let (source, requested) = if let Some(effort) = caller {
		(EffortSource::Caller, Some(effort))
	} else if let Some(effort) = model.and_then(effort_suffix) {
		(EffortSource::ModelSuffix, Some(effort))
	} else if let Some(effort) = frontmatter.and_then(parse_effort) {
		(EffortSource::Frontmatter, Some(effort))
	} else if let Some(effort) = pattern.and_then(effort_suffix) {
		(EffortSource::Pattern, Some(effort))
	} else {
		(EffortSource::None, None)
	};
	let ceiling = ceiling_effort(settings_ceiling);
	let ceiling = model_ceiling.map_or(ceiling, |model| min_effort(ceiling, model));
	let effective = requested.map(|effort| min_effort(effort, ceiling));
	EffortResolution { source, requested, effective, clamped: requested != effective }
}

fn tool_allowed(name: &str, options: &ChildSnapshotOptions<'_>) -> bool {
	if options.plan_mode {
		return matches!(name, "read" | "grep" | "glob" | "web_search")
			|| (name == "ast_grep"
				&& options
					.definition
					.tools
					.iter()
					.any(|tool| tool.as_str() == "ast_grep"));
	}
	if matches!(name, "checkpoint" | "rewind") {
		return false;
	}
	if name == "lsp" && !(options.settings.enable_lsp && options.enable_lsp) {
		return false;
	}
	if name == "todo" && !options.prewalk_gate {
		return false;
	}
	if name == "eval" && options.definition.spawns.default_definition().is_none() {
		return false;
	}
	let declared = &options.definition.tools;
	declared.is_empty()
		|| name == "hub"
		|| declared.iter().any(|tool| tool.as_str() == name)
		|| (declared.iter().any(|tool| tool.as_str() == "exec") && matches!(name, "bash" | "eval"))
		|| (declared.iter().any(|tool| tool.as_str() == "task") && name == "eval")
}

fn context_applies(path: &Path, cwd: &Path) -> bool {
	if path.file_name().and_then(ffi::OsStr::to_str) != Some("AGENTS.md") || path.is_relative() {
		return true;
	}
	path.parent().is_some_and(|parent| cwd.starts_with(parent))
}

fn effort_suffix(selector: &str) -> Option<SpawnEffort> {
	selector
		.rsplit_once(':')
		.and_then(|(_, suffix)| parse_effort(suffix))
}

fn strip_effort_suffix(selector: &str) -> &str {
	selector
		.rsplit_once(':')
		.filter(|(_, suffix)| parse_effort(suffix).is_some())
		.map_or(selector, |(model, _)| model)
}

fn parse_effort(value: &str) -> Option<SpawnEffort> {
	match value.trim().to_ascii_lowercase().as_str() {
		"minimal" | "min" => Some(SpawnEffort::Minimal),
		"low" | "lo" => Some(SpawnEffort::Low),
		"medium" | "med" => Some(SpawnEffort::Medium),
		"high" | "hi" => Some(SpawnEffort::High),
		"xhigh" | "xhi" => Some(SpawnEffort::Xhigh),
		"max" => Some(SpawnEffort::Max),
		_ => None,
	}
}

const fn ceiling_effort(ceiling: TaskEffortCeiling) -> SpawnEffort {
	match ceiling {
		TaskEffortCeiling::Minimal => SpawnEffort::Minimal,
		TaskEffortCeiling::Low => SpawnEffort::Low,
		TaskEffortCeiling::Medium => SpawnEffort::Medium,
		TaskEffortCeiling::High => SpawnEffort::High,
		TaskEffortCeiling::Xhigh => SpawnEffort::Xhigh,
		TaskEffortCeiling::Max => SpawnEffort::Max,
	}
}

const fn effort_rank(effort: SpawnEffort) -> u8 {
	match effort {
		SpawnEffort::Minimal => 0,
		SpawnEffort::Low => 1,
		SpawnEffort::Medium => 2,
		SpawnEffort::High => 3,
		SpawnEffort::Xhigh => 4,
		SpawnEffort::Max => 5,
	}
}

const fn min_effort(left: SpawnEffort, right: SpawnEffort) -> SpawnEffort {
	if effort_rank(left) <= effort_rank(right) {
		left
	} else {
		right
	}
}

const fn effort_proto(effort: SpawnEffort) -> Effort {
	match effort {
		SpawnEffort::Minimal => Effort::Minimal,
		SpawnEffort::Low => Effort::Low,
		SpawnEffort::Medium => Effort::Medium,
		SpawnEffort::High => Effort::High,
		SpawnEffort::Xhigh => Effort::Xhigh,
		SpawnEffort::Max => Effort::Max,
	}
}
#[cfg(test)]
mod tests {
	use omp_tool::{Claims, Precedence, Presentation};

	use super::*;

	#[test]
	fn child_gains_hidden_yield_absent_from_parent() {
		let mut registry = Registry::new();
		registry
			.register(omp_tools::yield_tool::tool(), Presentation::Hidden, Claims {
				precedence: Precedence::CORE,
				claimant:   sf!("omp/core"),
				replaces:   None,
			})
			.expect("register hidden yield");
		let mut parent = AgentSnapshot::default();
		parent.registry = Arc::new(registry);
		let definition =
			AgentDefinition::parse_markdown("child", "---\ndescription: test child\n---\nbody")
				.expect("definition");
		let settings = TaskSettings::default();
		let child = child_snapshot(&parent, ChildSnapshotOptions {
			definition:        &definition,
			settings:          &settings,
			cwd:               Path::new("/"),
			selected_model:    None,
			inference_role:    None,
			inherited_pattern: None,
			caller_effort:     None,
			model_ceiling:     None,
			plan_mode:         false,
			enable_lsp:        false,
			prewalk_gate:      false,
		});
		assert!(child.enabled_tools.iter().any(|name| name == "yield"));
		let tool = child
			.turn
			.params
			.tools
			.iter()
			.find(|tool| tool.name == "yield")
			.expect("child protocol tool list carries yield");
		let Some(tool_def::Input::JsonSchema(schema)) = &tool.input else {
			panic!("yield rides a JSON schema declaration");
		};
		assert_eq!(schema.strict, Some(false), "yield must never request strict sampling");
	}
}
