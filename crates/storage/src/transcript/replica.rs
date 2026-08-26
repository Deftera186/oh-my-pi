//! Crash-safe transcript-v4 replicas for remote collaboration sessions.

use std::{
	fs::File,
	io,
	io::{BufRead as _, BufReader, Read as _, Write as _},
	path::{Path, PathBuf},
};

use bytes::Bytes;
use omp_core::Str;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{self as transcript, Event, Kind, SessionId, Writer};
use crate::atomic;

const HEADER_MAX_BYTES: usize = 16 * 1024;
const OMISSION_KIND: &str = "collab_omitted";
const OMISSION_REVISION: &str = "host_local.v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OmissionMarker {
	#[serde(rename = "ts")]
	_ts: u64,
	k:   Str,
	rev: Str,
}

/// Secret-free provenance stored in a collaboration replica's v4 header.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RemoteProvenance {
	/// Stable host transcript identity.
	pub host_session: SessionId,
	/// Stable encrypted-room identity; never a room key or write token.
	pub room_id:      Str,
	/// Host transcript creation time, retained for presentation only.
	pub host_created: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ReplicaHeader {
	v:       u8,
	id:      SessionId,
	created: u64,
	cwd:     PathBuf,
	remote:  RemoteProvenance,
}

/// Visibility assigned by the host journal owner to one physical revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ReplicaVisibility {
	/// Exact public transcript-v4 bytes.
	PublicTranscript,
	/// Exact public presentation state encoded as a transcript record.
	PublicPresentation,
	/// A non-semantic marker replacing a strictly host-local record.
	HostLocalOmitted,
}

/// One host-revision-fenced transcript-v4 record received by a guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRecord {
	/// One-based physical host-journal revision.
	pub revision:   u64,
	/// Host-assigned visibility class.
	pub visibility: ReplicaVisibility,
	/// Exact record JSON without a trailing newline.
	pub json:       Bytes,
}

/// Generation token fencing live frames from an older connection or reseed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicaFence(u64);

/// Collaboration replica failure.
#[derive(Debug, Error)]
pub enum ReplicaError {
	/// Replica storage failed.
	#[error(transparent)]
	Transcript(#[from] transcript::Error),
	/// Atomic reseed failed before replacing the prior durable replica.
	#[error(transparent)]
	Atomic(#[from] atomic::Error),
	/// Header I/O failed.
	#[error("replica header I/O failed")]
	Io(#[source] io::Error),
	/// The bounded replica header was not newline-terminated.
	#[error("replica header exceeds 16 KiB or is unterminated")]
	HeaderTooLarge,
	/// The requested room differs from the durable remote provenance.
	#[error("replica belongs to a different collaboration room")]
	RoomMismatch,
	/// A frame belongs to a superseded connection or reseed generation.
	#[error("replica frame belongs to a stale generation")]
	StaleFence,
	/// A live or snapshot record was not the exact next host revision.
	#[error("replica expected host revision {expected}, received {actual}")]
	Revision {
		/// Required next revision.
		expected: u64,
		/// Received revision.
		actual:   u64,
	},
	/// A completed snapshot did not cover its declared host watermark.
	#[error("replica snapshot has {records} records for host watermark {watermark}")]
	SnapshotLength {
		/// Declared host revision watermark.
		watermark: u64,
		/// Number of physical records supplied.
		records:   usize,
	},
	/// Public visibility was attached to an undecodable transcript record.
	#[error("public replica record is not a canonical transcript-v4 event")]
	PublicUndecodable,
	/// A host-local visibility slot did not contain the fixed omission marker.
	#[error("host-local replica record is not a valid omission marker")]
	InvalidOmission,
}

/// Durable collaboration replica with an immutable local identity and cwd.
pub struct Replica {
	path:       PathBuf,
	header:     ReplicaHeader,
	writer:     Writer,
	generation: u64,
	watermark:  u64,
}

impl Replica {
	/// Creates an empty remote-provenance transcript-v4 replica.
	///
	/// `local_cwd` is the guest's preserved workspace root. The API accepts no
	/// host cwd or credentials, so neither can be adopted into durable state.
	pub fn create(
		path: &Path,
		id: SessionId,
		created: u64,
		local_cwd: PathBuf,
		remote: RemoteProvenance,
	) -> Result<Self, ReplicaError> {
		let header = ReplicaHeader { v: 4, id, created, cwd: local_cwd, remote };
		write_replica(path, &header, &[], || !path.exists())?;
		let writer = Writer::open_append(path)?;
		Ok(Self { path: path.to_owned(), header, writer, generation: 0, watermark: 0 })
	}

	/// Opens a durable replica without importing remote workspace or auth state.
	pub fn open(path: &Path, expected_room_id: &str) -> Result<Self, ReplicaError> {
		let header = read_replica_header(path)?;
		if header.remote.room_id != expected_room_id {
			return Err(ReplicaError::RoomMismatch);
		}
		let log = transcript::load(path)?;
		let watermark = u64::try_from(log.len()).expect("transcript event count fits in u64");
		let writer = Writer::open_append(path)?;
		Ok(Self { path: path.to_owned(), header, writer, generation: 0, watermark })
	}

	/// Returns the stable local replica session identity.
	pub const fn session_id(&self) -> &SessionId {
		&self.header.id
	}

	/// Returns the guest-local workspace root retained by the replica header.
	pub fn local_cwd(&self) -> &Path {
		&self.header.cwd
	}

	/// Returns the secret-free host provenance.
	pub const fn remote_provenance(&self) -> &RemoteProvenance {
		&self.header.remote
	}

	/// Returns the highest durably applied physical host revision.
	pub const fn host_revision_watermark(&self) -> u64 {
		self.watermark
	}

	/// Invalidates prior live frames and begins a new snapshot/live generation.
	pub fn begin_reseed(&mut self) -> ReplicaFence {
		self.generation = self.generation.wrapping_add(1);
		ReplicaFence(self.generation)
	}

	/// Atomically replaces replica records while preserving local identity and
	/// cwd.
	pub fn commit_reseed(
		&mut self,
		fence: ReplicaFence,
		host_revision_watermark: u64,
		records: &[RemoteRecord],
	) -> Result<(), ReplicaError> {
		self.check_fence(fence)?;
		if u64::try_from(records.len()).expect("record count fits in u64") != host_revision_watermark
		{
			return Err(ReplicaError::SnapshotLength {
				watermark: host_revision_watermark,
				records:   records.len(),
			});
		}
		for (offset, record) in records.iter().enumerate() {
			let expected = u64::try_from(offset)
				.expect("record index fits in u64")
				.saturating_add(1);
			if record.revision != expected {
				return Err(ReplicaError::Revision { expected, actual: record.revision });
			}
			validate_record(record)?;
		}
		write_replica(&self.path, &self.header, records, || self.generation == fence.0)?;
		self.check_fence(fence)?;
		self.writer = Writer::open_append(&self.path)?;
		self.watermark = host_revision_watermark;
		Ok(())
	}

	/// Durably appends the exact next live host revision for the active
	/// generation.
	pub fn append_live(
		&mut self,
		fence: ReplicaFence,
		record: &RemoteRecord,
	) -> Result<u64, ReplicaError> {
		self.check_fence(fence)?;
		let expected = self.watermark.saturating_add(1);
		if record.revision != expected {
			return Err(ReplicaError::Revision { expected, actual: record.revision });
		}
		let event = validate_record(record)?;
		let index = self.writer.append(&event)?;
		debug_assert_eq!(index.saturating_add(1), record.revision);
		self.watermark = record.revision;
		Ok(self.watermark)
	}

	fn check_fence(&self, fence: ReplicaFence) -> Result<(), ReplicaError> {
		if fence.0 != self.generation {
			return Err(ReplicaError::StaleFence);
		}
		Ok(())
	}
}

fn validate_record(record: &RemoteRecord) -> Result<Event, ReplicaError> {
	let event = transcript::read_line(&record.json)?;
	match (record.visibility, &event.kind) {
		(ReplicaVisibility::HostLocalOmitted, Kind::EntryUndecodable(_)) => {
			let marker: OmissionMarker =
				serde_json::from_slice(&record.json).map_err(|_| ReplicaError::InvalidOmission)?;
			if marker.k != OMISSION_KIND || marker.rev != OMISSION_REVISION {
				return Err(ReplicaError::InvalidOmission);
			}
		},
		(ReplicaVisibility::HostLocalOmitted, _) => return Err(ReplicaError::InvalidOmission),
		(
			ReplicaVisibility::PublicTranscript | ReplicaVisibility::PublicPresentation,
			Kind::EntryUndecodable(_),
		) => return Err(ReplicaError::PublicUndecodable),
		_ => {},
	}
	Ok(event)
}

fn read_replica_header(path: &Path) -> Result<ReplicaHeader, ReplicaError> {
	let file = File::open(path).map_err(ReplicaError::Io)?;
	let mut reader = BufReader::new(file)
		.take(u64::try_from(HEADER_MAX_BYTES + 1).expect("header bound fits in u64"));
	let mut line = Vec::with_capacity(512);
	reader
		.read_until(b'\n', &mut line)
		.map_err(ReplicaError::Io)?;
	if line.len() > HEADER_MAX_BYTES || line.last() != Some(&b'\n') {
		return Err(ReplicaError::HeaderTooLarge);
	}
	line.pop();
	let header: ReplicaHeader = serde_json::from_slice(&line).map_err(transcript::Error::from)?;
	if header.v != 4 {
		return Err(transcript::Error::InvalidHeaderVersion(header.v).into());
	}
	Ok(header)
}

fn write_replica(
	path: &Path,
	header: &ReplicaHeader,
	records: &[RemoteRecord],
	guard: impl FnOnce() -> bool,
) -> Result<(), ReplicaError> {
	atomic::commit_with(path, guard, |file| {
		serde_json::to_writer(&mut *file, header).map_err(io::Error::other)?;
		file.write_all(b"\n")?;
		for record in records {
			file.write_all(&record.json)?;
			file.write_all(b"\n")?;
		}
		Ok(())
	})?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use tempfile::tempdir;

	use super::*;

	fn provenance() -> RemoteProvenance {
		RemoteProvenance {
			host_session: SessionId(omp_core::sf!("host")),
			room_id:      Str::from("room"),
			host_created: 7,
		}
	}

	fn record(revision: u64, ts: u64) -> RemoteRecord {
		RemoteRecord {
			revision,
			visibility: ReplicaVisibility::PublicTranscript,
			json: Bytes::from(format!(r#"{{"ts":{ts},"k":"reset"}}"#)),
		}
	}

	fn omission(revision: u64, ts: u64) -> RemoteRecord {
		RemoteRecord {
			revision,
			visibility: ReplicaVisibility::HostLocalOmitted,
			json: Bytes::from(format!(r#"{{"ts":{ts},"k":"collab_omitted","rev":"host_local.v1"}}"#)),
		}
	}

	#[test]
	fn reseed_preserves_local_identity_and_fences_old_live_frames() {
		let directory = tempdir().expect("temporary directory");
		let path = directory.path().join("replica.jsonl");
		let local = directory.path().join("workspace");
		let mut replica = Replica::create(
			&path,
			SessionId(omp_core::sf!("replica")),
			1,
			local.clone(),
			provenance(),
		)
		.expect("create replica");
		let old = replica.begin_reseed();
		let live = replica.begin_reseed();
		assert!(matches!(replica.append_live(old, &record(1, 1)), Err(ReplicaError::StaleFence)));
		replica
			.commit_reseed(live, 1, &[record(1, 1)])
			.expect("commit snapshot");
		assert_eq!(replica.session_id().as_str(), "replica");
		assert_eq!(replica.local_cwd(), local);
		assert_eq!(replica.host_revision_watermark(), 1);
	}

	#[test]
	fn reseed_omissions_remain_physical_across_reopen_and_live_append() {
		let directory = tempdir().expect("temporary directory");
		let path = directory.path().join("replica.jsonl");
		let mut replica = Replica::create(
			&path,
			SessionId(omp_core::sf!("replica")),
			1,
			directory.path().join("local"),
			provenance(),
		)
		.expect("create replica");
		let fence = replica.begin_reseed();
		replica
			.commit_reseed(fence, 3, &[record(1, 1), omission(2, 2), record(3, 3)])
			.expect("commit snapshot with physical omission");
		drop(replica);

		let loaded = transcript::load(&path).expect("load replica transcript");
		assert_eq!(loaded.len(), 3);
		assert!(matches!(
			loaded.get(1),
			Some(transcript::Entry::Ok(event))
				if matches!(&event.kind, Kind::EntryUndecodable(_))
		));
		let mut reopened = Replica::open(&path, "room").expect("reopen replica");
		assert_eq!(reopened.host_revision_watermark(), 3);
		let fence = reopened.begin_reseed();
		assert_eq!(
			reopened
				.append_live(fence, &record(4, 4))
				.expect("append next physical revision"),
			4
		);
	}

	#[test]
	fn failed_reseed_keeps_prior_durable_replica() {
		let directory = tempdir().expect("temporary directory");
		let path = directory.path().join("replica.jsonl");
		let mut replica = Replica::create(
			&path,
			SessionId(omp_core::sf!("replica")),
			1,
			directory.path().join("local"),
			provenance(),
		)
		.expect("create replica");
		let fence = replica.begin_reseed();
		replica
			.commit_reseed(fence, 1, &[record(1, 1)])
			.expect("initial snapshot");
		let fence = replica.begin_reseed();
		assert!(matches!(
			replica.commit_reseed(fence, 2, &[record(1, 2)]),
			Err(ReplicaError::SnapshotLength { .. })
		));
		let reopened = Replica::open(&path, "room").expect("open prior replica");
		assert_eq!(reopened.host_revision_watermark(), 1);
	}
}
