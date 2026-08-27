//! Complete autoresearch experiment engine.

use std::{error::Error as StdError, future::Future, time::Duration};

use omp_agent::{
	AcquireOutcome, RegimeRecord, RegimeStatus, StartReceipt,
	control::{ControlError, ControlSender},
};
use omp_core::{Str, sf};
use omp_envd::vcs::git::mutation::{GitMutation, IsolationCommit};
use tokio_util::sync::CancellationToken;

use super::{
	git,
	git::{GitError, IsolationQueries, ensure_isolation, recover, run_delta, settle},
	helpers::{Measurement, infer_metric_unit, mad_confidence, parse_asi_lines, parse_metric_lines},
	storage::{JournalAppender, ProjectedSession, RecordError, Storage, StorageError},
	types::{
		Asi, DashboardMode, DispositionIntent, DispositionSettled, ExperimentStatus, JournalFact,
		MetricDirection, Metrics, RunCompletion, RunStart, RuntimeState, SessionConfig,
	},
};

/// Fixed harness entrypoint.
pub const HARNESS: &str = "./autoresearch.sh";
/// Default per-run timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

const REGIME_ID: &str = "autoresearch";

/// Streaming harness outcome supplied by Environment process authority.
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessOutput {
	/// Complete captured output used for metric parsing.
	pub output:          Str,
	/// Process exit code.
	pub exit_code:       Option<i32>,
	/// Wall duration.
	pub duration:        Duration,
	/// Whether the deadline cancelled the process.
	pub timed_out:       bool,
	/// Shared artifact authority URI for `benchmark.log`.
	pub benchmark_uri:   Str,
	/// Exact artifact byte length.
	pub benchmark_bytes: u64,
}

/// Regime authority used by the autoresearch engine.
///
/// Production uses [`ControlSender`], keeping all resource arbitration and
/// durable lifecycle transitions on the Agent's sole mutable owner.
pub trait AutoresearchRegimes: Send + Sync {
	/// Returns active and queued durable regime entries.
	fn active_regimes(&self)
	-> impl Future<Output = Result<Vec<RegimeRecord>, ControlError>> + Send;

	/// Starts the built-in autoresearch regime without queueing.
	fn start_autoresearch(&self) -> impl Future<Output = Result<StartReceipt, ControlError>> + Send;

	/// Stops a live autoresearch regime after its transaction settles.
	fn stop_autoresearch(
		&self,
		activation: Str,
	) -> impl Future<Output = Result<bool, ControlError>> + Send;

	/// Cuts a killed autoresearch regime without applying dwell.
	fn cancel_autoresearch(
		&self,
		activation: Str,
	) -> impl Future<Output = Result<bool, ControlError>> + Send;
}

impl AutoresearchRegimes for ControlSender {
	async fn active_regimes(&self) -> Result<Vec<RegimeRecord>, ControlError> {
		ControlSender::active_regimes(self).await
	}

	async fn start_autoresearch(&self) -> Result<StartReceipt, ControlError> {
		self.start_core_regime("autoresearch", false).await
	}

	async fn stop_autoresearch(&self, activation: Str) -> Result<bool, ControlError> {
		self.stop_regime(activation).await
	}

	async fn cancel_autoresearch(&self, activation: Str) -> Result<bool, ControlError> {
		self.cancel_regime(activation).await
	}
}

/// Runtime operations delegated to production Environment and artifact owners.
pub trait AutoresearchHost: IsolationQueries {
	/// Validates that `./autoresearch.sh` exists, is executable, exits zero, and
	/// emits the selected finite primary `METRIC` line.
	fn validate_harness(
		&self,
		primary_metric: &str,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<(), Self::Error>> + Send;

	/// Streams one fixed harness invocation into `artifact_dir/benchmark.log`.
	fn run_harness(
		&self,
		artifact_dir: &str,
		timeout: Duration,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<HarnessOutput, Self::Error>> + Send;

	/// Restores exact run-created paths in explicit unisolated mode through
	/// Environment document/VCS authority.
	fn rollback_unisolated(
		&self,
		rollback_head: Option<&str>,
		tracked: &[Str],
		untracked: &[Str],
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<(), Self::Error>> + Send;

	/// Current UTC `YYYYMMDD` used in isolation branch names.
	fn date_stamp(&self) -> Str;
	/// Current Unix timestamp in milliseconds.
	fn now_ms(&self) -> i64;
}

/// Initialization parameters for `init_experiment`.
#[derive(Clone, Debug, PartialEq)]
pub struct InitExperiment {
	/// Experiment display name.
	pub name:              Str,
	/// Optional objective override.
	pub goal:              Option<Str>,
	/// Primary metric emitted by the harness.
	pub primary_metric:    Str,
	/// Optional explicit display unit.
	pub metric_unit:       Option<Str>,
	/// Improvement direction.
	pub direction:         MetricDirection,
	/// Secondary metrics to retain.
	pub secondary_metrics: Vec<Str>,
	/// Expected edit prefixes.
	pub scope_paths:       Vec<Str>,
	/// Forbidden edit prefixes.
	pub off_limits:        Vec<Str>,
	/// Free-form constraints.
	pub constraints:       Vec<Str>,
	/// Optional iteration cap.
	pub max_iterations:    Option<u32>,
	/// Start a new segment on the active session.
	pub new_segment:       bool,
	/// Explicitly permit operation without Git isolation.
	pub unisolated:        bool,
}

/// Parameters for `log_experiment`.
#[derive(Clone, Debug, PartialEq)]
pub struct LogExperiment {
	/// Terminal disposition.
	pub status:        ExperimentStatus,
	/// Human-readable result description.
	pub description:   Str,
	/// Primary metric, normally copied from parsed harness output.
	pub metric:        f64,
	/// Secondary overrides merged over parsed harness metrics.
	pub metrics:       Metrics,
	/// ASI overrides merged over parsed harness metadata.
	pub asi:           Asi,
	/// Required explanation when retaining scope deviations.
	pub justification: Option<Str>,
	/// Prior runs to flag as suspect.
	pub flag_runs:     Vec<(i64, Str)>,
}

/// Tree handling for `/autoresearch clear`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearTree {
	/// Preserve current working-tree bytes.
	Keep,
	/// Roll back only the journaled pending run delta.
	Reset,
}

/// Engine failure.
#[derive(Debug, thiserror::Error)]
pub enum EngineError<H: StdError + 'static, J: StdError + 'static> {
	/// Environment, Git-query, or artifact host failed.
	#[error("autoresearch runtime host failed")]
	Host(#[source] H),
	/// Journal append or projection failed.
	#[error(transparent)]
	Record(#[from] RecordError<J>),
	/// Query projection failed.
	#[error(transparent)]
	Storage(#[from] StorageError),
	/// Isolation mutation or crash recovery failed.
	#[error(transparent)]
	Git(#[from] GitError<H>),
	/// Regime CONTROL or arbitration failed.
	#[error("autoresearch regime transition failed")]
	Regime(#[source] ControlError),
	/// A tool was called without an active, branch-matching session.
	#[error("no active autoresearch session for the current branch")]
	Inactive,
	/// `run_experiment` already has an unsettled process result.
	#[error("the prior autoresearch run must be logged or abandoned first")]
	PendingRun,
	/// `log_experiment` requires a completed run.
	#[error("no completed autoresearch run is awaiting disposition")]
	NoPendingRun,
	/// Primary metric was absent from otherwise successful harness output.
	#[error("autoresearch harness did not emit the configured primary METRIC")]
	MissingPrimaryMetric,
	/// A metric must be finite.
	#[error("autoresearch metric must be finite")]
	InvalidMetric,
	/// The session reached its configured iteration cap.
	#[error("autoresearch segment reached its maximum iteration count")]
	IterationLimit,
}

/// Journal-first autoresearch owner.
pub struct Engine<'a, H, J, C = ControlSender> {
	host:     &'a H,
	journal:  &'a mut J,
	storage:  &'a mut Storage,
	mutation: Option<&'a GitMutation>,
	regimes:  &'a C,
	runtime:  RuntimeState,
}

impl<'a, H, J, C> Engine<'a, H, J, C>
where
	H: AutoresearchHost,
	J: JournalAppender,
	C: AutoresearchRegimes,
{
	/// Creates one owner and reconstructs its query-backed runtime state.
	pub fn new(
		host: &'a H,
		journal: &'a mut J,
		storage: &'a mut Storage,
		mutation: Option<&'a GitMutation>,
		regimes: &'a C,
	) -> Self {
		Self { host, journal, storage, mutation, regimes, runtime: RuntimeState::default() }
	}

	/// Returns the reconstructed runtime state.
	pub const fn runtime(&self) -> &RuntimeState {
		&self.runtime
	}

	/// Enables autoresearch phase one, creates isolation, and returns the setup
	/// prompt that asks the agent to build the real harness.
	pub async fn start(
		&mut self,
		goal: Option<Str>,
		unisolated: bool,
		cancel: &CancellationToken,
	) -> Result<Str, EngineError<H::Error, J::Error>> {
		let newly_started = self.ensure_regime().await?;
		let isolation = match ensure_isolation(
			Some(self.host),
			self.mutation,
			goal.as_deref(),
			self.host.date_stamp().as_str(),
			unisolated,
			cancel,
		)
		.await
		{
			Ok(isolation) => isolation,
			Err(error) => {
				if newly_started {
					let _ = self.release_regime().await;
				}
				return Err(error.into());
			},
		};
		self.runtime.goal = goal.clone();
		self.runtime.resume_armed = true;
		Ok(Str::from(format!(
			"Autoresearch phase 1 is active{}. Goal: {}. Inspect the repository, create executable \
			 {HARNESS}, and make it exit 0 while printing `METRIC <name>=<finite value>`. Do not \
			 call init_experiment until the harness has actually run successfully.{}",
			isolation
				.branch
				.as_ref()
				.map_or_else(|| " without Git isolation".to_owned(), |branch| format!(" on {branch}")),
			goal
				.as_deref()
				.unwrap_or("discover and improve a measurable objective"),
			if isolation.preserved_paths.is_empty() {
				String::new()
			} else {
				format!(
					" Preserved {} dirty baseline path(s) in an isolation commit.",
					isolation.preserved_paths.len()
				)
			},
		)))
	}

	/// Starts or reconfigures autoresearch after validating the real harness.
	pub async fn init_experiment(
		&mut self,
		params: InitExperiment,
		cancel: &CancellationToken,
	) -> Result<i64, EngineError<H::Error, J::Error>> {
		let newly_started = self.ensure_regime().await?;
		if let Err(error) = self
			.host
			.validate_harness(params.primary_metric.as_str(), cancel)
			.await
		{
			if newly_started {
				let _ = self.release_regime().await;
			}
			return Err(EngineError::Host(error));
		}
		let isolation = match ensure_isolation(
			Some(self.host),
			self.mutation,
			params.goal.as_deref(),
			self.host.date_stamp().as_str(),
			params.unisolated,
			cancel,
		)
		.await
		{
			Ok(isolation) => isolation,
			Err(error) => {
				if newly_started {
					let _ = self.release_regime().await;
				}
				return Err(error.into());
			},
		};

		let branch = isolation.branch.clone();
		let existing = self.storage.active_session(branch.as_deref())?;
		let was_existing = existing.is_some();
		if let Some(session) = existing.as_ref()
			&& let Some(pending) = self.storage.pending_run(session.id)?
		{
			self.record(&JournalFact::RunAbandoned {
				run_id: pending.id,
				at_ms:  self.host.now_ms(),
			})?;
		}
		if let Some(session) = existing.as_ref()
			&& let Some(incomplete) = self.storage.incomplete_run(session.id)?
		{
			self.record(&JournalFact::RunAbandoned {
				run_id: incomplete.id,
				at_ms:  self.host.now_ms(),
			})?;
		}
		let id = existing
			.as_ref()
			.map_or(self.storage.next_session_id()?, |session| session.id);
		let segment = existing.as_ref().map_or(0, |session| {
			if params.new_segment {
				session.config.segment.saturating_add(1)
			} else {
				session.config.segment
			}
		});
		let status = self.host.status(cancel).await.map_err(EngineError::Host)?;
		let harness_paths = git::parse_status(&status)
			.into_iter()
			.map(|entry| entry.path)
			.collect::<Vec<_>>();
		if let Some(mutation) = self.mutation
			&& !harness_paths.is_empty()
		{
			let paths = harness_paths.iter().map(Str::as_str).collect::<Vec<_>>();
			let outcome = mutation
				.commit_isolation(
					IsolationCommit::AutoresearchHarness {
						name: params.name.as_str(),
						goal: params.goal.as_deref(),
					},
					&paths,
					cancel,
				)
				.await
				.map_err(GitError::from)?;
			if !outcome.is_applied() {
				return Err(GitError::Rejected.into());
			}
		}
		let baseline_commit = self
			.host
			.head(cancel)
			.await
			.map_err(EngineError::Host)?
			.or(isolation.baseline_commit);
		let unit = params
			.metric_unit
			.unwrap_or_else(|| Str::from(infer_metric_unit(params.primary_metric.as_str())));
		let config = SessionConfig {
			name: params.name,
			goal: params.goal,
			primary_metric: params.primary_metric,
			metric_unit: unit,
			direction: params.direction,
			branch,
			baseline_commit,
			segment,
			max_iterations: params.max_iterations.filter(|value| *value > 0),
			scope_paths: dedupe(params.scope_paths),
			off_limits: dedupe(params.off_limits),
			constraints: dedupe(params.constraints),
			secondary_metrics: dedupe(params.secondary_metrics),
			notes: existing.map_or_else(Str::default, |session| session.config.notes),
		};
		let at_ms = self.host.now_ms();
		let fact = if was_existing {
			JournalFact::SessionUpdated { id, config: config.clone(), at_ms }
		} else {
			JournalFact::SessionOpened { id, config: config.clone(), at_ms }
		};
		self.record(&fact)?;
		self.runtime.goal = config.goal.clone();
		self.runtime.session = Some(config);
		self.runtime.resume_armed = true;
		Ok(id)
	}

	/// Runs the validated fixed harness and records its log artifact.
	pub async fn run_experiment(
		&mut self,
		timeout: Option<Duration>,
		cancel: &CancellationToken,
	) -> Result<RunCompletion, EngineError<H::Error, J::Error>> {
		let session = self.branch_gated_session(cancel).await?;
		if self.storage.pending_run(session.id)?.is_some() {
			return Err(EngineError::PendingRun);
		}
		if let Some(limit) = session.config.max_iterations
			&& self
				.storage
				.segment_run_count(session.id, session.config.segment)?
				>= limit
		{
			return Err(EngineError::IterationLimit);
		}
		let run_id = self.storage.next_run_id()?;
		let pre_status = self.host.status(cancel).await.map_err(EngineError::Host)?;
		let pre_dirty_paths = git::parse_status(&pre_status)
			.into_iter()
			.map(|entry| entry.path)
			.collect();
		let pre_run_head = self.host.head(cancel).await.map_err(EngineError::Host)?;
		let artifact_dir = Str::from(
			self
				.storage
				.paths()
				.project_dir
				.join("runs")
				.join(format!("{run_id:04}"))
				.to_string_lossy()
				.as_ref(),
		);
		let start = RunStart {
			session_id: session.id,
			segment: session.config.segment,
			command: sf!(HARNESS),
			started_at_ms: self.host.now_ms(),
			pre_run_head,
			pre_dirty_paths,
			artifact_dir: artifact_dir.clone(),
		};
		self.record(&JournalFact::RunStarted { id: run_id, start })?;
		let output = self
			.host
			.run_harness(artifact_dir.as_str(), timeout.unwrap_or(DEFAULT_TIMEOUT), cancel)
			.await
			.map_err(EngineError::Host)?;
		let metrics = parse_metric_lines(output.output.as_str());
		let completion = RunCompletion {
			run_id,
			completed_at_ms: self.host.now_ms(),
			duration_ms: output.duration.as_millis().try_into().unwrap_or(i64::MAX),
			exit_code: output.exit_code,
			timed_out: output.timed_out,
			parsed_primary: metrics.get(session.config.primary_metric.as_str()).copied(),
			parsed_metrics: metrics,
			parsed_asi: parse_asi_lines(output.output.as_str()),
		};
		self.record(&JournalFact::RunCompleted(completion.clone()))?;
		self.record(&JournalFact::ArtifactRecorded {
			run_id,
			kind: sf!("benchmark_log"),
			uri: output.benchmark_uri,
			bytes: output.benchmark_bytes,
			at_ms: self.host.now_ms(),
		})?;
		self.runtime.pending_run = Some(run_id);
		self.runtime.resume_armed = true;
		Ok(completion)
	}

	/// Logs, commits or rolls back, and confidence-scores the latest run.
	pub async fn log_experiment(
		&mut self,
		params: LogExperiment,
		cancel: &CancellationToken,
	) -> Result<DispositionSettled, EngineError<H::Error, J::Error>> {
		if !params.metric.is_finite() {
			return Err(EngineError::InvalidMetric);
		}
		let session = self.branch_gated_session(cancel).await?;
		let pending = self
			.storage
			.pending_run(session.id)?
			.ok_or(EngineError::NoPendingRun)?;
		for (run_id, reason) in params.flag_runs {
			self.record(&JournalFact::RunFlagged { run_id, reason, at_ms: self.host.now_ms() })?;
		}
		let status = self.host.status(cancel).await.map_err(EngineError::Host)?;
		let delta = run_delta(&pending.pre_dirty_paths, &status, &session.config);
		let mut metrics = pending.completion.parsed_metrics.clone();
		metrics.remove(session.config.primary_metric.as_str());
		metrics.extend(params.metrics);
		let mut asi = pending.completion.parsed_asi.clone();
		asi.extend(params.asi);
		let intent = DispositionIntent {
			run_id: pending.id,
			status: params.status,
			description: params.description,
			metric: params.metric,
			metrics,
			asi,
			delta,
			justification: params.justification,
			rollback_head: pending.pre_run_head,
			started_at_ms: self.host.now_ms(),
		};
		self.record(&JournalFact::DispositionStarted(intent.clone()))?;
		let commit = self.execute_intent(&intent, false, cancel).await?;
		let mut measurements = self.storage.measurements(session.id)?;
		measurements.push(Measurement {
			metric:  intent.metric,
			status:  intent.status,
			segment: session.config.segment,
			flagged: false,
		});
		let confidence =
			mad_confidence(&measurements, session.config.segment, session.config.direction);
		let settled = DispositionSettled {
			run_id: pending.id,
			commit,
			confidence,
			settled_at_ms: self.host.now_ms(),
		};
		self.record(&JournalFact::DispositionSettled(settled.clone()))?;
		self.runtime.pending_run = None;
		self.runtime.resume_armed = true;
		if let Some(limit) = session.config.max_iterations
			&& self
				.storage
				.segment_run_count(session.id, session.config.segment)?
				>= limit
		{
			self.disable().await?;
		}
		Ok(settled)
	}

	/// Replaces durable playbook notes.
	pub fn update_notes(&mut self, notes: Str) -> Result<(), EngineError<H::Error, J::Error>> {
		let session = self.runtime.session.as_mut().ok_or(EngineError::Inactive)?;
		session.notes = notes.clone();
		let id = self
			.storage
			.active_session(session.branch.as_deref())?
			.ok_or(EngineError::Inactive)?
			.id;
		self.record(&JournalFact::NotesUpdated { id, notes, at_ms: self.host.now_ms() })
	}

	/// Stops autoresearch while retaining experiment history.
	pub async fn disable(&mut self) -> Result<(), EngineError<H::Error, J::Error>> {
		self.release_regime().await?;
		self.runtime.resume_armed = false;
		Ok(())
	}

	/// Cuts a killed loop immediately while retaining experiment history.
	pub async fn kill(&mut self) -> Result<(), EngineError<H::Error, J::Error>> {
		if let Some(activation) = self.runtime.activation.clone() {
			self
				.regimes
				.cancel_autoresearch(activation)
				.await
				.map_err(EngineError::Regime)?;
		}
		self.runtime.activation = None;
		self.runtime.resume_armed = false;
		Ok(())
	}

	/// Clears the active session and optionally rolls back its pending delta.
	pub async fn clear(
		&mut self,
		tree: ClearTree,
		cancel: &CancellationToken,
	) -> Result<(), EngineError<H::Error, J::Error>> {
		let branch = self
			.host
			.current_branch(cancel)
			.await
			.map_err(EngineError::Host)?;
		if tree == ClearTree::Reset
			&& let Some(intent) = self.storage.pending_disposition(branch.as_deref())?
		{
			self.execute_intent(&intent, true, cancel).await?;
		}
		if let Some(session) = self.runtime.session.as_ref()
			&& let Some(projected) = self.storage.active_session(session.branch.as_deref())?
		{
			self.record(&JournalFact::SessionClosed {
				id:    projected.id,
				at_ms: self.host.now_ms(),
			})?;
		}
		self.release_regime().await?;
		self.runtime = RuntimeState::default();
		Ok(())
	}

	/// Recovers an interrupted Git transaction and rehydrates branch-gated
	/// state.
	pub async fn rehydrate(
		&mut self,
		cancel: &CancellationToken,
	) -> Result<(), EngineError<H::Error, J::Error>> {
		let branch = self
			.host
			.current_branch(cancel)
			.await
			.map_err(EngineError::Host)?;
		self.restore_regime().await?;
		if self.runtime.activation.is_none() {
			return Ok(());
		}
		if let Some(intent) = self.storage.pending_disposition(branch.as_deref())? {
			let commit = self.execute_intent(&intent, true, cancel).await?;
			let settled = DispositionSettled {
				run_id: intent.run_id,
				commit,
				confidence: None,
				settled_at_ms: self.host.now_ms(),
			};
			self.record(&JournalFact::DispositionSettled(settled))?;
		}
		if let Some(session) = self.storage.active_session(branch.as_deref())? {
			if let Some(incomplete) = self.storage.incomplete_run(session.id)? {
				let status = self.host.status(cancel).await.map_err(EngineError::Host)?;
				let intent = DispositionIntent {
					run_id:        incomplete.id,
					status:        ExperimentStatus::Crash,
					description:   sf!("abandoned after process crash"),
					metric:        0.0,
					metrics:       Metrics::new(),
					asi:           Asi::new(),
					delta:         run_delta(&incomplete.pre_dirty_paths, &status, &session.config),
					justification: None,
					rollback_head: incomplete.pre_run_head,
					started_at_ms: self.host.now_ms(),
				};
				self.record(&JournalFact::DispositionStarted(intent.clone()))?;
				let commit = self.execute_intent(&intent, true, cancel).await?;
				self.record(&JournalFact::DispositionSettled(DispositionSettled {
					run_id: incomplete.id,
					commit,
					confidence: None,
					settled_at_ms: self.host.now_ms(),
				}))?;
			}
			self.runtime.pending_run = self.storage.pending_run(session.id)?.map(|run| run.id);
			self.runtime.goal = session.config.goal.clone();
			self.runtime.session = Some(session.config);
			self.runtime.resume_armed = self.runtime.activation.is_some();
		}
		Ok(())
	}

	/// Hidden continuation prompt admitted only after the parent settles.
	pub fn continuation_prompt(&mut self, parent_settled: bool) -> Option<Str> {
		if !parent_settled || self.runtime.activation.is_none() || !self.runtime.resume_armed {
			return None;
		}
		self.runtime.resume_armed = false;
		let session = self.runtime.session.as_ref()?;
		let phase = if self.runtime.pending_run.is_some() {
			"log the pending experiment"
		} else {
			"continue the next experiment"
		};
		Some(Str::from(format!(
			"Autoresearch resumes after settlement. Goal: {}. Primary metric: {} ({} is better). \
			 Segment: {}. Best-known notes:\n{}\nNow {phase}; obey scope and constraints.",
			session
				.goal
				.as_deref()
				.unwrap_or("improve the measured system"),
			session.primary_metric,
			match session.direction {
				MetricDirection::Lower => "lower",
				MetricDirection::Higher => "higher",
			},
			session.segment,
			session.notes,
		)))
	}

	/// Changes dashboard presentation without mutating experiment truth.
	pub const fn set_dashboard(&mut self, mode: DashboardMode) {
		self.runtime.dashboard = mode;
	}

	async fn execute_intent(
		&self,
		intent: &DispositionIntent,
		recovery: bool,
		cancel: &CancellationToken,
	) -> Result<Option<Str>, EngineError<H::Error, J::Error>> {
		if let Some(mutation) = self.mutation {
			return Ok(if recovery {
				recover(self.host, mutation, intent, cancel).await?
			} else {
				settle(self.host, mutation, intent, cancel).await?
			});
		}
		if intent.status == ExperimentStatus::Keep {
			return self.host.head(cancel).await.map_err(EngineError::Host);
		}
		self
			.host
			.rollback_unisolated(
				intent.rollback_head.as_deref(),
				&intent.delta.tracked,
				&intent.delta.untracked,
				cancel,
			)
			.await
			.map_err(EngineError::Host)?;
		Ok(None)
	}

	async fn ensure_regime(&mut self) -> Result<bool, EngineError<H::Error, J::Error>> {
		if self.runtime.activation.is_some() {
			return Ok(false);
		}
		let (activation, newly_started) = acquire_regime(self.regimes)
			.await
			.map_err(EngineError::Regime)?;
		self.runtime.activation = Some(activation);
		Ok(newly_started)
	}

	async fn restore_regime(&mut self) -> Result<(), EngineError<H::Error, J::Error>> {
		self.runtime.activation = revived_activation(
			self
				.regimes
				.active_regimes()
				.await
				.map_err(EngineError::Regime)?,
		);
		self.runtime.resume_armed = self.runtime.activation.is_some();
		Ok(())
	}

	async fn release_regime(&mut self) -> Result<(), EngineError<H::Error, J::Error>> {
		let Some(activation) = self.runtime.activation.clone() else {
			return Ok(());
		};
		self
			.regimes
			.stop_autoresearch(activation)
			.await
			.map_err(EngineError::Regime)?;
		self.runtime.activation = None;
		Ok(())
	}

	async fn branch_gated_session(
		&self,
		cancel: &CancellationToken,
	) -> Result<ProjectedSession, EngineError<H::Error, J::Error>> {
		if self.runtime.activation.is_none() {
			return Err(EngineError::Inactive);
		}
		let branch = self
			.host
			.current_branch(cancel)
			.await
			.map_err(EngineError::Host)?;
		self
			.storage
			.active_session(branch.as_deref())?
			.ok_or(EngineError::Inactive)
	}

	fn record(&mut self, fact: &JournalFact) -> Result<(), EngineError<H::Error, J::Error>> {
		self.storage.append_and_project(self.journal, fact)?;
		Ok(())
	}
}

async fn acquire_regime(regimes: &impl AutoresearchRegimes) -> Result<(Str, bool), ControlError> {
	if let Some(activation) = revived_activation(regimes.active_regimes().await?) {
		return Ok((activation, false));
	}
	let receipt = regimes.start_autoresearch().await?;
	debug_assert_eq!(
		receipt.outcome,
		AcquireOutcome::Granted,
		"non-queued activation either grants or returns a typed resource error"
	);
	Ok((receipt.activation, true))
}

fn revived_activation(entries: Vec<RegimeRecord>) -> Option<Str> {
	entries
		.into_iter()
		.find(|entry| entry.spec_id.as_str() == REGIME_ID && entry.status == RegimeStatus::Active)
		.map(|entry| entry.activation)
}

fn dedupe(values: Vec<Str>) -> Vec<Str> {
	let mut output = Vec::new();
	for value in values {
		let value = value.trim();
		if value.is_empty() || output.iter().any(|existing: &Str| *existing == value) {
			continue;
		}
		output.push(Str::from(value));
	}
	output
}

#[cfg(test)]
mod tests {
	use parking_lot::Mutex;

	use super::*;

	struct RegimeHarness {
		regimes: Mutex<omp_agent::RegimeSet>,
		now_ms:  u64,
	}

	impl RegimeHarness {
		fn new(now_ms: u64) -> Self {
			Self { regimes: Mutex::new(omp_agent::RegimeSet::new()), now_ms }
		}

		fn start(&self, spec_id: &str) -> Result<StartReceipt, omp_agent::StartError> {
			let (spec, regime) = omp_agent::core_regime(spec_id).expect("core regime");
			self
				.regimes
				.lock()
				.start(spec, regime, omp_agent::StartOptions { now_ms: self.now_ms, queue: false })
		}
	}

	impl AutoresearchRegimes for RegimeHarness {
		async fn active_regimes(&self) -> Result<Vec<RegimeRecord>, ControlError> {
			Ok(self.regimes.lock().records())
		}

		async fn start_autoresearch(&self) -> Result<StartReceipt, ControlError> {
			self.start(REGIME_ID).map_err(ControlError::from)
		}

		async fn stop_autoresearch(&self, activation: Str) -> Result<bool, ControlError> {
			self
				.regimes
				.lock()
				.stop(activation.as_str(), self.now_ms)
				.map_err(ControlError::from)
		}

		async fn cancel_autoresearch(&self, activation: Str) -> Result<bool, ControlError> {
			Ok(self.regimes.lock().cancel(activation.as_str()))
		}
	}

	#[tokio::test]
	async fn autoresearch_denial_preserves_plan_holder_facts() {
		let regimes = RegimeHarness::new(41);
		let plan = regimes.start("plan").expect("plan activation");

		let error = acquire_regime(&regimes)
			.await
			.expect_err("plan owns mode and worktree");

		assert!(matches!(
			error,
			ControlError::RegimeStart(omp_agent::StartError::Acquire {
				resource: omp_agent::Resource::Mode,
				outcome: AcquireOutcome::Denied { holder, since: 41 },
			}) if holder == plan.activation
		));
	}

	#[tokio::test]
	async fn autoresearch_revival_reuses_durable_activation() {
		let regimes = RegimeHarness::new(73);
		let first = acquire_regime(&regimes).await.expect("initial activation");
		assert!(first.1);

		let revived = acquire_regime(&regimes).await.expect("revived activation");
		assert_eq!(revived, (first.0, false));
		assert_eq!(regimes.active_regimes().await.expect("entries").len(), 1);
	}
}
