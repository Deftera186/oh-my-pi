//! Journal-first child-kernel spawn composition.

use std::{
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH},
};

use omp_agent::{
	BackgroundToolCancellation, JobBoard, RunControl, SessionTool, SessionToolCx, SessionToolFuture,
	TurnInput,
};
use omp_con::{CfgLoader, ConError, Ctx};
use omp_core::{Str, Ulid};
use omp_dom::{PropId, PropKey, Value};
use omp_session::{
	Session, SessionError,
	components::jobs::{self, JobSpec},
};
use omp_tool::{CallOutcome, ToolSpec};
use omp_tools::task::{
	ChildRequest, ChildResult, Fault as TaskFault, Params as TaskParams, Payload as TaskPayload,
	SubagentSpawner, Update as TaskUpdate,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::settings::{TaskSettings, child_ctx};
use crate::headless::{
	HeadlessError,
	kernel::{KernelOptions, compose_kernel},
};

/// Declaration-only spawner used to place `task@1` in the frozen registry.
///
/// Dispatcher session routing intercepts the call before this value can run.
pub struct TaskDeclarationSpawner;

impl SubagentSpawner for TaskDeclarationSpawner {
	async fn spawn<'a>(
		&'a self,
		_owner: &'a str,
		_request: TaskParams,
		_updates: &'a flume::Sender<TaskUpdate>,
	) -> Result<TaskPayload, TaskFault> {
		Err(TaskFault { message: Str::new_static("task session dispatcher is unavailable") })
	}
}

/// Concrete driver-owned implementation of the tools crate's spawn seam.
///
/// The parent session mutex is an integration boundary, not durable state: all
/// lifecycle truth is committed to its journal and DOM by [`spawn_child`].
pub struct DriverSubagentSpawner {
	/// Parent journal controller.
	pub parent:       Arc<tokio::sync::Mutex<Session>>,
	/// Production data root.
	pub data_dir:     PathBuf,
	/// Parent or isolated project root.
	pub project_root: PathBuf,
	/// Parent sessions directory.
	pub sessions_dir: PathBuf,
	/// Shared live-session routing authority.
	pub sessions:     Arc<crate::sessions::SessionRegistry>,
	/// Parent effective console context.
	pub parent_ctx:   Arc<Ctx>,
	/// Runtime job index paired with the parent DOM.
	pub jobs:         Arc<JobBoard>,
	/// Configuration script loader.
	pub cfg:          Arc<dyn CfgLoader>,
	/// Model selector used unless a driver policy resolves another route.
	pub model:        Str,
}

impl SubagentSpawner for DriverSubagentSpawner {
	async fn spawn<'a>(
		&'a self,
		owner: &'a str,
		request: TaskParams,
		updates: &'a flume::Sender<TaskUpdate>,
	) -> Result<TaskPayload, TaskFault> {
		let mut children = Vec::with_capacity(request.tasks.len());
		for child in request.tasks {
			let announced = child
				.name
				.clone()
				.unwrap_or_else(|| Str::new_static("pending"));
			let _ = updates
				.send_async(TaskUpdate { id: announced, status: Str::new_static("starting") })
				.await;
			let mut parent = self.parent.lock().await;
			let result = spawn_child(&mut parent, SpawnRequest {
				data_dir: &self.data_dir,
				project_root: &self.project_root,
				sessions_dir: &self.sessions_dir,
				sessions: &self.sessions,
				parent_ctx: &self.parent_ctx,
				cfg: self.cfg.as_ref(),
				jobs: &self.jobs,
				cancel: BackgroundToolCancellation::from_token_for_host(CancellationToken::new()),
				owner,
				context: request.context.as_str(),
				model: self.model.as_str(),
				child,
			})
			.await
			.map_err(|source| TaskFault { message: Str::new(source.to_string()) })?;
			let _ = updates
				.send_async(TaskUpdate {
					id:     result.id.clone(),
					status: Str::new_static("completed"),
				})
				.await;
			children.push(result);
		}
		Ok(TaskPayload { children })
	}
}

/// Session-owned `task@1` implementation composed by the driver.
pub struct TaskSessionTool {
	data_dir:     PathBuf,
	project_root: PathBuf,
	sessions_dir: PathBuf,
	sessions:     Arc<crate::sessions::SessionRegistry>,
	parent_ctx:   Arc<Ctx>,
	cfg:          Arc<dyn CfgLoader>,
	model:        Str,
	spec:         ToolSpec,
}

impl TaskSessionTool {
	/// Creates the task tool using host-owned child composition inputs.
	#[must_use]
	pub fn new(
		data_dir: PathBuf,
		project_root: PathBuf,
		sessions_dir: PathBuf,
		sessions: Arc<crate::sessions::SessionRegistry>,
		parent_ctx: Arc<Ctx>,
		cfg: Arc<dyn CfgLoader>,
		model: Str,
	) -> Self {
		Self {
			data_dir,
			project_root,
			sessions_dir,
			sessions,
			parent_ctx,
			cfg,
			model,
			spec: omp_tools::task::spec(),
		}
	}
}

impl SessionTool for TaskSessionTool {
	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'a>(
		&'a self,
		cx: SessionToolCx<'a>,
		args: Box<serde_json::value::RawValue>,
	) -> SessionToolFuture<'a> {
		Box::pin(async move {
			let mut value: serde_json::Value = serde_json::from_str(args.get())?;
			if let Some(object) = value.as_object_mut() {
				object.remove("i");
			}
			let request: TaskParams = serde_json::from_value(value)?;
			let mut children = Vec::with_capacity(request.tasks.len());
			for child in request.tasks {
				match spawn_child(cx.session, SpawnRequest {
					data_dir: &self.data_dir,
					project_root: &self.project_root,
					sessions_dir: &self.sessions_dir,
					sessions: &self.sessions,
					parent_ctx: &self.parent_ctx,
					cfg: self.cfg.as_ref(),
					jobs: cx.jobs,
					cancel: cx.cancel.clone(),
					owner: "Main",
					context: request.context.as_str(),
					model: self.model.as_str(),
					child,
				})
				.await
				{
					Ok(result) => children.push(result),
					Err(source) => {
						let fault = serde_json::value::to_raw_value(&TaskFault {
							message: Str::new(source.to_string()),
						})?;
						return Ok(CallOutcome::Faulted(fault));
					},
				}
			}
			let payload = serde_json::value::to_raw_value(&TaskPayload { children })?;
			Ok(CallOutcome::Ok(payload))
		})
	}
}

/// Failure to configure, journal, compose, or run one child kernel.
#[derive(Debug, Error)]
pub enum SpawnError {
	/// Child convar seeding or cfg execution failed.
	#[error("child console configuration failed")]
	Con(#[from] ConError),
	/// Parent job-tree update failed.
	#[error("parent job projection failed")]
	Session(#[from] SessionError),
	/// Production kernel composition failed.
	#[error("child kernel composition failed")]
	Headless(#[from] HeadlessError),
	/// Child turn failed.
	#[error("child turn failed")]
	Kernel(#[from] omp_agent::KernelError),
	/// System clock is unavailable.
	#[error("system clock predates the Unix epoch")]
	Clock(#[from] SystemTimeError),
	/// The parent session has no journal head.
	#[error("parent session has no journal head")]
	MissingParentHead,
	/// The standard jobs component is absent.
	#[error("parent session has no jobs component")]
	MissingJobs,
}

/// Host-owned inputs for one child run.
pub struct SpawnRequest<'a> {
	/// Data root used by production composition and artifact storage.
	pub data_dir:     &'a Path,
	/// Parent project root (or its isolated whole-workspace view).
	pub project_root: &'a Path,
	/// Directory in which the child's `.oms` is created.
	pub sessions_dir: &'a Path,
	/// Shared live-session routing authority.
	pub sessions:     &'a Arc<crate::sessions::SessionRegistry>,
	/// Parent's effective convar context at spawn time.
	pub parent_ctx:   &'a Ctx,
	/// User/project cfg loader.
	pub cfg:          &'a dyn CfgLoader,
	/// Runtime index paired with the parent DOM.
	pub jobs:         &'a JobBoard,
	/// Kill boundary for this child.
	pub cancel:       BackgroundToolCancellation,
	/// Parent job owner identity.
	pub owner:        &'a str,
	/// Shared batch context prepended to the child assignment.
	pub context:      &'a str,
	/// Requested model selector.
	pub model:        &'a str,
	/// Typed child request.
	pub child:        ChildRequest,
}

/// Journals a `<subagent>`, runs one independently configured child kernel,
/// then settles the parent element and returns the ordinary task payload row.
pub async fn spawn_child(
	parent: &mut Session,
	request: SpawnRequest<'_>,
) -> Result<ChildResult, SpawnError> {
	let agent = request
		.child
		.agent
		.clone()
		.unwrap_or_else(|| Str::new_static("task"));
	let requested_id = request
		.child
		.name
		.clone()
		.unwrap_or_else(|| Str::new(Ulid::generate().to_string()));
	let id = allocate_id(parent, normalize_id(requested_id));
	let session_path = child_session_path(request.sessions_dir, &id);
	let started = SystemTime::now()
		.duration_since(UNIX_EPOCH)?
		.as_millis()
		.to_string();
	let cause = parent.head().ok_or(SpawnError::MissingParentHead)?;
	let txn = jobs::insert(parent.dom(), cause, JobSpec {
		id:      id.clone(),
		kind:    Str::new_static("subagent"),
		owner:   Str::new(request.owner),
		started: Str::new(started),
		agent:   Some(agent.clone()),
	})
	.ok_or(SpawnError::MissingJobs)?;
	parent.patch(txn)?;
	let handle = parent
		.dom()
		.select(&format!("jobs subagent[id={id}]"))
		.ok()
		.and_then(|mut values| values.next());
	if let Some(handle) = handle {
		request
			.jobs
			.attach(parent.dom(), handle, request.cancel.token());
	}

	let outcome = async {
		let ctx = Arc::new(child_ctx(request.parent_ctx, request.cfg, agent.as_str())?);
		// `ai_task_model` (the picker's task mode) overrides the inherited
		// `ai_model` for the child's own route; empty keeps inheritance.
		let task_model = omp_con::AI_TASK_MODEL.get(&ctx);
		if !task_model.is_empty() {
			omp_con::AI_MODEL
				.set(&ctx, task_model)
				.map_err(SpawnError::Con)?;
		}
		let settings = TaskSettings::from_con(&ctx);
		let options = KernelOptions {
			session: Some(session_path.clone()),
			sessions_dir: Some(request.sessions_dir.to_path_buf()),
			sessions: Some(Arc::clone(request.sessions)),
			session_name: request.child.name.clone().or_else(|| Some(id.clone())),
			..KernelOptions::default()
		};
		let (mut kernel, mut child_session, _) =
			compose_kernel(request.data_dir, request.project_root, request.model, ctx, options)
				.await?;
		let cancellation = request.cancel.token();
		let deadline = (settings.max_runtime_ms != 0)
			.then(|| std::time::Instant::now() + Duration::from_millis(settings.max_runtime_ms));
		let prompt = Str::new(format!("{}\n\n{}", request.context, request.child.task));
		Ok::<_, SpawnError>(
			kernel
				.run_turn(
					&mut child_session,
					TurnInput { text: prompt, attachments: Vec::new() },
					RunControl::new(cancellation, deadline),
				)
				.await?,
		)
	}
	.await;
	if let Some(handle) = handle {
		let cause = parent.head().ok_or(SpawnError::MissingParentHead)?;
		let status = if outcome.is_ok() {
			"completed"
		} else {
			"failed"
		};
		parent.patch(jobs::set_status(cause, handle, status))?;
	}
	let outcome = outcome?;
	Ok(ChildResult {
		id,
		agent,
		text: outcome.assistant_text,
		session_path: Str::new(session_path.to_string_lossy()),
		tokens_in: outcome.tokens_in,
		tokens_out: outcome.tokens_out,
	})
}

fn normalize_id(requested: Str) -> Str {
	let value = requested
		.as_str()
		.chars()
		.filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
		.take(32)
		.collect::<String>();
	if value.is_empty() {
		Str::new(Ulid::generate().to_string())
	} else {
		Str::new(value)
	}
}

fn allocate_id(parent: &Session, requested: Str) -> Str {
	let Some(jobs) = jobs::jobs_handle(parent.dom()) else {
		return requested;
	};
	let exists = |candidate: &str| {
		parent.dom().children(jobs).iter().any(|handle| {
			parent
				.dom()
				.get(*handle)
				.and_then(|node| node.prop(&PropKey::from(PropId::Id)))
				.and_then(Value::as_str)
				.is_some_and(|id| id == candidate)
		})
	};
	if !exists(requested.as_str()) {
		return requested;
	}
	for suffix in 2_u32.. {
		let candidate = Str::new(format!("{requested}-{suffix}"));
		if !exists(candidate.as_str()) {
			return candidate;
		}
	}
	unreachable!("u32 job-name suffix space exhausted")
}

fn child_session_path(sessions_dir: &Path, id: &Str) -> PathBuf {
	let safe = id
		.as_str()
		.chars()
		.map(|ch| {
			if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
				ch
			} else {
				'_'
			}
		})
		.collect::<String>();
	sessions_dir.join(format!("{safe}.oms"))
}
