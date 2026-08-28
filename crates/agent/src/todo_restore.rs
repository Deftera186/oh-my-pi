//! Journal-derived recovery of the durable todo snapshot.
//!
//! Every successful `todo@1` outcome and every journaled `/todo` edit carries
//! the complete phased list, so the live todo state is always reconstructible
//! from journal truth alone. [`TodoRestore`] is the [`StatefulComponent`]
//! hosts register when the todo tool is exposed: it scans the journal with
//! [`latest_todo_phases`] and drives one `todo@1` init so the environment
//! executor matches the live prefix.

use bytes::Bytes;
use omp_core::sf;
use omp_env::{EnvClient, InvocationEvent};
use omp_proto::{
	env::v1::{Admission, InvokeTool},
	thread::{v1 as thread, v1::item},
	value_json::value_to_json,
};
use omp_storage::transcript::{Custom, Entry, Kind};

use crate::{
	Journal, JournalError,
	journal_kinds::TODO_EDIT_KIND,
	r#loop::now_ms,
	stateful::{RestoreFuture, StatefulComponent},
};

/// Restores the environment todo slot from journal truth.
///
/// Registered by hosts whose tool roster exposes `todo@1`; a journal with no
/// surviving snapshot clears the slot.
pub struct TodoRestore;

impl StatefulComponent for TodoRestore {
	fn name(&self) -> &'static str {
		"todo"
	}

	fn restore<'a>(&'a self, journal: &'a Journal, env: &'a EnvClient) -> RestoreFuture<'a> {
		Box::pin(async move {
			let phases = match latest_todo_phases(journal) {
				Ok(phases) => phases,
				Err(error) => {
					tracing::warn!(%error, "todo restore journal scan failed");
					return;
				},
			};
			restore_todo_slot(env, phases).await;
		})
	}
}

/// Drives one best-effort `todo@1` init restoring the journal-derived
/// snapshot; an absent snapshot clears the slot.
async fn restore_todo_slot(env: &EnvClient, phases: Option<serde_json::Value>) {
	let list = phases.unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
	let Ok(args) = serde_json::to_vec(&serde_json::json!({"op": "init", "list": list})) else {
		return;
	};
	let invocation_id = sf!("todo-restore-{}", omp_core::Ulid::generate());
	let Ok(mut invocation) = env
		.invoke(InvokeTool {
			invocation_id: invocation_id.to_string(),
			name: "todo".to_owned(),
			rev: "1".to_owned(),
			..Default::default()
		})
		.await
	else {
		tracing::warn!("todo restore invocation was not accepted by the environment");
		return;
	};
	if !matches!(invocation.next_event().await, Ok(Some(InvocationEvent::Accepted(_)))) {
		tracing::warn!("todo restore invocation was not accepted by the environment");
		return;
	}
	if invocation
		.commit_args(Bytes::from(args), Bytes::from_static(b"todo-restore"), now_ms(), None)
		.await
		.is_err()
	{
		tracing::warn!("todo restore argument commit failed");
		return;
	}
	loop {
		match invocation.next_event().await {
			Ok(Some(InvocationEvent::Verdict(verdict))) => {
				if verdict.is_error {
					tracing::warn!("todo restore invocation returned an error verdict");
				}
				return;
			},
			Ok(Some(InvocationEvent::Admission(query))) => {
				// The restore replays journal truth the user already authored;
				// leaving the query unanswered parks the environment gate until
				// its deadline and stalls host startup behind it.
				let allow = Admission {
					invocation_id: query.invocation_id,
					allow: true,
					..Admission::default()
				};
				if invocation.admit(allow).await.is_err() {
					tracing::warn!("todo restore admission answer failed");
					return;
				}
			},
			Ok(Some(_)) => {},
			_ => {
				tracing::warn!("todo restore invocation ended without a verdict");
				return;
			},
		}
	}
}

/// Latest durable todo phases on the live-intent chain, or `None` when a
/// rewind/reset dropped them all (or none were ever recorded).
///
/// The scan is a physical forward fold: rewind pops candidates past the
/// target and reset clears them, while compaction and prompt rewrites are
/// ignored — todo state semantically survives context surgery even though the
/// live-chain projection drops pre-compact events. Undecodable candidates are
/// skipped, never errors.
pub fn latest_todo_phases(journal: &Journal) -> Result<Option<serde_json::Value>, JournalError> {
	let log = journal.load()?;
	let mut candidates: Vec<u64> = Vec::new();
	for index in 0..u64::try_from(log.len()).expect("event indexes fit in u64") {
		let Some(Entry::Ok(event)) = log.get(index) else {
			continue;
		};
		match &event.kind {
			Kind::Item(record) => {
				if matches!(
					&record.item.kind,
					Some(item::Kind::ToolResult(result)) if result.name == "todo"
				) {
					candidates.push(index);
				}
			},
			Kind::Custom(custom) if custom.kind() == TODO_EDIT_KIND => candidates.push(index),
			Kind::Rewind { to } => match to {
				Some(to) => {
					while candidates.last().is_some_and(|last| last > to) {
						candidates.pop();
					}
				},
				None => candidates.clear(),
			},
			Kind::Reset => candidates.clear(),
			_ => {},
		}
	}
	for index in candidates.into_iter().rev() {
		let Some(Entry::Ok(event)) = log.get(index) else {
			continue;
		};
		let phases = match &event.kind {
			Kind::Item(record) => match &record.item.kind {
				Some(item::Kind::ToolResult(result)) => outcome_phases(result),
				_ => None,
			},
			Kind::Custom(custom) => edit_phases(custom),
			_ => None,
		};
		if phases.is_some() {
			return Ok(phases);
		}
	}
	Ok(None)
}

/// Extracts the phased list from a successful durable `todo@1` outcome.
fn outcome_phases(result: &thread::ToolResult) -> Option<serde_json::Value> {
	let details = value_to_json(result.details.as_ref()?)?;
	if details.get("kind")?.as_str()? != "ok" {
		return None;
	}
	details.get("value")?.get("phases").cloned()
}

/// Extracts the phased list from a journaled `/todo` edit snapshot.
fn edit_phases(custom: &Custom) -> Option<serde_json::Value> {
	let data: serde_json::Value = serde_json::from_str(custom.data()?.get()).ok()?;
	data.get("phases").cloned()
}
#[cfg(test)]
pub mod test_support {
	use omp_proto::{
		inference::{v1 as pb, v1::value},
		thread::v1 as thread,
	};

	use super::item;

	fn json_proto_value(value: &serde_json::Value) -> pb::Value {
		let kind = match value {
			serde_json::Value::Null => value::Kind::Null(true),
			serde_json::Value::Bool(boolean) => value::Kind::Bool(*boolean),
			serde_json::Value::String(string) => value::Kind::String(string.clone()),
			serde_json::Value::Number(number) => number.as_i64().map_or_else(
				|| value::Kind::Double(number.as_f64().unwrap_or_default()),
				value::Kind::Int,
			),
			serde_json::Value::Array(values) => value::Kind::List(pb::ValueList {
				values: values.iter().map(json_proto_value).collect(),
			}),
			serde_json::Value::Object(map) => value::Kind::Map(pb::ValueMap {
				fields: map
					.iter()
					.map(|(key, value)| (key.clone(), json_proto_value(value)))
					.collect(),
			}),
		};
		pb::Value { kind: Some(kind) }
	}

	/// Builds the canonical durable item of one successful `todo@1` outcome.
	pub fn todo_outcome_item(phases: &serde_json::Value) -> thread::Item {
		thread::Item {
			kind: Some(item::Kind::ToolResult(thread::ToolResult {
				call_id: "call-todo".to_owned(),
				name: "todo".to_owned(),
				details: Some(json_proto_value(&serde_json::json!({
					"kind": "ok",
					"value": {"phases": phases, "rendered": "rendered"},
				}))),
				..Default::default()
			})),
			..Default::default()
		}
	}
}

#[cfg(test)]
mod tests {
	use std::env;

	use omp_core::{Str, sf};
	use omp_storage::transcript::{Header, SessionId};

	use super::{latest_todo_phases, test_support::todo_outcome_item};
	use crate::{Journal, journal::Compact};

	fn journal(name: &str) -> (Journal, std::path::PathBuf) {
		let path = env::temp_dir().join(format!(
			"omp-agent-todo-restore-{name}-{}-{}.jsonl",
			std::process::id(),
			omp_core::Ulid::generate()
		));
		let journal = Journal::create(&path, &Header {
			v:       4,
			id:      SessionId(Str::new(name)),
			created: 1,
			cwd:     env::temp_dir(),
		})
		.expect("create test journal");
		(journal, path)
	}

	fn phases(text: &str) -> serde_json::Value {
		serde_json::json!([{"phase": "Build", "items": [{"text": text, "status": "pending"}]}])
	}

	#[test]
	fn latest_snapshot_wins_and_rewind_resurrects_the_earlier_one() {
		let (mut journal, path) = journal("latest-wins");
		let first = journal
			.append_optimistic(1, todo_outcome_item(&phases("one")), None)
			.expect("first outcome");
		journal
			.append_optimistic(2, todo_outcome_item(&phases("two")), None)
			.expect("second outcome");
		assert_eq!(latest_todo_phases(&journal).expect("scan"), Some(phases("two")));
		journal.truncate_to(3, Some(first)).expect("rewind");
		assert_eq!(latest_todo_phases(&journal).expect("scan"), Some(phases("one")));
		drop(journal);
		std::fs::remove_file(path).expect("remove journal");
	}

	#[test]
	fn rewind_to_root_and_reset_drop_every_snapshot() {
		let (mut journal, path) = journal("root-and-reset");
		journal
			.append_optimistic(1, todo_outcome_item(&phases("one")), None)
			.expect("outcome");
		journal.truncate_to(2, None).expect("rewind to root");
		assert_eq!(latest_todo_phases(&journal).expect("scan"), None);
		journal
			.append_optimistic(3, todo_outcome_item(&phases("two")), None)
			.expect("outcome");
		journal.reset(4).expect("reset");
		assert_eq!(latest_todo_phases(&journal).expect("scan"), None);
		drop(journal);
		std::fs::remove_file(path).expect("remove journal");
	}

	#[test]
	fn newer_todo_edit_beats_older_tool_outcome() {
		let (mut journal, path) = journal("edit-beats-outcome");
		journal
			.append_optimistic(1, todo_outcome_item(&phases("tool")), None)
			.expect("outcome");
		let edited = phases("edited");
		let raw = serde_json::value::to_raw_value(&edited).expect("raw phases");
		journal.todo_edit(2, &raw).expect("journal todo edit");
		assert_eq!(latest_todo_phases(&journal).expect("scan"), Some(edited));
		drop(journal);
		std::fs::remove_file(path).expect("remove journal");
	}

	#[test]
	fn compaction_between_outcome_and_head_keeps_state() {
		let (mut journal, path) = journal("compact-survives");
		journal
			.append_optimistic(1, todo_outcome_item(&phases("kept")), None)
			.expect("outcome");
		let tail = journal
			.append_optimistic(2, todo_outcome_item(&phases("kept")), None)
			.expect("tail item");
		journal
			.compact(3, Compact {
				summary:       sf!("summary"),
				short:         None,
				first_kept:    tail,
				tokens_before: 100,
				tokens_after:  Some(10),
				method:        None,
				warning:       None,
				snapcompact:   None,
				superseded:    Vec::new(),
			})
			.expect("compact");
		assert_eq!(latest_todo_phases(&journal).expect("scan"), Some(phases("kept")));
		drop(journal);
		std::fs::remove_file(path).expect("remove journal");
	}
}
