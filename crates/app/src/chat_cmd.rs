//! Interactive terminal and native hosts for the journal-first agent kernel.

use std::{env, fs, path::PathBuf, sync::Arc};

use miette::{IntoDiagnostic as _, miette};
use omp_core::Str;
use omp_driver::discovery::roles;

use crate::cli::ChatArgs;

/// Initial surface selected by the command boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatStart {
	/// Open the transcript and composer immediately.
	Session,
	/// Open the session index before the transcript.
	///
	/// The journal-first host currently resolves `--continue`/`--resume` at the
	/// controller boundary, so this selection opens that resolved session.
	SessionIndex,
}

/// Presentation selected for the interactive project-chat session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatPresentation {
	/// Render through the inline terminal host.
	Terminal,
	/// Render through the native GPU window host.
	Gui,
}

/// Runs one interactive durable project-chat session.
#[cfg(any(unix, windows))]
#[expect(
	clippy::future_not_send,
	reason = "interactive hosts own thread-confined terminal or window scenes"
)]
pub(crate) async fn run(
	mut args: ChatArgs,
	_start: ChatStart,
	presentation: ChatPresentation,
) -> miette::Result<()> {
	if args.fork.is_some() {
		return Err(miette!("forking a session requires an explicit journal branch target"));
	}
	if args.from_claude || args.from_codex {
		crate::session_import::prepare(&mut args)?;
	}

	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let project = fs::canonicalize(&args.project).into_diagnostic()?;
	// The host's one console mailbox: bound `cl_*` commands and reply lines
	// reach the actor through it (ADR 0014).
	let ctx = Arc::new(crate::process_ctx_with(
		&project,
		omp_chat::HostMailbox::new().attach(omp_con::Ctx::builder()),
	)?);
	for overlay in &args.config {
		let script = fs::read_to_string(overlay).into_diagnostic()?;
		ctx.exec(&script, omp_con::Source::Config(Str::new(overlay.to_string_lossy())))
			.into_diagnostic()?;
	}
	if args.hide_thinking {
		omp_con::CL_SHOWTHINKING
			.set(&ctx, false)
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
		.ok_or_else(|| miette!("chat requires a configured default model role"))?;
	if args.api_key.is_some() && args.model.is_none() && args.models.is_none() {
		return Err(miette!("--api-key requires a model to be specified via --model or --models"));
	}

	let gateway = match args.gateway.as_ref() {
		Some(endpoint) => Some(endpoint.connect().await.into_diagnostic()?),
		None => None,
	};
	let (mut kernel, mut session, _) = omp_driver::headless::kernel::compose_kernel(
		&data_dir,
		&project,
		model.as_str(),
		Arc::clone(&ctx),
		omp_driver::headless::kernel::KernelOptions {
			continue_session: args.continue_session,
			session: args
				.resume
				.as_ref()
				.map(|value| PathBuf::from(value.as_str())),
			sessions_dir: args.session_dir.clone(),
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
	let ephemeral_path = args
		.no_session
		.then(|| session.journal_path().to_path_buf());
	let (snapshot, dom_events) = session.subscribe();
	let kernel_events = kernel.subscribe();
	let up = kernel.mailbox();
	let (commands, command_rx) = flume::unbounded();
	let console_bindings = crate::keybindings::config::ConsoleKeybindings::from_ctx(&ctx)
		.map_err(|source| miette!(source))?;
	let bindings = omp_chat::input::Bindings::new(console_bindings.bindings);
	let resize_policy = match omp_con::CL_RESIZE_POLICY.get(&ctx) {
		omp_con::ResizePolicy::Preserve => omp_tui::slots::ResizePolicy::Preserve,
		omp_con::ResizePolicy::Append => omp_tui::slots::ResizePolicy::Append,
		omp_con::ResizePolicy::Rebuild => omp_tui::slots::ResizePolicy::Rebuild,
	};
	let model_badge = {
		let mut badge = omp_chat::ModelBadge::from_identifier(model.as_str());
		if let Some(spec) = catalog.model(&omp_catalog::ModelKey::from(model.as_str())) {
			badge.name = spec.display_name.clone();
			badge.context_window = spec.limits.context_window;
			badge.reasoning = spec.thinking.is_some();
		}
		badge
	};
	// Picker roster and role cycle for the model keybindings (alt+p/alt+m,
	// ctrl+p): catalog facts projected once at launch, never journaled.
	let models = crate::pickers::model_rows(catalog.as_ref(), &model_settings);
	let cycle = {
		let key_of = |key: &Option<omp_catalog::ModelKey>| {
			key.as_ref().map(|key| Str::new(key.as_str()))
		};
		let by_role = [
			("smol", key_of(&launch_roles.smol)),
			("default", Some(model.clone())),
			("slow", key_of(&launch_roles.slow)),
			("plan", key_of(&launch_roles.plan)),
		];
		model_settings
			.cycle_order
			.iter()
			.filter_map(|role| {
				by_role
					.iter()
					.find(|(name, _)| *name == role.as_str())
					.and_then(|(name, key)| key.clone().map(|key| (Str::new_static(name), key)))
			})
			.collect::<Vec<_>>()
	};
	// Observer-local git fact for the status band; a missing or detached
	// checkout simply hides the segment.
	let branch = omp_vcs::git::GitRepo::discover(&project)
		.ok()
		.flatten()
		.and_then(|repo| repo.current_branch().ok().flatten())
		.map(Str::from);
	let options = omp_chat::HostOptions {
		snapshot,
		dom_events,
		kernel_events,
		commands: commands.clone(),
		up: up.clone(),
		con: Arc::clone(&ctx),
		bindings,
		models,
		cycle,
		resize_policy,
		model: model_badge,
		branch,
		ui: omp_tui::UiContext::default(),
	};
	if !args.prompt.is_empty() {
		let mut text = String::new();
		for word in &args.prompt {
			if !text.is_empty() {
				text.push(' ');
			}
			text.push_str(word.as_str());
		}
		commands
			.send(omp_chat::HostCommand::Submit(Str::new(text)))
			.into_diagnostic()?;
	}

	let controller = async move {
		let mut quit = false;
		// pi `app.plan.toggle` arriving mid-turn applies once the turn ends.
		let mut pending_plan: Option<bool> = None;
		while let Ok(command) = command_rx.recv_async().await {
			let input = match command {
				omp_chat::HostCommand::PlanMode { engage } => {
					set_plan_mode(&mut session, engage).into_diagnostic()?;
					continue;
				},
				omp_chat::HostCommand::Submit(text) => {
					omp_agent::TurnInput { text, attachments: Vec::new() }
				},
				omp_chat::HostCommand::SubmitWithAttachments { text, attachments } => {
					omp_agent::TurnInput { text, attachments }
				},
				omp_chat::HostCommand::Steer(text) => {
					let _ = up.send(omp_agent::Up::Steer(text));
					continue;
				},
				omp_chat::HostCommand::Interrupt => {
					let _ = up.send(omp_agent::Up::Interrupt);
					continue;
				},
				omp_chat::HostCommand::Approve { id, decision } => {
					let _ = up.send(omp_agent::Up::Approve { id, decision });
					continue;
				},
				omp_chat::HostCommand::Overlay { .. } => continue,
				omp_chat::HostCommand::Quit => {
					session.process_exit().into_diagnostic()?;
					break;
				},
			};
			let failure = {
				let mut failure = None;
				let turn = kernel.run_turn(&mut session, input, omp_agent::RunControl::default());
				tokio::pin!(turn);
				loop {
					tokio::select! {
						result = &mut turn => {
							failure = result.err();
							break;
						},
						command = command_rx.recv_async() => match command {
							Ok(omp_chat::HostCommand::Submit(text) | omp_chat::HostCommand::Steer(text)) => {
								let _ = up.send(omp_agent::Up::Steer(text));
							},
							Ok(omp_chat::HostCommand::SubmitWithAttachments { text, .. }) => {
								let _ = up.send(omp_agent::Up::Steer(text));
							},
							Ok(omp_chat::HostCommand::Interrupt) => {
								let _ = up.send(omp_agent::Up::Interrupt);
							},
							Ok(omp_chat::HostCommand::Approve { id, decision }) => {
								let _ = up.send(omp_agent::Up::Approve { id, decision });
							},
							Ok(omp_chat::HostCommand::Quit) | Err(_) => {
								let _ = up.send(omp_agent::Up::Cancel);
								quit = true;
							},
							Ok(omp_chat::HostCommand::Overlay { .. }) => {},
							Ok(omp_chat::HostCommand::PlanMode { engage }) => pending_plan = Some(engage),
						},
					}
					if quit {
						failure = turn.await.err();
						break;
					}
				}
				failure
			};
			if let Some(error) = failure {
				// The kernel journals the failure as a `<notice kind=error>` before
				// returning; the host renders it and the composer stays live (pi
				// keeps the session open on a failed turn).
				record_turn_failure(&mut session, &error).into_diagnostic()?;
			}
			if let Some(engage) = pending_plan.take() {
				set_plan_mode(&mut session, engage).into_diagnostic()?;
			}
			if quit {
				session.process_exit().into_diagnostic()?;
				break;
			}
		}
		Ok::<(), miette::Report>(())
	};

	#[cfg(feature = "gui")]
	if presentation == ChatPresentation::Gui {
		let controller = tokio::spawn(controller);
		crate::gui::run(options)?;
		let _ = commands.send(omp_chat::HostCommand::Quit);
		controller.await.into_diagnostic()??;
		if let Some(path) = ephemeral_path {
			let _ = fs::remove_file(path);
		}
		return Ok(());
	}
	#[cfg(not(feature = "gui"))]
	if presentation == ChatPresentation::Gui {
		return Err(miette!("native GUI support was not included in this build"));
	}

	let host = omp_chat::Host::new(options).run();
	tokio::pin!(host);
	tokio::pin!(controller);
	tokio::select! {
		host_result = &mut host => {
			host_result.into_diagnostic()?;
			let _ = commands.send(omp_chat::HostCommand::Quit);
			controller.await?;
		},
		controller_result = &mut controller => {
			controller_result?;
			host.await.into_diagnostic()?;
		},
	}
	if let Some(path) = ephemeral_path {
		let _ = fs::remove_file(path);
	}
	Ok(())
}

/// pi `app.plan.toggle`: engages the plan Director (ADR 0015 `<meta>
/// <directors>` element) or exits it by removing its frame, between turns.
#[cfg(any(unix, windows))]
fn set_plan_mode(
	session: &mut omp_session::Session,
	engage: bool,
) -> Result<(), omp_agent::DirectorError> {
	use omp_dom::{KnownTag, Op, PropKey, Tag, Txn, Value};
	const PLAN: &str = "plan";
	let registry = omp_agent::DirectorRegistry::standard();
	let mut stack = omp_agent::DirectorStack::from_dom(session.dom(), &registry);
	let engaged = stack.active_ids().contains(&PLAN);
	if engage && !engaged {
		stack.engage(
			session,
			Box::new(omp_agent::directors::plan::Plan::new("local://plans/current.md")),
		)?;
		return Ok(());
	}
	if engage || !engaged {
		return Ok(());
	}
	let dom = session.dom();
	let Some(handle) = dom
		.select("directors director[family=plan]")
		.ok()
		.and_then(|mut handles| handles.next())
		.filter(|handle| {
			dom.get(*handle).is_some_and(|node| {
				node.tag == Tag::Known(KnownTag::Director)
					&& node
						.prop(&PropKey::Custom(Str::new_static("family")))
						.and_then(Value::as_str)
						== Some(PLAN)
			})
		})
	else {
		return Ok(());
	};
	let cause = session
		.head()
		.ok_or(omp_agent::DirectorError::MissingDirectors)?;
	session.patch(Txn {
		cause,
		label: Some(Str::new_static("director.exit")),
		ops: vec![Op::Rm(handle)],
	})?;
	Ok(())
}

/// Guarantees a failed turn leaves a visible `<notice kind=error>` in its
/// turn: a no-op when the kernel already journaled one, otherwise the error
/// chain is appended and any open assistant is closed.
#[cfg(any(unix, windows))]
fn record_turn_failure(
	session: &mut omp_session::Session,
	error: &omp_agent::KernelError,
) -> Result<(), omp_session::SessionError> {
	use omp_dom::{KnownTag, NodeSpec, Op, PropId, Tag, Value};
	tracing::warn!(%error, "turn failed");
	let dom = session.dom();
	let Some(turn) = dom.children(dom.body()).last().copied() else {
		return Ok(());
	};
	let already = dom
		.children(turn)
		.last()
		.and_then(|handle| dom.get(*handle))
		.is_some_and(|node| {
			node.tag == Tag::Known(KnownTag::Notice)
				&& node.prop(&PropId::Kind.into()).and_then(Value::as_str) == Some("error")
		});
	if already {
		return Ok(());
	}
	let _ = session.assistant_end("error");
	let mut text = error.to_string();
	let mut source = std::error::Error::source(error);
	while let Some(cause) = source {
		text.push_str("\n  caused by: ");
		text.push_str(&cause.to_string());
		source = cause.source();
	}
	let Some(cause) = session.head() else {
		return Ok(());
	};
	session.patch(omp_dom::Txn {
		cause,
		label: Some(Str::new_static("chat.turn-failure")),
		ops: vec![Op::Ins {
			parent: turn,
			after:  session.dom().children(turn).last().copied(),
			node:   NodeSpec::new(KnownTag::Notice)
				.with_prop(PropId::Kind, Value::Str(Str::new_static("error")))
				.with_content(Str::new(text)),
		}],
	})?;
	Ok(())
}

/// Reports the platform limitation before touching project state.
#[cfg(not(any(unix, windows)))]
pub(crate) async fn run(
	_args: ChatArgs,
	_start: ChatStart,
	_presentation: ChatPresentation,
) -> miette::Result<()> {
	Err(miette!("interactive chat is not supported on this platform"))
}
