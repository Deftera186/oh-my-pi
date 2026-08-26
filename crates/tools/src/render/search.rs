//! Native grep and glob renderers.

use std::fmt::Write as _;

use omp_core::Str;
use omp_tool::{CallOutcome, ToolIdentity, render::RenderFold};

use super::{fault_view, live_view, push_text};
use crate::{
	gallery::RendererGalleryFixture,
	glob::{Fault as GlobFault, Payload as GlobPayload, Update as GlobUpdate},
	grep::{Fault as GrepFault, Payload as GrepPayload, Update as GrepUpdate},
};

pub(super) struct GrepRenderer;

impl RenderFold for GrepRenderer {
	type Outcome = CallOutcome<GrepPayload, GrepFault>;
	type State = ();
	type Update = GrepUpdate;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn view(&self, _state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(live_view("grep", "searching")),
			Some(CallOutcome::Ok(payload)) => Some(render_grep_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("grep", &fault.to_string())),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

pub(super) struct GlobRenderer;

impl RenderFold for GlobRenderer {
	type Outcome = CallOutcome<GlobPayload, GlobFault>;
	type State = ();
	type Update = GlobUpdate;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn view(&self, _state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(live_view("glob", "matching paths")),
			Some(CallOutcome::Ok(payload)) => Some(render_glob_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("glob", &fault.to_string())),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

fn render_grep_payload(payload: &GrepPayload) -> Str {
	let matches = payload
		.files
		.iter()
		.map(|file| file.matches.len())
		.sum::<usize>();
	let mut output = String::from("<col gap=0><row gap=1><text bold>grep</text><text>");
	write!(output, "{matches} matches in {} files", payload.total_files)
		.expect("writing to String cannot fail");
	if payload.total_files_lower_bound {
		output.push_str(" or more");
	}
	output.push_str("</text></row>");
	for file in &payload.files {
		output.push_str("<row gap=1><text>");
		push_text(&mut output, &file.path);
		output.push_str("</text><text fg=muted>");
		write!(output, "{} matches", file.matches.len()).expect("writing to String cannot fail");
		output.push_str("</text></row>");
	}
	for note in &payload.notes {
		output.push_str("<text fg=muted>");
		push_text(&mut output, note);
		output.push_str("</text>");
	}
	output.push_str("</col>");
	Str::new(output)
}

fn render_glob_payload(payload: &GlobPayload) -> Str {
	let mut output = String::from("<col gap=0><row gap=1><text bold>glob</text><text>");
	write!(output, "{} paths", payload.matches.len()).expect("writing to String cannot fail");
	if payload.truncated {
		write!(output, " · truncated from {} partial matches", payload.partial_match_count)
			.expect("writing to String cannot fail");
	}
	if payload.timed_out {
		write!(output, " · timed out after {} ms", payload.timeout_ms)
			.expect("writing to String cannot fail");
	}
	output.push_str("</text></row>");
	for entry in &payload.matches {
		output.push_str("<text>");
		push_text(&mut output, &entry.path);
		if entry.is_dir {
			output.push('/');
		}
		output.push_str("</text>");
	}
	output.push_str("</col>");
	Str::new(output)
}

/// Native grep and glob renderer lifecycle fixtures for the visual QA gallery.
pub(crate) fn gallery_fixtures(
	grep: ToolIdentity,
	glob: ToolIdentity,
) -> Vec<RendererGalleryFixture> {
	vec![
	RendererGalleryFixture {
		identity: grep,
		title: "grep gallery",
		progress_update: None,
		success_outcome: br#"{"kind":"ok","value":{"files":[],"total_files":1,"total_files_lower_bound":false,"multi_scope":true,"skip":0,"file_limit_reached":false,"per_file_limit_reached":false,"notes":[],"projected_text":"src/gallery.rs:1:gallery","output_blob":null,"output_shown_lines":1,"output_total_lines":1}}"#,
		error_outcome: br#"{"kind":"faulted","value":{"kind":"invalid_regex","message":"unclosed group"}}"#,
	},
	RendererGalleryFixture {
		identity: glob,
		title: "glob crates/**/*.rs",
		progress_update: None,
		success_outcome: br#"{"kind":"ok","value":{"matches":[],"missing_paths":[],"timed_out":false,"truncated":false,"result_limit_reached":null,"partial_match_count":1,"timeout_ms":30000,"projected_text":"crates/app/src/gallery_cmd.rs","output_blob":null,"output_shown_lines":1,"output_total_lines":1}}"#,
		error_outcome: br#"{"kind":"faulted","value":{"kind":"invalid_pattern","pattern":"[","message":"unclosed character class"}}"#,
	},
	]
}
