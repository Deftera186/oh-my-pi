//! Native edit renderer: streaming diff previews and settled per-file diffs.

use std::fmt::Write as _;

use omp_core::Str;
use omp_tool::{CallOutcome, ToolIdentity, render::RenderFold};

use super::{fault_view, live_view, push_text};
use crate::{
	edit::{EditUpdate, Fault as EditFault, Payload as EditPayload},
	gallery::RendererGalleryFixture,
};

#[derive(Default)]
pub(super) struct EditState {
	latest: Option<EditUpdate>,
}

pub(super) struct EditRenderer;

impl RenderFold for EditRenderer {
	type Outcome = CallOutcome<EditPayload, EditFault>;
	type State = EditState;
	type Update = EditUpdate;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.latest = Some(update);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(render_edit_live(state.latest.as_ref())),
			Some(CallOutcome::Ok(payload)) => Some(render_edit_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("edit", &edit_fault(fault))),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

/// Physical wrapped-row budget for collapsed edit diff cards.
const COLLAPSED_EDIT_DIFF_ROWS: u16 = omp_hashline::diff_preview::COLLAPSED_DIFF_ROWS;

fn render_edit_live(update: Option<&EditUpdate>) -> Str {
	let Some(update) = update else {
		return live_view("edit", "preparing");
	};
	let mut output = String::from("<col gap=0><row gap=1><text bold>edit</text><text fg=muted>");
	if let Some(path) = update.paths.first() {
		output.push_str(" · ");
		push_text(&mut output, path);
		if update.paths.len() > 1 {
			write!(output, " (+{} more)", update.paths.len() - 1)
				.expect("writing to String cannot fail");
		}
		output.push_str(" · ");
	}
	write!(
		output,
		"preview · {} ops · +{} -{}",
		update.applied_ops, update.added_lines, update.removed_lines
	)
	.expect("writing to String cannot fail");
	write!(output, "</text></row><diff max={COLLAPSED_EDIT_DIFF_ROWS}>")
		.expect("writing to String cannot fail");
	push_text(&mut output, &update.preview);
	output.push_str("</diff></col>");
	Str::new(output)
}

fn render_edit_payload(payload: &EditPayload) -> Str {
	let (added, removed) = payload
		.sections
		.iter()
		.flat_map(|section| section.diff.lines())
		.fold((0usize, 0usize), |(added, removed), line| {
			(
				added + usize::from(line.starts_with('+') && !line.starts_with("+++")),
				removed + usize::from(line.starts_with('-') && !line.starts_with("---")),
			)
		});
	let mut output = String::from("<col gap=0><row gap=1><text bold>edit</text><text>");
	write!(output, "{} files changed · +{added} -{removed}", payload.sections.len())
		.expect("writing to String cannot fail");
	output.push_str("</text></row>");
	for section in &payload.sections {
		output.push_str("<row gap=1><text>");
		push_text(&mut output, &section.path);
		output.push_str("</text><text fg=muted>");
		write!(output, "{} ops", section.applied_ops.len()).expect("writing to String cannot fail");
		if section.rebased {
			output.push_str(" · rebased");
		}
		write!(output, "</text></row><diff max={COLLAPSED_EDIT_DIFF_ROWS}>")
			.expect("writing to String cannot fail");
		push_text(&mut output, &section.diff);
		for (index, diagnostic) in section.diagnostics.iter().enumerate() {
			if !section.diff.is_empty() || index > 0 {
				output.push('\n');
			}
			output.push_str("! ");
			if !diagnostic.source.is_empty() {
				push_text(&mut output, &diagnostic.source);
				if !diagnostic.code.is_empty() {
					output.push('[');
					push_text(&mut output, &diagnostic.code);
					output.push(']');
				}
				output.push_str(": ");
			}
			push_text(&mut output, &diagnostic.message);
		}
		if !section.diagnostics_complete {
			if !section.diff.is_empty() || !section.diagnostics.is_empty() {
				output.push('\n');
			}
			output.push_str("! Additional LSP diagnostics are still settling");
		}
		output.push_str("</diff>");
	}
	output.push_str("</col>");
	Str::new(output)
}

fn edit_fault(fault: &EditFault) -> String {
	use crate::edit::RejectionReason;
	let mut output = match &fault.reason {
		RejectionReason::Conflict => String::from("edit conflict"),
		RejectionReason::StaleUnrecoverable { message }
		| RejectionReason::Format { message }
		| RejectionReason::InvalidPatch { message } => message.to_string(),
	};
	for conflict in &fault.conflicts {
		write!(
			output,
			" · lines {}-{}: {}",
			conflict.start_line, conflict.end_line, conflict.message
		)
		.expect("writing to String cannot fail");
	}
	output
}

/// Native edit renderer lifecycle fixtures for the visual QA gallery.
pub(crate) fn gallery_fixtures(edit: ToolIdentity) -> Vec<RendererGalleryFixture> {
	vec![
	RendererGalleryFixture {
		identity: edit,
		title: "edit src/gallery.rs",
		progress_update: Some(
		br#"{"applied_ops":2,"preview":"--- src/gallery.rs\n+++ src/gallery.rs\n+gallery","added_lines":1,"removed_lines":0}"#,
		),
		success_outcome: br#"{"kind":"ok","value":{"sections":[]}}"#,
		error_outcome: br#"{"kind":"faulted","value":{"reason":{"kind":"invalid_patch","message":"gallery patch rejected"},"conflicts":[]}}"#,
	},
	]
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use omp_core::sf;
	use omp_tool::{CallOutcome, render::ViewState};

	use crate::{
		edit::{EditUpdate, Fault as EditFault, Payload as EditPayload},
		render::test_support::{identities, registry},
	};

	#[test]
	fn edit_update_reduces_to_compact_state_then_settles() {
		let (registry, identities) = registry(identities());
		let update = EditUpdate {
			applied_ops:   2,
			paths:         vec![sf!("src/lib.rs"), sf!("src/other.rs")],
			preview:       sf!("+&lt;already-markup"),
			added_lines:   3,
			removed_lines: 1,
		};
		let mut state = ViewState::new();
		registry
			.fold(
				identities.edit.as_ref().unwrap(),
				&mut state,
				Bytes::from(serde_json::to_vec(&update).expect("update serializes")),
			)
			.expect("typed update folds");
		assert_eq!(state.raw_update_count(), 0);
		assert_eq!(
			registry
				.view(identities.edit.as_ref().unwrap(), &state, None)
				.expect("live edit renders")
				.as_str(),
			"<col gap=0><row gap=1><text bold>edit</text><text fg=muted> · src/lib.rs (+1 more) · \
			 preview · 2 ops · +3 -1</text></row><diff max=40>+&amp;lt;already-markup</diff></col>",
		);

		let outcome = CallOutcome::<EditPayload, EditFault>::Ok(EditPayload { sections: Vec::new() });
		let encoded = serde_json::to_vec(&outcome).expect("outcome serializes");
		assert_eq!(
			registry
				.view(identities.edit.as_ref().unwrap(), &state, Some(&encoded))
				.expect("settled edit renders")
				.as_str(),
			"<col gap=0><row gap=1><text bold>edit</text><text>0 files changed · +0 \
			 -0</text></row></col>",
		);
	}
}
