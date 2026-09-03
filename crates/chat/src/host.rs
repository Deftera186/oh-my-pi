//! Interactive terminal actor over a detached DOM snapshot and event streams.
//!
//! Presentation model (ADR 0034): every transcript block is a mutable slot.
//! Blocks stay on screen — in a top-anchored document that switches to its
//! tail once it outgrows the terminal — and retire into native scrollback
//! only under row pressure, oldest first, once the DOM marks them done.

use std::{
	env, future, io,
	path::PathBuf,
	sync::Arc,
	time::{Duration, Instant},
};

use flume::{Receiver, Sender};
use omp_agent::{KernelEvent, Up};
use omp_con::{AI_COMPACT_THRESHOLD, AI_MODEL, AI_THINKING, CL_SHOWTHINKING, Ctx, Source};
use omp_core::Str;
use omp_dom::{Dom, Event, KnownTag, PropId, Snapshot, Tag, Value};
use omp_journal::blob::BlobRef;
use omp_tui::{
	CursorStyle, DebugOp, Dim, Frame, InputEvent, Key, Layer, OverlayAnchor, OverlayOptions,
	Renderer, Size, Terminal, TerminalEvent, TerminalOptions, TtyOut, Ui, UiContext,
	components::{Col, Spacer},
	detect, respond_debug_query,
	slots::{BlockId, Mode, ResizePolicy, Slots},
};
use thiserror::Error;

use crate::{
	actions::{HostAction, HostMailbox},
	cards::CardRegistry,
	chrome::{ModelBadge, StatusFacts, Welcome, display_path, tip_for},
	autocomplete::slash,
	composer::{Composer, ComposerAction},
	input::Bindings,
	overlays::{HistoryPicker, ModelPicker, ModelRow, Overlay, Overlays, PickerEvent},
	project::{BlockKind, BlockView, RenderedBlock, project},
	status_line::StatusLine,
};

/// Console command that engages the plan Director.
const PLAN_DIRECTOR: &str = "plan";
/// Notice shown when a bound command wants a reasoning level the model lacks.
const NO_THINKING: &str = "Current model does not support thinking";

/// Commands emitted by the presentation actor to the application controller.
#[derive(Clone, Debug)]
pub enum HostCommand {
	/// Begin a fresh explicit turn.
	Submit(Str),
	/// Begin a turn with content-addressed attachments.
	SubmitWithAttachments {
		/// User-authored text.
		text:        Str,
		/// Durable attachment references.
		attachments: Vec<BlobRef>,
	},
	/// Queue steering text at the kernel's next safe point.
	Steer(Str),
	/// Interrupt the active turn without exiting the chat.
	Interrupt,
	/// Deliver a decision for one controller-owned approval prompt.
	Approve {
		/// Stable prompt identity.
		id:       Str,
		/// User-authored decision.
		decision: omp_agent::ApprovalDecision,
	},
	/// Notify the app of an observer-local overlay transition.
	Overlay {
		/// Stable local overlay identity.
		id:   Str,
		/// Whether the overlay opened (`true`) or closed (`false`).
		open: bool,
	},
	/// Engage or exit the plan Director (pi `app.plan.toggle`).
	PlanMode {
		/// `true` engages plan mode; `false` exits it.
		engage: bool,
	},
	/// Stop the application-owned controller loop.
	Quit,
}

/// Public name for the actor's one upward event mailbox.
pub type UpEvent = HostCommand;

/// Result of applying `C-c` to the current chat activity state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CtrlCAction {
	/// Cancel the active turn while keeping the host open.
	Interrupt,
	/// Leave chat and restore the terminal.
	Quit,
}

/// Resolves pi-compatible `C-c` behavior.
#[must_use]
pub const fn ctrl_c_action(turn_active: bool, repeated: bool) -> CtrlCAction {
	if turn_active && !repeated {
		CtrlCAction::Interrupt
	} else {
		CtrlCAction::Quit
	}
}

/// Interactive actor construction options.
pub struct HostOptions {
	/// Initial detached controller snapshot.
	pub snapshot:      Snapshot,
	/// Ordered DOM event stream following `snapshot`.
	pub dom_events:    Receiver<Event>,
	/// Ephemeral kernel progress notifications.
	pub kernel_events: Receiver<KernelEvent>,
	/// Commands back to the application controller.
	pub commands:      Sender<HostCommand>,
	/// Kernel's one upward steering/cancellation mailbox.
	pub up:            Sender<Up>,
	/// Shared command-stream context. It carries policy, not session state.
	pub con:           Arc<Ctx>,
	/// App-normalized physical bindings.
	pub bindings:      Bindings,
	/// Catalog roster for the model picker (`cl_model_select`).
	pub models:        Vec<ModelRow>,
	/// `(role, model key)` roster for `cl_model_cycle`, in cycle order.
	pub cycle:         Vec<(Str, Str)>,
	/// Transcript resize policy.
	pub resize_policy: ResizePolicy,
	/// Launch model facts for the banner and status band.
	pub model:         ModelBadge,
	/// Checked-out git branch of the project: an observer-local fact the
	/// app probes, never journaled.
	pub branch:        Option<Str>,
	/// Ambient renderer context.
	pub ui:            UiContext,
}

/// Chat actor or terminal delivery failure.
#[derive(Debug, Error)]
pub enum HostError {
	/// Terminal lifecycle or geometry failed.
	#[error(transparent)]
	Io(#[from] io::Error),
	/// A controller event could not be applied to the replica.
	#[error(transparent)]
	Dom(#[from] omp_dom::DomError),
	/// A bound console command failed.
	#[error(transparent)]
	Con(#[from] omp_con::ConError),
	/// A renderer delivery transaction failed.
	#[error(transparent)]
	Delivery(#[from] omp_tui::DeliveryError),
}

/// Presentation state shared by the terminal and native actors.
struct Presenter {
	replica:        Dom,
	dom_events:     Receiver<Event>,
	kernel_events:  Receiver<KernelEvent>,
	commands:       Sender<HostCommand>,
	up:             Sender<Up>,
	con:            Arc<Ctx>,
	bindings:       Bindings,
	cards:          CardRegistry,
	ui:             UiContext,
	model:          ModelBadge,
	local:          LocalFacts,
	composer:       Composer,
	overlays:       Overlays,
	turn_active:    bool,
	/// Presentation-clock start of the in-flight turn (the band timer).
	turn_started:   Option<Duration>,
	last_interrupt: Option<Instant>,
	/// Last `cl_clear` press, for pi's double-press exit window.
	last_clear:     Option<Instant>,
	clock:          Instant,
	/// The one console mailbox: bound commands post actions here.
	mailbox:        Arc<HostMailbox>,
	models:         Vec<ModelRow>,
	cycle:          Vec<(Str, Str)>,
	/// Last prompt sent as a turn, for `cl_retry`.
	last_prompt:    Option<Str>,
	/// Text the composer asked to copy; the terminal loop drains it into
	/// the clipboard (OSC 52 / native).
	clipboard:      Option<Str>,
}

/// Observer-local band facts that never enter the DOM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFacts {
	/// Checked-out git branch.
	pub branch:   Option<Str>,
	/// Platform temp directory, for pi's scratch-project labeling.
	pub tmp:      Option<Str>,
	/// Live reasoning level when the model can reason.
	pub thinking: Option<Str>,
	/// Live model route override (`ai_model`) when set.
	pub model:    Option<Str>,
	/// Auto-compaction threshold as a whole percent (`ai_compact_threshold`).
	pub compact:  u8,
}

impl Default for LocalFacts {
	fn default() -> Self {
		Self { branch: None, tmp: None, thinking: None, model: None, compact: 80 }
	}
}

impl LocalFacts {
	/// Facts fixed at launch: the app's branch probe and the platform temp
	/// directory.
	fn at_launch(branch: Option<Str>) -> Self {
		let tmp = env::temp_dir();
		let tmp = tmp
			.to_str()
			.map(|tmp| Str::new(tmp.trim_end_matches('/')));
		Self { branch, tmp, ..Self::default() }
	}

	/// Refreshes the convar-backed facts (`ai_thinking`, `ai_model`,
	/// `ai_compact_threshold`).
	fn sync_con(&mut self, con: &Ctx, badge: &ModelBadge) {
		self.thinking = badge.reasoning.then(|| AI_THINKING.get(con));
		self.model = Some(AI_MODEL.get(con)).filter(|model| !model.is_empty());
		self.compact = (AI_COMPACT_THRESHOLD.get(con) * 100.0).round().clamp(0.0, 100.0) as u8;
	}
}

/// What one routed input asked the host to do next. Ordered by strength so
/// several actions from one console line fold to the strongest.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Routed {
	/// Nothing changed.
	Ignored,
	/// Presentation may have changed; repaint.
	Repaint,
	/// An observer-local projection toggle flipped; rebuild the projection.
	RebuildProjection,
	/// Re-probe the terminal and repaint everything from the retained
	/// document.
	DisplayReset,
	/// Leave the terminal, run the external editor over the draft, re-enter.
	ExternalEditor,
	/// Job-control suspend: leave the terminal, stop, re-enter on resume.
	Suspend,
	/// Leave the host.
	Quit,
}

/// Why the terminal actor released the tty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Pause {
	/// The chat is over.
	Quit,
	/// Stop the process group until the shell resumes it.
	Suspend,
	/// Run the external editor over the draft.
	ExternalEditor,
	/// Re-probe capabilities and repaint from the retained document.
	DisplayReset,
}

/// pi `handleCtrlZ`: job-control suspend of the whole process group. A
/// no-op on platforms without POSIX job control.
fn suspend_process() {
	#[cfg(unix)]
	{
		use nix::{sys::signal, unistd::Pid};
		if let Err(error) = signal::kill(Pid::from_raw(0), signal::Signal::SIGTSTP) {
			tracing::warn!(%error, "failed to suspend process group");
		}
	}
}

impl Presenter {
	fn new(options: HostOptions, width: u16) -> Self {
		let mailbox = options.con.user::<HostMailbox>().unwrap_or_else(|| {
			HostMailbox::install(&options.con);
			options
				.con
				.user::<HostMailbox>()
				.expect("mailbox installed on the console")
		});
		let replica = Dom::from_snapshot(&options.snapshot);
		let mut overlays = Overlays::default();
		overlays.sync_approval(&replica);
		let mut local = LocalFacts::at_launch(options.branch);
		local.sync_con(&options.con, &options.model);
		let facts = status_facts(&replica, &options.model, &local, None);
		let composer = Composer::new(
			width,
			options.ui.clone(),
			facts,
			slash::roster(&options.con),
			project_root(&replica).as_deref(),
		);
		Self {
			replica,
			dom_events: options.dom_events,
			kernel_events: options.kernel_events,
			commands: options.commands,
			up: options.up,
			con: options.con,
			bindings: options.bindings,
			cards: CardRegistry::standard(),
			ui: options.ui,
			model: options.model,
			local,
			composer,
			overlays,
			turn_active: false,
			turn_started: None,
			last_interrupt: None,
			last_clear: None,
			clock: Instant::now(),
			mailbox,
			models: options.models,
			cycle: options.cycle,
			last_prompt: None,
			clipboard: None,
		}
	}

	/// Records a turn-activity edge; the band timer starts on the rising edge
	/// and clears on the falling one.
	fn set_turn_active(&mut self, active: bool) {
		if active && !self.turn_active {
			self.turn_started = Some(self.clock.elapsed());
		} else if !active {
			self.turn_started = None;
		}
		self.turn_active = active;
	}

	fn show_thinking(&self) -> bool {
		CL_SHOWTHINKING.get(&self.con)
	}

	fn welcome(&self) -> RenderedBlock {
		let status = StatusLine::from_dom(&self.replica);
		let welcome = Welcome::new(
			Str::new_static(env!("CARGO_PKG_VERSION")),
			&self.model,
			tip_for(status.session.as_str()),
		);
		RenderedBlock {
			view:      BlockView {
				key:       0,
				kind:      BlockKind::Welcome,
				text:      Str::new_static("welcome"),
				mode:      Mode::Mutable,
				finalized: true,
			},
			component: Box::new(welcome),
		}
	}

	fn blocks(&self) -> Vec<RenderedBlock> {
		let mut blocks = vec![self.welcome()];
		blocks.extend(project(&self.replica, &self.cards, &self.ui, self.show_thinking()));
		blocks
	}

	fn apply_dom_event(&mut self, event: &Event) -> Result<(), HostError> {
		self.replica.apply_event(event)?;
		self.set_turn_active(has_active_turn(&self.replica));
		self.overlays.sync_approval(&self.replica);
		Ok(())
	}

	fn sync_status(&mut self) -> bool {
		self.local.sync_con(&self.con, &self.model);
		let facts = status_facts(&self.replica, &self.model, &self.local, self.turn_started);
		self.composer.set_status(facts)
	}

	/// Routes one decoded key: an open picker consumes it first, then the
	/// approval hotkeys, then the console bind table (pi checks app actions
	/// before editor keys, except Esc while the autocomplete popup is open),
	/// then the composer.
	fn route_key(&mut self, key: Key) -> Result<Routed, HostError> {
		// pi's status row disappears on the next input.
		let had_notice = self.overlays.notice().is_some();
		self.overlays.clear_notice();
		match self.overlays.active_mut() {
			Some(Overlay::Models(picker)) => {
				let event = picker.key(key);
				return self.apply_picker_event(event);
			},
			Some(Overlay::History(picker)) => {
				let event = picker.key(key);
				return self.apply_picker_event(event);
			},
			Some(Overlay::Approval(approval)) => {
				if let Key::Char(value @ ('y' | 'n' | 'a')) = key {
					let approval = approval.clone();
					if let Some(decision) = approval.decision(value) {
						let _ = self
							.commands
							.send(HostCommand::Approve { id: approval.id, decision });
					}
					self.overlays.dismiss();
					return Ok(Routed::Repaint);
				}
			},
			Some(Overlay::Notice(_)) | None => {},
		}
		if let Some(command) = self.bindings.command(key) {
			let esc_to_popup = command == "cl_interrupt" && self.composer.popup_open();
			if !esc_to_popup {
				let command = Str::new(command);
				return self.run_console(command.as_str());
			}
		} else if key == Key::Ctrl('c') {
			// Emergency control: pi keeps Ctrl+C live even before bindings load.
			return self.act(HostAction::Clear);
		}
		let routed = match self.composer.key(key) {
			ComposerAction::Submit(text) => self.submit(text),
			// A submitted `/name args` line is the console statement
			// `name args`, exactly like a bound key.
			ComposerAction::Command(statement) => self.run_console(statement.as_str())?,
			ComposerAction::Copy(text) => {
				self.clipboard = Some(text);
				Routed::Repaint
			},
			ComposerAction::Changed => Routed::Repaint,
			ComposerAction::Ignored => Routed::Ignored,
		};
		Ok(if had_notice && routed == Routed::Ignored {
			Routed::Repaint
		} else {
			routed
		})
	}

	/// Executes one console line and applies every action it posted.
	///
	/// Console failures become a notice rather than ending the host: a
	/// mistyped `bind` in `config.cfg` must not kill the chat.
	fn run_console(&mut self, command: &str) -> Result<Routed, HostError> {
		let before = self.projection_inputs();
		let failure = self.con.exec(command, Source::Console).err();
		let mut routed = if before == self.projection_inputs() {
			Routed::Repaint
		} else {
			Routed::RebuildProjection
		};
		let actions = self.mailbox.drain().collect::<Vec<_>>();
		for action in actions {
			routed = routed.max(self.act(action)?);
		}
		if let Some(error) = failure {
			self.overlays.show(Overlay::Notice(Str::new(error.to_string())));
			routed = routed.max(Routed::Repaint);
		}
		Ok(routed)
	}

	/// Convars whose change forces a transcript rebuild.
	fn projection_inputs(&self) -> (bool, bool, bool) {
		(
			self.show_thinking(),
			crate::actions::CL_SHOWTOOLS.get(&self.con),
			crate::actions::CL_TOOLS_EXPANDED.get(&self.con),
		)
	}

	fn notice(&mut self, text: impl Into<Str>) -> Routed {
		self.overlays.show(Overlay::Notice(text.into()));
		Routed::Repaint
	}

	/// Sends the draft as a submission. The controller — which knows whether
	/// a turn is really running — starts a turn or steers the active one;
	/// the replica's view may lag the kernel, so the host never decides.
	fn submit(&mut self, text: Str) -> Routed {
		if text.trim().is_empty() {
			return Routed::Ignored;
		}
		if !self.turn_active {
			self.set_turn_active(true);
			self.last_prompt = Some(text.clone());
		}
		let _ = self.commands.send(HostCommand::Submit(text));
		Routed::Repaint
	}

	/// Applies one posted host action.
	fn act(&mut self, action: HostAction) -> Result<Routed, HostError> {
		Ok(match action {
			HostAction::Interrupt => {
				if self.overlays.modal() {
					self.overlays.dismiss();
					Routed::Repaint
				} else if self.turn_active {
					self.last_interrupt = Some(Instant::now());
					let _ = self.commands.send(HostCommand::Interrupt);
					Routed::Ignored
				} else {
					// Esc never destroys an in-progress draft.
					Routed::Ignored
				}
			},
			HostAction::Clear => {
				let now = Instant::now();
				let repeated = self
					.last_clear
					.is_some_and(|prior| now.duration_since(prior) <= Duration::from_millis(500));
				self.last_clear = Some(now);
				if !self.composer.text().is_empty() {
					self.composer.clear();
					return Ok(Routed::Repaint);
				}
				let interrupted = self
					.last_interrupt
					.is_some_and(|prior| now.duration_since(prior) <= Duration::from_secs(1));
				match ctrl_c_action(self.turn_active, repeated || interrupted) {
					CtrlCAction::Interrupt => {
						self.last_interrupt = Some(now);
						let _ = self.commands.send(HostCommand::Interrupt);
						Routed::Ignored
					},
					CtrlCAction::Quit => Routed::Quit,
				}
			},
			HostAction::Exit => Routed::Quit,
			HostAction::Suspend => Routed::Suspend,
			HostAction::DisplayReset => Routed::DisplayReset,
			HostAction::ThinkingCycle => self.cycle_thinking(),
			HostAction::ModelCycle { backward } => self.cycle_model(backward),
			HostAction::ModelSelect { session_only } => {
				if self.models.is_empty() {
					return Ok(self.notice("No models are available to switch to"));
				}
				let current = self.current_model_index();
				let picker = ModelPicker::open(
					self.models.clone(),
					current,
					current,
					session_only,
					self.composer.frame().size().width,
					&self.ui,
				);
				self.overlays.show(Overlay::Models(picker));
				let _ = self.commands.send(HostCommand::Overlay {
					id:   Str::new_static("models"),
					open: true,
				});
				Routed::Repaint
			},
			HostAction::FollowUp => {
				let text = Str::new(self.composer.text());
				let routed = self.submit(text);
				if routed == Routed::Repaint {
					self.composer.clear();
				}
				routed
			},
			HostAction::Retry => {
				if self.turn_active {
					return Ok(self.notice("A turn is already running"));
				}
				match (self.last_turn_failed(), self.last_prompt.clone()) {
					(true, Some(text)) => self.submit(text),
					(true, None) => self.notice("Nothing to retry in this session"),
					(false, _) => self.notice("Last turn did not fail; nothing to retry"),
				}
			},
			HostAction::PlanToggle => {
				let engaged = self.plan_engaged();
				let _ = self
					.commands
					.send(HostCommand::PlanMode { engage: !engaged });
				self.notice(if engaged {
					"Plan mode off"
				} else {
					"Plan mode on: the next turn must write a plan and ask before acting"
				})
			},
			HostAction::HistorySearch => {
				let prompts = crate::overlays::prompt_history(&self.replica);
				if prompts.is_empty() {
					return Ok(self.notice("No prompt history in this session"));
				}
				let picker =
					HistoryPicker::open(prompts, self.composer.frame().size().width, &self.ui);
				self.overlays.show(Overlay::History(picker));
				Routed::Repaint
			},
			HostAction::ExternalEditor => Routed::ExternalEditor,
			HostAction::Reply { severity, text } => match severity {
				omp_con::Severity::Info if text.is_empty() => Routed::Ignored,
				_ => self.notice(text),
			},
		})
	}

	fn apply_picker_event(&mut self, event: PickerEvent) -> Result<Routed, HostError> {
		Ok(match event {
			PickerEvent::Consumed => Routed::Repaint,
			PickerEvent::Close => {
				self.close_overlay();
				Routed::Repaint
			},
			PickerEvent::Pick(index) | PickerEvent::PickTask(index) => {
				let Some(Overlay::Models(picker)) = self.overlays.active() else {
					return Ok(Routed::Repaint);
				};
				let session_only = picker.session_only();
				let Some(row) = picker.rows().get(index).cloned() else {
					return Ok(Routed::Repaint);
				};
				let task = matches!(event, PickerEvent::PickTask(_));
				self.close_overlay();
				self.select_model(&row, task, session_only)
			},
			PickerEvent::Recall(text) => {
				self.close_overlay();
				self.composer.set_text(text.as_str());
				Routed::Repaint
			},
		})
	}

	fn close_overlay(&mut self) {
		if matches!(self.overlays.active(), Some(Overlay::Models(_))) {
			let _ = self.commands.send(HostCommand::Overlay {
				id:   Str::new_static("models"),
				open: false,
			});
		}
		self.overlays.dismiss();
	}

	/// Writes the picked model to the control plane: `ai_model` for the
	/// session, `ai_task_model` for task subagents, archived to `config.cfg`
	/// unless the picker was opened session-only.
	fn select_model(&mut self, row: &ModelRow, task: bool, session_only: bool) -> Routed {
		let var = if task { "ai_task_model" } else { "ai_model" };
		let script = format!("{var} {}", omp_con::Value::Str(row.key.clone()));
		if let Err(error) = self.con.exec(&script, Source::Console) {
			return self.notice(error.to_string());
		}
		if !task {
			self.reset_thinking_for(row);
			self.sync_status();
		}
		if !session_only && let Err(error) = self.con.exec("writecfg", Source::Console) {
			return self.notice(format!("{} set for this session only: {error}", row.key));
		}
		let label = if row.name.is_empty() {
			row.key.clone()
		} else {
			row.name.clone()
		};
		self.notice(if task {
			format!("Task subagents now use {label}")
		} else if session_only {
			format!("Session model: {label}")
		} else {
			format!("Model: {label} (saved to config.cfg)")
		})
	}

	/// Clamps `ai_thinking` to what the newly selected model supports.
	fn reset_thinking_for(&self, row: &ModelRow) {
		let current = AI_THINKING.get(&self.con);
		let supported = current == "off" || row.efforts.iter().any(|effort| *effort == current);
		if !supported {
			let next = row
				.efforts
				.last()
				.cloned()
				.unwrap_or_else(|| Str::new_static("off"));
			let _ = AI_THINKING.set(&self.con, next);
		}
	}

	/// Index of the live model in the picker roster.
	fn current_model_index(&self) -> usize {
		let live = self.live_model();
		self
			.models
			.iter()
			.position(|row| row.key == live)
			.unwrap_or(0)
	}

	/// The model the next turn will use: `ai_model` when set, else the launch
	/// route.
	fn live_model(&self) -> Str {
		let model = AI_MODEL.get(&self.con);
		if model.is_empty() {
			self.model.identifier.clone()
		} else {
			model
		}
	}

	/// pi `cycleThinkingLevel`: off → each catalog effort → off.
	fn cycle_thinking(&mut self) -> Routed {
		let live = self.live_model();
		let efforts = self
			.models
			.iter()
			.find(|row| row.key == live)
			.map(|row| row.efforts.clone())
			.unwrap_or_default();
		if efforts.is_empty() {
			return self.notice(NO_THINKING);
		}
		let current = AI_THINKING.get(&self.con);
		let next = match efforts.iter().position(|effort| *effort == current) {
			Some(index) if index + 1 < efforts.len() => efforts[index + 1].clone(),
			Some(_) => Str::new_static("off"),
			None => efforts[0].clone(),
		};
		match AI_THINKING.set(&self.con, next.clone()) {
			Ok(()) => {
				self.sync_status();
				self.notice(format!("Thinking: {next}"))
			},
			Err(error) => self.notice(error.to_string()),
		}
	}

	/// pi `cycleRoleModels`: step `ai_model` through the role roster and
	/// show the role track with the active role bracketed.
	fn cycle_model(&mut self, backward: bool) -> Routed {
		let distinct = self
			.cycle
			.iter()
			.map(|(_, model)| model)
			.collect::<std::collections::BTreeSet<_>>();
		if distinct.len() < 2 {
			return self.notice("Only one role model available");
		}
		let live = self.live_model();
		let at = self
			.cycle
			.iter()
			.position(|(_, model)| *model == live);
		let len = self.cycle.len();
		let next = match (at, backward) {
			(Some(index), false) => (index + 1) % len,
			(Some(index), true) => (index + len - 1) % len,
			(None, _) => 0,
		};
		let (role, model) = self.cycle[next].clone();
		let row = self.models.iter().find(|row| row.key == model).cloned();
		let script = format!("ai_model {}", omp_con::Value::Str(model.clone()));
		if let Err(error) = self.con.exec(&script, Source::Console) {
			return self.notice(error.to_string());
		}
		if let Some(row) = row.as_ref() {
			self.reset_thinking_for(row);
		}
		self.sync_status();
		let mut track = String::new();
		for (index, (name, _)) in self.cycle.iter().enumerate() {
			if index > 0 {
				track.push_str("  ");
			}
			if index == next {
				track.push('[');
				track.push_str(name);
				track.push(']');
			} else {
				track.push_str(name);
			}
		}
		let label = row.map_or(model, |row| {
			if row.name.is_empty() {
				row.key
			} else {
				row.name
			}
		});
		self.notice(format!("{track}  ·  {role}: {label}"))
	}

	/// Whether the plan Director is engaged on the live chain: an active
	/// `<director family=plan>` anywhere under `<meta><directors>` (frames
	/// nest, so the scan is recursive).
	fn plan_engaged(&self) -> bool {
		let dom = &self.replica;
		let Some(root) = dom
			.children(dom.meta())
			.iter()
			.copied()
			.find(|handle| {
				dom.get(*handle)
					.is_some_and(|node| node.tag == Tag::Known(KnownTag::Directors))
			})
		else {
			return false;
		};
		let family = omp_dom::PropKey::Custom(Str::new_static("family"));
		let status = omp_dom::PropKey::Custom(Str::new_static("status"));
		let mut pending = dom.children(root).to_vec();
		while let Some(handle) = pending.pop() {
			let Some(node) = dom.get(handle) else { continue };
			if node.tag == Tag::Known(KnownTag::Director)
				&& node.prop(&family).and_then(Value::as_str) == Some(PLAN_DIRECTOR)
				&& node.prop(&status).and_then(Value::as_str) == Some("active")
			{
				return true;
			}
			pending.extend(node.kids.iter().copied());
		}
		false
	}

	/// Whether the newest turn closed with an error notice.
	fn last_turn_failed(&self) -> bool {
		let dom = &self.replica;
		let Some(turn) = dom.children(dom.body()).last() else {
			return false;
		};
		dom
			.children(*turn)
			.iter()
			.rev()
			.filter_map(|handle| dom.get(*handle))
			.find(|node| node.tag == Tag::Known(KnownTag::Notice))
			.is_some_and(|node| {
				node.prop(&PropId::Kind.into()).and_then(Value::as_str) == Some("error")
			})
	}

	fn approval_frame(&self, width: u16) -> Option<Frame> {
		self
			.overlays
			.approval()
			.map(|approval| approval_frame(approval, width, &self.ui))
	}

	/// Bottom-anchored picker frame, when a picker is open.
	fn picker_frame(&mut self, size: Size) -> Option<Frame> {
		match self.overlays.active_mut() {
			Some(Overlay::Models(picker)) => Some(picker.frame(size).clone()),
			Some(Overlay::History(picker)) => Some(picker.frame(size).clone()),
			_ => None,
		}
	}

	/// One-row notice frame above the composer, when a notice is showing.
	fn notice_frame(&self, width: u16) -> Option<Frame> {
		let text = Str::new(self.overlays.notice()?);
		let tree = omp_tui::dom! { <text fg=muted truncate>{" "}{text}</text> };
		Some(Ui::from_root(tree, width, self.ui.clone()).frame().clone())
	}
}

/// Projection actor retaining only presentation state and a detached DOM
/// replica.
pub struct Host {
	presenter:     Presenter,
	resize_policy: ResizePolicy,
	projection:    Option<Projection>,
}

impl Host {
	/// Creates an actor. No controller or journal handle is retained.
	#[must_use]
	pub fn new(options: HostOptions) -> Self {
		let resize_policy = options.resize_policy;
		Self { presenter: Presenter::new(options, 80), resize_policy, projection: None }
	}

	/// Runs the real-terminal actor until `C-c`, debug quit, or terminal
	/// closure.
	pub async fn run(mut self) -> Result<(), HostError> {
		let mut caps = detect();
		loop {
			self.presenter.ui = self.presenter.ui.with_terminal_caps(&caps);
			self
				.presenter
				.composer
				.set_context(self.presenter.ui.clone());
			let mut terminal =
				Terminal::enter(TerminalOptions::new(caps).cursor_style(CursorStyle::BlinkingBar))?;
			let mut renderer = Renderer::new(TtyOut::new()?);
			renderer.apply_caps(&caps)?;
			let size = terminal.size()?;
			self.presenter.composer.resize(size.width);
			self.rebuild_projection(size);
			let result = self.event_loop(&mut terminal, &mut renderer, size).await;
			let leave = terminal.leave();
			let pause = match (result, leave) {
				(Err(error), _) => return Err(error),
				(Ok(_), Err(error)) => return Err(error.into()),
				(Ok(pause), Ok(())) => pause,
			};
			// The terminal is fully restored here: the shell, a child editor,
			// or a fresh probe owns it until the loop re-enters.
			match pause {
				Pause::Quit => return Ok(()),
				Pause::Suspend => suspend_process(),
				Pause::ExternalEditor => {
					let draft = self.presenter.composer.text();
					match crate::editor::edit_draft_detached(
						&draft,
						crate::editor::EditorOptions::default(),
					) {
						Ok(Some(edited)) => self.presenter.composer.set_text(&edited),
						Ok(None) => {},
						Err(error) => {
							self.presenter.notice(error.to_string());
						},
					}
				},
				Pause::DisplayReset => caps = detect(),
			}
		}
	}

	async fn event_loop(
		&mut self,
		terminal: &mut Terminal,
		renderer: &mut Renderer<TtyOut>,
		mut size: Size,
	) -> Result<Pause, HostError> {
		self.present(renderer, size)?;
		loop {
			let deadline = self.next_deadline();
			tokio::select! {
				biased;
				terminal_event = terminal.next() => {
					match terminal_event? {
						TerminalEvent::Resize => {
							if let Some(next) = terminal.take_resize()? {
								size = next;
								self.presenter.composer.resize(next.width);
								self.projection_mut().resize(next);
								self.present(renderer, size)?;
							}
						},
						TerminalEvent::Input(event) => {
							if terminal.handle_input_event(&event, renderer)? {
								continue;
							}
							let routed = self.input(event)?;
							if let Some(text) = self.presenter.clipboard.take() {
								terminal.copy_to_clipboard(&text)?;
							}
							match routed {
								Routed::Quit => break,
								Routed::Suspend => return Ok(Pause::Suspend),
								Routed::ExternalEditor => return Ok(Pause::ExternalEditor),
								Routed::DisplayReset => return Ok(Pause::DisplayReset),
								Routed::Ignored => {},
								Routed::Repaint => self.present(renderer, size)?,
								Routed::RebuildProjection => {
									self.rebuild_projection(size);
									self.present(renderer, size)?;
								},
							}
						},
						TerminalEvent::Debug(query) => {
							let value = self.debug_response(query.op, size);
							respond_debug_query(query.id, value);
						},
						TerminalEvent::Effect(_) => {},
						TerminalEvent::Closed => break,
					}
				},
				() = frame_deadline(self.presenter.clock, deadline) => {
					if self.tick() {
						self.present(renderer, size)?;
					}
				},
				dom_event = self.presenter.dom_events.recv_async() => {
					let Ok(event) = dom_event else { break };
					let reset = matches!(event, Event::Reset { .. });
					self.presenter.apply_dom_event(&event)?;
					if reset {
						self.rebuild_projection(size);
					} else {
						self.reconcile_projection(size);
					}
					self.present(renderer, size)?;
				},
				kernel_event = self.presenter.kernel_events.recv_async() => {
					if kernel_event.is_err() {
						break;
					}
				},
			}
		}
		let _ = self.presenter.up.send(Up::Cancel);
		let _ = self.presenter.commands.send(HostCommand::Quit);
		Ok(Pause::Quit)
	}

	/// Earliest animation wake across the composer and mounted blocks, in
	/// host-clock time.
	fn next_deadline(&self) -> Option<Duration> {
		let composer = self.presenter.composer.next_wake();
		let blocks = self
			.projection
			.as_ref()
			.and_then(|projection| projection.next_wake());
		match (composer, blocks) {
			(Some(a), Some(b)) => Some(a.min(b)),
			(a, b) => a.or(b),
		}
	}

	fn tick(&mut self) -> bool {
		let now = self.presenter.clock.elapsed();
		let composer = self.presenter.composer.tick(now);
		let blocks = self
			.projection
			.as_mut()
			.is_some_and(|projection| projection.tick(now));
		composer || blocks
	}

	fn debug_response(&self, op: DebugOp, size: Size) -> serde_json::Value {
		let presenter = &self.presenter;
		match op {
			DebugOp::Frame => {
				let mut lines =
					crate::project::block_views(&presenter.replica, presenter.show_thinking())
						.into_iter()
						.flat_map(|block| {
							block
								.text
								.as_str()
								.lines()
								.map(str::to_owned)
								.collect::<Vec<_>>()
						})
						.collect::<Vec<_>>();
				lines.push(StatusLine::from_dom(&presenter.replica).text().to_string());
				lines.push(presenter.composer.text());
				if let Some(notice) = presenter.overlays.notice() {
					lines.push(notice.to_owned());
				}
				if let Some(approval) = presenter.overlays.approval() {
					lines.push(approval.title.to_string());
					lines.push(approval.reason.to_string());
					lines.push("y approve  a approve for session  n deny".to_owned());
				}
				serde_json::json!({"ok": true, "lines": lines})
			},
			DebugOp::Tree => {
				let children =
					crate::project::block_views(&presenter.replica, presenter.show_thinking())
						.into_iter()
						.map(|block| {
							serde_json::json!({
								"kind": "TranscriptBlock",
								"id": block.key.to_string(),
								"rect": [0, 0, size.width, 0],
								"children": [],
							})
						})
						.collect::<Vec<_>>();
				let overlays = presenter
					.overlays
					.approval()
					.map(|approval| {
						vec![serde_json::json!({
							"kind": "Approval",
							"id": approval.id,
							"rect": [0, 0, size.width, size.height],
							"visible": true,
							"focus": true,
						})]
					})
					.unwrap_or_default();
				serde_json::json!({
					"ok": true,
					"tree": {
						"root": {
							"kind": "Chat",
							"id": "chat",
							"rect": [0, 0, size.width, size.height],
							"children": children,
						},
						"overlays": overlays,
					},
				})
			},
			DebugOp::Values => serde_json::json!({
				"ok": true,
				"values": {
					"composer": presenter.composer.text(),
					"overlay_open": presenter.overlays.modal(),
					"overlay": match presenter.overlays.active() {
						Some(Overlay::Models(_)) => "models",
						Some(Overlay::History(_)) => "history",
						Some(Overlay::Approval(_)) => "approval",
						Some(Overlay::Notice(_)) => "notice",
						None => "",
					},
					"notice": presenter.overlays.notice().unwrap_or_default(),
					"model": presenter.live_model(),
					"thinking": AI_THINKING.get(&presenter.con),
					"turn_active": presenter.turn_active,
					"cursor": presenter.composer.frame().cursor().map(|(column, row)| vec![row, column]),
				},
			}),
			_ => serde_json::json!({"ok": false}),
		}
	}

	fn input(&mut self, event: InputEvent) -> Result<Routed, HostError> {
		match event {
			InputEvent::Key(key) => self.presenter.route_key(key),
			InputEvent::Paste(text) => {
				self.presenter.composer.paste(text.as_str());
				Ok(Routed::Repaint)
			},
			InputEvent::Mouse(_) | InputEvent::Focus(_) | InputEvent::Response(_) => {
				Ok(Routed::Ignored)
			},
		}
	}

	const fn projection_mut(&mut self) -> &mut Projection {
		self
			.projection
			.as_mut()
			.expect("projection initialized before event loop")
	}

	fn rebuild_projection(&mut self, size: Size) {
		let now = self.presenter.clock.elapsed();
		let blocks = self.presenter.blocks();
		let mirror = self.presenter.blocks();
		self.projection =
			Some(Projection::new(size, self.resize_policy, &self.presenter.ui, blocks, mirror, now));
	}

	fn reconcile_projection(&mut self, size: Size) {
		let now = self.presenter.clock.elapsed();
		let blocks = self.presenter.blocks();
		let mirror = self.presenter.blocks();
		if !self.projection_mut().reconcile(blocks, mirror, now) {
			self.rebuild_projection(size);
		}
	}

	fn present(&mut self, renderer: &mut Renderer<TtyOut>, size: Size) -> Result<(), HostError> {
		self.presenter.sync_status();
		let approval = self.presenter.approval_frame(size.width);
		let picker = self.presenter.picker_frame(size);
		let notice = self.presenter.notice_frame(size.width);
		let composer = &self.presenter.composer;
		let projection = self
			.projection
			.as_mut()
			.expect("projection initialized before presentation");
		projection.retire_under_pressure(composer.height(), size.height);
		let document = projection.document(composer.frame(), size);
		let document_options = OverlayOptions::default()
			.width(Dim::Cells(size.width))
			.anchor(OverlayAnchor::TopLeft)
			.non_modal()
			.z(10);
		let approval_options = OverlayOptions::default()
			.width(Dim::Pct(80))
			.anchor(OverlayAnchor::Center)
			.z(30);
		// Pickers replace the composer band (pi swaps the editor slot).
		let picker_options = OverlayOptions::default()
			.width(Dim::Cells(size.width))
			.anchor(OverlayAnchor::BottomLeft)
			.z(20);
		// pi's status row sits directly above the editor.
		let notice_options = OverlayOptions::default()
			.width(Dim::Cells(size.width))
			.anchor(OverlayAnchor::BottomLeft)
			.margin(omp_tui::OverlayMargin { bottom: composer.height(), ..Default::default() })
			.non_modal()
			.z(15);
		let modal = approval.is_some() || picker.is_some();
		let mut layers = vec![Layer {
			frame:   &document,
			options: &document_options,
			active:  !modal,
		}];
		if let Some(frame) = notice.as_ref() {
			layers.push(Layer { frame, options: &notice_options, active: false });
		}
		if let Some(frame) = picker.as_ref() {
			layers.push(Layer { frame, options: &picker_options, active: approval.is_none() });
		}
		if let Some(frame) = approval.as_ref() {
			layers.push(Layer { frame, options: &approval_options, active: true });
		}
		let plan = projection.slots.plan();
		match renderer.present_plan(&plan, &layers) {
			Ok(delivered) => {
				projection.slots.commit(plan, delivered);
				Ok(())
			},
			Err(error) => {
				let delivered = error.delivered();
				projection.slots.commit(plan, delivered);
				Err(error.into())
			},
		}
	}
}

/// Effect requested by the detached native-window actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeEffect {
	/// No controller or paint state changed.
	Ignored,
	/// The event changed presentation state.
	Consumed,
	/// Close the native window.
	Quit,
}

/// Native-window actor over the same detached snapshot and patch stream as
/// [`Host`].
///
/// Window creation and GPU delivery stay in `omp-gui`; this type owns only the
/// projection, composer, overlays, and command mailbox.
pub struct NativeHost {
	presenter:      Presenter,
	frame:          Frame,
	approval_frame: Option<Frame>,
	size:           Size,
}

impl NativeHost {
	/// Creates a native actor without retaining a controller or journal handle.
	#[must_use]
	pub fn new(options: HostOptions, size: Size) -> Self {
		let mut host = Self {
			presenter: Presenter::new(options, size.width),
			frame: Frame::new(size),
			approval_frame: None,
			size,
		};
		host.refresh();
		host
	}

	/// Applies queued controller events, returning whether a repaint is needed.
	pub fn poll(&mut self) -> Result<NativeEffect, HostError> {
		let mut changed = false;
		while let Ok(event) = self.presenter.dom_events.try_recv() {
			self.presenter.apply_dom_event(&event)?;
			changed = true;
		}
		while self.presenter.kernel_events.try_recv().is_ok() {
			changed = true;
		}
		let now = self.presenter.clock.elapsed();
		changed |= self.presenter.composer.tick(now);
		if changed {
			self.refresh();
			Ok(NativeEffect::Consumed)
		} else {
			Ok(NativeEffect::Ignored)
		}
	}

	/// Reflows the native projection for a new cell viewport.
	pub fn resize(&mut self, size: Size) {
		self.size = size;
		self.presenter.composer.resize(size.width);
		self.refresh();
	}

	/// Routes one real native key through the chat input path.
	pub fn key(&mut self, key: Key) -> Result<NativeEffect, HostError> {
		Ok(match self.presenter.route_key(key)? {
			Routed::Quit => NativeEffect::Quit,
			Routed::Ignored => NativeEffect::Ignored,
			// A window has no tty to release: the external editor runs in
			// place, suspend and display reset degrade to a repaint.
			Routed::ExternalEditor => {
				let draft = self.presenter.composer.text();
				match crate::editor::edit_draft_detached(
					&draft,
					crate::editor::EditorOptions::default(),
				) {
					Ok(Some(edited)) => self.presenter.composer.set_text(&edited),
					Ok(None) => {},
					Err(error) => {
						self.presenter.notice(error.to_string());
					},
				}
				self.refresh();
				NativeEffect::Consumed
			},
			Routed::Repaint
			| Routed::RebuildProjection
			| Routed::Suspend
			| Routed::DisplayReset => {
				self.refresh();
				NativeEffect::Consumed
			},
		})
	}

	/// Executes a console line exactly as a bound key would, applying every
	/// host action it posts.
	pub fn console(&mut self, command: &str) -> Result<NativeEffect, HostError> {
		Ok(match self.presenter.run_console(command)? {
			Routed::Quit => NativeEffect::Quit,
			Routed::Ignored => NativeEffect::Ignored,
			_ => {
				self.refresh();
				NativeEffect::Consumed
			},
		})
	}

	/// Text of the visible transient notice, when one is showing.
	#[must_use]
	pub fn notice(&self) -> Option<&str> {
		self.presenter.overlays.notice()
	}

	/// Whether a key-consuming overlay (picker or approval) is open.
	#[must_use]
	pub fn overlay_open(&self) -> bool {
		self.presenter.overlays.modal()
	}

	/// Frame of the open picker, when one is showing.
	pub fn picker_frame(&mut self) -> Option<Frame> {
		self.presenter.picker_frame(self.size)
	}

	/// Routes clipboard text through the same composer used by the terminal.
	pub fn paste(&mut self, text: &str) -> NativeEffect {
		self.presenter.composer.paste(text);
		self.refresh();
		NativeEffect::Consumed
	}

	/// Returns the current document frame.
	#[must_use]
	pub const fn frame(&self) -> &Frame {
		&self.frame
	}

	/// Returns the current approval layer, when controller policy is blocked.
	#[must_use]
	pub const fn approval_frame(&self) -> Option<&Frame> {
		self.approval_frame.as_ref()
	}

	/// Returns the number of document-tail rows owned by the composer.
	#[must_use]
	pub const fn editor_rows(&self) -> u16 {
		self.presenter.composer.height()
	}

	fn refresh(&mut self) {
		self.presenter.sync_status();
		let components = self
			.presenter
			.blocks()
			.into_iter()
			.map(|block| block.component)
			.collect::<Vec<_>>();
		let tree = omp_tui::dom! { <col gap=1>{components}</col> };
		let transcript = Ui::from_root(tree, self.size.width, self.presenter.ui.clone());
		let rows = transcript.frame().size().height;
		let composer = self.presenter.composer.frame();
		let height = rows.saturating_add(composer.size().height);
		let mut frame = Frame::new(Size::new(self.size.width, height));
		frame.blit(transcript.frame(), 0, rows, 0, 0);
		frame.blit(composer, 0, composer.size().height, 0, rows);
		self.frame = frame;
		self.approval_frame = self.presenter.approval_frame(self.size.width);
	}
}

impl Drop for NativeHost {
	fn drop(&mut self) {
		let _ = self.presenter.up.send(Up::Cancel);
		let _ = self.presenter.commands.send(HostCommand::Quit);
	}
}

/// Sleeps until `deadline` on the presentation clock whose epoch is `clock`.
async fn frame_deadline(clock: Instant, deadline: Option<Duration>) {
	match deadline {
		Some(deadline) => tokio::time::sleep_until((clock + deadline).into()).await,
		None => future::pending().await,
	}
}

/// Project root for `@` file completion: the session cwd projected into
/// the prompt facts, when the kernel has published one.
fn project_root(dom: &Dom) -> Option<PathBuf> {
	let session = StatusLine::from_dom(dom).session;
	(!session.is_empty()).then(|| PathBuf::from(session.as_str()))
}

/// Derives the composer status facts from the replica, the launch badge, and
/// the observer-local facts. `working` is the in-flight turn's start on the
/// presentation clock.
fn status_facts(
	dom: &Dom,
	badge: &ModelBadge,
	local: &LocalFacts,
	working: Option<Duration>,
) -> StatusFacts {
	let status = StatusLine::from_dom(dom);
	// A live `ai_model` pick shows before its first turn journals it.
	let route = local
		.model
		.as_deref()
		.unwrap_or(status.model.as_str());
	let model = if route.is_empty() || route == badge.identifier {
		badge.short_name()
	} else {
		ModelBadge::from_identifier(route).short_name()
	};
	let home = (!status.home.is_empty()).then_some(status.home.as_str());
	let path = display_path(status.session.as_str(), home, local.tmp.as_deref());
	StatusFacts {
		model,
		thinking: local.thinking.clone(),
		cwd: path.text,
		scratch: path.scratch,
		branch: local.branch.clone(),
		tokens: status.context,
		context_window: badge.context_window,
		compact_percent: local.compact,
		working,
	}
}

/// Retained transcript: the history ledger plus one live tree per block.
struct Projection {
	slots:  Slots,
	blocks: Vec<Mounted>,
	ctx:    UiContext,
	width:  u16,
}

struct Mounted {
	view:    BlockView,
	id:      BlockId,
	ui:      Ui,
	/// Host-clock time when `ui` was created; its animation clock epoch.
	epoch:   Duration,
	retired: bool,
}

impl Mounted {
	const fn rows(&self) -> u16 {
		self.ui.frame().size().height
	}
}

/// Wraps a block for retention: the block plus one blank separator row,
/// so history rows carry the same spacing as the live document.
fn spaced(component: crate::cards::Component) -> Col {
	Col::new().child(component).child(Spacer::new())
}

impl Projection {
	fn new(
		size: Size,
		policy: ResizePolicy,
		ctx: &UiContext,
		blocks: Vec<RenderedBlock>,
		mirror: Vec<RenderedBlock>,
		now: Duration,
	) -> Self {
		let mut slots = Slots::new(size.width, size.height, policy);
		slots.set_context(ctx.clone());
		let mut projection = Self {
			slots,
			blocks: Vec::with_capacity(blocks.len()),
			ctx: ctx.clone(),
			width: size.width,
		};
		for (block, twin) in blocks.into_iter().zip(mirror) {
			projection.mount(block, twin, now);
		}
		projection
	}

	fn mount(&mut self, block: RenderedBlock, twin: RenderedBlock, now: Duration) {
		let mounted = self.open(block, twin, now);
		self.blocks.push(mounted);
	}

	fn open(&mut self, block: RenderedBlock, twin: RenderedBlock, now: Duration) -> Mounted {
		let id = self.slots.open(Mode::Mutable);
		self.slots.set(id, spaced(block.component));
		let ui = Ui::from_root(spaced(twin.component), self.width, self.ctx.clone());
		Mounted { view: block.view, id, ui, epoch: now, retired: false }
	}

	/// Applies a fresh projection. Blocks may appear anywhere (a thinking
	/// block materializing after the answer started); a mounted block that
	/// disappears or reorders returns `false` and the caller rebuilds.
	fn reconcile(
		&mut self,
		blocks: Vec<RenderedBlock>,
		mirror: Vec<RenderedBlock>,
		now: Duration,
	) -> bool {
		let mut next_keys = blocks.iter().map(|block| block.view.key);
		let is_subsequence = self
			.blocks
			.iter()
			.all(|mounted| next_keys.any(|key| key == mounted.view.key));
		if !is_subsequence {
			return false;
		}
		let mut old = std::mem::take(&mut self.blocks).into_iter().peekable();
		let mut merged = Vec::with_capacity(blocks.len());
		for (next, twin) in blocks.into_iter().zip(mirror) {
			let Some(mut mounted) = old.next_if(|mounted| mounted.view.key == next.view.key) else {
				merged.push(self.open(next, twin, now));
				continue;
			};
			// Rows already in native scrollback are never rewritten (ADR 0034).
			if mounted.view != next.view && !mounted.retired {
				self.slots.set(mounted.id, spaced(next.component));
				mounted.ui = Ui::from_root(spaced(twin.component), self.width, self.ctx.clone());
				mounted.epoch = now;
			}
			mounted.view = next.view;
			merged.push(mounted);
		}
		self.blocks = merged;
		true
	}

	fn resize(&mut self, size: Size) {
		self.width = size.width;
		self.slots.resize(size.width, size.height);
		for mounted in &mut self.blocks {
			mounted.ui.resize(size.width);
		}
	}

	fn live(&self) -> impl Iterator<Item = &Mounted> {
		self.blocks.iter().filter(|mounted| !mounted.retired)
	}

	fn live_rows(&self) -> u32 {
		self.live().map(|mounted| u32::from(mounted.rows())).sum()
	}

	/// Retires the oldest finished blocks into native scrollback until the
	/// live document fits the terminal (the row-pressure rule of ADR 0034).
	fn retire_under_pressure(&mut self, chrome_rows: u16, height: u16) {
		let budget = u32::from(height);
		let mut live_rows = self.live_rows().saturating_add(u32::from(chrome_rows));
		for mounted in &mut self.blocks {
			if live_rows <= budget {
				break;
			}
			if mounted.retired {
				continue;
			}
			if !mounted.view.finalized {
				break;
			}
			self.slots.finalize(mounted.id);
			mounted.retired = true;
			live_rows = live_rows.saturating_sub(u32::from(mounted.rows()));
		}
	}

	/// Composes the on-screen document: live blocks then the composer,
	/// top-anchored while everything fits and tail-anchored otherwise.
	fn document(&self, composer: &Frame, size: Size) -> Frame {
		let mut document = Frame::new(size);
		let chrome_rows = composer.size().height.min(size.height);
		let content_rows = self.live_rows();
		let available = u32::from(size.height.saturating_sub(chrome_rows));
		if content_rows <= available {
			let mut y = 0_u16;
			for mounted in self.live() {
				let frame = mounted.ui.frame();
				document.blit(frame, 0, frame.size().height, 0, y);
				y = y.saturating_add(frame.size().height);
			}
			document.blit(composer, 0, chrome_rows, 0, y);
			return document;
		}
		let mut bottom = u16::try_from(available).unwrap_or(u16::MAX);
		let live = self.live().collect::<Vec<_>>();
		for mounted in live.into_iter().rev() {
			if bottom == 0 {
				break;
			}
			let frame = mounted.ui.frame();
			let rows = frame.size().height;
			if rows <= bottom {
				bottom -= rows;
				document.blit(frame, 0, rows, 0, bottom);
			} else {
				document.blit(frame, rows - bottom, bottom, 0, 0);
				bottom = 0;
			}
		}
		let chrome_top = u16::try_from(available).unwrap_or(u16::MAX);
		document.blit(composer, composer.size().height - chrome_rows, chrome_rows, 0, chrome_top);
		document
	}

	fn next_wake(&self) -> Option<Duration> {
		self
			.live()
			.filter_map(|mounted| {
				mounted
					.ui
					.next_wake()
					.map(|wake| mounted.epoch.saturating_add(wake))
			})
			.min()
	}

	fn tick(&mut self, now: Duration) -> bool {
		let mut changed = false;
		for mounted in self.blocks.iter_mut().filter(|mounted| !mounted.retired) {
			let local = now.saturating_sub(mounted.epoch);
			changed |= mounted.ui.tick(local);
		}
		changed
	}
}

/// Whether the kernel is still working on the last turn: decided by the
/// newest lifecycle element in it. A receipt or notice closes the turn; an
/// open assistant or running tool keeps it active; a turn with only the
/// user's message is awaiting its first inference.
fn has_active_turn(dom: &Dom) -> bool {
	let Some(turn) = dom.children(dom.body()).last() else {
		return false;
	};
	for child in dom.children(*turn).iter().rev() {
		let Some(node) = dom.get(*child) else {
			continue;
		};
		match node.tag {
			Tag::Known(KnownTag::Usage | KnownTag::Notice) => return false,
			Tag::Known(KnownTag::Assistant) => {
				return node.prop(&PropId::StopReason.into()).is_none();
			},
			Tag::Custom(_) => {
				return !node
					.prop(&PropId::Status.into())
					.and_then(Value::as_str)
					.is_some_and(|status| matches!(status, "ok" | "error" | "cancelled" | "aborted"));
			},
			_ => {},
		}
	}
	true
}

fn approval_frame(
	approval: &crate::overlays::ApprovalOverlay,
	width: u16,
	ui: &UiContext,
) -> Frame {
	let title = approval.title.clone();
	let reason = approval.reason.clone();
	let scope = Str::new(approval.scope.as_str());
	let tree = omp_tui::dom! {
		<box border=round bc=warning pad="1 2">
			<col gap=1>
				<text fg=warning attr=bold>{title}</text>
				<md>{reason}</md>
				<text fg=muted>{"Scope: "}{scope}</text>
				<row gap=1>
					<text fg=accent attr=bold>{"y"}</text>
					<text>{"approve"}</text>
					<text fg=accent attr=bold>{"a"}</text>
					<text>{"approve for session"}</text>
					<text fg=error attr=bold>{"n"}</text>
					<text>{"deny"}</text>
				</row>
			</col>
		</box>
	};
	Ui::from_root(tree, width, ui.clone()).frame().clone()
}

/// Renders the interactive surface headlessly at `size`: the document a
/// terminal host would paint from `snapshot` before any input (the chrome
/// golden test's entry point).
#[must_use]
pub fn render_surface(
	snapshot: &Snapshot,
	model: &ModelBadge,
	local: &LocalFacts,
	size: Size,
	ui: &UiContext,
) -> Frame {
	let replica = Dom::from_snapshot(snapshot);
	let working = has_active_turn(&replica).then_some(Duration::ZERO);
	let facts = status_facts(&replica, model, local, working);
	let composer = Composer::new(size.width, ui.clone(), facts, Vec::new(), None);
	let status = StatusLine::from_dom(&replica);
	let welcome = || RenderedBlock {
		view:      BlockView {
			key:       0,
			kind:      BlockKind::Welcome,
			text:      Str::new_static("welcome"),
			mode:      Mode::Mutable,
			finalized: true,
		},
		component: Box::new(Welcome::new(
			Str::new_static(env!("CARGO_PKG_VERSION")),
			model,
			tip_for(status.session.as_str()),
		)),
	};
	let cards = CardRegistry::standard();
	let mut blocks = vec![welcome()];
	blocks.extend(project(&replica, &cards, ui, true));
	let mut mirror = vec![welcome()];
	mirror.extend(project(&replica, &cards, ui, true));
	let projection =
		Projection::new(size, ResizePolicy::Rebuild, ui, blocks, mirror, Duration::ZERO);
	projection.document(composer.frame(), size)
}

#[cfg(test)]
mod tests {
	use omp_tui::IntoComponent as _;

	use super::*;

	fn block(key: u64, kind: BlockKind, text: &'static str, finalized: bool) -> RenderedBlock {
		RenderedBlock {
			view:      BlockView {
				key,
				kind,
				text: Str::new_static(text),
				mode: Mode::Mutable,
				finalized,
			},
			component: text.into_component(),
		}
	}

	fn projection(rows: u16, finalized: &[bool]) -> Projection {
		let build = || {
			finalized
				.iter()
				.enumerate()
				.map(|(index, done)| block(index as u64 + 1, BlockKind::User, "row", *done))
				.collect::<Vec<_>>()
		};
		Projection::new(
			Size::new(20, rows),
			ResizePolicy::Rebuild,
			&UiContext::default(),
			build(),
			build(),
			Duration::ZERO,
		)
	}

	#[test]
	fn blocks_stay_live_until_row_pressure_then_retire_oldest_done_first() {
		// Three two-row blocks (text + spacer) plus a three-row chrome fit in ten rows.
		let mut projection = projection(10, &[true, true, false]);
		projection.retire_under_pressure(3, 10);
		assert_eq!(projection.live().count(), 3);
		assert!(projection.slots.plan().rows().is_empty(), "nothing retires without pressure");

		// Shrinking to seven rows retires exactly the oldest finished block.
		projection.retire_under_pressure(3, 7);
		assert!(projection.blocks[0].retired);
		assert!(!projection.blocks[1].retired);
		assert_eq!(projection.slots.plan().rows().len(), 2);

		// An unfinished frontier block stalls later retirement (ADR 0034).
		projection.retire_under_pressure(3, 1);
		assert!(projection.blocks[1].retired);
		assert!(!projection.blocks[2].retired);
	}

	#[test]
	fn document_is_top_anchored_when_it_fits_and_tail_anchored_otherwise() {
		let projection = projection(6, &[true, true]);
		let mut chrome = Frame::new(Size::new(20, 1));
		chrome.put(0, 0, "chrome", omp_tui::Style::default());
		chrome.set_cursor(6, 0);
		let fitting = projection.document(&chrome, Size::new(20, 6));
		let rows = omp_tui::frame_text(&fitting)
			.lines()
			.map(str::to_owned)
			.collect::<Vec<_>>();
		assert_eq!(rows[0].trim_end(), "row");
		assert_eq!(rows[4].trim_end(), "chrome");
		assert_eq!(fitting.cursor(), Some((6, 4)));

		let tail = projection.document(&chrome, Size::new(20, 3));
		let rows = omp_tui::frame_text(&tail)
			.lines()
			.map(str::to_owned)
			.collect::<Vec<_>>();
		assert_eq!(rows[0].trim_end(), "row", "the newest block's rows fill from the bottom");
		assert_eq!(rows[2].trim_end(), "chrome");
		assert_eq!(tail.cursor(), Some((6, 2)));
	}

	#[test]
	fn streaming_under_pressure_never_rebuilds_history() {
		use omp_tui::slots::Delivered;
		let lines = |count: usize| -> String {
			(1..=count)
				.map(|index| index.to_string())
				.collect::<Vec<_>>()
				.join("\n")
		};
		let mut projection = projection(12, &[true]);
		let mut retired_rows = 0;
		let mut step = |projection: &mut Projection, blocks: Vec<(u64, String, bool)>| {
			let build = || {
				let mut out = vec![block(1, BlockKind::User, "row", true)];
				out.extend(blocks.iter().map(|(key, text, done)| RenderedBlock {
					view:      BlockView {
						key:       *key,
						kind:      BlockKind::Assistant,
						text:      Str::new(text.as_str()),
						mode:      Mode::Mutable,
						finalized: *done,
					},
					component: Str::new(text.as_str()).into_component(),
				}));
				out
			};
			assert!(projection.reconcile(build(), build(), Duration::ZERO));
			projection.retire_under_pressure(3, 12);
			let plan = projection.slots.plan();
			assert!(!plan.rebuild(), "streaming must never reset native history");
			retired_rows += plan.rows().len();
			projection.slots.commit(plan, Delivered::All);
		};
		for count in 1..=20 {
			step(&mut projection, vec![(2, lines(count), false)]);
		}
		step(&mut projection, vec![(2, lines(20), true)]);
		step(&mut projection, vec![(2, lines(20), true), (3, lines(5), true)]);
		assert_eq!(retired_rows, projection.slots.logical_history().count());
		assert_eq!(retired_rows, 2 + 21, "the first block and the finished stream retired once each");
	}

	#[test]
	fn live_turn_reconciles_without_rebuilding() {
		use omp_session::{ComponentRegistry, Session};
		let directory = tempfile::tempdir().expect("temp directory");
		let path = directory.path().join("turn.oms");
		let mut session = Session::create(path, ComponentRegistry::standard()).expect("session");
		let cards = CardRegistry::standard();
		let ui = UiContext::default();
		let blocks = |session: &Session| {
			let mut out = vec![block(0, BlockKind::Welcome, "welcome", true)];
			out.extend(project(session.dom(), &cards, &ui, true));
			out
		};
		let mut projection = Projection::new(
			Size::new(60, 12),
			ResizePolicy::Rebuild,
			&ui,
			blocks(&session),
			blocks(&session),
			Duration::ZERO,
		);
		let mut check = |session: &Session, projection: &mut Projection, step: &str| {
			assert!(
				projection.reconcile(blocks(session), blocks(session), Duration::ZERO),
				"reconcile rebuilt at {step}"
			);
		};
		session.begin_turn().expect("turn");
		check(&session, &mut projection, "turn");
		session.user("hello", Vec::new()).expect("user");
		check(&session, &mut projection, "user");
		session
			.assistant_start("test/model", "test", "test/model")
			.expect("assistant");
		check(&session, &mut projection, "assistant start");
		let turn = *session
			.dom()
			.children(session.dom().body())
			.last()
			.expect("turn");
		let assistant = *session.dom().children(turn).last().expect("assistant");
		let thinking = session
			.stream_open(assistant, PropId::Thinking.into())
			.expect("thinking");
		check(&session, &mut projection, "thinking open");
		for delta in ["think", "ing\nmore"] {
			session.stream_append(thinking, delta).expect("delta");
			check(&session, &mut projection, "thinking delta");
		}
		session.stream_close(thinking).expect("close");
		check(&session, &mut projection, "thinking close");
		let text = session
			.stream_open(assistant, PropId::Text.into())
			.expect("text");
		check(&session, &mut projection, "text open");
		for delta in ["1\n", "2\n", "3\n"] {
			session.stream_append(text, delta).expect("delta");
			check(&session, &mut projection, "text delta");
		}
		let live = crate::project::block_views(session.dom(), true);
		assert_eq!(live[1].kind, BlockKind::Thinking);
		assert_eq!(live[1].text, "thinking\nmore", "open thinking stream projects live");
		assert_eq!(live[2].kind, BlockKind::Assistant);
		assert_eq!(live[2].text, "1\n2\n3\n", "open text stream projects live");
		session.stream_close(text).expect("close");
		session.assistant_end("stop").expect("end");
		check(&session, &mut projection, "assistant end");
		session.receipt(10, 5, 0).expect("receipt");
		check(&session, &mut projection, "receipt");
	}

	#[test]
	fn reconcile_keeps_retired_rows_and_replaces_changed_live_blocks() {
		let mut projection = projection(4, &[true, true]);
		projection.retire_under_pressure(3, 4);
		assert!(projection.blocks[0].retired);
		let next = vec![
			block(1, BlockKind::User, "changed", true),
			block(2, BlockKind::User, "changed", true),
		];
		let mirror = vec![
			block(1, BlockKind::User, "changed", true),
			block(2, BlockKind::User, "changed", true),
		];
		assert!(projection.reconcile(next, mirror, Duration::ZERO));
		assert_eq!(projection.blocks[0].view.text, "changed");
		assert!(!projection.reconcile(
			vec![block(9, BlockKind::User, "x", true)],
			vec![block(9, BlockKind::User, "x", true)],
			Duration::ZERO
		));
	}

	#[test]
	fn reconcile_inserts_a_block_that_materializes_before_an_existing_one() {
		let mut projection = projection(20, &[true, false]);
		let build = || {
			vec![
				block(1, BlockKind::User, "row", true),
				block(5, BlockKind::Thinking, "late thinking", false),
				block(2, BlockKind::Assistant, "row", false),
			]
		};
		assert!(projection.reconcile(build(), build(), Duration::ZERO));
		assert_eq!(
			projection
				.blocks
				.iter()
				.map(|mounted| mounted.view.key)
				.collect::<Vec<_>>(),
			[1, 5, 2]
		);
		assert_eq!(projection.slots.logical_history().count(), 0);
		let dropped = || {
			vec![block(1, BlockKind::User, "row", true), block(2, BlockKind::Assistant, "row", false)]
		};
		assert!(
			!projection.reconcile(dropped(), dropped(), Duration::ZERO),
			"a vanished block rebuilds"
		);
	}
}
