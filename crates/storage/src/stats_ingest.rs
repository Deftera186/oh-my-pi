//! Background incremental transcript ingestion with durable byte watermarks.

use std::{
	fmt, fs,
	fs::File,
	io,
	io::{Read as _, Seek as _, SeekFrom},
	path::{Path, PathBuf},
	sync::Arc,
	thread,
	time::UNIX_EPOCH,
};

use flume::Receiver;
use omp_core::Str;
use omp_observability::{sentiment::analyze_user_sentiment, stats::LocalAnalyticsConsent};
use serde_json::Value;
use thiserror::Error;

use crate::stats_db::{AgentType, MessageFact, StatsDb, StatsDbError, ToolCallFact};

/// Result of scanning one journal suffix.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanReport {
	/// Complete JSON lines examined.
	pub lines:          u64,
	/// Structural message rows written.
	pub messages:       u64,
	/// Derived user-counter rows written.
	pub user_messages:  u64,
	/// Tool-call rows written.
	pub tool_calls:     u64,
	/// Malformed complete lines skipped.
	pub malformed:      u64,
	/// New complete-line byte watermark.
	pub byte_watermark: u64,
}

/// Incremental scan failure.
#[derive(Debug, Error)]
pub enum StatsIngestError {
	/// Journal I/O failed.
	#[error("failed to scan statistics journal")]
	Io(#[from] io::Error),
	/// Statistics database operation failed.
	#[error(transparent)]
	Database(#[from] StatsDbError),
	/// Worker pool is shutting down.
	#[error("statistics ingestion worker pool is closed")]
	Closed,
}

struct Job {
	path:     PathBuf,
	response: flume::Sender<Result<ScanReport, StatsIngestError>>,
}

/// Fixed-size background journal scanner.
pub struct StatsIngestor {
	sender:  Option<flume::Sender<Job>>,
	workers: Vec<thread::JoinHandle<()>>,
}

impl fmt::Debug for StatsIngestor {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("StatsIngestor")
			.field("workers", &self.workers.len())
			.finish_non_exhaustive()
	}
}

impl StatsIngestor {
	/// Starts a bounded worker pool. `worker_count` is clamped to at least one.
	pub fn new(database: Arc<StatsDb>, worker_count: usize, queue_capacity: usize) -> Self {
		let (sender, receiver) = flume::bounded(queue_capacity.max(1));
		let mut workers = Vec::with_capacity(worker_count.max(1));
		for index in 0..worker_count.max(1) {
			let receiver = receiver.clone();
			let database = Arc::clone(&database);
			workers.push(
				thread::Builder::new()
					.name(format!("omp-stats-ingest-{index}"))
					.spawn(move || worker(&database, &receiver))
					.expect("statistics ingestion worker must start"),
			);
		}
		Self { sender: Some(sender), workers }
	}

	/// Queues one journal and returns a one-shot completion receiver.
	pub fn scan(
		&self,
		path: impl Into<PathBuf>,
	) -> Result<Receiver<Result<ScanReport, StatsIngestError>>, StatsIngestError> {
		let (response, receiver) = flume::bounded(1);
		self
			.sender
			.as_ref()
			.ok_or(StatsIngestError::Closed)?
			.send(Job { path: path.into(), response })
			.map_err(|_| StatsIngestError::Closed)?;
		Ok(receiver)
	}
}

impl Drop for StatsIngestor {
	fn drop(&mut self) {
		self.sender.take();
		for worker in self.workers.drain(..) {
			let _ = worker.join();
		}
	}
}

fn worker(database: &StatsDb, receiver: &Receiver<Job>) {
	while let Ok(job) = receiver.recv() {
		let result = scan_journal(database, &job.path);
		let _ = job.response.send(result);
	}
}

/// Scans only complete bytes after the durable watermark.
///
/// Disabled consent does not advance the watermark, so enabling analytics
/// later performs a complete private backfill.
#[tracing::instrument(
	name = "statistics_journal_scan",
	level = "debug",
	skip_all,
	fields(path = %path.display())
)]
pub fn scan_journal(database: &StatsDb, path: &Path) -> Result<ScanReport, StatsIngestError> {
	if database.consent() == LocalAnalyticsConsent::Disabled {
		return Ok(ScanReport::default());
	}
	let canonical = fs::canonicalize(path)?;
	let mut file = File::open(&canonical)?;
	let size = file.metadata()?.len();
	let prior = database.file_offset(&canonical)?.min(size);
	file.seek(SeekFrom::Start(prior))?;
	let mut bytes = Vec::new();
	file.read_to_end(&mut bytes)?;
	let complete = bytes
		.iter()
		.rposition(|byte| *byte == b'\n')
		.map_or(0, |index| index + 1);
	let mut report = ScanReport { byte_watermark: prior + complete as u64, ..ScanReport::default() };
	let agent_type = classify_agent(&canonical, None);
	let mut cursor = prior;
	for line in bytes[..complete].split(|byte| *byte == b'\n') {
		if line.is_empty() {
			cursor = cursor.saturating_add(1);
			continue;
		}
		report.lines = report.lines.saturating_add(1);
		let line_end = cursor.saturating_add(line.len() as u64);
		let Ok(value) = serde_json::from_slice::<Value>(line) else {
			report.malformed = report.malformed.saturating_add(1);
			cursor = line_end.saturating_add(1);
			continue;
		};
		if value.get("v").is_some() && value.get("id").is_some() && value.get("created").is_some() {
			cursor = line_end.saturating_add(1);
			continue;
		}
		let entry_id = string_field(&value, &["entry_id", "entryId", "id", "i"])
			.map_or_else(|| Str::new(line_end.to_string()), Str::new);
		let kind = string_field(&value, &["kind", "k", "type"]).unwrap_or("event");
		let role = string_field(&value, &["role"]).unwrap_or_else(|| role_from_kind(kind));
		let classified = classify_agent(&canonical, Some(&value));
		let fact = MessageFact {
			session_file: &canonical,
			entry_id: entry_id.as_str(),
			timestamp_ms: u64_field(&value, &["timestamp_ms", "timestampMs", "ts", "created"]),
			provider: string_field(&value, &["provider"]),
			model: string_field(&value, &["model"]),
			role,
			agent_type: if classified == AgentType::Main {
				agent_type
			} else {
				classified
			},
			input_tokens: u64_field(&value, &["input_tokens", "inputTokens"]),
			output_tokens: u64_field(&value, &["output_tokens", "outputTokens"]),
		};
		database.insert_message(&fact)?;
		report.messages = report.messages.saturating_add(1);
		if role == "user" || kind.to_ascii_lowercase().contains("user") {
			let mut text = String::new();
			collect_user_text(&value, &mut text);
			let metrics = analyze_user_sentiment(&text);
			database.insert_user_metrics(&canonical, entry_id.as_str(), metrics)?;
			report.user_messages = report.user_messages.saturating_add(1);
		}
		if let Some(tool_name) = string_field(&value, &["tool_name", "toolName", "name"])
			&& (kind.to_ascii_lowercase().contains("tool") || value.get("call_id").is_some())
		{
			database.insert_tool_call(&ToolCallFact {
				session_file: &canonical,
				entry_id: entry_id.as_str(),
				tool_name,
				outcome: string_field(&value, &["outcome", "status"]),
				duration_ms: u64_field(&value, &["duration_ms", "durationMs"]),
				agent_type: classified,
			})?;
			report.tool_calls = report.tool_calls.saturating_add(1);
		}
		cursor = line_end.saturating_add(1);
	}
	let modified_ns = file.metadata()?.modified().ok().and_then(|modified| {
		modified.duration_since(UNIX_EPOCH).ok().map(|duration| {
			duration
				.as_secs()
				.saturating_mul(1_000_000_000)
				.saturating_add(u64::from(duration.subsec_nanos()))
		})
	});
	database.set_file_offset(&canonical, report.byte_watermark, modified_ns)?;
	if report.malformed != 0 {
		tracing::warn!(
			malformed_record_count = report.malformed,
			"statistics journal scan skipped malformed records"
		);
	}
	tracing::debug!(
		event_count = report.lines,
		message_count = report.messages,
		user_message_count = report.user_messages,
		tool_call_count = report.tool_calls,
		"statistics journal scan completed"
	);
	Ok(report)
}

fn classify_agent(path: &Path, value: Option<&Value>) -> AgentType {
	let path = path.to_string_lossy().to_ascii_lowercase();
	let declared = value
		.and_then(|value| string_field(value, &["agent_type", "agentType", "kind", "role"]))
		.unwrap_or("")
		.to_ascii_lowercase();
	if declared.contains("advisor") || path.contains("advisor") || path.contains("reviewer") {
		AgentType::Advisor
	} else if declared.contains("subagent")
		|| declared.contains("task")
		|| path.contains("subagent")
		|| path.contains("/agent-")
	{
		AgentType::Subagent
	} else {
		AgentType::Main
	}
}

fn string_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a str> {
	names
		.iter()
		.find_map(|name| value.get(*name).and_then(Value::as_str))
}

fn u64_field(value: &Value, names: &[&str]) -> Option<u64> {
	names
		.iter()
		.find_map(|name| value.get(*name).and_then(Value::as_u64))
}

fn role_from_kind(kind: &str) -> &str {
	let lower = kind.to_ascii_lowercase();
	if lower.contains("user") {
		"user"
	} else if lower.contains("assistant") {
		"assistant"
	} else if lower.contains("tool") {
		"tool"
	} else {
		"event"
	}
}

fn collect_user_text(value: &Value, output: &mut String) {
	match value {
		Value::Array(values) => {
			for value in values {
				collect_user_text(value, output);
			}
		},
		Value::Object(object) => {
			for (key, value) in object {
				if matches!(key.as_str(), "text" | "content" | "prompt") {
					if let Some(text) = value.as_str() {
						if !output.is_empty() {
							output.push('\n');
						}
						output.push_str(text);
					}
				} else if !matches!(
					key.as_str(),
					"thinking" | "raw" | "provider_metadata" | "arguments"
				) {
					collect_user_text(value, output);
				}
			}
		},
		Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {},
	}
}
