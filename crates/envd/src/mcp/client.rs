//! MCP initialization and server-request handling.

use std::sync::Arc;

use omp_core::Str;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::transport::{IncomingMessage, McpTransport, ServerResponseError, TransportError};

/// Preferred MCP revision and the explicit downgrade set accepted by OMP.
pub const PREFERRED_PROTOCOL_VERSION: &str = "2025-11-25";
/// Known protocol revisions implemented by this client, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
	&["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Validated initialize result.
#[derive(Clone, Debug)]
pub struct InitializedServer {
	/// Negotiated exact protocol revision.
	pub protocol_version: Str,
	/// Server implementation name.
	pub name:             Str,
	/// Optional server implementation version.
	pub version:          Option<Str>,
	/// Optional server display title.
	pub title:            Option<Str>,
	/// Optional server description.
	pub description:      Option<Str>,
	/// Advertised capabilities retained for feature gating.
	pub capabilities:     Value,
	/// Bounded device documentation supplied by the server.
	pub instructions:     Option<Str>,
}

/// Environment-scoped MCP protocol client.
pub struct McpClient {
	transport: Arc<dyn McpTransport>,
	roots:     Arc<[Str]>,
}

impl McpClient {
	/// Creates a client with a stable snapshot of Environment workspace roots.
	pub fn new(transport: Arc<dyn McpTransport>, roots: Arc<[Str]>) -> Self {
		Self { transport, roots }
	}

	/// Performs initialize, validates the selected revision, then emits
	/// `notifications/initialized` in protocol order.
	pub async fn initialize(
		&self,
		cancel: CancellationToken,
	) -> Result<InitializedServer, ClientError> {
		let response = self
			.transport
			.request(
				"initialize",
				json!({
					"protocolVersion": PREFERRED_PROTOCOL_VERSION,
					"capabilities": {
						"roots": { "listChanged": false }
					},
					"clientInfo": { "name": "omp", "version": env!("CARGO_PKG_VERSION") }
				}),
				cancel.child_token(),
			)
			.await?;
		let raw: InitializeResult =
			serde_json::from_value(response.result).map_err(|_| ClientError::MalformedInitialize)?;
		if !SUPPORTED_PROTOCOL_VERSIONS.contains(&raw.protocol_version.as_str()) {
			return Err(ClientError::UnsupportedProtocol(Str::from(raw.protocol_version)));
		}
		if raw.server_info.name.trim().is_empty() || !raw.capabilities.is_object() {
			return Err(ClientError::MalformedInitialize);
		}
		let protocol_version = Str::from(raw.protocol_version);
		self
			.transport
			.set_protocol_version(protocol_version.clone());
		self
			.transport
			.notify("notifications/initialized", json!({}), cancel)
			.await?;
		Ok(InitializedServer {
			protocol_version,
			name: Str::from(raw.server_info.name),
			version: raw.server_info.version.map(Str::from),
			title: raw
				.server_info
				.title
				.filter(|value| !value.is_empty())
				.map(Str::from),
			description: raw
				.server_info
				.description
				.filter(|value| !value.is_empty())
				.map(Str::from),
			capabilities: raw.capabilities,
			instructions: raw
				.instructions
				.filter(|value| !value.is_empty())
				.map(Str::from),
		})
	}

	/// Handles one server-initiated message. Notifications are returned to the
	/// supervisor; requests are answered before returning.
	pub async fn next(
		&self,
		cancel: CancellationToken,
	) -> Result<Option<(Str, Value)>, ClientError> {
		match self.transport.next_message(cancel.child_token()).await? {
			IncomingMessage::Notification { method, params } => Ok(Some((method, params))),
			IncomingMessage::Closed => Ok(None),
			IncomingMessage::Request { id, method, params: _ } => {
				let answer = match method.as_str() {
					"ping" => Ok(json!({})),
					"roots/list" => Ok(json!({
						"roots": self.roots.iter().map(|root| json!({
							"uri": root,
							"name": root
						})).collect::<Vec<_>>()
					})),
					_ => Err(ServerResponseError {
						code:    -32601,
						message: Str::new_static("Method not found"),
						data:    None,
					}),
				};
				self.transport.respond(id, answer, cancel).await?;
				Ok(Some((method, Value::Null)))
			},
		}
	}

	/// Borrows the shared transport for resource, prompt, and tool clients.
	pub fn transport(&self) -> &Arc<dyn McpTransport> {
		&self.transport
	}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult {
	protocol_version: String,
	capabilities:     Value,
	server_info:      ServerInfo,
	instructions:     Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerInfo {
	name:        String,
	version:     Option<String>,
	title:       Option<String>,
	description: Option<String>,
}

/// MCP initialization or message-loop failure.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
	/// Transport failed.
	#[error(transparent)]
	Transport(#[from] TransportError),
	/// Initialize response was structurally invalid.
	#[error("MCP initialize response is malformed")]
	MalformedInitialize,
	/// Server selected a revision outside the explicit compatibility set.
	#[error("MCP server selected unsupported protocol revision {0}")]
	UnsupportedProtocol(Str),
}
#[cfg(test)]
mod tests {
	use parking_lot::Mutex;

	use super::*;
	use crate::mcp::{
		json_rpc::RequestId,
		transport::{
			DispatchState, IncomingMessage, McpTransport, ServerResponseError, TransportFuture,
			TransportResponse,
		},
	};

	struct RecordingTransport {
		initialize: Mutex<Option<Value>>,
	}

	impl McpTransport for RecordingTransport {
		fn request<'a>(
			&'a self,
			method: &'a str,
			params: Value,
			_cancellation: CancellationToken,
		) -> TransportFuture<'a, Result<TransportResponse, TransportError>> {
			assert_eq!(method, "initialize");
			*self.initialize.lock() = Some(params);
			Box::pin(async {
				Ok(TransportResponse {
					id:       RequestId::Number(1),
					result:   json!({
						"protocolVersion": PREFERRED_PROTOCOL_VERSION,
						"capabilities": {},
						"serverInfo": { "name": "fixture" }
					}),
					dispatch: DispatchState::Responded,
				})
			})
		}

		fn notify<'a>(
			&'a self,
			_method: &'a str,
			_params: Value,
			_cancellation: CancellationToken,
		) -> TransportFuture<'a, Result<DispatchState, TransportError>> {
			Box::pin(async { Ok(DispatchState::Dispatched) })
		}

		fn next_message<'a>(
			&'a self,
			_cancellation: CancellationToken,
		) -> TransportFuture<'a, Result<IncomingMessage, TransportError>> {
			Box::pin(async { Ok(IncomingMessage::Closed) })
		}

		fn respond<'a>(
			&'a self,
			_id: RequestId,
			_result: Result<Value, ServerResponseError>,
			_cancellation: CancellationToken,
		) -> TransportFuture<'a, Result<DispatchState, TransportError>> {
			Box::pin(async { Ok(DispatchState::Dispatched) })
		}

		fn close(&self) -> TransportFuture<'_, Result<(), TransportError>> {
			Box::pin(async { Ok(()) })
		}
	}

	#[tokio::test]
	async fn initialize_advertises_only_implemented_roots_capability() {
		let transport = Arc::new(RecordingTransport { initialize: Mutex::new(None) });
		let client = McpClient::new(transport.clone(), Arc::from([]));
		client
			.initialize(CancellationToken::new())
			.await
			.expect("initialize");
		let request = transport.initialize.lock();
		let capabilities = &request.as_ref().expect("request")["capabilities"];
		assert!(capabilities.get("roots").is_some());
		assert!(capabilities.get("sampling").is_none());
		assert!(capabilities.get("elicitation").is_none());
	}
}
