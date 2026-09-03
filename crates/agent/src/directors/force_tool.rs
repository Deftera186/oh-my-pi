//! Forced-tool Director and its bounded escalation ladder.

use omp_core::Str;
use omp_dom::{Dom, Node};
use omp_inference::{ChatRequest, Setting, ToolChoice};

use crate::director::{
	BindValue, Director, DirectorCx, DirectorEffect, ForceUntil, StateUpdate, TurnView, Verdict,
	prepend_system, state_int, state_str, turn_called,
};

const SOFT_PROMPT_PREFIX: &str = "You must invoke the following tool before yielding: ";

/// Requires a named tool call before its parent may inspect the yield.
pub struct ForceTool {
	name:     Str,
	until:    ForceUntil,
	reminder: Option<Str>,
	retries:  u32,
	attempts: u32,
}

impl ForceTool {
	/// Creates a bounded forced-call engagement.
	#[must_use]
	pub fn new(
		name: impl Into<Str>,
		until: ForceUntil,
		reminder: Option<Str>,
		retries: u32,
	) -> Self {
		Self { name: name.into(), until, reminder, retries, attempts: 0 }
	}

	/// Reconstructs a forced-call engagement from DOM properties.
	#[must_use]
	pub fn from_node(node: &Node) -> Self {
		let name = state_str(node, "tool").unwrap_or_else(|| Str::new_static("required"));
		let until = match state_str(node, "until").as_deref() {
			Some("*") | None => ForceUntil::AnyToolCall,
			Some(tool) => ForceUntil::ToolCalled(Str::new(tool)),
		};
		let reminder = state_str(node, "reminder").filter(|value| !value.is_empty());
		let retries = state_int(node, "retries")
			.and_then(|value| u32::try_from(value).ok())
			.unwrap_or(3);
		let attempts = state_int(node, "attempts")
			.and_then(|value| u32::try_from(value).ok())
			.unwrap_or(0);
		Self { name, until, reminder, retries, attempts }
	}

	fn satisfied(&self, dom: &Dom, turn: &TurnView) -> bool {
		match &self.until {
			ForceUntil::AnyToolCall => turn.had_tool_calls,
			ForceUntil::ToolCalled(tool) => turn_called(dom, turn.turn, tool),
		}
	}
}

impl Director for ForceTool {
	fn id(&self) -> &'static str {
		"force_tool"
	}

	fn claims(&self) -> &'static [crate::director::Slot] {
		&[crate::director::Slot::ToolChoice]
	}

	fn state(&self) -> Vec<(Str, BindValue)> {
		vec![
			(Str::new_static("tool"), BindValue::Str(self.name.clone())),
			(
				Str::new_static("until"),
				BindValue::Str(match &self.until {
					ForceUntil::AnyToolCall => Str::new_static("*"),
					ForceUntil::ToolCalled(tool) => tool.clone(),
				}),
			),
			(Str::new_static("reminder"), BindValue::Str(self.reminder.clone().unwrap_or_default())),
			(Str::new_static("retries"), BindValue::Int(i64::from(self.retries))),
			(Str::new_static("attempts"), BindValue::Int(i64::from(self.attempts))),
		]
	}

	fn prepare_inference(&self, cx: &DirectorCx<'_>, req: &mut ChatRequest) {
		let directive = Str::new(format!("{SOFT_PROMPT_PREFIX}{}.", self.name));
		prepend_system(req, directive);
		if cx.route.forced_choice_free || self.attempts >= self.retries {
			req.tool_choice = Setting::Require(ToolChoice::Named(self.name.clone()));
		}
	}

	fn evaluate(&self, dom: &Dom, _cx: &DirectorCx<'_>, turn: &TurnView) -> DirectorEffect {
		if self.satisfied(dom, turn) {
			return DirectorEffect::new(Verdict::Done);
		}
		if self.attempts >= self.retries {
			return DirectorEffect::new(Verdict::Fail(Str::new(format!(
				"model did not call required tool {} after {} retries",
				self.name, self.retries
			))));
		}
		let reminder = self.reminder.clone().or_else(|| {
			Some(Str::new(format!("Call {} now; do not answer without using it.", self.name)))
		});
		DirectorEffect {
			verdict: Verdict::Continue { reminder },
			updates: vec![StateUpdate::new(
				"attempts",
				BindValue::Int(i64::from(self.attempts.saturating_add(1))),
			)],
			asides:  Vec::new(),
		}
	}
}
