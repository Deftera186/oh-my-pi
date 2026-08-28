//! Revisioned model-facing Language Server Protocol tool.

use std::{future::Future, sync::Arc, time::Duration};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, DocEffects, Effects, Ev, IncomingParams,
	ParamError, Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub mod actions;
pub mod checkers;
pub mod diagnostics;
pub mod navigation;
pub mod refactor;
pub mod render;

/// One discoverable LSP operation.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	JsonSchema,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Action {
	/// Fresh diagnostics for a file, capped glob, or `*` workspace.
	Diagnostics,
	/// Go to definition.
	Definition,
	/// Go to type definition.
	TypeDefinition,
	/// Go to implementation.
	Implementation,
	/// Find references.
	References,
	/// Resolve hover documentation.
	Hover,
	/// List document or workspace symbols.
	Symbols,
	/// Preview or apply a symbol rename.
	Rename,
	/// Plan and atomically apply a path rename with import updates.
	RenameFile,
	/// List, resolve, or execute code actions.
	CodeActions,
	/// Send an advanced raw LSP request.
	Request,
	/// Report selected server capabilities.
	Capabilities,
	/// Report native daemon and binding status.
	Status,
	/// Reload selected native bindings.
	Reload,
}

impl Action {
	/// Whether the action may mutate workspace state.
	pub const fn mutative(self, apply: Option<bool>) -> bool {
		match self {
			Self::Rename | Self::RenameFile => !matches!(apply, Some(false)),
			Self::CodeActions => matches!(apply, Some(true)),
			Self::Reload => true,
			_ => false,
		}
	}
}

/// Arguments for `lsp@1`.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// Operation discriminator.
	pub action:   Action,
	/// Workspace-relative file, glob for diagnostics, or `*` workspace.
	#[serde(default)]
	pub file:     Option<Str>,
	/// One-based source line.
	#[serde(default)]
	pub line:     Option<u32>,
	/// Identifier or `identifier#N` occurrence target.
	#[serde(default)]
	pub symbol:   Option<Str>,
	/// Workspace symbol query, code-action selector, or rename destination path.
	#[serde(default)]
	pub query:    Option<Str>,
	/// New identifier for rename.
	#[serde(default)]
	pub new_name: Option<Str>,
	/// Apply a rename/code action; false requests a dry-run.
	#[serde(default)]
	pub apply:    Option<bool>,
	/// Optional native binding name.
	#[serde(default)]
	pub server:   Option<Str>,
	/// Raw request method for `request`.
	#[serde(default)]
	pub method:   Option<Str>,
	/// Raw JSON parameters for `request`; textDocument and position are
	/// auto-filled when omitted.
	#[serde(default)]
	pub params:   Option<Value>,
	/// Wall-clock timeout in seconds, clamped to 5–300 and the configured
	/// maximum.
	#[serde(default)]
	pub timeout:  Option<u64>,
}

/// Durable typed result independent of the interactive renderer.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Payload {
	/// Applied action.
	pub action:  Action,
	/// Selected binding names.
	pub servers: Vec<Str>,
	/// Bounded model-visible projection.
	pub output:  Str,
	/// Structured revisioned result used by enhanced views.
	pub data:    Value,
}

/// LSP operations do not stream intermediate updates.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Update {}

/// Typed LSP tool failure.
#[derive(Clone, Debug, Deserialize, Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// Arguments do not describe a valid action.
	#[error("invalid LSP action arguments")]
	InvalidArguments,
	/// No native binding applies.
	#[error("no language server is available for this target")]
	Unavailable,
	/// Environment policy rejected the action tier.
	#[error("LSP action is not authorized")]
	Unauthorized,
	/// Selected binding timed out.
	#[error("LSP action timed out")]
	TimedOut,
	/// Server returned a protocol error.
	#[error("language server request failed")]
	Server,
	/// Transactional workspace edit was rejected or rolled back.
	#[error("LSP workspace edit failed")]
	WorkspaceEdit,
	/// Caller cancelled the action.
	#[error("LSP action was cancelled")]
	Cancelled,
}

/// Application-owned bridge to the project Environment's document authority.
pub trait LspControl: Clone + Send + Sync + 'static {
	/// Executes one validated action under the supplied bounded deadline.
	fn execute(
		&self,
		params: Params,
		timeout: Duration,
		cancel: CancellationToken,
	) -> impl Future<Output = Result<Payload, Fault>> + Send + '_;
}

/// Frozen LSP tool binding.
pub struct LspTool<C> {
	control: C,
	maximum: Duration,
	spec:    ToolSpec,
}

/// Returns the host-free `lsp@1` specification.
pub fn spec() -> ToolSpec {
	ToolSpec {
		name:            sf!("lsp"),
		rev:             Rev { family: Str::default(), n: 1 },
		description:     sf!(
			"Queries and transactionally applies project language-server diagnostics, navigation, \
			 symbols, refactors, code actions, raw requests, status, and reloads."
		),
		schema:          omp_tool::schema::<Params>(),
		constraint:      Constraint::Schema {
			priority:       100,
			on_unsupported: omp_tool::Fallback::Unspecified,
		},
		effects:         Effects {
			documents: Some(DocEffects {
				read:        true,
				write_globs: [sf!("*")].into_iter().collect::<Arc<[_]>>(),
			}),
			exec:      None,
			inference: None,
			desktop:   None,
			subagents: 0,
		},
		projection_code: omp_tool::native_projection_code(
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			include_bytes!("mod.rs"),
		)
		.into(),
	}
}

/// Creates discoverable `lsp@1` with an environment-configured timeout ceiling.
pub fn tool<C: LspControl>(control: C, maximum: Duration) -> LspTool<C> {
	LspTool {
		control,
		maximum: maximum.clamp(Duration::from_secs(5), Duration::from_secs(300)),
		spec: spec(),
	}
}

impl<C: LspControl> Tool for LspTool<C> {
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
				Ok(Ok(payload)) => {
					let useless = matches!(payload.action, Action::Definition | Action::TypeDefinition | Action::Implementation | Action::References | Action::Symbols)
						&& payload.data.as_array().is_some_and(Vec::is_empty);
					yield done(Ok(payload), useless);
				},
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
	if params.timeout == Some(0) || params.line == Some(0) {
		return false;
	}
	match params.action {
		Action::Diagnostics | Action::Status | Action::Capabilities | Action::Reload => true,
		Action::Request => params
			.method
			.as_ref()
			.is_some_and(|method| !method.is_empty()),
		Action::Symbols => params.file.is_some() || params.query.is_some(),
		Action::Rename => {
			params.file.is_some()
				&& params.line.is_some()
				&& params.symbol.is_some()
				&& params.new_name.is_some()
		},
		Action::RenameFile => params.file.is_some() && params.query.is_some(),
		_ => params.file.is_some() && params.line.is_some(),
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
		example:  Some(Str::new_static(r#"{"action":"status","file":"src/lib.rs"}"#)),
		found:    Some(message),
	}
}
