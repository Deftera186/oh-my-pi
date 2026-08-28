//! Token-throughput measurement shared by live and finalized status facts.

use std::{
	collections::HashSet,
	time::{Duration, Instant},
};

use omp_core::{Str, sf};

const MIN_SAMPLE_DURATION: Duration = Duration::from_millis(100);
const APPROXIMATE_UTF8_BYTES_PER_TOKEN: u64 = 4;

/// One daily quota window shown in the status line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusQuotaWindow {
	/// Rounded consumed percentage.
	pub percent:       u8,
	/// Whole minutes until reset, when the provider supplied a reset instant.
	pub reset_minutes: Option<u64>,
}

/// Active model-family quota projected by the host.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StatusQuota {
	/// Daily quota window.
	pub daily: Option<StatusQuotaWindow>,
}

/// Counts running jobs that are not the same running subagents shown
/// separately.
///
/// Filtering by stable identity, rather than job kind, preserves queued task
/// jobs and every independent background job.
pub fn independent_job_count(
	running_jobs: &HashSet<Str>,
	running_subagents: &HashSet<Str>,
) -> usize {
	running_jobs
		.iter()
		.filter(|id| !running_subagents.contains(*id))
		.count()
}

/// Formats daily quota with minute-precision reset context.
pub fn daily_quota_label(window: StatusQuotaWindow) -> Str {
	let Some(minutes) = window.reset_minutes else {
		return sf!("1d {}%", window.percent);
	};
	let reset = if minutes < 60 {
		sf!("{minutes}m")
	} else {
		let hours = minutes / 60;
		let minutes = minutes % 60;
		if minutes == 0 {
			sf!("{hours}h")
		} else {
			sf!("{hours}h {minutes}m")
		}
	};
	sf!("1d {}% ({reset})", window.percent)
}

/// Streaming token-rate accumulator.
///
/// Provider receipts remain authoritative at finalization. While a provider has
/// not reported usage, output bytes provide a bounded live estimate so the
/// status line does not remain blank for the entire generation.
#[derive(Clone, Copy, Debug)]
pub struct TokenRateMeter {
	started:        Instant,
	streamed_bytes: u64,
	final_tokens:   Option<u64>,
}

impl TokenRateMeter {
	/// Starts a fresh generation sample.
	pub fn start(now: Instant) -> Self {
		Self { started: now, streamed_bytes: 0, final_tokens: None }
	}

	/// Adds one visible provider text fragment to the live estimate.
	pub fn observe_fragment(&mut self, fragment: &str) {
		self.streamed_bytes = self
			.streamed_bytes
			.saturating_add(u64::try_from(fragment.len()).unwrap_or(u64::MAX));
	}

	/// Replaces the estimate with authoritative provider output usage.
	pub const fn finalize(&mut self, output_tokens: u64) {
		self.final_tokens = Some(output_tokens);
	}

	/// Calculates rounded tokens per second at `now`.
	pub fn rate(&self, now: Instant) -> Option<u64> {
		let elapsed = now.saturating_duration_since(self.started);
		if elapsed < MIN_SAMPLE_DURATION {
			return None;
		}
		let tokens = self.final_tokens.unwrap_or_else(|| {
			self
				.streamed_bytes
				.saturating_add(APPROXIMATE_UTF8_BYTES_PER_TOKEN - 1)
				/ APPROXIMATE_UTF8_BYTES_PER_TOKEN
		});
		if tokens == 0 {
			return None;
		}
		let rate = tokens as f64 / elapsed.as_secs_f64();
		(rate.is_finite() && rate > 0.0).then(|| rate.round() as u64)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn subagent_identity_filter_preserves_queued_and_background_jobs() {
		let running_jobs = HashSet::from([
			Str::new_static("sub-running"),
			Str::new_static("task-queued"),
			Str::new_static("bash-background"),
			Str::new_static("eval-background"),
		]);
		let running_subagents = HashSet::from([Str::new_static("sub-running")]);
		let actual = independent_job_count(&running_jobs, &running_subagents);
		assert_eq!(
			actual, 3,
			"jobs={running_jobs:?}, subagents={running_subagents:?}, actual={actual}"
		);
	}

	#[test]
	fn daily_quota_retains_reset_minute_precision() {
		let actual =
			daily_quota_label(StatusQuotaWindow { percent: 24, reset_minutes: Some(71) });
		assert_eq!(actual.as_str(), "1d 24% (1h 11m)", "actual={actual}");
	}
}
