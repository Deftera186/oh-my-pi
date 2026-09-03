//! Native shell profile, interception, direnv, and minimizer settings.

use omp_con::{Ctx, Kv, Value};
use omp_core::Str;
use serde::{Deserialize, Serialize};

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
	strum::VariantNames,
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

omp_con::con_enum!(ShellProfile);

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
	strum::VariantNames,
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

omp_con::con_enum!(DirenvMode);

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
	strum::VariantNames,
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

omp_con::con_enum!(SourceOutlineLevel);

/// Optional legacy-filter migration override.
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
#[strum(serialize_all = "lowercase")]
pub enum LegacyFilterMode {
	/// No explicit override.
	#[default]
	Default,
	/// Enable the legacy filters.
	True,
	/// Disable the legacy filters.
	False,
}

omp_con::con_enum!(LegacyFilterMode);

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

omp_con::var! {
	/// Enable environment-owned shell execution.
	pub static SV_SHELL_ENABLED = sv_shell_enabled: bool { default: true, flags: archive | inherit };
	/// Profile requested for new persistent shell sessions.
	pub static SV_SHELL_PROFILE = sv_shell_profile: ShellProfile {
		default: ShellProfile::Brush,
		flags: archive | inherit,
	};
	/// Explicit executable used by the user-shell profile; empty selects the profile default.
	pub static SV_SHELL_EXECUTABLE = sv_shell_executable: Str {
		default: Str::default(),
		flags: archive | inherit,
	};
	/// Arguments passed to the explicit shell executable.
	pub static SV_SHELL_ARGS = sv_shell_args: Vec<Str> {
		default: Vec::new(),
		flags: archive | inherit,
	};
	/// Request login semantics for the explicit user shell.
	pub static SV_SHELL_LOGIN = sv_shell_login: bool { default: false, flags: archive | inherit };
	/// Wrapper placed before every admitted shell command; empty disables the wrapper.
	pub static SV_SHELL_COMMAND_PREFIX = sv_shell_command_prefix: Str {
		default: Str::default(),
		flags: archive | inherit,
	};
	/// Advertise and enable the embedded builtin command set.
	pub static SV_SHELL_EMBEDDED_BUILTINS = sv_shell_embedded_builtins: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Detach eligible long-running commands after the foreground threshold.
	pub static SV_SHELL_AUTO_BACKGROUND_ENABLED = sv_shell_auto_background_enabled: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Foreground milliseconds before eligible shell execution detaches.
	pub static SV_SHELL_AUTO_BACKGROUND_THRESHOLD_MS = sv_shell_auto_background_threshold_ms: i64 {
		default: 60_000,
		min: 0,
		flags: archive | inherit,
	};
	/// Return dedicated-tool guidance for configured command intents.
	pub static SV_SHELL_INTERCEPTOR_ENABLED = sv_shell_interceptor_enabled: bool {
		default: false,
		flags: archive | inherit,
	};
	/// Ordered regular-expression rules gated by live sibling tools.
	pub static SV_SHELL_INTERCEPTOR_PATTERNS = sv_shell_interceptor_patterns: Vec<Kv> {
		default: default_interceptor_kv(),
		flags: archive | inherit,
	};
	/// Load the nearest allowed `.envrc` before shell execution.
	pub static SV_SHELL_DIRENV = sv_shell_direnv: DirenvMode {
		default: DirenvMode::Auto,
		flags: archive | inherit,
	};
	/// Maximum milliseconds allowed for direnv export.
	pub static SV_SHELL_DIRENV_LOAD_TIMEOUT_MS = sv_shell_direnv_load_timeout_ms: i64 {
		default: 30_000,
		min: 1,
		flags: archive | inherit,
	};
	/// Minimize supported verbose command output while retaining raw truth.
	pub static SV_SHELL_MINIMIZER_ENABLED = sv_shell_minimizer_enabled: bool {
		default: true,
		flags: archive | inherit,
	};
	/// Optional native minimizer settings file; empty selects the built-in policy.
	pub static SV_SHELL_MINIMIZER_SETTINGS_PATH = sv_shell_minimizer_settings_path: Str {
		default: Str::default(),
		flags: archive | inherit,
	};
	/// Command families eligible for minimization.
	pub static SV_SHELL_MINIMIZER_ONLY = sv_shell_minimizer_only: Vec<Str> {
		default: Vec::new(),
		flags: archive | inherit,
	};
	/// Command families excluded from minimization.
	pub static SV_SHELL_MINIMIZER_EXCEPT = sv_shell_minimizer_except: Vec<Str> {
		default: Vec::new(),
		flags: archive | inherit,
	};
	/// Maximum raw bytes retained before artifact spill.
	pub static SV_SHELL_MINIMIZER_MAX_CAPTURE_BYTES = sv_shell_minimizer_max_capture_bytes: i64 {
		default: 4 * 1024 * 1024,
		min: 1,
		flags: archive | inherit,
	};
	/// Source outline compression policy.
	pub static SV_SHELL_MINIMIZER_SOURCE_OUTLINE_LEVEL =
		sv_shell_minimizer_source_outline_level: SourceOutlineLevel {
			default: SourceOutlineLevel::Default,
			flags: archive | inherit,
		};
	/// Optional legacy-filter migration override.
	pub static SV_SHELL_MINIMIZER_LEGACY_FILTERS =
		sv_shell_minimizer_legacy_filters: LegacyFilterMode {
			default: LegacyFilterMode::Default,
			flags: archive | inherit,
		};
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

impl ShellSettings {
	/// Resolves shell construction policy from the process control context.
	#[must_use]
	pub fn from_con(ctx: &Ctx) -> Self {
		Self {
			enabled:                SV_SHELL_ENABLED.get(ctx),
			profile:                SV_SHELL_PROFILE.get(ctx),
			executable:             nonempty(SV_SHELL_EXECUTABLE.get(ctx)),
			args:                   SV_SHELL_ARGS.get(ctx),
			login:                  SV_SHELL_LOGIN.get(ctx),
			command_prefix:         nonempty(SV_SHELL_COMMAND_PREFIX.get(ctx)),
			embedded_builtins:      SV_SHELL_EMBEDDED_BUILTINS.get(ctx),
			auto_background:        AutoBackgroundSettings {
				enabled:      SV_SHELL_AUTO_BACKGROUND_ENABLED.get(ctx),
				threshold_ms: SV_SHELL_AUTO_BACKGROUND_THRESHOLD_MS.get(ctx) as u64,
			},
			interceptor:            InterceptorSettings {
				enabled:  SV_SHELL_INTERCEPTOR_ENABLED.get(ctx),
				patterns: SV_SHELL_INTERCEPTOR_PATTERNS
					.get(ctx)
					.into_iter()
					.filter_map(interceptor_rule)
					.collect(),
			},
			direnv:                 SV_SHELL_DIRENV.get(ctx),
			direnv_load_timeout_ms: SV_SHELL_DIRENV_LOAD_TIMEOUT_MS.get(ctx) as u64,
			minimizer:              MinimizerSettings {
				enabled:              SV_SHELL_MINIMIZER_ENABLED.get(ctx),
				settings_path:        nonempty(SV_SHELL_MINIMIZER_SETTINGS_PATH.get(ctx)),
				only:                 SV_SHELL_MINIMIZER_ONLY.get(ctx),
				except:               SV_SHELL_MINIMIZER_EXCEPT.get(ctx),
				max_capture_bytes:    SV_SHELL_MINIMIZER_MAX_CAPTURE_BYTES.get(ctx) as u64,
				source_outline_level: SV_SHELL_MINIMIZER_SOURCE_OUTLINE_LEVEL.get(ctx),
				legacy_filters:       match SV_SHELL_MINIMIZER_LEGACY_FILTERS.get(ctx) {
					LegacyFilterMode::Default => None,
					LegacyFilterMode::True => Some(true),
					LegacyFilterMode::False => Some(false),
				},
			},
		}
	}
}

fn nonempty(value: Str) -> Option<Str> {
	(!value.is_empty()).then_some(value)
}

fn interceptor_rule(value: Kv) -> Option<InterceptorRule> {
	Some(InterceptorRule {
		pattern: value.get("pattern")?.as_str()?.into(),
		tool:    value.get("tool")?.as_str()?.into(),
		message: value.get("message")?.as_str()?.into(),
	})
}

fn default_interceptor_kv() -> Vec<Kv> {
	default_interceptor_rules()
		.into_iter()
		.map(|rule| {
			Kv(vec![
				(Str::new_static("pattern"), Value::Str(rule.pattern)),
				(Str::new_static("tool"), Value::Str(rule.tool)),
				(Str::new_static("message"), Value::Str(rule.message)),
			])
		})
		.collect()
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
	use super::*;

	#[test]
	fn shell_auto_background_is_enabled_by_default() {
		let settings = ShellSettings::from_con(&Ctx::new());
		assert!(settings.auto_background.enabled);
		assert_eq!(settings.auto_background.threshold_ms, 60_000);
	}

	#[test]
	fn shell_con_projection_round_trips() {
		let ctx = Ctx::new();
		SV_SHELL_PROFILE
			.set(&ctx, ShellProfile::Zsh)
			.expect("set profile");
		SV_SHELL_COMMAND_PREFIX
			.set(&ctx, Str::new_static("time"))
			.expect("set prefix");
		assert_eq!(ShellSettings::from_con(&ctx), ShellSettings {
			profile: ShellProfile::Zsh,
			command_prefix: Some(Str::new_static("time")),
			..ShellSettings::default()
		});
	}
}
