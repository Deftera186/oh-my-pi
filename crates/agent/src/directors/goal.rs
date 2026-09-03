//! Goal-mode Director.

use omp_core::Str;
use omp_dom::{Dom, Node};

use crate::director::{
	BindValue, Director, DirectorCx, DirectorEffect, Slot, StateUpdate, TurnView, Verdict,
	state_bool, state_int, state_str, turn_call_inputs, turn_tokens,
};

const CLAIMS: &[Slot] = &[Slot::Mode, Slot::Loop];

/// Keeps the loop occupied until a goal completes, drops, or exhausts its token
/// budget.
pub struct Goal {
	objective:    Str,
	token_budget: Option<u64>,
	tokens_used:  u64,
	done:         bool,
	dropped:      bool,
}

impl Goal {
	/// Creates a goal engagement.
	#[must_use]
	pub fn new(objective: impl Into<Str>, token_budget: Option<u64>) -> Self {
		Self {
			objective: objective.into(),
			token_budget,
			tokens_used: 0,
			done: false,
			dropped: false,
		}
	}

	/// Reconstructs goal state from its DOM element.
	#[must_use]
	pub fn from_node(node: &Node) -> Self {
		Self {
			objective:    state_str(node, "objective").unwrap_or_default(),
			token_budget: state_int(node, "token_budget").and_then(|value| u64::try_from(value).ok()),
			tokens_used:  state_int(node, "tokens_used")
				.and_then(|value| u64::try_from(value).ok())
				.unwrap_or(0),
			done:         state_bool(node, "done").unwrap_or(false),
			dropped:      state_bool(node, "dropped").unwrap_or(false),
		}
	}
}

impl Director for Goal {
	fn id(&self) -> &'static str {
		"goal"
	}

	fn claims(&self) -> &'static [Slot] {
		CLAIMS
	}

	fn state(&self) -> Vec<(Str, BindValue)> {
		vec![
			(Str::new_static("objective"), BindValue::Str(self.objective.clone())),
			(
				Str::new_static("token_budget"),
				BindValue::Int(
					self
						.token_budget
						.and_then(|value| i64::try_from(value).ok())
						.unwrap_or(-1),
				),
			),
			(
				Str::new_static("tokens_used"),
				BindValue::Int(i64::try_from(self.tokens_used).unwrap_or(i64::MAX)),
			),
			(Str::new_static("done"), BindValue::Bool(self.done)),
			(Str::new_static("dropped"), BindValue::Bool(self.dropped)),
			(Str::new_static("tool"), BindValue::Str(Str::new_static("goal"))),
		]
	}

	fn observe_turn(&self, dom: &Dom, _cx: &DirectorCx<'_>, turn: &TurnView) -> Vec<StateUpdate> {
		let mut updates = vec![StateUpdate::new(
			"tokens_used",
			BindValue::Int(
				i64::try_from(self.tokens_used.saturating_add(turn_tokens(dom, turn.turn)))
					.unwrap_or(i64::MAX),
			),
		)];
		for input in turn_call_inputs(dom, turn.turn, "goal") {
			let op = serde_json::from_str::<serde_json::Value>(input)
				.ok()
				.and_then(|value| {
					value
						.get("op")
						.and_then(|op| op.as_str())
						.map(str::to_owned)
				});
			match op.as_deref() {
				Some("complete") => updates.push(StateUpdate::new("done", BindValue::Bool(true))),
				Some("drop") => updates.push(StateUpdate::new("dropped", BindValue::Bool(true))),
				_ => {},
			}
		}
		updates
	}

	fn evaluate(&self, _dom: &Dom, _cx: &DirectorCx<'_>, turn: &TurnView) -> DirectorEffect {
		if self.done || self.dropped {
			return DirectorEffect::new(Verdict::Done);
		}
		if self
			.token_budget
			.is_some_and(|budget| self.tokens_used >= budget)
		{
			return DirectorEffect::new(Verdict::Done)
				.with_aside("Goal token budget exhausted; returning control to the user.");
		}
		if !turn.had_tool_calls {
			return DirectorEffect::new(Verdict::Yield);
		}
		DirectorEffect::new(Verdict::Continue {
			reminder: Some(Str::new(format!("Continue toward the active goal: {}", self.objective))),
		})
	}
}
