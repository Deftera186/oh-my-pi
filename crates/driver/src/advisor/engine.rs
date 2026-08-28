//! Session-scoped advisor coordination.

use std::{
	collections::BTreeMap,
	path::PathBuf,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, Instant},
};

use omp_agent::advisor::{
	AdviceDelivery, AdviceSeverity, AdvisorAdviceQueue, AdvisorDeltaBatch, AdvisorDeltaSync,
	AdvisorEmissionGuard, AdvisorQuarantineReason, AdvisorRuntimeState, AdvisorSuppression,
	DeliveryContext, ImmuneTurnAccount, RoutedAdvice, quarantine_advisor_turn,
};
use omp_catalog::{known_roles, snapshot};
use omp_core::{Str, StrMut};

use super::{
	config::{AdvisorConfigSnapshot, AdvisorProviderSessions, discover},
	runtime::{
		AdvisorFailureClass, AdvisorFallbackChain, AdvisorRetryDecision, AdvisorRetryManager,
	},
	transcript::{
		AdvisorStatisticsSink, AdvisorTranscriptRecord, AdvisorTranscriptStore, AdvisorUsageTotals,
	},
};

const BASELINE_PROMPT: &str =
	"You are an advisor observing updates from another agent's session. Investigate concrete risks \
	 with only the read-only tools granted to you. Call advise sparingly, using nit for optional \
	 cleanup, concern for material risk, and blocker only for work that is broken. Never address \
	 the user or attempt to take over the session. Your guidance is weighed by the primary agent, \
	 not blindly obeyed.";
const DELTA_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(30);
const RETRIES_PER_MODEL: u32 = 2;

/// Runtime state of one composed advisor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AdvisorRunState {
	/// Ready to receive primary-session updates.
	Running,
	/// Waiting for a retry cooldown to elapse.
	Paused,
	/// Provider quota is hard-latched.
	QuotaExhausted,
	/// Retry or permanent-failure policy was exhausted.
	Failed,
	/// Unsafe output exhausted the quarantine allowance.
	Muted,
}

/// One resolved, enabled advisor ready to run.
pub struct AdvisorWorker {
	/// Stable advisor slug.
	pub id:            Str,
	/// Human-facing roster name.
	pub display_name:  Str,
	/// Resolved catalog model key.
	pub model:         Str,
	/// Evaluated investigative tool grants.
	pub tools:         Vec<Str>,
	/// Complete advisor system prompt.
	pub system_prompt: Str,
	delta:             AdvisorDeltaSync,
	guard:             AdvisorEmissionGuard,
	immunity:          ImmuneTurnAccount,
	queue:             AdvisorAdviceQueue,
	runtime:           AdvisorRuntimeState,
	usage:             AdvisorUsageTotals,
	state:             AdvisorRunState,
	messages:          u64,
	pending:           bool,
	retry:             AdvisorRetryManager,
}

/// Inputs needed to compose a session-scoped advisor engine.
pub struct AdvisorEngineOptions {
	/// Project root used for WATCHDOG discovery and transcript persistence.
	pub project_root:    PathBuf,
	/// Owning primary session identity.
	pub primary_session: Str,
	/// Effective invocation-local advisor toggle.
	pub enabled:         bool,
	/// Completed primary turns suppressed after a steering delivery.
	pub immune_turns:    u32,
	/// Session tools against which advisor grants are evaluated.
	pub available_tools: Vec<Str>,
	/// Clone-shared session queue backing the environment's `advise@1` device.
	pub advice_queue:    AdvisorAdviceQueue,
}

/// One pending advisor prompt generated at a primary turn boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorPromptJob {
	/// Advisor child which should receive this update.
	pub advisor_id: Str,
	/// Coalesced, secret-safe primary-session delta.
	pub batch:      AdvisorDeltaBatch,
}

/// Admission result for one advisor note.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdviceOutcome {
	/// Safe, concrete advice with its selected primary-loop route.
	Deliver {
		/// Advisor-authored note with stable source identity.
		advice:   RoutedAdvice,
		/// Selected primary-loop delivery channel.
		delivery: AdviceDelivery,
	},
	/// The emission guard rejected this note.
	Suppressed(AdvisorSuppression),
	/// Unsafe advisor output was retained outside the primary mailbox.
	Quarantined(AdvisorQuarantineReason),
}

/// Snapshot used by advisor status presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorEngineStatus {
	/// Whether dispatch is currently enabled.
	pub enabled:  bool,
	/// Stable roster-order advisor rows.
	pub advisors: Vec<AdvisorStatusRow>,
}

/// One advisor's status projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorStatusRow {
	/// Stable advisor slug.
	pub id:           Str,
	/// Human-facing advisor name.
	pub display_name: Str,
	/// Resolved catalog model key.
	pub model:        Str,
	/// Current retry, quota, or quarantine state.
	pub state:        AdvisorRunState,
	/// Inference usage attributed to this advisor.
	pub usage:        AdvisorUsageTotals,
	/// Prompt updates dispatched to this advisor.
	pub messages:     u64,
}

#[derive(Clone, Default)]
struct EngineAdvisorStatistics {
	cost_changed: Arc<AtomicBool>,
}

impl AdvisorStatisticsSink for EngineAdvisorStatistics {
	fn record_advisor_usage(&self, _: &str, _: &str, _: AdvisorUsageTotals) {}

	fn advisor_cost_changed(&self, _: &str) {
		self.cost_changed.store(true, Ordering::Release);
	}
}

/// Coordinates configured advisor workers for one primary session.
pub struct AdvisorEngine {
	enabled:         bool,
	workers:         Vec<AdvisorWorker>,
	worker_index:    BTreeMap<Str, usize>,
	transcripts:     Option<AdvisorTranscriptStore<EngineAdvisorStatistics>>,
	cost_changed:    Arc<AtomicBool>,
	primary_turn_id: u64,
}

impl AdvisorEngine {
	/// Discovers WATCHDOG configuration and resolves its runtime-ready roster
	/// against the supplied catalog.
	pub fn compose(options: AdvisorEngineOptions, catalog: &snapshot::Catalog) -> Self {
		let snapshot = discover(&options.project_root, None);
		Self::compose_snapshot(options, snapshot, catalog)
	}

	fn compose_snapshot(
		options: AdvisorEngineOptions,
		config: AdvisorConfigSnapshot,
		catalog: &snapshot::Catalog,
	) -> Self {
		let config = config.with_invocation_enabled(true);
		let sessions = AdvisorProviderSessions::default();
		let roles = known_roles(&[]);
		let mru = BTreeMap::new();
		let schedule = config.schedule_resolved(
			options.primary_session.as_str(),
			&options.available_tools,
			&sessions,
			catalog,
			&roles,
			&mru,
		);
		let mut workers = Vec::new();
		match schedule {
			Ok(schedule) => {
				workers.reserve(schedule.advisors.len());
				for resolved in schedule.advisors.iter() {
					let scheduled = &resolved.scheduled;
					let id = scheduled.rule.slug.clone();
					let display_name = scheduled.rule.name.clone();
					let model = Str::from(resolved.selection.model.as_str());
					let tools = scheduled.tools.to_vec();
					let system_prompt = compose_system_prompt(&config, &scheduled.rule.instructions);
					let chain = AdvisorFallbackChain::new([model.clone()])
						.expect("a resolved catalog key is a non-empty retry selector");
					let retry = AdvisorRetryManager::new(
						chain,
						RETRIES_PER_MODEL,
						RETRY_INITIAL_BACKOFF,
						RETRY_MAX_BACKOFF,
					)
					.expect("the advisor retry budget is positive");
					workers.push(AdvisorWorker {
						id: id.clone(),
						display_name: display_name.clone(),
						model,
						tools,
						system_prompt,
						delta: AdvisorDeltaSync::new(DELTA_MAINTENANCE_INTERVAL, None),
						guard: AdvisorEmissionGuard::default(),
						immunity: ImmuneTurnAccount::new(options.immune_turns),
						queue: options.advice_queue.clone(),
						runtime: AdvisorRuntimeState {
							id,
							parent_id: options.primary_session.clone(),
							display_name,
							history_cursor: 0,
							input_tokens: 0,
							output_tokens: 0,
						},
						usage: AdvisorUsageTotals::default(),
						state: AdvisorRunState::Running,
						messages: 0,
						pending: false,
						retry,
					});
				}
			},
			Err(error) => tracing::warn!(%error, "advisor roster model resolution failed"),
		}
		let worker_index = workers
			.iter()
			.enumerate()
			.map(|(index, worker)| (worker.id.clone(), index))
			.collect();
		let cost_changed = Arc::new(AtomicBool::new(false));
		let transcripts = AdvisorTranscriptStore::open(
			&options.project_root,
			options.primary_session,
			EngineAdvisorStatistics { cost_changed: Arc::clone(&cost_changed) },
		)
		.map_err(|error| tracing::warn!(%error, "advisor transcript store could not be opened"))
		.ok();
		Self {
			enabled: options.enabled,
			workers,
			worker_index,
			transcripts,
			cost_changed,
			primary_turn_id: 0,
		}
	}

	/// Returns whether advisor dispatch is enabled.
	pub const fn enabled(&self) -> bool {
		self.enabled
	}

	/// Changes advisor dispatch state and returns whether it changed.
	pub fn set_enabled(&mut self, enabled: bool) -> bool {
		let changed = self.enabled != enabled;
		self.enabled = enabled;
		changed
	}

	/// Iterates resolved workers in roster order.
	pub fn workers(&self) -> impl Iterator<Item = &AdvisorWorker> {
		self.workers.iter()
	}

	/// Returns a clone-sharing handle to one advisor's advise queue.
	pub fn advice_queue(&self, advisor_id: &str) -> Option<AdvisorAdviceQueue> {
		self.worker(advisor_id).map(|worker| worker.queue.clone())
	}

	/// Invalidates every advisor's pending primary delta after a history
	/// rewrite (rewind or reset); the next update re-primes full context.
	pub fn history_rewritten(&mut self) {
		for worker in &mut self.workers {
			worker.delta.history_rewritten();
			worker.pending = false;
		}
	}

	/// Pushes rendered primary-session text into every advisor's pending delta.
	pub fn observe_primary_text(&mut self, text: &str) {
		if text.is_empty() {
			return;
		}
		for worker in &mut self.workers {
			worker.queue.set_mid_turn(true);
			if worker.pending {
				worker.delta.push(text);
			} else {
				let mut update = StrMut::new("### Session update\n\n");
				update.push_str(text);
				worker.delta.push(update.freeze());
				worker.pending = true;
			}
		}
	}

	/// Closes a primary turn and returns one coalesced job per worker with
	/// pending context while dispatch is enabled.
	pub fn end_primary_turn(&mut self, will_continue: bool) -> Vec<AdvisorPromptJob> {
		self.primary_turn_id = self.primary_turn_id.saturating_add(1);
		for worker in &mut self.workers {
			worker
				.immunity
				.record_primary_completion(self.primary_turn_id);
		}
		if !self.enabled {
			return Vec::new();
		}
		let mut jobs = Vec::new();
		for worker in &mut self.workers {
			if !worker.pending {
				continue;
			}
			if will_continue {
				worker.delta.push("[in progress — more steps follow]");
			}
			let Some(batch) = worker.delta.drain_coalesced() else {
				worker.pending = false;
				continue;
			};
			worker.pending = false;
			worker.guard.begin_update();
			if let Some(transcripts) = self.transcripts.as_ref() {
				transcripts.begin_turn(worker.id.as_str());
			}
			worker.queue.set_mid_turn(false);
			worker.runtime.history_cursor = batch.next_cursor;
			worker.messages = worker.messages.saturating_add(1);
			jobs.push(AdvisorPromptJob { advisor_id: worker.id.clone(), batch });
		}
		jobs
	}

	/// Guards, quarantines, and routes one advisor note.
	pub fn admit_advice(
		&mut self,
		advisor_id: &str,
		note: Str,
		severity: AdviceSeverity,
		context: DeliveryContext,
	) -> AdviceOutcome {
		let Some(worker) = self.worker_mut(advisor_id) else {
			return AdviceOutcome::Suppressed(AdvisorSuppression::ContentFree);
		};
		let guarded = match worker.guard.admit(note.as_str(), severity) {
			Ok(guarded) => guarded,
			Err(suppression) => return AdviceOutcome::Suppressed(suppression),
		};
		if let Some(reason) = quarantine_advisor_turn(&[], &worker.tools, guarded.note.as_str(), "") {
			if worker.guard.record_quarantine() {
				worker.state = AdvisorRunState::Muted;
			}
			return AdviceOutcome::Quarantined(reason);
		}
		worker.guard.record_safe_turn();
		let advice = RoutedAdvice {
			advisor_id: worker.id.clone(),
			note:       guarded.note,
			severity:   guarded.severity,
		};
		let delivery = worker.immunity.evaluate(advice.severity, context);
		if delivery == AdviceDelivery::Steer {
			worker.immunity.record_steer();
		}
		AdviceOutcome::Deliver { advice, delivery }
	}

	/// Accumulates one inference-usage delta for an advisor.
	pub fn record_usage(&mut self, advisor_id: &str, usage: AdvisorUsageTotals) {
		let Some(worker) = self.worker_mut(advisor_id) else {
			return;
		};
		worker.usage.input_tokens = worker.usage.input_tokens.saturating_add(usage.input_tokens);
		worker.usage.cache_read_tokens = worker
			.usage
			.cache_read_tokens
			.saturating_add(usage.cache_read_tokens);
		worker.usage.cache_write_tokens = worker
			.usage
			.cache_write_tokens
			.saturating_add(usage.cache_write_tokens);
		worker.usage.output_tokens = worker
			.usage
			.output_tokens
			.saturating_add(usage.output_tokens);
		worker.usage.cost_micro_usd = worker
			.usage
			.cost_micro_usd
			.saturating_add(usage.cost_micro_usd);
		worker.runtime.input_tokens = worker
			.runtime
			.input_tokens
			.saturating_add(usage.input_tokens);
		worker.runtime.output_tokens = worker
			.runtime
			.output_tokens
			.saturating_add(usage.output_tokens);
	}

	/// Persists one append-only advisor transcript record.
	pub fn record_transcript(&mut self, record: &AdvisorTranscriptRecord) {
		if let Some(store) = self.transcripts.as_mut()
			&& let Err(error) = store.append(record)
		{
			tracing::warn!(%error, advisor = %record.advisor_id, "advisor transcript append failed");
		}
	}

	/// Advances retry, cooldown, and quota policy after a failed advisor update.
	pub fn record_failure(
		&mut self,
		advisor_id: &str,
		class: AdvisorFailureClass,
	) -> AdvisorRetryDecision {
		let Some(worker) = self.worker_mut(advisor_id) else {
			return AdvisorRetryDecision::Permanent;
		};
		let decision = worker
			.retry
			.record_failure(advisor_id, class, Instant::now());
		worker.state = match decision {
			AdvisorRetryDecision::Cooldown { .. } => AdvisorRunState::Paused,
			AdvisorRetryDecision::QuotaLatched => AdvisorRunState::QuotaExhausted,
			AdvisorRetryDecision::Exhausted | AdvisorRetryDecision::Permanent => {
				AdvisorRunState::Failed
			},
			AdvisorRetryDecision::Attempt { .. } => AdvisorRunState::Running,
		};
		if let Some(transcripts) = self.transcripts.as_ref() {
			transcripts.abandon_turn(advisor_id);
		}
		decision
	}

	/// Clears retry state after a successful advisor update.
	pub fn record_success(&mut self, advisor_id: &str) {
		if let Some(worker) = self.worker_mut(advisor_id) {
			worker.retry.record_success(advisor_id);
			if worker.state != AdvisorRunState::Muted {
				worker.guard.record_safe_turn();
				worker.state = AdvisorRunState::Running;
			}
		}
		if let Some(transcripts) = self.transcripts.as_ref() {
			transcripts.commit_turn(advisor_id);
		}
	}

	/// Returns and clears the resume-cost completion notification.
	pub fn take_cost_changed(&self) -> bool {
		self.cost_changed.swap(false, Ordering::AcqRel)
	}

	/// Returns whether resume-time advisor cost restoration has settled.
	pub fn cost_restore_finished(&self) -> bool {
		self
			.transcripts
			.as_ref()
			.is_none_or(AdvisorTranscriptStore::cost_restore_finished)
	}

	/// Returns a presentation-safe snapshot of all advisor workers.
	pub fn status(&self) -> AdvisorEngineStatus {
		AdvisorEngineStatus {
			enabled:  self.enabled,
			advisors: self
				.workers
				.iter()
				.map(|worker| AdvisorStatusRow {
					id:           worker.id.clone(),
					display_name: worker.display_name.clone(),
					model:        worker.model.clone(),
					state:        worker.state,
					usage:        self.usage_for(worker),
					messages:     worker.messages,
				})
				.collect(),
		}
	}

	/// Renders the current advisor roster and accounting as Markdown.
	pub fn dump(&self, compact: bool) -> Str {
		let mut output = String::new();
		output.push_str(if self.enabled {
			"# Advisors (enabled)\n"
		} else {
			"# Advisors (disabled)\n"
		});
		for worker in &self.workers {
			let usage = self.usage_for(worker);
			if compact {
				use std::fmt::Write as _;
				let _ = writeln!(
					output,
					"- **{}** — `{}` — {} — {} messages — {} µUSD",
					worker.display_name,
					worker.model,
					worker.state,
					worker.messages,
					usage.cost_micro_usd,
				);
			} else {
				use std::fmt::Write as _;
				let _ = writeln!(output, "\n## {} (`{}`)", worker.display_name, worker.id);
				let _ = writeln!(output, "- Model: `{}`", worker.model);
				let _ = writeln!(output, "- State: {}", worker.state);
				let _ = writeln!(output, "- Messages: {}", worker.messages);
				let _ = writeln!(output, "- Cost: {} µUSD", usage.cost_micro_usd);
				let _ = writeln!(output, "- Input tokens: {}", usage.input_tokens);
				let _ = writeln!(output, "- Output tokens: {}", usage.output_tokens);
				let _ = writeln!(output, "- Tools: {}", worker.tools.join(", "));
			}
		}
		Str::from(output)
	}

	/// Counts workers with a pending primary-session batch.
	pub fn backlog(&self) -> usize {
		self.workers.iter().filter(|worker| worker.pending).count()
	}

	fn usage_for(&self, worker: &AdvisorWorker) -> AdvisorUsageTotals {
		self
			.transcripts
			.as_ref()
			.map_or(worker.usage, |transcripts| {
				if transcripts.cost_restore_finished() {
					transcripts.totals(worker.id.as_str())
				} else {
					worker.usage
				}
			})
	}

	fn worker(&self, advisor_id: &str) -> Option<&AdvisorWorker> {
		self
			.worker_index
			.get(advisor_id)
			.map(|index| &self.workers[*index])
	}

	fn worker_mut(&mut self, advisor_id: &str) -> Option<&mut AdvisorWorker> {
		let index = self.worker_index.get(advisor_id).copied()?;
		self.workers.get_mut(index)
	}
}

fn compose_system_prompt(config: &AdvisorConfigSnapshot, rule_instructions: &Option<Str>) -> Str {
	let mut prompt = StrMut::new(BASELINE_PROMPT);
	if let Some(instructions) = config.roster.instructions.as_ref() {
		push_prompt_block(&mut prompt, instructions);
	}
	if let Some(instructions) = rule_instructions.as_ref() {
		push_prompt_block(&mut prompt, instructions);
	}
	for attention in config.attention.iter() {
		push_prompt_block(&mut prompt, attention);
	}
	if let Some(project_context) = config.project_context.as_ref() {
		push_prompt_block(&mut prompt, project_context);
	}
	if let Some(active_repo_context) = config.active_repo_context.as_ref() {
		push_prompt_block(&mut prompt, active_repo_context);
	}
	prompt.freeze()
}

fn push_prompt_block(prompt: &mut StrMut, block: &Str) {
	prompt.push_str("\n\n");
	prompt.push_str(block.as_str());
}

#[cfg(test)]
mod tests {
	use std::sync::{
		Arc,
		atomic::{AtomicU64, Ordering as AtomicOrdering},
	};

	use omp_agent::advisor::{AdvisorRoster, MAX_DELTA_COALESCE_ROUNDS};
	use omp_catalog::snapshot::Catalog;

	use super::*;
	static NEXT_ENGINE_ROOT: AtomicU64 = AtomicU64::new(0);

	fn options(enabled: bool) -> AdvisorEngineOptions {
		let nonce = NEXT_ENGINE_ROOT.fetch_add(1, AtomicOrdering::Relaxed);
		AdvisorEngineOptions {
			project_root: std::env::temp_dir()
				.join(format!("omp-advisor-engine-tests-{}-{nonce}", std::process::id())),
			primary_session: Str::new_static("primary"),
			enabled,
			immune_turns: 3,
			available_tools: vec![
				Str::new_static("read"),
				Str::new_static("grep"),
				Str::new_static("glob"),
			],
			advice_queue: AdvisorAdviceQueue::default(),
		}
	}

	fn empty_snapshot() -> AdvisorConfigSnapshot {
		AdvisorConfigSnapshot {
			roster:              AdvisorRoster::default(),
			attention:           Arc::default(),
			project_context:     None,
			active_repo_context: None,
			sources:             Arc::default(),
			diagnostics:         Arc::default(),
		}
	}

	fn make_engine(enabled: bool) -> AdvisorEngine {
		AdvisorEngine::compose_snapshot(options(enabled), empty_snapshot(), Catalog::embedded())
	}

	#[test]
	fn compose_empty_enabled_roster_yields_default_advisor() {
		let engine = make_engine(true);
		let workers = engine.workers().collect::<Vec<_>>();
		assert_eq!(workers.len(), 1);
		assert_eq!(workers[0].id, "default");
		assert_eq!(workers[0].tools, ["read", "grep", "glob"]);
		assert!(workers[0].system_prompt.contains("Never address the user"));
	}
	#[test]
	fn background_cost_restore_emits_one_engine_notification() {
		let engine = make_engine(true);
		let deadline = Instant::now() + Duration::from_secs(2);
		while !engine.cost_restore_finished() {
			assert!(Instant::now() < deadline, "advisor cost restore did not settle");
			std::thread::sleep(Duration::from_millis(1));
		}
		assert!(engine.take_cost_changed());
		assert!(!engine.take_cost_changed());
	}

	#[test]
	fn disabled_engine_retains_backlog_without_dispatching_jobs() {
		let mut engine = make_engine(false);
		assert_eq!(engine.workers().count(), 1);
		engine.observe_primary_text("working");
		assert!(engine.end_primary_turn(false).is_empty());
		assert_eq!(engine.backlog(), 1);
	}

	#[test]
	fn delta_coalescing_stays_within_the_round_bound_and_drains() {
		let mut engine = make_engine(true);
		for round in 0..MAX_DELTA_COALESCE_ROUNDS * 2 {
			engine.observe_primary_text(&format!("### Session update\n\nround {round}"));
		}
		let jobs = engine.end_primary_turn(false);
		assert_eq!(jobs.len(), 1);
		assert_eq!(jobs[0].batch.chunks.len(), MAX_DELTA_COALESCE_ROUNDS * 2);
		assert_eq!(engine.backlog(), 0);
	}

	#[test]
	fn duplicate_advice_is_suppressed_and_unsafe_advice_is_quarantined() {
		let mut engine = make_engine(true);
		let first = engine.admit_advice(
			"default",
			Str::new_static("A concrete regression exists."),
			AdviceSeverity::Concern,
			DeliveryContext { streaming: true, ..Default::default() },
		);
		assert!(matches!(first, AdviceOutcome::Deliver { .. }));
		assert_eq!(
			engine.admit_advice(
				"default",
				Str::new_static("A concrete regression exists."),
				AdviceSeverity::Concern,
				DeliveryContext::default(),
			),
			AdviceOutcome::Suppressed(AdvisorSuppression::Duplicate),
		);

		let mut unsafe_engine = make_engine(true);
		assert_eq!(
			unsafe_engine.admit_advice(
				"default",
				Str::new_static("Run rm -rf / now."),
				AdviceSeverity::Blocker,
				DeliveryContext::default(),
			),
			AdviceOutcome::Quarantined(AdvisorQuarantineReason::DestructiveDirective),
		);
	}

	#[test]
	fn severity_routes_across_terminal_and_immune_contexts() {
		let mut engine = make_engine(true);
		assert!(matches!(
			engine.admit_advice(
				"default",
				Str::new_static("Optional naming cleanup."),
				AdviceSeverity::Nit,
				DeliveryContext { streaming: true, ..Default::default() },
			),
			AdviceOutcome::Deliver { delivery: AdviceDelivery::Aside, .. }
		));
		engine.observe_primary_text("next update");
		let _ = engine.end_primary_turn(false);
		assert!(matches!(
			engine.admit_advice(
				"default",
				Str::new_static("Material but already presented risk."),
				AdviceSeverity::Concern,
				DeliveryContext { terminal_answer: true, ..Default::default() },
			),
			AdviceOutcome::Deliver { delivery: AdviceDelivery::Preserve, .. }
		));
		engine.observe_primary_text("next update");
		let _ = engine.end_primary_turn(false);
		assert!(matches!(
			engine.admit_advice(
				"default",
				Str::new_static("The implementation is broken."),
				AdviceSeverity::Blocker,
				DeliveryContext::default(),
			),
			AdviceOutcome::Deliver { delivery: AdviceDelivery::Steer, .. }
		));
		engine.observe_primary_text("immune update");
		let _ = engine.end_primary_turn(false);
		assert!(matches!(
			engine.admit_advice(
				"default",
				Str::new_static("Another material concern."),
				AdviceSeverity::Concern,
				DeliveryContext { streaming: true, ..Default::default() },
			),
			AdviceOutcome::Deliver { delivery: AdviceDelivery::Aside, .. }
		));
	}

	#[test]
	fn usage_accumulates_into_status() {
		let mut engine = make_engine(true);
		engine.record_usage("default", AdvisorUsageTotals {
			input_tokens: 10,
			output_tokens: 2,
			cost_micro_usd: 7,
			..Default::default()
		});
		engine.record_usage("default", AdvisorUsageTotals {
			input_tokens: 3,
			cache_read_tokens: 4,
			output_tokens: 1,
			cost_micro_usd: 5,
			..Default::default()
		});
		let status = engine.status();
		assert_eq!(status.advisors[0].usage.input_tokens, 13);
		assert_eq!(status.advisors[0].usage.cache_read_tokens, 4);
		assert_eq!(status.advisors[0].usage.output_tokens, 3);
		assert_eq!(status.advisors[0].usage.cost_micro_usd, 12);
	}
}
