//! MCP projection of the app's combined encrypted credential authority.

use std::{fmt, sync::Arc, time::SystemTime};

use futures::future::BoxFuture;
use omp_catalog::AuthSpecId;
use omp_core::{ExposeSecret as _, Hash32, SecretBox, SecretString, Str};
use omp_inference::{
	auth::{
		AuthRejection, CredentialError, CredentialLease, CredentialNeed, CredentialOrigin,
		CredentialSource, CredentialStore, CredentialWrite, StoreError, StoredCredentialSource,
	},
	id::{AccountId, PrincipalId},
};
use serde::{Deserialize, Serialize};

/// Opaque session-safe affinity. It contains no token, key, header, or URL.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthAffinity {
	/// Encrypted-store account identity.
	pub account:   AccountId,
	/// Authenticated profile/principal identity.
	pub principal: PrincipalId,
}

/// Shared provider+MCP lease and refresh boundary.
///
/// Consumers receive only sealed [`CredentialLease`] values. Refresh first
/// rejects the observed generation with typed evidence, then reacquires the
/// same opaque affinity; token bytes never cross this trait.
pub trait CredentialAuthority: CredentialSource {
	/// Issues a provider lease from the combined encrypted store.
	fn provider_lease(
		&self,
		need: CredentialNeed,
	) -> BoxFuture<'_, Result<CredentialLease, CredentialError>>;

	/// Issues an MCP lease pinned to session-safe affinity.
	fn mcp_lease<'a>(
		&'a self,
		affinity: &'a AuthAffinity,
		valid_after: SystemTime,
	) -> BoxFuture<'a, Result<CredentialLease, CredentialError>>;

	/// Rejects and refreshes one observed MCP generation.
	fn refresh_mcp<'a>(
		&'a self,
		affinity: &'a AuthAffinity,
		rejected: &'a CredentialLease,
		evidence: AuthRejection,
		valid_after: SystemTime,
	) -> BoxFuture<'a, Result<CredentialLease, CredentialError>>;
}

/// One provider+MCP encrypted credential authority.
///
/// Provider and MCP leases traverse the same [`CredentialStore`] and sealed
/// [`CredentialLease`] type; MCP never receives a plaintext token accessor.
#[derive(Clone)]
pub struct CombinedAuthAuthority {
	store:  Arc<CredentialStore>,
	stored: StoredCredentialSource,
}

impl CombinedAuthAuthority {
	/// Composes both credential domains over one already-open encrypted store.
	pub fn new(store: Arc<CredentialStore>) -> Self {
		Self { stored: StoredCredentialSource::new(store.clone()), store }
	}

	/// Issues a provider lease through the shared encrypted-store source.
	pub async fn provider_lease(
		&self,
		need: CredentialNeed,
	) -> Result<CredentialLease, CredentialError> {
		self.stored.lease(need).await
	}

	/// Derives a non-reversible account identity from profile and MCP server
	/// identity. The source URL/name is never persisted as affinity.
	pub fn mcp_affinity(
		profile: &str,
		server_identity: &str,
		principal: PrincipalId,
	) -> AuthAffinity {
		let mut hasher = Hash32::hasher();
		hasher.update(b"omp-mcp-affinity/v1\0");
		hasher.update(profile.as_bytes());
		hasher.update(b"\0");
		hasher.update(server_identity.as_bytes());
		let digest = hasher.finalize();
		AuthAffinity {
			account: AccountId::new(format!("mcp/{}", digest.to_hex())),
			principal,
		}
	}

	/// Atomically imports or rotates one MCP bearer token at the sole secret
	/// ingress boundary.
	pub fn persist_mcp_bearer(
		&self,
		affinity: &AuthAffinity,
		token: SecretString,
		expires_at_ms: Option<u64>,
		now_ms: u64,
		expected_generation: Option<u64>,
	) -> Result<u64, StoreError> {
		self.persist_mcp_secret(affinity, "bearer", token, expires_at_ms, now_ms, expected_generation)
	}

	/// Atomically imports or rotates one MCP API key at the sole encrypted
	/// secret-ingress boundary.
	pub fn persist_mcp_api_key(
		&self,
		affinity: &AuthAffinity,
		key: SecretString,
		now_ms: u64,
		expected_generation: Option<u64>,
	) -> Result<u64, StoreError> {
		self.persist_mcp_secret(affinity, "api-key", key, None, now_ms, expected_generation)
	}

	fn persist_mcp_secret(
		&self,
		affinity: &AuthAffinity,
		kind: &'static str,
		value: SecretString,
		expires_at_ms: Option<u64>,
		now_ms: u64,
		expected_generation: Option<u64>,
	) -> Result<u64, StoreError> {
		let secret = SecretBox::new(Box::new(value.expose_secret().as_bytes().to_vec()));
		let metadata = self.store.put(CredentialWrite {
			account_id: &affinity.account,
			principal_id: &affinity.principal,
			kind,
			secret: &secret,
			expires_at_ms,
			origin: CredentialOrigin::Persistent,
			now_ms,
			expected_generation,
		})?;
		Ok(metadata.generation)
	}

	/// Deletes every stored secret for one MCP affinity.
	pub fn delete_mcp(&self, affinity: &AuthAffinity) -> Result<bool, StoreError> {
		self.store.delete(&affinity.account)
	}

	/// Issues an MCP bearer lease pinned to opaque affinity and minimum expiry.
	pub async fn mcp_lease(
		&self,
		affinity: &AuthAffinity,
		valid_after: SystemTime,
	) -> Result<CredentialLease, CredentialError> {
		self
			.stored
			.lease(CredentialNeed {
				spec: AuthSpecId::from(Str::new_static("mcp")),
				account: Some(affinity.account.clone()),
				principal: Some(affinity.principal.clone()),
				valid_after,
			})
			.await
	}
}

impl CredentialAuthority for CombinedAuthAuthority {
	fn provider_lease(
		&self,
		need: CredentialNeed,
	) -> BoxFuture<'_, Result<CredentialLease, CredentialError>> {
		self.stored.lease(need)
	}

	fn mcp_lease<'a>(
		&'a self,
		affinity: &'a AuthAffinity,
		valid_after: SystemTime,
	) -> BoxFuture<'a, Result<CredentialLease, CredentialError>> {
		Box::pin(async move { CombinedAuthAuthority::mcp_lease(self, affinity, valid_after).await })
	}

	fn refresh_mcp<'a>(
		&'a self,
		affinity: &'a AuthAffinity,
		rejected: &'a CredentialLease,
		evidence: AuthRejection,
		valid_after: SystemTime,
	) -> BoxFuture<'a, Result<CredentialLease, CredentialError>> {
		Box::pin(async move {
			self.stored.reject(rejected, evidence).await?;
			CombinedAuthAuthority::mcp_lease(self, affinity, valid_after).await
		})
	}
}

impl CredentialSource for CombinedAuthAuthority {
	fn lease(
		&self,
		need: CredentialNeed,
	) -> BoxFuture<'_, Result<CredentialLease, CredentialError>> {
		self.stored.lease(need)
	}

	fn reject<'a>(
		&'a self,
		lease: &'a CredentialLease,
		evidence: AuthRejection,
	) -> BoxFuture<'a, Result<(), CredentialError>> {
		self.stored.reject(lease, evidence)
	}
}

impl fmt::Debug for CombinedAuthAuthority {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("CombinedAuthAuthority(..)")
	}
}

#[cfg(test)]
mod tests {
	use omp_inference::auth::{CredentialKind, HeadlessKeySource, KeyId};

	use super::*;

	#[tokio::test]
	async fn provider_and_mcp_leases_share_one_encrypted_store() {
		let directory = tempfile::tempdir().expect("credential directory");
		let keys = Arc::new(HeadlessKeySource::new(KeyId::new("test-key"), [7; 32]));
		let store = Arc::new(
			CredentialStore::open(directory.path().join("credentials.sqlite3"), keys)
				.expect("credential store"),
		);
		let authority = CombinedAuthAuthority::new(store);
		let affinity = CombinedAuthAuthority::mcp_affinity(
			"work",
			"https://mcp.example/tenant?token=never-persist-this",
			PrincipalId::from("profile"),
		);
		assert!(!affinity.account.as_str().contains("mcp.example"));
		authority
			.persist_mcp_bearer(&affinity, SecretString::from("opaque-token"), None, 1, None)
			.expect("persist bearer");
		let mcp = authority
			.mcp_lease(&affinity, SystemTime::UNIX_EPOCH)
			.await
			.expect("MCP lease");
		let provider = authority
			.provider_lease(CredentialNeed {
				spec:        AuthSpecId::from("provider"),
				account:     Some(affinity.account.clone()),
				principal:   Some(affinity.principal.clone()),
				valid_after: SystemTime::UNIX_EPOCH,
			})
			.await
			.expect("provider lease");
		assert_eq!(mcp.kind(), CredentialKind::Bearer);
		assert_eq!(mcp.meta(), provider.meta());
	}

	#[tokio::test]
	async fn deleting_mcp_credential_removes_it_from_the_shared_store() {
		let directory = tempfile::tempdir().expect("credential directory");
		let keys = Arc::new(HeadlessKeySource::new(KeyId::new("test-key"), [7; 32]));
		let store = Arc::new(
			CredentialStore::open(directory.path().join("credentials.sqlite3"), keys)
				.expect("credential store"),
		);
		let authority = CombinedAuthAuthority::new(store);
		let affinity = CombinedAuthAuthority::mcp_affinity(
			"default",
			"https://mcp.example/server",
			PrincipalId::from("default"),
		);
		authority
			.persist_mcp_bearer(&affinity, SecretString::from("opaque-token"), None, 1, None)
			.expect("persist bearer");

		assert!(authority.delete_mcp(&affinity).expect("delete bearer"));
		assert!(
			authority
				.mcp_lease(&affinity, SystemTime::UNIX_EPOCH)
				.await
				.is_err(),
			"deleted MCP credential must not remain leasable",
		);
		assert!(
			!authority
				.delete_mcp(&affinity)
				.expect("delete absent bearer")
		);
	}
}
