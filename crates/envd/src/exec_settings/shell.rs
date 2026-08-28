//! Native shell profile, interception, direnv, and minimizer settings.

use omp_core::Str;
use omp_settings::{
	FieldDescriptor, OptionProvider, SettingKind, SettingOption, SettingScope, SettingsDomain,
	ValidationError,
};
use serde::{Deserialize, Serialize};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const PROFILE_VALUES: &[&str] = &["brush", "user", "bash", "zsh", "fish"];
const PROFILE_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "brush",
		label:       "Brush",
		description: Some("Deterministic embedded shell."),
	},
	SettingOption {
		value:       "user",
		label:       "User shell",
		description: Some("Use a supported configured user shell."),
	},
	SettingOption { value: "bash", label: "Bash", description: None },
	SettingOption { value: "zsh", label: "Zsh", description: None },
	SettingOption { value: "fish", label: "Fish", description: None },
];
const DIRENV_VALUES: &[&str] = &["auto", "off"];
const OUTLINE_VALUES: &[&str] = &["default", "aggressive"];

/// Shell implementation requested for newly opened exec sessions.
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
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ShellProfile {
	/// Deterministic embedded Brush shell.
	#[default]
	Brush,
	/// Supported shell selected from explicit executable or environment
	/// metadata.
	User,
	/// Explicit Bash profile.
	Bash,
	/// Explicit Zsh profile.
	Zsh,
	/// Explicit Fish profile.
	Fish,
}

/// Whether the nearest allowed direnv environment is loaded.
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
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DirenvMode {
	/// Load only an `.envrc` accepted by direnv's own allow list.
	#[default]
	Auto,
	/// Never run direnv preflight.
	Off,
}

/// Source-outline strategy used by shell-output minimization.
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
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SourceOutlineLevel {
	/// Preserve the standard source outline.
	#[default]
	Default,
	/// Prefer a smaller, more aggressive source outline.
	Aggressive,
}

/// One configurable shell-intent interception rule.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InterceptorRule {
	/// Regular expression applied to an admitted shell segment.
	pub pattern: Str,
	/// Live sibling tool which must exist before this rule is active.
	pub tool:    Str,
	/// Model-facing guidance returned instead of executing the command.
	pub message: Str,
}

/// Automatic backgrounding policy for long shell calls.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoBackgroundSettings {
	/// Whether eligible calls may detach after the foreground threshold.
	pub enabled:      bool,
	/// Foreground duration before detachment is attempted.
	pub threshold_ms: u64,
}

impl Default for AutoBackgroundSettings {
	fn default() -> Self {
		Self { enabled: true, threshold_ms: 60_000 }
	}
}

/// Shell-intent interception policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct InterceptorSettings {
	/// Whether matching commands return dedicated-tool guidance.
	pub enabled:  bool,
	/// Ordered rules evaluated against admitted command segments.
	pub patterns: Vec<InterceptorRule>,
}

impl Default for InterceptorSettings {
	fn default() -> Self {
		Self { enabled: false, patterns: default_interceptor_rules() }
	}
}

/// Shell-output minimization policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MinimizerSettings {
	/// Whether supported command output may be minimized.
	pub enabled:              bool,
	/// Optional native minimizer settings path.
	pub settings_path:        Option<Str>,
	/// If nonempty, only these command families are minimized.
	pub only:                 Vec<Str>,
	/// Command families excluded from minimization.
	pub except:               Vec<Str>,
	/// Maximum lossless raw capture retained before spill.
	pub max_capture_bytes:    u64,
	/// Source-file outline policy.
	pub source_outline_level: SourceOutlineLevel,
	/// Optional legacy-filter override retained as a migration target.
	pub legacy_filters:       Option<bool>,
}

impl Default for MinimizerSettings {
	fn default() -> Self {
		Self {
			enabled:              true,
			settings_path:        None,
			only:                 Vec::new(),
			except:               Vec::new(),
			max_capture_bytes:    4 * 1024 * 1024,
			source_outline_level: SourceOutlineLevel::Default,
			legacy_filters:       None,
		}
	}
}

/// Complete immutable settings projection consumed by shell construction.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellSettings {
	/// Whether the shell tool is registered.
	pub enabled:                bool,
	/// Default shell profile.
	pub profile:                ShellProfile,
	/// Optional explicit executable for the user-shell profile.
	pub executable:             Option<Str>,
	/// Arguments passed to the explicit shell executable.
	pub args:                   Vec<Str>,
	/// Whether the explicit shell requests login semantics.
	pub login:                  bool,
	/// Wrapper placed before each admitted command.
	pub command_prefix:         Option<Str>,
	/// Whether embedded shell builtins are advertised and enabled.
	pub embedded_builtins:      bool,
	/// Long-call detachment policy.
	pub auto_background:        AutoBackgroundSettings,
	/// Dedicated-tool interception policy.
	pub interceptor:            InterceptorSettings,
	/// direnv loading mode.
	pub direnv:                 DirenvMode,
	/// Maximum direnv preflight duration.
	pub direnv_load_timeout_ms: u64,
	/// Output minimization policy.
	pub minimizer:              MinimizerSettings,
}

impl Default for ShellSettings {
	fn default() -> Self {
		Self {
			enabled:                true,
			profile:                ShellProfile::Brush,
			executable:             None,
			args:                   Vec::new(),
			login:                  false,
			command_prefix:         None,
			embedded_builtins:      true,
			auto_background:        AutoBackgroundSettings::default(),
			interceptor:            InterceptorSettings::default(),
			direnv:                 DirenvMode::Auto,
			direnv_load_timeout_ms: 30_000,
			minimizer:              MinimizerSettings::default(),
		}
	}
}

impl SettingsDomain for ShellSettings {
	const DOMAIN: &'static str = "shell";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"shell.enabled",
			"Shell",
			"Enable environment-owned shell execution.",
			SettingKind::Boolean,
			10,
			None,
		),
		field(
			"shell.profile",
			"Shell Profile",
			"Profile requested for new persistent shell sessions.",
			SettingKind::Enum(PROFILE_VALUES),
			20,
			Some(OptionProvider::Static(PROFILE_OPTIONS)),
		),
		field(
			"shell.executable",
			"Shell Executable",
			"Explicit executable used by the user-shell profile.",
			SettingKind::Path,
			30,
			None,
		),
		field(
			"shell.args",
			"Shell Arguments",
			"Arguments passed to the explicit shell executable.",
			SettingKind::Array,
			40,
			None,
		),
		field(
			"shell.login",
			"Login Shell",
			"Request login semantics for the explicit user shell.",
			SettingKind::Boolean,
			50,
			None,
		),
		field(
			"shell.command_prefix",
			"Shell Command Prefix",
			"Wrapper placed before every admitted shell command.",
			SettingKind::String,
			60,
			None,
		),
		field(
			"shell.embedded_builtins",
			"Embedded Shell Builtins",
			"Advertise and enable the embedded builtin command set.",
			SettingKind::Boolean,
			70,
			None,
		),
		field(
			"shell.auto_background.enabled",
			"Shell Auto-Background",
			"Detach eligible long-running commands after the foreground threshold.",
			SettingKind::Boolean,
			80,
			None,
		),
		field(
			"shell.auto_background.threshold_ms",
			"Auto-Background Threshold",
			"Foreground milliseconds before eligible shell execution detaches.",
			SettingKind::Integer,
			90,
			None,
		),
		field(
			"shell.interceptor.enabled",
			"Shell Interceptor",
			"Return dedicated-tool guidance for configured command intents.",
			SettingKind::Boolean,
			100,
			None,
		),
		field(
			"shell.interceptor.patterns",
			"Shell Interceptor Rules",
			"Ordered regular-expression rules gated by live sibling tools.",
			SettingKind::Array,
			110,
			None,
		),
		field(
			"shell.direnv",
			"direnv Auto-Load",
			"Load the nearest allowed `.envrc` before shell execution.",
			SettingKind::Enum(DIRENV_VALUES),
			120,
			None,
		),
		field(
			"shell.direnv_load_timeout_ms",
			"direnv Load Timeout",
			"Maximum milliseconds allowed for direnv export.",
			SettingKind::Integer,
			130,
			None,
		),
		field(
			"shell.minimizer.enabled",
			"Shell Minimizer",
			"Minimize supported verbose command output while retaining raw truth.",
			SettingKind::Boolean,
			140,
			None,
		),
		field(
			"shell.minimizer.settings_path",
			"Minimizer Settings Path",
			"Optional native minimizer settings file.",
			SettingKind::Path,
			150,
			None,
		),
		field(
			"shell.minimizer.only",
			"Minimizer Include List",
			"Command families eligible for minimization.",
			SettingKind::Array,
			160,
			None,
		),
		field(
			"shell.minimizer.except",
			"Minimizer Exclude List",
			"Command families excluded from minimization.",
			SettingKind::Array,
			170,
			None,
		),
		field(
			"shell.minimizer.max_capture_bytes",
			"Minimizer Capture Ceiling",
			"Maximum raw bytes retained before artifact spill.",
			SettingKind::Integer,
			180,
			None,
		),
		field(
			"shell.minimizer.source_outline_level",
			"Source Outline Level",
			"Source outline compression policy.",
			SettingKind::Enum(OUTLINE_VALUES),
			190,
			None,
		),
		field(
			"shell.minimizer.legacy_filters",
			"Legacy Minimizer Filters",
			"Optional legacy-filter migration override.",
			SettingKind::Boolean,
			200,
			None,
		),
	];

	fn validate(&self) -> Result<(), ValidationError> {
		if self.direnv_load_timeout_ms == 0 || self.minimizer.max_capture_bytes == 0 {
			return Err(ValidationError::DomainInvariant { domain: Self::DOMAIN });
		}
		Ok(())
	}
}

const fn field(
	path: &'static str,
	label: &'static str,
	description: &'static str,
	kind: SettingKind,
	order: u16,
	options: Option<OptionProvider>,
) -> FieldDescriptor {
	FieldDescriptor {
		path,
		label,
		description,
		kind,
		scopes: PERSISTED,
		order,
		options,
		condition: None,
		secret: false,
	}
}

fn default_interceptor_rules() -> Vec<InterceptorRule> {
	[
		(r"^\s*(cat|head|tail|less|more)\s+", "read", "Use the read tool for bounded file access."),
		(
			r"^\s*(grep|rg|ripgrep|ag|ack)\s+",
			"grep",
			"Use the grep tool for repository-aware search.",
		),
		(
			r"^\s*(find|fd|locate)\s+.*(-name|-iname|-type|--type|-glob)",
			"glob",
			"Use the glob tool for repository-aware path discovery.",
		),
		(r"^\s*sed\s+(-i|--in-place)", "edit", "Use the edit tool for in-place changes."),
		(r"^\s*perl\s+.*-[pn]?i", "edit", "Use the edit tool for in-place changes."),
		(r"^\s*awk\s+.*-i\s+inplace", "edit", "Use the edit tool for in-place changes."),
		(
			r"^\s*(echo|printf|cat\s*<<).*>{1,2}\|?\s+[^&]",
			"write",
			"Use the write tool for file replacement.",
		),
		(r"(^\s*nohup\s+)|(&\s*$)", "hub", "Use hub start for supervised background processes."),
		(
			r"^\s*(vite|next\s+dev|nuxt\s+dev|nodemon|lldb|gdb|tail\s+-f)(\s|$)",
			"hub",
			"Use hub start for services, watchers, and debuggers.",
		),
	]
	.into_iter()
	.map(|(pattern, tool, message)| InterceptorRule {
		pattern: Str::new_static(pattern),
		tool:    Str::new_static(tool),
		message: Str::new_static(message),
	})
	.collect()
}

#[cfg(test)]
mod tests {
	use omp_settings::SettingsSnapshot;

	use super::*;

	#[test]
	fn shell_auto_background_is_enabled_by_default() {
		let settings = AutoBackgroundSettings::default();
		assert!(settings.enabled);
		assert_eq!(settings.threshold_ms, 60_000);
	}

	#[test]
	fn shell_projection_round_trips() {
		let expected = ShellSettings {
			profile: ShellProfile::Zsh,
			command_prefix: Some(Str::new_static("time")),
			..ShellSettings::default()
		};
		let snapshot = SettingsSnapshot::isolated(expected.clone(), crate::TEST_SETTINGS_CATALOG)
			.expect("isolated snapshot");
		assert_eq!(
			snapshot
				.project::<ShellSettings>()
				.expect("projection")
				.get(),
			&expected
		);
	}

	#[test]
	fn shell_projection_rejects_zero_runtime_ceilings() {
		let invalid = ShellSettings { direnv_load_timeout_ms: 0, ..ShellSettings::default() };
		assert!(invalid.validate().is_err());
	}
}
