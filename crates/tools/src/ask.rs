//! Interactive question selection with a host-provided presentation seam.

use std::{
	collections::HashSet,
	error,
	fmt::{self, Display},
	future,
	future::Future,
	pin::Pin,
	sync::Arc,
};

use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, Constraint, Effects, Ev, IncomingParams, ParamError, Part,
	PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use parking_lot::RwLock;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Label used for a host-provided free-text alternative.
pub const OTHER_OPTION: &str = "Other (type your own)";

const RESERVED_LABELS: [&str; 3] = [OTHER_OPTION, "Chat about this", "Next →"];

/// Arguments for `ask@1`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// Questions presented in order.
	pub questions: Vec<Question>,
}
/// One picker question.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
	/// Stable key returned with the answer.
	pub id:          Str,
	/// User-visible question text.
	pub question:    Str,
	/// Compact section label.
	#[schemars(default, skip_serializing_if = "Option::is_none")]
	pub header:      Option<Str>,
	/// Available choices.
	pub options:     Vec<OptionItem>,
	/// Allow more than one choice.
	#[serde(default)]
	pub multi:       bool,
	/// Zero-based default choice used only by headless hosts.
	#[schemars(default, skip_serializing_if = "Option::is_none")]
	pub recommended: Option<usize>,
}
/// One picker choice.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OptionItem {
	/// Returned choice label.
	pub label:       Str,
	/// Optional explanation.
	#[schemars(default, skip_serializing_if = "Option::is_none")]
	pub description: Option<Str>,
	/// Optional rich preview source.
	#[schemars(default, skip_serializing_if = "Option::is_none")]
	pub preview:     Option<Str>,
}
/// A resolved answer to one question.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Answer {
	/// The corresponding question identifier.
	pub id:        Str,
	/// Choice labels in selection order.
	pub selected:  Vec<Str>,
	/// Whether the headless fallback generated this answer.
	pub timed_out: bool,
}
/// Structured ask result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Payload {
	/// Answers ordered like the request questions.
	pub answers:  Vec<Answer>,
	/// Whether the presentation host was noninteractive.
	pub headless: bool,
}
/// Ask has no genuine output updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}
/// Ask validation or presenter failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// Arguments violate the picker contract.
	Invalid {
		/// Stable validation explanation.
		message: Str,
	},
	/// The environment presentation bridge failed.
	Presenter {
		/// Stable bridge failure explanation.
		message: Str,
	},
}
impl Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Invalid { message } | Self::Presenter { message } => f.write_str(message),
		}
	}
}
impl error::Error for Fault {}

/// UI bridge implemented by the environment's `omp.ui.v1.UiRequest` dispatcher.
///
/// The tools crate deliberately does not manufacture UI outcomes: interactive
/// hosts implement this trait and route `Params` through their dialog request
/// path. The default presenter is the explicit headless policy specified by pi
/// parity.
pub trait AskPresenter: Send + Sync + 'static {
	/// Presents ordered questions and returns durable selections.
	fn present<'p>(
		&'p self,
		questions: &'p [Question],
	) -> Pin<Box<dyn Future<Output = Result<Presentation, Fault>> + Send + 'p>>;
}

/// Replaceable per-environment presentation bridge.
#[derive(Clone)]
pub struct PresenterSlot {
	inner: Arc<RwLock<Arc<dyn AskPresenter>>>,
}

impl PresenterSlot {
	/// Creates a slot with the specified fallback presenter.
	pub fn new(presenter: Arc<dyn AskPresenter>) -> Self {
		Self { inner: Arc::new(RwLock::new(presenter)) }
	}

	/// Replaces the presenter used by subsequent ask invocations.
	pub fn bind(&self, presenter: Arc<dyn AskPresenter>) {
		*self.inner.write() = presenter;
	}
}

impl AskPresenter for PresenterSlot {
	fn present<'p>(
		&'p self,
		questions: &'p [Question],
	) -> Pin<Box<dyn Future<Output = Result<Presentation, Fault>> + Send + 'p>> {
		let presenter = Arc::clone(&*self.inner.read());
		Box::pin(async move { presenter.present(questions).await })
	}
}
/// Presenter result, preserving whether answers came from headless fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Presentation {
	/// Answers selected by the host.
	pub answers:  Vec<Answer>,
	/// Whether selection used the noninteractive fallback.
	pub headless: bool,
}
/// One ordered spoken line for an ask dialog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpokenLine {
	/// Text spoken in presentation order.
	pub text:        Str,
	/// Whether this line identifies the recommended option.
	pub recommended: bool,
}

/// Cancellable host-owned dialog vocalizer.
#[async_trait]
pub trait AskVocalizer: Send + Sync + 'static {
	/// Speaks the complete ordered dialog or returns silently when disabled.
	async fn speak(
		&self,
		lines: &[SpokenLine],
		cancellation: CancellationToken,
	) -> Result<(), Fault>;
}
/// Deterministic noninteractive picker: every recommended choice wins.
#[derive(Default)]
pub struct HeadlessPresenter;
impl AskPresenter for HeadlessPresenter {
	fn present<'p>(
		&'p self,
		questions: &'p [Question],
	) -> Pin<Box<dyn Future<Output = Result<Presentation, Fault>> + Send + 'p>> {
		Box::pin(future::ready(
			questions
				.iter()
				.map(headless_answer)
				.collect::<Result<_, _>>()
				.map(|answers| Presentation { answers, headless: true }),
		))
	}
}

/// Ask tool backed by a UI presentation bridge.
pub struct Ask {
	presenter: Arc<dyn AskPresenter>,
	vocalizer: Option<Arc<dyn AskVocalizer>>,
	spec:      ToolSpec,
}
/// Creates `ask@1` with the specified environment presentation bridge.
pub fn tool(presenter: Arc<dyn AskPresenter>) -> Ask {
	Ask { presenter, vocalizer: None, spec: spec() }
}
/// Creates `ask@1` with ordered cancellable speech.
pub fn tool_with_vocalizer(
	presenter: Arc<dyn AskPresenter>,
	vocalizer: Arc<dyn AskVocalizer>,
) -> Ask {
	Ask { presenter, vocalizer: Some(vocalizer), spec: spec() }
}
/// Creates `ask@1` with explicit headless recommendation selection.
pub fn headless_tool() -> Ask {
	tool(Arc::new(HeadlessPresenter))
}
fn spec() -> ToolSpec {
	ToolSpec {
		name:            sf!("ask"),
		rev:             Rev { family: Str::new(""), n: 1 },
		description:     sf!(
			"Asks the user one or more picker questions. Options may include descriptions and \
			 previews; use `multi` for multi-selection and `recommended` for headless defaults.",
		),
		schema:          omp_tool::schema::<Params>(),
		constraint:      Constraint::Schema {
			priority:       100,
			on_unsupported: omp_tool::Fallback::Unspecified,
		},
		effects:         Effects::default(),
		projection_code: omp_tool::native_projection_code(
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			include_bytes!("ask.rs"),
		)
		.into(),
	}
}
impl Tool for Ask {
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
	) -> impl Stream<Item = Ev<Update, Payload, Fault>> + Send + 'c {
		stream! {
			let arguments = match params.whole::<Params>().await {
				Ok(value) => value,
				Err(error) => { yield param_event(error); return; },
			};
			if let Err(error) = params.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			if let Err(fault) = validate(&arguments.questions) {
				yield done(Err(fault));
				return;
			}
			if let Some(vocalizer) = &self.vocalizer {
				let cancellation = CancellationToken::new();
				let lines = spoken_lines(&arguments.questions);
				let speech = vocalizer.speak(&lines, cancellation.clone());
				tokio::pin!(speech);
				tokio::select! {
					result = &mut speech => {
						if let Err(fault) = result {
							yield done(Err(fault));
							return;
						}
					},
					interrupt = params.next_interrupt() => {
						cancellation.cancel();
						if let Ok(interrupt) = interrupt {
							yield Ev::Aborted(Abort::Interrupted { reason: interrupt.reason });
						} else {
							yield Ev::Aborted(Abort::InputDropped);
						}
						return;
					},
				}
			}
			let result = self.presenter.present(&arguments.questions).await.map(|presentation| Payload {
				answers: presentation.answers,
				headless: presentation.headless,
			});
			yield done(result);
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: Str::new(match view {
				Ok(payload) => serde_json::to_string(&payload.answers).expect("answers serialize"),
				Err(fault) => fault.to_string(),
			}),
		}]
	}
}
/// Projects questions, options, previews, and recommendations into
/// deterministic speech order.
pub fn spoken_lines(questions: &[Question]) -> Vec<SpokenLine> {
	let mut lines = Vec::new();
	for question in questions {
		if let Some(header) = &question.header {
			lines.push(SpokenLine { text: header.clone(), recommended: false });
		}
		lines.push(SpokenLine { text: question.question.clone(), recommended: false });
		for (index, option) in question.options.iter().enumerate() {
			let recommended = question.recommended == Some(index);
			lines.push(SpokenLine { text: option.label.clone(), recommended });
			if let Some(description) = &option.description {
				lines.push(SpokenLine { text: description.clone(), recommended });
			}
			if let Some(preview) = &option.preview {
				lines.push(SpokenLine { text: preview.clone(), recommended });
			}
		}
	}
	lines
}

/// Validates a nonempty request, nonempty unique identifiers, permitted option
/// labels, and headless default indexes.
pub fn validate(questions: &[Question]) -> Result<(), Fault> {
	if questions.is_empty() {
		return Err(invalid("`questions` must not be empty"));
	}
	let mut ids = HashSet::new();
	for question in questions {
		if question.id.trim().is_empty() || !ids.insert(question.id.clone()) {
			return Err(invalid("question ids must be non-empty and unique"));
		}
		if let Some(index) = question.recommended
			&& index >= question.options.len()
		{
			return Err(invalid("`recommended` must index an option"));
		}
		for option in &question.options {
			if option.label.trim().is_empty() || RESERVED_LABELS.contains(&option.label.as_ref()) {
				return Err(invalid("option labels must be non-empty and not reserved"));
			}
		}
	}
	Ok(())
}
fn headless_answer(question: &Question) -> Result<Answer, Fault> {
	let index = question
		.recommended
		.ok_or_else(|| invalid("headless ask requires `recommended` for every question"))?;
	let option = question
		.options
		.get(index)
		.ok_or_else(|| invalid("`recommended` must index an option"))?;
	Ok(Answer {
		id:        question.id.clone(),
		selected:  vec![option.label.clone()],
		timed_out: true,
	})
}
fn invalid(message: &str) -> Fault {
	Fault::Invalid { message: Str::new(message) }
}
const fn done(result: Result<Payload, Fault>) -> Ev<Update, Payload, Fault> {
	Ev::Done(ToolTerminal::Done { result, useless: false })
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
fn commit_event(error: omp_tool::CommitError) -> Ev<Update, Payload, Fault> {
	match error {
		omp_tool::CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		omp_tool::CommitError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		omp_tool::CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}
fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed JSON argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"questions":[...] }}"#)),
		found:    Some(message),
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use futures::StreamExt as _;
	use tokio::time;

	use super::*;
	fn question(recommended: Option<usize>) -> Question {
		Question {
			id: sf!("format"),
			question: sf!("Which?"),
			header: None,
			options: vec![
				OptionItem { label: sf!("Markdown"), description: None, preview: None },
				OptionItem {
					label:       sf!("Text"),
					description: None,
					preview:     Some(sf!("plain")),
				},
			],
			multi: false,
			recommended,
		}
	}
	#[test]
	fn headless_selection_uses_recommended_index() {
		let answer = headless_answer(&question(Some(1))).unwrap();
		assert_eq!(answer.selected, [sf!("Text")]);
		assert!(answer.timed_out);
	}
	#[test]
	fn rejects_reserved_labels_and_missing_headless_default() {
		let mut reserved = question(Some(0));
		reserved.options[0].label = sf!("Next →");
		assert!(validate(&[reserved]).is_err());
		assert!(headless_answer(&question(None)).is_err());
	}

	struct DelayedPresenter;

	impl AskPresenter for DelayedPresenter {
		fn present<'p>(
			&'p self,
			questions: &'p [Question],
		) -> Pin<Box<dyn Future<Output = Result<Presentation, Fault>> + Send + 'p>> {
			Box::pin(async move {
				time::sleep(Duration::from_millis(10)).await;
				Ok(Presentation {
					answers:  vec![Answer {
						id:        questions[0].id.clone(),
						selected:  vec![questions[0].options[0].label.clone()],
						timed_out: false,
					}],
					headless: false,
				})
			})
		}
	}

	#[tokio::test(flavor = "current_thread")]
	async fn call_awaits_async_presenter_on_current_thread_runtime() {
		let ask = tool(Arc::new(DelayedPresenter));
		let (feed, params) = IncomingParams::channel();
		feed
			.args_committed(Str::new(
				r#"{"questions":[{"id":"format","question":"Which?","options":[{"label":"Markdown"}]}]}"#,
			))
			.expect("ask invocation remains live");

		let events = ask.call(params).collect::<Vec<_>>().await;
		let [Ev::Done(ToolTerminal::Done { result: Ok(Payload { answers, headless }), .. })] =
			events.as_slice()
		else {
			panic!("expected successful async ask result: {events:?}");
		};
		assert!(!headless);
		assert_eq!(answers[0].selected, [sf!("Markdown")]);
		assert!(!answers[0].timed_out);
	}
}
