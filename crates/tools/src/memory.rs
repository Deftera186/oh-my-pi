//! Typed Mnemopi recall, reflect, and retain tools.

use std::sync::Arc;

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, StrMut, sf};
use omp_memory::{
	MemoryRuntime,
	recall::{RecallBounds, RecallResult},
};
use omp_tool::{
	ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, IncomingParams, ParamError, Part,
	PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_TOKEN_BUDGET: usize = 2_000;
const MAX_TOKEN_BUDGET: usize = 16_000;
const MAX_RETAIN_ITEMS: usize = 64;

/// Arguments accepted by `recall@1`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecallParams {
	/// Natural-language search query.
	pub query:        Str,
	/// Approximate result token budget.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub token_budget: Option<usize>,
}

/// Deterministic recall payload.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RecallPayload {
	/// Original query.
	pub query: Str,
	/// Relevance-ranked scoped results.
	pub items: Vec<RecallResult>,
}

/// Arguments accepted by `reflect@1`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReflectParams {
	/// Question answered from long-term memory.
	pub query:        Str,
	/// Optional angle or current context for synthesis.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub context:      Option<Str>,
	/// Approximate recall token budget.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub token_budget: Option<usize>,
}

/// Synthesized reflection payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReflectPayload {
	/// Coherent answer produced from recalled evidence.
	pub answer:   Str,
	/// Number of memories supplied to synthesis.
	pub recalled: usize,
}

/// One durable fact supplied to `retain@1`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainItem {
	/// Specific, self-contained information to remember.
	pub content: Str,
	/// Optional source context.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub context: Option<Str>,
}

/// Arguments accepted by `retain@1`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainParams {
	/// Durable facts to store as one bounded batch.
	pub items: Vec<RetainItem>,
}

/// Durable retain receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RetainPayload {
	/// Stored memory ids in input order.
	pub ids: Vec<Str>,
}

/// Memory tools do not stream progress.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// Typed memory-tool failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// Mnemopi is not live for this session.
	#[error("Mnemopi memory is unavailable")]
	Unavailable,
	/// A query or retain batch was empty or outside its documented bound.
	#[error("memory tool arguments are invalid")]
	InvalidInput,
	/// The durable memory operation failed.
	#[error("memory operation failed")]
	Operation,
	/// Auxiliary synthesis failed without a model answer.
	#[error("memory reflection synthesis failed")]
	Synthesis,
}

/// Bounded reflection request crossing from the memory device to app inference.
#[derive(Clone, Debug)]
pub struct ReflectionRequest {
	/// Question to answer.
	pub query:    Str,
	/// Optional current context.
	pub context:  Option<Str>,
	/// Bounded relevance-ranked evidence.
	pub memories: Arc<[RecallResult]>,
}

/// Typed refusal from the app inference authority.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ReflectionHostError {
	/// No app inference authority is currently bound.
	#[error("memory reflection host is unavailable")]
	Unavailable,
	/// Inference ended without a usable synthesis.
	#[error("memory reflection inference failed")]
	Inference,
}

/// App-owned auxiliary synthesis authority injected into the memory device.
#[async_trait::async_trait]
pub trait ReflectionHost: Send + Sync + 'static {
	/// Synthesizes an answer from bounded, relevance-ranked memories.
	async fn reflect(&self, request: ReflectionRequest) -> Result<Str, ReflectionHostError>;
}

#[async_trait::async_trait]
impl<H: ReflectionHost + ?Sized> ReflectionHost for Arc<H> {
	async fn reflect(&self, request: ReflectionRequest) -> Result<Str, ReflectionHostError> {
		self.as_ref().reflect(request).await
	}
}

/// Typed `recall@1` executor.
pub struct RecallTool {
	runtime: Arc<MemoryRuntime>,
	spec:    ToolSpec,
}

/// Typed `reflect@1` executor.
pub struct ReflectTool<H> {
	runtime: Arc<MemoryRuntime>,
	host:    H,
	spec:    ToolSpec,
}

/// Typed `retain@1` executor.
pub struct RetainTool {
	runtime: Arc<MemoryRuntime>,
	spec:    ToolSpec,
}

/// Builds the host-free `recall@1` declaration.
pub fn recall_spec() -> ToolSpec {
	memory_spec::<RecallParams>("recall", RECALL_DESCRIPTION)
}

/// Builds the host-free `reflect@1` declaration.
pub fn reflect_spec() -> ToolSpec {
	memory_spec::<ReflectParams>("reflect", REFLECT_DESCRIPTION)
}

/// Builds the host-free `retain@1` declaration.
pub fn retain_spec() -> ToolSpec {
	memory_spec::<RetainParams>("retain", RETAIN_DESCRIPTION)
}

/// Creates the revisioned recall leaf.
pub fn recall_tool(runtime: Arc<MemoryRuntime>) -> RecallTool {
	RecallTool { runtime, spec: recall_spec() }
}

/// Creates the revisioned reflect leaf.
pub fn reflect_tool<H: ReflectionHost>(runtime: Arc<MemoryRuntime>, host: H) -> ReflectTool<H> {
	ReflectTool { runtime, host, spec: reflect_spec() }
}

/// Creates the revisioned retain leaf.
pub fn retain_tool(runtime: Arc<MemoryRuntime>) -> RetainTool {
	RetainTool { runtime, spec: retain_spec() }
}

impl Tool for RecallTool {
	type Fault = Fault;
	type Params = RecallParams;
	type Payload = RecallPayload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, RecallPayload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<RecallParams>().await {
				Ok(value) => value,
				Err(error) => { yield param_event(error); return; },
			};
			if params.query.trim().is_empty() {
				yield terminal(Err(Fault::InvalidInput), true);
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			let token_budget = params.token_budget.unwrap_or(DEFAULT_TOKEN_BUDGET).clamp(1, MAX_TOKEN_BUDGET);
			match self.runtime.search(
				params.query.as_str(),
				None,
				RecallBounds { token_budget, ..RecallBounds::default() },
			) {
				Ok(outcome) if outcome.message.is_some() => yield terminal(Err(Fault::Unavailable), false),
				Ok(outcome) => {
					let useless = outcome.items.is_empty();
					yield terminal(Ok(RecallPayload { query: outcome.query, items: outcome.items }), useless);
				},
				Err(_) => yield terminal(Err(Fault::Operation), false),
			}
		}
	}

	fn prompt(&self, view: Result<&RecallPayload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) => render_recall(payload),
				Err(fault) => Str::new(fault.to_string()),
			},
		}]
	}
}

impl<H: ReflectionHost> Tool for ReflectTool<H> {
	type Fault = Fault;
	type Params = ReflectParams;
	type Payload = ReflectPayload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, ReflectPayload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<ReflectParams>().await {
				Ok(value) => value,
				Err(error) => { yield param_event(error); return; },
			};
			if params.query.trim().is_empty() {
				yield terminal(Err(Fault::InvalidInput), true);
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			let token_budget = params
				.token_budget
				.unwrap_or(DEFAULT_TOKEN_BUDGET)
				.clamp(1, MAX_TOKEN_BUDGET);
			let mut recall_query = params.query.to_string();
			if let Some(context) = params
				.context
				.as_ref()
				.map(|value| value.trim())
				.filter(|value| !value.is_empty())
			{
				recall_query.push_str("\n\nAdditional context:\n");
				recall_query.push_str(context.as_str());
			}
			let outcome = match self.runtime.search(
				&recall_query,
				None,
				RecallBounds { token_budget, ..RecallBounds::default() },
			) {
				Ok(value) if value.message.is_none() => value,
				Ok(_) => {
					yield terminal(Err(Fault::Unavailable), false);
					return;
				},
				Err(_) => {
					yield terminal(Err(Fault::Operation), false);
					return;
				},
			};
			if outcome.items.is_empty() {
				yield terminal(
					Ok(ReflectPayload {
						answer: sf!("No relevant information found to reflect on."),
						recalled: 0,
					}),
					true,
				);
				return;
			}
			let request = ReflectionRequest {
				query: params.query,
				context: params.context,
				memories: Arc::from(outcome.items),
			};
			let recalled = request.memories.len();
			match self.host.reflect(request).await {
				Ok(answer) if !answer.trim().is_empty() => {
					yield terminal(Ok(ReflectPayload { answer, recalled }), false);
				},
				_ => yield terminal(Err(Fault::Synthesis), false),
			}
		}
	}

	fn prompt(&self, view: Result<&ReflectPayload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) => payload.answer.clone(),
				Err(fault) => Str::new(fault.to_string()),
			},
		}]
	}
}

impl Tool for RetainTool {
	type Fault = Fault;
	type Params = RetainParams;
	type Payload = RetainPayload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, RetainPayload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<RetainParams>().await {
				Ok(value) => value,
				Err(error) => { yield param_event(error); return; },
			};
			if params.items.is_empty()
				|| params.items.len() > MAX_RETAIN_ITEMS
				|| params.items.iter().any(|item| item.content.trim().is_empty())
			{
				yield terminal(Err(Fault::InvalidInput), true);
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			let mut ids = Vec::with_capacity(params.items.len());
			for item in params.items {
				match self.runtime.save(
					item.content.as_str(),
					"coding-agent-retain",
					0.75,
					item.context.as_deref(),
				) {
					Ok(outcome) => match outcome.id {
						Some(id) => ids.push(id),
						None => { yield terminal(Err(Fault::Unavailable), false); return; },
					},
					Err(_) => { yield terminal(Err(Fault::Operation), false); return; },
				}
			}
			yield terminal(Ok(RetainPayload { ids }), false);
		}
	}

	fn prompt(&self, view: Result<&RetainPayload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) => sf!(
					"{} {} stored.",
					payload.ids.len(),
					if payload.ids.len() == 1 {
						"memory"
					} else {
						"memories"
					}
				),
				Err(fault) => Str::new(fault.to_string()),
			},
		}]
	}
}

fn memory_spec<P: JsonSchema>(name: &'static str, description: &'static str) -> ToolSpec {
	ToolSpec {
		name:            Str::new_static(name),
		rev:             Rev { family: Str::default(), n: 1 },
		description:     Str::new_static(description),
		schema:          omp_tool::schema::<P>(),
		constraint:      Constraint::Schema {
			priority:       100,
			on_unsupported: omp_tool::Fallback::Unspecified,
		},
		effects:         Effects::empty(),
		projection_code: omp_tool::native_projection_code(
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			include_bytes!("memory.rs"),
		)
		.into(),
	}
}

fn render_recall(payload: &RecallPayload) -> Str {
	if payload.items.is_empty() {
		return sf!("No relevant memories found.");
	}
	let mut output = StrMut::new("");
	use std::fmt::Write as _;
	let _ = writeln!(output, "Found {} relevant memories:\n", payload.items.len());
	for item in &payload.items {
		let _ = writeln!(
			output,
			"- [{}] {} (memory://{})",
			item.memory.bank, item.memory.content, item.memory.id,
		);
	}
	output.freeze()
}

fn terminal<U, P>(result: Result<P, Fault>, useless: bool) -> Ev<U, P, Fault> {
	Ev::Done(ToolTerminal::Done { result, useless })
}

fn param_event<U, P>(error: ParamError) -> Ev<U, P, Fault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(omp_tool::Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn commit_event<U, P>(error: CommitError) -> Ev<U, P, Fault> {
	match error {
		CommitError::Aborted => Ev::Aborted(omp_tool::Abort::InputDropped),
		CommitError::Interrupted(interrupt) => {
			Ev::Aborted(omp_tool::Abort::Interrupted { reason: interrupt.reason })
		},
		CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed JSON argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  None,
		found:    Some(message),
	}
}

const RECALL_DESCRIPTION: &str = "Search long-term memory for raw relevance-ranked entries. Use \
                                  before questions about prior conversations, preferences, or \
                                  project decisions. Read a full memory:// id before updating it.";
const REFLECT_DESCRIPTION: &str = "Synthesize a coherent answer across relevant long-term \
                                   memories. Use for open-ended questions spanning many stored \
                                   facts; optional context focuses the synthesis.";
const RETAIN_DESCRIPTION: &str = "Store one or more specific, self-contained durable facts in \
                                  long-term memory. Do not retain ephemeral task state.";
