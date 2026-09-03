//! Observer-facing notifications derived while journal entries are committed.

use std::{sync::Arc, time::Duration};

use omp_core::Str;
use omp_inference::{ContentPart, Message};
use parking_lot::Mutex;

/// Ephemeral notification for hosts that want immediate turn progress.
///
/// The session journal and DOM remain authoritative; dropping these events
/// cannot change replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelEvent {
	/// An inference response selected its concrete route.
	InferenceStarted,
	/// The transport layer is about to wait `delay` before same-route retry
	/// `attempt` of `max_attempts` (pi `auto_retry_start`). Pre-commit and
	/// replay-irrelevant, so it is never journaled.
	InferenceRetry {
		/// One-based retry number.
		attempt:      u32,
		/// Retry cap on this route.
		max_attempts: u32,
		/// Backoff before the retry.
		delay:        Duration,
		/// Human-readable failure summary.
		reason:       Str,
	},
	/// Cumulative provider usage observed mid-stream; ephemeral.
	Usage {
		/// Output tokens so far.
		output_tokens:    u64,
		/// Reasoning tokens so far.
		reasoning_tokens: u64,
	},
	/// One explicit turn returned control (pi `agent_end`); the journal
	/// carries the durable outcome, this only wakes hosts promptly.
	TurnEnded {
		/// Why the kernel stopped.
		stop: crate::TurnStop,
	},
	/// Visible assistant text arrived.
	TextDelta(Str),
	/// Assistant reasoning text arrived.
	ThinkingDelta(Str),
	/// A validated tool call became executable.
	ToolReady {
		/// Stable provider call identity.
		call_id: Str,
		/// Resolved tool name.
		name:    Str,
	},
	/// A tool emitted an ephemeral update.
	ToolUpdate {
		/// Stable provider call identity.
		call_id: Str,
	},
	/// A tool reached a durable terminal outcome.
	ToolSettled {
		/// Stable provider call identity.
		call_id:  Str,
		/// Whether the outcome is model-facing error content.
		is_error: bool,
	},
}

#[derive(Clone, Default)]
pub(crate) struct KernelEvents {
	subscribers: Arc<Mutex<Vec<flume::Sender<KernelEvent>>>>,
}

impl KernelEvents {
	pub(crate) fn subscribe(&self) -> flume::Receiver<KernelEvent> {
		let (sender, receiver) = flume::unbounded();
		self.subscribers.lock().push(sender);
		receiver
	}

	pub(crate) fn publish(&self, event: KernelEvent) {
		self
			.subscribers
			.lock()
			.retain(|sender| sender.send(event.clone()).is_ok());
	}
}

pub(crate) fn strip_unsigned_reasoning(messages: &mut [Message]) {
	for message in messages {
		if message
			.content
			.iter()
			.any(|part| matches!(part, ContentPart::Reasoning { proof: None, .. }))
		{
			message.content = message
				.content
				.iter()
				.filter(|part| !matches!(part, ContentPart::Reasoning { proof: None, .. }))
				.cloned()
				.collect::<Vec<_>>()
				.into();
		}
	}
}
