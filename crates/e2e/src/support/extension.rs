//! Real local Environment plus Python extension-host process harness.

use std::sync::Arc;

use async_trait::async_trait;
use flume::{Receiver, Sender};
use omp_app::chat_ui::presentation_authority::{
	PresentationAuthority, PresentationAuthorityError, PresentationCallback,
	PresentationCallbackDispatcher, PresentationClient, PresentationEffect, PresentationIdentity,
	PresentationRequest, PresentationResponse,
};
use omp_env::EnvClient;
use omp_envd::{
	EnvServer, ExtensionDataBinding, RegistryBridges,
	exthost::{
		UiControlAuthority,
		control::{
			ControlAuthority, ControlAuthorityFactory, ControlConnectionIdentity,
			ControlInvocationAuthority,
		},
	},
	worker::ExtHostConfig,
};
use omp_proto::{SCHEMA_REV, env::v1::ClientHello};
use omp_tool::Registry;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
	Context as _, Result,
	support::{AllowAdmission, DEFAULT_TIMEOUT, Scratch, within},
};

/// Real local Environment plus its owned Python extension-host process tree.
pub struct ExtensionHarness {
	client:        EnvClient,
	server:        Arc<EnvServer>,
	task:          Option<JoinHandle<()>>,
	data_tasks:    Vec<JoinHandle<()>>,
	data_shutdown: CancellationToken,
}

impl ExtensionHarness {
	/// Starts envd in process while extension and worker entrypoints re-enter
	/// the production `omp_e2e_host` child executable.
	pub async fn spawn(scratch: &Scratch, mut config: ExtHostConfig) -> Result<Self> {
		let mut data_bindings = Vec::new();
		for extension in &mut config.extensions {
			let mut binding = ExtensionDataBinding::scoped(
				scratch.state(),
				extension.key.clone(),
				config.session_id.as_str(),
				config.session_generation,
				extension.data_grants.clone(),
			);
			binding
				.prepare_endpoint()
				.context("preparing extension DATA endpoint")?;
			extension.data_socket = Some(binding.path().to_owned());
			data_bindings.push(binding);
		}
		let server = Arc::new(
			EnvServer::open_local(
				scratch.project(),
				scratch.state(),
				Registry::new(),
				config,
				RegistryBridges::default(),
			)
			.await
			.context("opening extension proof environment")?,
		);
		let data_shutdown = CancellationToken::new();
		let data_tasks = data_bindings
			.into_iter()
			.map(|binding| {
				let server = Arc::clone(&server);
				let shutdown = data_shutdown.clone();
				tokio::spawn(async move {
					let _ = server.serve_extension_uds(binding, shutdown).await;
				})
			})
			.collect();
		let (client, transport) = EnvClient::in_process(64);
		client.set_admitter(AllowAdmission);
		let task_server = Arc::clone(&server);
		let task = tokio::spawn(async move { task_server.serve_in_process(transport).await });
		within(
			"extension environment hello",
			DEFAULT_TIMEOUT,
			client.hello(ClientHello {
				client: "omp-e2e-extension-control".to_owned(),
				schema_rev: SCHEMA_REV,
				..ClientHello::default()
			}),
		)
		.await??;
		Ok(Self { client, server, task: Some(task), data_tasks, data_shutdown })
	}

	/// Returns the hello-complete Environment client.
	pub const fn client(&self) -> &EnvClient {
		&self.client
	}

	/// Returns the registry populated from the manifest-verified worker FREEZE.
	pub fn registry(&self) -> Arc<Registry> {
		self.server.registry()
	}

	/// Returns the live extension-backed hook admission gate.
	pub fn admission_gate(&self) -> Arc<omp_agent::HookGate> {
		self.server.admission_gate()
	}

	/// Stops the connection task and drops the server-owned child process tree.
	pub async fn shutdown(mut self) {
		self.data_shutdown.cancel();
		for task in self.data_tasks.drain(..) {
			let _ = task.await;
		}
		if let Some(task) = self.task.take() {
			task.abort();
			let _ = task.await;
		}
	}
}

impl Drop for ExtensionHarness {
	fn drop(&mut self) {
		self.data_shutdown.cancel();
		for task in self.data_tasks.drain(..) {
			task.abort();
		}
		if let Some(task) = self.task.take() {
			task.abort();
		}
	}
}

/// Creates the real app presentation owner behind envd's identity-fenced UI
/// CONTROL adapter and returns its observable effect stream.
pub fn recording_ui_factory() -> (Arc<dyn ControlAuthorityFactory>, Receiver<PresentationEffect>) {
	let (effects, received) = flume::unbounded();
	let factory: Arc<dyn ControlAuthorityFactory> =
		Arc::new(move |identity: Arc<ControlConnectionIdentity>| {
			let presentation_identity = Arc::new(PresentationIdentity {
				principal:          identity.principal.id().into(),
				extension:          identity.extension.clone(),
				artifact_digest:    identity.artifact_digest.clone(),
				host_generation:    identity.host_generation,
				session_generation: identity.session_generation,
				capabilities:       Arc::clone(&identity.capabilities),
			});
			let owner = Arc::new(PresentationAuthority::new(
				presentation_identity,
				Arc::new(RecordingPresentationClient { effects: effects.clone() }),
				Arc::new(UnusedPresentationCallbacks),
			));
			Ok(Arc::new(UiControlAuthority::new(identity, owner)) as Arc<dyn ControlAuthority>)
		});
	(factory, received)
}

struct RecordingPresentationClient {
	effects: Sender<PresentationEffect>,
}

#[async_trait]
impl PresentationClient for RecordingPresentationClient {
	async fn effect(
		&self,
		_identity: Arc<PresentationIdentity>,
		effect: PresentationEffect,
	) -> Result<(), PresentationAuthorityError> {
		self
			.effects
			.send(effect)
			.map_err(|_| PresentationAuthorityError::Unavailable)
	}

	async fn request(
		&self,
		_identity: Arc<PresentationIdentity>,
		_request: PresentationRequest,
	) -> Result<PresentationResponse, PresentationAuthorityError> {
		Err(PresentationAuthorityError::Unavailable)
	}
}

struct UnusedPresentationCallbacks;

#[async_trait]
impl PresentationCallbackDispatcher for UnusedPresentationCallbacks {
	async fn dispatch(
		&self,
		_identity: Arc<PresentationIdentity>,
		_invocation: ControlInvocationAuthority,
		_callback: PresentationCallback,
	) -> Result<serde_json::Value, PresentationAuthorityError> {
		Err(PresentationAuthorityError::Unavailable)
	}
}
