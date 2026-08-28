//! Round-trip tests for transcript persistence.

use std::{
	collections::BTreeMap,
	fs,
	fs::OpenOptions,
	io::{Seek as _, SeekFrom, Write as _},
	path::PathBuf,
	str,
};

use bytes::Bytes;
use omp_core::{ArtifactDigest, Hash32, InvocationPhase, Principal, Provenance, Str, sf};
use omp_proto::{
	inference::v1 as pb,
	thread::v1::{self as thread_pb, item, part, server_tool},
};
use omp_storage::{
	blob::BlobRef,
	transcript::{
		AmendPatch, Attribution, Block, BlockKind, CallId, ChildLifecycleEntry, ChildSessionInit,
		ChildWorkspaceIdentity, CtxSnapshot, Custom, DialectId, Entry, EntryUndecodable, Error,
		Event, FeatureId, Header, InvocationTransition, ItemRecord, Kind, LiveSet, ModelChange,
		ModelId, ModelRef, Msg, Patch, Pin, PromptRewriteCommit, PromptRewriteIntent,
		PromptRewriteStage, ProviderId, Reader, RefreshState, Replay, RequestAudit, RequestError,
		SessionId, Stop, ThinkingSel, Timing, TitleSource, ToolBatchAuthorized, TurnInputItem,
		TurnInputRecord, TurnOptionsRecord, TurnReceipt, TurnStart, Usage, UserBlock, Writer, load,
		read_line, write_header, write_line,
	},
};
use omp_tool::{CallOutcome, CallOutcomeDetails};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tempfile::tempdir;

fn text(value: &str) -> Str {
	Str::new(value)
}

fn raw(value: &str) -> Box<RawValue> {
	RawValue::from_string(value.to_owned()).expect("valid raw JSON")
}

const fn blob(byte: u8, size: u64) -> BlobRef {
	BlobRef { hash: Hash32::new([byte; 32]), size }
}

fn principal() -> Principal {
	Principal::new(text("os:test"), text("Test User"))
}

fn provenance() -> Provenance {
	Provenance::new(
		text("publisher-key"),
		text("com.example.test"),
		text("1.2.3"),
		ArtifactDigest::new([0x42; 32]),
		text("workspace"),
		text("sandboxed"),
		7,
	)
}

fn custom(kind: &str, data: Option<Box<RawValue>>, display: bool) -> Custom {
	Custom::new(
		text(kind),
		Some(text("schema.2")),
		Some(text("worker")),
		principal(),
		provenance(),
		data,
		None,
		display,
	)
	.expect("custom data is valid")
}

fn model() -> ModelRef {
	ModelRef {
		provider: ProviderId(text("provider")),
		api:      text("responses"),
		model:    ModelId(text("model")),
	}
}

fn header() -> Header {
	Header {
		v:       4,
		id:      SessionId(text("session")),
		created: 123,
		cwd:     PathBuf::from("/tmp/work"),
	}
}

fn title(ts: u64, value: &str) -> Event {
	Event { ts, kind: Kind::Title { title: text(value), source: TitleSource::User } }
}

fn every_kind() -> Vec<Event> {
	let mut replay_fields = BTreeMap::new();
	replay_fields.insert(text("id"), raw(r#"{ "z":2, "a":1 }"#));
	let assistant = Msg::Assistant {
		content:     vec![Block {
			kind: BlockKind::Tool {
				id:   CallId(text("call")),
				name: text("tool"),
				wire: Some(text("wire_tool")),
				args: text(r#"{"b":2,"a":1}"#),
			},
			re:   Some(Replay { p: DialectId(text("oai")), f: replay_fields }),
		}],
		model:       model(),
		stop:        Stop::ToolUse,
		usage:       Usage { input: 10, output: 2, cache_read: 3, cache_write: 4 },
		response_id: Some(text("response")),
		upstream:    Some(text("route")),
		ctx:         Some(CtxSnapshot { tokens: 19, limit: 100 }),
		timing:      Timing { duration_ms: 50, ttft_ms: 10 },
		disabled:    vec![FeatureId(text("search"))],
	};
	vec![
		Event {
			ts:   1,
			kind: Kind::Init {
				system_prompt: blob(1, 8),
				tools:         vec![text("read"), text("write")],
				agent:         Some(text("worker")),
				output_schema: Some(raw(r#"{ "required" : ["x"], "type":"object" }"#)),
				revival:       Some(ChildSessionInit {
					display_name:        Str::default(),
					parent_id:           Str::default(),
					definition:          sf!("reviewer"),
					depth:               2,
					prompt_ref:          blob(2, 80),
					schema_ref:          Some(blob(3, 40)),
					policy_snapshot_ref: blob(4, 20),
					grant_snapshot_ref:  blob(5, 20),
					tool_snapshot_ref:   blob(6, 20),
					model_role:          sf!("subagent:worker"),
					workspace:           ChildWorkspaceIdentity {
						root_uri:     sf!("env://workspace/worker"),
						isolation_id: Some(sf!("iso-worker")),
						revision:     Some(Hash32::new([7; 32])),
					},
					serving_model:       Some(model()),
				}),
			},
		},
		Event {
			ts:   2,
			kind: Kind::ChildLifecycle(ChildLifecycleEntry {
				child_id:        sf!("worker"),
				generation:      0,
				init_event:      0,
				lifecycle:       sf!("running"),
				terminal_status: None,
			}),
		},
		Event {
			ts:   2,
			kind: Kind::Msg(Msg::User {
				content:     vec![UserBlock::Text { text: text("hello") }],
				synthetic:   false,
				steering:    false,
				attribution: Some(Attribution { source: text("human"), id: None }),
			}),
		},
		Event { ts: 3, kind: Kind::Msg(assistant) },
		Event {
			ts:   4,
			kind: Kind::Failed {
				error: RequestError {
					message: text("failed"),
					code:    Some(text("bad_request")),
					status:  Some(400),
					details: Some(raw(r#"{"b": 2, "a":1}"#)),
				},
				model: model(),
				usage: Some(Usage::default()),
			},
		},
		Event {
			ts:   5,
			kind: Kind::Infer {
				thinking: Patch::Set(ThinkingSel {
					effective:  text("high"),
					configured: text("auto"),
				}),
				model:    Patch::Set(ModelChange {
					role:     text("primary"),
					model:    model(),
					fallback: false,
				}),
				tier:     Patch::Clear,
				cred_pin: Patch::Set(Pin {
					provider: ProviderId(text("provider")),
					affinity: text("affinity"),
				}),
			},
		},
		Event { ts: 6, kind: Kind::Rewind { to: Some(1) } },
		Event {
			ts:   7,
			kind: Kind::Compact {
				snapcompact:   None,
				summary:       text("summary"),
				short:         Some(text("short")),
				first_kept:    2,
				tokens_before: 99,
				tokens_after:  Some(21),
				method:        Some(text("remote")),
				warning:       Some(text("warning")),
				superseded:    Vec::new(),
			},
		},
		Event { ts: 8, kind: Kind::Branch { from: 1, summary: text("branch") } },
		Event { ts: 9, kind: Kind::Reset },
		Event { ts: 9, kind: Kind::ProviderReset },
		Event { ts: 9, kind: Kind::MoveRoot { root: PathBuf::from("/future-root") } },
		Event {
			ts:   10,
			kind: Kind::Title { title: text("title"), source: TitleSource::Assistant },
		},
		Event { ts: 11, kind: Kind::AddDirs { dirs: vec![PathBuf::from("/tmp/other")] } },
		Event { ts: 12, kind: Kind::RemoveDirs { dirs: vec![PathBuf::from("/tmp/other")] } },
		Event {
			ts:   12,
			kind: Kind::ForkedFrom { session: SessionId(text("parent")), at: Some(4) },
		},
		Event {
			ts:   13,
			kind: Kind::NativeCheckpoint {
				provider: ProviderId(text("provider")),
				model:    ModelId(text("model")),
				items:    blob(2, 16),
			},
		},
		Event { ts: 14, kind: Kind::Aborted { tool_call_ids: vec![CallId(text("call"))] } },
		Event {
			ts:   15,
			kind: Kind::Amend { target: 2, patch: AmendPatch::Prune { keep_blocks: 1 } },
		},
		Event { ts: 16, kind: Kind::Label { target: 2, label: Some(text("good")) } },
		Event {
			ts:   17,
			kind: Kind::Custom(
				Custom::new(
					text("extension"),
					Some(text("schema.2")),
					Some(text("worker")),
					principal(),
					provenance(),
					Some(raw(r#"{ "z" : [3,2,1], "a":"x&y" }"#)),
					Some(vec![UserBlock::Image { blob: blob(3, 32) }]),
					true,
				)
				.expect("custom data"),
			),
		},
		Event {
			ts:   18,
			kind: Kind::Item(ItemRecord {
				item:        thread_pb::Item {
					seq:           0,
					created_at_ms: 18,
					kind:          Some(thread_pb::item::Kind::Message(thread_pb::Message {
						role: thread_pb::Role::User as i32,
						parts: vec![thread_pb::Part {
							kind: Some(thread_pb::part::Kind::Text("canonical".to_owned())),
						}],
						..Default::default()
					})),
					props:         None,
				},
				turn_id:     Some(text("turn-1")),
				prompt_hash: Some(Hash32::new([4; 32])),
			}),
		},
		Event { ts: 19, kind: Kind::Amend { target: 17, patch: AmendPatch::Seq { seq: 7 } } },
		Event {
			ts:   20,
			kind: Kind::TurnStart(TurnStart {
				turn_id:            text("turn-1"),
				item_events:        vec![17],
				prompt_hash:        Hash32::new([4; 32]),
				prompt_head_events: vec![17],
				toolset_hash:       Hash32::new([5; 32]),
				enabled_tools:      Vec::new(),
				sequence_targets:   vec![17],
				input:              TurnInputRecord::Delta {
					context: pb::ContextRef {
						context_id: "context".to_owned(),
						expected:   Some(thread_pb::Revision { head: 6, token: vec![8; 32].into() }),
					},
					delta:   pb::ThreadDelta { truncate_to: None, append: Vec::new() },
				},
				options:            TurnOptionsRecord {
					context_id: None,
					params:     pb::ChatParams::default(),
					executor:   None,
					props:      None,
				},
			}),
		},
		Event {
			ts:   21,
			kind: Kind::TurnReceipt(TurnReceipt {
				turn_id:            text("turn-1"),
				prompt_hash:        Hash32::new([4; 32]),
				prompt_head_events: vec![17],
				item_events:        vec![17],
				outcome:            pb::Outcome {
					output: vec![thread_pb::Item {
						seq:           7,
						created_at_ms: 18,
						kind:          Some(thread_pb::item::Kind::Message(thread_pb::Message {
							role: thread_pb::Role::Assistant as i32,
							parts: vec![thread_pb::Part {
								kind: Some(thread_pb::part::Kind::Text("done".to_owned())),
							}],
							..Default::default()
						})),
						props:         None,
					}],
					stop: pb::StopReason::StopEndTurn as i32,
					revision: Some(thread_pb::Revision { head: 7, token: vec![9; 32].into() }),
					provider: "fixture".to_owned(),
					model: "fixture-model".to_owned(),
					..Default::default()
				},
			}),
		},
		Event {
			ts:   22,
			kind: Kind::TurnInput(TurnInputItem {
				turn_id:     text("turn-2"),
				item:        thread_pb::Item {
					seq:           0,
					created_at_ms: 22,
					kind:          Some(thread_pb::item::Kind::Message(thread_pb::Message {
						role: thread_pb::Role::User as i32,
						parts: vec![thread_pb::Part {
							kind: Some(thread_pb::part::Kind::Text("next".to_owned())),
						}],
						..Default::default()
					})),
					props:         None,
				},
				prompt_hash: Some(Hash32::new([4; 32])),
			}),
		},
		Event {
			ts:   23,
			kind: Kind::ToolBatchAuthorized(ToolBatchAuthorized {
				turn_id:  text("turn-1"),
				call_ids: vec![text("call-1")],
			}),
		},
		Event {
			ts:   24,
			kind: Kind::RequestAudit(RequestAudit {
				request_id:         text("attempt-1"),
				idempotency_key:    text("logical-1"),
				extension_id:       text("com.example.test"),
				host_generation:    7,
				session_generation: 3,
				operation:          text("append_atomic"),
				indexes:            vec![16, 17].into(),
			}),
		},
		Event {
			ts:   25,
			kind: Kind::InvocationTransition(InvocationTransition {
				invocation_id:        text("invocation-1"),
				call_id:              CallId(text("call-1")),
				phase:                InvocationPhase::ArgsFinalized,
				requested_args:       Some(raw(r#"{"path":"src"}"#)),
				transformations:      None,
				effective_args:       None,
				admission_receipt:    None,
				assistant_item_event: None,
				effect_token:         None,
				effects:              None,
				authorized_at:        None,
				outcome:              None,
			}),
		},
		Event {
			ts:   26,
			kind: Kind::EntryUndecodable(EntryUndecodable {
				kind:   Some(text("alien")),
				rev:    None,
				value:  None,
				raw:    raw(r#"{"ts":26,"k":"alien","foreign":true}"#),
				reason: text("unknown event kind `alien`"),
			}),
		},
	]
}

#[test]
fn unknown_line_is_byte_verbatim() {
	let source = br#"{  "foreign" : "a&b", "z" : [3, 2], "ts" : 77, "k" : "alien" }"#;
	let event = read_line(source).expect("foreign object is readable");
	let mut encoded = Vec::new();
	write_line(&event, &mut encoded).expect("foreign object is writable");
	assert_eq!(encoded.as_slice(), source);
}

#[test]
fn unknown_record_is_typed_and_addressable() {
	let source = br#"{"ts":77,"k":"future_machine_record","rev":"future.9","payload":1}"#;
	let event = read_line(source).expect("valid unknown JSON is retained");
	let Kind::EntryUndecodable(entry) = &event.kind else {
		panic!("unknown record must remain typed");
	};
	assert_eq!(entry.kind.as_ref().map(Str::as_str), Some("future_machine_record"));
	assert_eq!(entry.rev.as_ref().map(Str::as_str), Some("future.9"));
	assert!(entry.value.is_none());
	assert_eq!(entry.raw.get().as_bytes(), source);
}

#[test]
fn unknown_amendment_is_inert_and_round_trips_verbatim() {
	let source = br#"{"ts":77,"k":"amend","target":4,"patch":{"op":"future_amend","data":{"x":1}}}"#;
	let event = read_line(source).expect("future amendment is readable");
	let Kind::Amend { patch: AmendPatch::Unknown(raw), .. } = &event.kind else {
		panic!("future amendment remains an inert raw operation");
	};
	assert_eq!(raw.get(), r#"{"op":"future_amend","data":{"x":1}}"#);
	let mut encoded = Vec::new();
	write_line(&event, &mut encoded).expect("future amendment is writable");
	let decoded = read_line(&encoded).expect("rewritten amendment is readable");
	assert_eq!(decoded, event);
	let Kind::Amend { patch: AmendPatch::Unknown(raw), .. } = decoded.kind else {
		panic!("future amendment remains inert after round trip");
	};
	assert_eq!(raw.get(), r#"{"op":"future_amend","data":{"x":1}}"#);
}

#[test]
fn corrupt_known_record_preserves_exact_bytes() {
	let source = br#"{"ts":8,"k":"custom","kind":"memo","rev":"m.1","data":{"x":1}}"#;
	let event = read_line(source).expect("corrupt machine record remains addressable");
	assert!(matches!(&event.kind, Kind::EntryUndecodable(entry) if entry.value.is_none()));
	let mut rewritten = Vec::new();
	write_line(&event, &mut rewritten).expect("corrupt record rewrites");
	assert_eq!(rewritten, source);
}

#[test]
fn noncanonical_machine_record_is_not_charitably_repaired() {
	let source = br#"{ "ts":8,"k":"reset"}"#;
	let event = read_line(source).expect("noncanonical JSON remains addressable");
	let Kind::EntryUndecodable(entry) = &event.kind else {
		panic!("noncanonical machine truth must not decode");
	};
	assert!(entry.value.is_none());
	assert_eq!(entry.raw.get().as_bytes(), source);
}

#[test]
fn invocation_transition_rejects_facts_from_another_phase() {
	let source = br#"{"ts":9,"k":"invocation_transition","invocation_id":"inv-1","call_id":"call-1","phase":"ARGS_FINALIZED","requested_args":{"x":1},"transformations":null,"effective_args":null,"admission_receipt":null,"assistant_item_event":null,"effect_token":"forged","authorized_at":null,"outcome":null}"#;
	let event = read_line(source).expect("invalid transition is preserved");
	let Kind::EntryUndecodable(entry) = &event.kind else {
		panic!("phase-inconsistent transition must not decode");
	};
	assert!(entry.value.is_none());
	assert_eq!(entry.raw.get().as_bytes(), source);
}

#[test]
fn every_invocation_phase_accepts_only_its_fixed_facts() {
	for phase in InvocationPhase::ALL {
		let mut transition = InvocationTransition {
			invocation_id: text("inv-1"),
			call_id: CallId(text("call-1")),
			phase,
			requested_args: None,
			transformations: None,
			effective_args: None,
			admission_receipt: None,
			assistant_item_event: None,
			effect_token: None,
			effects: None,
			authorized_at: None,
			outcome: None,
		};
		match phase {
			InvocationPhase::Open | InvocationPhase::Admission => {},
			InvocationPhase::ArgsFinalized => {
				transition.requested_args = Some(raw(r#"{"path":"src"}"#));
			},
			InvocationPhase::Admitted => {
				transition.transformations = Some(Default::default());
				transition.effective_args = Some(raw(r#"{"path":"src"}"#));
				transition.admission_receipt = Some(raw(r#"{"decision":"allow"}"#));
			},
			InvocationPhase::AssistantItemCommitted => {
				transition.assistant_item_event = Some(42);
			},
			InvocationPhase::EffectsAuthorized => {
				transition.effect_token = Some(text("scoped-token"));
				transition.effects = Some(omp_tool::Effects::empty());
				transition.authorized_at = Some(1_700_000_000_000);
			},
			InvocationPhase::Settled => {
				transition.outcome = Some(CallOutcome::Ok(CallOutcomeDetails::Inline {
					json: Bytes::from_static(br#"{"ok":true}"#),
				}));
			},
		}
		transition.validate().expect("phase facts are valid");
		let event = Event { ts: 9, kind: Kind::InvocationTransition(transition) };
		let mut encoded = Vec::new();
		write_line(&event, &mut encoded).expect("transition writes");
		assert_eq!(read_line(&encoded).expect("transition reads"), event);
	}
}

#[test]
fn custom_revision_uses_tool_attribution_key() {
	let entry = custom("memo", None, false);
	assert_eq!(entry.rev_attribution(), Some((omp_tool::TOOL_REV_PROP, "schema.2")));
}

#[test]
fn custom_payload_does_not_determine_kind_size() {
	assert!(std::mem::size_of::<Custom>() < std::mem::size_of::<TurnStart>());
}

#[test]
fn custom_data_is_canonical_and_stable() {
	let event = Event {
		ts:   4,
		kind: Kind::Custom(custom("raw", Some(raw(r#"{ "z" : [3, 2], "a" : "x&y" }"#)), false)),
	};
	let mut first = Vec::new();
	write_line(&event, &mut first).expect("custom event writes");
	assert!(
		str::from_utf8(&first)
			.expect("JSON is UTF-8")
			.contains(r#"{"z":[3,2],"a":"x&y"}"#)
	);
	let decoded = read_line(&first).expect("custom event reads");
	let mut second = Vec::new();
	write_line(&decoded, &mut second).expect("custom event rewrites");
	assert_eq!(second, first);
}

#[test]
fn transcript_hashes_are_lowercase_hex_strings() {
	let event = Event {
		ts:   1,
		kind: Kind::PromptRewriteIntent(PromptRewriteIntent {
			prompt_hash:    Hash32::new([7; 32]),
			head:           Vec::new(),
			preserved_tail: Vec::new(),
		}),
	};
	let mut encoded = Vec::new();
	write_line(&event, &mut encoded).expect("prompt rewrite writes");
	assert_eq!(
		encoded,
		br#"{"ts":1,"k":"prompt_rewrite_intent","prompt_hash":"0707070707070707070707070707070707070707070707070707070707070707","head":[],"preserved_tail":[]}"#
	);
}

#[test]
fn every_event_kind_is_idempotent() {
	for event in every_kind() {
		let mut first = Vec::new();
		write_line(&event, &mut first).expect("event writes");
		let decoded = read_line(&first).expect("event reads");
		assert_eq!(decoded, event);
		let mut second = Vec::new();
		write_line(&decoded, &mut second).expect("event rewrites");
		assert_eq!(second, first);
	}
}

#[test]
fn header_is_single_and_torn_tail_is_truncated() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("new transcript");
	assert!(matches!(writer.write_header(&header()), Err(Error::DuplicateHeader)));
	let mut duplicate = Vec::new();
	write_header(&header(), &mut duplicate).expect("duplicate header encodes");
	let duplicate = Event {
		ts:   0,
		kind: Kind::EntryUndecodable(EntryUndecodable {
			kind:   None,
			rev:    None,
			value:  None,
			raw:    raw(str::from_utf8(&duplicate).expect("header is UTF-8")),
			reason: text("header in event position"),
		}),
	};
	assert!(matches!(writer.append(&duplicate), Err(Error::DuplicateHeader)));
	assert_eq!(writer.append(&title(1, "first")).expect("first event"), 0);
	drop(writer);

	let mut file = OpenOptions::new()
		.append(true)
		.open(&path)
		.expect("append torn fragment");
	file
		.write_all(br#"{"ts":2,"k":"title","title":"tor"#)
		.expect("write torn fragment");
	drop(file);

	let mut writer = Writer::open_append(&path).expect("repair torn tail");
	assert_eq!(writer.append(&title(3, "second")).expect("second event"), 1);
	drop(writer);
	let log = load(&path).expect("repaired transcript loads");
	assert_eq!(log.len(), 2);
}

#[test]
fn malformed_trailing_record_is_preserved_at_its_physical_index() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("new transcript");
	assert_eq!(writer.append(&title(1, "first")).expect("first event"), 0);
	drop(writer);

	let mut file = OpenOptions::new()
		.append(true)
		.open(&path)
		.expect("append malformed record");
	file
		.write_all(b"{not json}\n")
		.expect("write malformed record");
	drop(file);

	let mut writer = Writer::open_append(&path).expect("repair malformed tail");
	assert_eq!(writer.append(&title(2, "second")).expect("second event"), 2);
	drop(writer);

	let log = load(&path).expect("repaired transcript loads");
	assert_eq!(log.len(), 3);
	assert!(matches!(log.get(1), Some(Entry::Tombstone(_))));
	assert!(matches!(
		log.get(2),
		Some(Entry::Ok(event)) if matches!(&event.kind, Kind::Title { title, .. } if title.as_str() == "second")
	));
}

#[test]
fn malformed_middle_line_is_a_tombstone() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut bytes = Vec::new();
	write_header(&header(), &mut bytes).expect("header writes");
	bytes.extend_from_slice(b"\n{\"ts\":1,\"k\":\"reset\"}\n{not json}\n{\"ts\":3,\"k\":\"title\",\"title\":\"later\",\"source\":\"user\"}\n");
	fs::write(&path, bytes).expect("fixture writes");
	let log = load(&path).expect("fixture loads");
	assert_eq!(log.len(), 3);
	assert!(matches!(log.get(1), Some(Entry::Tombstone(_))));
	assert!(matches!(
		log.get(2),
		Some(Entry::Ok(event)) if matches!(&event.kind, Kind::Title { title, .. } if title.as_str() == "later")
	));
}

#[test]
fn oversized_tool_search_history_round_trips_byte_for_byte() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let oversized = "W".repeat(600_000);
	let server_tool = |kind: server_tool::Kind, payload: Vec<u8>| thread_pb::Part {
		kind: Some(part::Kind::ServerTool(thread_pb::ServerTool {
			provider:          "anthropic".to_owned(),
			kind:              kind as i32,
			id:                "srvtoolu_tool_search".to_owned(),
			name:              "tool_search_tool_bm25".to_owned(),
			payload_json:      payload.into(),
			provider_metadata: None,
		})),
	};
	let event = Event {
		ts:   1,
		kind: Kind::Item(ItemRecord {
			item:        thread_pb::Item {
				seq:           0,
				created_at_ms: 1,
				kind:          Some(item::Kind::Message(thread_pb::Message {
					role:  thread_pb::Role::Assistant as i32,
					parts: vec![
						server_tool(
							thread_pb::server_tool::Kind::Call,
							br#"{"query":"read"}"#.to_vec(),
						),
						server_tool(
							thread_pb::server_tool::Kind::Result,
							format!(
								r#"{{"type":"tool_search_tool_search_result","tool_references":[{{"type":"tool_reference","tool_name":"READ_TOOL_{oversized}"}}]}}"#
							)
							.into_bytes(),
						),
					],
					..Default::default()
				})),
				props:         None,
			},
			turn_id:     Some(text("turn-1")),
			prompt_hash: None,
		}),
	};
	let mut writer = Writer::create(&path, &header()).expect("new transcript");
	assert_eq!(writer.append(&event).expect("server-tool item appends"), 0);
	drop(writer);

	let log = load(&path).expect("transcript loads");
	let Some(Entry::Ok(loaded)) = log.get(0) else {
		panic!("server-tool item survives reload as a decoded event");
	};
	assert_eq!(**loaded, event);
	let mut expected = Vec::new();
	write_line(&event, &mut expected).expect("server-tool item encodes");
	let mut reencoded = Vec::new();
	write_line(loaded, &mut reencoded).expect("loaded server-tool item re-encodes");
	assert_eq!(reencoded, expected, "tool-search history must persist byte-for-byte");

	let mut writer = Writer::open_append(&path).expect("reopen for append");
	assert_eq!(writer.append(&title(2, "after")).expect("subsequent event"), 1);
	drop(writer);
	let log = load(&path).expect("reopened transcript loads");
	assert!(matches!(
		log.get(0),
		Some(Entry::Ok(kept)) if **kept == event
	));
}

#[test]
fn forward_fold_applies_rewind_reset_and_compact() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("new transcript");
	writer.append(&title(1, "zero")).expect("event zero");
	writer.append(&title(2, "one")).expect("event one");
	writer
		.append(&Event { ts: 3, kind: Kind::Rewind { to: Some(0) } })
		.expect("rewind");
	writer.append(&title(4, "three")).expect("event three");
	writer
		.append(&Event { ts: 5, kind: Kind::Reset })
		.expect("reset");
	writer.append(&title(6, "five")).expect("event five");
	writer.append(&title(7, "six")).expect("event six");
	writer
		.append(&Event {
			ts:   8,
			kind: Kind::Compact {
				snapcompact:   None,
				summary:       text("summary"),
				short:         None,
				first_kept:    5,
				tokens_before: 50,
				tokens_after:  None,
				method:        None,
				warning:       None,
				superseded:    Vec::new(),
			},
		})
		.expect("compact");
	drop(writer);
	let log = load(&path).expect("transcript loads");
	assert_eq!(log.live(), vec![7, 5, 6]);
	let mut live = LiveSet::new();
	log.live_into(&mut live);
	assert!(live.iter().eq([7, 5, 6]));
}

#[test]
fn reusable_live_set_matches_live_vectors_for_navigation_and_rewrite() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("new transcript");
	writer.append(&title(1, "zero")).expect("event zero");
	writer.append(&title(2, "one")).expect("event one");
	writer
		.append(&Event { ts: 3, kind: Kind::Rewind { to: Some(0) } })
		.expect("rewind");
	writer.append(&title(4, "three")).expect("event three");
	writer
		.append(&Event {
			ts:   5,
			kind: Kind::Compact {
				snapcompact:   None,
				summary:       text("summary"),
				short:         None,
				first_kept:    3,
				tokens_before: 20,
				tokens_after:  None,
				method:        None,
				warning:       None,
				superseded:    Vec::new(),
			},
		})
		.expect("compact");
	let replacement = thread_pb::Item::default();
	writer
		.append(&Event {
			ts:   6,
			kind: Kind::PromptRewriteIntent(PromptRewriteIntent {
				prompt_hash:    Hash32::new([7; 32]),
				head:           vec![replacement.clone()],
				preserved_tail: vec![3],
			}),
		})
		.expect("rewrite intent");
	writer
		.append(&Event {
			ts:   7,
			kind: Kind::PromptRewriteStage(PromptRewriteStage {
				intent:  5,
				ordinal: 0,
				item:    replacement,
			}),
		})
		.expect("rewrite stage");
	writer
		.append(&Event {
			ts:   8,
			kind: Kind::PromptRewriteCommit(PromptRewriteCommit {
				intent:      5,
				head_events: vec![6],
			}),
		})
		.expect("rewrite commit");
	drop(writer);

	let log = load(&path).expect("transcript loads");
	assert_eq!(log.live(), vec![6, 3]);
	let mut live = LiveSet::new();
	log.live_into(&mut live);
	assert!(live.iter().eq([6, 3]));
	assert!(live.contains(6));
	assert!(live.contains(3));
	assert!(!live.contains(4));

	assert!(log.live_through_into(4, &mut live));
	assert!(live.iter().eq([4, 3]));
	let before_absent = live.clone();
	assert!(!log.live_through_into(8, &mut live));
	assert_eq!(live, before_absent);
}

#[test]
fn unchanged_reader_refresh_reuses_projection_capacity() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("new transcript");
	writer.append(&title(1, "first")).expect("first event");
	drop(writer);
	let mut reader = Reader::open(&path).expect("reader opens");
	let bit_capacity = reader.live().capacity();
	let chain_capacity = reader.live().chain_capacity();
	let append_offset = reader.append_offset();

	let report = reader.refresh().expect("unchanged refresh succeeds");
	assert_eq!(report.state, RefreshState::Unchanged);
	assert_eq!(report.next_index, 1);
	assert_eq!(report.append_offset, append_offset);
	assert_eq!(report.tail_bytes, 0);
	assert_eq!(reader.live().capacity(), bit_capacity);
	assert_eq!(reader.live().chain_capacity(), chain_capacity);
}

#[test]
fn reader_parses_only_new_complete_lines() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("new transcript");
	writer.append(&title(1, "first")).expect("first event");
	drop(writer);
	let mut reader = Reader::open(&path).expect("reader opens");

	let bytes = fs::read(&path).expect("transcript reads");
	let title_offset = bytes
		.windows(b"first".len())
		.position(|window| window == b"first")
		.expect("first title is present");
	let mut file = OpenOptions::new()
		.write(true)
		.open(&path)
		.expect("transcript opens");
	file
		.seek(SeekFrom::Start(u64::try_from(title_offset).expect("file offset fits in u64")))
		.expect("prior line seek succeeds");
	file
		.write_all(b"other")
		.expect("same-size prior-line mutation writes");
	drop(file);
	let mut writer = Writer::open_append(&path).expect("writer reopens");
	writer.append(&title(2, "second")).expect("second event");
	drop(writer);

	let report = reader.refresh().expect("append refresh succeeds");
	assert_eq!(report.state, RefreshState::Advanced { records: 1 });
	assert_eq!(report.next_index, 2);
	assert!(matches!(
		reader.log().get(0),
		Some(Entry::Ok(event))
			if matches!(&event.kind, Kind::Title { title, .. } if title.as_str() == "first")
	));
	assert!(matches!(
		reader.log().get(1),
		Some(Entry::Ok(event))
			if matches!(&event.kind, Kind::Title { title, .. } if title.as_str() == "second")
	));
}

#[test]
fn reader_reports_and_repairs_a_torn_tail_without_shifting_indexes() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let writer = Writer::create(&path, &header()).expect("new transcript");
	drop(writer);
	let mut reader = Reader::open(&path).expect("reader opens");
	let append_offset = reader.append_offset();
	let mut encoded = Vec::new();
	write_line(&title(1, "repaired"), &mut encoded).expect("event encodes");
	let split = encoded.len() / 2;
	let mut file = OpenOptions::new()
		.append(true)
		.open(&path)
		.expect("transcript opens");
	file
		.write_all(&encoded[..split])
		.expect("torn prefix writes");
	drop(file);

	let report = reader.refresh().expect("torn refresh reports");
	assert_eq!(report.state, RefreshState::TornTail { records: 0 });
	assert_eq!(report.next_index, 0);
	assert_eq!(report.append_offset, append_offset);
	assert_eq!(report.tail_bytes, u64::try_from(split).expect("tail size fits in u64"));
	assert!(reader.log().is_empty());

	let mut file = OpenOptions::new()
		.append(true)
		.open(&path)
		.expect("transcript reopens");
	file
		.write_all(&encoded[split..])
		.expect("torn suffix writes");
	file.write_all(b"\n").expect("record terminator writes");
	drop(file);
	let report = reader.refresh().expect("repaired refresh succeeds");
	assert_eq!(report.state, RefreshState::Advanced { records: 1 });
	assert_eq!(report.next_index, 1);
	assert_eq!(report.tail_bytes, 0);
	assert!(matches!(reader.log().get(0), Some(Entry::Ok(_))));

	let mut file = OpenOptions::new()
		.append(true)
		.open(&path)
		.expect("transcript reopens");
	file
		.write_all(b"{not json}\n")
		.expect("corrupt complete record writes");
	drop(file);
	let report = reader.refresh().expect("corrupt record is retained");
	assert_eq!(report.state, RefreshState::Advanced { records: 1 });
	assert_eq!(report.next_index, 2);
	assert!(matches!(reader.log().get(1), Some(Entry::Tombstone(_))));
}

#[test]
fn reader_rejects_replacement_without_discarding_prior_state() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let displaced = directory.path().join("displaced.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("new transcript");
	writer.append(&title(1, "first")).expect("first event");
	drop(writer);
	let mut reader = Reader::open(&path).expect("reader opens");
	fs::rename(&path, &displaced).expect("old transcript moves");
	let mut replacement = Writer::create(&path, &header()).expect("replacement transcript");
	replacement
		.append(&title(2, "replacement"))
		.expect("replacement event");
	drop(replacement);

	assert!(reader.refresh().is_err());
	assert_eq!(reader.log().len(), 1);
	assert!(matches!(
		reader.log().get(0),
		Some(Entry::Ok(event))
			if matches!(&event.kind, Kind::Title { title, .. } if title.as_str() == "first")
	));
}

#[test]
fn custom_iterator_filters_live_events_without_collection() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let custom = |ts, kind: &str| Event { ts, kind: Kind::Custom(custom(kind, None, false)) };
	let mut writer = Writer::create(&path, &header()).expect("new transcript");
	writer.append(&custom(1, "wanted")).expect("wanted event");
	writer.append(&custom(2, "other")).expect("other event");
	writer
		.append(&Event { ts: 3, kind: Kind::Rewind { to: Some(0) } })
		.expect("rewind");
	writer
		.append(&custom(4, "wanted"))
		.expect("later wanted event");
	drop(writer);

	let reader = Reader::open(&path).expect("reader opens");
	let mut matching = reader.log().custom(reader.live(), "wanted");
	assert_eq!(matching.next().map(|(index, _)| index), Some(0));
	assert_eq!(matching.next_back().map(|(index, _)| index), Some(3));
	assert_eq!(matching.next(), None);
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PatchHolder {
	#[serde(default, skip_serializing_if = "Patch::is_unchanged")]
	value: Patch<u64>,
}

#[test]
fn patch_absent_null_and_value_are_distinct() {
	let absent: PatchHolder = serde_json::from_str("{}").expect("absent patch");
	let clear: PatchHolder = serde_json::from_str(r#"{"value":null}"#).expect("clear patch");
	let set: PatchHolder = serde_json::from_str(r#"{"value":9}"#).expect("set patch");
	assert_eq!(absent.value, Patch::Unchanged);
	assert_eq!(clear.value, Patch::Clear);
	assert_eq!(set.value, Patch::Set(9));
	assert_eq!(serde_json::to_string(&absent).expect("serialize absent patch"), "{}");
}

#[test]
fn writer_rejects_empty_infer_event() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("new transcript");
	let event = Event {
		ts:   1,
		kind: Kind::Infer {
			thinking: Patch::Unchanged,
			model:    Patch::Unchanged,
			tier:     Patch::Unchanged,
			cred_pin: Patch::Unchanged,
		},
	};
	assert!(matches!(writer.append(&event), Err(Error::EmptyInfer)));
}
