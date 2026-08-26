//! URL fetching through the shared reader-mode and document conversion
//! pipeline.

use std::{
	error,
	fmt::{self, Display},
	sync::Arc,
};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, ExecEffects,
	IncomingParams, ParamError, Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::read::{
	selector::ParsedSelector,
	web::{self, types::WebError},
};

/// Arguments accepted by `fetch@1`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// HTTP(S) URL, optionally followed by a read line selector or `:raw`.
	pub url: Str,
}

/// Clean fetched document content and extraction metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Payload {
	/// URL after redirects.
	pub url:          Str,
	/// Document title recovered from the rendered content, when present.
	pub title:        Option<Str>,
	/// Rendered MIME type.
	pub content_type: Option<Str>,
	/// Extraction or conversion method.
	pub method:       Str,
	/// Clean text, Markdown, or verbatim response body.
	pub content:      Str,
	/// Ordered extraction notes.
	pub notes:        Vec<Str>,
}

/// Fetch does not stream partial updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// URL validation, transport, or conversion failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// The target was invalid or could not be rendered.
	Fetch {
		/// Stable model-facing failure detail.
		message: Str,
	},
}

impl Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Fetch { message } => f.write_str(message),
		}
	}
}
impl error::Error for Fault {}

/// Fetch executor using the exact HTTP and markit pipeline owned by `read`.
pub struct Fetch<C> {
	client: C,
	spec:   ToolSpec,
}

/// Creates `fetch@1` over an application-owned HTTP transport.
pub fn tool<C: web::types::HttpClient + Send + Sync + 'static>(client: C) -> Fetch<C> {
	Fetch {
		client,
		spec: ToolSpec {
			name:            sf!("fetch"),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(
				"Fetches a URL as reader-mode clean text or Markdown. Append `:raw` to bypass HTML \
				 and document conversion; line selectors use the read syntax.",
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects {
				documents: None,
				exec:      Some(ExecEffects { network: true, commands: Arc::default() }),
				inference: None,
				desktop:   None,
				subagents: 0,
			},
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("fetch.rs"),
			)
			.into(),
		},
	}
}

impl<C: web::types::HttpClient + Send + Sync + 'static> Tool for Fetch<C> {
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
			let params = match incoming.whole::<Params>().await {
				Ok(value) => value,
				Err(error) => { yield param_event(error); return; }
			};
			if let Err(error) = incoming.interruptable().committed().await { yield commit_event(error); return; }
			let result = execute(&self.client, &params.url).await;
			yield Ev::Done(ToolTerminal::Done { result, useless: false });
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, caps: &PromptCaps) -> Vec<Part> {
		let text = match view {
			Ok(payload) => payload.content.as_str(),
			Err(fault) => return vec![Part::Text { text: fault.to_string().into() }],
		};
		let mut end = text.len().min(caps.maximum_text_bytes as usize);
		while end != 0 && !text.is_char_boundary(end) {
			end -= 1;
		}
		(end != 0)
			.then(|| Part::Text { text: Str::new(&text[..end]) })
			.into_iter()
			.collect()
	}
}

async fn execute<C: web::types::HttpClient + Sync>(
	client: &C,
	authored: &str,
) -> Result<Payload, Fault> {
	let target = web::parse_target(authored)
		.map_err(fetch_fault)?
		.ok_or_else(|| Fault::Fetch { message: sf!("fetch requires an HTTP(S) URL") })?;
	let raw = target.selector.is_raw();
	let fetched = web::read_resource(client, &target.url, raw)
		.await
		.map_err(fetch_fault)?;
	let title = content_title(&fetched.render.content);
	let mut content = fetched.render.content;
	if !matches!(target.selector, ParsedSelector::None | ParsedSelector::Raw) {
		content = select_lines(&content, &target.selector)?;
	}
	Ok(Payload {
		url: fetched.final_url,
		title,
		content_type: fetched.render.content_type,
		method: fetched.render.method,
		content,
		notes: fetched.render.notes.into_iter().collect(),
	})
}

fn content_title(content: &str) -> Option<Str> {
	content.lines().find_map(|line| {
		let title = line.trim().strip_prefix("# ")?.trim();
		(!title.is_empty()).then(|| Str::new(title))
	})
}

fn select_lines(content: &str, selector: &ParsedSelector) -> Result<Str, Fault> {
	let ParsedSelector::Lines { ranges, .. } = selector else {
		return Err(Fault::Fetch { message: sf!("unsupported fetch selector") });
	};
	let lines = content.lines().collect::<Vec<_>>();
	let mut selected = String::new();
	for range in ranges {
		let start = usize::try_from(range.start_line.saturating_sub(1))
			.unwrap_or(usize::MAX)
			.min(lines.len());
		let end = range
			.end_line
			.and_then(|line| usize::try_from(line).ok())
			.unwrap_or(lines.len())
			.min(lines.len());
		if start < end {
			if !selected.is_empty() {
				selected.push('\n');
			}
			selected.push_str(&lines[start..end].join("\n"));
		}
	}
	Ok(selected.into())
}

fn param_event(error: ParamError) -> Ev<Update, Payload, Fault> {
	match error {
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

fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed JSON argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"url":"https://example.com"}}"#)),
		found:    Some(message),
	}
}

fn fetch_fault(error: WebError) -> Fault {
	Fault::Fetch { message: error.message() }
}

#[cfg(test)]
mod tests {

	use super::*;
	use crate::read::web::types::{HttpRequest, HttpResponse};

	#[test]
	fn raw_selector_bypasses_conversion_and_is_removed_from_url() {
		let target = web::parse_target("https://example.com/report.docx:raw")
			.unwrap()
			.unwrap();
		assert_eq!(target.url.as_str(), "https://example.com/report.docx");
		assert!(target.selector.is_raw());
	}

	#[test]
	fn line_selection_uses_read_selector_ranges() {
		let target = web::parse_target("https://example.com/page:2-3")
			.unwrap()
			.unwrap();
		assert_eq!(select_lines("one\ntwo\nthree\nfour", &target.selector).unwrap(), "two\nthree");
	}

	#[test]
	fn content_title_uses_the_first_level_one_markdown_heading() {
		assert_eq!(
			content_title("intro\n## Section\n# Documentation title\nbody").as_deref(),
			Some("Documentation title"),
		);
		assert_eq!(content_title("## Section only"), None);
	}

	#[derive(Clone)]
	struct Client;
	impl web::types::HttpClient for Client {
		async fn get(&self, request: HttpRequest) -> Result<HttpResponse, WebError> {
			Ok(HttpResponse {
				final_url:    request.url,
				status:       200,
				content_type: Some(sf!("text/html")),
				headers:      smallvec::SmallVec::new(),
				body:         bytes::Bytes::from_static(b"<html><body>verbatim</body></html>"),
			})
		}
	}

	#[tokio::test]
	async fn raw_fetch_bypasses_html_reader_mode() {
		let payload = execute(&Client, "https://example.com/:raw").await.unwrap();
		assert_eq!(payload.method, "raw");
		assert_eq!(payload.content, "<html><body>verbatim</body></html>");
	}
}
