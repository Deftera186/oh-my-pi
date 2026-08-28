//! Native desktop capture, input, and accessibility device.

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
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, DesktopEffects, Effects, Ev,
	IncomingParams, ParamError, Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Desktop session operation.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Action {
	/// Report capture/input/accessibility capabilities.
	Capabilities,
	/// List attached displays.
	ListDisplays,
	/// List capturable windows.
	ListWindows,
	/// Capture a desktop or window.
	Capture,
	/// Click a capture-relative point.
	Click,
	/// Move the pointer.
	MoveMouse,
	/// Drag through capture-relative points.
	Drag,
	/// Scroll at a point.
	Scroll,
	/// Type text.
	Type,
	/// Press a key chord.
	KeyChord,
	/// Raise a window.
	RaiseWindow,
	/// Capture a bounded accessibility tree.
	AxSnapshot,
	/// Query accessibility nodes.
	AxQuery,
	/// Hit-test an accessibility node.
	AxElementAt,
	/// Return the focused accessibility node.
	AxFocused,
	/// Resolve an accessibility reference.
	AxNode,
	/// Read native attributes from an accessibility reference.
	AxAttributes,
}

/// Desktop device arguments.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// Operation to perform.
	pub action:     Action,
	/// Explicitly prohibit input/focus mutation for this call.
	#[serde(default)]
	pub read_only:  bool,
	/// Window id; absence selects the complete desktop.
	pub window:     Option<Str>,
	/// Accessibility reference or window id depending on the action.
	pub reference:  Option<Str>,
	/// Text, key chord, role, or query name.
	pub value:      Option<Str>,
	/// Primary x coordinate.
	pub x:          Option<f64>,
	/// Primary y coordinate.
	pub y:          Option<f64>,
	/// Horizontal scroll delta.
	pub dx:         Option<f64>,
	/// Vertical scroll delta.
	pub dy:         Option<f64>,
	/// Drag path as ordered `[x, y]` pairs.
	pub points:     Option<Vec<[f64; 2]>>,
	/// Capture width cap.
	pub max_width:  Option<u32>,
	/// Capture height cap.
	pub max_height: Option<u32>,
	/// Accessibility tree depth cap.
	pub max_depth:  Option<u32>,
	/// Accessibility result cap.
	pub limit:      Option<u32>,
}

impl Params {
	/// Exact desktop authority required by this invocation.
	pub const fn required_effects(&self) -> DesktopEffects {
		match self.action {
			Action::Capabilities | Action::ListDisplays | Action::ListWindows | Action::Capture => {
				DesktopEffects { capture: true, accessibility: false, input: false }
			},
			Action::AxSnapshot
			| Action::AxQuery
			| Action::AxElementAt
			| Action::AxFocused
			| Action::AxNode
			| Action::AxAttributes => {
				DesktopEffects { capture: false, accessibility: true, input: false }
			},
			Action::Click
			| Action::MoveMouse
			| Action::Drag
			| Action::Scroll
			| Action::Type
			| Action::KeyChord
			| Action::RaiseWindow => {
				DesktopEffects { capture: false, accessibility: false, input: true }
			},
		}
	}
}

/// Desktop operation result.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Payload {
	/// Completed action.
	pub action:    Action,
	/// Structured native result.
	pub result:    Value,
	/// Content-addressed screenshots.
	pub artifacts: Vec<Str>,
}

/// Native desktop failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fault {
	/// Stable failure category.
	pub code:    Str,
	/// Secret-free permission or backend diagnostic.
	pub message: Str,
}

impl Display for Fault {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.message)
	}
}
impl error::Error for Fault {}

/// Desktop operations currently settle as one bounded result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// Harness-owned persistent desktop session contract.
#[async_trait]
pub trait ComputerHost: Send + Sync + 'static {
	/// Execute one admission-approved desktop operation.
	async fn execute(&self, params: Params) -> Result<Payload, Fault>;
}

/// Computer tool routed to one native session.
pub struct Computer {
	host: Arc<dyn ComputerHost>,
	spec: ToolSpec,
}

/// Builds the host-free `computer@1` declaration.
pub fn spec() -> ToolSpec {
	ToolSpec {
		name:            sf!("computer"),
		rev:             Rev { family: Str::default(), n: 1 },
		description:     sf!(
			"Captures and controls the native desktop through a persistent supervised session. Set \
			 read_only for inspection-only calls."
		),
		schema:          omp_tool::schema::<Params>(),
		constraint:      Constraint::Schema {
			priority:       100,
			on_unsupported: omp_tool::Fallback::Unspecified,
		},
		effects:         Effects {
			documents: None,
			exec:      None,
			inference: None,
			desktop:   Some(DesktopEffects {
				capture:       true,
				accessibility: true,
				input:         true,
			}),
			subagents: 0,
		},
		projection_code: omp_tool::native_projection_code(
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			include_bytes!("computer.rs"),
		)
		.into(),
	}
}

/// Creates `computer@1`.
pub fn tool(host: Arc<dyn ComputerHost>) -> Computer {
	Computer { host, spec: spec() }
}

impl Tool for Computer {
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
			if params.read_only && params.required_effects().input {
				yield Ev::Done(ToolTerminal::Done {
					result: Err(Fault { code: sf!("desktop_read_only"), message: sf!("read_only calls cannot perform desktop input or focus mutation") }),
					useless: false,
				});
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			yield Ev::Done(ToolTerminal::Done { result: self.host.execute(params).await, useless: false });
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) => {
					Str::new(serde_json::to_string(payload).expect("computer payload serializes"))
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
		expected: sf!("one committed desktop argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  None,
		found:    Some(message),
	}
}
