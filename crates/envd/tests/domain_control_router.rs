//! Proves domain CONTROL routing preserves request, callback, and effect
//! ownership.
use std::{
	collections::BTreeSet,
	sync::{Arc, Mutex},
};

use async_trait::async_trait;
use omp_core::{Principal, Str, sf};
use omp_envd::exthost::control::{
	ControlAuthority, ControlAuthorityFactory, ControlConnectionIdentity, ControlEffect,
	ControlProtocolError, ControlRequestContext, EnvdControlAuthorities, ExternalControlAuthorities,
	FixedControlAuthorityFactory, HostControlAuthorityFactory, PersistenceControlAuthorities,
	PolicyControlAuthorities, PresentationControlAuthorities, ProviderControlAuthorities,
	RegistryControlAuthorities,
};
use serde_json::{Value, json};

struct Owner {
	name:  &'static str,
	calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ControlAuthority for Owner {
	fn handles(&self, _operation: &str) -> bool {
		true
	}

	fn authorize(
		&self,
		_context: &ControlRequestContext,
		_operation: &str,
		_arguments: &serde_json::Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		Ok(())
	}

	async fn request(
		&self,
		_context: ControlRequestContext,
		operation: Str,
		_arguments: serde_json::Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		self
			.calls
			.lock()
			.expect("calls")
			.push(format!("{}:{operation}", self.name));
		Ok(Value::String(self.name.to_owned()))
	}

	async fn effect(
		&self,
		_context: ControlRequestContext,
		effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		let kind = match effect {
			ControlEffect::Registry(_) => "registry",
			ControlEffect::Ui(_) => "ui",
			ControlEffect::Instrument(_) => "instrument",
			ControlEffect::Intent(_) => "intent",
			ControlEffect::Log(_) => "log",
		};
		self
			.calls
			.lock()
			.expect("calls")
			.push(format!("{}:{kind}", self.name));
		Ok(())
	}
}

fn owner(name: &'static str, calls: &Arc<Mutex<Vec<String>>>) -> Arc<dyn ControlAuthorityFactory> {
	Arc::new(FixedControlAuthorityFactory::new(Arc::new(Owner { name, calls: Arc::clone(calls) })))
}

fn context(request_id: u64) -> ControlRequestContext {
	ControlRequestContext {
		connection: Arc::new(ControlConnectionIdentity {
			extension:          sf!("router.test"),
			principal:          Principal::new(sf!("test"), sf!("Test")),
			artifact_digest:    sf!("sha256:test"),
			layer:              sf!("project"),
			tier:               sf!("trusted"),
			trust:              sf!("trusted"),
			host_generation:    3,
			session_generation: 5,
			capabilities:       Arc::new(BTreeSet::new()),
		}),
		request_id,
		invocation: None,
	}
}

#[tokio::test]
async fn routes_requests_callbacks_and_effects_to_domain_owners() {
	let calls = Arc::new(Mutex::new(Vec::new()));
	let envd = EnvdControlAuthorities::new(
		RegistryControlAuthorities::new(
			owner("registry", &calls),
			owner("devices", &calls),
			owner("hooks", &calls),
		),
		PersistenceControlAuthorities::new(
			owner("sessions", &calls),
			owner("artifacts", &calls),
			owner("credentials", &calls),
		),
		PolicyControlAuthorities::new(owner("policy", &calls), owner("prompts", &calls)),
		PresentationControlAuthorities::new(
			owner("ui", &calls),
			owner("telemetry", &calls),
			owner("verdicts", &calls),
		),
		ProviderControlAuthorities::new(owner("provider", &calls), owner("services", &calls)),
		owner("auxiliary", &calls),
		owner("effects", &calls),
	);
	let router = HostControlAuthorityFactory::new(
		envd,
		ExternalControlAuthorities::new(owner("agents", &calls), owner("mcp", &calls)),
	)
	.bind(context(0).connection)
	.expect("complete router");

	let requests = [
		("omp.registry.freeze", "registry"),
		("omp.devices.invoke", "devices"),
		("omp.hooks.dispatch", "hooks"),
		("omp.state_dir", "auxiliary"),
		("omp.sessions.get", "sessions"),
		("omp.artifacts.stat", "artifacts"),
		("omp.creds.list", "credentials"),
		("omp.policy.authorize", "policy"),
		("omp.prompts.confirm", "prompts"),
		("omp.ui.form", "ui"),
		("omp.telemetry.query", "telemetry"),
		("omp.jobs.register", "verdicts"),
		("omp.provider.request", "provider"),
		("omp.services.call", "services"),
		("omp.params.pull", "auxiliary"),
		("omp.workers.spawn", "auxiliary"),
		("omp.direct_filesystem.request", "auxiliary"),
		("omp.agents.spawn", "agents"),
		("omp.mcp.invoke", "mcp"),
	];
	for (index, (operation, expected)) in requests.into_iter().enumerate() {
		let result = router
			.request(context(index as u64 + 1), Str::from(operation), serde_json::Map::new())
			.await
			.expect("routed request/callback");
		assert_eq!(result, Value::String(expected.to_owned()));
	}
	assert!(!router.handles("omp.context.view"));
	assert!(!router.handles("omp.journal.append"));
	assert!(!router.handles("omp.state.latest"));
	assert!(!router.handles("omp.regimes.start"));

	for (index, (effect, expected)) in [
		(ControlEffect::Registry(json!({})), "registry:registry"),
		(ControlEffect::Ui(json!({})), "ui:ui"),
		(ControlEffect::Instrument(json!({})), "telemetry:instrument"),
		(
			ControlEffect::Intent(json!({
				"operation": "omp.intents.clear",
				"arguments": {"key": "fixture"}
			})),
			"provider:intent",
		),
		(ControlEffect::Log(json!({})), "effects:log"),
	]
	.into_iter()
	.enumerate()
	{
		router
			.effect(context(100 + index as u64), effect)
			.await
			.expect("routed effect");
		assert!(
			calls
				.lock()
				.expect("calls")
				.iter()
				.any(|call| call == expected)
		);
	}
}
