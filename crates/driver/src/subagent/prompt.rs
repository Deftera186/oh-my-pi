//! Typed first-turn and revival prompt composition for subagents.

use std::{path::Path, sync::LazyLock};

use omp_agent::{AgentDefinition, AgentRecord};
use omp_core::Str;
use omp_scribe::{
	Engine, Error as ScribeError, HelperError, Props, Template, Value as ScribeValue, map,
};
use serde_json::Value;

use super::settings::TaskEagerMode;
use crate::prompt_templates::schema::render as render_schema;
/// Returns the driver-side prompt engine with schema-domain functions
/// installed.
///
/// This engine is intentionally independent from the agent engine: helper
/// validation happens against the exact registry used to compile driver-owned
/// subagent templates.
pub fn engine() -> &'static Engine {
	static ENGINE: LazyLock<Engine> = LazyLock::new(|| {
		let mut engine = Engine::new();
		engine.add_function("jtd_ts", |args| {
			let schema = schema_argument("jtd_ts", args)?;
			let rendered = render_schema(&schema);
			let declarations = rendered
				.split_once("type YieldEnvelope")
				.map_or(rendered.as_str(), |(declarations, _)| declarations)
				.trim_end();
			Ok(ScribeValue::from(declarations.to_owned()))
		});
		engine.add_function("yield_schema", |args| {
			let schema = schema_argument("yield_schema", args)?;
			Ok(ScribeValue::from(render_schema(&schema)))
		});
		engine
	});
	&ENGINE
}
fn system_template() -> &'static Template {
	static TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
		engine()
			.compile("subagent/system", include_str!("../../../agent/prompts/subagent/system.md"))
			.expect("embedded subagent system template")
	});
	&TEMPLATE
}

fn schema_argument(name: &'static str, args: &[ScribeValue]) -> Result<Value, ScribeError> {
	if args.len() != 1 {
		return Err(ScribeError::helper(name, HelperError::Arity {
			expected: 1,
			got:      args.len(),
		}));
	}
	serde_json::to_value(&args[0]).map_err(|source| ScribeError::helper(name, source))
}

/// Model-family capabilities which affect delegation guidance.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelFamilyCapabilities {
	/// OpenAI Codex-style models benefit from explicit tool-call concurrency.
	pub codex_style:         bool,
	/// Model supports multiple independent tool calls in one assistant step.
	pub parallel_tool_calls: bool,
	/// Child result must terminate through the yield tool.
	pub structured_yield:    bool,
}

/// One peer projected into the generation-stamped IRC roster.
pub struct PromptPeer<'a> {
	/// Session-local display alias.
	pub name:     &'a str,
	/// Definition or root kind label.
	pub role:     &'a str,
	/// Current lifecycle label.
	pub status:   &'a str,
	/// Short current activity.
	pub activity: &'a str,
}

/// Complete immutable input for one child system prompt.
pub struct SubagentPromptInput<'a> {
	/// Selected agent definition.
	pub definition:        &'a AgentDefinition,
	/// Shared batch context, if any.
	pub shared_context:    Option<&'a str>,
	/// Active parent plan path.
	pub plan_path:         Option<&'a Path>,
	/// Exact active plan content.
	pub plan_content:      Option<&'a str>,
	/// Effective normal or isolated workspace root.
	pub workspace_root:    &'a Path,
	/// Effective normalized output schema.
	pub output_schema:     Option<&'a Value>,
	/// Stable display alias used by IRC for this loop.
	pub self_name:         &'a str,
	/// Resolved definition or root role for this loop.
	pub self_role:         &'a str,
	/// Whether spawn depth permits the loop to use the IRC bus.
	pub irc_enabled:       bool,
	/// IRC roster generation.
	pub roster_generation: u64,
	/// Peers visible to this child.
	pub peers:             &'a [PromptPeer<'a>],
	/// Current-root parked peers omitted from the initial prompt.
	pub parked_count:      usize,
	/// Live peers dropped by the bounded initial roster.
	pub omitted_count:     usize,
	/// Model-family behavioral capabilities.
	pub capabilities:      ModelFamilyCapabilities,
	/// Whether plan mode attenuated this child.
	pub plan_mode:         bool,
	/// Live eager-delegation guidance inherited by this child.
	pub eager:             TaskEagerMode,
}

/// Builds the child-wins prompt-property overlay inherited by a subagent.
pub fn props(input: &SubagentPromptInput<'_>, parent: &Props) -> Props {
	let mut patch = Props::new();
	patch.set("agent_name", input.definition.name.clone());
	patch.set("agent_description", input.definition.description.clone());
	patch.set("agent_prompt", input.definition.prompt.clone());
	if let Some(context) = input
		.shared_context
		.map(str::trim)
		.filter(|context| !context.is_empty())
	{
		patch.set("shared_context", context.to_owned());
	}
	if let Some(path) = input.plan_path {
		patch.set("plan_path", path.to_string_lossy().into_owned());
	}
	if let Some(plan) = input
		.plan_content
		.map(str::trim)
		.filter(|plan| !plan.is_empty())
	{
		patch.set("plan_content", plan.to_owned());
	}
	patch.set("workspace_root", input.workspace_root.to_string_lossy().into_owned());
	if let Some(schema) = input.output_schema {
		patch.set("output_schema", ScribeValue::from(schema));
		patch.set("output_schema_ts", render_schema(schema));
	}
	patch.set("self_name", input.self_name.to_owned());
	patch.set("self_role", input.self_role.to_owned());
	patch.set("irc_enabled", input.irc_enabled);
	patch.set("roster_generation", input.roster_generation as i64);
	patch.set(
		"peers",
		input
			.peers
			.iter()
			.map(|peer| {
				map! {
					"name" => peer.name.to_owned(),
					"role" => peer.role.to_owned(),
					"status" => peer.status.to_owned(),
					"activity" => peer.activity.to_owned(),
				}
			})
			.collect::<Vec<_>>(),
	);
	patch.set("parked_count", input.parked_count as i64);
	patch.set("omitted_count", input.omitted_count as i64);
	patch.set("caps", map! {
		"codex_style" => input.capabilities.codex_style,
		"parallel_tool_calls" => input.capabilities.parallel_tool_calls,
		"structured_yield" => input.capabilities.structured_yield,
	});
	patch.set("plan_mode", input.plan_mode);
	patch.set("eager", match input.eager {
		TaskEagerMode::Default => "default",
		TaskEagerMode::Preferred => "preferred",
		TaskEagerMode::Always => "always",
	});
	parent.overlay(&patch)
}

/// Composes the complete child prompt without creating a second policy owner.
pub fn compose(input: SubagentPromptInput<'_>, parent: &Props) -> Str {
	let props = props(&input, parent);
	system_template()
		.render_str(engine(), &props)
		.expect("typed subagent props satisfy the embedded template")
}

/// Projects one live roster record without transferring scheduling authority.
pub fn peer_from_record(record: &AgentRecord) -> (Str, Str, Str, Str) {
	(
		record.name.clone(),
		record
			.definition
			.clone()
			.unwrap_or_else(|| Str::from(record.kind.to_string())),
		Str::from(record.status.to_string()),
		record.activity.clone(),
	)
}
#[cfg(test)]
mod tests {
	use std::collections::HashSet;

	use omp_scribe::{Props, map};

	use super::{engine, system_template};

	#[test]
	fn subagent_template_parses_and_uses_registered_keys() {
		let keys = omp_agent::prompt_keys::ALL
			.iter()
			.copied()
			.collect::<HashSet<_>>();
		for key in system_template().referenced_keys() {
			assert!(keys.contains(key), "subagent template references unregistered key {key}");
		}
	}

	#[test]
	fn subagent_roster_reports_omitted_live_and_parked_counts() {
		let mut props = Props::new();
		props.set("irc_enabled", true);
		props.set("self_name", "Worker");
		props.set("self_role", "task");
		props.set("roster_generation", 7_i64);
		props.set("peers", vec![map! {
			"name" => "Main",
			"role" => "main",
			"status" => "running",
			"activity" => "working",
		}]);
		props.set("omitted_count", 3_i64);
		props.set("parked_count", 27_i64);
		props.set("caps", map! {
			"codex_style" => false,
			"parallel_tool_calls" => false,
			"structured_yield" => false,
		});
		let rendered = system_template()
			.render_str(engine(), &props)
			.expect("roster prompt renders");
		assert!(rendered.contains("3 more live peer(s) omitted."));
		assert!(rendered.contains("27 parked peer(s) omitted."));
		assert!(!rendered.contains("- (no live agents)"));
		props.set("peers", Vec::<omp_scribe::Value>::new());
		props.set("omitted_count", 0_i64);
		let empty = system_template()
			.render_str(engine(), &props)
			.expect("empty live roster renders");
		assert!(empty.contains("- (no live agents)"));
	}
}
