//! Laws for append, recovery, and branch selection.

use std::fs;

use omp_core::Str;
use omp_journal::{
	Entry, EntryDraft, Journal, JournalError, Kind, abandoned,
	kind::{JOURNAL, PATCH, TURN_START},
	live_chain,
};
use proptest::prelude::*;
use tempfile::tempdir;

fn kind(name: &'static str) -> Kind {
	Kind { name: Str::new_static(name), rev: 1 }
}

fn genesis() -> EntryDraft {
	EntryDraft {
		kind:  kind(JOURNAL),
		by:    None,
		prior: None,
		label: None,
		data:  Str::new_static(r#"{"version":1,"cwd":"/tmp","created":"now"}"#),
	}
}

fn draft(name: &'static str, by: omp_journal::EntryId) -> EntryDraft {
	EntryDraft {
		kind:  kind(name),
		by:    Some(by),
		prior: None,
		label: None,
		data:  Str::new_static("{}"),
	}
}

proptest! {
	#![proptest_config(ProptestConfig::with_cases(8))]

	#[test]
	fn torn_tail_recovers_at_every_byte_offset_without_losing_a_complete_frame(
		labels in prop::collection::vec("[^\\r\\n]{0,24}", 0..4),
	) {
		let directory = tempdir().expect("tempdir");
		let source_path = directory.path().join("source.oms");
		let mut journal = Journal::create(&source_path).expect("create");
		let genesis = journal.append(genesis()).expect("genesis");
		let mut expected_entries = vec![genesis.clone()];
		for (index, label) in labels.into_iter().enumerate() {
			let mut next = draft(TURN_START, genesis.id);
			next.label = Some(Str::new(label));
			next.data = Str::new(serde_json::json!({"turn": index}).to_string());
			expected_entries.push(journal.append(next).expect("append"));
		}
		drop(journal);
		let complete = fs::read(&source_path).expect("read complete journal");
		let mut frame_ends = Vec::new();
		let mut scanner = omp_journal::sse::Scanner::new(&complete);
		while let Some(frame) = scanner.next() {
			frame.expect("valid source frame");
			frame_ends.push(scanner.offset());
		}

		for cut in 0..=complete.len() {
			let cut_path = directory.path().join("cut.oms");
			fs::write(&cut_path, &complete[..cut]).expect("write prefix");
			let (opened, recovered) = Journal::open(&cut_path).expect("recover prefix");
			let expected_len = frame_ends.iter().take_while(|&&end| end <= cut).count();
			prop_assert_eq!(&recovered, &expected_entries[..expected_len]);
			let clean_len = frame_ends.get(expected_len.wrapping_sub(1)).copied().unwrap_or(0);
			prop_assert_eq!(opened.recovered_tail_bytes(), (cut - clean_len) as u64);
			drop(opened);
			fs::remove_file(&cut_path).expect("remove prefix");
		}
	}
}

#[test]
fn every_non_genesis_entry_requires_by() {
	let directory = tempdir().expect("tempdir");
	let path = directory.path().join("cause.oms");
	let mut journal = Journal::create(path).expect("create");
	let first_error = journal
		.append(EntryDraft {
			kind:  kind(TURN_START),
			by:    None,
			prior: None,
			label: None,
			data:  Str::new_static("{}"),
		})
		.expect_err("non-genesis without cause must fail");
	assert!(matches!(first_error, JournalError::MissingCause { .. }));
	let genesis = journal.append(genesis()).expect("genesis");
	let error = journal
		.append(EntryDraft {
			kind:  kind(PATCH),
			by:    None,
			prior: None,
			label: None,
			data:  Str::new_static(r#"{"ops":[]}"#),
		})
		.expect_err("missing cause must fail");
	assert!(matches!(error, JournalError::MissingCause { kind } if kind.name == PATCH));
	journal
		.append(draft(TURN_START, genesis.id))
		.expect("caused entry");
}

#[test]
fn prior_walk_selects_live_chain_and_retains_abandoned() {
	let directory = tempdir().expect("tempdir");
	let path = directory.path().join("branch.oms");
	let mut journal = Journal::create(path).expect("create");
	let genesis = journal.append(genesis()).expect("genesis");
	let first = journal
		.append(draft(TURN_START, genesis.id))
		.expect("first");
	let second = journal.append(draft(TURN_START, first.id)).expect("second");
	let mut branch = draft(PATCH, genesis.id);
	branch.prior = Some(genesis.id);
	branch.data = Str::new_static(r#"{"ops":[]}"#);
	let branch = journal.append(branch).expect("branch");
	let entries: Vec<Entry> = vec![genesis.clone(), first.clone(), second.clone(), branch.clone()];

	assert_eq!(
		live_chain(&entries)
			.map(|entry| entry.id)
			.collect::<Vec<_>>(),
		[genesis.id, branch.id]
	);
	assert_eq!(
		abandoned(&entries)
			.map(|entry| entry.id)
			.collect::<Vec<_>>(),
		[first.id, second.id]
	);
}
