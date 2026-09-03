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
use omp_con::{
	AI_COMPACT_THRESHOLD, AI_FASTMODE, AI_MODEL, AI_THINKING, CL_IME_SAFE_CURSOR, CL_SHOWTHINKING,
	CL_STATUS_COMPACT_THINKING, Ctx, Source,
};
use omp_core::Str;
use omp_dom::{Dom, Event, KnownTag, PropId, Snapshot, Tag, Value};
use omp_journal::{EntryId, blob::BlobRef};
use omp_tui::{
	CursorStyle, DebugOp, Dim, Frame, InputEvent, Key, Layer, OverlayAnchor, OverlayOptions,
	Renderer, Size, Terminal, TerminalEvent, TerminalOptions, TtyOut, Ui, UiContext,
	anim::Intro,
	components::Countdown,
	detect,
	paste::{Clipboard, ClipboardRead, spawn_clipboard_read},
	respond_debug_query,
	slots::{Mode, ResizePolicy},
};
use thiserror::Error;
use tokio::sync::oneshot;

use crate::{
	actions::{CL_DOUBLE_ESCAPE, CL_STT_HOLD, EscapeHook, EscapeRung, HostAction, HostMailbox},
	autocomplete::slash,
	cards::CardRegistry,
	chrome::{ModelBadge, StatusFacts, Welcome, display_path, tip_for},
	commands::{CompactionMethod, Selector},
	composer::{Composer, ComposerAction, SpaceHold, SpaceHoldEvent},
	gitwatch::{GitFacts, GitWatch},
	input::Bindings,
	overlays::{
		HistoryPicker, ModelPicker, ModelRow, Overlay, Overlays, PanelAction, PanelAnchor,
		PanelCx, PanelEvent, PickerEvent, Services,
	},
	project::{BlockKind, BlockView, RenderedBlock, project},
	status_band::Speculation,
	status_line::StatusLine,
	transcript::Projection,
	welcome::{WelcomeFacts, tip_seeded, welcome_seed},
};

/// Console command that engages the plan Director.
const PLAN_DIRECTOR: &str = "plan";
/// Director family engaged by `/loop` (rung 5 of the Esc ladder).
const LOOP_DIRECTOR: &str = "loop_mode";
/// Notice shown when a bound command wants a reasoning level the model lacks.
const NO_THINKING: &str = "Current model does not support thinking";
/// pi: a second Esc within this window on an empty composer opens the
/// rewind or tree selector.
const DOUBLE_ESCAPE_WINDOW: Duration = Duration::from_millis(500);
/// pi `LEFT_DOUBLE_TAP_MIN_GAP_MS`: taps closer than this are a terminal
/// burst, never a human double-tap.
const LEFT_DOUBLE_TAP_MIN_GAP: Duration = Duration::from_millis(40);
/// pi `LEFT_DOUBLE_TAP_MAX_GAP_MS`: a quiet gap this long starts a fresh
/// tap sequence.
const LEFT_DOUBLE_TAP_MAX_GAP: Duration = Duration::from_millis(500);
/// How long a background clipboard read may take before the paste is
/// abandoned.
const CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_secs(8);
/// pi `process.exit(130)`: a second Ctrl+C while teardown hangs.
const HARD_ABORT_CODE: i32 = 130;

/// Which side-channel spawn a slash command asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnKind {
	/// `/btw`: a side question that never enters the main transcript.
	Btw,
	/// `/tan`: a fire-and-forget background task.
	Tan,
}

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
	/// Switch to a stored session file (`/resume`, session picker).
	SessionOpen {
		/// Journal path.
		path: PathBuf,
	},
	/// Start a brand-new session file (`/new`, `/fresh`).
	SessionNew {
		/// Optional model selector for the new session.
		model: Option<Str>,
	},
	/// Delete the current session file and restart (`/drop`).
	SessionDrop,
	/// Roll the live chain back to `target` (`/rewind`, rewind selector).
	Rewind {
		/// Entry the next append branches from.
		target: EntryId,
	},
	/// Branch a new session file from `target` (`/fork`).
	Fork {
		/// Entry to branch from; `None` is the current head.
		target: Option<EntryId>,
	},
	/// Run a manual compaction path (`/compact`, `/handoff`, `/shake`).
	Compact {
		/// Summary path.
		method: CompactionMethod,
		/// Focus instructions, handoff prompt, or shake mode.
		hint:   Option<Str>,
	},
	/// Append a prompt to `<queues><prompts>` (`/queue`).
	Queue {
		/// Prompt run after the active turn.
		prompt: Str,
	},
	/// Engage or exit a Director by id (`/vibe`, `/goal`, `/loop`, `/force`).
	Director {
		/// Director family.
		id:     Str,
		/// `true` engages; `false` exits.
		engage: bool,
		/// Director arguments.
		args:   Vec<Str>,
	},
	/// Spawn a side-channel agent (`/btw`, `/tan`).
	Spawn {
		/// Which side channel.
		kind: SpawnKind,
		/// Prompt text.
		text: Str,
	},
	/// Rename the session (`/rename`).
	Rename {
		/// Human-readable session title.
		title: Str,
	},
	/// Edit the `<meta><todo>` checklist (`/todo`).
	Todo(crate::commands::TodoOp),
	/// Gate new turns and queued prompts (`/pause`).
	Pause {
		/// `true` pauses; `false` resumes.
		active: bool,
	},
	/// Mark queued prompts as dequeued: the host pulled their text back into
	/// the composer (pi `app.message.dequeue`).
	Dequeue {
		/// `<prompt id>` values under `<queues><prompts>`.
		prompts: Vec<Str>,
	},
	/// Push-to-talk recording edge: the app owns the microphone lease and
	/// the recognizer, and posts the transcript back through the console
	/// mailbox as [`HostAction::InsertText`].
	PushToTalk {
		/// `true` starts recording; `false` stops it and transcribes.
		active: bool,
	},
	/// Duplex live-voice session edge (pi `/live`, Ctrl+L): the app owns the
	/// microphone lease and the realtime transport.
	LiveVoice {
		/// `true` starts the session; `false` ends it.
		active: bool,
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
	/// Project directory. The host watches its git checkout for the band's
	/// branch/dirty facts: observer-local, never journaled.
	pub project:       PathBuf,
	/// Launch facts for the welcome box (recent sessions, language servers).
	pub welcome:       WelcomeFacts,
	/// Ambient renderer context.
	pub ui:            UiContext,
	/// Application-supplied data feeds for dashboards and account commands.
	pub services:      Arc<dyn Services>,
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

/// pi `#detectLeftDoubleTap`: two Left taps a human-plausible interval
/// apart, never a terminal-synthesized burst.
#[derive(Clone, Copy, Debug, Default)]
struct LeftTaps {
	last:  Option<Instant>,
	count: u8,
}

impl LeftTaps {
	/// Records a tap at `now`; returns whether it completed a double-tap.
	fn tap(&mut self, now: Instant) -> bool {
		let since = self.last.map(|last| now.duration_since(last));
		self.last = Some(now);
		match since {
			Some(gap) if gap < LEFT_DOUBLE_TAP_MAX_GAP => {
				self.count = self.count.saturating_add(1);
				if self.count == 2 && gap >= LEFT_DOUBLE_TAP_MIN_GAP {
					self.count = 0;
					self.last = None;
					return true;
				}
				false
			},
			_ => {
				self.count = 1;
				false
			},
		}
	}
}

/// Presentation state shared by the terminal and native actors.
pub(crate) struct Presenter {
	pub(crate) replica:        Dom,
	pub(crate) dom_events:     Receiver<Event>,
	pub(crate) kernel_events:  Receiver<KernelEvent>,
	pub(crate) commands:       Sender<HostCommand>,
	pub(crate) up:             Sender<Up>,
	pub(crate) con:            Arc<Ctx>,
	pub(crate) bindings:       Bindings,
	pub(crate) cards:          CardRegistry,
	pub(crate) ui:             UiContext,
	pub(crate) model:          ModelBadge,
	pub(crate) local:          LocalFacts,
	pub(crate) composer:       Composer,
	pub(crate) overlays:       Overlays,
	pub(crate) turn_active:    bool,
	/// Presentation-clock start of the in-flight turn (the band timer).
	pub(crate) turn_started:   Option<Duration>,
	pub(crate) last_interrupt: Option<Instant>,
	/// Last `cl_clear` press, for pi's double-press exit window.
	pub(crate) last_clear:     Option<Instant>,
	/// Last Esc that reached the empty-composer rung, for the double-Esc
	/// selector window.
	pub(crate) last_escape:    Option<Instant>,
	/// Double-Left gesture state (pi `#detectLeftDoubleTap`).
	left_taps:                 LeftTaps,
	pub(crate) clock:          Instant,
	/// Launch facts painted in the welcome box's right column.
	pub(crate) welcome:        WelcomeFacts,
	/// Presentation-clock start of pi's 3000ms brand intro; `None` once a
	/// rebuilt welcome should rest.
	intro:                     Option<Duration>,
	/// The one console mailbox: bound commands post actions here.
	pub(crate) mailbox:        Arc<HostMailbox>,
	pub(crate) models:         Vec<ModelRow>,
	pub(crate) cycle:          Vec<(Str, Str)>,
	/// Last prompt sent as a turn, for `cl_retry`.
	pub(crate) last_prompt:    Option<Str>,
	/// Text the composer asked to copy; the terminal loop drains it into
	/// the clipboard (OSC 52 / native).
	pub(crate) clipboard:      Option<Str>,
	/// Live git facts for the band; `None` outside a checkout. The watch is
	/// a drop guard: it runs for the presenter's lifetime.
	#[expect(dead_code, reason = "drop guard keeping the git watcher task alive")]
	pub(crate) git_watch:      Option<GitWatch>,
	pub(crate) git_facts:      Option<Receiver<GitFacts>>,
	/// Clipboard read the host asked for; the terminal loop starts it.
	pub(crate) clipboard_read: Option<ClipboardRead>,
	/// Application data feeds for panels.
	pub(crate) services:       Arc<dyn Services>,
	/// Registered Esc hooks (rungs 1 and 4 of the ladder).
	pub(crate) escape_hooks:   Vec<EscapeHook>,
	/// Subagent whose session the view shows (pi `focusedAgentId`).
	pub(crate) focused_agent:  Option<Str>,
	/// This actor is a collaboration guest (pi `collabGuest`).
	pub(crate) collab_guest:   bool,
	/// Space-hold push-to-talk detector.
	pub(crate) space_hold:     SpaceHold,
	/// Whether push-to-talk is recording (space hold or `cl_stt_toggle`).
	pub(crate) stt_recording:  bool,
	/// Whether a live-voice session is on (pi `liveVoiceActive`).
	pub(crate) live_active:    bool,
	/// Presentation-clock instant the visible approval prompt appeared, for
	/// its countdown.
	approval_shown:            Option<Duration>,
	/// Last presented terminal height, for panel viewports.
	viewport_height:           u16,
	/// Observer-local transcript facts: tool start instants, the thinking
	/// speed gauge, and the reset banner.
	pub(crate) transcript:     crate::transcript::Local,
	/// Decides which desktop toasts a settled turn earns (pi
	/// `sendCompletionNotification` / `sendErrorNotification`).
	pub(crate) notifier:       crate::notify::Notifier,
	/// Toasts decided since the last terminal delivery.
	pub(crate) notifications:  Vec<omp_tui::Notification>,
	/// Periodic Codex quota refresh behind the reset fireworks.
	quota:                     crate::celebrate::QuotaWatch,
}

/// Observer-local band facts that never enter the DOM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFacts {
	/// Checked-out git branch.
	pub branch:           Option<Str>,
	/// The checkout has staged, unstaged, or untracked changes.
	pub dirty:            bool,
	/// Platform temp directory, for pi's scratch-project labeling.
	pub tmp:              Option<Str>,
	/// Live reasoning level when the model can reason.
	pub thinking:         Option<Str>,
	/// Live model route override (`ai_model`) when set.
	pub model:            Option<Str>,
	/// Auto-compaction threshold as a whole percent (`ai_compact_threshold`).
	pub compact:          u8,
	/// Fast mode is on (`ai_fastmode`).
	pub fast:             bool,
	/// Thinking level rides the model icon (`cl_status_compact_thinking`).
	pub compact_thinking: bool,
}

impl Default for LocalFacts {
	fn default() -> Self {
		Self {
			branch:           None,
			dirty:            false,
			tmp:              None,
			thinking:         None,
			model:            None,
			compact:          80,
			fast:             false,
			compact_thinking: true,
		}
	}
}

impl LocalFacts {
	/// Facts fixed at launch: the git watch's launch probe and the platform
	/// temp directory.
	fn at_launch(git: Option<&GitFacts>) -> Self {
		let tmp = env::temp_dir();
		let tmp = tmp.to_str().map(|tmp| Str::new(tmp.trim_end_matches('/')));
		let mut facts = Self { tmp, ..Self::default() };
		if let Some(git) = git {
			facts.set_git(git);
		}
		facts
	}

	/// Applies one git watch delivery.
	fn set_git(&mut self, git: &GitFacts) {
		self.branch.clone_from(&git.branch);
		self.dirty = git.dirty;
	}

	/// Refreshes the convar-backed facts (`ai_thinking`, `ai_model`,
	/// `ai_compact_threshold`, `ai_fastmode`, `cl_status_compact_thinking`).
	fn sync_con(&mut self, con: &Ctx, badge: &ModelBadge) {
		self.thinking = badge.reasoning.then(|| AI_THINKING.get(con));
		self.model = Some(AI_MODEL.get(con)).filter(|model| !model.is_empty());
		self.compact = (AI_COMPACT_THRESHOLD.get(con) * 100.0)
			.round()
			.clamp(0.0, 100.0) as u8;
		self.fast = AI_FASTMODE.get(con);
		self.compact_thinking = CL_STATUS_COMPACT_THINKING.get(con);
	}
}

/// What one routed input asked the host to do next. Ordered by strength so
/// several actions from one console line fold to the strongest.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Routed {
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

/// pi `handleCtrlC` during shutdown: once the chat has quit and the tty is
/// restored, a further Ctrl+C (SIGINT) exits the process with 130 at once
/// instead of waiting for a hanging teardown (a wedged tool, a slow
/// `process_exit`). Defense in depth: the controller's own teardown still
/// runs first when it is quick.
fn arm_hard_abort() {
	tokio::spawn(async {
		if tokio::signal::ctrl_c().await.is_ok() {
			std::process::exit(HARD_ABORT_CODE);
		}
	});
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
		let (git_watch, git_facts) = GitWatch::start(&options.project).unzip();
		let mut local = LocalFacts::at_launch(git_watch.as_ref().map(GitWatch::launch));
		local.sync_con(&options.con, &options.model);
		let facts = status_facts(&replica, &options.model, &local, None);
		let composer = Composer::new(
			width,
			options.ui.clone(),
			facts,
			slash::roster(&options.con),
			project_root(&replica).as_deref(),
		);
		// A resumed or already-running session starts active (pi derives
		// `isStreaming` from the session, never from a local edge).
		let turn_active = has_active_turn(&replica);
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
			turn_active,
			turn_started: None,
			last_interrupt: None,
			last_clear: None,
			last_escape: None,
			left_taps: LeftTaps::default(),
			clock: Instant::now(),
			intro: Some(Duration::ZERO),
			mailbox,
			models: options.models,
			cycle: options.cycle,
			last_prompt: None,
			clipboard: None,
			git_watch,
			git_facts,
			welcome: options.welcome,
			clipboard_read: None,
			services: options.services,
			escape_hooks: Vec::new(),
			focused_agent: None,
			collab_guest: false,
			space_hold: SpaceHold::default(),
			stt_recording: false,
			live_active: false,
			approval_shown: None,
			viewport_height: 24,
			transcript: crate::transcript::Local::default(),
			quota: crate::celebrate::QuotaWatch::default(),
			notifier: crate::notify::Notifier::new(None),
			notifications: Vec::new(),
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

	/// The welcome block. While pi's 3000ms brand intro runs the block stays
	/// mutable and unfinalized so it cannot retire into scrollback
	/// (ADR 0034); the block mounts with the intro time already elapsed, since
	/// a mounted block's clock starts at its epoch. Once the intro settles
	/// `finalized` flips, the projection remounts the block once, and that
	/// remount paints the resting frame.
	fn welcome(&self) -> RenderedBlock {
		let status = StatusLine::from_dom(&self.replica);
		let now = self.clock.elapsed();
		let intro = self
			.intro
			.map(|start| now.saturating_sub(start))
			.filter(|elapsed| !Intro::done(*elapsed));
		let welcome = Welcome::new(
			Str::new_static(env!("CARGO_PKG_VERSION")),
			&self.model,
			tip_for(status.session.as_str(), self.ui.charset),
			self.welcome.clone(),
			intro,
		);
		RenderedBlock {
			view:      BlockView {
				key:       0,
				kind:      BlockKind::Welcome,
				text:      Str::new_static("welcome"),
				mode:      Mode::Mutable,
				finalized: intro.is_none(),
			},
			component: Box::new(welcome),
			stream:    None,
		}
	}

	/// Observer-local projection switches for this paint.
	fn project_options(&self) -> crate::project::Options<'_> {
		crate::project::Options {
			show_thinking: self.show_thinking(),
			expanded:      crate::actions::CL_TOOLS_EXPANDED.get(&self.con),
			smooth:        crate::transcript::CL_SMOOTH_STREAMING.get(&self.con),
			prose_only:    crate::transcript::CL_THINKING_PROSE_ONLY.get(&self.con),
			local:         &self.transcript,
		}
	}

	fn blocks(&self) -> Vec<RenderedBlock> {
		let mut blocks = vec![self.welcome()];
		blocks.extend(project(&self.replica, &self.cards, &self.ui, &self.project_options()));
		blocks
	}

	fn apply_dom_event(&mut self, event: &Event) -> Result<(), HostError> {
		if let Event::Reset { snapshot } = event {
			let next = Dom::from_snapshot(snapshot);
			self.transcript.on_reset(&self.replica, &next);
		}
		self.replica.apply_event(event)?;
		self.transcript.observe(&self.replica, self.clock.elapsed());
		let was_active = self.turn_active;
		self.set_turn_active(has_active_turn(&self.replica));
		if was_active
			&& !self.turn_active
			&& let Some(end) = crate::notify::Notifier::turn_end_from_dom(&self.replica)
			&& let Some(toast) = self.notifier.turn_ended(&self.con, end)
		{
			self.notifications.push(toast);
		}
		let before = self.overlays.approval().map(|approval| approval.id.clone());
		self.overlays.sync_approval(&self.replica);
		let after = self.overlays.approval().map(|approval| approval.id.clone());
		if before != after {
			self.approval_shown = after.is_some().then(|| self.clock.elapsed());
		}
		Ok(())
	}

	/// Facts a panel reads while opening or running a call.
	fn panel_cx(&self, viewport: Size) -> PanelCx<'_> {
		PanelCx {
			dom:      &self.replica,
			con:      &self.con,
			ui:       &self.ui,
			viewport,
			services: &self.services,
		}
	}

	/// Whether a Director of `family` is active on the live chain (frames
	/// nest under `<meta><directors>`, so the scan is recursive).
	fn director_engaged(&self, family: &str) -> bool {
		let dom = &self.replica;
		let Some(root) = dom.children(dom.meta()).iter().copied().find(|handle| {
			dom.get(*handle)
				.is_some_and(|node| node.tag == Tag::Known(KnownTag::Directors))
		}) else {
			return false;
		};
		let family_key = omp_dom::PropKey::Custom(Str::new_static("family"));
		let status = omp_dom::PropKey::Custom(Str::new_static("status"));
		let mut pending = dom.children(root).to_vec();
		while let Some(handle) = pending.pop() {
			let Some(node) = dom.get(handle) else {
				continue;
			};
			if node.tag == Tag::Known(KnownTag::Director)
				&& node.prop(&family_key).and_then(Value::as_str) == Some(family)
				&& node.prop(&status).and_then(Value::as_str) == Some("active")
			{
				return true;
			}
			pending.extend(node.kids.iter().copied());
		}
		false
	}

	/// Pending queued prompts under `<queues><prompts>` (`/queue`), oldest
	/// first, as `(id, text)`.
	fn queued_prompts(&self) -> Vec<(Str, Str)> {
		let dom = &self.replica;
		let kind = PropId::Kind.into();
		let status = PropId::Status.into();
		let id = PropId::Id.into();
		dom.children(dom.queues())
			.iter()
			.filter_map(|handle| dom.get(*handle))
			.find(|node| node.tag == Tag::Known(KnownTag::Prompts))
			.into_iter()
			.flat_map(|prompts| prompts.kids.iter())
			.filter_map(|handle| dom.get(*handle))
			.filter(|node| {
				node.tag == Tag::Known(KnownTag::Prompt)
					&& node.prop(&kind).and_then(Value::as_str) == Some("queued")
					&& node.prop(&status).and_then(Value::as_str) == Some("pending")
			})
			.filter_map(|node| {
				let id = node.prop(&id).and_then(Value::as_str)?;
				Some((Str::new(id), node.content.clone().unwrap_or_default()))
			})
			.collect()
	}

	fn sync_status(&mut self) -> bool {
		self.local.sync_con(&self.con, &self.model);
		let facts = status_facts(&self.replica, &self.model, &self.local, self.turn_started);
		// The composer wears the rail while the plan Director is engaged, and
		// the band sits flush under the notice row painted directly above it
		// (pi `EditorTopGap` / `statusRowOccupied`).
		let reshaped = self.composer.set_plan_mode(self.plan_engaged());
		let gapped = self
			.composer
			.set_status_row_occupied(self.overlays.notice().is_some());
		let ime = self
			.composer
			.set_ime_safe_cursor(CL_IME_SAFE_CURSOR.get(&self.con));
		self.composer.set_status(facts) || reshaped || gapped || ime
	}

	/// Routes one decoded key: the topmost focused overlay consumes it
	/// first (pickers, approval hotkeys, command panels), then the console
	/// bind table (pi checks app actions before editor keys, except Esc
	/// while the autocomplete popup is open), then the space-hold gesture
	/// and the double-Left gesture, then the composer.
	fn route_key(&mut self, key: Key) -> Result<Routed, HostError> {
		// pi's status row disappears on the next input.
		let had_notice = self.overlays.notice().is_some();
		self.overlays.clear_notice();
		if self.overlays.modal() {
			let event = match self.overlays.active_mut() {
				Some(Overlay::Models(picker)) => Some(picker.key(key)),
				Some(Overlay::History(picker)) => Some(picker.key(key)),
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
					None
				},
				Some(Overlay::Panel(panel)) => {
					let event = match PanelAction::from_key(key) {
						Some(action) => match panel.action(action) {
							PanelEvent::Ignored => panel.key(key),
							event => event,
						},
						None => panel.key(key),
					};
					let event = match (event, key) {
						// A panel that ignores Esc still closes on it.
						(PanelEvent::Ignored, Key::Esc) => PanelEvent::Close,
						(event, _) => event,
					};
					return self.apply_panel_event(event);
				},
				None => None,
			};
			if let Some(event) = event {
				return self.apply_picker_event(event);
			}
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
		if let Some(routed) = self.gesture(key)? {
			return Ok(routed);
		}
		let routed = self.composer_key(key)?;
		Ok(if had_notice && routed == Routed::Ignored {
			Routed::Repaint
		} else {
			routed
		})
	}

	/// Composer-bound gestures that run before the editor sees the key:
	/// the space-hold push-to-talk cadence and the double-Left subagent
	/// unfocus. `None` hands the key on.
	fn gesture(&mut self, key: Key) -> Result<Option<Routed>, HostError> {
		let now = self.clock.elapsed();
		let enabled = CL_STT_HOLD.get(&self.con) && !self.composer.popup_open();
		match self.space_hold.observe(key, now, enabled) {
			SpaceHoldEvent::Pass => {},
			SpaceHoldEvent::Swallow => return Ok(Some(Routed::Ignored)),
			SpaceHoldEvent::Begin { track_back } => {
				self.composer.delete_before_caret(track_back);
				return Ok(Some(self.set_recording(true)));
			},
			SpaceHoldEvent::EndThenPass => {
				self.set_recording(false);
			},
		}
		if key == Key::Left
			&& self.focused_agent.is_some()
			&& self.composer.text().is_empty()
			&& self.left_taps.tap(Instant::now())
		{
			return Ok(Some(self.act(HostAction::FocusAgent(None))?));
		}
		Ok(None)
	}

	/// Starts or stops push-to-talk; the app owns the microphone.
	fn set_recording(&mut self, active: bool) -> Routed {
		if self.stt_recording == active {
			return Routed::Ignored;
		}
		self.stt_recording = active;
		if !active {
			self.space_hold.end();
		}
		let _ = self.commands.send(HostCommand::PushToTalk { active });
		self.notice(if active {
			"Listening… release space to transcribe"
		} else {
			"Transcribing…"
		})
	}

	fn composer_key(&mut self, key: Key) -> Result<Routed, HostError> {
		Ok(match self.composer.key(key) {
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
		})
	}

	/// pi `onEscape`: the eleven-rung ladder, top to bottom. Each rung that
	/// applies consumes the key.
	fn escape(&mut self) -> Result<Routed, HostError> {
		// 1. `/mcp test` (and any one-shot cancel hook): fire all, forget all.
		let cancels = self
			.escape_hooks
			.iter()
			.filter(|hook| hook.rung == EscapeRung::Cancel)
			.cloned()
			.collect::<Vec<_>>();
		if !cancels.is_empty() {
			self.escape_hooks.retain(|hook| hook.rung != EscapeRung::Cancel);
			for hook in cancels {
				let _ = hook.fire();
			}
			return Ok(Routed::Repaint);
		}
		// 2. Side-channel panels (`/btw`, `/omfg`, `/cleanse`) are the topmost
		// view; a focused overlay never reaches here.
		if self.overlays.side_panel() {
			self.close_overlay();
			return Ok(Routed::Repaint);
		}
		// 3. Maintenance (compaction, handoff, retry backoff) runs inside the
		// kernel turn: interrupting the turn cancels it (main view only).
		if self.focused_agent.is_none() && self.maintenance_active() {
			return Ok(self.interrupt_turn());
		}
		// 4. Silence the vocalizer before touching the turn.
		let silenced = self
			.escape_hooks
			.iter()
			.filter(|hook| hook.rung == EscapeRung::Silence)
			.any(EscapeHook::fire);
		if silenced {
			self.last_escape = None;
			return Ok(Routed::Repaint);
		}
		// 5. Loop mode: abort the streaming iteration, else pause the loop.
		if self.director_engaged(LOOP_DIRECTOR) {
			if self.turn_active {
				return Ok(self.interrupt_turn());
			}
			return self.run_console("pause");
		}
		// 6. Subagent view: clear typed text, else return to main; never
		// interrupts the subagent's turn.
		if self.focused_agent.is_some() {
			if !self.composer.text().trim().is_empty() {
				self.composer.clear();
				return Ok(Routed::Repaint);
			}
			return self.act(HostAction::FocusAgent(None));
		}
		// 7. Collaboration guest: ask the host to interrupt its agent.
		if self.collab_guest {
			if self.turn_active {
				let _ = self.commands.send(HostCommand::Interrupt);
			}
			return Ok(Routed::Ignored);
		}
		// 8. Bash / eval prefix mode: clear the draft and leave the mode.
		if self.composer.prefix_mode().is_some() {
			self.composer.clear();
			return Ok(Routed::Repaint);
		}
		// 9. Streaming turn: abort, restoring queued messages to the editor.
		if self.turn_active {
			let restored = self.restore_queued(true);
			let routed = self.interrupt_turn();
			return Ok(routed.max(restored));
		}
		// 10. Esc must not destroy an in-progress draft.
		if !self.composer.text().trim().is_empty() {
			self.last_escape = None;
			return Ok(Routed::Ignored);
		}
		// 11. Double Esc on an empty composer opens the configured selector.
		let action = CL_DOUBLE_ESCAPE.get(&self.con);
		let selector = match action.as_str() {
			"tree" => Some(Selector::Tree),
			"none" | "off" => None,
			_ => Some(Selector::Rewind),
		};
		let Some(selector) = selector else {
			return Ok(Routed::Ignored);
		};
		let now = Instant::now();
		let doubled = self
			.last_escape
			.is_some_and(|prior| now.duration_since(prior) < DOUBLE_ESCAPE_WINDOW);
		if doubled {
			self.last_escape = None;
			return self.run_console(match selector {
				Selector::Tree => "tree",
				Selector::Rewind => "branch",
			});
		}
		self.last_escape = Some(now);
		Ok(Routed::Ignored)
	}

	/// Whether main-session maintenance (compaction, handoff) is in flight:
	/// the compaction Director is active while the kernel summarizes.
	fn maintenance_active(&self) -> bool {
		self.turn_active && self.director_engaged("compaction")
	}

	fn interrupt_turn(&mut self) -> Routed {
		self.last_interrupt = Some(Instant::now());
		let _ = self.commands.send(HostCommand::Interrupt);
		Routed::Ignored
	}

	/// pi `restoreQueuedMessagesToEditor`: pulls queued prompts (and, while
	/// a turn runs, undelivered steering) back into the composer ahead of
	/// the current draft. `abort` is the Esc path; a plain dequeue keeps the
	/// stream running.
	fn restore_queued(&mut self, abort: bool) -> Routed {
		let mut texts = Vec::new();
		if self.turn_active {
			let (tx, rx) = flume::bounded(1);
			if self.up.send(Up::Unqueue(tx)).is_ok()
				&& let Ok(steering) = rx.recv_timeout(Duration::from_millis(200))
			{
				texts.extend(steering);
			}
		}
		let queued = self.queued_prompts();
		if !queued.is_empty() {
			let _ = self.commands.send(HostCommand::Dequeue {
				prompts: queued.iter().map(|(id, _)| id.clone()).collect(),
			});
			texts.extend(queued.into_iter().map(|(_, text)| text));
		}
		if texts.is_empty() {
			return if abort {
				Routed::Ignored
			} else {
				self.notice("No queued messages to restore")
			};
		}
		let restored = texts.len();
		let current = self.composer.text();
		let mut combined = String::new();
		for text in texts.iter().chain(std::iter::once(&Str::new(current.as_str()))) {
			if text.trim().is_empty() {
				continue;
			}
			if !combined.is_empty() {
				combined.push_str("\n\n");
			}
			combined.push_str(text);
		}
		self.composer.set_text(&combined);
		if abort {
			Routed::Repaint
		} else {
			self.notice(format!(
				"Restored {restored} queued message{} to editor",
				if restored > 1 { "s" } else { "" }
			))
		}
	}

	/// Executes one console line and applies every action it posted.
	///
	/// Console failures become a notice rather than ending the host: a
	/// mistyped `bind` in `config.cfg` must not kill the chat.
	pub(crate) fn run_console(&mut self, command: &str) -> Result<Routed, HostError> {
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
			self.overlays.notify(error.to_string());
			routed = routed.max(Routed::Repaint);
		}
		Ok(routed)
	}

	/// Applies a finished clipboard read: an image persists to a temp file
	/// and stages as a chip through the drop path, text pastes sanitized or
	/// verbatim (`raw`, the Ctrl+Shift+V contract).
	fn deliver_clipboard(&mut self, clipboard: Option<Clipboard>, raw: bool) -> Routed {
		match clipboard {
			None => self.notice("Clipboard is empty"),
			Some(Clipboard::Text(text)) => {
				if raw {
					self.composer.paste_raw(&text);
				} else {
					self.composer.paste(&text);
				}
				Routed::Repaint
			},
			Some(Clipboard::Image(image)) => match image.persist() {
				Ok(path) => {
					self.composer.paste(&path.to_string_lossy());
					Routed::Repaint
				},
				Err(error) => self.notice(format!("Could not stage the pasted image: {error}")),
			},
			Some(Clipboard::Paths(paths)) => {
				let mut joined = String::new();
				for path in &paths {
					if !joined.is_empty() {
						joined.push(' ');
					}
					joined.push('"');
					joined.push_str(path);
					joined.push('"');
				}
				self.composer.paste(&joined);
				Routed::Repaint
			},
		}
	}

	/// Drains actions posted outside a console line (app-side results such
	/// as a speech transcript).
	fn drain_mailbox(&mut self) -> Result<Routed, HostError> {
		let mut routed = Routed::Ignored;
		let actions = self.mailbox.drain().collect::<Vec<_>>();
		for action in actions {
			routed = routed.max(self.act(action)?);
		}
		Ok(routed)
	}

	/// Convars whose change forces a transcript rebuild.
	fn projection_inputs(&self) -> (bool, bool, bool, bool, bool) {
		(
			self.show_thinking(),
			crate::actions::CL_SHOWTOOLS.get(&self.con),
			crate::actions::CL_TOOLS_EXPANDED.get(&self.con),
			crate::transcript::CL_SMOOTH_STREAMING.get(&self.con),
			crate::transcript::CL_THINKING_PROSE_ONLY.get(&self.con),
		)
	}

	pub(crate) fn notice(&mut self, text: impl Into<Str>) -> Routed {
		self.overlays.notify(text);
		Routed::Repaint
	}

	/// Sends the draft as a submission. The controller — which knows whether
	/// a turn is really running — starts a turn or steers the active one;
	/// the replica's view may lag the kernel, so the host never decides.
	pub(crate) fn submit(&mut self, text: Str) -> Routed {
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
	pub(crate) fn act(&mut self, action: HostAction) -> Result<Routed, HostError> {
		Ok(match action {
			HostAction::Interrupt => {
				if self.overlays.modal() {
					self.close_overlay();
					Routed::Repaint
				} else {
					return self.escape();
				}
			},
			HostAction::Clear => {
				let now = Instant::now();
				let repeated = self
					.last_clear
					.is_some_and(|prior| now.duration_since(prior) <= Duration::from_millis(500));
				self.last_clear = Some(now);
				if self.stt_recording {
					self.set_recording(false);
					return Ok(Routed::Repaint);
				}
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
				let _ = self
					.commands
					.send(HostCommand::Overlay { id: Str::new_static("models"), open: true });
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
				let picker = HistoryPicker::open(prompts, self.composer.frame().size().width, &self.ui);
				self.overlays.show(Overlay::History(picker));
				Routed::Repaint
			},
			HostAction::ExternalEditor => Routed::ExternalEditor,
			HostAction::Dequeue => self.restore_queued(false),
			HostAction::PasteImage => {
				self.clipboard_read = Some(ClipboardRead::Smart);
				Routed::Ignored
			},
			HostAction::PasteRaw => {
				self.clipboard_read = Some(ClipboardRead::Text);
				Routed::Ignored
			},
			HostAction::CopyLine => {
				let line = self.composer.current_line();
				if line.is_empty() {
					return Ok(self.notice("Nothing to copy on this line"));
				}
				self.clipboard = Some(line);
				self.notice("Copied line")
			},
			HostAction::CopyPrompt => {
				let text = self.composer.text();
				if text.is_empty() {
					return Ok(self.notice("Nothing to copy"));
				}
				self.clipboard = Some(Str::new(text));
				self.notice("Copied prompt")
			},
			HostAction::FocusAgent(agent) => {
				let changed = self.focused_agent != agent;
				let _ = self.commands.send(HostCommand::Overlay {
					id:   Str::new(format!(
						"agent:{}",
						agent
							.as_deref()
							.or(self.focused_agent.as_deref())
							.unwrap_or_default()
					)),
					open: agent.is_some(),
				});
				self.focused_agent = agent;
				self.left_taps = LeftTaps::default();
				self.last_escape = None;
				if !changed {
					return Ok(Routed::Ignored);
				}
				match self.focused_agent.as_deref() {
					Some(id) => self.notice(format!("Viewing subagent {id} · Esc returns to main")),
					None => self.notice("Back to main session"),
				}
			},
			HostAction::CollabGuest(guest) => {
				self.collab_guest = guest;
				Routed::Ignored
			},
			HostAction::SttToggle => {
				let active = !self.stt_recording;
				self.set_recording(active)
			},
			HostAction::PushToTalk { active } => self.set_recording(active),
			HostAction::LiveToggle => {
				if self.stt_recording {
					self.set_recording(false);
				}
				self.live_active = !self.live_active;
				let active = self.live_active;
				let _ = self.commands.send(HostCommand::LiveVoice { active });
				self.notice(if active {
					"Live voice on · Ctrl+L to stop"
				} else {
					"Live voice off"
				})
			},
			HostAction::InsertText(text) => {
				if self.stt_recording {
					// Recognized while a toggle recording continues.
					self.composer.paste(text.as_str());
				} else {
					self.composer.paste(text.as_str());
					self.overlays.clear_notice();
				}
				Routed::Repaint
			},
			HostAction::EscapeHook(hook) => {
				self.escape_hooks.retain(|prior| prior.id != hook.id);
				self.escape_hooks.push(hook);
				Routed::Ignored
			},
			HostAction::DropEscapeHook(id) => {
				self.escape_hooks.retain(|prior| prior.id != id);
				Routed::Ignored
			},
			HostAction::Open(opener) => {
				let viewport = self.viewport();
				let opened = opener.open(&self.panel_cx(viewport));
				match opened {
					Ok(panel) => {
						let _ = self.commands.send(HostCommand::Overlay {
							id:   Str::new_static(panel.id()),
							open: true,
						});
						self.overlays.show(Overlay::Panel(panel));
						Routed::Repaint
					},
					Err(error) => self.notice(error),
				}
			},
			HostAction::Call(call) => {
				let viewport = self.viewport();
				let event = call.call(&self.panel_cx(viewport));
				self.apply_panel_event(event)?
			},
			HostAction::Command(action) => self.run_command(action)?,
			HostAction::Reply { severity, text } => match severity {
				omp_con::Severity::Info if text.is_empty() => Routed::Ignored,
				_ => self.notice(text),
			},
		})
	}

	/// The terminal viewport panels size against: the composer width and
	/// the last presented height.
	fn viewport(&self) -> Size {
		Size::new(self.composer.frame().size().width, self.viewport_height)
	}

	/// Applies what a panel (or a command-owned call) asked for.
	fn apply_panel_event(&mut self, event: PanelEvent) -> Result<Routed, HostError> {
		Ok(match event {
			PanelEvent::Ignored => Routed::Ignored,
			PanelEvent::Consumed => Routed::Repaint,
			PanelEvent::Close => {
				self.close_overlay();
				Routed::Repaint
			},
			PanelEvent::Run(line) => self.run_console(line.as_str())?.max(Routed::Repaint),
			PanelEvent::Finish(line) => {
				self.close_overlay();
				self.run_console(line.as_str())?.max(Routed::Repaint)
			},
			PanelEvent::Recall(text) => {
				self.close_overlay();
				self.composer.set_text(text.as_str());
				Routed::Repaint
			},
			PanelEvent::Notice(text) => self.notice(text),
			PanelEvent::Copy(text) => {
				self.clipboard = Some(text);
				Routed::Repaint
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

	pub(crate) fn close_overlay(&mut self) {
		if let Some(overlay) = self.overlays.dismiss()
			&& matches!(overlay, Overlay::Models(_) | Overlay::Panel(_))
		{
			let _ = self
				.commands
				.send(HostCommand::Overlay { id: Str::new_static(overlay.id()), open: false });
		}
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
		let at = self.cycle.iter().position(|(_, model)| *model == live);
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
	pub(crate) fn plan_engaged(&self) -> bool {
		self.director_engaged(PLAN_DIRECTOR)
	}

	/// Whether the newest turn closed with an error notice.
	fn last_turn_failed(&self) -> bool {
		let dom = &self.replica;
		let Some(turn) = dom.children(dom.body()).last() else {
			return false;
		};
		dom.children(*turn)
			.iter()
			.rev()
			.filter_map(|handle| dom.get(*handle))
			.find(|node| node.tag == Tag::Known(KnownTag::Notice))
			.is_some_and(|node| {
				node.prop(&PropId::Kind.into()).and_then(Value::as_str) == Some("error")
			})
	}

	fn approval_frame(&self, width: u16) -> Option<Frame> {
		let approval = self.overlays.approval()?;
		let countdown = approval.timeout.and_then(|timeout| {
			let shown = self.approval_shown?;
			let countdown = Countdown::new("auto-decides in", shown, timeout);
			Some(countdown.remaining(self.clock.elapsed()))
		});
		Some(approval_frame(approval, countdown, width, &self.ui))
	}

	/// Next whole-second wake while an approval countdown is showing.
	fn approval_wake(&self) -> Option<Duration> {
		let approval = self.overlays.approval()?;
		let timeout = approval.timeout?;
		let shown = self.approval_shown?;
		let now = self.clock.elapsed();
		if now.saturating_sub(shown) >= timeout {
			return None;
		}
		let elapsed = now.saturating_sub(shown);
		Some(shown + Duration::from_secs(elapsed.as_secs() + 1))
	}

	/// Frame and anchor of the topmost focused overlay (picker or panel),
	/// when one is open.
	fn overlay_frame(&mut self, size: Size) -> Option<(Frame, PanelAnchor)> {
		let center = Size::new(size.width * 4 / 5, size.height.saturating_sub(2));
		match self.overlays.active_mut() {
			Some(Overlay::Models(picker)) => Some((picker.frame(size).clone(), PanelAnchor::Bottom)),
			Some(Overlay::History(picker)) => {
				Some((picker.frame(size).clone(), PanelAnchor::Bottom))
			},
			Some(Overlay::Panel(panel)) => {
				let anchor = panel.anchor();
				let viewport = match anchor {
					PanelAnchor::Center => center,
					PanelAnchor::Bottom | PanelAnchor::Full | PanelAnchor::Side => size,
				};
				Some((panel.frame(viewport).clone(), anchor))
			},
			Some(Overlay::Approval(_)) | None => None,
		}
	}

	/// Advances the topmost panel's animations, closing a panel that
	/// reports itself finished.
	fn tick_overlay(&mut self, now: Duration) -> Result<bool, HostError> {
		let (changed, finished, settled) = match self.overlays.active_mut() {
			Some(Overlay::Panel(panel)) => {
				let changed = panel.tick(now);
				let settled = if changed { panel.settled() } else { None };
				(changed, panel.finished(), settled)
			},
			_ => (false, false, None),
		};
		if finished {
			self.close_overlay();
			return Ok(true);
		}
		if let Some(event) = settled {
			self.apply_panel_event(event)?;
			return Ok(true);
		}
		Ok(changed)
	}

	/// Earliest wake among the overlay stack, the approval countdown, the
	/// space-hold release timer, and the quota refresh.
	fn next_wake(&self) -> Option<Duration> {
		let panel = match self.overlays.active() {
			Some(Overlay::Panel(panel)) => panel.next_wake(),
			_ => None,
		};
		let quota = crate::celebrate::CL_CODEX_FIREWORKS
			.get(&self.con)
			.then(|| {
				self
					.quota
					.next_wake(self.clock.elapsed(), self.model.provider.as_str())
			})
			.flatten();
		[panel, self.approval_wake(), self.space_hold.next_wake(), quota]
			.into_iter()
			.flatten()
			.min()
	}

	/// Polls the Codex quota watch and opens the reset fireworks when
	/// consecutive reports show an unscheduled weekly reset (pi
	/// `#applyUsageRefreshReports` → `showCodexResetFireworks`).
	fn tick_quota(&mut self, now: Duration) -> Result<bool, HostError> {
		if !crate::celebrate::CL_CODEX_FIREWORKS.get(&self.con) {
			return Ok(false);
		}
		let Some(event) = self
			.quota
			.poll(self.services.as_ref(), self.model.provider.as_str(), now)
		else {
			return Ok(false);
		};
		let showing = matches!(
			self.overlays.active(),
			Some(Overlay::Panel(panel)) if panel.id() == "fireworks"
		);
		if showing {
			return Ok(false);
		}
		let opener = crate::overlays::PanelOpener::new(move |cx| {
			Ok(Box::new(crate::overlays::fireworks::Fireworks::open(event, cx))
				as Box<dyn crate::overlays::Panel>)
		});
		self.act(HostAction::Open(opener))?;
		Ok(true)
	}

	/// Ends a push-to-talk hold whose release gap elapsed.
	fn tick_space_hold(&mut self, now: Duration) -> bool {
		if self.space_hold.release_due(now) {
			self.set_recording(false);
			return true;
		}
		false
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
			self.presenter.composer.resize(size.width, size.height);
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
				Pause::Quit => {
					arm_hard_abort();
					return Ok(());
				},
				Pause::Suspend => suspend_process(),
				Pause::ExternalEditor => {
					// pi `handleExternalEditor`: chips expand to their pasted text
					// before the draft reaches `$EDITOR`; the result lands verbatim.
					let draft = self.presenter.composer.text();
					match crate::editor::edit_draft_detached(
						&draft,
						crate::editor::EditorOptions::default(),
					) {
						Ok(Some(edited)) => self.presenter.composer.replace_edited(&edited),
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
		// One background clipboard read at a time (Ctrl+V / Ctrl+Shift+V);
		// a stale result is dropped with its receiver.
		let mut clipboard: Option<(oneshot::Receiver<Option<Clipboard>>, bool, Instant)> = None;
		let mailbox = Arc::clone(&self.presenter.mailbox);
		loop {
			let deadline = self.next_deadline();
			if let Some(scope) = self.presenter.clipboard_read.take() {
				clipboard = Some((spawn_clipboard_read(scope), scope == ClipboardRead::Text, Instant::now()));
			}
			let clipboard_pending = clipboard.is_some();
			tokio::select! {
				biased;
				terminal_event = terminal.next() => {
					match terminal_event? {
						TerminalEvent::Resize => {
							if let Some(next) = terminal.take_resize()? {
								size = next;
								self.presenter.composer.resize(next.width, next.height);
								self.projection_mut().resize(next);
								self.present(renderer, size)?;
							}
						},
						TerminalEvent::Input(event) => {
							if terminal.handle_input_event(&event, renderer)? {
								// A completed OSC 5522 offer carries an image or text
								// paste out of band (pi enhanced paste).
								if let Some(pasted) = terminal.take_paste() {
									let clipboard = match pasted {
										omp_tui::Pasted::Text(text) => Clipboard::Text(text.to_string()),
										omp_tui::Pasted::Image(image) => Clipboard::Image(image),
									};
									let routed = self.presenter.deliver_clipboard(Some(clipboard), false);
									if let Some(pause) = self.apply_routed(routed, renderer, size)? {
										return Ok(pause);
									}
								}
								continue;
							}
							let routed = self.input(event)?;
							if let Some(text) = self.presenter.clipboard.take() {
								terminal.copy_to_clipboard(&text)?;
							}
							match self.apply_routed(routed, renderer, size)? {
								Some(pause) => return Ok(pause),
								None => {},
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
				action = mailbox.next() => {
					if let Some(action) = action {
						let routed = self.presenter.act(action)?.max(self.presenter.drain_mailbox()?);
						if let Some(text) = self.presenter.clipboard.take() {
							terminal.copy_to_clipboard(&text)?;
						}
						if let Some(pause) = self.apply_routed(routed, renderer, size)? {
							return Ok(pause);
						}
					}
				},
				read = async {
					match clipboard.as_mut() {
						Some((rx, _, started)) => {
							let elapsed = started.elapsed();
							match tokio::time::timeout(CLIPBOARD_READ_TIMEOUT.saturating_sub(elapsed), rx).await {
								Ok(Ok(content)) => content,
								Ok(Err(_)) | Err(_) => None,
							}
						},
						None => future::pending().await,
					}
				}, if clipboard_pending => {
					let raw = clipboard.take().is_some_and(|(_, raw, _)| raw);
					let routed = self.presenter.deliver_clipboard(read, raw);
					if let Some(pause) = self.apply_routed(routed, renderer, size)? {
						return Ok(pause);
					}
				},
				dom_event = self.presenter.dom_events.recv_async() => {
					let Ok(event) = dom_event else { break };
					let reset = matches!(event, Event::Reset { .. });
					self.presenter.apply_dom_event(&event)?;
					if reset {
						self.reset_projection(size);
					} else {
						self.reconcile_projection(size);
					}
					self.present(renderer, size)?;
					// Toasts the settled turn earned go out after its paint (OSC
					// 99/9/777, then BEL by capability).
					for toast in self.presenter.notifications.drain(..) {
						if let Err(error) = terminal.notify(&toast) {
							tracing::debug!(%error, "notification delivery failed");
						}
					}
				},
				kernel_event = self.presenter.kernel_events.recv_async() => {
					let Ok(event) = kernel_event else {
						break;
					};
					let now = self.presenter.clock.elapsed();
					if self.presenter.transcript.on_kernel_event(&event, now) {
						self.reconcile_projection(size);
						self.present(renderer, size)?;
					}
				},
				git = recv_git(self.presenter.git_facts.as_ref()) => {
					self.presenter.local.set_git(&git);
					if self.presenter.sync_status() {
						self.present(renderer, size)?;
					}
				},
			}
		}
		let _ = self.presenter.up.send(Up::Cancel);
		let _ = self.presenter.commands.send(HostCommand::Quit);
		Ok(Pause::Quit)
	}

	/// Applies a routing outcome to the terminal; `Some` releases the tty.
	fn apply_routed(
		&mut self,
		routed: Routed,
		renderer: &mut Renderer<TtyOut>,
		size: Size,
	) -> Result<Option<Pause>, HostError> {
		match routed {
			Routed::Quit => {
				let _ = self.presenter.up.send(Up::Cancel);
				let _ = self.presenter.commands.send(HostCommand::Quit);
				Ok(Some(Pause::Quit))
			},
			Routed::Suspend => Ok(Some(Pause::Suspend)),
			Routed::ExternalEditor => Ok(Some(Pause::ExternalEditor)),
			Routed::DisplayReset => Ok(Some(Pause::DisplayReset)),
			Routed::Ignored => Ok(None),
			Routed::Repaint => {
				self.present(renderer, size)?;
				Ok(None)
			},
			Routed::RebuildProjection => {
				self.rebuild_projection(size);
				self.present(renderer, size)?;
				Ok(None)
			},
		}
	}

	/// Earliest animation wake across the composer, mounted blocks, the
	/// overlay stack, and the gesture timers, in host-clock time.
	fn next_deadline(&self) -> Option<Duration> {
		let composer = self.presenter.composer.next_wake();
		let blocks = self
			.projection
			.as_ref()
			.and_then(|projection| projection.next_wake());
		[composer, blocks, self.presenter.next_wake()]
			.into_iter()
			.flatten()
			.min()
	}

	fn tick(&mut self) -> bool {
		let now = self.presenter.clock.elapsed();
		let composer = self.presenter.composer.tick(now);
		let blocks = self
			.projection
			.as_mut()
			.is_some_and(|projection| projection.tick(now));
		let overlay = self.presenter.tick_overlay(now).unwrap_or_else(|error| {
			self.presenter.notice(error.to_string());
			true
		});
		let countdown = self.presenter.approval_wake().is_some();
		let hold = self.presenter.tick_space_hold(now);
		let quota = self.presenter.tick_quota(now).unwrap_or_else(|error| {
			self.presenter.notice(error.to_string());
			true
		});
		composer || blocks || overlay || countdown || hold || quota
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
					"overlay": presenter.overlays.active().map(Overlay::id).unwrap_or_default(),
					"overlay_depth": presenter.overlays.depth(),
					"notice": presenter.overlays.notice().unwrap_or_default(),
					"model": presenter.live_model(),
					"thinking": AI_THINKING.get(&presenter.con),
					"turn_active": presenter.turn_active,
					"focused_agent": presenter.focused_agent.as_deref().unwrap_or_default(),
					"collab_guest": presenter.collab_guest,
					"recording": presenter.stt_recording,
					"prefix_mode": presenter.composer.prefix_mode().map(|mode| format!("{mode:?}").to_lowercase()).unwrap_or_default(),
					"escape_hooks": presenter.escape_hooks.iter().map(|hook| hook.id.as_str()).collect::<Vec<_>>(),
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
				// An empty bracketed paste is how some terminals announce an
				// image-only pasteboard (macOS Cmd+V): read the clipboard.
				if text.is_empty() {
					self.presenter.clipboard_read = Some(ClipboardRead::Smart);
					return Ok(Routed::Ignored);
				}
				if let Some(Overlay::Panel(panel)) = self.presenter.overlays.active_mut()
					&& panel.anchor() != PanelAnchor::Side
				{
					let event = panel.paste(text.as_str());
					if event != PanelEvent::Ignored {
						return self.presenter.apply_panel_event(event);
					}
				}
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

	/// A session reset (`/new`, `/drop`, rewind, resume): the live document is
	/// replaced in place while rows already in native scrollback stay put
	/// (ADR 0034; pi `resetTranscript` minus its `clearScrollback`).
	fn reset_projection(&mut self, size: Size) {
		let now = self.presenter.clock.elapsed();
		let blocks = self.presenter.blocks();
		let mirror = self.presenter.blocks();
		match self.projection.as_mut() {
			Some(projection) => projection.reset_in_place(blocks, mirror, now),
			None => self.rebuild_projection(size),
		}
	}

	fn present(&mut self, renderer: &mut Renderer<TtyOut>, size: Size) -> Result<(), HostError> {
		self.presenter.sync_status();
		self.presenter.viewport_height = size.height;
		let approval = self.presenter.approval_frame(size.width);
		let overlay = self.presenter.overlay_frame(size);
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
		// Pickers replace the composer band (pi swaps the editor slot);
		// dialogs center; dashboards cover the viewport; side panels sit
		// above the still-live composer.
		let (picker_options, picker_modal) = match overlay.as_ref().map(|(_, anchor)| *anchor) {
			Some(PanelAnchor::Center) => (
				OverlayOptions::default()
					.width(Dim::Pct(80))
					.anchor(OverlayAnchor::Center)
					.z(20),
				true,
			),
			Some(PanelAnchor::Full) => (
				OverlayOptions::default()
					.width(Dim::Cells(size.width))
					.anchor(OverlayAnchor::TopLeft)
					.z(20),
				true,
			),
			Some(PanelAnchor::Side) => (
				OverlayOptions::default()
					.width(Dim::Cells(size.width))
					.anchor(OverlayAnchor::BottomLeft)
					.margin(omp_tui::OverlayMargin { bottom: composer.height(), ..Default::default() })
					.non_modal()
					.z(20),
				false,
			),
			Some(PanelAnchor::Bottom) | None => (
				OverlayOptions::default()
					.width(Dim::Cells(size.width))
					.anchor(OverlayAnchor::BottomLeft)
					.z(20),
				true,
			),
		};
		let picker = overlay.map(|(frame, _)| frame);
		// pi's status row sits directly above the editor.
		let notice_options = OverlayOptions::default()
			.width(Dim::Cells(size.width))
			.anchor(OverlayAnchor::BottomLeft)
			.margin(omp_tui::OverlayMargin { bottom: composer.height(), ..Default::default() })
			.non_modal()
			.z(15);
		let modal = approval.is_some() || (picker.is_some() && picker_modal);
		let mut layers =
			vec![Layer { frame: &document, options: &document_options, active: !modal }];
		if let Some(frame) = notice.as_ref() {
			layers.push(Layer { frame, options: &notice_options, active: false });
		}
		if let Some(frame) = picker.as_ref() {
			layers.push(Layer {
				frame,
				options: &picker_options,
				active: approval.is_none() && picker_modal,
			});
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
		while let Some(git) = self
			.presenter
			.git_facts
			.as_ref()
			.and_then(|facts| facts.try_recv().ok())
		{
			self.presenter.local.set_git(&git);
			changed |= self.presenter.sync_status();
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
		self.presenter.composer.resize(size.width, size.height);
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
				// Chips expand into the draft; the edited text lands verbatim.
				let draft = self.presenter.composer.text();
				match crate::editor::edit_draft_detached(
					&draft,
					crate::editor::EditorOptions::default(),
				) {
					Ok(Some(edited)) => self.presenter.composer.replace_edited(&edited),
					Ok(None) => {},
					Err(error) => {
						self.presenter.notice(error.to_string());
					},
				}
				self.refresh();
				NativeEffect::Consumed
			},
			Routed::Repaint | Routed::RebuildProjection | Routed::Suspend | Routed::DisplayReset => {
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

	/// Frame of the open picker or panel, when one is showing.
	pub fn picker_frame(&mut self) -> Option<Frame> {
		self
			.presenter
			.overlay_frame(self.size)
			.map(|(frame, _)| frame)
	}

	/// Identity of the topmost overlay, when one is open.
	#[must_use]
	pub fn overlay_id(&self) -> Option<&'static str> {
		self.presenter.overlays.active().map(Overlay::id)
	}

	/// Number of stacked overlays.
	#[must_use]
	pub fn overlay_depth(&self) -> usize {
		self.presenter.overlays.depth()
	}

	/// Current unsent draft.
	#[must_use]
	pub fn composer_text(&self) -> String {
		self.presenter.composer.text()
	}

	/// Subagent the view is focused on, when any.
	#[must_use]
	pub fn focused_agent(&self) -> Option<&str> {
		self.presenter.focused_agent.as_deref()
	}

	/// Whether push-to-talk is recording.
	#[must_use]
	pub const fn recording(&self) -> bool {
		self.presenter.stt_recording
	}

	/// Ids of the registered Esc hooks.
	#[must_use]
	pub fn escape_hooks(&self) -> Vec<Str> {
		self
			.presenter
			.escape_hooks
			.iter()
			.map(|hook| hook.id.clone())
			.collect()
	}

	/// Text the last key asked to copy, if any (drained).
	pub fn take_clipboard(&mut self) -> Option<Str> {
		self.presenter.clipboard.take()
	}

	/// Clipboard read the last key asked for, if any (drained).
	pub fn take_clipboard_read(&mut self) -> Option<ClipboardRead> {
		self.presenter.clipboard_read.take()
	}

	/// Delivers a finished clipboard read exactly as the terminal loop would.
	pub fn deliver_clipboard(&mut self, clipboard: Option<Clipboard>, raw: bool) -> NativeEffect {
		let routed = self.presenter.deliver_clipboard(clipboard, raw);
		self.native_effect(routed)
	}

	/// Applies one host action directly (what a posted mailbox action does).
	pub fn act(&mut self, action: HostAction) -> Result<NativeEffect, HostError> {
		let routed = self.presenter.act(action)?;
		Ok(self.native_effect(routed))
	}

	/// Advances the presentation clock to `now` (gesture and countdown
	/// timers); returns whether anything repainted.
	pub fn tick(&mut self, now: Duration) -> bool {
		let changed = self.presenter.composer.tick(now)
			| self.presenter.tick_overlay(now).unwrap_or(true)
			| self.presenter.tick_space_hold(now);
		if changed {
			self.refresh();
		}
		changed
	}

	/// Presentation-clock epoch, for driving [`NativeHost::tick`].
	#[must_use]
	pub const fn clock(&self) -> Instant {
		self.presenter.clock
	}

	fn native_effect(&mut self, routed: Routed) -> NativeEffect {
		match routed {
			Routed::Quit => NativeEffect::Quit,
			Routed::Ignored => NativeEffect::Ignored,
			_ => {
				self.refresh();
				NativeEffect::Consumed
			},
		}
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

/// Next git watch delivery; pends forever outside a checkout or once the
/// watcher has stopped.
async fn recv_git(facts: Option<&Receiver<GitFacts>>) -> GitFacts {
	match facts {
		Some(facts) => match facts.recv_async().await {
			Ok(facts) => facts,
			Err(_) => future::pending().await,
		},
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
	let route = local.model.as_deref().unwrap_or(status.model.as_str());
	let model = if route.is_empty() || route == badge.identifier {
		badge.short_name()
	} else {
		ModelBadge::from_identifier(route).short_name()
	};
	let home = (!status.home.is_empty()).then_some(status.home.as_str());
	let path = display_path(status.session.as_str(), home, local.tmp.as_deref());
	// Not yet journaled anywhere the replica can see: the advisor roster,
	// compaction speculation, the credential's billing plan and account.
	StatusFacts {
		model,
		thinking: local.thinking.clone(),
		compact_thinking: local.compact_thinking,
		fast: local.fast,
		advisor: None,
		cwd: path.text,
		scratch: path.scratch,
		branch: local.branch.clone(),
		dirty: local.dirty,
		session_name: status.name,
		tokens: status.context,
		context_window: badge.context_window,
		compact_percent: local.compact,
		speculation: Speculation::None,
		tokens_in: status.tokens_in,
		tokens_out: status.tokens_out,
		cache_read: status.cache_read,
		cache_write: status.cache_write,
		tokens_per_second: status.tokens_per_second,
		cost_nano_usd: status.cost_nano_usd,
		subscription: false,
		premium_requests: 0,
		account: None,
		working,
	}
}

/// Whether the kernel is still working on the last turn, decided by the
/// newest lifecycle element in it. A notice closes the turn (the kernel ends
/// an interrupted or failed turn with one); an open assistant or a running
/// tool keeps it active; a settled tool defers to its assistant, whose
/// `tool_calls` stop means another inference follows; `<usage>` is
/// per-inference accounting and closes nothing; a turn with only the user's
/// message is awaiting its first inference.
fn has_active_turn(dom: &Dom) -> bool {
	let Some(turn) = dom.children(dom.body()).last() else {
		return false;
	};
	for child in dom.children(*turn).iter().rev() {
		let Some(node) = dom.get(*child) else {
			continue;
		};
		match node.tag {
			Tag::Known(KnownTag::Notice) => return false,
			Tag::Known(KnownTag::Assistant) => {
				return node
					.prop(&PropId::StopReason.into())
					.and_then(Value::as_str)
					.is_none_or(|reason| reason == "tool_calls");
			},
			Tag::Custom(_) => {
				let settled = node
					.prop(&PropId::Status.into())
					.and_then(Value::as_str)
					.is_some_and(|status| matches!(status, "ok" | "error" | "cancelled" | "aborted"));
				if !settled {
					return true;
				}
			},
			_ => {},
		}
	}
	true
}

fn approval_frame(
	approval: &crate::overlays::ApprovalOverlay,
	countdown: Option<u64>,
	width: u16,
	ui: &UiContext,
) -> Frame {
	let title = approval.title.clone();
	let reason = approval.reason.clone();
	let scope = Str::new(approval.scope.as_str());
	// pi `CountdownTimer`: the modal shows `(Ns remaining)` ticking once a
	// second until the kernel answers with the prompt's default.
	let remaining = countdown.map_or_else(Str::default, |seconds| {
		Str::new(format!("  ({seconds}s remaining)"))
	});
	let tree = omp_tui::dom! {
		<box border=round bc=warning pad="1 2">
			<col gap=1>
				<row>
					<text fg=warning attr=bold>{title}</text>
					<text fg=muted>{remaining}</text>
				</row>
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
			tip_seeded(welcome_seed(status.session.as_str()), ui.charset),
			WelcomeFacts::default(),
			None,
		)),
		stream:    None,
	};
	let cards = CardRegistry::standard();
	let transcript = crate::transcript::Local::default();
	let options = crate::project::Options::new(&transcript);
	let mut blocks = vec![welcome()];
	blocks.extend(project(&replica, &cards, ui, &options));
	let mut mirror = vec![welcome()];
	mirror.extend(project(&replica, &cards, ui, &options));
	let projection =
		Projection::new(size, ResizePolicy::Rebuild, ui, blocks, mirror, Duration::ZERO);
	projection.document(composer.frame(), size)
}
