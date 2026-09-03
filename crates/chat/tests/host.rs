//! Session-DOM projection laws for the interactive chat actor.

use std::sync::Arc;

use omp_agent::{ApprovalBook, ApprovalScope, ApprovalSpec};
use omp_chat::{
	BlockKind, CtrlCAction, HostCommand, HostOptions, NativeEffect, NativeHost, block_views,
	ctrl_c_action, input::Bindings, overlays::Overlays,
};
use omp_dom::{Dom, Event, KnownTag, PropId, Tag};
use omp_session::{ComponentRegistry, Session};
use omp_tui::{Key, Size, UiContext, slots::ResizePolicy};
use tempfile::tempdir;

fn fixture() -> (Session, omp_journal::EntryId) {
	let directory = tempdir().expect("temp directory");
	let path = directory.keep().join("fixture.oms");
	let mut session = Session::create(path, ComponentRegistry::standard()).expect("create session");
	let genesis = session.head().expect("genesis");
	session.begin_turn().expect("begin turn");
	session.user("hello", Vec::new()).expect("user");
	session
		.assistant_start("test/model", "test", "test/model")
		.expect("assistant start");
	let turn = *session
		.dom()
		.children(session.dom().body())
		.last()
		.expect("turn");
	let assistant = session
		.dom()
		.children(turn)
		.iter()
		.copied()
		.find(|handle| {
			session
				.dom()
				.get(*handle)
				.is_some_and(|node| node.tag == Tag::Known(KnownTag::Assistant))
		})
		.expect("assistant");
	let thinking = session
		.stream_open(assistant, PropId::Thinking.into())
		.expect("thinking stream");
	session
		.stream_append(thinking, "considering")
		.expect("thinking delta");
	session.stream_close(thinking).expect("thinking close");
	let text = session
		.stream_open(assistant, PropId::Text.into())
		.expect("text stream");
	session.stream_append(text, "answer").expect("text delta");
	session.stream_close(text).expect("text close");
	session.assistant_end("tool_calls").expect("assistant end");
	let args =
		serde_json::value::to_raw_value(&serde_json::json!({"path":"note.txt"})).expect("args");
	let call = session
		.call("read", 1, "call-1", Some("read fixture".into()), Some(args), None)
		.expect("tool call");
	let outcome = serde_json::value::to_raw_value(&serde_json::json!({"text":"hello from fixture"}))
		.expect("outcome");
	session.settle(call, outcome).expect("tool result");
	session.receipt(12, 7, 0).expect("receipt");
	(session, genesis)
}

#[test]
fn fixture_session_projects_expected_block_sequence() {
	let (session, _) = fixture();
	let blocks = block_views(session.dom(), true);
	assert_eq!(blocks.iter().map(|block| block.kind).collect::<Vec<_>>(), [
		BlockKind::User,
		BlockKind::Thinking,
		BlockKind::Assistant,
		BlockKind::Tool,
		BlockKind::Usage,
	]);
	assert_eq!(blocks[0].text, "hello");
	assert_eq!(blocks[1].text, "considering");
	assert_eq!(blocks[2].text, "answer");
	assert!(blocks[3].text.contains("hello from fixture"));
	assert_eq!(blocks[4].text, "tokens 12 in / 7 out");
}

#[test]
fn reset_after_rewind_rebuilds_actor_blocks() {
	let (mut session, genesis) = fixture();
	let (snapshot, events) = session.subscribe();
	let mut replica = Dom::from_snapshot(&snapshot);
	assert!(!block_views(&replica, true).is_empty());

	session.rewind(genesis).expect("rewind");
	let event = events.recv().expect("reset event");
	assert!(matches!(event, Event::Reset { .. }));
	replica.apply_event(&event).expect("apply reset");
	assert!(block_views(&replica, true).is_empty());
}

#[test]
fn ctrl_c_interrupts_active_turn_and_quits_when_idle_or_repeated() {
	assert_eq!(ctrl_c_action(true, false), CtrlCAction::Interrupt);
	assert_eq!(ctrl_c_action(false, false), CtrlCAction::Quit);
	assert_eq!(ctrl_c_action(true, true), CtrlCAction::Quit);
}

#[test]
fn pending_approval_projects_overlay_and_hotkeys() {
	let directory = tempdir().expect("temp directory");
	let path = directory.path().join("approval.oms");
	let mut session =
		Session::create(path, ComponentRegistry::standard()).expect("create approval session");
	let ticket = ApprovalBook::default()
		.open(&mut session, ApprovalSpec {
			title:         "Run command".into(),
			body:          "The command changes the project.".into(),
			subject:       "cargo fix".into(),
			kind:          "exec".into(),
			scopes:        vec!["once".into()],
			default:       None,
			route:         "user".into(),
			approver:      None,
			timeout_ms:    0,
			unreachable:   "deny".into(),
			require_human: true,
			pattern:       None,
			evidence:      Vec::new(),
		})
		.expect("open approval");
	let mut overlays = Overlays::default();
	overlays.sync_approval(session.dom());
	let approval = overlays.approval().expect("approval overlay");
	assert_eq!(approval.id, ticket.ticket_id);
	assert_eq!(approval.title, "Run command");
	assert!(!approval.decision('n').expect("deny").approved);
	assert_eq!(approval.decision('a').expect("session approval").scope, ApprovalScope::Session);
	assert!(approval.decision('y').expect("approve").approved);

	let (snapshot, dom_events) = session.subscribe();
	let (_, kernel_events) = flume::unbounded();
	let (commands, command_rx) = flume::unbounded();
	let (up, _) = flume::unbounded();
	let mut host = NativeHost::new(
		HostOptions {
			model: omp_chat::ModelBadge::from_identifier("test/model"),
			snapshot,
			dom_events,
			kernel_events,
			commands,
			up,
			con: Arc::new(omp_con::Ctx::new()),
			bindings: Bindings::new(std::iter::empty::<(omp_core::Str, omp_core::Str)>()),
			models: Vec::new(),
			cycle: Vec::new(),
			resize_policy: ResizePolicy::Rebuild,
			branch: None,
			ui: UiContext::default(),
		},
		Size::new(80, 24),
	);
	assert_eq!(host.key(Key::Char('a')).expect("approval key"), NativeEffect::Consumed);
	match command_rx.recv().expect("approval command") {
		HostCommand::Approve { id, decision } => {
			assert_eq!(id, ticket.ticket_id);
			assert!(decision.approved);
			assert_eq!(decision.scope, ApprovalScope::Session);
		},
		other => panic!("unexpected host command: {other:?}"),
	}
}

fn bound_host(models: Vec<omp_chat::ModelRow>) -> (NativeHost, flume::Receiver<HostCommand>) {
	let (mut session, _) = fixture();
	let (snapshot, dom_events) = session.subscribe();
	let (_, kernel_events) = flume::unbounded();
	let (commands, command_rx) = flume::unbounded();
	let (up, _) = flume::unbounded();
	let con = Arc::new(omp_chat::HostMailbox::new().attach(omp_con::Ctx::builder()).build());
	con.run(
		r#"bind alt+p "cl_model_select session"; bind shift+tab cl_thinking_cycle; bind ctrl+r cl_history_search; bind escape cl_interrupt"#,
	)
	.expect("binds");
	let bindings = Bindings::new(
		con.binds()
			.into_iter()
			.map(|(chord, command)| (omp_chat::input::normalize_chord(&chord).unwrap(), command)),
	);
	let host = NativeHost::new(
		HostOptions {
			model: omp_chat::ModelBadge::from_identifier("test/model"),
			snapshot,
			dom_events,
			kernel_events,
			commands,
			up,
			con,
			bindings,
			models,
			cycle: Vec::new(),
			resize_policy: ResizePolicy::Rebuild,
			branch: None,
			ui: UiContext::default(),
		},
		Size::new(100, 30),
	);
	(host, command_rx)
}

fn row(key: &'static str, efforts: &[&'static str]) -> omp_chat::ModelRow {
	omp_chat::ModelRow {
		key:         key.into(),
		name:        key.into(),
		provider_id: "test".into(),
		provider:    "Test".into(),
		context:     Some(200_000),
		input_mtok:  None,
		output_mtok: None,
		efforts:     efforts.iter().map(|effort| omp_core::Str::new_static(effort)).collect(),
	}
}

#[test]
fn alt_p_opens_the_model_picker_and_enter_sets_ai_model_for_the_session() {
	let (mut host, commands) =
		bound_host(vec![row("test/model", &["low", "high"]), row("test/other", &[])]);
	assert!(!host.overlay_open());
	assert_eq!(host.key(Key::Alt('p')).expect("alt+p"), NativeEffect::Consumed);
	assert!(host.overlay_open(), "alt+p opens the picker");
	let frame = host.picker_frame().expect("picker frame");
	assert!(omp_tui::frame_text(&frame).contains("Switch Model"));
	assert!(matches!(commands.recv().expect("overlay open"), HostCommand::Overlay { open: true, .. }));
	host.key(Key::Down).expect("down");
	host.key(Key::Enter).expect("enter");
	assert!(!host.overlay_open(), "picking closes the picker");
	assert_eq!(host.notice(), Some("Session model: test/other"));
	assert!(matches!(commands.recv().expect("overlay close"), HostCommand::Overlay { open: false, .. }));
	assert!(commands.try_recv().is_err(), "a session-only pick never reaches the controller");
}

#[test]
fn escape_dismisses_the_picker_before_anything_else() {
	let (mut host, _commands) = bound_host(vec![row("test/model", &[])]);
	host.key(Key::Alt('p')).expect("alt+p");
	assert!(host.overlay_open());
	host.key(Key::Esc).expect("esc");
	assert!(!host.overlay_open());
}

#[test]
fn shift_tab_cycles_ai_thinking_through_the_model_efforts_then_off() {
	let (mut host, _commands) = bound_host(vec![row("test/model", &["low", "high"])]);
	let mut seen = Vec::new();
	for _ in 0..3 {
		host.key(Key::BackTab).expect("shift+tab");
		seen.push(host.notice().expect("thinking notice").to_owned());
	}
	assert_eq!(seen, ["Thinking: low", "Thinking: high", "Thinking: off"]);
	let (mut host, _commands) = bound_host(vec![row("test/model", &[])]);
	host.key(Key::BackTab).expect("shift+tab");
	assert_eq!(host.notice(), Some("Current model does not support thinking"));
}

#[test]
fn ctrl_r_recalls_a_prior_prompt_into_the_composer() {
	let (mut host, _commands) = bound_host(vec![row("test/model", &[])]);
	host.key(Key::Ctrl('r')).expect("ctrl+r");
	assert!(host.overlay_open(), "history picker opens over the fixture's prompt");
	host.key(Key::Enter).expect("enter");
	assert!(!host.overlay_open());
	assert_eq!(host.key(Key::Char('!')).expect("type"), NativeEffect::Consumed);
}

#[test]
fn slash_and_unknown_commands_surface_as_notices_not_host_errors() {
	let (mut host, _commands) = bound_host(Vec::new());
	assert_eq!(host.console("no_such_command").expect("console"), NativeEffect::Consumed);
	assert!(host.notice().is_some_and(|text| text.contains("no_such_command")));
	assert_eq!(host.console("cl_model_select").expect("console"), NativeEffect::Consumed);
	assert_eq!(host.notice(), Some("No models are available to switch to"));
}

#[test]
fn thinking_toggle_changes_projection_without_touching_dom() {
	let (session, _) = fixture();
	let ctx = omp_con::Ctx::new();
	let before = session.dom().snapshot();
	let shown = block_views(session.dom(), omp_con::CL_SHOWTHINKING.get(&ctx));
	ctx.exec("toggle cl_showthinking", omp_con::Source::Console)
		.expect("toggle command");
	let hidden = block_views(session.dom(), omp_con::CL_SHOWTHINKING.get(&ctx));
	let after = session.dom().snapshot();

	assert!(shown.iter().any(|block| block.kind == BlockKind::Thinking));
	assert!(!hidden.iter().any(|block| block.kind == BlockKind::Thinking));
	assert_eq!(before, after);
}
