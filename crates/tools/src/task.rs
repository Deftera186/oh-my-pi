//! Child-agent task tool over an injected host-side spawner.
//!
//! This crate owns only the typed tool contract.  Driver composition owns
//! child kernels, convar seeding, cfg execution, journals and filesystem views.

use std::future::Future;

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, Constraint, Effects, Ev, IncomingParams, ParamError, Part,
	PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DESCRIPTION: &str = "Runs one or more child agents. Each child is a job backed by its own \
                           session journal; the parent receives the final text, session path, and \
                           usage.";

/// One requested child run.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChildRequest {
	/// Complete child assignment.
	pub task:          Str,
	/// Optional stable display name.
	pub name:          Option<Str>,
	/// Agent class; omitted selects the configured default.
	pub agent:         Option<Str>,
	/// Invocation-specific JSON output schema.
	#[serde(rename = "outputSchema")]
	pub output_schema: Option<serde_json::Value>,
	/// Schema failure mode (`permissive` or `strict`).
	#[serde(rename = "schemaMode")]
	pub schema_mode:   Option<Str>,
}

/// Model arguments for `task@1`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// Shared goal, constraints, and interface contract for every child.
	pub context: Str,
	/// Independent child assignments. The driver may run these concurrently.
	pub tasks:   Vec<ChildRequest>,
}

/// Progress emitted while a child job is live.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Update {
	/// Stable child identity.
	pub id:     Str,
	/// Journal-derived lifecycle status.
	pub status: Str,
}

/// One settled child result.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ChildResult {
	/// Stable child identity.
	pub id:           Str,
	/// Agent class used for the run.
	pub agent:        Str,
	/// Final assistant text.
	pub text:         Str,
	/// Child `.oms` journal path.
	pub session_path: Str,
	/// Input tokens consumed by the child.
	pub tokens_in:    u64,
	/// Output tokens consumed by the child.
	pub tokens_out:   u64,
}

/// Settled task payload.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Payload {
	/// Results in request order.
	pub children: Vec<ChildResult>,
}

/// Stable task-spawn failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct Fault {
	/// Model-facing explanation.
	pub message: Str,
}

/// Host composition seam for child kernels.
///
/// Implementations must seed a child `omp_con::Ctx` from the caller's current
/// effective values, execute `subagent.cfg` then `<agent>.cfg`, create a child
/// `.oms` beneath the parent sessions directory, and journal a `<subagent>`
/// insertion before starting the kernel.
pub trait SubagentSpawner: Send + Sync + 'static {
	/// Spawns every requested child and returns only after they settle.
	fn spawn<'a>(
		&'a self,
		owner: &'a str,
		request: Params,
		updates: &'a flume::Sender<Update>,
	) -> impl Future<Output = Result<Payload, Fault>> + Send + 'a;
}

/// Native task tool over injected driver composition.
pub struct Task<S> {
	spawner: S,
	spec:    ToolSpec,
}

/// Returns the canonical `task@1` declaration shared by registry advertisement
/// and session-owned execution.
#[must_use]
pub fn spec() -> ToolSpec {
	ToolSpec {
		name:            sf!("task"),
		rev:             Rev { family: Default::default(), n: 1 },
		description:     sf!(DESCRIPTION),
		schema:          omp_tool::schema::<Params>(),
		constraint:      Constraint::Schema {
			priority:       100,
			on_unsupported: omp_tool::Fallback::Unspecified,
		},
		effects:         Effects { subagents: u32::MAX, ..Effects::default() },
		projection_code: omp_tool::native_projection_code(
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			include_bytes!("task.rs"),
		)
		.into(),
	}
}

/// Constructs `task@1`.
#[must_use]
pub fn tool<S: SubagentSpawner>(spawner: S) -> Task<S> {
	Task { spawner, spec: spec() }
}

impl<S: SubagentSpawner> Tool for Task<S> {
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
			let Some(owner) = params.owner().cloned() else {
				yield done(Err(Fault { message: sf!("task requires an authenticated invocation owner") }));
				return;
			};
			let request = match params.whole::<Params>().await {
				Ok(request) => request,
				Err(error) => {
					yield param_event(error);
					return;
				},
			};
			if request.tasks.is_empty() {
				yield done(Err(Fault { message: sf!("task requires at least one child") }));
				return;
			}
			if let Err(error) = params.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			let (tx, rx) = flume::bounded(16);
			let spawning = self.spawner.spawn(&owner, request, &tx);
			tokio::pin!(spawning);
			loop {
				match tokio::select! {
					biased;
					result = &mut spawning => Ok(result),
					update = rx.recv_async() => Err(update),
				} {
					Ok(result) => { yield done(result); break; },
					Err(Ok(update)) => yield Ev::Update(update),
					Err(Err(_)) => continue,
				}
			}
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _caps: &PromptCaps) -> Vec<Part> {
		match view {
			Ok(payload) => payload
				.children
				.iter()
				.map(|child| Part::Text { text: child.text.clone() })
				.collect(),
			Err(fault) => vec![Part::Text { text: fault.message.clone() }],
		}
	}
}

fn done(result: Result<Payload, Fault>) -> Ev<Update, Payload, Fault> {
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
		expected: sf!("one committed task@1 argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"context":"shared","tasks":[{{"task":"inspect"}}]}}"#)),
		found:    Some(message),
	}
}
