use std::{
	path::Path,
	time::{SystemTime, SystemTimeError, UNIX_EPOCH},
};

use flume::Receiver;
use omp_core::{FastHashMap, Str};
use omp_dom::{Dom, Event, Handle, Op, PropKey, Sid, Snapshot, Txn, Value};
use omp_journal::{
	Entry, EntryDraft, EntryId, Journal, JournalError, Kind, KindName,
	blob::BlobRef,
	data::{
		Compaction, Genesis, MsgAssistantEnd, MsgAssistantStart, MsgUser, Patch, Stream, StreamOp,
		ToolCall, ToolResult, ToolUpdate, TurnReceipt, TurnStart,
	},
};
use serde::Serialize;
use serde_json::value::RawValue;
use thiserror::Error;

use crate::{
	ComponentRegistry,
	rewind::{LifecycleWork, diff},
};

/// Failure to append, decode, or fold a session entry.
#[derive(Debug, Error)]
pub enum SessionError {
	/// Journal persistence or framing failed.
	#[error(transparent)]
	Journal(#[from] JournalError),
	/// A DOM transaction or stream operation failed.
	#[error(transparent)]
	Dom(#[from] omp_dom::DomError),
	/// A compacted summary blob could not be read.
	#[error(transparent)]
	Blob(#[from] omp_journal::blob::Error),
	/// A compacted summary blob is not UTF-8.
	#[error("compaction summary is not UTF-8")]
	SummaryUtf8 {
		/// UTF-8 validation failure.
		#[source]
		source: std::str::Utf8Error,
	},
	/// A typed journal payload was not valid JSON.
	#[error("session entry payload is invalid")]
	Payload(#[from] serde_json::Error),
	/// A model-facing JSON part was not valid UTF-8.
	#[error("tool JSON projection is not UTF-8")]
	ToolPartUtf8 {
		/// UTF-8 validation failure.
		#[source]
		source: std::str::Utf8Error,
	},
	/// The process working directory could not be read for genesis metadata.
	#[error("current working directory is unavailable")]
	CurrentDirectory {
		/// Operating-system lookup failure.
		#[source]
		source: std::io::Error,
	},
	/// The system clock predates the Unix epoch.
	#[error("system clock predates the Unix epoch")]
	Clock {
		/// Clock conversion failure.
		#[source]
		source: SystemTimeError,
	},
	/// A turn-scoped write was attempted outside an explicit turn.
	#[error("turn-scoped entry requires an active turn")]
	NoActiveTurn,
	/// An assistant completion was attempted before an assistant start.
	#[error("assistant completion requires an active assistant message")]
	NoActiveAssistant,
	/// A rewind target is not retained in this journal.
	#[error("rewind target {id} is not retained in this journal")]
	UnknownEntry {
		/// Missing entry identity.
		id: EntryId,
	},
	/// No unused stream identity remains in the `u32` namespace.
	#[error("session stream identity space is exhausted")]
	StreamIdExhausted,
	/// A stream frame's optional fields do not match its operation.
	#[error("stream frame fields do not match its operation")]
	InvalidStreamFrame,
	/// A caller supplied a stream id other than the next writer id.
	#[error("stream id {actual} does not match next id {expected}")]
	UnexpectedStreamId {
		/// Next valid writer id.
		expected: Sid,
		/// Supplied id.
		actual:   Sid,
	},
	/// A caller attempted to forge a kernel-reserved tool update.
	#[error("kernel-reserved tool update must use the typed session API")]
	ReservedToolUpdate,
	/// A journal stream-open entry contained handle zero.
	#[error("stream target handle {value} is invalid")]
	InvalidHandle {
		/// Invalid numeric handle.
		value: u64,
	},
	/// The standard session-transition component is not registered.
	#[error("session-transition component is not registered")]
	MissingSessionTransitions,
	/// An entry refers to a tool call that is absent from the selected branch.
	#[error("tool event refers to unknown call entry {id}")]
	UnknownCall {
		/// Missing call entry identity.
		id: EntryId,
	},
}

/// The mutable controller for one journal-derived session tree.
pub struct Session {
	pub(crate) journal:               Journal,
	pub(crate) entries:               Vec<Entry>,
	pub(crate) entry_index:           FastHashMap<EntryId, usize>,
	pub(crate) handle_floors:         Vec<u64>,
	pub(crate) stream_floors:         Vec<Sid>,
	pub(crate) dom:                   Dom,
	pub(crate) components:            ComponentRegistry,
	pub(crate) head:                  Option<EntryId>,
	pub(crate) current_turn:          Option<EntryId>,
	pub(crate) current_assistant:     Option<EntryId>,
	pub(crate) call_handles:          FastHashMap<EntryId, Handle>,
	pub(crate) stream_targets:        FastHashMap<Sid, Handle>,
	pub(crate) summaries:             FastHashMap<BlobRef, Str>,
	pub(crate) entry_patch_published: bool,
	pub(crate) next_sid:              Sid,
	pending_prior:                    Option<EntryId>,
}

impl Session {
	/// Creates a new `.oms` journal and commits its genesis entry.
	pub fn create(
		path: impl AsRef<Path>,
		components: ComponentRegistry,
	) -> Result<Self, SessionError> {
		let journal = Journal::create(path)?;
		let cwd =
			std::env::current_dir().map_err(|source| SessionError::CurrentDirectory { source })?;
		let created = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map_err(|source| SessionError::Clock { source })?
			.as_millis()
			.to_string();
		let mut session = Self::empty(journal, components);
		let genesis = Genesis {
			version: 1,
			cwd:     Str::new(cwd.to_string_lossy()),
			created: Str::new(created),
		};
		session.commit(KindName::Journal, None, None, None, &genesis)?;
		Ok(session)
	}

	/// Opens a journal by replaying every file-prefix operation and historical
	/// prior jump.
	pub fn open(
		path: impl AsRef<Path>,
		components: ComponentRegistry,
	) -> Result<Self, SessionError> {
		let (journal, entries) = Journal::open(path)?;
		let mut session = Self::empty(journal, components);
		session.entries = entries;
		session.entry_index = session
			.entries
			.iter()
			.enumerate()
			.map(|(index, entry)| (entry.id, index))
			.collect();
		session.rebuild_all()?;
		Ok(session)
	}

	fn empty(journal: Journal, components: ComponentRegistry) -> Self {
		Self {
			journal,
			entries: Vec::new(),
			entry_index: FastHashMap::default(),
			handle_floors: vec![4],
			stream_floors: vec![1],
			dom: Dom::new(),
			components,
			head: None,
			current_turn: None,
			current_assistant: None,
			call_handles: FastHashMap::default(),
			stream_targets: FastHashMap::default(),
			summaries: FastHashMap::default(),
			entry_patch_published: false,
			next_sid: 0,
			pending_prior: None,
		}
	}

	/// Returns the authoritative materialized DOM.
	#[must_use]
	pub const fn dom(&self) -> &Dom {
		&self.dom
	}

	/// Returns the selected journal head.
	#[must_use]
	pub const fn head(&self) -> Option<EntryId> {
		self.head
	}

	/// Returns one materialized journal entry by identity.
	#[must_use]
	pub fn entry(&self, id: EntryId) -> Option<&Entry> {
		self
			.entry_index
			.get(&id)
			.and_then(|index| self.entries.get(*index))
	}

	/// Returns the journal file path.
	#[must_use]
	pub fn journal_path(&self) -> &Path {
		self.journal.path()
	}

	/// Records switching away from this session without marking process exit.
	pub fn session_switch(&mut self) -> Result<EntryId, SessionError> {
		use crate::components::lifecycle::{SWITCHES, session_switch_count, transitions_handle};

		let handle = transitions_handle(&self.dom).ok_or(SessionError::MissingSessionTransitions)?;
		let next = session_switch_count(&self.dom).saturating_add(1);
		self.patch(Txn {
			cause: self.head.ok_or(SessionError::MissingSessionTransitions)?,
			label: Some(Str::new_static("session.switch")),
			ops:   vec![Op::Set {
				h:     handle,
				prop:  PropKey::Custom(Str::new_static(SWITCHES)),
				value: Value::Int(i64::try_from(next).unwrap_or(i64::MAX)),
			}],
		})
	}

	/// Records process exit as a transition distinct from a session switch.
	pub fn process_exit(&mut self) -> Result<EntryId, SessionError> {
		use crate::components::lifecycle::{PROCESS_EXITED, transitions_handle};

		let handle = transitions_handle(&self.dom).ok_or(SessionError::MissingSessionTransitions)?;
		self.patch(Txn {
			cause: self.head.ok_or(SessionError::MissingSessionTransitions)?,
			label: Some(Str::new_static("process.exit")),
			ops:   vec![Op::Set {
				h:     handle,
				prop:  PropKey::Custom(Str::new_static(PROCESS_EXITED)),
				value: Value::Bool(true),
			}],
		})
	}

	/// Returns an actor snapshot followed by ordered patch, stream, and reset
	/// events.
	pub fn subscribe(&mut self) -> (Snapshot, Receiver<Event>) {
		self.dom.subscribe()
	}

	/// Begins an explicit turn, caused by the previous journal head.
	pub fn begin_turn(&mut self) -> Result<EntryId, SessionError> {
		let by = self.head.ok_or(SessionError::NoActiveTurn)?;
		self.commit(KindName::TurnStart, Some(by), None, None, &TurnStart {})
	}

	/// Appends a user message to the active turn.
	pub fn user(
		&mut self,
		text: impl Into<Str>,
		attachments: Vec<BlobRef>,
	) -> Result<EntryId, SessionError> {
		let by = self.turn_cause()?;
		self.commit(KindName::MsgUser, Some(by), None, None, &MsgUser {
			text: text.into(),
			attachments,
		})
	}

	/// Starts an assistant message in the active turn.
	pub fn assistant_start(
		&mut self,
		model: impl Into<Str>,
		provider: impl Into<Str>,
		route: impl Into<Str>,
	) -> Result<EntryId, SessionError> {
		let by = self.turn_cause()?;
		self.commit(KindName::MsgAssistantStart, Some(by), None, None, &MsgAssistantStart {
			model:    model.into(),
			provider: provider.into(),
			route:    route.into(),
		})
	}

	/// Opens an append-only DOM text-property stream and returns its id.
	pub fn stream_open(&mut self, node: Handle, prop: PropKey) -> Result<Sid, SessionError> {
		let by = self.turn_cause()?;
		let sid = self
			.next_sid
			.checked_add(1)
			.ok_or(SessionError::StreamIdExhausted)?;
		let payload = Stream {
			sid,
			op: StreamOp::Open,
			node: Some(node.get()),
			prop: Some(Self::encode_stream_prop(&prop)),
			text: None,
		};
		self.dom.validate_stream_open_with_id(sid, node, &prop)?;
		self.commit(KindName::Stream, Some(by), None, None, &payload)?;
		Ok(sid)
	}

	/// Appends one delta to an open text stream.
	pub fn stream_append(&mut self, sid: Sid, text: &str) -> Result<EntryId, SessionError> {
		let by = self.turn_cause()?;
		self.dom.validate_stream_append(sid)?;
		let text = Str::new(text);
		self.commit(KindName::Stream, Some(by), None, None, &Stream {
			sid,
			op: StreamOp::Append,
			node: None,
			prop: None,
			text: Some(text),
		})
	}

	/// Closes an open text stream.
	pub fn stream_close(&mut self, sid: Sid) -> Result<EntryId, SessionError> {
		let by = self.turn_cause()?;
		self.dom.validate_stream_close(sid)?;
		self.commit(KindName::Stream, Some(by), None, None, &Stream {
			sid,
			op: StreamOp::Close,
			node: None,
			prop: None,
			text: None,
		})
	}

	/// Completes the active assistant message.
	pub fn assistant_end(&mut self, stop_reason: impl Into<Str>) -> Result<EntryId, SessionError> {
		self.current_assistant_handle()?;
		let by = self.turn_cause()?;
		self.commit(KindName::MsgAssistantEnd, Some(by), None, None, &MsgAssistantEnd {
			stop_reason: stop_reason.into(),
		})
	}

	/// Records one tool invocation in the active turn.
	pub fn call(
		&mut self,
		name: impl Into<Str>,
		rev: u32,
		call_id: impl Into<Str>,
		i: Option<Str>,
		args: Option<Box<RawValue>>,
		sid: Option<Sid>,
	) -> Result<EntryId, SessionError> {
		let by = self.turn_cause()?;
		if let Some(actual) = sid {
			let expected = self
				.next_sid
				.checked_add(1)
				.ok_or(SessionError::StreamIdExhausted)?;
			if actual != expected {
				return Err(SessionError::UnexpectedStreamId { expected, actual });
			}
			self.dom.validate_stream_open_with_id(
				actual,
				self.current_turn_handle()?,
				&PropKey::from(omp_dom::PropId::Text),
			)?;
		}
		self.commit(KindName::ToolCall, Some(by), None, None, &ToolCall {
			name: name.into(),
			rev,
			call_id: call_id.into(),
			i,
			args,
			sid,
		})
	}

	/// Starts a tool call whose arguments are still streaming.
	///
	/// This materializes the call in `arguments` state for live preview, but
	/// does not authorize execution. [`Self::call_ready`] is the sole
	/// authorization boundary.
	pub fn call_streaming(
		&mut self,
		name: impl Into<Str>,
		rev: u32,
		call_id: impl Into<Str>,
		i: Option<Str>,
	) -> Result<(EntryId, Sid), SessionError> {
		let sid = self
			.next_sid
			.checked_add(1)
			.ok_or(SessionError::StreamIdExhausted)?;
		let call = self.call(name, rev, call_id, i, None, Some(sid))?;
		Ok((call, sid))
	}

	/// Commits canonical streamed arguments and authorizes tool execution.
	pub fn call_ready(
		&mut self,
		call: EntryId,
		args: Box<RawValue>,
	) -> Result<EntryId, SessionError> {
		self.ensure_call(call)?;
		let args: serde_json::Value = serde_json::from_str(args.get())?;
		let i = args.get("i").and_then(serde_json::Value::as_str);
		let update = serde_json::value::to_raw_value(&serde_json::json!({
			"kernel": "ready",
			"args": args,
			"i": i,
		}))?;
		self.commit(KindName::ToolUpdate, Some(call), None, None, &ToolUpdate(update))
	}

	/// Records a typed ephemeral update caused by a tool call.
	pub fn call_update(
		&mut self,
		call: EntryId,
		update: Box<RawValue>,
	) -> Result<EntryId, SessionError> {
		self.ensure_call(call)?;
		let value: serde_json::Value = serde_json::from_str(update.get())?;
		if value.get("kernel").is_some() {
			return Err(SessionError::ReservedToolUpdate);
		}
		self.commit(KindName::ToolUpdate, Some(call), None, None, &ToolUpdate(update))
	}

	/// Records a successful terminal tool outcome.
	pub fn settle(
		&mut self,
		call: EntryId,
		outcome: Box<RawValue>,
	) -> Result<EntryId, SessionError> {
		self.ensure_call(call)?;
		self.commit(KindName::ToolResult, Some(call), None, None, &ToolResult::Outcome {
			outcome,
			prompt_parts: None,
		})
	}

	/// Records a successful terminal tool outcome with its durable model
	/// projection.
	pub fn settle_projected(
		&mut self,
		call: EntryId,
		outcome: Box<RawValue>,
		prompt_parts: Box<RawValue>,
	) -> Result<EntryId, SessionError> {
		self.ensure_call(call)?;
		crate::fold::prompt_parts_text(&prompt_parts)?;
		self.commit(KindName::ToolResult, Some(call), None, None, &ToolResult::Outcome {
			outcome,
			prompt_parts: Some(prompt_parts),
		})
	}

	/// Records a failed terminal tool outcome.
	pub fn fail(&mut self, call: EntryId, fault: Box<RawValue>) -> Result<EntryId, SessionError> {
		self.ensure_call(call)?;
		self.commit(KindName::ToolResult, Some(call), None, None, &ToolResult::Fault {
			fault,
			prompt_parts: None,
		})
	}

	/// Records a failed terminal tool outcome with its durable model projection.
	pub fn fail_projected(
		&mut self,
		call: EntryId,
		fault: Box<RawValue>,
		prompt_parts: Box<RawValue>,
	) -> Result<EntryId, SessionError> {
		self.ensure_call(call)?;
		crate::fold::prompt_parts_text(&prompt_parts)?;
		self.commit(KindName::ToolResult, Some(call), None, None, &ToolResult::Fault {
			fault,
			prompt_parts: Some(prompt_parts),
		})
	}

	/// Records token, cache, cost, and timing accounting for the active turn.
	pub fn receipt(&mut self, receipt: TurnReceipt) -> Result<EntryId, SessionError> {
		let by = self.turn_cause()?;
		self.commit(KindName::TurnReceipt, Some(by), None, None, &receipt)
	}

	/// Journals and applies a caller-built DOM transaction.
	pub fn patch(&mut self, txn: Txn) -> Result<EntryId, SessionError> {
		let data = serde_json::to_string(&txn.ops)?;
		let decoded = serde_json::from_str(&data)?;
		self
			.dom
			.validate(&Txn { cause: txn.cause, label: txn.label.clone(), ops: decoded })?;
		let ops = RawValue::from_string(data)?;
		self.commit(KindName::Patch, Some(txn.cause), txn.label, None, &Patch { ops })
	}

	/// Records a content-addressed compaction summary, its hidden boundary,
	/// and the maintenance facts the transcript divider labels.
	pub fn compaction(&mut self, compaction: Compaction) -> Result<EntryId, SessionError> {
		let by = self.turn_cause()?;
		if !self
			.chain_indices(self.head.ok_or(SessionError::NoActiveTurn)?)?
			.into_iter()
			.any(|index| self.entries[index].id == compaction.boundary)
		{
			return Err(SessionError::UnknownEntry { id: compaction.boundary });
		}
		self.compaction_summary(&compaction.summary)?;
		self.commit(KindName::Compaction, Some(by), None, None, &compaction)
	}

	/// Selects `target` by canonical prefix replay and stages it as the next
	/// entry's `prior`.
	pub fn rewind(&mut self, target: EntryId) -> Result<LifecycleWork, SessionError> {
		self
			.entry_index
			.get(&target)
			.ok_or(SessionError::UnknownEntry { id: target })?;
		let before = self.dom.snapshot();
		let high_water = *self.handle_floors.last().expect("floor zero is retained");
		let next_sid = *self.stream_floors.last().expect("floor zero is retained");
		let mut subscribed = std::mem::replace(&mut self.dom, Dom::new());
		let replay = self.rederive_chain(target);
		if let Err(error) = replay {
			self.dom = subscribed;
			return Err(error);
		}
		self.dom.raise_high_water(high_water);
		self.dom.raise_stream_high_water(next_sid);
		self.next_sid = self.next_sid.max(next_sid.saturating_sub(1));
		let after = self.dom.snapshot();
		subscribed.reset(after.clone());
		self.dom = subscribed;
		self.pending_prior = Some(target);
		Ok(diff(&before, &after))
	}

	fn rebuild_all(&mut self) -> Result<(), SessionError> {
		self.dom = Dom::new();
		self.reset_caches();
		self.handle_floors.clear();
		self.handle_floors.push(self.dom.high_water());
		self.stream_floors.clear();
		self.stream_floors.push(1);
		for index in 0..self.entries.len() {
			let entry = self.entries[index].clone();
			let previous = index
				.checked_sub(1)
				.map(|previous| self.entries[previous].id);
			if let Some(target) = entry.prior.filter(|prior| Some(*prior) != previous) {
				self.rederive_chain(target)?;
			}
			self.dom.raise_high_water(self.handle_floors[index]);
			self.dom.raise_stream_high_water(self.stream_floors[index]);
			self.next_sid = self
				.next_sid
				.max(self.stream_floors[index].saturating_sub(1));
			self.apply(&entry)?;
			self.handle_floors.push(self.dom.high_water());
			self
				.stream_floors
				.push(self.next_sid.saturating_add(1).max(1));
		}
		Ok(())
	}

	fn rederive_chain(&mut self, target: EntryId) -> Result<(), SessionError> {
		let chain = self.chain_indices(target)?;
		self.dom = Dom::new();
		self.reset_caches();
		for index in chain {
			self.dom.raise_high_water(self.handle_floors[index]);
			self.dom.raise_stream_high_water(self.stream_floors[index]);
			self.next_sid = self
				.next_sid
				.max(self.stream_floors[index].saturating_sub(1));
			let entry = self.entries[index].clone();
			self.apply(&entry)?;
		}
		Ok(())
	}

	fn reset_caches(&mut self) {
		self.head = None;
		self.current_turn = None;
		self.current_assistant = None;
		self.call_handles.clear();
		self.stream_targets.clear();
		self.next_sid = 0;
	}

	fn chain_indices(&self, target: EntryId) -> Result<Vec<usize>, SessionError> {
		let mut reverse = Vec::new();
		let mut index = *self
			.entry_index
			.get(&target)
			.ok_or(SessionError::UnknownEntry { id: target })?;
		loop {
			reverse.push(index);
			let Some(parent) = self.entries[index].prior else {
				let Some(previous) = index.checked_sub(1) else {
					break;
				};
				index = previous;
				continue;
			};
			index = *self
				.entry_index
				.get(&parent)
				.ok_or(SessionError::UnknownEntry { id: parent })?;
		}
		reverse.reverse();
		Ok(reverse)
	}

	pub(crate) fn decode_stream_prop(value: &str) -> PropKey {
		value.strip_prefix("custom:").map_or_else(
			|| value.parse().expect("property parsing is infallible"),
			|name| PropKey::Custom(Str::new(name)),
		)
	}

	fn encode_stream_prop(prop: &PropKey) -> Str {
		match prop {
			PropKey::Known(_) => Str::new(prop.as_str()),
			PropKey::Custom(name) => Str::new(format!("custom:{}", name.as_str())),
		}
	}

	fn turn_cause(&self) -> Result<EntryId, SessionError> {
		self.current_turn_handle()?;
		self.current_turn.ok_or(SessionError::NoActiveTurn)
	}

	fn ensure_call(&self, call: EntryId) -> Result<(), SessionError> {
		let handle = self
			.call_handles
			.get(&call)
			.copied()
			.ok_or(SessionError::UnknownCall { id: call })?;
		let complete = [
			omp_dom::KnownTag::Input,
			omp_dom::KnownTag::Result,
			omp_dom::KnownTag::Diag,
			omp_dom::KnownTag::Usage,
		]
		.into_iter()
		.all(|tag| {
			self.dom.children(handle).iter().any(|child| {
				self
					.dom
					.get(*child)
					.is_some_and(|node| node.tag == omp_dom::Tag::Known(tag))
			})
		});
		complete
			.then_some(())
			.ok_or(SessionError::UnknownCall { id: call })
	}

	pub(crate) fn compaction_summary(&mut self, summary: &BlobRef) -> Result<Str, SessionError> {
		if let Some(text) = self.summaries.get(summary) {
			return Ok(text.clone());
		}
		let root = self
			.journal
			.path()
			.parent()
			.unwrap_or_else(|| Path::new("."));
		let store = omp_journal::blob::BlobStore::open(root)?;
		let bytes = store.get(summary)?;
		let text =
			std::str::from_utf8(&bytes).map_err(|source| SessionError::SummaryUtf8 { source })?;
		let text = Str::new(text);
		self.summaries.insert(*summary, text.clone());
		Ok(text)
	}

	fn commit<T: Serialize>(
		&mut self,
		kind: KindName,
		by: Option<EntryId>,
		label: Option<Str>,
		prior: Option<EntryId>,
		data: &T,
	) -> Result<EntryId, SessionError> {
		let data = Str::new(serde_json::to_string(data)?);
		let draft = EntryDraft {
			kind: Kind::known(kind),
			by,
			prior: prior.or(self.pending_prior),
			label,
			data,
		};
		let entry = self.journal.append(draft)?;
		self.pending_prior = None;
		let id = entry.id;
		let index = self.entries.len();
		self.entries.push(entry.clone());
		self.entry_index.insert(id, index);
		self.apply(&entry)?;
		self.handle_floors.push(self.dom.high_water());
		self
			.stream_floors
			.push(self.next_sid.saturating_add(1).max(1));
		Ok(id)
	}
}
