//! Extension-host child spawning over a dedicated CONTROL descriptor.

use std::{
	env, io, mem,
	os::fd,
	path::PathBuf,
	process::Stdio,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use flume::Receiver;
#[cfg(unix)]
use nix::{sys::signal, unistd::Pid};
use omp_core::Str;
use omp_sandbox::{
	DegradationPolicy, NetworkMode, PreparedSandbox, Runner, SandboxError, SandboxSpec, WriteMode,
};
use pyo3::{
	prelude::*,
	types::{PyList, PyModule},
};
use thiserror::Error;
use tokio::{
	io::{AsyncRead, AsyncReadExt},
	net::UnixStream,
	process::{Child, Command},
	task, time,
};

use super::{
	cancel::{CancellationError, CancellationLadder, CancellationOutcome},
	control::{
		ControlAuthority, ControlAuthoritySnapshot, ControlConnectionIdentity, ControlHandle,
		ControlProtocolError, ControlRuntime, ControlRuntimeError,
	},
};
use crate::worker::HostKey;

/// Hidden argv selector for one extension-host child.
pub const EXT_HOST_ARG: &str = "__omp-ext-host";
/// Environment variable carrying the inherited CONTROL descriptor number.
pub const CONTROL_FD_ENV: &str = "OMP_EXT_CONTROL_FD";
/// Environment variable carrying the extension-scoped DATA socket path.
pub const ENV_SOCKET_ENV: &str = "OMP_EXT_ENV_SOCKET";
/// Environment variable carrying the extension-private Python site tree.
pub const PY_SITE_ENV: &str = "OMP_PY_SITE";
/// Environment variable carrying the verified package snapshot JSON.
pub const PACKAGE_SNAPSHOT_ENV: &str = "OMP_EXT_PACKAGE_SNAPSHOT";
/// Environment variable carrying the admitted declaration manifest JSON.
pub const MANIFEST_SNAPSHOT_ENV: &str = "OMP_EXT_MANIFEST_SNAPSHOT";
/// Environment variable carrying manifest-ordered declaration modules as JSON.
pub const DECLARATION_MODULES_ENV: &str = "OMP_EXT_DECLARATION_MODULES";

/// One captured child output fragment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostLog {
	/// Stream which emitted the fragment.
	pub stream: HostLogStream,
	/// Raw output bytes; framing is intentionally not interpreted as CONTROL.
	pub bytes:  Vec<u8>,
}

/// Captured output source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostLogStream {
	/// Child standard output.
	Stdout,
	/// Child standard error.
	Stderr,
}

/// Spawn inputs authenticated before a child is reached.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
	/// Isolated host identity `(layer, tier, unit)`; the existing `HostKey`
	/// calls the unit `extension`.
	pub key:                 HostKey,
	/// Same-binary executable to re-enter.
	pub executable:          PathBuf,
	/// Per-extension Python site tree.
	pub python_site:         PathBuf,
	/// Scoped Environment DATA socket.
	pub env_socket:          PathBuf,
	/// Generation assigned to this newly spawned child.
	pub host_generation:     u64,
	/// Session generation shared with the CONTROL parent.
	pub session_generation:  u64,
	/// Verified package ownership snapshot encoded for the Python bootstrap.
	///
	/// `None` identifies an anonymous or development extension and installs an
	/// explicitly empty package snapshot in the child.
	pub package_snapshot:    Option<Str>,
	/// Admitted declaration manifest, never inferred from runtime registration.
	pub manifest_snapshot:   Str,
	/// Entry and declaration modules in deterministic manifest order.
	pub declaration_modules: Box<[Str]>,
}

/// Owned parent ends for an extension-host child.
pub struct SpawnedHost {
	/// Authenticated host identity.
	pub key:      HostKey,
	/// Supervised child process group leader.
	pub child:    Child,
	/// Dedicated bidirectional CONTROL transport, never stdio.
	pub control:  UnixStream,
	/// Captured stdout/stderr records.
	pub logs:     Receiver<HostLog>,
	restart_spec: SpawnSpec,
	sandbox:      Option<PreparedSandbox>,
}
/// Live supervised child with its sole CONTROL pump and cancellation state.
pub struct RunningHost {
	/// Authenticated isolated child identity.
	pub key:      HostKey,
	child:        Child,
	control:      ControlHandle,
	logs:         Receiver<HostLog>,
	pump:         task::JoinHandle<Result<(), ControlRuntimeError>>,
	cancellation: CancellationLadder,
	restart_spec: SpawnSpec,
	identity:     ControlConnectionIdentity,
	authority:    Arc<dyn ControlAuthority>,
	snapshot:     ControlAuthoritySnapshot,
	sandbox:      Option<PreparedSandbox>,
}

/// Failure while driving a live child or its cancellation ladder.
#[derive(Debug, Error)]
pub enum RunningHostError {
	/// CONTROL transport or protocol failure.
	#[error(transparent)]
	Control(#[from] ControlRuntimeError),
	/// Forced process-group cancellation failed.
	#[error(transparent)]
	Cancellation(#[from] CancellationError),
	/// Replacement child spawn failed.
	#[error(transparent)]
	Spawn(#[from] SpawnError),
	/// Child generation counter cannot be advanced safely.
	#[error("extension host generation is exhausted")]
	GenerationExhausted,
}

impl SpawnedHost {
	/// Starts the sole parent reader and installs synchronous Core authority.
	pub async fn start_control(
		self,
		identity: ControlConnectionIdentity,
		authority: Arc<dyn ControlAuthority>,
		snapshot: &ControlAuthoritySnapshot,
	) -> Result<RunningHost, RunningHostError> {
		let Self { key, mut child, control, logs, restart_spec, sandbox } = self;
		let (runtime, handle) =
			ControlRuntime::new(control, key.clone(), identity.clone(), Arc::clone(&authority));
		let pump = tokio::spawn(runtime.serve());
		if let Err(error) = handle.install_authority_snapshot(snapshot).await {
			pump.abort();
			let _ = child.start_kill();
			return Err(error.into());
		}
		Ok(RunningHost {
			key,
			child,
			control: handle,
			logs,
			pump,
			cancellation: CancellationLadder::default(),
			restart_spec,
			identity,
			authority,
			snapshot: snapshot.clone(),
			sandbox,
		})
	}
}

impl RunningHost {
	/// Returns the cloneable host-to-child dispatch handle.
	pub fn control(&self) -> ControlHandle {
		self.control.clone()
	}

	/// Returns captured stdout/stderr records without mixing them into CONTROL.
	pub const fn logs(&self) -> &Receiver<HostLog> {
		&self.logs
	}

	/// Returns the generation authenticated by this live CONTROL child.
	pub const fn generation(&self) -> u64 {
		self.restart_spec.host_generation
	}

	/// Reports a child or CONTROL-pump exit without consuming its owner.
	pub fn has_exited(&mut self) -> Result<bool, RunningHostError> {
		let child_exited = self.child.try_wait().map_err(SpawnError::Spawn)?.is_some();
		if child_exited {
			self.sandbox.take();
		}
		Ok(self.pump.is_finished() || child_exited)
	}

	/// Returns whether repeated forced cancellation disabled this host.
	pub fn is_disabled(&self) -> bool {
		self.cancellation.disabled(&self.key)
	}

	/// Reaps the current process group and starts its next authenticated
	/// generation.
	pub async fn restart(&mut self) -> Result<(), RunningHostError> {
		self.terminate().await;
		let mut spec = self.restart_spec.clone();
		spec.host_generation = spec
			.host_generation
			.checked_add(1)
			.ok_or(RunningHostError::GenerationExhausted)?;
		let mut identity = self.identity.clone();
		identity.host_generation = spec.host_generation;
		let cancellation = mem::take(&mut self.cancellation);
		let spawned = spawn(spec).await?;
		let mut replacement = spawned
			.start_control(identity, Arc::clone(&self.authority), &self.snapshot)
			.await?;
		replacement.cancellation = cancellation;
		*self = replacement;
		Ok(())
	}

	/// Terminates and reaps this owned child process group.
	pub async fn shutdown(mut self) {
		self.terminate().await;
	}

	/// Runs all three cancellation stages, killing only this process group when
	/// Python remains live after both courtesy graces.
	pub async fn cancel_dispatch(
		&mut self,
		invocation: &str,
	) -> Result<CancellationOutcome, RunningHostError> {
		let last_frame = self.control.last_frame(invocation).unwrap_or(0);
		self.control.cancel(invocation).await?;
		CancellationLadder::grace_timer().await;
		if !self.control.is_live(invocation) {
			return Ok(self.cancellation.begin());
		}
		CancellationLadder::grace_timer().await;
		if !self.control.is_live(invocation) {
			return Ok(self.cancellation.interrupt_after_grace());
		}
		let outcome =
			self
				.cancellation
				.kill_after_grace(self.key.clone(), &mut self.child, last_frame)?;
		match outcome {
			CancellationOutcome::Killed(_) => self.restart().await?,
			CancellationOutcome::Disabled(_) => self.terminate().await,
			CancellationOutcome::DispatchCancel | CancellationOutcome::InterruptThread => {},
		}
		Ok(outcome)
	}

	async fn terminate(&mut self) {
		self.pump.abort();
		if let Some(pid) = self.child.id() {
			#[cfg(unix)]
			{
				let group = Pid::from_raw(pid.cast_signed());
				let _ = signal::killpg(group, signal::Signal::SIGTERM);
				time::sleep(Duration::from_millis(150)).await;
				let _ = signal::killpg(group, signal::Signal::SIGKILL);
			}
			#[cfg(windows)]
			{
				let _ = self.child.start_kill();
			}
		}
		let _ = self.child.wait().await;
		self.sandbox.take();
	}

	/// Waits for the sole CONTROL pump to finish.
	pub async fn wait_control(mut self) -> Result<(), RunningHostError> {
		let result = match (&mut self.pump).await {
			Ok(result) => result.map_err(Into::into),
			Err(error) => Err(
				ControlRuntimeError::Protocol(ControlProtocolError::new(
					"control_task_failed",
					error.to_string(),
				))
				.into(),
			),
		};
		self.terminate().await;
		result
	}
}

/// Host-child bound and spawn failures.
#[derive(Debug, Error)]
pub enum SpawnError {
	/// The session already reached its admitted child bound.
	#[error("omp.MAX_HOST_CHILDREN ({limit}) is exhausted")]
	ChildLimit {
		/// Configured session bound.
		limit: usize,
	},
	/// Creating or configuring the CONTROL socket failed.
	#[error("CONTROL descriptor setup failed: {0}")]
	Control(#[from] io::Error),
	/// The embedded Python extension-host runtime failed to boot.
	#[error("extension host Python runtime failed: {0}")]
	Python(String),
	/// Native sandbox installation failed for a sandboxed host.
	#[error(transparent)]
	Sandbox(#[from] SandboxError),
	/// The host trust tier does not have an explicit launch policy.
	#[error("unsupported extension host trust tier: {0}")]
	UnsupportedTier(Str),
	/// The child process could not be spawned.
	#[error("extension host spawn failed: {0}")]
	Spawn(io::Error),
}

/// Session-local lazy child admission bound.
#[derive(Clone, Debug)]
pub struct HostChildLimit {
	limit: usize,
	live:  Arc<AtomicUsize>,
}

impl HostChildLimit {
	/// Creates a lazy-spawn admission bound.
	pub fn new(limit: usize) -> Self {
		Self { limit, live: Arc::new(AtomicUsize::new(0)) }
	}

	/// Starts a child only after its declared surface is reached.
	///
	/// The returned permit is released when [`Self::release`] is called after
	/// the process is reaped.
	pub async fn spawn_on_reach(&self, spec: SpawnSpec) -> Result<SpawnedHost, SpawnError> {
		let previous = self.live.fetch_add(1, Ordering::AcqRel);
		if previous >= self.limit {
			self.live.fetch_sub(1, Ordering::AcqRel);
			return Err(SpawnError::ChildLimit { limit: self.limit });
		}
		match spawn(spec).await {
			Ok(host) => Ok(host),
			Err(error) => {
				self.live.fetch_sub(1, Ordering::AcqRel);
				Err(error)
			},
		}
	}

	/// Releases one reaped child slot.
	pub fn release(&self) {
		self.live.fetch_sub(1, Ordering::AcqRel);
	}
}

/// Spawns one isolated extension host with CONTROL on descriptor three.
pub async fn spawn(spec: SpawnSpec) -> Result<SpawnedHost, SpawnError> {
	let restart_spec = spec.clone();
	let (parent, child_control) = UnixStream::pair()?;
	let fd = fd::AsRawFd::as_raw_fd(&child_control);
	let sandbox_launch = match spec.key.tier().as_str() {
		"sandboxed" => {
			let mut sandbox = SandboxSpec::new(spec.executable.as_os_str());
			sandbox.arg(EXT_HOST_ARG);
			sandbox.allow_read(&spec.executable)?;
			sandbox.allow_read(&spec.python_site)?;
			sandbox.set_write(WriteMode::Scoped);
			sandbox.allow_write(&spec.env_socket)?;
			sandbox.allow_unix_socket(&spec.env_socket)?;
			sandbox.set_network(NetworkMode::Disabled);
			sandbox.set_degradation(DegradationPolicy::Reject);
			Some({
				let runner = Runner::native_command()?;
				let plan = runner.compile(&sandbox)?;
				runner.prepare(plan, &sandbox)?
			})
		},
		"trusted" => None,
		tier => return Err(SpawnError::UnsupportedTier(Str::from(tier))),
	};
	let mut command = if let Some(launch) = &sandbox_launch {
		Command::from(launch.command()?)
	} else {
		let mut command = Command::new(&spec.executable);
		command.arg(EXT_HOST_ARG);
		command
	};
	command
		.env(CONTROL_FD_ENV, "3")
		.env(PY_SITE_ENV, &spec.python_site)
		.env(ENV_SOCKET_ENV, &spec.env_socket)
		.env("OMP_EXT_LAYER", spec.key.layer().as_str())
		.env("OMP_EXT_TIER", spec.key.tier().as_str())
		.env("OMP_EXT_HOST_GENERATION", spec.host_generation.to_string())
		.env("OMP_EXT_SESSION_GENERATION", spec.session_generation.to_string())
		.env(MANIFEST_SNAPSHOT_ENV, spec.manifest_snapshot.as_str())
		.env(
			DECLARATION_MODULES_ENV,
			serde_json::to_string(
				&spec
					.declaration_modules
					.iter()
					.map(Str::as_str)
					.collect::<Vec<_>>(),
			)
			.map_err(|error| SpawnError::Python(error.to_string()))?,
		)
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true);
	if let Some(snapshot) = &spec.package_snapshot {
		command.env(PACKAGE_SNAPSHOT_ENV, snapshot.as_str());
	} else {
		command.env_remove(PACKAGE_SNAPSHOT_ENV);
	}
	#[cfg(unix)]
	{
		// The child owns a fresh process group. Its CONTROL peer is duplicated
		// onto a stable descriptor; stdio remains ordinary captured logging.
		unsafe {
			command.pre_exec(move || {
				if nix::libc::setpgid(0, 0) == -1 {
					return Err(io::Error::last_os_error());
				}
				if nix::libc::dup2(fd, 3) == -1 {
					return Err(io::Error::last_os_error());
				}
				let flags = nix::libc::fcntl(3, nix::libc::F_GETFD);
				if flags == -1
					|| nix::libc::fcntl(3, nix::libc::F_SETFD, flags & !nix::libc::FD_CLOEXEC) == -1
				{
					return Err(io::Error::last_os_error());
				}
				Ok(())
			});
		}
	}
	let mut child = command.spawn().map_err(SpawnError::Spawn)?;
	drop(child_control);
	let (logs_tx, logs) = flume::unbounded();
	if let Some(stdout) = child.stdout.take() {
		capture(stdout, HostLogStream::Stdout, logs_tx.clone());
	}
	if let Some(stderr) = child.stderr.take() {
		capture(stderr, HostLogStream::Stderr, logs_tx);
	}
	Ok(SpawnedHost {
		key: spec.key,
		child,
		control: parent,
		logs,
		restart_spec,
		sandbox: sandbox_launch,
	})
}

fn capture<R>(stream: R, source: HostLogStream, logs: flume::Sender<HostLog>)
where
	R: AsyncRead + Unpin + Send + 'static,
{
	tokio::spawn(async move {
		let mut stream = stream;
		let mut bytes = [0_u8; 4096];
		loop {
			let Ok(read) = stream.read(&mut bytes).await else {
				return;
			};
			if read == 0 {
				return;
			}
			if logs
				.send_async(HostLog { stream: source, bytes: bytes[..read].to_vec() })
				.await
				.is_err()
			{
				return;
			}
		}
	});
}

/// Runs the hidden extension-host child entry.
///
/// The Python runtime owns the protocol loop after this function establishes
/// that CONTROL is an inherited descriptor rather than standard input.
pub fn run_ext_host_entry() -> Result<(), SpawnError> {
	let fd = env::var(CONTROL_FD_ENV)
		.ok()
		.and_then(|value| value.parse::<i32>().ok())
		.filter(|fd| *fd >= 0)
		.ok_or_else(|| {
			SpawnError::Control(io::Error::new(
				io::ErrorKind::InvalidInput,
				"missing OMP_EXT_CONTROL_FD",
			))
		})?;
	#[cfg(unix)]
	unsafe {
		if nix::libc::fcntl(fd, nix::libc::F_GETFD) == -1 {
			return Err(SpawnError::Control(io::Error::last_os_error()));
		}
	}
	let engine = omp_py::Engine::builder()
		.init()
		.map_err(|error| SpawnError::Python(error.to_string()))?;
	install_package_snapshot(&engine)?;
	engine
		.attach(|py| -> PyResult<()> {
			let module = PyModule::import(py, "omp._host")?;
			let host = module.getattr("bootstrap")?.call0()?;
			host.call_method0("run_forever")?;
			Ok(())
		})
		.map_err(|error| SpawnError::Python(error.to_string()))
}

/// Installs the private site tree and parent-verified snapshot before any
/// extension module imports.
fn install_package_snapshot(engine: &omp_py::Engine) -> Result<(), SpawnError> {
	let snapshot = env::var(PACKAGE_SNAPSHOT_ENV).unwrap_or_else(|_| {
		String::from(r#"{"distributions":[],"modules":{},"own":null,"tree":null}"#)
	});
	engine
		.attach(|py| -> PyResult<()> {
			if let Ok(site) = env::var(PY_SITE_ENV) {
				let sys = PyModule::import(py, "sys")?;
				let value = sys.getattr("path")?;
				let path = value.cast::<PyList>()?;
				path.insert(0, site)?;
			}
			let packages = PyModule::import(py, "omp.packages")?;
			packages.call_method1("_install_snapshot_json", (snapshot,))?;
			Ok(())
		})
		.map_err(|error| SpawnError::Python(error.to_string()))
}
#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn unknown_trust_tier_never_falls_back_to_raw_spawn() {
		let result = spawn(SpawnSpec {
			key:                 HostKey::new("workspace", "unknown", "fixture"),
			executable:          PathBuf::from("/definitely/not/an/executable"),
			python_site:         PathBuf::from("/definitely/not/a/site"),
			env_socket:          PathBuf::from("/definitely/not/a/socket"),
			host_generation:     1,
			session_generation:  1,
			package_snapshot:    None,
			manifest_snapshot:   Str::new_static("{}"),
			declaration_modules: Box::new([]),
		})
		.await;
		assert!(matches!(result, Err(SpawnError::UnsupportedTier(tier)) if tier == "unknown"));
	}
}
