//! Single-shot adapter over the journal-first production agent kernel.

use std::{fs, io::IsTerminal as _, path::PathBuf, sync::Arc, time::Instant};

use miette::{IntoDiagnostic as _, miette};
use omp_agent::{RunControl, TurnInput, TurnStop};
use omp_core::{FastHashMap, Str};
use omp_dom::{Dom, Event, KnownTag, Op, PropId, PropKey, Sid, StreamOp, Tag, Value};
use omp_driver::{
	discovery::roles,
	headless::kernel::{KernelOptions, compose_kernel},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

use crate::{cli::PrintArgs, usage_error::CliUsageError};

/// Runs prompts through the new durable headless kernel.
pub async fn run(args: PrintArgs) -> miette::Result<()> {
	let Some(max_time) = args.max_time.map(|duration| duration.0) else {
		return run_inner(args).await;
	};
	match tokio::time::timeout(max_time, run_inner(args)).await {
		Ok(result) => result,
		Err(_) => Err(miette!("print mode exceeded --max-time")),
	}
}

async fn run_inner(args: PrintArgs) -> miette::Result<()> {
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let project = fs::canonicalize(&args.project).into_diagnostic()?;
	let ctx = Arc::new(crate::process_ctx(&project)?);
	for overlay in &args.config {
		let script = fs::read_to_string(overlay).into_diagnostic()?;
		ctx.exec(&script, omp_con::Source::Config(Str::new(overlay.to_string_lossy())))
			.into_diagnostic()?;
	}
	let home = std::env::var_os("HOME").map_or_else(|| project.clone(), PathBuf::from);
	let model_settings =
		omp_catalog::settings::ModelSettings::from_con(&ctx).resolve_path_scopes(&project, &home);
	let catalog =
		omp_driver::registry::production_catalog(&data_dir).map_err(|source| miette!(source))?;
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
		.or_else(|| launch_roles.primary.map(|model| Str::from(model.as_str())))
		.ok_or_else(|| miette!("print mode requires a configured default model role"))?;
	if args.api_key.is_some() && args.model.is_none() && args.models.is_none() {
		return Err(miette!("--api-key requires a model to be specified via --model or --models"));
	}
	if args.fork.is_some() || args.from_claude || args.from_codex {
		return Err(miette!(
			"print on the new spine does not accept legacy session imports or forks"
		));
	}

	let initial = initial_prompt(&args.prompt).await?;
	if initial.is_empty() {
		return Err(
			CliUsageError::new("print mode requires a prompt or piped standard input").into(),
		);
	}
	let explicit_session = args
		.resume
		.as_ref()
		.map(|value| PathBuf::from(value.as_str()));
	let (mut kernel, mut session, _) =
		compose_kernel(&data_dir, &project, model.as_str(), Arc::clone(&ctx), KernelOptions {
			continue_session:   args.continue_session,
			session:            explicit_session,
			sessions_dir:       args.session_dir.clone(),
			ephemeral:          args.no_session,
			no_tools:           args.no_tools,
			py_eval:            args.py_eval,
			spawn_idle_timeout: args.envd_idle_timeout,
			api_key:            args.api_key.clone(),
			provider:           args
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
			gateway:            None,
			sessions:           None,
			session_name:       None,
			tool_registry:      None,
		})
		.await
		.into_diagnostic()?;
	let ephemeral_path = args
		.no_session
		.then(|| session.journal_path().to_path_buf());
	let (snapshot, events) = session.subscribe();
	let mut replica = Dom::from_snapshot(&snapshot);
	let mut streams = FastHashMap::default();
	let mut stdout = tokio::io::stdout();
	let mut prompts = Vec::with_capacity(1 + args.follow_ups.len());
	prompts.push(initial);
	prompts.extend(args.follow_ups.iter().cloned());

	for prompt in prompts {
		let deadline = args.max_time.map(|duration| Instant::now() + duration.0);
		let control = RunControl::new(CancellationToken::new(), deadline);
		let turn = kernel.run_turn(
			&mut session,
			TurnInput { text: prompt, attachments: Vec::new() },
			control,
		);
		tokio::pin!(turn);
		let mut ended_with_newline = true;
		let outcome = loop {
			tokio::select! {
				biased;
				event = events.recv_async() => {
					if let Ok(event) = event {
						print_event(
							&mut stdout,
							&args,
							&mut replica,
							&mut streams,
							event,
							&mut ended_with_newline,
						).await?;
					}
				},
				result = &mut turn => break result.into_diagnostic()?,
			}
		};
		while let Ok(event) = events.try_recv() {
			print_event(
				&mut stdout,
				&args,
				&mut replica,
				&mut streams,
				event,
				&mut ended_with_newline,
			)
			.await?;
		}
		if args.mode == "text" && !ended_with_newline {
			stdout.write_all(b"\n").await.into_diagnostic()?;
		}
		stdout.flush().await.into_diagnostic()?;
		if outcome.stop != TurnStop::Completed {
			return Err(miette!("print turn stopped before completion: {:?}", outcome.stop));
		}
	}

	drop(session);
	if let Some(path) = ephemeral_path {
		let _ = fs::remove_file(path);
	}
	Ok(())
}

#[derive(Clone, Copy)]
enum PrintedStream {
	Text,
	Thinking,
}

async fn print_event(
	stdout: &mut tokio::io::Stdout,
	args: &PrintArgs,
	replica: &mut Dom,
	streams: &mut FastHashMap<Sid, PrintedStream>,
	event: Event,
	ended_with_newline: &mut bool,
) -> miette::Result<()> {
	let mut text = None;
	let mut tool = None;
	match &event {
		Event::Patch(patch) => {
			for op in &patch.ops {
				if let Op::Ins { parent, node, .. } = op
					&& replica
						.get(*parent)
						.is_some_and(|parent| parent.tag == Tag::Known(KnownTag::Turn))
					&& let Tag::Custom(name) = &node.tag
				{
					tool = Some(name.clone());
					break;
				}
			}
		},
		Event::Stream { sid, op: StreamOp::Open, node: Some(node), prop: Some(prop), .. } => {
			if replica
				.get(*node)
				.is_some_and(|node| node.tag == Tag::Known(KnownTag::Assistant))
			{
				let kind = if *prop == PropKey::Known(PropId::Text) {
					Some(PrintedStream::Text)
				} else if *prop == PropKey::Known(PropId::Thinking) {
					Some(PrintedStream::Thinking)
				} else {
					None
				};
				if let Some(kind) = kind {
					streams.insert(*sid, kind);
				}
			}
		},
		Event::Stream { sid, op: StreamOp::Append, text: Some(delta), .. } => {
			if streams.get(sid).is_some_and(|kind| {
				matches!(kind, PrintedStream::Text)
					|| args.print_thoughts && matches!(kind, PrintedStream::Thinking)
			}) {
				text = Some(delta.clone());
			}
		},
		Event::Stream { sid, op: StreamOp::Close, .. } => {
			streams.remove(sid);
		},
		Event::Reset { .. } | Event::Stream { .. } => {},
	}
	replica.apply_event(&event).into_diagnostic()?;

	if args.mode == "json" {
		let value = if let Some(text) = text {
			serde_json::json!({"type":"text_delta","text":text})
		} else if let Some(name) = tool {
			serde_json::json!({"type":"tool_call","name":name})
		} else {
			return Ok(());
		};
		let mut line = serde_json::to_vec(&value).into_diagnostic()?;
		line.push(b'\n');
		stdout.write_all(&line).await.into_diagnostic()?;
		*ended_with_newline = true;
		return Ok(());
	}
	if let Some(text) = text {
		stdout.write_all(text.as_bytes()).await.into_diagnostic()?;
		*ended_with_newline = text.ends_with('\n');
	}
	if let Some(name) = tool {
		if !*ended_with_newline {
			stdout.write_all(b"\n").await.into_diagnostic()?;
		}
		stdout
			.write_all(format!("[tool: {name}]\n").as_bytes())
			.await
			.into_diagnostic()?;
		*ended_with_newline = true;
	}
	Ok(())
}

async fn initial_prompt(words: &[Str]) -> miette::Result<Str> {
	if !words.is_empty() {
		return Ok(Str::new(words.iter().map(Str::as_str).collect::<Vec<_>>().join(" ")));
	}
	if std::io::stdin().is_terminal() {
		return Ok(Str::default());
	}
	let mut input = String::new();
	tokio::io::stdin()
		.read_to_string(&mut input)
		.await
		.into_diagnostic()?;
	Ok(Str::new(input))
}

/// Projects the plain headless transcript from the authoritative session DOM.
#[must_use]
pub fn transcript_text(dom: &Dom) -> String {
	let mut output = String::new();
	for turn in dom.children(dom.body()) {
		for handle in dom.children(*turn) {
			let Some(node) = dom.get(*handle) else {
				continue;
			};
			match &node.tag {
				Tag::Known(KnownTag::Assistant) => {
					if let Some(Value::Str(text)) = node.prop(&PropId::Text.into()) {
						output.push_str(text.as_str());
						if !text.is_empty() && !text.ends_with('\n') {
							output.push('\n');
						}
					}
				},
				Tag::Custom(name) => {
					output.push_str("[tool: ");
					output.push_str(name.as_str());
					output.push_str("]\n");
				},
				_ => {},
			}
		}
	}
	output
}
