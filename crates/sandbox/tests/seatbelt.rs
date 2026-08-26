//! Seatbelt profile, Darwin runtime, and live confinement contracts.

use std::{fs, path::Path};

use omp_sandbox::{
	Backend, Capability, DegradationPolicy, FilesystemVirtualizationKind, NetworkMode,
	ResourceLimits, Runner, SandboxSpec, WriteMode,
};
use tempfile::tempdir;

fn caveated_spec(program: impl AsRef<Path>) -> SandboxSpec {
	let mut spec = SandboxSpec::new(program.as_ref().as_os_str());
	spec.set_degradation(DegradationPolicy::AllowCaveats);
	spec
}

fn compile(spec: &SandboxSpec) -> omp_sandbox::Plan {
	let plan = Runner::for_backend(Backend::Seatbelt)
		.compile(spec)
		.expect("compile Seatbelt plan");
	assert!(
		plan
			.enforced()
			.difference(Backend::Seatbelt.capabilities())
			.is_empty()
	);
	plan
}

#[test]
fn profile_orders_mach_network_and_tls_trust_rules() {
	let program = std::env::current_exe().expect("test executable");
	let mut spec = caveated_spec(&program);
	spec
		.allow_mach_service("com.example.explicit")
		.expect("Mach service");
	spec.set_network(NetworkMode::Outbound);
	let plan = compile(&spec);
	let profile = plan.profile().expect("Seatbelt profile");

	assert_ordered(profile, &[
		"(deny mach-lookup)",
		"(allow mach-lookup (global-name \"com.example.explicit\"))",
		"(allow mach-lookup (global-name \"com.apple.trustd\"))",
		"(allow mach-lookup (global-name \"com.apple.trustd.agent\"))",
		"(allow mach-lookup (global-name \"com.apple.SecurityServer\"))",
		"(deny network-inbound)",
	]);
	assert!(plan.enforced().contains(Capability::MachRestrict));
	assert!(plan.enforced().contains(Capability::NetOutbound));
}

#[test]
fn scoped_reads_preserve_loader_devices_and_rule_precedence() {
	let root = tempdir().expect("temporary root");
	let readable = root.path().join("readable");
	let denied = readable.join("secret");
	fs::create_dir(&readable).expect("readable directory");
	fs::write(&denied, b"secret").expect("denied file");
	let program = std::env::current_exe().expect("test executable");
	let mut spec = caveated_spec(&program);
	spec.allow_read(&readable).expect("read scope");
	spec.deny_read(&denied).expect("read denial");
	let plan = compile(&spec);
	let profile = plan.profile().expect("Seatbelt profile");
	let readable = sbpl_path(&fs::canonicalize(readable).expect("canonical readable"));
	let denied = sbpl_path(&fs::canonicalize(denied).expect("canonical denied"));

	assert_ordered(profile, &[
		"(deny file-read* (subpath \"/\"))",
		"(allow file-read* (subpath \"/usr/lib\") (subpath \"/System\") (subpath \
		 \"/private/var/db/dyld\"))",
		"(deny file-read* (subpath \"/System/Volumes/Data\"))",
		"(allow file-read-data (literal \"/\"))",
		"(allow file-read* (literal \"/dev/null\") (literal \"/dev/zero\") (literal \
		 \"/dev/random\") (literal \"/dev/urandom\"))",
		&format!("(subpath \"{readable}\")"),
		&format!("(deny file-read* (literal \"{denied}\"))"),
	]);
	assert!(plan.enforced().contains(Capability::FsReadScope));
	assert!(plan.enforced().contains(Capability::FsReadDeny));
}

#[test]
fn broad_reads_mask_raw_disk_and_kernel_memory_devices() {
	let program = std::env::current_exe().expect("test executable");
	let plan = compile(&caveated_spec(program));
	let profile = plan.profile().expect("Seatbelt profile");
	assert!(profile.contains("(deny file-read* (regex #\"^/dev/r?disk\"))"));
	assert!(profile.contains("(deny file-read* (regex #\"^/dev/(mem|kmem|kcore)$\"))"));
	assert!(!profile.contains("com.apple.trustd"));
	assert!(plan.enforced().contains(Capability::FsReadHost));
}

#[test]
fn ephemeral_profile_has_typed_read_and_write_replacements() {
	let workspace = tempdir().expect("workspace");
	let program = std::env::current_exe().expect("test executable");
	let mut spec = caveated_spec(&program);
	spec.set_dir(workspace.path()).expect("workspace cwd");
	spec.allow_read(workspace.path()).expect("workspace read");
	spec.set_write(WriteMode::Ephemeral);
	let plan = compile(&spec);
	let profile = plan.profile().expect("Seatbelt profile");

	assert_eq!(
		profile.matches("<omp-sandbox-ephemeral-root>").count(),
		2,
		"scoped reads and writes must both follow the prepared clone",
	);
	assert_eq!(plan.filesystem_virtualization(), Some(FilesystemVirtualizationKind::WorkspaceClone),);
	assert!(plan.enforced().contains(Capability::FsWriteEphemeral));
}

#[test]
fn overlay_is_scoped_deny_without_claiming_ephemeral_redirects() {
	let writable = tempdir().expect("writable scope");
	let program = std::env::current_exe().expect("test executable");
	let mut spec = caveated_spec(&program);
	spec.set_write(WriteMode::Overlay);
	spec.allow_write(writable.path()).expect("write scope");
	let plan = compile(&spec);

	assert!(plan.enforced().contains(Capability::FsWriteScope));
	assert!(!plan.enforced().contains(Capability::FsWriteEphemeral));
	assert_eq!(plan.filesystem_virtualization(), Some(FilesystemVirtualizationKind::ScopedDeny),);
	assert!(
		plan
			.caveats()
			.iter()
			.any(|caveat| caveat.capability == Some(Capability::FsWriteEphemeral)),
	);
}

#[test]
fn no_exec_and_resources_are_caveated_without_false_capabilities() {
	let program = std::env::current_exe().expect("test executable");
	let mut spec = caveated_spec(&program);
	spec.set_no_exec(true);
	spec.set_resource_limits(
		ResourceLimits::new(Some(0.5), Some(64 * 1024 * 1024), Some(4)).expect("limits"),
	);
	let plan = compile(&spec);
	let profile = plan.profile().expect("Seatbelt profile");

	assert_ordered(profile, &["(deny process-exec*)", "(allow process-exec* (literal "]);
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
}

fn assert_ordered(profile: &str, fragments: &[&str]) {
	let mut previous = 0;
	for fragment in fragments {
		let offset = profile[previous..]
			.find(fragment)
			.unwrap_or_else(|| panic!("missing profile fragment: {fragment}"));
		previous += offset + fragment.len();
	}
}

fn sbpl_path(path: &Path) -> String {
	path
		.as_os_str()
		.to_string_lossy()
		.replace('\\', "\\\\")
		.replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
mod live {
	use std::{
		fs::{self, File},
		net::{TcpListener, TcpStream},
		os::{
			fd::AsRawFd as _,
			unix::{
				fs::PermissionsExt as _,
				net::{UnixListener, UnixStream},
				process::CommandExt as _,
			},
		},
		path::PathBuf,
		process::{Command, Output},
	};

	use omp_sandbox::{Backend, DegradationPolicy, PreparedSandbox, Runner, SandboxSpec, WriteMode};
	use tempfile::tempdir;

	fn runner() -> Option<Runner> {
		let runner = Runner::for_backend(Backend::Seatbelt);
		omp_sandbox::backend_status(Backend::Seatbelt)
			.is_available()
			.then_some(runner)
	}

	fn probe_spec() -> SandboxSpec {
		let mut spec = SandboxSpec::new(std::env::current_exe().expect("test executable"));
		spec.args(["--exact", "live::seatbelt_probe_entry", "--ignored", "--nocapture"]);
		spec.set_degradation(DegradationPolicy::AllowCaveats);
		spec
	}

	struct LiveCommand {
		_prepared: PreparedSandbox,
		command:   Command,
	}

	impl LiveCommand {
		fn output(mut self) -> std::io::Result<Output> {
			self.command.output()
		}
	}

	fn command_for(runner: Runner, spec: &SandboxSpec, operation: &str) -> LiveCommand {
		let plan = runner.compile(spec).expect("compile live Seatbelt plan");
		let prepared = runner
			.prepare(plan, spec)
			.expect("prepare live Seatbelt plan");
		let mut command = prepared.command().expect("Seatbelt command");
		command.env("OMP_SEATBELT_PROBE", operation);
		LiveCommand { _prepared: prepared, command }
	}

	fn assert_success(output: &Output) {
		assert!(
			output.status.success(),
			"sandbox probe failed: status {:?}, stdout={}, stderr={}",
			output.status,
			String::from_utf8_lossy(&output.stdout),
			String::from_utf8_lossy(&output.stderr),
		);
	}

	#[test]
	fn scoped_writes_persist_and_outside_writes_fail() {
		let Some(runner) = runner() else { return };
		let root = tempdir().expect("temporary root");
		let allowed = root.path().join("allowed");
		let outside = root.path().join("outside");
		fs::create_dir(&allowed).expect("allowed directory");
		fs::create_dir(&outside).expect("outside directory");
		let mut spec = probe_spec();
		spec.set_write(WriteMode::Scoped);
		spec.allow_write(&allowed).expect("write scope");
		let mut command = command_for(runner, &spec, "scoped-write");
		command
			.command
			.env("OMP_ALLOWED", allowed.join("persisted"));
		command.command.env("OMP_DENIED", outside.join("blocked"));
		assert_success(&command.output().expect("run scoped-write probe"));
		assert_eq!(fs::read(allowed.join("persisted")).expect("persisted write"), b"written");
		assert!(!outside.join("blocked").exists());
	}

	#[test]
	fn write_deny_blocks_create_and_delete() {
		let Some(runner) = runner() else { return };
		let root = tempdir().expect("temporary root");
		let existing = root.path().join("existing");
		let created = root.path().join("created");
		fs::write(&existing, b"keep").expect("existing file");
		let spec = probe_spec();
		let mut command = command_for(runner, &spec, "write-deny");
		command
			.command
			.env("OMP_EXISTING", &existing)
			.env("OMP_DENIED", &created);
		assert_success(&command.output().expect("run write-deny probe"));
		assert_eq!(fs::read(existing).expect("original remains"), b"keep");
		assert!(!created.exists());
	}

	#[test]
	fn scoped_reads_block_an_outside_file() {
		let Some(runner) = runner() else { return };
		let allowed_root = tempdir().expect("allowed root");
		let denied_root = tempdir().expect("denied root");
		let allowed = allowed_root.path().join("allowed");
		let denied = denied_root.path().join("denied");
		fs::write(&allowed, b"allowed").expect("allowed file");
		fs::write(&denied, b"denied").expect("denied file");
		let mut spec = probe_spec();
		spec.allow_read(allowed_root.path()).expect("read scope");
		spec.set_dir(allowed_root.path()).expect("readable cwd");
		let mut command = command_for(runner, &spec, "scoped-read");
		command
			.command
			.env("OMP_ALLOWED", fs::canonicalize(&allowed).expect("canonical allowed file"))
			.env("OMP_DENIED", fs::canonicalize(&denied).expect("canonical denied file"));
		assert_success(&command.output().expect("run scoped-read probe"));
	}

	#[test]
	fn ephemeral_writes_are_visible_inside_but_not_persisted() {
		let Some(runner) = runner() else { return };
		let workspace = tempdir().expect("workspace");
		let original = workspace.path().join("original");
		fs::write(&original, b"host").expect("original file");
		fs::set_permissions(&original, fs::Permissions::from_mode(0o640)).expect("file mode");
		std::os::unix::fs::symlink("original", workspace.path().join("link"))
			.expect("workspace symlink");
		let mut spec = probe_spec();
		spec.set_dir(workspace.path()).expect("workspace cwd");
		spec.allow_read(workspace.path()).expect("workspace read");
		spec.set_write(WriteMode::Ephemeral);
		let command = command_for(runner, &spec, "ephemeral");
		assert!(!command._prepared.args().iter().any(|argument| {
			argument
				.to_string_lossy()
				.contains("<omp-sandbox-ephemeral-root>")
		}),);
		assert_ne!(
			command._prepared.cwd(),
			Some(
				fs::canonicalize(workspace.path())
					.expect("canonical workspace")
					.as_path()
			),
		);
		let clone = command._prepared.cwd().expect("prepared clone");
		assert_eq!(
			fs::metadata(clone.join("original"))
				.expect("cloned metadata")
				.permissions()
				.mode() & 0o777,
			0o640,
		);
		assert_eq!(
			fs::read_link(clone.join("link")).expect("cloned symlink"),
			PathBuf::from("original"),
		);
		assert_success(&command.output().expect("run ephemeral probe"));
		assert_eq!(fs::read(&original).expect("host original"), b"host");
		assert!(!workspace.path().join("created").exists());
	}

	#[test]
	fn denied_ip_allows_declared_unix_socket_and_inherited_fd() {
		let Some(runner) = runner() else { return };
		let root = tempdir().expect("socket root");
		let socket = root.path().join("service.sock");
		let _listener = UnixListener::bind(&socket).expect("Unix listener");
		let inherited = File::open("/dev/null").expect("inherited descriptor");
		let mut spec = probe_spec();
		spec.allow_unix_socket(&socket).expect("socket allowance");
		let mut command = command_for(runner, &spec, "socket-fd");
		command.command.env("OMP_SOCKET", &socket);
		let fd = inherited.as_raw_fd();
		unsafe {
			command.command.pre_exec(move || {
				if libc::dup2(fd, 9) == -1 {
					return Err(std::io::Error::last_os_error());
				}
				Ok(())
			});
		}
		assert_success(&command.output().expect("run socket/fd probe"));
	}

	#[test]
	fn disabled_network_rejects_a_live_tcp_listener() {
		let Some(runner) = runner() else { return };
		let listener = TcpListener::bind("127.0.0.1:0").expect("TCP listener");
		let spec = probe_spec();
		let mut command = command_for(runner, &spec, "tcp-denied");
		command
			.command
			.env("OMP_TCP", listener.local_addr().expect("listener address").to_string());
		assert_success(&command.output().expect("run TCP denial probe"));
	}

	#[test]
	fn libinfo_mach_allow_restores_username_lookup() {
		let Some(runner) = runner() else { return };
		let host = Command::new("/usr/bin/id")
			.arg("-un")
			.output()
			.expect("host username lookup");
		assert_success(&host);
		let host_name = String::from_utf8_lossy(&host.stdout).trim().to_owned();
		if host_name.parse::<u32>().is_ok() {
			return;
		}

		let mut denied = SandboxSpec::new("/usr/bin/id");
		denied
			.arg("-un")
			.set_degradation(DegradationPolicy::AllowCaveats);
		let denied = command_for(runner, &denied, "mach-denied")
			.output()
			.expect("run denied Mach lookup");
		assert_success(&denied);
		assert_ne!(String::from_utf8_lossy(&denied.stdout).trim(), host_name);

		let mut allowed = SandboxSpec::new("/usr/bin/id");
		allowed
			.arg("-un")
			.set_degradation(DegradationPolicy::AllowCaveats);
		allowed
			.allow_mach_service("com.apple.system.opendirectoryd.libinfo")
			.expect("libinfo Mach service");
		let allowed = command_for(runner, &allowed, "mach-libinfo")
			.output()
			.expect("run allowed Mach lookup");
		assert_success(&allowed);
		assert_eq!(String::from_utf8_lossy(&allowed.stdout).trim(), host_name);
	}

	#[test]
	#[ignore = "invoked as the confined probe by the live Seatbelt tests"]
	fn seatbelt_probe_entry() {
		match std::env::var("OMP_SEATBELT_PROBE")
			.expect("probe operation")
			.as_str()
		{
			"scoped-write" => {
				fs::write(required_path("OMP_ALLOWED"), b"written").expect("allowed write");
				assert!(fs::write(required_path("OMP_DENIED"), b"blocked").is_err());
			},
			"write-deny" => {
				assert!(fs::write(required_path("OMP_DENIED"), b"blocked").is_err());
				assert!(fs::remove_file(required_path("OMP_EXISTING")).is_err());
			},
			"scoped-read" => {
				assert_eq!(fs::read(required_path("OMP_ALLOWED")).expect("allowed read"), b"allowed");
				assert!(fs::read(required_path("OMP_DENIED")).is_err());
			},
			"ephemeral" => {
				assert_eq!(fs::read("original").expect("cloned original"), b"host");
				assert_eq!(
					fs::metadata("original")
						.expect("cloned metadata")
						.permissions()
						.mode() & 0o777,
					0o640,
				);
				assert_eq!(fs::read_link("link").expect("cloned symlink"), PathBuf::from("original"));
				fs::write("original", b"sandbox").expect("modify clone");
				fs::write("created", b"sandbox").expect("create in clone");
				assert_eq!(fs::read("original").expect("modified clone"), b"sandbox");
			},
			"socket-fd" => {
				UnixStream::connect(required_path("OMP_SOCKET")).expect("allowed Unix socket");
				assert_ne!(unsafe { libc::fcntl(9, libc::F_GETFD) }, -1, "fd 9 must be inherited");
			},
			"tcp-denied" => {
				let address = std::env::var("OMP_TCP").expect("TCP address");
				assert!(TcpStream::connect(address).is_err());
			},
			operation => panic!("unknown probe operation {operation}"),
		}
	}

	fn required_path(name: &str) -> PathBuf {
		let value = std::env::var_os(name).unwrap_or_else(|| panic!("missing {name}"));
		PathBuf::from(value)
	}
}
