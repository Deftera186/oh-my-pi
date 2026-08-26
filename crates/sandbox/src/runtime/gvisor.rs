use std::{
	env,
	ffi::{OsStr, OsString},
	fs::OpenOptions,
	io,
	os::unix::fs::OpenOptionsExt as _,
	path::{Path, PathBuf},
	process::{Command as StdCommand, ExitStatus, Output, Stdio},
};

use omp_core::CowBytes;
use tempfile::{Builder, TempDir};
use tokio::{
	io::{AsyncReadExt as _, AsyncWriteExt as _},
	process::{Child, Command},
	task::JoinHandle,
	time,
};

use crate::{
	Backend, CleanupFailure, CleanupFailures, OutputMode, Plan, PreparedSandbox, RunFailure,
	RunOptions, RunOutput, SandboxError, SandboxExit, SandboxInput, SandboxOperation, SandboxSpec,
	backends::{
		gvisor,
		gvisor_oci::{GvisorOciPlan, GvisorPlaceholder, config, needs_oci, validate_resources},
	},
	environment::split_entry,
	paths::resolve_program,
	runtime::linux_view,
};

const DIAGNOSTIC_LIMIT: usize = 4096;
const NETNS_DIRECTORY: &str = "/var/run/netns";

#[derive(Debug)]
pub(crate) enum GvisorResource {
	Directory(Option<TempDir>),
	NetworkNamespace { name: String },
	HostLink { name: String },
	Sysctl { key: &'static str, old: String },
	Firewall { args: Vec<OsString> },
	Container { runtime: OsString, id: String },
}

impl GvisorResource {
	fn cleanup(&mut self) -> Result<(), CleanupFailure> {
		match self {
			Self::Directory(directory) => {
				let Some(directory) = directory.take() else {
					return Ok(());
				};
				let path = directory.path().to_path_buf();
				directory
					.close()
					.map_err(|source| CleanupFailure::BackendPath {
						backend: Backend::Gvisor,
						operation: SandboxOperation::Cleanup,
						path,
						source,
					})
			},
			Self::NetworkNamespace { name } => cleanup_command("ip", [
				OsString::from("netns"),
				OsString::from("delete"),
				OsString::from(name.as_str()),
			]),
			Self::HostLink { name } => cleanup_command("ip", [
				OsString::from("link"),
				OsString::from("delete"),
				OsString::from(name.as_str()),
			]),
			Self::Sysctl { key, old } => cleanup_command("sysctl", [
				OsString::from("-w"),
				OsString::from(format!("{key}={old}")),
			]),
			Self::Firewall { args } => {
				let mut delete = args.clone();
				if let Some(insert) = delete.iter_mut().find(|arg| arg.as_os_str() == "-I") {
					*insert = OsString::from("-D");
				}
				cleanup_command("iptables", delete)
			},
			Self::Container { runtime, id } => cleanup_command(runtime.as_os_str(), [
				OsString::from("delete"),
				OsString::from("--force"),
				OsString::from(id.as_str()),
			]),
		}
	}
}

#[derive(Debug, Default)]
pub(crate) struct GvisorPrepared {
	resources: Vec<GvisorResource>,
	container: Option<(OsString, String)>,
}

impl GvisorPrepared {
	pub(crate) fn push(&mut self, resource: GvisorResource) {
		self.resources.push(resource);
	}

	fn mark_container_started(&mut self) {
		if let Some((runtime, id)) = self.container.take() {
			self.push(GvisorResource::Container { runtime, id });
		}
	}

	pub(crate) fn cleanup(&mut self) -> Result<(), CleanupFailures> {
		let mut failures = Vec::new();
		while let Some(mut resource) = self.resources.pop() {
			if let Err(failure) = resource.cleanup() {
				failures.push(failure);
			}
		}
		if failures.is_empty() {
			Ok(())
		} else {
			Err(CleanupFailures::new(failures))
		}
	}
}

impl Drop for GvisorPrepared {
	fn drop(&mut self) {
		let _ = self.cleanup();
	}
}

pub(crate) fn prepare(
	prepared: &mut PreparedSandbox,
	plan: &Plan,
	spec: &SandboxSpec,
) -> Result<GvisorPrepared, SandboxError> {
	let mut state = GvisorPrepared::default();
	validate_resources(spec)?;
	if !needs_oci(spec) {
		return Ok(state);
	}
	let program = resolve_program(&spec.program)?;
	let runtime = prepared
		.program()
		.ok_or(SandboxError::EmptyPlanArgv { backend: Backend::Gvisor })?
		.to_owned();
	let oci = GvisorOciPlan::new(spec, runtime_flags_from_plan(plan));
	if !oci.denied_syscalls.is_empty() {
		ensure_oci_seccomp(&runtime)?;
	}

	let bundle = Builder::new()
		.prefix("omp-sandbox-runsc-")
		.tempdir()
		.map_err(|source| SandboxError::BackendIo {
			backend: Backend::Gvisor,
			operation: SandboxOperation::Prepare,
			source,
		})?;
	let bundle_path = bundle.path().to_path_buf();
	state.push(GvisorResource::Directory(Some(bundle)));

	let filesystem = if oci.needs_filesystem_view {
		linux_view::prepare(spec, &program, &mut state)?
	} else {
		linux_view::PreparedFilesystem { rootfs: PathBuf::from("/"), mounts: Vec::new() }
	};
	let netns = prepare_network(&oci, &mut state)?;
	let environment = prepared
		.environment
		.take()
		.unwrap_or_else(current_environment);
	let rendered = config(
		spec,
		&program,
		&oci,
		&filesystem.rootfs,
		filesystem.mounts,
		Some(&netns),
		&environment,
	);
	let config_path = bundle_path.join("config.json");
	let mut bytes =
		serde_json::to_vec_pretty(&rendered).map_err(|source| SandboxError::BackendJson {
			backend: Backend::Gvisor,
			operation: SandboxOperation::Prepare,
			source,
		})?;
	bytes.push(b'\n');
	let mut config_file = OpenOptions::new()
		.write(true)
		.create_new(true)
		.mode(0o600)
		.open(&config_path)
		.map_err(|source| SandboxError::BackendPath {
			backend: Backend::Gvisor,
			operation: SandboxOperation::Prepare,
			path: config_path.clone(),
			source,
		})?;
	use std::io::Write as _;
	config_file
		.write_all(&bytes)
		.map_err(|source| SandboxError::BackendPath {
			backend: Backend::Gvisor,
			operation: SandboxOperation::Prepare,
			path: config_path,
			source,
		})?;

	let mut replaced = false;
	let bundle_placeholder = GvisorPlaceholder::Bundle.value();
	for arg in &mut prepared.args {
		if arg.as_os_str() == bundle_placeholder {
			*arg = bundle_path.as_os_str().to_owned();
			replaced = true;
		}
	}
	if !replaced {
		return Err(SandboxError::MissingPlanPlaceholder {
			backend:     Backend::Gvisor,
			placeholder: bundle_placeholder,
		});
	}
	prepared.cwd = None;
	prepared.environment = None;
	state.container = Some((runtime, oci.id));
	Ok(state)
}

fn runtime_flags_from_plan(plan: &Plan) -> Vec<OsString> {
	let Some((_, args)) = plan.argv().split_first() else {
		return Vec::new();
	};
	args
		.iter()
		.take_while(|arg| {
			let arg = arg.to_string_lossy();
			arg.starts_with('-') && arg != "--oci-seccomp"
		})
		.cloned()
		.collect()
}

fn current_environment() -> Vec<OsString> {
	env::vars_os()
		.map(|(name, value)| {
			let mut entry = name;
			entry.push("=");
			entry.push(value);
			entry
		})
		.collect()
}

fn ensure_oci_seccomp(runtime: &OsStr) -> Result<(), SandboxError> {
	let mut output =
		command_output(runtime, [OsString::from("features")], SandboxOperation::Prepare)?;
	if output.status.success()
		&& (String::from_utf8_lossy(&output.stdout).contains("oci-seccomp")
			|| String::from_utf8_lossy(&output.stderr).contains("oci-seccomp"))
	{
		Ok(())
	} else {
		output.stderr.extend_from_slice(&output.stdout);
		Err(command_rejected(output, SandboxOperation::Prepare))
	}
}

fn prepare_network(
	plan: &GvisorOciPlan,
	state: &mut GvisorPrepared,
) -> Result<PathBuf, SandboxError> {
	let name = short_linux_name("cg", &plan.id);
	run_checked("ip", ["netns", "add", name.as_str()], SandboxOperation::Prepare)?;
	state.push(GvisorResource::NetworkNamespace { name: name.clone() });
	run_checked(
		"ip",
		["netns", "exec", name.as_str(), "ip", "link", "set", "lo", "up"],
		SandboxOperation::Prepare,
	)?;
	if plan.network != crate::NetworkMode::Enabled {
		for setting in ["net.ipv6.conf.all.disable_ipv6=1", "net.ipv6.conf.default.disable_ipv6=1"] {
			run_checked(
				"ip",
				["netns", "exec", name.as_str(), "sysctl", "-w", setting],
				SandboxOperation::Prepare,
			)?;
		}
	}
	if plan.network == crate::NetworkMode::Disabled {
		return Ok(Path::new(NETNS_DIRECTORY).join(name));
	}

	let host_link = short_linux_name("ch", &plan.id);
	let guest_link = short_linux_name("cs", &plan.id);
	let (subnet, host_ip, guest_ip) = subnet(&plan.id);
	run_checked(
		"ip",
		["link", "add", host_link.as_str(), "type", "veth", "peer", "name", guest_link.as_str()],
		SandboxOperation::Prepare,
	)?;
	state.push(GvisorResource::HostLink { name: host_link.clone() });
	for args in [
		vec!["addr".into(), "add".into(), format!("{host_ip}/30"), "dev".into(), host_link.clone()],
		vec!["link".into(), "set".into(), host_link.clone(), "up".into()],
		vec!["link".into(), "set".into(), guest_link.clone(), "netns".into(), name.clone()],
		vec![
			"netns".into(),
			"exec".into(),
			name.clone(),
			"ip".into(),
			"addr".into(),
			"add".into(),
			format!("{guest_ip}/30"),
			"dev".into(),
			guest_link.clone(),
		],
		vec![
			"netns".into(),
			"exec".into(),
			name.clone(),
			"ip".into(),
			"link".into(),
			"set".into(),
			guest_link.clone(),
			"up".into(),
		],
		vec![
			"netns".into(),
			"exec".into(),
			name.clone(),
			"ip".into(),
			"route".into(),
			"add".into(),
			"default".into(),
			"via".into(),
			host_ip.clone(),
		],
	] {
		run_checked_os("ip", args.into_iter().map(OsString::from), SandboxOperation::Prepare)?;
	}

	let old_forward = read_sysctl("net.ipv4.ip_forward")?;
	if old_forward != "1" {
		run_checked("sysctl", ["-w", "net.ipv4.ip_forward=1"], SandboxOperation::Prepare)?;
		state.push(GvisorResource::Sysctl { key: "net.ipv4.ip_forward", old: old_forward });
	}
	add_firewall(state, [
		"-t",
		"nat",
		"-I",
		"POSTROUTING",
		"-s",
		subnet.as_str(),
		"-j",
		"MASQUERADE",
	])?;
	add_firewall(state, ["-I", "FORWARD", "-s", subnet.as_str(), "-j", "ACCEPT"])?;
	add_firewall(state, [
		"-I",
		"FORWARD",
		"-d",
		subnet.as_str(),
		"-m",
		"conntrack",
		"--ctstate",
		"ESTABLISHED,RELATED",
		"-j",
		"ACCEPT",
	])?;
	if plan.network == crate::NetworkMode::Outbound {
		for args in outbound_firewall_rules(&subnet) {
			add_firewall_os(state, args)?;
		}
	}
	Ok(Path::new(NETNS_DIRECTORY).join(name))
}
pub(crate) fn probe_requirements(spec: &SandboxSpec) -> Result<crate::BackendStatus, SandboxError> {
	validate_resources(spec)?;
	let plan = GvisorOciPlan::new(spec, Vec::new());
	if !plan.denied_syscalls.is_empty() {
		let status = gvisor::probe_oci_seccomp();
		if !status.is_available() {
			return Ok(status);
		}
	}
	let mut state = GvisorPrepared::default();
	if let Err(error) = prepare_network(&plan, &mut state) {
		let status = status_from_setup_error(error)?;
		let _ = state.cleanup();
		return Ok(status);
	}
	if state.cleanup().is_err() {
		return Ok(crate::BackendStatus::unavailable(
			Backend::Gvisor,
			crate::ProbeFailure::Rejected {
				backend:    Backend::Gvisor,
				operation:  SandboxOperation::Probe,
				status:     None,
				diagnostic: CowBytes::from(Vec::new()),
			},
		));
	}
	Ok(crate::BackendStatus::available(Backend::Gvisor))
}

fn status_from_setup_error(error: SandboxError) -> Result<crate::BackendStatus, SandboxError> {
	let failure = match error {
		SandboxError::BackendIo { source, .. } => crate::ProbeFailure::Start {
			backend: Backend::Gvisor,
			operation: SandboxOperation::Probe,
			source,
		},
		SandboxError::BackendCommand { status, diagnostic, .. } => crate::ProbeFailure::Rejected {
			backend: Backend::Gvisor,
			operation: SandboxOperation::Probe,
			status,
			diagnostic,
		},
		error => return Err(error),
	};
	Ok(crate::BackendStatus::unavailable(Backend::Gvisor, failure))
}

fn add_firewall<const N: usize>(
	state: &mut GvisorPrepared,
	args: [&str; N],
) -> Result<(), SandboxError> {
	add_firewall_os(state, args.into_iter().map(OsString::from).collect())
}

fn add_firewall_os(state: &mut GvisorPrepared, args: Vec<OsString>) -> Result<(), SandboxError> {
	run_checked_os("iptables", args.clone(), SandboxOperation::Prepare)?;
	state.push(GvisorResource::Firewall { args });
	Ok(())
}

pub(crate) fn outbound_firewall_rules(subnet: &str) -> [Vec<OsString>; 2] {
	[
		["-I", "FORWARD", "-d", subnet, "-m", "conntrack", "--ctstate", "NEW", "-j", "DROP"]
			.into_iter()
			.map(OsString::from)
			.collect(),
		["-I", "OUTPUT", "-d", subnet, "-m", "conntrack", "--ctstate", "NEW", "-j", "DROP"]
			.into_iter()
			.map(OsString::from)
			.collect(),
	]
}

fn read_sysctl(key: &'static str) -> Result<String, SandboxError> {
	let output = command_output(
		"sysctl",
		[OsString::from("-n"), OsString::from(key)],
		SandboxOperation::Prepare,
	)?;
	if !output.status.success() {
		return Err(command_rejected(output, SandboxOperation::Prepare));
	}
	Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn run_checked<const N: usize>(
	program: impl AsRef<OsStr>,
	args: [&str; N],
	operation: SandboxOperation,
) -> Result<(), SandboxError> {
	run_checked_os(program, args.into_iter().map(OsString::from), operation)
}

fn run_checked_os(
	program: impl AsRef<OsStr>,
	args: impl IntoIterator<Item = OsString>,
	operation: SandboxOperation,
) -> Result<(), SandboxError> {
	let output = command_output(program, args, operation)?;
	if output.status.success() {
		Ok(())
	} else {
		Err(command_rejected(output, operation))
	}
}

fn command_output(
	program: impl AsRef<OsStr>,
	args: impl IntoIterator<Item = OsString>,
	operation: SandboxOperation,
) -> Result<Output, SandboxError> {
	StdCommand::new(program.as_ref())
		.args(args)
		.output()
		.map_err(|source| SandboxError::BackendIo { backend: Backend::Gvisor, operation, source })
}

fn command_rejected(mut output: Output, operation: SandboxOperation) -> SandboxError {
	output.stderr.truncate(DIAGNOSTIC_LIMIT);
	SandboxError::BackendCommand {
		backend: Backend::Gvisor,
		operation,
		status: output.status.code(),
		diagnostic: CowBytes::from(output.stderr),
	}
}

fn cleanup_command(
	program: impl AsRef<OsStr>,
	args: impl IntoIterator<Item = OsString>,
) -> Result<(), CleanupFailure> {
	let output = StdCommand::new(program.as_ref())
		.args(args)
		.output()
		.map_err(|source| CleanupFailure::BackendIo {
			backend: Backend::Gvisor,
			operation: SandboxOperation::Cleanup,
			source,
		})?;
	if output.status.success() {
		Ok(())
	} else {
		let mut diagnostic = output.stderr;
		diagnostic.truncate(DIAGNOSTIC_LIMIT);
		Err(CleanupFailure::BackendCommand {
			backend:    Backend::Gvisor,
			operation:  SandboxOperation::Cleanup,
			status:     output.status.code(),
			diagnostic: CowBytes::from(diagnostic),
		})
	}
}

pub(crate) fn short_linux_name(prefix: &str, id: &str) -> String {
	let digest = id.rsplit('-').next().unwrap_or(id);
	let digest = &digest[..digest.len().min(8)];
	let name = format!("{prefix}{digest}");
	name[..name.len().min(15)].to_owned()
}

pub(crate) fn subnet(id: &str) -> (String, String, String) {
	let digest = id.rsplit('-').next().unwrap_or(id);
	let parsed = (digest.len() >= 4)
		.then(|| u16::from_str_radix(&digest[..4], 16).ok())
		.flatten()
		.unwrap_or(0x5800);
	let octet = (parsed >> 8) as u8;
	let block = ((parsed as u8) % 64) * 4;
	let base = format!("10.203.{octet}.");
	(format!("{base}{block}/30"), format!("{base}{}", block + 1), format!("{base}{}", block + 2))
}

pub(crate) async fn run(
	prepared: &PreparedSandbox,
	state: &mut GvisorPrepared,
	options: RunOptions,
) -> Result<RunOutput, SandboxError> {
	let result = run_process(prepared, state, options).await;
	let cleanup = state.cleanup();
	match (result, cleanup) {
		(Ok(output), Ok(())) => Ok(output),
		(Ok(_), Err(cleanup)) => Err(SandboxError::Cleanup(cleanup)),
		(Err(error), Ok(())) => Err(error),
		(Err(error), Err(cleanup)) => {
			let run = run_failure(error)?;
			Err(SandboxError::RunAndCleanup { backend: Backend::Gvisor, run, cleanup })
		},
	}
}

async fn run_process(
	prepared: &PreparedSandbox,
	state: &mut GvisorPrepared,
	options: RunOptions,
) -> Result<RunOutput, SandboxError> {
	let mut command = prepared_command(prepared)?;
	command.stdin(match &options.input {
		SandboxInput::Inherit => Stdio::inherit(),
		SandboxInput::Null => Stdio::null(),
		SandboxInput::Bytes(_) => Stdio::piped(),
	});
	command.stdout(output_stdio(options.stdout));
	command.stderr(output_stdio(options.stderr));
	command.process_group(0);
	let child = command
		.spawn()
		.map_err(|source| SandboxError::Launch { backend: Backend::Gvisor, source })?;
	state.mark_container_started();
	let mut child = GvisorChild::new(child);
	let input = match options.input {
		SandboxInput::Bytes(bytes) => child.child_mut().stdin.take().map(|mut stdin| {
			tokio::spawn(async move {
				stdin.write_all(bytes.as_ref()).await?;
				stdin.shutdown().await
			})
		}),
		SandboxInput::Inherit | SandboxInput::Null => None,
	};
	let stdout = capture(child.child_mut().stdout.take());
	let stderr = capture(child.child_mut().stderr.take());
	let status = if let Some(timeout) = options.timeout {
		match time::timeout(timeout, child.wait()).await {
			Ok(result) => result?,
			Err(_) => {
				child.kill_and_reap().await;
				return Err(SandboxError::Timeout { backend: Backend::Gvisor });
			},
		}
	} else {
		child.wait().await?
	};
	join_input(input).await?;
	let stdout = join_output(stdout).await?;
	let stderr = join_output(stderr).await?;
	Ok(RunOutput { exit: sandbox_exit(status), stdout: stdout.into(), stderr: stderr.into() })
}
fn run_failure(error: SandboxError) -> Result<RunFailure, SandboxError> {
	match error {
		SandboxError::Launch { source, .. } => Ok(RunFailure::Launch { source }),
		SandboxError::Wait { source, .. } => Ok(RunFailure::Wait { source }),
		SandboxError::Input { source, .. } => Ok(RunFailure::Input { source }),
		SandboxError::Output { source, .. } => Ok(RunFailure::Output { source }),
		SandboxError::Timeout { .. } => Ok(RunFailure::Timeout),
		SandboxError::BackendCommand { operation, status, diagnostic, .. } => {
			Ok(RunFailure::BackendCommand { operation, status, diagnostic })
		},
		SandboxError::BackendIo { operation, source, .. } => {
			Ok(RunFailure::BackendIo { operation, source })
		},
		error => Err(error),
	}
}

fn prepared_command(prepared: &PreparedSandbox) -> Result<Command, SandboxError> {
	let Some(program) = prepared.program() else {
		return Err(SandboxError::ExternalCommandUnsupported { backend: Backend::Gvisor });
	};
	let mut command = Command::new(program);
	command.args(prepared.args());
	if let Some(cwd) = prepared.cwd() {
		command.current_dir(cwd);
	}
	if let Some(environment) = prepared.environment() {
		command.env_clear();
		for entry in environment {
			let (name, value) = split_entry(entry);
			command.env(name, value);
		}
	}
	Ok(command)
}

fn output_stdio(mode: OutputMode) -> Stdio {
	match mode {
		OutputMode::Inherit => Stdio::inherit(),
		OutputMode::Null => Stdio::null(),
		OutputMode::Capture => Stdio::piped(),
	}
}

fn capture<R>(stream: Option<R>) -> Option<JoinHandle<io::Result<Vec<u8>>>>
where
	R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
	stream.map(|mut stream| {
		tokio::spawn(async move {
			let mut bytes = Vec::new();
			stream.read_to_end(&mut bytes).await?;
			Ok(bytes)
		})
	})
}

async fn join_input(input: Option<JoinHandle<io::Result<()>>>) -> Result<(), SandboxError> {
	if let Some(input) = input {
		input
			.await
			.map_err(|error| SandboxError::Input {
				backend: Backend::Gvisor,
				source:  io::Error::other(error),
			})?
			.map_err(|source| SandboxError::Input { backend: Backend::Gvisor, source })?;
	}
	Ok(())
}

async fn join_output(
	output: Option<JoinHandle<io::Result<Vec<u8>>>>,
) -> Result<Vec<u8>, SandboxError> {
	match output {
		Some(output) => output
			.await
			.map_err(|error| SandboxError::Output {
				backend: Backend::Gvisor,
				source:  io::Error::other(error),
			})?
			.map_err(|source| SandboxError::Output { backend: Backend::Gvisor, source }),
		None => Ok(Vec::new()),
	}
}

pub(crate) struct GvisorChild {
	child:  Option<Child>,
	reaped: bool,
}

impl GvisorChild {
	fn new(child: Child) -> Self {
		Self { child: Some(child), reaped: false }
	}

	fn child_mut(&mut self) -> &mut Child {
		self
			.child
			.as_mut()
			.expect("gVisor child remains owned until reaped")
	}

	pub(crate) async fn wait(&mut self) -> Result<ExitStatus, SandboxError> {
		let status = self
			.child_mut()
			.wait()
			.await
			.map_err(|source| SandboxError::Wait { backend: Backend::Gvisor, source })?;
		self.reaped = true;
		Ok(status)
	}

	pub(crate) async fn kill_and_reap(&mut self) {
		if self.reaped {
			return;
		}
		if let Some(id) = self.child_mut().id() {
			let _ = nix::sys::signal::kill(
				nix::unistd::Pid::from_raw(-(id as i32)),
				nix::sys::signal::Signal::SIGKILL,
			);
		}
		let _ = self.child_mut().start_kill();
		let _ = self.child_mut().wait().await;
		self.reaped = true;
	}
}

impl Drop for GvisorChild {
	fn drop(&mut self) {
		if self.reaped {
			return;
		}
		if let Some(child) = &mut self.child {
			if let Some(id) = child.id() {
				let _ = nix::sys::signal::kill(
					nix::unistd::Pid::from_raw(-(id as i32)),
					nix::sys::signal::Signal::SIGKILL,
				);
			}
			let _ = child.start_kill();
		}
		if let Some(mut child) = self.child.take()
			&& let Ok(handle) = tokio::runtime::Handle::try_current()
		{
			handle.spawn(async move {
				let _ = child.wait().await;
			});
		}
	}
}

fn sandbox_exit(status: ExitStatus) -> SandboxExit {
	use std::os::unix::process::ExitStatusExt as _;
	SandboxExit { code: status.code(), signal: status.signal() }
}
#[cfg(test)]
mod tests {
	use std::io;

	use omp_core::CowBytes;

	use super::{outbound_firewall_rules, short_linux_name, status_from_setup_error, subnet};
	use crate::{Backend, ProbeFailure, SandboxError, SandboxOperation};

	#[test]
	fn outbound_rules_drop_new_host_and_forwarded_ingress() {
		let rules = outbound_firewall_rules("10.203.9.40/30");
		assert_eq!(rules[0], [
			"-I",
			"FORWARD",
			"-d",
			"10.203.9.40/30",
			"-m",
			"conntrack",
			"--ctstate",
			"NEW",
			"-j",
			"DROP"
		],);
		assert_eq!(rules[1], [
			"-I",
			"OUTPUT",
			"-d",
			"10.203.9.40/30",
			"-m",
			"conntrack",
			"--ctstate",
			"NEW",
			"-j",
			"DROP"
		],);
	}

	#[test]
	fn deterministic_subnet_uses_two_digest_bytes() {
		assert_eq!(
			subnet("omp-sandbox-gvisor-0123456789abcdef"),
			("10.203.1.140/30".into(), "10.203.1.141".into(), "10.203.1.142".into(),),
		);
		assert_eq!(short_linux_name("cg", "omp-sandbox-gvisor-0123456789abcdef"), "cg01234567");
	}
	#[test]
	fn privilege_setup_io_failure_remains_typed_in_probe_status() {
		let status = status_from_setup_error(SandboxError::BackendIo {
			backend:   Backend::Gvisor,
			operation: SandboxOperation::Prepare,
			source:    io::Error::from(io::ErrorKind::PermissionDenied),
		})
		.expect("probe status");
		assert!(matches!(
			status.failure(),
			Some(ProbeFailure::Start { source, .. })
				if source.kind() == io::ErrorKind::PermissionDenied
		));
	}

	#[test]
	fn privilege_setup_rejection_retains_status_and_diagnostic() {
		let status = status_from_setup_error(SandboxError::BackendCommand {
			backend:    Backend::Gvisor,
			operation:  SandboxOperation::Prepare,
			status:     Some(1),
			diagnostic: CowBytes::from(b"operation not permitted".to_vec()),
		})
		.expect("probe status");
		assert!(matches!(
			status.failure(),
			Some(ProbeFailure::Rejected { status: Some(1), diagnostic, .. })
				if diagnostic.as_ref() == b"operation not permitted"
		));
	}
}
