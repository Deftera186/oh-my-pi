//! Journal-level abandoned-branch pruning.

use std::{
	fs, io,
	path::{Path, PathBuf},
};

use miette::IntoDiagnostic as _;
use omp_journal::{Journal, abandoned, gc::prune_abandoned};
use serde_json::json;

use crate::cli::GcArgs;

/// Scans native `.oms` journals and optionally prunes abandoned branches.
pub fn run(args: GcArgs) -> miette::Result<()> {
	let data_dir = omp_core::dirs::data_dir(args.data_dir).into_diagnostic()?;
	let sessions = args
		.sessions_dir
		.unwrap_or_else(|| data_dir.join("sessions"));
	let mut paths = Vec::new();
	collect_journals(&sessions, &mut paths).into_diagnostic()?;
	paths.sort();

	let mut journals = 0usize;
	let mut entries_pruned = 0usize;
	let mut bytes_reclaimed = 0u64;
	for path in paths {
		let (_, entries) = Journal::open(&path).into_diagnostic()?;
		let abandoned_count = abandoned(&entries).count();
		if abandoned_count == 0 {
			continue;
		}
		journals += 1;
		entries_pruned += abandoned_count;
		if args.apply {
			bytes_reclaimed += prune_abandoned(&path).into_diagnostic()?.bytes_reclaimed();
		}
	}

	if args.json {
		println!(
			"{}",
			json!({
				"applied": args.apply,
				"journals": journals,
				"entries_pruned": entries_pruned,
				"bytes_reclaimed": bytes_reclaimed,
			})
		);
	} else if args.apply {
		println!(
			"pruned {entries_pruned} abandoned entries from {journals} journals; reclaimed \
			 {bytes_reclaimed} bytes"
		);
	} else {
		println!(
			"dry run: {entries_pruned} abandoned entries in {journals} journals; pass --apply to \
			 prune"
		);
	}
	Ok(())
}

fn collect_journals(directory: &Path, output: &mut Vec<PathBuf>) -> io::Result<()> {
	let entries = match fs::read_dir(directory) {
		Ok(entries) => entries,
		Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
		Err(error) => return Err(error),
	};
	for entry in entries {
		let path = entry?.path();
		if path.is_dir() {
			collect_journals(&path, output)?;
		} else if path.extension().and_then(|value| value.to_str())
			== Some(omp_journal::FILE_EXTENSION)
		{
			output.push(path);
		}
	}
	Ok(())
}
