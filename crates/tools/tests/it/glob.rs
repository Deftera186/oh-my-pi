//! `glob@1` schema, traversal, and model-facing output contracts.

use std::{future, str, sync::Arc};

use bytes::Bytes;
use futures::{StreamExt, executor::block_on};
use omp_core::{Str, sf};
use omp_tool::{
	BlobRef, CapsBase, Ev, IncomingParams, ModelClass, Part, PromptCaps, Tool, ToolTerminal,
};
use omp_tools::{
	glob, grep,
	read::{Fault as ReadFault, ReadBlobs, StoredArtifact},
};
use parking_lot::Mutex;
use serde_json::json;

#[derive(Clone)]
struct FakeWorkspace {
	result: Result<glob::WalkResult, glob::Fault>,
	seen:   Arc<Mutex<Vec<glob::WalkRequest>>>,
}

impl grep::WorkspaceSearch for FakeWorkspace {
	fn search(
		&self,
		_request: grep::SearchRequest,
	) -> impl Future<Output = Result<grep::SearchResult, grep::Fault>> + Send + '_ {
		future::ready(Err(grep::Fault::Workspace { message: sf!("unused fake search boundary") }))
	}

	fn record_snapshots(&self, _records: Vec<grep::SnapshotRecord>) -> Result<(), grep::Fault> {
		Err(grep::Fault::Workspace { message: sf!("unused fake snapshot boundary") })
	}

	fn glob(
		&self,
		request: glob::WalkRequest,
	) -> impl Future<Output = Result<glob::WalkResult, glob::Fault>> + Send + '_ {
		let result = self.result.clone();
		let seen = Arc::clone(&self.seen);
		async move {
			seen.lock().push(request);
			result
		}
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
			Ok(BlobRef { hash: sf!("glob-full"), media_type, byte_len })
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
				blob: BlobRef { hash: sf!("glob-full"), media_type, byte_len },
				uri:  sf!("artifact://1"),
			})
		}
	}
}

struct Invocation {
	result:  Result<glob::Payload, glob::Fault>,
	useless: bool,
	text:    String,
}

fn fake(result: glob::WalkResult) -> FakeWorkspace {
	FakeWorkspace { result: Ok(result), seen: Arc::new(Mutex::new(Vec::new())) }
}

fn faulty(fault: glob::Fault) -> FakeWorkspace {
	FakeWorkspace { result: Err(fault), seen: Arc::new(Mutex::new(Vec::new())) }
}

const fn walk(matches: Vec<glob::WalkMatch>) -> glob::WalkResult {
	glob::WalkResult { matches, missing_paths: Vec::new(), timed_out: false, truncated: false }
}

const fn matched(path: &'static str, modified_ms: u64) -> glob::WalkMatch {
	glob::WalkMatch { path: sf!(path), modified_ms, is_dir: false }
}

const fn directory(path: &'static str, modified_ms: u64) -> glob::WalkMatch {
	glob::WalkMatch { path: sf!(path), modified_ms, is_dir: true }
}

fn invoke_with_blobs(workspace: FakeWorkspace, raw: &str, blobs: RecordingBlobs) -> Invocation {
	let tool = glob::tool(workspace, blobs);
	let (feed, params) = IncomingParams::channel();
	feed
		.args_committed(Str::new(raw))
		.expect("invocation consumer remains live");
	let events = block_on(tool.call(params).collect::<Vec<_>>());
	let [Ev::Done(ToolTerminal::Done { result, useless })] = events.as_slice() else {
		panic!("expected one terminal glob outcome: {events:?}");
	};
	let parts = tool.prompt(
		result.as_ref(),
		&PromptCaps::for_tool(
			CapsBase {
				maximum_parts:      1,
				maximum_text_bytes: 64 * 1024,
				media:              false,
				model_class:        ModelClass::Standard,
			},
			&tool.spec().rev,
		),
	);
	let text = parts
		.into_iter()
		.map(|part| match part {
			Part::Text { text } => text.to_string(),
			Part::Json { .. } => panic!("glob must project text only"),
			Part::Blob { .. } => panic!("glob must never project blobs"),
		})
		.collect();
	Invocation { result: result.clone(), useless: *useless, text }
}

fn invoke(workspace: FakeWorkspace, raw: &str) -> Invocation {
	invoke_with_blobs(workspace, raw, RecordingBlobs::default())
}

#[test]
fn schema_and_defaults_are_exact() {
	let workspace = fake(walk(Vec::new()));
	let seen = Arc::clone(&workspace.seen);
	let tool = glob::tool(workspace.clone(), RecordingBlobs::default());
	let actual: serde_json::Value =
		serde_json::from_slice(&tool.spec().schema).expect("glob schema is JSON");
	assert_eq!(
		tool.spec().schema.as_ref(),
		omp_tool::schema::<glob::Params>().as_ref(),
		"tool schema must be generated directly from Params",
	);
	assert_eq!(
		actual,
		json!({
			"type": "object",
			"additionalProperties": false,
			"required": ["i"],
			"properties": {
				"path": {
					"type": "string",
					"description": "glob, file, or directory to search — a single path or a semicolon-delimited list (\"src/**/*.ts; test/**/*.ts\"). Omitted -> searches the workspace root (\".\")"
				},
				"hidden": {
					"type": "boolean",
					"description": "include hidden files"
				},
				"gitignore": {
					"type": "boolean",
					"description": "respect gitignore"
				},
				"limit": {
					"type": "number",
					"description": "max results"
				},
				"i": {
					"type": "string",
					"description": "Short present-participle intent for this call."
				}
			}
		})
	);
	assert!(
		serde_json::from_value::<glob::Params>(json!({"patterns": ["**/*.rs"]})).is_err(),
		"glob params must reject the legacy patterns field"
	);

	let invocation = invoke(workspace, "{}");
	assert_eq!(invocation.text, "No files found matching pattern");
	assert!(invocation.useless);
	let requests = seen.lock();
	let [request] = requests.as_slice() else {
		panic!("default invocation must issue one walk: {requests:?}");
	};
	assert_eq!(request.path, ".");
	assert!(request.hidden);
	assert!(request.gitignore);
	assert_eq!(request.limit, 200);
	assert_eq!(request.timeout_ms, 5_000);
}

#[test]
fn newest_first_matches_are_prefix_grouped_and_directories_keep_their_slash() {
	let workspace = fake(walk(vec![
		matched("src/old.rs", 10),
		directory("fixtures/generated", 20),
		matched("src/new.rs", 30),
	]));
	let seen = Arc::clone(&workspace.seen);
	let invocation = invoke(workspace, r#"{"path":"**/*.rs","hidden":false,"gitignore":false}"#);
	assert_eq!(invocation.text, "# src/\nnew.rs\nold.rs\n# fixtures/generated/");
	assert!(!invocation.useless);
	let payload = invocation.result.expect("glob succeeds");
	assert_eq!(payload.matches, vec![
		matched("src/new.rs", 30),
		directory("fixtures/generated/", 20),
		matched("src/old.rs", 10),
	]);
	let requests = seen.lock();
	assert_eq!(requests[0].path, "**/*.rs");
	assert!(!requests[0].hidden);
	assert!(!requests[0].gitignore);
}

#[test]
fn limit_one_keeps_only_the_newest_match_and_records_truncation_truth() {
	let workspace = fake(walk(vec![matched("old.rs", 1), matched("new.rs", 2)]));
	let seen = Arc::clone(&workspace.seen);
	let invocation = invoke(workspace, r#"{"path":"*.rs","limit":1}"#);
	assert_eq!(invocation.text, "new.rs\n\n1 results limit reached. Use limit=2 for more.");
	assert!(!invocation.useless);
	let payload = invocation.result.expect("glob succeeds");
	assert_eq!(payload.matches, vec![matched("new.rs", 2)]);
	assert!(payload.truncated);
	assert_eq!(payload.result_limit_reached, Some(1));
	assert_eq!(seen.lock()[0].limit, 1);
}

#[test]
fn root_search_is_rejected_before_workspace_traversal() {
	let workspace = fake(walk(Vec::new()));
	let seen = Arc::clone(&workspace.seen);
	let invocation = invoke(workspace, r#"{"path":"/"}"#);
	assert!(invocation.result.is_err());
	assert_eq!(invocation.text, "Searching from root directory '/' is not allowed");
	assert!(!invocation.useless);
	assert!(seen.lock().is_empty());
}

#[test]
fn missing_paths_fault_only_when_no_target_survives() {
	let invocation = invoke(
		faulty(glob::Fault::PathNotFound { paths: vec![sf!("missing")] }),
		r#"{"path":"missing"}"#,
	);
	assert!(invocation.result.is_err());
	assert_eq!(invocation.text, "Path not found: missing");
	assert!(!invocation.useless);

	let invocation = invoke(
		faulty(glob::Fault::PathNotFound { paths: vec![sf!("one"), sf!("two")] }),
		r#"{"path":"one; two"}"#,
	);
	assert!(invocation.result.is_err());
	assert_eq!(invocation.text, "Path not found: one, two");
	assert!(!invocation.useless);
}

#[test]
fn surviving_multi_target_appends_the_missing_path_note() {
	let mut result = walk(vec![matched("src/lib.rs", 1)]);
	result.missing_paths = vec![sf!("gone"), sf!("also-gone")];
	let invocation = invoke(fake(result), r#"{"path":"src; gone; also-gone"}"#);
	assert_eq!(invocation.text, "# src/\nlib.rs\n\nSkipped missing paths: gone, also-gone");
	assert!(!invocation.useless);
	assert_eq!(
		invocation
			.result
			.expect("surviving target succeeds")
			.missing_paths
			.len(),
		2
	);
}

#[test]
fn exact_file_target_is_returned_without_a_synthetic_header() {
	let workspace = fake(walk(vec![matched("Cargo.toml", 42)]));
	let seen = Arc::clone(&workspace.seen);
	let invocation = invoke(workspace, r#"{"path":"Cargo.toml"}"#);
	assert_eq!(invocation.text, "Cargo.toml");
	assert!(!invocation.useless);
	assert_eq!(seen.lock()[0].path, "Cargo.toml");
}

#[test]
fn directory_star_stays_nonrecursive() {
	let workspace = fake(walk(vec![matched("dir/direct.rs", 2)]));
	let seen = Arc::clone(&workspace.seen);
	let invocation = invoke(workspace, r#"{"path":"dir/*"}"#);
	assert_eq!(invocation.text, "# dir/\ndirect.rs");
	assert_eq!(seen.lock()[0].path, "dir/*");
}

#[test]
fn a_leading_glob_search_can_return_nested_matches() {
	let workspace = fake(walk(vec![matched("nested/deep/match.rs", 2)]));
	let seen = Arc::clone(&workspace.seen);
	let invocation = invoke(workspace, r#"{"path":"*.rs"}"#);
	assert_eq!(invocation.text, "# nested/deep/\nmatch.rs");
	assert_eq!(seen.lock()[0].path, "*.rs");
}

#[test]
fn timeout_with_partial_matches_returns_ranked_incomplete_output() {
	let mut result = walk(vec![matched("old.rs", 1), matched("new.rs", 2)]);
	result.timed_out = true;
	let invocation = invoke(fake(result), r#"{"path":"*.rs"}"#);
	assert_eq!(
		invocation.text,
		"new.rs\nold.rs\n\nglob timed out after 5s; returning 2 partial matches — results are \
		 incomplete, scope to a deeper directory instead of retrying blindly"
	);
	assert!(!invocation.useless);
	let payload = invocation.result.expect("partial timeout is successful");
	assert!(payload.timed_out);
	assert!(payload.truncated);
	assert_eq!(payload.partial_match_count, 2);
	assert_eq!(payload.timeout_ms, 5_000);
}

#[test]
fn timeout_without_matches_is_not_reported_as_proof_of_absence() {
	let mut result = walk(Vec::new());
	result.timed_out = true;
	let invocation = invoke(fake(result), r#"{"path":"*.rs"}"#);
	assert_eq!(
		invocation.text,
		"Glob timed out after 5s before finding any matches — the scan is incomplete, NOT proof of \
		 absence. The walk is bounded by directory size, not pattern width; scope the search to a \
		 deeper directory (e.g. `sub/dir/*.ext` instead of `*.ext` at a huge root)."
	);
	assert!(!invocation.useless, "an incomplete traversal is useful partial truth");
	let payload = invocation.result.expect("empty timeout is successful");
	assert!(payload.timed_out);
	assert!(payload.truncated);
	assert_eq!(payload.partial_match_count, 0);
	assert_eq!(payload.timeout_ms, 5_000);
}

#[test]
fn oversized_projection_spills_complete_output_with_truthful_footer() {
	let matches = (0..200)
		.map(|index| glob::WalkMatch {
			path:        sf!("dir/{index:03}-{}.rs", "x".repeat(400)),
			modified_ms: index,
			is_dir:      false,
		})
		.collect();
	let blobs = RecordingBlobs::default();
	let invocation = invoke_with_blobs(fake(walk(matches)), r#"{"path":"dir/*.rs"}"#, blobs.clone());
	let payload = invocation
		.result
		.as_ref()
		.expect("large glob output succeeds");
	let stored = blobs.stored.lock();
	let [full] = stored.as_slice() else {
		panic!("glob must store exactly one complete pre-truncation output");
	};
	let full = str::from_utf8(full).expect("rendered glob output is UTF-8");
	assert!(full.starts_with("# dir/\n199-"));
	assert!(full.to_ascii_lowercase().ends_with(".rs"));
	assert_eq!(payload.output_total_lines, 201);
	assert!(payload.output_shown_lines < payload.output_total_lines);
	assert_eq!(payload.output_blob.as_ref().map(|blob| blob.hash.as_str()), Some("glob-full"));
	assert_eq!(payload.output_artifact_uri.as_deref(), Some("artifact://1"));
	let expected_footer = format!(
		"[truncated: {} of {} lines shown; read artifact://1 for full output]",
		payload.output_shown_lines, payload.output_total_lines
	);
	assert!(invocation.text.ends_with(expected_footer.as_str()));

	let zero_tool = glob::tool(fake(walk(Vec::new())), RecordingBlobs::default());
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
