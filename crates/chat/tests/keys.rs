//! Key semantics of the interactive actor: pi's Escape ladder, Ctrl+C,
//! dequeue, clipboard chords, panel routing, gestures, and Esc hooks.

use std::{
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use omp_agent::Up;
use omp_chat::{
	HostAction, HostCommand, HostOptions, NativeEffect, NativeHost,
	actions::{EscapeHook, EscapeRung},
	composer::{SPACE_HOLD_RELEASE, SpaceHold, SpaceHoldEvent},
	input::{Bindings, normalize_chord},
	overlays::{NoServices, Panel, PanelAction, PanelAnchor, PanelCall, PanelEvent, PanelOpener},
};
use omp_core::Str;
use omp_dom::{KnownTag, NodeSpec, Op, PropId, Tag, Txn, Value};
use omp_session::{ComponentRegistry, Session};
use omp_tui::{Frame, Key, Size, UiContext, paste::ClipboardRead, slots::ResizePolicy};
use tempfile::tempdir;

const BINDS: &str = r#"
bind escape cl_interrupt
bind ctrl+c cl_clear
bind alt+up cl_dequeue
bind alt+shift+l cl_copy_line
bind alt+shift+c cl_copy_prompt
bind ctrl+v cl_paste_image
bind ctrl+shift+v cl_paste_raw
"#;

struct Harness {
	host:     NativeHost,
	commands: flume::Receiver<HostCommand>,
	up:       flume::Receiver<Up>,
	session:  Session,
	con:      Arc<omp_con::Ctx>,
}

fn idle_session() -> Session {
	let directory = tempdir().expect("temp directory");
	let path = directory.keep().join("keys.oms");
	let mut session = Session::create(path, ComponentRegistry::standard()).expect("create session");
	session.begin_turn().expect("begin turn");
	session.user("earlier prompt", Vec::new()).expect("user");
	session
		.assistant_start("test/model", "test", "test/model")
		.expect("assistant start");
	session.assistant_end("stop").expect("assistant end");
	session
		.receipt(omp_journal::data::TurnReceipt::tokens(1, 1, 0))
		.expect("receipt");
	session
}

fn harness(mut session: Session) -> Harness {
	let (snapshot, dom_events) = session.subscribe();
	let (_, kernel_events) = flume::unbounded();
	let (commands, command_rx) = flume::unbounded();
	let (up, up_rx) = flume::unbounded();
	let con = Arc::new(
		omp_chat::HostMailbox::new()
			.attach(omp_con::Ctx::builder())
			.build(),
	);
	con.run(BINDS).expect("binds");
	let bindings = Bindings::new(
		con.binds()
			.into_iter()
			.map(|(chord, command)| (normalize_chord(&chord).unwrap(), command)),
	);
	let host = NativeHost::new(
		HostOptions {
			model: omp_chat::ModelBadge::from_identifier("test/model"),
			snapshot,
			dom_events,
			kernel_events,
			commands,
			up,
			con: Arc::clone(&con),
			bindings,
			models: Vec::new(),
			cycle: Vec::new(),
			resize_policy: ResizePolicy::Rebuild,
			project: std::path::PathBuf::new(),
			welcome: omp_chat::welcome::WelcomeFacts::default(),
			ui: UiContext::default(),
			services: Arc::new(NoServices),
		},
		Size::new(100, 30),
	);
	Harness { host, commands: command_rx, up: up_rx, session, con }
}

fn open_turn(session: &mut Session) {
	session.begin_turn().expect("begin turn");
	session.user("streaming", Vec::new()).expect("user");
	session
		.assistant_start("test/model", "test", "test/model")
		.expect("assistant start");
}

fn type_text(host: &mut NativeHost, text: &str) {
	for character in text.chars() {
		host.key(Key::Char(character)).expect("type");
	}
}

fn engage_director(session: &mut Session, family: &'static str) {
	let dom = session.dom();
	let directors = dom
		.children(dom.meta())
		.iter()
		.copied()
		.find(|handle| {
			dom.get(*handle)
				.is_some_and(|node| node.tag == Tag::Known(KnownTag::Directors))
		})
		.expect("directors component");
	let cause = session.head().expect("head");
	session
		.patch(Txn {
			cause,
			label: None,
			ops: vec![Op::Ins {
				parent: directors,
				after:  None,
				node:   NodeSpec::new(KnownTag::Director)
					.with_prop(
						omp_dom::PropKey::Custom(Str::new_static("family")),
						Value::Str(Str::new_static(family)),
					)
					.with_prop(
						omp_dom::PropKey::Custom(Str::new_static("status")),
						Value::Str(Str::new_static("active")),
					),
			}],
		})
		.expect("engage director");
}

fn queue_prompt(session: &mut Session, id: &'static str, text: &'static str) {
	let dom = session.dom();
	let prompts = dom
		.children(dom.queues())
		.iter()
		.copied()
		.find(|handle| {
			dom.get(*handle)
				.is_some_and(|node| node.tag == Tag::Known(KnownTag::Prompts))
		})
		.expect("prompts queue");
	let cause = session.head().expect("head");
	session
		.patch(Txn {
			cause,
			label: None,
			ops: vec![Op::Ins {
				parent: prompts,
				after:  session.dom().children(prompts).last().copied(),
				node:   NodeSpec::new(KnownTag::Prompt)
					.with_prop(PropId::Id, Value::Str(Str::new_static(id)))
					.with_prop(PropId::Kind, Value::Str(Str::new_static("queued")))
					.with_prop(PropId::Status, Value::Str(Str::new_static("pending")))
					.with_content(Str::new_static(text)),
			}],
		})
		.expect("queue prompt");
}

// ---------------------------------------------------------------- escape ladder

#[test]
fn escape_preserves_a_draft_and_never_interrupts_an_idle_session() {
	let mut h = harness(idle_session());
	type_text(&mut h.host, "draft");
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.composer_text(), "draft");
	assert!(h.commands.try_recv().is_err(), "an idle session has nothing to interrupt");
}

#[test]
fn escape_interrupts_a_streaming_turn_and_restores_queued_prompts() {
	let mut session = idle_session();
	open_turn(&mut session);
	queue_prompt(&mut session, "q1", "queued one");
	let mut h = harness(session);
	// The kernel answers the unqueue with one undelivered steer.
	let up = h.up.clone();
	std::thread::spawn(move || {
		if let Ok(Up::Unqueue(reply)) = up.recv_timeout(Duration::from_secs(2)) {
			let _ = reply.send(vec![Str::new_static("steer one")]);
		}
	});
	type_text(&mut h.host, "draft");
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.composer_text(), "steer one\n\nqueued one\n\ndraft");
	let mut saw_dequeue = false;
	let mut saw_interrupt = false;
	while let Ok(command) = h.commands.try_recv() {
		match command {
			HostCommand::Dequeue { prompts } => {
				assert_eq!(prompts, [Str::new_static("q1")]);
				saw_dequeue = true;
			},
			HostCommand::Interrupt => saw_interrupt = true,
			other => panic!("unexpected {other:?}"),
		}
	}
	assert!(saw_dequeue && saw_interrupt);
}

#[test]
fn double_escape_within_500ms_on_an_empty_composer_runs_the_selector_line() {
	let mut h = harness(idle_session());
	// What the configured `branch` line does when run directly.
	h.host.console("branch").expect("console");
	let expected = (h.host.overlay_id(), h.host.notice().map(str::to_owned));
	while h.host.overlay_id().is_some() {
		h.host.key(Key::Esc).expect("close");
	}
	h.host.key(Key::Char('x')).expect("clear notice");
	h.host.key(Key::Backspace).expect("clear notice");

	// A lone Esc, then a late second one: nothing.
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.overlay_id(), None);
	std::thread::sleep(Duration::from_millis(600));
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.overlay_id(), None);
	// Two within the window: the `branch` line runs.
	h.host.key(Key::Esc).expect("esc");
	assert_eq!((h.host.overlay_id(), h.host.notice().map(str::to_owned)), expected);
	assert!(
		expected.0.is_some() || expected.1.is_some(),
		"double escape must reach the console"
	);
	// `none` disables it.
	h.con
		.run("cl_double_escape none")
		.expect("set");
	while h.host.overlay_id().is_some() {
		h.host.key(Key::Esc).expect("close");
	}
	h.host.key(Key::Esc).expect("esc");
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.overlay_id(), None);
}

#[test]
fn double_escape_never_fires_while_a_draft_exists() {
	let mut h = harness(idle_session());
	type_text(&mut h.host, "keep me");
	h.host.key(Key::Esc).expect("esc");
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.overlay_id(), None);
	assert_eq!(h.host.composer_text(), "keep me");
}

#[test]
fn escape_in_bash_or_eval_prefix_mode_clears_the_draft_instead_of_interrupting() {
	let mut session = idle_session();
	open_turn(&mut session);
	let mut h = harness(session);
	type_text(&mut h.host, "!ls -la");
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.composer_text(), "");
	assert!(h.commands.try_recv().is_err(), "prefix mode wins over the streaming rung");
	type_text(&mut h.host, "$1+1");
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.composer_text(), "");
}

#[test]
fn escape_cancel_hooks_fire_once_and_silence_hooks_stay() {
	let mut session = idle_session();
	open_turn(&mut session);
	let mut h = harness(session);
	let cancelled = Arc::new(AtomicUsize::new(0));
	let silenced = Arc::new(AtomicUsize::new(0));
	let speaking = Arc::new(AtomicUsize::new(1));
	{
		let cancelled = Arc::clone(&cancelled);
		h.host
			.act(HostAction::EscapeHook(EscapeHook::new("mcp-test", EscapeRung::Cancel, move || {
				cancelled.fetch_add(1, Ordering::SeqCst);
				true
			})))
			.expect("hook");
	}
	{
		let silenced = Arc::clone(&silenced);
		let speaking = Arc::clone(&speaking);
		h.host
			.act(HostAction::EscapeHook(EscapeHook::new("vocalizer", EscapeRung::Silence, move || {
				if speaking.swap(0, Ordering::SeqCst) == 1 {
					silenced.fetch_add(1, Ordering::SeqCst);
					true
				} else {
					false
				}
			})))
			.expect("hook");
	}
	assert_eq!(h.host.escape_hooks(), ["mcp-test", "vocalizer"]);
	// Rung 1: the cancel hook fires and is forgotten; nothing else happens.
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(cancelled.load(Ordering::SeqCst), 1);
	assert_eq!(silenced.load(Ordering::SeqCst), 0);
	assert_eq!(h.host.escape_hooks(), ["vocalizer"]);
	assert!(h.commands.try_recv().is_err());
	// Rung 4: the vocalizer is silenced before the turn is touched.
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(silenced.load(Ordering::SeqCst), 1);
	assert!(h.commands.try_recv().is_err());
	// Nothing left to silence: the streaming turn is interrupted.
	h.host.key(Key::Esc).expect("esc");
	assert!(matches!(h.commands.try_recv(), Ok(HostCommand::Interrupt)));
	assert_eq!(h.host.escape_hooks(), ["vocalizer"], "silence hooks persist");
}

#[test]
fn escape_in_loop_mode_pauses_when_idle_and_interrupts_when_streaming() {
	let mut session = idle_session();
	engage_director(&mut session, "loop_mode");
	let mut h = harness(session);
	h.host.key(Key::Esc).expect("esc");
	// `pause` is the console line; whatever it does, it never reaches
	// Interrupt and never opens a selector.
	assert!(!matches!(h.commands.try_recv(), Ok(HostCommand::Interrupt)));
	assert!(!matches!(h.host.overlay_id(), Some("rewind" | "tree")));
	while h.host.overlay_id().is_some() {
		h.host.key(Key::Esc).expect("close");
	}
	open_turn(&mut h.session);
	h.host.poll().expect("apply");
	h.host.key(Key::Esc).expect("esc");
	assert!(h.commands.try_iter().any(|command| matches!(command, HostCommand::Interrupt)));
}

#[test]
fn escape_in_a_subagent_view_clears_text_then_returns_to_main() {
	let mut session = idle_session();
	open_turn(&mut session);
	let mut h = harness(session);
	h.host
		.act(HostAction::FocusAgent(Some(Str::new_static("worker-1"))))
		.expect("focus");
	assert_eq!(h.host.focused_agent(), Some("worker-1"));
	assert!(matches!(
		h.commands.try_recv(),
		Ok(HostCommand::Overlay { open: true, .. })
	));
	type_text(&mut h.host, "note");
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.composer_text(), "");
	assert_eq!(h.host.focused_agent(), Some("worker-1"), "first Esc only clears text");
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.focused_agent(), None, "second Esc returns to main");
	assert!(matches!(
		h.commands.try_recv(),
		Ok(HostCommand::Overlay { open: false, .. })
	));
	assert!(
		h.commands.try_recv().is_err(),
		"the focused subagent's turn is never interrupted"
	);
}

#[test]
fn double_left_on_an_empty_composer_unfocuses_the_subagent() {
	let mut h = harness(idle_session());
	h.host
		.act(HostAction::FocusAgent(Some(Str::new_static("worker-1"))))
		.expect("focus");
	// A synthesized burst (no gap) never counts.
	h.host.key(Key::Left).expect("left");
	h.host.key(Key::Left).expect("left");
	assert_eq!(h.host.focused_agent(), Some("worker-1"));
	std::thread::sleep(Duration::from_millis(600));
	h.host.key(Key::Left).expect("left");
	std::thread::sleep(Duration::from_millis(80));
	h.host.key(Key::Left).expect("left");
	assert_eq!(h.host.focused_agent(), None);
}

#[test]
fn collab_guest_escape_forwards_an_interrupt_and_stops_there() {
	let mut session = idle_session();
	open_turn(&mut session);
	let mut h = harness(session);
	h.host.act(HostAction::CollabGuest(true)).expect("guest");
	type_text(&mut h.host, "draft");
	h.host.key(Key::Esc).expect("esc");
	assert!(matches!(h.commands.try_recv(), Ok(HostCommand::Interrupt)));
	assert_eq!(h.host.composer_text(), "draft", "guest Esc never touches the draft");
}

#[test]
fn escape_cancels_main_session_maintenance_but_not_from_a_subagent_view() {
	let mut session = idle_session();
	open_turn(&mut session);
	engage_director(&mut session, "compaction");
	let mut h = harness(session);
	h.host.key(Key::Esc).expect("esc");
	assert!(matches!(h.commands.try_recv(), Ok(HostCommand::Interrupt)));
	h.host
		.act(HostAction::FocusAgent(Some(Str::new_static("worker-1"))))
		.expect("focus");
	let _ = h.commands.try_recv();
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.focused_agent(), None);
	assert!(
		!h.commands.try_iter().any(|command| matches!(command, HostCommand::Interrupt)),
		"Esc from a focused subagent returns to main instead of cancelling maintenance"
	);
}

// ---------------------------------------------------------------- ctrl+c

#[test]
fn ctrl_c_clears_the_draft_then_stops_a_recording() {
	let mut h = harness(idle_session());
	type_text(&mut h.host, "draft");
	h.host.key(Key::Ctrl('c')).expect("ctrl+c");
	assert_eq!(h.host.composer_text(), "");
	h.host.act(HostAction::SttToggle).expect("record");
	assert!(h.host.recording());
	assert!(matches!(h.commands.try_recv(), Ok(HostCommand::PushToTalk { active: true })));
	h.host.key(Key::Ctrl('c')).expect("ctrl+c");
	assert!(!h.host.recording());
	assert!(matches!(h.commands.try_recv(), Ok(HostCommand::PushToTalk { active: false })));
}

// ---------------------------------------------------------------- dequeue

#[test]
fn alt_up_restores_queued_prompts_ahead_of_the_draft() {
	let mut session = idle_session();
	queue_prompt(&mut session, "q1", "first");
	queue_prompt(&mut session, "q2", "second");
	let mut h = harness(session);
	type_text(&mut h.host, "draft");
	h.host.key(Key::RestoreQueue).expect("alt+up");
	assert_eq!(h.host.composer_text(), "first\n\nsecond\n\ndraft");
	assert_eq!(h.host.notice(), Some("Restored 2 queued messages to editor"));
	assert!(matches!(
		h.commands.try_recv(),
		Ok(HostCommand::Dequeue { prompts }) if prompts == [Str::new_static("q1"), Str::new_static("q2")]
	));
	assert!(h.up.try_recv().is_err(), "no turn: the kernel is not asked");
}

#[test]
fn alt_up_with_nothing_queued_reports_and_keeps_the_draft() {
	let mut h = harness(idle_session());
	type_text(&mut h.host, "draft");
	h.host.key(Key::RestoreQueue).expect("alt+up");
	assert_eq!(h.host.composer_text(), "draft");
	assert_eq!(h.host.notice(), Some("No queued messages to restore"));
}

// ---------------------------------------------------------------- clipboard

#[test]
fn copy_line_and_copy_prompt_hand_text_to_the_clipboard() {
	let mut h = harness(idle_session());
	type_text(&mut h.host, "one");
	h.host.key(Key::ShiftEnter).expect("newline");
	type_text(&mut h.host, "two");
	h.host.key(Key::CopyLine).expect("alt+shift+l");
	assert_eq!(h.host.take_clipboard().as_deref(), Some("two"));
	assert_eq!(h.host.notice(), Some("Copied line"));
	h.host.key(Key::CopyPrompt).expect("alt+shift+c");
	assert_eq!(h.host.take_clipboard().as_deref(), Some("one\ntwo"));
	assert_eq!(h.host.notice(), Some("Copied prompt"));
}

#[test]
fn paste_chords_request_the_matching_clipboard_read_and_deliver_it() {
	let mut h = harness(idle_session());
	h.host.key(Key::Paste).expect("ctrl+v");
	assert_eq!(h.host.take_clipboard_read(), Some(ClipboardRead::Smart));
	h.host.key(Key::PasteRaw).expect("ctrl+shift+v");
	assert_eq!(h.host.take_clipboard_read(), Some(ClipboardRead::Text));
	// Raw text keeps its newlines verbatim.
	h.host
		.deliver_clipboard(Some(omp_tui::paste::Clipboard::Text("a\nb".into())), true);
	assert_eq!(h.host.composer_text(), "a\nb");
	// An image lands as an attachment chip referencing the persisted file.
	// A 1x1 PNG: signature, IHDR, IDAT, IEND.
	let png = omp_tui::PastedImage::from_bytes(vec![
		0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
		0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
		0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8,
		0xFF, 0xFF, 0x3F, 0x00, 0x05, 0xFE, 0x02, 0xFE, 0xA7, 0x35, 0x81, 0x84, 0x00, 0x00, 0x00,
		0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
	])
	.expect("png header");
	h.host.key(Key::Ctrl('c')).expect("clear");
	h.host
		.deliver_clipboard(Some(omp_tui::paste::Clipboard::Image(png)), false);
	let text = h.host.composer_text();
	assert!(text.contains("omp-tui-paste-"), "chip references the temp file: {text:?}");
	h.host.key(Key::Ctrl('c')).expect("clear");
	h.host.deliver_clipboard(None, false);
	assert_eq!(h.host.notice(), Some("Clipboard is empty"));
}

// ---------------------------------------------------------------- panels

struct Probe {
	id:      &'static str,
	anchor:  PanelAnchor,
	actions: Arc<parking_lot::Mutex<Vec<PanelAction>>>,
	frame:   Frame,
}

impl Panel for Probe {
	fn id(&self) -> &'static str {
		self.id
	}

	fn anchor(&self) -> PanelAnchor {
		self.anchor
	}

	fn action(&mut self, action: PanelAction) -> PanelEvent {
		self.actions.lock().push(action);
		PanelEvent::Consumed
	}

	fn key(&mut self, key: Key) -> PanelEvent {
		match key {
			Key::Enter => PanelEvent::Finish(Str::new_static("echo picked")),
			Key::Char('r') => PanelEvent::Recall(Str::new_static("recalled")),
			Key::Char('c') => PanelEvent::Copy(Str::new_static("copied")),
			_ => PanelEvent::Ignored,
		}
	}

	fn frame(&mut self, _viewport: Size) -> &Frame {
		&self.frame
	}
}

fn open_probe(
	host: &mut NativeHost,
	id: &'static str,
	anchor: PanelAnchor,
) -> Arc<parking_lot::Mutex<Vec<PanelAction>>> {
	let actions = Arc::new(parking_lot::Mutex::new(Vec::new()));
	let seen = Arc::clone(&actions);
	host
		.act(HostAction::Open(PanelOpener::new(move |_cx| {
			Ok(Box::new(Probe {
				id,
				anchor,
				actions: Arc::clone(&seen),
				frame: Frame::new(Size::new(10, 1)),
			}) as Box<dyn Panel>)
		})))
		.expect("open");
	actions
}

#[test]
fn panels_receive_lowered_session_and_tree_chords_before_raw_keys() {
	let mut h = harness(idle_session());
	let actions = open_probe(&mut h.host, "sessions", PanelAnchor::Bottom);
	assert_eq!(h.host.overlay_id(), Some("sessions"));
	assert!(matches!(
		h.commands.try_recv(),
		Ok(HostCommand::Overlay { open: true, .. })
	));
	for key in [
		Key::Ctrl('p'),
		Key::Ctrl('s'),
		Key::Ctrl('r'),
		Key::Ctrl('d'),
		Key::Ctrl('w'),
		Key::WordLeft,
		Key::WordRight,
		Key::Ctrl('o'),
	] {
		h.host.key(key).expect("panel key");
	}
	assert_eq!(&*actions.lock(), &[
		PanelAction::TogglePath,
		PanelAction::ToggleSort,
		PanelAction::Rename,
		PanelAction::Delete,
		PanelAction::DeleteFast,
		PanelAction::FoldUp,
		PanelAction::UnfoldDown,
		PanelAction::Expand,
	]);
	// Esc closes a panel that ignores it.
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.overlay_id(), None);
	assert!(matches!(
		h.commands.try_recv(),
		Ok(HostCommand::Overlay { open: false, .. })
	));
}

#[test]
fn panel_events_run_console_lines_recall_text_and_copy() {
	let mut h = harness(idle_session());
	open_probe(&mut h.host, "probe", PanelAnchor::Center);
	h.host.key(Key::Char('c')).expect("copy");
	assert_eq!(h.host.take_clipboard().as_deref(), Some("copied"));
	assert_eq!(h.host.overlay_id(), Some("probe"), "copy keeps the panel open");
	h.host.key(Key::Enter).expect("finish");
	assert_eq!(h.host.overlay_id(), None);
	assert_eq!(h.host.notice(), Some("picked"), "Finish closes, then runs the line");
	open_probe(&mut h.host, "probe", PanelAnchor::Center);
	h.host.key(Key::Char('r')).expect("recall");
	assert_eq!(h.host.overlay_id(), None);
	assert_eq!(h.host.composer_text(), "recalled");
}

#[test]
fn side_panels_leave_the_composer_live_and_close_at_escape_rung_two() {
	let mut session = idle_session();
	open_turn(&mut session);
	let mut h = harness(session);
	open_probe(&mut h.host, "btw", PanelAnchor::Side);
	assert!(!h.host.overlay_open(), "a side panel is not modal");
	type_text(&mut h.host, "typed");
	assert_eq!(h.host.composer_text(), "typed");
	h.host.key(Key::Esc).expect("esc");
	assert_eq!(h.host.overlay_depth(), 0, "rung 2 closes the side panel");
	assert!(
		!h.commands.try_iter().any(|command| matches!(command, HostCommand::Interrupt)),
		"the streaming turn survives the side-panel Esc"
	);
	assert_eq!(h.host.composer_text(), "typed");
}

#[test]
fn a_panel_call_feeds_its_event_through_the_same_path() {
	let mut h = harness(idle_session());
	h.host
		.act(HostAction::Call(PanelCall::new(|cx| {
			PanelEvent::Notice(Str::new(format!("width {}", cx.viewport.width)))
		})))
		.expect("call");
	assert_eq!(h.host.notice(), Some("width 100"));
	h.host
		.act(HostAction::Open(PanelOpener::new(|_cx| Err(Str::new_static("nope")))))
		.expect("open");
	assert_eq!(h.host.notice(), Some("nope"));
	assert_eq!(h.host.overlay_id(), None);
}

// ---------------------------------------------------------------- push-to-talk

#[test]
fn space_hold_recognizes_a_metronomic_repeat_and_tracks_back_typed_spaces() {
	let mut hold = SpaceHold::default();
	let ms = Duration::from_millis;
	// Two deliberate spaces: typed.
	assert_eq!(hold.observe(Key::Space, ms(0), true), SpaceHoldEvent::Pass);
	assert_eq!(hold.observe(Key::Space, ms(400), true), SpaceHoldEvent::Pass);
	// A held bar: 33ms repeat. The first repeat gap is not yet a pattern.
	assert_eq!(hold.observe(Key::Space, ms(433), true), SpaceHoldEvent::Pass);
	assert_eq!(hold.observe(Key::Space, ms(466), true), SpaceHoldEvent::Swallow);
	assert_eq!(hold.observe(Key::Space, ms(499), true), SpaceHoldEvent::Begin { track_back: 3 });
	assert!(hold.active());
	assert_eq!(hold.observe(Key::Space, ms(532), true), SpaceHoldEvent::Swallow);
	assert_eq!(hold.next_wake(), Some(ms(532) + SPACE_HOLD_RELEASE));
	assert!(!hold.release_due(ms(700)));
	assert!(hold.release_due(ms(532) + SPACE_HOLD_RELEASE));
	assert!(!hold.active());
	// Jittery smashing never escalates.
	let mut smash = SpaceHold::default();
	for at in [0, 60, 150, 200, 300] {
		assert_eq!(smash.observe(Key::Space, ms(at), true), SpaceHoldEvent::Pass);
	}
	// A non-space during a hold ends it and passes through.
	let mut hold = SpaceHold::default();
	for at in [0, 33, 66] {
		hold.observe(Key::Space, ms(at), true);
	}
	hold.observe(Key::Space, ms(99), true);
	assert!(hold.active());
	assert_eq!(hold.observe(Key::Char('a'), ms(120), true), SpaceHoldEvent::EndThenPass);
	// Disabled: plain spaces.
	let mut off = SpaceHold::default();
	for at in [0, 33, 66, 99] {
		assert_eq!(off.observe(Key::Space, ms(at), false), SpaceHoldEvent::Pass);
	}
}

#[test]
fn a_held_space_bar_starts_recording_and_release_stops_it() {
	let mut h = harness(idle_session());
	type_text(&mut h.host, "hi");
	let epoch = h.host.clock();
	// Feed the gesture on the real clock: repeats 30ms apart.
	for _ in 0..5 {
		h.host.key(Key::Space).expect("space");
		std::thread::sleep(Duration::from_millis(30));
	}
	assert!(h.host.recording(), "metronomic repeat begins push-to-talk");
	assert_eq!(h.host.composer_text(), "hi", "pre-burst spaces are tracked back");
	assert!(matches!(h.commands.try_recv(), Ok(HostCommand::PushToTalk { active: true })));
	// Release: the idle gap elapses on the presentation clock.
	std::thread::sleep(SPACE_HOLD_RELEASE + Duration::from_millis(20));
	assert!(h.host.tick(epoch.elapsed()));
	assert!(!h.host.recording());
	assert!(matches!(h.commands.try_recv(), Ok(HostCommand::PushToTalk { active: false })));
	// The recognizer's text lands at the caret.
	h.host
		.act(HostAction::InsertText(Str::new_static(" there")))
		.expect("insert");
	assert_eq!(h.host.composer_text(), "hi there");
}

#[test]
fn live_toggle_flips_the_session_and_stops_push_to_talk_first() {
	let mut h = harness(idle_session());
	h.host.act(HostAction::SttToggle).expect("record");
	let _ = h.commands.try_recv();
	h.host.act(HostAction::LiveToggle).expect("live");
	assert!(!h.host.recording());
	assert!(matches!(h.commands.try_recv(), Ok(HostCommand::PushToTalk { active: false })));
	assert!(matches!(h.commands.try_recv(), Ok(HostCommand::LiveVoice { active: true })));
	assert_eq!(h.host.notice(), Some("Live voice on · Ctrl+L to stop"));
	h.host.act(HostAction::LiveToggle).expect("live");
	assert!(matches!(h.commands.try_recv(), Ok(HostCommand::LiveVoice { active: false })));
}

// ---------------------------------------------------------------- console words

#[test]
fn every_key_command_is_registered_on_the_console() {
	let h = harness(idle_session());
	let registered = h
		.con
		.items()
		.filter_map(|item| match item {
			omp_con::RegItem::Cmd(spec) => Some(spec.name),
			_ => None,
		})
		.collect::<Vec<_>>();
	for word in [
		"cl_dequeue",
		"cl_paste_image",
		"cl_paste_raw",
		"cl_copy_line",
		"cl_copy_prompt",
		"cl_agent_focus",
		"cl_collab_guest",
		"cl_stt_toggle",
		"cl_live_toggle",
		"cl_escape_unhook",
	] {
		assert!(registered.contains(&word), "{word} missing from the console");
	}
	assert_eq!(
		h.con.get("cl_double_escape").expect("var"),
		omp_con::Value::Str(Str::new_static("branch"))
	);
	assert!(h.session.head().is_some());
}

#[test]
fn console_words_drive_focus_and_guest_state() {
	let mut h = harness(idle_session());
	assert_eq!(h.host.console("cl_agent_focus worker-9").expect("console"), NativeEffect::Consumed);
	assert_eq!(h.host.focused_agent(), Some("worker-9"));
	h.host.console("cl_agent_focus").expect("console");
	assert_eq!(h.host.focused_agent(), None);
	h.host.console("cl_collab_guest on").expect("console");
	h.host.console("cl_escape_unhook nothing").expect("console");
	assert!(h.host.escape_hooks().is_empty());
}
