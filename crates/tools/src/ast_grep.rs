//! Multi-target structural search with stable pagination and hashline
//! locations.

use std::{error, fmt, fmt::Display, fs, path::PathBuf, sync::Arc};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, DocEffects, Effects, Ev, IncomingParams,
	ParamError, Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
/// Agent-supplied structural search arguments.
pub struct Params {
	/// Ast-grep structural pattern, including any metavariables to bind.
	pub pat:    Str,
	#[serde(default)]
	/// Semicolon-separated workspace-relative files, directories, or globs;
	/// defaults to `"."`.
	pub path:   Option<Str>,
	#[serde(default)]
	/// Zero-based result offset at which to start the page; defaults to `0`.
	pub cursor: usize,
	#[serde(default)]
	/// Maximum matches in the page; defaults to 100 and is clamped to 1–500.
	pub limit:  Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// One structural source match returned to the agent.
pub struct Match {
	/// Workspace-relative path of the matched source file.
	pub path:       Str,
	/// One-based source line at which the matched node starts.
	pub line:       usize,
	/// One-based source column at which the matched node starts.
	pub column:     usize,
	/// One-based source line at which the matched node ends.
	pub end_line:   usize,
	/// One-based source column at which the matched node ends.
	pub end_column: usize,
	/// Exact source text covered by the matched AST node.
	pub text:       Str,
	/// Stable, display-ready metavariable bindings (`$A=value, $B=value`).
	pub bindings:   Str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Non-fatal reason a targeted file could not be searched.
pub struct Advisory {
	/// Workspace-relative path of the skipped target.
	pub path:    Str,
	/// Language-resolution, pattern-compilation, or file-read explanation.
	pub message: Str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Paginated structural-search result returned to the agent.
pub struct Payload {
	/// Current page of matches in stable path and source order.
	pub matches:     Vec<Match>,
	/// Per-file failures that did not prevent other targets from being searched.
	pub advisories:  Vec<Advisory>,
	/// Number of matches across all targets before pagination.
	pub total:       usize,
	/// Zero-based offset for the next page, or `None` when this is the final
	/// page.
	pub next_cursor: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Empty update type because structural search emits only a terminal result.
pub enum Update {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Terminal argument, target-discovery, or search failure.
pub struct Fault {
	message: Str,
}
impl Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}
impl error::Error for Fault {}

/// Workspace-scoped structural-search tool exposed as `ast_grep`.
pub struct AstGrep {
	root: PathBuf,
	spec: ToolSpec,
}

/// Builds an `ast_grep` tool whose relative files and globs resolve under
/// `root`.
pub fn tool(root: PathBuf) -> AstGrep {
	AstGrep {
		root,
		spec: ToolSpec {
			name:            sf!("ast_grep"),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(
				"Searches multiple files structurally with ast-grep metavariables. `path` accepts \
				 semicolon-separated files, directories, and globs. Results use stable path/source \
				 ordering; `cursor` resumes pagination."
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
				include_bytes!("ast_grep.rs"),
			)
			.into(),
		},
	}
}

impl Tool for AstGrep {
	type Fault = Fault;
	type Params = Params;
	type Payload = Payload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, Payload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<Params>().await { Ok(v) => v, Err(e) => { yield param_event(e); return; } };
			if params.pat.trim().is_empty() { yield done(Err(Fault { message: sf!("pat must not be empty") })); return; }
			if let Err(error) = incoming.interruptable().committed().await { yield commit_event(error); return; }
			let targets = params.path.as_deref().unwrap_or(".").split(';').map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect::<Vec<_>>();
			let files = match omp_ast::ops::collect_matched_files(&self.root, &targets) { Ok(v) => v, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
			let mut matches = Vec::new();
			let mut advisories = Vec::new();
			for file in files {
				let language = match omp_ast::ops::resolve_language(None, &file.absolute_path) { Ok(v) => v, Err(e) => { advisories.push(Advisory { path: file.relative_path, message: Str::new(e.to_string()) }); continue; } };
				let patterns = match omp_ast::ops::compile_search_patterns(&params.pat, language) { Ok(v) => v, Err(e) => { advisories.push(Advisory { path: file.relative_path, message: Str::new(e.to_string()) }); continue; } };
				let source = match fs::read_to_string(&file.absolute_path) { Ok(v) => v, Err(e) => { advisories.push(Advisory { path: file.relative_path, message: Str::new(e.to_string()) }); continue; } };
				for found in omp_ast::ops::collect_matches(&source, language, &patterns) {
					matches.push(Match { path: file.relative_path.clone(), line: found.line, column: found.column, end_line: found.end_line, end_column: found.end_column, text: found.text, bindings: render_bindings(&found.bindings) });
				}
			}
			matches.sort_unstable_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)).then(a.column.cmp(&b.column)));
			let total = matches.len();
			let start = params.cursor.min(total);
			let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
			let end = start.saturating_add(limit).min(total);
			let page = matches.drain(start..end).collect();
			yield done(Ok(Payload { matches: page, advisories, total, next_cursor: (end < total).then_some(end) }));
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		let text = match view {
			Err(e) => Str::new(e.to_string()),
			Ok(payload) => {
				let mut out = String::new();
				for found in &payload.matches {
					use std::fmt::Write as _;
					let _ =
						writeln!(out, "{}:{}:{}\n{}", found.path, found.line, found.column, found.text);
					if !found.bindings.is_empty() {
						let _ = writeln!(out, "  meta: {}", found.bindings);
					}
				}
				for advisory in &payload.advisories {
					use std::fmt::Write as _;
					let _ = writeln!(out, "[advisory {}] {}", advisory.path, advisory.message);
				}
				if let Some(cursor) = payload.next_cursor {
					use std::fmt::Write as _;
					let _ = writeln!(out, "[next cursor: {cursor}; total: {}]", payload.total);
				}
				Str::new(out)
			},
		};
		vec![Part::Text { text }]
	}
}
fn render_bindings(bindings: &[omp_ast::ops::AstBinding]) -> Str {
	Str::new(
		bindings
			.iter()
			.map(|binding| format!("{}={}", binding.name, binding.value))
			.collect::<Vec<_>>()
			.join(", "),
	)
}
fn done(result: Result<Payload, Fault>) -> Ev<Update, Payload, Fault> {
	Ev::Done(ToolTerminal::Done {
		useless: result.as_ref().is_ok_and(|p| p.matches.is_empty()),
		result,
	})
}
#[cfg(test)]
mod tests {
	use omp_ast::ops::AstBinding;

	use super::*;

	#[test]
	fn renders_metavariable_bindings_in_stable_order() {
		let bindings = [AstBinding { name: sf!("$NAME"), value: sf!("answer") }, AstBinding {
			name:  sf!("$VALUE"),
			value: sf!("42"),
		}];
		assert_eq!(render_bindings(&bindings), "$NAME=answer, $VALUE=42");
	}
}
fn param_event(error: ParamError) -> Ev<Update, Payload, Fault> {
	match error {
		ParamError::Args(v) => Ev::Args(*v),
		ParamError::Interrupted(v) => Ev::Aborted(Abort::Interrupted { reason: v.reason }),
		ParamError::Protocol(v) => Ev::Args(issue(v)),
	}
}
fn commit_event(error: CommitError) -> Ev<Update, Payload, Fault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(v) => Ev::Aborted(Abort::Interrupted { reason: v.reason }),
		CommitError::Protocol(v) => Ev::Args(issue(v)),
	}
}
fn issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed JSON argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  None,
		found:    Some(message),
	}
}
