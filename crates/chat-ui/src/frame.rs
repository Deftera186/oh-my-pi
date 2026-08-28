//! Bounded retained-frame validation and exact-key storage.

use std::{collections::BTreeMap, fmt::Write as _, str};

use omp_core::{IntoStr, Str, sf};
use omp_proto::omp::ui::v1::{
	FrameActionFired, RetainedFrame, RetainedFrameEnvelope, RetainedFrameKey,
	retained_frame_envelope,
};
use prost::Message as _;
use thiserror::Error;

/// Maximum encoded size accepted for one retained-frame envelope.
pub const MAX_FRAME_ENVELOPE_BYTES: usize = 512 * 1024;
/// Maximum typed payload bytes retained by one frame.
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 256 * 1024;
/// Maximum generic TML fallback bytes retained by one frame.
pub const MAX_FRAME_FALLBACK_BYTES: usize = 256 * 1024;
/// Maximum actions declared by one frame.
pub const MAX_FRAME_ACTIONS: usize = 32;
/// Maximum frames retained by one store.
pub const MAX_RETAINED_FRAMES: usize = 2_048;

const MAX_KIND_BYTES: usize = 64;
const MAX_REV_BYTES: usize = 64;
const MAX_STABLE_ID_BYTES: usize = 256;
const MAX_ACTION_NAME_BYTES: usize = 64;
const MAX_CORRELATION_BYTES: usize = 128;

/// Exact retained-frame identity. No kind-only or revision-only lookup exists.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FrameIdentity {
	kind:      Str,
	rev:       Str,
	stable_id: Str,
}

impl FrameIdentity {
	/// Creates an exact identity for a locally projected retained frame.
	pub fn new(kind: impl IntoStr, rev: impl IntoStr, stable_id: impl IntoStr) -> Self {
		Self {
			kind:      kind.into_str(),
			rev:       rev.into_str(),
			stable_id: stable_id.into_str(),
		}
	}

	/// Borrows the semantic frame kind.
	pub fn kind(&self) -> &str {
		self.kind.as_str()
	}

	/// Borrows the schema revision.
	pub fn rev(&self) -> &str {
		self.rev.as_str()
	}

	/// Borrows the producer-stable frame identity.
	pub fn stable_id(&self) -> &str {
		self.stable_id.as_str()
	}
}

/// Result of applying one ordered frame envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameMutation {
	/// A frame was inserted or replaced in place.
	Upserted(FrameIdentity),
	/// An exact frame key was removed. The flag reports whether it existed.
	Removed {
		/// Removed exact identity.
		identity: FrameIdentity,
		/// Whether a retained frame existed for the key.
		existed:  bool,
	},
}

/// Deterministic rejection of malformed or oversized retained UI data.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum FrameError {
	/// The envelope has no typed mutation.
	#[error("retained-frame envelope has no mutation")]
	MissingMutation,
	/// The protobuf envelope exceeds its encoded bound.
	#[error("retained-frame envelope exceeds the encoded size bound")]
	EnvelopeTooLarge,
	/// A frame or removal has no exact key.
	#[error("retained-frame mutation has no key")]
	MissingKey,
	/// A required identity field is empty.
	#[error("retained-frame identity fields must not be empty")]
	EmptyIdentity,
	/// An identity field exceeds its byte bound.
	#[error("retained-frame identity exceeds its byte bound")]
	IdentityTooLarge,
	/// The typed payload exceeds its byte bound.
	#[error("retained-frame payload exceeds its byte bound")]
	PayloadTooLarge,
	/// A frame omitted the deterministic generic TML fallback.
	#[error("retained-frame requires a generic TML fallback")]
	MissingFallback,
	/// Generic TML fallback source exceeds its byte bound.
	#[error("retained-frame fallback exceeds its byte bound")]
	FallbackTooLarge,
	/// A frame declares too many actions.
	#[error("retained-frame declares too many actions")]
	TooManyActions,
	/// An action name or correlation is empty or exceeds its byte bound.
	#[error("retained-frame action identity is invalid")]
	InvalidAction,
	/// An action correlation is duplicated within one frame.
	#[error("retained-frame action correlation is duplicated")]
	DuplicateActionCorrelation,
	/// The store's hard frame capacity was reached.
	#[error("retained-frame store capacity reached")]
	Capacity,
	/// A fired action does not identify a retained frame.
	#[error("retained-frame action targets an unknown frame")]
	UnknownActionFrame,
	/// A fired action does not match the exact declared name and correlation.
	#[error("retained-frame action does not match its declaration")]
	ActionMismatch,
}

/// Exact-key retained frame store with bounded ingress and deterministic
/// fallback.
#[derive(Default)]
pub struct RetainedFrames {
	frames: BTreeMap<FrameIdentity, RetainedFrame>,
}

impl RetainedFrames {
	/// Creates an empty retained-frame store.
	pub fn new() -> Self {
		Self::default()
	}

	/// Returns the retained frame count.
	pub fn len(&self) -> usize {
		self.frames.len()
	}

	/// Reports whether no frames are retained.
	pub fn is_empty(&self) -> bool {
		self.frames.is_empty()
	}

	/// Borrows one frame by its exact `(kind, rev, stable_id)` identity.
	pub fn get(&self, identity: &FrameIdentity) -> Option<&RetainedFrame> {
		self.frames.get(identity)
	}

	/// Retains only frames accepted by `keep`.
	pub fn retain(&mut self, mut keep: impl FnMut(&FrameIdentity) -> bool) {
		self.frames.retain(|identity, _| keep(identity));
	}

	/// Applies one validated ordered envelope.
	pub fn apply(&mut self, envelope: RetainedFrameEnvelope) -> Result<FrameMutation, FrameError> {
		if envelope.encoded_len() > MAX_FRAME_ENVELOPE_BYTES {
			return Err(FrameError::EnvelopeTooLarge);
		}
		match envelope.mutation.ok_or(FrameError::MissingMutation)? {
			retained_frame_envelope::Mutation::Upsert(frame) => {
				let identity = validate_frame(&frame)?;
				if !self.frames.contains_key(&identity) && self.frames.len() == MAX_RETAINED_FRAMES {
					return Err(FrameError::Capacity);
				}
				self.frames.insert(identity.clone(), frame);
				Ok(FrameMutation::Upserted(identity))
			},
			retained_frame_envelope::Mutation::Remove(remove) => {
				let identity = validate_key(remove.key.as_ref())?;
				let existed = self.frames.remove(&identity).is_some();
				Ok(FrameMutation::Removed { identity, existed })
			},
		}
	}

	/// Validates a fired action against the exact retained declaration.
	pub fn validate_action(&self, fired: &FrameActionFired) -> Result<(), FrameError> {
		let identity = validate_key(fired.key.as_ref())?;
		let frame = self
			.frames
			.get(&identity)
			.ok_or(FrameError::UnknownActionFrame)?;
		let matched = frame
			.actions
			.iter()
			.any(|action| action.name == fired.name && action.correlation == fired.correlation);
		if matched {
			Ok(())
		} else {
			Err(FrameError::ActionMismatch)
		}
	}
}

fn validate_frame(frame: &RetainedFrame) -> Result<FrameIdentity, FrameError> {
	let identity = validate_key(frame.key.as_ref())?;
	if frame.payload.len() > MAX_FRAME_PAYLOAD_BYTES {
		return Err(FrameError::PayloadTooLarge);
	}
	let fallback = frame.fallback.as_ref().ok_or(FrameError::MissingFallback)?;
	if fallback.source.len() > MAX_FRAME_FALLBACK_BYTES {
		return Err(FrameError::FallbackTooLarge);
	}
	if frame.actions.len() > MAX_FRAME_ACTIONS {
		return Err(FrameError::TooManyActions);
	}
	let mut correlations = BTreeMap::<&str, ()>::new();
	for action in &frame.actions {
		if action.name.is_empty()
			|| action.name.len() > MAX_ACTION_NAME_BYTES
			|| action.correlation.is_empty()
			|| action.correlation.len() > MAX_CORRELATION_BYTES
		{
			return Err(FrameError::InvalidAction);
		}
		if correlations
			.insert(action.correlation.as_str(), ())
			.is_some()
		{
			return Err(FrameError::DuplicateActionCorrelation);
		}
	}
	Ok(identity)
}

fn validate_key(key: Option<&RetainedFrameKey>) -> Result<FrameIdentity, FrameError> {
	let key = key.ok_or(FrameError::MissingKey)?;
	if key.kind.is_empty() || key.rev.is_empty() || key.stable_id.is_empty() {
		return Err(FrameError::EmptyIdentity);
	}
	if key.kind.len() > MAX_KIND_BYTES
		|| key.rev.len() > MAX_REV_BYTES
		|| key.stable_id.len() > MAX_STABLE_ID_BYTES
	{
		return Err(FrameError::IdentityTooLarge);
	}
	Ok(FrameIdentity {
		kind:      key.kind.as_str().to_str(),
		rev:       key.rev.as_str().to_str(),
		stable_id: key.stable_id.as_str().to_str(),
	})
}

/// Builds the enhanced card for a known typed frame revision, otherwise
/// returns the producer's required generic TML fallback.
pub fn render_frame_tml(frame: &RetainedFrame) -> Str {
	let fallback = || {
		frame
			.fallback
			.as_ref()
			.and_then(|tml| str::from_utf8(&tml.source).ok())
			.map_or_else(|| sf!("<text fg=error>invalid retained-frame fallback</text>"), Str::from)
	};
	let Some(key) = frame.key.as_ref() else {
		return fallback();
	};
	if !matches!(key.rev.as_str(), "1" | "v1") {
		return fallback();
	}
	let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&frame.payload) else {
		return fallback();
	};
	if let Some(rendered) = render_portfolio_card(&key.kind, &payload) {
		return rendered;
	}
	let Some(spec) = card_spec(&key.kind) else {
		return fallback();
	};
	render_typed_card(spec, &payload).unwrap_or_else(fallback)
}

fn render_portfolio_card(kind: &str, payload: &serde_json::Value) -> Option<Str> {
	match kind {
		"shell" | "exec" => render_shell_frame(payload),
		"hub" => render_hub_frame(payload),
		"irc" => render_irc_frame(payload),
		"async-job" | "async_job" | "async-jobs" | "async_jobs" => render_job_frame(payload),
		"process" | "processes" | "process-log" | "process_log" => render_process_frame(payload),
		_ => None,
	}
}

fn render_shell_frame(payload: &serde_json::Value) -> Option<Str> {
	let object = payload.as_object()?;
	if !["command", "status", "tail", "output", "detach_reason", "detachReason"]
		.iter()
		.any(|key| object.contains_key(*key))
	{
		return None;
	}
	let status = object
		.get("status")
		.and_then(serde_json::Value::as_str)
		.unwrap_or("running");
	let running = matches!(status, "running" | "starting" | "queued");
	let color = match status {
		"exited" | "succeeded" | "success" => "success",
		"timeout" | "timed_out" => "warning",
		"failed" | "cancelled" | "denied" => "error",
		_ => "accent",
	};
	let mut output = String::from("<box border=round pad=\"0 1\" bc=");
	output.push_str(color);
	output.push_str("><col gap=0><row gap=1><text bold fg=accent>$</text><text bold>shell</text>");
	if running {
		output.push_str("<spinner>");
		push_tml_text(&mut output, status);
		output.push_str("</spinner>");
	} else {
		output.push_str("<text fg=");
		output.push_str(color);
		output.push('>');
		push_tml_text(&mut output, status);
		output.push_str("</text>");
	}
	if let Some(code) = object.get("exit_code").and_then(serde_json::Value::as_i64) {
		let _ = write!(output, "<text fg={color}>exit {code}</text>");
	}
	if let Some(wall_ms) = object
		.get("wall_time_ms")
		.or_else(|| object.get("wallClockMs"))
		.and_then(serde_json::Value::as_u64)
	{
		let _ = write!(output, "<text dim>{wall_ms} ms</text>");
	}
	output.push_str("</row>");
	if let Some(command) = object.get("command").and_then(serde_json::Value::as_str) {
		output.push_str("<pre fg=accent>");
		push_tml_text(&mut output, "$ ");
		push_tml_text(&mut output, command);
		output.push_str("</pre>");
	}
	if let Some(cwd) = object.get("cwd").and_then(serde_json::Value::as_str) {
		output.push_str("<row gap=1><text dim>cwd</text><text truncate>");
		push_tml_text(&mut output, cwd);
		output.push_str("</text></row>");
	}
	if let Some(environment) = object.get("env").and_then(serde_json::Value::as_object) {
		output.push_str("<row gap=1><text dim>env</text><text truncate>");
		for (index, key) in environment.keys().take(12).enumerate() {
			if index > 0 {
				output.push_str(" · ");
			}
			push_tml_text(&mut output, key);
		}
		output.push_str("</text></row>");
	}
	if let Some(tail) = object
		.get("tail")
		.or_else(|| object.get("output"))
		.and_then(serde_json::Value::as_str)
	{
		output.push_str("<pre fg=muted>");
		push_tml_text(&mut output, tail);
		output.push_str("</pre>");
		if object.get("truncated").and_then(serde_json::Value::as_bool) == Some(true) {
			output.push_str("<text dim>earlier output hidden · ctrl+o to expand</text>");
		}
	}
	if let Some(reason) = object
		.get("detach_reason")
		.or_else(|| object.get("detachReason"))
		.and_then(serde_json::Value::as_str)
	{
		output.push_str("<row gap=1><text fg=info>detached</text><text>");
		push_tml_text(&mut output, reason);
		output.push_str("</text></row>");
	}
	output.push_str("</col></box>");
	Some(Str::new(output))
}

fn render_hub_frame(payload: &serde_json::Value) -> Option<Str> {
	let object = payload.as_object()?;
	if object.contains_key("peers") {
		return render_roster_frame(payload);
	}
	if object.contains_key("jobs") {
		return render_job_frame(payload);
	}
	if object.contains_key("processes") || object.contains_key("lines") {
		return render_process_frame(payload);
	}
	if object.contains_key("messages")
		|| object.contains_key("message")
		|| object.contains_key("deliveries")
	{
		return render_irc_frame(payload);
	}
	let waited = value_u64(object, &["waiting_ms", "waitingMs", "waitedMs"])?;
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><row gap=1><spinner>hub wait</spinner><text dim>",
	);
	let _ = write!(output, "{waited} ms elapsed");
	output.push_str("</text></row></box>");
	Some(Str::new(output))
}

fn render_roster_frame(payload: &serde_json::Value) -> Option<Str> {
	let peers = payload.get("peers")?.as_array()?;
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>@</text><text bold>Hub roster</text><text dim>",
	);
	let _ = write!(output, "{} peers", peers.len());
	output.push_str("</text></row>");
	for peer in peers.iter().take(24) {
		let Some(peer) = peer.as_object() else {
			continue;
		};
		let name = value_string(peer, &["name", "callerName", "id"]).unwrap_or("unknown");
		let status = value_string(peer, &["status", "lifecycle"]).unwrap_or("unknown");
		output.push_str("<row gap=1>");
		if matches!(status, "running" | "active" | "reviving" | "queued") {
			output.push_str("<spinner></spinner>");
		} else {
			output.push_str("<text dim>○</text>");
		}
		output.push_str("<text bold>");
		push_tml_text(&mut output, name);
		output.push_str("</text><text dim>");
		push_tml_text(&mut output, status);
		if let Some(parent) = value_string(peer, &["parent", "parentId"]) {
			output.push_str(" · child of ");
			push_tml_text(&mut output, parent);
		}
		if let Some(unread) = value_u64(peer, &["unread", "unreadCount"])
			&& unread > 0
		{
			let _ = write!(output, " · {unread} unread");
		}
		output.push_str("</text></row>");
	}
	output.push_str("</col></box>");
	Some(Str::new(output))
}

fn render_irc_frame(payload: &serde_json::Value) -> Option<Str> {
	let object = payload.as_object()?;
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>@</text><text bold>IRC</text></row>",
	);
	let rows = object
		.get("messages")
		.or_else(|| object.get("deliveries"))
		.and_then(serde_json::Value::as_array);
	if let Some(rows) = rows {
		for row in rows.iter().take(24) {
			push_irc_row(&mut output, row);
		}
	} else if object.contains_key("message") || object.contains_key("text") {
		push_irc_row(&mut output, payload);
	} else {
		return None;
	}
	output.push_str("</col></box>");
	Some(Str::new(output))
}

fn push_irc_row(output: &mut String, value: &serde_json::Value) {
	let Some(message) = value.as_object() else {
		return;
	};
	let text = value_string(message, &["text", "message", "status"]).unwrap_or_default();
	output.push_str("<row gap=1><text fg=info>");
	if let Some(title) = value_string(message, &["title"]) {
		push_tml_text(output, title);
	} else {
		let from = value_string(message, &["from", "sender"]).unwrap_or("me");
		let to = value_string(message, &["to", "recipient"]).unwrap_or("hub");
		push_tml_text(output, from);
		output.push_str(" → ");
		push_tml_text(output, to);
	}
	output.push_str("</text><text>");
	push_tml_text(output, text);
	output.push_str("</text></row>");
}

fn render_job_frame(payload: &serde_json::Value) -> Option<Str> {
	let object = payload.as_object()?;
	if !["jobs", "name", "id", "job", "status", "state"]
		.iter()
		.any(|key| object.contains_key(*key))
	{
		return None;
	}
	let jobs = object.get("jobs").and_then(serde_json::Value::as_array);
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>&amp;</text><text bold>Jobs</text></row>",
	);
	if let Some(jobs) = jobs {
		for job in jobs.iter().take(24) {
			push_job_row(&mut output, job);
		}
	} else {
		push_job_row(&mut output, payload);
	}
	output.push_str("</col></box>");
	Some(Str::new(output))
}

fn push_job_row(output: &mut String, value: &serde_json::Value) {
	let Some(job) = value.as_object() else {
		return;
	};
	let name = value_string(job, &["name", "id", "job"]).unwrap_or("unknown");
	let status = value_string(job, &["status", "state", "lifecycle"]).unwrap_or("unknown");
	output.push_str("<row gap=1>");
	if matches!(status, "running" | "active" | "queued" | "waiting") {
		output.push_str("<spinner></spinner>");
	} else {
		output.push_str("<text dim>└</text>");
	}
	output.push_str("<text bold>");
	push_tml_text(output, name);
	output.push_str("</text><text dim>");
	push_tml_text(output, status);
	if let Some(duration) = value_u64(job, &["durationMs", "elapsedMs"]) {
		let _ = write!(output, " · {duration} ms");
	}
	output.push_str("</text></row>");
	if let Some(detail) = value_string(job, &["detail", "result", "error", "reason"]) {
		output.push_str("<text dim>  ");
		push_tml_text(output, detail);
		output.push_str("</text>");
	}
}

fn render_process_frame(payload: &serde_json::Value) -> Option<Str> {
	let object = payload.as_object()?;
	if !["processes", "lines", "name", "status", "state"]
		.iter()
		.any(|key| object.contains_key(*key))
	{
		return None;
	}
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=secondary><col gap=0><row gap=1><text bold \
		 fg=secondary>&gt;_</text><text bold>Processes</text></row>",
	);
	if let Some(lines) = object.get("lines") {
		output.push_str("<box border=round bc=muted><pre>");
		match lines {
			serde_json::Value::Array(lines) => {
				for (index, line) in lines.iter().take(80).enumerate() {
					if index > 0 {
						output.push('\n');
					}
					push_tml_text(&mut output, line.as_str().unwrap_or_default());
				}
			},
			serde_json::Value::String(lines) => push_tml_text(&mut output, lines),
			_ => {},
		}
		output.push_str("</pre></box>");
	} else if let Some(processes) = object
		.get("processes")
		.and_then(serde_json::Value::as_array)
	{
		for process in processes.iter().take(24) {
			push_process_row(&mut output, process);
		}
	} else {
		push_process_row(&mut output, payload);
	}
	output.push_str("</col></box>");
	Some(Str::new(output))
}

fn push_process_row(output: &mut String, value: &serde_json::Value) {
	let Some(process) = value.as_object() else {
		return;
	};
	let name = value_string(process, &["name"]).unwrap_or("unknown");
	let status = value_string(process, &["status", "state"]).unwrap_or("unknown");
	output.push_str("<row gap=1><text bold>");
	push_tml_text(output, name);
	output.push_str("</text><text dim>");
	push_tml_text(output, status);
	if let Some(pid) = value_u64(process, &["pid"]) {
		let _ = write!(output, " · pid {pid}");
	}
	if let Some(uptime) = value_u64(process, &["uptimeMs", "elapsedMs"]) {
		let _ = write!(output, " · up {uptime} ms");
	}
	output.push_str("</text></row>");
}

fn value_string<'a>(
	object: &'a serde_json::Map<String, serde_json::Value>,
	keys: &[&str],
) -> Option<&'a str> {
	keys
		.iter()
		.find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
}

fn value_u64(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<u64> {
	keys
		.iter()
		.find_map(|key| object.get(*key).and_then(serde_json::Value::as_u64))
}

#[derive(Clone, Copy)]
struct CardSpec {
	label:    &'static str,
	color:    &'static str,
	icon:     &'static str,
	max_rows: usize,
}

fn card_spec(kind: &str) -> Option<CardSpec> {
	let (label, color, icon, max_rows) = match kind {
		"diagnostic" | "diagnostics" => ("Diagnostics", "warning", "!", 12),
		"todo" | "todos" => ("TODO", "accent", "[]", 12),
		"usage" => ("Usage", "info", "%", 10),
		"skill" => ("Skill", "accent", "*", 8),
		"hook" => ("Hook", "warning", "~", 8),
		"advisor" => ("Advisor", "secondary", "?", 8),
		"tan" => ("TAN", "accent", "^", 8),
		"irc" => ("IRC", "info", "@", 8),
		"async-job" | "async_job" | "async-jobs" | "async_jobs" => ("Async jobs", "info", "&", 12),
		"file-mention" | "file_mention" | "file-mentions" | "file_mentions" => {
			("File mentions", "secondary", "#", 12)
		},
		"stripped-tool" | "stripped_tool" | "stripped-tools" | "stripped_tools" => {
			("Stripped tools", "muted", "-", 12)
		},
		"policy" | "policy-fact" | "policy_fact" | "ttsr" => ("Policy", "warning", "!", 10),
		_ => return None,
	};
	Some(CardSpec { label, color, icon, max_rows })
}

fn render_typed_card(spec: CardSpec, payload: &serde_json::Value) -> Option<Str> {
	let object = payload.as_object()?;
	let title = object
		.get("title")
		.or_else(|| object.get("name"))
		.or_else(|| object.get("status"))
		.and_then(serde_json::Value::as_str);
	let detail = object
		.get("detail")
		.or_else(|| object.get("message"))
		.or_else(|| object.get("summary"))
		.or_else(|| object.get("reason"))
		.and_then(serde_json::Value::as_str);
	let mut output = String::from("<box border=round pad=\"0 1\" bc=");
	output.push_str(spec.color);
	output.push_str("><col gap=0><row gap=1><text bold fg=");
	output.push_str(spec.color);
	output.push('>');
	push_tml_text(&mut output, spec.icon);
	output.push_str("</text><text bold>");
	push_tml_text(&mut output, spec.label);
	output.push_str("</text>");
	if let Some(title) = title {
		output.push_str("<text dim truncate>");
		push_tml_text(&mut output, title);
		output.push_str("</text>");
	}
	output.push_str("</row>");
	if let Some(detail) = detail {
		output.push_str("<text>");
		push_tml_text(&mut output, detail);
		output.push_str("</text>");
	}
	let rows = card_rows(object);
	for row in rows.iter().take(spec.max_rows) {
		output.push_str("<row gap=1><text fg=");
		output.push_str(spec.color);
		output.push_str(">·</text><text truncate>");
		push_tml_text(&mut output, row);
		output.push_str("</text></row>");
	}
	if rows.len() > spec.max_rows {
		output.push_str("<text dim>");
		let _ = write!(output, "+{} more", rows.len() - spec.max_rows);
		output.push_str("</text>");
	}
	if object
		.get("ttl_ms")
		.and_then(serde_json::Value::as_u64)
		.is_some()
	{
		output.push_str("<text dim>presentation expires; durable fact retained</text>");
	}
	output.push_str("</col></box>");
	Some(Str::new(output))
}

fn card_rows(object: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
	for key in
		["items", "rows", "diagnostics", "todos", "jobs", "files", "messages", "tools", "facts"]
	{
		if let Some(values) = object.get(key).and_then(serde_json::Value::as_array) {
			return values.iter().map(row_label).collect();
		}
	}
	let mut rows = Vec::new();
	for (key, value) in object {
		if matches!(
			key.as_str(),
			"title" | "name" | "status" | "detail" | "message" | "summary" | "reason" | "ttl_ms"
		) {
			continue;
		}
		let value = row_label(value);
		rows.push(format!("{key}: {value}"));
	}
	rows
}

fn row_label(value: &serde_json::Value) -> String {
	match value {
		serde_json::Value::String(text) => text.clone(),
		serde_json::Value::Object(object) => {
			let primary = object
				.get("label")
				.or_else(|| object.get("title"))
				.or_else(|| object.get("path"))
				.or_else(|| object.get("name"))
				.or_else(|| object.get("message"))
				.and_then(serde_json::Value::as_str);
			let state = object
				.get("status")
				.or_else(|| object.get("severity"))
				.or_else(|| object.get("state"))
				.and_then(serde_json::Value::as_str);
			match (primary, state) {
				(Some(primary), Some(state)) => format!("{primary} · {state}"),
				(Some(primary), None) => primary.to_owned(),
				(None, Some(state)) => state.to_owned(),
				(None, None) => serde_json::to_string(value).unwrap_or_default(),
			}
		},
		_ => value.to_string(),
	}
}

/// Escapes markup-significant characters so `text` renders literally in TML.
fn push_tml_text(output: &mut String, text: &str) {
	for character in text.chars() {
		match character {
			'&' => output.push_str("&amp;"),
			'<' => output.push_str("&lt;"),
			'>' => output.push_str("&gt;"),
			'"' => output.push_str("&quot;"),
			'\'' => output.push_str("&apos;"),
			_ => output.push(character),
		}
	}
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use omp_proto::omp::ui::v1::{
		FrameActionFired, RemoveRetainedFrame, RetainedFrame, RetainedFrameAction,
		RetainedFrameEnvelope, RetainedFrameKey, Tml, retained_frame_envelope,
	};

	use super::{FrameError, FrameMutation, RetainedFrames, render_frame_tml};

	fn key(rev: &str) -> RetainedFrameKey {
		RetainedFrameKey {
			kind:      "diagnostic".into(),
			rev:       rev.into(),
			stable_id: "turn:4:event:9".into(),
		}
	}

	fn upsert(rev: &str, payload: &'static [u8], fallback: &'static [u8]) -> RetainedFrameEnvelope {
		RetainedFrameEnvelope {
			mutation: Some(retained_frame_envelope::Mutation::Upsert(RetainedFrame {
				key:      Some(key(rev)),
				payload:  Bytes::from_static(payload),
				fallback: Some(Tml { source: Bytes::from_static(fallback), hash: 7 }),
				actions:  vec![RetainedFrameAction {
					name:        "open".into(),
					correlation: "open:9".into(),
					args:        None,
				}],
			})),
		}
	}

	#[test]
	fn exact_revision_updates_in_place_and_unknown_revision_keeps_fallback() {
		let mut frames = RetainedFrames::new();
		let first = frames.apply(upsert("v99", br#"{"n":1}"#, b"<text>generic</text>"));
		let identity = match first.expect("first frame") {
			FrameMutation::Upserted(identity) => identity,
			FrameMutation::Removed { .. } => panic!("unexpected removal"),
		};
		frames
			.apply(upsert("v99", br#"{"n":2}"#, b"<text>updated</text>"))
			.expect("replace exact key");
		assert_eq!(frames.len(), 1);
		assert_eq!(
			frames
				.get(&identity)
				.and_then(|frame| frame.fallback.as_ref())
				.map(|tml| &tml.source[..]),
			Some(&b"<text>updated</text>"[..])
		);
	}

	#[test]
	fn malformed_and_oversized_frames_fail_boundedly() {
		let mut frames = RetainedFrames::new();
		let missing = RetainedFrameEnvelope {
			mutation: Some(retained_frame_envelope::Mutation::Upsert(RetainedFrame {
				key:      Some(key("v1")),
				payload:  Bytes::new(),
				fallback: None,
				actions:  Vec::new(),
			})),
		};
		assert_eq!(frames.apply(missing), Err(FrameError::MissingFallback));

		let mut oversized = upsert("v1", b"", b"");
		let Some(retained_frame_envelope::Mutation::Upsert(frame)) = oversized.mutation.as_mut()
		else {
			unreachable!()
		};
		frame.payload = Bytes::from(vec![0; super::MAX_FRAME_PAYLOAD_BYTES + 1]);
		assert_eq!(frames.apply(oversized), Err(FrameError::PayloadTooLarge));
	}

	#[test]
	fn actions_require_exact_key_name_and_correlation() {
		let mut frames = RetainedFrames::new();
		frames
			.apply(upsert("v1", b"{}", b"fallback"))
			.expect("frame");
		let mut fired = FrameActionFired {
			key:         Some(key("v1")),
			name:        "open".into(),
			correlation: "open:9".into(),
			args:        None,
		};
		frames.validate_action(&fired).expect("declared action");
		fired.correlation = "open:other".into();
		assert_eq!(frames.validate_action(&fired), Err(FrameError::ActionMismatch));
	}

	#[test]
	fn removal_is_exact_revision_only() {
		let mut frames = RetainedFrames::new();
		frames
			.apply(upsert("v1", b"{}", b"fallback"))
			.expect("frame");
		let removed = frames
			.apply(RetainedFrameEnvelope {
				mutation: Some(retained_frame_envelope::Mutation::Remove(RemoveRetainedFrame {
					key: Some(key("v2")),
				})),
			})
			.expect("remove unknown exact key");
		assert!(matches!(removed, FrameMutation::Removed { existed: false, .. }));
		assert_eq!(frames.len(), 1);
	}
	#[test]
	fn exact_v1_shell_and_hub_frames_render_specialized_cards() {
		let shell = RetainedFrame {
			key:      Some(RetainedFrameKey {
				kind:      "shell".into(),
				rev:       "v1".into(),
				stable_id: "shell:1".into(),
			}),
			payload:  Bytes::from_static(
				br#"{"command":"printf '<ok>'","cwd":"/work","status":"timeout","exit_code":124,"wall_time_ms":5000,"tail":"partial","truncated":true}"#,
			),
			fallback: Some(Tml {
				source: Bytes::from_static(b"<text>fallback</text>"),
				hash:   0,
			}),
			actions:  Vec::new(),
		};
		let rendered = render_frame_tml(&shell);
		assert!(rendered.contains("shell"));
		assert!(rendered.contains("exit 124"));
		assert!(rendered.contains("printf &apos;&lt;ok&gt;&apos;"));
		assert!(rendered.contains("ctrl+o to expand"));

		let mut unknown_shell = shell.clone();
		unknown_shell.key.as_mut().expect("key").rev = "v2".into();
		assert_eq!(render_frame_tml(&unknown_shell).as_str(), "<text>fallback</text>");
		let mut malformed_shell = shell.clone();
		malformed_shell.payload = Bytes::from_static(b"{}");
		assert_eq!(render_frame_tml(&malformed_shell).as_str(), "<text>fallback</text>");
		let hub = RetainedFrame {
			key:      Some(RetainedFrameKey {
				kind:      "hub".into(),
				rev:       "v1".into(),
				stable_id: "hub:1".into(),
			}),
			payload:  Bytes::from_static(
				br#"{"peers":[{"name":"Scout","status":"running","parent":"Main","unreadCount":3}]}"#,
			),
			fallback: Some(Tml { source: Bytes::from_static(b"<text>fallback</text>"), hash: 0 }),
			actions:  Vec::new(),
		};
		let rendered = render_frame_tml(&hub);
		assert!(rendered.contains("Hub roster"));
		assert!(rendered.contains("<spinner>"));
		assert!(rendered.contains("3 unread"));
	}

	/// Every card color in [`super::card_spec`] and the diagnostics payload
	/// path must survive the TML parser; a rejected attribute value would
	/// degrade the whole card into raw markup text.
	#[test]
	fn typed_card_tml_parses_for_every_card_color() {
		let payload = serde_json::json!({
			"title": "Agent error",
			"message": "terminal turn error (Auth): Authentication failed for provider `anthropic`. Use `/login anthropic` in chat or run `omp auth login anthropic`.",
			"severity": "error",
		});
		for kind in [
			"diagnostic",
			"todo",
			"usage",
			"skill",
			"hook",
			"advisor",
			"tan",
			"irc",
			"async-job",
			"file-mention",
			"stripped-tool",
			"policy",
		] {
			let frame = RetainedFrame {
				key:      Some(RetainedFrameKey {
					kind:      kind.into(),
					rev:       "v1".into(),
					stable_id: "card-1".into(),
				}),
				payload:  Bytes::from(serde_json::to_vec(&payload).expect("payload encodes")),
				fallback: None,
				actions:  Vec::new(),
			};
			let source = render_frame_tml(&frame);
			let parsed = omp_tui::Ui::from_markup(source.clone(), 80, omp_tui::UiContext::default());
			assert!(parsed.is_ok(), "{kind} card must parse: {source}");
		}
	}
}
