//! Model-facing behavioral contracts for pi-compatible `grep@1`.

use std::{future, future::Future, str, sync::Arc};

use bytes::Bytes;
use futures::{StreamExt, executor::block_on};
use omp_core::{Str, sf};
use omp_tool::{
	BlobRef, CallOutcome, CapsBase, Claims, ErasedEv, ErasedOutcome, IncomingParams, ModelClass,
	Part, Precedence, Presentation, PromptCaps, Registry, Tool,
};
use omp_tools::{
	glob, grep,
	read::{Fault as ReadFault, ReadBlobs, StoredArtifact},
};
use parking_lot::Mutex;
use serde_json::json;

#[derive(Clone)]
struct FakeWorkspace {
	result:   Result<grep::SearchResult, grep::Fault>,
	recorded: Arc<Mutex<Vec<grep::SnapshotRecord>>>,
}

impl grep::WorkspaceSearch for FakeWorkspace {
	fn search(
		&self,
		_request: grep::SearchRequest,
	) -> impl Future<Output = Result<grep::SearchResult, grep::Fault>> + Send + '_ {
		let result = self.result.clone();
		async move { result }
	}

	fn record_snapshots(&self, records: Vec<grep::SnapshotRecord>) -> Result<(), grep::Fault> {
		self.recorded.lock().extend(records);
		Ok(())
	}

	fn glob(
		&self,
		_request: glob::WalkRequest,
	) -> impl Future<Output = Result<glob::WalkResult, glob::Fault>> + Send + '_ {
		future::ready(Err(glob::Fault::Workspace { message: sf!("unused fake glob boundary") }))
	}
}

#[derive(Clone, Default)]
struct RecordingBlobs {
	stored: Arc<Mutex<Vec<Bytes>>>,
}

impl ReadBlobs for RecordingBlobs {
	fn store(
		&self,
		bytes: Bytes,
		media_type: Str,
	) -> impl Future<Output = Result<BlobRef, ReadFault>> + Send + '_ {
		let stored = Arc::clone(&self.stored);
		async move {
			let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
			stored.lock().push(bytes);
			Ok(BlobRef { hash: sf!("grep-full"), media_type, byte_len })
		}
	}

	fn store_artifact(
		&self,
		bytes: Bytes,
		media_type: Str,
	) -> impl Future<Output = Result<StoredArtifact, ReadFault>> + Send + '_ {
		let stored = Arc::clone(&self.stored);
		async move {
			let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
			stored.lock().push(bytes);
			Ok(StoredArtifact {
				blob: BlobRef { hash: sf!("grep-full"), media_type, byte_len },
				uri:  sf!("artifact://1"),
			})
		}
	}
}

struct Invocation {
	outcome: CallOutcome<grep::Payload, grep::Fault>,
	useless: bool,
}

fn fake(result: grep::SearchResult) -> FakeWorkspace {
	FakeWorkspace { result: Ok(result), recorded: Arc::default() }
}

fn failed(fault: grep::Fault) -> FakeWorkspace {
	FakeWorkspace { result: Err(fault), recorded: Arc::default() }
}

fn matched(path: &str, line_number: u32, line: &str, tag: Option<&str>) -> grep::SearchMatch {
	grep::SearchMatch {
		source_key: Str::new(path),
		path: Str::new(path),
		root_index: 0,
		line_number,
		line: Str::new(line),
		truncated: false,
		context_before: Vec::new(),
		context_after: Vec::new(),
		snapshot_tag: tag.map(Str::new),
	}
}

fn invoke_with_blobs(workspace: &FakeWorkspace, raw: &str, blobs: RecordingBlobs) -> Invocation {
	let mut registry = Registry::new();
	registry
		.register(grep::tool(workspace.clone(), blobs), Presentation::Slot, Claims {
			precedence: Precedence::CORE,
			claimant:   sf!("omp/core"),
			replaces:   None,
		})
		.expect("grep schema and revision register");
	let (feed, params) = IncomingParams::channel();
	feed
		.args_committed(Str::new(raw))
		.expect("invocation consumer remains live");
	let events = block_on(
		registry
			.invoke("grep", params)
			.expect("registered grep is invokable")
			.collect::<Vec<_>>(),
	);
	let [Ok(ErasedEv::Done(ErasedOutcome::Done { verdict, useless }))] = events.as_slice() else {
		panic!("expected one terminal grep event: {events:?}");
	};
	Invocation {
		outcome: serde_json::from_slice(verdict)
			.expect("typed grep outcome survives registry erasure"),
		useless: *useless,
	}
}

fn invoke(workspace: &FakeWorkspace, raw: &str) -> Invocation {
	invoke_with_blobs(workspace, raw, RecordingBlobs::default())
}

fn prompt(workspace: &FakeWorkspace, outcome: &CallOutcome<grep::Payload, grep::Fault>) -> String {
	let tool = grep::tool(workspace.clone(), RecordingBlobs::default());
	let caps = PromptCaps::for_tool(
		CapsBase {
			maximum_parts:      1,
			maximum_text_bytes: u32::MAX,
			media:              false,
			model_class:        ModelClass::Standard,
		},
		&tool.spec().rev,
	);
	let parts = match outcome {
		CallOutcome::Ok(payload) => tool.prompt(Ok(payload), &caps),
		CallOutcome::Faulted(fault) => tool.prompt(Err(fault), &caps),
		other => panic!("expected a projectable grep outcome, got {other:?}"),
	};
	let [Part::Text { text }] = parts.as_slice() else {
		panic!("grep must project exactly one text part: {parts:?}");
	};
	text.to_string()
}

fn invoke_prompt(workspace: &FakeWorkspace, raw: &str) -> (String, bool) {
	let invocation = invoke(workspace, raw);
	(prompt(workspace, &invocation.outcome), invocation.useless)
}

#[test]
fn schema_is_exactly_the_pi_grep_schema() {
	let tool = grep::tool(fake(grep::SearchResult::default()), RecordingBlobs::default());
	let actual: serde_json::Value =
		serde_json::from_slice(&tool.spec().schema).expect("grep schema is JSON");
	assert_eq!(
		tool.spec().schema.as_ref(),
		omp_tool::schema::<grep::Params>().as_ref(),
		"tool schema must be generated directly from Params",
	);
	assert_eq!(
		actual,
		json!({
			"type": "object",
			"additionalProperties": false,
			"required": ["pattern"],
			"properties": {
				"pattern": {"type": "string", "description": "regex pattern"},
				"path": {
					"type": "string",
					"description": "file, directory, glob, internal URL, or \"<file>:<lines>\" selector to search; pass several as a semicolon-delimited list (\"src; tests\"). Omitted -> searches the workspace root (\".\")"
				},
				"case": {"type": "boolean", "description": "case-sensitive search"},
				"gitignore": {"type": "boolean", "description": "respect gitignore"},
				"skip": {
					"type": ["number", "null"],
					"description": "files to skip before collecting results — use to paginate when the prior call hit the file limit"
				}
			}
		})
	);
	for legacy in [
		json!({"pattern": "needle", "patterns": ["needle"]}),
		json!({"pattern": "needle", "include": "*.rs"}),
		json!({"pattern": "needle", "exclude": "target/**"}),
		json!({"pattern": "needle", "mode": "files"}),
		json!({"pattern": "needle", "limit": 20}),
	] {
		assert!(
			serde_json::from_value::<grep::Params>(legacy).is_err(),
			"grep params must reject legacy fields"
		);
	}
}

#[test]
fn grouped_matches_have_folded_headers_tags_and_hashline_match_rows() {
	let workspace = fake(grep::SearchResult {
		matches: vec![
			matched("dir/alpha.rs", 2, "let needle = 1;", Some("A1B2")),
			matched("dir/beta.rs", 7, "// needle", Some("C3D4")),
		],
		multi_scope: true,
		..grep::SearchResult::default()
	});
	let (text, useless) = invoke_prompt(&workspace, r#"{"pattern":"needle","path":"dir"}"#);
	assert_eq!(text, "# dir/\n## alpha.rs#A1B2\n*2:let needle = 1;\n## beta.rs#C3D4\n*7:// needle");
	assert!(!useless);
}

#[test]
fn single_file_match_has_hashline_header_and_no_group_heading() {
	let workspace = fake(grep::SearchResult {
		matches: vec![matched("src/one.rs", 4, "needle();", Some("BEEF"))],
		multi_scope: false,
		..grep::SearchResult::default()
	});
	let (text, useless) = invoke_prompt(&workspace, r#"{"pattern":"needle","path":"src/one.rs"}"#);
	assert_eq!(text, "[src/one.rs#BEEF]\n*4:needle();");
	assert!(!useless);
}

#[test]
fn twenty_file_window_has_exact_footer_and_skip_twenty_returns_next_page() {
	let matches = (1..=21)
		.map(|index| {
			let path = format!("page/file-{index:02}.rs");
			matched(&path, 1, "needle", Some("CAFE"))
		})
		.collect();
	let workspace =
		fake(grep::SearchResult { matches, multi_scope: true, ..grep::SearchResult::default() });

	let (first, first_useless) = invoke_prompt(&workspace, r#"{"pattern":"needle","path":"page"}"#);
	let expected_files = (1..=20)
		.map(|index| format!("## file-{index:02}.rs#CAFE\n*1:needle"))
		.collect::<Vec<_>>()
		.join("\n");
	assert_eq!(
		first,
		format!(
			"# page/\n{expected_files}\n\nShowing files 1-20 of 21. Use skip=20 for the next page, \
			 or narrow paths/pattern."
		)
	);
	assert!(!first_useless);

	let (second, second_useless) =
		invoke_prompt(&workspace, r#"{"pattern":"needle","path":"page","skip":20}"#);
	assert_eq!(second, "# page/\n## file-21.rs#CAFE\n*1:needle");
	assert!(!second_useless);
}

#[test]
fn no_matches_projects_the_pi_message_and_is_useless() {
	let workspace = fake(grep::SearchResult { multi_scope: true, ..grep::SearchResult::default() });
	let (text, useless) = invoke_prompt(&workspace, r#"{"pattern":"absent","path":"src"}"#);
	assert_eq!(text, "No matches found");
	assert!(useless);
}

#[test]
fn invalid_regex_is_mapped_to_the_pi_fault_text() {
	let workspace = failed(grep::Fault::InvalidRegex { message: sf!("unclosed group") });
	let (text, useless) = invoke_prompt(&workspace, r#"{"pattern":"(","path":"src"}"#);
	assert_eq!(text, "Invalid regex: unclosed group");
	assert!(!useless);
}

#[test]
fn line_selector_filters_matches_before_projection() {
	let workspace = fake(grep::SearchResult {
		matches: vec![
			matched("src/range.rs", 2, "needle before", Some("F00D")),
			matched("src/range.rs", 3, "needle in range", Some("F00D")),
			matched("src/range.rs", 5, "needle after", Some("F00D")),
		],
		multi_scope: false,
		..grep::SearchResult::default()
	});
	let (text, useless) =
		invoke_prompt(&workspace, r#"{"pattern":"needle","path":"src/range.rs:3-4"}"#);
	assert_eq!(text, "[src/range.rs#F00D]\n*3:needle in range");
	assert!(!useless);
}

#[test]
fn range_overfetch_and_output_truncation_authorize_only_emitted_rows() {
	let mut matches = (1..500)
		.map(|line_number| matched("src/range.rs", line_number, "needle before range", Some("F00D")))
		.collect::<Vec<_>>();
	matches.extend((500..=600).map(|line_number| {
		matched("src/range.rs", line_number, &format!("needle {}", "x".repeat(505)), Some("F00D"))
	}));
	let workspace = fake(grep::SearchResult {
		matches,
		snapshots: vec![grep::SearchSnapshot {
			source_key: sf!("src/range.rs"),
			revision:   Bytes::from_static(b"range-revision"),
			bytes:      Bytes::from(vec![b'x'; 80 * 1024]),
		}],
		multi_scope: false,
		..grep::SearchResult::default()
	});
	let blobs = RecordingBlobs::default();
	let invocation =
		invoke_with_blobs(&workspace, r#"{"pattern":"needle","path":"src/range.rs:500-600"}"#, blobs);
	let text = prompt(&workspace, &invocation.outcome);
	let visible = text
		.lines()
		.filter_map(|line| line.strip_prefix('*'))
		.filter_map(|line| line.split_once(':'))
		.filter_map(|(number, _)| number.parse::<usize>().ok())
		.collect::<Vec<_>>();
	let recorded = workspace.recorded.lock();
	let [record] = recorded.as_slice() else {
		panic!("one visible file snapshot must be recorded: {recorded:?}");
	};

	assert!(text.starts_with("[src/range.rs#F00D]\n*500:needle "));
	assert!(text.contains("[truncated:"));
	assert!(!visible.is_empty());
	assert!(visible.iter().all(|line| (500..=600).contains(line)));
	assert!(!visible.contains(&600), "the shared byte cap must omit tail matches");
	assert_eq!(record.seen_lines, visible);
}

#[test]
fn explicit_oversized_file_note_is_appended_verbatim() {
	let workspace = fake(grep::SearchResult {
		matches: vec![matched("large.log", 1, "needle", None)],
		multi_scope: false,
		oversized_files: vec![sf!("large.log")],
		..grep::SearchResult::default()
	});
	let (text, useless) = invoke_prompt(&workspace, r#"{"pattern":"needle","path":"large.log"}"#);
	assert_eq!(
		text,
		"*1:needle\n\nSearched only the first 4MB of large files (matches past the 4MB window are \
		 not shown; use `read` for the rest): large.log"
	);
	assert!(!useless);
}

#[test]
fn injected_timeout_projects_the_fixed_thirty_second_mapping() {
	let workspace = failed(grep::Fault::TimedOut);
	let (text, useless) = invoke_prompt(&workspace, r#"{"pattern":"needle","path":"src"}"#);
	assert_eq!(
		text,
		"Grep timed out after 30s; narrow paths or pattern, or scope with `glob` first"
	);
	assert!(!useless);
}

#[test]
fn oversized_projection_spills_complete_output_with_truthful_footer() {
	let matches = (1..=200)
		.map(|line_number| {
			matched("large.rs", line_number, &format!("needle {}", "x".repeat(400)), Some("B10B"))
		})
		.collect();
	let workspace =
		fake(grep::SearchResult { matches, multi_scope: false, ..grep::SearchResult::default() });
	let blobs = RecordingBlobs::default();
	let invocation =
		invoke_with_blobs(&workspace, r#"{"pattern":"needle","path":"large.rs"}"#, blobs.clone());
	let text = prompt(&workspace, &invocation.outcome);
	let CallOutcome::Ok(payload) = &invocation.outcome else {
		panic!("large grep output must succeed");
	};
	let stored = blobs.stored.lock();
	let [full] = stored.as_slice() else {
		panic!("grep must store exactly one complete pre-truncation output");
	};
	let full = str::from_utf8(full).expect("rendered grep output is UTF-8");
	assert!(full.starts_with("[large.rs#B10B]\n*1:needle "));
	let expected_tail = format!("*200:needle {}", "x".repeat(400));
	assert!(full.ends_with(expected_tail.as_str()));
	assert_eq!(payload.output_total_lines, 201);
	assert!(payload.output_shown_lines < payload.output_total_lines);
	assert_eq!(payload.output_blob.as_ref().map(|blob| blob.hash.as_str()), Some("grep-full"));
	assert_eq!(payload.output_artifact_uri.as_deref(), Some("artifact://1"));
	let expected_footer = format!(
		"[truncated: {} of {} lines shown; read artifact://1 for full output]",
		payload.output_shown_lines, payload.output_total_lines
	);
	assert!(text.ends_with(expected_footer.as_str()));

	let zero_tool = grep::tool(workspace, RecordingBlobs::default());
	let zero = zero_tool.prompt(
		Ok(payload),
		&PromptCaps::for_tool(
			CapsBase {
				maximum_parts:      0,
				maximum_text_bytes: 0,
				media:              false,
				model_class:        ModelClass::Standard,
			},
			&zero_tool.spec().rev,
		),
	);
	assert!(zero.is_empty());
}
