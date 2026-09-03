//! Journal-derived automatic and manual context compaction.

use std::{str::FromStr, sync::Arc};

use futures::StreamExt;
use omp_core::{Str, StrMut};
use omp_dom::{Dom, PropId, PropKey, Value};
use omp_inference::{
	ChatEvent, ChatRequest, ContentPart, MediaInput, Message, OpaqueJson, Role, Setting, ToolChoice,
	ToolInputConstraint, ToolResultContent,
};
use omp_journal::EntryId;

use crate::director::{BoxFut, Director, DirectorError, MutDirectorCx, Prepared};

const DEFAULT_THRESHOLD: f64 = 0.80;
const BYTES_PER_TOKEN: u64 = 4;
const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
const TOOL_OVERHEAD_TOKENS: u64 = 16;
const MEDIA_OVERHEAD_TOKENS: u64 = 256;
const SUMMARY_INSTRUCTION: &str = include_str!("../../prompts/compaction/handoff-document.md");

/// Compacts projected history before inference when its estimated occupancy
/// reaches the threshold.
#[derive(Clone, Debug, Default)]
pub struct CompactionDirector {
	focus:  Option<Str>,
	manual: bool,
}

impl CompactionDirector {
	/// Creates the standard automatic compaction director.
	#[must_use]
	pub const fn new() -> Self {
		Self { focus: None, manual: false }
	}

	/// Creates a one-shot manual compaction request with optional summary focus.
	#[must_use]
	pub const fn manual(focus: Option<Str>) -> Self {
		Self { focus, manual: true }
	}

	async fn compact(
		&self,
		cx: &mut MutDirectorCx<'_>,
		request: &ChatRequest,
	) -> Result<Prepared, DirectorError> {
		let Some(boundary) = cx.session.head() else {
			return Ok(Prepared::Unchanged);
		};
		if !self.manual {
			if newest_compaction_is_head(cx.session.dom(), boundary)
				|| !over_threshold(
					request,
					cx.route.context_window,
					compact_threshold(cx.session.dom()),
				) {
				return Ok(Prepared::Unchanged);
			}
		}

		let summary_request = summary_request(request, self.focus.as_deref());
		let mut stream = cx.inference.execute(summary_request).await?;
		let mut summary = StrMut::new("");
		while let Some(event) = stream.next().await {
			if let ChatEvent::TextDelta { text, .. } = event? {
				summary.push_str(text.as_str());
			}
		}
		let summary = summary.freeze();
		if summary.trim().is_empty() {
			return Err(DirectorError::EmptyCompactionSummary);
		}
		let blob = cx.blobs.put(summary.as_bytes())?;
		cx.session.compaction(blob, boundary)?;
		Ok(Prepared::Rebuild)
	}
}

impl Director for CompactionDirector {
	fn id(&self) -> &'static str {
		"compaction"
	}

	fn before_inference<'a>(
		&'a self,
		cx: &'a mut MutDirectorCx<'_>,
		request: &'a ChatRequest,
	) -> BoxFut<'a, Result<Prepared, DirectorError>> {
		Box::pin(self.compact(cx, request))
	}
}

fn summary_request(request: &ChatRequest, focus: Option<&str>) -> ChatRequest {
	let mut instruction = StrMut::new(SUMMARY_INSTRUCTION);
	if let Some(focus) = focus.filter(|focus| !focus.trim().is_empty()) {
		instruction.push_str("\n\nFocus the handoff on: ");
		instruction.push_str(focus);
	}
	let instruction = Message {
		role:    Role::System,
		content: Arc::from([ContentPart::Text { text: instruction.freeze(), proof: None }]),
		name:    None,
	};
	let mut messages = Vec::with_capacity(request.messages.len().saturating_add(1));
	messages.push(instruction);
	messages.extend(request.messages.iter().cloned());
	let mut summary = request.clone();
	summary.messages = messages.into();
	summary.tools = Arc::from([]);
	summary.hosted_tools = Arc::from([]);
	summary.tool_choice = Setting::Require(ToolChoice::Disabled);
	summary
}

fn over_threshold(request: &ChatRequest, context_window: u64, threshold: f64) -> bool {
	if context_window == 0 {
		return false;
	}
	let usable_window = context_window.saturating_sub(request.max_output_tokens.unwrap_or_default());
	if usable_window == 0 {
		return true;
	}
	let target = (usable_window as f64 * threshold).floor() as u64;
	estimate_request_tokens(request) >= target
}

fn compact_threshold(dom: &Dom) -> f64 {
	let Ok(vars) = dom.select("con var") else {
		return DEFAULT_THRESHOLD;
	};
	for handle in vars {
		let Some(node) = dom.get(handle) else {
			continue;
		};
		let name = node
			.prop(&PropKey::from(PropId::Name))
			.or_else(|| node.prop(&PropKey::Custom(Str::new_static("name"))))
			.and_then(Value::as_str);
		if name != Some("ai_compact_threshold") {
			continue;
		}
		let value = node
			.prop(&PropKey::from(PropId::Value))
			.or_else(|| node.prop(&PropKey::Custom(Str::new_static("value"))))
			.and_then(threshold_value)
			.or_else(|| node.content.as_deref().and_then(|value| value.parse().ok()));
		return value
			.filter(|value| value.is_finite() && *value > 0.0 && *value <= 1.0)
			.unwrap_or(DEFAULT_THRESHOLD);
	}
	DEFAULT_THRESHOLD
}

fn threshold_value(value: &Value) -> Option<f64> {
	match value {
		Value::Float(value) => Some(*value),
		Value::Int(value) => Some(*value as f64),
		Value::Str(value) => value.parse().ok(),
		_ => None,
	}
}

fn newest_compaction_is_head(dom: &Dom, head: EntryId) -> bool {
	dom.children(dom.meta()).iter().rev().any(|handle| {
		let Some(node) = dom.get(*handle) else {
			return false;
		};
		if node.tag.as_str() != "compaction" {
			return false;
		}
		node
			.prop(&PropKey::from(PropId::Cause))
			.and_then(Value::as_str)
			.and_then(|value| EntryId::from_str(value).ok())
			== Some(head)
	})
}

fn estimate_request_tokens(request: &ChatRequest) -> u64 {
	let message_bytes = request.messages.iter().fold(0_u64, |total, message| {
		total
			.saturating_add(estimate_message_bytes(message))
			.saturating_add(MESSAGE_OVERHEAD_TOKENS.saturating_mul(BYTES_PER_TOKEN))
	});
	let tool_bytes = request.tools.iter().fold(0_u64, |total, tool| {
		let schema = match &tool.input {
			ToolInputConstraint::JsonSchema { parameters, .. } => estimate_json_bytes(parameters),
			ToolInputConstraint::Grammar { grammar, fallback } => {
				u64_len(grammar.definition.len()).saturating_add(estimate_json_bytes(fallback))
			},
		};
		total
			.saturating_add(u64_len(tool.name.len()))
			.saturating_add(
				tool
					.description
					.as_ref()
					.map_or(0, |text| u64_len(text.len())),
			)
			.saturating_add(schema)
			.saturating_add(TOOL_OVERHEAD_TOKENS.saturating_mul(BYTES_PER_TOKEN))
	});
	message_bytes
		.saturating_add(tool_bytes)
		.div_ceil(BYTES_PER_TOKEN)
}

fn estimate_message_bytes(message: &Message) -> u64 {
	message.content.iter().fold(0_u64, |total, part| {
		total.saturating_add(match part {
			ContentPart::Text { text, .. } | ContentPart::Reasoning { text, .. } => {
				u64_len(text.len())
			},
			ContentPart::Image(media) | ContentPart::Audio(media) | ContentPart::Document(media) => {
				estimate_media_bytes(media)
			},
			ContentPart::ToolCall { name, arguments, .. } => {
				u64_len(name.len()).saturating_add(estimate_json_bytes(arguments))
			},
			ContentPart::ToolResult { name, content, .. } => name
				.as_ref()
				.map_or(0, |name| u64_len(name.len()))
				.saturating_add(content.iter().fold(0_u64, |subtotal, item| {
					subtotal.saturating_add(estimate_tool_result_bytes(item))
				})),
			ContentPart::CachePoint(_) => 0,
		})
	})
}

fn estimate_tool_result_bytes(content: &ToolResultContent) -> u64 {
	match content {
		ToolResultContent::Text(text) => u64_len(text.len()),
		ToolResultContent::Json(value) => estimate_json_bytes(value),
		ToolResultContent::Image(media) | ToolResultContent::Document(media) => {
			estimate_media_bytes(media)
		},
	}
}

fn estimate_media_bytes(media: &MediaInput) -> u64 {
	match media {
		MediaInput::Bytes { data, .. } => u64_len(data.len()),
		MediaInput::Stored(_) | MediaInput::Body { .. } => {
			MEDIA_OVERHEAD_TOKENS.saturating_mul(BYTES_PER_TOKEN)
		},
		MediaInput::Remote { uri, .. } => {
			u64_len(uri.len()).saturating_add(MEDIA_OVERHEAD_TOKENS.saturating_mul(BYTES_PER_TOKEN))
		},
	}
}

fn estimate_json_bytes(value: &OpaqueJson) -> u64 {
	fn value_bytes(value: &serde_json::Value) -> u64 {
		match value {
			serde_json::Value::Null => 4,
			serde_json::Value::Bool(value) => {
				if *value {
					4
				} else {
					5
				}
			},
			serde_json::Value::Number(_) => 24,
			serde_json::Value::String(value) => u64_len(value.len()).saturating_add(2),
			serde_json::Value::Array(values) => values
				.iter()
				.fold(2_u64, |total, value| total.saturating_add(value_bytes(value)).saturating_add(1)),
			serde_json::Value::Object(values) => values.iter().fold(2_u64, |total, (key, value)| {
				total
					.saturating_add(u64_len(key.len()))
					.saturating_add(value_bytes(value))
					.saturating_add(4)
			}),
		}
	}
	value_bytes(value.as_value())
}

fn u64_len(length: usize) -> u64 {
	u64::try_from(length).unwrap_or(u64::MAX)
}
