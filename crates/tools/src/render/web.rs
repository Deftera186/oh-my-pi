//! Native web search renderer.

use std::fmt::Write as _;

use omp_core::Str;
use omp_tool::{CallOutcome, ToolIdentity, render::RenderFold};

use super::{live_view, push_attr, push_text};
use crate::{
	gallery::RendererGalleryFixture,
	web_search::{Fault as WebSearchFault, Payload as WebSearchPayload, Update as WebSearchUpdate},
};

pub(super) struct WebSearchRenderer;

impl RenderFold for WebSearchRenderer {
	type Outcome = CallOutcome<WebSearchPayload, WebSearchFault>;
	type State = ();
	type Update = WebSearchUpdate;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn view(&self, _state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(live_view("web_search", "searching providers")),
			Some(CallOutcome::Ok(payload)) => Some(render_web_search_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(render_web_search_fault(&fault.to_string())),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

fn render_web_search_payload(payload: &WebSearchPayload) -> Str {
	let response = &payload.response;
	let mut output = String::from("<col gap=0><row gap=1><text bold>web_search</text>");
	if !response.engine.is_empty() {
		output.push_str("<text fg=accent bold>");
		push_text(&mut output, &response.engine);
		output.push_str("</text>");
	}
	if !response.auth_mode.is_empty() {
		output.push_str("<text fg=muted>");
		push_text(&mut output, &response.auth_mode);
		output.push_str("</text>");
	}
	output.push_str("</row>");
	if !response.answer.is_empty() {
		output.push_str("<md>");
		push_text(&mut output, &response.answer);
		output.push_str("</md>");
	}
	if !response.sources.is_empty() {
		output.push_str("<text bold>Sources</text><col gap=0>");
		for (index, source) in response.sources.iter().enumerate() {
			output.push_str("<row gap=1><text fg=muted>");
			write!(output, "{}.", index + 1).expect("writing to a String cannot fail");
			output.push_str("</text><text href=\"");
			push_attr(&mut output, &source.url);
			output.push_str("\" fg=accent underline>");
			if source.title.is_empty() {
				push_text(&mut output, &source.url);
			} else {
				push_text(&mut output, &source.title);
			}
			output.push_str("</text>");
			if !source.snippet.is_empty() {
				output.push_str("<text fg=muted truncate>");
				push_text(&mut output, &source.snippet);
				output.push_str("</text>");
			}
			output.push_str("</row>");
		}
		output.push_str("</col>");
	}
	if let Some(usage) = response.usage.as_ref() {
		let total = usage
			.total_tokens
			.unwrap_or_else(|| usage.input_tokens.saturating_add(usage.output_tokens));
		let searches = usage
			.server_tools
			.as_ref()
			.and_then(|tools| tools.web_search_requests)
			.unwrap_or(0);
		if total != 0 || searches != 0 {
			output.push_str("<row gap=1><text fg=muted>");
			if total != 0 {
				write!(output, "{total} tokens").expect("writing to a String cannot fail");
			}
			if total != 0 && searches != 0 {
				output.push_str(" · ");
			}
			if searches != 0 {
				write!(output, "{searches} search requests").expect("writing to a String cannot fail");
			}
			output.push_str("</text></row>");
		}
	}
	for warning in &response.warnings {
		output.push_str("<row gap=1><text fg=warn bold>relaxed</text><text fg=warn>");
		push_text(&mut output, warning);
		output.push_str("</text></row>");
	}
	for failure in &response.failures {
		output.push_str("<row gap=1><text fg=muted>");
		push_text(&mut output, &failure.provider);
		output.push_str("</text><text fg=warn>");
		push_text(&mut output, &failure.code);
		if let Some(status) = failure.status {
			write!(output, " · HTTP {status}").expect("writing to a String cannot fail");
		}
		output.push_str("</text></row>");
	}
	output.push_str("</col>");
	Str::new(output)
}

fn render_web_search_fault(message: &str) -> Str {
	let mut output = String::from("<col gap=0><row gap=1><text bold fg=error>web_search</text>");
	output.push_str("<text fg=error>failed</text></row><text fg=error>");
	push_text(&mut output, message);
	output.push_str("</text></col>");
	Str::new(output)
}

/// Native web search renderer lifecycle fixtures for the visual QA gallery.
pub(crate) fn gallery_fixtures(web_search: ToolIdentity) -> Vec<RendererGalleryFixture> {
	vec![
	RendererGalleryFixture {
		identity: web_search,
		title: "web_search native gallery",
		progress_update: None,
		success_outcome: br#"{"kind":"ok","value":{"response":{"engine":"gallery","answer":"Native renderer gallery result.","sources":[],"citations":[],"searchQueries":[],"related":[],"warnings":[],"unsupported":[],"account":"","authMode":"","failures":[]}}}"#,
		error_outcome: br#"{"kind":"faulted","value":{"kind":"search","code":"gallery","message":"sample provider failure"}}"#,
	},
	]
}
