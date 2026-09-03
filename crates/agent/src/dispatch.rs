//! Whole-lifetime tool dispatch and central execution policy.

use std::{
	future::Future,
	pin::Pin,
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use flume::Receiver;
use futures::{Stream, StreamExt as _};
use omp_core::{FastHashMap, Str, sf};
use omp_dom::{Handle, KnownTag, PropId, Sid, Tag};
use omp_journal::{
	EntryId,
	blob::{BlobRef, BlobStore},
};
use omp_session::{Session, SessionError};
use omp_tool::{
	Abort, ArtifactLifetime, CallOutcome, CapsBase, ErasedEv, ErasedOutcome, ExpectedArtifact,
	IncomingParams, Interrupt, JobKind, JobMetadata, JobOwner, JobRef, ModelClass, Part, PromptCaps,
	Registry, RegistryError, Rev, ToolIdentity, ToolRoute, ToolSpec,
};
use serde_json::value::RawValue;
use thiserror::Error;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

use crate::{
	JobBoard, KernelEvent, SessionAuthority,
	cancel::{BackgroundToolCancellation, ForegroundMutationCancellation, ReadOnlyToolCancellation},
	events::KernelEvents,
};

/// Cancellation authority selected from a tool's declared effects.
#[derive(Clone, Debug)]
pub enum ToolCancellation {
	/// Session-only cancellation for a foreground mutation.
	Foreground(ForegroundMutationCancellation),
	/// Turn-scoped cancellation for a read-only call.
	ReadOnly(ReadOnlyToolCancellation),
	/// Turn-scoped cancellation for detached or background work.
	Background(BackgroundToolCancellation),
}

impl ToolCancellation {
	/// Host stop request for this call: turn interruption or session
	/// cancellation for every scope.
	fn interrupt_token(&self) -> CancellationToken {
		match self {
			Self::Foreground(scope) => scope.interrupt_token(),
			Self::ReadOnly(scope) => scope.token(),
			Self::Background(scope) => scope.token(),
		}
	}
}

/// Central policy applied once to every tool call.
#[derive(Clone, Debug)]
pub struct DispatchPolicy {
	/// Maximum inline output bytes.
	pub max_output_bytes: usize,
	/// Maximum bytes retained from one output line.
	pub max_line_bytes:   usize,
	/// Maximum time a call may block the turn.
	pub blocking_limit:   Duration,
	/// Bounded wait after a stop request before a call that has not settled
	/// is forcibly terminated and journaled as effects-unknown (ADR 0011).
	/// Execution units apply their own courtesy grace inside this bound.
	pub interrupt_grace:  Duration,
	/// Content-addressed store for complete spilled output.
	pub spill:            BlobStore,
}

impl DispatchPolicy {
	/// Creates the standard 64 KiB / 512-byte / 30-second / 1-second policy.
	#[must_use]
	pub const fn new(spill: BlobStore) -> Self {
		Self {
			max_output_bytes: 64 * 1024,
			max_line_bytes: 512,
			blocking_limit: Duration::from_secs(30),
			interrupt_grace: Duration::from_secs(1),
			spill,
		}
	}

	/// Replaces the bounded settle window granted after a stop request.
	#[must_use]
	pub const fn with_interrupt_grace(mut self, interrupt_grace: Duration) -> Self {
		self.interrupt_grace = interrupt_grace;
		self
	}

	/// Replaces central limits while retaining the selected blob store.
	#[must_use]
	pub const fn with_limits(
		mut self,
		max_output_bytes: usize,
		max_line_bytes: usize,
		blocking_limit: Duration,
	) -> Self {
		self.max_output_bytes = max_output_bytes;
		self.max_line_bytes = max_line_bytes;
		self.blocking_limit = blocking_limit;
		self
	}
}

/// Per-call policy choices parsed from model arguments.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DispatchOptions {
	/// Explicitly bypasses both central inline limits.
	pub notrunc: bool,
}

impl DispatchOptions {
	/// Reads the caller-owned `notrunc` escape hatch from canonical arguments.
	#[must_use]
	pub fn from_args(args: &RawValue) -> Self {
		let notrunc = serde_json::from_str::<serde_json::Value>(args.get())
			.ok()
			.and_then(|value| value.get("notrunc").and_then(serde_json::Value::as_bool))
			.unwrap_or(false);
		Self { notrunc }
	}
}

/// One authorized invocation ready for registry dispatch.
pub struct DispatchRequest {
	/// Exact live tool identity recorded on the call element.
	pub identity:     ToolIdentity,
	/// Stable provider call identity.
	pub call_id:      Str,
	/// Journal identity of the corresponding `tool.call@1`.
	pub call:         EntryId,
	/// Canonical committed argument object.
	pub args:         Box<RawValue>,
	/// Central caller choices.
	pub options:      DispatchOptions,
	/// Cancellation scope selected from the tool's effects.
	pub cancellation: ToolCancellation,
}

/// One externally routed invocation with committed canonical arguments.
pub struct ExternalDispatchRequest {
	/// Exact selected tool identity.
	pub identity:       ToolIdentity,
	/// Stable provider call identity.
	pub call_id:        Str,
	/// Canonical committed argument object.
	pub args:           Box<RawValue>,
	/// Resolved worker or remote execution route.
	pub route:          ToolRoute,
	/// Maximum time the invocation may block this turn.
	pub blocking_limit: Duration,
	/// Turn/session cancellation the executor must honor (ADR 0011): once
	/// cancelled, the invocation is interrupted and settles aborted.
	pub cancellation:   CancellationToken,
}

/// One state mutation produced by an externally routed tool executor.
pub enum ExternalDispatchEvent {
	/// Ephemeral structured progress.
	Update(Box<RawValue>),
	/// Durable structured outcome and its canonical model-facing projection.
	Done {
		/// Serialized `CallOutcome` truth.
		outcome:  Box<RawValue>,
		/// Canonical bounded-later model-facing parts.
		parts:    Vec<Part>,
		/// Whether the outcome is model-facing error content.
		is_error: bool,
	},
	/// Execution stopped without a normal typed verdict.
	Aborted(Abort),
}

/// Owned externally routed tool event stream.
pub type ExternalDispatchStream =
	Pin<Box<dyn Stream<Item = ExternalDispatchEvent> + Send + 'static>>;

/// Host composition seam for worker- and remote-routed tool execution.
pub trait ExternalToolExecutor: Send + Sync {
	/// Opens one committed invocation. Every stream must end in `Done` or
	/// `Aborted`; transport failures are mapped to an explicit abort by the
	/// adapter while their typed source is logged at that boundary.
	fn invoke(&self, request: ExternalDispatchRequest) -> ExternalDispatchStream;
}

/// One boxed cold-call future at the session-tool dynamic quarantine.
///
/// Session-owned tools are rare, spawn-scale operations (`task`, `hub`), so
/// one allocation per call is intentional; ordinary tools retain static
/// dispatch through [`Registry`].
pub type SessionToolFuture<'a> = Pin<
	Box<
		dyn Future<Output = Result<CallOutcome<Box<RawValue>, Box<RawValue>>, SessionToolError>>
			+ Send
			+ 'a,
	>,
>;

/// Runtime context available only to a session-owned tool.
pub struct SessionToolCx<'a> {
	/// Authoritative parent session controller.
	pub session:   &'a mut Session,
	/// Materialized tool-call element.
	pub call:      Handle,
	/// Disposable runtime index over `<meta><jobs>`.
	pub jobs:      &'a JobBoard,
	/// Kill boundary for detached work.
	pub cancel:    BackgroundToolCancellation,
	/// Host-owned routing authority for live peer sessions.
	pub authority: Option<&'a dyn SessionAuthority>,
}

/// Failure before a session tool can produce its typed terminal outcome.
#[derive(Debug, Error)]
pub enum SessionToolError {
	/// Session-tool argument or outcome JSON was malformed.
	#[error("session tool JSON is invalid")]
	Json(#[from] serde_json::Error),
	/// Host composition rejected the operation before a typed tool fault.
	#[error("{message}")]
	Rejected {
		/// Stable diagnostic.
		message: Str,
	},
}

/// A host-authority tool whose operation requires the session DOM.
///
/// Implementations must journal through `Session`; private durable state is
/// prohibited.
pub trait SessionTool: Send + Sync {
	/// Exact model-facing declaration.
	fn spec(&self) -> &ToolSpec;
	/// Executes one committed call against the authoritative session.
	fn call<'a>(&'a self, cx: SessionToolCx<'a>, args: Box<RawValue>) -> SessionToolFuture<'a>;
}

/// Live ordered output of one dispatched call (ADR 0008 tool output
/// streaming).
///
/// A tool update carrying `sequence` and `data` is an ordered output frame
/// (`omp_tools::shell::Update`, eval process frames). The dispatcher binds a
/// DOM text stream to the call's `<result>` text at the first frame, appends
/// each frame's bytes as UTF-8 in sequence order, and closes the stream
/// before the terminal, so a running card reads the whole output in O(Δ)
/// per frame and replay reproduces it byte for byte. The typed update still
/// journals the frame's metadata; its bytes live only in the stream.
#[derive(Default)]
struct OutputStream {
	sid:   Option<Sid>,
	/// Highest sequence appended; stale or duplicate frames are dropped.
	last:  Option<u64>,
	/// Bytes of a UTF-8 sequence split across frames, completed by the next.
	carry: Vec<u8>,
}

impl OutputStream {
	/// Reads the frame's ordering and bytes when `value` is an output frame.
	fn frame(value: &serde_json::Value) -> Option<(u64, Vec<u8>)> {
		let sequence = value.get("sequence")?.as_u64()?;
		let bytes = match value.get("data")? {
			serde_json::Value::String(text) => text.as_bytes().to_vec(),
			serde_json::Value::Array(items) => items
				.iter()
				.filter_map(serde_json::Value::as_u64)
				.filter_map(|byte| u8::try_from(byte).ok())
				.collect(),
			_ => return None,
		};
		Some((sequence, bytes))
	}

	/// Decodes a frame's bytes, carrying an incomplete trailing sequence to
	/// the next frame instead of replacing it.
	fn decode(&mut self, bytes: &[u8]) -> String {
		let mut buffer = std::mem::take(&mut self.carry);
		buffer.extend_from_slice(bytes);
		let text = match std::str::from_utf8(&buffer) {
			Ok(text) => text.to_owned(),
			Err(error) if error.error_len().is_none() => {
				let valid = error.valid_up_to();
				let text = String::from_utf8_lossy(&buffer[..valid]).into_owned();
				buffer.drain(..valid);
				self.carry = buffer;
				return text;
			},
			Err(_) => String::from_utf8_lossy(&buffer).into_owned(),
		};
		buffer.clear();
		self.carry = buffer;
		text
	}

	/// Appends one frame in order; returns the update with its bytes
	/// removed so they are not journaled twice.
	fn push(
		&mut self,
		session: &mut Session,
		call: EntryId,
		mut value: serde_json::Value,
		sequence: u64,
		bytes: &[u8],
	) -> Result<Box<RawValue>, DispatchError> {
		if self.last.is_none_or(|last| sequence > last) {
			self.last = Some(sequence);
			let sid = match self.sid {
				Some(sid) => sid,
				None => {
					let sid = session.stream_open(result_handle(session, call)?, PropId::Text.into())?;
					self.sid = Some(sid);
					sid
				},
			};
			let text = self.decode(bytes);
			if !text.is_empty() {
				session.stream_append(sid, &text)?;
			}
		}
		if let Some(data) = value.get_mut("data") {
			*data = match data {
				serde_json::Value::String(_) => serde_json::Value::String(String::new()),
				_ => serde_json::Value::Array(Vec::new()),
			};
		}
		Ok(serde_json::value::to_raw_value(&value)?)
	}

	/// Closes the stream, flushing any dangling partial sequence lossily.
	fn close(&mut self, session: &mut Session) -> Result<(), DispatchError> {
		let Some(sid) = self.sid.take() else {
			return Ok(());
		};
		if !self.carry.is_empty() {
			let tail = String::from_utf8_lossy(&self.carry).into_owned();
			self.carry.clear();
			session.stream_append(sid, &tail)?;
		}
		session.stream_close(sid)?;
		Ok(())
	}
}

/// The `<result>` element of a live call.
fn result_handle(session: &Session, call: EntryId) -> Result<Handle, DispatchError> {
	let element = session.call_handle(call)?;
	let dom = session.dom();
	dom.children(element)
		.iter()
		.copied()
		.find(|child| {
			dom.get(*child)
				.is_some_and(|node| node.tag == Tag::Known(KnownTag::Result))
		})
		.ok_or(DispatchError::Session(SessionError::UnknownCall { id: call }))
}

/// Durable result of one dispatched invocation.
#[derive(Clone, Debug)]
pub struct DispatchReport {
	/// Whether the model-facing terminal is an error.
	pub is_error:      bool,
	/// Complete-output artifact created by central bounding.
	pub spilled:       Option<BlobRef>,
	/// Number of individual lines clamped.
	pub lines_clamped: u64,
	/// Job reference when execution detached.
	pub detached:      Option<JobRef>,
}

/// Registry dispatch, projection, persistence, or journal failure.
#[derive(Debug, Error)]
pub enum DispatchError {
	/// Tool registry operation failed.
	#[error(transparent)]
	Registry(#[from] RegistryError),
	/// Session journal or DOM fold failed.
	#[error(transparent)]
	Session(#[from] SessionError),
	/// Blob persistence failed.
	#[error(transparent)]
	Blob(#[from] omp_journal::blob::Error),
	/// JSON serialization failed.
	#[error(transparent)]
	Json(#[from] serde_json::Error),
	/// Tool event task failed independently of its terminal stream.
	#[error(transparent)]
	Join(#[from] JoinError),
	/// Invocation input was dropped before the executor received commitment.
	#[error("tool invocation input channel closed before commitment")]
	InputClosed,
	/// Session-owned tool failed before producing a terminal outcome.
	#[error(transparent)]
	SessionTool(#[from] SessionToolError),
	/// No host executor was injected for an externally routed tool.
	#[error("externally routed tool {name} has no host executor")]
	ExternalExecutorMissing {
		/// Selected tool name.
		name: Str,
	},
	/// A model-facing JSON part contained invalid UTF-8.
	#[error("tool JSON projection is not UTF-8")]
	ProjectionUtf8 {
		/// UTF-8 validation failure.
		#[source]
		source: std::str::Utf8Error,
	},
}

/// Executes registry calls and commits every event through `omp-session`.
#[derive(Clone)]
pub struct Dispatcher {
	registry:      Arc<Registry>,
	policy:        DispatchPolicy,
	events:        KernelEvents,
	external:      Option<Arc<dyn ExternalToolExecutor>>,
	session_tools: FastHashMap<Str, Arc<dyn SessionTool>>,
	jobs:          Arc<JobBoard>,
	authority:     Option<Arc<dyn SessionAuthority>>,
}

impl Dispatcher {
	/// Creates a dispatcher over one runtime registry and central policy.
	#[must_use]
	pub fn new(registry: Arc<Registry>, policy: DispatchPolicy) -> Self {
		Self {
			registry,
			policy,
			events: KernelEvents::default(),
			external: None,
			session_tools: FastHashMap::default(),
			jobs: Arc::new(JobBoard::new()),
			authority: None,
		}
	}

	/// Injects the host adapter for worker- and remote-routed tools.
	#[must_use]
	pub fn with_external_executor(mut self, executor: Arc<dyn ExternalToolExecutor>) -> Self {
		self.external = Some(executor);
		self
	}

	/// Registers a session-authority tool before registry route lookup.
	#[must_use]
	pub fn with_session_tool(mut self, tool: Arc<dyn SessionTool>) -> Self {
		self.session_tools.insert(tool.spec().name.clone(), tool);
		self
	}

	/// Uses the supplied runtime job index for session tools and rewind work.
	#[must_use]
	pub fn with_job_board(mut self, jobs: Arc<JobBoard>) -> Self {
		self.jobs = jobs;
		self
	}

	/// Injects the host-owned live-session routing authority.
	#[must_use]
	pub fn with_session_authority(mut self, authority: Arc<dyn SessionAuthority>) -> Self {
		self.authority = Some(authority);
		self
	}

	pub(crate) fn with_events(mut self, events: KernelEvents) -> Self {
		self.events = events;
		self
	}

	/// Borrows the runtime registry.
	#[must_use]
	pub fn registry(&self) -> &Arc<Registry> {
		&self.registry
	}

	/// Borrows the central dispatch policy.
	#[must_use]
	pub const fn policy(&self) -> &DispatchPolicy {
		&self.policy
	}

	/// Drives one authorized call to exactly one journaled terminal.
	pub async fn dispatch(
		&self,
		session: &mut Session,
		request: DispatchRequest,
	) -> Result<DispatchReport, DispatchError> {
		let raw = Str::new(request.args.get());
		if let Some(tool) = self.session_tools.get(&request.identity.name).cloned() {
			self.jobs.rebuild(session);
			let call = session.call_handle(request.call)?;
			let cancel =
				BackgroundToolCancellation::from_token(request.cancellation.interrupt_token());
			let outcome = tool
				.call(
					SessionToolCx {
						session,
						call,
						jobs: &self.jobs,
						cancel,
						authority: self.authority.as_deref(),
					},
					request.args.clone(),
				)
				.await?;
			let is_error = matches!(
				outcome,
				CallOutcome::Faulted(_) | CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }
			);
			let parts = match &outcome {
				CallOutcome::Ok(payload) | CallOutcome::Faulted(payload) => {
					vec![Part::Json { json: bytes::Bytes::copy_from_slice(payload.get().as_bytes()) }]
				},
				CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. } => Vec::new(),
			};
			let outcome = serde_json::value::to_raw_value(&outcome)?;
			let mut output = OutputStream::default();
			return self.finish_external(session, &request, outcome, parts, is_error, &mut output);
		}
		let mut output = OutputStream::default();
		let interrupt = request.cancellation.interrupt_token();
		if interrupt.is_cancelled() {
			// A stop already requested never starts new work.
			return self.commit_abort(
				session,
				&request,
				Abort::Interrupted { reason: Str::new_static("tool execution cancelled") },
				&mut output,
			);
		}
		let (feed, params) = IncomingParams::channel();
		feed
			.arg_text(raw.clone())
			.map_err(|_| DispatchError::InputClosed)?;
		feed
			.args_committed(raw)
			.map_err(|_| DispatchError::InputClosed)?;
		let (event_tx, event_rx) = flume::unbounded();
		let registry = Arc::clone(&self.registry);
		let name = request.identity.name.clone();
		let route = registry.route(name.as_str())?;
		let external = match route {
			ToolRoute::Native => None,
			_ => Some(
				self
					.external
					.clone()
					.ok_or_else(|| DispatchError::ExternalExecutorMissing { name: name.clone() })?,
			),
		};
		let external_request = ExternalDispatchRequest {
			identity: request.identity.clone(),
			call_id: request.call_id.clone(),
			args: request.args.clone(),
			route,
			blocking_limit: self.policy.blocking_limit,
			cancellation: interrupt.clone(),
		};
		let mut task = tokio::spawn(async move {
			if let Some(external) = external {
				let mut stream = external.invoke(external_request);
				while let Some(event) = stream.next().await {
					if event_tx.send(DispatchEvent::External(event)).is_err() {
						break;
					}
				}
			} else {
				let mut stream = registry.invoke(name.as_str(), params)?;
				while let Some(event) = stream.next().await {
					if event_tx.send(DispatchEvent::Native(event)).is_err() {
						break;
					}
				}
			}
			Ok::<_, RegistryError>(())
		});
		let deadline = tokio::time::sleep(self.policy.blocking_limit);
		tokio::pin!(deadline);
		// ADR 0011 ladder: the stop request asks the unit to settle
		// cooperatively (an in-process tool sees the interrupt on its feed; an
		// external unit sees its cancellation token), the grace bounds how
		// long its own verdict may take, and expiry forces termination
		// recorded as uncertainty rather than a missing event.
		let grace = tokio::time::sleep(self.policy.interrupt_grace);
		tokio::pin!(grace);
		let mut interrupting = false;
		let mut closed = false;
		loop {
			tokio::select! {
				biased;
				() = interrupt.cancelled(), if !interrupting => {
					interrupting = true;
					grace.as_mut().reset(tokio::time::Instant::now() + self.policy.interrupt_grace);
					let _ = feed.interrupt(Interrupt {
						class: Str::new_static(Interrupt::ESCAPE),
						reason: Str::new_static("tool execution cancelled"),
					});
				},
				() = &mut grace, if interrupting => {
					task.abort();
					let _ = task.await;
					return self.commit_abort(session, &request, Abort::EffectsUnknown {
						reason: Str::new_static(
							"tool execution cancelled; the call did not settle within the interrupt grace and was terminated",
						),
					}, &mut output);
				},
				() = &mut deadline, if !interrupting => {
					let job = timeout_job(&request.identity);
					let outcome = detached_outcome(&job)?;
					let prompt = vec![Part::Text {
						text: sf!("detached job {}", job.id),
					}];
					self.commit_terminal(session, &request, outcome, prompt, false, &mut output)?;
					return Ok(DispatchReport {
						is_error: false,
						spilled: None,
						lines_clamped: 0,
						detached: Some(job),
					});
				},
				event = recv_event(&event_rx), if !closed => {
					match event {
						Some(DispatchEvent::Native(Ok(ErasedEv::Update(update)))) => {
							let update = RawValue::from_string(String::from_utf8(update.to_vec())
								.map_err(|source| serde_json::Error::io(std::io::Error::new(
									std::io::ErrorKind::InvalidData,
									source,
								)))?)?;
							self.commit_update(session, &request, update, &mut output)?;
						},
						Some(DispatchEvent::Native(Ok(ErasedEv::Done(outcome)))) => {
							let report = self.finish(session, &request, outcome, &mut output)?;
							let _ = task.await?;
							return Ok(report);
						},
						Some(DispatchEvent::Native(Err(error))) => {
							task.abort();
							let _ = task.await;
							return Err(error.into());
						},
						Some(DispatchEvent::External(ExternalDispatchEvent::Update(update))) => {
							self.commit_update(session, &request, update, &mut output)?;
						},
						Some(DispatchEvent::External(ExternalDispatchEvent::Done {
							outcome,
							parts,
							is_error,
						})) => {
							let report = self.finish_external(
								session, &request, outcome, parts, is_error, &mut output,
							)?;
							let _ = task.await?;
							return Ok(report);
						},
						Some(DispatchEvent::External(ExternalDispatchEvent::Aborted(abort))) => {
							let report = self.commit_abort(session, &request, abort, &mut output)?;
							let _ = task.await?;
							return Ok(report);
						},
						None => closed = true,
					}
				},
				joined = &mut task => {
					joined??;
					while let Ok(event) = event_rx.try_recv() {
						match event {
							DispatchEvent::Native(Ok(ErasedEv::Update(update))) => {
								let update = RawValue::from_string(
									String::from_utf8_lossy(&update).into_owned(),
								)?;
								self.commit_update(session, &request, update, &mut output)?;
							},
							DispatchEvent::Native(Ok(ErasedEv::Done(outcome))) => {
								return self.finish(session, &request, outcome, &mut output);
							},
							DispatchEvent::Native(Err(error)) => return Err(error.into()),
							DispatchEvent::External(ExternalDispatchEvent::Update(update)) => {
								self.commit_update(session, &request, update, &mut output)?;
							},
							DispatchEvent::External(ExternalDispatchEvent::Done {
								outcome,
								parts,
								is_error,
							}) => {
								return self.finish_external(
									session,
									&request,
									outcome,
									parts,
									is_error,
									&mut output,
								);
							},
							DispatchEvent::External(ExternalDispatchEvent::Aborted(abort)) => {
								return self.commit_abort(session, &request, abort, &mut output);
							},
						}
					}
					return self.commit_abort(session, &request, Abort::MissingOutcome, &mut output);
				},
			}
		}
	}

	fn finish(
		&self,
		session: &mut Session,
		request: &DispatchRequest,
		outcome: ErasedOutcome,
		output: &mut OutputStream,
	) -> Result<DispatchReport, DispatchError> {
		match outcome {
			ErasedOutcome::Detached(job) => {
				let raw = detached_outcome(&job)?;
				self.commit_terminal(
					session,
					request,
					raw,
					vec![Part::Text { text: sf!("detached job {}", job.id) }],
					false,
					output,
				)?;
				Ok(DispatchReport {
					is_error:      false,
					spilled:       None,
					lines_clamped: 0,
					detached:      Some(job),
				})
			},
			ErasedOutcome::Done { verdict, useless } => {
				let caps = PromptCaps::for_tool(
					CapsBase {
						maximum_parts:      u16::MAX,
						maximum_text_bytes: u32::MAX,
						media:              true,
						model_class:        ModelClass::Standard,
					},
					&request.identity.rev,
				);
				let projected =
					self
						.registry
						.project_verdict(&request.identity, &verdict, useless, &caps)?;
				let bounded = bound_parts(&projected.parts, request.options, &self.policy)?;
				if let Some(artifact) = bounded.spilled {
					let diag = serde_json::value::to_raw_value(&serde_json::json!({
						"diag": {
							"kind": "truncated",
							"artifact": artifact_address(&artifact),
							"lines_clamped": bounded.lines_clamped,
						}
					}))?;
					self.commit_update(session, request, diag, output)?;
				}
				let raw =
					RawValue::from_string(String::from_utf8(verdict.to_vec()).map_err(|source| {
						serde_json::Error::io(std::io::Error::new(
							std::io::ErrorKind::InvalidData,
							source,
						))
					})?)?;
				self.commit_terminal(
					session,
					request,
					raw,
					bounded.parts,
					projected.is_error,
					output,
				)?;
				Ok(DispatchReport {
					is_error:      projected.is_error,
					spilled:       bounded.spilled,
					lines_clamped: bounded.lines_clamped,
					detached:      None,
				})
			},
		}
	}

	fn finish_external(
		&self,
		session: &mut Session,
		request: &DispatchRequest,
		outcome: Box<RawValue>,
		parts: Vec<Part>,
		is_error: bool,
		output: &mut OutputStream,
	) -> Result<DispatchReport, DispatchError> {
		let bounded = bound_parts(&parts, request.options, &self.policy)?;
		if let Some(artifact) = bounded.spilled {
			let diag = serde_json::value::to_raw_value(&serde_json::json!({
				"diag": {
					"kind": "truncated",
					"artifact": artifact_address(&artifact),
					"lines_clamped": bounded.lines_clamped,
				}
			}))?;
			self.commit_update(session, request, diag, output)?;
		}
		self.commit_terminal(session, request, outcome, bounded.parts, is_error, output)?;
		Ok(DispatchReport {
			is_error,
			spilled: bounded.spilled,
			lines_clamped: bounded.lines_clamped,
			detached: None,
		})
	}

	fn commit_abort(
		&self,
		session: &mut Session,
		request: &DispatchRequest,
		abort: Abort,
		output: &mut OutputStream,
	) -> Result<DispatchReport, DispatchError> {
		// An abort is harness-owned: its projection never depends on the tool
		// or its route, so external units settle exactly like native ones.
		let parts = vec![Part::Text { text: abort.render() }];
		let outcome = serde_json::value::to_raw_value(&CallOutcome::<
			serde_json::Value,
			serde_json::Value,
		>::aborted(abort))?;
		self.finish_external(session, request, outcome, parts, true, output)
	}

	fn commit_update(
		&self,
		session: &mut Session,
		request: &DispatchRequest,
		update: Box<RawValue>,
		output: &mut OutputStream,
	) -> Result<(), DispatchError> {
		let value: serde_json::Value = serde_json::from_str(update.get())?;
		let update = match OutputStream::frame(&value) {
			Some((sequence, bytes)) => output.push(session, request.call, value, sequence, &bytes)?,
			None => update,
		};
		session.call_update(request.call, update)?;
		self
			.events
			.publish(KernelEvent::ToolUpdate { call_id: request.call_id.clone() });
		Ok(())
	}

	fn commit_terminal(
		&self,
		session: &mut Session,
		request: &DispatchRequest,
		outcome: Box<RawValue>,
		parts: Vec<Part>,
		is_error: bool,
		output: &mut OutputStream,
	) -> Result<(), DispatchError> {
		output.close(session)?;
		let parts = serde_json::value::to_raw_value(&parts)?;
		if is_error {
			session.fail_projected(request.call, outcome, parts)?;
		} else {
			session.settle_projected(request.call, outcome, parts)?;
		}
		Ok(())
	}
}

enum DispatchEvent {
	Native(Result<ErasedEv, RegistryError>),
	External(ExternalDispatchEvent),
}

async fn recv_event(receiver: &Receiver<DispatchEvent>) -> Option<DispatchEvent> {
	receiver.recv_async().await.ok()
}

fn detached_outcome(job: &JobRef) -> Result<Box<RawValue>, serde_json::Error> {
	serde_json::value::to_raw_value(&serde_json::json!({
		"kind": "detached",
		"id": job.id,
		"job": job,
	}))
}

fn timeout_job(identity: &ToolIdentity) -> JobRef {
	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64;
	let id = Str::new(omp_core::Ulid::generate().to_string());
	JobRef {
		id:       id.clone(),
		owner:    JobOwner::AgentLoop { agent_id: Str::new_static("kernel") },
		metadata: Arc::new(JobMetadata::running(JobKind::Shell, identity.name.clone(), now)),
		artifact: ExpectedArtifact {
			description: sf!("detached {} output", identity.name),
			media_type:  Some(Str::new_static("application/json")),
			lifetime:    ArtifactLifetime::Session,
		},
	}
}

struct BoundedParts {
	parts:         Vec<Part>,
	spilled:       Option<BlobRef>,
	lines_clamped: u64,
}

fn bound_parts(
	parts: &[Part],
	options: DispatchOptions,
	policy: &DispatchPolicy,
) -> Result<BoundedParts, DispatchError> {
	if options.notrunc {
		return Ok(BoundedParts {
			parts:         parts.to_vec(),
			spilled:       None,
			lines_clamped: 0,
		});
	}
	let mut output = Vec::with_capacity(parts.len());
	let mut full = String::new();
	let mut shown_bytes = 0;
	let mut lines_clamped = 0;
	let mut changed = false;
	for part in parts {
		match part {
			Part::Text { text } => {
				full.push_str(text.as_str());
				let (line_bounded, count) = clamp_lines(text.as_str(), policy.max_line_bytes);
				lines_clamped += count;
				changed |= count != 0;
				let available = policy.max_output_bytes.saturating_sub(shown_bytes);
				let visible = utf8_prefix(&line_bounded, available);
				shown_bytes += visible.len();
				changed |= visible.len() != line_bounded.len();
				output.push(Part::Text { text: Str::new(visible) });
			},
			Part::Json { json } => {
				let text = std::str::from_utf8(json)
					.map_err(|source| DispatchError::ProjectionUtf8 { source })?;
				full.push_str(text);
				let (line_bounded, count) = clamp_lines(text, policy.max_line_bytes);
				lines_clamped += count;
				changed |= count != 0;
				let available = policy.max_output_bytes.saturating_sub(shown_bytes);
				let visible = utf8_prefix(&line_bounded, available);
				shown_bytes += visible.len();
				changed |= visible.len() != line_bounded.len();
				output.push(Part::Text { text: Str::new(visible) });
			},
			Part::Blob { blob, alt } => {
				if let Some(alt) = alt {
					full.push_str(alt.as_str());
				}
				output.push(Part::Blob { blob: blob.clone(), alt: alt.clone() });
			},
		}
	}
	let spilled = changed
		.then(|| policy.spill.put(full.as_bytes()))
		.transpose()?;
	if let Some(artifact) = spilled {
		output.push(Part::Text { text: artifact_address(&artifact) });
	}
	Ok(BoundedParts { parts: output, spilled, lines_clamped })
}

fn clamp_lines(text: &str, maximum: usize) -> (String, u64) {
	let mut output = String::with_capacity(text.len().min(maximum.saturating_mul(2)));
	let mut line_bytes: usize = 0;
	let mut eliding = false;
	let mut clamped = 0;
	for character in text.chars() {
		if character == '\n' {
			output.push(character);
			line_bytes = 0;
			eliding = false;
			continue;
		}
		if eliding {
			continue;
		}
		if line_bytes.saturating_add(character.len_utf8()) > maximum {
			output.push('…');
			eliding = true;
			clamped += 1;
			continue;
		}
		output.push(character);
		line_bytes += character.len_utf8();
	}
	(output, clamped)
}

fn utf8_prefix(text: &str, maximum: usize) -> &str {
	if text.len() <= maximum {
		return text;
	}
	let mut end = maximum;
	while !text.is_char_boundary(end) {
		end -= 1;
	}
	&text[..end]
}

fn artifact_address(blob: &BlobRef) -> Str {
	sf!("artifact://sha256/{}", blob.to_hex())
}

/// Converts a semantic revision into the journal's numeric revision field.
#[must_use]
pub fn journal_revision(rev: &Rev) -> u32 {
	u32::from(rev.n)
}
