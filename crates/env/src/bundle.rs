//! Air-gap extension bundles transferred through the existing blob service.

use std::str;

use bytes::Bytes;
use omp_ar::{Archive, Format, zip::Writer};
use omp_core::{Hash32, Str, encoding::hex, sf};
use omp_proto::blob::v1::{Chunk, GetRequest, StatRequest};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml::{de, ser};

use crate::{BlobDownloadEvent, ClientError, EnvClient};

const BUNDLE_FORMAT: u32 = 1;
const BLOB_CHUNK_BYTES: usize = 64 * 1024;

/// One content-addressed payload included in an air-gap bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleFile {
	/// Fixed-layout bundle-relative pathname, such as `wheels/<hash>.whl`.
	pub path:     Str,
	/// Exact payload bytes.
	pub contents: Bytes,
}

/// Serializable content index carried by `bundle.toml`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BundleEntry {
	/// Fixed-layout bundle-relative pathname.
	pub path:   Str,
	/// Lowercase SHA-256 hex digest of the payload.
	pub sha256: Str,
	/// Exact payload length in bytes.
	pub size:   u64,
}

/// The deterministic metadata portion of an `.ompb` archive.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BundleManifest {
	/// Bundle-format revision. Version 1 is the only accepted revision.
	pub format:     u32,
	/// Program and version which created the archive.
	pub created_by: Str,
	/// Target triples represented by the included lock closure.
	pub targets:    Vec<Str>,
	/// Unordered, content-addressed archive payload index.
	pub contents:   Vec<BundleEntry>,
}

/// Decoded bundle metadata and verified payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AirgapBundle {
	/// Bundle metadata read from `bundle.toml`.
	pub manifest: BundleManifest,
	/// Every non-manifest file from the archive, indexed by its manifest entry.
	pub files:    Vec<BundleFile>,
}

/// Bundle encoding, integrity, or blob-transfer failure.
#[derive(Debug, Error)]
pub enum BundleError {
	/// The archive was malformed or violated bounded archive rules.
	#[error(transparent)]
	Archive(#[from] omp_ar::Error),
	/// The bundle manifest could not be encoded or decoded as TOML.
	#[error(transparent)]
	TomlDe(#[from] de::Error),
	/// The bundle manifest could not be encoded as TOML.
	#[error(transparent)]
	TomlSer(#[from] ser::Error),
	/// The existing environment blob transport rejected the transfer.
	#[error(transparent)]
	Client(#[from] ClientError),
	/// The archive layout was not the fixed air-gap layout.
	#[error("invalid air-gap bundle layout: {0}")]
	Layout(Str),
	/// A bundle payload did not match its manifest's content address.
	#[error("bundle integrity mismatch for {0}")]
	Integrity(Str),
}

/// Encodes a deterministic `.ompb` ZIP with the required `bundle.toml` index.
///
/// `files` must include the deployment artifacts (`omp.lock`, wheels, binaries,
/// keys, attestations, and revocations) appropriate to the caller's closure.
/// Payload ordering does not affect the manifest semantics; archive members are
/// written in lexical path order for reproducible output.
///
/// # Errors
///
/// Returns [`BundleError::Layout`] for unsafe or duplicate paths and propagates
/// ZIP or manifest encoding failures.
pub fn pack_bundle(
	created_by: impl Into<Str>,
	targets: Vec<Str>,
	mut files: Vec<BundleFile>,
) -> Result<Bytes, BundleError> {
	files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
	let mut contents = Vec::with_capacity(files.len());
	let mut previous = None;
	for file in &files {
		validate_payload_path(&file.path)?;
		if previous == Some(file.path.as_str()) {
			return Err(BundleError::Layout(sf!("duplicate bundle member")));
		}
		previous = Some(file.path.as_str());
		let digest = Hash32::sum(&file.contents).to_hex();
		if !payload_name_matches_digest(&file.path, digest.as_str()) {
			return Err(BundleError::Layout(sf!("artifact pathname does not match its digest",)));
		}
		contents.push(BundleEntry {
			path:   file.path.clone(),
			sha256: Str::new(digest.as_str()),
			size:   u64::try_from(file.contents.len())
				.map_err(|_| BundleError::Layout(sf!("payload length exceeds u64")))?,
		});
	}
	let manifest =
		BundleManifest { format: BUNDLE_FORMAT, created_by: created_by.into(), targets, contents };
	let mut writer = Writer::new(Vec::new());
	writer.add_file("bundle.toml", toml::to_string(&manifest)?.as_bytes())?;
	for file in files {
		writer.add_file(file.path.as_str(), &file.contents)?;
	}
	Ok(Bytes::from(writer.finish()?))
}

/// Decodes an `.ompb` archive and verifies every indexed payload digest.
///
/// # Errors
///
/// Returns [`BundleError::Integrity`] when an indexed payload is missing,
/// unexpected, or differs from its SHA-256 address.
pub fn unpack_bundle(bytes: &[u8]) -> Result<AirgapBundle, BundleError> {
	let mut archive = Archive::from_bytes_with_format(bytes, Format::Zip)?;
	let mut files = archive.read_all()?;
	let manifest = files
		.remove("bundle.toml")
		.ok_or_else(|| BundleError::Layout(sf!("bundle.toml is missing")))?;
	let manifest = toml::from_str::<BundleManifest>(
		str::from_utf8(&manifest)
			.map_err(|_| BundleError::Layout(sf!("bundle.toml is not UTF-8")))?,
	)?;
	if manifest.format != BUNDLE_FORMAT {
		return Err(BundleError::Layout(sf!("unsupported bundle format")));
	}
	let mut decoded = Vec::with_capacity(manifest.contents.len());
	for entry in &manifest.contents {
		validate_payload_path(&entry.path)?;
		let contents = files
			.remove(entry.path.as_str())
			.ok_or_else(|| BundleError::Integrity(entry.path.clone()))?;
		verify_entry(entry, &contents)?;
		if !payload_name_matches_digest(&entry.path, &entry.sha256) {
			return Err(BundleError::Integrity(entry.path.clone()));
		}
		decoded.push(BundleFile { path: entry.path.clone(), contents: Bytes::from(contents) });
	}
	if !files.is_empty() {
		return Err(BundleError::Layout(sf!("bundle contains an unindexed member")));
	}
	Ok(AirgapBundle { manifest, files: decoded })
}

/// Pushes every missing bundle payload using only blob `Stat` and streaming
/// `Put`.
///
/// Existing content-addressed entries are never re-uploaded.
///
/// # Errors
///
/// Returns an error when a local payload no longer matches the manifest or the
/// remote blob service refuses a stream.
pub async fn push_bundle(client: &EnvClient, bundle: &AirgapBundle) -> Result<(), BundleError> {
	if bundle.manifest.format != BUNDLE_FORMAT
		|| bundle.manifest.contents.len() != bundle.files.len()
	{
		return Err(BundleError::Layout(sf!("bundle manifest and payloads differ")));
	}
	for (entry, file) in bundle.manifest.contents.iter().zip(&bundle.files) {
		if entry.path != file.path {
			return Err(BundleError::Layout(sf!("bundle payload order differs from manifest",)));
		}
		validate_payload_path(&entry.path)?;
		if !payload_name_matches_digest(&entry.path, &entry.sha256) {
			return Err(BundleError::Integrity(entry.path.clone()));
		}
		verify_entry(entry, &file.contents)?;
		let hash = hash_bytes(&entry.sha256)?;
		let stat = client.blob_stat(StatRequest { hash: hash.clone() }).await?;
		if stat.present {
			if stat.size != entry.size {
				return Err(BundleError::Integrity(entry.path.clone()));
			}
			continue;
		}
		let upload = client.blob_put()?;
		if file.contents.is_empty() {
			upload
				.send_chunk(Chunk { data: Bytes::new(), hash: hash.clone(), size: Some(0) })
				.await?;
		} else {
			for (index, chunk) in file.contents.chunks(BLOB_CHUNK_BYTES).enumerate() {
				upload
					.send_chunk(Chunk {
						data: Bytes::copy_from_slice(chunk),
						hash: if index == 0 {
							hash.clone()
						} else {
							Bytes::new()
						},
						size: if index == 0 { Some(entry.size) } else { None },
					})
					.await?;
			}
		}
		let stored = upload.commit().await?;
		if stored.hash != hash || stored.size != entry.size {
			return Err(BundleError::Integrity(entry.path.clone()));
		}
	}
	Ok(())
}

/// Pulls and verifies every manifest-addressed payload through streaming blob
/// `Get`.
///
/// The caller supplies the manifest from a trusted local bundle or deployment
/// record; the remote blob service is only the content-addressed transport.
pub async fn pull_bundle(
	client: &EnvClient,
	manifest: BundleManifest,
) -> Result<AirgapBundle, BundleError> {
	if manifest.format != BUNDLE_FORMAT {
		return Err(BundleError::Layout(sf!("unsupported bundle format")));
	}
	let mut files = Vec::with_capacity(manifest.contents.len());
	for entry in &manifest.contents {
		validate_payload_path(&entry.path)?;
		if !payload_name_matches_digest(&entry.path, &entry.sha256) {
			return Err(BundleError::Integrity(entry.path.clone()));
		}
		let hash = hash_bytes(&entry.sha256)?;
		let mut download = client
			.blob_get(GetRequest { hash, offset: 0, length: 0 })
			.await?;
		let mut contents = Vec::with_capacity(usize::try_from(entry.size).unwrap_or(0));
		let mut complete = false;
		while let Some(event) = download.next_event().await? {
			match event {
				BlobDownloadEvent::Chunk(chunk) => contents.extend_from_slice(&chunk.data),
				BlobDownloadEvent::Complete(_) => {
					complete = true;
					break;
				},
			}
		}
		if !complete {
			return Err(BundleError::Integrity(entry.path.clone()));
		}
		verify_entry(entry, &contents)?;
		files.push(BundleFile { path: entry.path.clone(), contents: Bytes::from(contents) });
	}
	Ok(AirgapBundle { manifest, files })
}

fn validate_payload_path(path: &str) -> Result<(), BundleError> {
	let valid_root = match path {
		"omp.lock" | "keys.toml" | "attestations.jsonl" | "revocations.json" => true,
		_ if path.starts_with("wheels/") => path
			.strip_prefix("wheels/")
			.and_then(|name| name.strip_suffix(".whl"))
			.is_some_and(is_sha256_hex),
		_ if path.starts_with("bin/") => path.strip_prefix("bin/").is_some_and(is_sha256_hex),
		_ => false,
	};
	if !valid_root
		|| path.contains('\\')
		|| path
			.split('/')
			.any(|part| part.is_empty() || part == "." || part == "..")
	{
		return Err(BundleError::Layout(sf!("member path is outside the fixed bundle layout",)));
	}
	Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
	value.parse::<Hash32>().is_ok()
}

fn verify_entry(entry: &BundleEntry, contents: &[u8]) -> Result<(), BundleError> {
	let size =
		u64::try_from(contents.len()).map_err(|_| BundleError::Integrity(entry.path.clone()))?;
	let digest = Hash32::sum(contents).to_hex();
	if size != entry.size || digest.as_str() != entry.sha256 {
		return Err(BundleError::Integrity(entry.path.clone()));
	}
	Ok(())
}

fn payload_name_matches_digest(path: &str, digest: &str) -> bool {
	match path {
		path if path.starts_with("wheels/") => path
			.strip_prefix("wheels/")
			.and_then(|name| name.strip_suffix(".whl"))
			.is_some_and(|name| name == digest),
		path if path.starts_with("bin/") => {
			path.strip_prefix("bin/").is_some_and(|name| name == digest)
		},
		_ => true,
	}
}

fn hash_bytes(value: &str) -> Result<Bytes, BundleError> {
	if value.len() != 64
		|| !value
			.bytes()
			.all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
	{
		return Err(BundleError::Layout(sf!("bundle digest is not lowercase SHA-256 hex",)));
	}
	let hash = hex::decode(value)
		.into_array::<32>()
		.map_err(|_| BundleError::Layout(sf!("bundle digest is not SHA-256")))?;
	Ok(Bytes::copy_from_slice(&hash))
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use omp_core::{Hash32, Str, sf};

	use super::{BundleFile, pack_bundle, unpack_bundle};

	#[test]
	fn bundle_round_trip_preserves_content_addressed_layout() {
		let wheel = Bytes::from_static(b"wheel");
		let digest = Hash32::sum(&wheel).to_hex();
		let archive = pack_bundle("omp-test", vec![sf!("aarch64-apple-darwin")], vec![
			BundleFile { path: sf!("omp.lock"), contents: Bytes::from_static(b"version = 1\n") },
			BundleFile {
				path:     Str::from(format!("wheels/{}.whl", digest.as_str())),
				contents: wheel,
			},
		])
		.unwrap();
		let bundle = unpack_bundle(&archive).unwrap();
		assert_eq!(bundle.files.len(), 2);
		assert_eq!(bundle.manifest.format, 1);
	}
}
