//! Stateful JSON-line RPC actor over the journal-first kernel and session DOM.

use std::{env, fs, path::PathBuf, sync::Arc};

use miette::{IntoDiagnostic as _, miette};
use omp_agent::{Inference, Kernel, RunControl, TurnInput, Up};
use omp_core::Str;
use omp_dom::Event;
use omp_driver::discovery::roles;
use omp_rpc::{
	framing::{JsonLineDecoder, MAX_FRAME_BYTES, MAX_REASSEMBLED_BYTES},
	protocol::{
		PROTOCOL_V1, PROTOCOL_V2, ReadyFrame, RequestId, RpcErrorCode, RpcRequest, RpcResponse,
	},
};
use omp_session::Session;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, stdin, stdout};

use crate::cli::{ChatArgs, RpcArgs};

/// Runs the RPC server using stdin exclusively for protocol input and stdout
/// exclusively for protocol output.
pub async fn run(args: RpcArgs, _ui_enabled: bool) -> miette::Result<()> {
	let max_time = args.max_time.map(|duration| duration.0);
	let future = run_inner(args.launch);
	match max_time {
		Some(limit) => tokio::time::timeout(limit, future)
			.await
			.map_err(|_| miette!("RPC mode exceeded --max-time"))?,
		None => future.await,
	}
}

async fn run_inner(args: ChatArgs) -> miette::Result<()> {
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let project = fs::canonicalize(&args.project).into_diagnostic()?;
	let ctx = Arc::new(crate::process_ctx(&project)?);
	for overlay in &args.config {
		let script = fs::read_to_string(overlay).into_diagnostic()?;
		ctx.exec(&script, omp_con::Source::Config(Str::new(overlay.to_string_lossy())))
			.into_diagnostic()?;
	}
	let home = env::var_os("HOME").map_or_else(|| project.clone(), PathBuf::from);
	let model_settings =
		omp_catalog::settings::ModelSettings::from_con(&ctx).resolve_path_scopes(&project, &home);
	let catalog = if args.gateway.is_some() {
		Arc::new(omp_catalog::snapshot::Catalog::embedded().clone())
	} else {
		omp_driver::registry::production_catalog(&data_dir).map_err(|source| miette!(source))?
	};
	let launch_roles = roles::resolve_launch_roles(
		catalog.as_ref(),
		&model_settings,
		None,
		args.smol.as_deref(),
		args.slow.as_deref(),
		args.plan.as_deref(),
	)
	.map_err(|source| miette!(source))?;
	let model = args
		.model
		.clone()
		.or_else(|| launch_roles.primary.map(|value| Str::from(value.as_str())))
		.ok_or_else(|| miette!("rpc mode requires a configured default model role"))?;
	let gateway = match args.gateway.as_ref() {
		Some(endpoint) => Some(endpoint.connect().await.into_diagnostic()?),
		None => None,
	};
	let (kernel, session, _) = omp_driver::headless::kernel::compose_kernel(
		&data_dir,
		&project,
		model.as_str(),
		ctx,
		omp_driver::headless::kernel::KernelOptions {
			continue_session: args.continue_session,
			session: args
				.resume
				.as_ref()
				.map(|value| PathBuf::from(value.as_str())),
			sessions_dir: args.session_dir,
			ephemeral: args.no_session,
			no_tools: args.no_tools,
			py_eval: args.py_eval,
			spawn_idle_timeout: args.envd_idle_timeout,
			api_key: args.api_key.clone(),
			provider: args
				.provider
				.as_ref()
				.map(|value| omp_catalog::ProviderId::from(value.as_str()))
				.or_else(|| {
					args.api_key.as_ref().and_then(|_| {
						model
							.split_once('/')
							.map(|(provider, _)| omp_catalog::ProviderId::from(provider))
					})
				}),
			gateway,
			sessions: None,
			session_name: None,
			tool_registry: None,
		},
	)
	.await
	.into_diagnostic()?;
	serve_rpc(kernel, session, stdin(), stdout()).await
}

/// Serves RPC over caller-provided transport halves.
///
/// Exposed for the joined scripted-kernel transport proof. Production passes
/// stdio; tests pass an in-memory duplex stream through this exact path.
#[doc(hidden)]
pub async fn serve_rpc<C, R, W>(
	mut kernel: Kernel<C>,
	mut session: Session,
	mut input: R,
	mut output: W,
) -> miette::Result<()>
where
	C: Inference + Send + 'static,
	R: AsyncRead + Unpin + Send + 'static,
	W: AsyncWrite + Unpin + Send + 'static,
{
	let (frames_tx, frames_rx) = flume::unbounded::<Value>();
	let writer = tokio::spawn(async move {
		while let Ok(value) = frames_rx.recv_async().await {
			let mut bytes = serde_json::to_vec(&value).into_diagnostic()?;
			bytes.push(b'\n');
			output.write_all(&bytes).await.into_diagnostic()?;
			output.flush().await.into_diagnostic()?;
		}
		Ok::<(), miette::Report>(())
	});
	frames_tx
		.send(
			serde_json::to_value(ReadyFrame::v2_capable(MAX_FRAME_BYTES, MAX_REASSEMBLED_BYTES))
				.into_diagnostic()?,
		)
		.into_diagnostic()?;

	let (snapshot, events) = session.subscribe();
	frames_tx
		.send(json!({
			"type": "snapshot",
			"snapshot": serde_json::from_slice::<Value>(snapshot.as_bytes()).into_diagnostic()?,
		}))
		.into_diagnostic()?;
	let event_tx = frames_tx.clone();
	let event_forwarder = tokio::spawn(async move {
		while let Ok(event) = events.recv_async().await {
			if event_tx.send(dom_event_value(event)?).is_err() {
				break;
			}
		}
		Ok::<(), miette::Report>(())
	});

	let mailbox = kernel.mailbox();
	let mut decoder = JsonLineDecoder::new();
	let mut buffer = [0_u8; 16 * 1024];
	'transport: loop {
		let count = input.read(&mut buffer).await.into_diagnostic()?;
		if count == 0 {
			break;
		}
		let batch = decoder.push(&buffer[..count]);
		for diagnostic in batch.diagnostics {
			frames_tx
				.send(error_frame(None, "transport", "invalid_frame", diagnostic.reason))
				.into_diagnostic()?;
		}
		for bytes in batch.frames {
			let request = match serde_json::from_slice::<RpcRequest>(&bytes) {
				Ok(request) => request,
				Err(source) => {
					frames_tx
						.send(error_frame(None, "parse", "invalid_request", &source.to_string()))
						.into_diagnostic()?;
					continue;
				},
			};
			let id = request.id.clone();
			let command = request.command.clone();
			let response = match command.as_str() {
				"negotiate_protocol" => negotiate(id, &request.params),
				"prompt" => {
					let text = request
						.params
						.get("message")
						.or_else(|| request.params.get("text"))
						.and_then(Value::as_str);
					match text {
						Some(text) => {
							match kernel
								.run_turn(
									&mut session,
									TurnInput { text: Str::new(text), attachments: Vec::new() },
									RunControl::default(),
								)
								.await
							{
								Ok(outcome) => RpcResponse::success(
									id,
									command.as_str(),
									json!({
										"cancelled": outcome.stop == omp_agent::TurnStop::Cancelled,
										"steered": outcome.stop == omp_agent::TurnStop::Steered,
										"text": outcome.assistant_text,
										"tokensIn": outcome.tokens_in,
										"tokensOut": outcome.tokens_out,
									}),
								)
								.into_diagnostic()?,
								Err(source) => RpcResponse::error(
									id,
									command.as_str(),
									source.to_string(),
									Some(RpcErrorCode::new("agent_error")),
								),
							}
						},
						None => RpcResponse::error(
							id,
							command.as_str(),
							"prompt requires `message` or `text`",
							Some(RpcErrorCode::new("invalid_params")),
						),
					}
				},
				"steer" => up_response(id, command.as_str(), &request.params, &mailbox, true),
				"interrupt" | "abort" => {
					let _ = mailbox.send(Up::Interrupt);
					RpcResponse::success_empty(id, command.as_str())
				},
				"cancel" => {
					let _ = mailbox.send(Up::Cancel);
					RpcResponse::success_empty(id, command.as_str())
				},
				"get_state" => RpcResponse::success(
					id,
					command.as_str(),
					serde_json::from_slice::<Value>(session.dom().snapshot().as_bytes())
						.into_diagnostic()?,
				)
				.into_diagnostic()?,
				"quit" | "shutdown" => {
					let response = RpcResponse::success_empty(id, command.as_str());
					frames_tx
						.send(serde_json::to_value(response).into_diagnostic()?)
						.into_diagnostic()?;
					break 'transport;
				},
				_ => RpcResponse::error(
					id,
					command.as_str(),
					"unknown RPC command",
					Some(RpcErrorCode::new("unknown_command")),
				),
			};
			frames_tx
				.send(serde_json::to_value(response).into_diagnostic()?)
				.into_diagnostic()?;
		}
	}
	if !decoder.remainder().is_empty() {
		frames_tx
			.send(error_frame(None, "transport", "truncated_frame", "input ended mid-frame"))
			.into_diagnostic()?;
	}
	session.process_exit().into_diagnostic()?;
	drop(session);
	drop(frames_tx);
	event_forwarder.await.into_diagnostic()??;
	writer.await.into_diagnostic()??;
	Ok(())
}

fn negotiate(id: Option<RequestId>, params: &Map<String, Value>) -> RpcResponse {
	let version = params.get("protocolVersion").and_then(Value::as_u64);
	if matches!(version, Some(value) if value == u64::from(PROTOCOL_V1) || value == u64::from(PROTOCOL_V2))
	{
		RpcResponse::success(id, "negotiate_protocol", json!({ "protocolVersion": version }))
			.expect("static protocol response serializes")
	} else {
		RpcResponse::error(
			id,
			"negotiate_protocol",
			"only protocol versions 1 and 2 are supported",
			Some(RpcErrorCode::new(RpcErrorCode::UNSUPPORTED_PROTOCOL)),
		)
	}
}

fn up_response(
	id: Option<RequestId>,
	command: &str,
	params: &Map<String, Value>,
	mailbox: &flume::Sender<Up>,
	steer: bool,
) -> RpcResponse {
	let text = params
		.get("message")
		.or_else(|| params.get("text"))
		.and_then(Value::as_str);
	match text {
		Some(text) => {
			if steer {
				let _ = mailbox.send(Up::Steer(Str::new(text)));
			}
			RpcResponse::success(id, command, json!({ "queued": true }))
				.expect("static steering response serializes")
		},
		None => RpcResponse::error(
			id,
			command,
			"steer requires `message` or `text`",
			Some(RpcErrorCode::new("invalid_params")),
		),
	}
}

fn dom_event_value(event: Event) -> miette::Result<Value> {
	match event {
		Event::Patch(patch) => Ok(json!({
			"type": "session_event",
			"event": "patch@1",
			"data": serde_json::to_value(patch).into_diagnostic()?,
		})),
		Event::Reset { snapshot } => Ok(json!({
			"type": "snapshot",
			"snapshot": serde_json::from_slice::<Value>(snapshot.as_bytes()).into_diagnostic()?,
		})),
		Event::Stream { cause, sid, op, node, prop, text } => Ok(json!({
			"type": "session_event",
			"event": "stream@1",
			"data": {
				"cause": cause,
				"sid": sid,
				"op": op,
				"node": node,
				"prop": prop,
				"text": text,
			},
		})),
	}
}

fn error_frame(id: Option<RequestId>, command: &str, code: &str, message: &str) -> Value {
	serde_json::to_value(RpcResponse::error(id, command, message, Some(RpcErrorCode::new(code))))
		.expect("RPC error envelope serializes")
}
