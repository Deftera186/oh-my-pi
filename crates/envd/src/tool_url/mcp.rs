//! Opaque advertised MCP resource URI reads.

use std::sync::Arc;

use omp_core::{CowBytes, Str};
use omp_proto::env::v1::McpResourceRequest;
use omp_tools::read::{
	Fault,
	resolver::{Resolve, ResourceCompletion, fuzzy_score},
	selector::ParsedSelector,
};
use tokio_util::sync::CancellationToken;

use crate::mcp::McpService;

/// Environment-scoped MCP resource resolver.
pub(crate) struct McpUrlResolver {
	service: Arc<McpService>,
}

impl McpUrlResolver {
	pub(super) fn new(service: Arc<McpService>) -> Self {
		Self { service }
	}

	fn parse<'a>(&self, resource: &'a str) -> Result<&'a str, Fault> {
		if resource.is_empty() {
			return Err(Fault::Invalid {
				message: Str::new_static("mcp:// reads require a nonempty advertised resource URI."),
			});
		}
		Ok(resource)
	}
}

impl Resolve for McpUrlResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		_selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let uri = self.parse(resource)?;
		let server = self
			.service
			.resolve_resource_server(uri)
			.ok_or_else(|| Fault::Source {
				message: Str::new(format!("MCP resource '{uri}' is not advertised.")),
			})?;
		let result = self
			.service
			.resource(
				McpResourceRequest {
					server:        Some(server),
					uri:           uri.to_owned(),
					max_bytes:     8 * 1024 * 1024,
					wire_revision: 1,
				},
				CancellationToken::new(),
			)
			.await
			.map_err(|error| Fault::Source { message: Str::new(error.to_string()) })?;
		if result.truncated {
			return Err(Fault::Source {
				message: Str::new_static("MCP resource exceeded the read size limit."),
			});
		}
		Ok(CowBytes::from(result.content))
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		let mut matches = self
			.service
			.resource_uris()
			.into_iter()
			.filter_map(|uri| {
				let value = Str::new(format!("mcp://{uri}"));
				let score = fuzzy_score(query, &value)?;
				Some(ResourceCompletion {
					value,
					description: Str::new_static("advertised MCP resource"),
					score,
				})
			})
			.collect::<Vec<_>>();
		matches.sort_unstable_by(|left, right| {
			right
				.score
				.cmp(&left.score)
				.then_with(|| left.value.cmp(&right.value))
		});
		matches.truncate(max_results);
		Ok(matches)
	}
}
