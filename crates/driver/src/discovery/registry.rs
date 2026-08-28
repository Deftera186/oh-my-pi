//! Deterministic, failure-isolated capability provider registry.

use std::{
	collections::{BTreeMap, BTreeSet},
	fmt,
	future::Future,
	io,
	path::PathBuf,
	pin::Pin,
	sync::Arc,
	time::{Duration, Instant},
};

use futures::future::join_all;
use omp_core::Str;
use strum::{Display, EnumString, IntoStaticStr};
use tokio::time;

use super::{
	cache::DiscoveryCache,
	manifest::{
		CapabilityKind, CapabilityProvenance, CapabilityRecord, DiscoveredCapability,
		ProviderProvenance, SourceScope, ValidationIssue,
	},
	settings::DiscoverySettings,
};

const MIN_PROVIDER_DEADLINE: Duration = Duration::from_millis(1);
const MAX_PROVIDER_DEADLINE: Duration = Duration::from_secs(30);

/// Canonical capability kind order used by diagnostics and frozen snapshots.
pub const CAPABILITY_KINDS: &[CapabilityKind] = &[
	CapabilityKind::Skills,
	CapabilityKind::Themes,
	CapabilityKind::Rules,
	CapabilityKind::Mcps,
	CapabilityKind::ContextFiles,
	CapabilityKind::Hooks,
	CapabilityKind::Prompts,
	CapabilityKind::Instructions,
	CapabilityKind::Extensions,
	CapabilityKind::SlashCommands,
	CapabilityKind::Tools,
	CapabilityKind::Settings,
	CapabilityKind::Ssh,
	CapabilityKind::SystemPrompt,
	CapabilityKind::Agents,
];

struct CapabilityMetadata {
	kind:         CapabilityKind,
	display_name: &'static str,
	description:  &'static str,
}

const CAPABILITY_METADATA: &[CapabilityMetadata] = &[
	CapabilityMetadata {
		kind:         CapabilityKind::Skills,
		display_name: "Skills",
		description:  "Specialized knowledge and workflow declarations",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Themes,
		display_name: "Themes",
		description:  "Named terminal appearance resources",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Rules,
		display_name: "Rules",
		description:  "Declarative project and user constraints",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Mcps,
		display_name: "MCP servers",
		description:  "Native Model Context Protocol server declarations",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::ContextFiles,
		display_name: "Context files",
		description:  "Persistent repository instruction documents",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Hooks,
		display_name: "Hooks",
		description:  "Declared pre/post tool hooks",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Prompts,
		display_name: "Prompts",
		description:  "Reusable prompt templates",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Instructions,
		display_name: "Instructions",
		description:  "File-targeted instruction documents",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Extensions,
		display_name: "Extensions",
		description:  "Static native OMP extension manifests",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::SlashCommands,
		display_name: "Slash commands",
		description:  "Markdown slash-command templates",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Tools,
		display_name: "Custom tools",
		description:  "Static custom-tool declarations",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Settings,
		display_name: "Settings",
		description:  "Native settings contributions",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Ssh,
		display_name: "SSH hosts",
		description:  "Native SSH host declarations",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::SystemPrompt,
		display_name: "System prompt",
		description:  "System-prompt projections",
	},
	CapabilityMetadata {
		kind:         CapabilityKind::Agents,
		display_name: "Agents",
		description:  "Static subagent definitions",
	},
];

/// Context supplied to every provider load.
#[derive(Clone, Debug)]
pub struct LoadContext {
	/// Canonical current workspace path or Environment projection.
	pub cwd:             PathBuf,
	/// Canonical native user root, if the composition grants one.
	pub home:            Option<PathBuf>,
	/// Canonical repository root, if present.
	pub repository_root: Option<PathBuf>,
	/// Composition-owned parsed capability cache.
	pub cache:           Arc<DiscoveryCache>,
}

/// Provider-originated non-fatal diagnostic category.
#[derive(Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum ProviderNoticeCode {
	/// A source was malformed and skipped.
	MalformedSource,
	/// A source could not be read through its authority.
	UnreadableSource,
	/// A declaration used an unsupported field or value.
	UnsupportedDeclaration,
	/// Provider output was bounded and truncated.
	Truncated,
}

/// One provider-originated non-fatal diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderNotice {
	/// Stable diagnostic category.
	pub code:   ProviderNoticeCode,
	/// Source path, when the warning concerns one document.
	pub path:   Option<PathBuf>,
	/// Sanitized provider detail for diagnostics.
	pub detail: Option<Str>,
}

/// Successful output of one provider for one capability family.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderLoad {
	/// Parsed static declarations.
	pub declarations: Vec<DiscoveredCapability>,
	/// Non-fatal provider warnings.
	pub warnings:     Vec<ProviderNotice>,
}

/// Typed provider load failures.
#[derive(Debug, thiserror::Error)]
pub enum ProviderLoadError {
	/// The configured provider source is currently unavailable.
	#[error("provider source is unavailable")]
	Unavailable,
	/// An authority read failed.
	#[error("provider source read failed at {path}")]
	Io {
		/// Source path requested from the authority.
		path:   PathBuf,
		/// Typed I/O failure.
		#[source]
		source: io::Error,
	},
	/// Provider input was structurally rejected before declarations were
	/// emitted.
	#[error("provider source data was rejected")]
	Rejected,
}

/// Heap-erased provider future.
///
/// Provider loading is a cold dynamic boundary dominated by authority I/O; the
/// single allocation permits a heterogeneous registry without allocating on
/// any tool, token, or frame path.
pub type ProviderFuture<'a> =
	Pin<Box<dyn Future<Output = Result<ProviderLoad, ProviderLoadError>> + Send + 'a>>;

/// Static capability provider. Implementations parse declarations but never
/// activate executable extension code.
pub trait CapabilityProvider: Send + Sync {
	/// Stable provider ID.
	fn id(&self) -> &str;
	/// Human-readable provider label.
	fn display_name(&self) -> &str;
	/// Provider purpose for settings and diagnostics.
	fn description(&self) -> &str;
	/// Higher values collate first. Provider ID breaks equal-priority ties.
	fn priority(&self) -> i32;
	/// Capability families emitted by this provider.
	fn capabilities(&self) -> &[CapabilityKind];
	/// Requested provider deadline. The registry clamps it to its bounded range.
	fn deadline(&self) -> Duration;
	/// Loads one supported capability family.
	fn load<'a>(&'a self, kind: CapabilityKind, context: &'a LoadContext) -> ProviderFuture<'a>;
}

/// Provider registration failures.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
	/// A stable provider ID was registered more than once.
	#[error("discovery provider {provider_id} is already registered")]
	DuplicateProvider {
		/// Duplicate stable provider identity.
		provider_id: Str,
	},
}

/// Provider introspection row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderInfo {
	/// Stable provider ID.
	pub id:           Str,
	/// Human-readable label.
	pub display_name: Str,
	/// Settings/diagnostic description.
	pub description:  Str,
	/// Stable provider priority.
	pub priority:     i32,
	/// Effective enablement from the typed settings projection.
	pub enabled:      bool,
	/// Capability families served by this provider.
	pub capabilities: Vec<CapabilityKind>,
	/// Registry-clamped provider deadline.
	pub deadline:     Duration,
}

/// Capability introspection row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityInfo {
	/// Capability family.
	pub kind:         CapabilityKind,
	/// Human-readable name.
	pub display_name: &'static str,
	/// Human-readable purpose.
	pub description:  &'static str,
	/// Stable provider rows for this family.
	pub providers:    Vec<ProviderInfo>,
}

/// Per-load provider filter and declaration admission options.
#[derive(Clone, Copy, Default)]
pub struct LoadOptions<'a> {
	/// Optional provider allowlist.
	pub providers:         Option<&'a [&'a str]>,
	/// Providers excluded after allowlist selection.
	pub exclude_providers: &'a [&'a str],
	/// Retain validation-failing winners.
	pub include_invalid:   bool,
	/// Retain disabled winners rather than using them only as key claims.
	pub include_disabled:  bool,
	/// Drop a declaration before deduplication so it claims no key.
	pub filter:            Option<&'a dyn Fn(&CapabilityRecord) -> bool>,
	/// Hide a declaration while preserving its key claim.
	pub suppress:          Option<&'a dyn Fn(&CapabilityRecord) -> bool>,
}

/// Outcome of one bounded provider load.
#[derive(Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum ProviderOutcome {
	/// Provider completed successfully.
	Succeeded,
	/// Provider returned a typed failure.
	Failed,
	/// Provider exceeded its bounded deadline.
	TimedOut,
}

/// Stable provider timing and cardinality diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTiming {
	/// Stable provider ID.
	pub provider_id:  Str,
	/// Wall-clock load duration.
	pub elapsed:      Duration,
	/// Terminal load outcome.
	pub outcome:      ProviderOutcome,
	/// Declaration count before registry filtering.
	pub declarations: usize,
}

/// Structured provider failure reason.
#[derive(Clone, Debug)]
pub enum ProviderFailureKind {
	/// Provider deadline elapsed.
	Timeout,
	/// Provider returned a typed error.
	Load(Arc<ProviderLoadError>),
}

/// Isolated provider failure retained in discovery diagnostics.
#[derive(Clone, Debug)]
pub struct ProviderFailure {
	/// Stable provider ID.
	pub provider_id: Str,
	/// Capability family being loaded.
	pub kind:        CapabilityKind,
	/// Structured failure.
	pub failure:     ProviderFailureKind,
}

/// Registry-produced warning.
#[derive(Clone, Debug, PartialEq)]
pub enum RegistryWarning {
	/// Provider-originated warning with provider provenance.
	Provider {
		/// Provider identity.
		provider: ProviderProvenance,
		/// Typed provider notice.
		notice:   ProviderNotice,
	},
	/// A provider emitted a payload for the wrong capability family.
	KindMismatch {
		/// Provider identity.
		provider: ProviderProvenance,
		/// Requested family.
		expected: CapabilityKind,
		/// Emitted family.
		actual:   CapabilityKind,
		/// Source path.
		path:     PathBuf,
	},
	/// A provisional winner failed canonical validation.
	Validation {
		/// Complete source provenance.
		provenance: CapabilityProvenance,
		/// Stable validation category.
		issue:      ValidationIssue,
	},
}

/// Stable identity of the declaration responsible for suppressing another.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimRef {
	/// Winning or claiming provider ID.
	pub provider_id: Str,
	/// Winning or claiming key.
	pub key:         Option<Str>,
	/// Winning or claiming source path.
	pub path:        PathBuf,
}

impl ClaimRef {
	fn from_record(record: &CapabilityRecord) -> Self {
		Self {
			provider_id: record.provenance.provider.id.clone(),
			key:         record.key.clone(),
			path:        record.provenance.source.path.clone(),
		}
	}
}

/// Final disposition of one post-filter declaration claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimDisposition {
	/// Declaration belongs to the winning set.
	Winner,
	/// A prior key claim won.
	ShadowedByKey {
		/// Earlier declaration that claimed the key.
		by: ClaimRef,
	},
	/// A prior semantically equivalent declaration won.
	ShadowedByEquivalent {
		/// Earlier declaration with equivalent semantics.
		by: ClaimRef,
	},
	/// Disabled declaration claimed its key but was not admitted.
	Disabled,
	/// Embedder suppression claimed the key but hid the declaration.
	Suppressed,
	/// Explicit user configuration suppresses this bundled declaration.
	ConfiguredUserShadow,
	/// Provisional winner failed validation and was removed.
	Invalid {
		/// Canonical validation category.
		issue: ValidationIssue,
	},
}

/// One declaration and its deterministic claim disposition.
#[derive(Clone, Debug, PartialEq)]
pub struct CapabilityClaim {
	/// Normalized declaration.
	pub capability:  Arc<CapabilityRecord>,
	/// Registry disposition.
	pub disposition: ClaimDisposition,
}

/// Deterministic merged result for one capability family.
#[derive(Clone, Debug)]
pub struct CapabilityResult {
	/// Capability family loaded.
	pub kind:     CapabilityKind,
	/// Winning declarations in stable collation order.
	pub winners:  Arc<[Arc<CapabilityRecord>]>,
	/// Every post-filter declaration and its disposition.
	pub claims:   Arc<[CapabilityClaim]>,
	/// Provider and validation warnings.
	pub warnings: Arc<[RegistryWarning]>,
	/// Isolated provider failures.
	pub failures: Arc<[ProviderFailure]>,
	/// One timing row per attempted provider.
	pub timings:  Arc<[ProviderTiming]>,
}

/// Immutable provider registry assembled at the application boundary.
pub struct DiscoveryRegistry {
	providers: Vec<Arc<dyn CapabilityProvider>>,
	settings:  Arc<DiscoverySettings>,
	cache:     Arc<DiscoveryCache>,
}

impl fmt::Debug for DiscoveryRegistry {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("DiscoveryRegistry")
			.field("providers", &self.providers.len())
			.field("settings", &self.settings)
			.field("cache", &self.cache)
			.finish()
	}
}

impl DiscoveryRegistry {
	/// Creates an empty registry from one immutable settings projection and one
	/// composition-owned cache.
	pub fn new(settings: Arc<DiscoverySettings>, cache: Arc<DiscoveryCache>) -> Self {
		Self { providers: Vec::new(), settings, cache }
	}

	/// Returns the composition-owned cache supplied to providers.
	pub fn cache(&self) -> &Arc<DiscoveryCache> {
		&self.cache
	}

	/// Registers one provider and restores stable priority/ID order.
	pub fn register(&mut self, provider: Arc<dyn CapabilityProvider>) -> Result<(), RegistryError> {
		if self
			.providers
			.iter()
			.any(|registered| registered.id() == provider.id())
		{
			return Err(RegistryError::DuplicateProvider { provider_id: provider.id().into() });
		}
		self.providers.push(provider);
		self.providers.sort_by(|left, right| {
			right
				.priority()
				.cmp(&left.priority())
				.then_with(|| left.id().cmp(right.id()))
		});
		Ok(())
	}

	/// Lists every registered provider in deterministic collation order.
	pub fn providers(&self) -> Vec<ProviderInfo> {
		self
			.providers
			.iter()
			.map(|provider| self.provider_row(provider.as_ref()))
			.collect()
	}

	/// Returns one provider introspection row.
	pub fn provider(&self, provider_id: &str) -> Option<ProviderInfo> {
		self
			.providers
			.iter()
			.find(|provider| provider.id() == provider_id)
			.map(|provider| self.provider_row(provider.as_ref()))
	}

	/// Lists every canonical capability and its registered providers.
	pub fn capabilities(&self) -> Vec<CapabilityInfo> {
		CAPABILITY_METADATA
			.iter()
			.map(|metadata| self.capability_row(metadata))
			.collect()
	}

	/// Returns one canonical capability introspection row.
	pub fn capability(&self, kind: CapabilityKind) -> CapabilityInfo {
		let metadata = CAPABILITY_METADATA
			.iter()
			.find(|metadata| metadata.kind == kind)
			.expect("canonical capability metadata is complete");
		self.capability_row(metadata)
	}

	/// Concurrently loads all selected providers with bounded deadlines, then
	/// collates their declarations in stable priority/ID order.
	pub async fn load(
		&self,
		kind: CapabilityKind,
		context: &LoadContext,
		options: LoadOptions<'_>,
	) -> CapabilityResult {
		let selected = self.selected_providers(kind, options);
		let attempts = join_all(selected.iter().map(|provider| async move {
			let started = Instant::now();
			let deadline = bounded_deadline(provider.deadline());
			let result = time::timeout(deadline, provider.load(kind, context)).await;
			(provider, started.elapsed(), result)
		}))
		.await;

		let mut records = Vec::new();
		let mut warnings = Vec::new();
		let mut failures = Vec::new();
		let mut timings = Vec::with_capacity(attempts.len());

		for (provider, elapsed, attempt) in attempts {
			let provenance = provider_provenance(provider.as_ref());
			match attempt {
				Err(_) => {
					timings.push(ProviderTiming {
						provider_id: provenance.id.clone(),
						elapsed,
						outcome: ProviderOutcome::TimedOut,
						declarations: 0,
					});
					failures.push(ProviderFailure {
						provider_id: provenance.id,
						kind,
						failure: ProviderFailureKind::Timeout,
					});
				},
				Ok(Err(source)) => {
					timings.push(ProviderTiming {
						provider_id: provenance.id.clone(),
						elapsed,
						outcome: ProviderOutcome::Failed,
						declarations: 0,
					});
					failures.push(ProviderFailure {
						provider_id: provenance.id,
						kind,
						failure: ProviderFailureKind::Load(Arc::new(source)),
					});
				},
				Ok(Ok(load)) => {
					timings.push(ProviderTiming {
						provider_id: provenance.id.clone(),
						elapsed,
						outcome: ProviderOutcome::Succeeded,
						declarations: load.declarations.len(),
					});
					warnings.extend(
						load
							.warnings
							.into_iter()
							.map(|notice| RegistryWarning::Provider {
								provider: provenance.clone(),
								notice,
							}),
					);
					for declaration in load.declarations {
						let actual = declaration.payload.kind();
						if actual != kind {
							warnings.push(RegistryWarning::KindMismatch {
								provider: provenance.clone(),
								expected: kind,
								actual,
								path: declaration.source.path,
							});
							continue;
						}
						let payload_enabled = declaration.payload.declared_enabled();
						let record = CapabilityRecord {
							key:          declaration.key,
							semantic_key: declaration.semantic_key,
							enabled:      declaration.enabled
								&& payload_enabled
								&& self.settings.source_enabled(&declaration.source.source_id),
							payload:      declaration.payload,
							provenance:   CapabilityProvenance {
								provider: provenance.clone(),
								source:   declaration.source,
							},
						};
						if options.filter.is_none_or(|filter| filter(&record)) {
							records.push(Arc::new(record));
						}
					}
				},
			}
		}

		let (winners, claims) = self.collate(kind, records, options, &mut warnings);
		CapabilityResult {
			kind,
			winners: winners.into(),
			claims: claims.into(),
			warnings: warnings.into(),
			failures: failures.into(),
			timings: timings.into(),
		}
	}

	fn collate(
		&self,
		kind: CapabilityKind,
		records: Vec<Arc<CapabilityRecord>>,
		options: LoadOptions<'_>,
		warnings: &mut Vec<RegistryWarning>,
	) -> (Vec<Arc<CapabilityRecord>>, Vec<CapabilityClaim>) {
		let explicit_user_claims = records
			.iter()
			.filter(|record| record.provenance.source.scope == SourceScope::User)
			.filter_map(|record| record.key.as_ref())
			.filter(|key| self.settings.shadows_builtin(kind, key))
			.cloned()
			.collect::<BTreeSet<_>>();

		let mut key_claims = BTreeMap::<Str, ClaimRef>::new();
		let mut semantic_claims = BTreeMap::<Str, ClaimRef>::new();
		let mut claims = Vec::with_capacity(records.len());
		let mut provisional_winners = Vec::<(usize, Arc<CapabilityRecord>)>::new();

		for record in records {
			let reference = ClaimRef::from_record(&record);
			if record.provenance.source.scope == SourceScope::BuiltIn
				&& record
					.key
					.as_ref()
					.is_some_and(|key| explicit_user_claims.contains(key))
			{
				claims.push(CapabilityClaim {
					capability:  record,
					disposition: ClaimDisposition::ConfiguredUserShadow,
				});
				continue;
			}

			if let Some(key) = record.key.as_ref() {
				if let Some(by) = key_claims.get(key) {
					claims.push(CapabilityClaim {
						capability:  record,
						disposition: ClaimDisposition::ShadowedByKey { by: by.clone() },
					});
					continue;
				}
				key_claims.insert(key.clone(), reference.clone());
			}

			let suppressed = options.suppress.is_some_and(|suppress| suppress(&record));
			if suppressed || (!record.enabled && !options.include_disabled) {
				claims.push(CapabilityClaim {
					capability:  record,
					disposition: if suppressed {
						ClaimDisposition::Suppressed
					} else {
						ClaimDisposition::Disabled
					},
				});
				continue;
			}

			let equivalent = record
				.semantic_key
				.as_ref()
				.and_then(|key| semantic_claims.get(key).cloned())
				.or_else(|| {
					provisional_winners.iter().find_map(|(_, winner)| {
						record
							.payload
							.semantically_equivalent(&winner.payload)
							.then(|| ClaimRef::from_record(winner))
					})
				});
			if let Some(by) = equivalent {
				claims.push(CapabilityClaim {
					capability:  record,
					disposition: ClaimDisposition::ShadowedByEquivalent { by },
				});
				continue;
			}

			if let Some(key) = record.semantic_key.as_ref() {
				semantic_claims.insert(key.clone(), reference);
			}
			let claim_index = claims.len();
			claims.push(CapabilityClaim {
				capability:  Arc::clone(&record),
				disposition: ClaimDisposition::Winner,
			});
			provisional_winners.push((claim_index, record));
		}

		let mut winners = Vec::with_capacity(provisional_winners.len());
		for (claim_index, record) in provisional_winners {
			if let Some(issue) = record.payload.validation_issue() {
				warnings
					.push(RegistryWarning::Validation { provenance: record.provenance.clone(), issue });
				if !options.include_invalid {
					claims[claim_index].disposition = ClaimDisposition::Invalid { issue };
					continue;
				}
			}
			winners.push(record);
		}
		(winners, claims)
	}

	fn selected_providers(
		&self,
		kind: CapabilityKind,
		options: LoadOptions<'_>,
	) -> Vec<&Arc<dyn CapabilityProvider>> {
		self
			.providers
			.iter()
			.filter(|provider| self.settings.provider_enabled(provider.id()))
			.filter(|provider| provider.capabilities().contains(&kind))
			.filter(|provider| {
				options
					.providers
					.is_none_or(|allowed| allowed.contains(&provider.id()))
			})
			.filter(|provider| !options.exclude_providers.contains(&provider.id()))
			.collect()
	}

	fn provider_row(&self, provider: &dyn CapabilityProvider) -> ProviderInfo {
		ProviderInfo {
			id:           provider.id().into(),
			display_name: provider.display_name().into(),
			description:  provider.description().into(),
			priority:     provider.priority(),
			enabled:      self.settings.provider_enabled(provider.id()),
			capabilities: provider.capabilities().to_vec(),
			deadline:     bounded_deadline(provider.deadline()),
		}
	}

	fn capability_row(&self, metadata: &CapabilityMetadata) -> CapabilityInfo {
		CapabilityInfo {
			kind:         metadata.kind,
			display_name: metadata.display_name,
			description:  metadata.description,
			providers:    self
				.providers
				.iter()
				.filter(|provider| provider.capabilities().contains(&metadata.kind))
				.map(|provider| self.provider_row(provider.as_ref()))
				.collect(),
		}
	}
}

fn bounded_deadline(requested: Duration) -> Duration {
	requested.clamp(MIN_PROVIDER_DEADLINE, MAX_PROVIDER_DEADLINE)
}

fn provider_provenance(provider: &dyn CapabilityProvider) -> ProviderProvenance {
	ProviderProvenance {
		id:           provider.id().into(),
		display_name: provider.display_name().into(),
		priority:     provider.priority(),
	}
}

#[cfg(test)]
mod tests {
	use tokio::time;

	use super::*;
	use crate::discovery::{
		manifest::{CapabilityPayload, PromptPayload, SourceProvenance},
		settings::BuiltinShadow,
	};

	#[derive(Clone)]
	enum Behavior {
		Load(ProviderLoad),
		Fail,
		Delay(ProviderLoad),
	}

	struct TestProvider {
		id:       &'static str,
		priority: i32,
		deadline: Duration,
		behavior: Behavior,
	}

	impl CapabilityProvider for TestProvider {
		fn id(&self) -> &str {
			self.id
		}

		fn display_name(&self) -> &str {
			self.id
		}

		fn description(&self) -> &str {
			"test provider"
		}

		fn priority(&self) -> i32 {
			self.priority
		}

		fn capabilities(&self) -> &[CapabilityKind] {
			&[CapabilityKind::Prompts]
		}

		fn deadline(&self) -> Duration {
			self.deadline
		}

		fn load<'a>(&'a self, _: CapabilityKind, _: &'a LoadContext) -> ProviderFuture<'a> {
			Box::pin(async move {
				match &self.behavior {
					Behavior::Load(load) => Ok(load.clone()),
					Behavior::Fail => Err(ProviderLoadError::Unavailable),
					Behavior::Delay(load) => {
						time::sleep(Duration::from_millis(50)).await;
						Ok(load.clone())
					},
				}
			})
		}
	}

	fn prompt(
		provider_source: &'static str,
		scope: SourceScope,
		key: &'static str,
		content: &'static str,
	) -> DiscoveredCapability {
		DiscoveredCapability::keyed(
			key,
			CapabilityPayload::Prompts(PromptPayload {
				name:    key.into(),
				path:    PathBuf::from(provider_source),
				content: content.into(),
			}),
			SourceProvenance::native(provider_source, PathBuf::from(provider_source), scope),
		)
	}

	fn provider(id: &'static str, priority: i32, behavior: Behavior) -> Arc<dyn CapabilityProvider> {
		Arc::new(TestProvider { id, priority, deadline: Duration::from_secs(1), behavior })
	}

	fn context(cache: Arc<DiscoveryCache>) -> LoadContext {
		LoadContext {
			cwd: PathBuf::from("/env/repo"),
			home: None,
			repository_root: Some(PathBuf::from("/env/repo")),
			cache,
		}
	}

	#[tokio::test]
	async fn discovery_precedence_dedup_and_shadowing_table() {
		let cache = Arc::new(DiscoveryCache::new());
		let settings = Arc::new(DiscoverySettings {
			disabled_sources: vec!["disabled-high".into()],
			builtin_shadows: vec![BuiltinShadow {
				kind: CapabilityKind::Prompts,
				key:  "review".into(),
			}],
			..DiscoverySettings::default()
		});
		let mut registry = DiscoveryRegistry::new(settings, Arc::clone(&cache));
		let mut disabled = prompt("disabled-high", SourceScope::Project, "blocked", "off");
		disabled.semantic_key = Some("unused".into());
		let mut equivalent_a = prompt("equivalent-a", SourceScope::Project, "alias-a", "a");
		equivalent_a.semantic_key = Some("same-endpoint".into());
		let mut equivalent_b = prompt("equivalent-b", SourceScope::Project, "alias-b", "b");
		equivalent_b.semantic_key = Some("same-endpoint".into());
		registry
			.register(provider(
				"native-high",
				300,
				Behavior::Load(ProviderLoad {
					declarations: vec![
						prompt("builtin", SourceScope::BuiltIn, "review", "bundled"),
						disabled,
						equivalent_a,
					],
					warnings:     Vec::new(),
				}),
			))
			.expect("register high");
		registry
			.register(provider(
				"user-low",
				10,
				Behavior::Load(ProviderLoad {
					declarations: vec![
						prompt("user", SourceScope::User, "review", "configured user"),
						prompt("enabled-low", SourceScope::Package, "blocked", "must stay shadowed"),
						equivalent_b,
					],
					warnings:     Vec::new(),
				}),
			))
			.expect("register low");

		let result = registry
			.load(CapabilityKind::Prompts, &context(cache), LoadOptions::default())
			.await;
		let winner_keys = result
			.winners
			.iter()
			.filter_map(|record| record.key.as_deref())
			.collect::<Vec<_>>();
		assert_eq!(winner_keys, ["alias-a", "review"]);
		assert!(result.claims.iter().any(|claim| {
			claim.capability.provenance.source.scope == SourceScope::BuiltIn
				&& claim.disposition == ClaimDisposition::ConfiguredUserShadow
		}));
		assert!(result.claims.iter().any(|claim| {
			claim.capability.provenance.source.source_id == "disabled-high"
				&& claim.disposition == ClaimDisposition::Disabled
		}));
		assert!(result.claims.iter().any(|claim| {
			claim.capability.key.as_deref() == Some("blocked")
				&& matches!(claim.disposition, ClaimDisposition::ShadowedByKey { .. })
		}));
		assert!(result.claims.iter().any(|claim| {
			claim.capability.key.as_deref() == Some("alias-b")
				&& matches!(claim.disposition, ClaimDisposition::ShadowedByEquivalent { .. })
		}));
	}

	#[tokio::test]
	async fn discovery_filter_drops_claims_while_suppress_retains_them() {
		let cache = Arc::new(DiscoveryCache::new());
		let mut registry =
			DiscoveryRegistry::new(Arc::new(DiscoverySettings::default()), Arc::clone(&cache));
		registry
			.register(provider(
				"high",
				20,
				Behavior::Load(ProviderLoad {
					declarations: vec![
						prompt("filtered", SourceScope::Project, "filtered-key", "high"),
						prompt("suppressed", SourceScope::Project, "suppressed-key", "high"),
					],
					warnings:     Vec::new(),
				}),
			))
			.expect("high provider");
		registry
			.register(provider(
				"low",
				10,
				Behavior::Load(ProviderLoad {
					declarations: vec![
						prompt("low-filtered", SourceScope::Package, "filtered-key", "low"),
						prompt("low-suppressed", SourceScope::Package, "suppressed-key", "low"),
					],
					warnings:     Vec::new(),
				}),
			))
			.expect("low provider");

		let filter = |record: &CapabilityRecord| record.provenance.source.source_id != "filtered";
		let suppress = |record: &CapabilityRecord| record.provenance.source.source_id == "suppressed";
		let result = registry
			.load(CapabilityKind::Prompts, &context(cache), LoadOptions {
				filter: Some(&filter),
				suppress: Some(&suppress),
				..LoadOptions::default()
			})
			.await;
		assert_eq!(
			result
				.winners
				.iter()
				.filter_map(|record| record.key.as_deref())
				.collect::<Vec<_>>(),
			["filtered-key"],
		);
		assert_eq!(result.claims.len(), 3);
		assert!(result.claims.iter().any(|claim| {
			claim.capability.provenance.source.source_id == "suppressed"
				&& claim.disposition == ClaimDisposition::Suppressed
		}));
		assert!(result.claims.iter().any(|claim| {
			claim.capability.provenance.source.source_id == "low-suppressed"
				&& matches!(claim.disposition, ClaimDisposition::ShadowedByKey { .. })
		}));
	}

	#[tokio::test]
	async fn discovery_provider_failure_and_timeout_are_isolated() {
		let cache = Arc::new(DiscoveryCache::new());
		let mut registry =
			DiscoveryRegistry::new(Arc::new(DiscoverySettings::default()), Arc::clone(&cache));
		registry
			.register(provider("failure", 30, Behavior::Fail))
			.expect("failure provider");
		registry
			.register(Arc::new(TestProvider {
				id:       "timeout",
				priority: 20,
				deadline: Duration::from_millis(5),
				behavior: Behavior::Delay(ProviderLoad::default()),
			}))
			.expect("timeout provider");
		registry
			.register(provider(
				"success",
				10,
				Behavior::Load(ProviderLoad {
					declarations: vec![prompt("success", SourceScope::Project, "ok", "survives")],
					warnings:     Vec::new(),
				}),
			))
			.expect("success provider");

		let result = registry
			.load(CapabilityKind::Prompts, &context(cache), LoadOptions::default())
			.await;
		assert_eq!(result.winners.len(), 1);
		assert_eq!(result.failures.len(), 2);
		assert_eq!(
			result
				.timings
				.iter()
				.map(|timing| timing.outcome)
				.collect::<Vec<_>>(),
			[ProviderOutcome::Failed, ProviderOutcome::TimedOut, ProviderOutcome::Succeeded,]
		);
	}
}
