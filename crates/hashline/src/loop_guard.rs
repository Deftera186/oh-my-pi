//! Session-local escalation for repeated byte-identical no-op edit payloads.

use std::collections::HashMap;

use bytes::Bytes;
use omp_core::Str;
use xxhash_rust::xxh32::xxh32;

/// Consecutive identical no-ops required before a hard diagnostic.
pub const NOOP_HARD_LIMIT: usize = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
struct NoopEntry {
	payload: Bytes,
	count:   usize,
}

/// Severity of a repeated no-op diagnostic.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoopSeverity {
	/// The caller should return guidance while allowing a corrected retry.
	Soft,
	/// The caller should fail the tool call to break an ignored retry loop.
	Hard,
}

/// Outcome of recording one byte-identical no-op payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoopRecord {
	count:      usize,
	severity:   NoopSeverity,
	diagnostic: Str,
}

impl NoopRecord {
	/// Returns the consecutive identical no-op count including this attempt.
	pub const fn count(&self) -> usize {
		self.count
	}

	/// Returns whether guidance remains soft or must be raised as a failure.
	pub const fn severity(&self) -> NoopSeverity {
		self.severity
	}

	/// Returns the user-facing escalating diagnostic.
	pub fn diagnostic(&self) -> &str {
		&self.diagnostic
	}

	/// Returns true when the caller must escalate to a tool failure.
	pub const fn should_escalate(&self) -> bool {
		matches!(self.severity, NoopSeverity::Hard)
	}
}

/// Per-session counters isolated by canonical path and exact raw payload bytes.
#[derive(Clone, Debug, Default)]
pub struct NoopLoopGuard {
	entries: HashMap<Str, NoopEntry>,
}

impl NoopLoopGuard {
	/// Creates an empty per-session guard.
	pub fn new() -> Self {
		Self::default()
	}

	/// Records a no-op using its canonical path for loop identity and display.
	pub fn record_noop(&mut self, canonical_path: impl Into<Str>, payload: Bytes) -> NoopRecord {
		let canonical_path = canonical_path.into();
		self.record_noop_for(canonical_path.clone(), &canonical_path, payload)
	}

	/// Records a no-op while keeping canonical identity separate from the
	/// authored path shown in diagnostics.
	///
	/// Exact payload bytes are retained so a hash collision can never escalate a
	/// different edit. A different payload on the same path starts again at one.
	pub fn record_noop_for(
		&mut self,
		canonical_path: impl Into<Str>,
		display_path: &str,
		payload: Bytes,
	) -> NoopRecord {
		let canonical_path = canonical_path.into();
		let count = self
			.entries
			.get(&canonical_path)
			.filter(|entry| entry.payload == payload)
			.map_or(1, |entry| entry.count.saturating_add(1));
		self
			.entries
			.insert(canonical_path, NoopEntry { payload, count });
		let severity = if count >= NOOP_HARD_LIMIT {
			NoopSeverity::Hard
		} else {
			NoopSeverity::Soft
		};
		let diagnostic = match severity {
			NoopSeverity::Soft => Str::from(format!(
				"Edits to {display_path} parsed and applied cleanly, but produced no change: your \
				 body row(s) are byte-identical to the file at the targeted lines. The bug is \
				 somewhere else — re-read the file before issuing another edit. Do NOT widen the \
				 payload or add lines; verify the anchor first."
			)),
			NoopSeverity::Hard => Str::from(format!(
				"STOP. Edits to {display_path} have been a byte-identical no-op {count} times in a \
				 row — the patch body matches the file at the targeted lines and the soft hint did \
				 not break the cycle. Cease re-issuing this payload. Either the intended change is \
				 already on disk (move on), or your anchor is wrong (re-read the file with `read` to \
				 observe the current line numbers and tag, then author a different edit). This exact \
				 payload will keep being rejected until it changes."
			)),
		};
		NoopRecord { count, severity, diagnostic }
	}

	/// Clears one path after a non-noop commit so a future no-op starts soft.
	pub fn reset(&mut self, canonical_path: &str) {
		self.entries.remove(canonical_path);
	}

	/// Clears every per-path counter in this session.
	pub fn clear(&mut self) {
		self.entries.clear();
	}

	/// Returns the number of canonical paths with an active no-op counter.
	pub fn len(&self) -> usize {
		self.entries.len()
	}

	/// Returns whether the guard contains no active counters.
	pub fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}
}

/// Computes a stable compact fingerprint for logs and metrics.
///
/// Loop identity deliberately uses retained exact bytes instead of this hash.
pub fn hash_patch_input(input: &[u8]) -> u32 {
	xxh32(input, 0)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn first_attempts_are_soft_and_third_identical_attempt_is_hard() {
		let mut guard = NoopLoopGuard::new();
		for count in 1..NOOP_HARD_LIMIT {
			let record = guard.record_noop("a.rs", Bytes::from_static(b"same"));
			assert_eq!(record.count(), count);
			assert_eq!(record.severity(), NoopSeverity::Soft);
			assert!(!record.diagnostic().contains("STOP."));
		}
		let record = guard.record_noop("a.rs", Bytes::from_static(b"same"));
		assert_eq!(record.count(), NOOP_HARD_LIMIT);
		assert!(record.should_escalate());
		assert!(record.diagnostic().contains("STOP."));
		assert!(record.diagnostic().contains("a.rs"));
	}

	#[test]
	fn diagnostics_match_pi_exactly() {
		let mut guard = NoopLoopGuard::new();
		let soft = guard.record_noop("src/a.rs", Bytes::from_static(b"same"));
		assert_eq!(
			soft.diagnostic(),
			"Edits to src/a.rs parsed and applied cleanly, but produced no change: your body row(s) \
			 are byte-identical to the file at the targeted lines. The bug is somewhere else — \
			 re-read the file before issuing another edit. Do NOT widen the payload or add lines; \
			 verify the anchor first."
		);
		guard.record_noop("src/a.rs", Bytes::from_static(b"same"));
		let hard = guard.record_noop("src/a.rs", Bytes::from_static(b"same"));
		assert_eq!(
			hard.diagnostic(),
			"STOP. Edits to src/a.rs have been a byte-identical no-op 3 times in a row — the patch \
			 body matches the file at the targeted lines and the soft hint did not break the cycle. \
			 Cease re-issuing this payload. Either the intended change is already on disk (move on), \
			 or your anchor is wrong (re-read the file with `read` to observe the current line \
			 numbers and tag, then author a different edit). This exact payload will keep being \
			 rejected until it changes."
		);
	}

	#[test]
	fn canonical_identity_does_not_leak_into_diagnostic() {
		let mut guard = NoopLoopGuard::new();
		let record =
			guard.record_noop_for("/workspace/src/a.rs", "src/a.rs", Bytes::from_static(b"same"));
		assert!(record.diagnostic().starts_with("Edits to src/a.rs "));
		assert!(!record.diagnostic().contains("/workspace"));
	}

	#[test]
	fn different_payload_and_successful_commit_reset_the_count() {
		let mut guard = NoopLoopGuard::new();
		guard.record_noop("a.rs", Bytes::from_static(b"one"));
		guard.record_noop("a.rs", Bytes::from_static(b"one"));
		assert_eq!(
			guard
				.record_noop("a.rs", Bytes::from_static(b"two"))
				.count(),
			1
		);
		guard.reset("a.rs");
		assert_eq!(
			guard
				.record_noop("a.rs", Bytes::from_static(b"two"))
				.count(),
			1
		);
	}

	#[test]
	fn canonical_paths_and_sessions_are_isolated() {
		let mut first = NoopLoopGuard::new();
		first.record_noop("a.rs", Bytes::from_static(b"same"));
		first.record_noop("a.rs", Bytes::from_static(b"same"));
		assert_eq!(
			first
				.record_noop("b.rs", Bytes::from_static(b"same"))
				.count(),
			1
		);
		let mut second = NoopLoopGuard::new();
		assert_eq!(
			second
				.record_noop("a.rs", Bytes::from_static(b"same"))
				.count(),
			1
		);
	}

	#[test]
	fn patch_fingerprint_is_stable_and_byte_sensitive() {
		assert_eq!(hash_patch_input(b"payload"), hash_patch_input(b"payload"));
		assert_ne!(hash_patch_input(b"payload"), hash_patch_input(b"payload\n"));
	}
}
