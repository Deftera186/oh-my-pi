//! Typed settings reflection and immutable snapshot primitives.

pub mod browser;
pub mod io;
pub mod manager;
pub mod migrate;
pub mod schema;
pub mod snapshot;
pub mod subscription;

pub use browser::BrowserSettings;
pub use inventory;
pub use schema::{
	Condition, DomainDescriptor, DomainRegistration, DynamicOption, FieldDescriptor, OptionProvider,
	SettingKind, SettingOption, SettingScope, SettingsDomain, ValidationError, registered_domains,
};
pub use snapshot::{
	DomainRevision, Revision, ScopedValues, SettingsSnapshot, SnapshotError, SnapshotMode,
	SnapshotPublisher, Subscription, TypedProjection, deep_merge, resolve_path_scoped,
};
/// Link-time hook normalizing one persisted layer before it is merged.
///
/// A domain whose accepted TOML shape is wider than its projected type — a
/// boolean where an enum is stored, a scalar where a table is stored —
/// registers a normalizer so every layer is canonicalized *before* layering,
/// keeping precedence independent of which shape each layer happened to use.
///
/// ```ignore
/// omp_settings::inventory::submit! {
///     omp_settings::LayerNormalizer::new(normalize_persisted_agent_overrides)
/// }
/// ```
pub struct LayerNormalizer {
	hook: fn(&mut toml::Table),
}

impl LayerNormalizer {
	/// Registers `hook` as a per-layer document normalizer.
	pub const fn new(hook: fn(&mut toml::Table)) -> Self {
		Self { hook }
	}

	/// Canonicalizes one persisted layer in place.
	pub fn apply(&self, document: &mut toml::Table) {
		(self.hook)(document);
	}
}

inventory::collect!(LayerNormalizer);
