//! Agent command sandbox posture and policy settings.

use std::path::Path;

use omp_core::Str;
use omp_settings::{
	Condition, DomainRegistration, FieldDescriptor, OptionProvider, SettingKind, SettingOption,
	SettingScope, SettingsDomain, ValidationError,
};
use serde::{Deserialize, Serialize};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const MODE_VALUES: &[&str] = &["off", "read-only", "workspace-write"];
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

/// User-facing sandbox configuration for agent command execution.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxSettings {
	/// Sandbox posture applied to agent command execution.
	pub mode:            ExecSandboxMode,
	/// Whether sandboxed commands may access the network.
	pub network:         bool,
	/// Additional absolute roots that workspace-write mode may modify.
	pub writable_roots:  Vec<Str>,
	/// Policy for writes outside configured roots in workspace-write mode.
	pub unscoped_writes: UnscopedWrites,
	/// Exported environment variable name globs withheld from external commands.
	pub env_deny:        Vec<Str>,
}

impl Default for SandboxSettings {
	fn default() -> Self {
		Self {
			mode:            ExecSandboxMode::Off,
			network:         false,
			writable_roots:  Vec::new(),
			unscoped_writes: UnscopedWrites::Deny,
			env_deny:        default_env_deny(),
		}
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
			"sandbox.network",
			"Sandbox Network Access",
			"Allow sandboxed commands to access the network.",
			SettingKind::Boolean,
			20,
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
	];

	fn validate(&self) -> Result<(), ValidationError> {
		if self
			.writable_roots
			.iter()
			.any(|root| !Path::new(root.as_str()).is_absolute())
			|| self.env_deny.iter().any(Str::is_empty)
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

omp_settings::inventory::submit! {
	DomainRegistration::of::<SandboxSettings>()
}

#[cfg(test)]
mod tests {
	use omp_settings::{SettingsSnapshot, registered_domains};

	use super::*;

	#[test]
	fn default_sandbox_round_trips_and_absent_table_projects_off() {
		let expected = SandboxSettings::default();
		let snapshot = SettingsSnapshot::isolated(expected.clone()).expect("isolated sandbox");
		assert_eq!(
			snapshot
				.project::<SandboxSettings>()
				.expect("round-trip sandbox projection")
				.get(),
			&expected
		);
		let absent = SettingsSnapshot::isolated_document(toml::Table::new());
		let settings = absent
			.project::<SandboxSettings>()
			.expect("absent sandbox projection");
		assert_eq!(settings.get(), &expected);
		assert_eq!(settings.get().mode, ExecSandboxMode::Off);
		assert!(
			registered_domains()
				.iter()
				.any(|domain| domain.name == SandboxSettings::DOMAIN)
		);
	}

	#[test]
	fn fully_configured_sandbox_table_projects() {
		let document = toml::from_str::<toml::Table>(
			r#"
[sandbox]
mode = "workspace-write"
network = true
writable_roots = ["/workspace", "/var/cache/omp"]
unscoped_writes = "overlay"
env_deny = ["*PASSWORD*", "CI_JOB_TOKEN"]
"#,
		)
		.expect("sandbox TOML");
		let snapshot = SettingsSnapshot::isolated_document(document);
		let settings = snapshot
			.project::<SandboxSettings>()
			.expect("configured sandbox projection");
		assert_eq!(settings.get(), &SandboxSettings {
			mode:            ExecSandboxMode::WorkspaceWrite,
			network:         true,
			writable_roots:  vec![Str::new_static("/workspace"), Str::new_static("/var/cache/omp"),],
			unscoped_writes: UnscopedWrites::Overlay,
			env_deny:        vec![Str::new_static("*PASSWORD*"), Str::new_static("CI_JOB_TOKEN"),],
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
}
