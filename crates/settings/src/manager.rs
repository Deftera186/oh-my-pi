//! Production settings authority and layering manager.

use std::{
	collections::BTreeMap,
	env,
	path::{Path, PathBuf},
	slice,
	sync::Arc,
};

use parking_lot::{Mutex, RwLock};

use super::{migrate, migrate::MigrationError};
use crate::{
	DomainRevision, DynamicOption, FieldDescriptor, LayerNormalizer, OptionProvider, Revision,
	SettingScope, SettingsDomain, SettingsSnapshot, SnapshotError, SnapshotPublisher, Subscription,
	ValidationError, deep_merge,
	io::{
		DocumentMutation, QuarantineDiagnostic, SettingsIoError, mutate_document, read_document,
		read_or_quarantine,
	},
	registered_domains,
};

/// Selected writable native settings layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationScope {
	/// User/profile `config.toml`.
	Global,
	/// Exact project `.omp/config.toml`.
	Project,
	/// Process-local, non-persistent override.
	Runtime,
}

impl MutationScope {
	const fn schema_scope(self) -> SettingScope {
		match self {
			Self::Global => SettingScope::Global,
			Self::Project => SettingScope::Project,
			Self::Runtime => SettingScope::Runtime,
		}
	}
}

/// Native source paths used by the settings authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsPaths {
	/// User/profile writable TOML settings file.
	pub global:   PathBuf,
	/// Optional exact-project writable TOML settings file.
	pub project:  Option<PathBuf>,
	/// Ordered read-only TOML or YAML overlays.
	pub overlays: Vec<PathBuf>,
}
/// One reflected row consumed by the settings overlay.
#[derive(Clone, Debug)]
pub struct SettingsEditorField {
	/// Owning runtime domain.
	pub domain:     &'static str,
	/// Type-owned field contract.
	pub descriptor: FieldDescriptor,
	/// Current merged value, masked when secret.
	pub value:      Option<toml::Value>,
	/// Options resolved at query time.
	pub options:    Arc<[DynamicOption]>,
	/// Whether the field condition currently holds.
	pub visible:    bool,
}

/// One of the ten stable settings-overlay panels.
#[derive(Clone, Debug)]
pub struct SettingsEditorPanel {
	/// Stable panel identifier.
	pub id:     &'static str,
	/// Descriptor-ordered fields.
	pub fields: Vec<SettingsEditorField>,
}

impl SettingsPaths {
	/// Resolves the standard user and exact-project paths plus
	/// `OMP_CONFIG_FILES` overlays.
	pub fn discover(data_dir: &Path, project_root: Option<&Path>) -> Self {
		let overlays = env::var_os("OMP_CONFIG_FILES")
			.map(|value| env::split_paths(&value).collect())
			.unwrap_or_default();
		let project = project_root.map(|root| root.join(".omp/config.toml"));
		Self { global: data_dir.join("config.toml"), project, overlays }
	}
}

/// Sole production writer for reflected native settings.
pub struct SettingsManager {
	paths:       SettingsPaths,
	runtime:     RwLock<toml::Table>,
	snapshot:    RwLock<Arc<SettingsSnapshot>>,
	publisher:   SnapshotPublisher,
	mutation:    Mutex<()>,
	read_only:   bool,
	diagnostics: RwLock<Vec<QuarantineDiagnostic>>,
}

impl SettingsManager {
	/// Loads and validates a writable production authority.
	pub fn open(paths: SettingsPaths) -> Result<Self, SettingsManagerError> {
		let preflight = read_or_quarantine(&paths.global)?;
		if let Some(data_dir) = paths.global.parent() {
			migrate::migrate_legacy_settings(data_dir)?;
		}
		let manager = Self::load(paths, false)?;
		if let Some(diagnostic) = preflight.quarantine {
			report_quarantine(&diagnostic);
			manager.diagnostics.write().push(diagnostic);
		}
		Ok(manager)
	}

	/// Loads a read-only authority without migration or mutation rights.
	pub fn open_read_only(paths: SettingsPaths) -> Result<Self, SettingsManagerError> {
		Self::load(paths, true)
	}

	/// Constructs a filesystem-isolated authority from a merged document.
	pub fn isolated(document: toml::Table) -> Result<Self, SettingsManagerError> {
		validate_document(&document)?;
		let snapshot = SettingsSnapshot::isolated_document(document);
		Ok(Self {
			paths:       SettingsPaths {
				global:   PathBuf::new(),
				project:  None,
				overlays: Vec::new(),
			},
			runtime:     RwLock::new(toml::Table::new()),
			snapshot:    RwLock::new(Arc::new(snapshot)),
			publisher:   SnapshotPublisher::default(),
			mutation:    Mutex::new(()),
			read_only:   true,
			diagnostics: RwLock::new(Vec::new()),
		})
	}

	fn load(paths: SettingsPaths, read_only: bool) -> Result<Self, SettingsManagerError> {
		validate_registry()?;
		let mut diagnostics = Vec::new();
		let global_toml = if read_only {
			read_document(&paths.global)?
		} else {
			let read = read_or_quarantine(&paths.global)?;
			if let Some(diagnostic) = read.quarantine {
				report_quarantine(&diagnostic);
				diagnostics.push(diagnostic);
			}
			read.document
		};
		let mut global = load_yaml_compatibility(&paths.global)?;
		deep_merge(&mut global, global_toml);
		let project_toml = match &paths.project {
			Some(path) if read_only => read_document(path)?,
			Some(path) => {
				let read = read_or_quarantine(path)?;
				if let Some(diagnostic) = read.quarantine {
					report_quarantine(&diagnostic);
					diagnostics.push(diagnostic);
				}
				read.document
			},
			None => toml::Table::new(),
		};
		let mut project = paths
			.project
			.as_deref()
			.map(load_yaml_compatibility)
			.transpose()?
			.unwrap_or_default();
		deep_merge(&mut project, project_toml);
		let overlays = load_overlays(&paths.overlays)?;
		let document = compose_document(global, project, overlays, toml::Table::new());
		validate_document(&document)?;
		let revisions = registered_domains()
			.into_iter()
			.map(|domain| (domain.name, DomainRevision(1)))
			.collect();
		let snapshot = if read_only {
			SettingsSnapshot::read_only(document)
		} else {
			SettingsSnapshot::persistent(Revision(1), revisions, document)
		};
		Ok(Self {
			paths,
			runtime: RwLock::new(toml::Table::new()),
			snapshot: RwLock::new(Arc::new(snapshot)),
			publisher: SnapshotPublisher::default(),
			mutation: Mutex::new(()),
			read_only,
			diagnostics: RwLock::new(diagnostics),
		})
	}

	/// Returns the current immutable snapshot synchronously.
	pub fn snapshot(&self) -> Arc<SettingsSnapshot> {
		Arc::clone(&self.snapshot.read())
	}

	/// Returns startup/write quarantine diagnostics without secret values.
	pub fn diagnostics(&self) -> Vec<QuarantineDiagnostic> {
		self.diagnostics.read().clone()
	}

	/// Subscribes an owning runtime to its domain revision.
	pub fn subscribe<D: SettingsDomain>(&self) -> Subscription {
		let current = self.snapshot().domain_revision(D::DOMAIN);
		self.publisher.subscribe(D::DOMAIN, current)
	}

	fn record_quarantine(&self, diagnostic: QuarantineDiagnostic) {
		report_quarantine(&diagnostic);
		self.diagnostics.write().push(diagnostic);
	}

	/// Finds a reflected field by exact dotted path.
	pub fn field(&self, path: &str) -> Option<FieldDescriptor> {
		registered_domains()
			.into_iter()
			.flat_map(|domain| domain.fields.iter().copied())
			.find(|field| field.path == path)
	}

	/// Iterates reflected fields in stable domain/order/path order.
	pub fn fields(&self) -> Vec<FieldDescriptor> {
		let mut fields = registered_domains()
			.into_iter()
			.flat_map(|domain| domain.fields.iter().copied())
			.collect::<Vec<_>>();
		fields.sort_unstable_by_key(|field| field.path);
		fields.dedup_by_key(|field| field.path);
		fields.sort_unstable_by(|left, right| {
			left
				.order
				.cmp(&right.order)
				.then_with(|| left.path.cmp(right.path))
		});
		fields
	}

	/// Builds a current, secret-safe editor model from typed descriptors.
	pub fn editor_panels(&self) -> Vec<SettingsEditorPanel> {
		const IDS: &[&str] = &[
			"appearance",
			"model",
			"interaction",
			"context",
			"files_shell",
			"tools_tasks",
			"orchestration",
			"providers",
			"extensions",
			"lifecycle",
		];
		let snapshot = self.snapshot();
		let mut panels = IDS
			.iter()
			.map(|id| SettingsEditorPanel { id, fields: Vec::new() })
			.collect::<Vec<_>>();
		for domain in registered_domains() {
			let target = panels
				.iter_mut()
				.find(|panel| panel.id == panel_for_domain(domain.name))
				.expect("domain panel exists");
			let mut fields = domain.fields.to_vec();
			fields.sort_unstable_by_key(|field| (field.order, field.path));
			for descriptor in fields {
				let raw = value_at(snapshot.document(), descriptor.path).cloned();
				let value = if descriptor.secret {
					raw.as_ref()
						.map(|_| toml::Value::String("********".to_owned()))
				} else {
					raw
				};
				let options = match descriptor.options {
					Some(OptionProvider::Static(options)) => options
						.iter()
						.map(|option| DynamicOption {
							value:       Arc::from(option.value),
							label:       Arc::from(option.label),
							description: option.description.map(Arc::from),
						})
						.collect::<Vec<_>>()
						.into(),
					Some(OptionProvider::Dynamic(provider)) => provider(),
					None => Arc::from([]),
				};
				let visible = descriptor.condition.is_none_or(|condition| {
					value_at(snapshot.document(), condition.field)
						.is_some_and(|value| scalar_spelling(value) == condition.equals)
				});
				target.fields.push(SettingsEditorField {
					domain: domain.name,
					descriptor,
					value,
					options,
					visible,
				});
			}
		}
		panels
	}

	/// Sets one reflected value. Persistent mutations are serialized, lock and
	/// re-read the target, then merge only this whole field before replacement.
	pub async fn set(
		&self,
		scope: MutationScope,
		path: &str,
		raw: &str,
	) -> Result<Arc<SettingsSnapshot>, SettingsManagerError> {
		self.set_sync(scope, path, raw)
	}

	/// Synchronous mutation entry point for non-async application callbacks.
	pub fn set_sync(
		&self,
		scope: MutationScope,
		path: &str,
		raw: &str,
	) -> Result<Arc<SettingsSnapshot>, SettingsManagerError> {
		let field = self
			.field(path)
			.ok_or_else(|| SettingsManagerError::UnsupportedKey { path: path.to_owned() })?;
		if scope != MutationScope::Runtime && !field.scopes.contains(&scope.schema_scope()) {
			return Err(SettingsManagerError::UnsupportedScope { path: field.path, scope });
		}
		let value = field.parse(raw)?;
		self.apply(scope, DocumentMutation::Set { path: field.path, value })
	}

	/// Removes one reflected value from the selected layer.
	pub async fn unset(
		&self,
		scope: MutationScope,
		path: &str,
	) -> Result<Arc<SettingsSnapshot>, SettingsManagerError> {
		self.unset_sync(scope, path)
	}

	/// Synchronous unset entry point for non-async application callbacks.
	pub fn unset_sync(
		&self,
		scope: MutationScope,
		path: &str,
	) -> Result<Arc<SettingsSnapshot>, SettingsManagerError> {
		let field = self
			.field(path)
			.ok_or_else(|| SettingsManagerError::UnsupportedKey { path: path.to_owned() })?;
		if scope != MutationScope::Runtime && !field.scopes.contains(&scope.schema_scope()) {
			return Err(SettingsManagerError::UnsupportedScope { path: field.path, scope });
		}
		self.apply(scope, DocumentMutation::Unset { path: field.path })
	}

	fn apply(
		&self,
		scope: MutationScope,
		mutation: DocumentMutation,
	) -> Result<Arc<SettingsSnapshot>, SettingsManagerError> {
		if self.read_only {
			return Err(SettingsManagerError::ReadOnly);
		}
		let _guard = self.mutation.lock();
		let mut candidate = self.snapshot().document().clone();
		apply_runtime(&mut candidate, &mutation);
		validate_document(&candidate)?;
		match scope {
			MutationScope::Global => {
				let read = mutate_document(&self.paths.global, slice::from_ref(&mutation))?;
				if let Some(diagnostic) = read.quarantine {
					self.record_quarantine(diagnostic);
				}
			},
			MutationScope::Project => {
				let path = self
					.paths
					.project
					.as_ref()
					.ok_or(SettingsManagerError::NoProjectScope)?;
				let read = mutate_document(path, slice::from_ref(&mutation))?;
				if let Some(diagnostic) = read.quarantine {
					self.record_quarantine(diagnostic);
				}
			},
			MutationScope::Runtime => apply_runtime(&mut self.runtime.write(), &mutation),
		}
		self.reload()
	}

	fn reload(&self) -> Result<Arc<SettingsSnapshot>, SettingsManagerError> {
		let global_read = read_or_quarantine(&self.paths.global)?;
		if let Some(diagnostic) = global_read.quarantine {
			self.record_quarantine(diagnostic);
		}
		let mut global = load_yaml_compatibility(&self.paths.global)?;
		deep_merge(&mut global, global_read.document);
		let mut project = toml::Table::new();
		if let Some(path) = self.paths.project.as_deref() {
			project = load_yaml_compatibility(path)?;
			let read = read_or_quarantine(path)?;
			if let Some(diagnostic) = read.quarantine {
				self.record_quarantine(diagnostic);
			}
			deep_merge(&mut project, read.document);
		}
		let overlays = load_overlays(&self.paths.overlays)?;
		let document = compose_document(global, project, overlays, self.runtime.read().clone());
		validate_document(&document)?;

		let previous = self.snapshot();
		let mut revisions = BTreeMap::new();
		for domain in registered_domains() {
			let changed = domain.fields.iter().any(|field| {
				value_at(previous.document(), field.path) != value_at(&document, field.path)
			});
			let old = previous.domain_revision(domain.name);
			let revision = if changed {
				DomainRevision(old.0 + 1)
			} else {
				old
			};
			revisions
				.entry(domain.name)
				.and_modify(|current| {
					if revision > *current {
						*current = revision;
					}
				})
				.or_insert(revision);
		}
		let snapshot = Arc::new(SettingsSnapshot::persistent(
			Revision(previous.revision().0 + 1),
			revisions,
			document,
		));
		*self.snapshot.write() = Arc::clone(&snapshot);
		self.publisher.publish(Arc::clone(&snapshot));
		Ok(snapshot)
	}
}

fn report_quarantine(diagnostic: &QuarantineDiagnostic) {
	tracing::warn!(
		path = %diagnostic.path.display(),
		backup_path = %diagnostic.backup_path.display(),
		line = diagnostic.line,
		column = diagnostic.column,
		"quarantined corrupt native settings TOML",
	);
}

fn value_at<'a>(document: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
	let mut segments = path.split('.');
	let mut value = document.get(segments.next()?)?;
	for segment in segments {
		value = value.as_table()?.get(segment)?;
	}
	Some(value)
}

fn scalar_spelling(value: &toml::Value) -> String {
	match value {
		toml::Value::String(value) => value.clone(),
		toml::Value::Integer(value) => value.to_string(),
		toml::Value::Float(value) => value.to_string(),
		toml::Value::Boolean(value) => value.to_string(),
		_ => value.to_string(),
	}
}

fn panel_for_domain(domain: &str) -> &'static str {
	match domain {
		"tui" | "chat_ui" | "appearance" => "appearance",
		"model" | "catalog" | "inference" => "model",
		"interaction" | "voice" | "collaboration" => "interaction",
		"agent" | "memory" | "compaction" => "context",
		"files" | "shell" | "lsp" | "eval" => "files_shell",
		"tools" | "tasks" | "approvals" => "tools_tasks",
		"orchestration" | "subagent" => "orchestration",
		"providers" | "search" => "providers",
		"extensions" | "mcp" => "extensions",
		_ => "lifecycle",
	}
}

/// Applies every linked layer normalizer to one persisted document.
fn normalize_layer(document: &mut toml::Table) {
	for normalizer in inventory::iter::<LayerNormalizer> {
		normalizer.apply(document);
	}
}

fn compose_document(
	mut global: toml::Table,
	mut project: toml::Table,
	mut overlays: Vec<toml::Table>,
	mut runtime: toml::Table,
) -> toml::Table {
	normalize_layer(&mut global);
	normalize_layer(&mut project);
	for overlay in &mut overlays {
		normalize_layer(overlay);
	}
	normalize_layer(&mut runtime);
	let mut document = toml::Table::new();
	for domain in registered_domains() {
		deep_merge(&mut document, (domain.default_document)());
	}
	deep_merge(&mut document, global);
	deep_merge(&mut document, project);
	for overlay in overlays {
		deep_merge(&mut document, overlay);
	}
	deep_merge(&mut document, runtime);
	document
}

fn validate_registry() -> Result<(), SettingsManagerError> {
	let mut paths = BTreeMap::new();
	for domain in registered_domains() {
		for field in domain.fields {
			if let Some(previous) = paths.insert(field.path, *field)
				&& (previous.kind != field.kind
					|| previous.scopes != field.scopes
					|| previous.secret != field.secret)
			{
				return Err(SettingsManagerError::ConflictingField { path: field.path });
			}
		}
	}
	Ok(())
}

fn validate_document(document: &toml::Table) -> Result<(), SettingsManagerError> {
	for domain in registered_domains() {
		(domain.validate)(document)?;
	}
	Ok(())
}

fn load_overlays(paths: &[PathBuf]) -> Result<Vec<toml::Table>, SettingsManagerError> {
	paths
		.iter()
		.map(|path| read_document(path).map_err(Into::into))
		.collect()
}
/// Loads the first canonical Pi YAML sibling below a writable TOML layer.
///
/// `config.yml` is canonical and shadows `config.yaml` when both exist. The
/// writable TOML layer is merged afterward, so native edits always win while
/// untouched YAML keys remain effective.
fn load_yaml_compatibility(path: &Path) -> Result<toml::Table, SettingsManagerError> {
	let Some(parent) = path.parent() else {
		return Ok(toml::Table::new());
	};
	for name in ["config.yml", "config.yaml"] {
		let candidate = parent.join(name);
		if candidate.is_file() {
			return read_document(&candidate).map_err(Into::into);
		}
	}
	Ok(toml::Table::new())
}

fn apply_runtime(document: &mut toml::Table, mutation: &DocumentMutation) {
	fn set(document: &mut toml::Table, path: &str, value: toml::Value) {
		let mut segments = path.split('.').peekable();
		let mut current = document;
		while let Some(segment) = segments.next() {
			if segments.peek().is_none() {
				current.insert(segment.to_owned(), value);
				return;
			}
			let value = current
				.entry(segment.to_owned())
				.or_insert_with(|| toml::Value::Table(toml::Table::new()));
			if !value.is_table() {
				*value = toml::Value::Table(toml::Table::new());
			}
			current = value.as_table_mut().expect("table established above");
		}
	}
	fn unset(document: &mut toml::Table, path: &str) {
		let mut segments = path.split('.').peekable();
		let mut current = document;
		while let Some(segment) = segments.next() {
			if segments.peek().is_none() {
				current.remove(segment);
				return;
			}
			let Some(next) = current.get_mut(segment).and_then(toml::Value::as_table_mut) else {
				return;
			};
			current = next;
		}
	}
	match mutation {
		DocumentMutation::Set { path, value } => set(document, path, value.clone()),
		DocumentMutation::Unset { path } => unset(document, path),
	}
}

/// Settings authority failure.
#[derive(Debug, thiserror::Error)]
pub enum SettingsManagerError {
	/// Native persistence failed.
	#[error(transparent)]
	Io(#[from] SettingsIoError),
	/// Legacy migration failed.
	#[error(transparent)]
	Migration(#[from] MigrationError),
	/// Typed schema validation failed.
	#[error(transparent)]
	Validation(#[from] ValidationError),
	/// Typed snapshot projection failed.
	#[error(transparent)]
	Projection {
		#[from]
		/// Snapshot conversion failure returned by a linked settings domain.
		source: SnapshotError,
	},
	/// Linked domain fragments disagreed about a shared field contract.
	#[error("conflicting linked settings field {path}")]
	ConflictingField {
		/// Shared schema path claimed with incompatible field metadata.
		path: &'static str,
	},
	/// No domain owns the requested path.
	#[error("unsupported settings key {path}")]
	UnsupportedKey {
		/// Requested dotted key that no linked settings domain owns.
		path: String,
	},
	/// The field cannot be written at the selected scope.
	#[error("setting {path} cannot be written at scope {scope:?}")]
	UnsupportedScope {
		/// Owned schema path that rejects the requested persistence scope.
		path:  &'static str,
		/// Project or user scope selected for the rejected mutation.
		scope: MutationScope,
	},
	/// Project scope was requested without a project root.
	#[error("project settings scope is unavailable")]
	NoProjectScope,
	/// Mutation was attempted through a read-only authority.
	#[error("settings authority is read-only")]
	ReadOnly,
}

#[cfg(test)]
mod tests {
	use std::{collections::BTreeMap, fs};

	use serde::{Deserialize, Serialize};

	use super::*;
	use crate::{
		FieldDescriptor, LayerNormalizer, SettingKind, SettingScope, SettingsDomain, SettingsSnapshot,
	};

	const PROBE_SCOPES: &[SettingScope] =
		&[SettingScope::Global, SettingScope::Project, SettingScope::Runtime];

	/// Prefix-less aggregate standing in for an application core domain.
	#[derive(Clone, Debug, Default, Serialize, Deserialize)]
	struct CoreProbe {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		probe_value: Option<String>,
	}

	impl SettingsDomain for CoreProbe {
		const DOMAIN: &'static str = "probe-core";
		const FIELDS: &'static [FieldDescriptor] = &[FieldDescriptor {
			path:        "probe_value",
			label:       "Default Model",
			description: "Model selected by default.",
			kind:        SettingKind::String,
			scopes:      PROBE_SCOPES,
			order:       0,
			options:     None,
			condition:   None,
			secret:      false,
		}];
		const PREFIX: Option<&'static str> = None;
	}

	inventory::submit! {
		crate::DomainRegistration::of::<CoreProbe>()
	}

	/// Table-shaped domain whose values accept booleans on disk.
	#[derive(Clone, Debug, Default, Serialize, Deserialize)]
	struct ToggleProbe {
		#[serde(default)]
		toggles: BTreeMap<String, String>,
	}

	impl SettingsDomain for ToggleProbe {
		const DOMAIN: &'static str = "probe";
		const FIELDS: &'static [FieldDescriptor] = &[];
	}

	inventory::submit! {
		crate::DomainRegistration::of::<ToggleProbe>()
	}

	fn normalize_probe_toggles(document: &mut toml::Table) {
		let Some(probe) = document
			.get_mut("probe")
			.and_then(toml::Value::as_table_mut)
		else {
			return;
		};
		let Some(toggles) = probe.get_mut("toggles").and_then(toml::Value::as_table_mut) else {
			return;
		};
		for (_, value) in toggles.iter_mut() {
			if let toml::Value::Boolean(enabled) = value {
				*value = toml::Value::String(if *enabled { "on" } else { "off" }.to_owned());
			}
		}
	}

	inventory::submit! {
		LayerNormalizer::new(normalize_probe_toggles)
	}

	#[test]
	fn domain_subscription_wakes_on_owning_revision() {
		let tree = tempfile::tempdir().expect("tree");
		let manager = SettingsManager::open(SettingsPaths {
			global:   tree.path().join("config.toml"),
			project:  None,
			overlays: Vec::new(),
		})
		.expect("manager");
		let mut subscription = manager.subscribe::<CoreProbe>();
		manager
			.set_sync(MutationScope::Runtime, "probe_value", "demo/model")
			.expect("mutation");
		let snapshot = subscription.recv().expect("revision");
		assert_eq!(
			snapshot
				.project::<CoreProbe>()
				.expect("projection")
				.get()
				.probe_value
				.as_deref(),
			Some("demo/model"),
		);
	}

	#[test]
	fn layering_precedence_is_defaults_global_project_overlay_runtime() {
		let tree = tempfile::tempdir().expect("tree");
		let global = tree.path().join("global/config.toml");
		let project = tree.path().join("project/.omp/config.toml");
		let overlay = tree.path().join("overlay.toml");
		fs::create_dir_all(global.parent().expect("global parent")).expect("global dir");
		fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
		fs::write(&global, "probe_value = 'global'").expect("global");
		fs::write(&project, "probe_value = 'project'").expect("project");
		fs::write(&overlay, "probe_value = 'overlay'").expect("overlay");
		let manager = SettingsManager::open(SettingsPaths {
			global,
			project: Some(project),
			overlays: vec![overlay],
		})
		.expect("manager");
		let projected = manager
			.snapshot()
			.project::<CoreProbe>()
			.expect("projection");
		assert_eq!(projected.get().probe_value.as_deref(), Some("overlay"));
		manager
			.set_sync(MutationScope::Runtime, "probe_value", "runtime")
			.expect("override");
		let projected = manager
			.snapshot()
			.project::<CoreProbe>()
			.expect("projection");
		assert_eq!(projected.get().probe_value.as_deref(), Some("runtime"));
	}

	#[test]
	fn discovery_binds_only_the_exact_project_directory() {
		let tree = tempfile::tempdir().expect("tree");
		let project = tree.path().join("repo/package");
		fs::create_dir_all(tree.path().join("repo/.omp")).expect("ancestor native dir");
		fs::create_dir_all(&project).expect("exact project");
		let paths = SettingsPaths::discover(tree.path(), Some(&project));
		assert_eq!(paths.project, Some(project.join(".omp/config.toml")));
	}

	#[test]
	fn canonical_yaml_and_yaml_overlay_participate_in_layering() {
		let tree = tempfile::tempdir().expect("tree");
		let data = tree.path().join("data");
		let project = tree.path().join("project");
		fs::create_dir_all(&data).expect("global dir");
		fs::create_dir_all(project.join(".omp")).expect("project dir");
		fs::write(data.join("config.yml"), "probe_value: global-yaml\n").expect("global yaml");
		fs::write(project.join(".omp/config.yml"), "probe_value: project-yaml\n")
			.expect("project yaml");
		let overlay = tree.path().join("overlay.yaml");
		fs::write(&overlay, "probe_value: overlay-yaml\n").expect("overlay yaml");
		let mut paths = SettingsPaths::discover(&data, Some(&project));
		paths.overlays.push(overlay);
		let manager = SettingsManager::open(paths).expect("manager");
		assert_eq!(
			manager
				.snapshot()
				.project::<CoreProbe>()
				.expect("projection")
				.get()
				.probe_value
				.as_deref(),
			Some("overlay-yaml"),
		);
	}

	#[test]
	fn pi_root_prompt_keys_migrate_per_layer_before_native_precedence() {
		let tree = tempfile::tempdir().expect("tree");
		let data = tree.path().join("data");
		let project = tree.path().join("project");
		fs::create_dir_all(&data).expect("global dir");
		fs::create_dir_all(project.join(".omp")).expect("project dir");
		fs::write(
			data.join("config.yml"),
			"includeModelInPrompt: false\nincludeWorkspaceTree: true\n",
		)
		.expect("global yaml");
		fs::write(data.join("config.toml"), "[prompt]\nincludeModelInPrompt = true\n")
			.expect("global native");
		let global = SettingsManager::open(SettingsPaths::discover(&data, None)).expect("global");
		let snapshot = global.snapshot();
		let prompt = snapshot
			.document()
			.get("prompt")
			.and_then(toml::Value::as_table)
			.expect("global prompt");
		assert_eq!(
			prompt
				.get("includeModelInPrompt")
				.and_then(toml::Value::as_bool),
			Some(true)
		);
		assert_eq!(
			prompt
				.get("includeWorkspaceTree")
				.and_then(toml::Value::as_bool),
			Some(true)
		);

		fs::write(
			project.join(".omp/config.yml"),
			"includeModelInPrompt: false\nincludeWorkspaceTree: false\n",
		)
		.expect("project yaml");
		fs::write(project.join(".omp/config.toml"), "[prompt]\nincludeWorkspaceTree = true\n")
			.expect("project native");
		let layered =
			SettingsManager::open(SettingsPaths::discover(&data, Some(&project))).expect("layered");
		let snapshot = layered.snapshot();
		let prompt = snapshot
			.document()
			.get("prompt")
			.and_then(toml::Value::as_table)
			.expect("layered prompt");
		assert_eq!(
			prompt
				.get("includeModelInPrompt")
				.and_then(toml::Value::as_bool),
			Some(false)
		);
		assert_eq!(
			prompt
				.get("includeWorkspaceTree")
				.and_then(toml::Value::as_bool),
			Some(true)
		);
	}

	#[test]
	fn registered_normalizers_run_before_layer_merging() {
		let global =
			toml::from_str("[probe.toggles]\nleft = true\nright = true\n").expect("global layer");
		let project = toml::from_str("[probe.toggles]\nleft = false\n").expect("project layer");
		let document = compose_document(global, project, Vec::new(), toml::Table::new());
		let snapshot = SettingsSnapshot::isolated_document(document);
		let settings = snapshot.project::<ToggleProbe>().expect("probe projection");
		assert_eq!(settings.get().toggles.get("left").map(String::as_str), Some("off"));
		assert_eq!(settings.get().toggles.get("right").map(String::as_str), Some("on"));
	}
}
