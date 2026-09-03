//! Host actions: console commands the interactive actor executes locally.
//!
//! ADR 0014: keybindings are `bind <chord> "<command>"` lines over the one
//! command stream, never an action-id schema. Every pi app keybinding
//! therefore maps to a `cl_*` console command declared here. A bound key,
//! a `/`-prefixed composer line, and a cfg script all run the same words;
//! the command posts a [`HostAction`] into the actor's one console mailbox
//! ([`HostMailbox`]), which the actor drains after each `exec`.
//!
//! Commands only *ask*; presentation state stays observer-local (ADR 0005)
//! and never enters the session DOM.

use std::{fmt, sync::Arc};

use flume::{Receiver, Sender};
use omp_con::{ConResult, Ctx, CtxBuilder, Severity};
use omp_core::Str;

use crate::{
	commands::CommandAction,
	overlays::{PanelCall, PanelOpener},
};

/// Which rung of pi's Escape ladder an [`EscapeHook`] answers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EscapeRung {
	/// Rung 1 (`/mcp test`): every registered hook fires on one Esc and is
	/// removed, so the next Esc reaches the rungs below.
	Cancel,
	/// Rung 4 (vocalizer): persistent; consumes Esc only while it reports
	/// something to silence.
	Silence,
}

/// An observer-local Esc handler registered by a command (`/mcp test`
/// cancellation, vocalizer silence). Compared by identity.
#[derive(Clone)]
pub struct EscapeHook {
	/// Stable identity; re-registering an id replaces the prior hook.
	pub id:   Str,
	/// Ladder rung.
	pub rung: EscapeRung,
	hook:     Arc<dyn Fn() -> bool + Send + Sync>,
}

impl EscapeHook {
	/// Registers `hook`, which returns whether it consumed the Esc.
	pub fn new(
		id: impl Into<Str>,
		rung: EscapeRung,
		hook: impl Fn() -> bool + Send + Sync + 'static,
	) -> Self {
		Self { id: id.into(), rung, hook: Arc::new(hook) }
	}

	/// Fires the hook; returns whether it consumed the Esc.
	#[must_use]
	pub fn fire(&self) -> bool {
		(self.hook)()
	}
}

impl fmt::Debug for EscapeHook {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("EscapeHook")
			.field("id", &self.id)
			.field("rung", &self.rung)
			.finish_non_exhaustive()
	}
}

impl PartialEq for EscapeHook {
	fn eq(&self, other: &Self) -> bool {
		self.id == other.id && self.rung == other.rung && Arc::ptr_eq(&self.hook, &other.hook)
	}
}

impl Eq for EscapeHook {}

/// One observer-local request posted by a console command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostAction {
	/// `cl_interrupt` (pi `app.interrupt`, Esc): dismiss the topmost local
	/// surface, else interrupt the active turn, else preserve the draft.
	Interrupt,
	/// `cl_clear` (pi `app.clear`, Ctrl+C): clear the draft; on an empty
	/// draft interrupt the active turn, and a repeat quits.
	Clear,
	/// `cl_exit` (pi `app.exit`, Ctrl+D): leave the chat.
	Exit,
	/// `cl_suspend` (pi `app.suspend`, Ctrl+Z): job-control suspend.
	Suspend,
	/// `cl_display_reset` (pi `app.display.reset`, Alt+L): repaint from the
	/// retained document after re-probing the terminal.
	DisplayReset,
	/// `cl_thinking_cycle` (pi `app.thinking.cycle`, Shift+Tab): step
	/// `ai_thinking` through the current model's catalog efforts.
	ThinkingCycle,
	/// `cl_model_cycle [back]` (pi `app.model.cycleForward/Backward`,
	/// Ctrl+P / Ctrl+Shift+P): step `ai_model` through the role roster.
	ModelCycle {
		/// Step toward the previous role instead of the next.
		backward: bool,
	},
	/// `cl_model_select [session]` (pi `app.model.select` Alt+M /
	/// `app.model.selectTemporary` Alt+P): open the model picker.
	ModelSelect {
		/// Only this session: skip archiving the choice to `config.cfg`.
		session_only: bool,
	},
	/// `cl_followup` (pi `app.message.followUp`, Ctrl+Q / Alt+Enter):
	/// submit the draft as steering while a turn runs, else as a turn.
	FollowUp,
	/// `cl_retry` (pi `app.retry`, F5 / Alt+R): resend the last user prompt
	/// when its turn ended in an error notice.
	Retry,
	/// `cl_plan_toggle` (pi `app.plan.toggle`, Alt+Shift+P): flip the
	/// plan-mode Director engagement.
	PlanToggle,
	/// `cl_history_search` (pi `app.history.search`, Ctrl+R): open the
	/// prompt-history picker.
	HistorySearch,
	/// `cl_editor_external` (pi `app.editor.external`, Ctrl+G): edit the
	/// draft in `$VISUAL`/`$EDITOR`.
	ExternalEditor,
	/// `cl_dequeue` (pi `app.message.dequeue`, Alt+Up / Shift+Up): pull
	/// every queued message back into the composer.
	Dequeue,
	/// `cl_paste_image` (pi `app.clipboard.pasteImage`, Ctrl+V / Cmd+V):
	/// read the system clipboard, preferring an image, and stage it as a
	/// composer chip.
	PasteImage,
	/// `cl_paste_raw` (pi `app.clipboard.pasteTextRaw`, Ctrl+Shift+V /
	/// Alt+Shift+V): paste clipboard text verbatim.
	PasteRaw,
	/// `cl_copy_line` (pi `app.clipboard.copyLine`, Alt+Shift+L): copy the
	/// current composer line.
	CopyLine,
	/// `cl_copy_prompt` (pi `app.clipboard.copyPrompt`, Alt+Shift+C): copy
	/// the whole draft.
	CopyPrompt,
	/// `cl_agent_focus [id]`: view a subagent (`None` returns to the main
	/// session). pi `focusSession` / `unfocusSession`.
	FocusAgent(Option<Str>),
	/// `cl_collab_guest on|off`: this actor is a collaboration guest, so
	/// Esc asks the remote host to interrupt instead of aborting locally.
	CollabGuest(bool),
	/// `cl_stt_toggle` (pi `app.stt.toggle`): start or stop push-to-talk
	/// recording without the space-hold gesture.
	SttToggle,
	/// `cl_live_toggle` (pi `app.live.toggle`, Ctrl+L): start or stop the
	/// duplex live-voice session; the app owns the microphone and transport.
	LiveToggle,
	/// Push-to-talk recording edge from the space-hold gesture.
	PushToTalk {
		/// `true` when recording begins, `false` when the bar is released.
		active: bool,
	},
	/// Text recognized by speech-to-text, inserted at the caret.
	InsertText(Str),
	/// Register (or replace) an observer-local Esc hook.
	EscapeHook(EscapeHook),
	/// Remove an Esc hook by id.
	DropEscapeHook(Str),
	/// Open a command-owned panel on the overlay stack.
	Open(PanelOpener),
	/// Run a command-owned host effect and apply its panel event.
	Call(PanelCall),
	/// A typed slash-command request (`crate::commands`).
	Command(CommandAction),
	/// A console reply line (the sink installed by [`HostMailbox::attach`]).
	Reply {
		/// Reply severity.
		severity: Severity,
		/// Reply text.
		text:     Str,
	},
}

/// The actor's one inbound console mailbox: commands post actions, the
/// reply sink posts output lines, and the actor drains both after every
/// `exec`.
pub struct HostMailbox {
	tx: Sender<HostAction>,
	rx: Receiver<HostAction>,
}

impl Default for HostMailbox {
	fn default() -> Self {
		Self::new()
	}
}

impl HostMailbox {
	/// Creates an unbounded mailbox.
	#[must_use]
	pub fn new() -> Self {
		let (tx, rx) = flume::unbounded();
		Self { tx, rx }
	}

	/// Installs this mailbox as the builder's user object and routes the
	/// reply sink into it.
	#[must_use]
	pub fn attach(self, builder: CtxBuilder) -> CtxBuilder {
		let sink = self.tx.clone();
		builder
			.sink(move |severity, text| {
				let _ = sink.send(HostAction::Reply { severity, text: Str::new(text) });
			})
			.user(self)
	}

	/// Installs a fresh mailbox on an already-built context (no reply sink).
	pub fn install(ctx: &Ctx) {
		ctx.insert_user(Self::new());
	}

	/// Posts an action directly, bypassing the command stream.
	pub fn post(&self, action: HostAction) {
		let _ = self.tx.send(action);
	}

	/// Takes every queued action without blocking.
	pub fn drain(&self) -> impl Iterator<Item = HostAction> + '_ {
		self.rx.try_iter()
	}

	/// Waits for the next posted action (app-side results such as a
	/// speech transcript wake the actor through this).
	pub async fn next(&self) -> Option<HostAction> {
		self.rx.recv_async().await.ok()
	}
}

/// Posts `action` into the attached host mailbox, or warns on the console
/// when no interactive host is attached (a cfg script under `omp print`).
pub fn post(ctx: &Ctx, action: HostAction) -> ConResult<()> {
	match ctx.user::<HostMailbox>() {
		Some(mailbox) => {
			mailbox.post(action);
			Ok(())
		},
		None => {
			ctx.reply(Severity::Warn, "no interactive host is attached to this console");
			Ok(())
		},
	}
}

omp_con::var! {
	/// Expands tool cards in the transcript (pi `app.tools.expand`, Ctrl+O).
	pub static CL_TOOLS_EXPANDED = cl_tools_expanded: bool {
		default: false,
		flags: session | inherit,
	};
	/// Shows tool activity in the transcript (pi `app.tools.toggleVisibility`,
	/// Ctrl+Shift+O).
	pub static CL_SHOWTOOLS = cl_showtools: bool {
		default: true,
		flags: archive | session | inherit,
	};
}

omp_con::cmd! {
	/// Dismisses the topmost overlay, else interrupts the active turn.
	cl_interrupt() = |ctx, _args| post(ctx, HostAction::Interrupt);

	/// Clears the draft; on an empty draft interrupts the turn, twice quits.
	cl_clear() = |ctx, _args| post(ctx, HostAction::Clear);

	/// Leaves the chat.
	cl_exit() = |ctx, _args| post(ctx, HostAction::Exit);

	/// Suspends the chat to the shell (job control).
	cl_suspend() = |ctx, _args| post(ctx, HostAction::Suspend);

	/// Repaints the terminal from the retained transcript.
	cl_display_reset() = |ctx, _args| post(ctx, HostAction::DisplayReset);

	/// Cycles `ai_thinking` through the current model's reasoning efforts.
	cl_thinking_cycle() = |ctx, _args| post(ctx, HostAction::ThinkingCycle);

	/// Cycles `ai_model` through the role roster; `back` steps backward.
	cl_model_cycle(?direction: Str) = |ctx, args| {
		let backward = args
			.opt::<Str>(0)?
			.is_some_and(|direction| matches!(direction.as_str(), "back" | "backward" | "prev"));
		post(ctx, HostAction::ModelCycle { backward })
	};

	/// Opens the model picker; `session` keeps the choice out of config.cfg.
	cl_model_select(?scope: Str) = |ctx, args| {
		let session_only = args
			.opt::<Str>(0)?
			.is_some_and(|scope| matches!(scope.as_str(), "session" | "temporary" | "temp"));
		post(ctx, HostAction::ModelSelect { session_only })
	};

	/// Sends the draft as steering while a turn runs, else as a new turn.
	cl_followup() = |ctx, _args| post(ctx, HostAction::FollowUp);

	/// Resends the last prompt after a failed turn.
	cl_retry() = |ctx, _args| post(ctx, HostAction::Retry);

	/// Toggles plan mode.
	cl_plan_toggle() = |ctx, _args| post(ctx, HostAction::PlanToggle);

	/// Searches prompt history.
	cl_history_search() = |ctx, _args| post(ctx, HostAction::HistorySearch);

	/// Edits the draft in the external editor.
	cl_editor_external() = |ctx, _args| post(ctx, HostAction::ExternalEditor);

	/// Restores every queued message to the composer.
	cl_dequeue() = |ctx, _args| post(ctx, HostAction::Dequeue);

	/// Pastes the clipboard, attaching an image as a chip.
	cl_paste_image() = |ctx, _args| post(ctx, HostAction::PasteImage);

	/// Pastes clipboard text verbatim.
	cl_paste_raw() = |ctx, _args| post(ctx, HostAction::PasteRaw);

	/// Copies the current composer line to the clipboard.
	cl_copy_line() = |ctx, _args| post(ctx, HostAction::CopyLine);

	/// Copies the whole draft to the clipboard.
	cl_copy_prompt() = |ctx, _args| post(ctx, HostAction::CopyPrompt);

	/// Views a subagent's session; no id returns to the main session.
	cl_agent_focus(?id: Str) = |ctx, args| {
		let id = args.opt::<Str>(0)?.filter(|id| !id.is_empty());
		post(ctx, HostAction::FocusAgent(id))
	};

	/// Marks this actor as a collaboration guest (`on`) or host (`off`).
	cl_collab_guest(state: Str) = |ctx, args| {
		let state = args.get::<Str>(0)?;
		post(ctx, HostAction::CollabGuest(matches!(state.as_str(), "on" | "1" | "true")))
	};

	/// Starts or stops push-to-talk recording.
	cl_stt_toggle() = |ctx, _args| post(ctx, HostAction::SttToggle);

	/// Starts or stops the duplex live-voice session.
	cl_live_toggle() = |ctx, _args| post(ctx, HostAction::LiveToggle);

	/// Removes an observer-local Esc hook by id.
	cl_escape_unhook(id: Str) = |ctx, args| {
		let id = args.get::<Str>(0)?;
		post(ctx, HostAction::DropEscapeHook(id))
	};
}

omp_con::var! {
	/// What a double Esc on an empty composer opens (pi
	/// `doubleEscapeAction`): `branch` (rewind selector), `tree`, or `none`.
	pub static CL_DOUBLE_ESCAPE = cl_double_escape: Str {
		default: Str::new_static("branch"),
		flags: archive,
	};
	/// Whether holding the space bar starts push-to-talk (pi
	/// `stt.holdToTalk`).
	pub static CL_STT_HOLD = cl_stt_hold: bool {
		default: true,
		flags: archive,
	};
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn bound_command_posts_into_the_installed_mailbox() {
		let ctx = HostMailbox::new().attach(Ctx::builder()).build();
		ctx.run("cl_model_select session; cl_model_cycle back; echo hi")
			.expect("commands run");
		let mailbox = ctx.user::<HostMailbox>().expect("mailbox installed");
		let actions = mailbox.drain().collect::<Vec<_>>();
		assert_eq!(actions, [
			HostAction::ModelSelect { session_only: true },
			HostAction::ModelCycle { backward: true },
			HostAction::Reply { severity: Severity::Info, text: Str::new_static("hi") },
		]);
	}

	#[test]
	fn host_commands_without_a_mailbox_warn_instead_of_failing() {
		let ctx = Ctx::new();
		ctx.run("cl_interrupt")
			.expect("command degrades to a warning");
	}
}
