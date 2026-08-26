//! Proves every external control domain for an extension shares one atomically
//! leased host session.

#[cfg(unix)]
use std::fs;
use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use omp_core::{ArtifactDigest, Principal, Provenance, Str, sf};
use omp_envd::{
	ProjectEnvironment, RegistryBridges,
	exthost::{
		ControlAuthority, ControlAuthorityFactory, ControlCompositionError, ExtensionManifest,
		ExternalDomainControlFactories,
		control::{
			ControlConnectionIdentity, ControlEffect, ControlProtocolError, ControlRequestContext,
		},
	},
	worker::{ExtHostSpec, HostKey},
};
use omp_ext::config::{StaticDeclaration, StaticDeclarations};
use serde_json::Value;

struct TaggedFactory {
	operation: &'static str,
	tag:       &'static str,
}

impl ControlAuthorityFactory for TaggedFactory {
	fn bind(
		&self,
		identity: Arc<ControlConnectionIdentity>,
	) -> Result<Arc<dyn ControlAuthority>, ControlCompositionError> {
		Ok(Arc::new(TaggedAuthority { identity, operation: self.operation, tag: self.tag }))
	}
}

struct TaggedAuthority {
	identity:  Arc<ControlConnectionIdentity>,
	operation: &'static str,
	tag:       &'static str,
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
				"request did not reach its exact session owner",
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
		Ok(Value::String(self.tag.to_owned()))
	}

	async fn effect(
		&self,
		_context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		Err(ControlProtocolError::new("UnsupportedEffect", "test owner accepts requests only"))
	}
}

fn factory(operation: &'static str, tag: &'static str) -> Arc<dyn ControlAuthorityFactory> {
	Arc::new(TaggedFactory { operation, tag })
}

fn factories(generation: &'static str) -> ExternalDomainControlFactories {
	ExternalDomainControlFactories {
		policy:            Some(factory("omp.policy.capabilities", generation)),
		parameters:        Some(factory("omp.params.args", generation)),
		workers:           Some(factory("omp.workers.list", generation)),
		direct_filesystem: Some(factory("omp.direct_filesystem.request", generation)),
		credentials:       Some(factory("omp.creds.list", generation)),
		prompts:           Some(factory("omp.prompts.invalidate", generation)),
		ui:                Some(factory("omp.ui.presentation", generation)),
		telemetry:         Some(factory("omp.telemetry.query", generation)),
		jobs:              Some(factory("omp.jobs.register", generation)),
		provider:          Some(factory("omp.provider.models", generation)),
		regimes:           Some(factory("omp.regimes.active", generation)),
		// Envd replaces this sentinel with its own sole live service broker/router.
		services:          Some(factory("omp.services.connect", "must-be-overridden-by-envd")),
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
	let executable = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../target/debug/omp")
		.canonicalize()
		.expect("built omp test host executable");
	assert!(executable.is_file(), "omp test host executable: {executable:?}");
	extension.host_executable = Some(executable);
	extension
}

#[cfg(unix)]
#[tokio::test]
async fn every_external_domain_uses_one_atomic_session_lease() {
	let scratch = tempfile::tempdir().expect("scratch");
	let root = scratch.path().join("project");
	let state = scratch.path().join("state");
	fs::create_dir_all(&root).expect("project root");
	fs::create_dir_all(&state).expect("state root");
	let environment = ProjectEnvironment::connect_or_start(
		&root,
		&state,
		&state.join("env.sock"),
		&state.join("docs.sock"),
		false,
		None,
		&[extension()],
		&[],
		omp_tool::DEFAULT_INTERRUPT_GRACE,
		RegistryBridges::default(),
	)
	.await
	.expect("embedded environment");

	let first_lease = environment.bind_external_control_authorities(
		factory("omp.agents.list", "session-one"),
		factories("session-one"),
	);
	let connection = identity(environment.session_generation());
	let authority = environment
		.extension_control_authority(Arc::clone(&connection))
		.expect("production control authority");
	let context = ControlRequestContext {
		connection: Arc::clone(&connection),
		request_id: 1,
		invocation: None,
	};
	for operation in ["omp.agents.list", "omp.params.args"] {
		if operation == "omp.agents.list" {
			authority
				.authorize(&context, operation, &serde_json::Map::new())
				.unwrap_or_else(|error| panic!("{operation} was not authorized: {error}"));
		}
		let value = authority
			.request(context.clone(), Str::new(operation), serde_json::Map::new())
			.await
			.unwrap_or_else(|error| panic!("{operation} did not reach its owner: {error}"));
		assert_eq!(value, Value::String("session-one".to_owned()));
	}

	let second_lease = environment.bind_external_control_authorities(
		factory("omp.agents.list", "session-two"),
		factories("session-two"),
	);
	drop(first_lease);
	let stale = authority
		.request(context, sf!("omp.params.args"), serde_json::Map::new())
		.await
		.expect_err("a connection from the superseded session is fenced");
	assert_eq!(stale.code.as_str(), "StaleGeneration");

	let replacement = environment
		.extension_control_authority(Arc::clone(&connection))
		.expect("replacement control connection");
	let replacement_context = ControlRequestContext {
		connection: Arc::clone(&connection),
		request_id: 2,
		invocation: None,
	};
	let value = replacement
		.request(replacement_context, sf!("omp.params.args"), serde_json::Map::new())
		.await
		.expect("replacement reaches new session owner");
	assert_eq!(value, Value::String("session-two".to_owned()));
	let revoked_context = ControlRequestContext {
		connection: Arc::clone(&connection),
		request_id: 3,
		invocation: None,
	};
	replacement
		.authorize(&revoked_context, "omp.agents.list", &serde_json::Map::new())
		.expect("agents request authorized before lease release");
	drop(second_lease);
	let revoked = replacement
		.request(revoked_context, sf!("omp.agents.list"), serde_json::Map::new())
		.await
		.expect_err("dropping the atomic lease revokes agents and domain authorities together");
	assert_eq!(revoked.code.as_str(), "StaleGeneration");
}
