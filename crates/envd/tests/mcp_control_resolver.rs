//! Proves extension-scoped MCP mounts project, list, invoke, and remove through
//! CONTROL.
use std::{
	collections::BTreeSet,
	env, fs,
	future::Future,
	pin::Pin,
	sync::{Arc, Mutex},
	time::{SystemTime, UNIX_EPOCH},
};

use omp_core::{Principal, Str};
use omp_envd::{
	exthost::control::ControlConnectionIdentity,
	mcp::{
		McpService,
		client::{InitializedServer, McpClient},
		control::{McpControl, McpControlError},
		json_rpc::RequestId,
		manager::{
			ConnectedClient, ControlMountAuth, ManagerControlMountResolver, ManagerError,
			McpConnector, McpManager, MountSpec,
		},
		transport::{
			DispatchState, IncomingMessage, McpTransport, ServerResponseError, TransportError,
			TransportFailure, TransportFuture, TransportResponse,
		},
	},
};
use serde_json::{Value, json};
use tokio::task;
use tokio_util::sync::CancellationToken;

struct Connector {
	transport: Arc<FixtureTransport>,
	seen_auth: Mutex<Vec<ControlMountAuth>>,
}

impl McpConnector for Connector {
	fn connect<'a>(
		&'a self,
		spec: &'a MountSpec,
		roots: Arc<[Str]>,
		_: CancellationToken,
	) -> Pin<Box<dyn Future<Output = Result<ConnectedClient, ManagerError>> + Send + 'a>> {
		self
			.seen_auth
			.lock()
			.expect("auth observation")
			.push(spec.auth.clone());
		let transport: Arc<dyn McpTransport> = self.transport.clone();
		Box::pin(async move {
			Ok(ConnectedClient {
				client:      Arc::new(McpClient::new(transport, roots)),
				initialized: InitializedServer {
					protocol_version: Str::new_static("2025-11-25"),
					name:             Str::new_static("fixture"),
					version:          None,
					title:            None,
					description:      None,
					capabilities:     json!({}),
					instructions:     Some(Str::new_static("server docs")),
				},
			})
		})
	}
}

struct FixtureTransport;

impl McpTransport for FixtureTransport {
	fn request<'a>(
		&'a self,
		method: &'a str,
		params: Value,
		_: CancellationToken,
	) -> TransportFuture<'a, Result<TransportResponse, TransportError>> {
		Box::pin(async move {
			let result = match method {
				"tools/list" => json!({
					"tools": [
						{"name":"create_issue","description":"create","inputSchema":{"type":"object"}},
						{"name":"delete_issue","description":"delete","inputSchema":{"type":"object"}},
						{"name":"skip_me","description":"skip","inputSchema":{"type":"object"}}
					]
				}),
				"tools/call" => {
					assert_eq!(params["name"], "create_issue");
					json!({"content":[{"type":"text","text":"created"}],"isError":false})
				},
				other => panic!("unexpected MCP method {other}"),
			};
			Ok(TransportResponse {
				id: RequestId::Number(1),
				result,
				dispatch: DispatchState::Responded,
			})
		})
	}

	fn notify<'a>(
		&'a self,
		_: &'a str,
		_: Value,
		_: CancellationToken,
	) -> TransportFuture<'a, Result<DispatchState, TransportError>> {
		Box::pin(async { Ok(DispatchState::Responded) })
	}

	fn next_message<'a>(
		&'a self,
		cancellation: CancellationToken,
	) -> TransportFuture<'a, Result<IncomingMessage, TransportError>> {
		Box::pin(async move {
			cancellation.cancelled().await;
			Err(TransportError::pre_dispatch(TransportFailure::Cancelled))
		})
	}

	fn respond<'a>(
		&'a self,
		_: RequestId,
		_: Result<Value, ServerResponseError>,
		_: CancellationToken,
	) -> TransportFuture<'a, Result<DispatchState, TransportError>> {
		Box::pin(async { Ok(DispatchState::Responded) })
	}

	fn close(&self) -> TransportFuture<'_, Result<(), TransportError>> {
		Box::pin(async { Ok(()) })
	}
}

fn identity(
	extension: &'static str,
	capabilities: &[&'static str],
) -> Arc<ControlConnectionIdentity> {
	Arc::new(ControlConnectionIdentity {
		extension:          Str::new_static(extension),
		principal:          Principal::new(Str::new_static("fixture"), Str::new_static("Fixture")),
		artifact_digest:    Str::new_static("sha256:fixture"),
		layer:              Str::new_static("workspace"),
		tier:               Str::new_static("trusted"),
		trust:              Str::new_static("trusted"),
		host_generation:    7,
		session_generation: 11,
		capabilities:       Arc::new(
			capabilities
				.iter()
				.copied()
				.map(Str::new_static)
				.collect::<BTreeSet<_>>(),
		),
	})
}

#[tokio::test]
async fn extension_scoped_mount_projects_lists_invokes_and_removes() {
	let nonce = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("wall clock")
		.as_nanos();
	let directory = env::temp_dir().join(format!("omp-mcp-control-{}-{nonce}", std::process::id()));
	fs::create_dir_all(&directory).expect("temporary environment");
	let service = McpService::open(directory.join("mcp.sqlite3")).expect("MCP service");
	let connector = Arc::new(Connector {
		transport: Arc::new(FixtureTransport),
		seen_auth: Mutex::new(Vec::new()),
	});
	let manager = McpManager::new(
		Arc::clone(&service),
		connector.clone(),
		Arc::from([Str::new_static("file:///workspace")]),
		directory.clone(),
	);
	service.bind_manager(&manager);
	let cancellation = CancellationToken::new();
	let owner_identity = identity("fixture.extension", &["env.net"]);
	let resolver = Arc::new(ManagerControlMountResolver::new(
		Arc::clone(&manager),
		Arc::clone(&owner_identity),
		cancellation.clone(),
	));
	let control = McpControl::new(Arc::clone(&manager), resolver, owner_identity);

	let mounted = control
		.dispatch_with_cancel(
			"omp.mcp.mount",
			json!({"spec": {
				"server": "github",
				"transport": {"type":"http","url":"https://mcp.example.test/","headers":{}},
				"auth": {"kind":"oauth","scopes":["repo","read:org"],"name":null},
				"include": ["*_issue", "skip_*"],
				"exclude": ["delete_*", "skip_*"],
				"rename": {"create_issue":"issue"},
				"docs": {"create_issue":"projected docs"},
				"precedence": 700,
				"tier": "privileged",
				"timeout": "5s",
				"restart": "no"
			}}),
			cancellation.child_token(),
		)
		.await
		.expect("mount");
	assert_eq!(mounted["devices"].as_array().expect("devices").len(), 1);
	assert_eq!(mounted["devices"][0]["name"], "issue");
	assert_eq!(mounted["devices"][0]["documentation"], "projected docs");
	assert_eq!(mounted["devices"][0]["precedence"], 700);
	assert_eq!(mounted["devices"][0]["tier"], "privileged");

	assert_eq!(
		connector
			.seen_auth
			.lock()
			.expect("auth observation")
			.as_slice(),
		[ControlMountAuth::OAuth {
			scopes: Box::new([Str::new_static("repo"), Str::new_static("read:org")]),
		}]
	);

	let intruder_identity = identity("other.extension", &["env.net"]);
	let intruder = McpControl::new(
		Arc::clone(&manager),
		Arc::new(ManagerControlMountResolver::new(
			Arc::clone(&manager),
			Arc::clone(&intruder_identity),
			cancellation.clone(),
		)),
		intruder_identity,
	);
	let intruder_servers = intruder
		.dispatch("omp.mcp.servers", json!({}))
		.await
		.expect("scoped server inventory");
	assert_eq!(intruder_servers["servers"], json!([]));
	assert!(matches!(
		intruder
			.dispatch("omp.mcp.unmount", json!({"server":"github"}))
			.await,
		Err(McpControlError::Manager(ManagerError::OwnershipDenied))
	));
	assert!(matches!(
		intruder
			.dispatch(
				"omp.mcp.mount",
				json!({"spec": {
					"server":"process",
					"transport":{"type":"stdio","command":"server","args":[],"env":{},"cwd":null},
					"auth":{"kind":"none","scopes":[],"name":null},
					"include":["*"],
					"exclude":[],
					"rename":{},
					"docs":{},
					"precedence":0,
					"tier":"write",
					"timeout":"5s",
					"restart":"on-failure"
				}}),
			)
			.await,
		Err(McpControlError::DeclarationRejected)
	));
	drop(intruder);

	let servers = control
		.dispatch("omp.mcp.servers", json!({}))
		.await
		.expect("servers");
	assert_eq!(servers["servers"][0]["name"], "github");
	assert_eq!(servers["servers"][0]["endpoints"], json!(["create_issue"]));
	assert_eq!(servers["servers"][0]["instructions"], "server docs");

	let invoked = control
		.dispatch_with_cancel(
			"omp.mcp.invoke",
			json!({"server":"github","tool":"create_issue","arguments":{"title":"bug"}}),
			cancellation.child_token(),
		)
		.await
		.expect("invoke");
	assert_eq!(invoked["content"][0]["text"], "created");
	assert_eq!(invoked["is_error"], false);

	let removed = control
		.dispatch("omp.mcp.unmount", json!({"server":"github"}))
		.await
		.expect("unmount");
	assert_eq!(removed["removed"], true);
	let servers = control
		.dispatch("omp.mcp.servers", json!({}))
		.await
		.expect("servers after unmount");
	assert_eq!(servers["servers"], json!([]));

	drop(control);
	drop(manager);
	drop(service);
	cancellation.cancel();
	task::yield_now().await;
	fs::remove_dir_all(directory).expect("remove temporary environment");
}
