//! Journal and DOM contracts for complete, tool-using, steered, and interrupted
//! turns.

use std::time::Duration;

use omp_agent::{
	DispatchPolicy, Kernel, KernelEvent, RunControl, StaticPrompt, TurnInput, TurnStop, Up,
};
use omp_core::Str;
use omp_dom::{PropId, PropKey};
use omp_inference::{BlockKind, ChatEvent, ContentPart, Role};
use omp_journal::{blob::BlobStore, kind};

mod support;

use support::{
	ScriptedInference, assert_all_entries_caused, completed, fresh_session, journal_entries,
	registry, spec, text_script, tool_script,
};

fn input(text: &str) -> TurnInput {
	TurnInput { text: Str::new(text), attachments: Vec::new() }
}

fn policy(path: &std::path::Path) -> DispatchPolicy {
	DispatchPolicy::new(BlobStore::open(path).expect("blob store opens"))
}

fn prop_text<'a>(session: &'a omp_session::Session, selector: &str, prop: PropId) -> &'a str {
	let handle = session
		.dom()
		.select(selector)
		.expect("selector parses")
		.next()
		.expect("node exists");
	let key = PropKey::from(prop);
	session
		.dom()
		.get(handle)
		.expect("node materializes")
		.prop(&key)
		.and_then(omp_dom::Value::as_str)
		.expect("text property exists")
}

#[tokio::test]
async fn user_turn_journals_assistant_text_in_the_explicit_turn() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let journal_path = directory.path().join("text.oms");
	let (inference, requests) = ScriptedInference::new([text_script("pong")]);
	let mut kernel = Kernel::new(
		inference,
		registry(std::iter::empty()),
		policy(&directory.path().join("blobs")),
		StaticPrompt(Str::new_static("test system")),
	);
	let mut session = fresh_session(&journal_path);

	let outcome = kernel
		.run_turn(&mut session, input("reply once"), RunControl::default())
		.await
		.expect("turn completes");

	assert_eq!(outcome.stop, TurnStop::Completed);
	assert_eq!(outcome.assistant_text, "pong");
	assert_eq!(requests.lock().len(), 1);
	assert_eq!(
		session
			.dom()
			.select("body turn user")
			.expect("selector")
			.count(),
		1
	);
	assert_eq!(
		session
			.dom()
			.select("body turn assistant")
			.expect("selector")
			.count(),
		1
	);
	assert_eq!(prop_text(&session, "body turn assistant", PropId::Text), "pong");
	// pi's usage row needs TTFT and duration on the receipt; both are
	// kernel-clock measurements the projection cannot derive later.
	let usage = session
		.dom()
		.select("body turn usage")
		.expect("selector")
		.next()
		.expect("receipt materializes");
	let usage = session.dom().get(usage).expect("usage node");
	assert!(matches!(usage.prop(&PropKey::from(PropId::DurationMs)), Some(omp_dom::Value::Int(_))));
	assert!(matches!(usage.prop(&PropKey::from(PropId::TtftMs)), Some(omp_dom::Value::Int(_))));
	assert!(matches!(usage.prop(&PropKey::from(PropId::CacheRead)), Some(omp_dom::Value::Int(0))));

	drop(session);
	let entries = journal_entries(&journal_path);
	assert_all_entries_caused(&entries);
	for required in [
		kind::TURN_START,
		kind::MSG_USER,
		kind::MSG_ASSISTANT_START,
		kind::MSG_ASSISTANT_END,
		kind::TURN_RECEIPT,
	] {
		assert!(
			entries
				.iter()
				.any(|entry| entry.kind.name.as_str() == required),
			"missing {required}"
		);
	}
}

#[tokio::test]
async fn tool_call_round_settles_in_the_dom_then_runs_second_inference() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let journal_path = directory.path().join("tool.oms");
	let mut tool_round = vec![
		ChatEvent::BlockStarted { index: 0, kind: BlockKind::Thinking },
		ChatEvent::ThinkingDelta { index: 0, text: Str::new_static("unsigned reasoning") },
	];
	tool_round.extend(tool_script("echo-1", "echo", serde_json::json!({})));
	let (inference, requests) = ScriptedInference::new([tool_round, text_script("hello from tool")]);
	let mut kernel = Kernel::new(
		inference,
		registry([spec("echo", 1, "hello").streaming("progress", Duration::ZERO)]),
		policy(&directory.path().join("blobs")),
		StaticPrompt(Str::new_static("test system")),
	);
	let events = kernel.subscribe();
	let mut session = fresh_session(&journal_path);

	let outcome = kernel
		.run_turn(&mut session, input("use echo"), RunControl::default())
		.await
		.expect("tool turn completes");

	assert_eq!(outcome.stop, TurnStop::Completed);
	assert_eq!(outcome.assistant_text, "hello from tool");
	let events = events.try_iter().collect::<Vec<_>>();
	assert_eq!(events, [
		KernelEvent::InferenceStarted,
		KernelEvent::ThinkingDelta(Str::new_static("unsigned reasoning")),
		KernelEvent::ToolReady {
			call_id: Str::new_static("echo-1"),
			name:    Str::new_static("echo"),
		},
		KernelEvent::ToolUpdate { call_id: Str::new_static("echo-1") },
		KernelEvent::ToolSettled { call_id: Str::new_static("echo-1"), is_error: false },
		KernelEvent::InferenceStarted,
		KernelEvent::TextDelta(Str::new_static("hello from tool")),
		KernelEvent::TurnEnded { stop: TurnStop::Completed },
	]);
	let requests = requests.lock();
	assert_eq!(requests.len(), 2);
	assert!(!requests[1].messages.iter().any(|message| {
		message
			.content
			.iter()
			.any(|part| matches!(part, ContentPart::Reasoning { proof: None, .. }))
	}));
	assert!(requests[1].messages.iter().any(|message| {
		message.content.iter().any(|part| matches!(part, ContentPart::ToolResult { content, .. }
			if content.iter().any(|part| matches!(part, omp_inference::ToolResultContent::Text(text) if text == "hello"))))
	}));
	drop(requests);
	assert_eq!(
		session
			.dom()
			.select("body turn echo")
			.expect("selector")
			.count(),
		1
	);
	assert_eq!(
		session
			.dom()
			.select("body turn echo input")
			.expect("selector")
			.count(),
		1
	);
	assert_eq!(
		session
			.dom()
			.select("body turn echo result")
			.expect("selector")
			.count(),
		1
	);
	assert_eq!(prop_text(&session, "body turn echo result", PropId::Text), "hello");

	drop(session);
	let entries = journal_entries(&journal_path);
	assert_all_entries_caused(&entries);
	let call = entries
		.iter()
		.find(|entry| entry.kind.name.as_str() == kind::TOOL_CALL)
		.expect("tool call journals");
	let result = entries
		.iter()
		.find(|entry| entry.kind.name.as_str() == kind::TOOL_RESULT)
		.expect("tool result journals");
	assert_eq!(result.by, Some(call.id));
}

#[tokio::test]
async fn steering_is_drained_after_tool_results_before_the_yield_decision() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let journal_path = directory.path().join("steer.oms");
	let (inference, requests) = ScriptedInference::new([
		tool_script("echo-1", "echo", serde_json::json!({})),
		text_script("steered answer"),
	]);
	let mut kernel = Kernel::new(
		inference,
		registry([spec("echo", 1, "tool settled")]),
		policy(&directory.path().join("blobs")),
		StaticPrompt(Str::new_static("test system")),
	);
	kernel
		.mailbox()
		.send(Up::Steer(Str::new_static("include the settled result")))
		.expect("steering queues");
	let mut session = fresh_session(&journal_path);

	let outcome = kernel
		.run_turn(&mut session, input("use echo"), RunControl::default())
		.await
		.expect("steered turn completes");

	assert_eq!(outcome.stop, TurnStop::Steered);
	let requests = requests.lock();
	assert_eq!(requests.len(), 2);
	let second = &requests[1];
	assert!(second.messages.iter().any(|message| {
		message.role == Role::Tool
			&& message
				.content
				.iter()
				.any(|part| matches!(part, ContentPart::ToolResult { .. }))
	}));
	assert!(second.messages.iter().any(|message| {
		message.role == Role::System
			&& message.content.iter().any(|part| {
				matches!(part,
				ContentPart::Text { text, .. } if text == "include the settled result")
			})
	}));
	drop(requests);
	let steering = session
		.dom()
		.select("queues steering user")
		.expect("selector")
		.next()
		.expect("steering queue records item");
	assert_eq!(
		session
			.dom()
			.get(steering)
			.and_then(|node| node.content.as_deref()),
		Some("include the settled result")
	);
	assert_eq!(
		session
			.dom()
			.select("body turn developer")
			.expect("selector")
			.count(),
		1
	);

	drop(session);
	assert_all_entries_caused(&journal_entries(&journal_path));
}

#[tokio::test]
async fn interrupt_returns_cancelled_without_journaling_a_false_completion() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let journal_path = directory.path().join("interrupt.oms");
	let (inference, _requests) =
		ScriptedInference::new([vec![completed(omp_inference::FinishReason::Stop, 0)]]);
	let mut kernel = Kernel::new(
		inference,
		registry(std::iter::empty()),
		policy(&directory.path().join("blobs")),
		StaticPrompt(Str::new_static("test system")),
	);
	kernel
		.mailbox()
		.send(Up::Interrupt)
		.expect("interrupt queues");
	let mut session = fresh_session(&journal_path);

	let outcome = kernel
		.run_turn(&mut session, input("cancel me"), RunControl::default())
		.await
		.expect("interrupt settles turn");

	assert_eq!(outcome.stop, TurnStop::Cancelled);
	assert_eq!(outcome.assistant_text, "");
	assert_eq!(session.dom().select("body turn").expect("selector").count(), 1);
	assert_eq!(
		session
			.dom()
			.select("body turn assistant")
			.expect("selector")
			.count(),
		0
	);
	drop(session);
	let entries = journal_entries(&journal_path);
	assert_all_entries_caused(&entries);
	assert!(
		!entries
			.iter()
			.any(|entry| entry.kind.name.as_str() == kind::MSG_ASSISTANT_END)
	);
	assert!(
		!entries
			.iter()
			.any(|entry| entry.kind.name.as_str() == kind::TURN_RECEIPT)
	);
}
