#[cfg(target_os = "linux")]
use std::process::Command;
use std::{
	env,
	ffi::OsString,
	fs,
	path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
use omp_core::CowBytes;

#[cfg(target_os = "linux")]
use crate::SandboxOperation;
use crate::{
	Backend, BackendStatus, Capability, CapabilitySet, Caveat, DegradationPolicy,
	FilesystemVirtualizationKind, NetworkMode, Plan, ProbeFailure, SandboxError, SandboxSpec,
	WriteMode, paths::path_under_any,
};

pub(crate) const FILE_MASK_PLACEHOLDER: &str = "@omp-bwrap-file-mask@";
pub(crate) const DIRECTORY_MASK_PLACEHOLDER: &str = "@omp-bwrap-directory-mask@";

const LAUNCHERS: [&str; 2] = ["/usr/bin/bwrap", "/bin/bwrap"];

pub(crate) fn compile(
	spec: &SandboxSpec,
	program: &Path,
	requested: CapabilitySet,
	mut enforced: CapabilitySet,
) -> Result<Plan, SandboxError> {
	let mut unavailable = requested.difference(Backend::Bubblewrap.capabilities());
	let has_future_deny = spec.read_deny.iter().any(|path| !path.exists());
	if has_future_deny {
		unavailable = unavailable.union(CapabilitySet::one(Capability::FsReadDeny));
		enforced = enforced.difference(CapabilitySet::one(Capability::FsReadDeny));
	}
	if spec.degradation == DegradationPolicy::Reject && !unavailable.is_empty() {
		return Err(SandboxError::BackendCapabilities {
			backend: Backend::Bubblewrap,
			missing: unavailable,
		});
	}

	if !spec.unix_sockets.is_empty() {
		enforced = enforced.difference(CapabilitySet::one(Capability::IpcRestrict));
	}
	if spec.degradation == DegradationPolicy::AllowCaveats {
		if spec.network == NetworkMode::Outbound {
			enforced = enforced.union(CapabilitySet::one(Capability::NetDisable));
		}
		if spec.write == WriteMode::Ephemeral {
			enforced = enforced.union(CapabilitySet::one(Capability::FsWriteDeny));
		}
	}

	let launcher = launcher();
	let mut argv = vec![
		launcher,
		OsString::from("--die-with-parent"),
		OsString::from("--new-session"),
		OsString::from("--unshare-all"),
		OsString::from("--preserve-fds"),
		OsString::from("1"),
	];
	if spec.network == NetworkMode::Enabled {
		argv.push(OsString::from("--share-net"));
	}

	if spec.readable.is_empty() {
		push_bind(&mut argv, "--ro-bind", Path::new("/"));
	} else {
		argv.extend([OsString::from("--tmpfs"), OsString::from("/")]);
		let mut readable = runtime_closure(program);
		readable.extend(spec.readable.iter().cloned());
		readable.sort();
		readable.dedup();
		for path in readable {
			if !path_under_any(&path, &spec.writable) {
				push_bind(&mut argv, "--ro-bind", &path);
			}
		}
	}

	argv.extend([OsString::from("--proc"), OsString::from("/proc")]);
	argv.extend([OsString::from("--dev"), OsString::from("/dev")]);
	if spec.allow_temp {
		argv.extend([OsString::from("--tmpfs"), OsString::from("/tmp")]);
	}
	if matches!(spec.write, WriteMode::Scoped | WriteMode::Overlay) {
		for path in &spec.writable {
			push_bind(&mut argv, "--bind", path);
		}
	}

	for path in spec.read_deny.iter().filter(|path| path.exists()) {
		let placeholder = if fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
			DIRECTORY_MASK_PLACEHOLDER
		} else {
			FILE_MASK_PLACEHOLDER
		};
		argv.extend([
			OsString::from("--ro-bind"),
			OsString::from(placeholder),
			path.as_os_str().to_owned(),
		]);
	}
	for socket in &spec.unix_sockets {
		push_bind(&mut argv, "--bind", socket);
	}

	argv.push(OsString::from("--"));
	argv.push(program.as_os_str().to_owned());
	argv.extend(spec.args.iter().cloned());

	let mut plan = Plan::new(Backend::Bubblewrap, requested, enforced, argv, true);
	if spec.degradation == DegradationPolicy::AllowCaveats {
		add_degradation_caveats(&mut plan, spec, unavailable, has_future_deny);
	}
	if spec.write == WriteMode::Overlay {
		plan.set_filesystem(FilesystemVirtualizationKind::ScopedDeny);
	}
	Ok(plan)
}

fn launcher() -> OsString {
	env::var_os("OMP_SANDBOX_BWRAP")
		.or_else(|| {
			LAUNCHERS
				.into_iter()
				.map(Path::new)
				.find(|path| path.is_file())
				.map(Path::as_os_str)
				.map(OsString::from)
		})
		.unwrap_or_else(|| OsString::from(LAUNCHERS[0]))
}

fn runtime_closure(program: &Path) -> Vec<PathBuf> {
	let mut paths = vec![program.to_path_buf()];
	for candidate in [
		"/etc/ld.so.cache",
		"/etc/ld.so.conf",
		"/etc/ld.so.conf.d",
		"/etc/ld.so.preload",
		"/lib",
		"/lib32",
		"/lib64",
		"/libx32",
		"/usr/lib",
		"/usr/lib32",
		"/usr/lib64",
		"/usr/libx32",
	] {
		let candidate = PathBuf::from(candidate);
		if candidate.exists() {
			paths.push(candidate);
		}
	}
	paths.sort();
	paths.dedup();
	paths
}

fn add_degradation_caveats(
	plan: &mut Plan,
	spec: &SandboxSpec,
	unavailable: CapabilitySet,
	has_future_deny: bool,
) {
	for capability in unavailable.iter() {
		let message = match capability {
			Capability::NetOutbound => {
				"Bubblewrap narrows outbound-only networking to a disabled network namespace"
			},
			Capability::FsWriteEphemeral if spec.write == WriteMode::Ephemeral => {
				"Bubblewrap narrows ephemeral writes to write denial"
			},
			Capability::FsWriteEphemeral => {
				"Bubblewrap persists configured scopes and denies writes elsewhere"
			},
			Capability::ProcNoExec => "Bubblewrap cannot prevent subsequent program execution",
			Capability::ResCpu => "Bubblewrap does not enforce CPU limits",
			Capability::ResMemory => "Bubblewrap does not enforce memory limits",
			Capability::ResPids => "Bubblewrap does not enforce process-count limits",
			Capability::FsReadDeny if has_future_deny => {
				"Bubblewrap masks existing read-deny paths but cannot mask a future path"
			},
			_ => "Bubblewrap cannot enforce this requested capability",
		};
		plan.add_caveat(Caveat::capability(capability, message));
	}
}

pub(crate) fn probe() -> BackendStatus {
	#[cfg(not(target_os = "linux"))]
	{
		return BackendStatus::unavailable(Backend::Bubblewrap, ProbeFailure::WrongHost {
			backend: Backend::Bubblewrap,
			os:      std::env::consts::OS,
		});
	}
	#[cfg(target_os = "linux")]
	{
		let launcher = launcher();
		let output = match Command::new(&launcher)
			.args([
				"--die-with-parent",
				"--new-session",
				"--unshare-all",
				"--ro-bind",
				"/",
				"/",
				"--",
				"/bin/true",
			])
			.output()
		{
			Ok(output) => output,
			Err(source) => {
				return BackendStatus::unavailable(Backend::Bubblewrap, ProbeFailure::Start {
					backend: Backend::Bubblewrap,
					operation: SandboxOperation::Probe,
					source,
				});
			},
		};
		if output.status.success() {
			BackendStatus::available(Backend::Bubblewrap)
		} else {
			let mut diagnostic = output.stderr;
			diagnostic.truncate(4096);
			BackendStatus::unavailable(Backend::Bubblewrap, ProbeFailure::Rejected {
				backend:    Backend::Bubblewrap,
				operation:  SandboxOperation::Probe,
				status:     output.status.code(),
				diagnostic: CowBytes::from(diagnostic),
			})
		}
	}
}

fn push_bind(argv: &mut Vec<OsString>, option: &str, path: &Path) {
	argv.push(OsString::from(option));
	argv.push(path.as_os_str().to_owned());
	argv.push(path.as_os_str().to_owned());
}
