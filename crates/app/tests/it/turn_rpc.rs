//! Integration coverage for daemon-backed RPC turn sessions.

use std::{
	collections::BTreeMap,
	fs,
	path::Path,
	sync::Arc,
	task::{Context, Poll},
	time::Duration,
};

use bytes::Bytes;
use flume::Receiver;
use futures::{StreamExt as _, future::BoxFuture};
use omp_agent::{
	Error as TurnError, InvokeFrame, RpcTurnClient, RpcTurnSession, TurnClient, TurnInput,
	TurnOptions, TurnSession,
};
use omp_app::{
	daemon::{DaemonConfig, DaemonHandle},
	endpoint::LocalEndpoint,
};
use omp_catalog::{
	ManagementCapabilities, OperationBits, OperationKind,
	snapshot::{Catalog, SnapshotProvenance},
};
use omp_core::{Str, sf};
use omp_inference::{
	Answer, Error as InferenceError, ErrorKind, ErrorPhase, Registry, RetryAction,
	call::{Call, OpaqueJson},
	event::{
		BlockKind, ChatEvent, Completion, FinishReason, ToolCall, WorkflowAction, WorkflowResponse,
		WorkflowResponseKind,
	},
	id::{ToolCallId, TurnId as ProviderTurnId},
	layer::{LayerCall, stack::RouteProviderService},
	provider::fake::{FakeProvider, FakeScript},
	receipt::{ExecutionReceipt, ReasonId, Usage},
	registry::RouteUnavailable,
	session::ConversationSessionPlanner,
};
use omp_proto::{
	inference::v1::{
		self as pb, exec_status,
		invoke_input::{self, chunk},
		turn_event, value,
	},
	prost::Message as _,
	thread::v1::{self as thread_pb, item},
};
use omp_tool::{
	Claims, Constraint, Effects, Ev, IncomingParams, LiftedCall, Part, Precedence, Presentation,
	PromptCaps, RecordedCall, Rev, Tool, ToolSpec,
};
use tokio::time;
use tower::Service;

const fn test_claims() -> Claims {
	Claims { precedence: Precedence::CORE, claimant: sf!("omp/core"), replaces: None }
}

struct RegisteredTestTool {
	spec: ToolSpec,
}

impl Tool for RegisteredTestTool {
	type Fault = serde_json::Value;
	type Params = serde_json::Value;
	type Payload = serde_json::Value;
	type Update = serde_json::Value;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		_params: IncomingParams<'c>,
	) -> impl futures::Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		futures::stream::empty()
	}

	fn prompt(&self, _view: Result<&Self::Payload, &Self::Fault>, _caps: &PromptCaps) -> Vec<Part> {
		Vec::new()
	}
}

fn tool_registry() -> Arc<omp_tool::Registry> {
	let mut registry = omp_tool::Registry::new();
	registry
		.register(
			RegisteredTestTool {
				spec: ToolSpec {
					name:            sf!("exec.shell"),
					rev:             Rev { family: sf!("shell"), n: 2 },
					description:     sf!("test shell"),
					schema:          Bytes::from_static(br#"{"type":"object"}"#),
					constraint:      Constraint::None,
					effects:         Effects::empty(),
					projection_code: [0; 32],
				},
			},
			Presentation::Slot,
			test_claims(),
		)
		.expect("test tool registers");
	Arc::new(registry)
}

const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
struct FakeRoute(FakeProvider);

impl Service<LayerCall<Call>> for FakeRoute {
	type Error = InferenceError;
	type Future = BoxFuture<'static, Result<Answer, InferenceError>>;
	type Response = Answer;

	fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		<FakeProvider as Service<Call>>::poll_ready(&mut self.0, context)
	}

	fn call(&mut self, request: LayerCall<Call>) -> Self::Future {
		<FakeProvider as Service<Call>>::call(&mut self.0, request.payload)
	}
}

fn completion(reason: FinishReason, blocks: u32) -> ChatEvent {
	ChatEvent::Completed(Completion {
		reason,
		blocks,
		usage: Usage::default(),
		receipt: ExecutionReceipt::default().into(),
	})
}

fn scripted_registry(
	database: &Path,
) -> (Registry, ConversationSessionPlanner, FakeProvider, String) {
	let mut compiled = Catalog::embedded().compiled().clone();
	for provider in &mut compiled.providers {
		provider.management = ManagementCapabilities {
			operations:        OperationBits::empty(),
			multiple_accounts: false,
			refresh:           false,
			principal_quota:   false,
		};
	}
	let artifacts = Catalog::encode(compiled, SnapshotProvenance { source_digest: [0; 32] })
		.expect("test catalog snapshot");
	let catalog = Arc::new(Catalog::decode(&artifacts.postcard).expect("test catalog"));
	let model = catalog
		.models()
		.iter()
		.find(|model| {
			model
				.capabilities
				.operations
				.contains_kind(OperationKind::Chat)
		})
		.expect("chat model");
	let route_id = model.routes.first().expect("chat route").clone();
	let route = catalog.route(&route_id).expect("catalog route");
	let fake = FakeProvider::new(route.provider.clone(), route_id.clone());
	fake.extend([
		FakeScript::chat(vec![
			Ok(ChatEvent::BlockStarted { index: 0, kind: BlockKind::Text }),
			Ok(ChatEvent::TextDelta { index: 0, text: sf!("I will inspect it. ") }),
			Ok(ChatEvent::ToolCallStarted {
				index: 1,
				id:    ToolCallId::from("call-1"),
				name:  sf!("exec.shell"),
			}),
			Ok(ChatEvent::ToolArgumentsDelta {
				index: 1,
				bytes: Bytes::from_static(br#"{"command":"pwd"}"#),
			}),
			Ok(ChatEvent::ToolCallReady {
				index: 1,
				call:  ToolCall {
					id:        ToolCallId::from("call-1"),
					name:      sf!("exec.shell"),
					arguments: OpaqueJson::new(serde_json::json!({"command": "pwd"})),
				},
			}),
			Ok(completion(FinishReason::ToolCalls, 2)),
		]),
		FakeScript::chat(vec![
			Ok(ChatEvent::WorkflowAction(WorkflowAction {
				invocation:    sf!("invoke-2"),
				call:          Some(ToolCallId::from("call-2")),
				name:          sf!("exec.shell"),
				arguments:     Bytes::from_static(br#"{"command":"echo live"}"#),
				timeout:       Some(Duration::from_secs(2)),
				response_kind: WorkflowResponseKind::Invoke,
			})),
			Ok(ChatEvent::BlockStarted { index: 0, kind: BlockKind::Text }),
			Ok(ChatEvent::TextDelta { index: 0, text: sf!("The live result arrived.") }),
			Ok(completion(FinishReason::Stop, 1)),
		]),
		FakeScript::chat(vec![
			Ok(ChatEvent::BlockStarted { index: 0, kind: BlockKind::Text }),
			Ok(ChatEvent::TextDelta { index: 0, text: sf!("Forked from the retained prefix.") }),
			Ok(completion(FinishReason::Stop, 1)),
		]),
		FakeScript::precommit(
			OperationKind::Chat,
			InferenceError::new(
				ErrorKind::RateLimited,
				ErrorPhase::Streaming,
				RetryAction::SameRoute { after: Duration::from_millis(37) },
				ExecutionReceipt::default(),
			),
		),
	]);
	let route_service = RouteProviderService::new(FakeRoute(fake.clone()));
	let mut builder = Registry::builder(catalog.clone());
	for candidate in catalog.routes() {
		builder = if candidate.id == route_id {
			builder
				.register_route(candidate.id.clone(), route_service.clone())
				.expect("register fake route")
		} else {
			builder
				.register_unavailable(RouteUnavailable {
					route:     candidate.id.clone(),
					reason:    ReasonId(sf!("turn-rpc-test-route-unavailable")),
					operation: None,
				})
				.expect("register unavailable route")
		};
	}
	let registry = builder.build().expect("deterministic registry");
	let model_key = model.key.as_str().to_owned();
	let sessions = ConversationSessionPlanner::open(database, catalog).expect("session planner");
	(registry, sessions, fake, model_key)
}

fn user_thread() -> thread_pb::Thread {
	thread_pb::Thread {
		items: vec![thread_pb::Item {
			seq:           1,
			created_at_ms: 0,
			kind:          Some(thread_pb::item::Kind::Message(thread_pb::Message {
				role:  thread_pb::Role::User as i32,
				parts: vec![thread_pb::Part {
					kind: Some(thread_pb::part::Kind::Text("Inspect the workspace".to_owned())),
				}],
				..Default::default()
			})),
			props:         None,
		}],
	}
}

async fn next_event(session: &mut RpcTurnSession) -> Option<Result<pb::TurnEvent, TurnError>> {
	let mut events = session.events();
	time::timeout(TIMEOUT, events.next())
		.await
		.expect("turn event timed out")
}

async fn outcome(session: &mut RpcTurnSession) -> pb::Outcome {
	loop {
		match next_event(session).await {
			Some(Ok(pb::TurnEvent { event: Some(turn_event::Event::Outcome(outcome)) })) => {
				return outcome;
			},
			Some(Ok(_)) => {},
			Some(Err(error)) => panic!("turn failed before outcome: {error}"),
			None => panic!("turn ended before outcome"),
		}
	}
}

async fn observed_response(responses: &Receiver<WorkflowResponse>) -> WorkflowResponse {
	time::timeout(TIMEOUT, responses.recv_async())
		.await
		.expect("provider response timed out")
		.expect("provider response observer closed")
}

#[tokio::test]
async fn rpc_turn_client_proves_stateful_replay_duplex_and_recovery_over_owner_uds() {
	use std::os::unix::fs::PermissionsExt as _;

	let scratch = tempfile::tempdir().expect("scratch directory");
	let socket = scratch.path().join("gateway.sock");
	let data = scratch.path().join("state");
	let (registry, sessions, fake, model) = scripted_registry(&scratch.path().join("sessions.db"));
	let (live_responses, responses) = flume::bounded(8);
	let daemon = DaemonHandle::start_for_test(
		DaemonConfig::local(LocalEndpoint::from(socket.clone())).with_data_dir(data),
		registry,
		sessions,
		tool_registry(),
		live_responses,
	)
	.await
	.expect("test gateway starts");
	let metadata = fs::metadata(&socket).expect("bound UDS metadata");
	assert_eq!(metadata.permissions().mode() & 0o077, 0, "UDS must be owner-only");

	let channel = omp_rpc::uds::connect(&socket)
		.await
		.expect("connect owner UDS");
	let client = RpcTurnClient::new(
		channel,
		omp_inference::auth::UsageAttribution::new("turn-rpc-test", "omp-test", None::<&str>),
	);
	let thread = user_thread();
	let options = TurnOptions {
		context_id:      Some(sf!("context-1")),
		params:          pb::ChatParams { model, ..Default::default() },
		executor:        Some(pb::Executor { tools: vec!["exec.shell".to_owned()] }),
		props:           None,
		provider_reset:  false,
		stream_watchdog: omp_agent::StreamWatchdog::default(),
	};

	let first_id = ProviderTurnId::from("turn-full");
	let mut full = client
		.turn(first_id.clone(), TurnInput::Full(thread.clone()), &options)
		.await
		.expect("full turn opens over UDS");
	assert!(matches!(
		next_event(&mut full).await,
		Some(Ok(pb::TurnEvent {
			event: Some(pb::turn_event::Event::Accepted(pb::Accepted { replay: false })),
		}))
	));
	let first = outcome(&mut full).await;
	let first_revision = first.revision.clone().expect("stateful full revision");
	assert!(first.output.iter().any(|item| matches!(
		&item.kind,
		Some(thread_pb::item::Kind::Message(message))
			if message.role == thread_pb::Role::Assistant as i32
	)));
	let first_call = first
		.output
		.iter()
		.find(|item| {
			matches!(
				&item.kind,
				Some(thread_pb::item::Kind::ToolCall(call))
					if call.id == "call-1" && call.name == "exec.shell"
			)
		})
		.expect("ordinary committed tool call");
	assert_eq!(
		first_call
			.props
			.as_ref()
			.and_then(|props| props.fields.get(omp_tool::TOOL_REV_PROP))
			.and_then(|value| value.kind.as_ref()),
		Some(&pb::value::Kind::String("shell.2".to_owned()))
	);

	let prior_result = thread_pb::Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(item::Kind::ToolResult(thread_pb::ToolResult {
			call_id: "call-1".to_owned(),
			parts: vec![thread_pb::Part {
				kind: Some(thread_pb::part::Kind::Text("/work/omp".to_owned())),
			}],
			..Default::default()
		})),
		props:         None,
	};
	let mut delta = client
		.turn(
			ProviderTurnId::from("turn-delta"),
			TurnInput::Delta(
				pb::ContextRef {
					context_id: "context-1".to_owned(),
					expected:   Some(first_revision.clone()),
				},
				pb::ThreadDelta { truncate_to: None, append: vec![prior_result] },
			),
			&options,
		)
		.await
		.expect("delta turn opens over UDS");
	assert!(matches!(
		next_event(&mut delta).await,
		Some(Ok(pb::TurnEvent {
			event: Some(pb::turn_event::Event::Accepted(pb::Accepted { replay: false })),
		}))
	));
	let invoke = loop {
		match next_event(&mut delta).await {
			Some(Ok(pb::TurnEvent { event: Some(turn_event::Event::Invoke(invoke)) })) => {
				break invoke;
			},
			Some(Ok(_)) => {},
			other => panic!("expected live invocation, got {other:?}"),
		}
	};
	assert_eq!(invoke.invocation_id, "invoke-2");
	assert_eq!(invoke.name, "exec.shell");
	let input = pb::InvokeInput {
		invocation_id: "invoke-2".to_owned(),
		payload:       Some(invoke_input::Payload::Chunk(pb::invoke_input::Chunk {
			channel: chunk::Channel::Stdout as i32,
			data:    Bytes::from_static(b"live output"),
		})),
	};
	let complete = pb::InvokeComplete {
		invocation_id: "invoke-2".to_owned(),
		tool_result: Some(thread_pb::ToolResult {
			call_id: "call-2".to_owned(),
			parts: vec![thread_pb::Part {
				kind: Some(thread_pb::part::Kind::Text("live output".to_owned())),
			}],
			..Default::default()
		}),
		status: Some(pb::ExecStatus {
			outcome: exec_status::Outcome::Exited as i32,
			exit_code: 0,
			..Default::default()
		}),
		..Default::default()
	};
	delta
		.submit(InvokeFrame::from(input.clone()))
		.await
		.expect("live input accepted");
	delta
		.submit(InvokeFrame::from(complete.clone()))
		.await
		.expect("live completion accepted");
	match observed_response(&responses).await {
		WorkflowResponse::InvokeInput(actual) => {
			assert_eq!(actual.invocation.as_str(), "invoke-2");
			assert_eq!(pb::InvokeInput::decode(actual.payload).expect("decode invoke input"), input);
		},
		other => panic!("expected provider invoke input, got {other:?}"),
	}
	match observed_response(&responses).await {
		WorkflowResponse::InvokeComplete(actual) => {
			assert_eq!(actual.invocation.as_str(), "invoke-2");
			assert_eq!(
				pb::InvokeComplete::decode(actual.payload).expect("decode invoke completion"),
				complete
			);
		},
		other => panic!("expected provider invoke completion, got {other:?}"),
	}
	let second = outcome(&mut delta).await;
	assert!(second.revision.is_some());
	assert!(second.output.iter().any(|item| matches!(
		&item.kind,
		Some(thread_pb::item::Kind::ToolResult(result)) if result.call_id == "call-2"
	)));
	let invoked_call = second
		.output
		.iter()
		.find(|item| {
			matches!(
				&item.kind,
				Some(thread_pb::item::Kind::ToolCall(call)) if call.id == "call-2"
			)
		})
		.expect("accepted in-turn tool call committed");
	assert_eq!(
		invoked_call
			.props
			.as_ref()
			.and_then(|props| props.fields.get(omp_tool::TOOL_REV_PROP))
			.and_then(|value| value.kind.as_ref()),
		Some(&pb::value::Kind::String("shell.2".to_owned()))
	);
	assert_eq!(fake.calls().len(), 2);

	let mut replay = client
		.turn(first_id, TurnInput::Full(thread), &options)
		.await
		.expect("same turn id replay opens");
	assert!(matches!(
		next_event(&mut replay).await,
		Some(Ok(pb::TurnEvent {
			event: Some(pb::turn_event::Event::Accepted(pb::Accepted { replay: true })),
		}))
	));
	assert_eq!(outcome(&mut replay).await, first);
	assert_eq!(fake.calls().len(), 2, "replay must not make another provider request");

	let mut stale = first_revision.clone();
	stale.head = stale.head.saturating_add(1);
	let mut conflict = client
		.turn(
			ProviderTurnId::from("turn-conflict"),
			TurnInput::Delta(
				pb::ContextRef { context_id: "context-1".to_owned(), expected: Some(stale) },
				pb::ThreadDelta::default(),
			),
			&options,
		)
		.await
		.expect("conflict stream opens");
	match next_event(&mut conflict).await {
		Some(Err(TurnError::Conflict(error))) => {
			assert_eq!(error.actual, second.revision, "conflict must carry authoritative head");
		},
		other => panic!("expected typed conflict, got {other:?}"),
	}

	let mut need_full = client
		.turn(
			ProviderTurnId::from("turn-need-full"),
			TurnInput::Delta(
				pb::ContextRef {
					context_id: "missing-context".to_owned(),
					expected:   Some(first_revision.clone()),
				},
				pb::ThreadDelta::default(),
			),
			&options,
		)
		.await
		.expect("need-full stream opens");
	assert!(matches!(next_event(&mut need_full).await, Some(Err(TurnError::NeedFull(_)))));
	assert_eq!(fake.calls().len(), 2, "recovery errors must not reach the provider");

	let mut fork = client
		.turn(
			ProviderTurnId::from("turn-fork"),
			TurnInput::Delta(
				pb::ContextRef {
					context_id: "context-1".to_owned(),
					expected:   second.revision.clone(),
				},
				pb::ThreadDelta { truncate_to: Some(first_revision.head), append: Vec::new() },
			),
			&options,
		)
		.await
		.expect("forked delta stream opens");
	assert!(matches!(
		next_event(&mut fork).await,
		Some(Ok(pb::TurnEvent {
			event: Some(pb::turn_event::Event::Accepted(pb::Accepted { replay: false })),
		}))
	));
	let forked = outcome(&mut fork).await;
	assert_eq!(forked.provider, first.provider);
	assert_eq!(forked.model, first.model);
	assert_eq!(fake.calls().len(), 3);

	let terminal_options = TurnOptions { context_id: None, ..options.clone() };
	let mut limited = client
		.turn(
			ProviderTurnId::from("turn-rate-limited"),
			TurnInput::Full(thread_pb::Thread::default()),
			&terminal_options,
		)
		.await
		.expect("classified terminal stream opens");
	match next_event(&mut limited).await {
		Some(Err(TurnError::Terminal(error))) => {
			assert_eq!(error.kind(), pb::turn_error::Kind::RateLimited);
			assert_eq!(error.retry_after_ms, 37);
			assert!(error.detail.contains("RateLimited"));
		},
		other => panic!("expected classified terminal failure, got {other:?}"),
	}
	assert_eq!(fake.calls().len(), 4);

	drop(client);
	daemon.shutdown().await.expect("gateway shutdown");
	assert!(!socket.exists(), "gateway shutdown must remove its UDS");
}

struct HistoryTool {
	spec:      ToolSpec,
	lift_from: Option<Rev>,
}

impl Tool for HistoryTool {
	type Fault = serde_json::Value;
	type Params = serde_json::Value;
	type Payload = serde_json::Value;
	type Update = serde_json::Value;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		_params: IncomingParams<'c>,
	) -> impl futures::Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		futures::stream::empty()
	}

	fn prompt(&self, view: Result<&Self::Payload, &Self::Fault>, caps: &PromptCaps) -> Vec<Part> {
		let branch = if view.is_ok() { "ok" } else { "fault" };
		vec![Part::Text {
			text: Str::from(format!(
				"{branch}|parts={}|text={}|media={}",
				caps.maximum_parts, caps.maximum_text_bytes, caps.media
			)),
		}]
	}

	fn lift(&self, from: &Rev, call: RecordedCall<'_>) -> Option<LiftedCall> {
		if self.lift_from.as_ref() != Some(from) {
			return None;
		}
		let args: serde_json::Value = serde_json::from_slice(call.raw_args).ok()?;
		let legacy = args.get("legacy")?.clone();
		let mut verdict: serde_json::Value = serde_json::from_slice(call.verdict).ok()?;
		*verdict.get_mut("value")?.get_mut("dialect")? = serde_json::json!("hl.2");
		verdict["kind"] = serde_json::json!("faulted");
		Some(LiftedCall {
			raw_args: Bytes::from(
				serde_json::to_vec(&serde_json::json!({
					"current": legacy
				}))
				.ok()?,
			),
			verdict:  Bytes::from(serde_json::to_vec(&verdict).ok()?),
		})
	}
}

fn history_tool(n: u16, lifts_hl1: bool) -> HistoryTool {
	let schema = if n == 1 {
		Bytes::from_static(
			br#"{"type":"object","properties":{"hl1_only":{"type":"string"}},"required":["hl1_only"]}"#,
		)
	} else {
		Bytes::from_static(
			br#"{"type":"object","properties":{"hl2_only":{"type":"string"}},"required":["hl2_only"]}"#,
		)
	};
	HistoryTool {
		spec:      ToolSpec {
			name: sf!("history_law"),
			rev: Rev { family: sf!("hl"), n },
			description: Str::from(format!("history law revision {n}")),
			schema,
			constraint: Constraint::None,
			effects: Effects::empty(),
			projection_code: [0; 32],
		},
		lift_from: lifts_hl1.then(|| Rev { family: sf!("hl"), n: 1 }),
	}
}

fn history_registry(with_lift: bool) -> omp_tool::Registry {
	let mut registry = omp_tool::Registry::new();
	registry
		.register(history_tool(1, false), Presentation::Slot, test_claims())
		.expect("historical revision registers");
	registry
		.register(history_tool(2, with_lift), Presentation::Slot, test_claims())
		.expect("live revision registers");
	registry
}

fn string_value(value: &str) -> pb::Value {
	pb::Value { kind: Some(value::Kind::String(value.to_owned())) }
}

fn map_value(fields: impl IntoIterator<Item = (&'static str, pb::Value)>) -> pb::Value {
	pb::Value {
		kind: Some(value::Kind::Map(pb::ValueMap {
			fields: fields
				.into_iter()
				.map(|(key, value)| (key.to_owned(), value))
				.collect(),
		})),
	}
}

fn historical_outcome() -> pb::Outcome {
	pb::Outcome {
		output: vec![
			thread_pb::Item {
				seq:           7,
				created_at_ms: 0,
				kind:          Some(thread_pb::item::Kind::ToolCall(thread_pb::ToolCall {
					id: "historical-call".to_owned(),
					name: "history_law".to_owned(),
					args_json: Bytes::from_static(br#"{"legacy":"kept"}"#),
					..Default::default()
				})),
				props:         Some(pb::ValueMap {
					fields: BTreeMap::from([("omp/tool-rev".to_owned(), string_value("hl.1"))]),
				}),
			},
			thread_pb::Item {
				seq:           8,
				created_at_ms: 0,
				kind:          Some(thread_pb::item::Kind::ToolResult(thread_pb::ToolResult {
					call_id: "historical-call".to_owned(),
					name: "history_law".to_owned(),
					parts: vec![thread_pb::Part {
						kind: Some(thread_pb::part::Kind::Text("historical result".to_owned())),
					}],
					details: Some(map_value([
						("kind", string_value("ok")),
						("value", map_value([("dialect", string_value("hl.1"))])),
					])),
					useless: Some(true),
					..Default::default()
				})),
				props:         None,
			},
		],
		..Default::default()
	}
}

fn provider_schema_bytes(request: &omp_inference::call::ChatRequest) -> Vec<u8> {
	let [definition] = request.tools.as_ref() else {
		panic!("provider request must advertise only the live definition")
	};
	let (schema, _) = definition
		.input
		.json_schema()
		.expect("history tool uses JSON Schema");
	serde_json::to_vec(schema.as_value()).expect("provider schema serializes")
}

#[test]
fn canonical_history_uses_only_live_definitions_and_lifts_deterministically() {
	let outcome = historical_outcome();
	let thread = thread_pb::Thread { items: outcome.output };
	let params = pb::ChatParams {
		tools: vec![pb::ToolDef {
			name:        "history_law".to_owned(),
			description: "stale caller definition".to_owned(),
			input:       Some(pb::tool_def::Input::JsonSchema(pb::tool_def::JsonSchema {
				schema_json: Bytes::from_static(
					br#"{"type":"object","properties":{"hl1_only":{"type":"string"}}}"#,
				),
				strict:      Some(true),
			})),
		}],
		..Default::default()
	};

	let without_lift = history_registry(false);
	let (data, data_request) =
		omp_serve::inference::project_provider_turn_for_test(&thread, &params, &without_lift)
			.expect("unliftable history remains projectable as transcript data");
	assert_eq!(
		data.encode_to_vec(),
		thread.encode_to_vec(),
		"an incomplete lift path must preserve the exact canonical items"
	);
	let data_schema = provider_schema_bytes(&data_request);
	assert!(data_schema.windows(8).any(|window| window == b"hl2_only"));
	assert!(
		!data_schema.windows(8).any(|window| window == b"hl1_only"),
		"historical schema bytes must not enter the provider request"
	);

	let with_lift = history_registry(true);
	let (first, first_request) =
		omp_serve::inference::project_provider_turn_for_test(&thread, &params, &with_lift)
			.expect("complete lift projects history");
	let (second, second_request) =
		omp_serve::inference::project_provider_turn_for_test(&thread, &params, &with_lift)
			.expect("repeated complete lift projects history");
	assert_eq!(
		first.encode_to_vec(),
		second.encode_to_vec(),
		"repeated canonical projection must be byte-identical"
	);
	assert_eq!(provider_schema_bytes(&first_request), provider_schema_bytes(&second_request));
	let lifted_call = match first.items[0].kind.as_ref() {
		Some(item::Kind::ToolCall(call)) => call,
		other => panic!("expected lifted ToolCall, got {other:?}"),
	};
	assert_eq!(lifted_call.args_json.as_ref(), br#"{"current":"kept"}"#);
	let revision = first.items[0]
		.props
		.as_ref()
		.and_then(|props| props.fields.get("omp/tool-rev"))
		.and_then(|value| value.kind.as_ref());
	assert!(matches!(revision, Some(pb::value::Kind::String(value)) if value == "hl.2"));
	let lifted_result = match first.items[1].kind.as_ref() {
		Some(item::Kind::ToolResult(result)) => result,
		other => panic!("expected lifted ToolResult, got {other:?}"),
	};
	assert!(lifted_result.is_error, "Ok-to-Fault lift must recompute branch metadata");
	assert_eq!(
		lifted_result.useless,
		Some(true),
		"Ok-to-Fault lift preserves sibling compaction metadata"
	);
	assert!(matches!(
		lifted_result.parts.as_slice(),
		[thread_pb::Part {
			kind: Some(thread_pb::part::Kind::Text(text)),
		}] if text == "fault|parts=1|text=65536|media=false"
	));
	let lifted_schema = provider_schema_bytes(&first_request);
	assert!(lifted_schema.windows(8).any(|window| window == b"hl2_only"));
	assert!(
		!lifted_schema.windows(8).any(|window| window == b"hl1_only"),
		"historical schema bytes must not enter the provider request"
	);
	let empty_registry = omp_tool::Registry::new();
	let error = omp_serve::inference::project_provider_turn_for_test(
		&thread_pb::Thread::default(),
		&params,
		&empty_registry,
	)
	.expect_err("caller definitions cannot invent an unversioned executable tool");
	assert_eq!(error.code(), tonic::Code::FailedPrecondition);
}

#[test]
fn empty_tool_selection_advertises_no_tools() {
	let registry = history_registry(true);
	let (_, request) = omp_serve::inference::project_provider_turn_for_test(
		&thread_pb::Thread::default(),
		&pb::ChatParams::default(),
		&registry,
	)
	.expect("empty selection projects");
	assert!(
		request.tools.is_empty(),
		"empty params.tools must mean no advertised tools, not every registry tool"
	);
}
