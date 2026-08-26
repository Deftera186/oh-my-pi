//! The historical replacement dialect and its lossless lift data.

use std::{fmt, path::Path, str};

use async_stream::stream;
use bytes::Bytes;
use futures::{FutureExt, Stream, pin_mut, select_biased};
use omp_core::{Str, sf};
use omp_hashline::{
	ReplaceError, ReplaceOptions, apply_replace,
	diff_preview::{CompactDiffOptions, build_compact_diff_preview},
	format_hashline_header, numbered_diff,
	recovery::recover_exact,
};
use omp_tool::{
	Abort, Constraint, DocEffects, Effects, Ev, IncomingParams, InterruptWaitError, Part,
	PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
	AppliedOp, CommittedSection, EditAction, EditCommitError, EditDocuments, EditPrepared,
	EditProposal, EditUpdate, Fault, FormatPolicy, NoopResult, Payload, PrepareRequest,
	RejectionReason, ResolvedEdit, SectionOp, SectionPayload, StalePolicy, commit_event, done_fault,
	observer::{AppliedEditSnapshot, EditObserver, PendingBlackbox},
	param_event, recovery_edits,
};
use crate::render::TextProjection;

const DESCRIPTION: &str = "Replace exact or uniquely recoverable text in a file. The matcher \
                           preserves BOM and line endings, adapts uniform indentation, and \
                           rejects ambiguous matches with previews.";

/// Arguments emitted by the `edit@rep.1` dialect.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceParams {
	/// One replacement per document snapshot.
	pub edits: Vec<ReplaceOperation>,
}

/// One old-text/new-text edit against an exact document snapshot.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceOperation {
	/// Workspace-relative document path.
	pub path:        Str,
	/// Text to locate using the progressive fallback ladder.
	pub old:         Str,
	/// Text replacing the selected match.
	pub new:         Str,
	/// Replace every independently safe occurrence.
	#[serde(default)]
	pub replace_all: bool,
	/// Disable fuzzy fallback after the exact normalization passes.
	#[serde(default = "default_allow_fuzzy")]
	pub allow_fuzzy: bool,
	/// Fuzzy similarity threshold, when deliberately overridden.
	pub threshold:   Option<f64>,
}

const fn default_allow_fuzzy() -> bool {
	true
}

/// `edit@rep.1` executor retained for small-model dialect selection and as the
/// source side of the `rep.1 -> hl.1` lift.
pub struct ReplaceTool<D> {
	documents:       D,
	format_policy:   FormatPolicy,
	observer:        EditObserver,
	guard_generated: bool,
	spec:            ToolSpec,
}

/// Constructs the old-text/new-text replacement dialect.
pub fn replace_tool<D: EditDocuments>(documents: D, format_policy: FormatPolicy) -> ReplaceTool<D> {
	replace_tool_with_observer(documents, format_policy, EditObserver::default(), true)
}

/// Constructs the replacement dialect with syntax observation.
pub fn replace_tool_with_observer<D: EditDocuments>(
	documents: D,
	format_policy: FormatPolicy,
	observer: EditObserver,
	guard_generated: bool,
) -> ReplaceTool<D> {
	ReplaceTool {
		documents,
		format_policy,
		observer,
		guard_generated,
		spec: ToolSpec {
			name:            sf!("edit"),
			rev:             Rev { family: sf!("rep"), n: 1 },
			description:     sf!(DESCRIPTION),
			schema:          omp_tool::schema::<ReplaceParams>(),
			constraint:      Constraint::Schema {
				priority:       100,
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
				include_bytes!("replace.rs"),
			)
			.into(),
		},
	}
}

struct Work<P> {
	op:       ReplaceOperation,
	prepared: P,
}

struct Projection {
	after:    Bytes,
	resolved: Vec<ResolvedEdit>,
	warnings: Vec<Str>,
}

impl<D: EditDocuments> Tool for ReplaceTool<D> {
	type Fault = Fault;
	type Params = ReplaceParams;
	type Payload = Payload;
	type Update = EditUpdate;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut params: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<EditUpdate, Payload, Fault>> + Send + 'c {
		stream! {
			let replace_params = match params.whole::<ReplaceParams>().await {
				Ok(params) => params,
				Err(error) => { yield param_event(error); return; },
			};
			if replace_params.edits.is_empty() {
				yield done_fault(Fault::invalid("No replacement operations found in edits."));
				return;
			}
			let observer_args = serde_json::to_value(&replace_params).unwrap_or_default();
			let journal_input = if let Ok(input) = serde_json::to_vec(&replace_params) { Bytes::from(input) } else { yield done_fault(Fault::invalid("Replacement arguments could not be journaled.")); return; };
			let mut works = Vec::with_capacity(replace_params.edits.len());
			for op in replace_params.edits {
				let prepared = match self.documents.prepare(PrepareRequest {
					path: op.path.clone(),
					file_hash: None,
					anchor_lines: Vec::new(),
					allow_unpinned: true,
					allow_missing: false,
					guard_generated: self.guard_generated,
				}).await {
					Ok(prepared) => prepared,
					Err(fault) => { yield done_fault(fault); return; },
				};
				if works.iter().any(|work: &Work<D::Prepared>| work.prepared.path() == prepared.path()) {
					yield done_fault(Fault::invalid("Multiple replacement operations resolve to the same file; combine their context into one operation."));
					return;
				}
				works.push(Work { op, prepared });
			}

			let mut proposals = Vec::with_capacity(works.len());
			let mut projections = Vec::with_capacity(works.len());
			let mut pending_blackbox = Vec::<PendingBlackbox>::new();
			for work in &works {
				let result = apply_replace(work.prepared.authored_bytes(), &work.op.old, &work.op.new, ReplaceOptions {
					replace_all: work.op.replace_all,
					allow_fuzzy: work.op.allow_fuzzy,
					threshold: work.op.threshold.unwrap_or(omp_hashline::replace::DEFAULT_FUZZY_THRESHOLD),
				});
				let (after, resolved, recovery_edits) = match result {
					Ok(result) => {
						let resolved = result.edits.iter().map(|edit| ResolvedEdit {
							start: line_at(work.prepared.authored_bytes(), edit.start),
							end: line_at_end(work.prepared.authored_bytes(), edit.start, edit.end),
							body: replacement_body(&edit.replacement),
						}).collect();
						let byte_edits = result
							.edits
							.iter()
							.map(|edit| omp_hashline::ByteEdit {
								start: edit.start,
								end: edit.end,
								replacement: edit.replacement.clone(),
							})
							.collect::<Vec<_>>();
						let recovery_edits = match recovery_edits(&byte_edits) {
							Ok(edits) => edits,
							Err(fault) => { yield done_fault(fault); return; },
						};
						(result.final_bytes, resolved, recovery_edits)
					},
					Err(ReplaceError::NoChanges) => {
						(work.prepared.authored_bytes().clone(), Vec::new(), Vec::new())
					},
					Err(error) => { yield done_fault(Fault::invalid(replacement_error(error))); return; },
				};
				let after = if work.prepared.authored_bytes() == work.prepared.base_bytes() {
					after
				} else if recovery_edits.is_empty() {
					yield done_fault(Fault::stale("The source snapshot changed before this replacement could be applied; re-read the document."));
					return;
				} else if let Ok(recovered) = recover_exact(work.prepared.authored_bytes(), work.prepared.base_bytes(), &recovery_edits) { recovered.content().clone() } else { yield done_fault(Fault::stale("The source snapshot changed and the replacement overlaps intervening edits; re-read the document.")); return; };
				let inspected = self.observer.inspect(
					AppliedEditSnapshot {
						path: work.prepared.path().clone(),
						before: work.prepared.base_bytes().clone(),
						after,
					},
					"replace",
					&observer_args,
				).await;
				let after = inspected.content;
				let warnings = inspected.notice.into_iter().collect();
				pending_blackbox.extend(inspected.pending);
				proposals.push(EditProposal {
					action: EditAction::Write { content: after.clone() },
					base_revision: work.prepared.base_revision().clone(),
					stale_policy: StalePolicy::RebaseNonOverlapping,
					format_policy: self.format_policy,
				});
				projections.push(Projection { after, resolved, warnings });
			}

			let mut preview = String::new();
			let mut added_lines = 0;
			let mut removed_lines = 0;
			for (work, projection) in works.iter().zip(&projections) {
				if let Ok(diff) = numbered_diff(work.prepared.base_bytes(), &projection.after, Some(Path::new(work.prepared.display_path().as_str()))) {
					let compact = build_compact_diff_preview(&diff.text, CompactDiffOptions::default());
					if !preview.is_empty() && !compact.preview.is_empty() { preview.push('\n'); }
					preview.push_str(&compact.preview);
					added_lines += compact.added_lines;
					removed_lines += compact.removed_lines;
				}
			}
			yield Ev::Update(EditUpdate { applied_ops: projections.iter().map(|projection| projection.resolved.len()).sum(), paths: works.iter().map(|work| work.prepared.display_path().clone()).collect(), preview: preview.into(), added_lines, removed_lines });
			match params.committed().await {
				Ok(_) => {},
				Err(error) => { yield commit_event(error); return; },
			}
			if let Some(index) = works.iter().zip(&projections).position(|(work, projection)| work.prepared.base_bytes() == &projection.after) {
				let work = &works[index];
				let NoopResult { diagnostic, escalate } = self.documents.record_noop(work.prepared.path(), work.prepared.display_path(), journal_input);
				if escalate || works.len() != 1 { yield done_fault(Fault::invalid(diagnostic)); return; }
				yield Ev::Done(ToolTerminal::Done { result: Ok(payload(&works, &projections, None)), useless: true });
				return;
			}
			let result = {
				let clipboard = self.documents.start_clipboard_batch();
				let prepared = works.iter_mut().map(|work| &mut work.prepared).collect();
				let commit = self.documents.commit(prepared, proposals, clipboard).fuse();
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
					for work in &works { self.documents.reset_noop(work.prepared.path()); }
					for pending in pending_blackbox {
						self.observer.record_committed(pending).await;
					}
					yield Ev::Done(ToolTerminal::Done { result: Ok(payload(&works, &projections, Some(&result.sections))), useless: false });
				},
				Ok(_) => yield Ev::Aborted(Abort::EffectsUnknown { reason: sf!("document transaction returned the wrong section count") }),
				Err(EditCommitError::Rejected(fault)) => yield done_fault(fault),
				Err(EditCommitError::EffectsUnknown { reason }) => yield Ev::Aborted(Abort::EffectsUnknown { reason }),
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
					let status = if section.op == SectionOp::Noop {
						"matched but changed no bytes"
					} else {
						"updated"
					};
					let _ = out.push(&format!("Replacement {status}: {}", section.path));
				}
			},
			Err(fault) => match &fault.reason {
				RejectionReason::Conflict => {
					let _ = out.push("Edit rejected: conflict");
				},
				RejectionReason::StaleUnrecoverable { message }
				| RejectionReason::Format { message }
				| RejectionReason::InvalidPatch { message } => {
					let _ = out.push(message);
				},
			},
		}
		out.finish()
	}
}

fn payload<P: EditPrepared>(
	works: &[Work<P>],
	projections: &[Projection],
	committed: Option<&[CommittedSection]>,
) -> Payload {
	Payload {
		sections: works
			.iter()
			.zip(projections)
			.enumerate()
			.map(|(index, (work, projection))| {
				let committed = committed.and_then(|sections| sections.get(index));
				let after = committed
					.and_then(|section| section.content.clone())
					.unwrap_or_else(|| projection.after.clone());
				let numbered = numbered_diff(
					work.prepared.base_bytes(),
					&after,
					Some(Path::new(work.prepared.display_path().as_str())),
				)
				.ok();
				let diff = numbered
					.as_ref()
					.map_or_else(Str::default, |diff| diff.text.clone());
				let preview = build_compact_diff_preview(&diff, CompactDiffOptions::default()).preview;
				SectionPayload {
					path: work.prepared.display_path().clone(),
					canonical_path: work.prepared.path().clone(),
					op: if work.prepared.base_bytes() == &projection.after {
						SectionOp::Noop
					} else {
						SectionOp::Update
					},
					move_dest: None,
					old_revision: work.prepared.base_revision().clone(),
					new_revision: committed.and_then(|section| section.new_revision.clone()),
					applied_ops: projection
						.resolved
						.iter()
						.enumerate()
						.map(|(index, edit)| AppliedOp {
							kind: sf!("replace"),
							patch_line: edit.start,
							index,
						})
						.collect(),
					resolved_edits: projection.resolved.clone(),
					rebased: committed.is_some_and(|section| section.rebased),
					before: work.prepared.base_bytes().clone(),
					before_blob: None,
					after: after.clone(),
					after_blob: None,
					header: Some(format_hashline_header(
						work.prepared.display_path(),
						&omp_hashline::compute_snapshot_tag(&after),
					)),
					diff,
					preview,
					first_changed_line: projection.resolved.first().map(|edit| edit.start),
					block_resolutions: Vec::new(),
					warnings: projection.warnings.clone(),
					diagnostics: committed.map_or_else(Vec::new, |section| section.diagnostics.clone()),
					diagnostics_complete: committed.is_none_or(|section| section.diagnostics_complete),
				}
			})
			.collect(),
	}
}

fn line_at(text: &[u8], offset: usize) -> usize {
	text[..offset].iter().filter(|byte| **byte == b'\n').count() + 1
}

fn line_at_end(text: &[u8], start: usize, end: usize) -> usize {
	let mut line = line_at(text, end);
	if end > start && text.get(end.saturating_sub(1)) == Some(&b'\n') {
		line = line.saturating_sub(1);
	}
	line.max(line_at(text, start))
}

fn replacement_error(error: ReplaceError) -> Str {
	match error {
		ReplaceError::AmbiguousExact { occurrences, lines, previews } => {
			let mut text = format!(
				"found {occurrences} exact occurrences; provide more context or enable replace-all"
			);
			for (line, preview) in lines.into_iter().zip(previews) {
				let _ = fmt::Write::write_fmt(&mut text, format_args!("\nline {line}: {preview}"));
			}
			text.into()
		},
		error => error.to_string().into(),
	}
}

fn replacement_body(text: &[u8]) -> Vec<Str> {
	let Ok(text) = str::from_utf8(text) else {
		return Vec::new();
	};
	if text.is_empty() {
		return Vec::new();
	}
	let text = text.replace("\r\n", "\n");
	text
		.strip_suffix('\n')
		.unwrap_or(&text)
		.split('\n')
		.map(Str::new)
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ladder_preserves_unicode_bom_crlf_and_indentation() {
		let unicode = apply_replace(
			b"\xef\xbb\xbfsay \xe2\x80\x9chello\xe2\x80\x9d\r\n",
			"say \"hello\"\n",
			"say \"goodbye\"\n",
			ReplaceOptions::default(),
		)
		.expect("unicode fallback");
		assert_eq!(unicode.final_bytes.as_ref(), b"\xef\xbb\xbfsay \"goodbye\"\r\n");
		assert_eq!(
			omp_hashline::replace::adjust_indentation("foo\nbar", "    foo\n    bar", "foo\nbaz\nbar",),
			"    foo\n    baz\n    bar"
		);
	}

	#[test]
	fn ambiguous_and_noop_replacements_remain_actionable() {
		let ambiguous = apply_replace(b"same\nsame\n", "same", "changed", ReplaceOptions::default())
			.expect_err("ambiguous exact matches must not select arbitrarily");
		assert!(
			matches!(ambiguous, ReplaceError::AmbiguousExact { previews, .. } if !previews.is_empty())
		);
		assert!(matches!(
			apply_replace(b"same\n", "same", "same", ReplaceOptions::default()),
			Err(ReplaceError::NoChanges)
		));
	}
}
