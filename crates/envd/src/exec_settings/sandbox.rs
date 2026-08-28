//! Agent command sandbox posture and policy settings.

use std::{collections::BTreeMap, path::Path};

use omp_core::Str;
use omp_settings::{
	Condition, FieldDescriptor, OptionProvider, SettingKind, SettingOption, SettingScope,
	SettingsDomain, ValidationError,
};
use serde::{Deserialize, Serialize};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const MODE_VALUES: &[&str] = &["off", "read-only", "workspace-write"];
const ENV_INHERIT_VALUES: &[&str] = &["all", "core", "none"];
const READ_MODE_VALUES: &[&str] = &["host", "minimal", "scoped"];
const NETWORK_MODE_VALUES: &[&str] = &["disabled", "open", "scoped"];
const ENV_INHERIT_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "all",
		label:       "All",
		description: Some("Start children with the complete inherited environment."),
	},
	SettingOption {
		value:       "core",
		label:       "Core",
		description: Some("Start children with only platform-core environment variables."),
	},
	SettingOption {
		value:       "none",
		label:       "None",
		description: Some("Start children with an empty environment."),
	},
];
const MODE_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "off",
		label:       "Off",
		description: Some("Do not sandbox agent commands."),
	},
	SettingOption {
		value:       "read-only",
		label:       "Read only",
		description: Some("Commands cannot write anywhere."),
	},
	SettingOption {
		value:       "workspace-write",
		label:       "Workspace write",
		description: Some(
			"Commands may write only to the workspace, /tmp, $TMPDIR, and extra roots.",
		),
	},
];
const UNSCOPED_WRITES_VALUES: &[&str] = &["deny", "overlay"];
const UNSCOPED_WRITES_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "deny",
		label:       "Deny",
		description: Some("Writes outside configured roots fail."),
	},
	SettingOption {
		value:       "overlay",
		label:       "Ephemeral overlay",
		description: Some(
			"Writes outside configured roots land in an ephemeral layer visible only inside the \
			 sandbox; hosts without overlay support deny them.",
		),
	},
];
const WORKSPACE_WRITE_ONLY: Option<Condition> =
	Some(Condition { field: "sandbox.mode", equals: "workspace-write" });

/// Exec sandbox posture selected by the user.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Serialize,
	Eq,
	PartialEq,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum ExecSandboxMode {
	/// Do not sandbox agent commands.
	#[default]
	Off,
	/// Prevent agent commands from writing anywhere.
	ReadOnly,
	/// Permit writes only to the workspace, temporary directories, and extra
	/// roots.
	WorkspaceWrite,
}

/// Handling of writes outside allowed roots under workspace-write.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Serialize,
	Eq,
	PartialEq,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum UnscopedWrites {
	/// Reject writes outside configured writable roots.
	#[default]
	Deny,
	/// Redirect unscoped writes to an ephemeral sandbox-private layer.
	Overlay,
}
/// Base environment inherited by child processes.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Serialize,
	Eq,
	PartialEq,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum EnvironmentInheritance {
	/// Inherit every exported environment variable.
	#[default]
	All,
	/// Inherit only platform-core environment variables.
	Core,
	/// Inherit no environment variables.
	None,
}
/// Read authority granted to sandboxed shell operations and children.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Serialize,
	Eq,
	PartialEq,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum ReadMode {
	/// Permit reads from the host subject to explicit denials.
	#[default]
	Host,
	/// Permit reads only from the workspace root.
	Minimal,
	/// Permit reads from the workspace and configured readable roots.
	Scoped,
}

/// Network authority granted to sandboxed commands.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Serialize,
	Eq,
	PartialEq,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum SandboxNetworkMode {
	/// Deny IP networking.
	#[default]
	Disabled,
	/// Permit normal IP egress.
	Open,
	/// Permit only policy-authorized egress through the sandbox broker.
	Scoped,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
/// User-facing sandbox configuration for agent command execution.
#[serde(default, deny_unknown_fields)]
pub struct SandboxSettings {
	/// Sandbox posture applied to agent command execution.
	pub mode:               ExecSandboxMode,
	/// Network authority granted to sandboxed commands.
	pub network_mode:       SandboxNetworkMode,
	/// Exact or leading `*.` wildcard domain names allowed in scoped mode.
	pub allow_domains:      Vec<Str>,
	/// Domain names denied before scoped allow rules.
	pub deny_domains:       Vec<Str>,
	/// TCP ports allowed in scoped mode.
	pub allow_ports:        Vec<u16>,
	/// Whether scoped mode may connect to loopback addresses.
	pub allow_localhost:    bool,
	/// Existing absolute Unix-domain socket paths allowed independently of IP
	/// networking.
	pub allow_unix_sockets: Vec<Str>,
	/// Additional absolute roots that workspace-write mode may modify.
	pub writable_roots:     Vec<Str>,
	/// Policy for writes outside configured roots in workspace-write mode.
	pub unscoped_writes:    UnscopedWrites,
	/// Exported environment variable name globs withheld from external commands.
	pub env_deny:           Vec<Str>,
	/// Base environment inherited by child processes.
	pub env_inherit:        EnvironmentInheritance,
	/// Environment variable name globs retained before deny filtering.
	pub env_include_only:   Vec<Str>,
	/// Explicit child environment values applied after filtering.
	pub env_set:            BTreeMap<Str, Str>,
	/// Whether workspace-write excludes the platform temporary directory.
	pub exclude_tmpdir:     bool,
	/// Whether workspace-write excludes `/tmp`.
	pub exclude_slash_tmp:  bool,
	/// Additional absolute paths hidden from sandboxed processes.
	pub read_deny:          Vec<Str>,
	/// Additional absolute roots available for reads in scoped mode.
	pub readable_roots:     Vec<Str>,
	/// Read authority posture for shell operations and sandboxed children.
	pub read_mode:          ReadMode,
	/// Denied path globs, rejected when the selected backend cannot enforce
	/// future matches.
	pub read_deny_globs:    Vec<Str>,
	/// Additional absolute paths protected from writes in both policy lanes.
	pub write_deny:         Vec<Str>,
}

impl Default for SandboxSettings {
	fn default() -> Self {
		Self {
			mode:               ExecSandboxMode::Off,
			network_mode:       SandboxNetworkMode::Disabled,
			allow_domains:      Vec::new(),
			deny_domains:       Vec::new(),
			allow_ports:        vec![80, 443],
			allow_localhost:    false,
			allow_unix_sockets: Vec::new(),
			writable_roots:     Vec::new(),
			unscoped_writes:    UnscopedWrites::Deny,
			env_deny:           default_env_deny(),
			env_inherit:        EnvironmentInheritance::All,
			env_include_only:   Vec::new(),
			env_set:            BTreeMap::new(),
			exclude_tmpdir:     false,
			exclude_slash_tmp:  false,
			read_deny:          Vec::new(),
			readable_roots:     Vec::new(),
			read_mode:          ReadMode::Host,
			read_deny_globs:    Vec::new(),
			write_deny:         Vec::new(),
		}
	}
}
impl SandboxSettings {
	/// Reports whether child environment behavior matches the default policy.
	pub(crate) fn environment_policy_is_default(&self) -> bool {
		self.env_inherit == EnvironmentInheritance::All
			&& self.env_include_only.is_empty()
			&& self.env_set.is_empty()
			&& self
				.env_deny
				.iter()
				.map(Str::as_str)
				.eq(["*KEY*", "*SECRET*", "*TOKEN*"])
	}
}

impl SettingsDomain for SandboxSettings {
	const DOMAIN: &'static str = "sandbox";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"sandbox.mode",
			"Command Sandbox",
			"Choose the filesystem sandbox posture for agent commands.",
			SettingKind::Enum(MODE_VALUES),
			10,
			Some(OptionProvider::Static(MODE_OPTIONS)),
			None,
		),
		field(
			"sandbox.network_mode",
			"Sandbox Network Access",
			"Choose disabled, open, or scoped network access.",
			SettingKind::Enum(NETWORK_MODE_VALUES),
			20,
			None,
			None,
		),
		field(
			"sandbox.allow_domains",
			"Allowed Domains",
			"Exact or leading wildcard domains allowed by scoped networking.",
			SettingKind::Array,
			21,
			None,
			None,
		),
		field(
			"sandbox.deny_domains",
			"Denied Domains",
			"Domains denied before scoped allow rules.",
			SettingKind::Array,
			22,
			None,
			None,
		),
		field(
			"sandbox.allow_ports",
			"Allowed Network Ports",
			"TCP ports allowed by scoped networking.",
			SettingKind::Array,
			23,
			None,
			None,
		),
		field(
			"sandbox.allow_localhost",
			"Allow Localhost",
			"Allow scoped networking to loopback addresses.",
			SettingKind::Boolean,
			24,
			None,
			None,
		),
		field(
			"sandbox.allow_unix_sockets",
			"Allowed Unix Sockets",
			"Existing absolute Unix-domain socket paths allowed independently of IP networking.",
			SettingKind::Array,
			25,
			None,
			None,
		),
		field(
			"sandbox.writable_roots",
			"Additional Writable Roots",
			"Absolute paths that workspace-write mode may modify.",
			SettingKind::Array,
			30,
			None,
			WORKSPACE_WRITE_ONLY,
		),
		field(
			"sandbox.unscoped_writes",
			"Unscoped Writes",
			"Choose how workspace-write handles writes outside configured roots.",
			SettingKind::Enum(UNSCOPED_WRITES_VALUES),
			40,
			Some(OptionProvider::Static(UNSCOPED_WRITES_OPTIONS)),
			WORKSPACE_WRITE_ONLY,
		),
		field(
			"sandbox.env_deny",
			"Denied Environment Variables",
			"Environment variable name globs withheld from external commands.",
			SettingKind::Array,
			50,
			None,
			None,
		),
		field(
			"sandbox.env_inherit",
			"Inherited Environment",
			"Choose the base environment inherited by child processes.",
			SettingKind::Enum(ENV_INHERIT_VALUES),
			60,
			Some(OptionProvider::Static(ENV_INHERIT_OPTIONS)),
			None,
		),
		field(
			"sandbox.env_include_only",
			"Included Environment Variables",
			"Environment variable name globs retained before deny filtering.",
			SettingKind::Array,
			70,
			None,
			None,
		),
		field(
			"sandbox.env_set",
			"Set Environment Variables",
			"Explicit child environment values applied after filtering.",
			SettingKind::Table,
			80,
			None,
			None,
		),
		field(
			"sandbox.exclude_tmpdir",
			"Exclude Platform Temporary Directory",
			"Do not grant workspace-write access to the platform temporary directory.",
			SettingKind::Boolean,
			90,
			None,
			WORKSPACE_WRITE_ONLY,
		),
		field(
			"sandbox.exclude_slash_tmp",
			"Exclude /tmp",
			"Do not grant workspace-write access to /tmp.",
			SettingKind::Boolean,
			100,
			None,
			WORKSPACE_WRITE_ONLY,
		),
		field(
			"sandbox.read_deny",
			"Denied Read Paths",
			"Additional absolute paths made unreadable by the kernel sandbox.",
			SettingKind::Array,
			110,
			None,
			None,
		),
		field(
			"sandbox.read_mode",
			"Read Authority",
			"Choose whether reads use host, workspace-only, or scoped roots.",
			SettingKind::Enum(READ_MODE_VALUES),
			115,
			None,
			None,
		),
		field(
			"sandbox.readable_roots",
			"Additional Readable Roots",
			"Absolute paths readable in scoped mode.",
			SettingKind::Array,
			116,
			None,
			None,
		),
		field(
			"sandbox.read_deny_globs",
			"Denied Read Path Globs",
			"Glob patterns denied when supported by the selected sandbox backend.",
			SettingKind::Array,
			117,
			None,
			None,
		),
		field(
			"sandbox.write_deny",
			"Denied Write Paths",
			"Additional absolute paths protected from writes.",
			SettingKind::Array,
			120,
			None,
			None,
		),
	];

	fn validate(&self) -> Result<(), ValidationError> {
		if self
			.writable_roots
			.iter()
			.chain(&self.readable_roots)
			.any(|root| !Path::new(root.as_str()).is_absolute())
			|| self
				.allow_unix_sockets
				.iter()
				.any(|path| !is_existing_unix_socket(Path::new(path.as_str())))
			|| self.allow_ports.contains(&0)
			|| self
				.allow_domains
				.iter()
				.chain(&self.deny_domains)
				.any(|domain| !valid_domain_pattern(domain.as_str()))
			|| self
				.read_deny
				.iter()
				.chain(&self.write_deny)
				.any(|path| !Path::new(path.as_str()).is_absolute())
			|| self
				.env_deny
				.iter()
				.chain(&self.env_include_only)
				.any(|pattern| omp_sandbox::validate_env_pattern(pattern.as_str()).is_err())
			|| self
				.read_deny_globs
				.iter()
				.any(|pattern| globset::Glob::new(pattern.as_str()).is_err())
		{
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
	condition: Option<Condition>,
) -> FieldDescriptor {
	FieldDescriptor {
		path,
		label,
		description,
		kind,
		scopes: PERSISTED,
		order,
		options,
		condition,
		secret: false,
	}
}

fn default_env_deny() -> Vec<Str> {
	["*KEY*", "*SECRET*", "*TOKEN*"]
		.into_iter()
		.map(Str::new_static)
		.collect()
}
fn valid_domain_pattern(pattern: &str) -> bool {
	let domain = pattern.strip_prefix("*.").unwrap_or(pattern);
	!domain.is_empty()
		&& domain.split('.').all(|label| {
			!label.is_empty()
				&& label.len() <= 63
				&& label
					.bytes()
					.all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
		})
}
fn is_existing_unix_socket(path: &Path) -> bool {
	if !path.is_absolute() {
		return false;
	}
	#[cfg(unix)]
	{
		use std::os::unix::fs::FileTypeExt as _;
		std::fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
	}
	#[cfg(not(unix))]
	{
		path.exists()
	}
}

#[cfg(test)]
mod tests {
	use omp_settings::SettingsSnapshot;

	use super::*;

	#[test]
	fn default_sandbox_round_trips_and_absent_table_projects_off() {
		let expected = SandboxSettings::default();
		let snapshot = SettingsSnapshot::isolated(expected.clone(), crate::TEST_SETTINGS_CATALOG)
			.expect("isolated sandbox");
		assert_eq!(
			snapshot
				.project::<SandboxSettings>()
				.expect("round-trip sandbox projection")
				.get(),
			&expected
		);
		let absent =
			SettingsSnapshot::isolated_document(toml::Table::new(), crate::TEST_SETTINGS_CATALOG);
		let settings = absent
			.project::<SandboxSettings>()
			.expect("absent sandbox projection");
		assert_eq!(settings.get(), &expected);
		assert_eq!(settings.get().mode, ExecSandboxMode::Off);
	}

	#[test]
	fn fully_configured_sandbox_table_projects() {
		let document = toml::from_str::<toml::Table>(
			r#"
[sandbox]
mode = "workspace-write"
network_mode = "scoped"
allow_domains = ["*.example.com", "api.omp.dev"]
deny_domains = ["telemetry.example.com"]
allow_ports = [443, 8443]
allow_localhost = true
writable_roots = ["/workspace", "/var/cache/omp"]
unscoped_writes = "overlay"
env_deny = ["*PASSWORD*", "CI_JOB_TOKEN"]
env_inherit = "core"
env_include_only = ["OMP_*", "PATH"]
env_set = { OMP_TEST = "yes" }
exclude_tmpdir = true
exclude_slash_tmp = true
read_deny = ["/private"]
read_mode = "scoped"
readable_roots = ["/reference"]
read_deny_globs = ["/private/**"]
write_deny = ["/protected"]
"#,
		)
		.expect("sandbox TOML");
		let snapshot = SettingsSnapshot::isolated_document(document, crate::TEST_SETTINGS_CATALOG);
		let settings = snapshot
			.project::<SandboxSettings>()
			.expect("configured sandbox projection");
		assert_eq!(settings.get(), &SandboxSettings {
			mode:               ExecSandboxMode::WorkspaceWrite,
			network_mode:       SandboxNetworkMode::Scoped,
			allow_domains:      vec![Str::new_static("*.example.com"), Str::new_static("api.omp.dev")],
			deny_domains:       vec![Str::new_static("telemetry.example.com")],
			allow_ports:        vec![443, 8443],
			allow_localhost:    true,
			allow_unix_sockets: Vec::new(),
			writable_roots:     vec![Str::new_static("/workspace"), Str::new_static("/var/cache/omp"),],
			unscoped_writes:    UnscopedWrites::Overlay,
			env_deny:           vec![Str::new_static("*PASSWORD*"), Str::new_static("CI_JOB_TOKEN"),],
			env_inherit:        EnvironmentInheritance::Core,
			env_include_only:   vec![Str::new_static("OMP_*"), Str::new_static("PATH")],
			env_set:            BTreeMap::from([(
				Str::new_static("OMP_TEST"),
				Str::new_static("yes")
			)]),
			exclude_tmpdir:     true,
			exclude_slash_tmp:  true,
			read_deny:          vec![Str::new_static("/private")],
			read_mode:          ReadMode::Scoped,
			readable_roots:     vec![Str::new_static("/reference")],
			read_deny_globs:    vec![Str::new_static("/private/**")],
			write_deny:         vec![Str::new_static("/protected")],
		});
	}

	#[test]
	fn sandbox_validation_rejects_relative_writable_root() {
		let settings = SandboxSettings {
			writable_roots: vec![Str::new_static("relative/path")],
			..SandboxSettings::default()
		};
		assert!(settings.validate().is_err());
	}
	#[test]
	fn sandbox_validation_rejects_relative_policy_paths_and_invalid_environment_globs() {
		let relative = SandboxSettings {
			read_deny: vec![Str::new_static("private")],
			..SandboxSettings::default()
		};
		assert!(relative.validate().is_err());
		let invalid_glob = SandboxSettings {
			env_include_only: vec![Str::new_static("[")],
			..SandboxSettings::default()
		};
		assert!(invalid_glob.validate().is_err());
	}
}
