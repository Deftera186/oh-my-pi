//! Journal-owner bridge for bounded collaboration snapshot and live
//! replication.

use std::mem;

use bytes::Bytes;
use omp_agent::{
	Journal, JournalError, ReplicationEvent, ReplicationRecord, ReplicationSubscription,
	ReplicationTerminal, ReplicationVisibility,
};
use omp_collab::{
	codec::REPEATED_MAX_COUNT,
	replication::{MAX_REPLICATED_PAYLOAD_BYTES, ReplicationError, shrink_for_replication},
};
use omp_proto::collab::v1::{JournalRecord, SnapshotChunk, VisibilityClass};
use prost::Message as _;
use thiserror::Error;

/// Soft protobuf size target for each initial snapshot frame.
pub const SNAPSHOT_CHUNK_SOFT_BYTES: usize = 512 * 1024;
const OMITTED_RECORD_JSON: &[u8] = br#"{"ts":0,"k":"collab_omitted","rev":"host_local.v1"}"#;

/// Catch-up snapshot plus ordered live records from the authoritative journal.
pub struct HostJournalBridge {
	subscription: ReplicationSubscription,
}

impl HostJournalBridge {
	/// Constructs a bridge from a subscription captured by the agent owner.
	pub const fn from_subscription(subscription: ReplicationSubscription) -> Self {
		Self { subscription }
	}

	/// Captures a race-free catch-up and registers bounded live delivery.
	pub fn subscribe(journal: &mut Journal) -> Result<Self, HostBridgeError> {
		Ok(Self { subscription: journal.subscribe_replication()? })
	}

	/// Builds ordered soft-bounded snapshot chunks, always ending with `final`.
	pub fn snapshot_chunks(&mut self) -> Result<Vec<SnapshotChunk>, HostBridgeError> {
		let host_revision_watermark = self.subscription.host_revision();
		let mut chunks = Vec::new();
		let mut entries = Vec::new();
		let mut encoded_bytes = 0_usize;
		let mut expected_revision = 1_u64;
		while let Some(record) = self.subscription.next_catch_up() {
			let record = wire_record(record)?;
			if record.revision != expected_revision {
				return Err(HostBridgeError::Revision {
					expected: expected_revision,
					actual:   record.revision,
				});
			}
			expected_revision = expected_revision.saturating_add(1);
			let record_bytes = record.encoded_len().saturating_add(10);
			if !entries.is_empty()
				&& (entries.len() >= REPEATED_MAX_COUNT
					|| encoded_bytes.saturating_add(record_bytes) > SNAPSHOT_CHUNK_SOFT_BYTES)
			{
				chunks.push(SnapshotChunk {
					entries: mem::take(&mut entries),
					r#final: false,
					host_revision_watermark,
				});
				encoded_bytes = 0;
			}
			encoded_bytes = encoded_bytes.saturating_add(record_bytes);
			entries.push(record);
		}
		let record_count = expected_revision.saturating_sub(1);
		if record_count != host_revision_watermark {
			return Err(HostBridgeError::SnapshotFence {
				records:   record_count,
				watermark: host_revision_watermark,
			});
		}
		chunks.push(SnapshotChunk { entries, r#final: true, host_revision_watermark });
		Ok(chunks)
	}

	/// Receives the next live record or explicit bounded-lag terminal.
	pub async fn recv(&self) -> Result<HostReplicationEvent, HostBridgeError> {
		match self.subscription.recv().await? {
			ReplicationEvent::Record(record) => Ok(HostReplicationEvent::Record(wire_record(record)?)),
			ReplicationEvent::Terminal(terminal) => Ok(HostReplicationEvent::Terminal(terminal)),
		}
	}
}

/// Live host bridge delivery.
#[derive(Clone, Debug)]
pub enum HostReplicationEvent {
	/// One ordered committed transcript-v4 record.
	Record(JournalRecord),
	/// Bounded lag or journal-owner closure.
	Terminal(ReplicationTerminal),
}

/// Host journal bridge failure.
#[derive(Debug, Error)]
pub enum HostBridgeError {
	/// Journal subscription failed.
	#[error(transparent)]
	Journal(#[from] JournalError),
	/// The live journal owner closed its channel.
	#[error("journal replication subscription closed without a terminal event")]
	Closed(#[from] flume::RecvError),
	/// A public transcript record was not valid JSON.
	#[error("public transcript-v4 record is not valid JSON")]
	Json(#[source] serde_json::Error),
	/// Deterministic per-entry shrinking could not fit the hard frame ceiling.
	#[error(transparent)]
	Shrink(#[from] ReplicationError),
	/// Catch-up records were not physically contiguous.
	#[error("host replication expected revision {expected}, received {actual}")]
	Revision {
		/// Next required physical revision.
		expected: u64,
		/// Received physical revision.
		actual:   u64,
	},
	/// Catch-up records did not cover the journal's authoritative watermark.
	#[error("host replication snapshot has {records} records for watermark {watermark}")]
	SnapshotFence {
		/// Number of physical catch-up records.
		records:   u64,
		/// Authoritative host revision.
		watermark: u64,
	},
}

fn wire_record(record: ReplicationRecord) -> Result<JournalRecord, HostBridgeError> {
	let (visibility_class, transcript_v4_json) = match record.visibility {
		ReplicationVisibility::PublicTranscript => {
			let json = if record.json.len() <= MAX_REPLICATED_PAYLOAD_BYTES {
				record.json
			} else {
				let value = serde_json::from_slice(&record.json).map_err(HostBridgeError::Json)?;
				Bytes::from(shrink_for_replication(&value)?.encode()?)
			};
			(VisibilityClass::PublicTranscript as i32, json)
		},
		ReplicationVisibility::HostLocalOmitted => {
			(VisibilityClass::HostLocalOmitted as i32, Bytes::from_static(OMITTED_RECORD_JSON))
		},
	};
	Ok(JournalRecord { revision: record.revision, transcript_v4_json, visibility_class })
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn host_local_records_are_replaced_by_a_fixed_secret_free_marker() {
		let wire = wire_record(ReplicationRecord {
			revision:   7,
			visibility: ReplicationVisibility::HostLocalOmitted,
			json:       Bytes::from_static(br#"{"credential":"must-not-leak"}"#),
		})
		.expect("wire omission");
		assert_eq!(wire.revision, 7);
		assert_eq!(wire.visibility_class, VisibilityClass::HostLocalOmitted as i32,);
		assert_eq!(wire.transcript_v4_json.as_ref(), OMITTED_RECORD_JSON);
	}
}
