//! Session memory notifications and bounded, ordered shutdown coordination.

use std::{
	sync::Arc,
	time::{Duration, Instant},
};

use omp_core::Str;
use thiserror::Error;
use tokio::{
	task::{JoinError, JoinHandle},
	time,
};

use crate::PromptMemoryInput;

/// Borrowed immutable facts for one fresh turn's proactive recall lookup.
#[derive(Clone, Copy, Debug)]
pub struct PromptMemoryQuery<'a> {
	turn_id:     &'a str,
	item_events: &'a [u64],
	user_text:   &'a str,
}

impl<'a> PromptMemoryQuery<'a> {
	/// Creates a query from committed turn-input journal ids and bounded user
	/// text.
	pub const fn new(turn_id: &'a str, item_events: &'a [u64], user_text: &'a str) -> Self {
		Self { turn_id, item_events, user_text }
	}

	/// Stable fresh turn identity.
	pub const fn turn_id(self) -> &'a str {
		self.turn_id
	}

	/// Committed physical turn-input item ids.
	pub const fn item_events(self) -> &'a [u64] {
		self.item_events
	}

	/// Bounded canonical user text for semantic recall.
	pub const fn user_text(self) -> &'a str {
		self.user_text
	}
}

/// App/runtime adapter supplying one immutable prompt-memory snapshot per turn.
pub trait PromptMemorySnapshotSource: Send + Sync {
	/// Returns the current Memory, Standing, then Recall slot snapshot.
	fn snapshot(&self, query: PromptMemoryQuery<'_>) -> PromptMemoryInput;
}

/// Memory lifecycle transition emitted after its durable session boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryHookEvent {
	/// Extract enabled session memory after primary work settles.
	Extract {
		/// Stable session identity.
		session_id: Str,
		/// Canonical primary-root bank key.
		bank_key:   Str,
		/// Bounded durable source window resolved by the app journal owner.
		window:     MemoryExtractionWindow,
	},
	/// Move subsequent memory operations to a new canonical primary-root bank.
	Rekey {
		/// Previous bank key.
		from: Str,
		/// New bank key.
		to:   Str,
	},
	/// Clear volatile recall after a durable session reset.
	Reset {
		/// Stable session identity.
		session_id: Str,
	},
	/// Cancel branch-owned extraction and consolidation.
	CancelBranch {
		/// Stable session identity.
		session_id: Str,
	},
}

/// Maximum durable journal items supplied to one memory extraction.
pub const MAX_MEMORY_EXTRACTION_ITEMS: usize = 4096;

/// Immutable settled transcript window for one memory extraction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryExtractionWindow {
	turn_id:          Str,
	source_memory_id: Option<Str>,
	item_events:      Arc<[u64]>,
}

impl MemoryExtractionWindow {
	/// Validates a bounded, strictly increasing set of physical journal item
	/// ids.
	pub fn new(
		turn_id: Str,
		source_memory_id: Option<Str>,
		item_events: Arc<[u64]>,
	) -> Result<Self, MemoryExtractionWindowError> {
		if item_events.is_empty() {
			return Err(MemoryExtractionWindowError::Empty);
		}
		if item_events.len() > MAX_MEMORY_EXTRACTION_ITEMS {
			return Err(MemoryExtractionWindowError::TooManyItems {
				actual:  item_events.len(),
				maximum: MAX_MEMORY_EXTRACTION_ITEMS,
			});
		}
		if item_events.windows(2).any(|pair| pair[0] >= pair[1]) {
			return Err(MemoryExtractionWindowError::NonMonotonic);
		}
		Ok(Self { turn_id, source_memory_id, item_events })
	}

	/// Stable settled turn identity.
	pub const fn turn_id(&self) -> &Str {
		&self.turn_id
	}

	/// Previous memory record to consolidate, when this extraction supersedes
	/// one.
	pub const fn source_memory_id(&self) -> Option<&Str> {
		self.source_memory_id.as_ref()
	}

	/// Physical journal item ids resolved by the app-owned journal authority.
	pub fn item_events(&self) -> &[u64] {
		&self.item_events
	}
}

/// Invalid memory extraction source window.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MemoryExtractionWindowError {
	/// Extraction must have at least one committed journal item.
	#[error("memory extraction window is empty")]
	Empty,
	/// The extraction exceeds the hard source-item bound.
	#[error("memory extraction has {actual} items; maximum is {maximum}")]
	TooManyItems {
		/// Requested item count.
		actual:  usize,
		/// Hard item bound.
		maximum: usize,
	},
	/// Physical ids must be unique and strictly increasing.
	#[error("memory extraction item ids are not strictly increasing")]
	NonMonotonic,
}

/// Default-off memory hook publisher.
#[derive(Clone, Debug)]
pub struct MemoryHooks {
	enabled: bool,
	sender:  flume::Sender<MemoryHookEvent>,
}

impl MemoryHooks {
	/// Creates a publisher. Disabled publishers perform no sends or memory work.
	pub const fn new(enabled: bool, sender: flume::Sender<MemoryHookEvent>) -> Self {
		Self { enabled, sender }
	}

	/// Publishes one transition after its durable journal boundary.
	///
	/// A disconnected consumer is equivalent to disabled memory during teardown.
	pub fn publish(&self, event: MemoryHookEvent) -> bool {
		self.enabled && self.sender.try_send(event).is_ok()
	}

	/// Whether this session may perform memory work.
	pub const fn enabled(&self) -> bool {
		self.enabled
	}
}

/// Strict shutdown order. Earlier stages must settle or exhaust their budget
/// before later authorities are released.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum ShutdownStage {
	/// Stop accepting and settle detached/child jobs.
	Jobs,
	/// Drain and cancel advisor child loops.
	Advisors,
	/// Interrupt and dispose evaluation kernels.
	Kernels,
	/// Complete or cancel post-session skill capture.
	Autolearn,
	/// Run bounded memory extraction and consolidation.
	Memory,
	/// Dispatch final lifecycle hooks after durable work settles.
	Hooks,
	/// Release platform sleep/power assertions last.
	Power,
}

/// One spawned stage owned by the lifecycle coordinator.
pub struct ShutdownTask {
	stage: ShutdownStage,
	task:  JoinHandle<()>,
}

impl ShutdownTask {
	/// Associates an already-spawned task with its shutdown stage.
	pub const fn new(stage: ShutdownStage, task: JoinHandle<()>) -> Self {
		Self { stage, task }
	}
}

/// Terminal outcome of one ordered shutdown stage.
#[derive(Debug)]
pub enum ShutdownOutcome {
	/// Stage settled within the shared deadline.
	Settled(ShutdownStage),
	/// Stage panicked or was externally cancelled.
	Failed {
		/// Failed stage.
		stage:  ShutdownStage,
		/// Tokio task failure.
		source: JoinError,
	},
	/// Shared shutdown deadline elapsed; this and all later tasks were aborted.
	TimedOut(ShutdownStage),
}

/// Runs lifecycle tasks serially under one absolute shutdown budget.
///
/// Ordering is derived from [`ShutdownStage`], not caller insertion order. A
/// timeout aborts the current task and every unstarted later task so no
/// background memory/advisor work can orphan itself after power release.
pub async fn shutdown_ordered(
	mut tasks: Vec<ShutdownTask>,
	budget: Duration,
) -> Vec<ShutdownOutcome> {
	tracing::info!(task_count = tasks.len(), "agent lifecycle shutdown started");
	tasks.sort_by_key(|task| task.stage);
	let deadline = Instant::now() + budget;
	let mut outcomes = Vec::with_capacity(tasks.len());
	let mut tasks = tasks.into_iter();
	while let Some(mut task) = tasks.next() {
		let remaining = deadline.saturating_duration_since(Instant::now());
		if remaining.is_zero() {
			tracing::warn!(
				stage = %task.stage,
				aborted_later = tasks.len(),
				"agent lifecycle shutdown timed out"
			);
			task.task.abort();
			outcomes.push(ShutdownOutcome::TimedOut(task.stage));
			for later in tasks {
				later.task.abort();
				outcomes.push(ShutdownOutcome::TimedOut(later.stage));
			}
			break;
		}
		match time::timeout(remaining, &mut task.task).await {
			Ok(Ok(())) => {
				tracing::info!(stage = %task.stage, "agent lifecycle stage stopped");
				outcomes.push(ShutdownOutcome::Settled(task.stage));
			},
			Ok(Err(source)) => {
				tracing::warn!(
					stage = %task.stage,
					%source,
					"agent lifecycle stage failed while stopping"
				);
				outcomes.push(ShutdownOutcome::Failed { stage: task.stage, source });
			},
			Err(_) => {
				tracing::warn!(
					stage = %task.stage,
					aborted_later = tasks.len(),
					"agent lifecycle shutdown timed out"
				);
				task.task.abort();
				outcomes.push(ShutdownOutcome::TimedOut(task.stage));
				for later in tasks {
					later.task.abort();
					outcomes.push(ShutdownOutcome::TimedOut(later.stage));
				}
				break;
			},
		}
	}
	tracing::info!(outcome_count = outcomes.len(), "agent lifecycle shutdown completed");
	outcomes
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn extraction_window_requires_bounded_monotonic_durable_ids() {
		assert_eq!(
			MemoryExtractionWindow::new(Str::new_static("turn"), None, Arc::from([])),
			Err(MemoryExtractionWindowError::Empty)
		);
		assert_eq!(
			MemoryExtractionWindow::new(
				Str::new_static("turn"),
				Some(Str::new_static("memory-1")),
				Arc::from([2, 2])
			),
			Err(MemoryExtractionWindowError::NonMonotonic)
		);
		let window = MemoryExtractionWindow::new(
			Str::new_static("turn"),
			Some(Str::new_static("memory-1")),
			Arc::from([2, 5, 9]),
		)
		.expect("valid extraction window");
		assert_eq!(window.item_events(), &[2, 5, 9]);
	}
}
