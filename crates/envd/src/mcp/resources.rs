//! MCP resource discovery, reads, and subscriptions.

use std::{collections::BTreeSet, sync::Arc};

use omp_core::Str;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::transport::{McpTransport, TransportError, TransportFailure};

const MAX_PAGES: usize = 1_024;

pub(crate) fn template_match_score(template: &str, uri: &str) -> Option<usize> {
	let mut remainder = uri;
	let mut literal_bytes = 0usize;
	let mut cursor = 0usize;
	while let Some(open_offset) = template[cursor..].find('{') {
		let open = cursor + open_offset;
		let close = template[open + 1..]
			.find('}')
			.map(|offset| open + 1 + offset)?;
		let literal = &template[cursor..open];
		if !remainder.starts_with(literal) {
			return None;
		}
		remainder = &remainder[literal.len()..];
		literal_bytes += literal.len();
		cursor = close + 1;
		let next_open = template[cursor..].find('{').map(|offset| cursor + offset);
		let next_literal_end = next_open.unwrap_or(template.len());
		let next_literal = &template[cursor..next_literal_end];
		if !next_literal.is_empty() {
			let consumed = remainder.find(next_literal)?;
			remainder = &remainder[consumed..];
		}
	}
	let tail = &template[cursor..];
	remainder
		.ends_with(tail)
		.then_some(literal_bytes + tail.len())
}

/// Advertised concrete MCP resource.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceDefinition {
	/// Resource URI.
	pub uri:         Str,
	/// Display name.
	pub name:        Str,
	/// Optional description.
	#[serde(default)]
	pub description: Option<Str>,
	/// Optional media type.
	#[serde(default, rename = "mimeType")]
	pub mime_type:   Option<Str>,
}

/// Advertised RFC 6570 resource template.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceTemplate {
	/// URI template.
	#[serde(rename = "uriTemplate")]
	pub uri_template: Str,
	/// Display name.
	pub name:         Str,
	/// Optional description.
	#[serde(default)]
	pub description:  Option<Str>,
	/// Optional media type.
	#[serde(default, rename = "mimeType")]
	pub mime_type:    Option<Str>,
}

/// Decoded resource body.
#[derive(Clone, Debug)]
pub struct ResourceContent {
	/// Canonical URI returned by the server.
	pub uri:       Str,
	/// Optional media type.
	pub mime_type: Option<Str>,
	/// Decoded bytes (UTF-8 text bytes or decoded base64 blob).
	pub bytes:     Vec<u8>,
	/// Whether the source representation was text.
	pub text:      bool,
}

/// Resource protocol facade over one initialized transport.
pub struct ResourcesClient {
	transport: Arc<dyn McpTransport>,
}

impl ResourcesClient {
	/// Creates a resource facade.
	pub fn new(transport: Arc<dyn McpTransport>) -> Self {
		Self { transport }
	}

	/// Lists all resource pages, rejecting cursor cycles and unreasonable peers.
	pub async fn list(
		&self,
		cancel: CancellationToken,
	) -> Result<Vec<ResourceDefinition>, ResourceError> {
		self.paginate("resources/list", "resources", cancel).await
	}

	/// Lists every template page. Servers which do not implement the optional
	/// method are treated as advertising no templates.
	pub async fn templates(
		&self,
		cancel: CancellationToken,
	) -> Result<Vec<ResourceTemplate>, ResourceError> {
		match self
			.paginate("resources/templates/list", "resourceTemplates", cancel)
			.await
		{
			Err(ResourceError::Transport(error))
				if matches!(error.cause, TransportFailure::JsonRpc { code: -32601 }) =>
			{
				Ok(Vec::new())
			},
			result => result,
		}
	}

	/// Reads and decodes all contents returned for a resource URI.
	pub async fn read(
		&self,
		uri: &str,
		cancel: CancellationToken,
	) -> Result<Vec<ResourceContent>, ResourceError> {
		let result = self
			.transport
			.request("resources/read", json!({ "uri": uri }), cancel)
			.await?;
		let response: ReadResponse =
			serde_json::from_value(result.result).map_err(|_| ResourceError::Malformed)?;
		response.contents.into_iter().map(decode_content).collect()
	}

	/// Subscribes to updates for an exact URI.
	pub async fn subscribe(
		&self,
		uri: &str,
		cancel: CancellationToken,
	) -> Result<(), ResourceError> {
		self
			.transport
			.request("resources/subscribe", json!({ "uri": uri }), cancel)
			.await?;
		Ok(())
	}

	/// Removes an exact URI subscription.
	pub async fn unsubscribe(
		&self,
		uri: &str,
		cancel: CancellationToken,
	) -> Result<(), ResourceError> {
		self
			.transport
			.request("resources/unsubscribe", json!({ "uri": uri }), cancel)
			.await?;
		Ok(())
	}

	/// Decodes a resource-updated notification into its exact URI.
	pub fn decode_update(params: Value) -> Result<Str, ResourceError> {
		#[derive(Deserialize)]
		struct Update {
			uri: Str,
		}
		serde_json::from_value::<Update>(params)
			.map(|update| update.uri)
			.map_err(|_| ResourceError::Malformed)
	}

	async fn paginate<T: serde::de::DeserializeOwned>(
		&self,
		method: &str,
		field: &str,
		cancel: CancellationToken,
	) -> Result<Vec<T>, ResourceError> {
		let mut output = Vec::new();
		let mut cursor: Option<Str> = None;
		let mut seen = BTreeSet::new();
		for _ in 0..MAX_PAGES {
			let params = cursor
				.as_ref()
				.map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
			let response = self
				.transport
				.request(method, params, cancel.child_token())
				.await?;
			let mut object = response
				.result
				.as_object()
				.cloned()
				.ok_or(ResourceError::Malformed)?;
			let values = object.remove(field).ok_or(ResourceError::Malformed)?;
			output.extend(
				serde_json::from_value::<Vec<T>>(values).map_err(|_| ResourceError::Malformed)?,
			);
			cursor = object.remove("nextCursor").and_then(|value| {
				value
					.as_str()
					.filter(|value| !value.is_empty())
					.map(Str::from)
			});
			let Some(next) = cursor.as_ref() else {
				return Ok(output);
			};
			if !seen.insert(next.clone()) {
				return Err(ResourceError::CursorCycle);
			}
		}
		Err(ResourceError::TooManyPages)
	}
}

#[derive(Deserialize)]
struct ReadResponse {
	contents: Vec<WireContent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireContent {
	uri:       Str,
	mime_type: Option<Str>,
	text:      Option<String>,
	blob:      Option<String>,
}

fn decode_content(content: WireContent) -> Result<ResourceContent, ResourceError> {
	match (content.text, content.blob) {
		(Some(text), None) => Ok(ResourceContent {
			uri:       content.uri,
			mime_type: content.mime_type,
			bytes:     text.into_bytes(),
			text:      true,
		}),
		(None, Some(blob)) => Ok(ResourceContent {
			uri:       content.uri,
			mime_type: content.mime_type,
			bytes:     omp_core::base64::decode(blob.as_bytes())
				.into_vec()
				.map_err(|_| ResourceError::InvalidBase64)?,
			text:      false,
		}),
		_ => Err(ResourceError::Malformed),
	}
}

/// Resource operation failure.
#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
	/// Transport failed.
	#[error(transparent)]
	Transport(#[from] TransportError),
	/// Response shape was invalid.
	#[error("MCP resource response is malformed")]
	Malformed,
	/// Blob content was not valid base64.
	#[error("MCP resource blob is not valid base64")]
	InvalidBase64,
	/// Pagination cursor repeated.
	#[error("MCP resource pagination cursor repeated")]
	CursorCycle,
	/// Peer exceeded the bounded page count.
	#[error("MCP resource pagination exceeded its page limit")]
	TooManyPages,
}
