//! DOM-only message projection laws.

use omp_core::{Hash32, Str};
use omp_dom::{KnownTag, Tag};
use omp_journal::blob::{BlobRef, BlobStore};
use omp_proto::thread::v1::{item, part};
use omp_session::{ComponentRegistry, Session, project_thread};
use serde_json::value::RawValue;

fn raw(value: serde_json::Value) -> Box<RawValue> {
	serde_json::value::to_raw_value(&value).expect("test JSON serializes")
}

fn find_tag(session: &Session, tag: KnownTag) -> Vec<omp_dom::Handle> {
	session
		.dom()
		.handles()
		.filter(|handle| {
			session
				.dom()
				.get(*handle)
				.is_some_and(|node| node.tag == Tag::Known(tag))
		})
		.collect()
}

#[test]
fn every_body_element_is_inside_an_explicit_turn() {
	let directory = tempfile::tempdir().expect("temporary session directory");
	let mut session =
		Session::create(directory.path().join("turns.oms"), ComponentRegistry::default())
			.expect("session creates");
	session.begin_turn().expect("turn starts");
	session.user("question", Vec::new()).expect("user appends");
	session
		.assistant_start("model", "provider", "route")
		.expect("assistant starts");
	let call = session
		.call(
			"read",
			1,
			"call-1",
			Some(Str::new_static("read a file")),
			Some(raw(serde_json::json!({"path":"README.md"}))),
			None,
		)
		.expect("tool call appends");
	session
		.settle(call, raw(serde_json::json!({"text":"contents"})))
		.expect("tool settles");
	let turn = *session
		.dom()
		.children(session.dom().body())
		.last()
		.expect("turn exists");
	let tool = session
		.dom()
		.children(turn)
		.iter()
		.copied()
		.find(|handle| {
			session
				.dom()
				.get(*handle)
				.is_some_and(|node| matches!(node.tag, Tag::Custom(_)))
		})
		.expect("tool exists");
	let child_tags: Vec<_> = session
		.dom()
		.children(tool)
		.iter()
		.map(|handle| {
			session
				.dom()
				.get(*handle)
				.expect("tool child exists")
				.tag
				.clone()
		})
		.collect();
	assert_eq!(child_tags, [
		Tag::Known(KnownTag::Input),
		Tag::Known(KnownTag::Result),
		Tag::Known(KnownTag::Diag),
		Tag::Known(KnownTag::Usage),
	]);

	for child in session.dom().children(session.dom().body()) {
		assert_eq!(
			session.dom().get(*child).expect("body child exists").tag,
			Tag::Known(KnownTag::Turn)
		);
	}
}

#[test]
fn message_projection_is_a_pure_function_of_the_dom() {
	let directory = tempfile::tempdir().expect("temporary session directory");
	let mut session =
		Session::create(directory.path().join("projection.oms"), ComponentRegistry::default())
			.expect("session creates");
	session.begin_turn().expect("turn starts");
	session.user("question", Vec::new()).expect("user appends");
	session
		.assistant_start("model", "provider", "route")
		.expect("assistant starts");
	let assistant = session
		.dom()
		.children(
			*session
				.dom()
				.children(session.dom().body())
				.last()
				.expect("turn"),
		)
		.last()
		.copied()
		.expect("assistant");
	let sid = session
		.stream_open(assistant, omp_dom::PropId::Text.into())
		.expect("stream opens");
	session
		.stream_append(sid, "answer")
		.expect("stream appends");
	session.stream_close(sid).expect("stream closes");
	session.assistant_end("stop").expect("assistant ends");
	let call = session
		.call("read", 1, "call-1", None, Some(raw(serde_json::json!({"path":"README.md"}))), None)
		.expect("tool call appends");
	session
		.settle(call, raw(serde_json::json!({"text":"contents"})))
		.expect("tool settles");
	let before = session.dom().snapshot();
	let first = project_thread(session.dom());
	let second = project_thread(session.dom());
	assert_eq!(first, second);
	assert_eq!(session.dom().snapshot().as_bytes(), before.as_bytes());
	assert!(
		first
			.iter()
			.any(|item| matches!(item.kind, Some(item::Kind::ToolCall(_))))
	);
	assert!(
		first
			.iter()
			.any(|item| matches!(item.kind, Some(item::Kind::ToolResult(_))))
	);
}

#[test]
fn todo_and_jobs_are_journal_derived_meta_components() {
	let directory = tempfile::tempdir().expect("temporary session directory");
	let path = directory.path().join("components.oms");
	let mut session = Session::create(&path, ComponentRegistry::default()).expect("session creates");
	session.begin_turn().expect("turn starts");
	let todo = session
		.call("todo", 1, "todo-1", None, Some(raw(serde_json::json!({}))), None)
		.expect("todo call appends");
	session
		.settle(
			todo,
			raw(serde_json::json!({
				"phases": [{
					"phase": "Build",
					"items": [{"text": "land substrate", "status": "in_progress"}]
				}],
				"rendered": "Build"
			})),
		)
		.expect("todo snapshot settles");
	let items = find_tag(&session, KnownTag::Item);
	assert_eq!(items.len(), 1);
	let item = session.dom().get(items[0]).expect("todo item exists");
	assert_eq!(
		item
			.prop(&omp_dom::PropKey::from(omp_dom::PropId::Status))
			.and_then(omp_dom::Value::as_str),
		Some("in_progress")
	);

	let call = session
		.call("bash", 1, "job-1", None, Some(raw(serde_json::json!({}))), None)
		.expect("detached call appends");
	session
		.settle(call, raw(serde_json::json!({"kind":"detached","id":"job-1"})))
		.expect("detached terminal settles");
	assert_eq!(find_tag(&session, KnownTag::Job).len(), 1);
	drop(session);

	let restored = Session::open(path, ComponentRegistry::default()).expect("session restores");
	assert_eq!(find_tag(&restored, KnownTag::Item).len(), 1);
	assert_eq!(find_tag(&restored, KnownTag::Job).len(), 1);
}

#[test]
fn projection_excludes_pre_compaction_turns_and_prepends_summary() {
	let directory = tempfile::tempdir().expect("temporary session directory");
	let path = directory.path().join("compact.oms");
	let store = BlobStore::open(directory.path()).expect("blob store opens");
	let bytes = b"summary of earlier turns";
	let summary = store.put(bytes).expect("summary stores");
	assert_eq!(summary, BlobRef { hash: Hash32::sum(bytes), size: bytes.len() as u64 });
	let mut session = Session::create(&path, ComponentRegistry::default()).expect("session creates");
	session.begin_turn().expect("old turn starts");
	let boundary = session.user("old", Vec::new()).expect("old user appends");
	session
		.assistant_start("model", "provider", "route")
		.expect("post-boundary assistant starts");
	let turn = *session
		.dom()
		.children(session.dom().body())
		.last()
		.expect("turn");
	let assistant = *session.dom().children(turn).last().expect("assistant");
	let sid = session
		.stream_open(assistant, omp_dom::PropId::Text.into())
		.expect("assistant stream opens");
	session
		.stream_append(sid, "after-boundary")
		.expect("assistant delta appends");
	session.stream_close(sid).expect("assistant stream closes");
	session
		.compaction(omp_journal::data::Compaction::new(summary, boundary))
		.expect("compaction appends");
	session.begin_turn().expect("new turn starts");
	session.user("new", Vec::new()).expect("new user appends");

	let items = project_thread(session.dom());
	assert_eq!(items.len(), 3);
	let texts: Vec<_> = items
		.iter()
		.filter_map(|item| match item.kind.as_ref()? {
			item::Kind::Message(message) => match message.parts.first()?.kind.as_ref()? {
				part::Kind::Text(text) => Some(text.as_str()),
				_ => None,
			},
			_ => None,
		})
		.collect();
	assert_eq!(texts, ["summary of earlier turns", "after-boundary", "new"]);
}

#[test]
fn streamed_call_ready_and_abandoned_argument_state_replay_identically() {
	let directory = tempfile::tempdir().expect("temporary session directory");
	let path = directory.path().join("streamed-call.oms");
	let mut session = Session::create(&path, ComponentRegistry::default()).expect("session creates");
	session.begin_turn().expect("turn starts");
	let (ready_call, ready_sid) = session
		.call_streaming("read", 1, "ready-call", None)
		.expect("streaming call starts");
	session
		.stream_append(ready_sid, r#"{"path":"#)
		.expect("first argument delta");
	session
		.stream_append(ready_sid, r#"README.md"}"#)
		.expect("second argument delta");
	session
		.call_ready(ready_call, raw(serde_json::json!({"path":"README.md"})))
		.expect("streaming call becomes executable");
	let (_abandoned_call, abandoned_sid) = session
		.call_streaming("grep", 1, "abandoned-call", None)
		.expect("second streaming call starts");
	session
		.stream_append(abandoned_sid, r#"{"pattern":"#)
		.expect("partial argument delta");
	let live = session.dom().snapshot();
	drop(session);

	let restored = Session::open(&path, ComponentRegistry::default()).expect("session restores");
	assert_eq!(restored.dom().snapshot().as_bytes(), live.as_bytes());
	let statuses: std::collections::BTreeMap<_, _> = restored
		.dom()
		.handles()
		.filter_map(|handle| {
			let node = restored.dom().get(handle)?;
			let Tag::Custom(_) = node.tag else {
				return None;
			};
			let id = node
				.prop(&omp_dom::PropKey::from(omp_dom::PropId::Id))
				.and_then(omp_dom::Value::as_str)?;
			let status = node
				.prop(&omp_dom::PropKey::from(omp_dom::PropId::Status))
				.and_then(omp_dom::Value::as_str)?;
			Some((id.to_owned(), status.to_owned()))
		})
		.collect();
	assert_eq!(statuses.get("ready-call").map(String::as_str), Some("running"));
	assert_eq!(statuses.get("abandoned-call").map(String::as_str), Some("arguments"));
}

#[test]
fn streamed_call_carries_intent_on_ready() {
	let directory = tempfile::tempdir().expect("temporary session directory");
	let path = directory.path().join("streamed-intent.oms");
	let mut session = Session::create(&path, ComponentRegistry::default()).expect("session creates");
	session.begin_turn().expect("turn starts");
	let (call, sid) = session
		.call_streaming("read", 1, "intent-call", None)
		.expect("streaming call starts");
	session
		.stream_append(sid, r#"{"i":"Reading project manifest","path":"Cargo.toml"}"#)
		.expect("argument delta");
	session
		.call_ready(
			call,
			raw(serde_json::json!({
				"i": "Reading project manifest",
				"path": "Cargo.toml"
			})),
		)
		.expect("streaming call becomes executable");
	let live = session.dom().snapshot();
	let tool = session
		.dom()
		.handles()
		.find(|handle| {
			session.dom().get(*handle).is_some_and(|node| {
				matches!(node.tag, Tag::Custom(_))
					&& node
						.prop(&omp_dom::PropKey::from(omp_dom::PropId::Id))
						.and_then(omp_dom::Value::as_str)
						== Some("intent-call")
			})
		})
		.expect("tool element exists");
	assert_eq!(
		session
			.dom()
			.get(tool)
			.and_then(|node| node.prop(&omp_dom::PropKey::from(omp_dom::PropId::I)))
			.and_then(omp_dom::Value::as_str),
		Some("Reading project manifest"),
	);
	drop(session);

	let restored = Session::open(&path, ComponentRegistry::default()).expect("session restores");
	assert_eq!(restored.dom().snapshot().as_bytes(), live.as_bytes());
}

#[test]
fn projected_results_and_reserved_ready_updates_preflight_before_journaling() {
	let directory = tempfile::tempdir().expect("temporary session directory");
	let path = directory.path().join("projected-preflight.oms");
	let mut session = Session::create(&path, ComponentRegistry::default()).expect("session creates");
	session.begin_turn().expect("turn starts");
	let (call, sid) = session
		.call_streaming("read", 1, "call-1", None)
		.expect("streaming call starts");
	session.stream_append(sid, "{}").expect("argument delta");
	let before_file = std::fs::read(&path).expect("journal bytes");
	let before_dom = session.dom().snapshot();
	let malformed_ready = raw(serde_json::json!({"kernel":"ready"}));
	assert!(matches!(
		session.call_update(call, malformed_ready),
		Err(omp_session::SessionError::ReservedToolUpdate)
	));
	assert_eq!(std::fs::read(&path).expect("journal unchanged"), before_file);
	assert_eq!(session.dom().snapshot().as_bytes(), before_dom.as_bytes());
	session
		.call_ready(call, raw(serde_json::json!({})))
		.expect("typed ready succeeds");

	let invalid_parts = raw(serde_json::json!({}));
	let before_file = std::fs::read(&path).expect("journal bytes");
	let before_dom = session.dom().snapshot();
	assert!(
		session
			.settle_projected(call, raw(serde_json::json!({"text":"ok"})), invalid_parts)
			.is_err()
	);
	assert_eq!(std::fs::read(&path).expect("journal unchanged"), before_file);
	assert_eq!(session.dom().snapshot().as_bytes(), before_dom.as_bytes());

	let invalid_utf8_parts = raw(serde_json::json!([{"kind":"json","json":[255]}]));
	let before_file = std::fs::read(&path).expect("journal bytes");
	let before_dom = session.dom().snapshot();
	assert!(matches!(
		session.settle_projected(call, raw(serde_json::json!({"text":"ok"})), invalid_utf8_parts,),
		Err(omp_session::SessionError::ToolPartUtf8 { .. })
	));
	assert_eq!(std::fs::read(&path).expect("journal unchanged"), before_file);
	assert_eq!(session.dom().snapshot().as_bytes(), before_dom.as_bytes());

	let second = session
		.call("grep", 1, "call-2", None, Some(raw(serde_json::json!({}))), None)
		.expect("second call");
	let before_file = std::fs::read(&path).expect("journal bytes");
	let before_dom = session.dom().snapshot();
	assert!(
		session
			.fail_projected(second, raw(serde_json::json!({"code":"bad"})), raw(serde_json::json!({})),)
			.is_err()
	);
	assert_eq!(std::fs::read(&path).expect("journal unchanged"), before_file);
	assert_eq!(session.dom().snapshot().as_bytes(), before_dom.as_bytes());
}
