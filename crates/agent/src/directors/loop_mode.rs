//! Fixed-count loop Director.

use omp_core::Str;
use omp_dom::{Dom, Node};

use crate::director::{
	BindValue, Director, DirectorCx, DirectorEffect, Slot, TurnView, Verdict, state_int, state_str,
};

const CLAIMS: &[Slot] = &[Slot::Loop];

/// Replays a prompt at yield up to an optional count.
pub struct LoopMode {
	prompt: Str,
	count:  Option<u32>,
	used:   u32,
}

impl LoopMode {
	/// Creates a loop engagement.
	#[must_use]
	pub fn new(prompt: impl Into<Str>, count: Option<u32>) -> Self {
		Self { prompt: prompt.into(), count, used: 0 }
	}

	/// Reconstructs loop state from its DOM element.
	#[must_use]
	pub fn from_node(node: &Node) -> Self {
		Self {
			prompt: state_str(node, "prompt").unwrap_or_default(),
			count:  state_int(node, "count").and_then(|value| u32::try_from(value).ok()),
			used:   state_int(node, "used")
				.and_then(|value| u32::try_from(value).ok())
				.unwrap_or(0),
		}
	}
}

impl Director for LoopMode {
	fn id(&self) -> &'static str {
		"loop_mode"
	}

	fn claims(&self) -> &'static [Slot] {
		CLAIMS
	}

	fn state(&self) -> Vec<(Str, BindValue)> {
		vec![
			(Str::new_static("prompt"), BindValue::Str(self.prompt.clone())),
			(Str::new_static("count"), BindValue::Int(self.count.map_or(-1, i64::from))),
			(Str::new_static("used"), BindValue::Int(i64::from(self.used))),
		]
	}

	fn evaluate(&self, _dom: &Dom, _cx: &DirectorCx<'_>, _turn: &TurnView) -> DirectorEffect {
		if self.count.is_some_and(|count| self.used >= count) {
			return DirectorEffect::new(Verdict::Done);
		}
		DirectorEffect::new(Verdict::Continue { reminder: Some(self.prompt.clone()) })
			.with_update("used", BindValue::Int(i64::from(self.used.saturating_add(1))))
	}
}
