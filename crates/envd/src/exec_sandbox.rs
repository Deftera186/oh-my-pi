//! Sandboxing policy compiled once and shared by one execution host.

use std::{
	ffi::{OsStr, OsString},
	fs, io,
	path::{Component, Path, PathBuf},
	sync::Arc,
};

use omp_core::{Str, StrMut};
use omp_sandbox::{
	CommandWrapper, DegradationPolicy, NetworkMode, Runner, SandboxError, SandboxSpec, WriteMode,
};
use omp_shell_engine::{PathPolicy, SpawnWrapper, WriteDenied};

use crate::exec_settings::{ExecSandboxMode, SandboxSettings, UnscopedWrites};

const CARVE_OUTS: [&str; 3] = [".git", ".omp", ".agents"];

/// Precompiled kernel launcher and matching in-process write policy.
pub(crate) struct ExecSandbox {
	wrapper:      Arc<CommandWrapper>,
	write_policy: WritePolicy,
	failure_note: Str,
}

#[derive(Clone)]
struct WritePolicy {
	writable: Arc<[PathBuf]>,
	denied:   Arc<[PathBuf]>,
}

struct PolicyParts {
	spec:          SandboxSpec,
	write_policy:  WritePolicy,
	roots_label:   Str,
	#[cfg(test)]
	spec_snapshot: Str,
}

impl ExecSandbox {
	/// Compiles one reusable native command wrapper for `settings`.
	pub(crate) fn compile(
		settings: &SandboxSettings,
		workspace_root: &Path,
	) -> Result<Option<Arc<Self>>, SandboxError> {
		if settings.mode == ExecSandboxMode::Off {
			return Ok(None);
		}
		let runner = Runner::native_command()?;
		let requested_write = if settings.mode == ExecSandboxMode::WorkspaceWrite
			&& settings.unscoped_writes == UnscopedWrites::Overlay
		{
			WriteMode::Overlay
		} else if settings.mode == ExecSandboxMode::WorkspaceWrite {
			WriteMode::Scoped
		} else {
			WriteMode::Deny
		};
		let parts = policy_parts(settings, workspace_root, requested_write)?;
		let (wrapper, parts, degraded) = match runner.wrap_template(&parts.spec) {
			Ok(wrapper) => (wrapper, parts, false),
			Err(source) if requested_write == WriteMode::Overlay && capability_failure(&source) => {
				let scoped = policy_parts(settings, workspace_root, WriteMode::Scoped)?;
				let wrapper = runner.wrap_template(&scoped.spec)?;
				(wrapper, scoped, true)
			},
			Err(source) => return Err(source),
		};
		let mut note = StrMut::new("");
		note.push_str("sandbox: mode=");
		note.push_str(<&'static str>::from(settings.mode));
		note.push_str("; writes outside ");
		note.push_str(parts.roots_label.as_str());
		note.push_str(" are denied");
		if degraded {
			note.push_str("; overlay unavailable, using scoped writes");
		}
		Ok(Some(Arc::new(Self {
			wrapper:      Arc::new(wrapper),
			write_policy: parts.write_policy,
			failure_note: note.freeze(),
		})))
	}

	/// Returns the one-line diagnostic appended to failed shell results.
	pub(crate) fn failure_note(&self) -> &Str {
		&self.failure_note
	}

	/// Creates a launcher command followed by the real program and arguments.
	pub(crate) fn command(&self, program: &OsStr, args: &[&OsStr]) -> std::process::Command {
		let mut command = std::process::Command::new(self.wrapper.launcher());
		command
			.args(self.wrapper.prefix_args())
			.arg(program)
			.args(args);
		command
	}

	/// Creates an asynchronous launcher command prefixed with the real program.
	pub(crate) fn tokio_command(&self, program: &OsStr) -> tokio::process::Command {
		let mut command = tokio::process::Command::new(self.wrapper.launcher());
		command.args(self.wrapper.prefix_args()).arg(program);
		command
	}

	/// Reports whether an environment name may reach a child.
	pub(crate) fn env_allowed_os(&self, key: &OsStr) -> bool {
		key.to_str()
			.is_some_and(|key| self.wrapper.env_allowed(key))
	}
}

impl PathPolicy for ExecSandbox {
	fn check_write(&self, path: &Path) -> Result<(), WriteDenied> {
		self.write_policy.check_write(path)
	}
}

impl SpawnWrapper for ExecSandbox {
	fn launcher(&self) -> Option<(&OsStr, &[OsString])> {
		Some((self.wrapper.launcher(), self.wrapper.prefix_args()))
	}

	fn env_allowed(&self, key: &str) -> bool {
		self.wrapper.env_allowed(key)
	}
}

impl WritePolicy {
	fn check_write(&self, path: &Path) -> Result<(), WriteDenied> {
		let resolved = resolve_write_path(path).unwrap_or_else(|_| path.to_path_buf());
		let allowed = self.writable.iter().any(|root| resolved.starts_with(root));
		let denied = self.denied.iter().any(|root| resolved.starts_with(root));
		if allowed && !denied {
			Ok(())
		} else {
			Err(WriteDenied { path: resolved })
		}
	}
}

fn policy_parts(
	settings: &SandboxSettings,
	workspace_root: &Path,
	write: WriteMode,
) -> Result<PolicyParts, SandboxError> {
	let mut spec = SandboxSpec::new(OsString::new());
	spec
		.set_write(write)
		.set_network(if settings.network {
			NetworkMode::Outbound
		} else {
			NetworkMode::Disabled
		})
		.set_degradation(DegradationPolicy::Reject);
	for pattern in &settings.env_deny {
		spec.deny_env(pattern.as_str())?;
	}

	let mut writable = Vec::new();
	let mut denied = Vec::new();
	let roots_label = if settings.mode == ExecSandboxMode::ReadOnly {
		Str::new_static("no roots")
	} else {
		let mut configured = Vec::with_capacity(1 + settings.writable_roots.len());
		configured.push(workspace_root.to_path_buf());
		configured.extend(
			settings
				.writable_roots
				.iter()
				.map(|root| PathBuf::from(root.as_str())),
		);
		for root in configured {
			spec.allow_write(&root)?;
			let root = resolve_write_path(&root)
				.map_err(|source| SandboxError::Canonicalize { path: root.clone(), source })?;
			if !writable.contains(&root) {
				writable.push(root.clone());
			}
			for name in CARVE_OUTS {
				let carve_out = root.join(name);
				spec.deny_write(&carve_out)?;
				let carve_out = resolve_write_path(&carve_out)
					.map_err(|source| SandboxError::Canonicalize { path: carve_out.clone(), source })?;
				if !denied.contains(&carve_out) {
					denied.push(carve_out);
				}
			}
		}
		spec.set_allow_temp(true);
		for temp in [std::env::temp_dir(), PathBuf::from("/tmp")] {
			if let Ok(temp) = resolve_write_path(&temp)
				&& !writable.contains(&temp)
			{
				writable.push(temp);
			}
		}
		let mut label = StrMut::new("");
		for (index, root) in writable.iter().enumerate() {
			if index != 0 {
				label.push_str(", ");
			}
			label.push_str(root.to_string_lossy().as_ref());
		}
		label.freeze()
	};
	Ok(PolicyParts {
		spec,
		write_policy: WritePolicy { writable: writable.into(), denied: denied.into() },
		roots_label,
		#[cfg(test)]
		spec_snapshot: {
			let mut snapshot = StrMut::new("network=");
			snapshot.push_str(<&'static str>::from(if settings.network {
				NetworkMode::Outbound
			} else {
				NetworkMode::Disabled
			}));
			snapshot.push_str(";write=");
			snapshot.push_str(<&'static str>::from(write));
			snapshot.push_str(";allow_temp=");
			snapshot.push_str(if settings.mode == ExecSandboxMode::WorkspaceWrite {
				"true"
			} else {
				"false"
			});
			snapshot.push_str(";env_deny=");
			for pattern in &settings.env_deny {
				snapshot.push_str(pattern.as_str());
				snapshot.push_str(",");
			}
			snapshot.freeze()
		},
	})
}

fn capability_failure(error: &SandboxError) -> bool {
	matches!(
		error,
		SandboxError::BackendCapabilities { .. } | SandboxError::NoBackendCapabilities { .. }
	)
}

fn resolve_write_path(path: &Path) -> io::Result<PathBuf> {
	if !path.is_absolute() {
		return Err(io::Error::new(io::ErrorKind::InvalidInput, "write path is not absolute"));
	}
	let normalized = normalize_absolute(path)?;
	let mut ancestor = normalized.as_path();
	let mut suffix = Vec::new();
	loop {
		match fs::canonicalize(ancestor) {
			Ok(mut canonical) => {
				for component in suffix.iter().rev() {
					canonical.push(component);
				}
				return normalize_absolute(&canonical);
			},
			Err(error) if error.kind() == io::ErrorKind::NotFound => {
				let Some(name) = ancestor.file_name() else {
					return Err(error);
				};
				suffix.push(name.to_os_string());
				let Some(parent) = ancestor.parent() else {
					return Err(error);
				};
				ancestor = parent;
			},
			Err(error) => return Err(error),
		}
	}
}

fn normalize_absolute(path: &Path) -> io::Result<PathBuf> {
	let mut normalized = PathBuf::new();
	for component in path.components() {
		match component {
			Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
			Component::RootDir => normalized.push(component.as_os_str()),
			Component::CurDir => {},
			Component::ParentDir => {
				if !normalized.pop() {
					return Err(io::Error::new(io::ErrorKind::InvalidInput, "write path escapes root"));
				}
			},
			Component::Normal(name) => normalized.push(name),
		}
	}
	Ok(normalized)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn workspace_settings() -> SandboxSettings {
		SandboxSettings { mode: ExecSandboxMode::WorkspaceWrite, ..SandboxSettings::default() }
	}

	#[test]
	fn default_workspace_policy_has_roots_carve_outs_network_and_env_scrubbing() {
		let workspace = tempfile::tempdir().expect("workspace");
		for name in CARVE_OUTS {
			fs::create_dir(workspace.path().join(name)).expect("carve-out");
		}
		let settings = workspace_settings();
		let parts = policy_parts(&settings, workspace.path(), WriteMode::Scoped).expect("policy");
		let root = fs::canonicalize(workspace.path()).expect("canonical workspace");
		assert!(parts.write_policy.writable.contains(&root));
		let expected = CARVE_OUTS.map(|name| root.join(name));
		assert_eq!(parts.write_policy.denied.as_ref(), expected.as_slice());
		assert!(
			parts
				.write_policy
				.check_write(&root.join("src/new.rs"))
				.is_ok()
		);
		assert!(
			parts
				.write_policy
				.check_write(&root.join(".git/config"))
				.is_err()
		);
		assert!(
			parts
				.write_policy
				.check_write(&std::env::temp_dir().join("omp-sandbox-test"))
				.is_ok()
		);
		assert!(parts.spec_snapshot.contains("network=disable"));
		assert!(parts.spec_snapshot.contains("write=scope"));
		for pattern in ["*KEY*", "*SECRET*", "*TOKEN*"] {
			assert!(parts.spec_snapshot.contains(pattern));
		}
	}

	#[test]
	fn read_only_policy_denies_every_write() {
		let workspace = tempfile::tempdir().expect("workspace");
		let settings = SandboxSettings { mode: ExecSandboxMode::ReadOnly, ..Default::default() };
		let policy = policy_parts(&settings, workspace.path(), WriteMode::Deny)
			.expect("policy")
			.write_policy;
		assert!(policy.check_write(&workspace.path().join("file")).is_err());
		assert!(
			policy
				.check_write(&std::env::temp_dir().join("file"))
				.is_err()
		);
	}

	#[test]
	fn parent_escape_is_resolved_before_root_matching() {
		let sandbox = tempfile::tempdir().expect("sandbox");
		let workspace = sandbox.path().join("workspace");
		fs::create_dir(&workspace).expect("workspace root");
		let settings = workspace_settings();
		let policy = policy_parts(&settings, &workspace, WriteMode::Scoped)
			.expect("policy")
			.write_policy;
		let escaped = workspace.join("missing/../../outside.txt");
		assert!(policy.check_write(&escaped).is_err());
	}
	#[cfg(unix)]
	#[test]
	fn symlink_escape_is_resolved_before_root_matching() {
		use std::os::unix::fs::symlink;

		let sandbox = tempfile::tempdir().expect("sandbox");
		let workspace = sandbox.path().join("workspace");
		let outside = sandbox.path().join("outside");
		fs::create_dir(&workspace).expect("workspace root");
		fs::create_dir(&outside).expect("outside root");
		symlink(&outside, workspace.join("link")).expect("escape symlink");
		let policy = policy_parts(&workspace_settings(), &workspace, WriteMode::Scoped)
			.expect("policy")
			.write_policy;
		assert!(policy.check_write(&workspace.join("link/new.txt")).is_err());
	}
}
