//! Production composition for the journal-first headless agent kernel.

use std::{
	fs,
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use omp_agent::{
	CanonicalPromptSource, DirectorRegistry, DispatchPolicy, ExtensionRegistrar,
	ExternalDispatchEvent, ExternalDispatchRequest, ExternalDispatchStream, ExternalToolExecutor,
	Kernel, RouteFacts,
};
use omp_core::{SecretString, Str, Ulid};
use omp_dom::{Op, PropKey, Txn, Value};
use omp_inference::{
	CallMeta, ChatRequest, ChatStream, Client, ExecutionBudget, ProviderService, RequestId, Target,
	router::Router,
};
use omp_session::{ComponentRegistry, Session};
use omp_tool::{Abort, BlobRef as ToolBlobRef, Part as ToolPart, Registry};
use parking_lot::RwLock;

use super::{HeadlessError, gateway::GatewayInference};
use crate::registry::{
	InferenceSessionOverrides, ProductionInference as ProductionStack,
	production_inference_for_session,
};

/// Session selection and invocation-local production options.
#[derive(Clone, Default)]
pub struct KernelOptions {
	/// Resume the newest journal in the project session directory.
	pub continue_session:   bool,
	/// Open this exact journal path or session id.
	pub session:            Option<PathBuf>,
	/// Override the project-native durable session directory.
	pub sessions_dir:       Option<PathBuf>,
	/// Create the journal in the system temporary directory.
	pub ephemeral:          bool,
	/// Disable every project tool while retaining normal inference discovery.
	pub no_tools:           bool,
	/// Enable the Python evaluation tool in the project environment.
	pub py_eval:            bool,
	/// Detached environment daemon idle timeout.
	pub spawn_idle_timeout: Option<u64>,
	/// Invocation-only provider API key.
	pub api_key:            Option<SecretString>,
	/// Optional provider routing constraint.
	pub provider:           Option<omp_catalog::ProviderId>,
	/// Connected inference gateway used instead of local provider composition.
	pub gateway:            Option<tonic::transport::Channel>,
	/// Process-local live-session routing authority.
	pub sessions:           Option<Arc<crate::sessions::SessionRegistry>>,
	/// Human-readable routing name for this kernel.
	pub session_name:       Option<Str>,
	/// Explicit restricted registry for specialized child compositions.
	pub tool_registry:      Option<Arc<Registry>>,
}

/// Direct production inference client plus the authorities that keep its
/// environment and authentication stack alive.
pub struct ProductionInference {
	client:             Client<ProviderService, Router>,
	/// Call metadata as composed at launch; `ai_model` re-targets a copy.
	meta:               CallMeta,
	/// Model the client is currently targeted at.
	model:              omp_catalog::ModelKey,
	catalog:            Arc<omp_catalog::snapshot::Catalog>,
	_environment:       omp_envd::ProjectEnvironment,
	_stack:             ProductionStack,
	con:                Arc<omp_con::Ctx>,
	_python_components: Vec<omp_envd::exthost::PyComponent>,
}

impl ProductionInference {
	/// Applies the control plane to the next call: `ai_model` re-targets the
	/// client when it names a different catalog model (ADR 0012: the convar
	/// is the live route), and `ai_thinking` sets the reasoning effort.
	fn apply_convars(&mut self, request: &mut ChatRequest) {
		let selector = omp_con::AI_MODEL.get(&self.con);
		if !selector.is_empty()
			&& let Ok(model) = resolve_model_selector(self.catalog.as_ref(), selector.as_str())
			&& model.as_str() != self.model.as_str()
		{
			let key = omp_catalog::ModelKey::from(model.as_str());
			let mut meta = self.meta.clone();
			meta.target = match &self.meta.target {
				Target::Provider { provider, .. } => {
					Target::Provider { provider: provider.clone(), model: key.clone() }
				},
				_ => Target::Model(key.clone()),
			};
			self.client.set_call_meta(meta);
			self.model = key;
		}
		let thinking = omp_con::AI_THINKING.get(&self.con);
		let reasons = self
			.catalog
			.model(&self.model)
			.is_some_and(|spec| spec.thinking.is_some());
		if reasons
			&& let Ok(effort) = thinking.as_str().parse::<omp_catalog::ReasoningEffort>()
			&& matches!(request.reasoning, omp_inference::Setting::Unset)
		{
			request.reasoning = omp_inference::Setting::Prefer(omp_inference::ReasoningRequest {
				visibility:          omp_inference::ReasoningVisibility::Visible,
				effort:              Some(effort),
				max_tokens:          None,
				preserve_signatures: true,
			});
		}
	}
}

#[derive(Clone)]
struct EnvToolExecutor {
	client: omp_env::EnvClient,
}

impl ExternalToolExecutor for EnvToolExecutor {
	fn invoke(&self, request: ExternalDispatchRequest) -> ExternalDispatchStream {
		let client = self.client.clone();
		Box::pin(async_stream::stream! {
			let opened = client.invoke(omp_env::frame::InvokeTool {
				invocation_id: request.call_id.to_string(),
				name: request.identity.name.to_string(),
				rev: request.identity.rev.to_string(),
				deadline_ms: u64::try_from(request.blocking_limit.as_millis()).unwrap_or(u64::MAX),
				..Default::default()
			}).await;
			let mut invocation = match opened {
				Ok(invocation) => invocation,
				Err(source) => {
					tracing::warn!(%source, call_id = %request.call_id, "environment tool open failed");
					yield ExternalDispatchEvent::Aborted(Abort::Interrupted {
						reason: Str::new_static("environment tool open failed"),
					});
					return;
				},
			};
			match invocation.next_event().await {
				Ok(Some(omp_env::InvocationEvent::Accepted(_))) => {},
				Ok(_) => {
					yield ExternalDispatchEvent::Aborted(Abort::InputDropped);
					return;
				},
				Err(source) => {
					tracing::warn!(%source, call_id = %request.call_id, "environment tool acceptance failed");
					yield ExternalDispatchEvent::Aborted(Abort::Interrupted {
						reason: Str::new_static("environment tool acceptance failed"),
					});
					return;
				},
			}
			let token = Bytes::from(Ulid::generate().to_string());
			let authorized_at_ms = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.map_or(0, |duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
			if let Err(source) = invocation
				.commit_args(Bytes::copy_from_slice(request.args.get().as_bytes()), token, authorized_at_ms, None)
				.await
			{
				tracing::warn!(%source, call_id = %request.call_id, "environment tool commit failed");
				yield ExternalDispatchEvent::Aborted(Abort::Interrupted {
					reason: Str::new_static("environment tool commit failed"),
				});
				return;
			}
			loop {
				match invocation.next_event().await {
					Ok(Some(omp_env::InvocationEvent::Accepted(_))) => {},
					Ok(Some(omp_env::InvocationEvent::Admission(query))) => {
						if let Err(source) = invocation.admit(omp_env::frame::Admission {
							invocation_id: query.invocation_id,
							allow: true,
							..Default::default()
						}).await {
							tracing::warn!(%source, call_id = %request.call_id, "environment tool admission failed");
							yield ExternalDispatchEvent::Aborted(Abort::Interrupted {
								reason: Str::new_static("environment tool admission failed"),
							});
							return;
						}
					},
					Ok(Some(omp_env::InvocationEvent::Update(update))) => {
						match raw_json(update.json) {
							Ok(update) => yield ExternalDispatchEvent::Update(update),
							Err(()) => {
								yield ExternalDispatchEvent::Aborted(Abort::MissingOutcome);
								return;
							},
						}
					},
					Ok(Some(omp_env::InvocationEvent::Verdict(verdict))) => {
						let outcome = match raw_json(verdict.json) {
							Ok(outcome) => outcome,
							Err(()) => {
								yield ExternalDispatchEvent::Aborted(Abort::MissingOutcome);
								return;
							},
						};
						let mut parts = verdict.parts.into_iter().filter_map(tool_part).collect::<Vec<_>>();
						if parts.is_empty() {
							parts = structured_parts(outcome.get());
						}
						if verdict.is_error && parts.is_empty() {
							parts.push(ToolPart::Text { text: Str::new(outcome.get()) });
						}
						yield ExternalDispatchEvent::Done {
							outcome,
							parts,
							is_error: verdict.is_error,
						};
						return;
					},
					Ok(None) => {
						yield ExternalDispatchEvent::Aborted(Abort::MissingOutcome);
						return;
					},
					Err(source) => {
						tracing::warn!(%source, call_id = %request.call_id, "environment tool stream failed");
						yield ExternalDispatchEvent::Aborted(Abort::Interrupted {
							reason: Str::new_static("environment tool stream failed"),
						});
						return;
					},
				}
			}
		})
	}
}

fn raw_json(bytes: Bytes) -> Result<Box<serde_json::value::RawValue>, ()> {
	let text = String::from_utf8(bytes.to_vec()).map_err(|_| ())?;
	serde_json::value::RawValue::from_string(text).map_err(|_| ())
}

fn structured_parts(outcome: &str) -> Vec<ToolPart> {
	let Ok(value) = serde_json::from_str::<serde_json::Value>(outcome) else {
		return Vec::new();
	};
	value
		.get("value")
		.and_then(|value| value.get("parts"))
		.and_then(serde_json::Value::as_array)
		.into_iter()
		.flatten()
		.filter_map(|part| match part.get("kind").and_then(serde_json::Value::as_str) {
			Some("text") => part
				.get("text")
				.and_then(serde_json::Value::as_str)
				.map(|text| ToolPart::Text { text: Str::new(text) }),
			Some("json") => part
				.get("json")
				.map(|json| ToolPart::Json { json: Bytes::from(json.to_string()) }),
			_ => None,
		})
		.collect()
}

fn tool_part(part: omp_proto::thread::v1::Part) -> Option<ToolPart> {
	match part.kind? {
		omp_proto::thread::v1::part::Kind::Text(text) => {
			Some(ToolPart::Text { text: Str::new(text) })
		},
		omp_proto::thread::v1::part::Kind::Thinking(thinking) => {
			Some(ToolPart::Text { text: Str::new(thinking.text) })
		},
		omp_proto::thread::v1::part::Kind::Blob(blob) => Some(ToolPart::Blob {
			blob: ToolBlobRef {
				hash:       Str::new(omp_core::hex::encode(&blob.hash).to_string()),
				media_type: Str::new(blob.mime),
				byte_len:   blob.size,
			},
			alt:  None,
		}),
		omp_proto::thread::v1::part::Kind::Fallback(_)
		| omp_proto::thread::v1::part::Kind::ServerTool(_) => None,
	}
}

impl omp_agent::Inference for ProductionInference {
	fn chat(
		&mut self,
		mut request: ChatRequest,
	) -> impl Future<Output = Result<ChatStream, omp_inference::Error>> + Send {
		self.apply_convars(&mut request);
		self.client.execute(request)
	}
}

/// Inference selected by one headless invocation.
pub enum ComposedInference {
	/// Direct production provider stack.
	Production(ProductionInference),
	/// Remote inference gateway plus its local project-tool authority.
	Gateway {
		/// Raw gateway turn adapter.
		inference:          GatewayInference,
		/// Environment owner retained for local tool execution.
		_environment:       omp_envd::ProjectEnvironment,
		/// Live Python Component reducers retained for the controller lifetime.
		_python_components: Vec<omp_envd::exthost::PyComponent>,
	},
}

impl ComposedInference {
	/// Borrows the project environment client retained by this composition.
	#[must_use]
	pub fn environment_client(&self) -> &omp_env::EnvClient {
		match self {
			Self::Production(inference) => inference._environment.client(),
			Self::Gateway { _environment, .. } => _environment.client(),
		}
	}
}

impl omp_agent::Inference for ComposedInference {
	fn chat(
		&mut self,
		request: ChatRequest,
	) -> impl Future<Output = Result<ChatStream, omp_inference::Error>> + Send {
		async move {
			match self {
				Self::Production(inference) => inference.chat(request).await,
				Self::Gateway { inference, .. } => inference.chat(request).await,
			}
		}
	}
}

/// Concrete prompt projection returned by [`compose_kernel`].
pub type PromptSource = CanonicalPromptSource;

/// Composes the production environment, inference route, tools, prompt, and
/// authoritative `.oms` session for a headless command.
pub async fn compose_kernel(
	data_dir: &Path,
	project_root: &Path,
	model_selector: &str,
	ctx: Arc<omp_con::Ctx>,
	options: KernelOptions,
) -> Result<(Kernel<ComposedInference>, Session, PromptSource), HeadlessError> {
	let project_root = fs::canonicalize(project_root)?;
	let state_dir = omp_env::project_state::directory(data_dir, &project_root)?;
	let sessions_dir = options
		.sessions_dir
		.clone()
		.unwrap_or_else(|| state_dir.join("sessions"));
	fs::create_dir_all(&sessions_dir)?;
	let tools_enabled = !options.no_tools;
	let live_sessions = options
		.sessions
		.clone()
		.unwrap_or_else(|| Arc::new(crate::sessions::SessionRegistry::new()));
	let bridges = if tools_enabled {
		omp_envd::RegistryBridges {
			dynamic_tools: vec![
				omp_envd::DynamicTool::new(
					omp_tools::task::tool(crate::subagent::spawn::TaskDeclarationSpawner),
					omp_tool::Presentation::Slot,
					omp_tool::Claims {
						precedence: omp_tool::Precedence::CORE,
						claimant:   Str::new_static("omp/core"),
						replaces:   None,
					},
				),
				omp_envd::DynamicTool::new(
					omp_tools::hub::tool(crate::subagent::hub::HubDeclarationBackend),
					omp_tool::Presentation::Slot,
					omp_tool::Claims {
						precedence: omp_tool::Precedence::CORE,
						claimant:   Str::new_static("omp/core"),
						replaces:   None,
					},
				),
			],
			..omp_envd::RegistryBridges::default()
		}
	} else {
		omp_envd::RegistryBridges::default()
	};

	let environment =
		omp_envd::ProjectEnvironment::attach(&project_root, &state_dir, omp_envd::AttachOptions {
			py_eval: options.py_eval,
			approval_mode: None,
			trusted_extensions: Vec::new(),
			contributed_values: Vec::new(),
			con: Arc::clone(&ctx),
			bridges,
			spawn_idle_timeout: options.spawn_idle_timeout,
		})
		.await?;
	let mut component_registry = ComponentRegistry::standard();
	let mut director_registry = DirectorRegistry::standard();
	let mut extension_registrar = ExtensionRegistrar::new();
	let python_components = environment.register_python_extensions(&mut extension_registrar)?;
	let live_python_components = python_components.clone();
	let _installed = extension_registrar.install(&mut director_registry, &mut component_registry);

	let registry = if let Some(registry) = options.tool_registry.clone() {
		registry
	} else if options.no_tools {
		Arc::new(Registry::new())
	} else {
		environment.registry()
	};

	let catalog = if options.gateway.is_some() {
		Arc::new(omp_catalog::snapshot::Catalog::embedded().clone())
	} else {
		crate::registry::production_catalog(data_dir)?
	};
	let model = resolve_model_selector(catalog.as_ref(), model_selector)?;
	let model_key = omp_catalog::ModelKey::from(model.as_str());
	let model_spec = catalog
		.model(&model_key)
		.ok_or_else(|| HeadlessError::UnknownModel { selector: model.clone() })?;
	let route_facts = RouteFacts {
		forced_choice_free: catalog
			.wire_policy(&model_spec.wire_policy)
			.and_then(|policy| policy.tool.forced_choice)
			.unwrap_or(false),
		context_window:     model_spec.limits.context_window.unwrap_or(0),
	};
	let external = Arc::new(EnvToolExecutor { client: environment.client().clone() });

	let inference = if let Some(channel) = options.gateway {
		ComposedInference::Gateway {
			inference:          GatewayInference::new(channel, model.as_str()),
			_environment:       environment,
			_python_components: python_components,
		}
	} else {
		let stack = production_inference_for_session(
			data_dir,
			Arc::clone(&registry),
			Some(&project_root),
			InferenceSessionOverrides {
				provider: options.api_key.as_ref().and(options.provider.clone()),
				api_key: options.api_key,
				con: Some(Arc::clone(&ctx)),
				..InferenceSessionOverrides::default()
			},
		)
		.await?;
		let planner = Router::new(stack.registry.clone(), Duration::from_secs(30));
		let target = match options.provider {
			Some(provider) => Target::Provider { provider, model: model_key },
			None => Target::Model(model_key),
		};
		let meta = CallMeta {
			id: RequestId::from(format!("omp-print-{}", Ulid::generate())),
			target,
			deadline: None,
			budget: ExecutionBudget::default(),
			session: None,
			response_hooks: Default::default(),
		};
		let client = Client::new(stack.registry.service(), planner, meta.clone());
		ComposedInference::Production(ProductionInference {
			client,
			meta,
			model: omp_catalog::ModelKey::from(model.as_str()),
			catalog: Arc::clone(&catalog),
			_environment: environment,
			_stack: stack,
			con: Arc::clone(&ctx),
			_python_components: python_components,
		})
	};

	let journal_path = select_journal_path(
		&sessions_dir,
		options.session.as_deref(),
		options.continue_session,
		options.ephemeral,
	)?;
	let mut session = if journal_path.exists() {
		Session::open(&journal_path, component_registry)?
	} else {
		if let Some(parent) = journal_path.parent() {
			fs::create_dir_all(parent)?;
		}
		Session::create(&journal_path, component_registry)?
	};
	install_prompt_facts(&mut session, &project_root, model.as_str())?;

	let prompt = CanonicalPromptSource;
	let spill = omp_journal::blob::BlobStore::open(data_dir.join("artifacts"))?;
	let policy = DispatchPolicy::new(spill);
	let mut kernel = Kernel::new(inference, registry, policy, prompt)
		.with_director_registry(director_registry)
		.with_external_executor(external)
		.with_route_facts(route_facts);
	for component in live_python_components {
		kernel.register_live_component(Box::new(component));
	}
	kernel = kernel
		.with_session_authority(Arc::clone(&live_sessions) as Arc<dyn omp_agent::SessionAuthority>);
	let id = journal_path
		.file_stem()
		.and_then(|name| name.to_str())
		.map_or_else(|| Str::new(Ulid::generate().to_string()), Str::new);
	let name = options.session_name.clone().unwrap_or_else(|| id.clone());
	live_sessions.register(name.clone(), crate::sessions::KernelHandle {
		id: crate::sessions::SessionId::new(id),
		name,
		up: kernel.mailbox(),
		snapshot: Arc::new(RwLock::new(session.dom().snapshot())),
	});
	if tools_enabled {
		let cfg_root = project_root.join(".omp");
		let cfg: Arc<dyn omp_con::CfgLoader> =
			Arc::new(move |name: &str| fs::read_to_string(cfg_root.join(name)).ok().map(Str::new));
		kernel = kernel
			.with_session_tool(Arc::new(crate::subagent::spawn::TaskSessionTool::new(
				data_dir.to_path_buf(),
				project_root.clone(),
				sessions_dir,
				Arc::clone(&live_sessions),
				Arc::clone(&ctx),
				cfg,
				model,
			)))
			.with_session_tool(Arc::new(crate::subagent::hub::HubSessionTool::new()));
	}
	Ok((kernel, session, prompt))
}

fn resolve_model_selector(
	catalog: &omp_catalog::snapshot::Catalog,
	selector: &str,
) -> Result<Str, HeadlessError> {
	if let Some(model) = catalog.model(omp_catalog::ModelKey::from_ref(selector)) {
		return Ok(Str::new(model.key.as_str()));
	}
	if let Some(model) = catalog.resolve_alias(selector) {
		return Ok(Str::new(model.key.as_str()));
	}
	Err(HeadlessError::UnknownModel { selector: Str::new(selector) })
}

fn select_journal_path(
	sessions_dir: &Path,
	explicit: Option<&Path>,
	continue_session: bool,
	ephemeral: bool,
) -> Result<PathBuf, HeadlessError> {
	if let Some(path) = explicit {
		if path.components().count() > 1 || path.extension().is_some() {
			return Ok(path.to_path_buf());
		}
		return Ok(sessions_dir.join(path).with_extension("oms"));
	}
	if continue_session {
		let mut journals = fs::read_dir(sessions_dir)?
			.filter_map(Result::ok)
			.map(|entry| entry.path())
			.filter(|path| path.extension().is_some_and(|extension| extension == "oms"))
			.collect::<Vec<_>>();
		journals.sort_by_key(|path| fs::metadata(path).and_then(|meta| meta.modified()).ok());
		return journals.pop().ok_or(HeadlessError::NoSession);
	}
	let name = format!("{}.oms", Ulid::generate());
	Ok(if ephemeral {
		std::env::temp_dir().join(name)
	} else {
		sessions_dir.join(name)
	})
}

fn install_prompt_facts(
	session: &mut Session,
	project_root: &Path,
	model: &str,
) -> Result<(), omp_session::SessionError> {
	let home = std::env::var_os("HOME").map_or_else(|| project_root.to_path_buf(), PathBuf::from);
	let facts = serde_json::json!({
		"cwd": project_root.to_string_lossy(),
		"home": home.to_string_lossy(),
		"model": { "identifier": model, "codex_task_policy": false },
		"context_files": [],
		"date": jiff::Zoned::now().strftime("%Y-%m-%d").to_string(),
	});
	let raw = serde_json::value::to_raw_value(&facts)?;
	session.patch(Txn {
		cause: session
			.head()
			.ok_or(omp_session::SessionError::NoActiveTurn)?,
		label: Some(Str::new_static("prompt.facts")),
		ops:   vec![Op::Set {
			h:     session.dom().meta(),
			prop:  PropKey::Custom(Str::new_static("prompt-facts")),
			value: Value::Json(raw),
		}],
	})?;
	Ok(())
}
