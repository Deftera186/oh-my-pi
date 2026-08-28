//! Standalone fullscreen Git workbench command.

use std::{
	collections::VecDeque,
	env,
	io::{self, IsTerminal as _},
	path::PathBuf,
	time::Duration,
};

use clap::Args;
use miette::{IntoDiagnostic as _, miette};
use omp_chat_ui::git::{GitIntent, GitWorkbench, GitWorkbenchEvent};
use omp_tui::{
	Chord, Frame, InputEvent, Key, Mods, Renderer, Size, Terminal, TerminalEvent, TerminalOptions,
	TtyOut,
};
use tokio_util::sync::CancellationToken;

use crate::{
	chat_ui::terminal_ui_context,
	git_tui::{GitSession, model::GitModelError},
};

/// Arguments for the standalone Git workbench.
#[derive(Args, Clone, Debug)]
pub struct GitArgs {
	/// Pin the view to one commit (any revision, e.g. HEAD~2 or a sha).
	pub revision: Option<String>,
	/// Run in another directory.
	#[arg(short = 'C', value_name = "DIR")]
	pub dir:      Option<PathBuf>,
}

/// Runs the standalone fullscreen Git workbench.
pub async fn run(args: GitArgs) -> miette::Result<()> {
	if !omp_tui::tty_overridden() && (!io::stdin().is_terminal() || !io::stdout().is_terminal()) {
		return Err(miette!("omp git is interactive and requires a TTY"));
	}
	let cwd = match args.dir {
		Some(path) => path,
		None => env::current_dir().into_diagnostic()?,
	};
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let _settings = omp_driver::settings::current(&data_dir).into_diagnostic()?;
	let cancel = CancellationToken::new();
	let session = GitSession::open(&cwd, args.revision.as_deref(), cancel.clone())
		.await
		.map_err(|error| miette!(git_open_error(&error)))?;
	let snapshot = session
		.initial_snapshot()
		.await
		.map_err(|error| miette!(git_open_error(&error)))?;

	let mut terminal = Terminal::enter(TerminalOptions::default().mouse(true)).into_diagnostic()?;
	let mut renderer = Renderer::new(TtyOut::new().into_diagnostic()?);
	renderer.apply_caps(&terminal.caps()).into_diagnostic()?;
	terminal.enter_alt().into_diagnostic()?;
	let previous_keymap = install_git_keymap(&mut terminal);
	let mut viewport = terminal.size().into_diagnostic()?;
	let context = terminal_ui_context(&terminal.caps());
	let mut workbench = GitWorkbench::open(snapshot, &context);
	let (update_tx, update_rx) = flume::unbounded();
	if let Some(intent) = workbench.initial_intent()
		&& dispatch(&session, &mut workbench, intent, &update_tx).await
	{
		cancel.cancel();
		terminal.edit_keymap(|keymap| *keymap = previous_keymap);
		return Ok(());
	}
	spawn_deferred_stats(&session, &update_tx);
	let mut refresh = tokio::time::interval(Duration::from_secs(2));
	refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	refresh.tick().await;

	loop {
		paint(&mut workbench, &mut renderer, viewport).into_diagnostic()?;
		tokio::select! {
			update = update_rx.recv_async() => {
				if let Ok(update) = update
					&& apply_update(&session, &mut workbench, update, &update_tx).await
				{
					break;
				}
			},
			_ = refresh.tick() => {
				if let Some(snapshot) = session.poll_refresh().await.map_err(|error| miette!(error))? {
					let close = apply_update(
						&session,
						&mut workbench,
						omp_chat_ui::git::GitUpdate::Snapshot(snapshot),
						&update_tx,
					)
					.await;
					spawn_deferred_stats(&session, &update_tx);
					if close {
						break;
					}
				}
			},
			event = terminal.next() => match event.into_diagnostic()? {
				TerminalEvent::Resize => {
					viewport = match terminal.take_resize().into_diagnostic()? {
						Some(size) => size,
						None => terminal.size().into_diagnostic()?,
					};
				},
				TerminalEvent::Input(InputEvent::Key(Key::Ctrl('c'))) => break,
				TerminalEvent::Input(InputEvent::Key(key)) => {
					let event = workbench.handle_key(key);
					if route_event(&session, &mut workbench, event, &update_tx).await {
						break;
					}
				},
				TerminalEvent::Input(InputEvent::Paste(text)) => {
					let event = workbench.handle_paste(&text);
					if route_event(&session, &mut workbench, event, &update_tx).await {
						break;
					}
				},
				TerminalEvent::Input(InputEvent::Mouse(report)) => {
					let event = workbench.handle_mouse(report.col, report.row, report.kind, viewport);
					if route_event(&session, &mut workbench, event, &update_tx).await {
						break;
					}
				},
				TerminalEvent::Input(event) => {
					let _ = terminal.handle_input_event(&event, &mut renderer).into_diagnostic()?;
				},
				TerminalEvent::Debug(_) | TerminalEvent::Effect(_) => {},
				TerminalEvent::Closed => break,
			},
		}
	}
	cancel.cancel();
	terminal.edit_keymap(|keymap| *keymap = previous_keymap);
	Ok(())
}

fn install_git_keymap(terminal: &mut Terminal) -> omp_tui::Keymap {
	let alt = Mods { alt: true, ..Default::default() };
	let super_alt = Mods { alt: true, super_key: true, ..Default::default() };
	let mut previous = None;
	terminal.edit_keymap(|keymap| {
		previous = Some(keymap.clone());
		for mods in [alt, super_alt] {
			keymap.bind(Chord::new(Key::Up, mods), Key::JumpPrevious);
			keymap.bind(Chord::new(Key::Down, mods), Key::JumpNext);
		}
	});
	previous.expect("terminal keymap was captured before Git bindings")
}

async fn route_event(
	session: &GitSession,
	workbench: &mut GitWorkbench,
	event: GitWorkbenchEvent,
	updates: &flume::Sender<omp_chat_ui::git::GitUpdate>,
) -> bool {
	match event {
		GitWorkbenchEvent::Consumed => false,
		GitWorkbenchEvent::Close => {
			let _ = session.handle(GitIntent::Close).await;
			true
		},
		GitWorkbenchEvent::Intent(intent) => dispatch(session, workbench, intent, updates).await,
	}
}

async fn dispatch(
	session: &GitSession,
	workbench: &mut GitWorkbench,
	intent: GitIntent,
	updates: &flume::Sender<omp_chat_ui::git::GitUpdate>,
) -> bool {
	let mut intents = VecDeque::from([intent]);
	while let Some(intent) = intents.pop_front() {
		if matches!(
			intent,
			GitIntent::Avatar { .. }
				| GitIntent::Load { .. }
				| GitIntent::GenerateCommit { .. }
				| GitIntent::AiStage { .. }
		) {
			let session = session.clone();
			let updates = updates.clone();
			drop(tokio::spawn(async move {
				let result = session
					.handle_with_progress(intent, |update| {
						let _ = updates.send(update);
					})
					.await;
				for update in result.updates {
					let _ = updates.send(update);
				}
			}));
			continue;
		}
		let result = session.handle(intent).await;
		if result.close {
			return true;
		}
		let mut snapshot_delivered = false;
		for update in result.updates {
			snapshot_delivered |= matches!(&update, omp_chat_ui::git::GitUpdate::Snapshot(_));
			if let Some(intent) = workbench.apply(update) {
				intents.push_back(intent);
			}
		}
		if snapshot_delivered {
			spawn_deferred_stats(session, updates);
		}
	}
	false
}

fn spawn_deferred_stats(
	session: &GitSession,
	updates: &flume::Sender<omp_chat_ui::git::GitUpdate>,
) {
	let session = session.clone();
	let updates = updates.clone();
	drop(tokio::spawn(async move {
		if let Ok(Some(snapshot)) = session.deferred_stats().await {
			let _ = updates.send(omp_chat_ui::git::GitUpdate::Snapshot(snapshot));
		}
	}));
}

async fn apply_update(
	session: &GitSession,
	workbench: &mut GitWorkbench,
	update: omp_chat_ui::git::GitUpdate,
	updates: &flume::Sender<omp_chat_ui::git::GitUpdate>,
) -> bool {
	match workbench.apply(update) {
		Some(intent) => dispatch(session, workbench, intent, updates).await,
		None => false,
	}
}

fn paint(
	workbench: &mut GitWorkbench,
	renderer: &mut Renderer<TtyOut>,
	viewport: Size,
) -> io::Result<()> {
	let base = Frame::new(Size::new(viewport.width, viewport.height.max(1)));

	renderer.repaint("", base, viewport.height, &[workbench.layer(viewport)])?;
	Ok(())
}

pub(crate) fn git_open_error(error: &GitModelError) -> String {
	match error {
		GitModelError::NotRepository => String::from("Not a git repository"),
		GitModelError::RevisionMissing { revision } => format!("Cannot resolve revision: {revision}"),
		_ => error.to_string(),
	}
}
