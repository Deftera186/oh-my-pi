//! Environment-host projection of the typed command-stream control context.
//!
//! The host enforces its tool, runtime, worktree, memory, and autolearn policy
//! from one immutable `from_con` projection at composition time.

use std::{fmt, path::PathBuf};

use omp_con::Ctx;
use omp_core::{Duration, DurationError, Str};
use omp_memory::config::{AutolearnSettings, MemorySettings, MnemopiSettings};
use omp_tool::DEFAULT_INTERRUPT_GRACE;
use serde::{
	Deserialize, Deserializer, Serialize, Serializer,
	de::{self, Visitor},
};

use super::tool_settings::ToolSettings;

/// Memory backend value exposed by the command stream.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	strum::EnumString,
	strum::IntoStaticStr,
	strum::VariantNames,
)]
#[strum(serialize_all = "kebab-case")]
pub enum MemoryBackendSetting {
	/// Disable durable memory.
	#[default]
	Off,
	/// Use Mnemopi durable memory.
	Mnemopi,
}

omp_con::con_enum!(MemoryBackendSetting);

/// Mnemopi bank scope exposed by the command stream.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	strum::EnumString,
	strum::IntoStaticStr,
	strum::VariantNames,
)]
#[strum(serialize_all = "kebab-case")]
pub enum BankScopingSetting {
	/// Use one global bank.
	Global,
	/// Use one canonical-project bank.
	#[default]
	PerProject,
	/// Recall from project and shared banks.
	PerProjectTagged,
}

omp_con::con_enum!(BankScopingSetting);

omp_con::var! {
	/// Courtesy interval before forced interruption.
	pub static SV_INTERRUPT_GRACE = sv_interrupt_grace: Duration {
		default: DEFAULT_INTERRUPT_GRACE,
		flags: archive | inherit,
	};
	/// Default-off durable memory backend.
	pub static AI_MEMORY_BACKEND = ai_memory_backend: MemoryBackendSetting {
		default: MemoryBackendSetting::Off,
		flags: archive | inherit,
	};
	/// Canonical-project and shared-bank recall policy.
	pub static AI_MNEMOPI_SCOPING = ai_mnemopi_scoping: BankScopingSetting {
		default: BankScopingSetting::PerProject,
		flags: archive | inherit,
	};
	/// Enable managed-skill guidance and capture eligibility.
	pub static AI_AUTOLEARN_ENABLED = ai_autolearn_enabled: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Run one private managed-skill or lesson capture after a substantive turn.
	pub static AI_AUTOLEARN_AUTO_CONTINUE = ai_autolearn_auto_continue: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Minimum settled tool executions required in one primary turn.
	pub static AI_AUTOLEARN_MIN_TOOL_CALLS = ai_autolearn_min_tool_calls: i64 {
		default: 5,
		min: 0,
		flags: archive | inherit,
	};
	/// Base directory for Environment-owned isolated worktrees; empty selects the default.
	pub static SV_WORKTREE_BASE = sv_worktree_base: Str {
		default: Str::default(),
		flags: archive | inherit,
	};
}

/// Runtime durations shared by the agent, eval, and extension-host control
/// planes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeDurations {
	/// Courtesy interval between cooperative cancellation and forced
	/// interruption.
	#[serde(with = "nonzero_duration")]
	pub interrupt_grace: Duration,
}

impl Default for RuntimeDurations {
	fn default() -> Self {
		Self { interrupt_grace: DEFAULT_INTERRUPT_GRACE }
	}
}

/// Placement policy for Environment-owned isolated worktrees.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorktreeSettings {
	/// Optional base directory. `OMP_WORKTREE_DIR` takes precedence.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub base: Option<PathBuf>,
}

/// Settings the environment host reads without owning the client aggregate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostSettings {
	/// Model key selected as the default, used to pin the edit dialect.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub default_model: Option<String>,
	/// Runtime timeout and cancellation settings.
	#[serde(default)]
	pub runtime:       RuntimeDurations,
	/// Built-in tool exposure and execution timeout policy.
	#[serde(default)]
	pub tools:         ToolSettings,
	/// Default-off memory backend selector.
	#[serde(default)]
	pub memory:        MemorySettings,
	/// Mnemopi-specific durable bank and lifecycle settings.
	#[serde(default)]
	pub mnemopi:       MnemopiSettings,
	/// Automatic-learning capture settings.
	#[serde(default)]
	pub autolearn:     AutolearnSettings,
	/// Isolated worktree placement policy.
	#[serde(default)]
	pub worktree:      WorktreeSettings,
}

impl HostSettings {
	/// Resolves host policy from the process control context.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		let mut settings = Self::default();
		settings.default_model = omp_catalog::settings::ModelSettings::from_con(ctx)
			.role_selector("default")
			.map(ToString::to_string);
		settings.runtime.interrupt_grace = SV_INTERRUPT_GRACE.get(ctx);
		settings.tools = ToolSettings::from_con(ctx);
		settings.memory.backend = match AI_MEMORY_BACKEND.get(ctx) {
			MemoryBackendSetting::Off => omp_memory::MemoryBackend::Off,
			MemoryBackendSetting::Mnemopi => omp_memory::MemoryBackend::Mnemopi,
		};
		settings.mnemopi.scoping = match AI_MNEMOPI_SCOPING.get(ctx) {
			BankScopingSetting::Global => omp_memory::config::BankScoping::Global,
			BankScopingSetting::PerProject => omp_memory::config::BankScoping::PerProject,
			BankScopingSetting::PerProjectTagged => omp_memory::config::BankScoping::PerProjectTagged,
		};
		settings.autolearn = AutolearnSettings {
			enabled:        AI_AUTOLEARN_ENABLED.get(ctx),
			auto_continue:  AI_AUTOLEARN_AUTO_CONTINUE.get(ctx),
			min_tool_calls: AI_AUTOLEARN_MIN_TOOL_CALLS.get(ctx) as usize,
		};
		settings.worktree.base = {
			let base = SV_WORKTREE_BASE.get(ctx);
			(!base.is_empty()).then(|| PathBuf::from(base.as_str()))
		};
		settings.mnemopi = settings.mnemopi.normalize();
		settings
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn host_settings_project_from_con() {
		let ctx = Ctx::new();
		let grace: Duration = "750ms".parse().expect("duration");
		SV_INTERRUPT_GRACE
			.set(&ctx, grace)
			.expect("set interrupt grace");
		AI_MEMORY_BACKEND
			.set(&ctx, MemoryBackendSetting::Mnemopi)
			.expect("set memory backend");
		AI_AUTOLEARN_MIN_TOOL_CALLS
			.set(&ctx, 9)
			.expect("set threshold");
		let settings = HostSettings::from_con(&ctx);
		assert_eq!(settings.runtime.interrupt_grace, grace);
		assert_eq!(settings.memory.backend, omp_memory::MemoryBackend::Mnemopi);
		assert_eq!(settings.autolearn.min_tool_calls, 9);
	}
}

mod nonzero_duration {
	use super::*;

	pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.collect_str(value)
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
	where
		D: Deserializer<'de>,
	{
		deserializer.deserialize_str(DurationVisitor)
	}

	struct DurationVisitor;

	impl Visitor<'_> for DurationVisitor {
		type Value = Duration;

		fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
			formatter.write_str("a positive integer duration with an explicit ns/us/ms/s/m/h unit")
		}

		fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
		where
			E: de::Error,
		{
			let duration = value.parse::<Duration>().map_err(E::custom)?;
			if duration.value() == 0 {
				return Err(E::custom("duration must be greater than zero"));
			}
			let standard = duration.to_std().map_err(|error| match error {
				DurationError::Overflow => E::custom("duration is too large"),
				other => E::custom(other),
			})?;
			i64::try_from(standard.as_nanos())
				.map_err(|_| E::custom("duration is too large for telemetry serialization"))?;
			Ok(duration)
		}
	}
}
