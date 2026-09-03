use omp_agent::{
	LoopDecision,
	directors::{plan::Plan, todo_reminder::TodoReminder},
};
use omp_dom::PropKey;

use crate::harness::{Call, Harness};

const PLAN_FILE: &str = "local://plans/current.md";

#[test]
fn test_stall_then_write_then_propose_advances_the_gate() {
	let mut world = Harness::new();
	world.engage(Plan::new(PLAN_FILE));
	assert!(matches!(world.turn("still thinking", &[], 0), LoopDecision::Continue { .. }));
	assert_eq!(world.active(), vec!["plan", "force_tool"]);
	world.turn(
		"",
		&[Call::new("write", serde_json::json!({"path": PLAN_FILE, "content": "plan"}))],
		0,
	);
	assert_eq!(world.state_bool("plan", "plan_written"), Some(true));
	assert_eq!(world.active(), vec!["plan", "force_tool"]);
	let result = world.turn(
		"",
		&[Call::new("write", serde_json::json!({"path": "xd://propose", "content": "approve?"}))],
		0,
	);
	assert_eq!(result, LoopDecision::Yield);
	assert_eq!(world.state_bool("plan", "decision_made"), Some(true));
}

#[test]
fn test_stall_rungs_cap_at_three_then_idle() {
	let mut world = Harness::new();
	world.route.forced_choice_free = false;
	world.engage(Plan::new(PLAN_FILE));
	for _ in 0..5 {
		let _ = world.turn("stall", &[], 0);
	}
	assert!(!world.notices().is_empty());
	assert!(
		world
			.state_int("plan", "write_attempts")
			.is_some_and(|attempts| attempts <= 3)
	);
}

#[test]
fn test_plan_todos_are_scoped_and_root_is_restored_by_scope_drop() {
	let mut world = Harness::new();
	let plan = world.engage(Plan::new(PLAN_FILE));
	let node = world.session.dom().get(plan).expect("plan node");
	assert_eq!(
		node
			.prop(&PropKey::Custom("bind/todo_scope".into()))
			.and_then(omp_dom::Value::as_str),
		Some("scoped"),
	);
	world.add_todo("root item");
	assert_eq!(
		world
			.session
			.dom()
			.count("todo item[status!=completed]")
			.unwrap(),
		1
	);
}

#[test]
fn test_todo_reminder_sees_empty_plan_scope_not_pending_root_items() {
	let mut world = Harness::new();
	world.add_todo("root work");
	world.engage(TodoReminder::new(3));
	world.engage(Plan::new(PLAN_FILE));
	world.turn("stall", &[], 0);
	let texts = world.developer_texts();
	assert!(texts.iter().any(|text| text.contains(PLAN_FILE)));
	assert!(!texts.iter().any(|text| text.contains("Todo items remain")));
}
