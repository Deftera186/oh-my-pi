//! Verifies atomic binding, replacement, and revocation of composed session
//! authorities.

use std::{collections::BTreeSet, fs, sync::Arc};

use async_trait::async_trait;
use omp_app::chat_cmd::SessionControlFactories;
use omp_core::{ArtifactDigest, Principal, Provenance, Str, sf};
use omp_envd::{
	AttachOptions, ProjectEnvironment, RegistryBridges,
	exthost::{
		ControlAuthority, ControlAuthorityFactory, ControlCompositionError, ExtensionManifest,
		control::{
			ControlConnectionIdentity, ControlEffect, ControlProtocolError, ControlRequestContext,
		},
	},
	worker::{ExtHostSpec, HostKey},
};
use omp_ext::config::{StaticDeclaration, StaticDeclarations};
use omp_settings::manager::{SettingsManager, SettingsPaths};
use serde_json::Value;

struct TaggedFactory {
	operation:  &'static str,
	generation: &'static str,
}

impl ControlAuthorityFactory for TaggedFactory {
	fn bind(
		&self,
		identity: Arc<ControlConnectionIdentity>,
	) -> Result<Arc<dyn ControlAuthority>, ControlCompositionError> {
		Ok(Arc::new(TaggedAuthority {
			identity,
			operation: self.operation,
			generation: self.generation,
		}))
	}
}

struct TaggedAuthority {
	identity:   Arc<ControlConnectionIdentity>,
	operation:  &'static str,
	generation: &'static str,
}

#[async_trait]
impl ControlAuthority for TaggedAuthority {
	fn handles(&self, operation: &str) -> bool {
		operation == self.operation
	}

	fn authorize(
		&self,
		context: &ControlRequestContext,
		operation: &str,
		_arguments: &serde_json::Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		if Arc::ptr_eq(&self.identity, &context.connection) && self.handles(operation) {
			Ok(())
		} else {
			Err(ControlProtocolError::new(
				"StaleGeneration",
				"request did not reach its exact session authority",
			))
		}
	}

	async fn request(
		&self,
		context: ControlRequestContext,
		operation: Str,
		arguments: serde_json::Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		self.authorize(&context, operation.as_str(), &arguments)?;
		Ok(Value::String(self.generation.to_owned()))
	}

	async fn effect(
		&self,
		_context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		Err(ControlProtocolError::new("UnsupportedEffect", "test owner accepts requests only"))
	}
}

fn factory(operation: &'static str, generation: &'static str) -> Arc<dyn ControlAuthorityFactory> {
	Arc::new(TaggedFactory { operation, generation })
}

fn factories(generation: &'static str) -> SessionControlFactories {
	SessionControlFactories {
		policy:            factory("omp.policy.capabilities", generation),
		parameters:        factory("omp.params.args", generation),
		workers:           factory("omp.workers.list", generation),
		direct_filesystem: factory("omp.direct_filesystem.request", generation),
		credentials:       factory("omp.creds.list", generation),
		prompts:           factory("omp.prompts.invalidate", generation),
		sessions:          factory("omp.sessions.create", generation),
		ui:                factory("omp.ui.presentation", generation),
		telemetry:         factory("omp.telemetry.query", generation),
		jobs:              factory("omp.jobs.register", generation),
		provider:          factory("omp.provider.models", generation),
		regimes:           factory("omp.regimes.active", generation),
	}
}

fn identity(session_generation: u64) -> Arc<ControlConnectionIdentity> {
	Arc::new(ControlConnectionIdentity {
		extension: sf!("fixture.extension"),
		principal: Principal::new(sf!("fixture"), sf!("Fixture")),
		artifact_digest: sf!("sha256:fixture"),
		layer: sf!("workspace"),
		tier: sf!("trusted"),
		trust: sf!("trusted"),
		host_generation: 7,
		session_generation,
		capabilities: Arc::new(BTreeSet::new()),
	})
}

fn extension() -> ExtHostSpec {
	let provenance = Provenance::new(
		sf!("publisher-key"),
		sf!("fixture.extension"),
		sf!("1.0.0"),
		ArtifactDigest::new([7; 32]),
		sf!("workspace"),
		sf!("trusted"),
		1,
	);
	let declaration = StaticDeclaration {
		id: sf!("py_eval@.1"),
		kind: sf!("soft"),
		module: sf!("omp_py_eval"),
		trigger: sf!("lazy"),
		key: sf!("py_eval@.1"),
		api: 1,
		failure: sf!("fault"),
		..StaticDeclaration::default()
	};
	let mut extension = ExtHostSpec::new(
		HostKey::new("workspace", "trusted", "fixture.extension"),
		ExtensionManifest::new_with_static(
			provenance,
			sf!("omp_py_eval"),
			[],
			omp_envd::exthost::DeclarationSet::new(
				[omp_envd::exthost::ToolDeclarationKey::new("py_eval", "", 1)],
				[],
			),
			omp_envd::exthost::ServiceManifest::default(),
			StaticDeclarations {
				ordered: vec![declaration].into_boxed_slice(),
				..StaticDeclarations::default()
			},
			[],
			[omp_envd::exthost::ActivationTrigger::FirstReach],
		),
	);
	extension.host_executable = Some(env!("CARGO_BIN_EXE_omp").into());
	extension
}

async fn request(
	authority: &Arc<dyn ControlAuthority>,
	connection: &Arc<ControlConnectionIdentity>,
	request_id: u64,
	operation: &'static str,
) -> Result<Value, ControlProtocolError> {
	let context =
		ControlRequestContext { connection: Arc::clone(connection), request_id, invocation: None };
	authority.authorize(&context, operation, &serde_json::Map::new())?;
	authority
		.request(context, Str::new_static(operation), serde_json::Map::new())
		.await
}

#[cfg(unix)]
#[tokio::test]
async fn session_bundle_binds_replaces_and_revokes_atomically() {
	let scratch = tempfile::tempdir().expect("scratch");
	let root = scratch.path().join("project");
	let state = scratch.path().join("state");
	fs::create_dir_all(&root).expect("project root");
	fs::create_dir_all(&state).expect("state root");
	let environment = ProjectEnvironment::attach(&root, &state, AttachOptions {
		py_eval:            false,
		approval_mode:      None,
		trusted_extensions: vec![extension()],
		contributed_values: Vec::new(),
		settings:           SettingsManager::open(
			SettingsPaths::discover(&state, Some(&root)),
			omp_app::SETTINGS_CATALOG,
		)
		.expect("settings manager")
		.snapshot(),
		bridges:            RegistryBridges::default(),
		spawn_idle_timeout: Some(2),
	})
	.await
	.expect("embedded environment");

	let first =
		factories("session-one").bind(&environment, factory("omp.agents.list", "session-one"));
	assert!(first.is_live());
	let connection = identity(environment.session_generation());
	let stale_authority = environment
		.extension_control_authority(Arc::clone(&connection))
		.expect("first composed authority");
	for operation in ["omp.agents.list", "omp.params.args"] {
		assert_eq!(
			request(&stale_authority, &connection, 1, operation)
				.await
				.unwrap_or_else(|error| panic!("{operation} was not atomically bound: {error}")),
			Value::String("session-one".to_owned()),
		);
	}

	let replacement =
		factories("session-two").bind(&environment, factory("omp.agents.list", "session-two"));
	assert!(replacement.is_live());
	assert!(!first.is_live());
	drop(first);
	let stale = request(&stale_authority, &connection, 2, "omp.params.args")
		.await
		.expect_err("superseded session authority must be stale");
	assert_eq!(stale.code.as_str(), "StaleGeneration");

	let live_authority = environment
		.extension_control_authority(Arc::clone(&connection))
		.expect("replacement composed authority");
	assert_eq!(
		request(&live_authority, &connection, 3, "omp.agents.list")
			.await
			.expect("replacement agents authority"),
		Value::String("session-two".to_owned()),
	);
	drop(replacement);
	let revoked = request(&live_authority, &connection, 4, "omp.params.args")
		.await
		.expect_err("teardown must revoke the entire session bundle");
	assert_eq!(revoked.code.as_str(), "unhandled_operation");
}
