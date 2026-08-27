//! Proves frozen regime declarations resolve only for their live host
//! generation and regime callback drafts retain typed middleware ordering on
//! the wire.
use std::{collections::BTreeSet, sync::Arc};

use omp_core::{ArtifactDigest, Point, Principal, Provenance, sf};
use omp_envd::{
	exthost::{
		control::{ControlConnectionIdentity, ControlDispatch, ControlProtocolError},
		dispatch::{CallbackDispatcher, decode_regime_draft},
		VerifiedUiRoster,
	},
	worker::{ExtensionRegimeResolver, SealedRegistryEvidence},
};
use omp_proto::toolhost::v1::{
	RegimeControl, RegimeControlKind, RegimeDraft, RegimeEffect, RegimeEffectKind,
	RegimeWorkerEnvelope, regime_worker_envelope,
};
use prost::Message;

struct NoCallbacks;

#[async_trait::async_trait]
impl CallbackDispatcher for NoCallbacks {
	async fn dispatch(
		&self,
		_target: Arc<ControlConnectionIdentity>,
		_dispatch: ControlDispatch,
	) -> Result<serde_json::Value, ControlProtocolError> {
		panic!("resolving a sealed regime must not invoke its callback")
	}
}

fn identity(host_generation: u64) -> Arc<ControlConnectionIdentity> {
	Arc::new(ControlConnectionIdentity {
		extension: sf!("fixture.extension"),
		principal: Principal::new(sf!("fixture"), sf!("Fixture")),
		artifact_digest: sf!("sha256:fixture"),
		layer: sf!("project"),
		tier: sf!("trusted"),
		trust: sf!("trusted"),
		host_generation,
		session_generation: 11,
		capabilities: Arc::new(BTreeSet::new()),
	})
}

fn encoded_json(value: &[u8]) -> serde_json::Value {
	serde_json::json!({"$bytes": omp_core::base64::encode(value)})
}

fn evidence(identity: Arc<ControlConnectionIdentity>) -> Arc<SealedRegistryEvidence> {
	let ui = VerifiedUiRoster {
		generation: identity.host_generation,
		extension: identity.extension.clone(),
		..Default::default()
	};
	Arc::new(SealedRegistryEvidence {
		identity,
		session: Some(sf!("session-1")),
		provenance: Provenance::new(
			sf!("publisher"),
			sf!("fixture.extension"),
			sf!("1.0.0"),
			ArtifactDigest::new([7; 32]),
			sf!("project"),
			sf!("trusted"),
			7,
		),
		tools: Arc::from([]),
		hooks: Arc::from([]),
		ui,
		providers: Arc::from([]),
		regimes: Arc::from([serde_json::json!({
			"id": "retry",
			"revision": 3,
			"points": ["settle"],
			"precedence": 4,
			"lifetime": "session",
			"max_steps": 3,
			"committed_step_interval_ms": null,
			"has_on_limit": true,
			"state_family": "fixture.RetryState",
			"state_revision": 2,
			"when": encoded_json(b"null"),
			"owns": ["mode", "custom-slot"],
			"sets": encoded_json(b"{}"),
			"minimum_duration_ms": null,
			"on_failure": "defer",
		})]),
	})
}

#[test]
fn regime_draft_wire_has_one_optional_control_and_ordered_typed_effects() {
	let expected = RegimeDraft {
		activation_id: "activation-1".to_owned(),
		regime_revision: 3,
		event_revision: 1,
		control: Some(RegimeControl { kind: RegimeControlKind::Retry.into(), ..Default::default() }),
		effects: vec![
			RegimeEffect {
				kind: RegimeEffectKind::RewriteContext.into(),
				payload: b"rewrite".to_vec().into(),
				..Default::default()
			},
			RegimeEffect {
				kind: RegimeEffectKind::ReplaceState.into(),
				payload: b"next".to_vec().into(),
				state_revision: Some(2),
				..Default::default()
			},
		],
		..Default::default()
	};
	let bytes = RegimeWorkerEnvelope {
		body: Some(regime_worker_envelope::Body::Draft(expected.clone())),
		..Default::default()
	}
	.encode_to_vec();

	let decoded = decode_regime_draft(&bytes).expect("typed regime draft");
	assert_eq!(decoded, expected);
	assert_eq!(decoded.activation_id, "activation-1");
	assert_eq!(decoded.regime_revision, 3);
	assert_eq!(decoded.event_revision, 1);
	assert_eq!(
		decoded.control.as_ref().map(|control| control.kind),
		Some(RegimeControlKind::Retry as i32)
	);
	assert_eq!(decoded.effects.len(), 2);
	assert_eq!(decoded.effects[0].kind, RegimeEffectKind::RewriteContext as i32);
	assert_eq!(decoded.effects[1].kind, RegimeEffectKind::ReplaceState as i32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_freeze_table_resolves_only_its_live_generation() {
	let live = identity(7);
	let retained = evidence(Arc::clone(&live));
	let resolver = ExtensionRegimeResolver::new(Arc::new(NoCallbacks), move |candidate| {
		(candidate.host_generation == 7).then(|| Arc::clone(&retained))
	});

	let (spec, machine) = resolver
		.resolve(&live, "retry", Some("seed"))
		.expect("live frozen declaration");
	assert_eq!(spec.id, "retry");
	assert_eq!(spec.family_rev, "fixture.RetryState@2");
	assert!(spec.events.contains(Point::Settle));
	assert_eq!(spec.max_steps, Some(3));
	assert!(spec.on_limit);
	assert_eq!(machine.state(), "seed");
	assert_eq!(resolver.owner("retry").as_deref(), Some("fixture.extension"));

	let stale = identity(8);
	let error = resolver
		.resolve(&stale, "retry", None)
		.err()
		.expect("stale generation must be rejected");
	assert!(error.to_string().contains("generation"));
}
