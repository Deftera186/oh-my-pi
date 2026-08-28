//! Apply-patch, patch, and sloppy edit revisions over `EditDocuments`.

use std::path::Path;

use async_stream::stream;
use bytes::Bytes;
use futures::{FutureExt, Stream, pin_mut, select_biased};
use omp_core::{Str, sf};
use omp_hashline::{
	diff_preview::CompactDiffOptions,
	foreign_patch::{ForeignPatchFile, parse_foreign_patch},
	sloppy::{apply_sloppy_detailed, split_sloppy_sections},
	unified_hunk::apply_file_operation,
};
use omp_tool::{
	Abort, Constraint, Dialect, DocEffects, Effects, Ev, IncomingParams, InterruptWaitError, Part,
	PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;

use super::{
	AppliedOp, CommittedSection, EditAction, EditCommitError, EditDocuments, EditPrepared,
	EditProposal, EditUpdate, Fault, FormatPolicy, Payload, PrepareRequest, ResolvedEdit, SectionOp,
	SectionPayload, StalePolicy, commit_event, done_fault,
	observer::{AppliedEditSnapshot, EditObserver, PendingBlackbox},
	param_event, rejection_text, warn_edit_rejection,
};
use crate::{
	path::{HostPaths, normalize_target},
	render::TextProjection,
};
const SLOPPY_DESCRIPTION: &str = include_str!("../sloppy_prompt.txt");

/// Freeform arguments shared by patch-envelope and sloppy revisions.
///
/// Unknown provider-attached keys are deliberately ignored; only `input` is
/// canonicalized into the recorded tool call.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct FreeformEditParams {
	/// Complete dialect input.
	pub input: Str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FreeformKind {
	Patch,
	ApplyPatch,
	Sloppy,
}

impl FreeformKind {
	const fn family(self) -> &'static str {
		match self {
			Self::Patch => "patch",
			Self::ApplyPatch => "apply_patch",
			Self::Sloppy => "sloppy",
		}
	}

	const fn dialect(self) -> Dialect {
		match self {
			Self::Patch => Dialect::Patch,
			Self::ApplyPatch => Dialect::ApplyPatch,
			Self::Sloppy => Dialect::Sloppy,
		}
	}

	const fn description(self) -> &'static str {
		match self {
			Self::Patch => "Apply a Codex begin/add/update/move/delete patch envelope atomically.",
			Self::ApplyPatch => {
				"Apply a Codex begin/add/update/move/delete patch envelope atomically."
			},
			Self::Sloppy => SLOPPY_DESCRIPTION,
		}
	}
}

/// A freeform edit revision.
pub struct FreeformEditTool<D> {
	documents:       D,
	format_policy:   FormatPolicy,
	kind:            FreeformKind,
	observer:        EditObserver,
	guard_generated: bool,
	spec:            ToolSpec,
}

/// Returns the host-free `edit@patch.1` specification.
pub fn patch_spec() -> ToolSpec {
	freeform_spec(FreeformKind::Patch)
}

/// Returns the host-free `edit@apply_patch.1` specification.
pub fn apply_patch_spec() -> ToolSpec {
	freeform_spec(FreeformKind::ApplyPatch)
}

/// Returns the host-free `edit@sloppy.1` specification.
pub fn sloppy_spec() -> ToolSpec {
	freeform_spec(FreeformKind::Sloppy)
}

fn freeform_spec(kind: FreeformKind) -> ToolSpec {
	ToolSpec {
		name:            sf!("edit"),
		rev:             Rev { family: Str::new_static(kind.family()), n: 1 },
		description:     Str::new_static(kind.description()),
		schema:          omp_tool::schema::<FreeformEditParams>(),
		constraint:      Constraint::Grammar {
			priority:       100,
			syntax:         omp_tool::GrammarSyntax::Lark,
			definition:     Str::new_static(match kind.dialect() {
				Dialect::Patch | Dialect::ApplyPatch => omp_hashline::grammars::APPLY_PATCH,
				Dialect::Sloppy => omp_hashline::grammars::SLOPPY,
				Dialect::Hashline | Dialect::Replace | Dialect::Native => "",
			}),
			on_unsupported: omp_tool::Fallback::Unspecified,
		},
		effects:         Effects {
			documents: Some(DocEffects {
				read:        true,
				write_globs: [sf!("**")].into_iter().collect(),
			}),
			exec:      None,
			inference: None,
			desktop:   None,
			subagents: 0,
		},
		projection_code: omp_tool::native_projection_code(
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			include_bytes!("apply_patch.rs"),
		)
		.into(),
	}
}

/// Constructs `edit@patch.1`.
pub fn patch_tool<D: EditDocuments>(
	documents: D,
	format_policy: FormatPolicy,
) -> FreeformEditTool<D> {
	patch_tool_with_observer(documents, format_policy, EditObserver::default(), true)
}

/// Constructs `edit@patch.1` with syntax observation.
pub fn patch_tool_with_observer<D: EditDocuments>(
	documents: D,
	format_policy: FormatPolicy,
	observer: EditObserver,
	guard_generated: bool,
) -> FreeformEditTool<D> {
	new_tool(documents, format_policy, FreeformKind::Patch, observer, guard_generated)
}

/// Constructs `edit@apply_patch.1`.
pub fn apply_patch_tool<D: EditDocuments>(
	documents: D,
	format_policy: FormatPolicy,
) -> FreeformEditTool<D> {
	apply_patch_tool_with_observer(documents, format_policy, EditObserver::default(), true)
}

/// Constructs `edit@apply_patch.1` with syntax observation.
pub fn apply_patch_tool_with_observer<D: EditDocuments>(
	documents: D,
	format_policy: FormatPolicy,
	observer: EditObserver,
	guard_generated: bool,
) -> FreeformEditTool<D> {
	new_tool(documents, format_policy, FreeformKind::ApplyPatch, observer, guard_generated)
}

/// Constructs `edit@sloppy.1`.
pub fn sloppy_tool<D: EditDocuments>(
	documents: D,
	format_policy: FormatPolicy,
) -> FreeformEditTool<D> {
	sloppy_tool_with_observer(documents, format_policy, EditObserver::default(), true)
}

/// Constructs `edit@sloppy.1` with syntax observation.
pub fn sloppy_tool_with_observer<D: EditDocuments>(
	documents: D,
	format_policy: FormatPolicy,
	observer: EditObserver,
	guard_generated: bool,
) -> FreeformEditTool<D> {
	new_tool(documents, format_policy, FreeformKind::Sloppy, observer, guard_generated)
}

fn new_tool<D: EditDocuments>(
	documents: D,
	format_policy: FormatPolicy,
	kind: FreeformKind,
	observer: EditObserver,
	guard_generated: bool,
) -> FreeformEditTool<D> {
	FreeformEditTool {
		documents,
		format_policy,
		kind,
		observer,
		guard_generated,
		spec: match kind {
			FreeformKind::Patch => patch_spec(),
			FreeformKind::ApplyPatch => apply_patch_spec(),
			FreeformKind::Sloppy => sloppy_spec(),
		},
	}
}

#[derive(Clone, Debug)]
enum AuthoredOperation {
	Foreign(ForeignPatchFile),
	Sloppy { path: Str, input: Str },
}

impl AuthoredOperation {
	fn path(&self) -> &str {
		match self {
			Self::Foreign(operation) => operation.path(),
			Self::Sloppy { path, .. } => path,
		}
	}
}

struct Work<P> {
	op:       AuthoredOperation,
	prepared: P,
}

struct Projection {
	after:     Option<Bytes>,
	operation: SectionOp,
	move_dest: Option<Str>,
	resolved:  Vec<ResolvedEdit>,
	warnings:  Vec<Str>,
}

impl<D: EditDocuments> Tool for FreeformEditTool<D> {
	type Fault = Fault;
	type Params = FreeformEditParams;
	type Payload = Payload;
	type Update = EditUpdate;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut params: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<EditUpdate, Payload, Fault>> + Send + 'c {
		let span = tracing::debug_span!(
			"edit_execution",
			revision = self.kind.family(),
			path_count = tracing::field::Empty,
			path = tracing::field::Empty,
		);
		stream! {
			let FreeformEditParams { input } = match params.whole::<FreeformEditParams>().await {
				Ok(params) => params,
				Err(error) => { yield param_event(error); return; },
			};
			let operations = match parse_operations(self.kind, &input) {
				Ok(operations) if !operations.is_empty() => operations,
				Ok(_) => { yield done_fault(Fault::invalid("No edit operations found.")); return; },
				Err(error) => { yield done_fault(Fault::invalid(error)); return; },
			};
			span.record("path_count", operations.len());
			if let Some(operation) = operations.first() {
				span.record("path", tracing::field::display(operation.path()));
			}
			let mut works = Vec::with_capacity(operations.len());
			for mut op in operations {
				let normalized = normalize_target(op.path(), None, HostPaths::current());
				match &mut op {
					AuthoredOperation::Foreign(ForeignPatchFile::Add { path, .. })
					| AuthoredOperation::Foreign(ForeignPatchFile::Delete { path })
					| AuthoredOperation::Foreign(ForeignPatchFile::Update { path, .. })
					| AuthoredOperation::Sloppy { path, .. } => *path = normalized.canonical,
				}
				let prepared = match self.documents.prepare(PrepareRequest {
					path: Str::new(op.path()),
					file_hash: None,
					anchor_lines: Vec::new(),
					allow_unpinned: true,
					allow_missing: matches!(
						op,
						AuthoredOperation::Foreign(ForeignPatchFile::Add { .. })
					),
					guard_generated: self.guard_generated,
				}).instrument(span.clone()).await {
					Ok(prepared) => prepared,
					Err(fault) => { yield done_fault(fault); return; },
				};
				if works.iter().any(|work: &Work<D::Prepared>| work.prepared.path() == prepared.path()) {
					yield done_fault(Fault::invalid("Multiple operations resolve to the same file; merge repeated sloppy sections or use one apply-patch file hunk."));
					return;
				}
				works.push(Work { op, prepared });
			}

			let mut proposals = Vec::with_capacity(works.len());
			let mut projections = Vec::with_capacity(works.len());
			let mut pending_blackbox = Vec::<Option<PendingBlackbox>>::with_capacity(works.len());
			let observer_args = serde_json::to_value(FreeformEditParams { input: input.clone() })
				.unwrap_or_default();
			for work in &works {
				let source = match std::str::from_utf8(work.prepared.authored_bytes()) {
					Ok(source) => source,
					Err(_) => { yield done_fault(Fault::invalid("edit dialect requires UTF-8 text")); return; },
				};
				let (mut after, operation, move_dest, mut warnings) = match &work.op {
					AuthoredOperation::Sloppy { input, path } => {
						match apply_sloppy_detailed(source, input, Some(path)) {
							Ok(applied) => (
								Some(Bytes::from(applied.content)),
								SectionOp::Update,
								None,
								applied.notes,
							),
							Err(error) => { yield done_fault(Fault::invalid(error.to_string())); return; },
						}
					},
					AuthoredOperation::Foreign(operation) => match apply_file_operation(
						work.prepared.exists().then_some(source),
						operation,
						false,
					) {
						Ok(after) => {
							let section_op = match operation {
								ForeignPatchFile::Delete { .. } => SectionOp::Delete,
								ForeignPatchFile::Update { move_to: Some(_), .. } => SectionOp::Move,
								ForeignPatchFile::Add { .. } | ForeignPatchFile::Update { .. } => SectionOp::Update,
							};
							let move_dest = match operation {
								ForeignPatchFile::Update { move_to, .. } => move_to.clone(),
								_ => None,
							};
							(after.map(Bytes::from), section_op, move_dest, Vec::new())
						},
						Err(error) => { yield done_fault(Fault::invalid(error.to_string())); return; },
					},
				};
				let mut pending = None;
				if work.prepared.exists() && operation != SectionOp::Delete
					&& let Some(content) = after.take()
				{
					let target = move_dest.clone().unwrap_or_else(|| work.prepared.path().clone());
					let inspected = self.observer.inspect(
						AppliedEditSnapshot {
							path: target,
							before: work.prepared.base_bytes().clone(),
							after: content,
						},
						self.kind.family(),
						&observer_args,
					).instrument(span.clone()).await;
					after = Some(inspected.content);
					warnings.extend(inspected.notice);
					pending = inspected.pending;
				}
				pending_blackbox.push(pending);
				let action = match (operation, after.clone(), move_dest.clone()) {
					(SectionOp::Delete, _, _) => EditAction::Delete,
					(SectionOp::Move, Some(content), Some(destination)) => EditAction::Move { destination, content },
					(_, Some(content), _) => EditAction::Write { content },
					_ => { yield done_fault(Fault::invalid("invalid edit operation state")); return; },
				};
				proposals.push(EditProposal {
					action: action.clone(),
					base_revision: work.prepared.base_revision().clone(),
					stale_policy: StalePolicy::RebaseNonOverlapping,
					format_policy: self.format_policy,
				});
				let resolved = after.as_ref().map_or_else(Vec::new, |after| vec![ResolvedEdit {
					start: 1,
					end: source.lines().count().max(1),
					body: String::from_utf8_lossy(after).lines().map(Str::new).collect(),
				}]);
				projections.push(Projection { after, operation, move_dest, resolved, warnings });
			}

			let (preview, added_lines, removed_lines) = preview(&works, &projections);
			yield Ev::Update(EditUpdate { applied_ops: projections.len(), paths: works.iter().map(|work| work.prepared.display_path().clone()).collect(), preview, added_lines, removed_lines });
			match params.committed().await {
				Ok(_) => {},
				Err(error) => { yield commit_event(error); return; },
			}

			let result = {
				let clipboard = self.documents.start_clipboard_batch();
				let prepared = works.iter_mut().map(|work| &mut work.prepared).collect();
				let commit = self.documents.commit(prepared, proposals, clipboard).instrument(span.clone()).fuse();
				let interrupt = params.next_interrupt().fuse();
				pin_mut!(commit, interrupt);
				select_biased! {
					result = commit => Some(result),
					interrupted = interrupt => { yield Ev::Aborted(match interrupted {
						Ok(value) => Abort::EffectsUnknown { reason: value.reason },
						Err(InterruptWaitError::Closed) => Abort::EffectsUnknown { reason: sf!("invocation owner disappeared during transaction") },
						Err(InterruptWaitError::Protocol(reason)) => Abort::EffectsUnknown { reason },
					}); None },
				}
			};
			let Some(result) = result else { return; };
			match result {
				Ok(result) if result.sections.len() == works.len() => {
					for (work, committed) in works.iter().zip(&result.sections) {
						if committed.rebased {
							tracing::warn!(
								parent: &span,
								path = %work.prepared.display_path(),
								"edit transaction rebased a concurrent change",
							);
						}
					}
					for work in &works { self.documents.reset_noop(work.prepared.path()); }
					for pending in pending_blackbox.into_iter().flatten() {
						self.observer.record_committed(pending).await;
					}
					yield Ev::Done(ToolTerminal::Done { result: Ok(payload(&works, &projections, &result.sections)), useless: false });
				},
				Ok(_) => yield Ev::Aborted(Abort::EffectsUnknown { reason: sf!("document transaction returned the wrong section count") }),
				Err(EditCommitError::Rejected(fault)) => {
					warn_edit_rejection(&span, &fault);
					yield done_fault(fault);
				},
				Err(EditCommitError::EffectsUnknown { reason }) => {
					tracing::warn!(parent: &span, "edit commit result is unknown");
					yield Ev::Aborted(Abort::EffectsUnknown { reason });
				},
			}
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, caps: &PromptCaps) -> Vec<Part> {
		let Some(mut out) = TextProjection::new(*caps) else {
			return Vec::new();
		};
		match view {
			Ok(payload) => {
				for section in &payload.sections {
					let _ =
						out.push(&format!("{} edit completed: {}", self.kind.family(), section.path));
				}
			},
			Err(fault) => {
				let _ = out.push(&rejection_text(fault));
			},
		}
		out.finish()
	}
}

fn parse_operations(kind: FreeformKind, input: &str) -> Result<Vec<AuthoredOperation>, String> {
	match kind {
		FreeformKind::Patch | FreeformKind::ApplyPatch => parse_foreign_patch(input)
			.map(|operations| {
				operations
					.into_iter()
					.map(AuthoredOperation::Foreign)
					.collect()
			})
			.map_err(|error| error.to_string()),
		FreeformKind::Sloppy => {
			let mut merged = Vec::<AuthoredOperation>::new();
			for section in split_sloppy_sections(input).map_err(|error| error.to_string())? {
				if let Some(AuthoredOperation::Sloppy { input, .. }) = merged
					.iter_mut()
					.find(|operation| operation.path() == section.path)
				{
					*input = sf!("{}\n{}", input, section.input);
				} else {
					merged.push(AuthoredOperation::Sloppy { path: section.path, input: section.input });
				}
			}
			Ok(merged)
		},
	}
}

fn preview<P: EditPrepared>(works: &[Work<P>], projections: &[Projection]) -> (Str, usize, usize) {
	let mut text = String::new();
	let mut added = 0;
	let mut removed = 0;
	for (work, projection) in works.iter().zip(projections) {
		let after = projection.after.as_deref().unwrap_or_default();
		if let Ok(diff) = omp_hashline::numbered_diff(
			work.prepared.base_bytes(),
			after,
			Some(Path::new(work.prepared.display_path().as_str())),
		) {
			let compact = omp_hashline::diff_preview::build_compact_diff_preview(
				&diff.text,
				CompactDiffOptions::default(),
			);
			if !text.is_empty() && !compact.preview.is_empty() {
				text.push('\n');
			}
			text.push_str(&compact.preview);
			added += compact.added_lines;
			removed += compact.removed_lines;
		}
	}
	(text.into(), added, removed)
}

fn payload<P: EditPrepared>(
	works: &[Work<P>],
	projections: &[Projection],
	committed: &[CommittedSection],
) -> Payload {
	Payload {
		sections: works
			.iter()
			.zip(projections)
			.zip(committed)
			.map(|((work, projection), committed)| {
				let after = committed
					.content
					.clone()
					.or_else(|| projection.after.clone())
					.unwrap_or_default();
				let diff = omp_hashline::numbered_diff(
					work.prepared.base_bytes(),
					&after,
					Some(Path::new(work.prepared.display_path().as_str())),
				)
				.ok();
				let compact = diff.as_ref().map(|diff| {
					omp_hashline::diff_preview::build_compact_diff_preview(
						&diff.text,
						CompactDiffOptions::default(),
					)
				});
				SectionPayload {
					path: work.prepared.display_path().clone(),
					canonical_path: work.prepared.path().clone(),
					op: projection.operation,
					move_dest: projection.move_dest.clone(),
					old_revision: work.prepared.base_revision().clone(),
					new_revision: committed.new_revision.clone(),
					applied_ops: vec![AppliedOp {
						kind:       Str::new_static("rewrite"),
						patch_line: 1,
						index:      0,
					}],
					resolved_edits: projection.resolved.clone(),
					rebased: committed.rebased,
					before: work.prepared.base_bytes().clone(),
					before_blob: None,
					after,
					after_blob: None,
					header: None,
					diff: diff
						.as_ref()
						.map_or_else(Str::default, |diff| diff.text.clone()),
					preview: compact
						.as_ref()
						.map_or_else(Str::default, |compact| compact.preview.clone()),
					first_changed_line: Some(1),
					block_resolutions: Vec::new(),
					warnings: work
						.prepared
						.warnings()
						.iter()
						.cloned()
						.chain(projection.warnings.iter().cloned())
						.collect(),
					diagnostics: committed.diagnostics.clone(),
					diagnostics_complete: committed.diagnostics_complete,
				}
			})
			.collect(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn freeform_schemas_ignore_provider_extras() {
		let params: FreeformEditParams =
			serde_json::from_str(r#"{"input":"x","provider_cache":true}"#).expect("extras ignored");
		assert_eq!(params.input, "x");
	}

	#[test]
	fn repeated_sloppy_sections_merge_in_authored_order() {
		let operations =
			parse_operations(FreeformKind::Sloppy, "§a\nx\n»\ny\n§a\ny\n»\nz").expect("parse");
		assert_eq!(operations.len(), 1);
		let AuthoredOperation::Sloppy { input, .. } = &operations[0] else {
			panic!("sloppy")
		};
		assert!(input.contains("y\n»\nz"));
	}
}
