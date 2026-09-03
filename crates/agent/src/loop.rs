//! Journal-first agent turn kernel.

use std::{sync::Arc, time::Instant};

use futures::StreamExt as _;
use omp_core::{FastHashMap, Str};
use omp_dom::{Handle, KnownTag, PropId, Tag, Txn};
use omp_inference::{
	ArtifactBody, BlockKind, ChatEvent, ChatRequest, ChatStream, Client, Completion, FinishReason,
	Message as InferenceMessage, NegotiationPolicy, Planner, SafetySetting, Sampling, Setting,
	Usage,
};
use omp_journal::{EntryId, blob::BlobRef, data::TurnReceipt};
use omp_proto::thread::v1::{Item, Message, Part as ThreadPart, Role, item, part};
use omp_session::{Session, SessionError, project_thread};
use omp_tool::{LoweringCaps, Registry, RegistryError, ToolIdentity};
use serde_json::value::RawValue;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tower::Service;

use crate::{
	CancelTree, Director as _, DirectorCx, DirectorError, DirectorRegistry, DirectorStack, DispatchError,
	DispatchOptions, DispatchPolicy, DispatchRequest, Dispatcher, ExternalToolExecutor, KernelEvent,
	LiveComponent, LiveComponentError, LoopDecision, MutDirectorCx, Prepared, RouteFacts,
	SessionTool, ToolCancellation, TurnView, Up,
	directors::compaction::CompactionDirector,
	steering::{
		EMPTY_OUTPUT_RETRY_CAP, append_empty_output_cap_notice, append_empty_output_retry,
		append_error_notice, append_interrupt_notice, append_notice, append_steering,
	},
};

/// Pure system-prompt projection from the authoritative session tree.
pub trait PromptSource: Send + Sync {
	/// Projects ordered system items without retaining parallel session state.
	///
	/// A failure (a template that cannot render from the journal-derived
	/// facts) ends the turn before inference and is journaled as a
	/// `<notice kind=error>` by the kernel rather than aborting the host.
	fn system_items(&self, dom: &omp_dom::Dom) -> Result<Vec<Item>, crate::PromptError>;
}

/// Fixed system prompt useful for tests and small embeddings.
#[derive(Clone, Debug)]
pub struct StaticPrompt(pub Str);

impl PromptSource for StaticPrompt {
	fn system_items(&self, _dom: &omp_dom::Dom) -> Result<Vec<Item>, crate::PromptError> {
		Ok(vec![Item {
			kind: Some(item::Kind::Message(Message {
				role: Role::System as i32,
				parts: vec![ThreadPart { kind: Some(part::Kind::Text(self.0.as_str().to_owned())) }],
				..Default::default()
			})),
			..Default::default()
		}])
	}
}

/// Minimal inference capability required by the agent kernel.
pub trait Inference: Send {
	/// Starts one canonical streaming chat operation.
	fn chat(
		&mut self,
		request: ChatRequest,
	) -> impl Future<Output = Result<ChatStream, omp_inference::Error>> + Send;

	/// Installs the observer that receives same-route retry notices for
	/// every subsequent chat. Inference stacks without a retry layer keep the
	/// default no-op.
	fn install_retry_sink(&mut self, sink: omp_inference::RetrySink) {
		let _ = sink;
	}
}

impl<S, P> Inference for Client<S, P>
where
	S: Service<
			omp_inference::call::Call,
			Response = omp_inference::Answer,
			Error = omp_inference::Error,
		> + Send,
	S::Future: Send,
	P: Planner + Send,
{
	fn chat(
		&mut self,
		request: ChatRequest,
	) -> impl Future<Output = Result<ChatStream, omp_inference::Error>> + Send {
		self.execute(request)
	}

	fn install_retry_sink(&mut self, sink: omp_inference::RetrySink) {
		let mut meta = self.call_meta().clone();
		meta.response_hooks = meta.response_hooks.with_retry_sink(sink);
		self.set_call_meta(meta);
	}
}

/// User input that begins one explicit session turn.
pub struct TurnInput {
	/// User-authored text.
	pub text:        Str,
	/// Content-addressed attachments.
	pub attachments: Vec<BlobRef>,
}

/// Why the kernel returned control to its caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnStop {
	/// The candidate yield passed the Director stack.
	Completed,
	/// Turn or session cancellation was observed.
	Cancelled,
	/// Steering was consumed at a safe point before yielding.
	Steered,
	/// The turn ended in a journaled error notice (only reported through
	/// [`KernelEvent::TurnEnded`]; `run_turn` returns the error itself).
	Failed,
}

/// Durable summary of one explicit turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnOutcome {
	/// Terminal control reason.
	pub stop:           TurnStop,
	/// Visible assistant text accumulated across tool continuations.
	pub assistant_text: Str,
	/// Total input tokens across inference attempts.
	pub tokens_in:      u64,
	/// Total output tokens across inference attempts.
	pub tokens_out:     u64,
}

/// Caller-owned cancellation and optional deadline for one turn.
#[derive(Clone, Debug)]
pub struct RunControl {
	cancellation: CancellationToken,
	deadline:     Option<Instant>,
}

impl RunControl {
	/// Creates turn control from an external cancellation token and deadline.
	#[must_use]
	pub const fn new(cancellation: CancellationToken, deadline: Option<Instant>) -> Self {
		Self { cancellation, deadline }
	}

	/// Reports whether cancellation or the deadline has already fired.
	#[must_use]
	pub fn is_expired(&self) -> bool {
		self.cancellation.is_cancelled()
			|| self
				.deadline
				.is_some_and(|deadline| Instant::now() >= deadline)
	}

	async fn cancelled(&self) {
		if let Some(deadline) = self.deadline {
			tokio::select! {
				() = self.cancellation.cancelled() => {},
				() = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {},
			}
		} else {
			self.cancellation.cancelled().await;
		}
	}
}

impl Default for RunControl {
	fn default() -> Self {
		Self::new(CancellationToken::new(), None)
	}
}

/// Turn-loop construction, inference, dispatch, or session failure.
#[derive(Debug, Error)]
pub enum KernelError {
	/// Session journal or DOM fold failed.
	#[error(transparent)]
	Session(#[from] SessionError),
	/// Inference planning or streaming failed.
	#[error(transparent)]
	Inference(#[from] omp_inference::Error),
	/// Tool registry operation failed.
	#[error(transparent)]
	Registry(#[from] RegistryError),
	/// Canonical thread projection failed.
	#[error(transparent)]
	ThreadProjection(#[from] omp_inference::ThreadProjectionError),
	/// Blob persistence failed.
	#[error(transparent)]
	Blob(#[from] omp_journal::blob::Error),
	/// Tool dispatch failed.
	#[error(transparent)]
	Dispatch(#[from] DispatchError),
	/// Director reconstruction or execution failed.
	#[error(transparent)]
	Director(#[from] DirectorError),
	/// JSON serialization failed.
	#[error(transparent)]
	Json(#[from] serde_json::Error),
	/// An inference stream emitted output before response metadata.
	#[error("inference output arrived before response metadata")]
	MissingResponseStart,
	/// A tool argument block did not contain UTF-8 JSON text.
	#[error("tool argument delta is not UTF-8")]
	ToolArgumentUtf8 {
		/// UTF-8 validation failure.
		#[source]
		source: std::str::Utf8Error,
	},
	/// A ready tool call conflicts with its streamed call identity.
	#[error("ready tool call does not match its streamed call")]
	ToolCallMismatch,
	/// A live extension Component reducer failed.
	#[error(transparent)]
	LiveComponent(#[from] LiveComponentError),
	/// The system prompt could not be projected from the session tree.
	#[error("system prompt projection failed")]
	Prompt(#[source] crate::PromptError),
}

/// Agent kernel composed from inference, tool, prompt, and Director registries.
pub struct Kernel<C> {
	client:            C,
	dispatcher:        Dispatcher,
	cancel:            CancelTree,
	director_registry: DirectorRegistry,
	live_components:   Vec<Box<dyn LiveComponent>>,
	events:            crate::events::KernelEvents,
	prompt:            Arc<dyn PromptSource>,
	route:             RouteFacts,
	mailbox_tx:        flume::Sender<Up>,
	mailbox_rx:        flume::Receiver<Up>,
}

impl<C> Kernel<C> {
	/// Constructs a kernel with the standard Director registry.
	#[must_use]
	pub fn new(
		mut client: C,
		registry: Arc<Registry>,
		policy: DispatchPolicy,
		prompt: impl PromptSource + 'static,
	) -> Self
	where
		C: Inference,
	{
		let (mailbox_tx, mailbox_rx) = flume::unbounded();
		let events = crate::events::KernelEvents::default();
		let retry_events = events.clone();
		client.install_retry_sink(Arc::new(move |notice: omp_inference::RetryNotice| {
			retry_events.publish(KernelEvent::InferenceRetry {
				attempt:      notice.attempt,
				max_attempts: notice.max_attempts,
				delay:        notice.delay,
				reason:       notice.message,
			});
		}));
		Self {
			client,
			dispatcher: Dispatcher::new(registry, policy).with_events(events.clone()),
			cancel: CancelTree::new(),
			director_registry: DirectorRegistry::standard(),
			live_components: Vec::new(),
			events,
			prompt: Arc::new(prompt),
			route: RouteFacts::default(),
			mailbox_tx,
			mailbox_rx,
		}
	}

	/// Replaces the Director registry assembled by the host.
	#[must_use]
	pub fn with_director_registry(mut self, registry: DirectorRegistry) -> Self {
		self.director_registry = registry;
		self
	}

	/// Registers a live extension Component reducer.
	pub fn register_live_component(&mut self, component: Box<dyn LiveComponent>) {
		self.live_components.push(component);
	}

	/// Replaces catalog-derived facts for the selected route.
	#[must_use]
	pub const fn with_route_facts(mut self, route: RouteFacts) -> Self {
		self.route = route;
		self
	}

	/// Injects execution for worker- and remote-routed tools.
	#[must_use]
	pub fn with_external_executor(mut self, executor: Arc<dyn ExternalToolExecutor>) -> Self {
		self.dispatcher = self.dispatcher.with_external_executor(executor);
		self
	}

	/// Registers a host-authority tool that operates on the session DOM.
	#[must_use]
	pub fn with_session_tool(mut self, tool: Arc<dyn SessionTool>) -> Self {
		self.dispatcher = self.dispatcher.with_session_tool(tool);
		self
	}

	/// Injects the host-owned live-session routing authority.
	#[must_use]
	pub fn with_session_authority(mut self, authority: Arc<dyn crate::SessionAuthority>) -> Self {
		self.dispatcher = self.dispatcher.with_session_authority(authority);
		self
	}

	/// Borrows the composed inference owner.
	#[must_use]
	pub const fn inference(&self) -> &C {
		&self.client
	}

	/// Borrows the composed runtime tool registry.
	#[must_use]
	pub fn tool_registry(&self) -> &Arc<Registry> {
		self.dispatcher.registry()
	}

	/// Returns the one upward control mailbox.
	#[must_use]
	pub fn mailbox(&self) -> flume::Sender<Up> {
		self.mailbox_tx.clone()
	}

	/// Subscribes to lossless observer notifications for subsequent journaled
	/// progress.
	pub fn subscribe(&mut self) -> flume::Receiver<KernelEvent> {
		self.events.subscribe()
	}

	/// Cancels the owning session and every active or future tool scope.
	pub fn cancel_session(&self) {
		self.cancel.cancel_session();
	}

	fn apply_live_components(&mut self, session: &mut Session) -> Result<(), KernelError> {
		let Some(head) = session.head() else {
			return Ok(());
		};
		let Some(entry) = session.entry(head).cloned() else {
			return Ok(());
		};
		let mut patches = Vec::new();
		let mut failed = false;
		for component in &mut self.live_components {
			if !component.interested(&entry.kind) {
				continue;
			}
			match component.reduce(&entry, session.dom()) {
				Ok(ops) if !ops.is_empty() => {
					patches.push((Str::new(component.id()), ops));
				},
				Ok(_) => {},
				Err(error) => {
					tracing::warn!(?error, component = component.id(), "live Component failed");
					failed = true;
				},
			}
		}
		for (id, ops) in patches {
			session.patch(Txn { cause: entry.id, label: Some(Str::new(format!("ext:{id}"))), ops })?;
		}
		if failed && let Ok(turn) = current_turn(session) {
			append_notice(
				session,
				turn,
				Str::new_static("Python extension Component callback failed"),
			)?;
		}
		Ok(())
	}
}

impl<C: Inference> Kernel<C> {
	/// Runs one explicit user turn through inference, tools, steering, and
	/// Directors.
	///
	/// A failure after the turn opened is journaled before it is returned: any
	/// open `<assistant>` is closed with stop reason `error` and the turn gains
	/// a `<notice kind=error>` carrying the full error chain, so a resumed or
	/// rendered session shows why the turn ended and observers never see a
	/// dangling assistant.
	pub async fn run_turn(
		&mut self,
		session: &mut Session,
		input: TurnInput,
		control: RunControl,
	) -> Result<TurnOutcome, KernelError> {
		if control.is_expired() || self.cancel.is_session_cancelled() {
			return Ok(cancelled_outcome());
		}
		let turn_cancel = self.cancel.begin_turn();
		session.begin_turn()?;
		self.apply_live_components(session)?;
		session.user(input.text, input.attachments)?;
		self.apply_live_components(session)?;
		let turn = current_turn(session)?;
		let result = self
			.run_turn_body(session, turn, &turn_cancel, &control)
			.await;
		match &result {
			Err(error) => self.journal_turn_failure(session, turn, error),
			Ok(outcome) if outcome.stop == TurnStop::Cancelled => {
				self.journal_turn_interrupt(session, turn);
			},
			Ok(_) => {},
		}
		self.events.publish(KernelEvent::TurnEnded {
			stop: match &result {
				Ok(outcome) => outcome.stop,
				Err(_) => TurnStop::Failed,
			},
		});
		result
	}

	/// Records an interrupted turn in the tree (ADR 0004: lifecycle derives
	/// from the tree): an open assistant closes with `cancelled` and the turn
	/// ends with `<notice kind=warn>`, never a receipt or a false completion.
	fn journal_turn_interrupt(&mut self, session: &mut Session, turn: Handle) {
		match session.assistant_end("cancelled") {
			Ok(_) => {
				if let Err(error) = self.apply_live_components(session) {
					tracing::warn!(?error, "live Components failed after an assistant interrupt close");
				}
			},
			Err(SessionError::NoActiveAssistant) => {},
			Err(journal) => {
				tracing::warn!(error = ?journal, "failed to close the assistant after an interrupt");
			},
		}
		if let Err(journal) = append_interrupt_notice(session, turn) {
			tracing::warn!(error = ?journal, "failed to journal the turn interrupt notice");
		}
	}

	fn journal_turn_failure(&mut self, session: &mut Session, turn: Handle, error: &KernelError) {
		match session.assistant_end("error") {
			Ok(_) => {
				if let Err(error) = self.apply_live_components(session) {
					tracing::warn!(?error, "live Components failed after an assistant error close");
				}
			},
			Err(SessionError::NoActiveAssistant) => {},
			Err(journal) => {
				tracing::warn!(error = ?journal, "failed to close the assistant after a turn error");
			},
		}
		if let Err(journal) = append_error_notice(session, turn, Str::new(error_chain(error))) {
			tracing::warn!(error = ?journal, "failed to journal the turn error notice");
		}
	}

	async fn run_turn_body(
		&mut self,
		session: &mut Session,
		turn: Handle,
		turn_cancel: &crate::TurnCancellation,
		control: &RunControl,
	) -> Result<TurnOutcome, KernelError> {
		let mut directors = DirectorStack::from_dom(session.dom(), &self.director_registry);
		if !directors.active_ids().contains(&"compaction")
			&& !directors.queued_ids().contains(&"compaction")
		{
			directors.engage(session, Box::new(CompactionDirector::new()))?;
		}
		let mut total_text = String::new();
		let mut tokens_in = 0_u64;
		let mut tokens_out = 0_u64;
		let mut was_steered = false;
		let mut empty_output_retries = 0_u8;
		let route = self.route;

		loop {
			if control.is_expired() || turn_cancel.is_turn_cancelled() {
				turn_cancel.cancel_turn();
				return Ok(outcome(TurnStop::Cancelled, total_text, tokens_in, tokens_out));
			}
			let mut request = self.build_request(session)?;
			let prepared = {
				let mut cx = MutDirectorCx {
					session,
					inference: &mut self.client,
					blobs: &self.dispatcher.policy().spill,
					route: &route,
					turn,
					director: None,
				};
				directors.before_inference(&mut cx, &request).await?
			};
			self.apply_live_components(session)?;
			if prepared == Prepared::Rebuild {
				request = self.build_request(session)?;
				directors = DirectorStack::from_dom(session.dom(), &self.director_registry);
			}
			let director_cx = DirectorCx::new(turn, &route);
			directors.prepare_inference(session.dom(), &director_cx, &mut request);
			let request_started = Instant::now();
			let stream = self.client.chat(request).await?;
			let mut driven = self
				.drive_inference(session, stream, control, turn_cancel, request_started)
				.await?;
			tokens_in = tokens_in.saturating_add(driven.usage.input_tokens);
			tokens_out = tokens_out.saturating_add(driven.usage.output_tokens);
			total_text.push_str(driven.text.as_str());
			if driven.cancelled {
				return Ok(outcome(TurnStop::Cancelled, total_text, tokens_in, tokens_out));
			}
			let mut streamed_steering = std::mem::take(&mut driven.steering);
			for call in driven.calls {
				let cancellation = tool_cancellation(
					self.dispatcher.registry(),
					call.identity.name.as_str(),
					turn_cancel,
				)?;
				let call_id = call.call_id;
				// The mailbox stays live while a tool runs: an interrupt cancels
				// the turn scope the tool observes (pi aborts running tools on
				// ctrl+c) instead of waiting for the tool to finish on its own.
				let mut approvals = Vec::new();
				let report = {
					let dispatch = self.dispatcher.dispatch(session, DispatchRequest {
						identity: call.identity,
						call_id: call_id.clone(),
						call: call.entry,
						options: DispatchOptions::from_args(&call.args),
						args: call.args,
						cancellation,
					});
					tokio::pin!(dispatch);
					loop {
						tokio::select! {
							biased;
							report = &mut dispatch => break report?,
							() = control.cancelled() => turn_cancel.cancel_turn(),
							message = self.mailbox_rx.recv_async() => match message {
								Ok(Up::Interrupt) => turn_cancel.cancel_turn(),
								Ok(Up::Cancel) => {
									self.cancel.cancel_session();
									turn_cancel.cancel_turn();
								},
								Ok(Up::Steer(text)) => streamed_steering.push(text),
								Ok(Up::Unqueue(reply)) => {
									let _ = reply.send(std::mem::take(&mut streamed_steering));
								},
								Ok(Up::Approve { id, decision }) => approvals.push((id, decision)),
								Ok(Up::Env(_)) | Err(_) => {},
							},
						}
					}
				};
				for (id, decision) in approvals {
					let _ = crate::ApprovalBook::new().decide(session, id.as_str(), decision);
				}
				self.apply_live_components(session)?;
				self
					.events
					.publish(KernelEvent::ToolSettled { call_id, is_error: report.is_error });
			}
			let mut steering = self.drain_mailbox(session, turn, turn_cancel);
			steering.items.extend(streamed_steering);
			if steering.cancelled {
				return Ok(outcome(TurnStop::Cancelled, total_text, tokens_in, tokens_out));
			}
			let steering_received = !steering.items.is_empty();
			if steering_received {
				was_steered = true;
				for text in steering.items {
					append_steering(session, turn, text)?;
					self.apply_live_components(session)?;
				}
			}
			let turn_view = TurnView {
				turn,
				had_tool_calls: !driven.call_ids.is_empty(),
				assistant_text: driven.text,
				stop_reason: driven.stop_reason,
			};
			directors.observe_turn(session, &director_cx, &turn_view)?;
			self.apply_live_components(session)?;
			if turn_view.had_tool_calls || steering_received {
				continue;
			}
			if turn_view.assistant_text.is_empty() {
				if empty_output_retries < EMPTY_OUTPUT_RETRY_CAP {
					empty_output_retries = empty_output_retries.saturating_add(1);
					append_empty_output_retry(session, turn, empty_output_retries)?;
					self.apply_live_components(session)?;
					continue;
				}
				append_empty_output_cap_notice(session, turn)?;
				self.apply_live_components(session)?;
				let stop = if was_steered {
					TurnStop::Steered
				} else {
					TurnStop::Completed
				};
				return Ok(outcome(stop, total_text, tokens_in, tokens_out));
			}
			let decision = directors.on_yield(session, &director_cx, &turn_view)?;
			self.apply_live_components(session)?;
			match decision {
				LoopDecision::Continue { .. } => continue,
				LoopDecision::Yield => {
					let stop = if was_steered {
						TurnStop::Steered
					} else {
						TurnStop::Completed
					};
					return Ok(outcome(stop, total_text, tokens_in, tokens_out));
				},
			}
		}
	}

	fn build_request(&self, session: &Session) -> Result<ChatRequest, KernelError> {
		let mut items = self
			.prompt
			.system_items(session.dom())
			.map_err(KernelError::Prompt)?;
		items.extend(project_thread(session.dom()));
		let mut messages = InferenceMessage::from_thread_items(&items)?;
		crate::events::strip_unsigned_reasoning(&mut messages);
		let tools = self
			.dispatcher
			.registry()
			.advertise(LoweringCaps {
				strict_schema:  false,
				grammar:        Default::default(),
				maximum_tools:  None,
				maximum_strict: None,
			})?
			.into_iter()
			.map(|tool| tool.definition)
			.collect::<Vec<_>>();
		Ok(ChatRequest {
			messages:          messages.into(),
			tools:             tools.into(),
			hosted_tools:      Arc::from([]),
			tool_choice:       Setting::Unset,
			output:            Setting::Unset,
			reasoning:         Setting::Unset,
			verbosity:         Setting::Unset,
			cache_retention:   Setting::Unset,
			service_tier:      Setting::Unset,
			sampling:          Sampling::default(),
			max_output_tokens: None,
			top_logprobs:      None,
			safety:            Arc::<[SafetySetting]>::from([]),
			negotiation:       NegotiationPolicy::default(),
		})
	}

	async fn drive_inference(
		&mut self,
		session: &mut Session,
		mut stream: ChatStream,
		control: &RunControl,
		turn_cancel: &crate::TurnCancellation,
		request_started: Instant,
	) -> Result<DrivenInference, KernelError> {
		let mut assistant = None;
		let mut content_streams = FastHashMap::<u32, u32>::default();
		let mut pending = FastHashMap::<u32, StreamingCall>::default();
		let mut ready = Vec::new();
		let mut text = String::new();
		let mut usage = Usage::default();
		let mut stop_reason = Str::new_static("stop");
		let mut call_ids = Vec::new();
		let mut completed = false;
		let mut steering = Vec::new();
		// pi `message.ttft`: first visible or reasoning byte (or the first
		// streamed tool-call fragment) after the request left the kernel.
		let mut first_token: Option<Instant> = None;
		let fold: Result<Fold, KernelError> = async {
			loop {
				let signal = tokio::select! {
					biased;
					() = control.cancelled() => StreamSignal::Cancelled,
					message = self.mailbox_rx.recv_async() => StreamSignal::Control(message.ok()),
					event = stream.next() => StreamSignal::Event(event),
				};
				let event = match signal {
					StreamSignal::Cancelled => {
						turn_cancel.cancel_turn();
						return Ok(Fold::Cancelled);
					},
					StreamSignal::Control(Some(Up::Steer(text))) => {
						steering.push(text);
						continue;
					},
					StreamSignal::Control(Some(Up::Unqueue(reply))) => {
						let _ = reply.send(std::mem::take(&mut steering));
						continue;
					},
					StreamSignal::Control(Some(Up::Interrupt)) => {
						turn_cancel.cancel_turn();
						return Ok(Fold::Cancelled);
					},
					StreamSignal::Control(Some(Up::Cancel)) => {
						self.cancel.cancel_session();
						turn_cancel.cancel_turn();
						return Ok(Fold::Cancelled);
					},
					StreamSignal::Control(Some(Up::Env(_))) => continue,
					StreamSignal::Control(Some(Up::Approve { id, decision })) => {
						let _ = crate::ApprovalBook::new().decide(session, id.as_str(), decision);
						continue;
					},
					StreamSignal::Control(None) => continue,
					StreamSignal::Event(Some(event)) => event?,
					StreamSignal::Event(None) => break Ok(Fold::Ended),
				};
				match event {
					ChatEvent::Started(meta) => {
						let model = meta.model.map_or_else(
							|| Str::new_static("unknown"),
							|value| Str::new(value.to_string()),
						);
						session.assistant_start(
							model,
							Str::new(meta.provider.to_string()),
							Str::new(meta.route.to_string()),
						)?;
						self.apply_live_components(session)?;
						assistant = Some(current_assistant(session)?);
						self.events.publish(KernelEvent::InferenceStarted);
					},
					ChatEvent::BlockStarted { index, kind } => match kind {
						BlockKind::Text => {
							let handle = assistant.ok_or(KernelError::MissingResponseStart)?;
							let sid = session.stream_open(handle, PropId::Text.into())?;
							self.apply_live_components(session)?;
							content_streams.insert(index, sid);
						},
						BlockKind::Thinking => {
							let handle = assistant.ok_or(KernelError::MissingResponseStart)?;
							let sid = session.stream_open(handle, PropId::Thinking.into())?;
							self.apply_live_components(session)?;
							content_streams.insert(index, sid);
						},
						BlockKind::ToolCall | BlockKind::Artifact => {},
					},
					ChatEvent::TextDelta { index, text: delta } => {
						first_token.get_or_insert_with(Instant::now);
						let sid =
							content_sid(session, assistant, &mut content_streams, index, PropId::Text)?;
						session.stream_append(sid, delta.as_str())?;
						self.apply_live_components(session)?;
						self.events.publish(KernelEvent::TextDelta(delta.clone()));
						text.push_str(delta.as_str());
					},
					ChatEvent::ThinkingDelta { index, text: delta } => {
						first_token.get_or_insert_with(Instant::now);
						let sid = content_sid(
							session,
							assistant,
							&mut content_streams,
							index,
							PropId::Thinking,
						)?;
						session.stream_append(sid, delta.as_str())?;
						self.apply_live_components(session)?;
						self.events.publish(KernelEvent::ThinkingDelta(delta));
					},
					ChatEvent::ToolCallStarted { index, id, name } => {
						first_token.get_or_insert_with(Instant::now);
						let identity = self
							.dispatcher
							.registry()
							.resolved_identity(name.as_str())
							.ok_or_else(|| RegistryError::UnknownTool(name.clone()))?;
						let (entry, sid) = session.call_streaming(
							name.clone(),
							crate::journal_revision(&identity.rev),
							Str::new(id.to_string()),
							None,
						)?;
						self.apply_live_components(session)?;
						pending.insert(index, StreamingCall {
							entry,
							sid,
							identity,
							call_id: Str::new(id.to_string()),
						});
					},
					ChatEvent::ToolArgumentsDelta { index, bytes } => {
						let call = pending.get(&index).ok_or(KernelError::ToolCallMismatch)?;
						let fragment = std::str::from_utf8(&bytes)
							.map_err(|source| KernelError::ToolArgumentUtf8 { source })?;
						session.stream_append(call.sid, fragment)?;
						self.apply_live_components(session)?;
					},
					ChatEvent::ToolCallReady { index, call } => {
						let args = serde_json::value::to_raw_value(call.arguments.as_value())?;
						let (entry, identity) = if let Some(streaming) = pending.remove(&index) {
							if streaming.call_id.as_str() != call.id.to_string()
								|| streaming.identity.name != call.name
							{
								return Err(KernelError::ToolCallMismatch);
							}
							session.call_ready(streaming.entry, args.clone())?;
							self.apply_live_components(session)?;
							(streaming.entry, streaming.identity)
						} else {
							let identity = self
								.dispatcher
								.registry()
								.resolved_identity(call.name.as_str())
								.ok_or_else(|| RegistryError::UnknownTool(call.name.clone()))?;
							let intent = call
								.arguments
								.as_value()
								.get("i")
								.and_then(serde_json::Value::as_str)
								.map(Str::new);
							let entry = session.call(
								call.name.clone(),
								crate::journal_revision(&identity.rev),
								Str::new(call.id.to_string()),
								intent,
								Some(args.clone()),
								None,
							)?;
							self.apply_live_components(session)?;
							(entry, identity)
						};
						let call_id = Str::new(call.id.to_string());
						call_ids.push(call_id.clone());
						self.events.publish(KernelEvent::ToolReady {
							call_id: call_id.clone(),
							name:    identity.name.clone(),
						});
						ready.push(ReadyCall { entry, identity, call_id, args });
					},
					ChatEvent::Usage(update) => {
						usage = update.usage;
						self.events.publish(KernelEvent::Usage {
							output_tokens:    usage.output_tokens,
							reasoning_tokens: usage.reasoning_tokens,
						});
					},
					ChatEvent::Completed(completion) => {
						close_streams(session, &mut content_streams)?;
						self.apply_live_components(session)?;
						stop_reason = finish_reason(&completion.reason);
						usage = completion.usage;
						session.assistant_end(stop_reason.clone())?;
						self.apply_live_components(session)?;
						session.receipt(receipt_facts(
							&usage,
							cost_nano_usd(&completion),
							request_started,
							first_token,
						))?;
						self.apply_live_components(session)?;
						completed = true;
						break Ok(Fold::Ended);
					},
					ChatEvent::Artifact { artifact, .. } => {
						let uri = self.artifact_uri(artifact).await?;
						let sid =
							content_sid(session, assistant, &mut content_streams, u32::MAX, PropId::Text)?;
						session.stream_append(sid, uri.as_str())?;
						self.apply_live_components(session)?;
						self.events.publish(KernelEvent::TextDelta(uri.clone()));
						text.push_str(uri.as_str());
					},
					ChatEvent::WorkflowAction(action) => {
						append_notice(
							session,
							current_turn(session)?,
							Str::new(format!("provider workflow action: {}", action.name)),
						)?;
						self.apply_live_components(session)?;
					},
					ChatEvent::WorkflowResume(resume) => {
						append_notice(
							session,
							current_turn(session)?,
							Str::new(format!("provider workflow resumed: {}", resume.workflow_id)),
						)?;
						self.apply_live_components(session)?;
					},
					ChatEvent::WorkflowCancelled { invocation } => {
						append_notice(
							session,
							current_turn(session)?,
							Str::new(format!("provider workflow cancelled: {invocation}")),
						)?;
						self.apply_live_components(session)?;
					},
				}
			}
		}
		.await;
		match fold {
			Ok(Fold::Ended) => {},
			Ok(Fold::Cancelled) => {
				// The reveal stopped mid-stream: close its open text streams so
				// the tree never carries a dangling stream past the interrupt.
				close_streams(session, &mut content_streams)?;
				return Ok(DrivenInference::cancelled(text, usage));
			},
			Err(error) => {
				// The stream failed mid-reveal: close its open text streams so the
				// tree never carries a dangling stream, then surface the failure.
				if let Err(journal) = close_streams(session, &mut content_streams) {
					tracing::warn!(error = ?journal, "failed to close reveal streams after a stream error");
				}
				return Err(error);
			},
		}
		if !completed {
			close_streams(session, &mut content_streams)?;
			self.apply_live_components(session)?;
			session.assistant_end("stream_closed")?;
			self.apply_live_components(session)?;
			session.receipt(receipt_facts(&usage, 0, request_started, first_token))?;
			self.apply_live_components(session)?;
		}
		Ok(DrivenInference {
			text: Str::new(text),
			usage,
			stop_reason,
			calls: ready,
			call_ids,
			steering,
			cancelled: false,
		})
	}

	async fn artifact_uri(&self, artifact: omp_inference::Artifact) -> Result<Str, KernelError> {
		match artifact.body {
			ArtifactBody::Bytes(bytes) => {
				let blob = self.dispatcher.policy().spill.put(&bytes)?;
				Ok(Str::new(format!("artifact://sha256/{}", blob.to_hex())))
			},
			ArtifactBody::Stored(reference) => {
				Ok(Str::new(format!("artifact://{}/{}", reference.store, reference.id)))
			},
			ArtifactBody::Stream(mut stream) => {
				let mut bytes = Vec::new();
				while let Some(chunk) = stream.next().await {
					bytes.extend_from_slice(&chunk?);
				}
				let blob = self.dispatcher.policy().spill.put(&bytes)?;
				Ok(Str::new(format!("artifact://sha256/{}", blob.to_hex())))
			},
		}
	}

	fn drain_mailbox(
		&self,
		session: &mut Session,
		turn_handle: Handle,
		turn: &crate::TurnCancellation,
	) -> DrainedSteering {
		let mut drained = DrainedSteering::default();
		while let Ok(message) = self.mailbox_rx.try_recv() {
			match message {
				Up::Steer(text) => drained.items.push(text),
				Up::Unqueue(reply) => {
					let _ = reply.send(std::mem::take(&mut drained.items));
				},
				Up::Interrupt => {
					turn.cancel_turn();
					drained.cancelled = true;
				},
				Up::Cancel => {
					self.cancel.cancel_session();
					drained.cancelled = true;
				},
				Up::Env(crate::EnvEvent::DeviceAvailability { payload }) => {
					let _ = append_notice(session, turn_handle, payload);
				},
				Up::Env(crate::EnvEvent::StagedPreview { proposal_id, source_tool }) => {
					let _ = append_notice(
						session,
						turn_handle,
						Str::new(format!(
							"Staged proposal {proposal_id} from {source_tool} awaits `dyn resolve` or \
							 `dyn reject`."
						)),
					);
				},
				Up::Env(crate::EnvEvent::CheckpointControl { .. }) => {},
				Up::Approve { id, decision } => {
					let _ = crate::ApprovalBook::new().decide(session, id.as_str(), decision);
				},
			}
		}
		drained
	}

	/// Runs the manual compaction path between turns (`/compact`,
	/// `/handoff`): summarizes the projected history through the
	/// [`CompactionDirector`] and journals a `compaction@1` labeled
	/// `method`. Returns whether a compaction landed (an empty session
	/// projects nothing to summarize and journals nothing).
	pub async fn compact(
		&mut self,
		session: &mut Session,
		focus: Option<Str>,
		method: &'static str,
	) -> Result<bool, KernelError> {
		let Ok(turn) = current_turn(session) else {
			return Ok(false);
		};
		let request = self.build_request(session)?;
		let director = CompactionDirector::manual(focus).with_method(method);
		let route = self.route;
		let prepared = {
			let mut cx = MutDirectorCx {
				session,
				inference: &mut self.client,
				blobs: &self.dispatcher.policy().spill,
				route: &route,
				turn,
				director: None,
			};
			director.before_inference(&mut cx, &request).await?
		};
		self.apply_live_components(session)?;
		Ok(prepared == Prepared::Rebuild)
	}
}

struct StreamingCall {
	entry:    EntryId,
	sid:      u32,
	identity: ToolIdentity,
	call_id:  Str,
}

struct ReadyCall {
	entry:    EntryId,
	identity: ToolIdentity,
	call_id:  Str,
	args:     Box<RawValue>,
}

struct DrivenInference {
	text:        Str,
	usage:       Usage,
	stop_reason: Str,
	calls:       Vec<ReadyCall>,
	call_ids:    Vec<Str>,
	steering:    Vec<Str>,
	cancelled:   bool,
}

impl DrivenInference {
	fn cancelled(text: String, usage: Usage) -> Self {
		Self {
			text: Str::new(text),
			usage,
			stop_reason: Str::new_static("cancelled"),
			calls: Vec::new(),
			call_ids: Vec::new(),
			steering: Vec::new(),
			cancelled: true,
		}
	}
}

enum StreamSignal {
	Event(Option<Result<ChatEvent, omp_inference::Error>>),
	Control(Option<Up>),
	Cancelled,
}

/// How one inference fold left the stream.
enum Fold {
	/// The stream completed or closed on its own.
	Ended,
	/// Caller control ended the stream before completion.
	Cancelled,
}

/// Renders an error with its full `source()` chain, one cause per line.
fn error_chain(error: &dyn std::error::Error) -> String {
	let mut text = error.to_string();
	let mut source = error.source();
	while let Some(cause) = source {
		text.push_str("\n  caused by: ");
		text.push_str(&cause.to_string());
		source = cause.source();
	}
	text
}

#[derive(Default)]
struct DrainedSteering {
	items:     Vec<Str>,
	cancelled: bool,
}

fn content_sid(
	session: &mut Session,
	assistant: Option<Handle>,
	streams: &mut FastHashMap<u32, u32>,
	index: u32,
	prop: PropId,
) -> Result<u32, KernelError> {
	if let Some(sid) = streams.get(&index) {
		return Ok(*sid);
	}
	let sid =
		session.stream_open(assistant.ok_or(KernelError::MissingResponseStart)?, prop.into())?;
	streams.insert(index, sid);
	Ok(sid)
}

fn close_streams(
	session: &mut Session,
	streams: &mut FastHashMap<u32, u32>,
) -> Result<(), SessionError> {
	for (_, sid) in streams.drain() {
		session.stream_close(sid)?;
	}
	Ok(())
}

fn current_turn(session: &Session) -> Result<Handle, KernelError> {
	session
		.dom()
		.children(session.dom().body())
		.last()
		.copied()
		.ok_or(KernelError::MissingResponseStart)
}

fn current_assistant(session: &Session) -> Result<Handle, KernelError> {
	let turn = current_turn(session)?;
	session
		.dom()
		.children(turn)
		.iter()
		.rev()
		.copied()
		.find(|handle| {
			session
				.dom()
				.get(*handle)
				.is_some_and(|node| node.tag == Tag::Known(KnownTag::Assistant))
		})
		.ok_or(KernelError::MissingResponseStart)
}

fn tool_cancellation(
	registry: &Registry,
	name: &str,
	turn: &crate::TurnCancellation,
) -> Result<ToolCancellation, RegistryError> {
	let effects = registry.effects_owned(name)?;
	let mutating = effects
		.documents
		.as_ref()
		.is_some_and(|effects| !effects.write_globs.is_empty())
		|| effects
			.exec
			.as_ref()
			.is_some_and(|effects| !effects.is_empty())
		|| effects
			.inference
			.as_ref()
			.is_some_and(|effects| !effects.is_empty())
		|| effects
			.desktop
			.as_ref()
			.is_some_and(|effects| effects.input)
		|| effects.subagents != 0;
	Ok(if mutating {
		ToolCancellation::Foreground(turn.foreground_mutation())
	} else {
		ToolCancellation::ReadOnly(turn.read_only_tool())
	})
}

fn finish_reason(reason: &FinishReason) -> Str {
	match reason {
		FinishReason::Stop => Str::new_static("stop"),
		FinishReason::Length => Str::new_static("length"),
		FinishReason::ToolCalls => Str::new_static("tool_calls"),
		FinishReason::ContentFilter => Str::new_static("content_filter"),
		FinishReason::Cancelled => Str::new_static("cancelled"),
		FinishReason::Other(reason) => reason.clone(),
	}
}

/// The `turn.receipt@1` payload for one completed inference: provider usage
/// plus the kernel-clock timings pi's usage row shows (TTFT, duration →
/// tok/s).
fn receipt_facts(
	usage: &Usage,
	cost_nano_usd: u64,
	request_started: Instant,
	first_token: Option<Instant>,
) -> TurnReceipt {
	let millis = |elapsed: std::time::Duration| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
	TurnReceipt {
		tokens_in: usage.input_tokens,
		tokens_out: usage.output_tokens,
		cost_nano_usd,
		cache_read: usage.cache_read_tokens,
		cache_write: usage.cache_write_tokens,
		ttft_ms: first_token.map(|at| millis(at.duration_since(request_started))),
		duration_ms: Some(millis(request_started.elapsed())),
	}
}

fn cost_nano_usd(completion: &Completion) -> u64 {
	completion
		.receipt
		.cost
		.micro_usd
		.max(0)
		.saturating_mul(1_000)
		.try_into()
		.unwrap_or(u64::MAX)
}

fn outcome(stop: TurnStop, text: String, tokens_in: u64, tokens_out: u64) -> TurnOutcome {
	TurnOutcome { stop, assistant_text: Str::new(text), tokens_in, tokens_out }
}

fn cancelled_outcome() -> TurnOutcome {
	TurnOutcome {
		stop:           TurnStop::Cancelled,
		assistant_text: Str::new_static(""),
		tokens_in:      0,
		tokens_out:     0,
	}
}
