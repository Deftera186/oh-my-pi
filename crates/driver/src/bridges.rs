//! Driver-owned capabilities injected into the environment host.

use std::{
	collections::BTreeSet,
	env,
	path::{Path, PathBuf},
	sync::{
		Arc, OnceLock,
		atomic::{AtomicU64, Ordering},
	},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::StreamExt as _;
use omp_agent::control;
use omp_core::{EnvPath, Str, sf};
use omp_envd::github_url::GithubCredentialBridge;
use omp_proto::{
	inference::{
		v1,
		v1::{image_event, speak_event},
	},
	thread,
};
use omp_storage::telemetry_index::TelemetryIndex;
use omp_tools::{
	goal,
	read::{
		Fault as ReadFault, resolver,
		resolver::{ResourceCompletion, ResourceList, Scheme, SchemeEntry},
		selector::ParsedSelector,
	},
};
use parking_lot::RwLock;

use crate::{
	discovery,
	discovery::{managed_skills, native},
	modes::{Goal as DriverGoal, GoalStatus, RegimeError, RegimeHandle},
	rulebook::RuleResolver,
	skills::SkillResolver,
	telemetry_upload,
};

/// Late-bound inference facade retained across environment-first composition.
#[derive(Default)]
pub struct InferenceBridge {
	inference: OnceLock<omp_serve::inference::InferenceRpc>,
}

impl InferenceBridge {
	/// Binds the one inference facade for this environment generation.
	pub fn bind(
		&self,
		inference: omp_serve::inference::InferenceRpc,
	) -> Result<(), omp_serve::inference::InferenceRpc> {
		self.inference.set(inference)
	}

	fn inference(
		&self,
	) -> Result<&omp_serve::inference::InferenceRpc, omp_tools::web_search::BackendError> {
		self
			.inference
			.get()
			.ok_or_else(|| omp_tools::web_search::BackendError {
				code:    sf!("backend_unbound"),
				message: sf!("inference is unavailable before composition completes"),
			})
	}
}

#[async_trait::async_trait]
impl omp_envd::SearchInference for InferenceBridge {
	async fn search(
		&self,
		request: v1::SearchRequest,
	) -> Result<v1::SearchResponse, omp_tools::web_search::BackendError> {
		use omp_proto::inference::v1::inference_server::Inference as _;
		self
			.inference()?
			.search(tonic::Request::new(request))
			.await
			.map(tonic::Response::into_inner)
			.map_err(rpc_backend_error)
	}

	async fn generate_image(
		&self,
		request: v1::GenerateImageRequest,
	) -> Result<Vec<thread::v1::Blob>, omp_tools::web_search::BackendError> {
		use omp_proto::inference::v1::inference_server::Inference as _;
		let mut events = self
			.inference()?
			.generate_image(tonic::Request::new(request))
			.await
			.map_err(rpc_backend_error)?
			.into_inner();
		while let Some(event) = events.next().await {
			let event = event.map_err(rpc_backend_error)?;
			if let Some(image_event::Event::Done(done)) = event.event {
				return Ok(done.images);
			}
		}
		Err(omp_tools::web_search::BackendError {
			code:    sf!("media_stream_incomplete"),
			message: sf!("image generation ended without final artifacts"),
		})
	}

	async fn speak(
		&self,
		request: v1::SpeakRequest,
	) -> Result<Vec<u8>, omp_tools::web_search::BackendError> {
		use omp_proto::inference::v1::inference_server::Inference as _;
		let mut events = self
			.inference()?
			.speak(tonic::Request::new(request))
			.await
			.map_err(rpc_backend_error)?
			.into_inner();
		let mut audio = Vec::new();
		while let Some(event) = events.next().await {
			match event.map_err(rpc_backend_error)?.event {
				Some(speak_event::Event::Chunk(chunk)) => {
					audio.extend_from_slice(&chunk.audio);
				},
				Some(speak_event::Event::Done(done)) => {
					if let Some(blob) = done.audio {
						audio.extend_from_slice(&blob.inline);
					}
					return Ok(audio);
				},
				None => {},
			}
		}
		Err(omp_tools::web_search::BackendError {
			code:    sf!("media_stream_incomplete"),
			message: sf!("speech synthesis ended without a final receipt"),
		})
	}
}

fn rpc_backend_error(status: tonic::Status) -> omp_tools::web_search::BackendError {
	omp_tools::web_search::BackendError {
		code:    Str::new(status.code().to_string()),
		message: sf!("inference request failed"),
	}
}

struct ResolverBridge<R> {
	inner: R,
	entry: SchemeEntry,
}

#[async_trait::async_trait]
impl<R> omp_envd::ContentResolver for ResolverBridge<R>
where
	R: resolver::Resolve,
{
	fn entry(&self) -> SchemeEntry {
		self.entry.clone()
	}

	async fn read(
		&self,
		resource: &str,
		selector: &ParsedSelector,
	) -> Result<omp_core::CowBytes<'static>, ReadFault> {
		self.inner.read(resource, selector).await
	}

	async fn read_query(
		&self,
		resource: &str,
		query: Option<&str>,
		selector: &ParsedSelector,
	) -> Result<omp_core::CowBytes<'static>, ReadFault> {
		self.inner.read_query(resource, query, selector).await
	}

	async fn list(
		&self,
		resource: &str,
		max_entries: usize,
		max_bytes: usize,
	) -> Result<ResourceList, ReadFault> {
		self.inner.list(resource, max_entries, max_bytes).await
	}

	async fn path(&self, resource: &str) -> Result<Option<Str>, ReadFault> {
		self.inner.path(resource).await
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, ReadFault> {
		self.inner.complete(query, max_results).await
	}
}

#[derive(Clone)]
struct GoalBinding {
	id:     u64,
	modes:  Arc<RegimeHandle>,
	sender: omp_agent::ControlSender,
}

/// Late-bound durable goal-mode authority for the active agent session.
#[derive(Clone, Default)]
pub struct AgentGoalControl {
	binding: Arc<RwLock<Option<GoalBinding>>>,
	next_id: Arc<AtomicU64>,
}

impl AgentGoalControl {
	/// Binds the active goal projection and agent regime authority until the
	/// returned lease is dropped.
	pub fn bind(
		&self,
		modes: Arc<RegimeHandle>,
		sender: omp_agent::ControlSender,
	) -> AgentGoalBinding {
		let id = self
			.next_id
			.fetch_add(1, Ordering::Relaxed)
			.saturating_add(1);
		*self.binding.write() = Some(GoalBinding { id, modes, sender });
		AgentGoalBinding { control: self.clone(), id }
	}

	fn binding(&self) -> Result<GoalBinding, goal::Fault> {
		self.binding.read().clone().ok_or(goal::Fault::Unavailable)
	}

	fn unbind(&self, id: u64) {
		let mut binding = self.binding.write();
		if binding.as_ref().is_some_and(|binding| binding.id == id) {
			*binding = None;
		}
	}
}

/// Sole-owner lease for one active goal binding.
#[must_use]
pub struct AgentGoalBinding {
	control: AgentGoalControl,
	id:      u64,
}

impl Drop for AgentGoalBinding {
	fn drop(&mut self) {
		self.control.unbind(self.id);
	}
}

#[async_trait::async_trait]
impl omp_envd::GoalAuthority for AgentGoalControl {
	async fn apply(
		&self,
		params: omp_tools::goal::Params,
	) -> Result<Option<omp_tools::goal::Goal>, goal::Fault> {
		let GoalBinding { modes, sender, .. } = self.binding()?;
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis()
			.try_into()
			.unwrap_or(u64::MAX);
		let outcome = match params.op {
			goal::Operation::Create => {
				let objective = params.objective.ok_or(goal::Fault::ObjectiveRequired)?;
				if objective.trim().is_empty() {
					return Err(goal::Fault::ObjectiveRequired);
				}
				if params.token_budget == Some(0) {
					return Err(goal::Fault::InvalidBudget);
				}
				let (activation, newly_started) = ensure_goal_regime(&sender).await?;
				let goal = match modes.set_goal(objective, params.token_budget, now) {
					Ok(goal) => goal,
					Err(error) => {
						if newly_started {
							let _ = sender.stop_regime(activation).await;
						}
						return Err(map_goal_error(error));
					},
				};
				if let Err(error) = update_goal_regime_state(&sender, &activation, &goal).await {
					let _ = modes.drop_goal(now);
					let _ = sender.stop_regime(activation).await;
					return Err(error);
				}
				Some(goal)
			},
			goal::Operation::Get => modes.goal(),
			goal::Operation::Complete => {
				let activation = active_goal_activation(&sender)
					.await?
					.ok_or(goal::Fault::Unavailable)?;
				let goal = modes.complete_goal(now).map_err(map_goal_error)?;
				sender
					.stop_regime(activation)
					.await
					.map_err(map_goal_regime_error)?;
				Some(goal)
			},
			goal::Operation::Resume => {
				let (activation, newly_started) = ensure_goal_regime(&sender).await?;
				let goal = match modes.resume_goal(now) {
					Ok(goal) => goal,
					Err(error) => {
						if newly_started {
							let _ = sender.stop_regime(activation).await;
						}
						return Err(map_goal_error(error));
					},
				};
				if let Err(error) = update_goal_regime_state(&sender, &activation, &goal).await {
					let _ = sender.stop_regime(activation).await;
					return Err(error);
				}
				Some(goal)
			},
			goal::Operation::Drop => {
				let activation = active_goal_activation(&sender)
					.await?
					.ok_or(goal::Fault::Unavailable)?;
				let goal = modes.drop_goal(now).map_err(map_goal_error)?;
				sender
					.stop_regime(activation)
					.await
					.map_err(map_goal_regime_error)?;
				Some(goal)
			},
		};
		Ok(outcome.map(project_goal))
	}
}

async fn update_goal_regime_state(
	sender: &omp_agent::ControlSender,
	activation: &Str,
	goal: &DriverGoal,
) -> Result<(), goal::Fault> {
	let state = omp_agent::GoalRegimeState {
		objective:          goal.objective.clone(),
		budget_tokens:      goal.token_budget,
		spent_tokens:       goal.tokens_used,
		thresholds_crossed: 0,
	};
	let payload = bytes::Bytes::from(
		serde_json::to_vec(&state).expect("goal regime state has infallible JSON serialization"),
	);
	sender
		.update_regime_state(activation.clone(), payload)
		.await
		.map_err(map_goal_regime_error)?;
	Ok(())
}

async fn active_goal_activation(
	sender: &omp_agent::ControlSender,
) -> Result<Option<Str>, goal::Fault> {
	Ok(sender
		.active_regimes()
		.await
		.map_err(map_goal_regime_error)?
		.into_iter()
		.find(|entry| {
			entry.spec_id.as_str() == "goal" && entry.status == omp_agent::RegimeStatus::Active
		})
		.map(|entry| entry.activation))
}

async fn ensure_goal_regime(sender: &omp_agent::ControlSender) -> Result<(Str, bool), goal::Fault> {
	if let Some(activation) = active_goal_activation(sender).await? {
		return Ok((activation, false));
	}
	let receipt = sender
		.start_core_regime("goal", false)
		.await
		.map_err(map_goal_regime_error)?;
	Ok((receipt.activation, true))
}

fn map_goal_regime_error(error: control::ControlError) -> goal::Fault {
	match error {
		control::ControlError::RegimeStart(omp_agent::StartError::Acquire {
			resource,
			outcome: omp_agent::AcquireOutcome::Denied { holder, since },
		}) => {
			goal::Fault::ResourceConflict { resource: Str::new(resource.name()), owner: holder, since }
		},
		_ => goal::Fault::Unavailable,
	}
}

fn map_goal_error(error: RegimeError) -> goal::Fault {
	match error {
		RegimeError::NoGoal => goal::Fault::NoGoal,
		RegimeError::EmptyObjective => goal::Fault::ObjectiveRequired,
		RegimeError::InvalidBudget => goal::Fault::InvalidBudget,
		RegimeError::RegimeInactive { .. } | RegimeError::InvalidPlanArtifact => {
			goal::Fault::ModeConflict
		},
		RegimeError::InvalidGoalTransition { .. } | RegimeError::GoalExists => {
			goal::Fault::InvalidTransition
		},
	}
}

fn project_goal(goal: DriverGoal) -> omp_tools::goal::Goal {
	let status = match goal.status {
		GoalStatus::Active => goal::Status::Active,
		GoalStatus::Paused => goal::Status::Paused,
		GoalStatus::BudgetLimited => goal::Status::BudgetLimited,
		GoalStatus::Complete => goal::Status::Complete,
		GoalStatus::Dropped => goal::Status::Dropped,
	};
	omp_tools::goal::Goal {
		id: goal.id,
		objective: goal.objective,
		status,
		token_budget: goal.token_budget,
		tokens_used: goal.tokens_used,
		time_used_secs: goal.time_used_seconds,
	}
}

struct TelemetryBridge;

impl omp_envd::TelemetryUpload for TelemetryBridge {
	fn start(&self, index: Arc<TelemetryIndex>, credentials: Arc<GithubCredentialBridge>) {
		telemetry_upload::start(index, credentials);
	}
}

/// Builds the deferred Environment command executor only after a live client
/// exists, so MCP configuration and model auth share the exact execution path.
struct CommandCredentialsBridge;

impl omp_envd::CommandCredentialExecutorFactory for CommandCredentialsBridge {
	fn make(
		&self,
		client: omp_env::EnvClient,
		cwd: &Path,
	) -> Arc<dyn omp_inference::auth::command::CommandCredentialExecutor> {
		let cwd = url::Url::from_file_path(cwd)
			.ok()
			.and_then(|url| EnvPath::new(Str::from(url.as_str())).ok())
			.expect("Environment project roots are absolute file paths");
		Arc::new(crate::auth_backend::EnvCommandCredentialExecutor::new(
			client,
			cwd,
			Duration::from_secs(30),
			64 * 1024,
		))
	}
}

/// Builds all driver-owned environment registry bridges for one project.
pub fn builtin(
	root: &Path,
	search: Arc<InferenceBridge>,
	goal_control: AgentGoalControl,
	host_resources: Option<Arc<dyn omp_envd::HostResources>>,
	advise_queue: omp_agent::advisor::AdvisorAdviceQueue,
) -> omp_envd::RegistryBridges {
	let active = discovery::active_content_snapshots(root);
	builtin_with_content(root, search, goal_control, host_resources, advise_queue, &active)
}

/// Builds driver-owned bridges from the exact frozen discovery snapshot used
/// by the owning session composition.
pub fn builtin_with_content(
	root: &Path,
	search: Arc<InferenceBridge>,
	goal_control: AgentGoalControl,
	host_resources: Option<Arc<dyn omp_envd::HostResources>>,
	advise_queue: omp_agent::advisor::AdvisorAdviceQueue,
	active: &discovery::ActiveContentSnapshots,
) -> omp_envd::RegistryBridges {
	let home = env::var_os("HOME").map_or_else(|| root.to_path_buf(), PathBuf::from);
	let authored_skills = active
		.skills
		.all()
		.iter()
		.filter(|skill| skill.source.as_str() != omp_envd::managed_skills_domain::PROVIDER_ID)
		.map(|skill| skill.name.clone())
		.collect::<BTreeSet<_>>();
	let managed_skills_root = Some(managed_skills::root(&native::user_config_root(&home)));
	let skill = ResolverBridge {
		inner: SkillResolver::new(Arc::clone(&active.skills)),
		entry: SchemeEntry::new(Scheme::Skill, true, false, "skills")
			.with_capabilities(true, true, true)
			.with_whole_body(true),
	};
	let rule = ResolverBridge {
		inner: RuleResolver::new(Arc::clone(&active.rules)),
		entry: SchemeEntry::new(Scheme::Rule, true, false, "rules")
			.with_capabilities(true, true, true)
			.with_whole_body(true),
	};
	let core_claims = omp_tool::Claims {
		precedence: omp_tool::Precedence::CORE,
		claimant:   sf!("omp/core"),
		replaces:   None,
	};
	let device_claims = omp_tool::Claims {
		precedence: omp_tool::Precedence::ENHANCEMENT,
		claimant:   sf!("omp/core"),
		replaces:   None,
	};
	omp_envd::RegistryBridges {
		command_credentials: Some(Arc::new(CommandCredentialsBridge)),
		dynamic_tools: vec![
			omp_envd::DynamicTool::new(
				crate::vibe::tool(),
				omp_tool::Presentation::Device,
				device_claims,
			),
			omp_envd::DynamicTool::new(crate::hub::tool(), omp_tool::Presentation::Slot, core_claims),
			omp_envd::DynamicTool::new(
				omp_agent::advisor::advise_tool(advise_queue),
				omp_tool::Presentation::Hidden,
				omp_tool::Claims {
					precedence: omp_tool::Precedence::CORE,
					claimant:   sf!("omp/advisor"),
					replaces:   None,
				},
			),
		],
		dynamic_tool_factories: vec![active.process_tools.clone()],
		url_resolvers: vec![Arc::new(skill), Arc::new(rule)],
		goal_control: Some(Arc::new(goal_control)),
		search: Some(search),
		edit_model: None,
		edit_repair: None,
		host_resources,
		telemetry_upload: Some(Arc::new(TelemetryBridge)),
		ask_presenter: None,
		content: omp_envd::ActiveContentInputs { authored_skills, managed_skills_root },
	}
}
