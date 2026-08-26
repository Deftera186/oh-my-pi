//! Session-scoped regime enforcing resolution of staged tool proposals.

use std::{
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use omp_agent::{
	Next, Regime, RegimeContext, RegimeError, RegimeLifetime, RegimeSpec, RegimeStateError,
};
use omp_core::{Point, Str, sf};
use omp_proto::thread::v1::{self as thread, Item, item};
use omp_tools::staging::{
	ActivationObserver, PROPOSAL_PENDING_NOTICE, ProposalActivationError, ProposalDecision,
	ProposalRejection, StagedProposal, StagedProposalRegistry,
};

const MAX_FORCE_ATTEMPTS: u32 = 3;
const MAX_COMMITTED_STEPS: u32 = 1 + MAX_FORCE_ATTEMPTS;
const LIMIT_DETAIL: &str = "staged proposal resolution reached its regime limit; the proposal was \
                            rejected without being applied";

/// Builds the late-bound observer installed at the environment staging seam.
pub(super) fn observer(
	sender: omp_agent::ControlSender,
	proposals: StagedProposalRegistry,
) -> ActivationObserver {
	Arc::new(move |pending| {
		let sender = sender.clone();
		let proposals = proposals.clone();
		Box::pin(async move {
			sender
				.start_regime(
					spec(),
					Box::new(StagedPreviewRegime::new(pending, proposals)),
					omp_agent::StartOptions { now_ms: now_ms(), queue: false },
				)
				.await
				.map_err(|_| ProposalActivationError::Rejected)?;
			Ok(())
		})
	})
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

/// Builds the finite staged-preview regime declaration.
pub(super) fn spec() -> Arc<RegimeSpec> {
	Arc::new(RegimeSpec {
		id: Str::new_static("staged-preview"),
		events: Point::Context.set().with(Point::ToolChoice),
		precedence: 0,
		max_steps: Some(MAX_COMMITTED_STEPS),
		committed_step_interval_ms: None,
		on_limit: true,
		lifetime: RegimeLifetime::Session,
		family_rev: Str::new_static("dev.omp.tools.staged-preview@1"),
		when: None,
		owns: Arc::from([]),
		sets: Arc::from([]),
		minimum_duration_ms: None,
	})
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviewAction {
	None,
	Notice,
	RequireTool,
	Complete,
}

/// One proposal-specific staged-preview regime.
pub(super) struct StagedPreviewRegime {
	pending:   StagedProposal,
	proposals: StagedProposalRegistry,
	step:      u32,
}

impl StagedPreviewRegime {
	/// Creates the regime for one exact pending proposal.
	pub(super) fn new(pending: StagedProposal, proposals: StagedProposalRegistry) -> Self {
		Self { pending, proposals, step: 0 }
	}

	fn reminder(&self) -> Item {
		let text = sf!(
			"{PROPOSAL_PENDING_NOTICE} The pending proposal was staged by {}.",
			self.pending.source_tool.as_str()
		);
		Item {
			kind: Some(item::Kind::Message(thread::Message {
				role:  thread::Role::User as i32,
				parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text.to_string())) }],
			})),
			..Item::default()
		}
	}

	fn action(&self, point: Point, committed_steps: u32) -> PreviewAction {
		if !self.proposals.is_pending(self.pending.id.as_str()) {
			return PreviewAction::Complete;
		}
		if committed_steps == 0 {
			if point != Point::Context {
				return PreviewAction::None;
			}
			return PreviewAction::Notice;
		}
		if point == Point::ToolChoice && committed_steps <= MAX_FORCE_ATTEMPTS {
			return PreviewAction::RequireTool;
		}
		PreviewAction::None
	}

	fn reject_at_limit(&mut self) {
		let _ =
			(self.pending.resolver)(ProposalDecision::Reject(ProposalRejection::RegimeLimitReached));
		self.step = MAX_FORCE_ATTEMPTS.saturating_add(2);
	}
}

impl Regime for StagedPreviewRegime {
	fn apply(&mut self, ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
		match self.action(ctx.point(), ctx.committed_steps()) {
			PreviewAction::None => {},
			PreviewAction::Notice => {
				ctx.append_context(vec![self.reminder()]);
				ctx.replace_state((ctx.committed_steps() + 1).to_string());
			},
			PreviewAction::RequireTool => {
				ctx.require_tool("bash");
				ctx.replace_state((ctx.committed_steps() + 1).to_string());
			},
			PreviewAction::Complete => next.complete(),
		}
		Ok(())
	}

	fn on_limit(&mut self, _ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
		self.reject_at_limit();
		next.fail(LIMIT_DETAIL);
		Ok(())
	}

	fn state(&self) -> Str {
		Str::from(self.step.to_string())
	}

	fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
		let step = payload
			.parse()
			.map_err(|_| RegimeStateError::InvalidPayload)?;
		if step > MAX_FORCE_ATTEMPTS.saturating_add(2) {
			return Err(RegimeStateError::InvalidPayload);
		}
		self.step = step;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use omp_tools::staging::{
		ActivationObserver, ProposalDecision, ProposalError, StagedProposalAction,
		StagedProposalRegistry,
	};
	use parking_lot::Mutex;
	use serde_json::{Value, json};

	use super::*;

	struct RecordingAction(Arc<Mutex<Vec<ProposalDecision>>>);

	impl StagedProposalAction for RecordingAction {
		fn finalize(&mut self, decision: &ProposalDecision) -> Result<Value, ProposalError> {
			self.0.lock().push(decision.clone());
			Ok(json!({ "rejected": true }))
		}
	}

	async fn staged(
		decisions: Arc<Mutex<Vec<ProposalDecision>>>,
	) -> (StagedProposalRegistry, StagedProposal) {
		let registry = StagedProposalRegistry::new();
		let observer: ActivationObserver = Arc::new(|_| Box::pin(async { Ok(()) }));
		registry.install_activation_observer(observer);
		let pending = registry
			.stage(
				Str::new_static("ast_edit"),
				Str::new_static("one file would change"),
				RecordingAction(decisions),
			)
			.await
			.expect("proposal staged");
		(registry, pending)
	}

	#[tokio::test]
	async fn limit_rejects_pending_proposal_with_typed_reason() {
		let decisions = Arc::new(Mutex::new(Vec::new()));
		let (registry, pending) = staged(Arc::clone(&decisions)).await;
		let mut regime = StagedPreviewRegime::new(pending.clone(), registry.clone());
		assert_eq!(regime.action(Point::Context, 0), PreviewAction::Notice);
		for committed_steps in 1..=MAX_FORCE_ATTEMPTS {
			assert_eq!(regime.action(Point::ToolChoice, committed_steps), PreviewAction::RequireTool,);
		}
		regime.reject_at_limit();
		assert_eq!(decisions.lock().as_slice(), &[ProposalDecision::Reject(
			ProposalRejection::RegimeLimitReached
		)]);
		assert!(!registry.is_pending(pending.id.as_str()));
		assert_eq!(spec().max_steps, Some(MAX_COMMITTED_STEPS));
		assert!(spec().on_limit);
	}

	#[tokio::test]
	async fn proposal_notice_required_tool_and_completion_are_ordered() {
		let decisions = Arc::new(Mutex::new(Vec::new()));
		let (registry, pending) = staged(Arc::clone(&decisions)).await;
		let regime = StagedPreviewRegime::new(pending.clone(), registry.clone());
		assert_eq!(regime.action(Point::ToolChoice, 0), PreviewAction::None);
		assert_eq!(regime.action(Point::Context, 0), PreviewAction::Notice);
		assert!(regime.reminder().kind.is_some());
		assert_eq!(regime.action(Point::ToolChoice, 1), PreviewAction::RequireTool);
		// A queued requirement has not committed, so the same attempt remains eligible.
		assert_eq!(regime.action(Point::ToolChoice, 1), PreviewAction::RequireTool);
		for committed_steps in 1..=MAX_FORCE_ATTEMPTS {
			assert_eq!(regime.action(Point::ToolChoice, committed_steps), PreviewAction::RequireTool,);
		}
		assert_eq!(regime.action(Point::ToolChoice, MAX_COMMITTED_STEPS), PreviewAction::None,);
		registry
			.finalize(pending.id.as_str(), ProposalDecision::Resolve {
				reason: Str::new_static("applied"),
			})
			.expect("proposal resolves directly");
		assert_eq!(regime.action(Point::Context, 0), PreviewAction::Complete);
		assert_eq!(decisions.lock().as_slice(), &[ProposalDecision::Resolve {
			reason: Str::new_static("applied"),
		}],);
	}
	#[tokio::test]
	async fn durable_state_restores_the_committed_attempt() {
		let decisions = Arc::new(Mutex::new(Vec::new()));
		let (registry, pending) = staged(decisions).await;
		let mut regime = StagedPreviewRegime::new(pending, registry);
		regime.restore("2").expect("state restores");
		assert_eq!(regime.state().as_str(), "2");
		assert_eq!(regime.action(Point::ToolChoice, 2), PreviewAction::RequireTool);
		assert_eq!(
			regime.restore(&(MAX_FORCE_ATTEMPTS + 3).to_string()),
			Err(RegimeStateError::InvalidPayload),
		);
	}
}
