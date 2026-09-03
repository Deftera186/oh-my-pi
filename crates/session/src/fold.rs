use omp_core::Str;
use omp_dom::{
	Applied, Dom, Handle, KnownTag, NodeSpec, Op, PropId, PropKey, StreamOp, Tag, Txn, Value,
};
use omp_journal::{
	Entry, EntryId,
	blob::BlobRef,
	data::{
		Compaction, MsgAssistantEnd, MsgAssistantStart, MsgUser, Patch, Stream, ToolCall, ToolResult,
		ToolUpdate, TurnReceipt,
	},
	kind,
};
use omp_tool::Part as ToolPart;
use serde_json::value::RawValue;

use crate::{Draft, Session, SessionError};

impl Session {
	pub(crate) fn apply(&mut self, entry: &Entry) -> Result<(), SessionError> {
		self.entry_patch_published = false;
		match (entry.kind.name.as_str(), entry.kind.rev) {
			(kind::JOURNAL, 1) => self.fold_genesis(entry)?,
			(kind::TURN_START, 1) => self.fold_turn_start(entry)?,
			(kind::MSG_USER, 1) => self.fold_user(entry)?,
			(kind::MSG_ASSISTANT_START, 1) => self.fold_assistant_start(entry)?,
			(kind::STREAM, 1) => self.fold_stream(entry)?,
			(kind::MSG_ASSISTANT_END, 1) => self.fold_assistant_end(entry)?,
			(kind::TOOL_CALL, 1) => self.fold_tool_call(entry)?,
			(kind::TOOL_UPDATE, 1) => self.fold_tool_update(entry)?,
			(kind::TOOL_RESULT, 1) => self.fold_tool_result(entry)?,
			(kind::TURN_RECEIPT, 1) => self.fold_receipt(entry)?,
			(kind::PATCH, 1) => self.fold_patch(entry)?,
			(kind::COMPACTION, 1) => self.fold_compaction(entry)?,
			_ => {},
		}

		let mut draft = Draft::new();
		for component in self.components.iter_mut() {
			if component.interested(&entry.kind) {
				component.apply(entry, &self.dom, &mut draft);
			}
		}
		if !draft.is_empty() {
			self.apply_entry_ops(entry, draft.into_ops())?;
		}
		self.head = Some(entry.id);
		Ok(())
	}

	fn fold_genesis(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let meta = self.dom.meta();
		let queues = self.dom.queues();
		let mut ops = Vec::with_capacity(6);
		for tag in [KnownTag::Todo, KnownTag::Jobs, KnownTag::Directors, KnownTag::Con] {
			ops.push(Op::Ins { parent: meta, after: None, node: NodeSpec::new(tag) });
		}
		for tag in [KnownTag::Steering, KnownTag::Prompts] {
			ops.push(Op::Ins { parent: queues, after: None, node: NodeSpec::new(tag) });
		}
		self.apply_ops(entry, ops)
	}

	fn fold_turn_start(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let body = self.dom.body();
		let after = self.dom.children(body).last().copied();
		let ordinal = i64::try_from(self.dom.children(body).len() + 1).unwrap_or(i64::MAX);
		let node = entry_node(KnownTag::Turn, entry).with_prop(PropId::Turn, Value::Int(ordinal));
		let applied = self.apply_entry_ops(entry, vec![Op::Ins { parent: body, after, node }])?;
		self.current_turn = Some(entry.id);
		self.current_assistant = None;
		if applied.minted.is_empty() {
			return Err(SessionError::NoActiveTurn);
		}
		Ok(())
	}

	fn fold_user(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let payload: MsgUser = serde_json::from_str(entry.data.as_str())?;
		let turn = self.current_turn_handle()?;
		let mut node = entry_node(KnownTag::User, entry).with_content(payload.text);
		if !payload.attachments.is_empty() {
			let raw = serde_json::value::to_raw_value(&payload.attachments)?;
			node = node.with_prop(PropId::Data, Value::Json(raw));
		}
		self.insert_last(entry, turn, node)?;
		Ok(())
	}

	fn fold_assistant_start(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let payload: MsgAssistantStart = serde_json::from_str(entry.data.as_str())?;
		let turn = self.current_turn_handle()?;
		let node = entry_node(KnownTag::Assistant, entry)
			.with_prop(PropId::Model, Value::Str(payload.model))
			.with_prop(PropId::Provider, Value::Str(payload.provider))
			.with_prop(PropId::Route, Value::Str(payload.route))
			.with_prop(PropId::Text, Value::Str(Str::new_static("")))
			.with_prop(PropId::Thinking, Value::Str(Str::new_static("")));
		self.insert_last(entry, turn, node)?;
		self.current_assistant = Some(entry.id);
		Ok(())
	}

	fn fold_stream(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let payload: Stream = serde_json::from_str(entry.data.as_str())?;
		self.next_sid = self.next_sid.max(payload.sid);
		match payload.op {
			omp_journal::data::StreamOp::Open => {
				let value = payload.node.ok_or(SessionError::InvalidStreamFrame)?;
				let node = Handle::new(value).ok_or(SessionError::InvalidHandle { value })?;
				let prop = payload
					.prop
					.map(|value| Self::decode_stream_prop(value.as_str()))
					.ok_or(SessionError::InvalidStreamFrame)?;
				if payload.text.is_some() {
					return Err(SessionError::InvalidStreamFrame);
				}
				self.apply_stream(entry, payload.sid, StreamOp::Open, Some(node), Some(prop), None)?;
				self.stream_targets.insert(payload.sid, node);
				self.set_stream_order(entry, node)
			},
			omp_journal::data::StreamOp::Append => {
				if payload.node.is_some() || payload.prop.is_some() {
					return Err(SessionError::InvalidStreamFrame);
				}
				let text = payload.text.ok_or(SessionError::InvalidStreamFrame)?;
				let node = self.stream_target(payload.sid)?;
				self.apply_stream(entry, payload.sid, StreamOp::Append, None, None, Some(text))?;
				self.set_stream_order(entry, node)
			},
			omp_journal::data::StreamOp::Close => {
				if payload.node.is_some() || payload.prop.is_some() || payload.text.is_some() {
					return Err(SessionError::InvalidStreamFrame);
				}
				let node = self.stream_target(payload.sid)?;
				self.apply_stream(entry, payload.sid, StreamOp::Close, None, None, None)?;
				self.stream_targets.remove(&payload.sid);
				self.set_stream_order(entry, node)
			},
		}
	}

	fn fold_assistant_end(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let payload: MsgAssistantEnd = serde_json::from_str(entry.data.as_str())?;
		let assistant = self.current_assistant_handle()?;
		self.apply_ops(entry, vec![
			Op::Set {
				h:     assistant,
				prop:  PropId::StopReason.into(),
				value: Value::Str(payload.stop_reason),
			},
			Op::Set {
				h:     assistant,
				prop:  PropId::Order.into(),
				value: Value::Str(Str::new(entry.id.to_string())),
			},
		])?;
		self.current_assistant = None;
		Ok(())
	}

	fn fold_tool_call(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let payload: ToolCall = serde_json::from_str(entry.data.as_str())?;
		let turn = self.current_turn_handle()?;
		let start = self.dom.high_water() + 1;
		let tool = Handle::new(start).ok_or(SessionError::InvalidHandle { value: start })?;
		let input = Handle::new(start + 1).ok_or(SessionError::InvalidHandle { value: start + 1 })?;
		let status = if payload.sid.is_some() {
			"arguments"
		} else {
			"running"
		};
		let mut tool_node = NodeSpec::new(Tag::Custom(payload.name.clone()))
			.with_prop(PropId::Id, Value::Str(payload.call_id.clone()))
			.with_prop(PropId::Cause, Value::Str(Str::new(entry.id.to_string())))
			.with_prop(PropId::Order, Value::Str(Str::new(entry.id.to_string())))
			.with_prop(PropId::Status, Value::Str(Str::new(status)))
			.with_prop(PropId::Rev, Value::Int(i64::from(payload.rev)));
		if let Some(intent) = payload.i {
			tool_node = tool_node.with_prop(PropId::I, Value::Str(intent));
		}
		let stream_sid = payload.sid;
		let input_node = match payload.args {
			Some(args) => NodeSpec::new(KnownTag::Input).with_content(args.get()),
			None => {
				NodeSpec::new(KnownTag::Input).with_prop(PropId::Text, Value::Str(Str::new_static("")))
			},
		};
		let after = self.dom.children(turn).last().copied();
		let ops = vec![
			Op::Ins { parent: turn, after, node: tool_node },
			Op::Ins { parent: tool, after: None, node: input_node },
			Op::Ins { parent: tool, after: Some(input), node: NodeSpec::new(KnownTag::Result) },
			Op::Ins {
				parent: tool,
				after:  Handle::new(start + 2),
				node:   NodeSpec::new(KnownTag::Diag),
			},
			Op::Ins {
				parent: tool,
				after:  Handle::new(start + 3),
				node:   NodeSpec::new(KnownTag::Usage),
			},
		];
		self.apply_ops(entry, ops)?;
		self.call_handles.insert(entry.id, tool);
		if let Some(sid) = stream_sid {
			self.next_sid = self.next_sid.max(sid);
			self.apply_stream(
				entry,
				sid,
				StreamOp::Open,
				Some(input),
				Some(PropId::Text.into()),
				None,
			)?;
			self.stream_targets.insert(sid, input);
		}
		Ok(())
	}

	fn fold_tool_update(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let ToolUpdate(update): ToolUpdate = serde_json::from_str(entry.data.as_str())?;
		let call = self.entry_call_handle(entry)?;
		let value: serde_json::Value = serde_json::from_str(update.get())?;
		if value.get("kernel").and_then(serde_json::Value::as_str) == Some("ready") {
			return self.fold_tool_ready(entry, call, &value);
		}
		let mut ops = vec![Op::Set {
			h:     call,
			prop:  PropId::Order.into(),
			value: Value::Str(Str::new(entry.id.to_string())),
		}];
		project_update(&self.dom, call, &value, &mut ops)?;
		self.apply_ops(entry, ops)
	}

	fn fold_tool_ready(
		&mut self,
		entry: &Entry,
		call: Handle,
		value: &serde_json::Value,
	) -> Result<(), SessionError> {
		let input =
			child_with_tag(&self.dom, call, KnownTag::Input).ok_or(SessionError::NoActiveTurn)?;
		let args = value.get("args").ok_or(SessionError::InvalidStreamFrame)?;
		let raw = serde_json::value::to_raw_value(args)?;
		if let Some(sid) = self
			.stream_targets
			.iter()
			.find_map(|(sid, target)| (*target == input).then_some(*sid))
		{
			self.apply_stream(entry, sid, StreamOp::Close, None, None, None)?;
			self.stream_targets.remove(&sid);
		}
		let mut ops = vec![
			Op::Set {
				h:     call,
				prop:  PropId::Order.into(),
				value: Value::Str(Str::new(entry.id.to_string())),
			},
			Op::Set {
				h:     call,
				prop:  PropId::Status.into(),
				value: Value::Str(Str::new_static("running")),
			},
			Op::Set { h: input, prop: PropId::Text.into(), value: Value::Str(json_text(&raw)) },
			Op::Set { h: input, prop: PropId::Data.into(), value: Value::Json(raw) },
		];
		if let Some(i) = value.get("i").and_then(serde_json::Value::as_str) {
			ops.push(Op::Set { h: call, prop: PropId::I.into(), value: Value::Str(Str::new(i)) });
		}
		self.apply_ops(entry, ops)
	}

	fn fold_tool_result(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let payload: ToolResult = serde_json::from_str(entry.data.as_str())?;
		let call = self.entry_call_handle(entry)?;
		let (status, target, raw, prompt_parts) = match payload {
			ToolResult::Outcome { outcome, prompt_parts } => {
				("ok", KnownTag::Result, outcome, prompt_parts)
			},
			ToolResult::Fault { fault, prompt_parts } => {
				("error", KnownTag::Diag, fault, prompt_parts)
			},
		};
		let child = child_with_tag(&self.dom, call, target)
			.ok_or(SessionError::UnknownCall { id: entry.by.expect("journal enforces causes") })?;
		let text = prompt_parts
			.as_deref()
			.map(prompt_parts_text)
			.transpose()?
			.unwrap_or_else(|| json_text(&raw));
		let mut ops = vec![
			Op::Set {
				h:     call,
				prop:  PropId::Order.into(),
				value: Value::Str(Str::new(entry.id.to_string())),
			},
			Op::Set {
				h:     call,
				prop:  PropId::Status.into(),
				value: Value::Str(Str::new_static(status)),
			},
			Op::Set { h: child, prop: PropId::Text.into(), value: Value::Str(text) },
		];
		if let Some(prompt_parts) = prompt_parts {
			ops.push(Op::Set {
				h:     child,
				prop:  PropId::Data.into(),
				value: Value::Json(prompt_parts),
			});
		}
		if status == "error" {
			ops.push(Op::Set {
				h:     child,
				prop:  PropId::Severity.into(),
				value: Value::Str(Str::new_static("error")),
			});
		}
		self.apply_ops(entry, ops)
	}

	fn fold_receipt(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let payload: TurnReceipt = serde_json::from_str(entry.data.as_str())?;
		let turn = self.current_turn_handle()?;
		let node = entry_node(KnownTag::Usage, entry)
			.with_prop(PropId::TokensIn, unsigned(payload.tokens_in))
			.with_prop(PropId::TokensOut, unsigned(payload.tokens_out))
			.with_prop(PropId::CostNanoUsd, unsigned(payload.cost_nano_usd));
		self.insert_last(entry, turn, node)?;
		Ok(())
	}

	fn fold_patch(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let payload: Patch = serde_json::from_str(entry.data.as_str())?;
		let ops: Vec<Op> = serde_json::from_str(payload.ops.get())?;
		self.apply_ops(entry, ops)
	}

	fn fold_compaction(&mut self, entry: &Entry) -> Result<(), SessionError> {
		let payload: Compaction = serde_json::from_str(entry.data.as_str())?;
		let summary = self.compaction_summary(&payload.summary)?;
		let meta = self.dom.meta();
		let node = NodeSpec::new(KnownTag::Compaction)
			.with_prop(PropId::Cause, Value::Str(Str::new(entry.id.to_string())))
			.with_prop(PropId::Boundary, Value::Str(Str::new(payload.boundary.to_string())))
			.with_prop(PropId::Summary, Value::Str(summary))
			.with_prop(PropId::Blob, Value::Str(blob_address(&payload.summary)));
		self.insert_last(entry, meta, node)?;
		Ok(())
	}

	fn insert_last(
		&mut self,
		entry: &Entry,
		parent: Handle,
		node: NodeSpec,
	) -> Result<Handle, SessionError> {
		let after = self.dom.children(parent).last().copied();
		let applied = self.apply_entry_ops(entry, vec![Op::Ins { parent, after, node }])?;
		Ok(applied.minted[0])
	}

	fn apply_ops(&mut self, entry: &Entry, ops: Vec<Op>) -> Result<(), SessionError> {
		self.apply_entry_ops(entry, ops)?;
		Ok(())
	}

	fn apply_entry_ops(&mut self, entry: &Entry, ops: Vec<Op>) -> Result<Applied, SessionError> {
		let prior = (!self.entry_patch_published)
			.then_some(entry.prior)
			.flatten();
		let txn = Txn { cause: entry.id, label: entry.label.clone(), ops };
		let applied = self.dom.apply_with_prior(&txn, prior)?;
		self.entry_patch_published = true;
		Ok(applied)
	}

	fn apply_stream(
		&mut self,
		entry: &Entry,
		sid: u32,
		op: StreamOp,
		node: Option<Handle>,
		prop: Option<PropKey>,
		text: Option<Str>,
	) -> Result<(), SessionError> {
		match op {
			StreamOp::Open => self.dom.stream_open_with_id(
				entry.id,
				sid,
				node.ok_or(SessionError::InvalidStreamFrame)?,
				prop.ok_or(SessionError::InvalidStreamFrame)?,
			)?,
			StreamOp::Append => self.dom.stream_append(
				entry.id,
				sid,
				text.as_deref().ok_or(SessionError::InvalidStreamFrame)?,
			)?,
			StreamOp::Close => self.dom.stream_close(entry.id, sid)?,
		}
		self.entry_patch_published = true;
		Ok(())
	}

	fn stream_target(&self, sid: u32) -> Result<Handle, SessionError> {
		self
			.stream_targets
			.get(&sid)
			.copied()
			.ok_or_else(|| omp_dom::DomError::MissingStream { sid }.into())
	}

	fn set_stream_order(&mut self, entry: &Entry, node: Handle) -> Result<(), SessionError> {
		self.apply_ops(entry, vec![Op::Set {
			h:     node,
			prop:  PropId::Order.into(),
			value: Value::Str(Str::new(entry.id.to_string())),
		}])
	}

	pub(crate) fn current_turn_handle(&self) -> Result<Handle, SessionError> {
		let id = self.current_turn.ok_or(SessionError::NoActiveTurn)?;
		find_entry_node(&self.dom, self.dom.body(), id).ok_or(SessionError::NoActiveTurn)
	}

	pub(crate) fn current_assistant_handle(&self) -> Result<Handle, SessionError> {
		let id = self
			.current_assistant
			.ok_or(SessionError::NoActiveAssistant)?;
		find_entry_node(&self.dom, self.dom.body(), id).ok_or(SessionError::NoActiveAssistant)
	}

	fn entry_call_handle(&self, entry: &Entry) -> Result<Handle, SessionError> {
		self.call_handle(entry.by.expect("journal enforces causes"))
	}

	/// Returns the DOM element materialized for a live tool-call entry.
	pub fn call_handle(&self, id: EntryId) -> Result<Handle, SessionError> {
		self
			.call_handles
			.get(&id)
			.copied()
			.ok_or(SessionError::UnknownCall { id })
	}
}

fn entry_node(tag: KnownTag, entry: &Entry) -> NodeSpec {
	NodeSpec::new(tag)
		.with_prop(PropId::Id, Value::Str(Str::new(entry.id.to_string())))
		.with_prop(PropId::Order, Value::Str(Str::new(entry.id.to_string())))
		.with_prop(
			PropId::Cause,
			entry
				.by
				.map_or(Value::Null, |id| Value::Str(Str::new(id.to_string()))),
		)
}

fn find_entry_node(dom: &Dom, root: Handle, id: EntryId) -> Option<Handle> {
	let wanted = id.to_string();
	descendants(dom, root).find(|handle| {
		dom.get(*handle)
			.and_then(|node| node.prop(&PropKey::from(PropId::Id)))
			.and_then(Value::as_str)
			.is_some_and(|value| value == wanted)
	})
}

fn descendants(dom: &Dom, root: Handle) -> impl Iterator<Item = Handle> + '_ {
	dom.handles().filter(move |handle| {
		let mut at = Some(*handle);
		while let Some(current) = at {
			if current == root {
				return true;
			}
			at = dom.parent(current);
		}
		false
	})
}

fn child_with_tag(dom: &Dom, parent: Handle, tag: KnownTag) -> Option<Handle> {
	dom.children(parent).iter().copied().find(|handle| {
		dom.get(*handle)
			.is_some_and(|node| node.tag == Tag::Known(tag))
	})
}

fn project_update(
	dom: &Dom,
	call: Handle,
	value: &serde_json::Value,
	ops: &mut Vec<Op>,
) -> Result<(), SessionError> {
	let object = value.as_object();
	let (target, severity, projected) = if let Some(diag) =
		object.and_then(|map| map.get("diag").or_else(|| map.get("diagnostic")))
	{
		(KnownTag::Diag, Some("info"), diag)
	} else if let Some(usage) = object.and_then(|map| map.get("usage")) {
		(KnownTag::Usage, None, usage)
	} else {
		let result = object
			.and_then(|map| {
				map.get("result")
					.or_else(|| map.get("output"))
					.or_else(|| map.get("text"))
			})
			.unwrap_or(value);
		(KnownTag::Result, None, result)
	};
	let child = child_with_tag(dom, call, target).ok_or(SessionError::NoActiveTurn)?;
	let text = match projected {
		serde_json::Value::String(text) => Str::new(text),
		_ => Str::new(serde_json::to_string(projected)?),
	};
	ops.push(Op::Set { h: child, prop: PropId::Text.into(), value: Value::Str(text) });
	ops.push(Op::Set {
		h:     child,
		prop:  PropId::Data.into(),
		value: Value::Json(RawValue::from_string(serde_json::to_string(projected)?)?),
	});
	if let Some(severity) = severity {
		ops.push(Op::Set {
			h:     child,
			prop:  PropId::Severity.into(),
			value: Value::Str(Str::new_static(severity)),
		});
	}
	Ok(())
}

fn json_text(raw: &RawValue) -> Str {
	serde_json::from_str::<Str>(raw.get()).unwrap_or_else(|_| Str::new(raw.get()))
}

pub(crate) fn prompt_parts_text(raw: &RawValue) -> Result<Str, SessionError> {
	let parts: Vec<ToolPart> = serde_json::from_str(raw.get())?;
	let mut text = String::new();
	for part in parts {
		match part {
			ToolPart::Text { text: part } => text.push_str(part.as_str()),
			ToolPart::Json { json } => {
				let part = std::str::from_utf8(&json)
					.map_err(|source| SessionError::ToolPartUtf8 { source })?;
				text.push_str(part);
			},
			ToolPart::Blob { alt: Some(part), .. } => text.push_str(part.as_str()),
			ToolPart::Blob { alt: None, .. } => {},
		}
	}
	Ok(Str::new(text))
}

fn unsigned(value: u64) -> Value {
	Value::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

fn blob_address(blob: &BlobRef) -> Str {
	Str::new(format!("artifact://sha256/{}", blob.to_hex()))
}
