//! Journal branch-pruning integration coverage.

use omp_core::Str;
use omp_journal::{EntryDraft, Journal, Kind, gc::prune_abandoned, kind::KindName, live_chain};

fn draft(
	kind: KindName,
	by: Option<omp_journal::EntryId>,
	prior: Option<omp_journal::EntryId>,
) -> EntryDraft {
	EntryDraft { kind: Kind::known(kind), by, prior, label: None, data: Str::new_static("{}") }
}

#[test]
fn prune_of_branched_journal_preserves_live_snapshot_and_shrinks_bytes() {
	let directory = tempfile::tempdir().expect("temporary directory");
	let path = directory.path().join("branched.oms");
	let mut journal = Journal::create(&path).expect("journal creates");
	let genesis = journal
		.append(draft(KindName::Journal, None, None))
		.expect("genesis appends");
	let branch_point = journal
		.append(draft(KindName::TurnStart, Some(genesis.id), None))
		.expect("branch point appends");
	let abandoned = journal
		.append(draft(KindName::MsgUser, Some(branch_point.id), None))
		.expect("abandoned message appends");
	journal
		.append(draft(KindName::TurnStart, Some(branch_point.id), Some(branch_point.id)))
		.expect("replacement turn appends");
	journal
		.append(draft(KindName::MsgUser, Some(branch_point.id), None))
		.expect("replacement message appends");
	drop(journal);

	let (_, before_entries) = Journal::open(&path).expect("journal opens before prune");
	let before_snapshot: Vec<_> = live_chain(&before_entries).cloned().collect();
	assert!(!before_snapshot.iter().any(|entry| entry.id == abandoned.id));
	let before_bytes = std::fs::metadata(&path).expect("metadata").len();

	let report = prune_abandoned(&path).expect("journal prunes");
	let (_, after_entries) = Journal::open(&path).expect("journal opens after prune");
	let after_snapshot: Vec<_> = live_chain(&after_entries).cloned().collect();

	assert_eq!(after_snapshot, before_snapshot);
	assert_eq!(report.entries_pruned(), 1);
	assert_eq!(report.entries_after, after_entries.len());
	assert!(report.bytes_after < before_bytes);
	assert_eq!(std::fs::metadata(path).expect("metadata").len(), report.bytes_after);
}
