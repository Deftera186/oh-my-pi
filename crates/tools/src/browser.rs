//! Stateful browser automation over a harness-owned supervised daemon.

use std::{
	error,
	fmt::{self, Display},
	sync::Arc,
};

use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, ExecEffects,
	IncomingParams, ParamError, Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Browser lifecycle operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Action {
	/// Create or replace a named tab.
	Open,
	/// Execute one automation operation in a named tab.
	Run,
	/// Close one named tab or every tab.
	Close,
}

/// Typed operation available to `run` in addition to raw JavaScript.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RunOperation {
	/// Evaluate JavaScript and return its JSON result.
	Evaluate,
	/// Observe interactive DOM nodes with stable ids.
	Observe,
	/// Render an ARIA snapshot.
	AriaSnapshot,
	/// Capture a PNG screenshot.
	Screenshot,
	/// Extract visible text.
	ExtractText,
	/// Extract serialized HTML.
	ExtractHtml,
	/// Click an element.
	Click,
	/// Append text to an element.
	Type,
	/// Replace a form value.
	Fill,
	/// Select one or more option values.
	Select,
	/// Press a key on an element.
	Press,
	/// Scroll an element into view.
	ScrollIntoView,
	/// Drag one element to another.
	Drag,
	/// Upload local files through a file input.
	Upload,
	/// Wait for a selector.
	WaitForSelector,
	/// Wait for a URL substring.
	WaitForUrl,
	/// Wait for a fetch/XHR URL substring.
	WaitForResponse,
}

/// Browser tool arguments.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// Lifecycle action.
	pub action:                  Action,
	/// Stable tab name; defaults to `main`.
	pub name:                    Option<Str>,
	/// Initial or navigated URL.
	pub url:                     Option<Str>,
	/// Typed operation for `run`; defaults to `evaluate` when `code` is present.
	pub operation:               Option<RunOperation>,
	/// JavaScript evaluated by `run`.
	pub code:                    Option<Str>,
	/// Primary selector accepted by the embedded automation engine.
	pub selector:                Option<Str>,
	/// Drag destination selector.
	pub target:                  Option<Str>,
	/// Text/value/key/URL-pattern argument.
	pub value:                   Option<Str>,
	/// Multiple select values or upload paths.
	pub values:                  Option<Vec<Str>>,
	/// Viewport width in CSS pixels.
	pub width:                   Option<u32>,
	/// Viewport height in CSS pixels.
	pub height:                  Option<u32>,
	/// Device scale factor.
	pub scale:                   Option<f64>,
	/// Bounded operation timeout in seconds.
	pub timeout:                 Option<u64>,
	/// Close every managed tab.
	#[serde(default)]
	pub all:                     bool,
	/// Capture the full page rather than the viewport.
	#[serde(default)]
	pub full_page:               bool,
	/// Private host-control signal used by `/browser` after persisting a mode
	/// change. This is intentionally absent from the model-facing schema.
	#[serde(default)]
	#[schemars(skip)]
	#[doc(hidden)]
	pub restart_for_mode_change: Option<bool>,
}

/// Browser operation result.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Payload {
	/// Completed lifecycle action.
	pub action:    Action,
	/// Stable tab name.
	pub name:      Str,
	/// Current committed URL, when a tab remains open.
	pub url:       Option<Str>,
	/// Current document title, when available.
	pub title:     Option<Str>,
	/// JSON result from a run operation.
	pub result:    Option<Value>,
	/// Content-addressed artifacts created by the operation.
	pub artifacts: Vec<Str>,
}

/// Browser daemon failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fault {
	/// Stable failure category.
	pub code:    Str,
	/// Secret-free diagnostic.
	pub message: Str,
}

impl Display for Fault {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.message)
	}
}
impl error::Error for Fault {}

/// Browser operations currently settle as one bounded result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// Harness-owned browser daemon contract.
#[async_trait]
pub trait BrowserHost: Send + Sync + 'static {
	/// Execute one lifecycle operation.
	async fn execute(&self, params: Params) -> Result<Payload, Fault>;
	/// Drop live browser surfaces and apply a new headless/windowed mode.
	async fn restart_for_mode_change(&self, headless: bool) -> Result<(), Fault>;
}

/// Browser tool routed to one supervised daemon.
pub struct Browser {
	host: Arc<dyn BrowserHost>,
	spec: ToolSpec,
}

/// Builds the host-free `browser@1` declaration.
pub fn spec() -> ToolSpec {
	ToolSpec {
		name:            sf!("browser"),
		rev:             Rev { family: Str::default(), n: 1 },
		description:     sf!(
			"Controls named tabs through the supervised embedded browser daemon. Use open before run \
			 and close when finished."
		),
		schema:          omp_tool::schema::<Params>(),
		constraint:      Constraint::Schema {
			priority:       100,
			on_unsupported: omp_tool::Fallback::Unspecified,
		},
		effects:         Effects {
			documents: None,
			exec:      Some(ExecEffects { commands: Arc::default(), network: true }),
			inference: None,
			desktop:   None,
			subagents: 0,
		},
		projection_code: omp_tool::native_projection_code(
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			include_bytes!("browser.rs"),
		)
		.into(),
	}
}

/// Creates `browser@1`.
pub fn tool(host: Arc<dyn BrowserHost>) -> Browser {
	Browser { host, spec: spec() }
}

impl Tool for Browser {
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
				Ok(params) => params,
				Err(error) => { yield param_event(error); return; },
			};
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			if let Some(headless) = params.restart_for_mode_change {
				let name = params.name.clone().unwrap_or_else(|| sf!("main"));
				let result = self.host.restart_for_mode_change(headless).await.map(|()| Payload {
					action: Action::Close,
					name,
					url: None,
					title: None,
					result: Some(json!({ "headless": headless })),
					artifacts: Vec::new(),
				});
				yield Ev::Done(ToolTerminal::Done { result, useless: false });
				return;
			}
			yield Ev::Done(ToolTerminal::Done { result: self.host.execute(params).await, useless: false });
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) => {
					Str::new(serde_json::to_string(payload).expect("browser payload serializes"))
				},
				Err(fault) => fault.message.clone(),
			},
		}]
	}
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
		expected: sf!("one committed browser argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  None,
		found:    Some(message),
	}
}
