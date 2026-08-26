//! Authenticated actor identity and extension provenance carried by durable
//! records.

use std::{
	cmp,
	fmt::{self, Display},
	hash::{Hash, Hasher},
	str::FromStr,
	sync::Arc,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::{Hash32, Str, hex};

/// Authorization tier derived from a timing-safe collaboration credential
/// check.
#[derive(
	Clone, Copy, Debug, Eq, Hash, PartialEq, strum::Display, strum::EnumString, strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
pub enum CredentialTier {
	/// The peer may observe public collaboration state but cannot mutate it.
	ReadOnly,
	/// The peer presented the room's writable credential.
	FullAccess,
}

/// Authenticated collaboration peer stamped onto every admitted remote
/// mutation.
///
/// The room credential itself is never retained. `token_digest` is an
/// audit-linkage digest of the accepted token and is absent for read-only
/// peers. The handle is Arc-backed because it crosses the Core, Environment,
/// approval, and journal boundaries with every remote operation.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct RemotePrincipal(Arc<RemotePrincipalData>);

#[derive(Eq, Hash, PartialEq)]
struct RemotePrincipalData {
	peer_id:         u32,
	display_name:    Str,
	credential_tier: CredentialTier,
	room_id:         Str,
	token_digest:    Option<Hash32>,
}

const _: () =
	assert!(std::mem::size_of::<RemotePrincipal>() <= 16, "RemotePrincipal must remain clone-cheap");

impl RemotePrincipal {
	/// Creates a principal from host-authenticated relay and credential facts.
	pub fn new(
		peer_id: u32,
		display_name: Str,
		credential_tier: CredentialTier,
		room_id: Str,
		token_digest: Option<Hash32>,
	) -> Self {
		Self(Arc::new(RemotePrincipalData {
			peer_id,
			display_name,
			credential_tier,
			room_id,
			token_digest,
		}))
	}

	/// Returns the relay-assigned peer identifier.
	pub fn peer_id(&self) -> u32 {
		self.0.peer_id
	}

	/// Returns the sanitized, human-readable peer name.
	pub fn display_name(&self) -> &str {
		self.0.display_name.as_str()
	}

	/// Returns the host-verified collaboration credential tier.
	pub fn credential_tier(&self) -> CredentialTier {
		self.0.credential_tier
	}

	/// Returns the stable room identifier.
	pub fn room_id(&self) -> &str {
		self.0.room_id.as_str()
	}

	/// Returns the accepted write-token digest used for audit linkage.
	pub fn token_digest(&self) -> Option<Hash32> {
		self.0.token_digest
	}

	/// Returns whether this peer may submit host mutations.
	pub fn may_mutate(&self) -> bool {
		matches!(self.0.credential_tier, CredentialTier::FullAccess)
	}
}

impl fmt::Debug for RemotePrincipal {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("RemotePrincipal")
			.field("peer_id", &self.0.peer_id)
			.field("display_name", &self.0.display_name)
			.field("credential_tier", &self.0.credential_tier)
			.field("room_id", &"[redacted]")
			.field("token_digest", &self.0.token_digest.map(|_| "[redacted]"))
			.finish()
	}
}

/// The authenticated person acting through an omp daemon.

/// The authenticated person acting through an omp daemon.
///
/// A principal is derived by the core from the authenticated connection. Its
/// identifier is intentionally redacted from [`Debug`] output so durable error
/// paths cannot accidentally disclose account identity.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Principal {
	id:      Str,
	display: Str,
}

impl Principal {
	/// Creates an authenticated principal from its stable identifier and safe
	/// human-readable display name.
	pub const fn new(id: Str, display: Str) -> Self {
		Self { id, display }
	}

	/// Returns the stable principal identifier.
	pub fn id(&self) -> &str {
		self.id.as_str()
	}

	/// Returns the human-readable principal name intended for UI surfaces.
	pub fn display(&self) -> &str {
		self.display.as_str()
	}
}

impl fmt::Debug for Principal {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("Principal")
			.field("id", &"[redacted]")
			.field("display", &self.display)
			.finish()
	}
}

/// A SHA-256 digest identifying the exact extension artifact that acted.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactDigest([u8; 32]);

impl ArtifactDigest {
	/// Creates an artifact digest from its raw SHA-256 bytes.
	pub const fn new(bytes: [u8; 32]) -> Self {
		Self(bytes)
	}

	/// Returns the raw SHA-256 digest bytes.
	pub const fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}

	/// Consumes the digest and returns its raw bytes.
	pub const fn into_bytes(self) -> [u8; 32] {
		self.0
	}
}

impl Display for ArtifactDigest {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("sha256:")?;
		Display::fmt(&hex::encode(&self.0), formatter)
	}
}

impl fmt::Debug for ArtifactDigest {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		Display::fmt(self, formatter)
	}
}

/// Failure to parse an extension artifact digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ArtifactDigestError {
	/// The digest did not use the canonical `sha256:` prefix.
	#[error("artifact digest must start with `sha256:`")]
	MissingPrefix,
	/// The hexadecimal payload was not exactly 32 bytes.
	#[error("artifact digest must contain exactly 64 lowercase hexadecimal digits")]
	InvalidLength,
	/// The hexadecimal payload was not canonical lowercase hexadecimal.
	#[error("artifact digest contains a non-lowercase-hexadecimal character")]
	InvalidHex,
}

impl FromStr for ArtifactDigest {
	type Err = ArtifactDigestError;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		let encoded = value
			.strip_prefix("sha256:")
			.ok_or(ArtifactDigestError::MissingPrefix)?;
		if encoded.len() != 64 {
			return Err(ArtifactDigestError::InvalidLength);
		}
		if !encoded
			.bytes()
			.all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
		{
			return Err(ArtifactDigestError::InvalidHex);
		}
		let bytes = <[u8; 32]>::try_from(hex::decode(encoded.as_bytes()))
			.map_err(|_| ArtifactDigestError::InvalidHex)?;
		Ok(Self(bytes))
	}
}

impl Serialize for ArtifactDigest {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.collect_str(self)
	}
}

impl<'de> Deserialize<'de> for ArtifactDigest {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let value = Str::deserialize(deserializer)?;
		value.as_str().parse().map_err(de::Error::custom)
	}
}

/// Core-stamped identity of the exact extension incarnation that acted.
///
/// The seven fields are the publisher, extension id, version, artifact digest,
/// installation layer, trust tier, and host generation. Workers may observe
/// this value but must never be trusted to author it.
#[derive(Clone)]
pub struct Provenance(Arc<ProvenanceData>);

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct ProvenanceData {
	publisher:       Str,
	extension_id:    Str,
	version:         Str,
	artifact_digest: ArtifactDigest,
	layer:           Str,
	tier:            Str,
	generation:      u64,
}

const _: () = assert!(std::mem::size_of::<Provenance>() <= 16, "Provenance must stay compact");

impl Provenance {
	/// Creates provenance from core-authenticated extension installation facts.
	pub fn new(
		publisher: Str,
		extension_id: Str,
		version: Str,
		artifact_digest: ArtifactDigest,
		layer: Str,
		tier: Str,
		generation: u64,
	) -> Self {
		Self(Arc::new(ProvenanceData {
			publisher,
			extension_id,
			version,
			artifact_digest,
			layer,
			tier,
			generation,
		}))
	}

	/// Returns the publisher key fingerprint.
	pub fn publisher(&self) -> &str {
		self.0.publisher.as_str()
	}

	/// Returns the dotted extension identifier.
	pub fn extension_id(&self) -> &str {
		self.0.extension_id.as_str()
	}

	/// Returns the exact extension version.
	pub fn version(&self) -> &str {
		self.0.version.as_str()
	}

	/// Returns the exact extension artifact digest.
	pub fn artifact_digest(&self) -> ArtifactDigest {
		self.0.artifact_digest
	}

	/// Returns the installation layer.
	pub fn layer(&self) -> &str {
		self.0.layer.as_str()
	}

	/// Returns the conferred trust tier.
	pub fn tier(&self) -> &str {
		self.0.tier.as_str()
	}

	/// Returns the host incarnation generation.
	pub fn generation(&self) -> u64 {
		self.0.generation
	}
}

impl fmt::Debug for Provenance {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("Provenance")
			.field("publisher", &self.0.publisher)
			.field("extension_id", &self.0.extension_id)
			.field("version", &self.0.version)
			.field("artifact_digest", &self.0.artifact_digest)
			.field("layer", &self.0.layer)
			.field("tier", &self.0.tier)
			.field("generation", &self.0.generation)
			.finish()
	}
}

impl PartialEq for Provenance {
	fn eq(&self, other: &Self) -> bool {
		self.0 == other.0
	}
}

impl Eq for Provenance {}

impl PartialOrd for Provenance {
	fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for Provenance {
	fn cmp(&self, other: &Self) -> cmp::Ordering {
		self.0.cmp(&other.0)
	}
}

impl Hash for Provenance {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.0.hash(state);
	}
}

impl Serialize for Provenance {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		self.0.serialize(serializer)
	}
}

impl<'de> Deserialize<'de> for Provenance {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		ProvenanceData::deserialize(deserializer).map(|data| Self(Arc::new(data)))
	}
}
