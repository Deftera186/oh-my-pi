//! Plan composition and its write/decision gates.

use omp_core::Str;
use omp_dom::{Dom, Node};

use crate::director::{
	BindValue, Director, DirectorCx, DirectorEffect, ForceUntil, Slot, StateUpdate, TurnView,
	Verdict, state_bool, state_int, state_str, turn_call_inputs, turn_called,
};

const CLAIMS: &[Slot] = &[Slot::Mode, Slot::Worktree];

/// Requires a durable plan followed by an explicit user decision request.
pub struct Plan {
	plan_file:         Str,
	plan_written:      bool,
	decision_made:     bool,
	write_attempts:    u32,
	decision_attempts: u32,
	binds:             Vec<(Str, BindValue)>,
}

impl Plan {
	/// Creates a plan engagement for one local artifact.
	#[must_use]
	pub fn new(plan_file: impl Into<Str>) -> Self {
		Self {
			plan_file:         plan_file.into(),
			plan_written:      false,
			decision_made:     false,
			write_attempts:    0,
			decision_attempts: 0,
			binds:             plan_binds(),
		}
	}

	/// Reconstructs plan state from its DOM element.
	#[must_use]
	pub fn from_node(node: &Node) -> Self {
		Self {
			plan_file:         state_str(node, "plan_file")
				.unwrap_or_else(|| Str::new_static("local://plans/current.md")),
			plan_written:      state_bool(node, "plan_written").unwrap_or(false),
			decision_made:     state_bool(node, "decision_made").unwrap_or(false),
			write_attempts:    u32_value(state_int(node, "write_attempts")),
			decision_attempts: u32_value(state_int(node, "decision_attempts")),
			binds:             plan_binds(),
		}
	}
}

impl Director for Plan {
	fn id(&self) -> &'static str {
		"plan"
	}

	fn claims(&self) -> &'static [Slot] {
		CLAIMS
	}

	fn binds(&self) -> &[(Str, BindValue)] {
		&self.binds
	}

	fn state(&self) -> Vec<(Str, BindValue)> {
		vec![
			(Str::new_static("plan_file"), BindValue::Str(self.plan_file.clone())),
			(Str::new_static("plan_written"), BindValue::Bool(self.plan_written)),
			(Str::new_static("decision_made"), BindValue::Bool(self.decision_made)),
			(Str::new_static("write_attempts"), BindValue::Int(i64::from(self.write_attempts))),
			(Str::new_static("decision_attempts"), BindValue::Int(i64::from(self.decision_attempts))),
			(Str::new_static("tools"), BindValue::Str(Str::new_static("write,ask,xd://propose"))),
		]
	}

	fn observe_turn(&self, dom: &Dom, _cx: &DirectorCx<'_>, turn: &TurnView) -> Vec<StateUpdate> {
		let wrote_plan = call_wrote_path(dom, turn.turn, self.plan_file.as_str());
		let proposed =
			turn_called(dom, turn.turn, "ask") || call_wrote_path(dom, turn.turn, "xd://propose");
		let mut updates = Vec::with_capacity(2);
		if wrote_plan && !self.plan_written {
			updates.push(StateUpdate::new("plan_written", BindValue::Bool(true)));
		}
		if proposed && !self.decision_made {
			updates.push(StateUpdate::new("decision_made", BindValue::Bool(true)));
		}
		updates
	}

	fn evaluate(&self, _dom: &Dom, cx: &DirectorCx<'_>, _turn: &TurnView) -> DirectorEffect {
		if !self.plan_written {
			if self.write_attempts >= 3 {
				return DirectorEffect::new(Verdict::Yield);
			}
			let verdict = cx.force_tool(
				"write",
				ForceUntil::ToolCalled(Str::new_static("write")),
				Some(Str::new(format!(
					"Write the plan to {} before asking for approval.",
					self.plan_file
				))),
				3,
			);
			return DirectorEffect {
				verdict,
				updates: vec![StateUpdate::new(
					"write_attempts",
					BindValue::Int(i64::from(self.write_attempts + 1)),
				)],
				asides: vec![Str::new(format!(
					"Write the plan to {} before asking for approval.",
					self.plan_file
				))],
			};
		}
		if self.decision_made {
			return DirectorEffect::new(Verdict::Yield);
		}
		if self.decision_attempts >= 3 {
			return DirectorEffect::new(Verdict::Yield);
		}
		DirectorEffect {
			verdict: cx.force_tool(
				"required",
				ForceUntil::AnyToolCall,
				Some(Str::new_static(
					"Present the completed plan for an explicit decision before yielding.",
				)),
				3,
			),
			updates: vec![StateUpdate::new(
				"decision_attempts",
				BindValue::Int(i64::from(self.decision_attempts + 1)),
			)],
			asides:  Vec::new(),
		}
	}
}

fn plan_binds() -> Vec<(Str, BindValue)> {
	vec![
		(Str::new_static("prompt_slot"), BindValue::Str(Str::new_static("plan"))),
		(Str::new_static("model_route"), BindValue::Str(Str::new_static("@plan"))),
		(Str::new_static("todo_scope"), BindValue::Str(Str::new_static("scoped"))),
	]
}

fn call_wrote_path(dom: &Dom, turn: omp_dom::Handle, expected: &str) -> bool {
	turn_call_inputs(dom, turn, "write").any(|input| {
		serde_json::from_str::<serde_json::Value>(input)
			.ok()
			.and_then(|value| {
				value
					.get("path")
					.and_then(|path| path.as_str())
					.map(str::to_owned)
			})
			.is_some_and(|path| path == expected)
	})
}

fn u32_value(value: Option<i64>) -> u32 {
	value
		.and_then(|value| u32::try_from(value).ok())
		.unwrap_or(0)
}
