//! P6: a killed mid-turn writer can lose only its torn tail; replay equals
//! committed truth.

#![cfg(unix)]

use std::time::Duration;

use omp_dom::{PropId, PropKey};
use omp_e2e::support::{OwnedProcess, create_session, reopen_session, within};
use tokio::{process::Command, time};

#[tokio::test]
async fn p6_killed_mid_turn_writer_replays_last_committed_snapshot_and_truncates_tail() {
	let temp = tempfile::tempdir().expect("P6 scratch");
	let path = temp.path().join("crash.oms");
	let mut live = create_session(&path).expect("session");
	live.begin_turn().expect("turn");
	live.user("stream before crash", Vec::new()).expect("user");
	live
		.assistant_start("model", "provider", "route")
		.expect("assistant");
	let assistant = live
		.dom()
		.select("body turn assistant")
		.expect("selector")
		.next()
		.expect("assistant handle");
	let sid = live
		.stream_open(assistant, PropKey::from(PropId::Text))
		.expect("stream");
	live.stream_append(sid, "committed prefix").expect("append");
	let committed = live.dom().snapshot();
	let committed_len = std::fs::metadata(&path).expect("metadata").len();
	drop(live);

	let marker = temp.path().join("writer-ready");
	let mut command = Command::new("/bin/sh");
	command.args([
		"-c",
		"printf 'event: stream@1\\nid: 01Torn' >> \"$1\"; : > \"$2\"; sleep 60",
		"p6-writer",
		path.to_str().expect("UTF-8 journal path"),
		marker.to_str().expect("UTF-8 marker path"),
	]);
	let writer = OwnedProcess::spawn(command).expect("spawn crashing writer");
	within("partial frame becomes durable", Duration::from_secs(3), async {
		while !marker.exists() {
			time::sleep(Duration::from_millis(5)).await;
		}
	})
	.await
	.expect("writer marker");
	assert!(std::fs::metadata(&path).expect("torn metadata").len() > committed_len);
	writer
		.terminate(Duration::from_millis(20))
		.await
		.expect("kill mid-turn writer");

	let replay = reopen_session(&path).expect("replay truncates torn tail");
	assert_eq!(replay.dom().snapshot(), committed);
	drop(replay);
	assert_eq!(std::fs::metadata(&path).expect("recovered metadata").len(), committed_len);
	let second = reopen_session(&path).expect("second replay");
	assert_eq!(second.dom().snapshot(), committed);
}

#[test]
fn p6_resume_preserves_open_stream_prefix_without_inventing_completion() {
	let temp = tempfile::tempdir().expect("P6 scratch");
	let path = temp.path().join("open-stream.oms");
	let mut live = create_session(&path).expect("session");
	live.begin_turn().expect("turn");
	live.user("resume", Vec::new()).expect("user");
	live
		.assistant_start("model", "provider", "route")
		.expect("assistant");
	let assistant = live
		.dom()
		.select("body turn assistant")
		.expect("selector")
		.next()
		.expect("assistant handle");
	let sid = live
		.stream_open(assistant, PropKey::from(PropId::Text))
		.expect("stream");
	live.stream_append(sid, "visible").expect("append");
	let expected = live.dom().snapshot();
	drop(live);
	let replay = reopen_session(&path).expect("resume");
	assert_eq!(replay.dom().snapshot(), expected);
	let journal = std::fs::read_to_string(path).expect("journal");
	assert!(!journal.contains("event: msg.assistant.end@1"));
	assert!(!journal.contains("event: turn.receipt@1"));
}
