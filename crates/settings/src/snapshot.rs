//! Immutable revisioned settings snapshots and path-scoped resolution.

use std::{collections::BTreeMap, marker::PhantomData, path::Path, sync::Arc};

use flume::Receiver;
use globset::Glob;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use toml::{de, ser};

use crate::{SettingsDomain, ValidationError, schema};

/// Monotonic whole-snapshot revision.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Revision(pub u64);

/// Monotonic revision of one settings domain.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct DomainRevision(pub u64);

/// Persistence capability associated with a snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotMode {
	/// Production snapshot backed by native files.
	Persistent,
	/// Snapshot loaded from files without mutation rights.
	ReadOnly,
	/// Test/embedded snapshot with no filesystem source.
	Isolated,
}

/// An immutable fully layered settings document.
#[derive(Clone, Debug)]
pub struct SettingsSnapshot {
	revision:         Revision,
	domain_revisions: Arc<BTreeMap<&'static str, DomainRevision>>,
	document:         Arc<toml::Table>,
	mode:             SnapshotMode,
}

impl SettingsSnapshot {
	/// Constructs a production snapshot from an already composed document.
	pub fn persistent(
		revision: Revision,
		domain_revisions: BTreeMap<&'static str, DomainRevision>,
		document: toml::Table,
	) -> Self {
		Self {
			revision,
			domain_revisions: Arc::new(domain_revisions),
			document: Arc::new(document),
			mode: SnapshotMode::Persistent,
		}
	}

	/// Constructs a read-only snapshot.
	pub fn read_only(document: toml::Table) -> Self {
		Self {
			revision:         Revision(0),
			domain_revisions: Arc::default(),
			document:         Arc::new(document),
			mode:             SnapshotMode::ReadOnly,
		}
	}

	/// Constructs a filesystem-isolated snapshot from a merged root document.
	pub fn isolated_document(document: toml::Table) -> Self {
		Self {
			revision:         Revision(0),
			domain_revisions: Arc::default(),
			document:         Arc::new(document),
			mode:             SnapshotMode::Isolated,
		}
	}

	/// Constructs a filesystem-isolated snapshot for tests or embedding.
	pub fn isolated<D: SettingsDomain>(domain: D) -> Result<Self, SnapshotError> {
		domain.validate()?;
		let domain_document = match toml::Value::try_from(domain)? {
			toml::Value::Table(document) => document,
			_ => return Err(SnapshotError::RootNotTable),
		};
		let document = if let Some(prefix) = D::PREFIX {
			let mut document = toml::Table::new();
			document.insert(prefix.to_owned(), toml::Value::Table(domain_document));
			document
		} else {
			domain_document
		};
		let mut revisions = BTreeMap::new();
		revisions.insert(D::DOMAIN, DomainRevision(0));
		Ok(Self {
			revision:         Revision(0),
			domain_revisions: Arc::new(revisions),
			document:         Arc::new(document),
			mode:             SnapshotMode::Isolated,
		})
	}

	/// Returns the whole-document revision.
	pub const fn revision(&self) -> Revision {
		self.revision
	}

	/// Returns the construction mode.
	pub const fn mode(&self) -> SnapshotMode {
		self.mode
	}

	/// Returns one domain's current revision, or zero before its first change.
	pub fn domain_revision(&self, domain: &str) -> DomainRevision {
		self
			.domain_revisions
			.get(domain)
			.copied()
			.unwrap_or_default()
	}

	/// Borrows the merged TOML document for reflected reads.
	pub fn document(&self) -> &toml::Table {
		&self.document
	}

	/// Decodes and validates one typed runtime projection.
	pub fn project<D: SettingsDomain>(&self) -> Result<TypedProjection<D>, SnapshotError> {
		let value = schema::projection_value::<D>(&self.document);
		let domain = value.try_into::<D>()?;
		domain.validate()?;
		Ok(TypedProjection {
			value:    Arc::new(domain),
			revision: self.domain_revision(D::DOMAIN),
			_marker:  PhantomData,
		})
	}
}

/// A typed, immutable projection and the domain revision it represents.
#[derive(Clone, Debug)]
pub struct TypedProjection<D> {
	value:    Arc<D>,
	revision: DomainRevision,
	_marker:  PhantomData<fn() -> D>,
}

impl<D> TypedProjection<D> {
	/// Borrows the projected settings.
	pub fn get(&self) -> &D {
		&self.value
	}

	/// Clones the shared projected settings handle.
	pub fn shared(&self) -> Arc<D> {
		Arc::clone(&self.value)
	}

	/// Returns the domain revision represented by this projection.
	pub const fn revision(&self) -> DomainRevision {
		self.revision
	}
}

/// Broadcasts new snapshots to domain-filtered subscribers.
#[derive(Debug, Default)]
pub struct SnapshotPublisher {
	subscribers: Mutex<Vec<DomainSubscriber>>,
}

#[derive(Debug)]
struct DomainSubscriber {
	domain:    &'static str,
	last_sent: DomainRevision,
	sender:    flume::Sender<Arc<SettingsSnapshot>>,
	drain:     Receiver<Arc<SettingsSnapshot>>,
}

impl SnapshotPublisher {
	/// Creates a subscription that wakes only when `domain` advances.
	pub fn subscribe(&self, domain: &'static str, current: DomainRevision) -> Subscription {
		let (sender, receiver) = flume::bounded(1);
		self.subscribers.lock().push(DomainSubscriber {
			domain,
			last_sent: current,
			sender,
			drain: receiver.clone(),
		});
		Subscription { domain, current, receiver }
	}

	/// Publishes a committed snapshot only to domains that advanced. Each
	/// subscriber retains at most the latest pending snapshot.
	pub fn publish(&self, snapshot: Arc<SettingsSnapshot>) {
		self.subscribers.lock().retain_mut(|subscriber| {
			let revision = snapshot.domain_revision(subscriber.domain);
			if revision <= subscriber.last_sent {
				return !subscriber.sender.is_disconnected();
			}
			subscriber.last_sent = revision;
			match subscriber.sender.try_send(Arc::clone(&snapshot)) {
				Ok(()) => true,
				Err(flume::TrySendError::Disconnected(_)) => false,
				Err(flume::TrySendError::Full(latest)) => {
					let _ = subscriber.drain.try_recv();
					subscriber.sender.try_send(latest).is_ok() || !subscriber.sender.is_disconnected()
				},
			}
		});
	}
}

/// A blocking/async stream of changes for one owning runtime.
#[derive(Debug)]
pub struct Subscription {
	domain:   &'static str,
	current:  DomainRevision,
	receiver: Receiver<Arc<SettingsSnapshot>>,
}

impl Subscription {
	/// Waits synchronously for the next revision of this domain.
	pub fn recv(&mut self) -> Result<Arc<SettingsSnapshot>, flume::RecvError> {
		loop {
			let snapshot = self.receiver.recv()?;
			let revision = snapshot.domain_revision(self.domain);
			if revision > self.current {
				self.current = revision;
				return Ok(snapshot);
			}
		}
	}

	/// Waits asynchronously for the next revision of this domain.
	pub async fn recv_async(&mut self) -> Result<Arc<SettingsSnapshot>, flume::RecvError> {
		loop {
			let snapshot = self.receiver.recv_async().await?;
			let revision = snapshot.domain_revision(self.domain);
			if revision > self.current {
				self.current = revision;
				return Ok(snapshot);
			}
		}
	}
}

/// One ordered glob-selected array contribution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScopedValues<T> {
	/// Glob matched against the normalized project-relative path.
	pub glob:   String,
	/// Values contributed when the glob matches.
	#[serde(default)]
	pub values: Vec<T>,
}

/// Resolves all matching contributions in declaration order.
pub fn resolve_path_scoped<'a, T>(
	entries: &'a [ScopedValues<T>],
	path: &Path,
) -> Result<Vec<&'a T>, SnapshotError> {
	let normalized = path.to_string_lossy().replace('\\', "/");
	let mut resolved = Vec::new();
	for entry in entries {
		let matcher = Glob::new(&entry.glob)
			.map_err(|source| SnapshotError::InvalidGlob { source })?
			.compile_matcher();
		if matcher.is_match(&normalized) {
			resolved.extend(entry.values.iter());
		}
	}
	Ok(resolved)
}

/// Deep-merges `overlay` into `base`; tables recurse and every other value is
/// replaced atomically.
pub fn deep_merge(base: &mut toml::Table, overlay: toml::Table) {
	for (key, value) in overlay {
		match (base.get_mut(&key), value) {
			(Some(toml::Value::Table(base_table)), toml::Value::Table(overlay_table)) => {
				deep_merge(base_table, overlay_table);
			},
			(_, value) => {
				base.insert(key, value);
			},
		}
	}
}

/// Immutable snapshot construction and projection failures.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
	/// A typed domain serialized to a non-table root.
	#[error("settings domain must serialize to a TOML table")]
	RootNotTable,
	/// TOML serialization failed.
	#[error(transparent)]
	Serialize(#[from] ser::Error),
	/// Typed projection decoding failed.
	#[error(transparent)]
	Decode(#[from] de::Error),
	/// Domain validation failed.
	#[error(transparent)]
	Validation(#[from] ValidationError),
	/// A path-scope glob was malformed.
	#[error("path-scoped settings glob is invalid")]
	InvalidGlob {
		/// Glob parser failure.
		#[source]
		source: globset::Error,
	},
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::FieldDescriptor;

	#[derive(Clone, Debug, Default, Deserialize, Serialize)]
	struct Demo {
		enabled: bool,
	}

	impl SettingsDomain for Demo {
		const DOMAIN: &'static str = "demo";
		const FIELDS: &'static [FieldDescriptor] = &[];
	}

	#[test]
	fn isolated_projection_is_typed() {
		let snapshot = SettingsSnapshot::isolated(Demo { enabled: true }).expect("snapshot");
		assert!(
			snapshot
				.project::<Demo>()
				.expect("projection")
				.get()
				.enabled
		);
		assert_eq!(snapshot.mode(), SnapshotMode::Isolated);
	}

	#[test]
	fn scoped_arrays_preserve_declaration_order() {
		let entries =
			vec![ScopedValues { glob: "src/**".to_owned(), values: vec![1, 2] }, ScopedValues {
				glob:   "**/*.rs".to_owned(),
				values: vec![3],
			}];
		assert_eq!(resolve_path_scoped(&entries, Path::new("src/lib.rs")).expect("resolve"), [
			&1, &2, &3
		]);
	}
	#[test]
	fn domain_publisher_filters_irrelevant_changes_and_coalesces_latest() {
		fn snapshot(whole: u64, demo: u64) -> Arc<SettingsSnapshot> {
			Arc::new(SettingsSnapshot::persistent(
				Revision(whole),
				BTreeMap::from([("demo", DomainRevision(demo))]),
				toml::Table::new(),
			))
		}

		let publisher = SnapshotPublisher::default();
		let subscription = publisher.subscribe("demo", DomainRevision(1));
		publisher.publish(snapshot(2, 1));
		assert!(subscription.receiver.try_recv().is_err());
		publisher.publish(snapshot(3, 2));
		publisher.publish(snapshot(4, 3));
		assert_eq!(
			subscription
				.receiver
				.try_recv()
				.expect("latest pending snapshot")
				.revision(),
			Revision(4),
		);
		assert!(subscription.receiver.try_recv().is_err());
	}
}
