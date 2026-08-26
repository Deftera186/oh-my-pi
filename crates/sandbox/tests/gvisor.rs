//! gVisor direct, OCI, filesystem, network, and live confinement contracts.

use std::{ffi::OsString, fs, path::Path};

use omp_sandbox::{
	Backend, Capability, DegradationPolicy, EnvironmentSource, FilesystemVirtualizationKind,
	NetworkMode, OutputMode, ResourceLimits, RunOptions, Runner, SandboxSpec, WriteMode,
};
use serde_json::Value;
use tempfile::tempdir;

#[test]
fn simple_plan_uses_do_with_a_mandatory_terminator() {
	let plan = compile(SandboxSpec::new(executable()));
	let argv = plan.argv();
	let do_index = argv.iter().position(|arg| arg == "do").expect("runsc do");
	assert_eq!(argv[do_index + 1], "--");
	assert_eq!(
		Path::new(&argv[do_index + 2]),
		fs::canonicalize(executable()).expect("resolved executable"),
	);
	assert!(argv[..do_index].iter().any(|arg| arg == "--network=none"));
	assert!(plan.profile().is_none());
	assert_enforced_subset(&plan);
}

#[test]
fn command_arguments_that_look_like_runsc_flags_stay_after_the_terminator() {
	let mut spec = SandboxSpec::new(executable());
	spec.args(["--network=host", "--force-overlay=false"]);
	let plan = compile(spec);
	let separator = plan
		.argv()
		.iter()
		.position(|arg| arg == "--")
		.expect("terminator");
	assert_eq!(plan.argv()[separator + 2], "--network=host");
	assert_eq!(plan.argv()[separator + 3], "--force-overlay=false");
}

#[test]
fn ephemeral_broad_view_remains_direct_and_uses_memory_overlay() {
	let mut spec = SandboxSpec::new(executable());
	spec.set_write(WriteMode::Ephemeral);
	let plan = compile(spec);
	assert!(plan.argv().iter().any(|arg| arg == "do"));
	assert!(plan.argv().iter().any(|arg| arg == "--overlay2=all:memory"));
	assert_eq!(plan.filesystem_virtualization(), Some(FilesystemVirtualizationKind::MemoryOverlay),);
	assert!(plan.enforced().contains(Capability::FsWriteEphemeral));
	assert_enforced_subset(&plan);
}

#[test]
fn outbound_forces_oci_seccomp_and_owned_network_namespace() {
	let mut spec = SandboxSpec::new(executable());
	spec.set_network(NetworkMode::Outbound);
	let plan = compile(spec);
	assert_oci_argv(&plan);
	assert!(plan.argv().iter().any(|arg| arg == "--oci-seccomp"));
	let json = profile_json(&plan);
	assert_eq!(json["ociVersion"], "1.0.2");
	assert_eq!(json["linux"]["namespaces"][4]["type"], "network");
	assert_eq!(json["linux"]["namespaces"][4]["path"], "<omp:gvisor-netns>");
	assert_eq!(
		json["linux"]["seccomp"]["syscalls"][0]["names"],
		serde_json::json!(["listen", "accept", "accept4"]),
	);
	assert_eq!(json["linux"]["seccomp"]["syscalls"][0]["errnoRet"], 1);
	assert!(plan.enforced().contains(Capability::NetOutbound));
	assert_enforced_subset(&plan);
}

#[test]
fn no_exec_forces_oci_and_denies_both_exec_syscalls() {
	let mut spec = SandboxSpec::new(executable());
	spec.set_no_exec(true);
	let plan = compile(spec);
	assert_oci_argv(&plan);
	let json = profile_json(&plan);
	assert_eq!(
		json["linux"]["seccomp"]["syscalls"][0]["names"],
		serde_json::json!(["execve", "execveat"]),
	);
	assert!(plan.enforced().contains(Capability::ProcNoExec));
	assert_enforced_subset(&plan);
}

#[test]
fn exact_resource_math_is_encoded_in_oci_linux_resources() {
	let mut spec = SandboxSpec::new(executable());
	spec.set_resource_limits(
		ResourceLimits::new(Some(0.333_335), Some(67_108_864), Some(41)).expect("limits"),
	);
	let plan = compile(spec);
	assert_oci_argv(&plan);
	let json = profile_json(&plan);
	assert_eq!(json["linux"]["resources"]["cpu"]["period"], 100_000);
	assert_eq!(json["linux"]["resources"]["cpu"]["quota"], 33_334);
	assert_eq!(json["linux"]["resources"]["memory"]["limit"], 67_108_864);
	assert_eq!(json["linux"]["resources"]["memory"]["swap"], 67_108_864);
	assert_eq!(json["linux"]["resources"]["pids"]["limit"], 41);
	assert_enforced_subset(&plan);
}
#[test]
fn tiny_cpu_quota_has_a_one_microsecond_floor() {
	let mut spec = SandboxSpec::new(executable());
	spec.set_resource_limits(ResourceLimits::new(Some(0.000_001), None, None).expect("limits"));
	let json = profile_json(&compile(spec));
	assert_eq!(json["linux"]["resources"]["cpu"]["quota"], 1);
}

#[test]
fn unrepresentable_oci_memory_limit_is_typed() {
	let mut spec = SandboxSpec::new(executable());
	spec.set_resource_limits(ResourceLimits::new(None, Some(u64::MAX), None).expect("limits"));
	let error = Runner::for_backend(Backend::Gvisor)
		.compile(&spec)
		.expect_err("signed OCI memory overflow");
	assert!(matches!(error, omp_sandbox::SandboxError::InvalidResourceLimit {
		resource: omp_sandbox::ResourceKind::Memory,
		value:    u64::MAX,
	}));
}

#[test]
fn oci_contract_has_readonly_root_empty_capabilities_and_no_new_privileges() {
	let mut spec = SandboxSpec::new(executable());
	spec.set_no_exec(true);
	let json = profile_json(&compile(spec));
	assert_eq!(json["root"]["readonly"], true);
	assert_eq!(json["process"]["noNewPrivileges"], true);
	assert_eq!(json["process"]["capabilities"], serde_json::json!({}));
	assert_eq!(json["mounts"][0]["destination"], "/proc");
	let kinds = json["linux"]["namespaces"]
		.as_array()
		.expect("namespace array")
		.iter()
		.map(|namespace| namespace["type"].as_str().expect("namespace type"))
		.collect::<Vec<_>>();
	assert_eq!(kinds, ["pid", "mount", "ipc", "uts", "network"]);
}

#[test]
fn scoped_filesystem_uses_placeholder_root_and_drops_ipc_claim() {
	let directory = tempdir().expect("temporary readable directory");
	let mut spec = SandboxSpec::new(executable());
	spec.allow_read(directory.path()).expect("read scope");
	spec.set_degradation(DegradationPolicy::AllowCaveats);
	let plan = compile(spec);
	assert_oci_argv(&plan);
	let json = profile_json(&plan);
	assert_eq!(json["root"]["path"], "<omp:gvisor-rootfs>");
	assert_eq!(json["root"]["readonly"], true);
	assert!(plan.enforced().contains(Capability::FsReadScope));
	assert!(!plan.enforced().contains(Capability::IpcRestrict));
	assert!(
		plan
			.caveats()
			.iter()
			.any(|caveat| caveat.capability == Some(Capability::IpcRestrict))
	);
	assert_enforced_subset(&plan);
}
#[test]
fn strict_scoped_filesystem_rejects_the_missing_ipc_guarantee() {
	let directory = tempdir().expect("temporary readable directory");
	let mut spec = SandboxSpec::new(executable());
	spec.allow_read(directory.path()).expect("read scope");
	let error = Runner::for_backend(Backend::Gvisor)
		.compile(&spec)
		.expect_err("strict scope must reject conditional IPC exposure");
	match error {
		omp_sandbox::SandboxError::BackendCapabilities { backend, missing } => {
			assert_eq!(backend, Backend::Gvisor);
			assert_eq!(missing.iter().collect::<Vec<_>>(), [Capability::IpcRestrict]);
		},
		other => panic!("unexpected error: {other:?}"),
	}
}

#[test]
fn overlay_forces_oci_and_records_root_memory_virtualization() {
	let writable = tempdir().expect("temporary writable directory");
	let mut spec = SandboxSpec::new(executable());
	spec.set_write(WriteMode::Overlay);
	spec.allow_write(writable.path()).expect("write scope");
	spec.set_degradation(DegradationPolicy::AllowCaveats);
	let plan = compile(spec);
	assert_oci_argv(&plan);
	assert!(
		plan
			.argv()
			.iter()
			.any(|arg| arg == "--overlay2=root:memory")
	);
	assert_eq!(plan.filesystem_virtualization(), Some(FilesystemVirtualizationKind::RootOverlay),);
	assert!(plan.enforced().contains(Capability::FsWriteScope));
	assert!(plan.enforced().contains(Capability::FsWriteEphemeral));
	assert_enforced_subset(&plan);
}

#[test]
fn read_deny_forces_oci_and_has_a_capability_specific_caveat() {
	let root = tempdir().expect("temporary root");
	let denied = root.path().join("secret");
	std::fs::write(&denied, b"secret").expect("denied fixture");
	let mut spec = SandboxSpec::new(executable());
	spec.deny_read(&denied).expect("deny path");
	spec.set_degradation(DegradationPolicy::AllowCaveats);
	let plan = compile(spec);
	assert_oci_argv(&plan);
	assert!(plan.enforced().contains(Capability::FsReadDeny));
	assert!(
		plan
			.caveats()
			.iter()
			.any(|caveat| caveat.capability == Some(Capability::FsReadDeny))
	);
	assert_enforced_subset(&plan);
}

#[test]
fn future_read_deny_is_caveated_without_false_enforcement() {
	let root = tempdir().expect("temporary root");
	let denied = root.path().join("future-secret");
	let mut spec = SandboxSpec::new(executable());
	spec.deny_read(&denied).expect("future deny path");
	spec.set_degradation(DegradationPolicy::AllowCaveats);
	let plan = compile(spec);
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
fn compiled_oci_plan_is_deterministic_and_environment_secret_free() {
	let mut spec = SandboxSpec::new(executable());
	spec.set_no_exec(true);
	spec.set_environment(EnvironmentSource::Exact(vec![OsString::from(
		"OMP_GVISOR_SECRET=highly-sensitive-value",
	)]));
	let first = compile(spec.clone());
	let second = compile(spec);
	assert_eq!(first.argv(), second.argv());
	assert_eq!(first.profile(), second.profile());
	assert!(!format!("{:?}", first.argv()).contains("highly-sensitive-value"));
	assert!(
		!first
			.profile()
			.expect("OCI profile")
			.contains("highly-sensitive-value")
	);
	assert!(profile_json(&first)["process"].get("env").is_none());
}

#[test]
fn stable_container_id_changes_when_an_observable_field_changes() {
	let mut first_spec = SandboxSpec::new(executable());
	first_spec.set_no_exec(true);
	let mut second_spec = first_spec.clone();
	second_spec.arg("different");
	let first = compile(first_spec);
	let second = compile(second_spec);
	assert_ne!(first.argv().last(), second.argv().last());
	assert!(
		first
			.argv()
			.last()
			.expect("container id")
			.to_string_lossy()
			.starts_with("omp-sandbox-gvisor-")
	);
}

#[tokio::test]
async fn opt_in_live_runsc_enforces_no_exec() {
	if std::env::var_os("OMP_SANDBOX_GVISOR_E2E").is_none() {
		return;
	}
	let mut spec = SandboxSpec::new("/bin/sh");
	spec.args(["-c", "/bin/true; test $? -ne 0"]);
	spec.set_no_exec(true);
	let output = Runner::for_backend(Backend::Gvisor)
		.run(&spec, RunOptions {
			stdout: OutputMode::Capture,
			stderr: OutputMode::Capture,
			..RunOptions::default()
		})
		.await
		.expect("gVisor no-exec live run");
	assert_eq!(
		output.exit.code,
		Some(0),
		"stderr={}",
		String::from_utf8_lossy(output.stderr.as_ref())
	);
}
#[tokio::test]
async fn opt_in_live_runsc_allows_connect_but_denies_listen() {
	if std::env::var_os("OMP_SANDBOX_GVISOR_E2E").is_none() {
		return;
	}
	let script = r#"
import socket
with socket.create_connection(("1.1.1.1", 53), timeout=5):
    pass
listener = socket.socket()
try:
    listener.bind(("0.0.0.0", 0))
    listener.listen(1)
except PermissionError:
    raise SystemExit(0)
raise SystemExit(70)
"#;
	let mut spec = SandboxSpec::new("python3");
	spec.args(["-c", script]);
	spec.set_network(NetworkMode::Outbound);
	let output = Runner::for_backend(Backend::Gvisor)
		.run(&spec, RunOptions {
			stdout: OutputMode::Capture,
			stderr: OutputMode::Capture,
			..RunOptions::default()
		})
		.await
		.expect("gVisor outbound live run");
	assert_eq!(
		output.exit.code,
		Some(0),
		"stderr={}",
		String::from_utf8_lossy(output.stderr.as_ref())
	);
}

#[tokio::test]
async fn opt_in_live_runsc_overlay_persists_only_declared_bind() {
	if std::env::var_os("OMP_SANDBOX_GVISOR_E2E").is_none() {
		return;
	}
	let root = tempdir().expect("overlay host root");
	let persistent = root.path().join("persistent");
	let outside = root.path().join("outside");
	fs::create_dir(&persistent).expect("persistent directory");
	fs::create_dir(&outside).expect("outside directory");
	let persist_file = persistent.join("kept");
	let outside_file = outside.join("discarded");
	let mut spec = SandboxSpec::new("/bin/sh");
	spec.args([
		OsString::from("-c"),
		OsString::from(format!(
			"printf kept > '{}'; printf discarded > '{}'",
			persist_file.display(),
			outside_file.display(),
		)),
	]);
	spec.set_write(WriteMode::Overlay);
	spec
		.allow_write(&persistent)
		.expect("persistent write bind");
	spec.set_degradation(DegradationPolicy::AllowCaveats);
	let output = Runner::for_backend(Backend::Gvisor)
		.run(&spec, RunOptions::default())
		.await
		.expect("gVisor overlay live run");
	assert_eq!(output.exit.code, Some(0));
	assert_eq!(fs::read_to_string(persist_file).expect("persistent content"), "kept");
	assert!(!outside_file.exists(), "outside overlay write reached the host");
}
#[tokio::test]
async fn opt_in_live_runsc_ephemeral_leaves_workspace_unchanged() {
	if std::env::var_os("OMP_SANDBOX_GVISOR_E2E").is_none() {
		return;
	}
	let workspace = tempdir().expect("ephemeral workspace");
	let output_path = workspace.path().join("ephemeral-output");
	let mut spec = SandboxSpec::new("/bin/sh");
	spec.args(["-c", "printf transient > ephemeral-output"]);
	spec.set_dir(workspace.path()).expect("workspace cwd");
	spec.set_write(WriteMode::Ephemeral);
	let output = Runner::for_backend(Backend::Gvisor)
		.run(&spec, RunOptions::default())
		.await
		.expect("gVisor ephemeral live run");
	assert_eq!(output.exit.code, Some(0));
	assert!(!output_path.exists(), "ephemeral write reached the host workspace");
}

fn compile(spec: SandboxSpec) -> omp_sandbox::Plan {
	Runner::for_backend(Backend::Gvisor)
		.compile(&spec)
		.expect("gVisor plan")
}

fn executable() -> &'static Path {
	Path::new("/bin/echo")
}

fn profile_json(plan: &omp_sandbox::Plan) -> Value {
	serde_json::from_str(plan.profile().expect("OCI profile")).expect("valid OCI JSON")
}

fn assert_oci_argv(plan: &omp_sandbox::Plan) {
	let argv = plan.argv();
	let run = argv.iter().position(|arg| arg == "run").expect("runsc run");
	assert_eq!(argv[run + 1], "--bundle");
	assert_eq!(argv[run + 2], "<omp:gvisor-bundle>");
	assert!(
		argv[run + 3]
			.to_string_lossy()
			.starts_with("omp-sandbox-gvisor-")
	);
	assert!(!argv.iter().any(|arg| arg == "do"));
}

fn assert_enforced_subset(plan: &omp_sandbox::Plan) {
	assert!(
		plan
			.enforced()
			.difference(plan.backend().capabilities())
			.is_empty()
	);
}
