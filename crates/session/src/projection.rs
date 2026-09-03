//! Pure inference-thread projection from the authoritative DOM.

use std::{collections::BTreeMap, str, str::FromStr};

use bytes::Bytes;
use omp_core::{Str, encoding::hex};
use omp_dom::{Dom, Handle, KnownTag, Node, PropId, PropKey, Tag, Value};
use omp_journal::{EntryId, blob::BlobRef};
use omp_proto::{
	inference::v1 as inference,
	thread::v1::{self as thread, Item, item, part},
};
use omp_tool::{
	CapsBase, Part as ToolPart, ProjectedCall, PromptCaps, RecordedCallOwned,
	Registry as ToolRegistry, Rev, TOOL_REV_PROP, ToolIdentity,
};
use thiserror::Error;

/// Durable property carrying an explicit provider-session reset request.
pub const PROVIDER_RESET_PROP: &str = "omp/session-provider-reset";

/// Stable diagnostics for provider turns that settle without usable output.
pub mod empty_stop {
	/// Provider returned no final output.
	pub const NO_FINAL_OUTPUT: &str = "empty_stop.no_final_output";
	/// Provider returned no content.
	pub const EMPTY: &str = "empty_stop.empty";
	/// Provider billed output tokens but returned no usable block.
	pub const BILLED_OUTPUT: &str = "empty_stop.billed_output";
}

/// Historical protobuf projection failure.
#[derive(Debug, Error)]
pub enum ProjectionError {
	/// A committed tool revision property had the wrong shape.
	#[error("omp/tool-rev must be a string")]
	RevisionType,
	/// A committed tool revision string was malformed.
	#[error("omp/tool-rev contains an invalid revision")]
	InvalidRevision,
	/// Structured tool call-outcome JSON was invalid.
	#[error("invalid tool call-outcome JSON")]
	OutcomeJson(#[from] serde_json::Error),
	/// A model-facing JSON part was not UTF-8.
	#[error("tool JSON part is not UTF-8")]
	PartUtf8(#[from] str::Utf8Error),
	/// A model-facing blob hash was not hexadecimal.
	#[error("tool blob hash is not valid hexadecimal")]
	BlobHash,
	/// The live tool could not deterministically render a lifted verdict.
	#[error("tool projection failed")]
	Tool(#[from] omp_tool::RegistryError),
}

/// Re-expresses historical tool calls through complete live revision lifts.
///
/// Calls without a complete lift path are retained exactly. Calls already at
/// the live revision preserve their original bytes and field presence.
pub fn project_thread_history(
	source: &thread::Thread,
	tool_registry: &ToolRegistry,
	caps: &CapsBase,
) -> Result<thread::Thread, ProjectionError> {
	let mut projected = source.clone();
	for call_index in 0..projected.items.len() {
		let Some(item::Kind::ToolCall(call)) = projected.items[call_index].kind.as_ref() else {
			continue;
		};
		let Some(rev) = tool_revision(&projected.items[call_index])? else {
			continue;
		};
		let call_id = call.id.clone();
		let name = call.name.clone();
		let Some(live_identity) = tool_registry.resolved_identity(&name) else {
			continue;
		};
		if live_identity.rev == rev {
			continue;
		}
		let Some(result_index) = projected
			.items
			.iter()
			.enumerate()
			.skip(call_index + 1)
			.find_map(|(index, item)| {
				matches!(
					item.kind.as_ref(),
					Some(item::Kind::ToolResult(result))
						if result.call_id == call_id && result.details.is_some()
				)
				.then_some(index)
			})
		else {
			continue;
		};
		let Some(item::Kind::ToolResult(result)) = projected.items[result_index].kind.as_ref() else {
			unreachable!("result index came from tool-result items")
		};
		let Some(verdict) = proto_json_bytes(
			result
				.details
				.as_ref()
				.expect("selected result has structured details"),
		) else {
			continue;
		};
		let original = RecordedCallOwned {
			identity: ToolIdentity { name: Str::new(&name), rev: rev.clone() },
			raw_args: Bytes::copy_from_slice(&call.args_json),
			verdict,
		};
		let ProjectedCall::Live(live) = tool_registry.project(original) else {
			continue;
		};
		let prompt_caps = PromptCaps::for_tool(*caps, &live.identity.rev);
		let rendered = tool_registry.project_verdict(
			&live.identity,
			&live.verdict,
			result.useless.unwrap_or(false),
			&prompt_caps,
		)?;
		let lifted_details =
			json_proto_value(serde_json::from_slice::<serde_json::Value>(&live.verdict)?);
		let lifted_parts = history_tool_parts(&rendered.parts)?;

		let Some(item::Kind::ToolCall(call)) = projected.items[call_index].kind.as_mut() else {
			unreachable!("call index came from tool-call items")
		};
		call.args_json = live.raw_args.clone();
		projected.items[call_index]
			.props
			.get_or_insert_default()
			.fields
			.insert(TOOL_REV_PROP.to_owned(), inference::Value {
				kind: Some(inference::value::Kind::String(live.identity.rev.to_string())),
			});
		projected.items[result_index]
			.props
			.get_or_insert_default()
			.fields
			.insert(TOOL_REV_PROP.to_owned(), inference::Value {
				kind: Some(inference::value::Kind::String(live.identity.rev.to_string())),
			});
		let Some(item::Kind::ToolResult(result)) = projected.items[result_index].kind.as_mut() else {
			unreachable!("result index came from tool-result items")
		};
		result.details = Some(lifted_details);
		result.parts = lifted_parts;
		result.is_error = rendered.is_error;
		result.useless = Some(rendered.useless);
	}
	Ok(projected)
}

fn tool_revision(value: &Item) -> Result<Option<Rev>, ProjectionError> {
	let Some(value) = value
		.props
		.as_ref()
		.and_then(|props| props.fields.get(TOOL_REV_PROP))
	else {
		return Ok(None);
	};
	let Some(inference::value::Kind::String(value)) = value.kind.as_ref() else {
		return Err(ProjectionError::RevisionType);
	};
	value
		.parse::<Rev>()
		.map(Some)
		.map_err(|_| ProjectionError::InvalidRevision)
}

fn proto_json_bytes(value: &inference::Value) -> Option<Bytes> {
	serde_json::to_vec(&proto_json_value(value)?)
		.ok()
		.map(Bytes::from)
}

fn proto_json_value(value: &inference::Value) -> Option<serde_json::Value> {
	match value.kind.as_ref()? {
		inference::value::Kind::Null(_) => Some(serde_json::Value::Null),
		inference::value::Kind::Int(value) => Some((*value).into()),
		inference::value::Kind::Uint(value) => Some((*value).into()),
		inference::value::Kind::Double(value) => {
			serde_json::Number::from_f64(*value).map(serde_json::Value::Number)
		},
		inference::value::Kind::Bool(value) => Some((*value).into()),
		inference::value::Kind::String(value) => Some(value.clone().into()),
		inference::value::Kind::List(values) => values
			.values
			.iter()
			.map(proto_json_value)
			.collect::<Option<Vec<_>>>()
			.map(serde_json::Value::Array),
		inference::value::Kind::Map(fields) => fields
			.fields
			.iter()
			.map(|(key, value)| Some((key.clone(), proto_json_value(value)?)))
			.collect::<Option<serde_json::Map<_, _>>>()
			.map(serde_json::Value::Object),
	}
}

fn json_proto_value(value: serde_json::Value) -> inference::Value {
	let kind = match value {
		serde_json::Value::Null => inference::value::Kind::Null(true),
		serde_json::Value::Bool(value) => inference::value::Kind::Bool(value),
		serde_json::Value::Number(value) => {
			if let Some(value) = value.as_i64() {
				inference::value::Kind::Int(value)
			} else if let Some(value) = value.as_u64() {
				inference::value::Kind::Uint(value)
			} else {
				inference::value::Kind::Double(value.as_f64().expect("JSON numbers are finite"))
			}
		},
		serde_json::Value::String(value) => inference::value::Kind::String(value),
		serde_json::Value::Array(values) => inference::value::Kind::List(inference::ValueList {
			values: values.into_iter().map(json_proto_value).collect(),
		}),
		serde_json::Value::Object(fields) => inference::value::Kind::Map(inference::ValueMap {
			fields: fields
				.into_iter()
				.map(|(key, value)| (key, json_proto_value(value)))
				.collect::<BTreeMap<_, _>>(),
		}),
	};
	inference::Value { kind: Some(kind) }
}

fn history_tool_parts(parts: &[ToolPart]) -> Result<Vec<thread::Part>, ProjectionError> {
	let mut projected = Vec::with_capacity(parts.len());
	for value in parts {
		match value {
			ToolPart::Text { text } => {
				projected.push(thread::Part { kind: Some(part::Kind::Text(text.as_str().to_owned())) })
			},
			ToolPart::Json { json } => projected
				.push(thread::Part { kind: Some(part::Kind::Text(str::from_utf8(json)?.to_owned())) }),
			ToolPart::Blob { blob, alt } => {
				if let Some(alt) = alt {
					projected
						.push(thread::Part { kind: Some(part::Kind::Text(alt.as_str().to_owned())) });
				}
				let hash = hex::decode(blob.hash.as_str())
					.into_vec()
					.map_err(|_| ProjectionError::BlobHash)?;
				if hash.len() != 32 {
					return Err(ProjectionError::BlobHash);
				}
				projected.push(thread::Part {
					kind: Some(part::Kind::Blob(thread::Blob {
						hash: hash.into(),
						mime: blob.media_type.as_str().to_owned(),
						size: blob.byte_len,
						..Default::default()
					})),
				});
			},
		}
	}
	Ok(projected)
}

/// Projects the selected session body into canonical inference thread items.
///
/// The function reads only the DOM. If a compaction marker exists, older
/// turns are omitted and its content-addressed summary is prepended as a
/// synthetic user message.
#[must_use]
pub fn project_thread(dom: &Dom) -> Vec<Item> {
	let (boundary, summary) = newest_compaction(dom);
	let mut items = Vec::new();
	if let Some(summary) = summary {
		items.push(message_item(thread::Role::User, summary, None, true));
	}
	for turn in dom.children(dom.body()) {
		if !is_tag(dom, *turn, KnownTag::Turn) {
			continue;
		}
		let usage = turn_usage(dom, *turn, boundary);
		for child in dom.children(*turn) {
			let Some(node) = dom.get(*child) else {
				continue;
			};
			if !element_after_boundary(node, boundary) {
				continue;
			}
			match &node.tag {
				Tag::Known(KnownTag::User) => project_message(node, thread::Role::User, &mut items),
				Tag::Known(KnownTag::Developer) => {
					project_message(node, thread::Role::System, &mut items);
				},
				Tag::Known(KnownTag::Assistant) => {
					project_assistant(node, usage.clone(), &mut items);
				},
				Tag::Custom(name) => project_tool(dom, *child, name.as_str(), node, &mut items),
				_ => {},
			}
		}
	}
	items
}

fn project_message(node: &Node, role: thread::Role, items: &mut Vec<Item>) {
	let mut parts = Vec::new();
	if let Some(text) = node
		.content
		.as_deref()
		.or_else(|| prop_text(node, PropId::Text))
	{
		parts.push(thread::Part { kind: Some(part::Kind::Text(text.to_owned())) });
	}
	if let Some(Value::Json(raw)) = node.prop(&PropKey::from(PropId::Data)) {
		if let Ok(blobs) = serde_json::from_str::<Vec<BlobRef>>(raw.get()) {
			parts.extend(blobs.into_iter().map(|blob| thread::Part {
				kind: Some(part::Kind::Blob(thread::Blob {
					hash: blob.hash.as_bytes().to_vec().into(),
					size: blob.size,
					..Default::default()
				})),
			}));
		}
	}
	items.push(Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(item::Kind::Message(thread::Message {
			role: role as i32,
			parts,
			..Default::default()
		})),
		props:         None,
	});
}

fn project_assistant(node: &Node, usage: Option<inference::Usage>, items: &mut Vec<Item>) {
	let mut parts = Vec::new();
	if let Some(thinking) = prop_text(node, PropId::Thinking).filter(|text| !text.is_empty()) {
		parts.push(thread::Part {
			kind: Some(part::Kind::Thinking(thread::Thinking {
				text: thinking.to_owned(),
				..Default::default()
			})),
		});
	}
	if let Some(text) = node
		.content
		.as_deref()
		.or_else(|| prop_text(node, PropId::Text))
		.filter(|text| !text.is_empty())
	{
		parts.push(thread::Part { kind: Some(part::Kind::Text(text.to_owned())) });
	}
	items.push(Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(item::Kind::Message(thread::Message {
			role: thread::Role::Assistant as i32,
			parts,
			usage,
			..Default::default()
		})),
		props:         None,
	});
}

fn project_tool(dom: &Dom, handle: Handle, name: &str, node: &Node, items: &mut Vec<Item>) {
	let id = prop_text(node, PropId::Id).unwrap_or_default().to_owned();
	let input = child(dom, handle, KnownTag::Input)
		.and_then(|handle| dom.get(handle))
		.and_then(node_text)
		.unwrap_or_default();
	items.push(Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(item::Kind::ToolCall(thread::ToolCall {
			id: id.clone(),
			name: name.to_owned(),
			args_json: input.as_bytes().to_vec().into(),
			intent: prop_text(node, PropId::I).map(str::to_owned),
			..Default::default()
		})),
		props:         None,
	});
	let status = prop_text(node, PropId::Status).unwrap_or("running");
	if matches!(status, "arguments" | "running") {
		return;
	}
	let result_tag = if status == "error" {
		KnownTag::Diag
	} else {
		KnownTag::Result
	};
	let result_node = child(dom, handle, result_tag).and_then(|handle| dom.get(handle));
	let parts = result_node
		.and_then(projected_tool_parts)
		.unwrap_or_else(|| {
			let result = result_node.and_then(node_text).unwrap_or_default();
			vec![thread::Part { kind: Some(part::Kind::Text(result.to_owned())) }]
		});
	items.push(Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(item::Kind::ToolResult(thread::ToolResult {
			call_id: id,
			name: name.to_owned(),
			is_error: status == "error",
			parts,
			attribution: thread::tool_result::Attribution::Agent as i32,
			..Default::default()
		})),
		props:         None,
	});
}

fn projected_tool_parts(node: &Node) -> Option<Vec<thread::Part>> {
	let Value::Json(raw) = node.prop(&PropKey::from(PropId::Data))? else {
		return None;
	};
	let parts: Vec<ToolPart> = serde_json::from_str(raw.get()).ok()?;
	let mut projected = Vec::with_capacity(parts.len());
	for part in parts {
		match part {
			ToolPart::Text { text } => {
				projected.push(thread::Part { kind: Some(part::Kind::Text(text.as_str().to_owned())) })
			},
			ToolPart::Json { json } => projected.push(thread::Part {
				kind: Some(part::Kind::Text(std::str::from_utf8(&json).ok()?.to_owned())),
			}),
			ToolPart::Blob { blob, alt } => {
				if let Some(alt) = alt {
					projected
						.push(thread::Part { kind: Some(part::Kind::Text(alt.as_str().to_owned())) });
				}
				let hash = hex::decode(blob.hash.as_str()).into_vec().ok()?;
				if hash.len() != 32 {
					return None;
				}
				projected.push(thread::Part {
					kind: Some(part::Kind::Blob(thread::Blob {
						hash: hash.into(),
						mime: blob.media_type.as_str().to_owned(),
						size: blob.byte_len,
						..Default::default()
					})),
				});
			},
		}
	}
	Some(projected)
}

fn message_item(
	role: thread::Role,
	text: &str,
	usage: Option<inference::Usage>,
	synthetic: bool,
) -> Item {
	Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(item::Kind::Message(thread::Message {
			role: role as i32,
			parts: vec![thread::Part { kind: Some(part::Kind::Text(text.to_owned())) }],
			synthetic: Some(synthetic),
			usage,
			..Default::default()
		})),
		props:         None,
	}
}

fn newest_compaction(dom: &Dom) -> (Option<EntryId>, Option<&str>) {
	let mut result = (None, None);
	for handle in dom.children(dom.meta()) {
		let Some(node) = dom.get(*handle) else {
			continue;
		};
		if node.tag.as_str() != "compaction" {
			continue;
		}
		let boundary =
			prop_text(node, PropId::Boundary).and_then(|value| EntryId::from_str(value).ok());
		let summary = prop_text(node, PropId::Summary);
		result = (boundary, summary);
	}
	result
}

fn element_after_boundary(node: &Node, boundary: Option<EntryId>) -> bool {
	let Some(boundary) = boundary else {
		return true;
	};
	prop_text(node, PropId::Order)
		.or_else(|| prop_text(node, PropId::Id))
		.or_else(|| prop_text(node, PropId::Cause))
		.and_then(|id| EntryId::from_str(id).ok())
		.is_some_and(|id| id > boundary)
}

fn turn_usage(dom: &Dom, turn: Handle, boundary: Option<EntryId>) -> Option<inference::Usage> {
	let usage = child(dom, turn, KnownTag::Usage).and_then(|handle| dom.get(handle))?;
	if !element_after_boundary(usage, boundary) {
		return None;
	}
	let input_tokens = prop_u64(usage, PropId::TokensIn).unwrap_or_default();
	let output_tokens = prop_u64(usage, PropId::TokensOut).unwrap_or_default();
	Some(inference::Usage {
		input_tokens,
		output_tokens,
		total_tokens: Some(input_tokens.saturating_add(output_tokens)),
		..Default::default()
	})
}

fn child(dom: &Dom, parent: Handle, tag: KnownTag) -> Option<Handle> {
	dom.children(parent)
		.iter()
		.copied()
		.find(|handle| is_tag(dom, *handle, tag))
}

fn is_tag(dom: &Dom, handle: Handle, tag: KnownTag) -> bool {
	dom.get(handle)
		.is_some_and(|node| node.tag == Tag::Known(tag))
}

fn prop_text(node: &Node, prop: PropId) -> Option<&str> {
	node.prop(&PropKey::from(prop)).and_then(Value::as_str)
}

fn prop_u64(node: &Node, prop: PropId) -> Option<u64> {
	match node.prop(&PropKey::from(prop))? {
		Value::Int(value) => u64::try_from(*value).ok(),
		_ => None,
	}
}

fn node_text(node: &Node) -> Option<&str> {
	node
		.content
		.as_deref()
		.or_else(|| prop_text(node, PropId::Text))
}
