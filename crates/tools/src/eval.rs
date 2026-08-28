//! Python-only, persistent-session evaluation tool.
//!
//! This crate defines the protocol boundary and the child-local embedded
//! kernel. Production composition owns one killable Python child process per
//! eval session, so interpreter globals, imported modules, environment changes,
//! and cancellation are contained by that session's process.

use std::{
	borrow::Cow,
	collections::{BTreeMap, VecDeque},
	fmt::Write as _,
	future,
	future::Future,
	io::Write as _,
	path::PathBuf,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use async_stream::stream;
use bytes::Bytes;
use dashmap::DashMap;
use futures::{FutureExt, Stream, future::Either, pin_mut};
use omp_core::{CowBytes, Str, sf};
use omp_env::EnvClient;
use omp_proto::inference::v1::{InvokeInput, invoke_input, invoke_input::chunk};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, BlobRef, CommitError, Constraint, DocEffects, Effects, Ev,
	ExecEffects, IncomingParams, InferenceEffects, Interrupt, InterruptWaitError, ParamError, Part,
	PromptCaps, Rev, Tool, ToolSpec, ToolTerminal, Usd,
};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{runtime, sync::OnceCell};

use crate::{
	auto_background::{
		DEFAULT_AUTO_BACKGROUND_THRESHOLD, DetachedJob, ForegroundWait, JobWait,
		managed_job_terminal, next_background_name,
	},
	render::TextProjection,
};

/// Runtime-work timeout accounting shared with host bridge scheduling.
pub mod idle_timeout;
/// Embedded CPython implementation of the eval resource boundary.
pub mod kernel;

const EVAL_DESCRIPTION: &str = r#"Run one step of code in a persistent Python kernel. State persists across calls.
__OMP_EVAL_AGENT_ISOLATION__

Work incrementally: imports → define → test → use, each its own cell. Re-run setup ONLY after `reset`, kernel crash.
Cells exceeding the configured foreground wait threshold continue as managed jobs; their results are delivered automatically.
`timeout: 0` disables the cell deadline; otherwise `timeout` sets it without extending foreground waiting.
Parallelize *within* a cell with `parallel(thunks)`, not by batching.

Top-level `await` works; `asyncio.run(…)` raises error.

On error, fix and re-run only the failing step.

<prelude>
Sync; kwargs.
```
display(value) → None        print(value, ...) → None
read(path, offset?=1, limit?=None) → str
write(path, content) → str
env(key?=None, value?=None) → str | None | dict
output(*ids, format?="raw", query=None, offset=None, limit=None) → str | dict | list[dict]
tool.<name>(args) → unknown
    Invoke any session tool; `args` = its parameter object.
completion(prompt, model?="default"|"smol"|"slow", system=None, schema=None) → str | dict
    Oneshot, stateless (no history/tools). `model`: "smol" fast | "default" session | "slow" most capable. `schema` (JSON-Schema) → parsed object.
__OMP_EVAL_AGENT_HELPER__
parallel(thunks) → list     pipeline(items, ...stages) → list
log(message) → None         phase(title) → None
budget → `budget.total` (ceiling or None), `budget.spent()`, `budget.remaining()`; ceiling `+Nk` advisory, `+Nk!` hard.
```
</prelude>
__OMP_EVAL_AGENT_DAG__

<critical>
Prior top-level names survive into the next cell — reuse; NEVER re-import/re-declare. Re-read only if file changed since last read.
</critical>"#;
const EVAL_AGENT_ISOLATION: &str = "Eval `agent()` children use independent kernels.";
const EVAL_AGENT_HELPER: &str = r#"agent(prompt, agent?="task", name=None, outputSchema=None, schemaMode?="permissive", isolated=None, apply=None, merge=None, handle=False) → str | dict
    Run a subagent → final output. `agent` selects a discovered agent; omit it to use `task`. `outputSchema` overrides agent/session schemas; `schemaMode`/`schemaMode`: "permissive" | "strict". Effective schemas return parsed data. `isolated` requests a worktree; `apply`/`merge` control its changes. Background via `local://` files named in the prompt. `handle` → { text, output, handle: "agent://<id>", id, agent }, parsed `data` when structured."#;
const EVAL_AGENT_DAG: &str = r#"<dag>
Acyclic waves via `agent(…, handle=true)` + `pipeline`/`parallel`:
- **Name nodes.** Capture agent result → `handle` (`agent://<id>`) + `output`.
- **Wire edges.** Put upstream `handle`/`output` in downstream prompt. Bulk: `write("local://<name>.md", …)`.
- **`pipeline`** = staged waves, barrier between stages. **`parallel`** = one wave.
- **Isolate failure.** Wrap risky nodes in try/except; a failure degrades only its subtree.
- **Acyclic only.** No node waits on its own descendant.
</dag>"#;

/// One live discovered agent projected into eval task guidance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskAgentDescription<'a> {
	/// Stable definition name.
	pub name:        &'a str,
	/// Human-readable role.
	pub description: &'a str,
	/// Whether every declared tool is read-only.
	pub read_only:   bool,
	/// Whether this role executes inline.
	pub blocking:    bool,
}
/// One extension-provided prelude helper projected into eval task guidance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreludeHelperDescription<'a> {
	/// Callable signature shown to the model.
	pub signature: &'a str,
	/// Concise description of the helper's behavior.
	pub summary:   &'a str,
}

/// Session facts used to build model-facing task guidance once.
#[derive(Clone, Copy, Debug)]
pub struct TaskDescriptionSnapshot<'a> {
	/// Effective default definition.
	pub default_agent:   &'a str,
	/// Effective discovered roster after disablement; empty means subagent
	/// spawning is unavailable and suppresses child-agent guidance.
	pub agents:          &'a [TaskAgentDescription<'a>],
	/// Extension-provided prelude helpers available in this session.
	pub helpers:         &'a [PreludeHelperDescription<'a>],
	/// Whether child jobs may continue asynchronously.
	pub asynchronous:    bool,
	/// Whether ordered batches are supported.
	pub batch:           bool,
	/// Whether isolated workspaces are enabled.
	pub isolation:       bool,
	/// Whether IRC coordination is enabled.
	pub irc:             bool,
	/// Whether the caller may select effort.
	pub effort:          bool,
	/// Effective maximum fan-out (`0` is unlimited).
	pub max_concurrency: usize,
}

/// Builds the task portion of the eval description from one session snapshot.
pub fn task_description(snapshot: TaskDescriptionSnapshot<'_>) -> Str {
	let subagents_available = !snapshot.agents.is_empty();
	let base = EVAL_DESCRIPTION
		.replace(
			"__OMP_EVAL_AGENT_ISOLATION__",
			if subagents_available {
				EVAL_AGENT_ISOLATION
			} else {
				""
			},
		)
		.replace(
			"__OMP_EVAL_AGENT_HELPER__",
			if subagents_available {
				EVAL_AGENT_HELPER
			} else {
				""
			},
		)
		.replace(
			"__OMP_EVAL_AGENT_DAG__",
			if subagents_available {
				EVAL_AGENT_DAG
			} else {
				""
			},
		);
	let mut output = String::with_capacity(base.len() + 2_048);
	output.push_str(&base);
	if subagents_available {
		output.push_str("\n\n<task-runtime>\n");
		let _ = writeln!(output, "Default agent: `{}`.", snapshot.default_agent);
		let _ = writeln!(
			output,
			"Execution: {}; batches: {}; isolation: {}; IRC: {}; effort: {}; concurrency: {}.",
			if snapshot.asynchronous {
				"async jobs"
			} else {
				"blocking"
			},
			if snapshot.batch {
				"enabled"
			} else {
				"disabled"
			},
			if snapshot.isolation {
				"enabled"
			} else {
				"disabled"
			},
			if snapshot.irc { "enabled" } else { "disabled" },
			if snapshot.effort {
				"enabled"
			} else {
				"disabled"
			},
			if snapshot.max_concurrency == 0 {
				"unlimited".to_owned()
			} else {
				snapshot.max_concurrency.to_string()
			},
		);
		output.push_str("Available agents:\n");
		for agent in snapshot.agents {
			let _ = writeln!(
				output,
				"- `{}`{}{}: {}",
				agent.name,
				if agent.read_only { " (READ-ONLY)" } else { "" },
				if agent.blocking { " (BLOCKING)" } else { "" },
				agent.description,
			);
		}
		output.push_str(
			"Choose the most specific specialist; omit `agent` only when the default fits. Use \
			 read-only agents only for investigation. For concurrent siblings, assign disjoint \
			 ownership and coordinate shared files over IRC before editing.\n</task-runtime>",
		);
	}
	if !snapshot.helpers.is_empty() {
		output.push_str(
			"\n\n<extension-helpers>\nExtension-provided prelude functions (call like any prelude \
			 helper):\n",
		);
		for helper in snapshot.helpers {
			let _ = writeln!(output, "- `{}` — {}", helper.signature, helper.summary);
		}
		output.push_str("</extension-helpers>");
	}
	Str::from(output)
}

const STANDARD_TASK_AGENTS: &[TaskAgentDescription<'static>] = &[
	TaskAgentDescription {
		name:        "task",
		description: "General-purpose delegated multi-step work",
		read_only:   false,
		blocking:    false,
	},
	TaskAgentDescription {
		name:        "scout",
		description: "Rapid read-only codebase research",
		read_only:   true,
		blocking:    false,
	},
	TaskAgentDescription {
		name:        "sonic",
		description: "Strictly mechanical updates or data collection",
		read_only:   false,
		blocking:    false,
	},
	TaskAgentDescription {
		name:        "designer",
		description: "UI/UX implementation and visual refinement",
		read_only:   false,
		blocking:    false,
	},
	TaskAgentDescription {
		name:        "reviewer",
		description: "Evidence-backed code review",
		read_only:   true,
		blocking:    false,
	},
	TaskAgentDescription {
		name:        "security-reviewer",
		description: "Read-only repository security review",
		read_only:   true,
		blocking:    false,
	},
	TaskAgentDescription {
		name:        "librarian",
		description: "Source-verified external library research",
		read_only:   true,
		blocking:    false,
	},
];

impl TaskDescriptionSnapshot<'static> {
	/// Returns the standard eval task-description snapshot.
	#[must_use]
	pub const fn standard() -> Self {
		Self {
			default_agent:   "task",
			agents:          STANDARD_TASK_AGENTS,
			helpers:         &[],
			asynchronous:    false,
			batch:           true,
			isolation:       true,
			irc:             true,
			effort:          true,
			max_concurrency: 32,
		}
	}
}

fn standard_eval_description() -> Str {
	task_description(TaskDescriptionSnapshot::standard())
}

const MAX_DISPLAY_TEXT_BYTES: usize = 8_000;
const MAX_RETAINED_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_CELL_TIMEOUT: Duration = Duration::from_secs(3_600);

/// Runtime accepted by this build of `eval@1`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
	/// OMP's embedded `CPython` runtime.
	Py,
}

impl JsonSchema for Language {
	fn inline_schema() -> bool {
		true
	}

	fn schema_name() -> Cow<'static, str> {
		"Language".into()
	}

	fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
		json_schema!({
			"type": "string",
			"enum": ["py"]
		})
	}
}

/// Lifetime policy for the Python kernel.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum KernelMode {
	/// Reuse the owner-scoped Python kernel.
	#[default]
	Persistent,
	/// Spawn a clean Python kernel for this call and dispose it at settlement.
	PerCall,
}

/// Complete arguments for one Python cell.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[schemars(description = "")]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// runtime: "py" for the Python kernel
	#[expect(
		clippy::doc_markdown,
		reason = "doc comment is the verbatim model-facing description; backticks would leak"
	)]
	#[schemars(required, description = "runtime: \"py\" for the Python kernel")]
	pub language:    Language,
	/// code to run in this eval call, verbatim. Use top-level await freely.
	#[schemars(
		required,
		with = "String",
		description = "code to run in this eval call, verbatim. Use top-level await freely."
	)]
	pub code:        Str,
	/// short label shown in transcript (e.g. "imports", "load config")
	#[schemars(
		default,
		skip_serializing_if = "Option::is_none",
		with = "String",
		description = "short label shown in transcript (e.g. \"imports\", \"load config\")"
	)]
	pub title:       Option<Str>,
	/// timeout for this eval call in seconds; 0 disables the cell timeout
	#[schemars(
		default,
		skip_serializing_if = "Option::is_none",
		with = "serde_json::Number",
		description = "timeout for this eval call in seconds; 0 disables the cell timeout"
	)]
	pub timeout:     Option<f64>,
	/// wipe this language's kernel before running. Other languages are
	/// untouched.
	#[schemars(
		default,
		skip_serializing_if = "Option::is_none",
		with = "bool",
		description = "wipe this language's kernel before running. Other languages are untouched."
	)]
	pub reset:       Option<bool>,
	/// Select a persistent kernel or an isolated one-shot process.
	#[schemars(default, skip_serializing_if = "Option::is_none")]
	pub kernel_mode: Option<KernelMode>,
}

/// Ordered text stream emitted by Python.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputChannel {
	/// Python standard output.
	Stdout,
	/// Python standard error.
	Stderr,
}

/// A live, cell-bounded output update.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Update {
	/// Stream that owns these bytes.
	pub channel:  OutputChannel,
	/// Exact bytes captured within this cell.
	#[serde(with = "cow_bytes")]
	pub data:     CowBytes<'static>,
	/// Monotonic sequence within the cell.
	pub sequence: u64,
}

/// One retained output frame in the durable result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutputFrame {
	/// Stream that owns these bytes.
	pub channel:  OutputChannel,
	/// Exact bytes captured within this cell.
	#[serde(with = "cow_bytes")]
	pub data:     CowBytes<'static>,
	/// Monotonic sequence within the cell.
	pub sequence: u64,
}

/// Rich output captured from a Python cell.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DisplayOutput {
	/// JSON-compatible display value.
	Json {
		/// Displayed value.
		data: Value,
	},
	/// Bounded encoded image awaiting host persistence.
	ImageData {
		/// Exact encoded PNG or JPEG bytes.
		#[serde(with = "cow_bytes")]
		data:      CowBytes<'static>,
		/// Validated image media type.
		mime_type: Str,
	},
	/// Image already persisted by the host.
	Image {
		/// Durable blob containing the encoded image.
		blob:        BlobRef,
		/// Image media type.
		mime_type:   Str,
		/// Model-visible source/final dimension and encoding note.
		description: Str,
	},
	/// Markdown display value.
	Markdown {
		/// Markdown source.
		text: Str,
	},
	/// Structured progress event emitted by a helper.
	Status {
		/// Helper event object.
		event: Value,
	},
}

/// REPL value of the final expression in a cell.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CellValue {
	/// Stable plain-text representation.
	pub text: Str,
	/// JSON value when Python's JSON encoder accepts the object.
	pub json: Option<Value>,
}

/// Python exception retained as durable cell truth.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PythonException {
	/// Python exception class name.
	pub name:      Str,
	/// Exception message without the class prefix.
	pub message:   Str,
	/// Formatted traceback lines in Python order.
	pub traceback: Vec<Str>,
}

/// Terminal disposition of a cell.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CellOutcome {
	/// Cell completed normally.
	Complete,
	/// Python raised an exception.
	Error,
	/// Runtime-work timeout expired.
	Timeout,
	/// Invocation owner interrupted the cell.
	Cancelled,
}

/// Terminal cell status.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CellStatus {
	/// Stable terminal disposition.
	pub outcome:     CellOutcome,
	/// Process-style status used by transcript consumers (`0` or `1`).
	pub exit_code:   Option<i32>,
	/// Host-measured execution duration.
	pub duration_ms: u64,
	/// Python exception, if any.
	pub exception:   Option<PythonException>,
}

/// Complete terminal result supplied by an eval resource.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RunCompletion {
	/// Terminal cell status.
	pub status:          CellStatus,
	/// Final REPL value, if the cell produced one.
	pub result:          Option<CellValue>,
	/// Rich display values emitted during execution.
	pub display_outputs: Vec<DisplayOutput>,
	/// Whether retained text was truncated.
	pub truncated:       bool,
	/// Durable full-output blob when text was truncated.
	pub spilled_output:  Option<BlobRef>,
	/// Full output line count before truncation.
	pub total_lines:     usize,
	/// Full output byte count before truncation.
	pub total_bytes:     usize,
}

/// Durable result of one eval call.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Payload {
	/// Stable identity of the persistent Python session.
	pub session_id:      Bytes,
	/// Host identity of this cell.
	pub cell_id:         Bytes,
	/// Executed runtime.
	pub language:        Language,
	/// Optional caller-provided label.
	pub title:           Option<Str>,
	/// Exact submitted source.
	pub code:            Str,
	/// Whether the namespace was reset immediately before this cell.
	pub reset:           bool,
	/// Ordered retained stdout/stderr frames.
	pub frames:          Vec<OutputFrame>,
	/// Final expression value.
	pub result:          Option<CellValue>,
	/// Rich display values.
	pub display_outputs: Vec<DisplayOutput>,
	/// Terminal status.
	pub status:          CellStatus,
	/// Whether retained output was truncated.
	pub truncated:       bool,
	/// Durable full-output blob when truncated.
	pub spilled_output:  Option<BlobRef>,
	/// Full output line count.
	pub total_lines:     usize,
	/// Full output byte count.
	pub total_bytes:     usize,
}

/// Typed eval resource or validation failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// The timeout was negative or not finite.
	InvalidTimeout,
	/// The environment resource rejected or lost an operation.
	Resource {
		/// Operation that failed.
		operation: Str,
		/// Resource-owned diagnostic.
		message:   Str,
	},
	/// A worker ended without a terminal cell event.
	SessionLost {
		/// Resource-owned diagnostic.
		message: Str,
	},
}

/// Opaque handle for one persistent Python session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
	/// Resource-owned stable session identifier.
	pub id: Bytes,
}

/// Host-authorized process state applied immediately before one cell.
///
/// Managed environment entries are deltas: `Some` replaces a value and `None`
/// removes it. The app authority sends every managed key on every run so state
/// cannot leak from a prior owner or session.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeSnapshot {
	/// Scoped working directory for this cell.
	pub cwd:         Option<PathBuf>,
	/// Sanitized managed-environment replacements and removals.
	pub managed_env: BTreeMap<Str, Option<Str>>,
}

/// Request to execute one cell in a persistent session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
	/// Exact source text.
	pub code:    Str,
	/// Runtime-work timeout. `None` disables the timeout.
	pub timeout: Option<Duration>,
	/// Whether to replace the persistent namespace first.
	pub reset:   bool,
	/// Frozen host-authorized runtime state for this run.
	pub runtime: RuntimeSnapshot,
}

/// Ordered event from an active cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEvent {
	/// Resource assigned a cell identity.
	Started {
		/// Stable resource-owned identity.
		cell_id: Bytes,
	},
	/// Cell-bounded stdout or stderr.
	Output(Update),
	/// Terminal result.
	Completed(RunCompletion),
}

enum PendingEval {
	Event(Result<Option<RunEvent>, Fault>),
	Interrupt(Result<Interrupt, InterruptWaitError>),
	Background,
}

/// Request-scoped active Python cell.
pub trait EvalRun: Send {
	/// Reports whether this run started from a fresh persistent namespace.
	fn reset(&self) -> bool;
	/// Waits for the next ordered event.
	fn next_event(&mut self) -> impl Future<Output = Result<Option<RunEvent>, Fault>> + Send + '_;

	/// Interrupts the active cell without disposing its session.
	fn cancel(&self) -> impl Future<Output = Result<(), Fault>> + Send + '_;

	/// Transfers this active cell to a managed background job.
	///
	/// Resource adapters that cannot authoritatively report detached settlement
	/// may retain the default refusal; the tool then keeps waiting in the
	/// foreground.
	fn detach(&self, _name: Str) -> impl Future<Output = Result<DetachedJob, Fault>> + Send + '_ {
		future::ready(Err(Fault::Resource {
			operation: sf!("detach"),
			message:   sf!("eval resource does not support managed detachment"),
		}))
	}
}

/// Zero-box resource boundary used by the native eval executor.
pub trait EvalExec: Clone + Send + Sync + 'static {
	/// Active run handle.
	type Run: EvalRun;

	/// Opens the persistent Python session owned by this tool instance.
	fn open_session(&self) -> impl Future<Output = Result<Session, Fault>> + Send + '_;

	/// Opens a persistent session for an authenticated invocation owner.
	///
	/// Executors that key resources by owner override this method. Child-local
	/// and test executors may use the owner-independent default.
	fn open_session_for(
		&self,
		_owner: &str,
	) -> impl Future<Output = Result<Session, Fault>> + Send + '_ {
		self.open_session()
	}

	/// Freezes the runtime state authorized for one owner/session pair.
	fn runtime_snapshot(&self, _owner: &str, _session: &Session) -> Result<RuntimeSnapshot, Fault> {
		Ok(RuntimeSnapshot::default())
	}

	/// Starts one cell in an existing session.
	fn run<'a>(
		&'a self,
		session: &'a Session,
		request: RunRequest,
	) -> impl Future<Output = Result<Self::Run, Fault>> + Send + 'a;

	/// Starts one cell with an explicit lifetime policy.
	fn run_with_mode<'a>(
		&'a self,
		session: &'a Session,
		request: RunRequest,
		_disposable: bool,
	) -> impl Future<Output = Result<Self::Run, Fault>> + Send + 'a {
		self.run(session, request)
	}

	/// Disposes one persistent session before a reset cell.
	fn dispose_session(
		&self,
		_session: &Session,
	) -> impl Future<Output = Result<(), Fault>> + Send + '_ {
		future::ready(Ok(()))
	}

	/// Requests disposal of every persistent session owned by this executor.
	fn dispose_all(&self) {}
}

fn format_display_json(outputs: &[DisplayOutput]) -> String {
	let mut rendered = Vec::new();
	let mut index = 0usize;
	for output in outputs {
		let DisplayOutput::Json { data } = output else {
			continue;
		};
		index += 1;
		let mut text = serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
		if text.len() > MAX_DISPLAY_TEXT_BYTES {
			let mut end = MAX_DISPLAY_TEXT_BYTES;
			while !text.is_char_boundary(end) {
				end -= 1;
			}
			let elided = text.len() - end;
			text.truncate(end);
			if !text.ends_with('\n') {
				text.push('\n');
			}
			let _ = writeln!(text, "[…{elided}ch elided…]");
		}
		rendered.push(format!("display[{index}]:\n{text}"));
	}
	rendered.join("\n\n")
}

/// Bounded live view of an eval cell's ordered text output.
///
/// The first and most recent halves are retained. Once output crosses the
/// bound, a stable truncation marker separates them; memory use never depends
/// on the total amount produced by the cell.
pub struct TailBuffer {
	head:        Vec<u8>,
	tail:        VecDeque<u8>,
	total_bytes: usize,
	max_bytes:   usize,
}

impl TailBuffer {
	/// Creates a live buffer with a hard byte bound.
	pub fn new(max_bytes: usize) -> Self {
		Self {
			head: Vec::with_capacity(max_bytes / 2),
			tail: VecDeque::with_capacity(max_bytes.saturating_sub(max_bytes / 2)),
			total_bytes: 0,
			max_bytes,
		}
	}

	/// Appends one ordered update and returns the current bounded snapshot.
	pub fn push(&mut self, update: &Update) -> Update {
		let head_limit = self.max_bytes / 2;
		let tail_limit = self.max_bytes.saturating_sub(head_limit);
		let data = update.data.as_ref();
		self.total_bytes = self.total_bytes.saturating_add(data.len());

		let head_remaining = head_limit.saturating_sub(self.head.len());
		let head_len = head_remaining.min(data.len());
		self.head.extend_from_slice(&data[..head_len]);
		for byte in &data[head_len..] {
			if tail_limit == 0 {
				break;
			}
			if self.tail.len() == tail_limit {
				self.tail.pop_front();
			}
			self.tail.push_back(*byte);
		}

		let mut snapshot = Vec::with_capacity(self.max_bytes.saturating_add(64));
		snapshot.extend_from_slice(&self.head);
		let retained = self.head.len().saturating_add(self.tail.len());
		if self.total_bytes > retained {
			let omitted = self.total_bytes - retained;
			let _ = write!(snapshot, "\n[…{omitted} bytes truncated…]\n");
		}
		snapshot.extend(self.tail.iter());
		Update {
			channel:  update.channel,
			data:     CowBytes::from(snapshot),
			sequence: update.sequence,
		}
	}
}

struct OutputRetention {
	head:       Vec<OutputFrame>,
	tail:       VecDeque<OutputFrame>,
	head_bytes: usize,
	tail_bytes: usize,
	truncated:  bool,
}

impl OutputRetention {
	const HALF: usize = MAX_RETAINED_OUTPUT_BYTES / 2;

	const fn new() -> Self {
		Self {
			head:       Vec::new(),
			tail:       VecDeque::new(),
			head_bytes: 0,
			tail_bytes: 0,
			truncated:  false,
		}
	}

	fn push(&mut self, update: &Update) {
		let data = update.data.as_ref();
		if self.head_bytes < Self::HALF {
			let retained = data.len().min(Self::HALF - self.head_bytes);
			if retained > 0 {
				self.head.push(OutputFrame {
					channel:  update.channel,
					data:     CowBytes::from(data[..retained].to_vec()),
					sequence: update.sequence,
				});
				self.head_bytes += retained;
			}
			if retained == data.len() {
				return;
			}
		}
		self.truncated = true;
		let retained = data.len().min(Self::HALF);
		if retained == 0 {
			return;
		}
		let tail = &data[data.len() - retained..];
		self.tail.push_back(OutputFrame {
			channel:  update.channel,
			data:     CowBytes::from(tail.to_vec()),
			sequence: update.sequence,
		});
		self.tail_bytes += retained;
		while self.tail_bytes > Self::HALF {
			let Some(front) = self.tail.pop_front() else {
				break;
			};
			self.tail_bytes = self.tail_bytes.saturating_sub(front.data.as_ref().len());
		}
	}

	fn finish(mut self) -> (Vec<OutputFrame>, bool) {
		self.head.extend(self.tail);
		(self.head, self.truncated)
	}
}

/// Python-only `eval@1` implementation retaining one lazy session per owner.
pub struct EvalTool<E: EvalExec> {
	exec: E,
	sessions: DashMap<Str, Arc<OwnerSession>>,
	control: EvalSessionControl,
	spec: ToolSpec,
	next_background_name: AtomicU64,
	auto_background_threshold: Duration,
}

struct OwnerSession {
	session:          OnceCell<Session>,
	reset_generation: AtomicU64,
}

/// External reset and disposal trigger used when chat identity changes.
#[derive(Clone)]
pub struct EvalSessionControl {
	inner: Arc<EvalSessionControlInner>,
}

enum EvalSessionControlInner {
	Local { reset_generation: AtomicU64, dispose_all: Arc<dyn Fn() + Send + Sync> },
	Remote { client: EnvClient, runtime: runtime::Handle },
}

impl Default for EvalSessionControl {
	fn default() -> Self {
		Self {
			inner: Arc::new(EvalSessionControlInner::Local {
				reset_generation: AtomicU64::new(0),
				dispose_all:      Arc::new(|| {}),
			}),
		}
	}
}

impl EvalSessionControl {
	/// Creates a reset capability backed by a remote Environment client.
	///
	/// Calls to [`Self::request_reset`] remain synchronous and schedule exactly
	/// one cold protocol request on the Tokio runtime active at construction.
	#[must_use]
	pub fn from_client(client: EnvClient) -> Self {
		Self {
			inner: Arc::new(EvalSessionControlInner::Remote {
				client,
				runtime: runtime::Handle::current(),
			}),
		}
	}

	/// Disposes every live process and makes each owner's next cell fresh.
	pub fn request_reset(&self) {
		match self.inner.as_ref() {
			EvalSessionControlInner::Local { reset_generation, dispose_all } => {
				reset_generation.fetch_add(1, Ordering::AcqRel);
				dispose_all();
			},
			EvalSessionControlInner::Remote { client, runtime } => {
				let client = client.clone();
				drop(runtime.spawn(async move {
					if let Err(error) = client.reset_eval().await {
						tracing::warn!(error = ?error, "remote evaluation reset failed");
					}
				}));
			},
		}
	}

	fn reset_generation(&self) -> u64 {
		match self.inner.as_ref() {
			EvalSessionControlInner::Local { reset_generation, .. } => {
				reset_generation.load(Ordering::Acquire)
			},
			EvalSessionControlInner::Remote { .. } => 0,
		}
	}
}

/// Constructs `eval@1` over a persistent Python resource.
pub fn eval<E: EvalExec>(exec: E) -> EvalTool<E> {
	eval_controlled(exec).0
}

/// Constructs `eval@1` together with its owning session reset capability.
pub fn eval_controlled<E: EvalExec>(exec: E) -> (EvalTool<E>, EvalSessionControl) {
	eval_controlled_described(exec, standard_eval_description())
}

/// Builds the host-free `eval@1` declaration for a frozen prompt description.
pub fn spec(description: Str) -> ToolSpec {
	ToolSpec {
		name: sf!("eval"),
		rev: Rev { family: Str::default(), n: 1 },
		description,
		schema: omp_tool::schema::<Params>(),
		constraint: Constraint::Schema {
			priority:       100,
			on_unsupported: omp_tool::Fallback::Unspecified,
		},
		effects: Effects {
			documents: Some(DocEffects {
				read:        true,
				write_globs: [sf!("**")].into_iter().collect(),
			}),
			exec:      Some(ExecEffects {
				commands: [sf!("*")].into_iter().collect(),
				network:  true,
			}),
			inference: Some(InferenceEffects {
				max_requests: u32::MAX,
				max_usd:      Usd::from_nanos(u64::MAX),
			}),
			desktop:   None,
			subagents: u32::MAX,
		},
		projection_code: omp_tool::native_projection_code(
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			include_bytes!("eval.rs"),
		)
		.into(),
	}
}

/// Constructs `eval@1` with task guidance frozen from one live session
/// snapshot.
pub fn eval_controlled_with_task_snapshot<E: EvalExec>(
	exec: E,
	snapshot: TaskDescriptionSnapshot<'_>,
) -> (EvalTool<E>, EvalSessionControl) {
	eval_controlled_described(exec, task_description(snapshot))
}

fn eval_controlled_described<E: EvalExec>(
	exec: E,
	description: Str,
) -> (EvalTool<E>, EvalSessionControl) {
	let disposer = exec.clone();
	let control = EvalSessionControl {
		inner: Arc::new(EvalSessionControlInner::Local {
			reset_generation: AtomicU64::new(0),
			dispose_all:      Arc::new(move || disposer.dispose_all()),
		}),
	};
	let tool = EvalTool {
		exec,
		sessions: DashMap::new(),
		control: control.clone(),
		next_background_name: AtomicU64::new(1),
		auto_background_threshold: DEFAULT_AUTO_BACKGROUND_THRESHOLD,
		spec: spec(description),
	};
	(tool, control)
}

impl<E: EvalExec> EvalTool<E> {
	/// Overrides how long eval cells wait before managed detachment.
	pub const fn with_auto_background_threshold(mut self, threshold: Duration) -> Self {
		self.auto_background_threshold = threshold;
		self
	}
}

impl<E: EvalExec> Tool for EvalTool<E> {
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
	) -> impl Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		let owner = params
			.owner()
			.cloned()
			.unwrap_or_else(|| sf!("__direct_eval_owner__"));
		stream! {
			let args = match params.whole::<Params>().await {
				Ok(args) => args,
				Err(error) => {
					yield param_event(error);
					return;
				},
			};
			let timeout = match args.timeout {
				None => Some(Duration::from_secs(30)),
				Some(0.0) => None,
				Some(value) if value.is_finite() && value > 0.0 =>
					Some(Duration::from_secs_f64(value).min(MAX_CELL_TIMEOUT)),
				Some(_) => {
					yield Ev::Done(ToolTerminal::Done { result: Err(Fault::InvalidTimeout), useless: false });
					return;
				},
			};
			if let Err(error) = params.committed().await {
				yield commit_event(error);
				return;
			}

			let reset_generation = self.control.reset_generation();
			let owned = self
				.sessions
				.entry(owner.clone())
				.or_insert_with(|| Arc::new(OwnerSession {
					session: OnceCell::new(),
					reset_generation: AtomicU64::new(reset_generation),
				}))
				.clone();
			let session = match owned
				.session
				.get_or_try_init(|| self.exec.open_session_for(owner.as_str()))
				.await
			{
				Ok(session) => session.clone(),
				Err(fault) => {
					yield Ev::Done(ToolTerminal::Done { result: Err(fault), useless: false });
					return;
				},
			};
			let reset = args.reset.unwrap_or(false)
				|| owned.reset_generation.swap(reset_generation, Ordering::AcqRel) != reset_generation;
			if reset
				&& let Err(fault) = self.exec.dispose_session(&session).await
			{
				yield Ev::Done(ToolTerminal::Done { result: Err(fault), useless: false });
				return;
			}
			let runtime = match self.exec.runtime_snapshot(owner.as_str(), &session) {
				Ok(runtime) => runtime,
				Err(fault) => {
					yield Ev::Done(ToolTerminal::Done { result: Err(fault), useless: false });
					return;
				},
			};
			let disposable = args.kernel_mode == Some(KernelMode::PerCall);
			let mut run = match self.exec.run_with_mode(&session, RunRequest {
				code: args.code.clone(),
				timeout,
				reset,
				runtime,
			}, disposable).await {
				Ok(run) => run,
				Err(fault) => {
					yield Ev::Done(ToolTerminal::Done { result: Err(fault), useless: false });
					return;
				},
			};
			let reset = run.reset();
			let foreground_wait =
				ForegroundWait::new(self.auto_background_threshold, timeout);
			let mut auto_background = true;

			let mut cell_id = Bytes::new();
			let mut frames = OutputRetention::new();
			let mut live_tail = TailBuffer::new(MAX_RETAINED_OUTPUT_BYTES);
			let mut cancellation_reason: Option<Str> = None;
			loop {
				let event = if cancellation_reason.is_some() {
					run.next_event().await
				} else {
					let selected = if auto_background {
						match foreground_wait
							.race(run.next_event(), params.next_interrupt())
							.await
						{
							JobWait::Settled(event) => PendingEval::Event(event),
							JobWait::Interrupted(interrupt) => PendingEval::Interrupt(interrupt),
							JobWait::Background => PendingEval::Background,
						}
					} else {
						let next = run.next_event().fuse();
						let interrupt = params.next_interrupt().fuse();
						pin_mut!(next, interrupt);
						match futures::future::select(interrupt, next).await {
							Either::Left((interrupt, _)) => PendingEval::Interrupt(interrupt),
							Either::Right((event, _)) => PendingEval::Event(event),
						}
					};
					match selected {
						PendingEval::Background => {
							let name =
								next_background_name("eval", &self.next_background_name);
							if let Ok(job) = run.detach(name).await {
								yield Ev::Done(detached_terminal(job));
								return;
							}
							auto_background = false;
							continue;
						},
						PendingEval::Interrupt(interrupt) => {
							let interrupt = match interrupt {
								Ok(interrupt) => interrupt,
								Err(InterruptWaitError::Closed) => Interrupt {
									class: sf!("closed"),
									reason: sf!("invocation owner disappeared"),
								},
								Err(InterruptWaitError::Protocol(reason)) => Interrupt {
									class: sf!("protocol"),
									reason,
								},
							};
							if interrupt.class == Interrupt::STEERING {
								let name =
									next_background_name("eval", &self.next_background_name);
								if let Ok(job) = run.detach(name).await {
									yield Ev::Done(detached_terminal(job));
									return;
								}
							}
							let reason = interrupt.reason;
							if run.cancel().await.is_err() {
								yield Ev::Aborted(Abort::EffectsUnknown { reason });
								return;
							}
							cancellation_reason = Some(reason);
							continue;
						},
						PendingEval::Event(event) => event,
					}
				};

				match event {
					Ok(Some(RunEvent::Started { cell_id: id })) => cell_id = id,
					Ok(Some(RunEvent::Output(update))) => {
						frames.push(&update);
						yield Ev::Update(live_tail.push(&update));
					},
					Ok(Some(RunEvent::Completed(done))) => {
						if let Some(reason) = cancellation_reason {
							yield Ev::Aborted(Abort::EffectsUnknown { reason });
							return;
						}
						let (frames, retained_truncated) = frames.finish();
						yield Ev::Done(ToolTerminal::Done {
							result: Ok(Payload {
								session_id: session.id,
								cell_id,
								language: args.language,
								title: args.title,
								code: args.code,
								reset,
								frames,
								result: done.result,
								display_outputs: done.display_outputs,
								status: done.status,
								truncated: done.truncated || retained_truncated,
								spilled_output: done.spilled_output,
								total_lines: done.total_lines,
								total_bytes: done.total_bytes,
							}),
							useless: false,
						});
						return;
					},
					Ok(None) => {
						yield Ev::Aborted(Abort::EffectsUnknown {
							reason: cancellation_reason.unwrap_or_else(|| sf!("eval event stream ended before terminal status")),
						});
						return;
					},
					Err(fault) => {
						if let Some(reason) = cancellation_reason {
							yield Ev::Aborted(Abort::EffectsUnknown { reason });
						} else {
							yield Ev::Done(ToolTerminal::Done { result: Err(fault), useless: false });
						}
						return;
					},
				}
			}
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, caps: &PromptCaps) -> Vec<Part> {
		let payload = match view {
			Ok(payload) => payload,
			Err(fault) => {
				let Some(mut projection) = TextProjection::new(*caps) else {
					return Vec::new();
				};
				let message = match fault {
					Fault::InvalidTimeout => {
						"eval timeout must be a finite non-negative number".to_owned()
					},
					Fault::Resource { operation, message } => {
						format!("eval {operation} failed: {message}")
					},
					Fault::SessionLost { message } => format!("eval session lost: {message}"),
				};
				projection.push(&message);
				return projection.finish();
			},
		};

		let mut stdout = String::new();
		for frame in &payload.frames {
			stdout.push_str(&String::from_utf8_lossy(&frame.data));
		}
		if let Some(result) = &payload.result
			&& !result.text.is_empty()
		{
			stdout.push_str(&result.text);
			if !result.text.ends_with('\n') {
				stdout.push('\n');
			}
		}
		for display in &payload.display_outputs {
			match display {
				DisplayOutput::Markdown { text } => {
					stdout.push_str(text);
					if !text.ends_with('\n') {
						stdout.push('\n');
					}
				},
				DisplayOutput::Image { description, .. } if !description.is_empty() => {
					stdout.push_str(description);
					if !description.ends_with('\n') {
						stdout.push('\n');
					}
				},
				DisplayOutput::Json { .. }
				| DisplayOutput::ImageData { .. }
				| DisplayOutput::Image { .. }
				| DisplayOutput::Status { .. } => {},
			}
		}
		if let Some(exception) = &payload.status.exception {
			if exception.traceback.is_empty() {
				stdout.push_str(&exception.name);
				stdout.push_str(": ");
				stdout.push_str(&exception.message);
				stdout.push('\n');
			} else {
				for line in &exception.traceback {
					stdout.push_str(line);
					if !line.ends_with('\n') {
						stdout.push('\n');
					}
				}
			}
		}

		let stdout = stdout.trim();
		let display_text = format_display_json(&payload.display_outputs);
		let image_count = payload
			.display_outputs
			.iter()
			.filter(|output| {
				matches!(output, DisplayOutput::Image { .. } | DisplayOutput::ImageData { .. })
			})
			.count();
		let visible_display = if display_text.is_empty() && image_count != 0 && stdout.is_empty() {
			format!(
				"(displayed {image_count} image{}; no text output)",
				if image_count == 1 { "" } else { "s" }
			)
		} else {
			display_text
		};
		let stdout_empty = stdout.is_empty();
		let visible_display_empty = visible_display.is_empty();
		let mut text = match (stdout_empty, visible_display_empty) {
			(false, false) => format!("{stdout}\n\n{visible_display}"),
			(false, true) => stdout.to_owned(),
			(true, false) => visible_display,
			(true, true) => "(no output)".to_owned(),
		};

		match payload.status.outcome {
			CellOutcome::Error => {
				let code = payload.status.exit_code.unwrap_or(1);
				text = if stdout_empty && visible_display_empty {
					format!("Command exited with code {code}")
				} else {
					format!("{text}\n\nCommand exited with code {code}")
				};
			},
			CellOutcome::Timeout if stdout_empty && visible_display_empty => {
				text.clear();
				text.push_str("Command timed out");
			},
			CellOutcome::Cancelled if stdout_empty && visible_display_empty => {
				text.clear();
				text.push_str("Command aborted");
			},
			CellOutcome::Complete | CellOutcome::Timeout | CellOutcome::Cancelled => {},
		}

		if payload.truncated {
			let shown_lines = if text.is_empty() {
				0
			} else {
				text.lines().count()
			};
			if let Some(blob) = &payload.spilled_output {
				let _ = write!(
					text,
					"\n\n[truncated: {shown_lines} of {} lines shown; full output in blob {}]",
					payload.total_lines, blob.hash
				);
			} else {
				let _ = write!(
					text,
					"\n\n[truncated: {shown_lines} of {} lines shown]",
					payload.total_lines
				);
			}
		}

		let Some(mut projection) = TextProjection::new(*caps) else {
			return Vec::new();
		};
		projection.push(&text);
		let mut parts = projection.finish();
		if caps.media {
			let mut image_index = 0usize;
			for output in &payload.display_outputs {
				if parts.len() >= usize::from(caps.maximum_parts) {
					break;
				}
				if let DisplayOutput::Image { blob, description, .. } = output {
					image_index += 1;
					parts.push(Part::Blob {
						blob: blob.clone(),
						alt:  Some(if description.is_empty() {
							sf!("display image {image_index}")
						} else {
							description.clone()
						}),
					});
				}
			}
		}
		parts
	}

	fn invoke_input(&self, update: &Update, invocation_id: &str) -> Option<InvokeInput> {
		let channel = match update.channel {
			OutputChannel::Stdout => chunk::Channel::Stdout,
			OutputChannel::Stderr => chunk::Channel::Stderr,
		};
		Some(InvokeInput {
			invocation_id: invocation_id.to_owned(),
			payload:       Some(invoke_input::Payload::Chunk(invoke_input::Chunk {
				channel: channel as i32,
				data:    update.data.clone().into_bytes(),
			})),
		})
	}
}

fn detached_terminal(job: DetachedJob) -> ToolTerminal<Payload, Fault> {
	managed_job_terminal(job, omp_tool::JobKind::Eval, sf!("eval cell settlement"))
}

fn param_event<U, P>(error: ParamError) -> Ev<U, P, Fault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(reason) => Ev::Args(protocol_issue(reason)),
	}
}

fn commit_event<U, P>(error: CommitError) -> Ev<U, P, Fault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		CommitError::Protocol(reason) => Ev::Args(protocol_issue(reason)),
	}
}

fn protocol_issue(reason: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one complete eval@1 Python cell object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"language":"py","code":"1 + 1"}}"#)),
		found:    Some(reason),
	}
}

mod cow_bytes {
	use omp_core::CowBytes;
	use serde::{Deserialize, Deserializer, Serialize, Serializer};

	pub(super) fn serialize<S: Serializer>(
		value: &CowBytes<'static>,
		serializer: S,
	) -> Result<S::Ok, S::Error> {
		value.serialize(serializer)
	}

	pub(super) fn deserialize<'de, D: Deserializer<'de>>(
		deserializer: D,
	) -> Result<CowBytes<'static>, D::Error> {
		Vec::<u8>::deserialize(deserializer).map(CowBytes::from)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn standard_task_description_omits_empty_extension_helpers() {
		let description = standard_eval_description();

		assert_eq!(description, task_description(TaskDescriptionSnapshot::standard()));
		assert!(!description.contains("<extension-helpers>"));
		assert!(description.ends_with("</task-runtime>"));
	}

	#[test]
	fn eval_guidance_tracks_subagent_availability_and_timeout_precedence() {
		let available = task_description(TaskDescriptionSnapshot::standard());
		assert!(available.contains("Eval `agent()` children use independent kernels."));
		assert!(available.contains("agent(prompt"));
		assert!(available.contains(
			"`timeout: 0` disables the cell deadline; otherwise `timeout` sets it without extending \
			 foreground waiting."
		));

		let unavailable = task_description(TaskDescriptionSnapshot {
			agents: &[],
			..TaskDescriptionSnapshot::standard()
		});
		assert!(!unavailable.contains("agent(prompt"));
		assert!(!unavailable.contains("agent()"));
		assert!(!unavailable.contains("<dag>"));
		assert!(!unavailable.contains("<task-runtime>"));
	}

	#[test]
	fn task_description_renders_extension_helpers_after_runtime() {
		let helpers = [
			PreludeHelperDescription {
				signature: "merge_patches(patches, *, strategy=\"sequential\")",
				summary:   "Merge patches using the requested strategy.",
			},
			PreludeHelperDescription {
				signature: "workspace_root()",
				summary:   "Return the active workspace root.",
			},
		];
		let description = task_description(TaskDescriptionSnapshot {
			helpers: &helpers,
			..TaskDescriptionSnapshot::standard()
		});

		assert!(description.contains("</task-runtime>\n\n<extension-helpers>"));
		assert!(description.ends_with(
			"<extension-helpers>\nExtension-provided prelude functions (call like any prelude \
			 helper):\n- `merge_patches(patches, *, strategy=\"sequential\")` — Merge patches using \
			 the requested strategy.\n- `workspace_root()` — Return the active workspace \
			 root.\n</extension-helpers>"
		));
	}

	#[test]
	fn params_accept_omitted_optionals_and_reject_invalid_fields() {
		let python: Params = serde_json::from_value(serde_json::json!({
			"language": "py",
			"code": "value = 1"
		}))
		.expect("Python cell parses");
		assert_eq!(python.language, Language::Py);
		assert_eq!(python.title, None);
		assert_eq!(python.timeout, None);
		assert_eq!(python.reset, None);
		assert_eq!(python.kernel_mode, None);
		assert!(
			serde_json::from_value::<Params>(serde_json::json!({
				"language": "js",
				"code": "1 + 1"
			}))
			.is_err()
		);
		assert!(
			serde_json::from_value::<Params>(serde_json::json!({
				"language": "py",
				"code": "1 + 1",
				"extra": true
			}))
			.is_err()
		);
	}

	#[test]
	fn tail_buffer_emits_moving_bounded_snapshots() {
		let mut tail = TailBuffer::new(12);
		let first = tail.push(&Update {
			channel:  OutputChannel::Stdout,
			data:     CowBytes::from(b"abcdef".to_vec()),
			sequence: 0,
		});
		assert_eq!(first.data.as_ref(), b"abcdef");

		let second = tail.push(&Update {
			channel:  OutputChannel::Stderr,
			data:     CowBytes::from(b"ghijklmnop".to_vec()),
			sequence: 1,
		});
		let rendered = String::from_utf8_lossy(second.data.as_ref());
		assert!(rendered.starts_with("abcdef"));
		assert!(rendered.contains("4 bytes truncated"));
		assert!(rendered.ends_with("klmnop"));
		assert_eq!(second.channel, OutputChannel::Stderr);
		assert_eq!(second.sequence, 1);
		assert!(second.data.as_ref().len() <= 64);
	}

	#[test]
	fn output_retention_keeps_bounded_head_and_tail() {
		let mut retention = OutputRetention::new();
		for (sequence, byte) in [(0, b'a'), (1, b'b'), (2, b'c')] {
			retention.push(&Update {
				channel: OutputChannel::Stdout,
				data: CowBytes::from(vec![byte; MAX_RETAINED_OUTPUT_BYTES]),
				sequence,
			});
		}
		let (frames, truncated) = retention.finish();
		assert!(truncated);
		assert!(
			frames
				.iter()
				.map(|frame| frame.data.as_ref().len())
				.sum::<usize>()
				<= MAX_RETAINED_OUTPUT_BYTES
		);
		assert_eq!(frames.first().and_then(|frame| frame.data.as_ref().first()), Some(&b'a'));
		assert_eq!(frames.last().and_then(|frame| frame.data.as_ref().last()), Some(&b'c'));
	}
	#[test]
	fn local_control_increments_generation_and_disposes_synchronously() {
		let disposals = Arc::new(AtomicU64::new(0));
		let observed = Arc::clone(&disposals);
		let control = EvalSessionControl {
			inner: Arc::new(EvalSessionControlInner::Local {
				reset_generation: AtomicU64::new(0),
				dispose_all:      Arc::new(move || {
					observed.fetch_add(1, Ordering::AcqRel);
				}),
			}),
		};

		control.request_reset();
		assert_eq!(control.reset_generation(), 1);
		assert_eq!(disposals.load(Ordering::Acquire), 1);
	}

	#[tokio::test]
	async fn remote_control_emits_exactly_one_reset_request() {
		let (client, transport) = EnvClient::in_process(0);
		let (requests, responses) = transport.into_parts();
		let control = EvalSessionControl::from_client(client);
		control.request_reset();

		let request = requests.recv_async().await.expect("receive eval reset");
		assert!(matches!(request.body, Some(omp_proto::env::v1::client_frame::Body::EvalReset(_))));
		responses
			.send_async(omp_proto::env::v1::ServerFrame {
				request_id: request.request_id,
				body: Some(omp_proto::env::v1::server_frame::Body::EvalReset(
					omp_proto::env::v1::EvalResetResponse {},
				)),
				..omp_proto::env::v1::ServerFrame::default()
			})
			.await
			.expect("answer eval reset");
		tokio::task::yield_now().await;
		assert!(requests.try_recv().is_err(), "control emitted more than one reset request");
	}
}
