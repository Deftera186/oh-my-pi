use omp_agent::{
	LoopDecision,
	directors::{goal::Goal, loop_mode::LoopMode},
};

use crate::harness::{Call, Harness};

#[test]
fn test_goal_continues_until_complete_and_accounts_tokens() {
	let mut world = Harness::new();
	world.engage(Goal::new("finish the task", None));
	assert!(matches!(
		world.turn("", &[Call::new("todo", serde_json::json!({"op": "add"}))], 11),
		LoopDecision::Continue { .. }
	));
	assert!(matches!(
		world.turn("", &[Call::new("todo", serde_json::json!({"op": "start"}))], 17),
		LoopDecision::Continue { .. }
	));
	assert_eq!(world.state_int("goal", "tokens_used"), Some(28));
	world.turn("", &[Call::new("goal", serde_json::json!({"op": "complete"}))], 23);
	assert!(!world.active().iter().any(|&id| id == "goal"));
}

#[test]
fn test_goal_exits_when_token_budget_is_exhausted() {
	let mut world = Harness::new();
	world.engage(Goal::new("bounded work", Some(5)));
	world.turn("", &[Call::new("todo", serde_json::json!({"op": "add"}))], 9);
	assert!(!world.active().iter().any(|&id| id == "goal"));
	assert!(
		world
			.developer_texts()
			.iter()
			.any(|text| text.contains("budget exhausted"))
	);
}

#[test]
fn test_goal_holds_instead_of_looping_on_prose_only_turn() {
	let mut world = Harness::new();
	world.engage(Goal::new("do not self-prompt on prose", None));
	assert_eq!(world.turn("I need user guidance", &[], 13), LoopDecision::Yield);
	assert!(world.active().iter().any(|&id| id == "goal"));
}

#[test]
fn test_goal_loop_claim_queues_and_promotes_a_contender() {
	let mut world = Harness::new();
	world.engage(Goal::new("finish before verification", None));
	world.engage(LoopMode::new("verify", Some(1)));
	assert_eq!(world.queued(), vec!["loop_mode"]);
	world.turn("", &[Call::new("goal", serde_json::json!({"op": "complete"}))], 4);
	assert_eq!(world.active(), vec!["loop_mode"]);
	assert!(world.queued().is_empty());
}

#[test]
fn test_goal_tool_is_unregistered_when_engagement_exits() {
	let mut world = Harness::new();
	world.engage(Goal::new("short-lived tool", None));
	assert_eq!(world.state_str("goal", "tool").as_deref(), Some("goal"));
	world.set_state("goal", "done", omp_agent::BindValue::Bool(true));
	world.turn("done", &[], 0);
	assert!(world.state_str("goal", "tool").is_none());
}
