//! Multi-file structural rewrites with dry-run validation and recovery
//! snapshots.
use std::{
	collections::HashSet,
	error,
	fmt::{self, Display},
	fs, io,
	path::{Path, PathBuf},
	str,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use futures::Stream;
use omp_core::{Hash32, Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, DocEffects, Effects, Ev, IncomingParams,
	ParamError, Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::staging::{
	PROPOSAL_PENDING_NOTICE, ProposalActionError, ProposalDecision, ProposalError,
	ProposalRejection, StagedProposalAction, StagedProposalRegistry,
};

const MAX_FILES: usize = 200;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
/// One ordered ast-grep pattern and replacement-template pair.
pub struct RewriteOp {
	/// Structural AST pattern whose metavariables may be reused by the
	/// replacement.
	pub pat: Str,
	/// Replacement template substituted for every match of `pat`.
	pub out: Str,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
/// Agent-supplied structural rewrite proposal.
pub struct Params {
	/// Required non-empty operations applied in order to every compatible
	/// target.
	pub ops:   Vec<RewriteOp>,
	/// Required workspace-relative files, directories, or globs selecting at
	/// most 200 files.
	pub paths: Vec<Str>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Per-file change summary for a staged proposal or finalized application.
pub struct ChangedFile {
	/// Workspace-relative path that the proposal would change or has changed.
	pub path:         Str,
	/// Number of structural matches replaced in this file.
	pub replacements: u32,
	/// Twelve-hex-character prefix of the original content's BLAKE3 digest.
	pub before_hash:  Str,
	/// Twelve-hex-character prefix of the proposed content's BLAKE3 digest.
	pub after_hash:   Str,
	/// Stable numbered source diff for this file.
	pub diff:         Str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Non-fatal reason a targeted file was omitted from the proposal.
pub struct Advisory {
	/// Workspace-relative path of the skipped target.
	pub path:    Str,
	/// Language-resolution, rule-compilation, or encoding explanation.
	pub message: Str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Structural-rewrite result before or after proposal resolution.
pub struct Payload {
	/// Proposed files while staged, or files written after `resolve` applies the
	/// proposal.
	pub files:            Vec<ChangedFile>,
	/// Per-file skips encountered while constructing the staged proposal.
	pub advisories:       Vec<Advisory>,
	/// Recovery-snapshot directory created on apply; `None` while the proposal
	/// is staged.
	pub recovery_root:    Option<Str>,
	/// Uncommitted proposal identity requiring resolve or reject.
	pub pending_proposal: Option<Str>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Empty update type because structural rewrites emit only a terminal result.
pub enum Update {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Terminal validation, target-discovery, staging, or rewrite failure.
pub struct Fault {
	message: Str,
}
impl Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}
impl error::Error for Fault {}

/// Workspace-scoped structural-rewrite tool exposed as `ast_edit`.
pub struct AstEdit {
	root:      PathBuf,
	spec:      ToolSpec,
	proposals: StagedProposalRegistry,
}

/// Builds an `ast_edit` tool that stages proposals in `proposals` for later
/// resolve or reject.
pub fn tool(root: PathBuf, proposals: StagedProposalRegistry) -> AstEdit {
	AstEdit {
		root,
		proposals,
		spec: ToolSpec {
			name:            sf!("ast_edit"),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(
				"Stages structural ast-grep rewrites across mixed-language targets. Every rewrite is \
				 dry-run first; duplicate patterns and more than 200 files are rejected. Source \
				 hashes are rechecked immediately before an all-file commit, and recovery snapshots \
				 are retained under the project .omp state."
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects {
				documents: Some(DocEffects {
					read:        true,
					write_globs: [sf!("**")].into_iter().collect::<Arc<_>>(),
				}),
				exec:      None,
				inference: None,
				desktop:   None,
				subagents: 0,
			},
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("ast_edit.rs"),
			)
			.into(),
		},
	}
}

struct Prepared {
	absolute:     PathBuf,
	relative:     Str,
	original:     Vec<u8>,
	updated:      String,
	replacements: u32,
	before:       [u8; 32],
	after:        [u8; 32],
}

impl Tool for AstEdit {
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
			if params.ops.is_empty() || params.paths.is_empty() { yield done(Err(fault("ops and paths must not be empty"))); return; }
			let mut unique = HashSet::with_capacity(params.ops.len());
			if params.ops.iter().any(|op| op.pat.trim().is_empty() || !unique.insert(op.pat.clone())) { yield done(Err(fault("rewrite patterns must be non-empty and unique"))); return; }
			if let Err(error) = incoming.interruptable().committed().await { yield commit_event(error); return; }
			let target_patterns = params.paths.iter().map(ToString::to_string).collect::<Vec<_>>();
			let files = match omp_ast::ops::collect_matched_files(&self.root, &target_patterns) { Ok(v) => v, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
			if files.len() > MAX_FILES { yield done(Err(fault("ast_edit target exceeds the 200-file hard cap"))); return; }
			let root = match self.root.canonicalize() { Ok(v) => v, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
			let mut prepared = Vec::new(); let mut advisories = Vec::new();
			for file in files {
				let absolute = match file.absolute_path.canonicalize() { Ok(v) if v.starts_with(&root) => v, Ok(_) => { yield done(Err(fault("ast_edit target escapes the workspace root"))); return; }, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
				let language = match omp_ast::ops::resolve_language(None, &absolute) { Ok(v) => v, Err(e) => { advisories.push(Advisory { path: file.relative_path, message: Str::new(e.to_string()) }); continue; } };
				let rules_input = params.ops.iter().map(|op| (op.pat.to_string(), op.out.to_string())).collect::<Vec<_>>();
				let rules = match omp_ast::ops::compile_rewrite_rules(&rules_input, language) { Ok(v) => v, Err((index, e)) => { advisories.push(Advisory { path: file.relative_path, message: sf!("operation {} does not parse for this language: {}", index + 1, e) }); continue; } };
				let original = match fs::read(&absolute) { Ok(v) => v, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
				let source = match str::from_utf8(&original) { Ok(v) => v, Err(_) => { advisories.push(Advisory { path: file.relative_path, message: sf!("non-UTF-8 file skipped") }); continue; } };
				let (updated, replacements) = match omp_ast::ops::rewrite_source(source, language, &rules) { Ok(v) => v, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
				if replacements != 0 { prepared.push(Prepared { absolute, relative: file.relative_path, before: *Hash32::sum(&original).as_bytes(), after: *Hash32::sum(updated.as_bytes()).as_bytes(), original, updated, replacements }); }
			}
			if prepared.is_empty() { yield done(Ok(Payload { files: Vec::new(), advisories, recovery_root: None, pending_proposal: None })); return; }
			let files = prepared.iter().map(|p| ChangedFile { path: p.relative.clone(), replacements: p.replacements, before_hash: short_hash(&p.before), after_hash: short_hash(&p.after), diff: prepared_diff(p) }).collect::<Vec<_>>();
			let summary = sf!("Pending proposal: ast_edit would change {} file(s).", files.len());
			let pending = match self.proposals.stage(
				sf!("ast_edit"),
				summary,
				AstEditAction { root, prepared },
			).await {
				Ok(pending) => pending,
				Err(error) => {
					yield done(Err(Fault { message: Str::new(error.to_string()) }));
					return;
				},
			};
			yield done(Ok(Payload {
				files,
				advisories,
				recovery_root: None,
				pending_proposal: Some(pending.id),
			}));
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Err(e) => Str::new(e.to_string()),
				Ok(p) => {
					let mut out = String::new();
					for file in &p.files {
						use std::fmt::Write as _;
						let _ = writeln!(
							out,
							"{}: {} replacements ({} -> {})",
							file.path, file.replacements, file.before_hash, file.after_hash
						);
					}
					for advisory in &p.advisories {
						use std::fmt::Write as _;
						let _ = writeln!(out, "[advisory {}] {}", advisory.path, advisory.message);
					}
					if p.pending_proposal.is_some() {
						out.push_str(PROPOSAL_PENDING_NOTICE);
					}
					Str::new(out)
				},
			},
		}]
	}
}

fn snapshot_all(root: &Path, prepared: &[Prepared]) -> io::Result<()> {
	for item in prepared {
		let target = root.join(item.relative.as_str());
		if let Some(parent) = target.parent() {
			fs::create_dir_all(parent)?;
		}
		fs::write(target, &item.original)?;
	}
	Ok(())
}
struct AstEditAction {
	root:     PathBuf,
	prepared: Vec<Prepared>,
}

impl StagedProposalAction for AstEditAction {
	fn finalize(&mut self, decision: &ProposalDecision) -> Result<serde_json::Value, ProposalError> {
		if matches!(
			decision,
			ProposalDecision::Reject(
				ProposalRejection::Requested { .. } | ProposalRejection::RegimeLimitReached
			)
		) {
			return Ok(serde_json::json!({ "rejected": true }));
		}
		self.apply().map_err(ProposalError::from)
	}
}

impl AstEditAction {
	fn apply(&mut self) -> Result<serde_json::Value, ProposalActionError> {
		for item in &self.prepared {
			let current = fs::read(&item.absolute)
				.map_err(|source| ProposalActionError::Io { path: item.absolute.clone(), source })?;
			if Hash32::sum(&current).as_bytes() != &item.before {
				return Err(ProposalActionError::RevisionChanged { path: item.absolute.clone() });
			}
		}
		let generation = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map_or(0, |duration| duration.as_nanos());
		let recovery = self
			.root
			.join(".omp/recovery/ast-edit")
			.join(generation.to_string());
		snapshot_all(&recovery, &self.prepared)
			.map_err(|source| ProposalActionError::Io { path: recovery.clone(), source })?;
		let mut committed = 0;
		for item in &self.prepared {
			let temporary = item
				.absolute
				.with_extension(format!("omp-ast-edit-{generation}"));
			let result = fs::write(&temporary, item.updated.as_bytes())
				.and_then(|()| fs::rename(&temporary, &item.absolute));
			if let Err(source) = result {
				for restore in self.prepared[..committed].iter().rev() {
					let _ = fs::write(&restore.absolute, &restore.original);
				}
				return Err(ProposalActionError::Io { path: item.absolute.clone(), source });
			}
			committed += 1;
		}
		let files = self
			.prepared
			.iter()
			.map(|prepared| ChangedFile {
				path:         prepared.relative.clone(),
				replacements: prepared.replacements,
				before_hash:  short_hash(&prepared.before),
				after_hash:   short_hash(&prepared.after),
				diff:         prepared_diff(prepared),
			})
			.collect();
		Ok(serde_json::to_value(Payload {
			files,
			advisories: Vec::new(),
			recovery_root: Some(Str::from(recovery.to_string_lossy().into_owned())),
			pending_proposal: None,
		})?)
	}
}

fn prepared_diff(prepared: &Prepared) -> Str {
	omp_hashline::numbered_diff(
		&prepared.original,
		prepared.updated.as_bytes(),
		Some(Path::new(prepared.relative.as_str())),
	)
	.map_or_else(|_| Str::new(""), |diff| diff.text)
}

fn short_hash(hash: &[u8; 32]) -> Str {
	use omp_core::encoding::hex;
	let mut out = [0_u8; 16];
	let count = hex::encode_mut(hash, &mut out);
	Str::new(str::from_utf8(&out[..count.min(12)]).expect("hex is UTF-8"))
}
fn fault(message: &'static str) -> Fault {
	Fault { message: Str::new_static(message) }
}
fn done(result: Result<Payload, Fault>) -> Ev<Update, Payload, Fault> {
	Ev::Done(ToolTerminal::Done {
		useless: result.as_ref().is_ok_and(|p| p.files.is_empty()),
		result,
	})
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
#[cfg(test)]
mod tests {
	use super::*;

	fn action(root: &Path, path: &Path, original: &[u8], updated: &str) -> AstEditAction {
		AstEditAction {
			root:     root.to_path_buf(),
			prepared: vec![Prepared {
				absolute:     path.to_path_buf(),
				relative:     Str::new("sample.rs"),
				original:     original.to_vec(),
				updated:      updated.to_owned(),
				replacements: 1,
				before:       *Hash32::sum(original).as_bytes(),
				after:        *Hash32::sum(updated.as_bytes()).as_bytes(),
			}],
		}
	}

	#[test]
	fn staged_action_mutates_only_after_resolve_and_regime_limit_is_effect_free() {
		let temp = tempfile::tempdir().expect("temporary workspace");
		let path = temp.path().join("sample.rs");
		let original = b"fn old() {}\n";
		fs::write(&path, original).expect("seed source");

		let mut rejected = action(temp.path(), &path, original, "fn new() {}\n");
		rejected
			.finalize(&ProposalDecision::Reject(ProposalRejection::RegimeLimitReached))
			.expect("proposal rejected");
		assert_eq!(fs::read(&path).expect("source readable"), original);

		let mut resolved = action(temp.path(), &path, original, "fn new() {}\n");
		let payload = resolved
			.finalize(&ProposalDecision::Resolve {
				reason: Str::new_static("Apply the reviewed rewrite."),
			})
			.expect("proposal resolved");
		assert_eq!(fs::read_to_string(&path).expect("source readable"), "fn new() {}\n");
		assert_eq!(payload["files"][0]["path"], "sample.rs");
		assert!(payload["recovery_root"].as_str().is_some());
	}
}
