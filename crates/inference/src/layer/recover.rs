//! Canonical response recovery before semantic output gating.

use std::{
	fmt, mem, str,
	sync::Arc,
	task::{Context, Poll},
	time::SystemTime,
};

use futures::StreamExt;
use omp_catalog::{PriceUnit, pricing::UsageDimensions};
use omp_core::Str;
use tower::{Layer, Service};

use crate::{
	answer::{AnswerBody, ModelDiscoveryPage},
	body::AttemptBodyEvidence,
	call::{Call, DiscoveryRequest, OperationCall, Setting, StructuredOutput, ToolDefinition},
	codec::{HandshakeMeta, HandshakenResponse, RawCompletion, RawEvent, UnvalidatedToolCall},
	error::{Error, ErrorDetail, ErrorKind, ErrorPhase, RetryAction},
	event::{ChatEvent, Completion, FinishReason},
	layer::{ExecutionContext, LayerCall},
	plan::{ExecutionPlan, NegotiationDecision},
	receipt::{
		Adjustment, AttemptOutcome, AttemptReceipt, Cost, ProviderEvidence, ReasonId,
		ServingModelAttribution,
	},
	recovery::{
		Stage,
		empty::{EmptyCompletionKind, EmptyCompletionStage, EmptyEvent, EmptyInput},
		json::{JsonEnforcement, JsonRepairLimits, JsonRepairStage},
		reasoning::{ReasoningLimits, ReasoningObservation, ReasoningStallGuard},
		repetition::{
			AttemptRepetitionGuard, LoopSignal, OutputVisibility, RepetitionLimits, recovery_record,
		},
		tools::{
			ToolAssembler, ToolAssemblyEvent, ToolAssemblyLimits, ToolFragment, validate_schema,
		},
	},
};

/// Route-scoped conservative normalization for provider discovery rows.
pub trait DiscoveryProjector: Send + Sync + 'static {
	/// Normalizes one provider page without mutating the bundled catalog.
	fn project(
		&self,
		request: &DiscoveryRequest,
		rows: Vec<omp_catalog::DiscoveredModel>,
		next_cursor: Option<Str>,
	) -> Result<ModelDiscoveryPage, Error>;
}

/// Applies catalog-selected deterministic recovery before semantic validation.
#[derive(Clone, Default)]
pub struct RecoveryLayer {
	discovery: Option<Arc<dyn DiscoveryProjector>>,
}
impl RecoveryLayer {
	/// Creates recovery with the exact route-scoped discovery projector.
	pub fn new(discovery: Arc<dyn DiscoveryProjector>) -> Self {
		Self { discovery: Some(discovery) }
	}

	/// Creates recovery for a route that does not advertise runtime discovery.
	pub const fn without_discovery() -> Self {
		Self { discovery: None }
	}
}
impl fmt::Debug for RecoveryLayer {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("RecoveryLayer")
			.field("discovery", &self.discovery.is_some())
			.finish()
	}
}

/// Response-recovery service retaining route-scoped immutable projectors.
#[derive(Clone)]
pub struct RecoveryService<S> {
	inner:     S,
	discovery: Option<Arc<dyn DiscoveryProjector>>,
}

impl<S> Layer<S> for RecoveryLayer {
	type Service = RecoveryService<S>;

	fn layer(&self, inner: S) -> Self::Service {
		RecoveryService { inner, discovery: self.discovery.clone() }
	}
}

impl<S> Service<LayerCall<Call>> for RecoveryService<S>
where
	S: Service<LayerCall<Call>, Response = HandshakenResponse, Error = Error> + Clone,
{
	type Error = Error;
	type Response = HandshakenResponse;

	type Future = impl Future<Output = Result<HandshakenResponse, Error>>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, request: LayerCall<Call>) -> Self::Future {
		let replacement = self.inner.clone();
		let mut inner = mem::replace(&mut self.inner, replacement);
		let context = request.context.clone();
		let tools = match &request.payload.operation {
			OperationCall::Chat(chat) => chat.tools.clone(),
			OperationCall::Realtime(realtime) => realtime.tools.clone(),
			_ => Default::default(),
		};
		let structured = match &request.payload.operation {
			OperationCall::Chat(chat) => match &chat.output {
				Setting::Require(output) => Some((output.clone(), false)),
				Setting::Prefer(output) => Some((output.clone(), true)),
				Setting::Unset => None,
			},
			_ => None,
		};
		let discovery_request = match &request.payload.operation {
			OperationCall::DiscoverModels(request) => Some(request.clone()),
			_ => None,
		};
		let discovery = self.discovery.clone();
		let plan = request.payload.execution.clone();
		async move {
			context.checkpoint(ErrorPhase::Recovery)?;
			let mut response = inner.call(request).await?;
			let structured = structured.and_then(|(output, preferred)| {
				(!preferred || !structured_preference_dropped(plan.as_deref(), &context))
					.then_some(output)
			});
			let evidence = response.body.clone();
			let handshake = response.meta.clone();
			if response.events.is_some() && response.realtime.is_some() {
				return Err(recovery_error("response.events-and-realtime-conflict", &context));
			}
			let Some(mut input) = response.events.take() else {
				if response.realtime.is_some() {
					return Ok(response);
				}
				return Err(recovery_error("response.missing-events-and-realtime", &context));
			};
			let output_context = context.clone();
			let empty_policy = plan
				.as_ref()
				.and_then(|plan| plan.policy_model.as_ref())
				.map(|model| model.wire_policy.clone());
			let mut structured_index = None;
			let mut json = structured.as_ref().and_then(|output| {
				empty_policy.clone().map(|policy| {
					let enforcement = match output {
						StructuredOutput::JsonSchema { strict: true, .. } => JsonEnforcement::Strict,
						_ => JsonEnforcement::NativeOrRepair,
					};
					JsonRepairStage::new(
						enforcement,
						JsonRepairLimits::default(),
						policy,
						output_context.attempts().saturating_sub(1),
					)
				})
			});
			let guard_reasoning = plan
				.as_ref()
				.is_some_and(|plan| plan.wire_policy.reasoning.loop_guard == Some(true));
			response.events = Some(Box::pin(async_stream::stream! {
				let mut completion: Option<RawCompletion> = None;
				let mut empty = empty_policy.map(|policy| EmptyCompletionStage::new(policy, output_context.attempts().saturating_sub(1)));
				let mut reasoning_guard = guard_reasoning.then(|| ReasoningStallGuard::new(ReasoningLimits::default()));
				let mut thinking_repetition = AttemptRepetitionGuard::new(RepetitionLimits::default());
				let mut text_repetition = AttemptRepetitionGuard::new(RepetitionLimits::default());
				while let Some(item) = input.next().await {
					if let Err(error) = output_context.checkpoint(ErrorPhase::Recovery) {
						output_context.cancel();
						yield Err(error);
						return;
					}
					match item {
						Err(mut error) => {
							output_context.finalize_error(&mut error);
							let error = error.committed(output_context.is_committed());
							yield Err(error);
							return;
						}
						Ok(RawEvent::Completion(terminal)) => {
							if let Some(signal) = thinking_repetition
								.finish_exact_cycle(OutputVisibility::Gated)
								.or_else(|| text_repetition.finish_exact_cycle(OutputVisibility::Gated))
							{
								yield Err(repetition_error(&signal, &output_context));
								return;
							}
							if completion.replace(terminal).is_some() {
								yield Err(recovery_error("response.duplicate-completion", &output_context));
								return;
							}
						}
						Ok(RawEvent::ToolCallComplete { index, call }) => {
							let event = match recover_tool(index, call, &tools, &output_context) {
								Ok(event) => event,
								Err(error) => { yield Err(error); return; },
							};
							if let Err(error) = observe_reasoning(
								&mut reasoning_guard,
								&mut thinking_repetition,
								&mut text_repetition,
								&event,
								&output_context,
							) {
								yield Err(error);
								return;
							}
							match observe_empty(&mut empty, event, &output_context) {
								Ok(event) => yield Ok(RawEvent::Chat(event)),
								Err(error) => { yield Err(error); return; },
							}
						}
						Ok(RawEvent::ProviderState(state)) => output_context.stage_provider_state(state),
						Ok(RawEvent::Metadata(metadata)) => output_context.observe_provider_metadata(metadata),
						Ok(RawEvent::Telemetry(telemetry)) => output_context.observe_provider_telemetry(telemetry),
						Ok(RawEvent::Failure(mut error)) => {
							output_context.finalize_error(&mut error);
							let error = error.committed(output_context.is_committed());
							yield Err(error);
							return;
						}
						Ok(RawEvent::Chat(ChatEvent::Completed(_))) => {
							yield Err(recovery_error("response.public-completion-before-finalization", &output_context));
							return;
						}
						Ok(RawEvent::Chat(ChatEvent::TextDelta { index, text })) => {
							let event = ChatEvent::TextDelta { index, text: text.clone() };
							if let Err(error) = observe_reasoning(
								&mut reasoning_guard,
								&mut thinking_repetition,
								&mut text_repetition,
								&event,
								&output_context,
							) {
								yield Err(error);
								return;
							}
							let event = match observe_empty(&mut empty, event, &output_context) {
								Ok(event) => event,
								Err(error) => { yield Err(error); return; },
							};
							if let Some(stage) = json.as_mut() {
								structured_index.get_or_insert(index);
								if stage.push(bytes::Bytes::copy_from_slice(text.as_bytes()), &mut |_| {}).is_err() {
									yield Err(structured_error("structured-output.repair-input", &output_context));
									return;
								}
							} else {
								yield Ok(RawEvent::Chat(event));
							}
						}
						Ok(RawEvent::Chat(event)) => {
							if let Err(error) = observe_reasoning(
								&mut reasoning_guard,
								&mut thinking_repetition,
								&mut text_repetition,
								&event,
								&output_context,
							) {
								yield Err(error);
								return;
							}
							match observe_empty(&mut empty, event, &output_context) {
								Ok(event) => yield Ok(RawEvent::Chat(event)),
								Err(error) => { yield Err(error); return; },
							}
						}
						Ok(RawEvent::ImageGeneration(event)) => yield Ok(RawEvent::ImageGeneration(event)),
						Ok(RawEvent::VideoGeneration(event)) => yield Ok(RawEvent::VideoGeneration(event)),
						Ok(RawEvent::Audio(chunk)) => yield Ok(RawEvent::Audio(chunk)),
						Ok(RawEvent::Transcript(event)) => yield Ok(RawEvent::Transcript(event)),
						Ok(RawEvent::Answer(body)) => yield Ok(RawEvent::Answer(body)),
						Ok(RawEvent::Control(control)) => yield Ok(RawEvent::Control(control)),
						Ok(RawEvent::NativeChunk(bytes)) => yield Ok(RawEvent::NativeChunk(bytes)),
						Ok(RawEvent::DiscoveredModels { rows, next_cursor }) => {
							match project_discovery(
								discovery.as_ref(),
								discovery_request.as_deref(),
								rows,
								next_cursor,
								&output_context,
							) {
								Ok(event) => yield Ok(event),
								Err(error) => { yield Err(error); return; },
							}
						},
					}
				}
				if let Some(terminal) = completion {
					let finalized = match finalize_completion(terminal, plan.as_deref(), &handshake, evidence.evidence(), &output_context) {
						Ok(event) => event,
						Err(error) => { yield Err(error); return; },
					};
					if let Err(error) =
						finish_empty(&mut empty, &finalized, &output_context)
					{
						yield Err(error);
						return;
					}
					if let Some(output) = structured.as_ref() {
						let repaired = match finish_json(&mut json, &output_context) {
							Ok(text) => text,
							Err(error) => { yield Err(error); return; },
						};
						if let Err(error) = validate_structured_output(output, &repaired, &output_context) {
							yield Err(error);
							return;
						}
						output_context.mark_structured_output_valid();
						yield Ok(RawEvent::Chat(ChatEvent::TextDelta {
							index: structured_index.unwrap_or(0),

							text: repaired.into(),
						}));
					}
					yield Ok(RawEvent::Chat(finalized));
				}
			}));
			Ok(response)
		}
	}
}
fn structured_preference_dropped(plan: Option<&ExecutionPlan>, context: &ExecutionContext) -> bool {
	let decision_dropped = plan.is_some_and(|plan| {
		plan.decisions.iter().any(|decision| {
			matches!(
				decision,
				NegotiationDecision::Dropped { feature, .. }
					if feature.0.as_str() == "chat.structured_output"
			)
		})
	});
	decision_dropped
		|| context.receipt().adjustments.iter().any(|adjustment| {
			matches!(
				adjustment,
				Adjustment::Dropped { feature, .. }
					if matches!(
						feature.0.as_str(),
						"chat.structured_output" | "response_format.grammar"
					)
			)
		})
}

fn project_discovery(
	projector: Option<&Arc<dyn DiscoveryProjector>>,
	request: Option<&DiscoveryRequest>,
	rows: Vec<omp_catalog::DiscoveredModel>,
	next_cursor: Option<Str>,
	context: &ExecutionContext,
) -> Result<RawEvent, Error> {
	let result = match (projector, request) {
		(Some(projector), Some(request)) => projector
			.project(request, rows, next_cursor)
			.map(|page| RawEvent::Answer(AnswerBody::Models(page))),
		(None, _) => Err(recovery_error("discovery.projector-missing", context)),
		(Some(_), None) => Err(recovery_error("discovery.request-missing", context)),
	};
	result.map_err(|mut error| {
		context.finalize_error(&mut error);
		error
	})
}
fn observe_reasoning(
	guard: &mut Option<ReasoningStallGuard>,
	thinking_repetition: &mut AttemptRepetitionGuard,
	text_repetition: &mut AttemptRepetitionGuard,
	event: &ChatEvent,
	context: &ExecutionContext,
) -> Result<(), Error> {
	let exact = match event {
		ChatEvent::ThinkingDelta { text, .. } => {
			thinking_repetition.observe_exact_cycle(text, OutputVisibility::Gated)
		},
		ChatEvent::TextDelta { text, .. } => {
			text_repetition.observe_exact_cycle(text, OutputVisibility::Gated)
		},
		_ => None,
	};
	if let Some(signal) = exact {
		return Err(repetition_error(&signal, context));
	}
	let Some(guard) = guard.as_mut() else {
		return Ok(());
	};
	let observation = match event {
		ChatEvent::ThinkingDelta { text, .. } => ReasoningObservation {
			delta:             text,
			semantic_progress: false,
			visibility:        OutputVisibility::Gated,
		},
		ChatEvent::TextDelta { .. }
		| ChatEvent::ToolCallReady { .. }
		| ChatEvent::Artifact { .. } => ReasoningObservation {
			delta:             "",
			semantic_progress: true,
			visibility:        OutputVisibility::Gated,
		},
		_ => return Ok(()),
	};
	let Some(signal) = guard.observe(observation) else {
		return Ok(());
	};
	Err(repetition_error(&signal, context))
}

fn repetition_error(signal: &LoopSignal, context: &ExecutionContext) -> Error {
	context.with_receipt(|receipt| {
		receipt
			.recoveries
			.push(recovery_record(context.attempts().saturating_sub(1), signal));
	});
	Error::new(
		ErrorKind::RepeatedReasoning,
		ErrorPhase::Recovery,
		RetryAction::SemanticRetry,
		context.receipt(),
	)
	.committed(context.is_committed())
	.detail(ErrorDetail::protocol(ReasonId::new_static("reasoning.loop-detected")))
}

fn observe_empty(
	stage: &mut Option<EmptyCompletionStage>,
	event: ChatEvent,
	context: &ExecutionContext,
) -> Result<ChatEvent, Error> {
	let Some(stage) = stage.as_mut() else {
		return Ok(event);
	};
	let mut output = None;
	stage
		.push(EmptyInput::Event(Box::new(event)), &mut |event| output = Some(event))
		.map_err(|_| recovery_error("empty-completion.observer", context))?;
	match output {
		Some(EmptyEvent::Event(event)) => Ok(*event),
		Some(EmptyEvent::Empty(_)) | None => {
			Err(recovery_error("empty-completion.invalid-observer-output", context))
		},
	}
}

fn finish_empty(
	stage: &mut Option<EmptyCompletionStage>,
	completion: &ChatEvent,
	context: &ExecutionContext,
) -> Result<(), Error> {
	let Some(stage) = stage.as_mut() else {
		return Ok(());
	};
	let mut empty = None;
	stage
		.push(EmptyInput::Completed, &mut |event| {
			if let EmptyEvent::Empty(classification) = event {
				empty = Some(classification);
			}
		})
		.map_err(|_| recovery_error("empty-completion.classification", context))?;
	stage
		.finish(&mut |_| {})
		.map_err(|_| recovery_error("empty-completion.finish", context))?;
	let Some(classification) = empty else {
		return Ok(());
	};
	if matches!(
		completion,
		ChatEvent::Completed(Completion { reason: FinishReason::Other(reason), .. })
			if reason.as_str() == "pause_turn"
	) {
		return Ok(());
	}
	context.with_receipt(|receipt| receipt.recoveries.push(classification.recovery));
	let (kind, action, reason) = match classification.kind {
		EmptyCompletionKind::ThinkingOnly => {
			(ErrorKind::EmptyOutput, RetryAction::Never, "empty-completion.thought-only")
		},
		EmptyCompletionKind::NoContent
		| EmptyCompletionKind::WhitespaceOnly
		| EmptyCompletionKind::EmptyBlocks => {
			(ErrorKind::EmptyCompletion, RetryAction::SemanticRetry, "empty-completion.classified")
		},
	};
	Err(
		Error::new(kind, ErrorPhase::Recovery, action, context.receipt())
			.committed(context.is_committed())
			.detail(ErrorDetail::protocol(ReasonId::new(reason))),
	)
}

fn finish_json(
	stage: &mut Option<JsonRepairStage>,
	context: &ExecutionContext,
) -> Result<String, Error> {
	let Some(stage) = stage.as_mut() else {
		return Err(structured_error("structured-output.repair-policy-missing", context));
	};
	let mut document = None;
	stage
		.finish(&mut |value| document = Some(value))
		.map_err(|_| structured_error("structured-output.invalid-json", context))?;
	let document =
		document.ok_or_else(|| structured_error("structured-output.missing-document", context))?;
	if let Some(recovery) = document.recovery {
		context.with_receipt(|receipt| receipt.recoveries.push(recovery));
	}
	String::from_utf8(document.bytes.to_vec())
		.map_err(|_| structured_error("structured-output.invalid-utf8", context))
}

fn validate_structured_output(
	output: &StructuredOutput,
	text: &str,
	context: &ExecutionContext,
) -> Result<(), Error> {
	let value = serde_json::from_str::<serde_json::Value>(text)
		.map_err(|_| structured_error("structured-output.invalid-json", context))?;
	match output {
		StructuredOutput::JsonObject if value.is_object() => Ok(()),
		StructuredOutput::JsonObject => {
			Err(structured_error("structured-output.not-object", context))
		},
		StructuredOutput::JsonSchema { schema, strict, .. } => {
			validate_schema(schema.as_value(), &value, *strict, ToolAssemblyLimits::default())
				.map_err(|_| structured_error("structured-output.schema-violation", context))
		},
		StructuredOutput::Regex(_) | StructuredOutput::Lark(_) | StructuredOutput::Ebnf(_) => {
			Err(structured_error("structured-output.validator-unavailable", context))
		},
	}
}

fn structured_error(reason: &'static str, context: &ExecutionContext) -> Error {
	Error::new(
		ErrorKind::StructuredOutputFailure,
		ErrorPhase::Recovery,
		RetryAction::SemanticRetry,
		context.receipt(),
	)
	.committed(context.is_committed())
	.detail(ErrorDetail::protocol(ReasonId::new(reason)))
}
fn finalize_completion(
	terminal: RawCompletion,
	plan: Option<&ExecutionPlan>,
	handshake: &HandshakeMeta,
	body: AttemptBodyEvidence,
	context: &ExecutionContext,
) -> Result<ChatEvent, Error> {
	let plan = plan.ok_or_else(|| recovery_error("completion.missing-execution-plan", context))?;
	let serving_model = plan
		.model
		.clone()
		.ok_or_else(|| recovery_error("completion.missing-serving-model", context))?;
	let model = plan
		.policy_model
		.as_ref()
		.ok_or_else(|| recovery_error("completion.missing-pricing-model", context))?;
	if model
		.pricing
		.components
		.iter()
		.any(|component| component.unit == PriceUnit::McharInput)
		|| model.pricing.tiers.iter().any(|tier| {
			tier
				.components
				.iter()
				.any(|component| component.unit == PriceUnit::McharInput)
		}) {
		return Err(recovery_error("completion.character-usage-unavailable", context));
	}
	let usage = terminal.usage;
	let dimensions = UsageDimensions {
		input_tokens:       usage.input_tokens,
		output_tokens:      usage.output_tokens.saturating_add(usage.reasoning_tokens),
		cache_read_tokens:  usage.cache_read_tokens,
		cache_write_tokens: usage.cache_write_tokens,
		images:             u64::from(usage.images),
		video_seconds:      usage.video_ms.div_ceil(1_000),
		audio_seconds:      usage
			.audio_input_ms
			.saturating_add(usage.audio_output_ms)
			.div_ceil(1_000),
		input_characters:   0,
		requests:           1,
	};
	let nanos = model
		.pricing
		.cost(dimensions)
		.map_err(|_| recovery_error("completion.pricing-overflow", context))?
		.as_nanos();
	let micro_usd = i128::from(nanos.div_ceil(1_000));
	let cost = Cost::from_micro_usd(micro_usd);
	context.charge_tokens(
		usage.input_tokens,
		usage.output_tokens.saturating_add(usage.reasoning_tokens),
	)?;
	context.charge_cost(cost)?;
	let index = context.attempts().saturating_sub(1);
	let routing = context.account_routing().unwrap_or_default();
	context.with_receipt(|receipt| {
		if !receipt
			.attempts
			.iter()
			.any(|attempt| attempt.index == index)
		{
			receipt.record_attempt(AttemptReceipt {
				index,
				hidden: false,
				provider: Some(plan.provider.clone()),
				route: Some(plan.route.clone()),
				account: routing.account,
				principal: routing.principal,
				body,
				outcome: AttemptOutcome::Succeeded,
				usage,
				cost,
				provider_evidence: ProviderEvidence {
					request_id: handshake
						.provider_request_id
						.clone()
						.or_else(|| context.provider_response_id()),
					status:     handshake.status,
					code:       None,
					summary:    None,
				},
				elapsed: context.attempt_elapsed(index),
			});
		}
		let _ = receipt.settle_serving_model(ServingModelAttribution {
			provider: plan.provider.clone(),
			model:    serving_model,
			attempt:  index,
		});
		receipt.timings.total = context.elapsed();
		receipt.timings.completed_at = Some(SystemTime::now());
	});
	let receipt = context.receipt();
	Ok(ChatEvent::Completed(Completion {
		reason: terminal.reason,
		blocks: terminal.blocks,
		usage,
		receipt: receipt.into(),
	}))
}

fn recover_tool(
	index: u32,
	call: UnvalidatedToolCall,
	definitions: &[ToolDefinition],
	context: &ExecutionContext,
) -> Result<ChatEvent, Error> {
	if !definitions
		.iter()
		.any(|definition| definition.name == call.name)
	{
		return Err(recovery_error("tool.not-declared", context));
	}
	let mut assembler = ToolAssembler::new(
		definitions,
		ToolAssemblyLimits::default(),
		context.attempts().saturating_sub(1),
	);
	let mut ready = None;
	for fragment in [
		ToolFragment::Start {
			source_index: index,
			id:           Some(call.id),
			name:         bytes::Bytes::copy_from_slice(call.name.as_bytes()),
			input_kind:   call.input_kind,
		},
		ToolFragment::ArgumentsDelta { source_index: index, bytes: call.arguments },
		ToolFragment::End { source_index: index },
	] {
		for event in assembler.push(fragment) {
			match event {
				ToolAssemblyEvent::Ready { call, .. } => ready = Some(call),
				ToolAssemblyEvent::Rejected { .. } => {
					context.with_receipt(|receipt| receipt.recoveries.extend(assembler.take_evidence()));
					return Err(recovery_error("tool.assembly-rejected", context));
				},
				ToolAssemblyEvent::Started { .. } | ToolAssemblyEvent::ArgumentsDelta { .. } => {},
			}
		}
	}
	context.with_receipt(|receipt| receipt.recoveries.extend(assembler.take_evidence()));
	let call = ready.ok_or_else(|| recovery_error("tool.assembly-incomplete", context))?;
	Ok(ChatEvent::ToolCallReady { index, call })
}

fn recovery_error(reason: &'static str, context: &ExecutionContext) -> Error {
	Error::new(
		ErrorKind::MalformedModelOutput,
		ErrorPhase::Recovery,
		RetryAction::SemanticRetry,
		context.receipt(),
	)
	.committed(context.is_committed())
	.detail(ErrorDetail::protocol(ReasonId::new(reason)))
}

#[cfg(test)]
mod tests {

	use omp_catalog::id::WirePolicyId;
	use omp_core::sf;

	use super::*;
	use crate::{
		call::DiscoveryRequest,
		layer::{ExecutionBudget, ExecutionContext},
	};

	struct TestProjector {
		fail: bool,
	}
	fn edit_grammar_definition() -> ToolDefinition {
		ToolDefinition {
			name:        sf!("edit"),
			description: None,
			input:       crate::call::ToolInputConstraint::Grammar {
				grammar:  crate::call::ToolGrammar {
					syntax:     crate::call::ToolGrammarSyntax::Lark,
					definition: sf!("start: LF"),
				},
				fallback: crate::call::OpaqueJson::new(serde_json::json!({
					"type": "object",
					"properties": {"input": {"type": "string"}},
					"required": ["input"],
				})),
			},
		}
	}

	#[test]
	fn grammar_tool_recovers_the_schema_lowered_json_wire_form() {
		let context = ExecutionContext::new(ExecutionBudget::default());
		let call = UnvalidatedToolCall {
			id:         crate::id::ToolCallId::new("call-edit"),
			name:       sf!("edit"),
			input_kind: crate::codec::ToolInputKind::Json,
			arguments:  bytes::Bytes::from_static(br#"{"input":"PUT 1.=1:"}"#),
		};
		let event = recover_tool(3, call, &[edit_grammar_definition()], &context)
			.expect("fallback JSON wire form must recover");
		let ChatEvent::ToolCallReady { index: 3, call } = event else {
			panic!("expected a ready call: {event:?}");
		};
		assert_eq!(call.arguments.as_value(), &serde_json::json!({"input": "PUT 1.=1:"}));
	}

	#[test]
	fn grammar_tool_recovers_the_freeform_wire_form_into_the_canonical_object() {
		let context = ExecutionContext::new(ExecutionBudget::default());
		let text = "[src/a.rs#1A2B]\nPUT 1.=1:\n+replacement";
		let call = UnvalidatedToolCall {
			id:         crate::id::ToolCallId::new("call-edit"),
			name:       sf!("edit"),
			input_kind: crate::codec::ToolInputKind::Freeform,
			arguments:  bytes::Bytes::copy_from_slice(text.as_bytes()),
		};
		let event = recover_tool(0, call, &[edit_grammar_definition()], &context)
			.expect("freeform wire form must recover");
		let ChatEvent::ToolCallReady { call, .. } = event else {
			panic!("expected a ready call: {event:?}");
		};
		assert_eq!(call.arguments.as_value(), &serde_json::json!({"input": text}));
	}
	#[test]
	fn dropped_preferred_structured_output_disables_repair() {
		let context = ExecutionContext::new(ExecutionBudget::default());
		context.with_receipt(|receipt| {
			receipt.adjustments.push(Adjustment::Dropped {
				feature: crate::receipt::FeatureId(sf!("chat.structured_output")),
				reason:  ReasonId(sf!("provider-dropped-preference")),
			});
		});
		assert!(structured_preference_dropped(None, &context));
	}

	impl DiscoveryProjector for TestProjector {
		fn project(
			&self,
			_: &DiscoveryRequest,
			_: Vec<omp_catalog::DiscoveredModel>,
			next_cursor: Option<Str>,
		) -> Result<ModelDiscoveryPage, Error> {
			if self.fail {
				return Err(Error::new(
					ErrorKind::MalformedModelOutput,
					ErrorPhase::Recovery,
					RetryAction::Never,
					Default::default(),
				));
			}
			Ok(ModelDiscoveryPage { models: Vec::new(), next_cursor })
		}
	}

	fn request() -> DiscoveryRequest {
		DiscoveryRequest {
			provider:  None,
			route:     None,
			cursor:    None,
			page_size: 10,
			operation: None,
		}
	}

	fn finish_empty_error(event: Option<ChatEvent>) -> Error {
		let context = ExecutionContext::new(ExecutionBudget::default());
		let mut stage = Some(EmptyCompletionStage::new(WirePolicyId::new("wire"), 0));
		if let Some(event) = event {
			observe_empty(&mut stage, event, &context).expect("empty observer accepts chat event");
		}
		let completion = ChatEvent::Completed(Completion {
			reason:  FinishReason::Stop,
			blocks:  0,
			usage:   Default::default(),
			receipt: Default::default(),
		});
		finish_empty(&mut stage, &completion, &context)
			.expect_err("empty completion must fail recovery")
	}

	#[test]
	fn pause_turn_accepts_an_empty_completion() {
		let context = ExecutionContext::new(ExecutionBudget::default());
		let mut stage = Some(EmptyCompletionStage::new(WirePolicyId::new("wire"), 0));
		let completion = ChatEvent::Completed(Completion {
			reason:  FinishReason::Other(sf!("pause_turn")),
			blocks:  0,
			usage:   Default::default(),
			receipt: Default::default(),
		});
		assert!(finish_empty(&mut stage, &completion, &context).is_ok());
	}

	#[test]
	fn thought_only_completion_requires_session_continuation_without_replay() {
		let error =
			finish_empty_error(Some(ChatEvent::ThinkingDelta { index: 0, text: "reasoning".into() }));

		assert_eq!(error.kind, ErrorKind::EmptyOutput);
		assert_eq!(error.action, RetryAction::Never);
		assert!(matches!(
			error.detail_ref(),
			Some(ErrorDetail::Protocol { reason })
				if reason.0.as_str() == "empty-completion.thought-only"
		));
	}

	#[test]
	fn eventless_completion_remains_semantically_retryable() {
		let error = finish_empty_error(None);

		assert_eq!(error.kind, ErrorKind::EmptyCompletion);
		assert_eq!(error.action, RetryAction::SemanticRetry);
		assert!(matches!(
			error.detail_ref(),
			Some(ErrorDetail::Protocol { reason })
				if reason.0.as_str() == "empty-completion.classified"
		));
	}

	#[test]
	fn exact_text_cycles_are_guarded_without_provider_opt_in() {
		let cycle = "shipped delivered verified validated approved accepted merged deployed live \
		             operational successful excellent perfect final absolute total whole full \
		             entire complete done finished";
		let runaway = cycle.repeat(8);
		let context =
			ExecutionContext::new(ExecutionBudget { max_attempts: 3, ..ExecutionBudget::default() });
		let mut reasoning = None;
		let mut thinking_repetition = AttemptRepetitionGuard::new(RepetitionLimits::default());
		let mut text_repetition = AttemptRepetitionGuard::new(RepetitionLimits::default());
		let mut failure = None;
		for chunk in runaway.as_bytes().chunks(23) {
			let event = ChatEvent::TextDelta {
				index: 0,
				text:  str::from_utf8(chunk).expect("ASCII fixture").into(),
			};
			if let Err(error) = observe_reasoning(
				&mut reasoning,
				&mut thinking_repetition,
				&mut text_repetition,
				&event,
				&context,
			) {
				failure = Some(error);
				break;
			}
		}
		let error = failure.expect("provider-independent exact cycle guard");
		assert_eq!(error.kind, ErrorKind::RepeatedReasoning);
		assert_eq!(error.action, RetryAction::SemanticRetry);
	}

	#[test]
	fn discovery_page_is_emitted_only_after_projection_succeeds() {
		let context = ExecutionContext::new(ExecutionBudget::default());
		let projector: Arc<dyn DiscoveryProjector> = Arc::new(TestProjector { fail: false });
		let projected = project_discovery(
			Some(&projector),
			Some(&request()),
			Vec::new(),
			Some("next".into()),
			&context,
		);
		assert!(matches!(
			projected,
			Ok(RawEvent::Answer(AnswerBody::Models(ModelDiscoveryPage { next_cursor: Some(cursor), .. })))
				if cursor.as_str() == "next"
		));
	}

	#[test]
	fn corrupt_or_unconfigured_discovery_page_is_terminal_without_output() {
		let context = ExecutionContext::new(ExecutionBudget::default());
		let corrupt: Arc<dyn DiscoveryProjector> = Arc::new(TestProjector { fail: true });
		assert!(
			project_discovery(Some(&corrupt), Some(&request()), Vec::new(), None, &context).is_err()
		);
		assert!(project_discovery(None, Some(&request()), Vec::new(), None, &context).is_err());
		assert!(project_discovery(Some(&corrupt), None, Vec::new(), None, &context).is_err());
	}
}
