//! Run-scoped host sleep-prevention assertions.

mod settings;

use std::{io, path::Path, process::Child, sync::Arc};

use omp_settings::manager::SettingsManagerError;
use parking_lot::Mutex;
pub use settings::PowerSettings;
use strum::{Display, EnumString};
use thiserror::Error;

/// Loads the configured sleep-prevention mode through the settings authority.
pub fn configured(data_dir: &Path) -> Result<SleepPrevention, SettingsManagerError> {
	Ok(settings::current(data_dir)?.sleep_prevention)
}

/// Configured macOS sleep-prevention strength.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Display,
	EnumString,
	Eq,
	PartialEq,
	serde::Deserialize,
	serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum SleepPrevention {
	/// Do not acquire a power assertion.
	Off,
	/// Prevent idle system sleep while the agent is actively running.
	#[default]
	Idle,
	/// Prevent display idle sleep while the agent is actively running.
	Display,
	/// Prevent system sleep while the agent is actively running.
	System,
}

/// Failure to acquire the configured host power assertion.
#[derive(Debug, Error)]
pub enum PowerError {
	/// The operating-system assertion helper could not be started.
	#[error("failed to acquire macOS sleep-prevention assertion")]
	Spawn(#[from] io::Error),
}

/// RAII assertion released on success, failure, cancellation, panic unwind, or
/// shutdown.
#[must_use]
pub struct PowerAssertion {
	child: Option<Child>,
}

impl PowerAssertion {
	/// Acquires the configured assertion for the lifetime of the returned guard.
	pub fn acquire(mode: SleepPrevention) -> Result<Self, PowerError> {
		#[cfg(target_os = "macos")]
		{
			if mode == SleepPrevention::Off {
				return Ok(Self { child: None });
			}
			let flag = match mode {
				SleepPrevention::Idle => "-i",
				SleepPrevention::Display => "-d",
				SleepPrevention::System => "-s",
				SleepPrevention::Off => unreachable!("off returned above"),
			};
			#[cfg(target_os = "macos")]
			use std::process;

			let child = process::Command::new("/usr/bin/caffeinate")
				.args([flag, "-w", &process::id().to_string()])
				.stdin(process::Stdio::null())
				.stdout(process::Stdio::null())
				.stderr(process::Stdio::null())
				.spawn()?;
			Ok(Self { child: Some(child) })
		}
		#[cfg(not(target_os = "macos"))]
		{
			let _ = mode;
			Ok(Self { child: None })
		}
	}

	/// Returns whether this process currently owns a host assertion helper.
	pub const fn is_active(&self) -> bool {
		self.child.is_some()
	}
}

/// Shared activity hook installed on an agent loop.
pub struct PowerActivity {
	mode:      SleepPrevention,
	assertion: Mutex<Option<PowerAssertion>>,
}

impl PowerActivity {
	/// Creates a run activity hook for `mode`.
	pub fn new(mode: SleepPrevention) -> Arc<Self> {
		Arc::new(Self { mode, assertion: Mutex::new(None) })
	}
}

impl omp_agent::RunActivity for PowerActivity {
	fn enter(&self) {
		let mut assertion = self.assertion.lock();
		if assertion.is_none()
			&& let Ok(acquired) = PowerAssertion::acquire(self.mode)
		{
			*assertion = Some(acquired);
		}
	}

	fn exit(&self) {
		self.assertion.lock().take();
	}
}

impl Drop for PowerAssertion {
	fn drop(&mut self) {
		if let Some(mut child) = self.child.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}
