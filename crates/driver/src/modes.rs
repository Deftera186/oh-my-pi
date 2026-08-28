//! Application execution modes and autonomous goal-loop policy.

/// Durable encoding and restoration of autonomous regime state.
pub mod persistence;

use std::sync::{
	Arc,
	atomic::{AtomicU64, Ordering},
};

use omp_agent::{
	AgentState, CachedContribution, Continuation, ContinuationPolicy, ContinuationSource,
	LoopSignal, PromptBands, PromptError, PromptSlotSource, PromptSource, Props, RESOURCE_TABLE,
	RegimeRecord, RegimeSet, RegimeStatus, Resource, SlotAssembler, SlotClass, SlotDecl, SlotId,
	SlotRegistration,
};
use omp_core::{Str, sf};
/// One visible resource owner projected by the driver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleResourceFacts {
	/// Canonical resource name.
	pub resource:    Str,
	/// Regime declaration currently owning the resource.
	pub owner:       Str,
	/// Durable FIFO tickets waiting behind the owner.
	pub queue_depth: usize,
}
use omp_proto::thread::v1::{Item, Message, Part, Role, item, part};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
	goal::{self, report::GoalBudgetReport},
	plan::{
		ModelSelection, PlanModelTransition, PlanState, PlanWorkflow, TransitionQueue,
		artifacts::canonical_url,
	},
};

/// Process startup surface selected by CLI command or `--mode`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StartupMode {
	/// Interactive terminal chat.
	#[default]
	Interactive,
	/// Non-interactive single response.
	Print,
	/// Headless framed RPC.
	Rpc,
	/// Framed RPC with retained UI envelopes.
	RpcUi,
	/// Agent Client Protocol.
	Acp,
}

/// Mode-neutral protocol defaults. Protocol startup consumes these values
/// without mutating persisted interactive settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolDefaults {
	/// Automatic terminal titles.
	pub titles:             bool,
	/// PTY-backed shell execution.
	pub pty:                bool,
	/// Interactive splash/chrome.
	pub interactive_chrome: bool,
}

impl StartupMode {
	/// Returns invocation-local defaults for this mode.
	pub const fn defaults(self) -> ProtocolDefaults {
		match self {
			Self::Interactive => ProtocolDefaults {
				titles:             true,
				pty:                true,
				interactive_chrome: true,
			},
			Self::Print => ProtocolDefaults {
				titles:             false,
				pty:                false,
				interactive_chrome: false,
			},
			Self::Rpc | Self::RpcUi | Self::Acp => ProtocolDefaults {
				titles:             false,
				pty:                true,
				interactive_chrome: false,
			},
		}
	}

	/// Rejects `@file` shorthand in RPC UI, where stdin and references belong to
	/// the framed protocol.
	pub fn validate_prompt_words(self, words: &[Str]) -> Result<(), StartupRegimeError> {
		if self == Self::RpcUi && words.iter().any(|word| word.starts_with('@')) {
			return Err(StartupRegimeError::RpcUiReference);
		}
		Ok(())
	}
}

/// Startup-mode usage failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StartupRegimeError {
	/// RPC UI accepts attachments only through typed protocol frames.
	#[error("rpc-ui does not accept @file arguments")]
	RpcUiReference,
}

/// Durable goal lifecycle state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
	/// Objective is eligible for continuation.
	Active,
	/// User-paused without losing accounting.
	Paused,
	/// Hard token budget was reached.
	BudgetLimited,
	/// Objective was achieved.
	Complete,
	/// Objective was abandoned.
	Dropped,
}

/// Goal state projected to commands, prompts, and continuation policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Goal {
	/// Stable goal identity.
	pub id:                Str,
	/// User-authored objective.
	pub objective:         Str,
	/// Current lifecycle state.
	pub status:            GoalStatus,
	/// Optional hard token budget.
	pub token_budget:      Option<u64>,
	/// Counted tokens, excluding reused cache reads.
	pub tokens_used:       u64,
	/// Accumulated wall-clock seconds.
	pub time_used_seconds: u64,
	started_ms:            u64,
}

/// One provider usage delta folded into goal accounting.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalUsage {
	/// Fresh input tokens.
	pub input_tokens:        u64,
	/// Newly written cache tokens.
	pub cache_write_tokens:  u64,
	/// Reused cache tokens, intentionally excluded from spend.
	pub cached_input_tokens: u64,
	/// Generated output tokens.
	pub output_tokens:       u64,
}

impl GoalUsage {
	/// Returns budget spend while excluding reused cached input.
	pub const fn charged_tokens(self) -> u64 {
		self
			.input_tokens
			.saturating_add(self.cache_write_tokens)
			.saturating_add(self.output_tokens)
	}
}

/// Invalid goal or regime-projection transition.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RegimeError {
	/// An operation requires a regime that is not the visible mode holder.
	#[error("the {required} regime is not active")]
	RegimeInactive {
		/// Required regime declaration.
		required: &'static str,
	},
	/// A goal operation was requested without a goal.
	#[error("no goal is configured")]
	NoGoal,
	/// The objective was empty.
	#[error("goal objective must not be empty")]
	EmptyObjective,
	/// A zero token budget was supplied.
	#[error("goal token budget must be positive")]
	InvalidBudget,
	/// The plan artifact was not a canonical session-local URL.
	#[error("plan artifact must be a relative local:// URL")]
	InvalidPlanArtifact,
	/// The requested lifecycle transition is invalid for the current goal.
	#[error("cannot {operation} a goal in {status:?} state")]
	InvalidGoalTransition {
		/// Requested lifecycle operation.
		operation: &'static str,
		/// Current durable state.
		status:    GoalStatus,
	},
	/// A new goal cannot replace a live goal.
	#[error("cannot create a new goal while an unfinished goal exists")]
	GoalExists,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LoopLimit {
	Iterations { initial: u64, remaining: u64 },
	Duration { duration_ms: u64, deadline_ms: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LoopMode {
	prompt: Option<Str>,
	limit:  Option<LoopLimit>,
}

/// Result of toggling interactive loop mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoopCommandOutcome {
	/// An existing loop was disabled.
	Disabled,
	/// A loop was armed, optionally with an inline first prompt.
	Enabled {
		/// Inline prompt submitted through the ordinary prompt path.
		prompt:  Option<Str>,
		/// Operator-facing summary of the active bounds.
		message: Str,
	},
}

/// Invalid bounded-loop arguments.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LoopCommandError {
	/// The leading token looked like a bound but was not valid.
	#[error("Usage: /loop [count|duration]. Examples: /loop 10, /loop 10m, /loop 10min.")]
	Usage,
	/// Iteration counts must be non-zero integers.
	#[error("Loop count must be a positive integer.")]
	Count,
	/// Durations must be non-zero.
	#[error("Loop duration must be positive.")]
	Duration,
	/// Only seconds, minutes, and hours are accepted.
	#[error("Loop duration unit must be seconds, minutes, or hours.")]
	Unit,
}

struct ParsedLoopArgs {
	limit:  Option<LoopLimit>,
	prompt: Option<Str>,
}

fn duration_unit_ms(unit: &str) -> Option<u64> {
	match unit {
		"s" | "sec" | "secs" | "second" | "seconds" => Some(1_000),
		"m" | "min" | "mins" | "minute" | "minutes" => Some(60_000),
		"h" | "hr" | "hrs" | "hour" | "hours" => Some(3_600_000),
		_ => None,
	}
}

fn positive_amount(text: &str, duration: bool) -> Result<u64, LoopCommandError> {
	let amount = text.parse::<u64>().map_err(|_| {
		if duration {
			LoopCommandError::Duration
		} else {
			LoopCommandError::Count
		}
	})?;
	if amount == 0 {
		return Err(if duration {
			LoopCommandError::Duration
		} else {
			LoopCommandError::Count
		});
	}
	Ok(amount)
}

fn duration_limit(amount: &str, unit_ms: u64) -> Result<LoopLimit, LoopCommandError> {
	let duration_ms = positive_amount(amount, true)?
		.checked_mul(unit_ms)
		.ok_or(LoopCommandError::Duration)?;
	Ok(LoopLimit::Duration { duration_ms, deadline_ms: 0 })
}

fn compound_duration(token: &str) -> Option<Result<LoopLimit, LoopCommandError>> {
	let bytes = token.as_bytes();
	if bytes.is_empty() || !bytes[0].is_ascii_digit() || !bytes.iter().any(u8::is_ascii_alphabetic) {
		return None;
	}
	if !bytes.iter().all(u8::is_ascii_alphanumeric)
		|| !bytes.last().is_some_and(u8::is_ascii_alphabetic)
	{
		return Some(Err(LoopCommandError::Usage));
	}
	let mut at = 0;
	let mut total = 0_u64;
	while at < bytes.len() {
		let amount_start = at;
		while at < bytes.len() && bytes[at].is_ascii_digit() {
			at += 1;
		}
		if amount_start == at {
			return Some(Err(LoopCommandError::Usage));
		}
		let unit_start = at;
		while at < bytes.len() && bytes[at].is_ascii_alphabetic() {
			at += 1;
		}
		if unit_start == at {
			return Some(Err(LoopCommandError::Usage));
		}
		let unit = &token[unit_start..at];
		let Some(unit_ms) = duration_unit_ms(unit) else {
			return Some(Err(LoopCommandError::Unit));
		};
		let amount = match positive_amount(&token[amount_start..unit_start], true) {
			Ok(amount) => amount,
			Err(error) => return Some(Err(error)),
		};
		let Some(segment) = amount.checked_mul(unit_ms) else {
			return Some(Err(LoopCommandError::Duration));
		};
		let Some(next) = total.checked_add(segment) else {
			return Some(Err(LoopCommandError::Duration));
		};
		total = next;
	}
	Some(if total == 0 {
		Err(LoopCommandError::Duration)
	} else {
		Ok(LoopLimit::Duration { duration_ms: total, deadline_ms: 0 })
	})
}

fn parse_loop_args(args: &str) -> Result<ParsedLoopArgs, LoopCommandError> {
	let trimmed = args.trim();
	if trimmed.is_empty() {
		return Ok(ParsedLoopArgs { limit: None, prompt: None });
	}
	let first_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
	let first = &trimmed[..first_end];
	let rest = trimmed[first_end..].trim();
	let lower = first.to_ascii_lowercase();
	let limit_shaped = lower
		.as_bytes()
		.first()
		.is_some_and(|first| first.is_ascii_digit() || matches!(first, b'+' | b'-'));
	if !limit_shaped {
		return Ok(ParsedLoopArgs { limit: None, prompt: Some(Str::new(trimmed)) });
	}
	if lower.bytes().all(|byte| byte.is_ascii_digit()) {
		let amount = positive_amount(lower.as_str(), false)?;
		let mut rest_tokens = rest.split_whitespace();
		if let Some(unit) = rest_tokens.next()
			&& let Some(unit_ms) = duration_unit_ms(&unit.to_ascii_lowercase())
		{
			let limit = duration_limit(lower.as_str(), unit_ms)?;
			let prompt = rest_tokens.collect::<Vec<_>>().join(" ");
			return Ok(ParsedLoopArgs {
				limit:  Some(limit),
				prompt: (!prompt.is_empty()).then(|| Str::from(prompt)),
			});
		}
		return Ok(ParsedLoopArgs {
			limit:  Some(LoopLimit::Iterations { initial: amount, remaining: amount }),
			prompt: (!rest.is_empty()).then(|| Str::new(rest)),
		});
	}
	if let Some(limit) = compound_duration(lower.as_str()) {
		return Ok(ParsedLoopArgs {
			limit:  Some(limit?),
			prompt: (!rest.is_empty()).then(|| Str::new(rest)),
		});
	}
	Err(LoopCommandError::Usage)
}

fn format_duration(duration_ms: u64) -> String {
	if duration_ms % 3_600_000 == 0 {
		let hours = duration_ms / 3_600_000;
		format!("{hours} {}", if hours == 1 { "hour" } else { "hours" })
	} else if duration_ms % 60_000 == 0 {
		let minutes = duration_ms / 60_000;
		format!("{minutes} {}", if minutes == 1 { "minute" } else { "minutes" })
	} else {
		let seconds = duration_ms / 1_000;
		format!("{seconds} {}", if seconds == 1 { "second" } else { "seconds" })
	}
}

fn describe_loop_limit(limit: &LoopLimit) -> Str {
	match limit {
		LoopLimit::Iterations { initial, remaining } => sf!(
			"{remaining} of {initial} {} remaining",
			if *initial == 1 {
				"iteration"
			} else {
				"iterations"
			}
		),
		LoopLimit::Duration { duration_ms, .. } => sf!("{} limit", format_duration(*duration_ms)),
	}
}

/// App-owned metadata paired with the authoritative regime-resource projection.
#[derive(Debug, Default)]
struct RegimeProjectionState {
	mode_holder:              Option<Str>,
	mode_activation:          Option<Str>,
	visible_resources:        Arc<[VisibleResourceFacts]>,
	goal:                     Option<Goal>,
	plan:                     PlanState,
	plan_seen:                bool,
	outcome_usage_cumulative: GoalUsage,
	goal_usage_checkpoint:    GoalUsage,
	budget_steering_pending:  bool,
	goal_todo_context:        Option<Str>,
	loop_mode:                Option<LoopMode>,
}

#[derive(Clone, Debug)]
struct PlanBinding {
	agent:     AgentState,
	selection: Option<ModelSelection>,
	handoff:   Option<ModelSelection>,
}

/// Read projection of the agent-owned [`RegimeSet`] plus app goal/plan
/// metadata.
#[derive(Clone, Debug)]
pub struct RegimeHandle {
	state:            Arc<Mutex<RegimeProjectionState>>,
	policy:           ContinuationPolicy,
	plan_binding:     Arc<Mutex<Option<PlanBinding>>>,
	plan_transitions: Arc<TransitionQueue>,
	revision:         Arc<AtomicU64>,
}
struct ModeAwarePromptSource {
	base:  Arc<dyn PromptSource>,
	modes: RegimeHandle,
}

impl ModeAwarePromptSource {
	fn registrations(&self) -> Vec<SlotRegistration> {
		let mut registrations = Vec::new();
		if let Some(slot) = self.modes.mode_holder() {
			registrations.push(PromptSlotSource::new(slot).registration());
		}
		if let Some(goal) = self
			.modes
			.holds_mode("goal")
			.then(|| self.modes.goal())
			.flatten()
			.filter(|goal| goal.status == GoalStatus::Active)
		{
			let todo = self.modes.goal_todo_context();
			registrations.push(SlotRegistration {
				decl:   SlotDecl {
					slot:     SlotId::Status,
					class:    SlotClass::Volatile,
					owner:    sf!("omp.goal"),
					priority: 110,
				},
				source: Arc::new(CachedContribution::new(goal::prompt_context(&goal, todo.as_deref()))),
			});
		}
		registrations
	}
}

impl PromptSource for ModeAwarePromptSource {
	fn render(&self, workspace: &Props) -> Result<Vec<Item>, PromptError> {
		let Some(bands) = self.banded_items_render(workspace)? else {
			return self.base.render(workspace);
		};
		Ok(bands.into_items())
	}

	fn banded_items_render(&self, workspace: &Props) -> Result<Option<PromptBands>, PromptError> {
		let Some(mut bands) = self.base.banded_items_render(workspace)? else {
			return Ok(None);
		};
		let registrations = self.registrations();
		if !registrations.is_empty()
			&& let Some(extra) = SlotAssembler::new(registrations).banded_items_render(workspace)?
		{
			bands.append(extra);
		}
		Ok(Some(bands))
	}
}

impl Default for RegimeHandle {
	fn default() -> Self {
		Self::new()
	}
}

impl RegimeHandle {
	/// Creates an empty read projection. [`Self::sync_regimes`] supplies
	/// authority.
	pub fn new() -> Self {
		Self {
			state:            Arc::new(Mutex::new(RegimeProjectionState::default())),
			policy:           ContinuationPolicy::default(),
			plan_binding:     Arc::new(Mutex::new(None)),
			plan_transitions: Arc::new(TransitionQueue::default()),
			revision:         Arc::new(AtomicU64::new(0)),
		}
	}

	/// Toggles prompt repetition, returning an inline prompt for normal
	/// submission.
	pub fn toggle_loop(
		&self,
		args: &str,
		now_ms: u64,
	) -> Result<LoopCommandOutcome, LoopCommandError> {
		{
			let mut state = self.state.lock();
			if state.loop_mode.take().is_some() {
				return Ok(LoopCommandOutcome::Disabled);
			}
		}
		let mut parsed = parse_loop_args(args)?;
		if let Some(LoopLimit::Duration { duration_ms, deadline_ms }) = parsed.limit.as_mut() {
			*deadline_ms = now_ms.saturating_add(*duration_ms);
		}
		let limit_message = parsed
			.limit
			.as_ref()
			.map(|limit| sf!(" {}.", describe_loop_limit(limit)))
			.unwrap_or_default();
		let prompt = parsed.prompt.clone();
		self.state.lock().loop_mode = Some(LoopMode { prompt: parsed.prompt, limit: parsed.limit });
		let tail = if prompt.is_some() {
			"Repeating it after each turn."
		} else {
			"Your next prompt will repeat after each turn."
		};
		Ok(LoopCommandOutcome::Enabled {
			prompt,
			message: sf!(
				"Loop mode enabled.{limit_message} {tail} Esc cancels the current iteration; /loop \
				 again to disable."
			),
		})
	}

	/// Captures the first ordinary prompt submitted after an unbound loop is
	/// armed.
	pub fn capture_loop_prompt(&self, prompt: &str) {
		let mut state = self.state.lock();
		if let Some(loop_mode) = state.loop_mode.as_mut()
			&& loop_mode.prompt.is_none()
		{
			loop_mode.prompt = Some(Str::new(prompt));
		}
	}

	/// Pauses repetition until the next ordinary prompt is captured.
	pub fn pause_loop(&self) {
		if let Some(loop_mode) = self.state.lock().loop_mode.as_mut() {
			loop_mode.prompt = None;
		}
	}

	/// Returns a concise projection of interactive loop state.
	pub fn loop_status(&self) -> Str {
		let state = self.state.lock();
		let Some(loop_mode) = state.loop_mode.as_ref() else {
			return sf!("Loop: off");
		};
		let phase = if loop_mode.prompt.is_some() {
			"running"
		} else {
			"waiting"
		};
		loop_mode.limit.as_ref().map_or_else(
			|| sf!("Loop: {phase}"),
			|limit| sf!("Loop: {phase} ({})", describe_loop_limit(limit)),
		)
	}

	fn loop_continuation(&self, now_ms: u64) -> Option<Continuation> {
		let active = self.state.lock().loop_mode.is_some();
		let Some(prompt) = self.take_loop_prompt(now_ms) else {
			return active.then_some(Continuation::Settle);
		};
		Some(Continuation::Continue {
			owner:          sf!("loop"),
			item:           user_item(prompt, now_ms),
			label:          Some(sf!("loop")),
			collapse_prior: false,
		})
	}

	/// Consumes one authorized loop repetition at a settled turn boundary.
	pub fn take_loop_prompt(&self, now_ms: u64) -> Option<Str> {
		let mut state = self.state.lock();
		let loop_mode = state.loop_mode.as_mut()?;
		let Some(prompt) = loop_mode.prompt.clone() else {
			return None;
		};
		match loop_mode.limit.as_mut() {
			Some(LoopLimit::Iterations { remaining: 0, .. }) => {
				state.loop_mode = None;
				return None;
			},
			Some(LoopLimit::Iterations { remaining, .. }) => {
				*remaining -= 1;
			},
			Some(LoopLimit::Duration { deadline_ms, .. }) if now_ms >= *deadline_ms => {
				state.loop_mode = None;
				return None;
			},
			Some(LoopLimit::Duration { .. }) | None => {},
		}
		Some(prompt)
	}

	/// Refreshes user-facing resource facts from the authoritative agent regime
	/// set.
	pub fn sync_regimes(&self, regimes: &RegimeSet) {
		let resources = [
			Resource::Worktree,
			Resource::Director,
			Resource::EditorSurface,
			Resource::BatchExecution,
			Resource::Mode,
		];
		let visible_resources = resources
			.iter()
			.filter(|resource| {
				regimes
					.resources()
					.declaration(resource)
					.is_some_and(|declaration| declaration.visible)
			})
			.filter_map(|resource| {
				let activation = regimes.resources().owner(resource)?;
				let owner = regimes.spec_id(activation).unwrap_or(activation);
				Some(VisibleResourceFacts {
					resource:    Str::new(resource.name()),
					owner:       Str::new(owner),
					queue_depth: regimes.resources().queue_depth(resource),
				})
			})
			.collect::<Vec<_>>();
		let mode_activation = regimes.resources().owner(&Resource::Mode);
		let mode_holder = mode_activation.and_then(|id| regimes.spec_id(id));
		self.apply_projection(
			visible_resources.into(),
			mode_holder.map(Str::new),
			mode_activation.map(Str::new),
		);
	}

	/// Refreshes resource facts from regime records returned by an actor
	/// command.
	pub fn sync_records(&self, records: &[RegimeRecord]) {
		let regimes = records
			.iter()
			.filter_map(|entry| {
				omp_agent::core_regime(entry.spec_id.as_str()).map(|(spec, _)| (entry, spec))
			})
			.collect::<Vec<_>>();
		let visible_resources = RESOURCE_TABLE
			.iter()
			.filter(|declaration| declaration.visible)
			.filter_map(|declaration| {
				let owner = regimes.iter().find(|(entry, spec)| {
					entry.status == RegimeStatus::Active
						&& spec
							.owns
							.iter()
							.any(|resource| resource.name() == declaration.name)
				})?;
				let queue_depth = regimes
					.iter()
					.filter(|(entry, spec)| {
						entry.status == RegimeStatus::Queued
							&& spec
								.owns
								.iter()
								.any(|resource| resource.name() == declaration.name)
					})
					.count();
				Some(VisibleResourceFacts {
					resource: Str::new_static(declaration.name),
					owner: owner.0.spec_id.clone(),
					queue_depth,
				})
			})
			.collect::<Vec<_>>();
		let mode = regimes.iter().find(|(entry, spec)| {
			entry.status == RegimeStatus::Active
				&& spec.owns.iter().any(|resource| resource == &Resource::Mode)
		});
		self.apply_projection(
			visible_resources.into(),
			mode.map(|(entry, _)| entry.spec_id.clone()),
			mode.map(|(entry, _)| entry.activation.clone()),
		);
	}

	fn apply_projection(
		&self,
		visible_resources: Arc<[VisibleResourceFacts]>,
		mode_holder: Option<Str>,
		mode_activation: Option<Str>,
	) {
		let mut state = self.state.lock();
		let previous_holder = state.mode_holder.clone();
		state.visible_resources = visible_resources;
		state.mode_holder = mode_holder;
		state.mode_activation = mode_activation;
		let plan_entered =
			previous_holder.as_deref() != Some("plan") && state.mode_holder.as_deref() == Some("plan");
		let plan_exited =
			previous_holder.as_deref() == Some("plan") && state.mode_holder.as_deref() != Some("plan");
		drop(state);
		if plan_entered {
			self.activate_plan(false);
		} else if plan_exited {
			self.deactivate_plan();
		}
		self.revision.fetch_add(1, Ordering::Release);
	}

	/// Returns the monotonic projection revision used by retained UI refresh.
	pub fn revision(&self) -> u64 {
		self.revision.load(Ordering::Acquire)
	}

	/// Returns the visible resource projection without rebuilding it per frame.
	pub fn visible_resources(&self) -> Arc<[VisibleResourceFacts]> {
		Arc::clone(&self.state.lock().visible_resources)
	}

	/// Returns the visible mode-holder declaration.
	pub fn mode_holder(&self) -> Option<Str> {
		self.state.lock().mode_holder.clone()
	}

	/// Returns the current mode-holder activation identity.
	pub fn mode_activation(&self) -> Option<Str> {
		self.state.lock().mode_activation.clone()
	}

	/// Returns whether `owner` owns the canonical mode resource.
	pub fn holds_mode(&self, owner: &str) -> bool {
		self
			.state
			.lock()
			.mode_holder
			.as_ref()
			.is_some_and(|active| active == owner)
	}

	/// Binds the active agent selection authority. Plan entry and exit then
	/// apply model/thinking changes without provider mutation during streaming.
	pub fn bind_plan_selection(&self, agent: AgentState, selection: Option<ModelSelection>) {
		*self.plan_binding.lock() = Some(PlanBinding { agent, selection, handoff: None });
	}

	/// Arms a one-shot selection applied when the plan regime exits,
	/// replacing restoration of the pre-plan selection (`--plan-yolo-into`).
	///
	/// Requires a prior [`Self::bind_plan_selection`]; the handoff is consumed
	/// by the first plan exit and later plan cycles restore normally.
	pub fn bind_plan_handoff(&self, selection: ModelSelection) {
		if let Some(binding) = self.plan_binding.lock().as_mut() {
			binding.handoff = Some(selection);
		}
	}

	/// Marks the current inference stream active for deferred plan transitions.
	pub fn begin_streaming(&self) {
		self.plan_transitions.begin_streaming();
	}

	/// Applies the newest queued plan transition at settlement.
	pub fn settle_plan_transition(&self) -> PlanModelTransition {
		self
			.plan_binding
			.lock()
			.as_ref()
			.map_or(PlanModelTransition::Unchanged, |binding| {
				self.plan_transitions.settle(&binding.agent)
			})
	}

	/// Applies app plan metadata after the plan regime acquires the mode slot.
	pub fn activate_plan(&self, _plan_yolo: bool) {
		{
			let mut state = self.state.lock();
			let previous = state.plan_seen.then(|| state.plan.clone());
			state.plan = PlanState::entered(previous.as_ref());
			state.plan_seen = true;
		}
		if let Some(binding) = self.plan_binding.lock().as_ref() {
			self
				.plan_transitions
				.enter(&binding.agent, binding.selection.clone());
		}
	}

	/// Returns the durable plan projection.
	pub fn plan(&self) -> Option<PlanState> {
		let state = self.state.lock();
		state.plan_seen.then(|| state.plan.clone())
	}

	/// Selects the approved-plan workflow.
	pub fn set_plan_workflow(&self, workflow: PlanWorkflow) -> Result<PlanState, RegimeError> {
		if !self.holds_mode("plan") {
			return Err(RegimeError::RegimeInactive { required: "plan" });
		}
		let plan = {
			let mut state = self.state.lock();
			if !state.plan.enabled {
				return Err(RegimeError::RegimeInactive { required: "plan" });
			}
			state.plan.workflow = workflow;
			state.plan.clone()
		};
		Ok(plan)
	}

	/// Replaces the canonical active plan artifact reference.
	pub fn set_plan_artifact(&self, artifact: impl Into<Str>) -> Result<PlanState, RegimeError> {
		let artifact = artifact.into();
		let artifact =
			canonical_url(artifact.as_str()).map_err(|_| RegimeError::InvalidPlanArtifact)?;
		if !self.holds_mode("plan") {
			return Err(RegimeError::RegimeInactive { required: "plan" });
		}
		let plan = {
			let mut state = self.state.lock();
			if !state.plan.enabled {
				return Err(RegimeError::RegimeInactive { required: "plan" });
			}
			state.plan.artifact = artifact;
			state.plan.clone()
		};
		Ok(plan)
	}

	/// Wraps an existing prompt source with the active mode `SlotSource`.
	pub fn prompt_source(&self, base: Arc<dyn PromptSource>) -> Arc<dyn PromptSource> {
		Arc::new(ModeAwarePromptSource { base, modes: self.clone() })
	}

	/// Applies app plan metadata after the plan regime releases the mode slot.
	pub fn deactivate_plan(&self) {
		let mut state = self.state.lock();
		state.plan = state.plan.exited();
		drop(state);
		if let Some(binding) = self.plan_binding.lock().as_mut() {
			match binding.handoff.take() {
				Some(target) => {
					self.plan_transitions.exit_into(&binding.agent, target);
				},
				None => {
					self.plan_transitions.exit(&binding.agent);
				},
			}
		}
	}

	/// Creates or replaces the active goal.
	pub fn set_goal(
		&self,
		objective: impl Into<Str>,
		token_budget: Option<u64>,
		now_ms: u64,
	) -> Result<Goal, RegimeError> {
		let objective = objective.into();
		if objective.as_str().trim().is_empty() {
			return Err(RegimeError::EmptyObjective);
		}
		if token_budget == Some(0) {
			return Err(RegimeError::InvalidBudget);
		}
		let mut state = self.state.lock();
		if state
			.goal
			.as_ref()
			.is_some_and(|goal| !matches!(goal.status, GoalStatus::Complete | GoalStatus::Dropped))
		{
			return Err(RegimeError::GoalExists);
		}
		let goal = Goal {
			id: Str::from(omp_core::Ulid::generate().to_string()),
			objective,
			status: GoalStatus::Active,
			token_budget,
			tokens_used: 0,
			time_used_seconds: 0,
			started_ms: now_ms,
		};
		state.goal = Some(goal.clone());
		state.budget_steering_pending = false;
		Ok(goal)
	}

	/// Returns the latest goal projection.
	pub fn goal(&self) -> Option<Goal> {
		self.state.lock().goal.clone()
	}

	/// Replaces the live todo context injected through the goal status slot.
	pub fn set_goal_todo_context(&self, todo: Option<Str>) {
		self.state.lock().goal_todo_context = todo;
	}

	/// Returns the current goal todo context.
	pub fn goal_todo_context(&self) -> Option<Str> {
		self.state.lock().goal_todo_context.clone()
	}

	/// Pauses active or budget-limited goal continuation and accounting time.
	pub fn pause_goal(&self, now_ms: u64) -> Result<Goal, RegimeError> {
		let status = self.goal().ok_or(RegimeError::NoGoal)?.status;
		if !matches!(status, GoalStatus::Active | GoalStatus::BudgetLimited) {
			return Err(RegimeError::InvalidGoalTransition { operation: "pause", status });
		}
		let goal = self.update_goal(now_ms, |goal| goal.status = GoalStatus::Paused)?;
		self.state.lock().budget_steering_pending = false;
		Ok(goal)
	}

	/// Resumes a paused, dropped, or budget-limited goal.
	pub fn resume_goal(&self, now_ms: u64) -> Result<Goal, RegimeError> {
		let status = self.goal().ok_or(RegimeError::NoGoal)?.status;
		if !matches!(status, GoalStatus::Paused | GoalStatus::Dropped | GoalStatus::BudgetLimited) {
			return Err(RegimeError::InvalidGoalTransition { operation: "resume", status });
		}
		let goal = self.update_goal(now_ms, |goal| goal.status = GoalStatus::Active)?;
		let mut state = self.state.lock();
		state.budget_steering_pending = false;
		Ok(goal)
	}

	/// Marks the goal complete and leaves goal mode.
	pub fn complete_goal(&self, now_ms: u64) -> Result<Goal, RegimeError> {
		let status = self.goal().ok_or(RegimeError::NoGoal)?.status;
		if matches!(status, GoalStatus::Complete | GoalStatus::Dropped) {
			return Err(RegimeError::InvalidGoalTransition { operation: "complete", status });
		}
		self.finish_goal(now_ms, GoalStatus::Complete)
	}

	/// Drops the goal and leaves goal mode.
	pub fn drop_goal(&self, now_ms: u64) -> Result<Goal, RegimeError> {
		let status = self.goal().ok_or(RegimeError::NoGoal)?.status;
		if status == GoalStatus::Dropped {
			return Err(RegimeError::InvalidGoalTransition { operation: "drop", status });
		}
		self.finish_goal(now_ms, GoalStatus::Dropped)
	}

	/// Returns the exact model-visible completion accounting report.
	pub fn goal_completion_report(&self) -> Result<Str, RegimeError> {
		let goal = self.goal().ok_or(RegimeError::NoGoal)?;
		if goal.status != GoalStatus::Complete {
			return Err(RegimeError::InvalidGoalTransition {
				operation: "report completion for",
				status:    goal.status,
			});
		}
		Ok(GoalBudgetReport::from_goal(&goal).model_prompt())
	}

	/// Replaces the hard token budget.
	pub fn set_goal_budget(&self, budget: u64) -> Result<Goal, RegimeError> {
		if budget == 0 {
			return Err(RegimeError::InvalidBudget);
		}
		let mut state = self.state.lock();
		let (goal, limited) = {
			let goal = state.goal.as_mut().ok_or(RegimeError::NoGoal)?;
			goal.token_budget = Some(budget);
			let limited = goal.tokens_used >= budget;
			if limited {
				goal.status = GoalStatus::BudgetLimited;
			}
			(goal.clone(), limited)
		};
		if limited {
			state.budget_steering_pending = true;
		} else if goal.status == GoalStatus::BudgetLimited {
			let goal = state
				.goal
				.as_mut()
				.expect("goal exists while updating its budget");
			goal.status = GoalStatus::Active;
			state.budget_steering_pending = false;
		}
		let goal = state
			.goal
			.clone()
			.expect("goal exists while updating its budget");
		Ok(goal)
	}

	/// Charges one usage delta and applies the hard budget transition.
	pub fn record_goal_usage(&self, usage: GoalUsage, now_ms: u64) -> Result<Goal, RegimeError> {
		let mut state = self.state.lock();
		let (goal, limited) = {
			let goal = state.goal.as_mut().ok_or(RegimeError::NoGoal)?;
			if goal.status == GoalStatus::Active {
				goal.tokens_used = goal.tokens_used.saturating_add(usage.charged_tokens());
				goal.time_used_seconds = goal
					.time_used_seconds
					.saturating_add(now_ms.saturating_sub(goal.started_ms) / 1_000);
				goal.started_ms = now_ms;
				if goal
					.token_budget
					.is_some_and(|budget| goal.tokens_used >= budget)
				{
					goal.status = GoalStatus::BudgetLimited;
				}
			}
			(goal.clone(), goal.status == GoalStatus::BudgetLimited)
		};
		if limited {
			state.budget_steering_pending = true;
		}
		Ok(goal)
	}

	/// Checkpoints cumulative provider usage at a non-goal tool boundary.
	///
	/// The stored checkpoint prevents double charging when the same cumulative
	/// receipt is observed at both a tool boundary and turn settlement.
	pub fn checkpoint_goal_usage(
		&self,
		cumulative: GoalUsage,
		now_ms: u64,
	) -> Result<Goal, RegimeError> {
		let delta = {
			let mut state = self.state.lock();
			let previous = state.goal_usage_checkpoint;
			state.goal_usage_checkpoint = cumulative;
			GoalUsage {
				input_tokens:        cumulative
					.input_tokens
					.saturating_sub(previous.input_tokens),
				cache_write_tokens:  cumulative
					.cache_write_tokens
					.saturating_sub(previous.cache_write_tokens),
				cached_input_tokens: cumulative
					.cached_input_tokens
					.saturating_sub(previous.cached_input_tokens),
				output_tokens:       cumulative
					.output_tokens
					.saturating_sub(previous.output_tokens),
			}
		};
		self.record_goal_usage(delta, now_ms)
	}

	/// Records one provider-turn usage delta while advancing the same
	/// monotonic session checkpoint used by cumulative tool receipts.
	///
	/// Call this for `Outcome.usage`, which is per-turn rather than cumulative.
	/// The checkpoint advances even when no goal is active so a later goal
	/// starts at the current session baseline instead of charging earlier work.
	pub fn record_goal_usage_delta(
		&self,
		delta: GoalUsage,
		now_ms: u64,
	) -> Result<Goal, RegimeError> {
		let cumulative = {
			let mut state = self.state.lock();
			state.outcome_usage_cumulative.input_tokens = state
				.outcome_usage_cumulative
				.input_tokens
				.saturating_add(delta.input_tokens);
			state.outcome_usage_cumulative.cache_write_tokens = state
				.outcome_usage_cumulative
				.cache_write_tokens
				.saturating_add(delta.cache_write_tokens);
			state.outcome_usage_cumulative.cached_input_tokens = state
				.outcome_usage_cumulative
				.cached_input_tokens
				.saturating_add(delta.cached_input_tokens);
			state.outcome_usage_cumulative.output_tokens = state
				.outcome_usage_cumulative
				.output_tokens
				.saturating_add(delta.output_tokens);
			state.outcome_usage_cumulative
		};
		self.checkpoint_goal_usage(cumulative, now_ms)
	}

	/// Pauses an active goal after a user interrupt while preserving its spend.
	pub fn interrupt_goal(
		&self,
		now_ms: u64,
		user_interrupt: bool,
	) -> Result<Option<Goal>, RegimeError> {
		if !user_interrupt || self.goal().is_none() {
			return Ok(self.goal());
		}
		if self
			.goal()
			.is_some_and(|goal| goal.status == GoalStatus::Active)
		{
			let goal = self.pause_goal(now_ms)?;
			return Ok(Some(goal));
		}
		Ok(self.goal())
	}

	/// Produces the settled-boundary goal decision using Core loop evidence.
	pub fn goal_continuation(&self, signal: &LoopSignal, now_ms: u64) -> Continuation {
		let goal_holds_mode = self.holds_mode("goal");
		let mut state = self.state.lock();
		let Some(goal) = state.goal.clone() else {
			return Continuation::Settle;
		};
		if state.budget_steering_pending {
			state.budget_steering_pending = false;
			return Continuation::Continue {
				owner:          sf!("goal"),
				item:           system_item(
					format!(
						"<system-injection reason=\"goal-budget-limit\">\nThe hard goal budget has been \
						 reached. Stop autonomous work and report the best achieved result \
						 now.\n{}\n</system-injection>",
						goal::prompt_context(&goal, None),
					),
					now_ms,
				),
				label:          Some(goal.id),
				collapse_prior: false,
			};
		}
		if !goal_holds_mode || goal.status != GoalStatus::Active || signal.stalled {
			return Continuation::Settle;
		}
		Continuation::Continue {
			owner:          sf!("goal"),
			item:           system_item(
				format!(
					"<system-injection>\nContinue working autonomously toward this \
					 objective:\n<objective>{}</objective>\n</system-injection>",
					escape_xml(goal.objective.as_str())
				),
				now_ms,
			),
			label:          Some(goal.id),
			collapse_prior: true,
		}
	}

	/// Returns the owner policy applied to goal continuations.
	pub const fn continuation_policy(&self) -> ContinuationPolicy {
		self.policy
	}

	fn update_goal(&self, now_ms: u64, update: impl FnOnce(&mut Goal)) -> Result<Goal, RegimeError> {
		let mut state = self.state.lock();
		let goal = state.goal.as_mut().ok_or(RegimeError::NoGoal)?;
		if goal.status == GoalStatus::Active {
			goal.time_used_seconds = goal
				.time_used_seconds
				.saturating_add(now_ms.saturating_sub(goal.started_ms) / 1_000);
		}
		goal.started_ms = now_ms;
		update(goal);
		Ok(goal.clone())
	}

	fn finish_goal(&self, now_ms: u64, status: GoalStatus) -> Result<Goal, RegimeError> {
		let goal = self.update_goal(now_ms, |goal| goal.status = status)?;
		self.state.lock().budget_steering_pending = false;
		Ok(goal)
	}
}

impl ContinuationSource for RegimeHandle {
	fn decide(&self, signal: &LoopSignal, now_ms: u64) -> (Continuation, ContinuationPolicy) {
		if let Some(candidate) = self.loop_continuation(now_ms) {
			return (candidate, ContinuationPolicy {
				max_consecutive: u32::MAX,
				..ContinuationPolicy::default()
			});
		}
		(self.goal_continuation(signal, now_ms), self.continuation_policy())
	}

	fn sync_regimes(&self, regimes: &RegimeSet) {
		RegimeHandle::sync_regimes(self, regimes);
	}
}

fn user_item(text: Str, now_ms: u64) -> Item {
	Item {
		seq:           0,
		created_at_ms: now_ms,
		kind:          Some(item::Kind::Message(Message {
			role:            i32::from(Role::User),
			parts:           vec![Part { kind: Some(part::Kind::Text(text.to_string())) }],
			synthetic:       None,
			user_initiated:  None,
			completed_at_ms: None,
			usage:           None,
		})),
		props:         None,
	}
}

fn system_item(text: String, now_ms: u64) -> Item {
	Item {
		seq:           0,
		created_at_ms: now_ms,
		kind:          Some(item::Kind::Message(Message {
			role:            i32::from(Role::System),
			parts:           vec![Part { kind: Some(part::Kind::Text(text)) }],
			synthetic:       None,
			user_initiated:  None,
			completed_at_ms: None,
			usage:           None,
		})),
		props:         None,
	}
}

fn escape_xml(value: &str) -> String {
	value
		.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn mode_resource_denials_preserve_owner_and_since_at_the_app_projection_seam() {
		let mut regimes = RegimeSet::new();
		let (plan, plan_regime) = omp_agent::core_regime("plan").expect("plan regime");
		let granted = regimes
			.start(plan, plan_regime, omp_agent::StartOptions { now_ms: 41, queue: false })
			.expect("plan grant");
		for contender in ["vibe", "goal"] {
			let (spec, regime) = omp_agent::core_regime(contender).expect("contender regime");
			let error = regimes
				.start(spec, regime, omp_agent::StartOptions { now_ms: 42, queue: false })
				.expect_err("mode resource must deny");
			assert_eq!(error, omp_agent::StartError::Acquire {
				resource: Resource::Mode,
				outcome:  omp_agent::AcquireOutcome::Denied {
					holder: granted.activation.clone(),
					since:  41,
				},
			});
		}
		let projection = RegimeHandle::new();
		projection.sync_regimes(&regimes);
		assert_eq!(projection.mode_holder().as_deref(), Some("plan"));
		assert_eq!(projection.mode_activation(), Some(granted.activation));
	}

	#[test]
	fn queued_mode_resource_projects_depth_and_auto_grants_on_release() {
		let mut regimes = RegimeSet::new();
		let (plan, plan_regime) = omp_agent::core_regime("plan").expect("plan regime");
		let granted = regimes
			.start(plan, plan_regime, omp_agent::StartOptions { now_ms: 41, queue: false })
			.expect("plan grant");
		let (vibe, vibe_regime) = omp_agent::core_regime("vibe").expect("vibe regime");
		let ticket = regimes
			.start(vibe, vibe_regime, omp_agent::StartOptions { now_ms: 42, queue: true })
			.expect("vibe queue ticket");
		assert!(matches!(ticket.outcome, omp_agent::AcquireOutcome::Queued { .. }));
		let projection = RegimeHandle::new();
		projection.sync_regimes(&regimes);
		let mode = projection
			.visible_resources()
			.iter()
			.find(|resource| resource.resource == "mode")
			.cloned()
			.expect("mode projection");
		assert_eq!(mode.owner, "plan");
		assert_eq!(mode.queue_depth, 1);

		regimes
			.stop(granted.activation.as_str(), 43)
			.expect("plan exit");
		projection.sync_regimes(&regimes);
		assert_eq!(projection.mode_holder().as_deref(), Some("vibe"));
		assert_eq!(projection.mode_activation(), Some(ticket.activation));
	}

	#[test]
	fn goal_accounting_excludes_cached_input_and_hard_stops() {
		let modes = RegimeHandle::new();
		modes.set_goal("ship", Some(10), 1_000).expect("set goal");
		let goal = modes
			.record_goal_usage(
				GoalUsage {
					input_tokens:        3,
					cache_write_tokens:  2,
					cached_input_tokens: 100,
					output_tokens:       5,
				},
				2_000,
			)
			.expect("record usage");
		assert_eq!(goal.tokens_used, 10);
		assert_eq!(goal.status, GoalStatus::BudgetLimited);
	}
	#[test]
	fn goal_accounting_preserves_session_baseline_and_mixes_delta_and_cumulative_receipts() {
		let modes = RegimeHandle::new();
		assert!(
			modes
				.record_goal_usage_delta(
					GoalUsage { input_tokens: 100, output_tokens: 20, ..GoalUsage::default() },
					500,
				)
				.is_err()
		);
		modes.set_goal("ship", Some(100), 1_000).expect("set goal");
		modes
			.checkpoint_goal_usage(
				GoalUsage { input_tokens: 105, output_tokens: 22, ..GoalUsage::default() },
				2_000,
			)
			.expect("partial cumulative tool receipt");
		let goal = modes
			.record_goal_usage_delta(
				GoalUsage { input_tokens: 10, output_tokens: 5, ..GoalUsage::default() },
				3_000,
			)
			.expect("turn delta");
		assert_eq!(goal.tokens_used, 15);
	}

	#[test]
	fn stalled_loop_signal_prevents_goal_continuation() {
		let mut regimes = RegimeSet::new();
		let (spec, regime) = omp_agent::core_regime("goal").expect("goal regime");
		regimes
			.start(spec, regime, omp_agent::StartOptions { now_ms: 0, queue: false })
			.expect("goal grant");
		let modes = RegimeHandle::new();
		modes.sync_regimes(&regimes);
		modes.set_goal("ship <safely>", None, 0).expect("set goal");
		assert!(matches!(
			modes.goal_continuation(&LoopSignal::default(), 1),
			Continuation::Continue { .. }
		));
		let signal = LoopSignal { stalled: true, ..LoopSignal::default() };
		assert_eq!(modes.goal_continuation(&signal, 2), Continuation::Settle);
	}
	#[test]
	fn bounded_loop_repeats_inline_prompt_then_disarms() {
		let modes = RegimeHandle::new();
		let outcome = modes.toggle_loop("2 keep going", 100).expect("arm loop");
		assert!(matches!(
			&outcome,
			LoopCommandOutcome::Enabled { prompt: Some(prompt), .. } if prompt == "keep going"
		));
		for now_ms in [101, 102] {
			assert!(matches!(
				modes.decide(&LoopSignal::default(), now_ms).0,
				Continuation::Continue { owner, .. } if owner == "loop"
			));
		}
		assert_eq!(modes.decide(&LoopSignal::default(), 103).0, Continuation::Settle);
		assert_eq!(modes.loop_status(), "Loop: off");
	}

	#[test]
	fn pausing_loop_preserves_bounds_and_waits_for_a_new_prompt() {
		let modes = RegimeHandle::new();
		modes.toggle_loop("2 first", 0).expect("arm loop");
		modes.pause_loop();
		assert_eq!(modes.loop_status(), "Loop: waiting (2 of 2 iterations remaining)");
		assert_eq!(modes.decide(&LoopSignal::default(), 1).0, Continuation::Settle);
		modes.capture_loop_prompt("second");
		assert!(matches!(modes.decide(&LoopSignal::default(), 2).0, Continuation::Continue { .. }));
	}

	#[test]
	fn loop_parses_compound_duration_and_captures_next_prompt() {
		let modes = RegimeHandle::new();
		let outcome = modes.toggle_loop("1h30m", 1_000).expect("arm timed loop");
		assert!(matches!(outcome, LoopCommandOutcome::Enabled { prompt: None, .. }));
		modes.capture_loop_prompt("continue");
		assert!(matches!(
			modes.decide(&LoopSignal::default(), 5_400_999).0,
			Continuation::Continue { .. }
		));
		assert_eq!(modes.decide(&LoopSignal::default(), 5_401_000).0, Continuation::Settle);
	}
}
