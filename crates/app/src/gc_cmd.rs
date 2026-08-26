//! Lock-safe session maintenance, cold archive, and blob reclamation.

use std::{
	collections::{HashMap, HashSet},
	fs::{self, File, OpenOptions},
	io,
	io::Write as _,
	path::{Path, PathBuf},
	time,
	time::Duration,
};

use flate2::{Compression, write::GzEncoder};
use miette::{IntoDiagnostic as _, miette};
use omp_storage::{
	blob::BlobStore,
	gc,
	index::{SessionFilter, SessionIndex, SessionInfo, SessionStatus},
	maintenance::{LineageTransferReport, MaintenanceMode, TransferCount},
	transcript::SessionId,
};
use rusqlite::Connection;
use serde_json::json;

use crate::cli::GcArgs;

#[must_use]
struct GcLock(PathBuf);

struct ArchivePlan {
	retained: Option<SessionId>,
	archived: Vec<SessionId>,
}

impl Drop for GcLock {
	fn drop(&mut self) {
		let _ = fs::remove_file(&self.0);
	}
}

/// Runs dry by default; destructive work requires `--apply`.
pub fn run(args: GcArgs) -> miette::Result<()> {
	let data_dir = omp_core::dirs::data_dir(args.data_dir).into_diagnostic()?;
	let explicit_sessions_dir = args.sessions_dir;
	let sessions_dir = explicit_sessions_dir
		.clone()
		.unwrap_or_else(|| data_dir.join("sessions"));
	let index_path = args
		.index
		.unwrap_or_else(|| sessions_dir.join("sessions.sqlite3"));
	let _lock = acquire_lock(&data_dir.join("gc.lock"))?;
	let index = SessionIndex::open(&index_path).into_diagnostic()?;
	let page = index
		.list(&SessionFilter { limit: u32::MAX, ..SessionFilter::default() })
		.into_diagnostic()?;
	let cutoff = now_ms().saturating_sub(args.cold_archive_after_days.saturating_mul(86_400_000));
	let mut protected = HashSet::new();
	for session in page.sessions.iter().take(args.retain_newest_global) {
		protected.insert(session.id.clone());
	}
	let mut per_cwd = HashMap::<String, usize>::new();
	for session in &page.sessions {
		let count = per_cwd.entry(session.cwd.as_str().to_owned()).or_default();
		if *count < args.retain_newest_per_cwd {
			protected.insert(session.id.clone());
			*count += 1;
		}
	}
	let mut tentative = HashSet::new();
	for session in &page.sessions {
		let active = matches!(
			session.status,
			SessionStatus::Pending | SessionStatus::Interrupted | SessionStatus::Unknown
		);
		if session.updated_ms < cutoff && !active && !protected.contains(&session.id) {
			tentative.insert(session.id.clone());
		}
	}
	let (plans, ambiguous_lineage) = archive_plans(&page.sessions, &tentative);
	let candidates = plans
		.iter()
		.flat_map(|plan| plan.archived.iter().cloned())
		.collect::<Vec<_>>();
	let mut archived_bytes = 0_u64;
	let mut lineage_transfer = LineageTransferReport::default();
	if args.archive {
		if args.apply {
			for session in &candidates {
				archived_bytes = archived_bytes.saturating_add(archive_session_files(
					&sessions_dir,
					&data_dir.join("archive/sessions"),
					session,
				)?);
			}
		}
		let mode = if args.apply {
			MaintenanceMode::Apply
		} else {
			MaintenanceMode::DryRun
		};
		for plan in &plans {
			if let Some(retained) = &plan.retained {
				let report = index
					.rekey_archived_lineage(retained, &plan.archived, mode)
					.into_diagnostic()?;
				add_lineage_report(&mut lineage_transfer, report);
			} else {
				index
					.remove_archived_sessions(&plan.archived, mode)
					.into_diagnostic()?;
			}
		}
	}
	if args.apply && args.archive {
		for session in &candidates {
			fs::remove_file(sessions_dir.join(format!("{}.jsonl", session.as_str())))
				.into_diagnostic()?;
		}
	}
	let retained = page
		.sessions
		.iter()
		.filter(|session| !args.apply || !args.archive || !candidates.contains(&session.id))
		.map(|session| session.id.clone())
		.collect::<Vec<_>>();
	let sweep = if args.apply {
		let store = BlobStore::open(&data_dir).into_diagnostic()?;
		let explicit_roots = explicit_sessions_dir.into_iter().collect::<Vec<_>>();
		let roots = gc::SessionRoots::discover(&store, &explicit_roots).into_diagnostic()?;
		gc::sweep(&store, &roots, Duration::from_secs(args.min_age_seconds)).into_diagnostic()?
	} else {
		gc::SweepReport::default()
	};
	if args.apply {
		optimize_index(&index_path)?;
		if args.wal {
			checkpoint_databases(&data_dir, &index_path)?;
		}
	}
	let report = json!({
		"applied": args.apply,
		"archiveRequested": args.archive,
		"archiveCandidates": candidates.len(),
		"archivedBytes": archived_bytes,
		"lineageProtected": ambiguous_lineage,
		"lineageRowsTransferred": lineage_transfer.transferred(),
		"lineageRowCollisions": lineage_transfer.collisions(),
		"retainedSessions": retained.len(),
		"blobsExamined": sweep.examined_count,
		"blobsReclaimed": sweep.reclaimed_count,
		"bytesReclaimed": sweep.reclaimed_bytes,
		"corruptReferences": sweep.corrupt_references,
	});
	if args.json {
		println!("{}", serde_json::to_string_pretty(&report).into_diagnostic()?);
	} else {
		println!(
			"{}: {} archive candidate(s), {} lineage-protected, {} lineage row(s) transferred, {} \
			 collision(s), {} blob(s) reclaimed ({} bytes)",
			if args.apply { "applied" } else { "dry run" },
			candidates.len(),
			ambiguous_lineage,
			lineage_transfer.transferred(),
			lineage_transfer.collisions(),
			sweep.reclaimed_count,
			sweep.reclaimed_bytes,
		);
	}
	Ok(())
}

fn archive_plans(
	sessions: &[SessionInfo],
	tentative: &HashSet<SessionId>,
) -> (Vec<ArchivePlan>, usize) {
	let by_id = sessions
		.iter()
		.enumerate()
		.map(|(index, session)| (session.id.as_str(), index))
		.collect::<HashMap<_, _>>();
	let parents = sessions
		.iter()
		.filter_map(|session| session.parent.as_ref().map(SessionId::as_str))
		.collect::<HashSet<_>>();
	let mut standalone = Vec::new();
	let mut families = HashMap::<usize, Vec<SessionId>>::new();
	let mut ambiguous = 0_usize;
	for (index, session) in sessions.iter().enumerate() {
		if !tentative.contains(&session.id) {
			continue;
		}
		if session.parent.is_none() && !parents.contains(session.id.as_str()) {
			standalone.push(session.id.clone());
			continue;
		}
		let Some(root) = lineage_root(index, sessions, &by_id) else {
			ambiguous = ambiguous.saturating_add(1);
			continue;
		};
		families.entry(root).or_default().push(session.id.clone());
	}

	let mut plans = Vec::with_capacity(
		families
			.len()
			.saturating_add(usize::from(!standalone.is_empty())),
	);
	if !standalone.is_empty() {
		plans.push(ArchivePlan { retained: None, archived: standalone });
	}
	for (root, archived) in families {
		let retained = sessions.iter().enumerate().find_map(|(index, session)| {
			if tentative.contains(&session.id) || lineage_root(index, sessions, &by_id) != Some(root) {
				return None;
			}
			Some(session.id.clone())
		});
		if let Some(retained) = retained {
			plans.push(ArchivePlan { retained: Some(retained), archived });
		} else {
			ambiguous = ambiguous.saturating_add(archived.len());
		}
	}
	(plans, ambiguous)
}

fn lineage_root(
	start: usize,
	sessions: &[SessionInfo],
	by_id: &HashMap<&str, usize>,
) -> Option<usize> {
	let mut current = start;
	let mut seen = HashSet::new();
	loop {
		let session = sessions.get(current)?;
		if !seen.insert(session.id.as_str()) {
			return None;
		}
		let Some(parent) = &session.parent else {
			return Some(current);
		};
		current = *by_id.get(parent.as_str())?;
	}
}

fn add_lineage_report(target: &mut LineageTransferReport, source: LineageTransferReport) {
	add_transfer_count(&mut target.receipts, source.receipts);
	add_transfer_count(&mut target.item_outcomes, source.item_outcomes);
	add_transfer_count(&mut target.model_performance, source.model_performance);
	add_transfer_count(&mut target.entry_kinds, source.entry_kinds);
	add_transfer_count(&mut target.prompts_fts, source.prompts_fts);
	target.archived_sessions = target
		.archived_sessions
		.saturating_add(source.archived_sessions);
}

fn add_transfer_count(target: &mut TransferCount, source: TransferCount) {
	target.transferred = target.transferred.saturating_add(source.transferred);
	target.collisions = target.collisions.saturating_add(source.collisions);
}

fn archive_session_files(
	sessions_dir: &Path,
	archive_dir: &Path,
	session: &SessionId,
) -> miette::Result<u64> {
	let source = sessions_dir.join(format!("{}.jsonl", session.as_str()));
	if !source.is_file() {
		return Err(miette!("session journal is missing: {}", source.display()));
	}
	fs::create_dir_all(archive_dir).into_diagnostic()?;
	let destination = archive_dir.join(format!("{}.jsonl.gz", session.as_str()));
	if destination.exists() {
		return Err(miette!("archive destination already exists: {}", destination.display()));
	}
	let temporary = destination.with_extension(format!("gz.tmp-{}", std::process::id()));
	let mut input = File::open(&source).into_diagnostic()?;
	let mut encoder =
		GzEncoder::new(File::create(&temporary).into_diagnostic()?, Compression::default());
	io::copy(&mut input, &mut encoder).into_diagnostic()?;
	let output = encoder.finish().into_diagnostic()?;
	output.sync_all().into_diagnostic()?;
	fs::rename(&temporary, &destination).into_diagnostic()?;
	let artifacts = sessions_dir.join(session.as_str());
	if artifacts.is_dir() {
		fs::rename(&artifacts, archive_dir.join(session.as_str())).into_diagnostic()?;
	}
	Ok(fs::metadata(destination).into_diagnostic()?.len())
}

fn optimize_index(path: &Path) -> miette::Result<()> {
	let connection = Connection::open(path).into_diagnostic()?;
	connection
		.execute("INSERT INTO prompts_fts(prompts_fts) VALUES('optimize')", [])
		.into_diagnostic()?;
	connection
		.execute_batch("PRAGMA optimize;")
		.into_diagnostic()?;
	Ok(())
}

fn checkpoint_databases(data_dir: &Path, index_path: &Path) -> miette::Result<()> {
	for path in [
		index_path.to_owned(),
		data_dir.join("credentials.db"),
		data_dir.join("models.db"),
		data_dir.join("history.db"),
		data_dir.join("stats.db"),
	] {
		if path.is_file() {
			Connection::open(path)
				.into_diagnostic()?
				.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
				.into_diagnostic()?;
		}
	}
	Ok(())
}

fn acquire_lock(path: &Path) -> miette::Result<GcLock> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).into_diagnostic()?;
	}
	let create = || OpenOptions::new().write(true).create_new(true).open(path);
	let mut file = match create() {
		Ok(file) => file,
		Err(error) if error.kind() == io::ErrorKind::AlreadyExists && stale_lock(path) => {
			fs::remove_file(path).into_diagnostic()?;
			create().into_diagnostic()?
		},
		Err(error) => return Err(error).into_diagnostic(),
	};
	writeln!(file, "{}", std::process::id()).into_diagnostic()?;
	file.sync_all().into_diagnostic()?;
	Ok(GcLock(path.to_owned()))
}

fn stale_lock(path: &Path) -> bool {
	let Ok(text) = fs::read_to_string(path) else {
		return false;
	};
	let Ok(pid) = text.trim().parse::<u32>() else {
		return false;
	};
	#[cfg(unix)]
	{
		use nix::{sys::signal, unistd::Pid};

		signal::kill(Pid::from_raw(pid as i32), None).is_err()
	}
	#[cfg(not(unix))]
	{
		let _ = pid;
		false
	}
}

fn now_ms() -> u64 {
	time::SystemTime::now()
		.duration_since(time::UNIX_EPOCH)
		.map_or(0, |duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}
