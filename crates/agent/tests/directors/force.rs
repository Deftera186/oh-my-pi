use std::sync::Arc;

use omp_agent::{
	DirectorCx, DirectorStack, ForceUntil, LoopDecision, RouteFacts,
	directors::force_tool::ForceTool,
};
use omp_core::Str;
use omp_inference::{ChatRequest, NegotiationPolicy, Sampling, Setting, ToolChoice};

use crate::harness::{Call, Harness};

#[test]
fn test_two_rung_program_forces_write_then_none_and_completes() {
	let mut world = Harness::new();
	world.engage(ForceTool::new("write", ForceUntil::ToolCalled(Str::new_static("write")), None, 2));
	let mut req = request();
	let cx = DirectorCx::new(world.session.dom().body(), &world.route);
	DirectorStack::from_dom(world.session.dom(), &world.registry).prepare_inference(
		world.session.dom(),
		&cx,
		&mut req,
	);
	assert!(
		matches!(req.tool_choice, Setting::Require(ToolChoice::Named(ref name)) if name == "write")
	);
	world.turn("", &[Call::new("write", serde_json::json!({}))], 0);
	assert!(!world.active().iter().any(|&id| id == "force_tool"));
	let req = request();
	assert!(matches!(req.tool_choice, Setting::Unset));
}

#[test]
fn test_run_scope_starts_after_prompt_and_expires_at_run_end() {
	let mut world = Harness::new();
	world.engage(ForceTool::new("write", ForceUntil::AnyToolCall, None, 2));
	world.turn("", &[Call::new("write", serde_json::json!({}))], 0);
	assert!(world.active().is_empty());
}

#[test]
fn test_provider_downgrade_injects_requirement_before_turn() {
	let mut world = Harness::new();
	world.route.forced_choice_free = false;
	world.engage(ForceTool::new("write", ForceUntil::AnyToolCall, None, 2));
	let mut req = request();
	let cx = DirectorCx::new(world.session.dom().body(), &world.route);
	DirectorStack::from_dom(world.session.dom(), &world.registry).prepare_inference(
		world.session.dom(),
		&cx,
		&mut req,
	);
	assert!(matches!(req.tool_choice, Setting::Unset));
	assert_eq!(req.messages.len(), 1);
	assert!(matches!(world.turn("provider response", &[], 0), LoopDecision::Continue { .. }));
	assert!(
		world
			.developer_texts()
			.iter()
			.any(|text| text.contains("write"))
	);
}

#[test]
fn test_claim_holder_outranks_queued_settle_force_and_ladder_pauses() {
	let mut world = Harness::new();
	world.engage(ForceTool::new("write", ForceUntil::AnyToolCall, None, 3));
	world.engage(ForceTool::new("ask", ForceUntil::AnyToolCall, None, 3));
	assert_eq!(world.queued(), vec!["force_tool"]);
	world.turn("first", &[], 0);
	assert_eq!(world.state_str("force_tool", "tool").as_deref(), Some("write"));
	world.turn("", &[Call::new("write", serde_json::json!({}))], 0);
	assert_eq!(world.state_str("force_tool", "tool").as_deref(), Some("ask"));
	assert_eq!(world.state_int("force_tool", "attempts"), Some(0));
}

#[test]
fn test_force_tool_is_evaluated_from_engagement_state() {
	let mut world = Harness::new();
	world.engage(ForceTool::new("grep", ForceUntil::AnyToolCall, None, 1));
	let mut req = request();
	let facts = RouteFacts { forced_choice_free: true, context_window: 128_000 };
	let cx = DirectorCx::new(world.session.dom().body(), &facts);
	DirectorStack::from_dom(world.session.dom(), &world.registry).prepare_inference(
		world.session.dom(),
		&cx,
		&mut req,
	);
	assert!(
		matches!(req.tool_choice, Setting::Require(ToolChoice::Named(ref name)) if name == "grep")
	);
}

fn request() -> ChatRequest {
	ChatRequest {
		messages:          Arc::from([]),
		tools:             Arc::from([]),
		hosted_tools:      Arc::from([]),
		tool_choice:       Setting::Unset,
		output:            Setting::Unset,
		reasoning:         Setting::Unset,
		verbosity:         Setting::Unset,
		cache_retention:   Setting::Unset,
		service_tier:      Setting::Unset,
		sampling:          Sampling::default(),
		max_output_tokens: None,
		top_logprobs:      None,
		safety:            Arc::from([]),
		negotiation:       NegotiationPolicy::default(),
	}
}
