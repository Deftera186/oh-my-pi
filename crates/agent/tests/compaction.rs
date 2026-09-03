//! Compaction director integration proofs over the journal-derived session
//! tree.

use std::{collections::VecDeque, sync::Arc};

use futures::stream;
use omp_agent::{
	director::{BoxFut, Director, ErasedInference, MutDirectorCx, Prepared, RouteFacts},
	directors::compaction::CompactionDirector,
};
use omp_core::Str;
use omp_dom::{KnownTag, NodeSpec, Op, PropId, Txn, Value};
use omp_inference::{
	ChatEvent, ChatRequest, ChatStream, ContentPart, Message, NegotiationPolicy, Role,
	SafetySetting, Sampling, Setting,
};
use omp_journal::{Journal, data::Compaction, kind};
use omp_session::{ComponentRegistry, Session, projection::project_thread};

struct FakeInference {
	replies:  VecDeque<Str>,
	requests: Vec<ChatRequest>,
}

impl FakeInference {
	fn with_reply(reply: &str) -> Self {
		Self { replies: VecDeque::from([Str::new(reply)]), requests: Vec::new() }
	}
}

impl ErasedInference for FakeInference {
	fn execute<'a>(
		&'a mut self,
		request: ChatRequest,
	) -> BoxFut<'a, Result<ChatStream, omp_inference::Error>> {
		self.requests.push(request);
		let reply = self
			.replies
			.pop_front()
			.expect("one summary reply configured");
		Box::pin(async move {
			Ok(ChatStream::ordinary(Box::pin(stream::iter([Ok(ChatEvent::TextDelta {
				index: 0,
				text:  reply,
			})]))))
		})
	}
}

fn request(text: &str) -> ChatRequest {
	ChatRequest {
		messages:          Arc::from([Message {
			role:    Role::User,
			content: Arc::from([ContentPart::Text { text: Str::new(text), proof: None }]),
			name:    None,
		}]),
		tools:             Arc::from([]),
		hosted_tools:      Arc::from([]),
		tool_choice:       Setting::Unset,
		output:            Setting::Unset,
		reasoning:         Setting::Unset,
		verbosity:         Setting::Unset,
		cache_retention:   Setting::Unset,
		service_tier:      Setting::Unset,
		sampling:          Sampling::default(),
		max_output_tokens: None,
		top_logprobs:      None,
		safety:            Arc::<[SafetySetting]>::from([]),
		negotiation:       NegotiationPolicy::default(),
	}
}

fn turn_handle(session: &Session) -> omp_dom::Handle {
	*session
		.dom()
		.children(session.dom().body())
		.last()
		.expect("turn is materialized")
}

fn set_compact_threshold(session: &mut Session, threshold: f64) {
	let con = session
		.dom()
		.select("con")
		.expect("con selector")
		.next()
		.expect("con component");
	let after = session.dom().children(con).last().copied();
	session
		.patch(Txn {
			cause: session.head().expect("journal head"),
			label: Some(Str::new_static("test.compaction.threshold")),
			ops:   vec![Op::Ins {
				parent: con,
				after,
				node: NodeSpec::new(KnownTag::Var)
					.with_prop(PropId::Name, Value::Str(Str::new_static("ai_compact_threshold")))
					.with_prop(PropId::Value, Value::Float(threshold)),
			}],
		})
		.expect("threshold patch");
}

#[tokio::test]
async fn under_threshold_skips_compaction() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let path = directory.path().join("session.oms");
	let blobs = omp_journal::blob::BlobStore::open(directory.path()).expect("blob store");
	let mut session = Session::create(&path, ComponentRegistry::standard()).expect("session");
	session.begin_turn().expect("turn");
	session.user("short", Vec::new()).expect("user");
	let mut inference = FakeInference::with_reply("unused");
	let route = RouteFacts { forced_choice_free: false, context_window: 16_000 };
	let turn = turn_handle(&session);
	let mut cx = MutDirectorCx {
		session: &mut session,
		inference: &mut inference,
		blobs: &blobs,
		route: &route,
		turn,
		director: None,
	};
	let prepared = CompactionDirector::new()
		.before_inference(&mut cx, &request("short"))
		.await
		.expect("preparation");
	assert_eq!(prepared, Prepared::Unchanged);
	assert_eq!(session.dom().count("compaction").expect("selector"), 0);
	assert!(inference.requests.is_empty());
}

#[tokio::test]
async fn dom_ai_compact_threshold_controls_compaction() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let path = directory.path().join("session.oms");
	let blobs = omp_journal::blob::BlobStore::open(directory.path()).expect("blob store");
	let mut session = Session::create(&path, ComponentRegistry::standard()).expect("session");
	session.begin_turn().expect("turn");
	let text = "small but configured ".repeat(12);
	session.user(text.clone(), Vec::new()).expect("user");
	set_compact_threshold(&mut session, 0.10);
	let mut inference = FakeInference::with_reply("threshold summary");
	let route = RouteFacts { forced_choice_free: false, context_window: 512 };
	let turn = turn_handle(&session);
	let mut cx = MutDirectorCx {
		session: &mut session,
		inference: &mut inference,
		blobs: &blobs,
		route: &route,
		turn,
		director: None,
	};
	assert_eq!(
		CompactionDirector::new()
			.before_inference(&mut cx, &request(&text))
			.await
			.expect("configured preparation"),
		Prepared::Rebuild
	);
	assert_eq!(session.dom().count("compaction").expect("selector"), 1);
}

#[tokio::test]
async fn over_threshold_commits_one_resolvable_compaction_and_replays_projection() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let path = directory.path().join("session.oms");
	let blobs = omp_journal::blob::BlobStore::open(directory.path()).expect("blob store");
	let mut session = Session::create(&path, ComponentRegistry::standard()).expect("session");
	let turn_id = session.begin_turn().expect("turn");
	let text = "history ".repeat(100);
	let boundary = session.user(text.clone(), Vec::new()).expect("user");
	let mut inference = FakeInference::with_reply("durable compacted context");
	let route = RouteFacts { forced_choice_free: false, context_window: 128 };
	let turn = turn_handle(&session);
	let mut cx = MutDirectorCx {
		session: &mut session,
		inference: &mut inference,
		blobs: &blobs,
		route: &route,
		turn,
		director: None,
	};
	let prepared = CompactionDirector::new()
		.before_inference(&mut cx, &request(&text))
		.await
		.expect("preparation");
	assert_eq!(prepared, Prepared::Rebuild);
	let repeated = CompactionDirector::new()
		.before_inference(&mut cx, &request(&text))
		.await
		.expect("re-entrant preparation");
	assert_eq!(repeated, Prepared::Unchanged);
	assert_eq!(session.dom().count("compaction").expect("selector"), 1);
	let live_projection = project_thread(session.dom());
	assert_eq!(live_projection.len(), 1);
	let projected = live_projection[0]
		.kind
		.as_ref()
		.and_then(|kind| match kind {
			omp_proto::thread::v1::item::Kind::Message(message) => message.parts.first(),
			_ => None,
		})
		.and_then(|part| part.kind.as_ref())
		.and_then(|kind| match kind {
			omp_proto::thread::v1::part::Kind::Text(text) => Some(text.as_str()),
			_ => None,
		});
	assert_eq!(projected, Some("durable compacted context"));
	drop(session);

	let (journal, entries) = Journal::open(&path).expect("journal reopens");
	let compact_entries = entries
		.iter()
		.filter(|entry| entry.kind.name == kind::COMPACTION && entry.kind.rev == 1)
		.collect::<Vec<_>>();
	assert_eq!(compact_entries.len(), 1);
	assert_eq!(compact_entries[0].by, Some(turn_id));
	let payload: Compaction =
		serde_json::from_str(compact_entries[0].data.as_str()).expect("compaction payload");
	assert_eq!(payload.boundary, boundary);
	assert_eq!(
		blobs.get(&payload.summary).expect("summary blob"),
		b"durable compacted context".as_slice()
	);
	drop(journal);

	let reopened = Session::open(&path, ComponentRegistry::standard()).expect("session replays");
	assert_eq!(project_thread(reopened.dom()), live_projection);
}

#[tokio::test]
async fn manual_compaction_carries_focus_and_ignores_threshold() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let path = directory.path().join("session.oms");
	let blobs = omp_journal::blob::BlobStore::open(directory.path()).expect("blob store");
	let mut session = Session::create(&path, ComponentRegistry::standard()).expect("session");
	session.begin_turn().expect("turn");
	session.user("small history", Vec::new()).expect("user");
	let mut inference = FakeInference::with_reply("focused context");
	let route = RouteFacts { forced_choice_free: false, context_window: 1_000_000 };
	let turn = turn_handle(&session);
	let mut cx = MutDirectorCx {
		session: &mut session,
		inference: &mut inference,
		blobs: &blobs,
		route: &route,
		turn,
		director: None,
	};
	let prepared = CompactionDirector::manual(Some(Str::new_static("database migration")))
		.before_inference(&mut cx, &request("small history"))
		.await
		.expect("manual compaction");
	assert_eq!(prepared, Prepared::Rebuild);
	assert_eq!(session.dom().count("compaction").expect("selector"), 1);
	let summary_request = inference.requests.first().expect("summary request");
	assert!(summary_request.tools.is_empty());
	assert!(summary_request.hosted_tools.is_empty());
	let instruction = summary_request.messages[0]
		.content
		.first()
		.and_then(|part| match part {
			ContentPart::Text { text, .. } => Some(text.as_str()),
			_ => None,
		})
		.expect("summary instruction");
	assert!(instruction.contains("database migration"));
}
