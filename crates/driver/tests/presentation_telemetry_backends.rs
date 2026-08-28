//! Proves telemetry queries enforce installation floors and detached jobs
//! remain idempotent.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use omp_agent::{JobBoard, Mailbox};
use omp_core::{InvocationPhase, Str};
use omp_driver::stats_api::{
	job_authority::{
		DurableJobRegistrar, JobAuthority, JobAuthorityError, JobAuthorityIdentity, JobCallContext,
		JobRegistration, PromptProjectionDispatcher, PromptProjectionRequest,
	},
	telemetry_backend::TelemetryIndexQuery,
};
use omp_env::EnvClient;
use omp_envd::exthost::control::ControlInvocationAuthority;
use omp_observability::authority::{DurableTelemetryQuery, TelemetryAuthorityIdentity};
use omp_storage::telemetry_index::TelemetryIndex;
use omp_tool::ArtifactLifetime;
use serde_json::{Value, json};
use tempfile::tempdir;

fn telemetry_identity(installed_at_ms: u64) -> TelemetryAuthorityIdentity {
	TelemetryAuthorityIdentity {
		principal: Str::new_static("principal"),
		artifact_digest: Str::new_static("digest"),
		host_generation: 1,
		session_generation: 1,
		installed_at_ms,
		capabilities: Arc::new(BTreeSet::new()),
	}
}

#[test]
fn durable_query_reads_real_indexed_frames_and_applies_install_floor() {
	let root = tempdir().unwrap();
	let index =
		Arc::new(TelemetryIndex::open(root.path(), &root.path().join("telemetry.sqlite")).unwrap());
	index
		.append(
			"session-a",
			"tool_call",
			10,
			&serde_json::to_vec(&json!({
				"tool": "edit", "rev": { "family": "hl", "n": 2 },
				"status": "ok", "turn": 1
			}))
			.unwrap(),
		)
		.unwrap();
	index
		.append(
			"session-a",
			"tool_call",
			20,
			&serde_json::to_vec(&json!({
				"tool": "edit", "rev": { "family": "hl", "n": 3 },
				"status": "fault", "turn": 2
			}))
			.unwrap(),
		)
		.unwrap();

	let query = TelemetryIndexQuery::new(index, "session-a");
	let result = query
		.query(
			&telemetry_identity(15),
			&json!({
				"match": [{ "kinds": ["tool_call"], "tool": "edit", "name": "call" }],
				"scope": "session", "select": ["rev.n"], "limit": 10
			}),
		)
		.unwrap();
	assert_eq!(result["total"], 1);
	assert_eq!(result["rows"][0]["turn"], 2);
	assert_eq!(result["rows"][0]["events"][0]["rev"]["n"], 3);
	assert_eq!(result["rows"][0]["bindings"]["call"]["tool"], "edit");
	assert_eq!(result["rows"][0]["values"]["rev.n"], 3);
	assert_eq!(result["floored"], true);

	let metrics = query
		.rev_metrics(&telemetry_identity(0), "edit", Some("hl"), None, "session")
		.unwrap();
	assert_eq!(metrics[0]["rev"]["n"], 3);
	assert_eq!(metrics[0]["calls"], 1);
}

struct UnusedProjection;

struct BoardRegistrar(JobBoard);

#[async_trait]
impl DurableJobRegistrar for BoardRegistrar {
	async fn register(&self, job: omp_tool::JobRef) -> Result<omp_tool::JobRef, JobAuthorityError> {
		self
			.0
			.reattach(job.clone())
			.map_err(|error| JobAuthorityError::JobAdmission(Str::new(error.to_string())))?;
		Ok(job)
	}
}

#[async_trait]
impl PromptProjectionDispatcher for UnusedProjection {
	async fn project(
		&self,
		_identity: Arc<JobAuthorityIdentity>,
		_invocation: ControlInvocationAuthority,
		_request: PromptProjectionRequest,
	) -> Result<Value, JobAuthorityError> {
		Ok(Value::Null)
	}
}

fn job_identity() -> Arc<JobAuthorityIdentity> {
	Arc::new(JobAuthorityIdentity {
		principal:          Str::new_static("principal"),
		extension:          Str::new_static("jobs"),
		artifact_digest:    Str::new_static("digest"),
		host_generation:    1,
		session_generation: 1,
		session:            Str::new_static("session-a"),
		capabilities:       Arc::new(BTreeSet::new()),
	})
}

fn registration(description: &'static str) -> JobRegistration {
	JobRegistration {
		id:               Str::new_static("durable-job"),
		owner_name:       Str::new_static("worker"),
		owner_generation: 7,
		description:      Str::new_static(description),
		media_type:       Some(Str::new_static("application/json")),
		lifetime:         ArtifactLifetime::Durable,
	}
}

#[tokio::test]
async fn job_registration_is_idempotent_across_authority_reconstruction() {
	let mailbox = Mailbox::new();
	let (env, _transport) = EnvClient::in_process(0);
	let board = JobBoard::new(env, mailbox.sender());
	let identity = job_identity();
	let context = JobCallContext {
		identity:   identity.as_ref(),
		phase:      InvocationPhase::EffectsAuthorized,
		cancelled:  false,
		invocation: None,
	};
	let first = JobAuthority::new(
		identity.clone(),
		board.clone(),
		Arc::new(BoardRegistrar(board.clone())),
		Arc::new(UnusedProjection),
	);
	let first_job = first
		.register_job(context, registration("report"))
		.await
		.unwrap();

	let reconstructed = JobAuthority::new(
		identity.clone(),
		board.clone(),
		Arc::new(BoardRegistrar(board.clone())),
		Arc::new(UnusedProjection),
	);
	let repeated = reconstructed
		.register_job(context, registration("report"))
		.await
		.unwrap();
	assert_eq!(repeated.id, first_job.id);
	assert_eq!(board.snapshot().len(), 1);
	assert_eq!(
		reconstructed
			.register_job(context, registration("different"))
			.await,
		Err(JobAuthorityError::JobConflict(Str::new_static("durable-job")))
	);
}
