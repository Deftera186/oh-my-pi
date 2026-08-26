//! Bubblewrap plan, preparation, and live confinement contracts.

use std::{fs, path::Path};

use omp_sandbox::{
	Backend, Capability, DegradationPolicy, FilesystemVirtualizationKind, NetworkMode,
	ResourceLimits, Runner, SandboxError, SandboxSpec, WriteMode,
};
use tempfile::tempdir;

#[test]
fn broad_view_has_required_namespaces_and_runtime_mounts() {
	let plan = compile(SandboxSpec::new(executable()));
	let argv = plan.argv();
	assert_eq!(argv[1], "--die-with-parent");
	assert_eq!(argv[2], "--new-session");
	assert_eq!(argv[3], "--unshare-all");
	assert_eq!(argv[4], "--preserve-fds");
	assert_eq!(argv[5], "1");
	assert!(has_mount(argv, "--ro-bind", Path::new("/"), Path::new("/")));
	assert!(has_pair(argv, "--proc", Path::new("/proc")));
	assert!(has_pair(argv, "--dev", Path::new("/dev")));
	assert!(!argv.iter().any(|argument| argument == "--share-net"));
	assert!(plan.enforced().contains(Capability::NetDisable));
	assert!(plan.enforced().contains(Capability::FsReadHost));
	assert_enforced_subset(&plan);
}

#[test]
fn enabled_network_alone_shares_the_host_network_namespace() {
	let mut spec = SandboxSpec::new(executable());
	spec.set_network(NetworkMode::Enabled);
	let plan = compile(spec);
	assert!(plan.argv().iter().any(|argument| argument == "--share-net"));
	assert!(plan.enforced().contains(Capability::NetEnable));
	assert!(!plan.enforced().contains(Capability::NetDisable));
	assert_enforced_subset(&plan);
}

#[test]
fn scoped_view_binds_loader_reads_and_writable_overrides() {
	let root = tempdir().expect("temporary paths");
	let readable = root.path().join("readable");
	let writable = root.path().join("writable");
	fs::create_dir(&readable).expect("readable directory");
	fs::create_dir(&writable).expect("writable directory");
	let readable = fs::canonicalize(readable).expect("canonical readable directory");
	let writable = fs::canonicalize(writable).expect("canonical writable directory");

	let mut spec = SandboxSpec::new(executable());
	spec.allow_read(&readable).expect("read scope");
	spec.set_write(WriteMode::Scoped);
	spec.allow_write(&writable).expect("write scope");
	spec.set_allow_temp(true);
	let plan = compile(spec);

	assert!(has_pair(plan.argv(), "--tmpfs", Path::new("/")));
	assert!(has_mount(plan.argv(), "--ro-bind", &readable, &readable));
	assert!(has_mount(plan.argv(), "--bind", &writable, &writable));
	assert!(has_pair(plan.argv(), "--tmpfs", Path::new("/tmp")));
	let resolved_program = fs::canonicalize(executable()).expect("resolved executable");
	assert!(has_mount(plan.argv(), "--ro-bind", &resolved_program, &resolved_program,));
	assert!(plan.enforced().contains(Capability::FsReadScope));
	assert!(plan.enforced().contains(Capability::FsWriteScope));
	assert_enforced_subset(&plan);
}

#[test]
fn existing_read_denies_compile_to_typed_mask_mounts() {
	let root = tempdir().expect("temporary paths");
	let denied_file = root.path().join("secret");
	let denied_directory = root.path().join("private");
	fs::write(&denied_file, "secret").expect("denied file");
	fs::create_dir(&denied_directory).expect("denied directory");
	let denied_file = fs::canonicalize(denied_file).expect("canonical denied file");
	let denied_directory = fs::canonicalize(denied_directory).expect("canonical denied directory");

	let mut spec = SandboxSpec::new(executable());
	spec.deny_read(&denied_file).expect("file deny");
	spec.deny_read(&denied_directory).expect("directory deny");
	let plan = compile(spec);

	let file_source = mount_source(plan.argv(), "--ro-bind", &denied_file);
	let directory_source = mount_source(plan.argv(), "--ro-bind", &denied_directory);
	assert_eq!(file_source.to_str(), Some("@omp-bwrap-file-mask@"));
	assert_eq!(directory_source.to_str(), Some("@omp-bwrap-directory-mask@"));
	assert!(plan.enforced().contains(Capability::FsReadDeny));
	assert_enforced_subset(&plan);
}

#[test]
fn future_read_denies_reject_or_drop_the_unenforceable_guarantee() {
	let root = tempdir().expect("temporary paths");
	let future = root.path().join("not-created");
	let mut strict = SandboxSpec::new(executable());
	strict.deny_read(&future).expect("future deny");
	let error = Runner::for_backend(Backend::Bubblewrap)
		.compile(&strict)
		.expect_err("future target cannot be overmounted");
	assert!(matches!(
		error,
		SandboxError::BackendCapabilities { backend: Backend::Bubblewrap, missing }
			if missing.contains(Capability::FsReadDeny)
	));

	strict.set_degradation(DegradationPolicy::AllowCaveats);
	let plan = compile(strict);
	assert!(!plan.enforced().contains(Capability::FsReadDeny));
	assert!(
		plan
			.caveats()
			.iter()
			.any(|caveat| caveat.capability == Some(Capability::FsReadDeny))
	);
	assert_enforced_subset(&plan);
}

#[test]
fn unsupported_modes_reject_or_narrow_authoritative_enforcement() {
	let mut outbound = SandboxSpec::new(executable());
	outbound.set_network(NetworkMode::Outbound);
	assert_missing(&outbound, Capability::NetOutbound);
	outbound.set_degradation(DegradationPolicy::AllowCaveats);
	let plan = compile(outbound);
	assert!(plan.enforced().contains(Capability::NetDisable));
	assert!(!plan.enforced().contains(Capability::NetOutbound));
	assert!(!plan.argv().iter().any(|argument| argument == "--share-net"));
	assert_caveat(&plan, Capability::NetOutbound);
	assert_enforced_subset(&plan);

	let mut ephemeral = SandboxSpec::new(executable());
	ephemeral.set_write(WriteMode::Ephemeral);
	assert_missing(&ephemeral, Capability::FsWriteEphemeral);
	ephemeral.set_degradation(DegradationPolicy::AllowCaveats);
	let plan = compile(ephemeral);
	assert!(plan.enforced().contains(Capability::FsWriteDeny));
	assert!(!plan.enforced().contains(Capability::FsWriteEphemeral));
	assert_caveat(&plan, Capability::FsWriteEphemeral);
	assert_enforced_subset(&plan);

	let root = tempdir().expect("temporary paths");
	let writable = root.path().join("writable");
	fs::create_dir(&writable).expect("writable directory");
	let writable = fs::canonicalize(writable).expect("canonical writable directory");
	let mut overlay = SandboxSpec::new(executable());
	overlay.set_write(WriteMode::Overlay);
	overlay.allow_write(&writable).expect("write scope");
	assert_missing(&overlay, Capability::FsWriteEphemeral);
	overlay.set_degradation(DegradationPolicy::AllowCaveats);
	let plan = compile(overlay);
	assert!(plan.enforced().contains(Capability::FsWriteScope));
	assert!(!plan.enforced().contains(Capability::FsWriteEphemeral));
	assert_eq!(plan.filesystem_virtualization(), Some(FilesystemVirtualizationKind::ScopedDeny),);
	assert!(has_mount(plan.argv(), "--bind", &writable, &writable));
	assert_caveat(&plan, Capability::FsWriteEphemeral);
	assert_enforced_subset(&plan);
}

#[test]
fn no_exec_and_resources_are_never_advertised() {
	let mut spec = SandboxSpec::new(executable());
	spec.set_no_exec(true);
	spec.set_resource_limits(
		ResourceLimits::new(Some(0.5), Some(1024), Some(2)).expect("resource limits"),
	);
	let error = Runner::for_backend(Backend::Bubblewrap)
		.compile(&spec)
		.expect_err("unsupported hard guarantees");
	let SandboxError::BackendCapabilities { backend, missing } = error else {
		panic!("unexpected error: {error:?}");
	};
	assert_eq!(backend, Backend::Bubblewrap);
	for capability in
		[Capability::ProcNoExec, Capability::ResCpu, Capability::ResMemory, Capability::ResPids]
	{
		assert!(missing.contains(capability));
	}

	spec.set_degradation(DegradationPolicy::AllowCaveats);
	let plan = compile(spec);
	for capability in
		[Capability::ProcNoExec, Capability::ResCpu, Capability::ResMemory, Capability::ResPids]
	{
		assert!(!plan.enforced().contains(capability));
		assert!(
			plan
				.caveats()
				.iter()
				.any(|caveat| caveat.capability == Some(capability))
		);
	}
	assert_enforced_subset(&plan);
}

#[cfg(unix)]
#[test]
fn explicit_unix_socket_is_bound_without_claiming_ipc_restriction() {
	use std::os::unix::net::UnixListener;

	let root = tempdir().expect("temporary paths");
	let socket = root.path().join("service.sock");
	let _listener = UnixListener::bind(&socket).expect("Unix listener");
	let mut spec = SandboxSpec::new(executable());
	spec.allow_read(executable()).expect("scoped executable");
	spec.allow_unix_socket(&socket).expect("socket grant");
	let plan = compile(spec);
	let socket = fs::canonicalize(socket).expect("canonical socket");
	assert!(has_mount(plan.argv(), "--bind", &socket, &socket));
	assert!(!plan.requested().contains(Capability::IpcRestrict));
	assert!(!plan.enforced().contains(Capability::IpcRestrict));
	assert_enforced_subset(&plan);
}

#[cfg(target_os = "linux")]
#[test]
fn prepared_masks_are_private_owned_and_removed_on_drop() {
	use std::os::unix::fs::PermissionsExt as _;

	if !omp_sandbox::backend_status(Backend::Bubblewrap).is_available() {
		return;
	}
	let root = tempdir().expect("temporary paths");
	let denied_file = root.path().join("secret");
	let denied_directory = root.path().join("private");
	fs::write(&denied_file, "secret").expect("denied file");
	fs::create_dir(&denied_directory).expect("denied directory");
	let mut spec = SandboxSpec::new(executable());
	spec.arg("@omp-bwrap-file-mask@");
	spec.deny_read(&denied_file).expect("file deny");
	spec.deny_read(&denied_directory).expect("directory deny");
	let runner = Runner::for_backend(Backend::Bubblewrap);
	let plan = runner.compile(&spec).expect("Bubblewrap plan");
	let prepared = runner.prepare(plan, &spec).expect("prepared masks");
	let file_mask = mount_source(prepared.args(), "--ro-bind", &denied_file).to_path_buf();
	let directory_mask = mount_source(prepared.args(), "--ro-bind", &denied_directory).to_path_buf();
	assert_eq!(
		prepared
			.args()
			.last()
			.and_then(|argument| argument.to_str()),
		Some("@omp-bwrap-file-mask@"),
	);
	assert_eq!(
		fs::metadata(&file_mask)
			.expect("file mask")
			.permissions()
			.mode()
			& 0o777,
		0o600
	);
	assert_eq!(
		fs::metadata(&directory_mask)
			.expect("directory mask")
			.permissions()
			.mode()
			& 0o777,
		0o700,
	);
	drop(prepared);
	assert!(!file_mask.exists());
	assert!(!directory_mask.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn live_bubblewrap_confines_reads_writes_socket_and_preserves_fd_three() {
	use std::{
		io::Read as _,
		os::{
			fd::AsRawFd as _,
			unix::{
				net::{UnixListener, UnixStream},
				process::CommandExt as _,
			},
		},
		process::Stdio,
		thread,
	};

	if !omp_sandbox::backend_status(Backend::Bubblewrap).is_available() {
		return;
	}
	let root = tempdir().expect("temporary paths");
	let readable = root.path().join("readable");
	let denied = root.path().join("denied");
	let writable_directory = root.path().join("writable");
	let writable = writable_directory.join("created");
	let outside = root.path().join("outside");
	let socket = root.path().join("service.sock");
	fs::write(&readable, "readable").expect("readable file");
	fs::write(&denied, "denied-secret").expect("denied file");
	fs::create_dir(&writable_directory).expect("writable directory");
	let listener = UnixListener::bind(&socket).expect("Unix listener");

	let script = r#"
import os, socket, sys
readable, denied, writable, outside, socket_path = sys.argv[1:]
assert open(readable).read() == "readable"
try:
    denied_value = open(denied).read()
except OSError:
    denied_value = ""
assert denied_value != "denied-secret"
with open(writable, "w") as stream:
    stream.write("persisted")
try:
    with open(outside, "w") as stream:
        stream.write("escaped")
except OSError:
    pass
else:
    raise AssertionError("outside write succeeded")
client = socket.socket(socket.AF_UNIX)
client.connect(socket_path)
client.sendall(b"socket")
client.close()
os.fstat(3)
print("ok")
"#;
	let mut spec = SandboxSpec::new("python3");
	spec
		.arg("-c")
		.arg(script)
		.arg(&readable)
		.arg(&denied)
		.arg(&writable)
		.arg(&outside)
		.arg(&socket);
	spec.set_write(WriteMode::Scoped);
	spec.allow_write(&writable_directory).expect("write scope");
	spec.deny_read(&denied).expect("read deny");
	spec.allow_unix_socket(&socket).expect("socket grant");

	let runner = Runner::for_backend(Backend::Bubblewrap);
	let plan = runner.compile(&spec).expect("Bubblewrap plan");
	let prepared = runner.prepare(plan, &spec).expect("prepared Bubblewrap");
	let server = thread::spawn(move || {
		let (mut stream, _) = listener.accept().expect("sandbox socket connection");
		let mut bytes = [0; 6];
		stream.read_exact(&mut bytes).expect("socket payload");
		bytes
	});
	let (inherited_child, _inherited_parent) =
		UnixStream::pair().expect("inherited descriptor pair");
	let inherited_fd = inherited_child.as_raw_fd();
	let mut command = prepared.command().expect("prepared command");
	command.stdout(Stdio::piped()).stderr(Stdio::piped());
	// SAFETY: the closure performs only async-signal-safe dup2 and error lookup
	// between fork and exec; the source descriptor remains owned until output().
	unsafe {
		command.pre_exec(move || {
			if libc::dup2(inherited_fd, 3) == -1 {
				return Err(std::io::Error::last_os_error());
			}
			Ok(())
		});
	}
	let output = command.output().expect("launch Bubblewrap probe");
	assert!(
		output.status.success(),
		"sandbox probe failed: {}",
		String::from_utf8_lossy(&output.stderr),
	);
	assert_eq!(output.stdout, b"ok\n");
	assert_eq!(server.join().expect("socket server"), *b"socket");
	assert_eq!(fs::read_to_string(&writable).expect("persistent write"), "persisted");
	assert!(!outside.exists());
	assert_eq!(fs::read_to_string(&denied).expect("host denied file"), "denied-secret");
}

fn compile(spec: SandboxSpec) -> omp_sandbox::Plan {
	Runner::for_backend(Backend::Bubblewrap)
		.compile(&spec)
		.expect("Bubblewrap plan")
}

fn assert_missing(spec: &SandboxSpec, capability: Capability) {
	let error = Runner::for_backend(Backend::Bubblewrap)
		.compile(spec)
		.expect_err("strict unsupported capability");
	assert!(matches!(
		error,
		SandboxError::BackendCapabilities { backend: Backend::Bubblewrap, missing }
			if missing.contains(capability)
	));
}

fn assert_caveat(plan: &omp_sandbox::Plan, capability: Capability) {
	assert!(
		plan
			.caveats()
			.iter()
			.any(|caveat| caveat.capability == Some(capability))
	);
}

fn assert_enforced_subset(plan: &omp_sandbox::Plan) {
	assert!(
		plan
			.enforced()
			.difference(Backend::Bubblewrap.capabilities())
			.is_empty()
	);
}

fn has_pair(argv: &[std::ffi::OsString], option: &str, value: &Path) -> bool {
	argv
		.windows(2)
		.any(|window| window[0] == option && Path::new(&window[1]) == value)
}

fn has_mount(argv: &[std::ffi::OsString], option: &str, source: &Path, target: &Path) -> bool {
	argv.windows(3).any(|window| {
		window[0] == option && Path::new(&window[1]) == source && Path::new(&window[2]) == target
	})
}

fn mount_source<'a>(argv: &'a [std::ffi::OsString], option: &str, target: &Path) -> &'a Path {
	argv
		.windows(3)
		.find(|window| window[0] == option && Path::new(&window[2]) == target)
		.map(|window| Path::new(&window[1]))
		.expect("mount target")
}

fn executable() -> &'static Path {
	#[cfg(target_os = "linux")]
	return Path::new("/bin/true");
	#[cfg(target_os = "macos")]
	return Path::new("/usr/bin/true");
	#[cfg(windows)]
	return Path::new(r"C:\Windows\System32\cmd.exe");
	#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
	return Path::new("/bin/true");
}
