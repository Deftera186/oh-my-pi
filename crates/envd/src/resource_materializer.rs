//! Environment-owned scoped resource materialization and lease cleanup.

use std::{
	collections::HashMap,
	fs::{self, OpenOptions},
	io::{self, Read as _, Write as _},
	path::{Component, Path, PathBuf},
	sync::{
		Arc, Weak,
		atomic::{AtomicU64, Ordering},
	},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use omp_core::{Hash32, hash32::Hasher};
use omp_proto::env::v1::{
	MaterializationLease, MaterializationReleased, MaterializeRequest, ReleaseMaterialization,
};
use parking_lot::Mutex;
use thiserror::Error;
use tokio::{task, time};
use url::Url;

const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_TTL: Duration = Duration::from_secs(60 * 60);

/// Scoped materialization failure.
#[derive(Debug, Error)]
pub enum MaterializationError {
	/// The resource URI is malformed.
	#[error("materialization resource URI is invalid")]
	InvalidUri,
	/// This resource is logical rather than path-backed at the Environment.
	#[error("resource scheme is not path-backed by this Environment")]
	UnsupportedScheme,
	/// The resolved source escapes its granted root.
	#[error("materialization source is outside its Environment grant")]
	OutsideGrant,
	/// The resolved resource does not exist.
	#[error("materialization source does not exist")]
	NotFound,
	/// Symbolic links are rejected during recursive copy.
	#[error("materialization source contains a symbolic link")]
	SymbolicLink,
	/// The resource exceeds the caller's byte ceiling.
	#[error("materialization source exceeds the {limit}-byte ceiling")]
	TooLarge {
		/// Effective byte ceiling.
		limit: u64,
	},
	/// A filesystem operation failed.
	#[error("materialization filesystem operation failed")]
	Io(#[from] io::Error),
}

#[derive(Clone)]
pub(crate) struct ResourceMaterializer {
	inner: Arc<Inner>,
}

struct Inner {
	workspace_root: PathBuf,
	local_root:     PathBuf,
	lease_root:     PathBuf,
	next_lease:     AtomicU64,
	leases:         Mutex<HashMap<Bytes, LeaseRecord>>,
}

struct LeaseRecord {
	path:          PathBuf,
	expires_at_ms: u64,
	write_back:    Option<PathBuf>,
}

impl ResourceMaterializer {
	pub(crate) fn open(
		workspace_root: &Path,
		state_dir: &Path,
	) -> Result<Self, MaterializationError> {
		let workspace_root = fs::canonicalize(workspace_root)?;
		let local_root = state_dir.join("local");
		let lease_root = state_dir.join("materializations");
		private_directory(&local_root)?;
		private_directory(&lease_root)?;
		let local_root = fs::canonicalize(local_root)?;
		let lease_root = fs::canonicalize(lease_root)?;
		Ok(Self {
			inner: Arc::new(Inner {
				workspace_root,
				local_root,
				lease_root,
				next_lease: AtomicU64::new(1),
				leases: Mutex::new(HashMap::new()),
			}),
		})
	}

	pub(crate) async fn materialize(
		&self,
		request: MaterializeRequest,
	) -> Result<MaterializationLease, MaterializationError> {
		let inner = Arc::clone(&self.inner);
		let lease = task::spawn_blocking(move || inner.materialize(request))
			.await
			.map_err(|source| io::Error::other(source))??;
		self.schedule_cleanup(lease.lease_id.clone(), lease.expires_at_ms);
		Ok(lease)
	}

	pub(crate) async fn release(
		&self,
		request: ReleaseMaterialization,
	) -> Result<MaterializationReleased, MaterializationError> {
		let lease_id = request.lease_id;
		let inner = Arc::clone(&self.inner);
		let released = lease_id.clone();
		task::spawn_blocking(move || inner.release(&lease_id))
			.await
			.map_err(|source| io::Error::other(source))??;
		Ok(MaterializationReleased { lease_id: released })
	}

	fn schedule_cleanup(&self, lease_id: Bytes, expires_at_ms: u64) {
		let weak = Arc::downgrade(&self.inner);
		tokio::spawn(async move {
			let now = now_ms();
			if expires_at_ms > now {
				time::sleep(Duration::from_millis(expires_at_ms - now)).await;
			}
			let _ = task::spawn_blocking(move || cleanup_expired(weak, lease_id, expires_at_ms)).await;
		});
	}
}

impl Inner {
	fn materialize(
		&self,
		request: MaterializeRequest,
	) -> Result<MaterializationLease, MaterializationError> {
		self.reap_expired()?;
		let mutable_local = request.resource_uri.starts_with("local://");
		let source = self.resolve_source(&request.resource_uri, mutable_local)?;
		let metadata = fs::symlink_metadata(&source).map_err(|error| {
			if error.kind() == io::ErrorKind::NotFound {
				MaterializationError::NotFound
			} else {
				MaterializationError::Io(error)
			}
		})?;
		if metadata.file_type().is_symlink() {
			return Err(MaterializationError::SymbolicLink);
		}

		let lease_id = self.new_lease_id();
		let lease_name = omp_core::hex::encode(&lease_id).to_string();
		let lease_path = self.lease_root.join(lease_name);
		private_directory(&lease_path)?;
		let destination = lease_path.join("resource");
		let limit = if request.max_bytes == 0 {
			DEFAULT_MAX_BYTES
		} else {
			request.max_bytes.min(DEFAULT_MAX_BYTES)
		};
		let copied = copy_resource(&source, &destination, limit);
		let (size, content_hash) = match copied {
			Ok(copied) => copied,
			Err(error) => {
				let _ = fs::remove_dir_all(&lease_path);
				return Err(error);
			},
		};
		let ttl = if request.ttl_ms == 0 {
			DEFAULT_TTL
		} else {
			Duration::from_millis(request.ttl_ms).min(MAX_TTL)
		};
		let expires_at_ms = now_ms().saturating_add(ttl.as_millis().try_into().unwrap_or(u64::MAX));
		let environment_uri = if metadata.is_dir() {
			Url::from_directory_path(&destination)
		} else {
			Url::from_file_path(&destination)
		}
		.map_err(|()| MaterializationError::InvalidUri)?
		.to_string();
		self.leases.lock().insert(lease_id.clone(), LeaseRecord {
			path: lease_path,
			expires_at_ms,
			write_back: mutable_local.then_some(source),
		});
		Ok(MaterializationLease {
			lease_id,
			environment_uri,
			expires_at_ms,
			size,
			content_hash: Bytes::copy_from_slice(content_hash.as_bytes()),
		})
	}

	fn release(&self, lease_id: &[u8]) -> Result<(), MaterializationError> {
		let record = self.leases.lock().remove(lease_id);
		if let Some(record) = record {
			finalize_record(&record)?;
		}
		Ok(())
	}

	fn reap_expired(&self) -> Result<(), MaterializationError> {
		let now = now_ms();
		let expired = {
			let mut leases = self.leases.lock();
			let ids = leases
				.iter()
				.filter(|(_, lease)| lease.expires_at_ms <= now)
				.map(|(id, _)| id.clone())
				.collect::<Vec<_>>();
			ids.into_iter()
				.filter_map(|id| leases.remove(&id))
				.collect::<Vec<_>>()
		};
		for lease in expired {
			finalize_record(&lease)?;
		}
		Ok(())
	}

	fn resolve_source(
		&self,
		resource_uri: &str,
		create_local: bool,
	) -> Result<PathBuf, MaterializationError> {
		let (candidate, grant) = if let Some(resource) = resource_uri.strip_prefix("local://") {
			let candidate = safe_relative(&self.local_root, resource)?;
			if create_local && !candidate.exists() {
				let parent = candidate.parent().ok_or(MaterializationError::InvalidUri)?;
				fs::create_dir_all(parent)?;
				let canonical_parent = fs::canonicalize(parent)?;
				if !canonical_parent.starts_with(&self.local_root) {
					return Err(MaterializationError::OutsideGrant);
				}
				OpenOptions::new()
					.write(true)
					.create_new(true)
					.open(&candidate)?;
				private_file(&candidate)?;
			}
			(candidate, &self.local_root)
		} else {
			let url = Url::parse(resource_uri).map_err(|_| MaterializationError::InvalidUri)?;
			if url.scheme() != "file" {
				return Err(MaterializationError::UnsupportedScheme);
			}
			(
				url.to_file_path()
					.map_err(|()| MaterializationError::InvalidUri)?,
				&self.workspace_root,
			)
		};
		let canonical = fs::canonicalize(candidate).map_err(|error| {
			if error.kind() == io::ErrorKind::NotFound {
				MaterializationError::NotFound
			} else {
				MaterializationError::Io(error)
			}
		})?;
		if !canonical.starts_with(grant) {
			return Err(MaterializationError::OutsideGrant);
		}
		Ok(canonical)
	}

	fn new_lease_id(&self) -> Bytes {
		let sequence = self.next_lease.fetch_add(1, Ordering::Relaxed);
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_nanos();
		let mut id = [0_u8; 24];
		id[..16].copy_from_slice(&now.to_be_bytes());
		id[16..].copy_from_slice(&sequence.to_be_bytes());
		Bytes::copy_from_slice(&id)
	}
}

impl Drop for Inner {
	fn drop(&mut self) {
		let leases = self.leases.get_mut();
		for (_, lease) in leases.drain() {
			let _ = finalize_record(&lease);
		}
	}
}

fn cleanup_expired(weak: Weak<Inner>, lease_id: Bytes, expires_at_ms: u64) {
	let Some(inner) = weak.upgrade() else { return };
	let record = {
		let mut leases = inner.leases.lock();
		if leases
			.get(&lease_id)
			.is_some_and(|lease| lease.expires_at_ms == expires_at_ms)
		{
			leases.remove(&lease_id)
		} else {
			None
		}
	};
	if let Some(record) = record {
		let _ = finalize_record(&record);
	}
}

fn safe_relative(root: &Path, resource: &str) -> Result<PathBuf, MaterializationError> {
	let resource = resource.trim_start_matches('/');
	if resource.is_empty() || resource.contains('\\') {
		return Err(MaterializationError::InvalidUri);
	}
	let relative = Path::new(resource);
	if relative
		.components()
		.any(|component| !matches!(component, Component::Normal(_)))
	{
		return Err(MaterializationError::OutsideGrant);
	}
	Ok(root.join(relative))
}

fn copy_resource(
	source: &Path,
	destination: &Path,
	limit: u64,
) -> Result<(u64, Hash32), MaterializationError> {
	let mut hasher = Hash32::hasher();
	let mut size = 0_u64;
	copy_entry(source, destination, source, limit, &mut size, &mut hasher)?;
	Ok((size, hasher.finalize()))
}

fn copy_entry(
	source: &Path,
	destination: &Path,
	root: &Path,
	limit: u64,
	size: &mut u64,
	hasher: &mut Hasher,
) -> Result<(), MaterializationError> {
	let metadata = fs::symlink_metadata(source)?;
	if metadata.file_type().is_symlink() {
		return Err(MaterializationError::SymbolicLink);
	}
	let relative = source.strip_prefix(root).unwrap_or(Path::new(""));
	hasher.update(relative.as_os_str().as_encoded_bytes());
	if metadata.is_dir() {
		private_directory(destination)?;
		let mut entries = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
		entries.sort_unstable_by_key(fs::DirEntry::file_name);
		for entry in entries {
			copy_entry(
				&entry.path(),
				&destination.join(entry.file_name()),
				root,
				limit,
				size,
				hasher,
			)?;
		}
		return Ok(());
	}
	if !metadata.is_file() {
		return Err(MaterializationError::InvalidUri);
	}
	let next = size.saturating_add(metadata.len());
	if next > limit {
		return Err(MaterializationError::TooLarge { limit });
	}
	let mut input = fs::File::open(source)?;
	let mut output = fs::File::create(destination)?;
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt as _;
		output.set_permissions(fs::Permissions::from_mode(0o600))?;
	}
	let mut buffer = [0_u8; 64 * 1024];
	loop {
		let read = input.read(&mut buffer)?;
		if read == 0 {
			break;
		}
		output.write_all(&buffer[..read])?;
		hasher.update(&buffer[..read]);
	}
	*size = next;
	Ok(())
}

fn write_back_local(source: &Path, target: &Path) -> Result<(), MaterializationError> {
	let mut size = 0;
	let mut hasher = Hash32::hasher();
	copy_entry(source, target, source, DEFAULT_MAX_BYTES, &mut size, &mut hasher)
}

fn finalize_record(record: &LeaseRecord) -> Result<(), MaterializationError> {
	if let Some(target) = &record.write_back {
		write_back_local(&record.path.join("resource"), target)?;
	}
	remove_lease_path(&record.path)
}

fn private_file(path: &Path) -> io::Result<()> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt as _;
		fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
	}
	Ok(())
}

fn private_directory(path: &Path) -> io::Result<()> {
	fs::create_dir_all(path)?;
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt as _;
		fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
	}
	Ok(())
}

fn remove_lease_path(path: &Path) -> Result<(), MaterializationError> {
	match fs::remove_dir_all(path) {
		Ok(()) => Ok(()),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
		Err(error) => Err(error.into()),
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn lease_materializes_inside_scratch_and_release_is_idempotent() {
		let workspace = tempfile::tempdir().unwrap();
		let state = tempfile::tempdir().unwrap();
		let source = workspace.path().join("input.txt");
		fs::write(&source, b"materialized").unwrap();
		let materializer = ResourceMaterializer::open(workspace.path(), state.path()).unwrap();
		let lease = materializer
			.materialize(MaterializeRequest {
				resource_uri: Url::from_file_path(&source).unwrap().to_string(),
				max_bytes: 64,
				ttl_ms: 60_000,
				..Default::default()
			})
			.await
			.unwrap();
		let environment_path = Url::parse(&lease.environment_uri)
			.unwrap()
			.to_file_path()
			.unwrap();
		assert!(
			environment_path
				.starts_with(std::fs::canonicalize(state.path().join("materializations")).unwrap(),)
		);
		assert_eq!(fs::read(&environment_path).unwrap(), b"materialized");
		let release =
			ReleaseMaterialization { lease_id: lease.lease_id.clone(), ..Default::default() };
		materializer.release(release.clone()).await.unwrap();
		assert!(!environment_path.exists());
		materializer.release(release).await.unwrap();
	}

	#[tokio::test]
	async fn mutable_local_parent_is_authorized_then_written_back_on_release() {
		let workspace = tempfile::tempdir().unwrap();
		let state = tempfile::tempdir().unwrap();
		let materializer = ResourceMaterializer::open(workspace.path(), state.path()).unwrap();
		let lease = materializer
			.materialize(MaterializeRequest {
				resource_uri: String::from("local://nested/result.txt"),
				max_bytes: 64,
				ttl_ms: 60_000,
				..Default::default()
			})
			.await
			.unwrap();
		let environment_path = Url::parse(&lease.environment_uri)
			.unwrap()
			.to_file_path()
			.unwrap();
		fs::write(&environment_path, b"changed").unwrap();
		materializer
			.release(ReleaseMaterialization { lease_id: lease.lease_id, ..Default::default() })
			.await
			.unwrap();
		assert_eq!(fs::read(state.path().join("local/nested/result.txt")).unwrap(), b"changed");
	}

	#[tokio::test]
	async fn containment_rejects_workspace_escape_and_cleans_failed_lease() {
		let workspace = tempfile::tempdir().unwrap();
		let state = tempfile::tempdir().unwrap();
		let outside = tempfile::NamedTempFile::new().unwrap();
		let materializer = ResourceMaterializer::open(workspace.path(), state.path()).unwrap();
		let error = materializer
			.materialize(MaterializeRequest {
				resource_uri: Url::from_file_path(outside.path()).unwrap().to_string(),
				max_bytes: 64,
				..Default::default()
			})
			.await
			.unwrap_err();
		assert!(matches!(error, MaterializationError::OutsideGrant));
		assert_eq!(
			fs::read_dir(state.path().join("materializations"))
				.unwrap()
				.count(),
			0
		);
	}
}
