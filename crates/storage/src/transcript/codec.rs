//! Hand-written JSON line codec preserving raw payload bytes.

use std::{
	io,
	path::PathBuf,
	str::{self, Utf8Error},
};

use bytes::BufMut;
use omp_core::{Hash32, Principal, Provenance, Str};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use thiserror::Error as ThisError;

use super::{
	event::{
		ApprovalDecided, ApprovalTicketFiled, ChildLifecycleEntry, ChildSessionInit, Custom,
		EntryUndecodable, Event, HookOutcome, ItemRecord, JobRegistered, JobSettled, Kind,
		PolicyDecision, PromptRewriteCommit, PromptRewriteIntent, PromptRewriteStage,
		SnapcompactArchive, SupersededCompaction, ToolBatchAuthorized, TurnAbort, TurnInputItem,
		TurnInputRecord, TurnOptionsRecord, TurnReceipt, TurnStart,
	},
	msg::{Content, Msg},
	patch::Patch,
	types::{
		AmendPatch, CallId, InvocationTransition, InvocationTransitionError, ModelChange, ModelId,
		ModelRef, Pin, ProviderId, RequestAudit, RequestError, SessionId, ThinkingSel, Tier,
		TitleSource, Usage,
	},
};
use crate::blob::BlobRef;

/// Transcript encoding, decoding, and file-integrity errors.
#[derive(Debug, ThisError)]
pub enum Error {
	/// A file-system operation failed.
	#[error("transcript I/O failed: {0}")]
	Io(#[from] io::Error),
	/// An append failed after writing bytes and the original file length could
	/// not be restored.
	#[error(
		"transcript append failed and partial bytes could not be rolled back: {write}; {rollback}"
	)]
	AppendRollback {
		/// The original append failure.
		write:    io::Error,
		/// The rollback failure that left durability indeterminate.
		rollback: io::Error,
	},
	/// A JSON object could not be encoded or decoded.
	#[error("invalid transcript JSON: {0}")]
	Json(#[from] serde_json::Error),
	/// A journal line was not valid UTF-8.
	#[error("transcript line is not UTF-8: {0}")]
	Utf8(#[from] Utf8Error),
	/// A journal did not contain its required line-zero header.
	#[error("transcript header is missing")]
	MissingHeader,
	/// The line-zero header used an unsupported format version.
	#[error("unsupported transcript version {0}")]
	InvalidHeaderVersion(u8),
	/// A writer was asked to add a second header.
	#[error("a transcript may contain exactly one header")]
	DuplicateHeader,
	/// A recognized event did not contain its timestamp.
	#[error("recognized transcript event is missing `ts`")]
	MissingTimestamp,
	/// An inference update did not change or clear any field.
	#[error("an infer event must change or clear at least one field")]
	EmptyInfer,
	/// An invocation transition carried facts outside its recorded phase.
	#[error(transparent)]
	InvalidInvocationTransition(#[from] InvocationTransitionError),
	/// An undecodable record was incorrectly paired with a decoded value.
	#[error("an EntryUndecodable record must have value=None")]
	UndecodableHasValue,
	/// A committed event-group envelope was empty, unsupported, or
	/// non-canonical.
	#[error("invalid committed transcript event group")]
	InvalidAtomicGroup,
}

/// The identity header stored at line zero of every transcript v4 file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
	/// Transcript format version; transcript v4 requires the value `4`.
	pub v:       u8,
	/// Stable session identifier.
	pub id:      SessionId,
	/// Session creation time in epoch milliseconds.
	pub created: u64,
	/// Absolute working directory at session creation.
	pub cwd:     PathBuf,
}

struct Object<'a, B> {
	out:   &'a mut B,
	first: bool,
}

impl<'a, B: BufMut> Object<'a, B> {
	fn new(out: &'a mut B) -> Self {
		out.put_u8(b'{');
		Self { out, first: true }
	}

	fn field<T>(&mut self, name: &'static str, value: &T) -> Result<(), Error>
	where
		T: Serialize + ?Sized,
	{
		if self.first {
			self.first = false;
		} else {
			self.out.put_u8(b',');
		}
		serde_json::to_writer((&mut *self.out).writer(), name)?;
		self.out.put_u8(b':');
		serde_json::to_writer((&mut *self.out).writer(), value)?;
		Ok(())
	}

	fn finish(self) {
		self.out.put_u8(b'}');
	}
}

/// Writes a header object without a trailing newline.
pub fn write_header(header: &Header, out: &mut impl BufMut) -> Result<(), Error> {
	let mut object = Object::new(out);
	object.field("v", &header.v)?;
	object.field("id", &header.id)?;
	object.field("created", &header.created)?;
	object.field("cwd", &header.cwd)?;
	object.finish();
	Ok(())
}

/// Reads and validates a transcript v4 header object.
pub fn read_header(line: &[u8]) -> Result<Header, Error> {
	let header: Header = serde_json::from_slice(line)?;
	if header.v != 4 {
		return Err(Error::InvalidHeaderVersion(header.v));
	}
	Ok(header)
}

/// Writes one event object without a trailing newline.
///
/// Undecodable event objects are copied byte-for-byte. Recognized objects are
/// emitted directly to the destination, and every [`RawValue`] is serialized as
/// a raw JSON fragment rather than buffered through an intermediate value tree.
pub fn write_line(event: &Event, out: &mut impl BufMut) -> Result<(), Error> {
	if let Kind::EntryUndecodable(entry) = &event.kind {
		if entry.value.is_some() {
			return Err(Error::UndecodableHasValue);
		}
		out.put_slice(entry.raw.get().as_bytes());
		return Ok(());
	}
	let mut object = Object::new(out);
	object.field("ts", &event.ts)?;
	match &event.kind {
		Kind::Init { system_prompt, tools, agent, output_schema, revival } => {
			object.field("k", "init")?;
			object.field("system_prompt", system_prompt)?;
			object.field("tools", tools)?;
			object.field("agent", agent)?;
			object.field("output_schema", output_schema)?;
			if let Some(revival) = revival {
				object.field("revival", revival)?;
			}
		},
		Kind::ChildLifecycle(value) => {
			object.field("k", "child_lifecycle")?;
			object.field("value", value)?;
		},
		Kind::Msg(message) => write_msg_fields(&mut object, message)?,
		Kind::Item(record) => {
			object.field("k", "item")?;
			object.field("item", &record.item)?;
			object.field("turn_id", &record.turn_id)?;
			object.field("prompt_hash", &record.prompt_hash)?;
		},
		Kind::Failed { error, model, usage } => {
			object.field("k", "failed")?;
			object.field("error", error)?;
			object.field("model", model)?;
			object.field("usage", usage)?;
		},
		Kind::Infer { thinking, model, tier, cred_pin } => {
			object.field("k", "infer")?;
			if !thinking.is_unchanged() {
				object.field("thinking", thinking)?;
			}
			if !model.is_unchanged() {
				object.field("model", model)?;
			}
			if !tier.is_unchanged() {
				object.field("tier", tier)?;
			}
			if !cred_pin.is_unchanged() {
				object.field("cred_pin", cred_pin)?;
			}
		},
		Kind::HookOutcome(outcome) => {
			object.field("k", "hook_outcome")?;
			object.field("value", outcome)?;
		},
		Kind::PolicyDecision(decision) => {
			object.field("k", "policy_decision")?;
			object.field("value", decision)?;
		},
		Kind::ApprovalTicketFiled(ticket) => {
			object.field("k", "approval_ticket_filed")?;
			object.field("value", ticket)?;
		},
		Kind::ApprovalDecided(decision) => {
			object.field("k", "approval_decided")?;
			object.field("value", decision)?;
		},
		Kind::Rewind { to } => {
			object.field("k", "rewind")?;
			object.field("to", to)?;
		},
		Kind::Compact {
			summary,
			short,
			first_kept,
			tokens_before,
			tokens_after,
			method,
			warning,
			superseded,
			snapcompact,
		} => {
			object.field("k", "compact")?;
			object.field("summary", summary)?;
			object.field("short", short)?;
			object.field("first_kept", first_kept)?;
			object.field("tokens_before", tokens_before)?;
			if let Some(tokens_after) = tokens_after {
				object.field("tokens_after", tokens_after)?;
			}
			if let Some(method) = method {
				object.field("method", method)?;
			}
			object.field("warning", warning)?;
			if !superseded.is_empty() {
				object.field("superseded", superseded)?;
			}
			if let Some(snapcompact) = snapcompact {
				object.field("snapcompact", snapcompact)?;
			}
		},
		Kind::Branch { from, summary } => {
			object.field("k", "branch")?;
			object.field("from", from)?;
			object.field("summary", summary)?;
		},
		Kind::Reset => object.field("k", "reset")?,
		Kind::ProviderReset => object.field("k", "provider_reset")?,
		Kind::Title { title, source } => {
			object.field("k", "title")?;
			object.field("title", title)?;
			object.field("source", source)?;
		},
		Kind::MoveRoot { root } => {
			object.field("k", "move_root")?;
			object.field("root", root)?;
		},
		Kind::AddDirs { dirs } => {
			object.field("k", "add_dirs")?;
			object.field("dirs", dirs)?;
		},
		Kind::RemoveDirs { dirs } => {
			object.field("k", "remove_dirs")?;
			object.field("dirs", dirs)?;
		},
		Kind::ForkedFrom { session, at } => {
			object.field("k", "forked_from")?;
			object.field("session", session)?;
			object.field("at", at)?;
		},
		Kind::NativeCheckpoint { provider, model, items } => {
			object.field("k", "native_checkpoint")?;
			object.field("provider", provider)?;
			object.field("model", model)?;
			object.field("items", items)?;
		},
		Kind::Aborted { tool_call_ids } => {
			object.field("k", "aborted")?;
			object.field("tool_call_ids", tool_call_ids)?;
		},
		Kind::Amend { target, patch } => {
			object.field("k", "amend")?;
			object.field("target", target)?;
			object.field("patch", patch)?;
		},
		Kind::PromptRewriteIntent(intent) => {
			object.field("k", "prompt_rewrite_intent")?;
			object.field("prompt_hash", &intent.prompt_hash)?;
			object.field("head", &intent.head)?;
			object.field("preserved_tail", &intent.preserved_tail)?;
		},
		Kind::TurnInput(input) => {
			object.field("k", "turn_input")?;
			object.field("turn_id", &input.turn_id)?;
			object.field("item", &input.item)?;
			object.field("prompt_hash", &input.prompt_hash)?;
		},
		Kind::PromptRewriteStage(stage) => {
			object.field("k", "prompt_rewrite_stage")?;
			object.field("intent", &stage.intent)?;
			object.field("ordinal", &stage.ordinal)?;
			object.field("item", &stage.item)?;
		},
		Kind::PromptRewriteCommit(commit) => {
			object.field("k", "prompt_rewrite_commit")?;
			object.field("intent", &commit.intent)?;
			object.field("head_events", &commit.head_events)?;
		},
		Kind::JobRegistered(registered) => {
			object.field("k", "job_registered")?;
			object.field("job", &registered.job)?;
		},
		Kind::JobSettled(settled) => {
			object.field("k", "job_settled")?;
			object.field("job_id", &settled.job_id)?;
			object.field("settlement", &settled.settlement)?;
		},
		Kind::ToolBatchAuthorized(batch) => {
			object.field("k", "tool_batch_authorized")?;
			object.field("turn_id", &batch.turn_id)?;
			object.field("call_ids", &batch.call_ids)?;
		},
		Kind::TurnStart(start) => {
			object.field("k", "turn_start")?;
			object.field("turn_id", &start.turn_id)?;
			object.field("item_events", &start.item_events)?;
			object.field("prompt_hash", &start.prompt_hash)?;
			object.field("prompt_head_events", &start.prompt_head_events)?;
			object.field("toolset_hash", &start.toolset_hash)?;
			object.field("enabled_tools", &start.enabled_tools)?;
			object.field("sequence_targets", &start.sequence_targets)?;
			object.field("input", &start.input)?;
			object.field("options", &start.options)?;
		},
		Kind::TurnAbort(abort) => {
			object.field("k", "turn_abort")?;
			object.field("turn_id", &abort.turn_id)?;
			object.field("recoverable", &abort.recoverable)?;
		},
		Kind::TurnReceipt(receipt) => {
			object.field("k", "turn_receipt")?;
			object.field("turn_id", &receipt.turn_id)?;
			object.field("prompt_hash", &receipt.prompt_hash)?;
			object.field("prompt_head_events", &receipt.prompt_head_events)?;
			object.field("item_events", &receipt.item_events)?;
			object.field("outcome", &receipt.outcome)?;
		},
		Kind::Label { target, label } => {
			object.field("k", "label")?;
			object.field("target", target)?;
			object.field("label", label)?;
		},
		Kind::Custom(custom) => {
			object.field("k", "custom")?;
			object.field("kind", custom.kind())?;
			object.field("rev", &custom.rev())?;
			object.field("source", &custom.source())?;
			object.field("principal", custom.principal())?;
			object.field("provenance", custom.provenance())?;
			object.field("data", &custom.data())?;
			object.field("context", &custom.context())?;
			object.field("display", &custom.display())?;
		},
		Kind::RequestAudit(audit) => {
			object.field("k", "request_audit")?;
			object.field("request_id", &audit.request_id)?;
			object.field("idempotency_key", &audit.idempotency_key)?;
			object.field("extension_id", &audit.extension_id)?;
			object.field("host_generation", &audit.host_generation)?;
			object.field("session_generation", &audit.session_generation)?;
			object.field("operation", &audit.operation)?;
			object.field("indexes", &audit.indexes)?;
		},
		Kind::InvocationTransition(transition) => {
			transition.validate()?;
			object.field("k", "invocation_transition")?;
			object.field("invocation_id", &transition.invocation_id)?;
			object.field("call_id", &transition.call_id)?;
			object.field("phase", &transition.phase)?;
			object.field("requested_args", &transition.requested_args)?;
			object.field("transformations", &transition.transformations)?;
			object.field("effective_args", &transition.effective_args)?;
			object.field("admission_receipt", &transition.admission_receipt)?;
			object.field("assistant_item_event", &transition.assistant_item_event)?;
			object.field("effect_token", &transition.effect_token)?;
			object.field("effects", &transition.effects)?;
			object.field("authorized_at", &transition.authorized_at)?;
			object.field("outcome", &transition.outcome)?;
		},
		Kind::EntryUndecodable(_) => {
			unreachable!("undecodable events return before object encoding")
		},
	}
	object.finish();
	Ok(())
}
const ATOMIC_GROUP_PREFIX: &[u8] = br#"{"$omp_group":"#;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AtomicGroup {
	#[serde(rename = "$omp_group")]
	version: u8,
	events:  Vec<Box<RawValue>>,
}

/// Writes one committed event group as a single newline-delimited record.
///
/// A reader publishes none of the enclosed events until this entire canonical
/// envelope and its terminating newline are present.
pub(crate) fn write_atomic_group(events: &[Event], out: &mut impl BufMut) -> Result<(), Error> {
	out.put_slice(ATOMIC_GROUP_PREFIX);
	out.put_u8(b'1');
	out.put_slice(br#","events":["#);
	for (index, event) in events.iter().enumerate() {
		if index != 0 {
			out.put_u8(b',');
		}
		if matches!(event.kind, Kind::Msg(_)) {
			let mut bounded = event.clone();
			if let Kind::Msg(message) = &mut bounded.kind {
				message.truncate_for_persistence();
			}
			write_line(&bounded, out)?;
		} else {
			write_line(event, out)?;
		}
	}
	out.put_slice(b"]}");
	Ok(())
}

/// Decodes a committed event-group envelope, or reports that the record is an
/// ordinary legacy event line.
pub(crate) fn read_atomic_group(line: &[u8]) -> Option<Result<Vec<Event>, Error>> {
	if !line.starts_with(ATOMIC_GROUP_PREFIX) {
		return None;
	}
	Some((|| {
		let group: AtomicGroup = serde_json::from_slice(line)?;
		if group.version != 1 || group.events.is_empty() || group.events.len() > 1_024 {
			return Err(Error::InvalidAtomicGroup);
		}
		let mut events = Vec::with_capacity(group.events.len());
		for raw in group.events {
			events.push(read_line(raw.get().as_bytes())?);
		}
		let mut canonical = Vec::with_capacity(line.len());
		write_atomic_group(&events, &mut canonical)?;
		if canonical != line {
			return Err(Error::InvalidAtomicGroup);
		}
		Ok(events)
	})())
}

fn write_msg_fields<B: BufMut>(object: &mut Object<'_, B>, message: &Msg) -> Result<(), Error> {
	object.field("k", "msg")?;
	match message {
		Msg::User { content, synthetic, steering, attribution } => {
			object.field("role", "user")?;
			object.field("content", content)?;
			object.field("synthetic", synthetic)?;
			object.field("steering", steering)?;
			object.field("attribution", attribution)?;
		},
		Msg::Developer { content, attribution } => {
			object.field("role", "developer")?;
			object.field("content", content)?;
			object.field("attribution", attribution)?;
		},
		Msg::Assistant {
			content,
			model,
			stop,
			usage,
			response_id,
			upstream,
			ctx,
			timing,
			disabled,
		} => {
			object.field("role", "assistant")?;
			object.field("content", content)?;
			object.field("model", model)?;
			object.field("stop", stop)?;
			object.field("usage", usage)?;
			object.field("response_id", response_id)?;
			object.field("upstream", upstream)?;
			object.field("ctx", ctx)?;
			object.field("timing", timing)?;
			object.field("disabled", disabled)?;
		},
		Msg::ToolResult { call, tool, content, details, error, useless, provider_meta } => {
			object.field("role", "tool_result")?;
			object.field("call", call)?;
			object.field("tool", tool)?;
			object.field("content", content)?;
			object.field("details", details)?;
			object.field("error", error)?;
			object.field("useless", useless)?;
			object.field("provider_meta", provider_meta)?;
		},
	}
	Ok(())
}

#[derive(Deserialize)]
struct Probe {
	#[serde(default)]
	ts:   Option<u64>,
	#[serde(default)]
	k:    Option<Str>,
	#[serde(default)]
	kind: Option<Str>,
	#[serde(default)]
	rev:  Option<Str>,
}

macro_rules! payload {
	($name:ident { $($(#[$attr:meta])* $field:ident : $ty:ty),* $(,)? }) => {
		#[derive(Deserialize)]
		struct $name {
			$(
				$(#[$attr])*
				$field: $ty,
			)*
		}
	};
}

payload!(InitPayload {
	system_prompt: BlobRef,
	tools: Vec<Str>,
	agent: Option<Str>,
	output_schema: Option<Box<RawValue>>,
	#[serde(default)]
	revival: Option<ChildSessionInit>,
});
payload!(ChildLifecyclePayload { value: ChildLifecycleEntry });
payload!(ItemPayload {
	item: omp_proto::thread::v1::Item,
	turn_id: Option<Str>,
	prompt_hash: Option<Hash32>,
});
payload!(FailedPayload {
	error: RequestError,
	model: ModelRef,
	usage: Option<Usage>,
});

#[derive(Serialize, Deserialize)]
struct InferPayload {
	#[serde(default, skip_serializing_if = "Patch::is_unchanged")]
	thinking: Patch<ThinkingSel>,
	#[serde(default, skip_serializing_if = "Patch::is_unchanged")]
	model:    Patch<ModelChange>,
	#[serde(default, skip_serializing_if = "Patch::is_unchanged")]
	tier:     Patch<Tier>,
	#[serde(default, skip_serializing_if = "Patch::is_unchanged")]
	cred_pin: Patch<Pin>,
}
payload!(HookOutcomePayload { value: HookOutcome });
payload!(PolicyDecisionPayload { value: PolicyDecision });
payload!(ApprovalTicketFiledPayload { value: ApprovalTicketFiled });
payload!(ApprovalDecidedPayload { value: ApprovalDecided });

payload!(RewindPayload { to: Option<u64> });
payload!(CompactPayload {
	summary: Str,
	short: Option<Str>,
	first_kept: u64,
	tokens_before: u64,
	#[serde(default)]
	tokens_after: Option<u64>,
	#[serde(default)]
	method: Option<Str>,
	warning: Option<Str>,
	#[serde(default)]
	superseded: Vec<SupersededCompaction>,
	#[serde(default)]
	snapcompact: Option<SnapcompactArchive>,
});
payload!(BranchPayload { from: u64, summary: Str });
payload!(TitlePayload { title: Str, source: TitleSource });
payload!(MoveRootPayload { root: PathBuf });
payload!(AddDirsPayload { dirs: Vec<PathBuf> });
payload!(RemoveDirsPayload { dirs: Vec<PathBuf> });
payload!(ForkedFromPayload { session: SessionId, at: Option<u64> });
payload!(CheckpointPayload { provider: ProviderId, model: ModelId, items: BlobRef });
payload!(AbortedPayload { tool_call_ids: Vec<CallId> });
payload!(AmendPayload { target: u64, patch: AmendPatch });
payload!(TurnInputPayload {
	turn_id: Str,
	item: omp_proto::thread::v1::Item,
	prompt_hash: Option<Hash32>,
});
payload!(PromptRewriteIntentPayload {
	prompt_hash: Hash32,
	head: Vec<omp_proto::thread::v1::Item>,
	preserved_tail: Vec<u64>,
});
payload!(PromptRewriteStagePayload {
	intent:  u64,
	ordinal: u64,
	item:    omp_proto::thread::v1::Item,
});
payload!(PromptRewriteCommitPayload {
	intent: u64,
	head_events: Vec<u64>,
});
payload!(JobRegisteredPayload { job: omp_tool::JobRef });
payload!(JobSettledPayload { job_id: Str, settlement: omp_proto::thread::v1::Item });
payload!(ToolBatchAuthorizedPayload {
	turn_id: Str,
	call_ids: Vec<Str>,
});
payload!(TurnAbortPayload { turn_id: Str, recoverable: bool });
payload!(TurnReceiptPayload {
	turn_id: Str,
	prompt_hash: Hash32,
	prompt_head_events: Vec<u64>,
	item_events: Vec<u64>,
	outcome: omp_proto::inference::v1::Outcome,
});
payload!(TurnStartPayload {
	turn_id: Str,
	item_events: Vec<u64>,
	prompt_hash: Hash32,
	prompt_head_events: Vec<u64>,
	toolset_hash: Hash32,
	enabled_tools: Vec<Str>,
	sequence_targets: Vec<u64>,
	input: TurnInputRecord,
	options: TurnOptionsRecord,
});
payload!(LabelPayload { target: u64, label: Option<Str> });
payload!(CustomPayload {
	kind: Str,
	rev: Option<Str>,
	source: Option<Str>,
	principal: Principal,
	provenance: Provenance,
	data: Option<Box<RawValue>>,
	context: Option<Content>,
	display: bool,
});
payload!(RequestAuditPayload {
	request_id: Str,
	idempotency_key: Str,
	extension_id: Str,
	host_generation: u64,
	session_generation: u64,
	operation: Str,
	indexes: smallvec::SmallVec<u64, 8>,
});
/// Reads one event object with strict canonical validation.
///
/// Valid JSON objects that use an unknown record tag, fail their recorded
/// schema, or differ from canonical append bytes are returned as
/// [`Kind::EntryUndecodable`] with the original bytes intact.
pub fn read_line(line: &[u8]) -> Result<Event, Error> {
	let probe: Probe = serde_json::from_slice(line)?;
	let decoded = match decode_line(line) {
		Ok(event) => event,
		Err(error) => {
			return undecodable_line(
				line,
				probe.ts.unwrap_or_default(),
				probe.kind.or(probe.k),
				probe.rev,
				error.to_string(),
			);
		},
	};
	if matches!(decoded.kind, Kind::EntryUndecodable(_)) {
		return Ok(decoded);
	}
	let mut canonical = Vec::with_capacity(line.len());
	write_line(&decoded, &mut canonical)?;
	if canonical != line {
		return undecodable_line(
			line,
			decoded.ts,
			probe.kind.or(probe.k),
			probe.rev,
			"record is not in canonical append encoding".to_owned(),
		);
	}
	Ok(decoded)
}

fn decode_line(line: &[u8]) -> Result<Event, Error> {
	let probe: Probe = serde_json::from_slice(line)?;
	let Some(tag) = probe.k.clone() else {
		return undecodable_line(
			line,
			probe.ts.unwrap_or_default(),
			probe.kind,
			probe.rev,
			"record has no event kind".to_owned(),
		);
	};
	let Some(ts) = probe.ts else {
		return Err(Error::MissingTimestamp);
	};

	let kind = match tag.as_str() {
		"init" => {
			let payload: InitPayload = serde_json::from_slice(line)?;
			Kind::Init {
				system_prompt: payload.system_prompt,
				tools:         payload.tools,
				agent:         payload.agent,
				output_schema: payload.output_schema,
				revival:       payload.revival,
			}
		},
		"child_lifecycle" => {
			Kind::ChildLifecycle(serde_json::from_slice::<ChildLifecyclePayload>(line)?.value)
		},
		"msg" => Kind::Msg(serde_json::from_slice::<Msg>(line)?),
		"item" => {
			let payload: ItemPayload = serde_json::from_slice(line)?;
			Kind::Item(ItemRecord {
				item:        payload.item,
				turn_id:     payload.turn_id,
				prompt_hash: payload.prompt_hash,
			})
		},
		"failed" => {
			let payload: FailedPayload = serde_json::from_slice(line)?;
			Kind::Failed { error: payload.error, model: payload.model, usage: payload.usage }
		},
		"infer" => {
			let payload: InferPayload = serde_json::from_slice(line)?;
			Kind::Infer {
				thinking: payload.thinking,
				model:    payload.model,
				tier:     payload.tier,
				cred_pin: payload.cred_pin,
			}
		},
		"hook_outcome" => {
			let payload: HookOutcomePayload = serde_json::from_slice(line)?;
			Kind::HookOutcome(payload.value)
		},
		"policy_decision" => {
			let payload: PolicyDecisionPayload = serde_json::from_slice(line)?;
			Kind::PolicyDecision(payload.value)
		},
		"approval_ticket_filed" => {
			let payload: ApprovalTicketFiledPayload = serde_json::from_slice(line)?;
			Kind::ApprovalTicketFiled(payload.value)
		},
		"approval_decided" => {
			let payload: ApprovalDecidedPayload = serde_json::from_slice(line)?;
			Kind::ApprovalDecided(payload.value)
		},
		"rewind" => {
			let payload: RewindPayload = serde_json::from_slice(line)?;
			Kind::Rewind { to: payload.to }
		},
		"compact" => {
			let payload: CompactPayload = serde_json::from_slice(line)?;
			Kind::Compact {
				summary:       payload.summary,
				short:         payload.short,
				first_kept:    payload.first_kept,
				tokens_before: payload.tokens_before,
				tokens_after:  payload.tokens_after,
				method:        payload.method,
				warning:       payload.warning,
				superseded:    payload.superseded,
				snapcompact:   payload.snapcompact,
			}
		},
		"branch" => {
			let payload: BranchPayload = serde_json::from_slice(line)?;
			Kind::Branch { from: payload.from, summary: payload.summary }
		},
		"reset" => Kind::Reset,
		"provider_reset" => Kind::ProviderReset,
		"title" => {
			let payload: TitlePayload = serde_json::from_slice(line)?;
			Kind::Title { title: payload.title, source: payload.source }
		},
		"move_root" => {
			let payload: MoveRootPayload = serde_json::from_slice(line)?;
			Kind::MoveRoot { root: payload.root }
		},
		"add_dirs" => {
			let payload: AddDirsPayload = serde_json::from_slice(line)?;
			Kind::AddDirs { dirs: payload.dirs }
		},
		"remove_dirs" => {
			let payload: RemoveDirsPayload = serde_json::from_slice(line)?;
			Kind::RemoveDirs { dirs: payload.dirs }
		},
		"forked_from" => {
			let payload: ForkedFromPayload = serde_json::from_slice(line)?;
			Kind::ForkedFrom { session: payload.session, at: payload.at }
		},
		"native_checkpoint" => {
			let payload: CheckpointPayload = serde_json::from_slice(line)?;
			Kind::NativeCheckpoint {
				provider: payload.provider,
				model:    payload.model,
				items:    payload.items,
			}
		},
		"aborted" => {
			let payload: AbortedPayload = serde_json::from_slice(line)?;
			Kind::Aborted { tool_call_ids: payload.tool_call_ids }
		},
		"amend" => {
			let payload: AmendPayload = serde_json::from_slice(line)?;
			Kind::Amend { target: payload.target, patch: payload.patch }
		},
		"turn_input" => {
			let payload: TurnInputPayload = serde_json::from_slice(line)?;
			Kind::TurnInput(TurnInputItem {
				turn_id:     payload.turn_id,
				item:        payload.item,
				prompt_hash: payload.prompt_hash,
			})
		},
		"prompt_rewrite_intent" => {
			let payload: PromptRewriteIntentPayload = serde_json::from_slice(line)?;
			Kind::PromptRewriteIntent(PromptRewriteIntent {
				prompt_hash:    payload.prompt_hash,
				head:           payload.head,
				preserved_tail: payload.preserved_tail,
			})
		},
		"prompt_rewrite_stage" => {
			let payload: PromptRewriteStagePayload = serde_json::from_slice(line)?;
			Kind::PromptRewriteStage(PromptRewriteStage {
				intent:  payload.intent,
				ordinal: payload.ordinal,
				item:    payload.item,
			})
		},
		"prompt_rewrite_commit" => {
			let payload: PromptRewriteCommitPayload = serde_json::from_slice(line)?;
			Kind::PromptRewriteCommit(PromptRewriteCommit {
				intent:      payload.intent,
				head_events: payload.head_events,
			})
		},
		"job_registered" => {
			let payload: JobRegisteredPayload = serde_json::from_slice(line)?;
			Kind::JobRegistered(JobRegistered { job: payload.job })
		},
		"job_settled" => {
			let payload: JobSettledPayload = serde_json::from_slice(line)?;
			Kind::JobSettled(JobSettled { job_id: payload.job_id, settlement: payload.settlement })
		},
		"tool_batch_authorized" => {
			let payload: ToolBatchAuthorizedPayload = serde_json::from_slice(line)?;
			Kind::ToolBatchAuthorized(ToolBatchAuthorized {
				turn_id:  payload.turn_id,
				call_ids: payload.call_ids,
			})
		},
		"turn_start" => {
			let payload: TurnStartPayload = serde_json::from_slice(line)?;
			Kind::TurnStart(TurnStart {
				turn_id:            payload.turn_id,
				item_events:        payload.item_events,
				prompt_hash:        payload.prompt_hash,
				prompt_head_events: payload.prompt_head_events,
				toolset_hash:       payload.toolset_hash,
				sequence_targets:   payload.sequence_targets,
				enabled_tools:      payload.enabled_tools,
				input:              payload.input,
				options:            payload.options,
			})
		},
		"turn_abort" => {
			let payload: TurnAbortPayload = serde_json::from_slice(line)?;
			Kind::TurnAbort(TurnAbort {
				turn_id:     payload.turn_id,
				recoverable: payload.recoverable,
			})
		},
		"turn_receipt" => {
			let payload: TurnReceiptPayload = serde_json::from_slice(line)?;
			Kind::TurnReceipt(TurnReceipt {
				turn_id:            payload.turn_id,
				prompt_hash:        payload.prompt_hash,
				prompt_head_events: payload.prompt_head_events,
				item_events:        payload.item_events,
				outcome:            payload.outcome,
			})
		},
		"label" => {
			let payload: LabelPayload = serde_json::from_slice(line)?;
			Kind::Label { target: payload.target, label: payload.label }
		},
		"custom" => {
			let payload: CustomPayload = serde_json::from_slice(line)?;
			Kind::Custom(Custom::new(
				payload.kind,
				payload.rev,
				payload.source,
				payload.principal,
				payload.provenance,
				payload.data,
				payload.context,
				payload.display,
			)?)
		},
		"request_audit" => {
			let payload: RequestAuditPayload = serde_json::from_slice(line)?;
			Kind::RequestAudit(RequestAudit {
				request_id:         payload.request_id,
				idempotency_key:    payload.idempotency_key,
				extension_id:       payload.extension_id,
				host_generation:    payload.host_generation,
				session_generation: payload.session_generation,
				operation:          payload.operation,
				indexes:            payload.indexes,
			})
		},
		"invocation_transition" => {
			let transition: InvocationTransition = serde_json::from_slice(line)?;
			transition.validate()?;
			Kind::InvocationTransition(transition)
		},
		_ => {
			return undecodable_line(
				line,
				ts,
				probe.kind.or(probe.k),
				probe.rev,
				format!("unknown event kind `{tag}`"),
			);
		},
	};
	Ok(Event { ts, kind })
}

fn undecodable_line(
	line: &[u8],
	ts: u64,
	kind: Option<Str>,
	rev: Option<Str>,
	reason: String,
) -> Result<Event, Error> {
	let source = str::from_utf8(line)?.to_owned();
	let raw = RawValue::from_string(source)?;
	Ok(Event {
		ts,
		kind: Kind::EntryUndecodable(EntryUndecodable {
			kind,
			rev,
			value: None,
			raw,
			reason: Str::new(&reason),
		}),
	})
}

#[cfg(test)]
mod tests;
