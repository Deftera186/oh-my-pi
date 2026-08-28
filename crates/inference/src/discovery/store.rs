//! Versioned SQLite cache for secret-free model discovery and provider
//! lifecycle.

use std::{path::Path, time::Duration};

use omp_catalog::{DiscoveredModel, ProviderId};
use omp_core::Str;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension as _, params, types};
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

// Version 3 invalidates rows written before discovered partial pricing and
// provider-local transport isolation became part of the serialized overlay.
const SCHEMA_VERSION: i64 = 3;

/// Secret-free identity of one provider discovery cache namespace.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiscoveryCacheKey {
	/// Provider identity.
	pub provider:         ProviderId,
	/// Opaque credential/account affinity. Empty means provider-wide.
	pub credential_scope: Option<Str>,
}

impl DiscoveryCacheKey {
	/// Creates a provider-wide cache identity.
	pub fn provider(provider: impl Into<ProviderId>) -> Self {
		Self { provider: provider.into(), credential_scope: None }
	}

	/// Creates a credential-scoped identity from a non-secret stable affinity.
	pub fn credential(provider: impl Into<ProviderId>, affinity: impl Into<Str>) -> Self {
		Self { provider: provider.into(), credential_scope: Some(affinity.into()) }
	}

	fn scope(&self) -> &str {
		self.credential_scope.as_deref().unwrap_or_default()
	}
}

/// Provider discovery lifecycle persisted without credential material.
#[derive(
	Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum ProviderDiscoveryState {
	/// No probe is in flight.
	Idle,
	/// A process currently owns the probe.
	Probing,
	/// The endpoint failed without proving model absence.
	Failed,
	/// The endpoint rejected the currently leased principal.
	Unauthorized,
	/// A complete generation was published.
	Ready,
}

/// Durable, redaction-safe provider lifecycle annotation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderLifecycle {
	/// Provider identity.
	pub provider:       ProviderId,
	/// Current state.
	pub state:          ProviderDiscoveryState,
	/// Stable error classification, never provider response text.
	pub error_code:     Option<Str>,
	/// Last successful or failed observation time.
	pub observed_at_ms: u64,
	/// Advisory retry time. This is not authoritative availability evidence.
	pub retry_at_ms:    Option<u64>,
}

/// One fresh cached provider generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedDiscovery {
	/// Exact cache namespace.
	pub key:           DiscoveryCacheKey,
	/// Atomic generation number.
	pub generation:    u64,
	/// Secret-free normalized rows.
	pub rows:          Vec<DiscoveredModel>,
	/// Expiry time.
	pub expires_at_ms: u64,
}

/// SQLite-backed discovery cache.
pub struct DiscoveryStore {
	connection: Mutex<Connection>,
}

impl DiscoveryStore {
	/// Opens or creates a discovery cache and applies its versioned schema.
	pub fn open(path: &Path) -> Result<Self, DiscoveryStoreError> {
		let connection = Connection::open(path)?;
		connection.pragma_update(None, "journal_mode", "WAL")?;
		connection.execute_batch(
			"CREATE TABLE IF NOT EXISTS discovery_meta (
			  key TEXT PRIMARY KEY,
			  value INTEGER NOT NULL
			);
			 CREATE TABLE IF NOT EXISTS provider_lifecycle (
			  provider TEXT PRIMARY KEY,
			  state TEXT NOT NULL,
			  error_code TEXT,
			  observed_at_ms INTEGER NOT NULL,
			  retry_at_ms INTEGER
			);
			 CREATE TABLE IF NOT EXISTS discovered_models (
			  provider TEXT NOT NULL,
			  cache_scope TEXT NOT NULL DEFAULT '',
			  generation INTEGER NOT NULL,
			  ordinal INTEGER NOT NULL,
			  row_json BLOB NOT NULL,
			  expires_at_ms INTEGER NOT NULL,
			  PRIMARY KEY(provider, generation, ordinal)
			);
			 CREATE INDEX IF NOT EXISTS discovered_models_fresh
			   ON discovered_models(provider, expires_at_ms, generation);",
		)?;
		let has_scope = {
			let mut statement = connection.prepare("PRAGMA table_info(discovered_models)")?;
			let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
			let mut has_scope = false;
			for column in columns {
				has_scope |= column? == "cache_scope";
			}
			has_scope
		};
		if !has_scope {
			connection.execute(
				"ALTER TABLE discovered_models ADD COLUMN cache_scope TEXT NOT NULL DEFAULT ''",
				[],
			)?;
		}
		connection.execute_batch(
			"DROP INDEX IF EXISTS discovered_models_fresh;
			 CREATE INDEX discovered_models_fresh
			   ON discovered_models(provider, cache_scope, expires_at_ms, generation);",
		)?;
		let stored_version = connection
			.query_row("SELECT value FROM discovery_meta WHERE key='schema_version'", [], |row| {
				row.get::<_, i64>(0)
			})
			.optional()?;
		if stored_version != Some(SCHEMA_VERSION) {
			connection.execute_batch(
				"DELETE FROM discovered_models;
				 DELETE FROM provider_lifecycle;",
			)?;
		}
		connection.execute(
			"INSERT INTO discovery_meta(key, value) VALUES('schema_version', ?1)
			 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
			[SCHEMA_VERSION],
		)?;
		Ok(Self { connection: Mutex::new(connection) })
	}

	/// Atomically replaces one cache namespace's complete row generation and
	/// updates provider lifecycle state.
	pub fn publish(
		&self,
		key: &DiscoveryCacheKey,
		rows: &[DiscoveredModel],
		now_ms: u64,
		ttl: Duration,
	) -> Result<u64, DiscoveryStoreError> {
		let provider = &key.provider;
		if rows.iter().any(|row| row.provider != *provider) {
			return Err(DiscoveryStoreError::ProviderMismatch);
		}
		let expires_at_ms = now_ms.saturating_add(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX));
		let mut connection = self.connection.lock();
		let transaction = connection.transaction()?;
		let generation: u64 = transaction.query_row(
			"SELECT COALESCE(MAX(generation), 0) + 1 FROM discovered_models WHERE provider=?1",
			[provider.as_str()],
			|row| row.get(0),
		)?;
		for (ordinal, row) in rows.iter().enumerate() {
			let encoded = serde_json::to_vec(row)?;
			transaction.execute(
				"INSERT INTO discovered_models(
				   provider, cache_scope, generation, ordinal, row_json, expires_at_ms
				 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
				params![
					provider.as_str(),
					key.scope(),
					generation,
					ordinal as u64,
					encoded,
					expires_at_ms
				],
			)?;
		}
		transaction.execute(
			"DELETE FROM discovered_models
			 WHERE provider=?1 AND cache_scope=?2 AND generation<>?3",
			params![provider.as_str(), key.scope(), generation],
		)?;
		upsert_lifecycle(&transaction, &ProviderLifecycle {
			provider:       provider.to_owned(),
			state:          ProviderDiscoveryState::Ready,
			error_code:     None,
			observed_at_ms: now_ms,
			retry_at_ms:    None,
		})?;
		transaction.commit()?;
		Ok(generation)
	}

	/// Loads the latest unexpired generation after restart.
	pub fn load_fresh(
		&self,
		key: &DiscoveryCacheKey,
		now_ms: u64,
	) -> Result<Option<CachedDiscovery>, DiscoveryStoreError> {
		let connection = self.connection.lock();
		let generation: Option<(u64, u64)> = connection
			.query_row(
				"SELECT generation, MAX(expires_at_ms) FROM discovered_models
				 WHERE provider=?1 AND cache_scope=?2 AND expires_at_ms>?3
				 GROUP BY generation ORDER BY generation DESC LIMIT 1",
				params![key.provider.as_str(), key.scope(), now_ms],
				|row| Ok((row.get(0)?, row.get(1)?)),
			)
			.optional()?;
		let Some((generation, expires_at_ms)) = generation else {
			return Ok(None);
		};
		let mut statement = connection.prepare(
			"SELECT row_json FROM discovered_models
			 WHERE provider=?1 AND cache_scope=?2 AND generation=?3 ORDER BY ordinal",
		)?;
		let encoded = statement
			.query_map(params![key.provider.as_str(), key.scope(), generation], |row| {
				row.get::<_, Vec<u8>>(0)
			})?;
		let mut rows = Vec::new();
		for row in encoded {
			rows.push(serde_json::from_slice(&row?)?);
		}
		Ok(Some(CachedDiscovery { key: key.clone(), generation, rows, expires_at_ms }))
	}

	/// Persists a lifecycle/error transition separately from non-authoritative
	/// retry timing.
	pub fn set_lifecycle(&self, lifecycle: &ProviderLifecycle) -> Result<(), DiscoveryStoreError> {
		upsert_lifecycle(&self.connection.lock(), lifecycle)
	}

	/// Loads one provider lifecycle annotation.
	pub fn lifecycle(
		&self,
		provider: &ProviderId<str>,
	) -> Result<Option<ProviderLifecycle>, DiscoveryStoreError> {
		self
			.connection
			.lock()
			.query_row(
				"SELECT state, error_code, observed_at_ms, retry_at_ms FROM provider_lifecycle
				 WHERE provider=?1",
				[provider.as_str()],
				|row| {
					let state: String = row.get(0)?;
					let state = state.parse().map_err(|_| {
						rusqlite::Error::InvalidColumnType(0, "state".to_owned(), types::Type::Text)
					})?;
					Ok(ProviderLifecycle {
						provider: provider.to_owned(),
						state,
						error_code: row.get::<_, Option<String>>(1)?.map(Str::from),
						observed_at_ms: row.get(2)?,
						retry_at_ms: row.get(3)?,
					})
				},
			)
			.optional()
			.map_err(DiscoveryStoreError::from)
	}
}

fn upsert_lifecycle(
	connection: &Connection,
	lifecycle: &ProviderLifecycle,
) -> Result<(), DiscoveryStoreError> {
	connection.execute(
		"INSERT INTO provider_lifecycle(provider, state, error_code, observed_at_ms, retry_at_ms)
		 VALUES(?1, ?2, ?3, ?4, ?5)
		 ON CONFLICT(provider) DO UPDATE SET state=excluded.state, error_code=excluded.error_code,
		 observed_at_ms=excluded.observed_at_ms, retry_at_ms=excluded.retry_at_ms",
		params![
			lifecycle.provider.as_str(),
			<&'static str>::from(lifecycle.state),
			lifecycle.error_code.as_deref(),
			lifecycle.observed_at_ms,
			lifecycle.retry_at_ms,
		],
	)?;
	Ok(())
}

/// Discovery cache failure.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryStoreError {
	/// SQLite operation failed.
	#[error(transparent)]
	Sqlite(#[from] rusqlite::Error),
	/// Secret-free row serialization failed.
	#[error(transparent)]
	Json(#[from] serde_json::Error),
	/// A row was published under another provider.
	#[error("discovery row provider does not match its generation")]
	ProviderMismatch,
}

#[cfg(test)]
mod tests {
	use std::str;

	use omp_catalog::{
		ModelAvailability, ModelLimits, OperationBits, Price, PriceUnit, RouteId, WireModelId,
	};

	use super::*;

	fn row(provider: &ProviderId<str>) -> DiscoveredModel {
		DiscoveredModel {
			provider:              provider.to_owned(),
			route:                 RouteId::from("route"),
			wire_model:            WireModelId::from("model"),
			aliases:               Box::new([]),
			display_name:          None,
			declared_class:        None,
			declared_operations:   OperationBits::empty(),
			declared_capabilities: None,
			declared_limits:       Some(ModelLimits {
				context_window:        Some(4096),
				maximum_input_tokens:  None,
				maximum_output_tokens: None,
				maximum_batch:         None,
			}),
			declared_pricing:      Box::new([]),
			extended_context_mode: None,
			availability:          Some(ModelAvailability::Available),
			source:                Str::new_static("fixture"),
			observed_at_ms:        Some(1),
			updated_at_ms:         None,
			deprecated:            None,
		}
	}

	#[test]
	fn cache_survives_reopen_and_ttl_is_non_authoritative() {
		let directory = tempfile::tempdir().expect("directory");
		let path = directory.path().join("models.db");
		let provider = ProviderId::from("local");
		let key = DiscoveryCacheKey::provider(provider.clone());
		{
			let store = DiscoveryStore::open(&path).expect("open");
			let mut priced = row(&provider);
			priced.declared_pricing =
				vec![Price { unit: PriceUnit::MtokCacheRead, nanos_usd: 550_000_000 }, Price {
					unit:      PriceUnit::MtokCacheWrite,
					nanos_usd: 6_875_000_000,
				}]
				.into_boxed_slice();
			store
				.publish(&key, &[priced], 100, Duration::from_secs(1))
				.expect("publish");
		}
		let reopened = DiscoveryStore::open(&path).expect("reopen");
		let cached = reopened
			.load_fresh(&key, 500)
			.expect("load")
			.expect("fresh cache");
		assert_eq!(cached.rows.len(), 1);
		assert_eq!(cached.rows[0].declared_pricing.as_ref(), [
			Price { unit: PriceUnit::MtokCacheRead, nanos_usd: 550_000_000 },
			Price { unit: PriceUnit::MtokCacheWrite, nanos_usd: 6_875_000_000 },
		]);
		assert!(reopened.load_fresh(&key, 1_101).expect("expired").is_none());
		assert_eq!(
			reopened
				.lifecycle(&provider)
				.expect("lifecycle")
				.unwrap()
				.state,
			ProviderDiscoveryState::Ready
		);
	}
	#[test]
	fn pre_pricing_schema_rows_are_invalidated() {
		let directory = tempfile::tempdir().expect("directory");
		let path = directory.path().join("models.db");
		let provider = ProviderId::from("opencode-zen");
		let legacy = Connection::open(&path).expect("legacy database");
		legacy
			.execute_batch(
				"CREATE TABLE discovery_meta (
				   key TEXT PRIMARY KEY,
				   value INTEGER NOT NULL
				 );
				 INSERT INTO discovery_meta(key, value) VALUES('schema_version', 2);
				 CREATE TABLE discovered_models (
				   provider TEXT NOT NULL,
				   generation INTEGER NOT NULL,
				   ordinal INTEGER NOT NULL,
				   row_json BLOB NOT NULL,
				   expires_at_ms INTEGER NOT NULL,
				   PRIMARY KEY(provider, generation, ordinal)
				 );",
			)
			.expect("legacy schema");
		legacy
			.execute(
				"INSERT INTO discovered_models(
				   provider, generation, ordinal, row_json, expires_at_ms
				 ) VALUES(?1, 1, 0, ?2, 1000)",
				params![provider.as_str(), serde_json::to_vec(&row(&provider)).expect("row JSON")],
			)
			.expect("legacy row");
		drop(legacy);

		let store = DiscoveryStore::open(&path).expect("migrate");
		assert!(
			store
				.load_fresh(&DiscoveryCacheKey::provider(provider), 100)
				.expect("load invalidated namespace")
				.is_none()
		);
	}

	#[test]
	fn credential_scopes_are_isolated_without_persisting_credentials() {
		let directory = tempfile::tempdir().expect("directory");
		let path = directory.path().join("models.db");
		let provider = ProviderId::from("github-copilot");
		let first = DiscoveryCacheKey::credential(provider.clone(), "affinity-a");
		let second = DiscoveryCacheKey::credential(provider.clone(), "affinity-b");
		let store = DiscoveryStore::open(&path).expect("open");
		store
			.publish(&first, &[row(&provider)], 100, Duration::from_secs(60))
			.expect("publish scoped cache");
		assert!(
			store
				.load_fresh(&first, 101)
				.expect("first scope")
				.is_some()
		);
		assert!(
			store
				.load_fresh(&second, 101)
				.expect("second scope")
				.is_none()
		);
		let encoded: Vec<u8> = store
			.connection
			.lock()
			.query_row(
				"SELECT row_json FROM discovered_models WHERE provider=?1 AND cache_scope=?2",
				params![provider.as_str(), first.scope()],
				|row| row.get(0),
			)
			.expect("cached row");
		let encoded = str::from_utf8(&encoded).expect("JSON text");
		assert!(!encoded.contains("authorization"));
		assert!(!encoded.contains("headers"));
		assert!(!encoded.contains("credential-secret"));
	}
}
