//! Canonical 32-byte digest representation.
//!
//! [`Hash32`] is the workspace-wide content digest: SHA-256 bytes rendered
//! as 64 lowercase hexadecimal characters in text form. [`Hash32::sum`] hashes
//! one buffer; [`Hash32::hasher`] returns an incremental [`Hasher`] for
//! multi-part input.

use std::{
	fmt::{self, Display},
	io,
	str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::encoding::{hex, hex::ArrayStr};

/// A 32-byte digest rendered as 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash32([u8; 32]);

impl Hash32 {
	/// Creates a digest from its raw bytes.
	pub const fn new(bytes: [u8; 32]) -> Self {
		Self(bytes)
	}

	/// Returns the raw digest bytes.
	pub const fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}

	/// Consumes the digest and returns its raw bytes.
	pub const fn into_bytes(self) -> [u8; 32] {
		self.0
	}

	/// Returns the digest as 64 lowercase hexadecimal characters in stack
	/// storage.
	pub const fn to_hex(&self) -> ArrayStr<32> {
		hex::encode_n(&self.0)
	}

	/// Returns the SHA-256 digest of `bytes` in one shot.
	pub fn sum(bytes: impl AsRef<[u8]>) -> Self {
		Self(Sha256::digest(bytes.as_ref()).into())
	}

	/// Returns an incremental SHA-256 hasher finalizing into a [`Hash32`].
	pub fn hasher() -> Hasher {
		Hasher::new()
	}
}

/// Incremental SHA-256 state produced by [`Hash32::hasher`].
///
/// Accepts input through chained [`Hasher::update`] calls or as an
/// [`io::Write`] sink (e.g. `serde_json::to_writer`), then yields the digest
/// via [`Hasher::finalize`].
#[derive(Clone, Debug, Default)]
pub struct Hasher(Sha256);

impl Hasher {
	/// Creates an empty hasher.
	pub fn new() -> Self {
		Self(Sha256::new())
	}

	/// Absorbs `bytes` and returns the hasher for chaining.
	#[inline]
	pub fn update(&mut self, bytes: impl AsRef<[u8]>) -> &mut Self {
		self.0.update(bytes.as_ref());
		self
	}

	/// Returns the digest of everything absorbed so far.
	pub fn finalize(&self) -> Hash32 {
		Hash32(self.0.clone().finalize().into())
	}
}

impl io::Write for Hasher {
	#[inline]
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.0.update(buf);
		Ok(buf.len())
	}

	#[inline]
	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

impl From<[u8; 32]> for Hash32 {
	fn from(bytes: [u8; 32]) -> Self {
		Self::new(bytes)
	}
}

impl From<Hash32> for [u8; 32] {
	fn from(hash: Hash32) -> Self {
		hash.into_bytes()
	}
}

impl Display for Hash32 {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.to_hex().as_str())
	}
}

impl fmt::Debug for Hash32 {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		Display::fmt(self, formatter)
	}
}

/// Failure to parse a [`Hash32`] from canonical lowercase hexadecimal text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Hash32ParseError {
	/// The hexadecimal representation was not exactly 64 characters.
	#[error("hash must contain exactly 64 lowercase hexadecimal characters")]
	InvalidLength,
	/// The representation contained a character outside lowercase hexadecimal.
	#[error("hash contains a non-lowercase-hexadecimal character")]
	InvalidHex,
}

impl FromStr for Hash32 {
	type Err = Hash32ParseError;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		if value.len() != 64 {
			return Err(Hash32ParseError::InvalidLength);
		}
		if !value
			.bytes()
			.all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
		{
			return Err(Hash32ParseError::InvalidHex);
		}

		let bytes = hex::Decoder::new(value.as_bytes())
			.into_array()
			.map_err(|_| Hash32ParseError::InvalidHex)?;
		Ok(Self(bytes))
	}
}

impl Serialize for Hash32 {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.serialize_str(self.to_hex().as_str())
	}
}

impl<'de> Deserialize<'de> for Hash32 {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		struct Hash32Visitor;

		impl de::Visitor<'_> for Hash32Visitor {
			type Value = Hash32;

			fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
				formatter.write_str("exactly 64 lowercase hexadecimal characters")
			}

			fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
			where
				E: de::Error,
			{
				value.parse().map_err(E::custom)
			}

			fn visit_borrowed_str<E>(self, value: &'_ str) -> Result<Self::Value, E>
			where
				E: de::Error,
			{
				self.visit_str(value)
			}
		}

		deserializer.deserialize_str(Hash32Visitor)
	}
}

#[cfg(test)]
mod tests {

	use std::{array, str};

	use super::{Hash32, Hash32ParseError};

	const HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

	#[test]
	fn hash32_serde_round_trip() {
		let hash: Hash32 = HEX.parse().unwrap();
		let encoded = serde_json::to_string(&hash).unwrap();
		assert_eq!(encoded, format!("\"{HEX}\""));
		assert_eq!(serde_json::from_str::<Hash32>(&encoded).unwrap(), hash);
	}

	#[test]
	fn hash32_display_and_debug_are_lowercase_hex() {
		let hash = Hash32::new(array::from_fn(|index| index as u8));
		assert_eq!(hash.to_string(), HEX);
		assert_eq!(format!("{hash:?}"), HEX);
	}

	#[test]
	fn hash32_rejects_noncanonical_text() {
		let uppercase = HEX.to_ascii_uppercase();
		assert_eq!(uppercase.parse::<Hash32>(), Err(Hash32ParseError::InvalidHex));
		assert_eq!(HEX[..63].parse::<Hash32>(), Err(Hash32ParseError::InvalidLength));

		let mut non_hex = HEX.as_bytes().to_owned();
		non_hex[0] = b'g';
		let non_hex = str::from_utf8(&non_hex).unwrap();
		assert_eq!(non_hex.parse::<Hash32>(), Err(Hash32ParseError::InvalidHex));

		assert!(serde_json::from_str::<Hash32>(&format!("\"{uppercase}\"")).is_err());
	}

	#[test]
	fn hash32_default_is_zero() {
		assert_eq!(Hash32::default().to_string(), "0".repeat(64));
	}

	#[test]
	fn sum_is_sha256() {
		// Known SHA-256 digest of the empty input.
		assert_eq!(
			Hash32::sum(b"").to_string(),
			"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
		);
	}

	#[test]
	fn hasher_matches_one_shot_sum() {
		let mut hasher = Hash32::hasher();
		hasher.update(b"omp/").update(b"digest");
		assert_eq!(hasher.finalize(), Hash32::sum(b"omp/digest"));
	}

	#[test]
	fn hasher_write_sink_matches_update() {
		use std::io::Write;
		let mut hasher = Hash32::hasher();
		hasher.write_all(b"streamed input").unwrap();
		assert_eq!(hasher.finalize(), Hash32::sum(b"streamed input"));
	}
}
