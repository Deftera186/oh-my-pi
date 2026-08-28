//! Persistent advisor-child composition and per-batch execution.

use std::{collections::BTreeMap, fs, io, mem, path::PathBuf, sync::Arc};

use omp_agent::{
	Agent, AgentEvent, AgentKind, AgentState, AgentStatus, AgentTree, Budget, EventSubscription,
	PromptError, PromptSource, SpawnRefusal, TurnClient, TurnId,
};
use omp_catalog::GrammarBits;
use omp_core::{Str, sf};
use omp_proto::thread::v1::{Item, Message, Part, Role, item, part};
use omp_storage::index::{SessionIndex, SessionKind};
use omp_tool::{LoweringCaps, RegistryError};
use parking_lot::Mutex;
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use crate::{
	advisor::transcript::AdvisorUsageTotals,
	chat::{CHAT_CAPS_BASE, ChatError, create_indexed_journal, now_ms, protocol_tool_definition},
	subagent::supervisor::{SessionSupervisor, SupervisedRuntime, SupervisorError},
};

/// Complete immutable configuration for one persistent advisor child.
pub struct AdvisorChildSpec {
	/// Stable advisor slug and child identity.
	pub id:            Str,
	/// Human-facing advisor name.
	pub display_name:  Str,
	/// Exact resolved model selector.
	pub model:         Str,
	/// Restricted model-callable tool names.
	pub tools:         Vec<Str>,
	/// Complete advisor system prompt.
	pub system_prompt: Str,
}

/// Result of one persistent advisor prompt batch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdvisorBatchOutcome {
	/// Authoritative final assistant text for transcript recording.
	pub final_text: Str,
	/// Inference usage observed during only this batch.
	pub usage:      AdvisorUsageTotals,
}

/// Failure while composing or prompting one advisor child.
#[derive(Debug, Error)]
pub enum AdvisorChildError {
	/// The advisor slug was empty or unsafe for an indexed journal filename.
	#[error("advisor child id is not a valid slug: {id}")]
	InvalidId {
		/// Rejected identity.
		id: Str,
	},
	/// This persistent advisor was already composed.
	#[error("advisor child is already spawned: {id}")]
	AlreadySpawned {
		/// Duplicate identity.
		id: Str,
	},
	/// The primary session node was unavailable for advisor parenting.
	#[error("primary session node is unavailable for advisor {id}: {parent}")]
	PrimaryUnavailable {
		/// Advisor identity.
		id:     Str,
		/// Expected primary identity.
		parent: Str,
	},
	/// A configured tool was absent from the advisor environment.
	#[error("advisor {id} requested unavailable tool {tool}")]
	ToolUnavailable {
		/// Advisor identity.
		id:   Str,
		/// Missing tool name.
		tool: Str,
	},
	/// Advisor state-directory creation failed.
	#[error("failed to create advisor {id} state directory at {path}")]
	StateDirectory {
		/// Advisor identity.
		id:     Str,
		/// Directory which could not be created.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: io::Error,
	},
	/// Advisor tool advertisement failed.
	#[error("failed to lower tools for advisor {id}")]
	ToolRegistry {
		/// Advisor identity.
		id:     Str,
		/// Registry failure.
		#[source]
		source: RegistryError,
	},
	/// Advisor tool protocol conversion failed.
	#[error("failed to encode tools for advisor {id}")]
	ToolProtocol {
		/// Advisor identity.
		id:     Str,
		/// Protocol conversion failure.
		#[source]
		source: ChatError,
	},
	/// Indexed advisor journal creation failed.
	#[error("failed to create journal for advisor {id}")]
	Journal {
		/// Advisor identity.
		id:     Str,
		/// Journal failure.
		#[source]
		source: ChatError,
	},
	/// A policy extension denied advisor admission before composition.
	#[error("advisor child spawn was denied by policy")]
	PolicyDenied {
		/// Canonical structured policy denial.
		denial: omp_tool::PolicyDenied,
	},
	/// Advisor tree admission failed.
	#[error("failed to admit advisor {id}")]
	Admission {
		/// Advisor identity.
		id:     Str,
		/// Tree admission failure.
		#[source]
		source: SpawnRefusal,
	},
	/// Advisor broker registration failed.
	#[error("failed to register advisor {id} with the session broker")]
	Broker {
		/// Advisor identity.
		id:     Str,
		/// Registry failure.
		#[source]
		source: omp_agent::RegistryError,
	},
	/// Advisor supervision or a prompted run failed.
	#[error("advisor {id} execution failed")]
	Supervisor {
		/// Advisor identity.
		id:     Str,
		/// Supervisor failure.
		#[source]
		source: SupervisorError,
	},
	/// The caller supplied no primary-delta chunks.
	#[error("advisor {id} batch contained no chunks")]
	EmptyBatch {
		/// Advisor identity.
		id: Str,
	},
}

struct AdvisorChildState {
	events:   EventSubscription,
	run_lock: Arc<AsyncMutex<()>>,
}

/// Session-local advisor runtime indexes retained by
/// [`crate::chat::ChatParentHost`].
#[derive(Default)]
pub(crate) struct AdvisorChildren {
	children: BTreeMap<Str, AdvisorChildState>,
}

/// Borrow-free parent facts captured once for advisor composition.
pub(crate) struct AdvisorSpawnContext<C: TurnClient + Clone + Send + 'static> {
	pub(crate) client:        C,
	pub(crate) env:           omp_env::EnvClient,
	pub(crate) broker:        omp_agent::Broker,
	pub(crate) supervisor:    Arc<SessionSupervisor<C>>,
	pub(crate) state:         AgentState,
	pub(crate) session_id:    Str,
	pub(crate) sessions_dir:  PathBuf,
	pub(crate) root:          PathBuf,
	pub(crate) session_index: Arc<SessionIndex>,
	pub(crate) tree:          Arc<AgentTree>,
	pub(crate) hook_gate:     Arc<omp_agent::HookGate>,
}

#[derive(Clone)]
struct AdvisorPromptSource {
	text: Str,
}

impl PromptSource for AdvisorPromptSource {
	fn render(&self, _: &omp_scribe::Props) -> Result<Vec<Item>, PromptError> {
		Ok(vec![message(Role::System, self.text.as_str(), 0)])
	}
}

/// Composes and registers one persistent advisor child without starting
/// inference.
pub(crate) async fn spawn<C: TurnClient + Clone + Send + 'static>(
	context: AdvisorSpawnContext<C>,
	children: &Mutex<AdvisorChildren>,
	spec: AdvisorChildSpec,
) -> Result<Str, AdvisorChildError> {
	let advisor_slug = spec.id.clone();
	validate_id(&advisor_slug)?;
	let id = sf!("advisor-{}-{}", context.session_id, advisor_slug);
	if children.lock().children.contains_key(&id) || context.supervisor.state(id.as_str()).is_some()
	{
		return Err(AdvisorChildError::AlreadySpawned { id });
	}
	if context.tree.node(context.session_id.as_str()).is_none() {
		return Err(AdvisorChildError::PrimaryUnavailable { id, parent: context.session_id });
	}

	let directory = context.sessions_dir.join("eval-agents");
	fs::create_dir_all(&directory).map_err(|source| AdvisorChildError::StateDirectory {
		id: id.clone(),
		path: directory.clone(),
		source,
	})?;
	let registry = Arc::clone(&context.state.snapshot().registry);
	if registry.live_identity("advise").is_none() {
		return Err(AdvisorChildError::ToolUnavailable { id, tool: sf!("advise") });
	}
	let mut enabled = Vec::with_capacity(spec.tools.len().saturating_add(1));
	for tool in spec.tools {
		if enabled.iter().any(|enabled| enabled == &tool) {
			continue;
		}
		if registry.live_identity(tool.as_str()).is_none() {
			return Err(AdvisorChildError::ToolUnavailable { id: id.clone(), tool });
		}
		enabled.push(tool);
	}
	if !enabled.iter().any(|tool| tool == "advise") {
		enabled.push(sf!("advise"));
	}
	let lowered = registry
		.advertise_selected(
			LoweringCaps {
				strict_schema:  true,
				grammar:        GrammarBits::ALL,
				maximum_tools:  None,
				maximum_strict: None,
			},
			&enabled,
		)
		.map_err(|source| AdvisorChildError::ToolRegistry { id: id.clone(), source })?;
	let mut tools = Vec::with_capacity(lowered.len());
	for tool in lowered {
		tools.push(
			protocol_tool_definition(tool.definition)
				.map_err(|source| AdvisorChildError::ToolProtocol { id: id.clone(), source })?,
		);
	}

	let parent_snapshot = context.state.snapshot();
	let mut snapshot = parent_snapshot.as_ref().clone();
	snapshot.turn.context_id = Some(id.clone());
	snapshot.turn.params.model = spec.model.to_string();
	snapshot.turn.params.tools = tools;
	snapshot.turn.params.response_format = None;
	snapshot.turn.params.task_budget = None;
	snapshot.enabled_tools = enabled.into();
	snapshot.registry = Arc::clone(&registry);
	snapshot.prompt_source = Arc::new(AdvisorPromptSource { text: spec.system_prompt });
	let meta = snapshot.turn.params.meta.get_or_insert_default();
	meta.initiator = format!("advisor:{id}");
	snapshot.props.set(
		omp_agent::prompt_keys::TOOLS,
		snapshot.enabled_tools.iter().cloned().collect::<Vec<_>>(),
	);

	let parent = omp_storage::transcript::SessionId(context.session_id.clone());
	let journal_path = directory.join(format!("{id}.jsonl"));
	let journal = create_indexed_journal(
		&journal_path,
		&context.root,
		&id,
		Arc::clone(&context.session_index),
		SessionKind::Subagent,
		Some(&parent),
	)
	.map_err(|source| AdvisorChildError::Journal { id: id.clone(), source })?;
	let node = context
		.tree
		.register(
			id.clone(),
			spec.display_name.clone(),
			AgentKind::Advisor,
			Some(context.session_id.clone()),
			context.session_id.clone(),
			Budget::default(),
		)
		.map_err(|source| AdvisorChildError::Admission { id: id.clone(), source })?;
	let mut child = Agent::new(
		context.client,
		context.env.clone(),
		AgentState::new(snapshot),
		journal,
		CHAT_CAPS_BASE,
	);
	child.set_hook_gate(context.hook_gate);
	child.enable_advisor_tool_loop_guard();
	let events = child.events().subscribe_lossless();
	context
		.broker
		.register(&node, child.mailbox())
		.map_err(|source| AdvisorChildError::Broker { id: id.clone(), source })?;
	let _ = context.broker.registry().set_history(
		id.as_str(),
		Some(journal_path),
		Some(spec.model),
		Some(sf!("Advisor: {}", spec.display_name)),
		omp_agent::AgentHistory::default(),
	);
	let runtime = SupervisedRuntime::new(child);
	if let Err(source) = context
		.supervisor
		.register(Arc::clone(&node), runtime, None)
	{
		context.broker.unregister(id.as_str());
		node.set_status(AgentStatus::Failed);
		return Err(AdvisorChildError::Supervisor { id, source });
	}
	children
		.lock()
		.children
		.insert(id.clone(), AdvisorChildState { events, run_lock: Arc::new(AsyncMutex::new(())) });
	Ok(id)
}

/// Tears down every persistent advisor owned by the current parent host.
pub(crate) async fn clear<C: TurnClient + Clone + Send + 'static>(
	broker: &omp_agent::Broker,
	supervisor: &SessionSupervisor<C>,
	children: &Mutex<AdvisorChildren>,
) -> Result<(), AdvisorChildError> {
	let ids = mem::take(&mut children.lock().children)
		.into_keys()
		.collect::<Vec<_>>();
	let mut first_error = None;
	for id in ids {
		let result = supervisor.teardown(id.as_str()).await;
		broker.unregister(id.as_str());
		if let Err(source) = result
			&& first_error.is_none()
		{
			first_error = Some(AdvisorChildError::Supervisor { id, source });
		}
	}
	first_error.map_or(Ok(()), Err)
}

/// Runs one serialized batch and attributes only events produced by that run.
pub(crate) async fn run_batch<C: TurnClient + Clone + Send + 'static>(
	broker: &omp_agent::Broker,
	supervisor: &SessionSupervisor<C>,
	children: &Mutex<AdvisorChildren>,
	advisor_id: &str,
	chunks: Vec<Str>,
	turn_id: TurnId,
) -> Result<AdvisorBatchOutcome, AdvisorChildError> {
	if chunks.is_empty() {
		return Err(AdvisorChildError::EmptyBatch { id: Str::new(advisor_id) });
	}
	let run_lock = children
		.lock()
		.children
		.get(advisor_id)
		.map(|child| Arc::clone(&child.run_lock))
		.ok_or_else(|| AdvisorChildError::Supervisor {
			id:     Str::new(advisor_id),
			source: SupervisorError::UnknownAgent { id: Str::new(advisor_id) },
		})?;
	let _run = run_lock.lock().await;
	if let Some(child) = children.lock().children.get(advisor_id) {
		let _ = drain_usage(&child.events);
	}
	let items = chunks
		.into_iter()
		.map(|chunk| message(Role::User, chunk.as_str(), now_ms()))
		.collect();
	let _ = broker.set_idle(advisor_id, false);
	let mut result = supervisor.run(advisor_id, items, turn_id).await;
	loop {
		match broker.finish_turn(advisor_id) {
			Ok(omp_agent::TurnEndDisposition::Terminal) => break,
			Ok(omp_agent::TurnEndDisposition::ContinuationPending) => {
				if result.is_err() {
					let _ = broker.finish_failed_turn(advisor_id);
					break;
				}
				result = supervisor
					.run(
						advisor_id,
						Vec::new(),
						TurnId::new(format!("advisor-irc-wake-{}", omp_core::Ulid::generate())),
					)
					.await;
			},
			Err(error) => {
				tracing::warn!(agent = %advisor_id, %error, "advisor turn settlement failed");
				break;
			},
		}
	}
	if let Some(terminal) = supervisor
		.state(advisor_id)
		.and_then(|state| state.terminal())
	{
		let _ = broker.registry().set_terminal(advisor_id, terminal);
	}
	let usage = children
		.lock()
		.children
		.get(advisor_id)
		.map_or_else(AdvisorUsageTotals::default, |child| drain_usage(&child.events));
	let summary = result
		.map_err(|source| AdvisorChildError::Supervisor { id: Str::new(advisor_id), source })?;
	Ok(AdvisorBatchOutcome {
		final_text: summary
			.final_assistant()
			.map_or_else(Str::default, Str::new),
		usage,
	})
}

fn validate_id(id: &Str) -> Result<(), AdvisorChildError> {
	if id.is_empty()
		|| !id.bytes().all(|byte| {
			byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
		}) {
		return Err(AdvisorChildError::InvalidId { id: id.clone() });
	}
	Ok(())
}

fn message(role: Role, text: &str, created_at_ms: u64) -> Item {
	Item {
		seq: 0,
		created_at_ms,
		kind: Some(item::Kind::Message(Message {
			role:            i32::from(role),
			parts:           vec![Part { kind: Some(part::Kind::Text(text.to_owned())) }],
			synthetic:       None,
			user_initiated:  None,
			completed_at_ms: None,
			usage:           None,
		})),
		props: None,
	}
}

fn drain_usage(events: &EventSubscription) -> AdvisorUsageTotals {
	let mut totals = AdvisorUsageTotals::default();
	let mut cost_nanos = 0_u64;
	while let Ok(event) = events.try_recv() {
		let AgentEvent::Turn { event, .. } = event.as_ref() else {
			continue;
		};
		let Some(omp_proto::inference::v1::turn_event::Event::Outcome(outcome)) =
			event.event.as_ref()
		else {
			continue;
		};
		if let Some(usage) = outcome.usage.as_ref() {
			totals.input_tokens = totals.input_tokens.saturating_add(usage.input_tokens);
			totals.cache_read_tokens = totals
				.cache_read_tokens
				.saturating_add(usage.cache_read_tokens);
			totals.cache_write_tokens = totals
				.cache_write_tokens
				.saturating_add(usage.cache_write_tokens);
			totals.output_tokens = totals.output_tokens.saturating_add(usage.output_tokens);
		}
		cost_nanos =
			cost_nanos.saturating_add(outcome.cost.as_ref().map_or(0, |cost| cost.nanos_usd));
	}
	totals.cost_micro_usd = i128::from(cost_nanos / 1_000);
	totals
}
