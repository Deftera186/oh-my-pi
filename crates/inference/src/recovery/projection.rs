//! Projection of recovered wire/text fragments into canonical chat events.

use std::{collections::BTreeMap, io::Cursor};

use bytes::{Bytes, BytesMut};
use omp_core::Str;
use xutf::BufReadCharsExt as _;

use super::{
	RecoveryError, Stage,
	dialect::{DialectEvent, ToolEnvelope},
	tools::{
		ToolAssembler, ToolAssemblyEvent, ToolAssemblyLimits, ToolFragment, ToolPairing,
		ToolRegistration, ToolResultPairer, ToolResultSource,
	},
};
use crate::{
	call::ToolDefinition,
	codec::ToolInputKind,
	event::{BlockKind, ChatEvent},
	id::ToolCallId,
	receipt::{ReasonId, RecoveryKind, RecoveryRecord},
};

/// Competing source of a model-requested tool call.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ToolChannel {
	/// Structured tool fragments decoded from the provider wire protocol.
	Native,
	/// Tool fragments recovered from model-authored text markup.
	Text,
}

/// Input to the deterministic recovery projector.
#[derive(Clone, Debug)]
pub enum ProjectionInput {
	/// Ordinary scanner-validated model text.
	Text(Bytes),
	/// One partial tool fragment from either channel.
	Tool {
		/// Candidate source channel.
		channel:  ToolChannel,
		/// Next assembly fragment.
		fragment: ToolFragment,
	},
	/// Output from the catalog-selected in-band dialect scanner.
	Dialect(DialectEvent),
	/// A caller/tool-executor result to pair with an authorized call.
	CallerToolResult {
		/// Supplied call identity, absent on repairable wires.
		call: Option<ToolCallId>,
	},
	/// A result-like boundary authored by the model, which must abort
	/// projection.
	ModelToolResult,
}

/// Non-provider recovery failure that permanently stops projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectionFailure {
	/// The model attempted to fabricate a result after requesting a real tool.
	FabricatedToolResult,
	/// A caller result could not be paired to exactly one authorized call.
	UnpairedToolResult,
	/// Text bytes were not a valid incrementally assembled UTF-8 stream.
	InvalidUtf8,
	/// A duplicate identity or total-call bound prevented safe result pairing.
	ToolRegistrationRejected,
}

/// Bounded output of one projector operation.
#[derive(Debug, Default)]
pub struct ProjectionBatch {
	/// Canonical events, in stream order.
	pub events:   Vec<ChatEvent>,
	/// A terminal recovery failure, if projection stopped.
	pub failure:  Option<ProjectionFailure>,
	/// Deterministic recovery records produced by completed tool assembly.
	pub evidence: Vec<RecoveryRecord>,
}

/// Stateful projector enforcing one tool channel and fabricated-result
/// rejection.
#[derive(Debug)]
pub struct RecoveryProjector<'a> {
	native:                ToolAssembler<'a>,
	text_tools:            ToolAssembler<'a>,
	selected_channel:      Option<ToolChannel>,
	canonical_indexes:     BTreeMap<(ToolChannel, u32), u32>,
	next_index:            u32,
	text_index:            Option<u32>,
	next_text_tool_source: u32,
	calls_started:         usize,
	max_total_calls:       usize,
	pairer:                ToolResultPairer,
	attempt:               u32,
	pending_text:          BytesMut,
	stopped:               bool,
}

impl<'a> RecoveryProjector<'a> {
	/// Creates a projector for one attempt.
	pub fn new(definitions: &'a [ToolDefinition], limits: ToolAssemblyLimits, attempt: u32) -> Self {
		Self {
			native: ToolAssembler::new(definitions, limits, attempt),
			text_tools: ToolAssembler::new(definitions, limits, attempt),
			selected_channel: None,
			canonical_indexes: BTreeMap::new(),
			next_index: 0,
			text_index: None,
			next_text_tool_source: 0,
			pairer: ToolResultPairer::new(limits.max_total_calls),
			calls_started: 0,
			max_total_calls: limits.max_total_calls,
			pending_text: BytesMut::new(),
			stopped: false,
			attempt,
		}
	}

	/// Projects one incremental input. Once stopped, all later input is dropped.
	pub fn push(&mut self, input: ProjectionInput) -> ProjectionBatch {
		if self.stopped {
			return ProjectionBatch::default();
		}
		match input {
			ProjectionInput::Text(bytes) => self.project_text(bytes),
			ProjectionInput::Tool { channel, fragment } => self.project_tool(channel, fragment),
			ProjectionInput::Dialect(event) => self.project_dialect(event),
			ProjectionInput::CallerToolResult { call } => {
				match self.pairer.pair(call.as_deref(), ToolResultSource::Caller) {
					ToolPairing::Paired(_) => ProjectionBatch::default(),
					ToolPairing::Repaired(_) => ProjectionBatch {
						evidence: vec![self.evidence(
							RecoveryKind::ToolResultRepair,
							"tool.result-id-repaired",
							0,
						)],
						..ProjectionBatch::default()
					},
					ToolPairing::RejectedFabricated => {
						let mut output = self.stop(ProjectionFailure::UnpairedToolResult);
						output.evidence.push(self.evidence(
							RecoveryKind::FabricatedResultRejection,
							"tool.result-unpaired-rejected",
							0,
						));
						output
					},
				}
			},
			ProjectionInput::ModelToolResult => {
				let mut output = self.stop(ProjectionFailure::FabricatedToolResult);
				output.evidence.push(self.evidence(
					RecoveryKind::FabricatedResultRejection,
					"tool.fabricated-result-rejected",
					0,
				));
				output
			},
		}
	}

	/// Flushes retained delimiter suffixes and rejects incomplete tool calls.
	pub fn finish(&mut self) -> ProjectionBatch {
		if self.stopped {
			return ProjectionBatch::default();
		}
		let mut output = ProjectionBatch::default();
		self.flush_text(&mut output);
		let selected = self.selected_channel;
		for (channel, events) in [
			(ToolChannel::Native, self.native.finish()),
			(ToolChannel::Text, self.text_tools.finish()),
		] {
			if selected == Some(channel) {
				self.apply_tool_events(channel, events, &mut output);
			}
		}
		output.evidence.extend(self.native.take_evidence());
		output.evidence.extend(self.text_tools.take_evidence());
		output
	}

	/// Returns whether a terminal fabricated/unpaired result stopped the stream.
	pub const fn is_stopped(&self) -> bool {
		self.stopped
	}

	fn project_tool(&mut self, channel: ToolChannel, fragment: ToolFragment) -> ProjectionBatch {
		if self.stopped {
			return ProjectionBatch::default();
		}
		if self
			.selected_channel
			.is_some_and(|selected| selected != channel)
		{
			return ProjectionBatch::default();
		}
		let events = match channel {
			ToolChannel::Native => self.native.push(fragment),
			ToolChannel::Text => self.text_tools.push(fragment),
		};
		let mut output = ProjectionBatch::default();
		self.apply_tool_events(channel, events, &mut output);
		output.evidence.extend(match channel {
			ToolChannel::Native => self.native.take_evidence(),
			ToolChannel::Text => self.text_tools.take_evidence(),
		});
		output
	}

	fn project_dialect(&mut self, event: DialectEvent) -> ProjectionBatch {
		match event {
			DialectEvent::Text(bytes) => self.project_text(bytes),
			DialectEvent::ToolEnvelope(envelope) => self.project_envelope(envelope),
		}
	}

	fn project_envelope(&mut self, envelope: ToolEnvelope) -> ProjectionBatch {
		if self.selected_channel == Some(ToolChannel::Native) {
			return ProjectionBatch::default();
		}
		let source_index = self.next_text_tool_source;
		self.next_text_tool_source = self.next_text_tool_source.saturating_add(1);
		let name = envelope
			.name
			.map_or_else(Bytes::new, |name| Bytes::copy_from_slice(name.as_bytes()));
		let mut output = self.project_tool(ToolChannel::Text, ToolFragment::Start {
			source_index,
			id: None,
			name,
			input_kind: ToolInputKind::Json,
		});
		append_batch(
			&mut output,
			self.project_tool(ToolChannel::Text, ToolFragment::ArgumentsDelta {
				source_index,
				bytes: envelope.arguments,
			}),
		);
		append_batch(
			&mut output,
			self.project_tool(ToolChannel::Text, ToolFragment::End { source_index }),
		);
		output.evidence.push(envelope.recovery);
		output
	}

	fn apply_tool_events(
		&mut self,
		channel: ToolChannel,
		events: Vec<ToolAssemblyEvent>,
		output: &mut ProjectionBatch,
	) {
		for event in events {
			match event {
				ToolAssemblyEvent::Started { source_index, id, name } => {
					if self.calls_started >= self.max_total_calls {
						self.stopped = true;
						output.failure = Some(ProjectionFailure::ToolRegistrationRejected);
						output.evidence.push(self.evidence(
							RecoveryKind::ToolAssembly,
							"tool.total-call-limit",
							0,
						));
						continue;
					}
					self.calls_started += 1;
					if self.selected_channel.get_or_insert(channel) != &channel {
						continue;
					}
					let index = self.allocate_tool_index(channel, source_index);
					output
						.events
						.push(ChatEvent::BlockStarted { index, kind: BlockKind::ToolCall });
					output
						.events
						.push(ChatEvent::ToolCallStarted { index, id, name });
				},
				ToolAssemblyEvent::ArgumentsDelta { source_index, bytes } => {
					if self.selected_channel != Some(channel) {
						continue;
					}
					if let Some(&index) = self.canonical_indexes.get(&(channel, source_index)) {
						output
							.events
							.push(ChatEvent::ToolArgumentsDelta { index, bytes });
					}
				},
				ToolAssemblyEvent::Ready { source_index, call } => {
					if self.selected_channel != Some(channel) {
						continue;
					}
					if let Some(index) = self.canonical_indexes.remove(&(channel, source_index)) {
						match self.pairer.register_ready(&call) {
							ToolRegistration::Registered => {
								output.events.push(ChatEvent::ToolCallReady { index, call });
							},
							ToolRegistration::Duplicate | ToolRegistration::LimitExceeded => {
								self.stopped = true;
								output.failure = Some(ProjectionFailure::ToolRegistrationRejected);
								output.evidence.push(self.evidence(
									RecoveryKind::ToolAssembly,
									"tool.registration-rejected",
									0,
								));
							},
						}
					}
				},
				ToolAssemblyEvent::Rejected { source_index, .. } => {
					self.canonical_indexes.remove(&(channel, source_index));
				},
			}
		}
	}

	fn project_text(&mut self, bytes: Bytes) -> ProjectionBatch {
		self.pending_text.extend_from_slice(&bytes);
		let mut output = ProjectionBatch::default();
		if let Some(position) = first_fabricated_opener(&self.pending_text) {
			let examined = self.pending_text.len() as u64;
			let prefix = self.pending_text.split_to(position).freeze();
			if decode_utf8(&prefix).is_none() {
				self.pending_text.clear();
				self.stopped = true;
				output.failure = Some(ProjectionFailure::InvalidUtf8);
				return output;
			}
			self.emit_text(prefix, &mut output);
			self.pending_text.clear();
			self.stopped = true;
			output.failure = Some(ProjectionFailure::FabricatedToolResult);
			output.evidence.push(self.evidence(
				RecoveryKind::FabricatedResultRejection,
				"tool.fabricated-result-marker",
				examined,
			));
			return output;
		}
		let hold = partial_opener_suffix(&self.pending_text);
		let Ok(valid) = valid_utf8_prefix(&self.pending_text) else {
			self.pending_text.clear();
			self.stopped = true;
			output.failure = Some(ProjectionFailure::InvalidUtf8);
			return output;
		};
		let emit = valid.saturating_sub(hold.min(valid));
		if emit != 0 {
			let prefix = self.pending_text.split_to(emit).freeze();
			self.emit_text(prefix, &mut output);
		}
		output
	}

	fn flush_text(&mut self, output: &mut ProjectionBatch) {
		if self.pending_text.is_empty() {
			return;
		}
		if decode_utf8(&self.pending_text).is_none() {
			self.pending_text.clear();
			self.stopped = true;
			output.failure = Some(ProjectionFailure::InvalidUtf8);
			return;
		}
		let text = self.pending_text.split().freeze();
		self.emit_text(text, output);
	}

	fn emit_text(&mut self, bytes: Bytes, output: &mut ProjectionBatch) {
		if bytes.is_empty() {
			return;
		}
		let Some(text) = decode_utf8(&bytes) else {
			return;
		};
		let index = if let Some(index) = self.text_index {
			index
		} else {
			let index = self.allocate_index();
			self.text_index = Some(index);
			output
				.events
				.push(ChatEvent::BlockStarted { index, kind: BlockKind::Text });
			index
		};
		output
			.events
			.push(ChatEvent::TextDelta { index, text: Str::new(text) });
	}

	fn allocate_tool_index(&mut self, channel: ToolChannel, source: u32) -> u32 {
		if let Some(&index) = self.canonical_indexes.get(&(channel, source)) {
			return index;
		}
		let index = self.allocate_index();
		self.canonical_indexes.insert((channel, source), index);
		index
	}

	const fn allocate_index(&mut self) -> u32 {
		let index = self.next_index;
		self.next_index = self.next_index.saturating_add(1);
		index
	}

	fn stop(&mut self, failure: ProjectionFailure) -> ProjectionBatch {
		self.stopped = true;
		ProjectionBatch { failure: Some(failure), ..ProjectionBatch::default() }
	}

	fn evidence(&self, kind: RecoveryKind, rule: &'static str, input_bytes: u64) -> RecoveryRecord {
		RecoveryRecord {
			attempt: self.attempt,
			kind,
			rule: ReasonId(Str::new(rule)),
			input_bytes,
			steps: 1,
		}
	}
}

impl Stage<ProjectionInput, ProjectionBatch> for RecoveryProjector<'_> {
	fn push(
		&mut self,
		input: ProjectionInput,
		emit: &mut dyn FnMut(ProjectionBatch),
	) -> Result<(), RecoveryError> {
		let output = RecoveryProjector::push(self, input);
		if !output.events.is_empty() || output.failure.is_some() || !output.evidence.is_empty() {
			emit(output);
		}
		Ok(())
	}

	fn finish(&mut self, emit: &mut dyn FnMut(ProjectionBatch)) -> Result<(), RecoveryError> {
		let output = RecoveryProjector::finish(self);
		if !output.events.is_empty() || output.failure.is_some() || !output.evidence.is_empty() {
			emit(output);
		}
		Ok(())
	}
}

fn append_batch(target: &mut ProjectionBatch, mut source: ProjectionBatch) {
	target.events.append(&mut source.events);
	if target.failure.is_none() {
		target.failure = source.failure;
	}
	target.evidence.append(&mut source.evidence);
}

const FABRICATED_OPENERS: &[&[u8]] = &[
	b"<tool_response",
	b"<tool_result",
	b"<function_response",
	b"<|tool_response|>",
	"<｜tool▁outputs▁begin｜>".as_bytes(),
	"<｜tool▁output▁begin｜>".as_bytes(),
];

fn valid_utf8_prefix(bytes: &[u8]) -> Result<usize, ()> {
	for held in 0..=bytes.len().min(3) {
		let end = bytes.len() - held;
		if decode_utf8(&bytes[..end]).is_some() {
			return Ok(end);
		}
	}
	Err(())
}

fn decode_utf8(bytes: &[u8]) -> Option<String> {
	let mut reader = Cursor::new(bytes);
	reader.chars().collect::<Result<String, _>>().ok()
}

fn first_fabricated_opener(bytes: &[u8]) -> Option<usize> {
	FABRICATED_OPENERS
		.iter()
		.filter_map(|token| {
			bytes
				.windows(token.len())
				.position(|window| window == *token)
		})
		.min()
}

fn partial_opener_suffix(bytes: &[u8]) -> usize {
	FABRICATED_OPENERS
		.iter()
		.map(|token| {
			let max = bytes.len().min(token.len().saturating_sub(1));
			(1..=max)
				.rev()
				.find(|&length| bytes[bytes.len() - length..] == token[..length])
				.unwrap_or(0)
		})
		.max()
		.unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use omp_core::sf;
	use serde_json::json;

	use super::*;
	use crate::call::{OpaqueJson, ToolInputConstraint};

	fn definition() -> ToolDefinition {
		ToolDefinition {
			name:        sf!("echo"),
			description: None,
			input:       ToolInputConstraint::JsonSchema {
				parameters: OpaqueJson::new(
					json!({"type":"object","required":["text"],"properties":{"text":{"type":"string"}}}),
				),
				strict:     true,
			},
		}
	}

	#[test]
	fn native_channel_wins_and_only_complete_valid_call_is_ready() {
		let definitions = [definition()];
		let mut projector = RecoveryProjector::new(&definitions, ToolAssemblyLimits::default(), 1);
		projector.push(ProjectionInput::Tool {
			channel:  ToolChannel::Native,
			fragment: ToolFragment::Start {
				input_kind:   ToolInputKind::Json,
				source_index: 5,
				id:           Some(ToolCallId::new("n1")),
				name:         Bytes::from_static(b"echo"),
			},
		});
		let ignored = projector.push(ProjectionInput::Tool {
			channel:  ToolChannel::Text,
			fragment: ToolFragment::Start {
				input_kind:   ToolInputKind::Json,
				source_index: 1,
				id:           None,
				name:         Bytes::from_static(b"echo"),
			},
		});
		assert!(ignored.events.is_empty());
		let partial = projector.push(ProjectionInput::Tool {
			channel:  ToolChannel::Native,
			fragment: ToolFragment::ArgumentsDelta {
				source_index: 5,
				bytes:        Bytes::from_static(b"{\"text\":"),
			},
		});
		assert!(
			!partial
				.events
				.iter()
				.any(|event| event.authorized_tool_call().is_some())
		);
		projector.push(ProjectionInput::Tool {
			channel:  ToolChannel::Native,
			fragment: ToolFragment::ArgumentsDelta {
				source_index: 5,
				bytes:        Bytes::from_static(b"\"ok\"}"),
			},
		});
		let ready = projector.push(ProjectionInput::Tool {
			channel:  ToolChannel::Native,
			fragment: ToolFragment::End { source_index: 5 },
		});
		assert_eq!(
			ready
				.events
				.iter()
				.filter(|event| event.authorized_tool_call().is_some())
				.count(),
			1
		);
	}

	#[test]
	fn replay_projection_is_invariant_to_utf8_chunk_boundaries() {
		fn project(chunks: &[&[u8]]) -> String {
			let definitions = [definition()];
			let mut projector = RecoveryProjector::new(&definitions, ToolAssemblyLimits::default(), 1);
			let mut output = String::new();
			for chunk in chunks {
				for event in projector
					.push(ProjectionInput::Text(Bytes::copy_from_slice(chunk)))
					.events
				{
					if let ChatEvent::TextDelta { text, .. } = event {
						output.push_str(text.as_str());
					}
				}
			}
			for event in projector.finish().events {
				if let ChatEvent::TextDelta { text, .. } = event {
					output.push_str(text.as_str());
				}
			}
			output
		}
		let text = "α-beta".as_bytes();
		assert_eq!(project(&[text]), project(&[&text[..1], &text[1..3], &text[3..]]));
	}

	#[test]
	fn fabricated_result_split_across_wire_chunks_aborts_once() {
		let definitions = [definition()];
		let mut projector = RecoveryProjector::new(&definitions, ToolAssemblyLimits::default(), 1);
		let first = projector.push(ProjectionInput::Text(Bytes::from_static(b"visible<tool_res")));
		assert!(first.failure.is_none());
		let second = projector.push(ProjectionInput::Text(Bytes::from_static(b"ponse>fake")));
		assert_eq!(second.failure, Some(ProjectionFailure::FabricatedToolResult));
		assert!(
			projector
				.push(ProjectionInput::Text(Bytes::from_static(b"tail")))
				.events
				.is_empty()
		);
	}
}
