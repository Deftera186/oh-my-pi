//! Agent Client Protocol adapter over the journal-first kernel and session.

use std::{env, fs, path::PathBuf, sync::Arc};

use miette::{IntoDiagnostic as _, miette};
use omp_agent::{
	ApprovalDecision, ApprovalScope, ApprovalSource, Inference, Kernel, RunControl, TurnInput, Up,
};
use omp_core::Str;
use omp_dom::Event;
use omp_driver::discovery::roles;
use omp_session::Session;
use serde_json::{Map, Value, json};
use tokio::io::{
	AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader, stdin, stdout,
};

use crate::cli::{AcpArgs, ChatArgs};

/// Runs ACP using stdin for NDJSON requests and stdout for NDJSON responses.
pub async fn run(args: AcpArgs) -> miette::Result<()> {
	let max_time = args.max_time.map(|duration| duration.0);
	let future = run_inner(args.launch);
	match max_time {
		Some(limit) => tokio::time::timeout(limit, future)
			.await
			.map_err(|_| miette!("ACP mode exceeded --max-time"))?,
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
		.ok_or_else(|| miette!("ACP mode requires a configured default model role"))?;
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
	serve_acp(kernel, session, stdin(), stdout()).await
}

/// Serves ACP over caller-provided NDJSON transport halves.
#[doc(hidden)]
pub async fn serve_acp<C, R, W>(
	mut kernel: Kernel<C>,
	mut session: Session,
	input: R,
	mut output: W,
) -> miette::Result<()>
where
	C: Inference + Send + 'static,
	R: AsyncRead + Unpin,
	W: AsyncWrite + Unpin + Send + 'static,
{
	let session_id = session
		.journal_path()
		.file_stem()
		.and_then(|value| value.to_str())
		.map_or_else(|| Str::new_static("session"), Str::new);
	let (output_tx, output_rx) = flume::unbounded::<Value>();
	let writer = tokio::spawn(async move {
		while let Ok(value) = output_rx.recv_async().await {
			let mut bytes = serde_json::to_vec(&value).into_diagnostic()?;
			bytes.push(b'\n');
			output.write_all(&bytes).await.into_diagnostic()?;
			output.flush().await.into_diagnostic()?;
		}
		Ok::<(), miette::Report>(())
	});
	let (_, events) = session.subscribe();
	let patch_tx = output_tx.clone();
	let patch_session_id = session_id.clone();
	let forwarder = tokio::spawn(async move {
		while let Ok(event) = events.recv_async().await {
			if patch_tx
				.send(acp_event_value(&patch_session_id, event)?)
				.is_err()
			{
				break;
			}
		}
		Ok::<(), miette::Report>(())
	});
	let mailbox = kernel.mailbox();
	let mut initialized = false;
	let mut lines = BufReader::new(input).lines();
	while let Some(line) = lines.next_line().await.into_diagnostic()? {
		if line.trim().is_empty() {
			continue;
		}
		let frame: Value = match serde_json::from_str(&line) {
			Ok(frame) => frame,
			Err(source) => {
				output_tx
					.send(error(Value::Null, -32700, &source.to_string()))
					.into_diagnostic()?;
				continue;
			},
		};
		let id = frame.get("id").cloned();
		let Some(method) = frame.get("method").and_then(Value::as_str) else {
			if let Some(id) = id {
				output_tx
					.send(error(id, -32600, "request has no method"))
					.into_diagnostic()?;
			}
			continue;
		};
		let params = frame
			.get("params")
			.and_then(Value::as_object)
			.cloned()
			.unwrap_or_default();
		if method != "initialize" && !initialized {
			if let Some(id) = id {
				output_tx
					.send(error(id, -32002, "initialize must complete before other requests"))
					.into_diagnostic()?;
			}
			continue;
		}
		let result = match method {
			"initialize" => {
				let version = params
					.get("protocolVersion")
					.and_then(Value::as_u64)
					.unwrap_or(1);
				if version != 1 {
					Err((-32602, "unsupported ACP protocol version"))
				} else {
					initialized = true;
					Ok(json!({
						"protocolVersion": 1,
						"agentInfo": {
							"name": "oh-my-pi",
							"title": "Oh My Pi",
							"version": env!("CARGO_PKG_VERSION"),
						},
						"authMethods": [],
						"agentCapabilities": {
							"loadSession": true,
							"sessionCapabilities": {"resume": {}, "close": {}},
							"promptCapabilities": {"image": false, "embeddedContext": false},
						},
					}))
				}
			},
			"authenticate" => Ok(json!({})),
			"session/new" | "session/load" | "session/resume" => Ok(json!({
				"sessionId": session_id,
				"modes": {"currentModeId": "default", "availableModes": []},
				"models": {"currentModelId": "configured", "availableModels": []},
			})),
			"session/prompt" => match prompt_text(&params) {
				Ok(text) => match kernel
					.run_turn(
						&mut session,
						TurnInput { text, attachments: Vec::new() },
						RunControl::default(),
					)
					.await
				{
					Ok(outcome) => Ok(json!({
						"stopReason": if outcome.stop == omp_agent::TurnStop::Cancelled {
							"cancelled"
						} else {
							"end_turn"
						},
						"text": outcome.assistant_text,
					})),
					Err(_) => Err((-32000, "agent turn failed")),
				},
				Err(message) => Err((-32602, message)),
			},
			"session/cancel" => {
				let _ = mailbox.send(Up::Interrupt);
				Ok(json!({}))
			},
			"session/approve" => match approval(&params) {
				Ok((id, decision)) => {
					let _ = mailbox.send(Up::Approve { id, decision });
					Ok(json!({}))
				},
				Err(message) => Err((-32602, message)),
			},
			"session/close" | "shutdown" => {
				if let Some(id) = id {
					output_tx.send(success(id, json!({}))).into_diagnostic()?;
				}
				break;
			},
			_ => Err((-32601, "unknown ACP method")),
		};
		if let Some(id) = id {
			let response = match result {
				Ok(value) => success(id, value),
				Err((code, message)) => error(id, code, message),
			};
			output_tx.send(response).into_diagnostic()?;
		}
	}
	session.process_exit().into_diagnostic()?;
	drop(session);
	drop(output_tx);
	forwarder.await.into_diagnostic()??;
	writer.await.into_diagnostic()??;
	Ok(())
}

fn prompt_text(params: &Map<String, Value>) -> Result<Str, &'static str> {
	if let Some(text) = params
		.get("prompt")
		.or_else(|| params.get("message"))
		.and_then(Value::as_str)
	{
		return Ok(Str::new(text));
	}
	let Some(parts) = params.get("prompt").and_then(Value::as_array) else {
		return Err("session/prompt requires a prompt");
	};
	let mut text = String::new();
	for part in parts {
		if part.get("type").and_then(Value::as_str) == Some("text")
			&& let Some(value) = part.get("text").and_then(Value::as_str)
		{
			text.push_str(value);
		}
	}
	if text.is_empty() {
		Err("prompt contains no text")
	} else {
		Ok(Str::new(text))
	}
}

fn approval(params: &Map<String, Value>) -> Result<(Str, ApprovalDecision), &'static str> {
	let id = params
		.get("promptId")
		.or_else(|| params.get("id"))
		.and_then(Value::as_str)
		.ok_or("session/approve requires promptId")?;
	let approved = params
		.get("approved")
		.and_then(Value::as_bool)
		.unwrap_or(false);
	let scope = params
		.get("scope")
		.and_then(Value::as_str)
		.unwrap_or("once")
		.parse::<ApprovalScope>()
		.expect("approval scope parsing is infallible");
	Ok((Str::new(id), ApprovalDecision {
		approved,
		scope,
		source: ApprovalSource::External,
		decided_by: None,
		reason: None,
		audited: false,
	}))
}

fn acp_event_value(session_id: &str, event: Event) -> miette::Result<Value> {
	let update = match event {
		Event::Patch(patch) => json!({
			"sessionUpdate": "patch",
			"event": "patch@1",
			"data": serde_json::to_value(patch).into_diagnostic()?,
		}),
		Event::Reset { snapshot } => json!({
			"sessionUpdate": "snapshot",
			"data": serde_json::from_slice::<Value>(snapshot.as_bytes()).into_diagnostic()?,
		}),
		Event::Stream { cause, sid, op, node, prop, text } => json!({
			"sessionUpdate": "patch",
			"event": "stream@1",
			"data": {"cause": cause, "sid": sid, "op": op, "node": node, "prop": prop, "text": text},
		}),
	};
	Ok(json!({
		"jsonrpc": "2.0",
		"method": "session/update",
		"params": {"sessionId": session_id, "update": update},
	}))
}

fn success(id: Value, result: Value) -> Value {
	json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, code: i64, message: &str) -> Value {
	json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
