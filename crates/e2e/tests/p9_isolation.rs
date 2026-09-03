//! Executable P9 proof that Environment-placed workers inherit an isolated
//! worktree root rather than the parent repository root.

#![cfg(unix)]

use std::{fs, future, path::PathBuf, process, sync::Arc};

use bytes::Bytes;
use omp_core::{ArtifactDigest, Principal, Provenance, sf};
use omp_e2e::{
	Context as _, Result, error,
	support::{DEFAULT_TIMEOUT, EnvHarness, Scratch, omp_binary, within},
};
use omp_env::{Admitter, EnvClient, InvocationEvent};
use omp_envd::{
	EnvServer, RegistryBridges,
	exthost::{
		ActivationTrigger, DeclarationSet, ExtensionManifest, ServiceManifest, ToolDeclarationKey,
	},
	worker::{ExtHostConfig, ExtHostSpec, HostKey},
};
use omp_proto::{
	SCHEMA_REV,
	env::v1::{Admission, AdmitInvocation, ClientHello, CreateWorktree, InvokeTool},
};
use omp_tool::{CallOutcome, Registry};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use url::Url;

const MODULE: &str = "p9_isolated_worker";
const MARKER: &str = "worker-proof.txt";
const WORKER_EXTENSION: &str = r#"
import os


def prove_root(params):
    path = params["path"]
    with open(path, "w", encoding="utf-8") as marker:
        marker.write("isolated-worker\n")
    with open(path, "r", encoding="utf-8") as marker:
        contents = marker.read()
    return {"parts": [], "details": {"cwd": os.getcwd(), "contents": contents}}


OMP_TOOLS = [
    {
        "name": "prove_isolated_root",
        "description": "writes and reads a path relative to the placed worker root",
        "schema": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": False,
        },
        "rev": "p9.1",
        "strict": True,
        "handler": prove_root,
    },
]
"#;

struct AllowAdmission;

impl Admitter for AllowAdmission {
	type Future<'client> = future::Ready<Admission>;

	fn admit<'client>(&'client self, query: AdmitInvocation) -> Self::Future<'client> {
		future::ready(Admission {
			invocation_id: query.invocation_id,
			allow: true,
			..Admission::default()
		})
	}
}

struct ChildEnvironment {
	client:  EnvClient,
	_task:   JoinHandle<()>,
	_server: Arc<EnvServer>,
	_state:  tempfile::TempDir,
}

impl ChildEnvironment {
	async fn spawn(root: PathBuf) -> Result<Self> {
		let state = tempfile::tempdir().context("create isolated environment state")?;
		let mut config = ExtHostConfig::new(
			omp_binary().context("resolve worker-capable host")?,
			Principal::new(sf!("p9-e2e"), sf!("P9 E2E")),
			sf!("p9-isolated-session"),
			1,
		);
		let key = HostKey::new("workspace", "trusted", MODULE);
		let provenance = Provenance::new(
			sf!("omp-e2e"),
			key.extension().clone(),
			sf!("1.0.0"),
			ArtifactDigest::new([9; 32]),
			key.layer().clone(),
			key.tier().clone(),
			1,
		);
		let manifest = ExtensionManifest::new(
			provenance,
			sf!(MODULE),
			[],
			DeclarationSet::new([ToolDeclarationKey::new("prove_isolated_root", "p9", 1)], []),
			ServiceManifest::default(),
			[],
			[ActivationTrigger::FirstReach],
		);
		let mut extension = ExtHostSpec::new(key, manifest);
		extension.python_site = Some(root.clone());
		config.extensions.push(extension);

		let server = Arc::new(
			EnvServer::open_local(
				&root,
				state.path(),
				Registry::new(),
				config,
				&omp_con::Ctx::new(),
				RegistryBridges::default(),
			)
			.await
			.context("open isolated environment")?,
		);
		let (client, transport) = EnvClient::in_process(64);
		client.set_admitter(AllowAdmission);
		let host = Arc::clone(&server);
		let task = tokio::spawn(async move { host.serve_in_process(transport).await });
		within(
			"isolated environment hello",
			DEFAULT_TIMEOUT,
			client.hello(ClientHello {
				client: "p9-e2e".to_owned(),
				schema_rev: SCHEMA_REV,
				..ClientHello::default()
			}),
		)
		.await??;
		Ok(Self { client, _task: task, _server: server, _state: state })
	}
}

impl Drop for ChildEnvironment {
	fn drop(&mut self) {
		self._task.abort();
	}
}

async fn invoke_worker(client: &EnvClient) -> Result<Value> {
	let mut invocation = within(
		"open isolated worker",
		DEFAULT_TIMEOUT,
		client.invoke(InvokeTool {
			invocation_id: "p9-worker-call".to_owned(),
			name: "prove_isolated_root".to_owned(),
			rev: "p9.1".to_owned(),
			..InvokeTool::default()
		}),
	)
	.await??;
	match within("worker acceptance", DEFAULT_TIMEOUT, invocation.next_event()).await?? {
		Some(InvocationEvent::Accepted(_)) => {},
		other => return Err(error(format!("expected worker acceptance, got {other:?}"))),
	}
	within(
		"commit worker arguments",
		DEFAULT_TIMEOUT,
		invocation.commit_args(
			Bytes::from(serde_json::to_vec(&json!({"path": MARKER}))?),
			Bytes::from_static(b"p9-e2e-token"),
			1000,
			None,
		),
	)
	.await??;
	loop {
		match within("isolated worker verdict", DEFAULT_TIMEOUT, invocation.next_event()).await?? {
			Some(InvocationEvent::Verdict(verdict)) => {
				let outcome: CallOutcome<Value, Value> = serde_json::from_slice(&verdict.json)?;
				return match outcome {
					CallOutcome::Ok(value) => Ok(value),
					other => return Err(error(format!("isolated worker returned {other:?}"))),
				};
			},
			Some(InvocationEvent::Update(_)) => {},
			Some(other) => return Err(error(format!("unexpected worker event {other:?}"))),
			None => return Err(error(format!("isolated worker closed before verdict"))),
		}
	}
}

#[tokio::test]
async fn p9_env_worker_is_rooted_in_the_isolated_worktree() -> Result<()> {
	let parent = Scratch::new().context("create parent project")?;
	parent.write("parent-only.txt", b"parent\n")?;
	let parent_env = EnvHarness::spawn(&parent, Registry::new()).await?;
	let created = within(
		"create isolated worktree",
		DEFAULT_TIMEOUT,
		parent_env.client().create_worktree(CreateWorktree {
			name: "p9-worker-sandbox".to_owned(),
			owner_pid: process::id(),
			..CreateWorktree::default()
		}),
	)
	.await??;
	let worktree = created
		.worktree
		.context("environment omitted worktree identity")?;
	let root = Url::parse(&worktree.root_uri)
		.context("parse worktree root URI")?
		.to_file_path()
		.map_err(|()| error(format!("worktree root was not a file URI")))?;
	fs::write(root.join(format!("{MODULE}.py")), WORKER_EXTENSION)
		.context("install isolated worker extension")?;

	let child = ChildEnvironment::spawn(root.clone()).await?;
	let proof = invoke_worker(&child.client).await?;
	assert_eq!(proof["contents"], "isolated-worker\n");
	assert_eq!(
		fs::canonicalize(proof["cwd"].as_str().context("worker omitted cwd")?)?,
		fs::canonicalize(&root)?,
		"placed worker inherited a root other than its isolated Environment",
	);
	assert_eq!(fs::read(root.join(MARKER))?, b"isolated-worker\n");
	assert!(
		!parent.project().join(MARKER).exists(),
		"worker write escaped into the parent repository",
	);

	drop(child);
	parent_env.shutdown().await?;
	Ok(())
}
