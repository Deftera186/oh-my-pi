//! Revisioned model-facing Debug Adapter Protocol tool.

use std::{future::Future, time::Duration};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, ExecEffects,
	IncomingParams, ParamError, Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::debug_render;

/// One discoverable debugger operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Action {
	/// Launch a program under a discovered adapter.
	Launch,
	/// Attach to a configured process or remote adapter.
	Attach,
	/// Add or replace a source breakpoint.
	SetBreakpoint,
	/// Remove a source breakpoint.
	RemoveBreakpoint,
	/// Add or replace a function breakpoint.
	SetFunctionBreakpoint,
	/// Remove a function breakpoint.
	RemoveFunctionBreakpoint,
	/// Add or replace an instruction breakpoint.
	SetInstructionBreakpoint,
	/// Remove an instruction breakpoint.
	RemoveInstructionBreakpoint,
	/// Resolve a data-breakpoint identifier.
	DataBreakpointInfo,
	/// Add or replace a data breakpoint.
	SetDataBreakpoint,
	/// Remove a data breakpoint.
	RemoveDataBreakpoint,
	/// Continue execution.
	Continue,
	/// Pause execution.
	Pause,
	/// Step over the current statement.
	StepOver,
	/// Step into the current statement.
	StepIn,
	/// Step out of the current frame.
	StepOut,
	/// List threads.
	Threads,
	/// Read stack frames.
	StackTrace,
	/// Read frame scopes.
	Scopes,
	/// Read variables with paging.
	Variables,
	/// Evaluate an expression.
	Evaluate,
	/// Disassemble instructions.
	Disassemble,
	/// Read process memory.
	ReadMemory,
	/// Write process memory.
	WriteMemory,
	/// List loaded modules.
	Modules,
	/// List loaded sources.
	LoadedSources,
	/// Send an adapter extension request.
	CustomRequest,
	/// Read the bounded output tail.
	Output,
	/// List live sessions.
	Sessions,
	/// Terminate a session tree.
	Terminate,
}

impl Action {
	/// Whether the Environment classifies this action as inspection-only.
	pub const fn read_only(self) -> bool {
		matches!(
			self,
			Self::Threads
				| Self::StackTrace
				| Self::Scopes
				| Self::Variables
				| Self::Disassemble
				| Self::ReadMemory
				| Self::Modules
				| Self::LoadedSources
				| Self::Output
				| Self::Sessions
		)
	}
}

/// Arguments for `debug@1`; action-specific fields are forwarded as DAP JSON.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// Operation discriminator.
	pub action:                Action,
	/// Adapter name for launch or attach.
	#[serde(default)]
	pub adapter:               Option<Str>,
	/// Stable session identity returned by launch or attach.
	#[serde(default)]
	pub session:               Option<Str>,
	/// Launch program or source path.
	#[serde(default)]
	pub path:                  Option<Str>,
	/// Process identity for attach.
	#[serde(default)]
	pub pid:                   Option<u32>,
	/// Configured remote adapter port.
	#[serde(default)]
	pub port:                  Option<u16>,
	/// Configured remote adapter host.
	#[serde(default)]
	pub host:                  Option<Str>,
	/// One-based source line.
	#[serde(default)]
	pub line:                  Option<u32>,
	/// One-based source column.
	#[serde(default)]
	pub column:                Option<u32>,
	/// Breakpoint condition.
	#[serde(default)]
	pub condition:             Option<Str>,
	/// Breakpoint hit condition.
	#[serde(default)]
	pub hit_condition:         Option<Str>,
	/// Function-breakpoint name.
	#[serde(default)]
	pub function:              Option<Str>,
	/// Instruction address/reference.
	#[serde(default)]
	pub instruction_reference: Option<Str>,
	/// Instruction or memory offset.
	#[serde(default)]
	pub offset:                Option<i64>,
	/// Data-breakpoint identifier.
	#[serde(default)]
	pub data_id:               Option<Str>,
	/// Data-breakpoint access type.
	#[serde(default)]
	pub access_type:           Option<Str>,
	/// Thread identity.
	#[serde(default)]
	pub thread_id:             Option<i64>,
	/// Frame identity.
	#[serde(default)]
	pub frame_id:              Option<i64>,
	/// Variables reference.
	#[serde(default)]
	pub variables_reference:   Option<i64>,
	/// Page start.
	#[serde(default)]
	pub start:                 Option<u32>,
	/// Requested item or byte count.
	#[serde(default)]
	pub count:                 Option<u32>,
	/// Expression for evaluate or dataBreakpointInfo.
	#[serde(default)]
	pub expression:            Option<Str>,
	/// Evaluation context.
	#[serde(default)]
	pub context:               Option<Str>,
	/// Adapter memory reference.
	#[serde(default)]
	pub memory_reference:      Option<Str>,
	/// Base64 bytes for writeMemory.
	#[serde(default)]
	pub data:                  Option<Str>,
	/// Stepping granularity.
	#[serde(default)]
	pub granularity:           Option<Str>,
	/// Adapter-specific request command.
	#[serde(default)]
	pub command:               Option<Str>,
	/// Raw launch, attach, or custom-request fields merged last.
	#[serde(default)]
	pub arguments:             Option<Value>,
	/// Wall-clock timeout in seconds, clamped to 5–300.
	#[serde(default)]
	pub timeout:               Option<u64>,
}

/// Durable debug result independent of the renderer.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Payload {
	/// Applied action.
	pub action:   Action,
	/// Current session identity, when applicable.
	pub session:  Option<Str>,
	/// Current revision fence.
	pub revision: Option<u64>,
	/// Bounded model projection.
	pub output:   Str,
	/// Structured snapshot for enhanced views.
	pub data:     Value,
}

/// Debug operations do not stream speculative updates.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Update {}

/// Typed debug tool failure.
#[derive(Clone, Debug, Deserialize, Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// Arguments do not satisfy the selected action.
	#[error("invalid debug action arguments")]
	InvalidArguments,
	/// No compatible adapter or session is available.
	#[error("debug adapter or session is unavailable")]
	Unavailable,
	/// Environment policy rejected the action tier.
	#[error("debug action is not authorized")]
	Unauthorized,
	/// Session revision no longer matches.
	#[error("debug session revision is stale")]
	Stale,
	/// Adapter request failed.
	#[error("debug adapter request failed")]
	Adapter,
	/// Bounded deadline elapsed.
	#[error("debug action timed out")]
	TimedOut,
	/// Caller cancelled the action.
	#[error("debug action was cancelled")]
	Cancelled,
}

/// Application-owned env/v1 bridge.
pub trait DebugControl: Clone + Send + Sync + 'static {
	/// Executes one validated action exclusively through the Environment DAP
	/// wire.
	fn execute(
		&self,
		params: Params,
		timeout: Duration,
		cancel: CancellationToken,
	) -> impl Future<Output = Result<Payload, Fault>> + Send + '_;
}

/// Frozen `debug@1` binding.
pub struct DebugTool<C> {
	control: C,
	maximum: Duration,
	spec:    ToolSpec,
}

/// Returns the host-free `debug@1` specification.
pub fn spec() -> ToolSpec {
	ToolSpec {
		name:            sf!("debug"),
		rev:             Rev { family: Str::default(), n: 1 },
		description:     sf!(
			"Launches or attaches native debug adapters; manages all breakpoint families, execution, \
			 stack and variable inspection, disassembly, memory, output, sessions, custom requests, \
			 and termination."
		),
		schema:          omp_tool::schema::<Params>(),
		constraint:      Constraint::Schema {
			priority:       100,
			on_unsupported: omp_tool::Fallback::Unspecified,
		},
		effects:         Effects {
			documents: None,
			exec:      Some(ExecEffects {
				commands: [sf!("*")].into_iter().collect(),
				network:  false,
			}),
			inference: None,
			desktop:   None,
			subagents: 0,
		},
		projection_code: omp_tool::native_projection_code(
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			include_bytes!("debug.rs"),
		)
		.into(),
	}
}

/// Creates the revisioned debug tool.
pub fn tool<C: DebugControl>(control: C, maximum: Duration) -> DebugTool<C> {
	DebugTool {
		control,
		maximum: maximum.clamp(Duration::from_secs(5), Duration::from_secs(300)),
		spec: spec(),
	}
}

impl<C: DebugControl> Tool for DebugTool<C> {
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
			if !valid(&params) {
				yield done(Err(Fault::InvalidArguments), true);
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			let timeout = Duration::from_secs(params.timeout.unwrap_or(30).clamp(5, 300)).min(self.maximum);
			let cancel = CancellationToken::new();
			match tokio::time::timeout(timeout, self.control.execute(params, timeout, cancel.clone())).await {
				Ok(Ok(payload)) => yield done(Ok(payload), false),
				Ok(Err(fault)) => yield done(Err(fault), true),
				Err(_) => { cancel.cancel(); yield done(Err(Fault::TimedOut), true); },
			}
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		let text = match view {
			Ok(payload) => payload.output.clone(),
			Err(fault) => Str::new(fault.to_string()),
		};
		vec![Part::Text { text }]
	}
}

fn valid(params: &Params) -> bool {
	if params.timeout == Some(0) {
		return false;
	}
	match params.action {
		Action::Launch | Action::Attach => params
			.adapter
			.as_ref()
			.is_some_and(|value| !value.is_empty()),
		Action::Sessions => true,
		Action::SetBreakpoint | Action::RemoveBreakpoint => {
			params.session.is_some()
				&& params.path.is_some()
				&& params.line.is_some_and(|line| line > 0)
		},
		Action::SetFunctionBreakpoint | Action::RemoveFunctionBreakpoint => {
			params.session.is_some() && params.function.is_some()
		},
		Action::SetInstructionBreakpoint | Action::RemoveInstructionBreakpoint => {
			params.session.is_some() && params.instruction_reference.is_some()
		},
		Action::SetDataBreakpoint | Action::RemoveDataBreakpoint => {
			params.session.is_some() && params.data_id.is_some()
		},
		Action::DataBreakpointInfo => params.session.is_some() && params.expression.is_some(),
		Action::StackTrace
		| Action::Continue
		| Action::Pause
		| Action::StepOver
		| Action::StepIn
		| Action::StepOut => params.session.is_some() && params.thread_id.is_some(),
		Action::Scopes => params.session.is_some() && params.frame_id.is_some(),
		Action::Variables => params.session.is_some() && params.variables_reference.is_some(),
		Action::Evaluate => params.session.is_some() && params.expression.is_some(),
		Action::Disassemble => {
			params.session.is_some() && params.memory_reference.is_some() && params.count.is_some()
		},
		Action::ReadMemory => {
			params.session.is_some() && params.memory_reference.is_some() && params.count.is_some()
		},
		Action::WriteMemory => {
			params.session.is_some() && params.memory_reference.is_some() && params.data.is_some()
		},
		Action::CustomRequest => params.session.is_some() && params.command.is_some(),
		_ => params.session.is_some(),
	}
}

fn done(result: Result<Payload, Fault>, useless: bool) -> Ev<Update, Payload, Fault> {
	Ev::Done(ToolTerminal::Done { result, useless })
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
		example:  Some(Str::new_static(r#"{"action":"sessions"}"#)),
		found:    Some(message),
	}
}

/// Builds the bounded model projection used by environment bridges.
pub fn render(action: Action, data: &Value) -> Str {
	debug_render::render(action, data)
}
