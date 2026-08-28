//! Production prompt invalidation authority and its consumed generation state.

use std::{collections::BTreeMap, str::FromStr, sync::Arc};

use async_trait::async_trait;
use omp_agent::{
	BandHash, CachedContribution, PromptBands, PromptError, PromptSource, SlotAssembler, SlotClass,
	SlotDecl, SlotId, SlotRegistration,
};
use omp_core::{Hash32, Str, hash32::Hasher};
use omp_envd::{
	exthost::{
		PromptContributionProvider, PromptContributionRecord, PromptPullContext, PromptSlotBinding,
	},
	worker::ExtHostSpec,
};
use omp_scribe::Props;
use parking_lot::{Mutex, RwLock};

use crate::rulebook::{PromptHeadAuthority, PromptInvalidationError};

/// Shared prompt-slot generations consumed by prompt assembly and cache keys.
///
/// A prompt consumer must read the generation for the same session and slot
/// while assembling its prompt snapshot. The value returned by
/// [`PromptHeadAuthority::invalidate`] is the value subsequently observed here.
#[derive(Clone, Default)]
pub struct PromptGenerationStore {
	generations: Arc<Mutex<BTreeMap<(u64, SlotId), u64>>>,
}

impl PromptGenerationStore {
	/// Returns the current generation for one session-scoped prompt slot.
	///
	/// A slot which has not yet been invalidated is at generation zero.
	pub fn generation(&self, session_generation: u64, slot: SlotId) -> u64 {
		self
			.generations
			.lock()
			.get(&(session_generation, slot))
			.copied()
			.unwrap_or(0)
	}

	fn advance(
		&self,
		session_generation: u64,
		slot: SlotId,
	) -> Result<u64, PromptInvalidationError> {
		let mut generations = self.generations.lock();
		let generation = generations.entry((session_generation, slot)).or_default();
		let next = generation
			.checked_add(1)
			.ok_or(PromptInvalidationError::GenerationExhausted)?;
		*generation = next;
		Ok(next)
	}

	fn fold_bands(&self, declarations: &[SlotDecl], bands: &mut [BandHash; 4]) {
		let generations = self.generations.lock();
		for (class, band) in bands.iter_mut().enumerate() {
			let mut relevant = generations.iter().filter(|((_, slot), _)| {
				declarations
					.iter()
					.any(|declaration| declaration.slot == *slot && declaration.class as usize == class)
			});
			let Some((first_key, first_generation)) = relevant.next() else {
				continue;
			};
			let mut hasher = Hash32::hasher();
			hasher.update(b"omp.prompt-slot-generation.v1");
			hasher.update(band.as_bytes());
			hash_generation(&mut hasher, first_key, *first_generation);
			for (key, generation) in relevant {
				hash_generation(&mut hasher, key, *generation);
			}
			*band = hasher.finalize().into();
		}
	}
}

fn hash_generation(hasher: &mut Hasher, key: &(u64, SlotId), generation: u64) {
	hasher.update(&key.0.to_le_bytes());
	hasher.update(&[key.1 as u8]);
	hasher.update(&generation.to_le_bytes());
}

/// Production authority over the prompt declarations used by assembly.
///
/// The declarations are retained as the single authoritative declaration
/// table. Invalidation validates directly against them instead of maintaining
/// a second slot or ownership registry.
pub struct ProductionPromptHead {
	declarations:  Arc<[SlotDecl]>,
	generations:   PromptGenerationStore,
	provider:      RwLock<Option<Arc<dyn PromptContributionProvider>>>,
	context:       RwLock<Option<PromptPullContext>>,
	contributions: Arc<RwLock<BTreeMap<(Str, Str), PromptContributionRecord>>>,
}

impl ProductionPromptHead {
	/// Creates an authority over the declarations supplied to prompt assembly.
	pub fn new(declarations: Vec<SlotDecl>) -> Self {
		Self {
			declarations:  declarations.into(),
			generations:   PromptGenerationStore::default(),
			provider:      RwLock::new(None),
			context:       RwLock::new(None),
			contributions: Arc::new(RwLock::new(BTreeMap::new())),
		}
	}

	/// Creates an authority from the sealed declarations admitted for extension
	/// hosts.
	///
	/// Static manifests retain one row per owning extension and prompt slot.
	/// Class and priority properties are used when present; otherwise the
	/// canonical extension slot catalog supplies the class and priority zero.
	/// A malformed declared class fails closed as frozen.
	pub fn from_extension_specs(specs: &[ExtHostSpec]) -> Self {
		let declarations = specs
			.iter()
			.flat_map(|spec| {
				spec
					.manifest
					.static_declarations()
					.prompt_slots
					.iter()
					.filter_map(move |declaration| {
						let name = declaration
							.properties
							.get("slot")
							.and_then(serde_json::Value::as_str)
							.or_else(|| (!declaration.key.is_empty()).then_some(declaration.key.as_str()))
							.unwrap_or(declaration.id.as_str());
						let slot = slot_id(name)?;
						let class = match declaration
							.properties
							.get("class")
							.or_else(|| declaration.properties.get("cls"))
						{
							Some(class) => class
								.as_str()
								.and_then(slot_class)
								.unwrap_or(SlotClass::Frozen),
							None => extension_slot_class(slot),
						};
						let priority = declaration
							.properties
							.get("priority")
							.and_then(serde_json::Value::as_i64)
							.and_then(|priority| i16::try_from(priority).ok())
							.unwrap_or(0);
						Some(SlotDecl { slot, class, owner: spec.key.extension().clone(), priority })
					})
			})
			.collect();
		Self::new(declarations)
	}

	/// Installs the live extension provider used for activation and
	/// invalidation pulls.
	pub fn bind_provider(&self, provider: Arc<dyn PromptContributionProvider>) {
		*self.provider.write() = Some(provider);
	}

	/// Pulls every eager prompt renderer before the first model request.
	pub async fn activate(&self, context: PromptPullContext) -> Result<(), PromptInvalidationError> {
		*self.context.write() = Some(context.clone());
		let Some(provider) = self.provider.read().clone() else {
			return Ok(());
		};
		let bindings = provider.declarations();
		for binding in &bindings {
			self.validate_binding(binding)?;
		}
		let mut refreshed = Vec::with_capacity(bindings.len());
		for binding in &bindings {
			refreshed.push(
				provider
					.pull(binding, &context)
					.await
					.map_err(PromptInvalidationError::Refresh)?,
			);
		}
		let mut contributions = self.contributions.write();
		contributions.clear();
		for contribution in refreshed {
			contributions.insert(
				(contribution.binding.owner.clone(), contribution.binding.key.clone()),
				contribution,
			);
		}
		Ok(())
	}

	/// Installs one already-pulled contribution.
	///
	/// This is the synchronous seam used by embedded hosts which performed
	/// their eager pull during startup.
	pub fn install(
		&self,
		contribution: PromptContributionRecord,
	) -> Result<(), PromptInvalidationError> {
		self.validate_binding(&contribution.binding)?;
		self.contributions.write().insert(
			(contribution.binding.owner.clone(), contribution.binding.key.clone()),
			contribution,
		);
		Ok(())
	}

	fn validate_binding(&self, binding: &PromptSlotBinding) -> Result<(), PromptInvalidationError> {
		if self.declarations.iter().any(|declaration| {
			declaration.owner == binding.owner
				&& declaration.slot == binding.slot
				&& declaration.class == binding.class
				&& declaration.priority == binding.priority
		}) {
			Ok(())
		} else {
			Err(PromptInvalidationError::Refresh(omp_envd::exthost::PromptDispatchError::Undeclared))
		}
	}

	/// Returns the shared generation store prompt assembly and cache keys
	/// consume.
	pub fn generation_store(&self) -> PromptGenerationStore {
		self.generations.clone()
	}

	/// Wraps the assembled prompt source so accepted generations enter its
	/// semantic band hashes without changing the wire prompt items.
	///
	/// Production prompt sources must expose semantic bands. An unbanded source
	/// fails rather than silently accepting invalidations its cache key cannot
	/// consume.
	pub fn wrap_prompt_source(&self, source: Arc<dyn PromptSource>) -> Arc<dyn PromptSource> {
		Arc::new(GenerationPromptSource {
			source,
			declarations: Arc::clone(&self.declarations),
			generations: self.generations.clone(),
			contributions: Arc::clone(&self.contributions),
		})
	}
}

struct GenerationPromptSource {
	source:        Arc<dyn PromptSource>,
	declarations:  Arc<[SlotDecl]>,
	generations:   PromptGenerationStore,
	contributions: Arc<RwLock<BTreeMap<(Str, Str), PromptContributionRecord>>>,
}

impl PromptSource for GenerationPromptSource {
	fn render(&self, props: &Props) -> Result<Vec<omp_agent::Item>, PromptError> {
		let Some(bands) = self.banded_items_render(props)? else {
			return Err(PromptError::Source(
				"prompt contributions require a banded prompt source".into(),
			));
		};
		Ok(bands.into_items())
	}

	fn banded_items_render(&self, props: &Props) -> Result<Option<PromptBands>, PromptError> {
		let Some(mut bands) = self.source.banded_items_render(props)? else {
			return Err(PromptError::Source(
				"prompt contributions require a banded prompt source".into(),
			));
		};
		let registrations = self
			.contributions
			.read()
			.values()
			.map(|contribution| SlotRegistration {
				decl:   SlotDecl {
					slot:     contribution.binding.slot,
					class:    contribution.binding.class,
					owner:    contribution.binding.owner.clone(),
					priority: contribution.binding.priority,
				},
				source: Arc::new(CachedContribution::new(contribution.content.clone())),
			})
			.collect::<Vec<_>>();
		if !registrations.is_empty()
			&& let Some(extra) = SlotAssembler::new(registrations).banded_items_render(props)?
		{
			bands.append(extra);
		}
		self
			.generations
			.fold_bands(&self.declarations, &mut bands.hashes);
		Ok(Some(bands))
	}
}

#[async_trait]
impl PromptHeadAuthority for ProductionPromptHead {
	async fn invalidate(
		&self,
		extension: &str,
		session_generation: u64,
		slot: &str,
	) -> Result<u64, PromptInvalidationError> {
		let slot = slot_id(slot).ok_or(PromptInvalidationError::UnknownSlot)?;
		let mut frozen = false;
		let mut volatile = true;
		for declaration in self
			.declarations
			.iter()
			.filter(|declaration| declaration.slot == slot)
		{
			if declaration.owner.as_str() == extension {
				frozen |= declaration.class == SlotClass::Frozen;
				volatile &= declaration.class == SlotClass::Volatile;
			}
		}
		if frozen {
			return Err(PromptInvalidationError::FrozenSlot);
		}
		if volatile {
			return Ok(self.generations.generation(session_generation, slot));
		}
		let provider = self.provider.read().clone();
		if let Some(provider) = provider {
			let context = self.context.read().clone().ok_or_else(|| {
				PromptInvalidationError::Refresh(omp_envd::exthost::PromptDispatchError::MissingContext)
			})?;
			let bindings = provider
				.declarations()
				.into_iter()
				.filter(|binding| binding.owner.as_str() == extension && binding.slot == slot)
				.collect::<Vec<_>>();
			if bindings.is_empty() {
				return Err(PromptInvalidationError::Refresh(
					omp_envd::exthost::PromptDispatchError::Undeclared,
				));
			}
			let mut refreshed = Vec::with_capacity(bindings.len());
			for binding in &bindings {
				refreshed.push(
					provider
						.pull(binding, &context)
						.await
						.map_err(PromptInvalidationError::Refresh)?,
				);
			}
			let mut contributions = self.contributions.write();
			for contribution in refreshed {
				contributions.insert(
					(contribution.binding.owner.clone(), contribution.binding.key.clone()),
					contribution,
				);
			}
		}
		self.generations.advance(session_generation, slot)
	}
}

fn slot_id(slot: &str) -> Option<SlotId> {
	SlotId::from_str(slot).ok()
}
fn slot_class(class: &str) -> Option<SlotClass> {
	SlotClass::from_str(class).ok()
}

const fn extension_slot_class(slot: SlotId) -> SlotClass {
	match slot {
		SlotId::Runtime | SlotId::Workflow => SlotClass::Frozen,
		SlotId::Policy | SlotId::Skills | SlotId::Rules | SlotId::Guidance | SlotId::Workspace => {
			SlotClass::Stable
		},
		SlotId::Memory | SlotId::Standing => SlotClass::Epochal,
		SlotId::Recall | SlotId::Status => SlotClass::Volatile,
		SlotId::Conventions | SlotId::Role | SlotId::Tools | SlotId::Delivery => SlotClass::Frozen,
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use async_trait::async_trait;
	use omp_agent::{CanonicalPromptSource, PromptSource};
	use omp_core::sf;
	use omp_envd::exthost::{
		PromptContributionProvider, PromptContributionRecord, PromptDispatchError, PromptPullContext,
		PromptSlotBinding,
	};
	use omp_proto::thread::v1::{item, part};
	use parking_lot::RwLock;

	use super::*;

	struct Provider {
		binding: PromptSlotBinding,
		content: RwLock<Str>,
	}

	#[async_trait]
	impl PromptContributionProvider for Provider {
		fn declarations(&self) -> Vec<PromptSlotBinding> {
			vec![self.binding.clone()]
		}

		async fn pull(
			&self,
			binding: &PromptSlotBinding,
			_context: &PromptPullContext,
		) -> Result<PromptContributionRecord, PromptDispatchError> {
			Ok(PromptContributionRecord {
				binding:   binding.clone(),
				content:   self.content.read().clone(),
				truncated: false,
			})
		}
	}

	fn binding() -> PromptSlotBinding {
		PromptSlotBinding {
			key:          sf!("dev.example.policy"),
			owner:        sf!("dev.example"),
			slot:         SlotId::Policy,
			class:        SlotClass::Stable,
			priority:     10,
			budget_bytes: 64,
		}
	}

	fn head() -> ProductionPromptHead {
		let binding = binding();
		ProductionPromptHead::new(vec![SlotDecl {
			slot:     binding.slot,
			class:    binding.class,
			owner:    binding.owner,
			priority: binding.priority,
		}])
	}

	fn context() -> PromptPullContext {
		PromptPullContext {
			session_id:     sf!("session"),
			model:          sf!("model"),
			provider:       sf!("provider"),
			context_window: 128_000,
			epoch:          0,
			cwd:            sf!("/workspace"),
			roots:          vec![sf!("/workspace")],
			vcs_branch:     None,
			vcs_commit:     None,
			is_subagent:    false,
			agent_kind:     None,
		}
	}

	fn band_text(source: &dyn PromptSource, class: SlotClass) -> String {
		let bands = source
			.banded_items_render(&Props::default())
			.expect("render")
			.expect("banded");
		bands.items[class as usize]
			.iter()
			.filter_map(|item| match item.kind.as_ref()? {
				item::Kind::Message(message) => Some(message),
				_ => None,
			})
			.flat_map(|message| &message.parts)
			.filter_map(|part| match part.kind.as_ref()? {
				part::Kind::Text(text) => Some(text.as_str()),
				_ => None,
			})
			.collect()
	}

	#[test]
	fn installed_contribution_renders_only_in_declared_band() {
		let head = head();
		head
			.install(PromptContributionRecord {
				binding:   binding(),
				content:   sf!("EXTENSION POLICY"),
				truncated: false,
			})
			.expect("install");
		let base = CanonicalPromptSource
			.banded_items_render(&Props::default())
			.expect("base render")
			.expect("base bands");
		let source = head.wrap_prompt_source(Arc::new(CanonicalPromptSource));
		let contributed = source
			.banded_items_render(&Props::default())
			.expect("contributed render")
			.expect("contributed bands");
		assert_eq!(
			contributed.hashes[SlotClass::Frozen as usize],
			base.hashes[SlotClass::Frozen as usize],
		);
		assert!(band_text(source.as_ref(), SlotClass::Stable).contains("EXTENSION POLICY"));
		assert!(!band_text(source.as_ref(), SlotClass::Frozen).contains("EXTENSION POLICY"));
	}

	#[tokio::test]
	async fn invalidation_pulls_fresh_content_before_advancing_generation() {
		let head = head();
		let provider = Arc::new(Provider {
			binding: binding(),
			content: RwLock::new(sf!("PROMPT_SLOT_OLD_SENTINEL")),
		});
		head.bind_provider(provider.clone());
		head.activate(context()).await.expect("activate");
		let source = head.wrap_prompt_source(Arc::new(CanonicalPromptSource));
		assert!(band_text(source.as_ref(), SlotClass::Stable).contains("PROMPT_SLOT_OLD_SENTINEL"),);
		*provider.content.write() = sf!("PROMPT_SLOT_NEW_SENTINEL");
		assert_eq!(
			head
				.invalidate("dev.example", 7, "policy")
				.await
				.expect("invalidate"),
			1
		);
		let rendered = band_text(source.as_ref(), SlotClass::Stable);
		assert!(rendered.contains("PROMPT_SLOT_NEW_SENTINEL"));
		assert!(!rendered.contains("PROMPT_SLOT_OLD_SENTINEL"));
	}
}
