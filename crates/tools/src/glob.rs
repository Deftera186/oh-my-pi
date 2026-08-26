//! Pi-compatible workspace path matching with mtime-ranked grouped output.

use std::{
	collections::HashSet,
	error,
	fmt::{self, Display},
	sync::Arc,
};

use async_stream::stream;
use futures::{FutureExt, Stream, pin_mut, select_biased};
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, BlobRef, CommitError, Constraint, DocEffects, Effects, Ev,
	IncomingParams, InterruptWaitError, ParamError, Part, PromptCaps, Rev, Tool, ToolSpec,
	ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
	grep::WorkspaceSearch,
	read::ReadBlobs,
	render::{TextProjection, paths::format_grouped_paths, truncate::spill_truncated_text},
};

/// Default number of paths returned by `glob@1`.
pub const DEFAULT_LIMIT: u64 = 200;
/// Maximum number of paths returned by `glob@1`.
pub const MAX_LIMIT: u64 = 200;
/// Maximum time allotted to the workspace traversal.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;

const fn default_true() -> bool {
	true
}

#[expect(
	clippy::trivially_copy_pass_by_ref,
	reason = "schemars skip_serializing_if predicates receive field references"
)]
const fn is_true(value: &bool) -> bool {
	*value
}

/// Model arguments for `glob@1`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[schemars(description = "")]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// glob, file, or directory to search — a single path or a
	/// semicolon-delimited list ("src/**/*.ts; test/**/*.ts"). Omitted ->
	/// searches the workspace root (".")
	#[schemars(description = "glob, file, or directory to search — a single path or a \
	                          semicolon-delimited list (\"src/**/*.ts; test/**/*.ts\"). Omitted \
	                          -> searches the workspace root (\".\")")]
	#[schemars(default, skip_serializing_if = "Option::is_none", with = "String")]
	pub path:      Option<Str>,
	/// include hidden files
	#[serde(default = "default_true")]
	#[schemars(skip_serializing_if = "is_true")]
	pub hidden:    bool,
	/// respect gitignore
	#[serde(default = "default_true")]
	#[schemars(skip_serializing_if = "is_true")]
	pub gitignore: bool,
	/// max results
	#[schemars(default, skip_serializing_if = "Option::is_none", with = "serde_json::Number")]
	pub limit:     Option<f64>,
}

/// Fully specified request passed to the workspace resource after commitment.
///
/// `path` stays unsplit so the resource can stat the literal spelling before
/// interpreting semicolons. This preserves real filenames containing `;`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WalkRequest {
	/// Raw model path, defaulted to `.`.
	pub path:       Str,
	/// Whether dot-prefixed paths are traversed.
	pub hidden:     bool,
	/// Whether ignore files are honored.
	pub gitignore:  bool,
	/// Effective per-call result cap.
	pub limit:      u64,
	/// Traversal deadline in milliseconds.
	pub timeout_ms: u64,
}

/// One workspace-relative path discovered by the resource.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WalkMatch {
	/// Workspace-relative model-facing path using `/` separators.
	pub path:        Str,
	/// Modification time in milliseconds, used for newest-first ranking.
	pub modified_ms: u64,
	/// Whether this path names a directory.
	pub is_dir:      bool,
}

/// Structured resource result, including partial traversal truth.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WalkResult {
	/// Matches gathered before completion or timeout.
	pub matches:       Vec<WalkMatch>,
	/// Missing targets skipped while at least one target survived. The resource
	/// returns [`Fault::PathNotFound`] instead when the sole or every target is
	/// missing.
	pub missing_paths: Vec<Str>,
	/// Whether the traversal deadline ended the scan.
	pub timed_out:     bool,
	/// Whether the resource omitted matches for a non-timeout limit.
	pub truncated:     bool,
}

/// Durable successful `glob@1` result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Payload {
	/// Newest-first, deduplicated, display-ready paths retained by the hard cap.
	pub matches:              Vec<WalkMatch>,
	/// User targets skipped because their base paths were missing.
	pub missing_paths:        Vec<Str>,
	/// Whether the traversal deadline ended the scan.
	pub timed_out:            bool,
	/// Whether timeout, the resource, or the hard result cap omitted matches.
	pub truncated:            bool,
	/// Effective result limit when it omitted otherwise available matches.
	pub result_limit_reached: Option<u64>,
	/// Number of distinct partial matches gathered before applying the limit.
	pub partial_match_count:  u64,
	/// Deadline used by this invocation, retained for exact timeout rendering.
	pub timeout_ms:           u64,
	/// Exact bounded model-facing text prepared before prompt projection.
	pub projected_text:       Str,
	/// Durable complete output when `projected_text` was pre-truncated.
	pub output_blob:          Option<BlobRef>,
	/// Complete lines retained in `projected_text` before its footer.
	pub output_shown_lines:   u64,
	/// Complete line count in the pre-truncation output.
	pub output_total_lines:   u64,
}

/// Ephemeral progress from `glob@1`; traversal has no durable updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// Durable typed `glob@1` failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// The caller supplied a non-positive or non-finite limit.
	InvalidLimit,
	/// The input contained no usable path target.
	EmptyPath,
	/// A traversal attempted to start at filesystem root.
	RootSearch,
	/// Every requested target, or the sole requested target, was missing.
	PathNotFound {
		/// Missing target spellings in model input order.
		paths: Vec<Str>,
	},
	/// A direct non-directory target could not be treated as a file.
	PathNotDirectory {
		/// Rejected target path.
		path: Str,
	},
	/// A URI scheme has no local path-backed glob implementation yet.
	UnsupportedScheme {
		/// Lowercase URI scheme without punctuation.
		scheme: Str,
	},
	/// A glob pattern could not be compiled by the workspace walker.
	InvalidPattern {
		/// Exact rejected pattern.
		pattern: Str,
		/// Resource-owned parser explanation.
		message: Str,
	},
	/// The workspace owner rejected or failed the request.
	Workspace {
		/// Stable resource-owned explanation.
		message: Str,
	},
	/// Durable blob storage failed while preserving complete output.
	Blob {
		/// Stable blob-owned explanation.
		message: Str,
	},
	/// The resource observed cancellation without an invocation interrupt.
	Cancelled {
		/// Stable resource-owned cancellation reason.
		reason: Str,
	},
}

impl Display for Fault {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidLimit => formatter.write_str("Limit must be a positive number"),
			Self::EmptyPath => formatter.write_str("`path` must contain non-empty globs or paths"),
			Self::RootSearch => {
				formatter.write_str("Searching from root directory '/' is not allowed")
			},
			Self::PathNotFound { paths } => {
				formatter.write_str("Path not found: ")?;
				for (index, path) in paths.iter().enumerate() {
					if index != 0 {
						formatter.write_str(", ")?;
					}
					formatter.write_str(path)?;
				}
				Ok(())
			},
			Self::PathNotDirectory { path } => write!(formatter, "Path is not a directory: {path}"),
			Self::UnsupportedScheme { scheme } => {
				write!(formatter, "{scheme}:// targets are not supported yet")
			},
			Self::InvalidPattern { pattern, message } => {
				write!(formatter, "invalid glob pattern {pattern}: {message}")
			},
			Self::Workspace { message } | Self::Blob { message } => formatter.write_str(message),
			Self::Cancelled { reason } => formatter.write_str(reason),
		}
	}
}

impl error::Error for Fault {}

/// Generic `glob@1` executor over environment-owned workspace and blob
/// resources.
pub struct Glob<W, B> {
	workspace: W,
	blobs:     B,
	spec:      ToolSpec,
}

/// Constructs `glob@1` over `workspace` and the shared durable blob namespace.
pub fn tool<W: WorkspaceSearch, B: ReadBlobs>(workspace: W, blobs: B) -> Glob<W, B> {
	Glob {
		workspace,
		blobs,
		spec: ToolSpec {
			name:            sf!("glob"),
			rev:             Rev { family: Str::new(""), n: 1 },
			description:     sf!(
				"Globs files and directories with fast pattern matching.\n\n<instruction>\n- `path`: \
				 glob, file, or directory; separate targets with `;` (`src/**/*.ts; \
				 test/**/*.ts`).\n- `gitignore` defaults `true`. Set `false` for ignored files such \
				 as `.env*`, logs, or build output.\n- `hidden` defaults `true`; pair it with \
				 `gitignore: false` for ignored dotfiles.\n</instruction>\n\n<output>\nMatches are \
				 newest-first and grouped by directory; directories end in `/`.\n</output>",
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects {
				documents: Some(DocEffects { read: true, write_globs: Arc::default() }),
				exec:      None,
				inference: None,
				desktop:   None,
				subagents: 0,
			},
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("glob.rs"),
			)
			.into(),
		},
	}
}

impl<W: WorkspaceSearch, B: ReadBlobs> Tool for Glob<W, B> {
	type Fault = Fault;
	type Params = Params;
	type Payload = Payload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut params: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		stream! {
					let arguments = match params.whole::<Params>().await {
						Ok(arguments) => arguments,
						Err(error) => {
							yield param_event(error);
							return;
						},
					};
					match params.interruptable().committed().await {
						Ok(_) => {},
						Err(error) => {
							yield commit_event(error);
							return;
						},
					}

					let limit = match effective_limit(arguments.limit) {
						Ok(limit) => limit,
						Err(fault) => {
							yield done(Err(fault));
							return;
						},
					};
					let path = arguments.path.unwrap_or_else(|| sf!("."));
					if path.trim().is_empty() {
						yield done(Err(Fault::EmptyPath));
						return;
					}
					if contains_root_target(&path) {
						yield done(Err(Fault::RootSearch));
						return;
					}
					let resource_scheme = unsupported_scheme(&path);
					let request = walk_request(path, arguments.hidden, arguments.gitignore, limit);
					if let Some(scheme) = resource_scheme {
						let resource = self.workspace.glob_resource(request.clone()).await;
						match resource {
							Some(Ok(result)) => {
								yield done(prepare_payload(result, limit, DEFAULT_TIMEOUT_MS, &self.blobs).await);
							},
							Some(Err(fault)) => yield done(Err(fault)),
							None => yield done(Err(Fault::UnsupportedScheme { scheme })),
						}
						return;
					}
		let operation = async {
						let result = self.workspace.glob(request).await?;
						prepare_payload(result, limit, DEFAULT_TIMEOUT_MS, &self.blobs).await
					}.fuse();
					let interruption = params.next_interrupt().fuse();
					pin_mut!(operation, interruption);
					select_biased! {
						result = operation => {
							yield done(result);
						},
						interrupt = interruption => {
							yield interrupt_event(interrupt, "glob traversal owner disappeared");
						},
					}
				}
	}

	fn prompt(&self, view: Result<&Self::Payload, &Self::Fault>, caps: &PromptCaps) -> Vec<Part> {
		let Some(mut projection) = TextProjection::new(*caps) else {
			return Vec::new();
		};
		let text = match view {
			Ok(payload) => payload.projected_text.as_str(),
			Err(fault) => {
				let message = fault.to_string();
				projection.push(&message);
				return projection.finish();
			},
		};
		for fragment in text.split_inclusive('\n') {
			if !projection.push(fragment) {
				break;
			}
		}
		projection.finish()
	}
}

fn effective_limit(limit: Option<f64>) -> Result<u64, Fault> {
	let requested = limit.unwrap_or(DEFAULT_LIMIT as f64);
	if !requested.is_finite() || requested <= 0.0 {
		return Err(Fault::InvalidLimit);
	}
	Ok((requested.floor() as u64).clamp(1, MAX_LIMIT))
}

const fn walk_request(path: Str, hidden: bool, gitignore: bool, limit: u64) -> WalkRequest {
	WalkRequest { path, hidden, gitignore, limit, timeout_ms: DEFAULT_TIMEOUT_MS }
}

pub(crate) fn display_scope(path: &str) -> Str {
	if path.contains(';') {
		return sf!(".");
	}
	let Some(wildcard) = path
		.char_indices()
		.find_map(|(index, character)| matches!(character, '*' | '?' | '[').then_some(index))
	else {
		return Str::new(path);
	};
	let prefix = &path[..wildcard];
	if prefix.ends_with('/') {
		let directory = prefix.trim_end_matches('/');
		return if directory.is_empty() {
			sf!(".")
		} else {
			Str::new(directory)
		};
	}
	prefix
		.rsplit_once('/')
		.map_or_else(|| sf!("."), |(directory, _)| Str::new(directory))
}

fn contains_root_target(path: &str) -> bool {
	path.split(';').any(|target| {
		let target = target.trim();
		!target.is_empty() && target.bytes().all(|byte| byte == b'/')
	})
}

fn unsupported_scheme(path: &str) -> Option<Str> {
	path.split(';').find_map(|target| {
		let target = target.trim();
		let (scheme, _) = target.split_once("://")?;
		let mut chars = scheme.bytes();
		let valid = matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
			&& chars.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-'));
		valid.then(|| Str::new(scheme.to_ascii_lowercase()))
	})
}

fn payload(mut result: WalkResult, limit: u64, timeout_ms: u64) -> Payload {
	for entry in &mut result.matches {
		let mut normalized = entry.path.replace('\\', "/");
		normalized.truncate(normalized.trim_end_matches('/').len());
		if entry.is_dir {
			normalized.push('/');
		}
		entry.path = normalized.into();
	}
	result.matches.sort_by(|left, right| {
		right
			.modified_ms
			.cmp(&left.modified_ms)
			.then_with(|| left.path.cmp(&right.path))
	});
	let mut seen = HashSet::with_capacity(result.matches.len());
	result
		.matches
		.retain(|entry| seen.insert(entry.path.clone()));
	let partial_match_count = u64::try_from(result.matches.len()).unwrap_or(u64::MAX);
	let over_limit = partial_match_count > limit;
	let retain = usize::try_from(limit)
		.unwrap_or(usize::MAX)
		.min(result.matches.len());
	result.matches.truncate(retain);
	Payload {
		matches: result.matches,
		missing_paths: result.missing_paths,
		timed_out: result.timed_out,
		truncated: result.timed_out || result.truncated || over_limit,
		result_limit_reached: (result.truncated || over_limit).then_some(limit),
		partial_match_count,
		timeout_ms,
		projected_text: Str::new(""),
		output_blob: None,
		output_shown_lines: 0,
		output_total_lines: 0,
	}
}

async fn prepare_payload<B: ReadBlobs>(
	result: WalkResult,
	limit: u64,
	timeout_ms: u64,
	blobs: &B,
) -> Result<Payload, Fault> {
	let mut payload = payload(result, limit, timeout_ms);
	let output = spill_truncated_text(render_payload(&payload), blobs)
		.await
		.map_err(|fault| Fault::Blob { message: fault.message().clone() })?;
	payload.projected_text = output.content;
	payload.output_blob = output.blob;
	payload.output_shown_lines = output.shown_lines;
	payload.output_total_lines = output.total_lines;
	Ok(payload)
}

fn render_payload(payload: &Payload) -> String {
	let paths: Vec<&str> = payload
		.matches
		.iter()
		.map(|entry| entry.path.as_ref())
		.collect();
	let missing_note = (!payload.missing_paths.is_empty()).then(|| {
		format!(
			"Skipped missing paths: {}",
			payload
				.missing_paths
				.iter()
				.map(AsRef::as_ref)
				.collect::<Vec<&str>>()
				.join(", ")
		)
	});
	let timeout_note = payload.timed_out.then(|| timeout_notice(payload));

	if paths.is_empty() {
		let mut parts = Vec::with_capacity(3);
		if !payload.timed_out {
			parts.push(String::from("No files found matching pattern"));
		}
		if let Some(note) = timeout_note {
			parts.push(note);
		}
		if let Some(note) = missing_note {
			parts.push(note);
		}
		return parts.join("\n");
	}

	let mut output = format_grouped_paths(&paths);
	let mut notes = Vec::with_capacity(2);
	if let Some(note) = timeout_note {
		notes.push(note);
	}
	if let Some(note) = missing_note {
		notes.push(note);
	}
	if !notes.is_empty() {
		output.push_str("\n\n");
		output.push_str(&notes.join("\n"));
	}
	output
}

fn timeout_notice(payload: &Payload) -> String {
	let seconds = if payload.timeout_ms.is_multiple_of(1_000) {
		(payload.timeout_ms / 1_000).to_string()
	} else {
		format!("{:.1}", payload.timeout_ms as f64 / 1_000.0)
	};
	if payload.partial_match_count > 0 {
		format!(
			"glob timed out after {seconds}s; returning {} partial matches — results are incomplete, \
			 scope to a deeper directory instead of retrying blindly",
			payload.partial_match_count
		)
	} else {
		format!(
			"Glob timed out after {seconds}s before finding any matches — the scan is incomplete, \
			 NOT proof of absence. The walk is bounded by directory size, not pattern width; scope \
			 the search to a deeper directory (e.g. `sub/dir/*.ext` instead of `*.ext` at a huge \
			 root)."
		)
	}
}

const fn done(result: Result<Payload, Fault>) -> Ev<Update, Payload, Fault> {
	let useless = matches!(&result, Ok(payload) if payload.matches.is_empty() && !payload.timed_out);
	Ev::Done(ToolTerminal::Done { result, useless })
}

fn param_event(error: ParamError) -> Ev<Update, Payload, Fault> {
	match error {
		ParamError::Args(issue) if issue.kind == ArgIssueKind::Aborted => {
			Ev::Aborted(Abort::InputDropped)
		},
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn commit_event(error: CommitError) -> Ev<Update, Payload, Fault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn interrupt_event(
	interrupt: Result<omp_tool::Interrupt, InterruptWaitError>,
	closed_reason: &'static str,
) -> Ev<Update, Payload, Fault> {
	match interrupt {
		Ok(interrupt) => Ev::Aborted(Abort::Interrupted { reason: interrupt.reason }),
		Err(InterruptWaitError::Closed) => {
			Ev::Aborted(Abort::Interrupted { reason: Str::new(closed_reason) })
		},
		Err(InterruptWaitError::Protocol(message)) => Ev::Args(protocol_issue(message)),
	}
}

fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed JSON argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"path":"crates/**/*.rs"}}"#)),
		found:    Some(message),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn walk_match(path: &str, modified_ms: u64, is_dir: bool) -> WalkMatch {
		WalkMatch { path: Str::new(path), modified_ms, is_dir }
	}

	#[test]
	fn renderer_scope_is_the_literal_directory_prefix() {
		assert_eq!(display_scope("packages/**/*.{test,spec}.ts"), "packages");
		assert_eq!(display_scope("src/foo*.rs"), "src");
		assert_eq!(display_scope("*.rs"), ".");
		assert_eq!(display_scope("src"), "src");
		assert_eq!(display_scope("src; tests"), ".");
	}

	#[test]
	fn params_default_to_visible_gitignored_walk_and_accept_toggles() {
		let defaults: Params = serde_json::from_str("{}").unwrap();
		assert_eq!(defaults.path, None);
		assert!(defaults.hidden);
		assert!(defaults.gitignore);
		assert_eq!(defaults.limit, None);

		let toggled: Params =
			serde_json::from_str(r#"{"path":"src; tests","hidden":false,"gitignore":false}"#).unwrap();
		assert_eq!(toggled.path.as_deref(), Some("src; tests"));
		assert!(!toggled.hidden);
		assert!(!toggled.gitignore);
	}

	#[test]
	fn walk_request_preserves_multi_root_path_and_toggles() {
		let request = walk_request(sf!("src; tests"), false, false, 17);
		assert_eq!(request.path, "src; tests");
		assert!(!request.hidden);
		assert!(!request.gitignore);
		assert_eq!(request.limit, 17);
		assert_eq!(request.timeout_ms, DEFAULT_TIMEOUT_MS);
	}

	#[test]
	fn payload_is_newest_first_deduplicated_and_hard_limited() {
		let ranked = payload(
			WalkResult {
				matches:       vec![
					walk_match("src/old.rs", 10, false),
					walk_match("src/new.rs", 30, false),
					walk_match("docs/generated", 40, true),
					walk_match("src/mid.rs", 20, false),
					walk_match("src/new.rs", 1, false),
				],
				missing_paths: Vec::new(),
				timed_out:     false,
				truncated:     false,
			},
			3,
			DEFAULT_TIMEOUT_MS,
		);
		assert_eq!(
			ranked
				.matches
				.iter()
				.map(|entry| entry.path.as_str())
				.collect::<Vec<_>>(),
			["docs/generated/", "src/new.rs", "src/mid.rs"]
		);
		assert_eq!(ranked.partial_match_count, 4);
		assert_eq!(ranked.result_limit_reached, Some(3));
		assert!(ranked.truncated);
		assert_eq!(
			render_payload(&ranked),
			["# docs/generated/", "# src/", "new.rs", "mid.rs"].join("\n")
		);
	}

	#[test]
	fn limit_is_positive_floored_and_capped() {
		assert_eq!(effective_limit(None), Ok(DEFAULT_LIMIT));
		assert_eq!(effective_limit(Some(3.9)), Ok(3));
		assert_eq!(effective_limit(Some(10_000.0)), Ok(MAX_LIMIT));
		assert_eq!(effective_limit(Some(0.0)), Err(Fault::InvalidLimit));
		assert_eq!(effective_limit(Some(f64::NAN)), Err(Fault::InvalidLimit));
	}
}
