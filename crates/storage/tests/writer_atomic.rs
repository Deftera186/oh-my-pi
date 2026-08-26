//! Atomic and staged transcript writer behavior.

use std::{fs, iter, path::PathBuf};

use omp_core::Str;
use omp_storage::transcript::{
	EntryUndecodable, Event, Header, Kind, Reader, SessionId, TitleSource, Writer,
	writer::{JournalError, MAX_ATOMIC_ENTRIES},
};
use serde_json::value::RawValue;
use tempfile::tempdir;

fn header() -> Header {
	Header {
		v:       4,
		id:      SessionId(Str::new("writer-atomic")),
		created: 1,
		cwd:     PathBuf::from("/tmp/work"),
	}
}

fn title(ts: u64, value: &str) -> Event {
	Event { ts, kind: Kind::Title { title: Str::new(value), source: TitleSource::User } }
}

#[test]
fn atomic_group_assigns_contiguous_physical_indexes_across_reopen() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("create transcript");

	assert_eq!(
		writer
			.append_atomic(&[title(2, "first"), title(3, "second")])
			.expect("atomic append")
			.as_slice(),
		[0, 1]
	);
	assert_eq!(
		writer.byte_watermark().expect("committed byte watermark"),
		std::fs::metadata(&path).expect("transcript metadata").len()
	);
	drop(writer);

	let mut writer = Writer::open_append(&path).expect("reopen transcript");
	assert_eq!(
		writer
			.append_atomic(&[title(4, "third"), title(5, "fourth")])
			.expect("atomic append after replay")
			.as_slice(),
		[2, 3]
	);
	assert!(!writer.is_poisoned());
}

#[test]
fn crash_visible_atomic_group_is_prefix_atomic_and_retryable() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("create transcript");
	writer
		.append(&title(2, "committed"))
		.expect("append committed prefix");
	let before_group = writer.byte_watermark().expect("prefix watermark");
	writer
		.append_atomic(&[title(3, "hidden one"), title(4, "hidden two")])
		.expect("append group fixture");
	let after_group = writer.byte_watermark().expect("group watermark");
	drop(writer);

	let file = fs::OpenOptions::new()
		.write(true)
		.open(&path)
		.expect("open crash fixture");
	file
		.set_len(before_group + (after_group - before_group) / 2)
		.expect("tear group envelope");
	file.sync_all().expect("persist crash fixture");
	drop(file);

	let reader = Reader::open(&path).expect("read crash-visible prefix");
	assert_eq!(reader.next_index(), 1);
	assert!(reader.has_torn_tail());
	drop(reader);

	let mut writer = Writer::open_append(&path).expect("repair torn group");
	assert_eq!(
		writer
			.append_atomic(&[title(3, "retry one"), title(4, "retry two")])
			.expect("retry complete group")
			.as_slice(),
		[1, 2]
	);
	drop(writer);
	assert_eq!(
		Reader::open(&path)
			.expect("read repaired journal")
			.next_index(),
		3
	);
}

#[test]
fn atomic_validation_failure_leaves_journal_unchanged() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("create transcript");
	let before = fs::read(&path).expect("read header");
	let duplicate_header =
		RawValue::from_string(serde_json::to_string(&header()).expect("encode duplicate header"))
			.expect("raw header");

	assert!(matches!(
		writer.append_atomic(&[title(2, "must not land"), Event {
			ts:   3,
			kind: Kind::EntryUndecodable(EntryUndecodable {
				kind:   None,
				rev:    None,
				value:  None,
				raw:    duplicate_header,
				reason: Str::new("duplicate header fixture"),
			}),
		},]),
		Err(JournalError::RolledBack { .. })
	));
	assert_eq!(std::fs::read(&path).expect("read unchanged journal"), before);
	assert_eq!(
		writer
			.append(&title(4, "index zero"))
			.expect("append after rollback"),
		0
	);
}

#[test]
fn append_many_returns_the_exact_assigned_prefix() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("create transcript");

	assert_eq!(
		writer
			.append_many(&[title(2, "zero"), title(3, "one"), title(4, "two")])
			.expect("staged append")
			.as_slice(),
		[0, 1, 2]
	);
	assert_eq!(
		writer
			.append(&title(5, "three"))
			.expect("append after batch"),
		3
	);
}

#[test]
fn oversized_atomic_group_is_rejected_before_writing() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("session.jsonl");
	let mut writer = Writer::create(&path, &header()).expect("create transcript");
	let before = fs::read(&path).expect("read header");
	let events = iter::repeat_with(|| title(2, "entry"))
		.take(MAX_ATOMIC_ENTRIES + 1)
		.collect::<Vec<_>>();

	assert!(matches!(
		writer.append_atomic(&events),
		Err(JournalError::TooManyEntries {
			entries,
			maximum: MAX_ATOMIC_ENTRIES,
		}) if entries == MAX_ATOMIC_ENTRIES + 1
	));
	assert_eq!(std::fs::read(&path).expect("read unchanged journal"), before);
}
