//! Turn-boundary automatic lesson capture over the Core mailbox.

use std::mem;

use omp_core::{Str, StrMut, sf};
use omp_proto::{
	inference::v1::{Value, ValueMap, value},
	thread::v1::{self as thread, Item, item},
};

use crate::{Interrupt, InterruptClass, InterruptSource, PromptNamedInput};

/// Durable item property marking the private synthetic capture turn.
pub const CAPTURE_PROP: &str = "omp/autolearn-capture";
/// Minimum substantive-turn threshold.
pub const DEFAULT_MIN_TOOL_CALLS: usize = 5;

const GUIDANCE: &str = "## Auto-Learn (experimental)\n\n`manage_skill`: build reusable \
                        managed-skill library.\nManaged skills: `SKILL.md` in isolated \
                        `~/.omp/agent/managed-skills`; surfaced in future sessions like other \
                        skills.\n\nFor repeatable procedures worth codifying—setup sequences, \
                        debugging recipes, project-specific workflows—use `manage_skill` to \
                        `create` | `update` | `delete`.\nIsolation: managed skills ONLY writable \
                        skills. NEVER edit user-authored skills in `~/.omp/agent/skills` or \
                        `.omp/skills`.\nCapture sparingly, specifically: skill requires reuse; \
                        prefer enhancing existing managed skill to creating near-duplicate.";
const LEARN_GUIDANCE: &str = "Durable fact—not procedure—(project convention, non-obvious fix, \
                              user preference): record with `learn` → long-term memory.\nFact and \
                              procedure: same `learn` call MAY mint or enhance a managed skill.";
const CAPTURE_NUDGE: &str =
	"Automated capture turn — not a user reply; user has not responded to your previous turn. Do \
	 not treat this prompt as their answer, approval to continue, or acceptance of any pending \
	 action; only the user can do so.\n\nIf your previous turn produced reusable output, capture \
	 it now only if it will genuinely help next time: repeatable procedure → managed skill \
	 (`manage_skill`); durable fact, convention, or user preference → remember with `learn` when \
	 memory enabled. If nothing worth keeping, do nothing.\n\nThen stop. Do not run other tools, \
	 resume prior work, answer pending questions, or produce a continuation reply. Yield; wait for \
	 the user's next prompt.";

/// Live automatic-learning activation and budget policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutolearnSettings {
	/// Enables standing guidance and capture eligibility.
	pub enabled:        bool,
	/// Automatically schedules one private capture at a settled boundary.
	pub auto_continue:  bool,
	/// Minimum settled tool executions in one primary turn.
	pub min_tool_calls: usize,
}

impl Default for AutolearnSettings {
	fn default() -> Self {
		Self { enabled: false, auto_continue: false, min_tool_calls: DEFAULT_MIN_TOOL_CALLS }
	}
}

/// Boundary action selected by the controller.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CaptureDecision {
	/// Settle without synthetic work.
	#[default]
	None,
	/// Enqueue one capture through the Idle mailbox lane.
	Enqueue,
}

/// Agent-owned substantive-turn detector and non-overlap controller.
#[derive(Clone, Debug)]
pub struct AutolearnController {
	settings:                AutolearnSettings,
	settled_tool_calls:      usize,
	turn_started_suppressed: bool,
	capture_in_flight:       bool,
	capture_pending:         bool,
}

impl AutolearnController {
	/// Creates a controller from the current live settings projection.
	pub const fn new(settings: AutolearnSettings) -> Self {
		Self {
			settings,
			settled_tool_calls: 0,
			turn_started_suppressed: false,
			capture_in_flight: false,
			capture_pending: false,
		}
	}

	/// Replaces live settings; subsequent boundaries observe the new opt-out.
	pub const fn update_settings(&mut self, settings: AutolearnSettings) {
		self.settings = settings;
	}

	/// Begins one primary caller submission and clears all prior boundary facts.
	pub fn begin_primary(&mut self, prompt_slot: &str) {
		self.settled_tool_calls = 0;
		self.turn_started_suppressed = suppressed_prompt_slot(prompt_slot);
	}

	/// Counts one tool execution only after it reached a settled terminal
	/// outcome.
	pub const fn observe_settled_tool_execution(&mut self) {
		self.settled_tool_calls = self.settled_tool_calls.saturating_add(1);
	}

	/// Finishes one primary turn, resetting its counter before every eligibility
	/// gate.
	pub fn finish_primary(&mut self, ended_prompt_slot: &str, aborted: bool) -> CaptureDecision {
		let tool_calls = mem::take(&mut self.settled_tool_calls);
		let started_suppressed = mem::take(&mut self.turn_started_suppressed);
		let eligible = !aborted
			&& self.settings.enabled
			&& self.settings.auto_continue
			&& tool_calls >= self.settings.min_tool_calls
			&& !started_suppressed
			&& !suppressed_prompt_slot(ended_prompt_slot);
		if !eligible {
			return CaptureDecision::None;
		}
		if self.capture_in_flight {
			self.capture_pending = true;
			tracing::debug!(settled_tool_call_count = tool_calls, "autolearn capture coalesced");
			CaptureDecision::None
		} else {
			self.capture_in_flight = true;
			tracing::debug!(settled_tool_call_count = tool_calls, "autolearn capture scheduled");
			CaptureDecision::Enqueue
		}
	}

	/// Completes or aborts the current synthetic capture and starts at most one
	/// coalesced newer capture.
	pub const fn finish_capture(&mut self, aborted: bool) -> CaptureDecision {
		self.settled_tool_calls = 0;
		self.turn_started_suppressed = false;
		self.capture_in_flight = false;
		if aborted || !self.capture_pending || !self.settings.enabled || !self.settings.auto_continue
		{
			self.capture_pending = false;
			return CaptureDecision::None;
		}
		self.capture_pending = false;
		self.capture_in_flight = true;
		CaptureDecision::Enqueue
	}

	/// Clears every boundary and overlap latch after a caller abort or terminal
	/// fault.
	pub const fn abort(&mut self) {
		self.settled_tool_calls = 0;
		self.turn_started_suppressed = false;
		self.capture_in_flight = false;
		self.capture_pending = false;
	}

	/// Whether the turn currently running is the controller's private capture.
	pub const fn capture_in_flight(&self) -> bool {
		self.capture_in_flight
	}
}

fn suppressed_prompt_slot(prompt_slot: &str) -> bool {
	matches!(prompt_slot, "plan" | "plan-yolo" | "goal")
}

/// Builds stable standing guidance from the tools actually active for this
/// session.
pub fn standing_guidance(manage_skill: bool, learn: bool) -> Option<PromptNamedInput> {
	if !manage_skill {
		return None;
	}
	let content = if learn {
		let mut text = StrMut::with_capacity(GUIDANCE.len() + LEARN_GUIDANCE.len() + 2);
		text.push_str(GUIDANCE);
		text.push_str("\n\n");
		text.push_str(LEARN_GUIDANCE);
		text.freeze()
	} else {
		Str::new_static(GUIDANCE)
	};
	Some(PromptNamedInput { id: sf!("autolearn"), origin: sf!("builtin://autolearn"), content })
}

/// Builds the one synthetic Idle-mailbox capture input.
pub fn capture_interrupt() -> Interrupt {
	let mut props = ValueMap::default();
	props
		.fields
		.insert(CAPTURE_PROP.to_owned(), Value { kind: Some(value::Kind::Bool(true)) });
	Interrupt {
		class:  InterruptClass::Idle,
		item:   Item {
			seq:           0,
			created_at_ms: 0,
			kind:          Some(item::Kind::Message(thread::Message {
				role:            thread::Role::System as i32,
				parts:           vec![thread::Part {
					kind: Some(thread::part::Kind::Text(CAPTURE_NUDGE.to_owned())),
				}],
				synthetic:       None,
				user_initiated:  None,
				completed_at_ms: None,
				usage:           None,
			})),
			props:         Some(props),
		},
		source: InterruptSource::Producer(sf!("autolearn")),
	}
}

/// Returns whether a durable item is the private capture marker.
pub fn is_capture_item(item: &Item) -> bool {
	item
		.props
		.as_ref()
		.and_then(|props| props.fields.get(CAPTURE_PROP))
		.and_then(|value| value.kind.as_ref())
		.is_some_and(|kind| matches!(kind, value::Kind::Bool(true)))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn active() -> AutolearnController {
		AutolearnController::new(AutolearnSettings {
			enabled:        true,
			auto_continue:  true,
			min_tool_calls: DEFAULT_MIN_TOOL_CALLS,
		})
	}

	#[test]
	fn threshold_resets_at_every_primary_boundary() {
		let mut controller = active();
		controller.begin_primary("standard");
		for _ in 0..4 {
			controller.observe_settled_tool_execution();
		}
		assert_eq!(controller.finish_primary("standard", false), CaptureDecision::None);
		controller.begin_primary("standard");
		controller.observe_settled_tool_execution();
		assert_eq!(controller.finish_primary("standard", false), CaptureDecision::None);
	}

	#[test]
	fn abort_plan_and_goal_boundaries_are_suppressed() {
		for (start, end, aborted) in [
			("standard", "standard", true),
			("plan", "standard", false),
			("standard", "plan-yolo", false),
			("goal", "standard", false),
			("standard", "goal", false),
		] {
			let mut controller = active();
			controller.begin_primary(start);
			for _ in 0..DEFAULT_MIN_TOOL_CALLS {
				controller.observe_settled_tool_execution();
			}
			assert_eq!(controller.finish_primary(end, aborted), CaptureDecision::None);
		}
	}

	#[test]
	fn coalesces_one_pending_capture() {
		let mut controller = active();
		controller.begin_primary("standard");
		for _ in 0..DEFAULT_MIN_TOOL_CALLS {
			controller.observe_settled_tool_execution();
		}
		assert_eq!(controller.finish_primary("standard", false), CaptureDecision::Enqueue);
		controller.begin_primary("standard");
		for _ in 0..DEFAULT_MIN_TOOL_CALLS {
			controller.observe_settled_tool_execution();
		}
		assert_eq!(controller.finish_primary("standard", false), CaptureDecision::None);
		assert_eq!(controller.finish_capture(false), CaptureDecision::Enqueue);
		assert_eq!(controller.finish_capture(false), CaptureDecision::None);
	}
}
