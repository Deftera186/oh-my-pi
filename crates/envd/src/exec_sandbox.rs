//! Sandboxing policy compiled once and shared by one execution host.

use std::{
	collections::VecDeque,
	ffi::{OsStr, OsString},
	fs, io,
	path::{Component, Path, PathBuf},
	sync::Arc,
};

use omp_core::{Str, StrMut};
use omp_sandbox::{
	Capability, CommandWrapper, DegradationPolicy, EnvironmentSource, NetworkMode, Runner,
	SandboxError, SandboxSpec, WriteMode,
};
use omp_shell_engine::{PathPolicy, SpawnWrapper, WriteDenied};

use crate::exec_settings::{
	EnvironmentInheritance, ExecSandboxMode, SandboxSettings, UnscopedWrites,
};

const CARVE_OUTS: [&str; 3] = [".git", ".omp", ".agents"];

/// Precompiled kernel launcher and matching in-process write policy.
pub(crate) struct ExecSandbox {
	wrapper:       Arc<CommandWrapper>,
	write_policy:  WritePolicy,
	failure_note:  Str,
	kernel_active: bool,
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
		supervised: bool,
	) -> Result<Option<Arc<Self>>, SandboxError> {
		if settings.mode == ExecSandboxMode::Off {
			return if settings.environment_policy_is_default() {
				Ok(None)
			} else {
				let mut parts = policy_parts(settings, workspace_root, WriteMode::Deny)?;
				parts.spec.set_supervised(supervised);
				let wrapper = CommandWrapper::environment_only(&parts.spec);
				Ok(Some(Arc::new(Self {
					wrapper:       Arc::new(wrapper),
					write_policy:  parts.write_policy,
					failure_note:  Str::new_static(""),
					kernel_active: false,
				})))
			};
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
		let mut parts = parts;
		parts.spec.set_supervised(supervised);
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
		note.push_str("; network=");
		note.push_str(if settings.network { "outbound" } else { "off" });
		if degraded {
			note.push_str("; overlay unavailable, using scoped writes");
		}
		for caveat in wrapper.caveats() {
			note.push_str("; ");
			note.push_str(caveat.message.as_str());
		}
		Ok(Some(Arc::new(Self {
			wrapper:       Arc::new(wrapper),
			write_policy:  parts.write_policy,
			failure_note:  note.freeze(),
			kernel_active: true,
		})))
	}

	/// Returns the one-line diagnostic appended to failed shell results.
	pub(crate) fn failure_note(&self) -> &Str {
		&self.failure_note
	}

	/// Reports whether this policy has a kernel sandbox launcher.
	pub(crate) const fn kernel_active(&self) -> bool {
		self.kernel_active
	}

	/// Creates a launcher command followed by the real program and arguments.
	pub(crate) fn command(&self, program: &OsStr, args: &[&OsStr]) -> std::process::Command {
		let mut command = std::process::Command::new(self.wrapper.launcher().unwrap_or(program));
		if self.wrapper.launcher().is_some() {
			command.args(self.wrapper.prefix_args()).arg(program);
		}
		command.args(args);
		command
	}

	/// Creates an asynchronous launcher command prefixed with the real program.
	pub(crate) fn tokio_command(&self, program: &OsStr) -> tokio::process::Command {
		let mut command = tokio::process::Command::new(self.wrapper.launcher().unwrap_or(program));
		if self.wrapper.launcher().is_some() {
			command.args(self.wrapper.prefix_args()).arg(program);
		}
		command
	}

	/// Applies the compiled child environment policy.
	pub(crate) fn resolve_env<I>(&self, environment: I) -> Vec<(OsString, OsString)>
	where
		I: IntoIterator<Item = (OsString, OsString)>,
	{
		self.wrapper.resolve_env(environment)
	}
}

impl PathPolicy for ExecSandbox {
	fn check_write(&self, path: &Path) -> Result<(), WriteDenied> {
		self.write_policy.check_write(path)
	}
}

impl SpawnWrapper for ExecSandbox {
	fn launcher(&self) -> Option<(&OsStr, &[OsString])> {
		self
			.wrapper
			.launcher()
			.map(|launcher| (launcher, self.wrapper.prefix_args()))
	}

	fn env_allowed(&self, key: &str) -> bool {
		self.wrapper.env_allowed(key)
	}

	fn resolve_env(&self, environment: &mut Vec<(OsString, OsString)>) {
		*environment = self.wrapper.resolve_env(environment.drain(..));
	}
}

impl WritePolicy {
	fn check_write(&self, path: &Path) -> Result<(), WriteDenied> {
		let resolved = resolve_write_path(path).map_err(|_| WriteDenied {
			path: std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
		})?;
		if resolved == Path::new("/dev/null") {
			return Ok(());
		}
		let allowed = self.writable.iter().any(|root| resolved.starts_with(root));
		let denied = self
			.denied
			.iter()
			.any(|root| resolved.starts_with(root) || root.starts_with(&resolved));
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
	// Seatbelt's deny-default profile still permits the baseline POSIX IPC and
	// DNS Unix sockets required by ordinary commands, so it cannot claim full
	// `ipc.restrict`. Everything else missing keeps rejecting compilation.
	spec.tolerate_missing(Capability::IpcRestrict);
	#[cfg(target_os = "linux")]
	if settings.network {
		// Bubblewrap cannot distinguish inbound from outbound networking. The
		// backend records this tolerated gap as a caveat on the wrapper.
		spec.tolerate_missing(Capability::NetOutbound);
	}

	match settings.env_inherit {
		EnvironmentInheritance::All => {},
		EnvironmentInheritance::Core => {
			spec.set_env_core(true);
		},
		EnvironmentInheritance::None => {
			spec.set_environment(EnvironmentSource::Exact(Vec::new()));
		},
	}
	for pattern in &settings.env_include_only {
		spec.allow_env(pattern.as_str())?;
	}
	for pattern in &settings.env_deny {
		spec.deny_env(pattern.as_str())?;
	}
	for (name, value) in &settings.env_set {
		spec.env_set(name.as_str(), value.as_str());
	}
	for path in &settings.read_deny {
		spec.deny_read(path.as_str())?;
	}

	let mut writable = Vec::new();
	let mut denied = Vec::new();
	if settings.mode == ExecSandboxMode::WorkspaceWrite {
		let mut configured = Vec::with_capacity(3 + settings.writable_roots.len());
		configured.push(workspace_root.to_path_buf());
		configured.extend(
			settings
				.writable_roots
				.iter()
				.map(|root| PathBuf::from(root.as_str())),
		);
		if !settings.exclude_tmpdir {
			configured.push(std::env::temp_dir());
		}
		if !settings.exclude_slash_tmp {
			configured.push(PathBuf::from("/tmp"));
		}

		for root in configured {
			spec.allow_write(&root)?;
			let resolved_root = resolve_write_path(&root)
				.map_err(|source| SandboxError::Canonicalize { path: root.clone(), source })?;
			push_unique(&mut writable, resolved_root.clone());
			if root == workspace_root
				|| settings
					.writable_roots
					.iter()
					.any(|configured| Path::new(configured.as_str()) == root)
			{
				for name in CARVE_OUTS {
					for carve_out in carve_out_paths(&root, &resolved_root, name)
						.map_err(|source| SandboxError::Canonicalize { path: root.join(name), source })?
					{
						record_write_deny(&mut spec, &writable, &mut denied, write, carve_out)?;
					}
				}
			}
		}
	}
	for path in &settings.write_deny {
		let absolute = std::path::absolute(path.as_str()).map_err(|source| {
			SandboxError::Canonicalize { path: PathBuf::from(path.as_str()), source }
		})?;
		let literal = normalize_absolute(&absolute).map_err(|source| SandboxError::Canonicalize {
			path: PathBuf::from(path.as_str()),
			source,
		})?;
		let resolved = resolve_write_path(&absolute)
			.map_err(|source| SandboxError::Canonicalize { path: literal.clone(), source })?;
		record_write_deny(&mut spec, &writable, &mut denied, write, literal)?;
		record_write_deny(&mut spec, &writable, &mut denied, write, resolved)?;
	}

	let roots_label = if writable.is_empty() {
		Str::new_static("no roots")
	} else {
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
			snapshot.push_str(";tmpdir=");
			snapshot.push_str(if settings.exclude_tmpdir {
				"exclude"
			} else {
				"allow"
			});
			snapshot.push_str(";slash_tmp=");
			snapshot.push_str(if settings.exclude_slash_tmp {
				"exclude"
			} else {
				"allow"
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

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
	if !paths.contains(&path) {
		paths.push(path);
	}
}

fn record_write_deny(
	spec: &mut SandboxSpec,
	writable: &[PathBuf],
	denied: &mut Vec<PathBuf>,
	write: WriteMode,
	path: PathBuf,
) -> Result<(), SandboxError> {
	let resolved = resolve_write_path(&path)
		.map_err(|source| SandboxError::Canonicalize { path: path.clone(), source })?;
	if write == WriteMode::Overlay || writable.iter().any(|root| resolved.starts_with(root)) {
		spec.deny_write(&path)?;
	}
	push_unique(denied, path);
	push_unique(denied, resolved);
	Ok(())
}

fn carve_out_paths(root: &Path, resolved_root: &Path, name: &str) -> io::Result<Vec<PathBuf>> {
	let mut paths = Vec::with_capacity(4);
	let absolute = std::path::absolute(root.join(name))?;
	let literal = normalize_absolute(&absolute)?;
	let resolved = resolve_write_path(&absolute)?;
	push_unique(&mut paths, literal);
	push_unique(&mut paths, resolved.clone());

	for candidate in paths.clone() {
		let metadata = match fs::metadata(&candidate) {
			Ok(metadata) => metadata,
			Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
			Err(error) => return Err(error),
		};
		if !metadata.is_file() {
			continue;
		}
		let contents = fs::read_to_string(&candidate)?;
		let Some(gitdir) = contents
			.lines()
			.next()
			.and_then(|line| line.strip_prefix("gitdir: "))
			.map(str::trim)
			.filter(|path| !path.is_empty())
		else {
			continue;
		};
		let target = Path::new(gitdir);
		let target = if target.is_absolute() {
			target.to_path_buf()
		} else {
			candidate.parent().unwrap_or(resolved_root).join(target)
		};
		let absolute_target = std::path::absolute(target)?;
		let literal_target = normalize_absolute(&absolute_target)?;
		let resolved_target = resolve_write_path(&absolute_target)?;
		push_unique(&mut paths, literal_target);
		push_unique(&mut paths, resolved_target);
	}
	Ok(paths)
}

fn capability_failure(error: &SandboxError) -> bool {
	matches!(
		error,
		SandboxError::BackendCapabilities { .. } | SandboxError::NoBackendCapabilities { .. }
	)
}

fn resolve_write_path(path: &Path) -> io::Result<PathBuf> {
	let mut pending = std::path::absolute(path)?;
	for _ in 0..40 {
		let mut components = pending.components().collect::<VecDeque<_>>();
		let mut resolved = PathBuf::new();
		let mut followed_symlink = false;
		while let Some(component) = components.pop_front() {
			match component {
				Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
				Component::RootDir => resolved.push(component.as_os_str()),
				Component::CurDir => {},
				Component::ParentDir => {
					if !resolved.pop() {
						return Err(io::Error::new(
							io::ErrorKind::InvalidInput,
							"write path escapes root",
						));
					}
				},
				Component::Normal(name) => {
					let candidate = resolved.join(name);
					match fs::symlink_metadata(&candidate) {
						Ok(metadata) if metadata.file_type().is_symlink() => {
							let target = fs::read_link(&candidate)?;
							let mut redirected = if target.is_absolute() {
								target
							} else {
								resolved.join(target)
							};
							for remaining in components {
								redirected.push(remaining.as_os_str());
							}
							pending = redirected;
							followed_symlink = true;
							break;
						},
						Ok(_) => resolved = candidate,
						Err(error) if error.kind() == io::ErrorKind::NotFound => {
							resolved = candidate;
						},
						Err(error) => return Err(error),
					}
				},
			}
		}
		if !followed_symlink {
			return normalize_absolute(&resolved);
		}
	}
	Err(io::Error::other("too many symbolic links in write path"))
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
		// Each carve-out is denied under both its literal spelling and its
		// firmlink/symlink-resolved form.
		for name in CARVE_OUTS {
			for form in [workspace.path().join(name), root.join(name)] {
				assert!(parts.write_policy.denied.contains(&form), "missing denied form {form:?}");
			}
		}
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
	fn off_mode_compiles_environment_only_policy_and_applies_overrides_last() {
		let workspace = tempfile::tempdir().expect("workspace");
		let settings = SandboxSettings {
			env_inherit: EnvironmentInheritance::None,
			env_deny: vec![Str::new_static("*KEY*")],
			env_set: std::collections::BTreeMap::from([(
				Str::new_static("FIXED"),
				Str::new_static("value"),
			)]),
			..SandboxSettings::default()
		};
		let sandbox = ExecSandbox::compile(&settings, workspace.path(), true)
			.expect("environment policy")
			.expect("environment-only wrapper");
		assert!(!sandbox.kernel_active());
		assert_eq!(
			sandbox.resolve_env([
				(OsString::from("api_key"), OsString::from("secret")),
				(OsString::from("KEEP"), OsString::from("discarded")),
			]),
			vec![(OsString::from("FIXED"), OsString::from("value"))],
		);
		let settings =
			SandboxSettings { env_deny: vec![Str::new_static("*KEY*")], ..SandboxSettings::default() };
		let sandbox = ExecSandbox::compile(&settings, workspace.path(), true)
			.expect("case-insensitive environment policy")
			.expect("environment-only wrapper");
		assert_eq!(
			sandbox.resolve_env([
				(OsString::from("api_key"), OsString::from("secret")),
				(OsString::from("KEEP"), OsString::from("retained")),
			]),
			vec![(OsString::from("KEEP"), OsString::from("retained"))],
		);
	}
	#[test]
	fn temporary_roots_can_be_excluded_from_both_policy_lanes() {
		let workspace = tempfile::tempdir().expect("workspace");
		let settings = SandboxSettings {
			mode: ExecSandboxMode::WorkspaceWrite,
			exclude_tmpdir: true,
			exclude_slash_tmp: true,
			..SandboxSettings::default()
		};
		let parts =
			policy_parts(&settings, workspace.path(), WriteMode::Scoped).expect("policy parts");
		assert!(
			parts
				.write_policy
				.check_write(&std::env::temp_dir().join("blocked"))
				.is_err()
		);
		assert!(parts.spec_snapshot.contains("tmpdir=exclude"));
		assert!(parts.spec_snapshot.contains("slash_tmp=exclude"));
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
		assert!(policy.check_write(Path::new("/dev/null")).is_ok());
	}

	#[test]
	fn denied_carve_out_also_protects_its_strict_ancestors() {
		let workspace = tempfile::tempdir().expect("workspace");
		fs::create_dir(workspace.path().join(".git")).expect("carve-out");
		let policy = policy_parts(&workspace_settings(), workspace.path(), WriteMode::Scoped)
			.expect("policy")
			.write_policy;
		assert!(policy.check_write(workspace.path()).is_err());
		assert!(policy.check_write(&workspace.path().join("src")).is_ok());
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
		// `..` traversal resolves before matching: the target lands in the
		// denied `.git` carve-out even though the lexical path never names it.
		let escaped = workspace.join("missing/../.git/config");
		assert!(policy.check_write(&escaped).is_err());
		// The sibling resolved the same way stays writable.
		assert!(
			policy
				.check_write(&workspace.join("missing/../kept.txt"))
				.is_ok()
		);
	}
	#[cfg(unix)]
	#[test]
	fn symlink_escape_is_resolved_before_root_matching() {
		use std::os::unix::fs::symlink;

		let sandbox = tempfile::tempdir().expect("sandbox");
		let workspace = sandbox.path().join("workspace");
		fs::create_dir(&workspace).expect("workspace root");
		fs::create_dir(workspace.join(".git")).expect("carve-out root");
		symlink(workspace.join(".git"), workspace.join("link")).expect("escape symlink");
		let policy = policy_parts(&workspace_settings(), &workspace, WriteMode::Scoped)
			.expect("policy")
			.write_policy;
		// The symlink resolves into the denied carve-out before matching.
		assert!(policy.check_write(&workspace.join("link/config")).is_err());
	}
	#[cfg(unix)]
	#[test]
	fn dangling_symlink_is_followed_into_a_future_carve_out_path() {
		use std::os::unix::fs::symlink;

		let workspace = tempfile::tempdir().expect("workspace");
		fs::create_dir(workspace.path().join(".git")).expect("carve-out root");
		symlink(".git/new", workspace.path().join("link")).expect("dangling redirect");
		let policy = policy_parts(&workspace_settings(), workspace.path(), WriteMode::Scoped)
			.expect("policy")
			.write_policy;
		assert!(
			policy
				.check_write(&workspace.path().join("link/config"))
				.is_err()
		);
	}
	#[cfg(unix)]
	#[test]
	fn resolution_errors_fail_closed() {
		use std::os::unix::fs::symlink;

		let workspace = tempfile::tempdir().expect("workspace");
		symlink("loop", workspace.path().join("loop")).expect("symlink loop");
		let policy = policy_parts(&workspace_settings(), workspace.path(), WriteMode::Scoped)
			.expect("policy")
			.write_policy;
		assert!(
			policy
				.check_write(&workspace.path().join("loop/file"))
				.is_err()
		);
	}

	#[cfg(unix)]
	#[test]
	fn carve_out_symlink_protects_literal_and_resolved_target() {
		use std::os::unix::fs::symlink;

		let workspace = tempfile::tempdir().expect("workspace");
		fs::create_dir(workspace.path().join("metadata")).expect("metadata target");
		symlink("metadata", workspace.path().join(".omp")).expect("carve-out symlink");
		let policy = policy_parts(&workspace_settings(), workspace.path(), WriteMode::Scoped)
			.expect("policy")
			.write_policy;
		assert!(policy.check_write(&workspace.path().join(".omp")).is_err());
		assert!(
			policy
				.check_write(&workspace.path().join("metadata/state"))
				.is_err()
		);
	}

	#[test]
	fn gitdir_pointer_protects_referenced_directory() {
		let workspace = tempfile::tempdir().expect("workspace");
		fs::create_dir(workspace.path().join("metadata")).expect("metadata target");
		fs::write(workspace.path().join(".git"), "gitdir: metadata\n").expect("gitdir pointer");
		let policy = policy_parts(&workspace_settings(), workspace.path(), WriteMode::Scoped)
			.expect("policy")
			.write_policy;
		assert!(
			policy
				.check_write(&workspace.path().join("metadata/config"))
				.is_err()
		);
	}
}
