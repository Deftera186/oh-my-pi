//! Immutable rule partitions, sticky synthesis, and `rule://` authority.

use std::{
	collections::{BTreeMap, BTreeSet},
	fs,
	path::PathBuf,
	sync::Arc,
};

use async_trait::async_trait;
use omp_agent::{
	PromptNamedInput, TtsrCompileError, TtsrInterruptMode, TtsrRegistry, TtsrRule, TtsrSettings,
};
use omp_core::{CowBytes, Str};
use omp_envd::exthost::control::{
	self, ControlAuthority, ControlConnectionIdentity, ControlEffect, ControlProtocolError,
	ControlRequestContext,
};
use omp_settings::{
	DomainRegistration, FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError,
};
use omp_tools::read::{
	Fault,
	resolver::{
		LineOffsetCache, Resolve, ResourceCompletion, ResourceEntry, ResourceList, fuzzy_score,
	},
	selector::ParsedSelector,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use url::Url;

use crate::{
	discovery::{
		manifest::{
			CapabilityPayload, CapabilityRecord, DiscoveredCapability, RuleInterruptMode, RulePayload,
			SourceScope,
		},
		rules::{applies_to, parse_static},
	},
	rules::assets::BUILTIN_RULES,
};

/// Rulebook settings applied before names claim precedence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RulebookSettings {
	/// Whether the bundled 27-rule modernization pack participates.
	pub builtins_enabled: bool,
	/// Explicit per-rule blocklist.
	pub blocked:          BTreeSet<Str>,
}

impl Default for RulebookSettings {
	fn default() -> Self {
		Self { builtins_enabled: true, blocked: BTreeSet::new() }
	}
}

const RULE_SCOPES: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

impl SettingsDomain for RulebookSettings {
	const DOMAIN: &'static str = "rules";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "rules.builtins_enabled",
			label:       "Built-in rules",
			description: "Enable the bundled lowest-priority modernization rule packs.",
			kind:        SettingKind::Boolean,
			scopes:      RULE_SCOPES,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "rules.blocked",
			label:       "Blocked rules",
			description: "Rule names excluded before partitioning and prompt injection.",
			kind:        SettingKind::Array,
			scopes:      RULE_SCOPES,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
	];

	fn validate(&self) -> Result<(), ValidationError> {
		if self.blocked.iter().all(|value| !value.is_empty()) {
			Ok(())
		} else {
			Err(ValidationError::DomainInvariant { domain: Self::DOMAIN })
		}
	}
}

omp_settings::inventory::submit! {
	DomainRegistration::of::<RulebookSettings>()
}

/// Frozen rule with source provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveRule {
	/// Parsed declaration.
	pub declaration: RulePayload,
	/// Stable source ID.
	pub source:      Str,
	/// Source scope.
	pub scope:       SourceScope,
	/// Whether this is a lowest-priority bundled declaration.
	pub builtin:     bool,
}

/// Immutable active rule snapshot consumed by prompts, TTSR, and `rule://`.
#[derive(Clone, Debug, Default)]
pub struct RuleSnapshot {
	ordered: Arc<[ActiveRule]>,
	by_name: Arc<BTreeMap<Str, usize>>,
	sticky:  Arc<[usize]>,
	indexed: Arc<[usize]>,
	ttsr:    Arc<[usize]>,
}

impl RuleSnapshot {
	/// Freezes provider declarations before registry provenance attachment.
	pub fn from_declarations(
		declarations: &[DiscoveredCapability],
		settings: &RulebookSettings,
	) -> Self {
		let mut claimed = BTreeSet::new();
		let mut rules = Vec::new();
		for declaration in declarations {
			let CapabilityPayload::Rules(rule) = &declaration.payload else {
				continue;
			};
			if settings.blocked.contains(&rule.name) || !claimed.insert(rule.name.clone()) {
				continue;
			}
			rules.push(ActiveRule {
				declaration: rule.clone(),
				source:      declaration.source.source_id.clone(),
				scope:       declaration.source.scope,
				builtin:     false,
			});
		}
		if settings.builtins_enabled {
			for asset in BUILTIN_RULES {
				if settings.blocked.contains(asset.name) || claimed.contains(asset.name) {
					continue;
				}
				let Ok(rule) = parse_static(
					asset.name,
					PathBuf::from(format!("builtin://rules/{}.md", asset.name)),
					asset.source,
				) else {
					continue;
				};
				claimed.insert(rule.name.clone());
				rules.push(ActiveRule {
					declaration: rule,
					source:      Str::from("builtin-defaults"),
					scope:       SourceScope::BuiltIn,
					builtin:     true,
				});
			}
		}
		Self::freeze(rules)
	}

	/// Freezes authored/package winners and optional bundled fallbacks. Exact
	/// blocks already present in any other prompt source are omitted.
	pub fn from_records(
		records: &[Arc<CapabilityRecord>],
		settings: &RulebookSettings,
		other_prompt_blocks: &[Str],
	) -> Self {
		let mut claimed = BTreeSet::new();
		let mut exact = other_prompt_blocks.iter().cloned().collect::<BTreeSet<_>>();
		let mut rules = Vec::new();
		for record in records {
			let CapabilityPayload::Rules(rule) = &record.payload else {
				continue;
			};
			if settings.blocked.contains(&rule.name)
				|| !claimed.insert(rule.name.clone())
				|| !exact.insert(rule.content.clone())
			{
				continue;
			}
			rules.push(ActiveRule {
				declaration: rule.clone(),
				source:      record.provenance.source.source_id.clone(),
				scope:       record.provenance.source.scope,
				builtin:     false,
			});
		}
		if settings.builtins_enabled {
			for asset in BUILTIN_RULES {
				if settings.blocked.contains(asset.name) || claimed.contains(asset.name) {
					continue;
				}
				let Ok(rule) = parse_static(
					asset.name,
					PathBuf::from(format!("builtin://rules/{}.md", asset.name)),
					asset.source,
				) else {
					continue;
				};
				if !exact.insert(rule.content.clone()) {
					continue;
				}
				claimed.insert(rule.name.clone());
				rules.push(ActiveRule {
					declaration: rule,
					source:      Str::from("builtin-defaults"),
					scope:       SourceScope::BuiltIn,
					builtin:     true,
				});
			}
		}
		Self::freeze(rules)
	}

	/// Freezes already lowered rules.
	pub fn freeze(rules: Vec<ActiveRule>) -> Self {
		let by_name = rules
			.iter()
			.enumerate()
			.map(|(index, rule)| (rule.declaration.name.clone(), index))
			.collect();
		let sticky = rules
			.iter()
			.enumerate()
			.filter_map(|(index, rule)| rule.declaration.always_apply.then_some(index))
			.collect::<Vec<_>>()
			.into();
		let indexed = rules
			.iter()
			.enumerate()
			.filter_map(|(index, rule)| {
				(!rule.declaration.always_apply && rule.declaration.description.is_some())
					.then_some(index)
			})
			.collect::<Vec<_>>()
			.into();
		let ttsr = rules
			.iter()
			.enumerate()
			.filter_map(|(index, rule)| {
				(!rule.declaration.conditions.is_empty() || !rule.declaration.ast_conditions.is_empty())
					.then_some(index)
			})
			.collect::<Vec<_>>()
			.into();
		Self { ordered: rules.into(), by_name: Arc::new(by_name), sticky, indexed, ttsr }
	}

	/// Every active rule in deterministic precedence order.
	pub fn all(&self) -> &[ActiveRule] {
		&self.ordered
	}

	/// Sticky always-apply partition.
	pub fn sticky(&self) -> impl Iterator<Item = &ActiveRule> {
		self.sticky.iter().map(|index| &self.ordered[*index])
	}

	/// Description-indexed on-demand partition.
	pub fn indexed(&self) -> impl Iterator<Item = &ActiveRule> {
		self.indexed.iter().map(|index| &self.ordered[*index])
	}

	/// TTSR trigger partition.
	pub fn ttsr(&self) -> impl Iterator<Item = &ActiveRule> {
		self.ttsr.iter().map(|index| &self.ordered[*index])
	}

	/// Looks up a frozen rule.
	pub fn get(&self, name: &str) -> Option<&ActiveRule> {
		self.by_name.get(name).map(|index| &self.ordered[*index])
	}

	/// Resolves frozen whole-body content for `rule://<name>`.
	pub fn resolve_body(&self, name: &str) -> Option<&str> {
		self.get(name).map(|rule| rule.declaration.content.as_str())
	}

	/// Synthesizes scoped sticky `RULES.md` contributions for a target path.
	/// User rules render before project rules only when precedence already
	/// placed them there; exact content is never emitted twice.
	pub fn sticky_rules_markdown(&self, target_path: &str) -> Str {
		let mut output = String::new();
		for rule in self
			.sticky()
			.filter(|rule| applies_to(&rule.declaration, target_path))
		{
			if !output.is_empty() {
				output.push_str("\n\n");
			}
			output.push_str("<!-- RULES.md: ");
			output.push_str(&rule.declaration.path.to_string_lossy());
			output.push_str(" -->\n");
			output.push_str(rule.declaration.content.as_str());
		}
		Str::from(output)
	}
}

/// Compiles the frozen discovery winners into the agent stream matcher.
pub fn ttsr_registry(snapshot: &RuleSnapshot) -> (TtsrRegistry, Vec<TtsrCompileError>) {
	let mut authored = Vec::new();
	let mut builtins = Vec::new();
	for active in snapshot.ttsr() {
		let declaration = &active.declaration;
		let rule = TtsrRule {
			name:           declaration.name.clone(),
			content:        declaration.content.clone(),
			conditions:     declaration.conditions.clone(),
			ast_conditions: declaration.ast_conditions.clone(),
			scopes:         declaration.scopes.clone(),
			globs:          declaration.globs.clone(),
			interrupt_mode: declaration.interrupt_mode.map(|mode| match mode {
				RuleInterruptMode::Never => TtsrInterruptMode::Never,
				RuleInterruptMode::ProseOnly => TtsrInterruptMode::ProseOnly,
				RuleInterruptMode::ToolOnly => TtsrInterruptMode::ToolOnly,
				RuleInterruptMode::Always => TtsrInterruptMode::Always,
			}),
		};
		if active.builtin {
			builtins.push(rule);
		} else {
			authored.push(rule);
		}
	}
	TtsrRegistry::from_layers(TtsrSettings::default(), authored, builtins)
}

/// Projects active rules into immutable prompt inputs without re-reading their
/// declaration files.
pub fn prompt_inputs(snapshot: &RuleSnapshot) -> Arc<[PromptNamedInput]> {
	snapshot
		.all()
		.iter()
		.map(|rule| PromptNamedInput {
			id:      rule.declaration.name.clone(),
			origin:  Str::from(format!("rule://{}", rule.declaration.name)),
			content: rule.declaration.content.clone(),
		})
		.collect::<Vec<_>>()
		.into()
}

/// Read-only `rule://` resolver over one immutable session snapshot.
pub struct RuleResolver {
	snapshot: Arc<RuleSnapshot>,
	lines:    LineOffsetCache,
}

impl RuleResolver {
	/// Creates a resolver which cannot observe rule winner or file changes after
	/// this call.
	pub fn new(snapshot: Arc<RuleSnapshot>) -> Self {
		Self { snapshot, lines: LineOffsetCache::default() }
	}
}

impl Resolve for RuleResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let name = resource.trim_matches('/');
		let rule = self.snapshot.get(name).ok_or_else(|| Fault::Source {
			message: Str::from(format!("rule resource not found: {name}")),
		})?;
		let bytes = CowBytes::from(rule.declaration.content.as_bytes().to_vec());
		let ParsedSelector::Lines { ranges, .. } = selector else {
			return Ok(bytes);
		};
		if ranges.len() == 1 {
			return self
				.lines
				.slice(name, &bytes, ranges[0])
				.map(CowBytes::into_owned)
				.map_err(|error| Fault::Invalid { message: Str::from(error.to_string()) });
		}
		let mut output = Vec::new();
		for range in ranges {
			output.extend_from_slice(
				&self
					.lines
					.slice(name, &bytes, *range)
					.map_err(|error| Fault::Invalid { message: Str::from(error.to_string()) })?,
			);
		}
		Ok(CowBytes::from(output))
	}

	async fn path(&self, resource: &str) -> Result<Option<Str>, Fault> {
		let name = resource.trim_matches('/');
		let rule = self.snapshot.get(name).ok_or_else(|| Fault::Source {
			message: Str::from(format!("rule resource not found: {name}")),
		})?;
		let path = fs::canonicalize(&rule.declaration.path).map_err(|_| Fault::Source {
			message: Str::from(format!("rule resource not found: {name}")),
		})?;
		let uri = Url::from_file_path(path).map_err(|()| Fault::Invalid {
			message: Str::from("rule path cannot be represented as a file URI"),
		})?;
		Ok(Some(Str::from(uri.to_string())))
	}

	async fn list(
		&self,
		resource: &str,
		max_entries: usize,
		max_bytes: usize,
	) -> Result<ResourceList, Fault> {
		if !resource.trim_matches('/').is_empty() {
			return Err(Fault::Invalid {
				message: Str::from("rule resources can only be listed at the scheme root"),
			});
		}
		let mut entries = Vec::new();
		let mut bytes: usize = 0;
		for rule in self.snapshot.all() {
			let uri = format!("rule://{}", rule.declaration.name);
			if entries.len() == max_entries || bytes.saturating_add(uri.len()) > max_bytes {
				return Ok(ResourceList { entries, truncated: true });
			}
			bytes += uri.len();
			entries.push(ResourceEntry {
				uri:       Str::from(uri),
				name:      rule.declaration.name.clone(),
				directory: false,
				size:      rule.declaration.content.len() as u64,
			});
		}
		Ok(ResourceList { entries, truncated: false })
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		let mut matches = self
			.snapshot
			.all()
			.iter()
			.filter_map(|rule| {
				Some(ResourceCompletion {
					value:       Str::from(format!("rule://{}", rule.declaration.name)),
					description: rule.declaration.description.clone().unwrap_or_default(),
					score:       fuzzy_score(query, &rule.declaration.name)?,
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
/// Manifest capability required to invalidate an extension prompt
/// contribution.
pub const PROMPTS_INVALIDATE_CAPABILITY: &str = "prompts.invalidate";

/// Prompt-head rejection for an authenticated invalidation.
#[derive(Debug, Error)]
pub enum PromptInvalidationError {
	/// The CONTROL connection was replaced.
	#[error("prompt invalidation authority belongs to a stale connection generation")]
	StaleGeneration,
	/// The extension lacks the exact manifest capability.
	#[error("prompt invalidation capability was not granted")]
	Capability,
	/// The callback phase cannot mutate prompt generations.
	#[error("prompt invalidation is illegal in the current callback phase")]
	Phase,
	/// Slot does not exist in the authoritative declaration table.
	#[error("unknown prompt slot")]
	UnknownSlot,
	/// Frozen prompt slots cannot change after activation.
	#[error("frozen prompt slot cannot be invalidated")]
	FrozenSlot,
	/// The extension has no contribution in the requested slot.
	#[error("extension does not own a contribution in the prompt slot")]
	NotOwner,
	/// Prompt-head generation space was exhausted.
	#[error("prompt-slot generation exhausted")]
	GenerationExhausted,
	/// The live extension contribution could not be refreshed.
	#[error("prompt contribution refresh failed")]
	Refresh(#[source] omp_envd::exthost::PromptDispatchError),
}

impl PartialEq for PromptInvalidationError {
	fn eq(&self, other: &Self) -> bool {
		std::mem::discriminant(self) == std::mem::discriminant(other)
	}
}

impl Eq for PromptInvalidationError {}

impl PromptInvalidationError {
	fn protocol(&self) -> ControlProtocolError {
		let code = match self {
			Self::StaleGeneration => "StaleGeneration",
			Self::Capability | Self::NotOwner => "PermissionDenied",
			Self::Phase => "InvalidPhase",
			Self::UnknownSlot => "UnknownSlot",
			Self::FrozenSlot => "SlotClassConflict",
			Self::GenerationExhausted | Self::Refresh(_) => "PromptInvalidationFailed",
		};
		ControlProtocolError::new(code, Str::from(self.to_string()))
	}
}

/// Existing prompt-head boundary. The returned generation must be the
/// generation subsequently consumed by prompt assembly and cache keys.
#[async_trait]
pub trait PromptHeadAuthority: Send + Sync + 'static {
	/// Invalidates one extension-owned contribution in the live prompt head.
	async fn invalidate(
		&self,
		extension: &str,
		session_generation: u64,
		slot: &str,
	) -> Result<u64, PromptInvalidationError>;
}

/// Authoritative `omp.prompts.invalidate` owner bound to one connection.
pub struct PromptControlOwner {
	identity: Arc<ControlConnectionIdentity>,
	head:     Arc<dyn PromptHeadAuthority>,
}

impl PromptControlOwner {
	/// Binds the prompt head to one authenticated extension generation.
	pub fn new(
		identity: Arc<ControlConnectionIdentity>,
		head: Arc<dyn PromptHeadAuthority>,
	) -> Self {
		Self { identity, head }
	}

	fn validate(
		&self,
		context: &control::ControlRequestContext,
	) -> Result<(), PromptInvalidationError> {
		let connection = &context.connection;
		if connection.extension != self.identity.extension
			|| connection.artifact_digest != self.identity.artifact_digest
			|| connection.host_generation != self.identity.host_generation
			|| connection.session_generation != self.identity.session_generation
			|| connection.capabilities != self.identity.capabilities
		{
			return Err(PromptInvalidationError::StaleGeneration);
		}
		let invocation = context
			.invocation
			.as_ref()
			.ok_or(PromptInvalidationError::Phase)?;
		if invocation.lifecycle != omp_core::LifecyclePhase::Active || invocation.phase.is_terminal()
		{
			return Err(PromptInvalidationError::Phase);
		}
		Ok(())
	}

	fn slot(arguments: &serde_json::Map<String, Value>) -> Result<&str, PromptInvalidationError> {
		arguments
			.get("slot")
			.and_then(Value::as_str)
			.filter(|slot| !slot.is_empty())
			.ok_or(PromptInvalidationError::UnknownSlot)
	}
}

#[async_trait]
impl ControlAuthority for PromptControlOwner {
	fn handles(&self, operation: &str) -> bool {
		operation == "omp.prompts.invalidate"
	}

	fn authorize(
		&self,
		context: &control::ControlRequestContext,
		operation: &str,
		arguments: &serde_json::Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		if operation != "omp.prompts.invalidate" {
			return Err(ControlProtocolError::new("UnknownOperation", "unknown prompts operation"));
		}
		self.validate(context).map_err(|error| error.protocol())?;
		Self::slot(arguments).map_err(|error| error.protocol())?;
		Ok(())
	}

	async fn request(
		&self,
		context: ControlRequestContext,
		operation: Str,
		arguments: serde_json::Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		self.authorize(&context, operation.as_str(), &arguments)?;
		let slot = Self::slot(&arguments).map_err(|error| error.protocol())?;
		let generation = self
			.head
			.invalidate(self.identity.extension.as_str(), self.identity.session_generation, slot)
			.await
			.map_err(|error| error.protocol())?;
		Ok(Value::from(generation))
	}

	async fn effect(
		&self,
		context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		self.validate(&context).map_err(|error| error.protocol())?;
		Err(ControlProtocolError::new(
			"UnsupportedEffect",
			"prompt authority accepts invalidation requests only",
		))
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicU64, Ordering};

	use serde_json::Map;

	use super::*;

	fn rule(name: &str, always: bool, description: Option<&str>, condition: &[&str]) -> ActiveRule {
		ActiveRule {
			declaration: RulePayload {
				name:           Str::from(name),
				path:           PathBuf::from(format!("{name}.md")),
				content:        Str::from(name),
				globs:          Vec::new(),
				always_apply:   always,
				description:    description.map(Str::from),
				conditions:     condition.iter().copied().map(Str::from).collect(),
				ast_conditions: Vec::new(),
				scopes:         Vec::new(),
				interrupt_mode: None,
			},
			source:      Str::from("test"),
			scope:       SourceScope::Project,
			builtin:     false,
		}
	}

	#[test]
	fn partitions_are_stable_and_rule_bodies_are_frozen() {
		let snapshot = RuleSnapshot::freeze(vec![
			rule("sticky", true, None, &[]),
			rule("indexed", false, Some("x"), &[]),
			rule("ttsr", false, None, &["x"]),
		]);
		assert_eq!(snapshot.sticky().count(), 1);
		assert_eq!(snapshot.indexed().count(), 1);
		assert_eq!(snapshot.ttsr().count(), 1);
		assert_eq!(snapshot.resolve_body("sticky"), Some("sticky"));
	}

	#[test]
	fn bundled_pack_is_complete_and_blockable() {
		assert_eq!(BUILTIN_RULES.len(), 27);
		let settings = RulebookSettings {
			builtins_enabled: true,
			blocked:          BTreeSet::from([Str::from(BUILTIN_RULES[0].name)]),
		};
		let snapshot = RuleSnapshot::from_records(&[], &settings, &[]);
		assert_eq!(snapshot.all().len(), 26);
	}
	struct PromptHead(AtomicU64);

	#[async_trait]
	impl PromptHeadAuthority for PromptHead {
		async fn invalidate(
			&self,
			extension: &str,
			session_generation: u64,
			slot: &str,
		) -> Result<u64, PromptInvalidationError> {
			assert_eq!(extension, "fixture.extension");
			assert_eq!(session_generation, 11);
			assert_eq!(slot, "memory");
			Ok(self.0.fetch_add(1, Ordering::AcqRel) + 1)
		}
	}

	#[tokio::test]
	async fn prompt_invalidation_uses_the_generation_consumed_by_the_head() {
		use omp_core::{InvocationPhase, LifecyclePhase, Principal, sf};
		use omp_envd::exthost::control::{
			ControlAuthority, ControlConnectionIdentity, ControlInvocationAuthority,
			ControlRequestContext,
		};

		let identity = Arc::new(ControlConnectionIdentity {
			extension:          sf!("fixture.extension"),
			principal:          Principal::new(sf!("fixture"), sf!("Fixture")),
			artifact_digest:    sf!("sha256:fixture"),
			layer:              sf!("project"),
			tier:               sf!("trusted"),
			trust:              sf!("trusted"),
			host_generation:    7,
			session_generation: 11,
			capabilities:       Arc::new(BTreeSet::from([sf!("prompts.invalidate")])),
		});
		let context = ControlRequestContext {
			connection: Arc::clone(&identity),
			request_id: 1,
			invocation: Some(ControlInvocationAuthority {
				invocation:        sf!("call-1"),
				phase:             InvocationPhase::Open,
				session:           sf!("session-1"),
				turn:              None,
				event:             None,
				call:              None,
				device:            None,
				effects:           Box::new([]),
				place_kind:        sf!("host"),
				lifecycle:         LifecyclePhase::Active,
				roots:             Box::new([]),
				remote:            false,
				has_ui:            true,
				headless:          false,
				settings:          Map::new(),
				secret_settings:   Box::new([]),
				data:              None,
				direct_filesystem: None,
			}),
		};
		let owner = PromptControlOwner::new(identity, Arc::new(PromptHead(AtomicU64::new(3))));
		let generation = ControlAuthority::request(
			&owner,
			context.clone(),
			sf!("omp.prompts.invalidate"),
			Map::from_iter([("slot".to_owned(), Value::String("memory".to_owned()))]),
		)
		.await
		.expect("prompt head accepts owned mutable slot");
		assert_eq!(generation, Value::from(4));
	}
}
