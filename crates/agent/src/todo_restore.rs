//! Journal-derived recovery of the durable todo snapshot.
//!
//! Every successful `todo@1` outcome and every journaled `/todo` edit carries
//! the complete phased list, so the live todo state is always reconstructible
//! from journal truth alone. [`latest_todo_phases`] performs that scan for the
//! history-rewrite reconciler and resume seeding.

use omp_proto::{
	thread::{v1 as thread, v1::item},
	value_json::value_to_json,
};
use omp_storage::transcript::{Custom, Entry, Kind};

use crate::{Journal, JournalError, journal_kinds::TODO_EDIT_KIND};

/// Latest durable todo phases on the live-intent chain, or `None` when a
/// rewind/reset dropped them all (or none were ever recorded).
///
/// The scan is a physical forward fold: rewind pops candidates past the
/// target and reset clears them, while compaction and prompt rewrites are
/// ignored — todo state semantically survives context surgery even though the
/// live-chain projection drops pre-compact events. Undecodable candidates are
/// skipped, never errors.
pub(crate) fn latest_todo_phases(
	journal: &Journal,
) -> Result<Option<serde_json::Value>, JournalError> {
	let log = journal.load()?;
	let mut candidates: Vec<u64> = Vec::new();
	for index in 0..u64::try_from(log.len()).expect("event indexes fit in u64") {
		let Some(Entry::Ok(event)) = log.get(index) else {
			continue;
		};
		match &event.kind {
			Kind::Item(record) => {
				if matches!(
					&record.item.kind,
					Some(item::Kind::ToolResult(result)) if result.name == "todo"
				) {
					candidates.push(index);
				}
			},
			Kind::Custom(custom) if custom.kind() == TODO_EDIT_KIND => candidates.push(index),
			Kind::Rewind { to } => match to {
				Some(to) => {
					while candidates.last().is_some_and(|last| last > to) {
						candidates.pop();
					}
				},
				None => candidates.clear(),
			},
			Kind::Reset => candidates.clear(),
			_ => {},
		}
	}
	for index in candidates.into_iter().rev() {
		let Some(Entry::Ok(event)) = log.get(index) else {
			continue;
		};
		let phases = match &event.kind {
			Kind::Item(record) => match &record.item.kind {
				Some(item::Kind::ToolResult(result)) => outcome_phases(result),
				_ => None,
			},
			Kind::Custom(custom) => edit_phases(custom),
			_ => None,
		};
		if phases.is_some() {
			return Ok(phases);
		}
	}
	Ok(None)
}

/// Extracts the phased list from a successful durable `todo@1` outcome.
fn outcome_phases(result: &thread::ToolResult) -> Option<serde_json::Value> {
	let details = value_to_json(result.details.as_ref()?)?;
	if details.get("kind")?.as_str()? != "ok" {
		return None;
	}
	details.get("value")?.get("phases").cloned()
}

/// Extracts the phased list from a journaled `/todo` edit snapshot.
fn edit_phases(custom: &Custom) -> Option<serde_json::Value> {
	let data: serde_json::Value = serde_json::from_str(custom.data()?.get()).ok()?;
	data.get("phases").cloned()
}
