//! Scripted backend used only by the terminal chat example.

use std::{
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::{Duration, Instant},
};

use flume::{Receiver, Sender};
use omp_chat_ui::{
	BackendEvent, CompactionBoundaries, GitFacts, Intent, ModelRow, QueuedPrompt, RewindTargetRow,
	SessionRow, SettingRow, StatusFacts, ThinkingLevel, ToolTerminal, ToolViewContent,
	WelcomeBanner, WelcomeLspServer, login_panel::LoginEvent,
};
use omp_core::{Str, sf};
use omp_tui::components::ComposerStyle;
use strum::IntoEnumIterator;

pub fn start() -> (Receiver<BackendEvent>, Sender<Intent>) {
	let (event_tx, event_rx) = flume::unbounded();
	let (intent_tx, intent_rx) = flume::unbounded();
	drop(tokio::spawn(run(event_tx, intent_rx)));
	(event_rx, intent_tx)
}

async fn run(events: Sender<BackendEvent>, intents: Receiver<Intent>) {
	let models = models();
	let generation = Arc::new(AtomicU64::new(0));
	let mut model = 0_usize;
	let mut composer_style = ComposerStyle::default();
	let mut messages: Vec<(u64, Str)> = Vec::new();
	let mut next_event = 1_u64;
	let mut queued: Vec<QueuedPrompt> = Vec::new();
	let mut streaming = false;
	let (done_tx, done_rx) = flume::unbounded::<u64>();
	let _ = events.send(BackendEvent::Sessions(sessions()));
	let _ = events.send(BackendEvent::ModelsUpdated {
		rows:         models.clone(),
		current:      model,
		task_current: model,
	});
	let _ = events.send(BackendEvent::Status(status(&models[model].name, false)));
	let _ = events.send(BackendEvent::WelcomeBanner(welcome_banner(&models[model])));

	while let Ok(intent) = intents.recv_async().await {
		while let Ok(turn) = done_rx.try_recv() {
			if generation.load(Ordering::SeqCst) == turn {
				streaming = false;
				if !queued.is_empty() {
					queued.clear();
					let _ = events.send(BackendEvent::QueuedPromptsSettled);
				}
			}
		}
		match intent {
			Intent::ExtensionShortcut(_) => {},
			Intent::Submit { text, attachments, mode: _ } => {
				if text == "/settings" {
					let _ = events.send(BackendEvent::SettingsSchema(setting_rows(composer_style)));
					continue;
				}
				if matches!(text.as_str(), "/model" | "/switch") {
					let _ = events.send(BackendEvent::OpenModelPicker {
						rows:         models.clone(),
						current:      model,
						task_current: model,
					});
					continue;
				}
				let event = next_event;
				next_event += 1;
				messages.push((event, Str::from(text.clone())));
				let chips = (0..attachments.len())
					.map(|index| Str::from(format!("attachment {}", index + 1)))
					.collect();
				if streaming {
					let _ = events.send(BackendEvent::UserReplayed {
						text: Str::from(text.clone()),
						chips,
						queued: true,
					});
					queued.push(QueuedPrompt { text: Str::from(text), attachments });
					continue;
				}
				let _ = events.send(BackendEvent::UserReplayed {
					text: Str::from(text),
					chips,
					queued: false,
				});
				let turn = generation.fetch_add(1, Ordering::SeqCst) + 1;
				streaming = true;
				let events = events.clone();
				let generation = Arc::clone(&generation);
				let model_name = models[model].name.clone();
				let done = done_tx.clone();
				tokio::spawn(stream_turn(events, generation, turn, model_name, done));
			},
			Intent::Abort => {
				generation.fetch_add(1, Ordering::SeqCst);
				streaming = false;
				let _ = events.send(BackendEvent::Ack { interrupted: true });
				let _ = events.send(BackendEvent::Status(status(&models[model].name, false)));
			},
			Intent::Dequeue => {
				if queued.is_empty() {
					let _ = events.send(BackendEvent::Notice(sf!("No queued messages to restore.")));
				} else {
					let restored = queued.len();
					let prompts = std::mem::take(&mut queued);
					let _ = events.send(BackendEvent::QueuedPromptsRestored(prompts));
					let _ = events.send(BackendEvent::Notice(sf!(
						"Restored {restored} queued message{} to the editor.",
						if restored == 1 { "" } else { "s" },
					)));
				}
			},
			Intent::RewindRequest => {
				let rows = messages
					.iter()
					.map(|(event, text)| RewindTargetRow { event: *event, text: text.clone() })
					.collect();
				let _ = events.send(BackendEvent::RewindTargets(rows));
			},
			Intent::Rewind { event } => {
				messages.retain(|(candidate, _)| *candidate <= event);
				let _ = events.send(BackendEvent::HistoryCleared);
				for (_, text) in &messages {
					let _ = events.send(BackendEvent::UserReplayed {
						text:   text.clone(),
						chips:  Vec::new(),
						queued: false,
					});
				}
			},
			Intent::SwitchModel(key) => {
				if let Some(index) = models.iter().position(|row| row.key == key) {
					model = index;
					let _ = events.send(BackendEvent::Status(status(&models[model].name, false)));
				}
			},
			Intent::Login(None) => {
				let _ = events.send(BackendEvent::LoginProviders(providers()));
			},
			Intent::Login(Some(provider)) => {
				let _ = events.send(BackendEvent::LoginPanel {
					provider: provider.clone(),
					event:    LoginEvent::Prompt {
						message: Str::from(format!("Enter credential for {provider}")),
						masked:  true,
					},
				});
			},
			Intent::AuthAnswer { value: _ } => {
				let _ = events.send(BackendEvent::LoginPanelClose);
				let _ = events.send(BackendEvent::Notice(sf!("Credential accepted by mock backend.")));
			},
			Intent::AuthCancel => {
				let _ = events.send(BackendEvent::LoginPanelClose);
			},
			Intent::Resume(None) => {
				let _ = events.send(BackendEvent::Sessions(sessions()));
			},
			Intent::Resume(Some(id)) => {
				let _ = events.send(BackendEvent::HistoryCleared);
				let _ = events.send(BackendEvent::SessionTitle(Str::from(format!("Resumed {id}"))));
				let _ = events.send(BackendEvent::UserReplayed {
					text:   sf!("Continue from the last checkpoint."),
					chips:  Vec::new(),
					queued: false,
				});
			},
			Intent::NewSession => {
				messages.clear();
				let _ = events.send(BackendEvent::HistoryCleared);
				let _ = events.send(BackendEvent::SessionTitle(sf!("New local session")));
				let _ = events.send(BackendEvent::WelcomeBanner(welcome_banner(&models[model])));
			},
			Intent::ApplySettings { changes, commit } => {
				for change in changes {
					if change.path == "composer.shape"
						&& let Some(value) = change.value.as_str()
						&& let Ok(style) = value.parse::<ComposerStyle>()
					{
						composer_style = style;
						let _ = events.send(BackendEvent::ComposerStyleChanged(style));
					}
				}
				if commit {
					let _ = events.send(BackendEvent::Notice(sf!("Settings saved by mock backend.")));
				}
			},
			Intent::Help => {
				let _ = events.send(BackendEvent::Notice(sf!(
					"Ctrl+P models · Ctrl+K commands · Ctrl+B sidebar · Esc Esc rewind",
				)));
			},
			Intent::Quit => break,
			_ => {},
		}
	}
}

async fn stream_turn(
	events: Sender<BackendEvent>,
	generation: Arc<AtomicU64>,
	turn: u64,
	model: Str,
	done: Sender<u64>,
) {
	let active = || generation.load(Ordering::SeqCst) == turn;
	let mut facts = status(&model, true);
	facts.turn_started = Some(Instant::now());
	let _ = events.send(BackendEvent::Status(facts));
	let assistant = Str::from(format!("assistant-{turn}"));
	let tool = Str::from(format!("tool-{turn}"));
	let _ =
		events.send(BackendEvent::AssistantBegin { id: assistant.clone(), thinking: false });
	for delta in [
		"I’ll inspect the rendering seam, ",
		"preserve stable scrollback rows, ",
		"and update the host wiring.\n\n",
	] {
		if !active() {
			return;
		}
		let _ =
			events.send(BackendEvent::AssistantDelta { id: assistant.clone(), text: sf!(delta) });
		tokio::time::sleep(Duration::from_millis(180)).await;
	}
	if !active() {
		return;
	}
	let _ = events.send(BackendEvent::ToolStarted { id: tool.clone(), name: sf!("shell") });
	for chunk in ["reading scene modules\n", "checking damage ranges\n", "done\n"] {
		if !active() {
			return;
		}
		let _ = events.send(BackendEvent::ToolOutput { id: tool.clone(), chunk: sf!(chunk) });
		tokio::time::sleep(Duration::from_millis(160)).await;
	}
	if !active() {
		return;
	}
	let _ = events.send(BackendEvent::ToolFinished {
		id:       tool,
		terminal: ToolTerminal::Succeeded,
		view:     ToolViewContent::Plain(sf!("Host seam verified\n3 files inspected")),
	});
	let _ = events.send(BackendEvent::AssistantDelta {
		id:   assistant.clone(),
		text: sf!("The immediate-mode scene is ready."),
	});
	let _ = events.send(BackendEvent::AssistantEnd { id: assistant });
	let _ = events.send(BackendEvent::Ack { interrupted: false });
	let _ = events.send(BackendEvent::Status(status(&model, false)));
	let _ = done.send(turn);
}

fn welcome_banner(model: &ModelRow) -> WelcomeBanner {
	WelcomeBanner {
		version:     sf!("0.1.0"),
		model:       model.name.clone(),
		provider:    model.provider.clone(),
		lsp_servers: vec![
			WelcomeLspServer {
				name:        sf!("rust-analyzer"),
				stage_label: sf!("ready (rs)"),
				failed:      false,
			},
			WelcomeLspServer {
				name:        sf!("tsserver"),
				stage_label: sf!("failed"),
				failed:      true,
			},
		],
		tip:         Some(sf!("Tired of typing \"keep going\"? Just send a '.'")),
	}
}

fn status(model: &Str, working: bool) -> StatusFacts {
	StatusFacts {
		model: model.clone(),
		working,
		turn_started: working.then(Instant::now),
		context_tokens: 391_000,
		context_window: Some(1_000_000),
		cost_nanos: 8_650_000_000,
		queued: 0,
		jobs: usize::from(working),
		attempt: 0,
		dropped: 0,
		git: Some(GitFacts { branch: sf!("main"), dirty: 5, staged: 9, untracked: 2 }),
		thinking: Some(ThinkingLevel::Max),
		cwd: Some(sf!("/work/omp")),
		compaction_boundaries: Some(CompactionBoundaries {
			threshold_percent:   80.0,
			speculation_percent: Some(70.0),
		}),
		..StatusFacts::default()
	}
}

fn setting_rows(style: ComposerStyle) -> Vec<SettingRow> {
	let mut rows = vec![SettingRow {
		panel:       sf!("appearance"),
		domain:      sf!("core"),
		path:        sf!("composer.shape"),
		label:       sf!("Composer shape"),
		description: sf!("Chat composer chrome."),
		kind:        sf!("enum"),
		secret:      false,
		value:       Some(Str::from(style.to_string())),
		options:     ComposerStyle::iter()
			.map(|style| Str::from(style.to_string()))
			.collect(),
		visible:     true,
	}];
	// Filler rows across several panels so the example exercises tab
	// wrapping, viewport scrolling, and live search.
	for (panel, domain, path, label, description, kind, value) in [
		("appearance", "tui", "tui.theme", "Theme", "Color theme name.", "text", "default"),
		("appearance", "tui", "tui.shimmer", "Shimmer", "Animated accent shimmer.", "bool", "true"),
		("model", "model", "model.effort", "Effort", "Default reasoning effort.", "text", "medium"),
		(
			"model",
			"model",
			"model.fallback",
			"Model fallback",
			"Retry on a fallback model.",
			"bool",
			"true",
		),
		(
			"interaction",
			"interaction",
			"interaction.steering",
			"Steering mode",
			"How mid-turn input steers the agent.",
			"text",
			"queue",
		),
		(
			"interaction",
			"interaction",
			"interaction.typo_detection",
			"Typo detection",
			"Warn on likely typos before submit.",
			"bool",
			"true",
		),
		(
			"context",
			"compaction",
			"compaction.enabled",
			"Automatic compaction",
			"Compact the thread near the context limit.",
			"bool",
			"true",
		),
		(
			"context",
			"compaction",
			"compaction.recent_tokens",
			"Recent tokens",
			"Tokens preserved verbatim during compaction.",
			"number",
			"20000",
		),
	] {
		rows.push(SettingRow {
			panel:       sf!(panel),
			domain:      sf!(domain),
			path:        sf!(path),
			label:       sf!(label),
			description: sf!(description),
			kind:        sf!(kind),
			secret:      false,
			value:       Some(Str::from(value)),
			options:     Vec::new(),
			visible:     true,
		});
	}
	for index in 0..24u32 {
		rows.push(SettingRow {
			panel:       sf!("tools_tasks"),
			domain:      sf!("tools"),
			path:        Str::from(format!("tools.filler_{index}")),
			label:       Str::from(format!("Filler setting {index}")),
			description: sf!("Synthetic row exercising the scrolling viewport."),
			kind:        sf!("bool"),
			secret:      false,
			value:       Some(sf!("false")),
			options:     Vec::new(),
			visible:     true,
		});
	}
	rows
}

fn models() -> Vec<ModelRow> {
	[
		("anthropic/claude-sonnet", "Claude Sonnet", "anthropic", "Anthropic", 200_000, 3.0, 15.0),
		("openai/gpt-5", "GPT-5", "openai", "OpenAI", 400_000, 1.25, 10.0),
		("google/gemini-pro", "Gemini Pro", "google", "Google", 1_000_000, 1.25, 10.0),
	]
	.into_iter()
	.map(|(key, name, provider_id, provider, context, input, output)| ModelRow {
		key:         Str::from(key),
		name:        Str::from(name),
		color:       None,
		provider_id: Str::from(provider_id),
		provider:    Str::from(provider),
		context:     Some(context),
		input_mtok:  Some(input),
		output_mtok: Some(output),
		efforts:     Arc::from([sf!("low"), sf!("medium"), sf!("high")]),
	})
	.collect()
}

fn providers() -> Vec<SessionRow> {
	[
		("anthropic", "Anthropic", "API key"),
		("openai", "OpenAI", "OAuth or API key"),
		("google", "Google", "OAuth"),
	]
	.into_iter()
	.map(|(id, label, detail)| SessionRow {
		id:     Str::from(id),
		label:  Str::from(label),
		detail: Str::from(detail),
		pinned: false,
	})
	.collect()
}

fn sessions() -> Vec<SessionRow> {
	[
		("local-1", "Optimize custom status widget rendering", "NOW"),
		("local-2", "Check Unicode character display", "01m"),
		("local-3", "Add cursor shift", "02m"),
	]
	.into_iter()
	.map(|(id, label, detail)| SessionRow {
		id:     Str::from(id),
		label:  Str::from(label),
		detail: Str::from(detail),
		pinned: false,
	})
	.collect()
}
