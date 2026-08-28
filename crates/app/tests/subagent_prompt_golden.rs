//! Golden parity oracle for subagent, auxiliary, and command prompt surfaces.

use std::path::Path;

use omp_agent::{AgentDefinition, PromptFacts, PromptSource, Props, render_compaction_summary};
use omp_app::chat_ui::template::{TemplateArguments, render as render_command};
use omp_core::sf;
use omp_driver::subagent::{
	prompt::{
		self, ModelFamilyCapabilities, PromptPeer, SubagentPromptInput, compose, props as child_props,
	},
	settings::TaskEagerMode,
};
use omp_proto::thread::v1::{item, part};
use serde_json::json;

const SUBAGENT_VALIDATION_GUIDANCE: &str =
	"Project-wide validation is the main agent's job, run once after all subagents land. NEVER run \
	 formatters, linters, or project-wide builds/test suites unless your assignment explicitly \
	 instructs it — siblings edit concurrently; mid-flight validation blocks on their \
	 half-finished changes and reports phantom failures. Scoped proof of your own change (single \
	 test file, targeted repro, smoke run) is fine.";

fn without_new_validation_guidance(prompt: &str) -> String {
	assert!(prompt.contains(SUBAGENT_VALIDATION_GUIDANCE));
	let start = prompt.find("# Validation\n").expect("validation heading");
	let end = prompt[start..]
		.find("# Runtime\n")
		.map(|offset| start + offset)
		.expect("runtime heading");
	format!("{}{}", &prompt[..start], &prompt[end..])
}

fn definition() -> AgentDefinition {
	AgentDefinition::parse_markdown(
		"golden",
		"---\ndescription: Golden specialist\ntools: [read, yield]\n---\n# Specialist\n\nComplete \
		 only the delegated target.",
	)
	.expect("golden definition")
}

#[test]
fn subagent_minimal_prompt() {
	let definition = definition();
	let prompt = compose(
		SubagentPromptInput {
			definition:        &definition,
			shared_context:    None,
			plan_path:         None,
			plan_content:      None,
			workspace_root:    Path::new("/workspace"),
			output_schema:     None,
			self_name:         "Golden",
			self_role:         "task",
			irc_enabled:       false,
			roster_generation: 0,
			peers:             &[],
			capabilities:      ModelFamilyCapabilities::default(),
			plan_mode:         false,
			eager:             TaskEagerMode::Default,
		},
		&Props::new(),
	);
	insta::assert_snapshot!("subagent_minimal", without_new_validation_guidance(&prompt));
}

#[test]
fn subagent_props_inherit_parent_secrets_policy() {
	let definition = definition();
	let mut facts = PromptFacts::default();
	facts.settings.secrets_enabled = true;
	let parent = facts.props().expect("parent props");
	let input = SubagentPromptInput {
		definition:        &definition,
		shared_context:    None,
		plan_path:         None,
		plan_content:      None,
		workspace_root:    Path::new("/workspace"),
		output_schema:     None,
		self_name:         "Golden",
		self_role:         "task",
		irc_enabled:       false,
		roster_generation: 0,
		peers:             &[],
		capabilities:      ModelFamilyCapabilities::default(),
		plan_mode:         false,
		eager:             TaskEagerMode::Default,
	};
	let child = child_props(&input, &parent);
	assert!(
		child
			.get(omp_agent::prompt_keys::SECRETS_ENABLED)
			.is_some_and(omp_scribe::Value::is_truthy),
		"child props must retain the parent secrets policy",
	);
	let items = omp_agent::CanonicalPromptSource
		.render(&child)
		.expect("child canonical prompt");
	assert!(
		items.iter().any(|item| {
			let Some(item::Kind::Message(message)) = &item.kind else {
				return false;
			};
			message.parts.iter().any(|part| {
				matches!(
					&part.kind,
					Some(part::Kind::Text(text))
						if text.contains("redaction tokens are opaque strings")
				)
			})
		}),
		"secrets policy must render in the banded child canonical prompt",
	);
}

#[test]
fn subagent_full_matrix() {
	let definition = definition();
	let peers = [
		PromptPeer { name: "Golden", role: "task", status: "running", activity: "editing" },
		PromptPeer { name: "Scout", role: "scout", status: "idle", activity: "" },
		PromptPeer {
			name:     "Reviewer",
			role:     "reviewer",
			status:   "parked",
			activity: "reviewing",
		},
	];
	let schema = json!({
		"type": "object",
		"properties": { "answer": { "type": "string" }, "count": { "type": "integer" } },
		"required": ["answer"]
	});
	for codex_style in [false, true] {
		for parallel_tool_calls in [false, true] {
			for structured_yield in [false, true] {
				for plan_mode in [false, true] {
					for eager in
						[TaskEagerMode::Default, TaskEagerMode::Preferred, TaskEagerMode::Always]
					{
						let prompt = compose(
							SubagentPromptInput {
								definition: &definition,
								shared_context: Some("Shared batch contract.\n\nPreserve ownership."),
								plan_path: Some(Path::new("/workspace/CPLAN.md")),
								plan_content: Some("# Plan\n\n1. Inspect.\n2. Implement."),
								workspace_root: Path::new("/workspace/.worktrees/golden"),
								output_schema: Some(&schema),
								self_name: "Golden",
								self_role: "task",
								irc_enabled: true,
								roster_generation: 42,
								peers: &peers,
								capabilities: ModelFamilyCapabilities {
									codex_style,
									parallel_tool_calls,
									structured_yield,
								},
								plan_mode,
								eager,
							},
							&Props::new(),
						);
						let name = format!(
							"subagent_full_codex_{codex_style}_parallel_{parallel_tool_calls}_yield_{structured_yield}_plan_{plan_mode}_eager_{eager}"
						);
						insta::assert_snapshot!(name, without_new_validation_guidance(&prompt));
					}
				}
			}
		}
	}
}

#[test]
fn recovery_steering_compaction_and_voice_prompts() {
	let mut empty_stop = String::new();
	omp_agent::prompt_assets::render_empty_stop_retry(&mut empty_stop, 2, 5);
	insta::assert_snapshot!("empty_stop_retry", empty_stop);

	let mut parent_irc = String::new();
	omp_agent::prompt_assets::render_parent_irc(
		&mut parent_irc,
		"Parent",
		"Finish the exact assigned slice.",
	);
	insta::assert_snapshot!("parent_irc", parent_irc);

	let mut loop_redirect = String::new();
	omp_agent::prompt_assets::render_tool_call_loop_redirect(
		&mut loop_redirect,
		4,
		"arguments: path=src/lib.rs; result: unchanged",
	);
	insta::assert_snapshot!("tool_call_loop_redirect", loop_redirect);

	insta::assert_snapshot!(
		"compaction_summary_ordinary",
		render_compaction_summary("Portable state.\n\nNext action.", Some("remote"))
	);
	insta::assert_snapshot!(
		"compaction_summary_handoff",
		render_compaction_summary("Handoff state.\n\nOwned files listed.", Some("handoff"))
	);
	insta::assert_snapshot!(
		"voice_live_instructions",
		omp_agent::voice::render_live_instructions("Ada", "ada-user")
	);
}

#[test]
fn native_command_helper_matrix() {
	let words = [sf!("one"), sf!("two words"), sf!("three")];
	let arguments = TemplateArguments { raw: "one \"two words\" three", words: &words };
	let cases = [
		("args_raw", "{{ args }}"),
		("args_indexed", "{{ arguments[0] }}|{{ arguments[1] }}|{{ arguments[9] | default(\"\") }}"),
		("list", "{{ arguments | bullets }}"),
		("join", "{{ arguments | join(\",\") }}"),
		("when_true", "{% if arguments %}yes{% else %}no{% endif %}"),
		("when_else", "{% if missing %}yes{% else %}no{% endif %}"),
		("table", "{{ table(arguments) }}"),
		("codeblock", "{% codeblock \"rust\" %}fn main() {}{% endcodeblock %}"),
		("xml", "{% xml \"note\" %}{{ \"<ok & safe>\" | escape_xml }}{% endxml %}"),
	];
	for (name, template) in cases {
		let rendered = render_command(template, arguments).expect("command template render");
		insta::assert_snapshot!(name, rendered);
	}
}

#[test]
fn driver_schema_helpers_are_registered() {
	let engine = prompt::engine();
	let template = engine
		.compile(
			"schema-helper-golden",
			"{{ jtd_ts(output_schema) }}\n---\n{{ yield_schema(output_schema) }}",
		)
		.expect("schema helper template");
	let mut props = omp_scribe::Props::new();
	props.set(
		"output_schema",
		omp_scribe::Value::from(&json!({
			"type": "object",
			"properties": { "answer": { "type": "string" } },
			"required": ["answer"]
		})),
	);
	let rendered = template
		.render_str(engine, &props)
		.expect("schema helper render");
	insta::assert_snapshot!("driver_schema_helpers", rendered);
}
