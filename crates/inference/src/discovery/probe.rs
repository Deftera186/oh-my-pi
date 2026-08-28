//! Active model-discovery probing over an injected HTTP boundary.

use std::{collections::BTreeMap, future::Future, mem, pin::Pin, time::Duration};

use bytes::Bytes;
use omp_catalog::{
	DiscoveredModel, ModelLimits, OperationBits, OperationKind, Price, PriceUnit, ProviderId,
	RouteId, WireModelId,
};
use omp_core::{Str, sf};
use tokio_util::sync::CancellationToken;

use super::endpoints::{DiscoveryEndpoint, DiscoveryEndpointKind};

/// One bounded HTTP probe request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeHttpRequest {
	/// HTTP method.
	pub method:   http::Method,
	/// Absolute URL.
	pub url:      Str,
	/// JSON request body for metadata probes.
	pub body:     Bytes,
	/// Endpoint-class deadline.
	pub deadline: Duration,
}

/// Cold injected HTTP future for endpoint discovery.
pub type ProbeHttpFuture =
	Pin<Box<dyn Future<Output = Result<Bytes, ProbeError>> + Send + 'static>>;

/// Injected HTTP transport used by active discovery.
pub trait DiscoveryHttpClient: Send + Sync + 'static {
	/// Executes one bounded request. Implementations must not follow
	/// cross-origin redirects with credentials.
	fn request(&self, request: ProbeHttpRequest, cancellation: CancellationToken)
	-> ProbeHttpFuture;
}

/// Active endpoint probe bound to one provider route.
#[derive(Clone, Debug)]
pub struct DiscoveryProbe {
	/// Commercial/local provider identity.
	pub provider: ProviderId,
	/// Route on which discovered wire model ids are valid.
	pub route:    RouteId,
	/// Typed endpoint.
	pub endpoint: DiscoveryEndpoint,
}

impl DiscoveryProbe {
	/// Re-probes one selected model against its native discovery endpoint.
	///
	/// Local runtimes expose load-sensitive limits only after selection or JIT
	/// load. LM Studio's native row prefers `loaded_context_length` while
	/// loaded and falls back to `max_context_length` after unload.
	pub async fn probe_model(
		&self,
		wire_model: &WireModelId<str>,
		client: &dyn DiscoveryHttpClient,
		cancellation: CancellationToken,
	) -> Result<Option<DiscoveredModel>, ProbeError> {
		Ok(self
			.probe(client, cancellation)
			.await?
			.into_iter()
			.find(|model| *model.wire_model == *wire_model))
	}

	/// Probes the endpoint family and returns normalized, secret-free rows.
	pub async fn probe(
		&self,
		client: &dyn DiscoveryHttpClient,
		cancellation: CancellationToken,
	) -> Result<Vec<DiscoveredModel>, ProbeError> {
		if self.endpoint.kind == DiscoveryEndpointKind::LiteLlm {
			return self.probe_litellm(client, cancellation).await;
		}
		let path = match self.endpoint.kind {
			DiscoveryEndpointKind::Ollama => "/api/tags",
			DiscoveryEndpointKind::LlamaCpp => "/v1/models",
			DiscoveryEndpointKind::LmStudio => "/api/v0/models",
			DiscoveryEndpointKind::OpenAi => "/v1/models",
			DiscoveryEndpointKind::LiteLlm => unreachable!("LiteLLM has a rich probe path"),
		};
		let payload = self
			.request(client, http::Method::GET, path, Bytes::new(), cancellation.clone())
			.await?;
		let mut rows = self.decode_models(&payload, path)?;
		match self.endpoint.kind {
			DiscoveryEndpointKind::Ollama => {
				for row in &mut rows {
					let body = serde_json::to_vec(&serde_json::json!({"name": row.wire_model.as_str()}))
						.map(Bytes::from)
						.map_err(|_| ProbeError::Protocol)?;
					if let Ok(show) = self
						.request(client, http::Method::POST, "/api/show", body, cancellation.clone())
						.await
					{
						apply_ollama_show(row, &show)?;
					}
				}
			},
			DiscoveryEndpointKind::LlamaCpp => {
				if let Ok(props) = self
					.request(client, http::Method::GET, "/props", Bytes::new(), cancellation)
					.await
				{
					apply_llama_props(&mut rows, &props)?;
				}
			},
			DiscoveryEndpointKind::LmStudio | DiscoveryEndpointKind::OpenAi => {},
			DiscoveryEndpointKind::LiteLlm => unreachable!("LiteLLM returned above"),
		}
		Ok(rows)
	}

	async fn probe_litellm(
		&self,
		client: &dyn DiscoveryHttpClient,
		cancellation: CancellationToken,
	) -> Result<Vec<DiscoveredModel>, ProbeError> {
		let mut merged = BTreeMap::<WireModelId, (DiscoveredModel, LiteLlmRouteEvidence)>::new();
		for path in ["/model_group/info", "/v2/model/info", "/model/info", "/v1/model/info"] {
			let Ok(payload) = self
				.request(client, http::Method::GET, path, Bytes::new(), cancellation.clone())
				.await
			else {
				continue;
			};
			let Ok(entries) = decode_json_rows(&payload) else {
				continue;
			};
			let had_prior_models = !merged.is_empty();
			for value in &entries {
				let Some(id) = litellm_public_id(value) else {
					continue;
				};
				let evidence = classify_litellm_route(Some(value), id);
				let next = self.decode_model(value, id, path, route_for_evidence(self, evidence));
				let key = next.wire_model.clone();
				if let Some((existing, held)) = merged.get_mut(&key) {
					merge_discovered_model(existing, next);
					*held = held.merge(evidence);
					existing.route = route_for_evidence(self, *held);
				} else if !had_prior_models {
					merged.insert(key, (next, evidence));
				}
			}
			if !merged.is_empty()
				&& merged.values().all(|(model, evidence)| {
					*evidence != LiteLlmRouteEvidence::Unknown
						&& !litellm_pricing_is_partial(&model.declared_pricing)
				}) {
				break;
			}
		}
		if !merged.is_empty() {
			return Ok(merged.into_values().map(|(model, _)| model).collect());
		}
		let path = "/v1/models";
		let payload = self
			.request(client, http::Method::GET, path, Bytes::new(), cancellation)
			.await?;
		let entries = decode_json_rows(&payload)?;
		Ok(entries
			.iter()
			.filter_map(|value| {
				let id = litellm_public_id(value)?;
				let evidence = classify_litellm_route(None, id);
				Some(self.decode_model(value, id, path, route_for_evidence(self, evidence)))
			})
			.collect())
	}

	async fn request(
		&self,
		client: &dyn DiscoveryHttpClient,
		method: http::Method,
		path: &str,
		body: Bytes,
		cancellation: CancellationToken,
	) -> Result<Bytes, ProbeError> {
		let configured = self.endpoint.base_url.trim_end_matches('/');
		let management =
			self.endpoint.kind == DiscoveryEndpointKind::LiteLlm && !path.starts_with("/v1/");
		let (base, path) = if management {
			(configured.strip_suffix("/v1").unwrap_or(configured), path)
		} else if configured.ends_with("/v1") {
			(configured, path.strip_prefix("/v1").unwrap_or(path))
		} else {
			(configured, path)
		};
		let mut url = String::with_capacity(base.len() + path.len());
		url.push_str(base);
		url.push_str(path);
		let deadline = self.endpoint.deadline();
		let request = ProbeHttpRequest { method, url: Str::new(url), body, deadline };
		let request_cancellation = cancellation.clone();
		tokio::select! {
			() = cancellation.cancelled() => Err(ProbeError::Cancelled),
			result = tokio::time::timeout(
				deadline,
				client.request(request, request_cancellation),
			) => {
				result.map_err(|_| ProbeError::Timeout)?
			},
		}
	}

	fn decode_models(
		&self,
		payload: &[u8],
		source_path: &str,
	) -> Result<Vec<DiscoveredModel>, ProbeError> {
		let rows = decode_json_rows(payload)?;
		let mut discovered = Vec::with_capacity(rows.len());
		for value in rows {
			let id = value
				.get("id")
				.or_else(|| value.get("name"))
				.or_else(|| value.get("model"))
				.and_then(serde_json::Value::as_str)
				.ok_or(ProbeError::Protocol)?;
			if id.trim().is_empty() {
				return Err(ProbeError::Protocol);
			}
			discovered.push(self.decode_model(&value, id, source_path, self.route.clone()));
		}
		Ok(discovered)
	}

	fn decode_model(
		&self,
		value: &serde_json::Value,
		id: &str,
		source_path: &str,
		route: RouteId,
	) -> DiscoveredModel {
		let context = if self.endpoint.kind == DiscoveryEndpointKind::LmStudio
			&& value.get("state").and_then(serde_json::Value::as_str) == Some("loaded")
		{
			positive_u64(value, &["loaded_context_length"])
				.or_else(|| positive_u64(value, &["max_context_length", "context_length"]))
		} else {
			positive_u64(value, &[
				"context_length",
				"contextWindow",
				"max_context_length",
				"max_input_tokens",
			])
			.or_else(|| nested_positive_u64(value, "model_info", &["max_input_tokens"]))
		};
		let output = positive_u64(value, &["max_output_tokens", "maxTokens"])
			.or_else(|| nested_positive_u64(value, "model_info", &["max_output_tokens"]));
		let limits = (context.is_some() || output.is_some()).then_some(ModelLimits {
			context_window:        context,
			maximum_input_tokens:  None,
			maximum_output_tokens: output,
			maximum_batch:         None,
		});
		let mut operations = OperationBits::empty();
		operations.insert_kind(OperationKind::Chat);
		let declared_pricing = if self.endpoint.kind == DiscoveryEndpointKind::LiteLlm {
			litellm_reported_prices(value)
		} else {
			Box::new([])
		};
		DiscoveredModel {
			provider: self.provider.clone(),
			route,
			wire_model: WireModelId::from(id),
			aliases: Box::new([]),
			display_name: value
				.get("display_name")
				.or_else(|| value.get("displayName"))
				.and_then(serde_json::Value::as_str)
				.map(Str::new),
			declared_class: None,
			declared_operations: operations,
			declared_capabilities: None,
			declared_limits: limits,
			declared_pricing,
			extended_context_mode: None,
			availability: None,
			source: sf!("{}:{}{source_path}", self.endpoint.kind, self.endpoint.base_url),
			observed_at_ms: None,
			updated_at_ms: None,
			deprecated: None,
		}
	}
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiteLlmRouteEvidence {
	OpenAi,
	Other,
	Unknown,
}

impl LiteLlmRouteEvidence {
	const fn merge(self, incoming: Self) -> Self {
		match (self, incoming) {
			(Self::Other, _) | (_, Self::Other) => Self::Other,
			(Self::OpenAi, _) | (_, Self::OpenAi) => Self::OpenAi,
			(Self::Unknown, Self::Unknown) => Self::Unknown,
		}
	}
}

fn decode_json_rows(payload: &[u8]) -> Result<Vec<serde_json::Value>, ProbeError> {
	let mut envelope: serde_json::Value =
		serde_json::from_slice(payload).map_err(|_| ProbeError::Protocol)?;
	take_json_rows(&mut envelope).ok_or(ProbeError::Protocol)
}

fn take_json_rows(envelope: &mut serde_json::Value) -> Option<Vec<serde_json::Value>> {
	if let serde_json::Value::Array(rows) = envelope {
		return Some(mem::take(rows));
	}
	for key in ["data", "models", "result", "items"] {
		if let Some(candidate) = envelope.get_mut(key)
			&& let Some(rows) = take_json_rows(candidate)
		{
			return Some(rows);
		}
	}
	None
}

fn litellm_public_id(value: &serde_json::Value) -> Option<&str> {
	value
		.get("model_group")
		.or_else(|| value.get("model_name"))
		.or_else(|| value.get("id"))
		.or_else(|| value.get("name"))
		.or_else(|| value.get("litellm_params")?.get("model"))
		.and_then(serde_json::Value::as_str)
		.map(str::trim)
		.filter(|id| !id.is_empty())
}

fn classify_litellm_route(value: Option<&serde_json::Value>, id: &str) -> LiteLlmRouteEvidence {
	if let Some(value) = value {
		if let Some(providers) = value.get("providers").and_then(serde_json::Value::as_array) {
			let mut saw_provider = false;
			let all_openai = providers
				.iter()
				.filter_map(serde_json::Value::as_str)
				.map(str::trim)
				.filter(|provider| !provider.is_empty())
				.all(|provider| {
					saw_provider = true;
					provider.eq_ignore_ascii_case("openai")
				});
			if saw_provider {
				return if all_openai {
					LiteLlmRouteEvidence::OpenAi
				} else {
					LiteLlmRouteEvidence::Other
				};
			}
		}
		if let Some(params) = value.get("litellm_params") {
			if let Some(provider) = params
				.get("custom_llm_provider")
				.and_then(serde_json::Value::as_str)
				.map(str::trim)
				.filter(|provider| !provider.is_empty())
			{
				return if provider.eq_ignore_ascii_case("openai") {
					LiteLlmRouteEvidence::OpenAi
				} else {
					LiteLlmRouteEvidence::Other
				};
			}
			if let Some(model) = params
				.get("model")
				.and_then(serde_json::Value::as_str)
				.map(str::trim)
				.filter(|model| !model.is_empty())
				&& let Some((provider, _)) = model.split_once('/')
			{
				return if provider.eq_ignore_ascii_case("openai") {
					LiteLlmRouteEvidence::OpenAi
				} else {
					LiteLlmRouteEvidence::Other
				};
			}
		}
		if let Some(base) = value
			.get("model_info")
			.and_then(|info| info.get("base_model"))
			.or_else(|| value.get("base_model"))
			.and_then(serde_json::Value::as_str)
			&& let Some((provider, _)) = base.trim().split_once('/')
		{
			return if provider.eq_ignore_ascii_case("openai") {
				LiteLlmRouteEvidence::OpenAi
			} else {
				LiteLlmRouteEvidence::Other
			};
		}
	}
	let normalized = id.trim().to_ascii_lowercase();
	if normalized.starts_with("openai/") {
		return LiteLlmRouteEvidence::OpenAi;
	}
	if omp_catalog::is_likely_openai_responses_id(&normalized) {
		LiteLlmRouteEvidence::OpenAi
	} else {
		LiteLlmRouteEvidence::Unknown
	}
}

fn route_for_evidence(probe: &DiscoveryProbe, evidence: LiteLlmRouteEvidence) -> RouteId {
	if evidence == LiteLlmRouteEvidence::OpenAi {
		RouteId::new(format!("{}/openai-responses", probe.provider.as_str()))
	} else {
		probe.route.clone()
	}
}

fn merge_discovered_model(existing: &mut DiscoveredModel, incoming: DiscoveredModel) {
	if incoming.display_name.is_some() {
		existing.display_name = incoming.display_name;
	}
	match (&mut existing.declared_limits, incoming.declared_limits) {
		(Some(existing), Some(incoming)) => {
			if incoming.context_window.is_some() {
				existing.context_window = incoming.context_window;
			}
			if incoming.maximum_output_tokens.is_some() {
				existing.maximum_output_tokens = incoming.maximum_output_tokens;
			}
		},
		(None, limits @ Some(_)) => existing.declared_limits = limits,
		_ => {},
	}
	let mut pricing = existing.declared_pricing.to_vec();
	for incoming in incoming.declared_pricing {
		if let Some(existing) = pricing.iter_mut().find(|price| price.unit == incoming.unit) {
			*existing = incoming;
		} else {
			pricing.push(incoming);
		}
	}
	pricing.sort_unstable_by_key(|price| price.unit);
	existing.declared_pricing = pricing.into_boxed_slice();
	existing.source = incoming.source;
}

fn litellm_reported_prices(value: &serde_json::Value) -> Box<[Price]> {
	[
		("input_cost_per_token", PriceUnit::MtokInput),
		("output_cost_per_token", PriceUnit::MtokOutput),
		("cache_read_input_token_cost", PriceUnit::MtokCacheRead),
		("cache_creation_input_token_cost", PriceUnit::MtokCacheWrite),
	]
	.into_iter()
	.filter_map(|(key, unit)| {
		let value = value
			.get(key)
			.filter(|value| !value.is_null())
			.or_else(|| {
				value
					.get("model_info")?
					.get(key)
					.filter(|value| !value.is_null())
			})?;
		let per_token = value
			.as_f64()
			.or_else(|| value.as_str()?.trim().parse::<f64>().ok())?;
		if !per_token.is_finite() || per_token <= 0.0 {
			return None;
		}
		let nanos_usd = (per_token * 1_000_000_000_000_000.0).round();
		(nanos_usd <= u64::MAX as f64).then_some(Price { unit, nanos_usd: nanos_usd as u64 })
	})
	.collect::<Vec<_>>()
	.into_boxed_slice()
}

fn litellm_pricing_is_partial(pricing: &[Price]) -> bool {
	!pricing.is_empty()
		&& [
			PriceUnit::MtokInput,
			PriceUnit::MtokOutput,
			PriceUnit::MtokCacheRead,
			PriceUnit::MtokCacheWrite,
		]
		.into_iter()
		.any(|unit| pricing.iter().all(|price| price.unit != unit))
}

fn nested_positive_u64(value: &serde_json::Value, object: &str, keys: &[&str]) -> Option<u64> {
	value
		.get(object)
		.and_then(|value| positive_u64(value, keys))
}

fn positive_u64(value: &serde_json::Value, keys: &[&str]) -> Option<u64> {
	keys
		.iter()
		.find_map(|key| value.get(*key).and_then(serde_json::Value::as_u64))
		.filter(|value| *value > 0)
}

fn apply_ollama_show(row: &mut DiscoveredModel, payload: &[u8]) -> Result<(), ProbeError> {
	let value: serde_json::Value =
		serde_json::from_slice(payload).map_err(|_| ProbeError::Protocol)?;
	let context = value
		.get("model_info")
		.and_then(serde_json::Value::as_object)
		.and_then(|info| {
			info
				.iter()
				.find(|(key, _)| key.ends_with(".context_length"))
				.and_then(|(_, value)| value.as_u64())
		})
		.or_else(|| positive_u64(&value, &["context_length"]));
	if let Some(context) = context.filter(|value| *value > 0) {
		row.declared_limits
			.get_or_insert(ModelLimits {
				context_window:        None,
				maximum_input_tokens:  None,
				maximum_output_tokens: None,
				maximum_batch:         None,
			})
			.context_window = Some(context);
	}
	Ok(())
}

fn apply_llama_props(rows: &mut [DiscoveredModel], payload: &[u8]) -> Result<(), ProbeError> {
	let value: serde_json::Value =
		serde_json::from_slice(payload).map_err(|_| ProbeError::Protocol)?;
	let context = positive_u64(&value, &["n_ctx", "n_ctx_train", "context_length"]);
	if let Some(context) = context {
		for row in rows {
			row.declared_limits
				.get_or_insert(ModelLimits {
					context_window:        None,
					maximum_input_tokens:  None,
					maximum_output_tokens: None,
					maximum_batch:         None,
				})
				.context_window = Some(context);
		}
	}
	Ok(())
}

/// Typed, redaction-safe probe failure.
#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
pub enum ProbeError {
	/// The endpoint missed its loopback/remote deadline.
	#[error("model discovery probe timed out")]
	Timeout,
	/// The caller cancelled discovery.
	#[error("model discovery probe was cancelled")]
	Cancelled,
	/// The endpoint transport failed.
	#[error("model discovery transport failed")]
	Transport,
	/// The endpoint response was malformed.
	#[error("model discovery response was malformed")]
	Protocol,
}

#[cfg(test)]
mod tests {
	use std::{collections::BTreeMap, sync::Arc};

	use parking_lot::Mutex;

	use super::*;
	use crate::discovery::endpoints::{EndpointOrigin, configured_endpoint};

	#[derive(Clone)]
	struct FixtureClient(Arc<Bytes>);
	impl DiscoveryHttpClient for FixtureClient {
		fn request(&self, _: ProbeHttpRequest, _: CancellationToken) -> ProbeHttpFuture {
			let payload = Arc::clone(&self.0);
			Box::pin(async move { Ok((*payload).clone()) })
		}
	}
	#[derive(Clone)]
	struct ScriptedClient {
		responses: Arc<BTreeMap<Str, Bytes>>,
		requests:  Arc<Mutex<Vec<Str>>>,
	}

	impl DiscoveryHttpClient for ScriptedClient {
		fn request(&self, request: ProbeHttpRequest, _: CancellationToken) -> ProbeHttpFuture {
			self.requests.lock().push(request.url.clone());
			let response = self.responses.get(request.url.as_str()).cloned();
			Box::pin(async move { response.ok_or(ProbeError::Transport) })
		}
	}

	#[tokio::test]
	async fn litellm_preserves_and_merges_route_evidence() {
		assert!(omp_catalog::is_likely_openai_responses_id("gpt-4.1"));
		assert!(omp_catalog::is_likely_openai_responses_id("chatgpt-4o-latest"));
		assert!(!omp_catalog::is_likely_openai_responses_id("text-embedding-3-large"));

		let endpoint = configured_endpoint(DiscoveryEndpointKind::LiteLlm, "http://primary:4000/v1")
			.expect("endpoint");
		let probe = DiscoveryProbe {
			provider: ProviderId::from("litellm"),
			route: RouteId::from("litellm/primary"),
			endpoint,
		};
		let requests = Arc::new(Mutex::new(Vec::new()));
		let client = ScriptedClient {
			responses: Arc::new(BTreeMap::from([
				(
					Str::new_static("http://primary:4000/model_group/info"),
					Bytes::from_static(
						br#"{"data":[
							{"model_group":"team","providers":["openai"]},
							{"model_group":"configured","litellm_params":{"custom_llm_provider":"openai"}},
							{"model_group":"backend","litellm_params":{"model":"openai/gpt-5.6"}},
							{"model_group":"mixed","providers":["openai"]},
							{"model_group":"within","providers":["openai"]},
							{"model_group":"within","providers":["anthropic"]},
							{"model_group":"opaque","supports_vision":false}
						]}"#,
					),
				),
				(
					Str::new_static("http://primary:4000/v2/model/info"),
					Bytes::from_static(
						br#"{"data":[
							{"model_name":"mixed","providers":["openai","azure"]},
							{"model_name":"opaque","litellm_params":{"custom_llm_provider":"openai"}}
						]}"#,
					),
				),
			])),
			requests:  requests.clone(),
		};
		let rows = probe
			.probe(&client, CancellationToken::new())
			.await
			.expect("LiteLLM probe");
		let route = |id: &str| {
			rows
				.iter()
				.find(|row| row.wire_model.as_str() == id)
				.unwrap_or_else(|| panic!("{id} row"))
				.route
				.as_str()
		};
		for id in ["team", "configured", "backend", "opaque"] {
			assert_eq!(route(id), "litellm/openai-responses", "{id}");
		}
		assert_eq!(route("within"), "litellm/primary");
		assert!(
			requests
				.lock()
				.iter()
				.any(|url| url.as_str().ends_with("/v2/model/info")),
			"unknown first-endpoint evidence must keep probing"
		);
	}

	#[tokio::test]
	async fn litellm_preserves_partial_and_late_cache_pricing() {
		let endpoint = configured_endpoint(DiscoveryEndpointKind::LiteLlm, "http://primary:4000/v1")
			.expect("endpoint");
		let probe = DiscoveryProbe {
			provider: ProviderId::from("litellm"),
			route: RouteId::from("litellm/primary"),
			endpoint,
		};
		let client = ScriptedClient {
			responses: Arc::new(BTreeMap::from([
				(
					Str::new_static("http://primary:4000/model_group/info"),
					Bytes::from_static(
						br#"{"data":[{"model_group":"priced","providers":["openai"],"input_cost_per_token":0.0000055,"cache_read_input_token_cost":0.00000055},{"model_group":"partial","providers":["openai"],"cache_read_input_token_cost":0.00000025}]}"#,
					),
				),
				(
					Str::new_static("http://primary:4000/v2/model/info"),
					Bytes::from_static(
						br#"{"data":[{"model_name":"priced","model_info":{"output_cost_per_token":0.000033,"cache_creation_input_token_cost":0.000006875}}]}"#,
					),
				),
			])),
			requests: Arc::new(Mutex::new(Vec::new())),
		};
		let rows = probe
			.probe(&client, CancellationToken::new())
			.await
			.expect("LiteLLM probe");
		let pricing = |id: &str| {
			rows
				.iter()
				.find(|row| row.wire_model.as_str() == id)
				.unwrap_or_else(|| panic!("{id} row"))
				.declared_pricing
				.iter()
				.map(|price| (price.unit, price.nanos_usd))
				.collect::<BTreeMap<_, _>>()
		};
		assert_eq!(
			pricing("priced"),
			BTreeMap::from([
				(PriceUnit::MtokInput, 5_500_000_000),
				(PriceUnit::MtokOutput, 33_000_000_000),
				(PriceUnit::MtokCacheRead, 550_000_000),
				(PriceUnit::MtokCacheWrite, 6_875_000_000),
			])
		);
		assert_eq!(pricing("partial"), BTreeMap::from([(PriceUnit::MtokCacheRead, 250_000_000)]));
	}

	#[tokio::test]
	async fn lm_studio_selected_model_tracks_loaded_context() {
		let endpoint = configured_endpoint(DiscoveryEndpointKind::LmStudio, "http://127.0.0.1:1234")
			.expect("endpoint");
		let probe = DiscoveryProbe {
			provider: ProviderId::from("lm-studio"),
			route: RouteId::from("lm-studio/primary"),
			endpoint,
		};
		let selected = WireModelId::from("big-model");
		let loaded = probe
			.probe_model(
				&selected,
				&FixtureClient(Arc::new(Bytes::from_static(
					br#"{"data":[{"id":"big-model","state":"loaded","max_context_length":262144,"loaded_context_length":81920}]}"#,
				))),
				CancellationToken::new(),
			)
			.await
			.expect("loaded probe")
			.expect("selected model");
		assert_eq!(
			loaded
				.declared_limits
				.as_ref()
				.and_then(|limits| limits.context_window),
			Some(81_920)
		);
		let unloaded = probe
			.probe_model(
				&selected,
				&FixtureClient(Arc::new(Bytes::from_static(
					br#"{"data":[{"id":"big-model","state":"not-loaded","max_context_length":262144,"loaded_context_length":null}]}"#,
				))),
				CancellationToken::new(),
			)
			.await
			.expect("unloaded probe")
			.expect("selected model");
		assert_eq!(
			unloaded
				.declared_limits
				.as_ref()
				.and_then(|limits| limits.context_window),
			Some(262_144)
		);
	}

	#[tokio::test]
	async fn generic_openai_probe_normalizes_models() {
		let endpoint =
			configured_endpoint(DiscoveryEndpointKind::OpenAi, "https://models.example/v1")
				.expect("endpoint");
		assert_eq!(endpoint.origin, EndpointOrigin::Configured);
		let probe = DiscoveryProbe {
			provider: ProviderId::from("custom"),
			route: RouteId::from("custom-route"),
			endpoint,
		};
		let rows = probe
			.probe(
				&FixtureClient(Arc::new(Bytes::from_static(
					br#"{"data":[{"id":"offline","context_length":8192}]}"#,
				))),
				CancellationToken::new(),
			)
			.await
			.expect("probe");
		assert_eq!(rows[0].wire_model.as_str(), "offline");
		assert_eq!(
			rows[0]
				.declared_limits
				.as_ref()
				.and_then(|limits| limits.context_window),
			Some(8192)
		);
	}
}
