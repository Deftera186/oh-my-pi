//! Central dispatch, projection, and cancellation contracts.

use std::{sync::Arc, time::Duration};

use omp_agent::{
	CancelTree, DispatchOptions, DispatchPolicy, DispatchRequest, Dispatcher, ExternalDispatchEvent,
	ExternalDispatchRequest, ExternalDispatchStream, ExternalToolExecutor, SessionTool,
	SessionToolCx, SessionToolFuture, ToolCancellation,
};
use omp_core::Str;
use omp_journal::blob::BlobStore;
use omp_proto::thread::v1::item;
use omp_session::project_thread;
use omp_tool::{
	CallOutcome, Claims, Part, Precedence, Presentation, PromptCaps, Rev, ToolIdentity, ToolRoute,
	ToolSpec,
};
use parking_lot::Mutex;

mod support;
use support::{
	Fault, Payload, assert_journal_cause, call, registry, request, result_text, session, spec,
	tool_spec,
};

struct SessionEcho(ToolSpec);

impl SessionTool for SessionEcho {
	fn spec(&self) -> &ToolSpec {
		&self.0
	}

	fn call<'a>(
		&'a self,
		cx: SessionToolCx<'a>,
		_args: Box<serde_json::value::RawValue>,
	) -> SessionToolFuture<'a> {
		Box::pin(async move {
			assert!(cx.session.dom().get(cx.call).is_some());
			Ok(CallOutcome::Ok(serde_json::value::to_raw_value("session-owned").expect("raw payload")))
		})
	}
}

#[tokio::test]
async fn session_tools_route_before_registry_invocation() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let tools = registry([spec("session_echo", 1, "wrong registry route")]);
	let identity = tools.resolved_identity("session_echo").expect("identity");
	let dispatcher = Dispatcher::new(
		Arc::clone(&tools),
		DispatchPolicy::new(BlobStore::open(directory.path()).expect("blob store")),
	)
	.with_session_tool(Arc::new(SessionEcho(tool_spec("session_echo", 1))));
	let mut active = session(&directory.path().join("session-tool.oms"));
	let (entry, args) = call(&mut active, &identity, "session-tool");
	dispatcher
		.dispatch(
			&mut active,
			request(
				entry,
				identity,
				args,
				ToolCancellation::ReadOnly(CancelTree::new().begin_turn().read_only_tool()),
				false,
			),
		)
		.await
		.expect("session tool dispatch");
	assert_eq!(result_text(&active, "session-tool"), ["\"session-owned\""]);
}

#[tokio::test]
async fn central_truncation_spills_and_notrunc_explicitly_opts_out() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let tools = registry([spec("echo", 1, "abcdefghij")]);
	let identity = tools.resolved_identity("echo").expect("identity");
	let policy = DispatchPolicy::new(BlobStore::open(directory.path()).expect("blob store"))
		.with_limits(5, usize::MAX, Duration::from_secs(5));
	let dispatcher = Dispatcher::new(Arc::clone(&tools), policy);
	let tree = CancelTree::new();
	let mut bounded = session(&directory.path().join("bounded.oms"));
	let (entry, args) = call(&mut bounded, &identity, "bounded");
	let report = dispatcher
		.dispatch(
			&mut bounded,
			request(
				entry,
				identity.clone(),
				args,
				ToolCancellation::ReadOnly(tree.begin_turn().read_only_tool()),
				false,
			),
		)
		.await
		.expect("bounded dispatch");
	let spilled = report.spilled.expect("full output spills");
	assert_eq!(
		dispatcher
			.policy()
			.spill
			.get(&spilled)
			.expect("artifact reads")
			.as_ref(),
		b"abcdefghij"
	);
	let parts = result_text(&bounded, "bounded");
	assert_eq!(parts[0], "abcde");
	assert!(parts[1].starts_with("artifact://sha256/"));
	assert_journal_cause(&bounded, entry);

	let mut unlimited = session(&directory.path().join("unlimited.oms"));
	let (entry, args) = call(&mut unlimited, &identity, "unlimited");
	let report = dispatcher
		.dispatch(
			&mut unlimited,
			request(
				entry,
				identity,
				args,
				ToolCancellation::ReadOnly(tree.begin_turn().read_only_tool()),
				true,
			),
		)
		.await
		.expect("unbounded dispatch");
	assert!(report.spilled.is_none());
	assert_eq!(result_text(&unlimited, "unlimited"), ["abcdefghij"]);
}

#[tokio::test]
async fn typed_batches_publish_before_streaming_tool_settles() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let tools =
		registry([spec("stream", 1, "settled").streaming("first", Duration::from_millis(300))]);
	let identity = tools.resolved_identity("stream").expect("identity");
	let dispatcher = Dispatcher::new(
		Arc::clone(&tools),
		DispatchPolicy::new(BlobStore::open(directory.path()).expect("blob store")),
	);
	let mut session = session(&directory.path().join("stream.oms"));
	let (entry, args) = call(&mut session, &identity, "streaming");
	let (_, patches) = session.subscribe();
	let cancellation = CancelTree::new().begin_turn();
	let dispatch = dispatcher.dispatch(
		&mut session,
		request(
			entry,
			identity,
			args,
			ToolCancellation::ReadOnly(cancellation.read_only_tool()),
			false,
		),
	);
	tokio::pin!(dispatch);
	tokio::select! {
		patch = patches.recv_async() => assert!(patch.is_ok(), "update patch publishes"),
		result = &mut dispatch => panic!("dispatch settled before update: {result:?}"),
		() = tokio::time::sleep(Duration::from_millis(150)) => panic!("update was not published"),
	}
	let report = dispatch.await.expect("streaming dispatch settles");
	assert!(!report.is_error);
}

#[tokio::test]
async fn cancelled_task_projects_cancelled_error_never_completed() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let tools = registry([spec("slow", 1, "never").streaming("started", Duration::from_secs(60))]);
	let identity = tools.resolved_identity("slow").expect("identity");
	let dispatcher = Dispatcher::new(
		Arc::clone(&tools),
		DispatchPolicy::new(BlobStore::open(directory.path()).expect("blob store")),
	);
	let mut session = session(&directory.path().join("cancelled.oms"));
	let (entry, args) = call(&mut session, &identity, "cancelled");
	let cancellation = CancelTree::new().begin_turn().read_only_tool();
	cancellation.cancel_tool();
	let report = dispatcher
		.dispatch(
			&mut session,
			request(entry, identity, args, ToolCancellation::ReadOnly(cancellation), false),
		)
		.await
		.expect("cancellation journals terminal");
	assert!(report.is_error);
	let items = project_thread(session.dom());
	let result = items
		.into_iter()
		.find_map(|item| match item.kind? {
			item::Kind::ToolResult(result) if result.call_id == "cancelled" => Some(result),
			_ => None,
		})
		.expect("cancelled result projects");
	assert!(result.is_error);
	assert!(
		result_text(&session, "cancelled")
			.join("")
			.contains("cancel")
	);
	assert_journal_cause(&session, entry);
}

#[test]
fn registry_keeps_historical_revisions_and_identity_caches_normalized_schema() {
	let tools = registry([spec("versioned", 1, "old"), spec("versioned", 2, "new")]);
	let live = tools.resolved_identity("versioned").expect("live identity");
	assert_eq!(live.rev.n, 2);
	let historical = ToolIdentity {
		name: Str::new_static("versioned"),
		rev:  Rev { family: Str::new_static("test"), n: 1 },
	};
	let verdict = serde_json::to_vec(&omp_tool::CallOutcome::<Payload, Fault>::Ok(Payload {
		text: Str::new_static("old"),
	}))
	.expect("verdict serializes");
	let caps = PromptCaps::for_tool(
		omp_tool::CapsBase {
			maximum_parts:      8,
			maximum_text_bytes: 1024,
			media:              false,
			model_class:        omp_tool::ModelClass::Standard,
		},
		&historical.rev,
	);
	let first = tools
		.project_verdict(&historical, &verdict, false, &caps)
		.expect("historical projects");
	let second = tools
		.project_verdict(&historical, &verdict, false, &caps)
		.expect("cached projects");
	assert!(Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn closed_progress_channel_does_not_starve_immediate_terminal_join() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let tools = registry([spec("immediate", 1, "failed").faulting()]);
	let identity = tools.resolved_identity("immediate").expect("identity");
	let dispatcher = Dispatcher::new(
		Arc::clone(&tools),
		DispatchPolicy::new(BlobStore::open(directory.path()).expect("blob store")),
	);
	let mut session = session(&directory.path().join("immediate.oms"));
	let (entry, args) = call(&mut session, &identity, "immediate");
	let cancellation = CancelTree::new().begin_turn();
	let report = tokio::time::timeout(
		Duration::from_millis(250),
		dispatcher.dispatch(
			&mut session,
			request(
				entry,
				identity,
				args,
				ToolCancellation::ReadOnly(cancellation.read_only_tool()),
				false,
			),
		),
	)
	.await
	.expect("terminal join is not starved")
	.expect("fault projects");
	assert!(report.is_error);
}

#[tokio::test]
async fn central_per_line_clamp_bounds_long_lines_and_records_the_count() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let output = "0123456789abcdef\nshort\n0123456789abcdef";
	let tools = registry([spec("lines", 1, output)]);
	let identity = tools.resolved_identity("lines").expect("identity");
	let dispatcher = Dispatcher::new(
		Arc::clone(&tools),
		DispatchPolicy::new(BlobStore::open(directory.path()).expect("blob store")).with_limits(
			64 * 1024,
			8,
			Duration::from_secs(5),
		),
	);
	let mut session = session(&directory.path().join("lines.oms"));
	let (entry, args) = call(&mut session, &identity, "lines");
	let cancellation = CancelTree::new().begin_turn();
	let report = dispatcher
		.dispatch(
			&mut session,
			request(
				entry,
				identity,
				args,
				ToolCancellation::ReadOnly(cancellation.read_only_tool()),
				false,
			),
		)
		.await
		.expect("line dispatch");
	assert_eq!(report.lines_clamped, 2);
	assert_eq!(result_text(&session, "lines")[0], "01234567…\nshort\n01234567…");
	let artifact = report.spilled.expect("clamped output spills");
	assert_eq!(
		dispatcher
			.policy()
			.spill
			.get(&artifact)
			.expect("artifact reads"),
		output.as_bytes()
	);
}

#[tokio::test]
async fn notrunc_disables_the_per_line_clamp() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let output = "0123456789abcdef";
	let tools = registry([spec("lines", 1, output)]);
	let identity = tools.resolved_identity("lines").expect("identity");
	let dispatcher = Dispatcher::new(
		Arc::clone(&tools),
		DispatchPolicy::new(BlobStore::open(directory.path()).expect("blob store")).with_limits(
			64 * 1024,
			8,
			Duration::from_secs(5),
		),
	);
	let mut session = session(&directory.path().join("notrunc.oms"));
	let (entry, args) = call(&mut session, &identity, "lines");
	let cancellation = CancelTree::new().begin_turn();
	let report = dispatcher
		.dispatch(
			&mut session,
			request(
				entry,
				identity,
				args,
				ToolCancellation::ReadOnly(cancellation.read_only_tool()),
				true,
			),
		)
		.await
		.expect("notrunc dispatch");
	assert_eq!(report.lines_clamped, 0);
	assert!(report.spilled.is_none());
	assert_eq!(result_text(&session, "lines"), [output]);
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExternalObserved {
	call_id: Str,
	args:    Str,
	route:   ToolRoute,
}

struct ScriptedExternal {
	observed: Arc<Mutex<Vec<ExternalObserved>>>,
}

impl ExternalToolExecutor for ScriptedExternal {
	fn invoke(&self, request: ExternalDispatchRequest) -> ExternalDispatchStream {
		self.observed.lock().push(ExternalObserved {
			call_id: request.call_id,
			args:    Str::new(request.args.get()),
			route:   request.route,
		});
		let update = serde_json::value::to_raw_value(&serde_json::json!({
			"text": "external progress"
		}))
		.expect("update serializes");
		let outcome = serde_json::value::to_raw_value(&CallOutcome::<Payload, Fault>::Ok(Payload {
			text: Str::new_static("external result"),
		}))
		.expect("outcome serializes");
		Box::pin(futures::stream::iter([
			ExternalDispatchEvent::Update(update),
			ExternalDispatchEvent::Done {
				outcome,
				parts: vec![Part::Text { text: Str::new_static("external result") }],
				is_error: false,
			},
		]))
	}
}

#[tokio::test]
async fn worker_routed_tools_use_the_injected_external_executor() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let mut tools = omp_tool::Registry::new();
	tools
		.register_worker(tool_spec("worker", 1), Presentation::Slot, Claims {
			precedence: Precedence::CORE,
			claimant:   Str::new_static("omp-agent/tests"),
			replaces:   None,
		})
		.expect("worker registers");
	let tools = Arc::new(tools);
	let identity = tools.resolved_identity("worker").expect("worker identity");
	let observed = Arc::new(Mutex::new(Vec::new()));
	let dispatcher = Dispatcher::new(
		Arc::clone(&tools),
		DispatchPolicy::new(BlobStore::open(directory.path()).expect("blob store")),
	)
	.with_external_executor(Arc::new(ScriptedExternal { observed: Arc::clone(&observed) }));
	let mut session = session(&directory.path().join("worker.oms"));
	let (entry, args) = call(&mut session, &identity, "worker-1");
	let cancellation = CancelTree::new().begin_turn();

	let report = dispatcher
		.dispatch(&mut session, DispatchRequest {
			identity,
			call_id: Str::new_static("worker-1"),
			call: entry,
			args,
			options: DispatchOptions::default(),
			cancellation: ToolCancellation::ReadOnly(cancellation.read_only_tool()),
		})
		.await
		.expect("external dispatch completes");

	assert!(!report.is_error);
	assert_eq!(result_text(&session, "worker-1"), ["external result"]);
	assert_eq!(observed.lock().as_slice(), [ExternalObserved {
		call_id: Str::new_static("worker-1"),
		args:    Str::new_static("{}"),
		route:   ToolRoute::Worker {
			site: omp_tool::WorkerSiteKind::Env,
			name: Str::new_static("worker"),
		},
	}]);
	assert_journal_cause(&session, entry);
}
