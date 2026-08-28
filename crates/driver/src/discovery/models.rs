//! Native `models.toml` decoding for configured catalog overlays.

use std::{
	collections::BTreeMap,
	fs, io,
	path::{Path, PathBuf},
};

use omp_catalog::{
	AccountScope, AuthSpec, AuthSpecKind, Availability, CatalogOverlay, CatalogOverlayBuilder,
	ClassId, ContextStrategy, CredentialSourceSpec, EvidenceConfidence, ModalityBits,
	ModelAvailability, ModelKey, ModelLimits, ModelOverlay, ModelPatch, ModelProvenance, ModelSpec,
	OverlaySource, OverlayStore, PremiumMultiplier, Pricing, ProvenanceKind, ProvenanceSource,
	ProviderDef, ProviderId, RouteDef, RouteId, RouteOverlay, RoutePatch, ThinkingPolicy,
	ThinkingRouting, WireModelId,
};
use omp_core::Str;
use serde::{Deserialize, Serialize};
use toml::{de, ser};
/// Native model configuration. TOML is OMP's native serialization.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModelsConfig {
	/// Provider definitions keyed by stable provider id.
	#[serde(default)]
	pub providers: BTreeMap<Str, ProviderConfig>,
}

/// Provider-level configuration facts.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
	/// Route base URL override.
	pub base_url:             Option<Str>,
	/// Static request headers.
	#[serde(default)]
	pub headers:              BTreeMap<Str, Str>,
	/// Authentication mode.
	pub auth:                 Option<Str>,
	/// Provider model-discovery configuration.
	pub discovery:            Option<toml::Value>,
	/// Wire compatibility configuration.
	pub compat:               Option<toml::Value>,
	/// Whether strict tool schemas are disabled.
	pub disable_strict_tools: Option<bool>,
	/// Per-model replacement facts keyed by model id.
	#[serde(default)]
	pub model_overrides:      BTreeMap<Str, ModelConfig>,
	/// Provider model definitions keyed by model id.
	#[serde(default)]
	pub models:               BTreeMap<Str, ModelConfig>,
}

/// Declarative configured header value source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeaderValueSource {
	/// Safe static public value.
	Public(Str),
	/// Environment-owned secret name.
	Environment(Str),
	/// Environment-executed secret command.
	Command(Str),
}

impl ProviderConfig {
	/// Classifies configured headers without resolving or copying secret
	/// material into the catalog.
	pub fn header_sources(&self) -> Vec<(Str, HeaderValueSource)> {
		self
			.headers
			.iter()
			.map(|(name, value)| {
				let source = if let Some(command) = value.strip_prefix("!") {
					HeaderValueSource::Command(Str::new(command.trim()))
				} else if let Some(environment) = value.strip_prefix("$") {
					HeaderValueSource::Environment(Str::new(environment))
				} else {
					HeaderValueSource::Public(value.clone())
				};
				(name.clone(), source)
			})
			.collect()
	}
}

/// Model-level configuration facts mapped one-for-one onto the catalog model
/// fields. Typed overlay lowering is intentionally done by the catalog owner.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
	/// Explicit normalized model id.
	pub id: Option<Str>,
	/// Picker display name.
	pub name: Option<Str>,
	/// Wire API selector.
	pub api: Option<Str>,
	/// Total context-window limit.
	pub context_window: Option<u64>,
	/// Maximum generated-token limit.
	pub max_tokens: Option<u64>,
	/// Tool-use support flag.
	pub supports_tools: Option<bool>,
	/// Streaming support flag.
	pub supports_streaming: Option<bool>,
	/// Reasoning-policy declaration.
	pub reasoning: Option<toml::Value>,
	/// Accepted input modalities.
	pub input: Option<toml::Value>,
	/// Price schedule.
	pub cost: Option<toml::Value>,
	/// Model-specific compatibility configuration.
	pub compat: Option<toml::Value>,
	/// Remote compaction contract.
	pub remote_compaction: Option<toml::Value>,
	/// Premium quota multiplier.
	pub premium_multiplier: Option<Str>,
	/// Compaction model selector.
	pub compaction_model: Option<Str>,
	/// Preferred edit-tool contract revision.
	pub edit_revision: Option<Str>,
	/// Context-promotion target selector.
	pub context_promotion_target: Option<Str>,
}

/// Decodes a native configured-model file.
pub fn load_models_config(path: &Path) -> Result<ModelsConfig, ModelsConfigError> {
	let source = fs::read_to_string(path)?;
	Ok(toml::from_str(&source)?)
}

/// Source label for a native model configuration or one-time legacy import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelsConfigSource {
	/// Canonical native TOML.
	NativeToml(PathBuf),
	/// Imported legacy JSON.
	LegacyJson(PathBuf),
	/// Imported legacy YAML.
	LegacyYaml(PathBuf),
}

/// Typed model config paired with its explicit provenance label.
#[derive(Clone, Debug)]
pub struct LoadedModelsConfig {
	/// Typed configuration.
	pub config: ModelsConfig,
	/// Decoder/import source.
	pub source: ModelsConfigSource,
}

/// Loads canonical TOML, or performs a one-time legacy JSON/YAML import when
/// no canonical file exists. Legacy formats are never live fallback decoders.
pub fn load_or_import_legacy(
	directory: &Path,
) -> Result<Option<LoadedModelsConfig>, ModelsConfigError> {
	let native = directory.join("models.toml");
	if native.exists() {
		return Ok(Some(LoadedModelsConfig {
			config: load_models_config(&native)?,
			source: ModelsConfigSource::NativeToml(native),
		}));
	}
	let marker = directory.join(".models-migration-v1");
	if marker.exists() {
		return Ok(None);
	}
	let candidates = [("models.json", false), ("models.yml", true), ("models.yaml", true)];
	let Some((path, yaml)) = candidates
		.into_iter()
		.map(|(name, yaml)| (directory.join(name), yaml))
		.find(|(path, _)| path.exists())
	else {
		omp_settings::io::atomic_replace(&marker, "revision = 1\n")?;
		return Ok(None);
	};
	let text = fs::read_to_string(&path)?;
	let config = if yaml {
		serde_yaml::from_str(&text)?
	} else {
		omp_slopjson::from_str(&text)?
	};
	omp_settings::io::atomic_replace(&native, &toml::to_string_pretty(&config)?)?;
	let backup = path.with_file_name(format!(
		"{}.pre-omp-migration.bak",
		path
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("models")
	));
	fs::copy(&path, &backup).map_err(|source| ModelsConfigError::Backup {
		path: path.clone(),
		backup,
		source,
	})?;
	omp_settings::io::atomic_replace(
		&marker,
		if yaml {
			"revision = 1\nsource = \"legacy-yaml\"\n"
		} else {
			"revision = 1\nsource = \"legacy-json\"\n"
		},
	)?;
	Ok(Some(LoadedModelsConfig {
		config,
		source: if yaml {
			ModelsConfigSource::LegacyYaml(path)
		} else {
			ModelsConfigSource::LegacyJson(path)
		},
	}))
}

/// Validates and lowers configured model facts into a secret-free immutable
/// overlay.
///
/// Omitted fields inherit bundled facts. Header and credential values remain
/// declarative in the configuration authority and are never copied into model
/// records.
pub fn lower_user_overlay(config: &ModelsConfig) -> Result<CatalogOverlay, ModelsConfigError> {
	let source = ProvenanceSource {
		kind:           ProvenanceKind::Configured,
		origin:         "models.toml".into(),
		revision:       None,
		confidence:     EvidenceConfidence::Declared,
		observed_at_ms: None,
	};
	let mut builder = CatalogOverlayBuilder::new(source.clone());
	let catalog = omp_catalog::Catalog::embedded();
	for (provider, definition) in &config.providers {
		let configured_auth = definition
			.auth
			.as_deref()
			.map(|auth| configured_auth_spec(provider, auth))
			.transpose()?;
		if let Some(spec) = &configured_auth {
			builder = builder.with_auth_spec(spec.clone());
		}
		let base_provider = catalog.provider(ProviderId::from_ref(provider.as_str()));
		let configured_route = if let Some(base_provider) = base_provider {
			for route_id in &base_provider.routes {
				let Some(route) = catalog.route(route_id) else {
					continue;
				};
				let endpoint = definition
					.base_url
					.as_ref()
					.map(|base_url| omp_catalog::EndpointSpec {
						base_url:    base_url.clone(),
						region:      route.endpoint.region.clone(),
						api_version: route.endpoint.api_version.clone(),
					});
				let discovery = definition.discovery.as_ref().and_then(|value| {
					value
						.get("id")
						.and_then(toml::Value::as_str)
						.map(omp_catalog::DiscoverySpecId::from)
				});
				builder = builder.with_route(RouteOverlay {
					route: route_id.clone(),
					added: None,
					patch: RoutePatch {
						endpoint,
						auth: configured_auth.as_ref().map(|spec| spec.id.clone()),
						discovery: definition.discovery.as_ref().map(|_| discovery),
						disable_strict_tools: definition.disable_strict_tools,
						..RoutePatch::default()
					},
				});
			}
			None
		} else {
			let (template_provider, template_route) =
				configured_provider_template(catalog, definition);
			let route_id = RouteId::from(format!("{provider}-configured"));
			let mut added_provider: ProviderDef = template_provider.clone();
			added_provider.id = ProviderId::from(provider.as_str());
			added_provider.name = provider.clone();
			added_provider.routes = Box::new([route_id.clone()]);
			if let Some(auth) = &configured_auth {
				added_provider.auth = Box::new([auth.id.clone()]);
			}
			builder = builder.with_provider(added_provider);
			let mut added_route: RouteDef = template_route.clone();
			added_route.id = route_id.clone();
			added_route.provider = ProviderId::from(provider.as_str());
			if let Some((codec, transport)) = definition
				.models
				.values()
				.chain(definition.model_overrides.values())
				.find_map(|model| model.api.as_deref())
				.and_then(omp_catalog::resolve_source_transport)
			{
				added_route.codec = codec;
				added_route.transport = transport;
			}
			if let Some(base_url) = &definition.base_url {
				added_route.endpoint.base_url = base_url.clone();
				if let Ok(url) = url::Url::parse(base_url)
					&& let Some(host) = url.host_str()
				{
					let mut origin = format!("{}://{host}", url.scheme());
					if let Some(port) = url.port() {
						origin.push(':');
						origin.push_str(&port.to_string());
					}
					added_route.trust_domain.origin = Str::from(origin);
					added_route.trust_domain.allow_plaintext = url.scheme() == "http";
				}
			}
			if let Some(auth) = &configured_auth {
				added_route.auth = auth.id.clone();
			}
			added_route.discovery = definition
				.discovery
				.as_ref()
				.and_then(|value| value.get("id"))
				.and_then(toml::Value::as_str)
				.map(omp_catalog::DiscoverySpecId::from);
			if let Some(disabled) = definition.disable_strict_tools {
				added_route.capability_limits.disable_strict_tools = disabled;
			}
			builder = builder.with_route(RouteOverlay {
				route: route_id.clone(),
				added: Some(added_route),
				patch: RoutePatch::default(),
			});
			Some(route_id)
		};
		for (name, model) in definition
			.models
			.iter()
			.chain(definition.model_overrides.iter())
		{
			let key = model.id.as_deref().unwrap_or(name.as_str());
			let limits =
				(model.context_window.is_some() || model.max_tokens.is_some()).then_some(ModelLimits {
					context_window:        model.context_window,
					maximum_input_tokens:  None,
					maximum_output_tokens: model.max_tokens,
					maximum_batch:         None,
				});
			let mut capabilities = None;
			if model.supports_tools.is_some()
				|| model.supports_streaming.is_some()
				|| model.input.is_some()
			{
				let inherited = omp_catalog::Catalog::embedded()
					.models()
					.iter()
					.find(|candidate| {
						candidate.key.as_str() == key
							|| candidate
								.key
								.as_str()
								.split_once('/')
								.is_some_and(|(_, id)| id == key)
					})
					.map(|candidate| candidate.capabilities.clone())
					.unwrap_or_else(omp_catalog::unknown_capabilities);
				let mut updated = inherited;
				updated
					.operations
					.insert_kind(omp_catalog::OperationKind::Chat);
				let chat = updated
					.chat
					.get_or_insert_with(omp_catalog::unknown_chat_capabilities);
				if let Some(supports) = model.supports_tools {
					chat.tools = if supports {
						Availability::Native(omp_catalog::ToolCapabilities {
							features:      omp_catalog::ToolFeatureBits::empty(),
							maximum_tools: None,
						})
					} else {
						Availability::Unsupported
					};
				}
				if let Some(input) = &model.input {
					chat.input_modalities =
						Availability::Native(parse_modalities(input, provider, key)?);
				}
				capabilities = Some(updated);
			}
			let thinking = match &model.reasoning {
				None => None,
				Some(toml::Value::Boolean(false)) => Some(None),
				Some(toml::Value::Boolean(true)) => None,
				Some(value) => {
					let policy = value.clone().try_into::<ThinkingPolicy>().map_err(|_| {
						ModelsConfigError::InvalidFact {
							provider: provider.clone(),
							model:    Str::new(key),
							field:    "reasoning",
						}
					})?;
					policy
						.validate()
						.map_err(|_| ModelsConfigError::InvalidFact {
							provider: provider.clone(),
							model:    Str::new(key),
							field:    "reasoning",
						})?;
					Some(Some(policy.content_id()))
				},
			};
			let pricing = model
				.cost
				.as_ref()
				.map(|value| {
					value
						.clone()
						.try_into::<Pricing>()
						.map_err(|_| ModelsConfigError::InvalidFact {
							provider: provider.clone(),
							model:    Str::new(key),
							field:    "cost",
						})
				})
				.transpose()?;
			if let Some(pricing) = &pricing {
				pricing
					.validate()
					.map_err(|_| ModelsConfigError::InvalidFact {
						provider: provider.clone(),
						model:    Str::new(key),
						field:    "cost",
					})?;
			}
			let premium_multiplier_millionths = model
				.premium_multiplier
				.as_deref()
				.map(|value| {
					parse_multiplier(value).ok_or_else(|| ModelsConfigError::InvalidFact {
						provider: provider.clone(),
						model:    Str::new(key),
						field:    "premiumMultiplier",
					})
				})
				.transpose()?
				.map(|value| Some(PremiumMultiplier::from_millionths(value)));
			let existing = catalog.models().iter().find(|candidate| {
				candidate.key.as_str() == key
					&& candidate.routes.iter().any(|route_id| {
						catalog
							.route(route_id)
							.is_some_and(|route| route.provider.as_str() == provider.as_str())
					})
			});
			let added = existing.is_none().then(|| {
				configured_model_record(
					catalog,
					provider,
					key,
					model,
					definition,
					configured_route
						.clone()
						.or_else(|| base_provider.and_then(|base| base.routes.first().cloned()))
						.expect("configured provider template always has a route"),
					&source,
				)
			});
			builder = builder.with_model(ModelOverlay {
				selector: omp_catalog::ExactSelector::new(provider.clone(), ModelKey::from(key)),
				added,
				patch: ModelPatch {
					display_name: model.name.clone(),
					capabilities,
					limits,
					thinking,
					pricing,
					premium_multiplier_millionths,
					compaction_model: model
						.compaction_model
						.clone()
						.map(|value| Some(ModelKey::from(value))),
					edit_revision: model.edit_revision.clone().map(Some),
					context_promotion_target: model
						.context_promotion_target
						.clone()
						.map(|value| Some(ModelKey::from(value))),
					..ModelPatch::default()
				},
			});
		}
	}
	Ok(builder.build())
}

fn configured_provider_template<'a>(
	catalog: &'a omp_catalog::Catalog,
	definition: &ProviderConfig,
) -> (&'a ProviderDef, &'a RouteDef) {
	let api = definition
		.models
		.values()
		.chain(definition.model_overrides.values())
		.find_map(|model| model.api.as_deref())
		.unwrap_or_default();
	let preferred = if api.contains("anthropic") {
		"anthropic"
	} else if api.contains("google") || api.contains("gemini") {
		"google"
	} else {
		"openai"
	};
	let provider = catalog
		.provider(ProviderId::from_ref(preferred))
		.or_else(|| catalog.providers().first())
		.expect("embedded catalog has a provider template");
	let route = provider
		.routes
		.iter()
		.find_map(|route| catalog.route(route))
		.or_else(|| catalog.routes().first())
		.expect("embedded catalog has a route template");
	(provider, route)
}
/// Chat-capable capability record with otherwise unknown evidence.
///
/// Configured `models.toml` entries are chat models by contract; without the
/// declared chat operation the router rejects every turn with
/// `catalog-operation-unsupported`.
fn configured_chat_capabilities() -> omp_catalog::ModelCapabilities {
	let mut capabilities = omp_catalog::unknown_capabilities();
	capabilities
		.operations
		.insert_kind(omp_catalog::OperationKind::Chat);
	capabilities.chat = Some(omp_catalog::unknown_chat_capabilities());
	capabilities
}
/// Synthesizes the interned authentication spec named by a configured
/// provider's `auth` mode.
///
/// `none` fits keyless or header-authenticated endpoints; `api_key`,
/// `bearer`, and `optional_bearer` read `OMP_<PROVIDER>_API_KEY` (falling
/// back to stored credentials); `basic` reads `OMP_<PROVIDER>_USERNAME` and
/// `OMP_<PROVIDER>_PASSWORD`.
///
/// Modes accept snake_case, kebab-case, and legacy camelCase spellings
/// (`apiKey`, `optional-bearer`, `API_KEY` are all `api_key`-class inputs).
fn configured_auth_spec(provider: &str, auth: &str) -> Result<AuthSpec, ModelsConfigError> {
	let mut normalized = String::with_capacity(auth.len() + 4);
	let camel = auth.chars().any(|c| c.is_ascii_lowercase());
	for c in auth.chars() {
		if c == '-' {
			normalized.push('_');
		} else if c.is_ascii_uppercase() {
			if camel && !normalized.is_empty() && !normalized.ends_with('_') {
				normalized.push('_');
			}
			normalized.push(c.to_ascii_lowercase());
		} else {
			normalized.push(c);
		}
	}
	let kind = normalized
		.parse::<AuthSpecKind>()
		.ok()
		.filter(|kind| {
			matches!(
				kind,
				AuthSpecKind::None
					| AuthSpecKind::ApiKey
					| AuthSpecKind::Bearer
					| AuthSpecKind::OptionalBearer
					| AuthSpecKind::Basic
			)
		})
		.ok_or_else(|| ModelsConfigError::InvalidAuth { provider: Str::new(provider) })?;
	let env_base: String = provider
		.chars()
		.map(|c| {
			if c.is_ascii_alphanumeric() {
				c.to_ascii_uppercase()
			} else {
				'_'
			}
		})
		.collect();
	let bearer =
		matches!(kind, AuthSpecKind::ApiKey | AuthSpecKind::Bearer | AuthSpecKind::OptionalBearer);
	let credential_sources: Box<[CredentialSourceSpec]> = match kind {
		AuthSpecKind::None => Box::new([]),
		AuthSpecKind::Basic => Box::new([CredentialSourceSpec::BasicEnvironment {
			username_names: Box::new([Str::from(format!("OMP_{env_base}_USERNAME"))]),
			password_names: Box::new([Str::from(format!("OMP_{env_base}_PASSWORD"))]),
		}]),
		_ => Box::new([
			CredentialSourceSpec::Environment {
				ordered_names: Box::new([Str::from(format!("OMP_{env_base}_API_KEY"))]),
			},
			CredentialSourceSpec::Stored,
		]),
	};
	Ok(AuthSpec {
		id: omp_catalog::AuthSpecId::from(format!("{provider}-configured-auth")),
		kind,
		header_name: bearer.then(|| Str::new_static("authorization")),
		query_parameter: None,
		prefix: bearer.then(|| Str::new_static("Bearer ")),
		sealed_body: None,
		scopes: Box::new([]),
		audience: None,
		account_scope: AccountScope::Provider,
		credential_sources,
		oauth: None,
		signing: None,
	})
}

fn configured_model_record(
	catalog: &omp_catalog::Catalog,
	_provider: &str,
	key: &str,
	model: &ModelConfig,
	definition: &ProviderConfig,
	route: RouteId,
	source: &ProvenanceSource,
) -> ModelSpec {
	let (template_provider, _) = configured_provider_template(catalog, definition);
	let template = catalog
		.models()
		.iter()
		.find(|candidate| candidate.routes.contains(&route))
		.or_else(|| {
			catalog.models().iter().find(|candidate| {
				candidate
					.routes
					.iter()
					.any(|route| template_provider.routes.contains(route))
			})
		})
		.or_else(|| catalog.models().first())
		.expect("embedded catalog has a model template");
	ModelSpec {
		key: ModelKey::from(key),
		class: ClassId::from(key),
		display_name: model.name.clone().unwrap_or_else(|| Str::new(key)),
		wire_ids: Box::new([(route.clone(), WireModelId::from(key))]),
		routes: Box::new([route]),
		capabilities: configured_chat_capabilities(),
		limits: ModelLimits::default(),
		thinking: None,
		thinking_routing: ThinkingRouting::default(),
		wire_policy: template.wire_policy.clone(),
		context: ContextStrategy::Replay,
		pricing: Pricing::default(),
		availability: ModelAvailability::Available,
		provenance: ModelProvenance {
			sources:          Box::new([source.clone()]),
			updated_at_ms:    None,
			blocked_until_ms: None,
			deprecated:       false,
		},
		context_promotion_target: None,
		compaction_model: None,
		edit_revision: None,
		remote_compaction: None,
		premium_multiplier_millionths: None,
	}
}

fn parse_modalities(
	value: &toml::Value,
	provider: &str,
	model: &str,
) -> Result<ModalityBits, ModelsConfigError> {
	let values = value
		.as_array()
		.ok_or_else(|| ModelsConfigError::InvalidFact {
			provider: Str::new(provider),
			model:    Str::new(model),
			field:    "input",
		})?;
	let mut modalities = ModalityBits::empty();
	for value in values {
		let modality = value
			.as_str()
			.ok_or_else(|| ModelsConfigError::InvalidFact {
				provider: Str::new(provider),
				model:    Str::new(model),
				field:    "input",
			})?;
		match modality {
			"text" => modalities.insert(ModalityBits::TEXT),
			"image" => modalities.insert(ModalityBits::IMAGE),
			"audio" => modalities.insert(ModalityBits::AUDIO),
			"video" => modalities.insert(ModalityBits::VIDEO),
			"document" => modalities.insert(ModalityBits::DOCUMENT),
			_ => {
				return Err(ModelsConfigError::InvalidFact {
					provider: Str::new(provider),
					model:    Str::new(model),
					field:    "input",
				});
			},
		}
	}
	Ok(modalities)
}

fn parse_multiplier(value: &str) -> Option<u64> {
	let value = value.trim();
	let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
	if whole.is_empty()
		|| fractional.len() > 6
		|| !whole.bytes().all(|byte| byte.is_ascii_digit())
		|| !fractional.bytes().all(|byte| byte.is_ascii_digit())
	{
		return None;
	}
	let whole = whole.parse::<u64>().ok()?;
	let fractional = if fractional.is_empty() {
		0
	} else {
		fractional
			.parse::<u64>()
			.ok()?
			.checked_mul(10_u64.pow(u32::try_from(6_usize.saturating_sub(fractional.len())).ok()?))?
	};
	whole
		.checked_mul(PremiumMultiplier::SCALE)?
		.checked_add(fractional)
}

/// Publishes the complete native user-config generation atomically.
pub fn publish_user_overlay(
	store: &OverlayStore,
	config: &ModelsConfig,
) -> Result<(), ModelsConfigError> {
	store.replace(OverlaySource::UserConfig, lower_user_overlay(config)?);
	Ok(())
}
/// Native model-config decoding failures.
#[derive(Debug, thiserror::Error)]
pub enum ModelsConfigError {
	/// A configured provider/model fact is malformed or internally inconsistent.
	#[error("invalid `{field}` for configured model {provider}/{model}")]
	InvalidFact {
		/// Provider containing the invalid fact.
		provider: Str,
		/// Model containing the invalid fact.
		model:    Str,
		/// Stable field name.
		field:    &'static str,
	},
	/// A configured provider `auth` mode names no supported specification kind.
	#[error(
		"invalid `auth` for configured provider {provider}; expected one of none, api_key, bearer, \
		 optional_bearer, basic"
	)]
	InvalidAuth {
		/// Provider containing the invalid auth mode.
		provider: Str,
	},
	/// Reading the configured source failed.
	#[error(transparent)]
	Io(#[from] io::Error),
	/// The TOML source was malformed.
	#[error(transparent)]
	Toml(#[from] de::Error),
	/// A legacy YAML source was malformed.
	#[error(transparent)]
	Yaml(#[from] serde_yaml::Error),
	/// A legacy JSON/JSONC source was malformed.
	#[error(transparent)]
	Json(#[from] omp_slopjson::ParseError),
	/// Native TOML encoding failed.
	#[error(transparent)]
	Encode(#[from] ser::Error),
	/// Atomic persistence failed.
	#[error(transparent)]
	Persist(#[from] omp_settings::io::SettingsIoError),
	/// A legacy source backup failed.
	#[error("failed to back up model config {path} to {backup}")]
	Backup {
		/// Legacy source path.
		path:   PathBuf,
		/// Backup path.
		backup: PathBuf,
		/// Filesystem failure.
		#[source]
		source: io::Error,
	},
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn legacy_yaml_is_imported_once_then_native_toml_is_live() {
		let directory = tempfile::tempdir().expect("directory");
		let legacy = directory.path().join("models.yml");
		fs::write(
			&legacy,
			"providers:\n  demo:\n    models:\n      fast:\n        contextWindow: 4096\n",
		)
		.expect("legacy");
		let imported = load_or_import_legacy(directory.path())
			.expect("import")
			.expect("config");
		assert!(matches!(imported.source, ModelsConfigSource::LegacyYaml(_)));
		assert_eq!(imported.config.providers["demo"].models["fast"].context_window, Some(4096));
		assert!(
			directory
				.path()
				.join("models.yml.pre-omp-migration.bak")
				.exists()
		);
		let native = load_or_import_legacy(directory.path())
			.expect("native")
			.expect("config");
		assert!(matches!(native.source, ModelsConfigSource::NativeToml(_)));
	}

	#[test]
	fn representative_models_toml_decodes_and_publishes() {
		let value: ModelsConfig = toml::from_str(
			"[providers.demo]\nbaseUrl='https://example.test/v1'\nauth='apiKey'\ndisableStrictTools=true\n[providers.demo.models.fast]\ncontextWindow=128000\nmaxTokens=8192\npremiumMultiplier='0.25'\ncontextPromotionTarget='large'\n",
		).expect("decode");
		let provider = &value.providers["demo"];
		assert_eq!(provider.base_url.as_deref(), Some("https://example.test/v1"));
		let model = &provider.models["fast"];
		assert_eq!(model.context_window, Some(128000));
		assert_eq!(model.max_tokens, Some(8192));
		let store = OverlayStore::default();
		publish_user_overlay(&store, &value).expect("publish");
		assert_eq!(store.load().sources(), &[OverlaySource::UserConfig]);
	}

	#[test]
	fn unknown_provider_lowers_complete_provider_route_and_model_records() {
		let value: ModelsConfig = toml::from_str(
			"[providers.demo]\nbaseUrl='https://example.test/v1'\nauth='apiKey'\n[providers.demo.models.fast]\napi='openai-completions'\ncontextWindow=128000\n",
		)
		.expect("decode");
		let overlay = lower_user_overlay(&value).expect("overlay");
		let stack = omp_catalog::OverlayStack::from_layers([(OverlaySource::UserConfig, overlay)]);
		let catalog = omp_catalog::Catalog::embedded()
			.with_overlay_stack(&stack, omp_catalog::UnsafeTrustScope::ALL)
			.expect("materialize configured provider");
		let provider = catalog
			.provider(ProviderId::from_ref("demo"))
			.expect("provider");
		assert_eq!(provider.routes.as_ref(), &[RouteId::from("demo-configured")]);
		let route = catalog.route(&provider.routes[0]).expect("route");
		assert_eq!(route.endpoint.base_url, "https://example.test/v1");
		assert_eq!(route.provider.as_str(), "demo");
		let model = catalog
			.models()
			.iter()
			.find(|model| model.key.as_str() == "fast")
			.expect("model");
		assert_eq!(model.routes.as_ref(), provider.routes.as_ref());
		assert_eq!(model.limits.context_window, Some(128_000));
	}
	#[test]
	fn configured_provider_is_chat_capable_and_speaks_its_declared_api() {
		let value: ModelsConfig = toml::from_str(
			"[providers.demo]\nbaseUrl='http://127.0.0.1:9/v1'\nauth='none'\n[providers.demo.models.fast]\napi='openai-completions'\nsupportsTools=true\n",
		)
		.expect("decode");
		let overlay = lower_user_overlay(&value).expect("overlay");
		let stack = omp_catalog::OverlayStack::from_layers([(OverlaySource::UserConfig, overlay)]);
		let catalog = omp_catalog::Catalog::embedded()
			.with_overlay_stack(&stack, omp_catalog::UnsafeTrustScope::ALL)
			.expect("materialize configured provider");
		let model = catalog
			.models()
			.iter()
			.find(|model| model.key.as_str() == "fast")
			.expect("model");
		assert!(
			model
				.capabilities
				.operations
				.contains_kind(omp_catalog::OperationKind::Chat),
			"configured models must admit the chat operation"
		);
		let chat = model.capabilities.chat.as_ref().expect("chat block");
		assert!(matches!(chat.tools, Availability::Native(_)));
		let route = catalog
			.route(&RouteId::from("demo-configured"))
			.expect("route");
		assert_eq!(route.codec.as_str(), "openai-chat");
		let auth = catalog.auth_spec(&route.auth).expect("interned auth spec");
		assert_eq!(auth.kind, AuthSpecKind::None);
		assert!(auth.credential_sources.is_empty());
	}

	#[test]
	fn configured_auth_modes_normalize_and_reject_unknown_kinds() {
		for (input, kind) in [
			("none", AuthSpecKind::None),
			("apiKey", AuthSpecKind::ApiKey),
			("api-key", AuthSpecKind::ApiKey),
			("API_KEY", AuthSpecKind::ApiKey),
			("optionalBearer", AuthSpecKind::OptionalBearer),
			("basic", AuthSpecKind::Basic),
		] {
			let spec = configured_auth_spec("demo", input).expect(input);
			assert_eq!(spec.kind, kind, "{input}");
		}
		let spec = configured_auth_spec("my-provider", "bearer").expect("bearer");
		assert!(matches!(
			&spec.credential_sources[0],
			CredentialSourceSpec::Environment { ordered_names }
				if ordered_names.as_ref() == ["OMP_MY_PROVIDER_API_KEY"]
		));
		assert!(matches!(
			configured_auth_spec("demo", "oauth"),
			Err(ModelsConfigError::InvalidAuth { .. })
		));
	}
}
