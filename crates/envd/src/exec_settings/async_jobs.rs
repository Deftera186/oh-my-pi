//! Native async-job capacity, retention, and wait settings.

use omp_con::Ctx;
use serde::{Deserialize, Serialize};

/// Maximum duration used by an implicit background-job wait.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Eq,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
	strum::VariantNames,
)]
pub enum PollWaitDuration {
	/// Wait five seconds.
	#[serde(rename = "5s")]
	#[strum(to_string = "5s")]
	Seconds5,
	/// Wait ten seconds.
	#[serde(rename = "10s")]
	#[strum(to_string = "10s")]
	Seconds10,
	/// Wait thirty seconds.
	#[serde(rename = "30s")]
	#[strum(to_string = "30s")]
	Seconds30,
	/// Wait one minute.
	#[serde(rename = "1m")]
	#[strum(to_string = "1m")]
	Minute1,
	/// Wait five minutes.
	#[serde(rename = "5m")]
	#[strum(to_string = "5m")]
	Minutes5,
	/// Apply the adaptive wait ladder.
	#[default]
	#[serde(rename = "smart")]
	#[strum(to_string = "smart")]
	Smart,
}

omp_con::con_enum!(PollWaitDuration);

omp_con::var! {
	/// Enable detached shell, task, and evaluation jobs.
	pub static SV_ASYNC_ENABLED = sv_async_enabled: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Maximum running detached jobs; zero removes the capacity ceiling.
	pub static SV_ASYNC_MAX_JOBS = sv_async_max_jobs: u32 {
		default: 100,
		flags: archive | inherit,
	};
	/// Milliseconds to retain terminal job rows for observation.
	pub static SV_ASYNC_RETENTION_MS = sv_async_retention_ms: i64 {
		default: 300_000,
		min: 0,
		flags: archive | inherit,
	};
	/// Maximum implicit wait, or the adaptive wait ladder.
	pub static SV_ASYNC_POLL_WAIT_DURATION = sv_async_poll_wait_duration: PollWaitDuration {
		default: PollWaitDuration::Smart,
		flags: archive | inherit,
	};
}

/// Settings consumed by the authoritative async job board.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsyncJobSettings {
	/// Whether detached execution is available.
	pub enabled:            bool,
	/// Concurrent running-job capacity; zero means unlimited.
	pub max_jobs:           u32,
	/// Duration terminal rows remain observable after settlement.
	pub retention_ms:       u64,
	/// Implicit wait duration or adaptive policy.
	pub poll_wait_duration: PollWaitDuration,
}

impl Default for AsyncJobSettings {
	fn default() -> Self {
		Self {
			enabled:            true,
			max_jobs:           100,
			retention_ms:       5 * 60 * 1_000,
			poll_wait_duration: PollWaitDuration::Smart,
		}
	}
}

impl AsyncJobSettings {
	/// Resolves async-job policy from the process control context.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self {
			enabled:            SV_ASYNC_ENABLED.get(ctx),
			max_jobs:           SV_ASYNC_MAX_JOBS.get(ctx),
			retention_ms:       SV_ASYNC_RETENTION_MS.get(ctx) as u64,
			poll_wait_duration: SV_ASYNC_POLL_WAIT_DURATION.get(ctx),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn async_con_projection_round_trips() {
		let ctx = Ctx::new();
		SV_ASYNC_MAX_JOBS.set(&ctx, 0).expect("set max jobs");
		SV_ASYNC_RETENTION_MS
			.set(&ctx, 42_000)
			.expect("set retention");
		SV_ASYNC_POLL_WAIT_DURATION
			.set(&ctx, PollWaitDuration::Seconds30)
			.expect("set wait");
		assert_eq!(AsyncJobSettings::from_con(&ctx), AsyncJobSettings {
			enabled:            true,
			max_jobs:           0,
			retention_ms:       42_000,
			poll_wait_duration: PollWaitDuration::Seconds30,
		});
	}
}
