//! Fail-closed native process confinement.
//!
//! A sandboxed caller must launch the returned [`SandboxLaunch`] rather than
//! the requested executable directly. Backend absence and invalid grants are
//! errors; this crate never substitutes an unconfined command.

use std::{
	ffi::OsString,
	fs, io,
	path::{Path, PathBuf},
	process::Command,
	sync::LazyLock,
};

use thiserror::Error;

/// Network access granted to a sandboxed process tree.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NetworkPolicy {
	/// Deny IP networking and ungranted local-socket operations.
	#[default]
	Deny,
	/// Permit networking inherited from the host.
	Allow,
}

/// Native confinement backend proven usable on this host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
	/// Apple's Seatbelt profile runner.
	Seatbelt,
	/// Linux bubblewrap namespace isolation.
	Bubblewrap,
}

static DETECTED_BACKEND: LazyLock<Result<Backend, String>> = LazyLock::new(probe_backend);

impl Backend {
	/// Detects and smoke-checks the native backend.
	///
	/// A present executable that cannot install confinement is reported as
	/// unavailable, so capability discovery cannot advertise a broken backend.
	pub fn detect() -> Result<Self, SandboxError> {
		match &*DETECTED_BACKEND {
			Ok(backend) => Ok(*backend),
			Err(reason) => Err(SandboxError::BackendUnavailable(reason.clone())),
		}
	}
}

/// Failure to construct an enforceable sandbox launch.
#[derive(Debug, Error)]
pub enum SandboxError {
	/// A filesystem grant or executable could not be resolved without symlinks.
	#[error("failed to canonicalize sandbox path {path}")]
	Canonicalize {
		/// Path supplied by the caller.
		path:   PathBuf,
		/// Filesystem error.
		#[source]
		source: io::Error,
	},
	/// No usable native backend exists.
	#[error("sandbox enforcement is unavailable: {0}")]
	BackendUnavailable(String),
}

/// Canonical filesystem and network capabilities for one process tree.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SandboxPolicy {
	read:    Vec<PathBuf>,
	write:   Vec<PathBuf>,
	network: NetworkPolicy,
}

impl SandboxPolicy {
	/// Creates an empty, network-denied policy.
	#[must_use]
	pub const fn new() -> Self {
		Self { read: Vec::new(), write: Vec::new(), network: NetworkPolicy::Deny }
	}

	/// Grants read access to an existing file or directory.
	///
	/// Granting a Unix-domain socket also grants connection to that exact local
	/// socket without enabling IP networking.
	pub fn allow_read(&mut self, path: impl AsRef<Path>) -> Result<&mut Self, SandboxError> {
		let path = canonicalize(path.as_ref())?;
		insert_unique(&mut self.read, path);
		Ok(self)
	}

	/// Grants read/write access to an existing file or directory.
	///
	/// Grant a directory when the child must create new descendants.
	pub fn allow_write(&mut self, path: impl AsRef<Path>) -> Result<&mut Self, SandboxError> {
		let path = canonicalize(path.as_ref())?;
		insert_unique(&mut self.write, path);
		Ok(self)
	}

	/// Sets the explicit network policy.
	pub const fn set_network(&mut self, network: NetworkPolicy) -> &mut Self {
		self.network = network;
		self
	}

	/// Returns canonical read-only grants.
	#[must_use]
	pub fn read_grants(&self) -> &[PathBuf] {
		&self.read
	}

	/// Returns canonical read/write grants.
	#[must_use]
	pub fn write_grants(&self) -> &[PathBuf] {
		&self.write
	}

	/// Returns the explicit network policy.
	#[must_use]
	pub const fn network(&self) -> NetworkPolicy {
		self.network
	}

	/// Prepares a native launch or fails without exposing a raw command.
	pub fn prepare(&self, program: impl AsRef<Path>) -> Result<SandboxLaunch, SandboxError> {
		let program = canonicalize(program.as_ref())?;
		let backend = Backend::detect()?;
		launch_for(backend, self, &program)
	}
}

/// Native launcher and mandatory argument prefix for a confined process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxLaunch {
	backend: Backend,
	program: PathBuf,
	args:    Vec<OsString>,
}

impl SandboxLaunch {
	/// Returns the native sandbox launcher executable.
	#[must_use]
	pub fn program(&self) -> &Path {
		&self.program
	}

	/// Returns arguments that must precede the requested process arguments.
	#[must_use]
	pub fn args(&self) -> &[OsString] {
		&self.args
	}

	/// Returns the native backend enforcing this launch.
	#[must_use]
	pub const fn backend(&self) -> Backend {
		self.backend
	}

	/// Constructs a standard-library command with confinement already applied.
	///
	/// Callers using an async process type must equivalently pass [`Self::args`]
	/// to [`Self::program`] before appending the requested process arguments.
	#[must_use]
	pub fn command(&self) -> Command {
		let mut command = Command::new(&self.program);
		command.args(&self.args);
		command
	}
}

fn canonicalize(path: &Path) -> Result<PathBuf, SandboxError> {
	fs::canonicalize(path)
		.map_err(|source| SandboxError::Canonicalize { path: path.to_path_buf(), source })
}

fn insert_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
	if !paths.contains(&path) {
		paths.push(path);
		paths.sort();
	}
}

#[cfg(target_os = "macos")]
fn probe_backend() -> Result<Backend, String> {
	let launcher = Path::new("/usr/bin/sandbox-exec");
	if !launcher.is_file() {
		return Err("/usr/bin/sandbox-exec is not installed".to_owned());
	}
	let output = Command::new(launcher)
		.args(["-p", "(version 1) (allow default)", "/usr/bin/true"])
		.output()
		.map_err(|error| format!("failed to start sandbox-exec probe: {error}"))?;
	if output.status.success() {
		Ok(Backend::Seatbelt)
	} else {
		Err(format!("sandbox-exec probe failed: {}", String::from_utf8_lossy(&output.stderr).trim()))
	}
}

#[cfg(target_os = "linux")]
fn probe_backend() -> Result<Backend, String> {
	let Some(launcher) = ["/usr/bin/bwrap", "/bin/bwrap"]
		.into_iter()
		.map(Path::new)
		.find(|path| path.is_file())
	else {
		return Err("bubblewrap is not installed".to_owned());
	};
	let output = Command::new(launcher)
		.args(["--die-with-parent", "--unshare-all", "--ro-bind", "/", "/", "--", "/bin/true"])
		.output()
		.map_err(|error| format!("failed to start bubblewrap probe: {error}"))?;
	if output.status.success() {
		Ok(Backend::Bubblewrap)
	} else {
		Err(format!("bubblewrap probe failed: {}", String::from_utf8_lossy(&output.stderr).trim()))
	}
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn probe_backend() -> Result<Backend, String> {
	Err(format!("{} has no supported native sandbox backend", std::env::consts::OS))
}

#[cfg(target_os = "macos")]
fn launch_for(
	backend: Backend,
	policy: &SandboxPolicy,
	program: &Path,
) -> Result<SandboxLaunch, SandboxError> {
	debug_assert_eq!(backend, Backend::Seatbelt);
	let profile = seatbelt_profile(policy, program);
	Ok(SandboxLaunch {
		backend,
		program: PathBuf::from("/usr/bin/sandbox-exec"),
		args: vec![OsString::from("-p"), OsString::from(profile), program.as_os_str().to_owned()],
	})
}

#[cfg(target_os = "macos")]
fn seatbelt_profile(policy: &SandboxPolicy, program: &Path) -> String {
	let mut profile =
		String::from("(version 1)\n(deny default)\n(allow process-fork)\n(allow process-exec ");
	push_seatbelt_path(&mut profile, program);
	profile.push_str(
		")\n(allow signal (target self))\n(allow sysctl-read)\n(allow file-read* (subpath \
		 \"/System\") (subpath \"/usr/lib\") (subpath \"/private/var/db/dyld\") (literal \
		 \"/dev/null\") (literal \"/dev/random\") (literal \"/dev/urandom\") ",
	);
	push_seatbelt_path(&mut profile, program);
	for path in policy.read.iter().chain(&policy.write) {
		profile.push(' ');
		push_seatbelt_path(&mut profile, path);
	}
	profile.push_str(")\n");
	if !policy.write.is_empty() {
		profile.push_str("(allow file-write* ");
		for path in &policy.write {
			push_seatbelt_path(&mut profile, path);
			profile.push(' ');
		}
		profile.push_str(")\n");
	}
	if policy.network == NetworkPolicy::Allow {
		profile.push_str("(allow network*)\n");
	} else {
		use std::os::unix::fs::FileTypeExt as _;
		for path in policy.read.iter().chain(&policy.write) {
			if fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
				profile.push_str("(allow network-outbound (remote unix-socket ");
				push_seatbelt_filter(&mut profile, "path-literal", path);
				profile.push_str("))\n");
			}
		}
	}
	profile
}

#[cfg(target_os = "macos")]
fn push_seatbelt_path(profile: &mut String, path: &Path) {
	let filter = if path.is_dir() { "subpath" } else { "literal" };
	push_seatbelt_filter(profile, filter, path);
}

#[cfg(target_os = "macos")]
fn push_seatbelt_filter(profile: &mut String, filter: &str, path: &Path) {
	profile.push('(');
	profile.push_str(filter);
	profile.push_str(" \"");
	for character in path.as_os_str().to_string_lossy().chars() {
		match character {
			'\\' => profile.push_str("\\\\"),
			'"' => profile.push_str("\\\""),
			character => profile.push(character),
		}
	}
	profile.push_str("\")");
}

#[cfg(target_os = "linux")]
fn launch_for(
	backend: Backend,
	policy: &SandboxPolicy,
	program: &Path,
) -> Result<SandboxLaunch, SandboxError> {
	debug_assert_eq!(backend, Backend::Bubblewrap);
	let launcher = ["/usr/bin/bwrap", "/bin/bwrap"]
		.into_iter()
		.map(Path::new)
		.find(|path| path.is_file())
		.ok_or_else(|| {
			SandboxError::BackendUnavailable("detected bubblewrap disappeared".to_owned())
		})?;
	let mut args = ["--die-with-parent", "--new-session", "--unshare-all"]
		.into_iter()
		.map(OsString::from)
		.collect::<Vec<_>>();
	if policy.network == NetworkPolicy::Allow {
		args.push(OsString::from("--share-net"));
	}
	for (option, destination) in [("--proc", "/proc"), ("--dev", "/dev"), ("--tmpfs", "/tmp")] {
		args.extend([OsString::from(option), OsString::from(destination)]);
	}
	for system in ["/lib", "/lib64", "/usr/lib", "/usr/lib64"] {
		let path = Path::new(system);
		if path.exists() {
			push_bind(&mut args, "--ro-bind", path);
		}
	}
	for path in &policy.read {
		if !covered_by(path, &policy.write) {
			push_bind(&mut args, "--ro-bind", path);
		}
	}
	for path in &policy.write {
		push_bind(&mut args, "--bind", path);
	}
	if !covered_by(program, &policy.read) && !covered_by(program, &policy.write) {
		push_bind(&mut args, "--ro-bind", program);
	}
	args.push(OsString::from("--"));
	args.push(program.as_os_str().to_owned());
	Ok(SandboxLaunch { backend, program: launcher.to_path_buf(), args })
}

#[cfg(target_os = "linux")]
fn push_bind(args: &mut Vec<OsString>, option: &str, path: &Path) {
	args.push(OsString::from(option));
	args.push(path.as_os_str().to_owned());
	args.push(path.as_os_str().to_owned());
}

#[cfg(target_os = "linux")]
fn covered_by(path: &Path, grants: &[PathBuf]) -> bool {
	grants
		.iter()
		.any(|grant| path == grant || path.starts_with(grant))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn launch_for(
	_backend: Backend,
	_policy: &SandboxPolicy,
	_program: &Path,
) -> Result<SandboxLaunch, SandboxError> {
	unreachable!("unsupported platforms cannot detect a backend")
}

#[cfg(test)]
mod tests {
	use std::{fs, process::Stdio};

	use tempfile::tempdir;

	use super::{Backend, NetworkPolicy, SandboxError, SandboxPolicy};

	#[test]
	fn grants_are_canonical_and_network_is_denied_by_default() {
		let directory = tempdir().expect("temp directory");
		let nested = directory.path().join("nested");
		fs::create_dir(&nested).expect("nested directory");
		let mut policy = SandboxPolicy::new();
		policy.allow_read(&nested).expect("read grant");
		policy.allow_read(&nested).expect("deduplicated read grant");
		assert_eq!(policy.read_grants(), &[fs::canonicalize(nested).unwrap()]);
		assert_eq!(policy.network(), NetworkPolicy::Deny);
		assert!(matches!(
			policy.allow_write(directory.path().join("missing")),
			Err(SandboxError::Canonicalize { .. })
		));
	}

	#[test]
	fn native_launch_denies_ungranted_file_reads() {
		if Backend::detect().is_err() {
			return;
		}
		let directory = tempdir().expect("temp directory");
		let granted = directory.path().join("granted");
		let denied = directory.path().join("denied");
		fs::write(&granted, "visible").expect("granted fixture");
		fs::write(&denied, "secret").expect("denied fixture");

		let mut policy = SandboxPolicy::new();
		policy.allow_read(&granted).expect("grant fixture");
		let launch = policy.prepare("/bin/cat").expect("enforced launch");

		let granted_output = launch
			.command()
			.arg(&granted)
			.stderr(Stdio::piped())
			.output()
			.expect("run granted read");
		assert!(
			granted_output.status.success(),
			"granted read failed: {}",
			String::from_utf8_lossy(&granted_output.stderr)
		);

		let denied_output = launch
			.command()
			.arg(&denied)
			.output()
			.expect("run denied read");
		assert!(!denied_output.status.success());
	}
}
