//! Per-advisor JSONL persistence and session-statistics attribution.

use std::{
	collections::{BTreeMap, BTreeSet},
	fs::{self, File, OpenOptions},
	io::{self, BufRead, BufReader, Read, Write},
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	thread,
};

use omp_core::Str;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Usage and integer cost attributed only to one advisor transcript.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdvisorUsageTotals {
	/// Fresh input tokens.
	pub input_tokens:       u64,
	/// Reused prompt-cache input.
	pub cache_read_tokens:  u64,
	/// Prompt-cache writes.
	pub cache_write_tokens: u64,
	/// Generated output tokens.
	pub output_tokens:      u64,
	/// Integer micro-US dollars charged to advisor inference.
	pub cost_micro_usd:     i128,
}

impl AdvisorUsageTotals {
	pub(crate) fn accumulate(&mut self, other: Self) {
		self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
		self.cache_read_tokens = self
			.cache_read_tokens
			.saturating_add(other.cache_read_tokens);
		self.cache_write_tokens = self
			.cache_write_tokens
			.saturating_add(other.cache_write_tokens);
		self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
		self.cost_micro_usd = self.cost_micro_usd.saturating_add(other.cost_micro_usd);
	}

	fn accrued_after(self, baseline: Self) -> Self {
		Self {
			input_tokens:       self.input_tokens.saturating_sub(baseline.input_tokens),
			cache_read_tokens:  self
				.cache_read_tokens
				.saturating_sub(baseline.cache_read_tokens),
			cache_write_tokens: self
				.cache_write_tokens
				.saturating_sub(baseline.cache_write_tokens),
			output_tokens:      self.output_tokens.saturating_sub(baseline.output_tokens),
			cost_micro_usd:     self.cost_micro_usd.saturating_sub(baseline.cost_micro_usd),
		}
	}
}

/// One append-only advisor transcript record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdvisorTranscriptRecord {
	/// Epoch-millisecond observation time.
	pub timestamp_ms: u64,
	/// Stable advisor child id.
	pub advisor_id:   Str,
	/// Record kind (`prompt`, `assistant`, `tool`, or `error`).
	pub kind:         Str,
	/// Secret-obfuscated model-visible body.
	pub content:      Str,
	/// Usage and cost contributed by this record.
	pub usage:        AdvisorUsageTotals,
}

/// Sink used by the app's session-statistics authority.
pub trait AdvisorStatisticsSink: Clone + Send + Sync + 'static {
	/// Attributes one advisor usage delta to the owning primary session.
	fn record_advisor_usage(
		&self,
		primary_session: &str,
		advisor_id: &str,
		usage: AdvisorUsageTotals,
	);
	/// Notifies the host that resume-time advisor totals have been reconciled.
	fn advisor_cost_changed(&self, _primary_session: &str) {}
}

/// A no-op statistics sink for hosts without a statistics authority.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopAdvisorStatistics;

impl AdvisorStatisticsSink for NoopAdvisorStatistics {
	fn record_advisor_usage(&self, _: &str, _: &str, _: AdvisorUsageTotals) {}
}

/// Advisor transcript persistence failure.
#[derive(Debug, thiserror::Error)]
pub enum AdvisorTranscriptError {
	/// Transcript directory or file I/O failed.
	#[error("advisor transcript I/O failed")]
	Io(#[from] io::Error),
	/// A typed record could not be encoded as JSON.
	#[error("advisor transcript serialization failed")]
	Json(#[from] serde_json::Error),
}

#[derive(Debug, Default)]
struct ReplayWindow {
	fingerprints: Vec<Str>,
	cursor:       usize,
}

#[derive(Debug, Default)]
struct TranscriptState {
	totals: BTreeMap<Str, AdvisorUsageTotals>,
	replay: BTreeMap<Str, ReplayWindow>,
}

struct CostFileSnapshot {
	path:       PathBuf,
	advisor_id: Str,
	max_bytes:  u64,
}

/// Per-primary-session transcript writer with per-advisor usage totals.
pub struct AdvisorTranscriptStore<S = NoopAdvisorStatistics> {
	root:              PathBuf,
	primary_session:   Str,
	statistics:        S,
	state:             Arc<Mutex<TranscriptState>>,
	restore_cancelled: Arc<AtomicBool>,
	restore_finished:  Arc<AtomicBool>,
}

impl<S: AdvisorStatisticsSink> AdvisorTranscriptStore<S> {
	/// Opens `.omp/advisors/<primary-session>/` and starts cost restoration on a
	/// background thread.
	pub fn open(
		project_root: &Path,
		primary_session: impl Into<Str>,
		statistics: S,
	) -> Result<Self, AdvisorTranscriptError> {
		let primary_session = primary_session.into();
		let root = project_root
			.join(".omp")
			.join("advisors")
			.join(safe_component(primary_session.as_str()));
		fs::create_dir_all(&root)?;
		let state = Arc::new(Mutex::new(TranscriptState::default()));
		let restore_cancelled = Arc::new(AtomicBool::new(false));
		let restore_finished = Arc::new(AtomicBool::new(false));
		start_cost_restore(
			root.clone(),
			primary_session.clone(),
			statistics.clone(),
			Arc::clone(&state),
			Arc::clone(&restore_cancelled),
			Arc::clone(&restore_finished),
		);
		Ok(Self { root, primary_session, statistics, state, restore_cancelled, restore_finished })
	}

	/// Rewinds one advisor's positional replay cursor before a delivery attempt.
	pub fn begin_turn(&self, advisor_id: &str) {
		self
			.state
			.lock()
			.replay
			.entry(Str::new(advisor_id))
			.or_default()
			.cursor = 0;
	}

	/// Clears replay identity after one advisor turn commits.
	pub fn commit_turn(&self, advisor_id: &str) {
		self.state.lock().replay.remove(advisor_id);
	}

	/// Clears replay identity after a failed batch is permanently abandoned.
	pub fn abandon_turn(&self, advisor_id: &str) {
		self.commit_turn(advisor_id);
	}

	/// Appends and flushes one JSONL record before updating in-memory totals.
	///
	/// Prompt records are compared positionally with the current uncommitted
	/// turn. Assistant, tool, and error records always write through.
	pub fn append(
		&mut self,
		record: &AdvisorTranscriptRecord,
	) -> Result<(), AdvisorTranscriptError> {
		let mut state = self.state.lock();
		let mut fingerprint = None;
		if record.kind == "prompt" {
			let window = state.replay.entry(record.advisor_id.clone()).or_default();
			if window.cursor < window.fingerprints.len() {
				if window.fingerprints[window.cursor] == record.content {
					window.cursor = window.cursor.saturating_add(1);
					return Ok(());
				}
				window.fingerprints.truncate(window.cursor);
			}
			fingerprint = Some(record.content.clone());
		}
		let path = self.path_for(record.advisor_id.as_str());
		let mut file = append_file(&path)?;
		serde_json::to_writer(&mut file, record)?;
		file.write_all(b"\n")?;
		file.flush()?;
		if let Some(fingerprint) = fingerprint {
			let window = state.replay.entry(record.advisor_id.clone()).or_default();
			window.fingerprints.push(fingerprint);
			window.cursor = window.fingerprints.len();
		}
		state
			.totals
			.entry(record.advisor_id.clone())
			.or_default()
			.accumulate(record.usage);
		self.statistics.record_advisor_usage(
			self.primary_session.as_str(),
			record.advisor_id.as_str(),
			record.usage,
		);
		Ok(())
	}

	/// Returns reconciled historical and current-process totals.
	pub fn totals(&self, advisor_id: &str) -> AdvisorUsageTotals {
		self
			.state
			.lock()
			.totals
			.get(advisor_id)
			.copied()
			.unwrap_or_default()
	}

	/// Returns whether the background cost scan has settled.
	pub fn cost_restore_finished(&self) -> bool {
		self.restore_finished.load(Ordering::Acquire)
	}

	/// Cancels an in-flight scan because this store no longer owns the active
	/// conversation.
	pub fn cancel_cost_restore(&self) {
		self.restore_cancelled.store(true, Ordering::Release);
		let _barrier = self.state.lock();
	}

	/// Returns the stable JSONL path for one advisor id.
	pub fn path_for(&self, advisor_id: &str) -> PathBuf {
		self
			.root
			.join(format!("{}.jsonl", safe_component(advisor_id)))
	}
}

impl<S> Drop for AdvisorTranscriptStore<S> {
	fn drop(&mut self) {
		self.restore_cancelled.store(true, Ordering::Release);
		let _barrier = self.state.lock();
	}
}

fn start_cost_restore<S: AdvisorStatisticsSink>(
	root: PathBuf,
	primary_session: Str,
	statistics: S,
	state: Arc<Mutex<TranscriptState>>,
	cancelled: Arc<AtomicBool>,
	finished: Arc<AtomicBool>,
) {
	let thread_finished = Arc::clone(&finished);
	let spawned = thread::Builder::new()
		.name(String::from("advisor-cost-restore"))
		.spawn(move || {
			// The state lock is the recorder write barrier: every append accepted
			// before it is reflected in both the baseline and captured byte
			// lengths; later appends wait until the fixed prefixes are known.
			let (baseline, snapshots) = {
				let state = state.lock();
				let baseline = state.totals.clone();
				let snapshots = snapshot_cost_files(&root, &cancelled);
				(baseline, snapshots)
			};
			if cancelled.load(Ordering::Acquire) {
				finished.store(true, Ordering::Release);
				return;
			}
			let restored = scan_cost_snapshots(&snapshots, &cancelled);
			if cancelled.load(Ordering::Acquire) {
				finished.store(true, Ordering::Release);
				return;
			}
			{
				let mut state = state.lock();
				if cancelled.load(Ordering::Acquire) {
					finished.store(true, Ordering::Release);
					return;
				}
				let current = std::mem::take(&mut state.totals);
				state.totals = reconcile_totals(&restored, &current, &baseline);
			}
			if !cancelled.load(Ordering::Acquire) {
				statistics.advisor_cost_changed(primary_session.as_str());
			}
			finished.store(true, Ordering::Release);
		});
	if spawned.is_err() {
		thread_finished.store(true, Ordering::Release);
	}
}

fn snapshot_cost_files(root: &Path, cancelled: &AtomicBool) -> Vec<CostFileSnapshot> {
	let Ok(entries) = fs::read_dir(root) else {
		return Vec::new();
	};
	let mut snapshots = Vec::new();
	for entry in entries.flatten() {
		if cancelled.load(Ordering::Acquire) {
			break;
		}
		let path = entry.path();
		if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
			continue;
		}
		let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
			continue;
		};
		let Ok(metadata) = entry.metadata() else {
			continue;
		};
		if metadata.is_file() {
			let advisor_id = Str::new(stem);
			snapshots.push(CostFileSnapshot { path, advisor_id, max_bytes: metadata.len() });
		}
	}
	snapshots
}

fn scan_cost_snapshots(
	snapshots: &[CostFileSnapshot],
	cancelled: &AtomicBool,
) -> BTreeMap<Str, AdvisorUsageTotals> {
	let mut restored = BTreeMap::new();
	for snapshot in snapshots {
		if cancelled.load(Ordering::Acquire) {
			break;
		}
		let Ok(file) = File::open(&snapshot.path) else {
			continue;
		};
		let reader = BufReader::new(file.take(snapshot.max_bytes));
		let mut total = AdvisorUsageTotals::default();
		for line in reader.lines() {
			if cancelled.load(Ordering::Acquire) {
				break;
			}
			let Ok(line) = line else {
				break;
			};
			let Ok(record) = serde_json::from_str::<AdvisorTranscriptRecord>(&line) else {
				continue;
			};
			total.accumulate(record.usage);
		}
		if total != AdvisorUsageTotals::default() {
			restored.insert(snapshot.advisor_id.clone(), total);
		}
	}
	restored
}

fn reconcile_totals(
	restored: &BTreeMap<Str, AdvisorUsageTotals>,
	current: &BTreeMap<Str, AdvisorUsageTotals>,
	baseline: &BTreeMap<Str, AdvisorUsageTotals>,
) -> BTreeMap<Str, AdvisorUsageTotals> {
	let advisor_ids = restored
		.keys()
		.chain(current.keys())
		.cloned()
		.collect::<BTreeSet<_>>();
	let mut totals = BTreeMap::new();
	for advisor_id in advisor_ids {
		let mut total = restored.get(&advisor_id).copied().unwrap_or_default();
		total.accumulate(
			current
				.get(&advisor_id)
				.copied()
				.unwrap_or_default()
				.accrued_after(baseline.get(&advisor_id).copied().unwrap_or_default()),
		);
		if total != AdvisorUsageTotals::default() {
			totals.insert(advisor_id, total);
		}
	}
	totals
}

fn append_file(path: &Path) -> io::Result<File> {
	OpenOptions::new().create(true).append(true).open(path)
}

fn safe_component(value: &str) -> String {
	let mut safe = String::with_capacity(value.len().max(1));
	for character in value.chars() {
		if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
			safe.push(character);
		} else {
			safe.push('-');
		}
	}
	if safe.is_empty() {
		safe.push_str("advisor");
	}
	safe
}
#[cfg(test)]
mod tests {
	use std::{
		sync::{
			Arc,
			atomic::{AtomicU64, Ordering},
		},
		time::{Duration, Instant},
	};

	use super::*;

	static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

	#[derive(Clone)]
	struct ChangeSink(Arc<AtomicBool>);

	impl AdvisorStatisticsSink for ChangeSink {
		fn record_advisor_usage(&self, _: &str, _: &str, _: AdvisorUsageTotals) {}

		fn advisor_cost_changed(&self, _: &str) {
			self.0.store(true, Ordering::Release);
		}
	}

	fn temp_root() -> PathBuf {
		let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
		let root = std::env::temp_dir()
			.join(format!("omp-advisor-transcript-{}-{nonce}", std::process::id()));
		fs::create_dir_all(&root).expect("create transcript test root");
		root
	}

	fn record(kind: &'static str, content: &'static str, cost: i128) -> AdvisorTranscriptRecord {
		AdvisorTranscriptRecord {
			timestamp_ms: 0,
			advisor_id:   Str::new_static("reviewer"),
			kind:         Str::new(kind),
			content:      Str::new(content),
			usage:        AdvisorUsageTotals { cost_micro_usd: cost, ..AdvisorUsageTotals::default() },
		}
	}

	fn wait_for_restore<S: AdvisorStatisticsSink>(store: &AdvisorTranscriptStore<S>) {
		let deadline = Instant::now() + Duration::from_secs(2);
		while !store.cost_restore_finished() {
			assert!(Instant::now() < deadline, "advisor cost restore did not settle");
			thread::sleep(Duration::from_millis(1));
		}
	}

	#[test]
	fn replay_dedup_is_positional_and_scoped_to_an_uncommitted_turn() {
		let root = temp_root();
		let mut store =
			AdvisorTranscriptStore::open(&root, "primary", NoopAdvisorStatistics).unwrap();
		store.begin_turn("reviewer");
		store.append(&record("prompt", "same delta", 0)).unwrap();
		store.append(&record("prompt", "same delta", 0)).unwrap();
		store
			.append(&record("assistant", "billed retry", 25))
			.unwrap();
		store.append(&record("tool", "same result", 0)).unwrap();
		store.begin_turn("reviewer");
		store.append(&record("prompt", "same delta", 0)).unwrap();
		store.append(&record("prompt", "same delta", 0)).unwrap();
		store
			.append(&record("assistant", "billed retry", 25))
			.unwrap();
		store.append(&record("tool", "same result", 0)).unwrap();
		store.commit_turn("reviewer");
		store.begin_turn("reviewer");
		store.append(&record("prompt", "same delta", 0)).unwrap();
		store.commit_turn("reviewer");
		store.begin_turn("reviewer");
		store.append(&record("prompt", "same delta", 0)).unwrap();
		store.abandon_turn("reviewer");
		store.begin_turn("reviewer");
		store.append(&record("prompt", "same delta", 0)).unwrap();
		store.commit_turn("reviewer");
		wait_for_restore(&store);

		let lines = BufReader::new(File::open(store.path_for("reviewer")).unwrap())
			.lines()
			.collect::<Result<Vec<_>, _>>()
			.unwrap();
		let records = lines
			.iter()
			.map(|line| serde_json::from_str::<AdvisorTranscriptRecord>(line).unwrap())
			.collect::<Vec<_>>();
		assert_eq!(
			records
				.iter()
				.filter(|record| record.kind == "prompt")
				.count(),
			5
		);
		assert_eq!(
			records
				.iter()
				.filter(|record| record.kind == "assistant")
				.count(),
			2
		);
		assert_eq!(
			records
				.iter()
				.filter(|record| record.kind == "tool")
				.count(),
			2
		);
		store.cancel_cost_restore();
		drop(store);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn restore_reconciliation_adds_only_post_snapshot_usage() {
		let restored = BTreeMap::from([(Str::new_static("reviewer"), AdvisorUsageTotals {
			input_tokens: 500,
			cost_micro_usd: 500_000,
			..AdvisorUsageTotals::default()
		})]);
		let baseline = BTreeMap::from([(Str::new_static("reviewer"), AdvisorUsageTotals {
			input_tokens: 100,
			cost_micro_usd: 100_000,
			..AdvisorUsageTotals::default()
		})]);
		let current = BTreeMap::from([(Str::new_static("reviewer"), AdvisorUsageTotals {
			input_tokens: 350,
			cost_micro_usd: 350_000,
			..AdvisorUsageTotals::default()
		})]);
		assert_eq!(
			reconcile_totals(&restored, &current, &baseline)["reviewer"],
			AdvisorUsageTotals {
				input_tokens: 750,
				cost_micro_usd: 750_000,
				..AdvisorUsageTotals::default()
			}
		);
	}
	#[test]
	fn background_restore_hydrates_totals_and_emits_cost_change() {
		let root = temp_root();
		let transcript_root = root.join(".omp").join("advisors").join("primary");
		fs::create_dir_all(&transcript_root).unwrap();
		let path = transcript_root.join("reviewer.jsonl");
		{
			let mut file = append_file(&path).unwrap();
			serde_json::to_writer(&mut file, &record("assistant", "historical", 500_000)).unwrap();
			file.write_all(b"\n").unwrap();
			file.flush().unwrap();
		}
		let changed = Arc::new(AtomicBool::new(false));
		let store =
			AdvisorTranscriptStore::open(&root, "primary", ChangeSink(Arc::clone(&changed))).unwrap();
		wait_for_restore(&store);
		assert_eq!(store.totals("reviewer").cost_micro_usd, 500_000);
		assert!(changed.load(Ordering::Acquire));
		drop(store);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn cost_scan_stops_at_each_captured_byte_length() {
		let root = temp_root();
		let path = root.join("reviewer.jsonl");
		let before = record("assistant", "before", 250_000);
		let after = record("assistant", "after", 500_000);
		{
			let mut file = append_file(&path).unwrap();
			serde_json::to_writer(&mut file, &before).unwrap();
			file.write_all(b"\n").unwrap();
			file.flush().unwrap();
		}
		let max_bytes = fs::metadata(&path).unwrap().len();
		{
			let mut file = append_file(&path).unwrap();
			serde_json::to_writer(&mut file, &after).unwrap();
			file.write_all(b"\n").unwrap();
			file.flush().unwrap();
		}
		let cancelled = AtomicBool::new(false);
		let restored = scan_cost_snapshots(
			&[CostFileSnapshot { path, advisor_id: Str::new_static("reviewer"), max_bytes }],
			&cancelled,
		);
		assert_eq!(restored["reviewer"].cost_micro_usd, 250_000);
		fs::remove_dir_all(root).unwrap();
	}
	#[test]
	fn cancelled_restore_skips_remaining_file_snapshots() {
		let cancelled = AtomicBool::new(true);
		let restored = scan_cost_snapshots(
			&[CostFileSnapshot {
				path:       PathBuf::from("must-not-be-opened.jsonl"),
				advisor_id: Str::new_static("reviewer"),
				max_bytes:  u64::MAX,
			}],
			&cancelled,
		);
		assert!(restored.is_empty());
	}
}
