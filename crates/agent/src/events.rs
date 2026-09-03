//! Observer-facing notifications derived while journal entries are committed.

use std::sync::Arc;

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
