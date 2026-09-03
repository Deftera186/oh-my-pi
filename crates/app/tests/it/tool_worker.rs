//! Worker process contract tests.

use std::{
	collections::BTreeMap,
	fs,
	io::Write as _,
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::Arc,
	time::{Duration, Instant},
};

use bytes::Bytes;
use nix::{errno::Errno, sys::signal, unistd::Pid};
use omp_core::{
	ArtifactDigest, Duration as CoreDuration, DurationUnit, Principal, Provenance, Str, sf,
};
use omp_envd::{
	DeviceCatalogObserver, DeviceControlFactory, DeviceInvocationAdmission,
	DynamicDeviceCatalogEntry, RegistryControlFactory,
	exthost::{
		ActivationTrigger, DeclarationSet, ExtensionManifest, ServiceManifest, ToolDeclarationKey,
		control::{
			ControlAuthority, ControlAuthorityFactory, ControlEffect, ControlError,
			ControlProtocolError, ControlRequestContext, EnvdControlAuthorities,
			ExternalControlAuthorities, FixedControlAuthorityFactory, HostControlAuthorityFactory,
			HostRequestMap, PersistenceControlAuthorities, PolicyControlAuthorities,
			PresentationControlAuthorities, ProviderControlAuthorities, RegistryControlAuthorities,
		},
		dispatch::CallbackDispatcherSlot,
	},
	worker::{
		ExtHostConfig, ExtHostSpec, ExtHostSupervisor, HostKey, OpenToolCall, PY_EVAL_MODULE,
		WorkerAbortKind, WorkerCompletion, WorkerError, WorkerEvent, WorkerInvocation,
		WorkerOutcomeKind,
	},
};
use omp_ext::config::{StaticDeclaration, StaticDeclarations};
use omp_proto::{
	env::v1::{ArgText, ArgsCommitted, Interrupt, InterruptClass},
	inference::v1::tool_def,
	prost::Message as _,
	thread::v1::{Blob, Part, part},
	toolhost::v1::{
		AdmitExtensions, AdmittedExtension, ArgIssue, HostFrame, LifecycleHostEnvelope, OutcomeKind,
		PullReply, PullRequest, ToolComplete, host_frame, lifecycle_host_envelope,
	},
};
use serde_json::{Value, json};
use tokio::time;

const EXTENSION: &str = r#"
import ctypes
import os
import signal

# The supervisor's SIGINT is explicitly a courtesy. Ignore it so the native
# sleep proves cancellation reaches the grace deadline and requires SIGKILL.
signal.signal(signal.SIGINT, signal.SIG_IGN)

_libc = ctypes.CDLL(None)
_sleep = _libc.sleep
_sleep.argtypes = [ctypes.c_uint]
_sleep.restype = ctypes.c_uint


def echo_update(params):
    if set(params) != {"message", "commit_seal"} or params["commit_seal"] != "committed":
        raise RuntimeError("tool observed arguments before commitment")
    result = {
        "message": params["message"],
        "commit_seal": params["commit_seal"],
        "pid": os.getpid(),
    }
    return {
        "updates": [result],
        "parts": [params["message"]],
        "details": result,
    }


def native_block(params):
    with open(params["started"], "w", encoding="utf-8") as marker:
        marker.write(str(os.getpid()))
        marker.flush()
    _sleep(params["seconds"])
    return {"parts": ["native sleep returned"], "details": {"pid": os.getpid()}}


def reject_args(_params):
    return {
        "args_issue": {
            "path": ["count"],
            "expected": "integer",
            "kind": "type",
            "example": "3",
            "found": "string",
        }
    }


OMP_TOOLS = [
    {
        "name": "echo_update",
        "description": "echoes one committed invocation and emits an update",
        "schema": {
            "type": "object",
            "properties": {
                "message": {"type": "string"},
                "commit_seal": {"const": "committed"},
            },
            "required": ["message", "commit_seal"],
            "additionalProperties": False,
        },
        "rev": "1",
        "strict": True,
        "handler": echo_update,
    },
    {
        "name": "reject_args",
        "description": "returns one structured argument rejection",
        "schema": {
            "type": "object",
            "properties": {},
            "additionalProperties": False,
        },
        "rev": "1",
        "strict": True,
        "handler": reject_args,
    },
    {
        "name": "native_block",
        "description": "blocks in the platform C sleep function",
        "schema": {
            "type": "object",
            "properties": {
                "started": {"type": "string"},
                "seconds": {"type": "integer"},
            },
            "required": ["started", "seconds"],
            "additionalProperties": False,
        },
        "rev": "1",
        "strict": True,
        "handler": native_block,
    },
]

"#;

const SIBLING_EXTENSION: &str = r#"
import os

def stable_echo(params):
    return {
        "parts": [params["message"]],
        "details": {
            "message": params["message"],
            "pid": os.getpid(),
            "env_socket": os.environ.get("OMP_EXT_ENV_SOCKET"),
        },
    }

OMP_TOOLS = [{
    "name": "stable_echo",
    "description": "proves another extension process survives",
    "schema": {
        "type": "object",
        "properties": {"message": {"type": "string"}},
        "required": ["message"],
        "additionalProperties": False,
    },
    "rev": "1",
    "strict": True,
    "handler": stable_echo,
}]
"#;

#[test]
fn completion_preserves_all_outcome_branches_and_presence_rules() {
	let legacy = WorkerCompletion::try_from(ToolComplete {
		call_id: "legacy".into(),
		details_json: Bytes::from_static(b"{}"),
		is_error: true,
		..Default::default()
	})
	.expect("legacy completion");
	assert_eq!(legacy.kind, WorkerOutcomeKind::Faulted);

	let rejected = WorkerCompletion::try_from(ToolComplete {
		call_id: "args".into(),
		details_json: Bytes::from_static(b"null"),
		kind: OutcomeKind::ArgsRejected.into(),
		args_issue: Some(ArgIssue {
			path: vec!["count".into()],
			expected: "integer".into(),
			kind: "type".into(),
			..Default::default()
		}),
		..Default::default()
	})
	.expect("structured argument rejection");
	assert_eq!(rejected.kind, WorkerOutcomeKind::ArgsRejected);
	assert!(rejected.args_issue.is_some());

	let aborted = WorkerCompletion::try_from(ToolComplete {
		call_id: "aborted".into(),
		kind: OutcomeKind::Aborted.into(),
		details_blob: Some(Blob {
			hash: Bytes::from_static(&[7; 32]),
			mime: "application/json".into(),
			size: 2,
			..Default::default()
		}),
		..Default::default()
	})
	.expect("spilled abort");
	assert_eq!(aborted.kind, WorkerOutcomeKind::Aborted);
	assert!(aborted.details_json.is_none());
	assert!(aborted.details_blob.is_some());

	assert!(
		WorkerCompletion::try_from(ToolComplete {
			call_id: "xor".into(),
			details_json: Bytes::from_static(b"{}"),
			details_blob: Some(Blob::default()),
			..Default::default()
		})
		.is_err()
	);
	assert!(
		WorkerCompletion::try_from(ToolComplete {
			call_id: "part".into(),
			parts: vec![Part { kind: None }],
			details_json: Bytes::from_static(b"{}"),
			..Default::default()
		})
		.is_err()
	);
}

#[test]
fn control_mapping_fences_stale_frames_and_one_pull_slot() {
	let mut requests = HostRequestMap::new();
	let ordinary = requests
		.open(sf!("ordinary"), sf!("ordinary-call"), false)
		.expect("map ordinary invocation");
	assert_eq!(ordinary.request_id, 1);
	assert_eq!(
		requests.arg_text(&ArgText {
			invocation_id: "ordinary".into(),
			fragment:      "{}".into(),
			props:         None,
		}),
		Err(ControlError::StreamingNotDeclared),
	);
	assert!(matches!(
		requests.arg_text(&ArgText {
			invocation_id: "stale".into(),
			fragment:      "{}".into(),
			props:         None,
		}),
		Err(ControlError::UnknownInvocation(_)),
	));

	let streaming = requests
		.open(sf!("stream"), sf!("stream-call"), true)
		.expect("map streaming invocation");
	let pull = PullRequest {
		call_id:     "stream-call".into(),
		path:        vec!["payload".into()],
		key:         Some("payload".into()),
		aliases:     vec!["data".into()],
		expected:    Some(Bytes::from_static(b"string")),
		chunk_bytes: 4096,
		props:       None,
	};
	requests
		.begin_pull(streaming.request_id, &pull)
		.expect("take pull slot");
	assert_eq!(requests.begin_pull(streaming.request_id, &pull), Err(ControlError::PullBusy),);
	assert!(
		!requests
			.accept_pull_reply(streaming.request_id, &PullReply {
				call_id:  "stream-call".into(),
				chunk:    Bytes::from_static(b"part"),
				complete: false,
				issue:    None,
				props:    None,
			},)
			.expect("accept streamed pull reply"),
	);
	assert_eq!(requests.begin_pull(streaming.request_id, &pull), Err(ControlError::PullBusy),);
	assert!(
		requests
			.accept_pull_reply(streaming.request_id, &PullReply {
				call_id:  "stream-call".into(),
				chunk:    Bytes::new(),
				complete: true,
				issue:    None,
				props:    None,
			},)
			.expect("fuse streamed pull reply"),
	);
	requests
		.begin_pull(streaming.request_id, &pull)
		.expect("terminal reply released pull slot");
	requests
		.fuse(streaming.request_id, "stream-call")
		.expect("fuse invocation");
	assert_eq!(
		requests.request(streaming.request_id),
		Err(ControlError::StaleRequest(streaming.request_id)),
	);
}
#[test]
fn worker_connection_rejects_nested_counts_before_decode() {
	let mut child = Command::new(env!("CARGO_BIN_EXE_omp"))
		.arg(omp_envd::worker::WORKER_ARG)
		.env_remove("OMP_PY_MODULES")
		.env("OMP_EXT_LAYER", "workspace")
		.env("OMP_EXT_TIER", "trusted")
		.env("OMP_EXT_HOST_GENERATION", "1")
		.env("OMP_EXT_SESSION_GENERATION", "1")
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.expect("spawn bounded-frame worker");
	let frame = HostFrame {
		request_id: 1,
		body:       Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
			body:  Some(lifecycle_host_envelope::Body::AdmitExtensions(AdmitExtensions {
				extensions: vec![AdmittedExtension::default(); 1_025],
				generation: 1,
				props:      None,
			})),
			props: None,
		})),
		props:      None,
	};
	let mut encoded = Vec::new();
	frame
		.encode_length_delimited(&mut encoded)
		.expect("encode over-count host frame");
	child
		.stdin
		.take()
		.expect("worker stdin")
		.write_all(&encoded)
		.expect("write over-count frame");
	let output = child
		.wait_with_output()
		.expect("wait for bounded-frame rejection");
	assert!(!output.status.success(), "worker accepted an over-count host frame");
}

#[tokio::test]
async fn supervisor_rejects_stale_host_generation() {
	use std::os::unix::fs::PermissionsExt as _;

	let scratch = tempfile::tempdir().expect("stale generation scratch");
	let wrapper = scratch.path().join("stale-worker");
	let executable = env!("CARGO_BIN_EXE_omp").replace('\'', "'\"'\"'");
	fs::write(
		&wrapper,
		format!("#!/bin/sh\nexport OMP_EXT_HOST_GENERATION=0\nexec '{executable}' \"$@\"\n"),
	)
	.expect("write stale-generation wrapper");
	fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))
		.expect("make stale-generation wrapper executable");
	let mut config = test_config(wrapper);
	let key = HostKey::new("workspace", "trusted", PY_EVAL_MODULE);
	config
		.extensions
		.push(ExtHostSpec::new(key.clone(), py_eval_manifest(&key)));
	let Err(error) = ExtHostSupervisor::spawn(config).await else {
		panic!("stale WorkerHello generation was accepted");
	};
	assert!(
		error.to_string().contains("identity or generation"),
		"unexpected stale-generation rejection: {error}"
	);
}

#[tokio::test]
async fn trusted_cli_module_is_loaded_and_activated_from_its_exact_file() {
	let scratch = tempfile::tempdir().expect("trusted module scratch");
	let module = scratch.path().join("trusted_policy.py");
	let marker = scratch.path().join("activated");
	let marker_json =
		serde_json::to_string(marker.to_string_lossy().as_ref()).expect("encode marker path");
	fs::write(
		&module,
		format!(
			"import omp\n\n@omp.tool(\"trusted_echo\", kind=\"hard\")\nasync def trusted_echo(value: \
			 str) -> str:\n    return value\n\ndef extension_activate(_event, _context):\n    with \
			 open({marker_json}, 'w', encoding='utf-8') as marker:\n        marker.write(__file__)\n",
		),
	)
	.expect("write trusted extension module");

	let extension = omp_app::cli::trusted_extension(
		omp_envd::validate_trusted_module(&module).expect("validate trusted module"),
	);
	assert!(extension.manifest.declarations.tools().next().is_none());
	assert!(extension.manifest.services.provides().next().is_none());
	assert!(extension.manifest.resource_limits.is_empty());
	let mut config = test_config(env!("CARGO_BIN_EXE_omp").into());
	config.extensions.push(extension);
	let supervisor = time::timeout(Duration::from_secs(60), ExtHostSupervisor::spawn(config))
		.await
		.expect("trusted worker activation timed out")
		.expect("activate exact trusted module");
	assert_eq!(
		fs::read_to_string(&marker).expect("activation marker"),
		fs::canonicalize(&module)
			.expect("canonical trusted module")
			.to_string_lossy(),
	);
	let registrations = supervisor.registrations();
	assert_eq!(registrations.len(), 1);
	assert!(registrations[0].hard_granted);
	assert_eq!(
		registrations[0]
			.declaration
			.definition
			.as_ref()
			.expect("trusted tool definition")
			.name,
		"trusted_echo",
	);
	supervisor.shutdown().await;
}

#[tokio::test]
async fn same_binary_worker_kills_native_call_and_respawns() {
	let site = tempfile::tempdir().expect("Python site scratch directory");
	fs::write(site.path().join("phase1_worker_tools.py"), EXTENSION)
		.expect("write temporary Python extension");
	fs::write(site.path().join("sibling_worker_tools.py"), SIBLING_EXTENSION)
		.expect("write sibling Python extension");

	let mut config = test_config(env!("CARGO_BIN_EXE_omp").into());
	let key = HostKey::new("workspace", "trusted", "phase1-worker-tools");
	let mut extension = ExtHostSpec::new(
		key.clone(),
		test_manifest(&key, "phase1_worker_tools", ["echo_update", "reject_args", "native_block"]),
	);
	extension.python_site = Some(site.path().to_owned());
	config.extensions.push(extension);
	let sibling_key = HostKey::new("workspace", "trusted", "sibling-worker-tools");
	let mut sibling = ExtHostSpec::new(
		sibling_key.clone(),
		test_manifest(&sibling_key, "sibling_worker_tools", ["stable_echo"]),
	);
	let scoped_socket = site.path().join("sibling-data.sock");
	sibling.data_socket = Some(scoped_socket.clone());
	sibling.python_site = Some(site.path().to_owned());
	config.extensions.push(sibling);
	config.interrupt_grace = CoreDuration::new(250, DurationUnit::Milliseconds);
	config.initial_backoff = Duration::from_millis(10);
	config.max_backoff = Duration::from_millis(50);
	let interrupt_grace = config
		.interrupt_grace
		.to_std()
		.expect("test interrupt grace");
	let respawn_timeout = config.spawn_timeout;
	let callbacks = bind_test_control(&mut config);

	let supervisor = Arc::new(
		time::timeout(Duration::from_secs(60), ExtHostSupervisor::spawn(config))
			.await
			.expect("worker hello and registration timed out")
			.expect("spawn same-binary Python worker"),
	);
	callbacks.bind(supervisor.clone());

	let names = supervisor
		.registrations()
		.iter()
		.map(|registration| {
			registration
				.declaration
				.definition
				.as_ref()
				.expect("registered definition")
				.name
				.as_str()
		})
		.collect::<Vec<_>>();
	assert_eq!(names, ["echo_update", "native_block", "reject_args", "stable_echo"]);
	assert!(
		supervisor
			.registrations()
			.iter()
			.all(|registration| registration.declaration.rev == "1")
	);

	let (first_update, first_complete) = time::timeout(
		Duration::from_secs(5),
		echo_roundtrip(&supervisor, "echo-before", "before kill"),
	)
	.await
	.expect("initial echo invocation timed out");
	assert_eq!(first_update["message"], "before kill");
	assert_eq!(first_update["commit_seal"], "committed");
	assert_eq!(completion_text(&first_complete), "before kill");
	let first_details: Value = serde_json::from_slice(
		first_complete
			.details_json
			.as_deref()
			.expect("inline echo completion details"),
	)
	.expect("echo completion details JSON");
	assert_eq!(first_details, first_update);
	let mut rejected = open_committed(
		&supervisor,
		call("args-rejected", "reject_args", json!({}), Duration::from_secs(5)),
	)
	.expect("dispatch structured argument rejection");
	match rejected.next().await.expect("argument rejection event") {
		WorkerEvent::Complete(complete) => {
			assert_eq!(complete.kind, WorkerOutcomeKind::ArgsRejected);
			let issue = complete.args_issue.expect("ToolArgs retained ArgIssue");
			assert_eq!(issue.path, ["count"]);
			assert_eq!(issue.expected, "integer");
			assert_eq!(issue.kind, "type");
		},
		WorkerEvent::ProtocolError(error) => {
			panic!("argument rejection flattened to protocol error: {}", error.message)
		},
		event => panic!("argument rejection flattened to wrong terminal event: {event:?}"),
	}
	let first_pid = first_details["pid"]
		.as_i64()
		.expect("worker pid in echo details") as i32;
	let started = site.path().join("native-call-started");

	let (sibling_pid, inherited_socket) =
		stable_roundtrip(&supervisor, "sibling-before", "still alive").await;
	assert_eq!(
		inherited_socket.as_deref(),
		scoped_socket.to_str(),
		"selected child did not inherit its scoped DATA socket"
	);
	let mut blocked = open_committed(
		&supervisor,
		call(
			"native-block",
			"native_block",
			json!({ "started": started, "seconds": 30 }),
			Duration::from_secs(60),
		),
	)
	.expect("dispatch committed native invocation");
	let blocked_pid = wait_for_marker(&started).await;
	assert_eq!(blocked_pid, first_pid, "native call did not run in the warm worker");

	blocked
		.interrupt(Interrupt {
			invocation_id: "native-block".into(),
			reason:        "courtesy interrupt".into(),
			class:         InterruptClass::Immediate.into(),
			props:         None,
		})
		.expect("forward courtesy interrupt");
	time::sleep(Duration::from_millis(75)).await;
	assert!(
		signal::kill(Pid::from_raw(blocked_pid), None).is_ok(),
		"courtesy interrupt structurally killed worker {blocked_pid}",
	);

	let cancelled_at = Instant::now();
	blocked.cancel("integration cancellation");
	let abort = match time::timeout(Duration::from_secs(3), blocked.next())
		.await
		.expect("native cancellation exceeded grace plus kill window")
		.expect("supervisor closed before reporting cancellation")
	{
		WorkerEvent::Aborted(abort) => abort,
		WorkerEvent::Update(_) => panic!("native blocker unexpectedly emitted an update"),
		WorkerEvent::Complete(_) => panic!("native blocker completed instead of being killed"),
		event => panic!("native blocker emitted unexpected CONTROL event: {event:?}"),
	};
	let cancel_elapsed = cancelled_at.elapsed();
	assert_eq!(abort.kind, WorkerAbortKind::Cancelled);
	assert!(abort.effects_unknown, "dispatched worker cancellation must report effects unknown");
	assert!(
		abort
			.reason
			.contains("no other extension host was terminated"),
		"isolated cancellation omitted its blast-radius truth: {}",
		abort.reason
	);
	assert!(
		cancel_elapsed >= interrupt_grace.saturating_sub(Duration::from_millis(25)),
		"native call ended cooperatively before the hard-kill grace elapsed: {cancel_elapsed:?}"
	);
	assert!(
		matches!(signal::kill(Pid::from_raw(blocked_pid), None), Err(Errno::ESRCH)),
		"cancelled native worker process {blocked_pid} is still alive"
	);
	let (sibling_after, _) = stable_roundtrip(&supervisor, "sibling-after", "never restarted").await;
	assert_eq!(
		sibling_after, sibling_pid,
		"cancelling one extension restarted its independently supervised sibling"
	);
	let (second_update, second_complete) =
		time::timeout(respawn_timeout, echo_roundtrip(&supervisor, "echo-after", "after respawn"))
			.await
			.expect("respawned worker did not serve the next invocation");
	assert_eq!(second_update["message"], "after respawn");
	assert_eq!(completion_text(&second_complete), "after respawn");
	let second_details: Value = serde_json::from_slice(
		second_complete
			.details_json
			.as_deref()
			.expect("inline respawn echo details"),
	)
	.expect("respawn echo details JSON");
	let second_pid = second_details["pid"]
		.as_i64()
		.expect("respawned worker pid") as i32;
	assert_ne!(second_pid, blocked_pid, "supervisor reused the cancelled worker process");

	supervisor.shutdown().await;
}

#[tokio::test]
async fn opt_in_py_eval_survives_cancel_and_respawn() {
	let disabled = ExtHostSupervisor::spawn(test_config("/definitely/not/an/extension-host".into()))
		.await
		.expect("spawn empty extension supervisor");
	assert!(
		disabled.registrations().is_empty(),
		"default worker unexpectedly advertised a Python tool"
	);
	disabled.shutdown().await;

	let mut config = test_config(env!("CARGO_BIN_EXE_omp").into());
	let key = HostKey::new("workspace", "trusted", PY_EVAL_MODULE);
	config
		.extensions
		.push(ExtHostSpec::new(key.clone(), py_eval_manifest(&key)));
	config.initial_backoff = Duration::from_millis(10);
	config.max_backoff = Duration::from_millis(50);
	let interrupt_grace = config
		.interrupt_grace
		.to_std()
		.expect("test interrupt grace");
	let respawn_timeout = config.spawn_timeout;
	let supervisor = time::timeout(Duration::from_secs(60), ExtHostSupervisor::spawn(config))
		.await
		.expect("py_eval worker registration timed out")
		.expect("spawn py_eval worker");

	let [registration] = supervisor.registrations() else {
		panic!("expected exactly one py_eval declaration");
	};
	let declaration = &registration.declaration;
	let definition = declaration.definition.as_ref().expect("py_eval definition");
	assert_eq!(definition.name, "py_eval");
	assert_eq!(declaration.rev, "1");
	let Some(tool_def::Input::JsonSchema(json_schema)) = definition.input.as_ref() else {
		panic!("py_eval uses JSON Schema input");
	};
	assert_eq!(json_schema.strict, Some(true));
	assert_eq!(
		serde_json::from_slice::<Value>(&json_schema.schema_json).expect("py_eval schema JSON"),
		json!({
			"type": "object",
			"properties": { "code": { "type": "string", "minLength": 1 } },
			"required": ["code"],
			"additionalProperties": false,
		})
	);

	let first = py_eval_roundtrip(&supervisor, "py-eval-before", "6 * 7").await;
	assert_eq!(
		serde_json::from_slice::<Value>(completion_details(&first)).expect("py_eval result JSON"),
		json!({ "result": 42 })
	);

	let repr = py_eval_roundtrip(&supervisor, "py-eval-repr", "{3, 1, 2}").await;
	assert_eq!(
		serde_json::from_slice::<Value>(completion_details(&repr)).expect("py_eval repr JSON"),
		json!({ "result": "{1, 2, 3}" })
	);

	let mut fault =
		open_committed(&supervisor, py_eval_call("py-eval-fault", "1 / 0", Duration::from_secs(5)))
			.expect("dispatch failing py_eval");
	match fault.next().await.expect("py_eval fault event") {
		WorkerEvent::Complete(complete) => {
			assert_eq!(
				complete.kind,
				WorkerOutcomeKind::Aborted,
				"Python exception flattened into the wrong terminal branch"
			);
			let details: Value =
				serde_json::from_slice(completion_details(&complete)).expect("py_eval abort details");
			assert_eq!(details["kind"], "effects_unknown");
			assert!(
				details["reason"]
					.as_str()
					.is_some_and(|reason| reason.contains("ZeroDivisionError")),
				"typed Python abort omitted ZeroDivisionError: {details}"
			);
		},
		WorkerEvent::Update(_) => panic!("failing py_eval unexpectedly emitted an update"),
		WorkerEvent::Aborted(abort) => panic!("failing py_eval aborted: {}", abort.reason),
		event => panic!("failing py_eval emitted unexpected CONTROL event: {event:?}"),
	}

	let mut sleeping = open_committed(
		&supervisor,
		py_eval_call("py-eval-sleep", "__import__('time').sleep(30)", Duration::from_secs(60)),
	)
	.expect("dispatch sleeping py_eval");
	time::sleep(Duration::from_millis(100)).await;
	let cancelled_at = Instant::now();
	sleeping.cancel("cancel sleeping evaluation");
	let abort = match time::timeout(Duration::from_secs(3), sleeping.next())
		.await
		.expect("py_eval cancellation exceeded kill window")
		.expect("supervisor closed before reporting py_eval cancellation")
	{
		WorkerEvent::Aborted(abort) => abort,
		WorkerEvent::Complete(complete) => {
			panic!("cancelled py_eval reported clean completion: {complete:?}")
		},
		WorkerEvent::Update(_) => panic!("py_eval unexpectedly emitted an update"),
		event => panic!("sleeping py_eval emitted unexpected CONTROL event: {event:?}"),
	};
	assert_eq!(abort.kind, WorkerAbortKind::Cancelled);
	assert!(abort.effects_unknown);
	let cancel_elapsed = cancelled_at.elapsed();
	assert!(
		cancel_elapsed <= interrupt_grace + Duration::from_secs(1),
		"sleeping evaluation did not terminate promptly: {cancel_elapsed:?}"
	);

	let second =
		time::timeout(respawn_timeout, py_eval_roundtrip(&supervisor, "py-eval-after", "40 + 2"))
			.await
			.expect("respawned py_eval worker did not recover");
	assert_eq!(
		serde_json::from_slice::<Value>(completion_details(&second)).expect("respawn result JSON"),
		json!({ "result": 42 })
	);
	assert_eq!(
		&supervisor.registrations()[0].declaration,
		declaration,
		"respawn changed the fenced registration set"
	);
	supervisor.shutdown().await;
}

struct TestCall {
	open: OpenToolCall,
	raw:  Bytes,
}

fn py_eval_call(call_id: &'static str, code: &'static str, deadline: Duration) -> TestCall {
	TestCall {
		open: OpenToolCall {
			invocation_id: sf!(call_id),
			name: sf!("py_eval"),
			rev: sf!("1"),
			deadline,
		},
		raw:  Bytes::from(
			serde_json::to_vec(&json!({ "code": code })).expect("serialize py_eval arguments"),
		),
	}
}

async fn py_eval_roundtrip(
	supervisor: &ExtHostSupervisor,
	call_id: &'static str,
	code: &'static str,
) -> WorkerCompletion {
	let mut invocation =
		open_committed(supervisor, py_eval_call(call_id, code, Duration::from_secs(5)))
			.expect("dispatch py_eval");
	match invocation.next().await.expect("py_eval event") {
		WorkerEvent::Complete(complete) => {
			assert_eq!(complete.kind, WorkerOutcomeKind::Ok);
			complete
		},
		WorkerEvent::Update(_) => panic!("py_eval unexpectedly emitted an update"),
		WorkerEvent::Aborted(abort) => panic!("py_eval aborted: {}", abort.reason),
		event => panic!("py_eval emitted unexpected CONTROL event: {event:?}"),
	}
}

fn call(call_id: &'static str, name: &'static str, args: Value, deadline: Duration) -> TestCall {
	TestCall {
		open: OpenToolCall { invocation_id: sf!(call_id), name: sf!(name), rev: sf!("1"), deadline },
		raw:  Bytes::from(serde_json::to_vec(&args).expect("serialize committed arguments")),
	}
}

fn open_committed(
	supervisor: &ExtHostSupervisor,
	call: TestCall,
) -> Result<WorkerInvocation, WorkerError> {
	let TestCall { open, raw } = call;
	let invocation_id = open.invocation_id.clone();
	let mut invocation = supervisor.open(open)?;
	invocation.args_committed(ArgsCommitted {
		invocation_id: invocation_id.to_string(),
		raw,
		effect_token: Bytes::from_static(b"test-effect-token"),
		authorized_at_ms: 1,
		effects: None,
		props: None,
	})?;
	Ok(invocation)
}

async fn echo_roundtrip(
	supervisor: &ExtHostSupervisor,
	call_id: &'static str,
	message: &'static str,
) -> (Value, WorkerCompletion) {
	let mut invocation = open_committed(
		supervisor,
		call(
			call_id,
			"echo_update",
			json!({ "message": message, "commit_seal": "committed" }),
			Duration::from_secs(5),
		),
	)
	.expect("dispatch committed echo invocation");
	let update = match invocation.next().await.expect("echo update event") {
		WorkerEvent::Update(update) => update,
		WorkerEvent::Complete(_) => panic!("echo completed before its update"),
		WorkerEvent::Aborted(abort) => panic!("echo aborted: {}", abort.reason),
		event => panic!("echo emitted unexpected CONTROL event: {event:?}"),
	};
	assert_eq!(update.call_id, call_id);
	let update = serde_json::from_slice(&update.json).expect("echo update JSON");
	let complete = match invocation.next().await.expect("echo completion event") {
		WorkerEvent::Complete(complete) => complete,
		WorkerEvent::Update(_) => panic!("echo emitted an unexpected second update"),
		WorkerEvent::Aborted(abort) => panic!("echo aborted after update: {}", abort.reason),
		event => panic!("echo emitted unexpected CONTROL event after update: {event:?}"),
	};
	assert_eq!(complete.call_id, call_id);
	assert_eq!(complete.kind, WorkerOutcomeKind::Ok);
	(update, complete)
}

async fn stable_roundtrip(
	supervisor: &ExtHostSupervisor,
	call_id: &'static str,
	message: &'static str,
) -> (i32, Option<String>) {
	let mut invocation = open_committed(
		supervisor,
		call(call_id, "stable_echo", json!({ "message": message }), Duration::from_secs(5)),
	)
	.expect("dispatch sibling invocation");
	let complete = match invocation.next().await.expect("sibling completion") {
		WorkerEvent::Complete(complete) => complete,
		WorkerEvent::Update(_) => panic!("sibling unexpectedly emitted an update"),
		WorkerEvent::Aborted(abort) => panic!("sibling aborted: {}", abort.reason),
		event => panic!("sibling emitted unexpected CONTROL event: {event:?}"),
	};
	let details =
		serde_json::from_slice::<Value>(completion_details(&complete)).expect("sibling details JSON");
	(
		details["pid"].as_i64().expect("sibling pid") as i32,
		details["env_socket"].as_str().map(ToOwned::to_owned),
	)
}

async fn wait_for_marker(path: &Path) -> i32 {
	time::timeout(Duration::from_secs(3), async {
		loop {
			if let Ok(pid) = fs::read_to_string(path) {
				return pid.parse().expect("native marker contains worker pid");
			}

			time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await
	.expect("native Python call did not enter ctypes sleep")
}

struct InertAuthority;

#[async_trait::async_trait]
impl ControlAuthority for InertAuthority {
	fn handles(&self, _operation: &str) -> bool {
		true
	}

	fn authorize(
		&self,
		_context: &ControlRequestContext,
		_operation: &str,
		_arguments: &serde_json::Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		Ok(())
	}

	async fn request(
		&self,
		_context: ControlRequestContext,
		_operation: Str,
		_arguments: serde_json::Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		Ok(Value::Null)
	}

	async fn effect(
		&self,
		_context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		Ok(())
	}
}

struct IgnoreCatalog;

impl DeviceCatalogObserver for IgnoreCatalog {
	fn catalog_changed(&self, _epoch: u64, _catalog: Arc<[DynamicDeviceCatalogEntry]>) {}
}

struct AllowDevices;

#[async_trait::async_trait]
impl DeviceInvocationAdmission for AllowDevices {
	async fn admit(
		&self,
		_caller: &ControlRequestContext,
		_target: &DynamicDeviceCatalogEntry,
		_arguments: &serde_json::Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		Ok(())
	}
}

fn inert_factory() -> Arc<dyn ControlAuthorityFactory> {
	Arc::new(FixedControlAuthorityFactory::new(Arc::new(InertAuthority)))
}

fn host_factory(
	registry: Arc<dyn ControlAuthorityFactory>,
	devices: Arc<dyn ControlAuthorityFactory>,
) -> Arc<HostControlAuthorityFactory> {
	let envd = EnvdControlAuthorities::new(
		RegistryControlAuthorities::new(registry, devices, inert_factory()),
		PersistenceControlAuthorities::new(inert_factory(), inert_factory(), inert_factory()),
		PolicyControlAuthorities::new(inert_factory(), inert_factory()),
		PresentationControlAuthorities::new(inert_factory(), inert_factory(), inert_factory()),
		ProviderControlAuthorities::new(inert_factory(), inert_factory()),
		inert_factory(),
		inert_factory(),
	);
	Arc::new(HostControlAuthorityFactory::new(
		envd,
		ExternalControlAuthorities::new(inert_factory(), inert_factory()),
	))
}

fn bind_test_control(config: &mut ExtHostConfig) -> Arc<CallbackDispatcherSlot> {
	let manifests = config
		.extensions
		.iter()
		.map(|extension| {
			(
				(
					extension.key.layer().clone(),
					extension.key.tier().clone(),
					extension.key.extension().clone(),
				),
				extension.manifest.clone(),
			)
		})
		.collect::<BTreeMap<_, _>>();
	let registry = RegistryControlFactory::new(manifests);
	let callbacks = CallbackDispatcherSlot::new();
	let devices: Arc<dyn ControlAuthorityFactory> = DeviceControlFactory::new(
		Arc::clone(&registry),
		callbacks.clone(),
		Arc::new(IgnoreCatalog),
		Arc::new(AllowDevices),
	);
	let registry_factory: Arc<dyn ControlAuthorityFactory> = registry.clone();
	config.bind_control_authorities(host_factory(registry_factory, devices));
	config.bind_registry_control(registry);
	callbacks
}

fn test_config(executable: PathBuf) -> ExtHostConfig {
	ExtHostConfig::new(executable, Principal::new(sf!("test"), sf!("Test")), sf!("test-session"), 1)
}

fn py_eval_manifest(key: &HostKey) -> ExtensionManifest {
	let declaration = StaticDeclaration {
		id: sf!("py_eval@.1"),
		kind: sf!("soft"),
		module: sf!("omp_py_eval"),
		trigger: sf!("lazy"),
		key: sf!("py_eval@.1"),
		api: 1,
		failure: sf!("fault"),
		..StaticDeclaration::default()
	};
	ExtensionManifest::new_with_static(
		test_provenance(key),
		sf!("omp_py_eval"),
		[],
		DeclarationSet::new([ToolDeclarationKey::new("py_eval", "", 1)], []),
		ServiceManifest::default(),
		StaticDeclarations {
			ordered: vec![declaration].into_boxed_slice(),
			..StaticDeclarations::default()
		},
		[],
		[ActivationTrigger::FirstReach],
	)
}

fn test_manifest<const N: usize>(
	key: &HostKey,
	entry: &'static str,
	tools: [&'static str; N],
) -> ExtensionManifest {
	let tools = tools
		.into_iter()
		.map(|name| ToolDeclarationKey::new(name, "", 1))
		.collect::<Vec<_>>();
	let ordered = tools
		.iter()
		.map(|tool| StaticDeclaration {
			id: Str::from(format!("{}@.1", tool.name)),
			kind: sf!("soft"),
			module: Str::from(entry),
			trigger: sf!("lazy"),
			key: Str::from(format!("{}@.1", tool.name)),
			api: 1,
			failure: sf!("fault"),
			..StaticDeclaration::default()
		})
		.collect::<Vec<_>>();
	ExtensionManifest::new_with_static(
		test_provenance(key),
		entry,
		[],
		DeclarationSet::new(tools, []),
		ServiceManifest::default(),
		StaticDeclarations { ordered: ordered.into_boxed_slice(), ..StaticDeclarations::default() },
		[],
		[ActivationTrigger::FirstReach],
	)
}

fn test_provenance(key: &HostKey) -> Provenance {
	Provenance::new(
		sf!("test-publisher"),
		key.extension().clone(),
		sf!("1.0.0"),
		ArtifactDigest::new([0; 32]),
		key.layer().clone(),
		key.tier().clone(),
		1,
	)
}
fn completion_text(complete: &WorkerCompletion) -> &str {
	match complete.parts.as_slice() {
		[part] => match part.kind.as_ref() {
			Some(part::Kind::Text(text)) => text,
			other => panic!("expected one text completion part, got {other:?}"),
		},
		parts => panic!("expected one completion part, got {}", parts.len()),
	}
}

fn completion_details(complete: &WorkerCompletion) -> &[u8] {
	complete
		.details_json
		.as_deref()
		.expect("completion carries inline JSON details")
}
