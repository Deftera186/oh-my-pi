//! Durable, bounded regimes resolved at the agent loop's fixed decision points.

use std::{
	collections::{BTreeMap, VecDeque},
	mem, str,
	sync::Arc,
	time::Duration,
};

use bytes::Bytes;
use flume::Receiver;
use omp_core::{Point, PointSet, Str, Ulid};
use omp_inference::call::ToolChoice;
use omp_proto::thread::v1::{Item, item, part};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::{sync::Notify, time};

use crate::{
	ContextPatch, JobBoard,
	arbiter::PointCx,
	r#loop::now_ms,
	tool_choice::{
		DirectiveCallbacks, DirectivePriority, PushOptions, RejectOutcome, ToolChoiceQueue,
	},
	ttsr::StreamSource,
};

/// Stable identity of a regime declaration.
pub type RegimeId = Str;
/// Stable identity of one regime activation.
pub type ActivationId = Str;

/// Lifetime over which an activation remains eligible.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum RegimeLifetime {
	/// The current model/tool turn only.
	Turn,
	/// The current caller submission.
	Run,
	/// The durable session, including process revival.
	Session,
}

/// Named exclusive resource owned by an activation.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
	/// The next forced tool choice.
	ToolChoice,
	/// Exclusive workspace mutation regime.
	Worktree,
	/// Exclusive loop director.
	Director,
	/// Exclusive editor surface.
	EditorSurface,
	/// Exclusive background batch execution.
	BatchExecution,
	/// User-visible regime exclusivity.
	Mode,
	/// A declaration supplied by a future core slot table.
	Named(Str),
}

impl Resource {
	/// Returns the canonical declaration name.
	pub fn name(&self) -> &str {
		match self {
			Self::ToolChoice => "tool_choice",
			Self::Worktree => "worktree",
			Self::Director => "director",
			Self::EditorSurface => "editor-surface",
			Self::BatchExecution => "batch-execution",
			Self::Mode => "mode",
			Self::Named(name) => name.as_str(),
		}
	}
}

/// One canonical exclusive-slot declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceDecl {
	/// Stable name accepted by regime declarations.
	pub name:      &'static str,
	/// Whether the holder belongs in user-facing resource projections.
	pub visible:   bool,
	/// Whether a conflicting activation may enter the FIFO.
	pub queueable: bool,
}

/// Core resource vocabulary registered by every [`ResourceRegistry`].
pub const RESOURCE_TABLE: [ResourceDecl; 6] = [
	ResourceDecl { name: "tool_choice", visible: false, queueable: true },
	ResourceDecl { name: "worktree", visible: true, queueable: true },
	ResourceDecl { name: "director", visible: true, queueable: true },
	ResourceDecl { name: "editor-surface", visible: true, queueable: true },
	ResourceDecl { name: "batch-execution", visible: true, queueable: true },
	ResourceDecl { name: "mode", visible: true, queueable: true },
];

/// Named stackable setting slot.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingSlot {
	/// Advertised tool set.
	Toolset,
	/// Model routing selection.
	ModelRoute,
	/// Prompt contribution slot.
	PromptSlot,
	/// Interrupt delivery policy.
	DeliveryPolicy,
	/// A core-registered additional setting slot.
	Named(Str),
}

/// One activation-scoped setting.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScopedSetting {
	/// Addressed stack.
	pub slot:  SettingSlot,
	/// Opaque value interpreted by the slot owner.
	pub value: Str,
}

/// Required-deadline park request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WaitTicket {
	/// Stable ticket identity.
	pub id:          Str,
	/// Absolute epoch-millisecond deadline.
	pub deadline_ms: u64,
	/// User-visible reason.
	pub reason:      Str,
}
/// Typed terminal failure retained without rendering structured extension data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegimeFailure {
	/// Core-authored diagnostic text.
	Message(Str),
	/// Canonical structured JSON supplied by an extension handler.
	Structured(Bytes),
}

impl RegimeFailure {
	/// Creates a core-authored diagnostic.
	pub fn message(message: impl Into<Str>) -> Self {
		Self::Message(message.into())
	}

	/// Preserves canonical structured JSON without stringifying it.
	pub const fn structured(payload: Bytes) -> Self {
		Self::Structured(payload)
	}
}

impl From<Str> for RegimeFailure {
	fn from(message: Str) -> Self {
		Self::Message(message)
	}
}

impl From<&'static str> for RegimeFailure {
	fn from(message: &'static str) -> Self {
		Self::Message(Str::new_static(message))
	}
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RegimeControl {
	Retry,
	Wait(WaitTicket),
	Reject(Str),
	Cancel(Str),
	Complete,
	Fail(RegimeFailure),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RegimeEffect {
	AppendContext(Vec<Item>),
	RewriteContext(ContextPatch),
	RequireTool(Str),
	SetScoped(ScopedSetting),
	ReplaceState(Str),
	Note(RegimeNote),
}
/// Typed durable side-record staged by a core lane and journaled by the
/// arbiter once the resolution lands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RegimeNote {
	/// One committed stream-rule injection, projected to extension hosts.
	TtsrInjection {
		/// Stream the rules matched on.
		source:  StreamSource,
		/// Matched rule names in delivery order.
		rules:   Vec<Str>,
		/// Injected reminder text.
		content: Str,
	},
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RegimeDraft {
	control: Option<RegimeControl>,
	effects: Vec<RegimeEffect>,
}
impl RegimeDraft {
	pub(crate) fn requests_retry(&self) -> bool {
		matches!(self.control, Some(RegimeControl::Retry))
	}

	/// Whether this draft selects no control and stages no effects.
	pub(crate) fn is_empty(&self) -> bool {
		self.control.is_none() && self.effects.is_empty()
	}
}

/// Typed failure returned when a regime handler cannot produce a valid draft.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RegimeError {
	/// The handler staged an effect that is invalid for the current event.
	#[error("regime effect is invalid for this event")]
	InvalidEffect,
	/// The handler's external adapter was unavailable.
	#[error("regime adapter is unavailable")]
	AdapterUnavailable,
	/// The handler's external adapter rejected the invocation.
	#[error("regime adapter rejected the invocation")]
	AdapterRejected,
}

/// Transactional effect writer for one isolated regime invocation.
pub struct RegimeContext<'a> {
	point:           Point,
	facts:           PointCx<'a>,
	activation:      &'a str,
	committed_steps: u32,
	effects:         &'a mut Vec<RegimeEffect>,
}

impl<'a> RegimeContext<'a> {
	/// Returns the fixed event currently being handled.
	pub const fn point(&self) -> Point {
		self.point
	}

	/// Returns immutable facts captured at the event boundary.
	pub const fn facts(&self) -> &PointCx<'a> {
		&self.facts
	}

	/// Returns the stable identity of the activation being invoked.
	pub const fn activation_id(&self) -> &str {
		self.activation
	}

	/// Returns the activation's committed-step count before this draft.
	pub const fn committed_steps(&self) -> u32 {
		self.committed_steps
	}

	/// Stages canonical context items for ordered append.
	pub fn append_context(&mut self, items: impl Into<Vec<Item>>) {
		self.effects.push(RegimeEffect::AppendContext(items.into()));
	}

	/// Stages an ordered provider-context rewrite.
	pub fn rewrite_context(&mut self, patch: ContextPatch) {
		self.effects.push(RegimeEffect::RewriteContext(patch));
	}

	/// Stages a required tool choice.
	pub fn require_tool(&mut self, tool: impl Into<Str>) {
		self.effects.push(RegimeEffect::RequireTool(tool.into()));
	}

	/// Stages an activation-scoped setting.
	pub fn set_scoped(&mut self, setting: ScopedSetting) {
		self.effects.push(RegimeEffect::SetScoped(setting));
	}

	/// Stages replacement of the regime's durable state.
	pub fn replace_state(&mut self, state: impl Into<Str>) {
		self.effects.push(RegimeEffect::ReplaceState(state.into()));
	}

	/// Stages one typed durable side-record journaled with the resolution.
	pub(crate) fn stage_note(&mut self, note: RegimeNote) {
		self.effects.push(RegimeEffect::Note(note));
	}
}

/// Consuming control selector for one isolated regime invocation.
#[must_use = "dropping Next selects no control"]
pub struct Next<'a> {
	control: &'a mut Option<RegimeControl>,
}

impl Next<'_> {
	fn select(self, control: RegimeControl) {
		*self.control = Some(control);
	}

	/// Requests another model/tool turn.
	pub fn retry(self) {
		self.select(RegimeControl::Retry);
	}

	/// Parks on a required-deadline ticket.
	pub fn wait(self, ticket: WaitTicket) {
		self.select(RegimeControl::Wait(ticket));
	}

	/// Rejects pending work with a unionable reason.
	pub fn reject(self, reason: impl Into<Str>) {
		self.select(RegimeControl::Reject(reason.into()));
	}

	/// Cancels active work with a durable reason.
	pub fn cancel(self, reason: impl Into<Str>) {
		self.select(RegimeControl::Cancel(reason.into()));
	}

	/// Completes this activation successfully.
	pub fn complete(self) {
		self.select(RegimeControl::Complete);
	}

	/// Fails this activation with a terminal diagnostic.
	pub fn fail(self, failure: impl Into<RegimeFailure>) {
		self.select(RegimeControl::Fail(failure.into()));
	}
}
pub(crate) fn evaluate_regime<F>(
	point: Point,
	facts: &PointCx<'_>,
	activation: &str,
	committed_steps: u32,
	handler: F,
) -> Result<RegimeDraft, RegimeError>
where
	F: for<'a> FnOnce(&mut RegimeContext<'a>, Next<'a>) -> Result<(), RegimeError>,
{
	let mut draft = RegimeDraft::default();
	let RegimeDraft { control, effects } = &mut draft;
	let mut context = RegimeContext { point, facts: *facts, activation, committed_steps, effects };
	handler(&mut context, Next { control })?;
	Ok(draft)
}

/// Data-only trigger evaluated by Core before starting a regime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RegimeWhen {
	/// Event whose facts may automatically start this declaration.
	pub point:             Point,
	/// Optional exact invocation identity.
	pub invocation_id:     Option<Str>,
	/// Optional streamed fragment substring.
	pub stream_contains:   Option<Str>,
	/// Optional delivered-effect predicate.
	pub delivered:         Option<bool>,
	/// Optional active-checkpoint predicate.
	pub checkpoint_active: Option<bool>,
}

impl RegimeWhen {
	/// Evaluates only immutable Core facts, without extension IPC.
	pub fn matches(&self, point: Point, cx: &PointCx<'_>) -> bool {
		self.point == point
			&& self
				.invocation_id
				.as_ref()
				.is_none_or(|expected| cx.invocation_id == Some(expected.as_str()))
			&& self.stream_contains.as_ref().is_none_or(|needle| {
				cx.stream_delta
					.is_some_and(|delta| delta.contains(needle.as_str()))
			}) && self
			.delivered
			.is_none_or(|delivered| cx.delivered == delivered)
			&& self
				.checkpoint_active
				.is_none_or(|active| cx.checkpoint_active == active)
	}
}

/// Immutable declaration shared by every activation of one regime.
#[derive(Clone)]
pub struct RegimeSpec {
	/// Stable declaration identity.
	pub id: RegimeId,
	/// Subscribed fixed events.
	pub events: PointSet,
	/// Higher values resolve first within one origin.
	pub precedence: i16,
	/// Maximum number of committed, non-waiting steps.
	pub max_steps: Option<u32>,
	/// Minimum milliseconds between committed steps.
	pub committed_step_interval_ms: Option<u64>,
	/// Whether the handler implements the same-shape limit callback.
	pub on_limit: bool,
	/// Activation lifetime.
	pub lifetime: RegimeLifetime,
	/// State schema identity (`family@rev`).
	pub family_rev: Str,
	/// Optional data-only Core-side auto-start predicate.
	pub when: Option<RegimeWhen>,
	/// Child specs whose lifetime is tied to this activation.
	/// Exclusive resources acquired atomically at start.
	pub owns: Arc<[Resource]>,
	/// Activation-scoped settings installed after ownership is granted.
	pub sets: Arc<[ScopedSetting]>,
	/// Minimum residence before an ordinary stop.
	pub minimum_duration_ms: Option<u64>,
}

/// Returns the core plan regime declaration.
pub fn plan_regime_spec() -> RegimeSpec {
	regime_spec("plan", [Resource::Mode, Resource::Worktree])
}

/// Returns the core vibe regime declaration.
pub fn vibe_regime_spec() -> RegimeSpec {
	regime_spec("vibe", [Resource::Mode, Resource::Director])
}

/// Returns the one-shot cheap-model prewalk declaration.
pub fn prewalk_regime_spec() -> RegimeSpec {
	let mut spec = regime_spec("prewalk", [Resource::Mode]);
	spec.sets = Arc::from([
		ScopedSetting { slot: SettingSlot::PromptSlot, value: Str::new_static("prewalk") },
		ScopedSetting { slot: SettingSlot::ModelRoute, value: Str::new_static("smol") },
	]);
	spec
}

/// Returns the core goal regime declaration.
pub fn goal_regime_spec() -> RegimeSpec {
	let mut spec = regime_spec("goal", [Resource::Mode]);
	spec.events = Point::Context.set();
	spec.family_rev = Str::new_static("dev.omp.core.goal@1");
	spec
}

/// Returns the core autoresearch regime declaration.
pub fn autoresearch_regime_spec() -> RegimeSpec {
	regime_spec("autoresearch", [Resource::Mode, Resource::Worktree])
}

fn regime_spec<const N: usize>(id: &'static str, owns: [Resource; N]) -> RegimeSpec {
	RegimeSpec {
		id: Str::new_static(id),
		events: PointSet::EMPTY,
		precedence: 0,
		max_steps: None,
		committed_step_interval_ms: None,
		on_limit: false,
		lifetime: RegimeLifetime::Session,
		family_rev: Str::new_static("dev.omp.core.regime@1"),
		when: None,
		owns: Arc::from(owns),
		sets: Arc::from([ScopedSetting {
			slot:  SettingSlot::PromptSlot,
			value: Str::new_static(id),
		}]),
		minimum_duration_ms: None,
	}
}

/// Stateful core or extension handler evaluated at subscribed events.
pub trait Regime: Send + Sync + 'static {
	/// Stages one isolated transaction.
	fn apply(&mut self, ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError>;

	/// Stages the same-shape transaction after `max_steps` is reached.
	fn on_limit(&mut self, _: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
		next.complete();
		Ok(())
	}

	/// Returns the durable state payload for journaling.
	fn state(&self) -> Str;

	/// Restores a payload after the declaration's state revision is validated.
	fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError>;

	/// Applies a live state update. Revival continues to call [`Self::restore`].
	fn update(&mut self, payload: &[u8]) -> Result<(), RegimeStateError> {
		let payload = str::from_utf8(payload).map_err(|_| RegimeStateError::InvalidPayload)?;
		self.restore(payload)
	}
}

/// Stateless handler backing built-in session regimes.
#[derive(Default)]
pub struct BuiltinRegime;

impl Regime for BuiltinRegime {
	fn apply(&mut self, _: &mut RegimeContext<'_>, _: Next<'_>) -> Result<(), RegimeError> {
		Ok(())
	}

	fn state(&self) -> Str {
		Str::new_static("{}")
	}

	fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
		if payload == "{}" {
			Ok(())
		} else {
			Err(RegimeStateError::InvalidPayload)
		}
	}
}

/// Durable state owned by the built-in goal regime.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalRegimeState {
	/// Durable objective supplied by the user.
	pub objective:          Str,
	/// Optional hard token budget.
	pub budget_tokens:      Option<u64>,
	/// Tokens charged to the objective so far.
	pub spent_tokens:       u64,
	/// Bitset for delivered 50%, 75%, and 90% transition steers.
	pub thresholds_crossed: u8,
}

/// Stateful goal regime that emits each budget-transition steer once.
#[derive(Default)]
pub struct GoalRegime {
	state:   GoalRegimeState,
	pending: u8,
}

impl GoalRegime {
	fn crossed(state: &GoalRegimeState) -> u8 {
		let Some(budget) = state.budget_tokens.filter(|budget| *budget != 0) else {
			return 0;
		};
		let ratio = state.spent_tokens.saturating_mul(100) / budget;
		u8::from(ratio >= 50) | (u8::from(ratio >= 75) << 1) | (u8::from(ratio >= 90) << 2)
	}
}

impl Regime for GoalRegime {
	fn apply(&mut self, ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
		if ctx.point() != Point::Context {
			return Ok(());
		}
		if self
			.state
			.budget_tokens
			.is_some_and(|budget| self.state.spent_tokens >= budget)
		{
			next.fail("goal token budget exhausted");
			return Ok(());
		}
		if self.pending == 0 {
			return Ok(());
		}
		let bit = self.pending.trailing_zeros() as u8;
		self.pending &= !(1 << bit);
		let percent = [50, 75, 90][usize::from(bit)];
		ctx.append_context(vec![regime_message(format!(
			"Goal budget reached {percent}% ({} tokens spent). Reassess progress against the \
			 objective and preserve budget for the highest-value remaining work.",
			self.state.spent_tokens,
		))]);
		Ok(())
	}

	fn state(&self) -> Str {
		Str::from(
			serde_json::to_string(&self.state)
				.expect("goal regime state has infallible JSON serialization"),
		)
	}

	fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
		self.state = serde_json::from_str(payload).map_err(|_| RegimeStateError::InvalidPayload)?;
		self.pending = 0;
		Ok(())
	}

	fn update(&mut self, payload: &[u8]) -> Result<(), RegimeStateError> {
		let mut next: GoalRegimeState =
			serde_json::from_slice(payload).map_err(|_| RegimeStateError::InvalidPayload)?;
		let crossed = Self::crossed(&next);
		self.pending |= crossed & !self.state.thresholds_crossed;
		next.thresholds_crossed |= crossed | self.state.thresholds_crossed;
		self.state = next;
		Ok(())
	}
}

/// Resolves one built-in regime declaration and its machine.
pub fn core_regime(id: &str) -> Option<(Arc<RegimeSpec>, Box<dyn Regime>)> {
	let (spec, machine): (RegimeSpec, Box<dyn Regime>) = match id {
		"plan" => (plan_regime_spec(), Box::new(BuiltinRegime)),
		"vibe" => (vibe_regime_spec(), Box::new(BuiltinRegime)),
		"prewalk" => (prewalk_regime_spec(), Box::new(BuiltinRegime)),
		"goal" => (goal_regime_spec(), Box::new(GoalRegime::default())),
		"autoresearch" => (autoresearch_regime_spec(), Box::new(BuiltinRegime)),
		_ => return None,
	};
	Some((Arc::new(spec), machine))
}

/// Failure to restore a declared state family.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RegimeStateError {
	/// The payload could not be decoded by its machine.
	#[error("regime state payload is invalid")]
	InvalidPayload,
	/// No active or queued activation matched a requested update.
	#[error("regime activation is not active")]
	MissingActivation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum ResolutionKind {
	None,
	Retry,
	Tool,
	Reject,
	Wait,
	Cancel,
	Fail,
}

#[derive(Clone, Debug)]
pub(crate) struct RegimeResolution {
	pub(crate) control:                ResolutionKind,
	pub(crate) controlling_activation: Option<ActivationId>,
	pub(crate) cancel_reason:          Option<Str>,
	pub(crate) patches:                Vec<ContextPatch>,
	pub(crate) injects:                Vec<Item>,
	pub(crate) notes:                  Vec<RegimeNote>,
	pub(crate) denials:                Vec<Str>,
	pub(crate) waits:                  Vec<WaitTicket>,
	pub(crate) settings:               Vec<ScopedSetting>,
	pub(crate) participants:           Vec<ActivationId>,
	pub(crate) terminated:             Vec<ActivationId>,
	pub(crate) failures:               Vec<RegimeFailure>,
}

impl Default for RegimeResolution {
	fn default() -> Self {
		Self {
			control:                ResolutionKind::None,
			controlling_activation: None,
			cancel_reason:          None,
			patches:                Vec::new(),
			injects:                Vec::new(),
			notes:                  Vec::new(),
			denials:                Vec::new(),
			waits:                  Vec::new(),
			settings:               Vec::new(),
			participants:           Vec::new(),
			terminated:             Vec::new(),
			failures:               Vec::new(),
		}
	}
}

#[derive(Clone, Debug)]
struct ResourceLease {
	resource:   Resource,
	activation: ActivationId,
	since:      u64,
}

#[derive(Clone, Debug)]
struct QueuedAcquire {
	activation: ActivationId,
	since:      u64,
}

/// Result of acquiring one exclusive resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcquireOutcome {
	/// The requester owns the resource now.
	Granted,
	/// The requester entered the FIFO behind the current owner.
	Queued {
		/// Current resource owner.
		holder: ActivationId,
		/// Epoch millisecond at which the holder activated.
		since:  u64,
	},
	/// The current owner rejected a non-queueing request.
	Denied {
		/// Current resource owner.
		holder: ActivationId,
		/// Epoch millisecond at which the holder activated.
		since:  u64,
	},
}

/// A regime declaration references an unknown canonical resource.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DeclareError {
	/// The resource is absent from the core resource table.
	#[error("regime declaration names an unknown resource")]
	UnknownResource {
		/// Unknown resource.
		resource: Resource,
	},
}

/// Exclusive resource leases, FIFO acquisition queues, and scoped settings.
pub struct ResourceRegistry {
	declarations: BTreeMap<Str, ResourceDecl>,
	leases:       BTreeMap<Str, ResourceLease>,
	queues:       BTreeMap<Str, VecDeque<QueuedAcquire>>,
	settings:     BTreeMap<SettingSlot, Vec<(ActivationId, Str)>>,
}

impl Default for ResourceRegistry {
	fn default() -> Self {
		let declarations = RESOURCE_TABLE
			.into_iter()
			.map(|declaration| (Str::new_static(declaration.name), declaration))
			.collect();
		Self {
			declarations,
			leases: BTreeMap::new(),
			queues: BTreeMap::new(),
			settings: BTreeMap::new(),
		}
	}
}

impl ResourceRegistry {
	/// Returns the canonical declaration for one resource.
	pub fn declaration(&self, resource: &Resource) -> Option<&ResourceDecl> {
		self.declarations.get(resource.name())
	}

	/// Validates every resource named by a regime declaration.
	pub(crate) fn declare(&self, spec: &RegimeSpec) -> Result<(), DeclareError> {
		for resource in spec.owns.iter() {
			if self.declaration(resource).is_none() {
				return Err(DeclareError::UnknownResource { resource: resource.clone() });
			}
		}
		Ok(())
	}

	/// Attempts to acquire one exclusive resource.
	pub fn acquire(
		&mut self,
		resource: Resource,
		activation: ActivationId,
		since: u64,
		queue: bool,
	) -> Result<AcquireOutcome, DeclareError> {
		let declaration = self
			.declaration(&resource)
			.copied()
			.ok_or_else(|| DeclareError::UnknownResource { resource: resource.clone() })?;
		if let Some(lease) = self.leases.get(resource.name()) {
			if lease.activation == activation {
				return Ok(AcquireOutcome::Granted);
			}
			let outcome = if queue && declaration.queueable {
				let waiting = self.queues.entry(Str::new(resource.name())).or_default();
				if !waiting
					.iter()
					.any(|candidate| candidate.activation == activation)
				{
					waiting.push_back(QueuedAcquire { activation, since });
				}
				AcquireOutcome::Queued { holder: lease.activation.clone(), since: lease.since }
			} else {
				AcquireOutcome::Denied { holder: lease.activation.clone(), since: lease.since }
			};
			return Ok(outcome);
		}
		self
			.leases
			.insert(Str::new(resource.name()), ResourceLease { resource, activation, since });
		Ok(AcquireOutcome::Granted)
	}

	/// Releases every lease and scoped setting owned by an activation.
	pub fn release(&mut self, activation: &str) -> Vec<(Resource, ActivationId)> {
		let released: Vec<_> = self
			.leases
			.iter()
			.filter(|(_, lease)| lease.activation == activation)
			.map(|(name, lease)| (name.clone(), lease.resource.clone()))
			.collect();
		let mut granted = Vec::new();
		for (name, resource) in released {
			self.leases.remove(name.as_str());
			if let Some(next) = self
				.queues
				.get_mut(name.as_str())
				.and_then(VecDeque::pop_front)
			{
				self.leases.insert(name, ResourceLease {
					resource:   resource.clone(),
					activation: next.activation.clone(),
					since:      next.since,
				});
				granted.push((resource, next.activation));
			}
		}
		for waiting in self.queues.values_mut() {
			waiting.retain(|candidate| candidate.activation != activation);
		}
		for stack in self.settings.values_mut() {
			stack.retain(|(owner, _)| owner != activation);
		}
		granted
	}

	/// Pushes one activation-scoped value.
	pub fn set(&mut self, activation: ActivationId, setting: ScopedSetting) {
		self
			.settings
			.entry(setting.slot)
			.or_default()
			.push((activation, setting.value));
	}

	/// Reads the current scoped value without allocating.
	pub fn current(&self, slot: &SettingSlot) -> Option<&str> {
		self
			.settings
			.get(slot)
			.and_then(|stack| stack.last())
			.map(|(_, value)| value.as_str())
	}

	/// Pops the current scoped value.
	pub fn pop(&mut self, slot: &SettingSlot) -> Option<(ActivationId, Str)> {
		self.settings.get_mut(slot).and_then(Vec::pop)
	}

	/// Returns the current exclusive owner.
	pub fn owner(&self, resource: &Resource) -> Option<&str> {
		self
			.leases
			.get(resource.name())
			.map(|lease| lease.activation.as_str())
	}

	/// Returns holder identity and activation time for an occupied resource.
	pub fn holder(&self, resource: &Resource) -> Option<(&str, u64)> {
		self
			.leases
			.get(resource.name())
			.map(|lease| (lease.activation.as_str(), lease.since))
	}

	/// Returns the durable FIFO depth for one resource.
	pub fn queue_depth(&self, resource: &Resource) -> usize {
		self.queues.get(resource.name()).map_or(0, VecDeque::len)
	}
}

#[derive(Clone, Copy, Debug)]
enum ForceFeedback {
	Resolved,
	Rejected,
}

#[derive(Clone, Debug)]
struct ForceEvent {
	activation: ActivationId,
	outcome:    ForceFeedback,
}

struct Activation {
	spec:                   Arc<RegimeSpec>,
	id:                     ActivationId,
	activated_at:           Ulid,
	activated_since_ms:     u64,
	committed_steps:        u32,
	last_committed_step_at: Option<u64>,
	machine:                Box<dyn Regime>,
	parent:                 Option<ActivationId>,
	last:                   Option<ResolutionKind>,
	queued:                 bool,
}

/// Options controlling resource acquisition for one regime start.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StartOptions {
	/// Epoch millisecond recorded as the resource-owner start.
	pub now_ms: u64,
	/// Enter each occupied resource's durable FIFO.
	pub queue:  bool,
}

/// Result of an accepted active or queued regime start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartReceipt {
	/// Stable activation or queue-ticket identity.
	pub activation: ActivationId,
	/// Conflicting resource for a queued ticket; absent for an immediate grant.
	pub resource:   Option<Resource>,
	/// Aggregate resource acquisition result.
	pub outcome:    AcquireOutcome,
}

/// Result of advancing committed-step accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegimeStepResult {
	/// No activation matched the identity.
	Missing,
	/// The activation remains below its bound.
	Advanced {
		/// Number of committed steps after the advance.
		committed_steps: u32,
	},
	/// The activation reached its declared bound.
	Limited {
		/// Number of committed steps at the bound.
		committed_steps: u32,
	},
}

/// Durable owner of active and queued regime activations.
pub struct RegimeSet {
	activations: BTreeMap<ActivationId, Activation>,
	resources:   ResourceRegistry,
	force_tx:    flume::Sender<ForceEvent>,
	force_rx:    Receiver<ForceEvent>,
}

impl Default for RegimeSet {
	fn default() -> Self {
		let (force_tx, force_rx) = flume::unbounded();
		Self {
			activations: BTreeMap::new(),
			resources: ResourceRegistry::default(),
			force_tx,
			force_rx,
		}
	}
}

/// Failure to start a regime.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StartError {
	/// The declaration is invalid.
	#[error(transparent)]
	Declare(#[from] DeclareError),
	/// Another activation already uses the same identity.
	#[error("regime activation identity is already active")]
	Duplicate,
	/// A named exclusive resource rejected this activation.
	#[error("regime resource acquisition was denied")]
	Acquire {
		/// Conflicting resource.
		resource: Resource,
		/// Structured acquisition result, normally [`AcquireOutcome::Denied`].
		outcome:  AcquireOutcome,
	},
}

/// Failure to stop a regime activation.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StopError {
	/// An ordinary stop arrived before the minimum duration elapsed.
	#[error("regime minimum duration has not elapsed")]
	MinimumDuration {
		/// Activation refusing the stop.
		activation: ActivationId,
		/// Earliest permitted stop, as epoch milliseconds.
		until_ms:   u64,
	},
}

impl RegimeSet {
	/// Creates an empty durable regime owner.
	pub fn new() -> Self {
		Self::default()
	}

	/// Validates a declaration against the canonical resource table.
	pub fn declare(&self, spec: &RegimeSpec) -> Result<(), DeclareError> {
		self.resources.declare(spec)
	}

	/// Starts one handler after atomically acquiring all declared resources.
	pub fn start(
		&mut self,
		spec: Arc<RegimeSpec>,
		machine: Box<dyn Regime>,
		options: StartOptions,
	) -> Result<StartReceipt, StartError> {
		self.start_child(spec, machine, None, options)
	}

	/// Starts a declared handler only when its Core-side predicate matches.
	pub fn start_when(
		&mut self,
		spec: Arc<RegimeSpec>,
		machine: Box<dyn Regime>,
		options: StartOptions,
		point: Point,
		cx: &PointCx<'_>,
	) -> Result<Option<StartReceipt>, StartError> {
		if !spec
			.when
			.as_ref()
			.is_some_and(|when| when.matches(point, cx))
		{
			return Ok(None);
		}
		self.start(spec, machine, options).map(Some)
	}

	/// Starts a lifetime-tied child of an existing activation.
	pub fn start_child(
		&mut self,
		spec: Arc<RegimeSpec>,
		machine: Box<dyn Regime>,
		parent: Option<ActivationId>,
		options: StartOptions,
	) -> Result<StartReceipt, StartError> {
		self.declare(&spec)?;
		let activated_at = Ulid::generate();
		let id = Str::from(activated_at.to_string());
		if self.activations.contains_key(id.as_str()) {
			return Err(StartError::Duplicate);
		}
		let mut conflict = None;
		for resource in spec.owns.iter() {
			match self
				.resources
				.acquire(resource.clone(), id.clone(), options.now_ms, false)?
			{
				AcquireOutcome::Granted => {},
				outcome @ (AcquireOutcome::Denied { .. } | AcquireOutcome::Queued { .. }) => {
					conflict = Some((resource.clone(), outcome));
					break;
				},
			}
		}
		if let Some((resource, denied)) = conflict {
			self.resources.release(id.as_str());
			if !options.queue {
				return Err(StartError::Acquire { resource, outcome: denied });
			}
			let outcome =
				self
					.resources
					.acquire(resource.clone(), id.clone(), options.now_ms, true)?;
			if matches!(outcome, AcquireOutcome::Denied { .. }) {
				return Err(StartError::Acquire { resource, outcome });
			}
			self.activations.insert(id.clone(), Activation {
				spec,
				id: id.clone(),
				activated_at,
				activated_since_ms: options.now_ms,
				committed_steps: 0,
				last_committed_step_at: None,
				machine,
				parent,
				last: None,
				queued: true,
			});
			return Ok(StartReceipt { activation: id, resource: Some(resource), outcome });
		}
		for setting in spec.sets.iter().cloned() {
			self.resources.set(id.clone(), setting);
		}
		self.activations.insert(id.clone(), Activation {
			spec,
			id: id.clone(),
			activated_at,
			activated_since_ms: options.now_ms,
			committed_steps: 0,
			last_committed_step_at: None,
			machine,
			parent,
			last: None,
			queued: false,
		});
		Ok(StartReceipt { activation: id, resource: None, outcome: AcquireOutcome::Granted })
	}

	/// Checks whether an activation may stop without mutating it.
	pub fn check_stop(&self, activation: &str, now_ms: u64) -> Result<bool, StopError> {
		let Some(active) = self.activations.get(activation) else {
			return Ok(false);
		};
		if !active.queued
			&& let Some(minimum) = active.spec.minimum_duration_ms
		{
			let until_ms = active.activated_since_ms.saturating_add(minimum);
			if now_ms < until_ms {
				return Err(StopError::MinimumDuration { activation: active.id.clone(), until_ms });
			}
		}
		Ok(true)
	}

	/// Stops an activation and its complete child subtree after minimum
	/// duration.
	pub fn stop(&mut self, activation: &str, now_ms: u64) -> Result<bool, StopError> {
		if !self.check_stop(activation, now_ms)? {
			return Ok(false);
		}
		Ok(self.remove_subtree(activation))
	}

	/// Cancels an activation and its child subtree immediately.
	pub fn cancel(&mut self, activation: &str) -> bool {
		self.remove_subtree(activation)
	}

	/// Completes an activation after its minimum duration.
	pub fn complete(&mut self, activation: &str, now_ms: u64) -> Result<bool, StopError> {
		self.stop(activation, now_ms)
	}

	/// Advances committed-step accounting for one activation.
	pub fn advance(&mut self, activation: &str, now_ms: u64) -> RegimeStepResult {
		let Some(active) = self.activations.get_mut(activation) else {
			return RegimeStepResult::Missing;
		};
		if active
			.spec
			.committed_step_interval_ms
			.zip(active.last_committed_step_at)
			.is_some_and(|(interval, last)| now_ms < last.saturating_add(interval))
		{
			return RegimeStepResult::Advanced { committed_steps: active.committed_steps };
		}
		if active
			.spec
			.max_steps
			.is_some_and(|limit| active.committed_steps >= limit)
		{
			return RegimeStepResult::Limited { committed_steps: active.committed_steps };
		}
		active.committed_steps = active.committed_steps.saturating_add(1);
		active.last_committed_step_at = Some(now_ms);
		if active
			.spec
			.max_steps
			.is_some_and(|limit| active.committed_steps >= limit)
		{
			RegimeStepResult::Limited { committed_steps: active.committed_steps }
		} else {
			RegimeStepResult::Advanced { committed_steps: active.committed_steps }
		}
	}

	fn remove_subtree(&mut self, activation: &str) -> bool {
		let mut subtree = vec![Str::new(activation)];
		let mut position = 0;
		while position < subtree.len() {
			let parent = subtree[position].clone();
			for child in self.activations.values() {
				if child
					.parent
					.as_ref()
					.is_some_and(|candidate| candidate == &parent)
					&& !subtree.contains(&child.id)
				{
					subtree.push(child.id.clone());
				}
			}
			position += 1;
		}
		let mut removed = false;
		let mut grants = Vec::new();
		for id in subtree.into_iter().rev() {
			removed |= self.activations.remove(id.as_str()).is_some();
			grants.extend(self.resources.release(id.as_str()));
		}
		for (_, granted) in grants {
			self.activate_grant(granted);
		}
		removed
	}

	fn activate_grant(&mut self, activation: ActivationId) {
		let Some(active) = self.activations.get(&activation) else {
			self.resources.release(activation.as_str());
			return;
		};
		if !active.queued {
			return;
		}
		let owns = Arc::clone(&active.spec.owns);
		let since = active.activated_since_ms;
		for resource in owns.iter() {
			match self
				.resources
				.acquire(resource.clone(), activation.clone(), since, false)
			{
				Ok(AcquireOutcome::Granted) => {},
				Ok(AcquireOutcome::Denied { .. } | AcquireOutcome::Queued { .. }) => {
					self.resources.release(activation.as_str());
					let _ = self
						.resources
						.acquire(resource.clone(), activation.clone(), since, true);
					return;
				},
				Err(_) => {
					self.resources.release(activation.as_str());
					return;
				},
			}
		}
		if let Some(active) = self.activations.get_mut(&activation) {
			for setting in active.spec.sets.iter().cloned() {
				self.resources.set(activation.clone(), setting);
			}
			active.queued = false;
		}
	}

	/// Resolves every active regime subscribed to `point` in deterministic
	/// order.
	pub(crate) fn resolve(
		&mut self,
		point: Point,
		cx: &PointCx<'_>,
		tool_choices: Option<&mut ToolChoiceQueue>,
	) -> RegimeResolution {
		self.apply_force_feedback();
		let mut order: Vec<_> = self
			.activations
			.values()
			.filter(|active| !active.queued && active.spec.events.contains(point))
			.map(|active| (active.spec.precedence, active.activated_at, active.id.clone()))
			.collect();
		order.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));

		let mut resolution = RegimeResolution::default();
		let mut tools = Vec::new();
		let mut committed = Vec::new();
		let mut failed = Vec::new();
		for (_, _, id) in order {
			resolution.participants.push(id.clone());
			let result = {
				let Some(active) = self.activations.get_mut(id.as_str()) else {
					continue;
				};
				if active
					.spec
					.committed_step_interval_ms
					.zip(active.last_committed_step_at)
					.is_some_and(|(interval, last)| cx.now_ms < last.saturating_add(interval))
				{
					continue;
				}
				let mut draft = RegimeDraft::default();
				let at_limit = active
					.spec
					.max_steps
					.is_some_and(|limit| active.committed_steps >= limit);
				let apply_result = {
					let RegimeDraft { control, effects } = &mut draft;
					let mut context = RegimeContext {
						point,
						facts: *cx,
						activation: id.as_str(),
						committed_steps: active.committed_steps,
						effects,
					};
					let next = Next { control };
					if at_limit {
						if active.spec.on_limit {
							active.machine.on_limit(&mut context, next)
						} else {
							next.complete();
							Ok(())
						}
					} else {
						active.machine.apply(&mut context, next)
					}
				};
				if apply_result.is_err() {
					Err(())
				} else if let Some(state) = draft.effects.iter().rev().find_map(|effect| match effect {
					RegimeEffect::ReplaceState(state) => Some(state),
					_ => None,
				}) {
					active
						.machine
						.restore(state.as_str())
						.map(|()| draft)
						.map_err(|_| ())
				} else {
					Ok(draft)
				}
			};
			let Ok(draft) = result else {
				failed.push(id.clone());
				resolution
					.failures
					.push(RegimeFailure::message("regime handler failed"));
				select_resolution(&mut resolution, ResolutionKind::Fail, &id);
				continue;
			};
			let has_tool = draft
				.effects
				.iter()
				.any(|effect| matches!(effect, RegimeEffect::RequireTool(_)));
			let waiting = matches!(draft.control, Some(RegimeControl::Wait(_)));
			let has_commit = draft.control.is_some() || !draft.effects.is_empty();
			for effect in draft.effects {
				match effect {
					RegimeEffect::AppendContext(mut items) => resolution.injects.append(&mut items),
					RegimeEffect::RewriteContext(patch) => resolution.patches.push(patch),
					RegimeEffect::RequireTool(tool) => tools.push((id.clone(), tool)),
					RegimeEffect::SetScoped(setting) => {
						self.resources.set(id.clone(), setting.clone());
						resolution.settings.push(setting);
					},
					RegimeEffect::ReplaceState(_) => {},
					RegimeEffect::Note(note) => resolution.notes.push(note),
				}
			}
			match draft.control {
				None => {},
				Some(RegimeControl::Retry) => {
					select_resolution(&mut resolution, ResolutionKind::Retry, &id)
				},
				Some(RegimeControl::Wait(ticket)) => {
					resolution.waits.push(ticket);
					select_resolution(&mut resolution, ResolutionKind::Wait, &id);
				},
				Some(RegimeControl::Reject(reason)) => {
					resolution.denials.push(reason);
					select_resolution(&mut resolution, ResolutionKind::Reject, &id);
				},
				Some(RegimeControl::Cancel(reason)) => {
					select_resolution(&mut resolution, ResolutionKind::Cancel, &id);
					if resolution.controlling_activation.as_deref() == Some(id.as_str()) {
						resolution.cancel_reason = Some(reason);
					}
				},
				Some(RegimeControl::Complete) => resolution.terminated.push(id.clone()),
				Some(RegimeControl::Fail(detail)) => {
					resolution.failures.push(detail);
					failed.push(id.clone());
					select_resolution(&mut resolution, ResolutionKind::Fail, &id);
				},
			}
			if cx.delivered && has_commit && !waiting && !has_tool {
				committed.push(id);
			}
		}

		if let Some((id, _)) = tools.first() {
			if !matches!(
				resolution.control,
				ResolutionKind::Cancel
					| ResolutionKind::Wait
					| ResolutionKind::Reject
					| ResolutionKind::Fail
			) {
				resolution.control = ResolutionKind::Tool;
				resolution.controlling_activation = Some(id.clone());
			}
			if let Some(queue) = tool_choices {
				for (position, (activation, tool)) in tools.into_iter().enumerate() {
					self.queue_force(queue, activation, tool, position == 0);
				}
			}
		}

		for participant in &resolution.participants {
			if let Some(active) = self.activations.get_mut(participant.as_str()) {
				active.last = Some(resolution.control);
			}
		}
		for activation in committed {
			let _ = self.advance(activation.as_str(), cx.now_ms);
		}
		for activation in failed {
			self.cancel(activation.as_str());
		}
		let mut terminated = Vec::new();
		for activation in mem::take(&mut resolution.terminated) {
			if matches!(self.complete(activation.as_str(), cx.now_ms), Ok(true)) {
				terminated.push(activation);
			}
		}
		resolution.terminated = terminated;
		resolution
	}

	/// Returns the active and queued activation count.
	pub fn len(&self) -> usize {
		self.activations.len()
	}

	/// Returns whether no regime is active or queued.
	pub fn is_empty(&self) -> bool {
		self.activations.is_empty()
	}

	/// Returns the current resource and scoped-setting registry.
	pub const fn resources(&self) -> &ResourceRegistry {
		&self.resources
	}

	/// Returns mutable access for loop-owned one-shot setting pops.
	pub(crate) const fn resources_mut(&mut self) -> &mut ResourceRegistry {
		&mut self.resources
	}

	/// Resolves an activation identity to its stable declaration identity.
	pub fn spec_id(&self, activation: &str) -> Option<&str> {
		self
			.activations
			.get(activation)
			.map(|active| active.spec.id.as_str())
	}

	/// Returns whether an accepted activation is waiting for a resource.
	pub fn is_queued(&self, activation: &str) -> bool {
		self
			.activations
			.get(activation)
			.is_some_and(|active| active.queued)
	}

	/// Applies a live handler-state update and returns its durable record.
	pub fn update_state(
		&mut self,
		activation: &str,
		payload: &[u8],
	) -> Result<RegimeRecord, RegimeStateError> {
		let active = self
			.activations
			.get_mut(activation)
			.ok_or(RegimeStateError::MissingActivation)?;
		active.machine.update(payload)?;
		self
			.records()
			.into_iter()
			.find(|record| record.activation == activation)
			.ok_or(RegimeStateError::MissingActivation)
	}

	/// Produces durable records for every active activation and queue ticket.
	pub fn records(&self) -> Vec<RegimeRecord> {
		self
			.activations
			.values()
			.map(|active| RegimeRecord {
				spec_id:            active.spec.id.clone(),
				family_rev:         active.spec.family_rev.clone(),
				state:              active.machine.state(),
				committed_steps:    active.committed_steps,
				activation:         active.id.clone(),
				activated_at:       active.activated_at.to_string().into(),
				activated_since_ms: active.activated_since_ms,
				parent:             active.parent.clone(),
				status:             if active.queued {
					RegimeStatus::Queued
				} else {
					RegimeStatus::Active
				},
			})
			.collect()
	}

	/// Rebuilds active activations from their latest durable records.
	pub fn revive<F>(
		&mut self,
		records: impl IntoIterator<Item = RegimeRecord>,
		mut resolve: F,
	) -> RevivalReport
	where
		F: FnMut(&str) -> Option<(Arc<RegimeSpec>, Box<dyn Regime>)>,
	{
		let mut report = RevivalReport::default();
		let mut records = records.into_iter().collect::<Vec<_>>();
		records.sort_by(|left, right| left.activated_at.cmp(&right.activated_at));
		for mut record in records {
			if !matches!(record.status, RegimeStatus::Active | RegimeStatus::Queued) {
				continue;
			}
			let Some((spec, mut machine)) = resolve(record.spec_id.as_str()) else {
				record.status = RegimeStatus::Failed;
				report.failed.push(record);
				continue;
			};
			let Ok(activated_at) = Ulid::from_string(record.activated_at.as_str()) else {
				record.status = RegimeStatus::Failed;
				report.failed.push(record);
				continue;
			};
			if spec.family_rev != record.family_rev
				|| self.declare(&spec).is_err()
				|| machine.restore(record.state.as_str()).is_err()
			{
				record.status = RegimeStatus::Failed;
				report.failed.push(record);
				continue;
			}
			let wants_queue = record.status == RegimeStatus::Queued;
			let mut queued = false;
			let mut acquired = true;
			for resource in spec.owns.iter() {
				let outcome = self.resources.acquire(
					resource.clone(),
					record.activation.clone(),
					record.activated_since_ms,
					wants_queue,
				);
				if wants_queue {
					if !matches!(outcome, Ok(AcquireOutcome::Granted | AcquireOutcome::Queued { .. })) {
						acquired = false;
						break;
					}
					if matches!(outcome, Ok(AcquireOutcome::Queued { .. })) {
						queued = true;
						break;
					}
				} else if !matches!(outcome, Ok(AcquireOutcome::Granted)) {
					acquired = false;
					break;
				}
			}
			if !acquired {
				self.resources.release(record.activation.as_str());
				record.status = RegimeStatus::Failed;
				report.failed.push(record);
				continue;
			}
			self
				.activations
				.insert(record.activation.clone(), Activation {
					spec,
					id: record.activation.clone(),
					activated_at,
					activated_since_ms: record.activated_since_ms,
					committed_steps: record.committed_steps,
					last_committed_step_at: None,
					machine,
					parent: record.parent.clone(),
					last: None,
					queued,
				});
			if !queued && let Some(active) = self.activations.get(record.activation.as_str()) {
				for setting in active.spec.sets.iter().cloned() {
					self.resources.set(record.activation.clone(), setting);
				}
			}
			report.resumed.push(record.activation);
		}
		report
	}

	fn queue_force(
		&self,
		queue: &mut ToolChoiceQueue,
		activation: ActivationId,
		tool: Str,
		head: bool,
	) {
		let resolved_tx = self.force_tx.clone();
		let rejected_tx = self.force_tx.clone();
		let resolved_id = activation.clone();
		let rejected_id = activation.clone();
		queue.push_once(ToolChoice::Named(tool), PushOptions {
			priority:  if head {
				DirectivePriority::Head
			} else {
				DirectivePriority::Tail
			},
			label:     Some(activation),
			callbacks: DirectiveCallbacks {
				on_resolved: Some(Arc::new(move |_| {
					let _ = resolved_tx.send(ForceEvent {
						activation: resolved_id.clone(),
						outcome:    ForceFeedback::Resolved,
					});
				})),
				on_rejected: Some(Arc::new(move |_| {
					let _ = rejected_tx.send(ForceEvent {
						activation: rejected_id.clone(),
						outcome:    ForceFeedback::Rejected,
					});
					RejectOutcome::Drop
				})),
			},
		});
	}

	fn apply_force_feedback(&mut self) {
		while let Ok(event) = self.force_rx.try_recv() {
			if matches!(event.outcome, ForceFeedback::Resolved) {
				let _ = self.advance(event.activation.as_str(), now_ms());
			}
		}
	}
}

fn resolution_rank(kind: ResolutionKind) -> u8 {
	match kind {
		ResolutionKind::None => 0,
		ResolutionKind::Retry => 1,
		ResolutionKind::Tool => 2,
		ResolutionKind::Reject => 3,
		ResolutionKind::Wait => 4,
		ResolutionKind::Cancel => 5,
		ResolutionKind::Fail => 6,
	}
}

fn select_resolution(
	resolution: &mut RegimeResolution,
	candidate: ResolutionKind,
	activation: &ActivationId,
) {
	if resolution_rank(candidate) > resolution_rank(resolution.control) {
		resolution.control = candidate;
		resolution.controlling_activation = Some(activation.clone());
	}
}

pub(crate) fn absorb_draft(
	resolution: &mut RegimeResolution,
	activation: ActivationId,
	draft: RegimeDraft,
) {
	resolution.participants.push(activation.clone());
	for effect in draft.effects {
		match effect {
			RegimeEffect::AppendContext(mut items) => resolution.injects.append(&mut items),
			RegimeEffect::RewriteContext(patch) => resolution.patches.push(patch),
			RegimeEffect::SetScoped(setting) => resolution.settings.push(setting),
			RegimeEffect::Note(note) => resolution.notes.push(note),
			RegimeEffect::RequireTool(_) | RegimeEffect::ReplaceState(_) => {},
		}
	}
	match draft.control {
		None => {},
		Some(RegimeControl::Retry) => {
			select_resolution(resolution, ResolutionKind::Retry, &activation);
		},
		Some(RegimeControl::Wait(ticket)) => {
			resolution.waits.push(ticket);
			select_resolution(resolution, ResolutionKind::Wait, &activation);
		},
		Some(RegimeControl::Reject(reason)) => {
			resolution.denials.push(reason);
			select_resolution(resolution, ResolutionKind::Reject, &activation);
		},
		Some(RegimeControl::Cancel(reason)) => {
			select_resolution(resolution, ResolutionKind::Cancel, &activation);
			if resolution.controlling_activation.as_deref() == Some(activation.as_str()) {
				resolution.cancel_reason = Some(reason);
			}
		},
		Some(RegimeControl::Complete) => resolution.terminated.push(activation),
		Some(RegimeControl::Fail(detail)) => {
			resolution.failures.push(detail);
			select_resolution(resolution, ResolutionKind::Fail, &activation);
		},
	}
}

/// Bounded subagent structured-yield escalation.
#[derive(Default)]
pub struct SubagentYieldRegime {
	rung: u8,
}

impl Regime for SubagentYieldRegime {
	fn apply(&mut self, ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
		if ctx.facts().empty_output {
			return Ok(());
		}
		match (self.rung, ctx.point()) {
			(0, Point::Settle) => {
				self.rung = 1;
				ctx.append_context(vec![regime_message(
					"Return the required structured yield payload now.",
				)]);
				next.retry();
			},
			(1, Point::Stream) => {
				self.rung = 2;
				next.cancel("yield budget exceeded");
			},
			(2, Point::ToolChoice) => {
				self.rung = 3;
				ctx.require_tool("yield");
			},
			(3, _) => {
				self.rung = 4;
				next.fail("structured yield regime exhausted");
			},
			_ => next.complete(),
		}
		Ok(())
	}

	fn state(&self) -> Str {
		Str::from(self.rung.to_string())
	}

	fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
		self.rung = payload
			.parse()
			.map_err(|_| RegimeStateError::InvalidPayload)?;
		Ok(())
	}
}

/// SETTLE barrier that vetoes stop while agent-loop jobs remain pending.
pub struct QuiescenceBarrier {
	jobs:    Arc<JobBoard>,
	pending: Vec<Item>,
}

impl QuiescenceBarrier {
	/// Creates a barrier over the authoritative job board.
	pub fn new(jobs: Arc<JobBoard>) -> Self {
		Self { jobs, pending: Vec::new() }
	}

	/// Queues one settled async-result injection for the next veto.
	pub fn push_async_result(&mut self, item: Item) {
		self.pending.push(item);
	}
}

impl Regime for QuiescenceBarrier {
	fn apply(&mut self, ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
		if ctx.point() != Point::Settle || ctx.facts().empty_output {
			return Ok(());
		}
		if self.jobs.is_empty() {
			next.complete();
			return Ok(());
		}
		ctx.append_context(mem::take(&mut self.pending));
		next.reject("agent-loop jobs pending");
		Ok(())
	}

	fn state(&self) -> Str {
		Str::new_static("{}")
	}

	fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
		if payload == "{}" {
			Ok(())
		} else {
			Err(RegimeStateError::InvalidPayload)
		}
	}
}

/// Bounded session-stop retry regime used by the hook bridge.
#[derive(Default)]
pub struct SessionStopRegime {
	committed_steps: u8,
}

impl Regime for SessionStopRegime {
	fn apply(&mut self, ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
		if ctx.point() != Point::Settle {
			return Ok(());
		}
		if self.committed_steps >= 8 {
			next.complete();
			return Ok(());
		}
		self.committed_steps = self.committed_steps.saturating_add(1);
		next.retry();
		Ok(())
	}

	fn state(&self) -> Str {
		Str::from(self.committed_steps.to_string())
	}

	fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
		self.committed_steps = payload
			.parse()
			.map_err(|_| RegimeStateError::InvalidPayload)?;
		Ok(())
	}
}

fn regime_message(text: impl Into<String>) -> Item {
	use omp_proto::thread::v1::{self as thread};
	Item {
		created_at_ms: now_ms(),
		kind: Some(item::Kind::Message(thread::Message {
			role:  thread::Role::User as i32,
			parts: vec![thread::Part { kind: Some(part::Kind::Text(text.into())) }],
		})),
		..Item::default()
	}
}

/// Durable first-class activation record stored by the journal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RegimeRecord {
	/// Stable declaration identity.
	pub spec_id:            RegimeId,
	/// State schema identity (`family@rev`).
	pub family_rev:         Str,
	/// Opaque typed-state payload.
	pub state:              Str,
	/// Number of committed, bound-accounted steps.
	pub committed_steps:    u32,
	/// Stable activation ULID string.
	pub activation:         ActivationId,
	/// Ordering ULID retained separately for forensic readability.
	pub activated_at:       Str,
	/// Epoch millisecond restored into resource-owner diagnostics.
	pub activated_since_ms: u64,
	/// Parent activation for lifetime-tied subtree revival.
	pub parent:             Option<ActivationId>,
	/// Lifecycle transition represented by this record.
	pub status:             RegimeStatus,
}
/// Outcome of rebuilding a [`RegimeSet`] from journal state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RevivalReport {
	/// Activation identities restored at their exact committed-step count.
	pub resumed: Vec<ActivationId>,
	/// Unloadable records durably failed.
	pub failed:  Vec<RegimeRecord>,
}

/// Lifecycle represented by one [`RegimeRecord`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum RegimeStatus {
	/// The activation is currently running.
	Active,
	/// The activation is a durable FIFO resource ticket.
	Queued,
	/// The activation completed successfully.
	Completed,
	/// The activation failed.
	Failed,
	/// The activation was explicitly stopped.
	Stopped,
}

/// Shared set of required-deadline waits used by PRE_MODEL and ADMISSION.
#[derive(Clone, Default)]
pub struct WaitSet {
	inner: Arc<WaitSetInner>,
}

#[derive(Default)]
struct WaitSetInner {
	tickets: Mutex<BTreeMap<Str, WaitTicket>>,
	notify:  Notify,
}

impl WaitSet {
	/// Inserts or replaces one ticket. A zero deadline is rejected.
	pub fn insert(&self, ticket: WaitTicket) -> Result<(), WaitError> {
		if ticket.deadline_ms == 0 {
			return Err(WaitError::MissingDeadline);
		}
		self.inner.tickets.lock().insert(ticket.id.clone(), ticket);
		self.inner.notify.notify_waiters();
		Ok(())
	}

	/// Resolves one ticket idempotently.
	pub fn resolve(&self, id: &str) -> bool {
		let removed = self.inner.tickets.lock().remove(id).is_some();
		if removed {
			self.inner.notify.notify_waiters();
		}
		removed
	}
}

mod wait_runtime {
	use tokio::sync::watch::Receiver;

	use super::{Duration, WaitError, WaitSet, now_ms, time};

	impl WaitSet {
		/// Parks until every ticket resolves, a deadline elapses, or abort
		/// changes. All tickets expired at the observed deadline are atomically
		/// retired before returning [`WaitError::Deadline`].
		pub async fn wait_empty(&self, mut abort: Receiver<u64>) -> Result<(), WaitError> {
			loop {
				let now = now_ms();
				let deadline = {
					let mut tickets = self.inner.tickets.lock();
					if tickets.is_empty() {
						return Ok(());
					}
					let expired = tickets.values().any(|ticket| ticket.deadline_ms <= now);
					if expired {
						tickets.retain(|_, ticket| ticket.deadline_ms > now);
						return Err(WaitError::Deadline);
					}
					tickets
						.values()
						.map(|ticket| ticket.deadline_ms)
						.min()
						.expect("nonempty")
				};
				let sleep = time::sleep(Duration::from_millis(deadline - now));
				tokio::pin!(sleep);
				tokio::select! {
					() = self.inner.notify.notified() => {},
					_ = &mut sleep => {},
					changed = abort.changed() => {
						if changed.is_ok() { return Err(WaitError::Aborted); }
					},
				}
			}
		}
	}
}

/// Failure while parking on regime waits.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WaitError {
	/// A wait omitted its mandatory deadline.
	#[error("regime wait requires a deadline")]
	MissingDeadline,
	/// A wait reached its deadline.
	#[error("regime wait deadline elapsed")]
	Deadline,
	/// The submission abort watch changed.
	#[error("regime wait was aborted")]
	Aborted,
}

#[cfg(test)]
mod builtin_tests {
	use omp_env::EnvClient;
	use omp_tool::{ArtifactLifetime, ExpectedArtifact, JobOwner, JobRef};

	use super::*;
	use crate::mailbox::Mailbox;

	fn apply(regime: &mut dyn Regime, point: Point) -> RegimeDraft {
		let facts = PointCx::default();
		let mut draft = RegimeDraft::default();
		let RegimeDraft { control, effects } = &mut draft;
		let mut context =
			RegimeContext { point, facts, activation: "test", committed_steps: 0, effects };
		regime.apply(&mut context, Next { control }).unwrap();
		draft
	}

	#[test]
	fn prewalk_scopes_cheap_model_and_prompt_until_mutation() {
		let spec = prewalk_regime_spec();
		assert_eq!(spec.owns.as_ref(), &[Resource::Mode]);
		assert!(
			spec.sets.iter().any(|setting| {
				setting.slot == SettingSlot::PromptSlot && setting.value == "prewalk"
			})
		);
		assert!(
			spec
				.sets
				.iter()
				.any(|setting| { setting.slot == SettingSlot::ModelRoute && setting.value == "smol" })
		);
	}

	#[test]
	fn subagent_yield_uses_one_control_per_step() {
		let mut regime = SubagentYieldRegime::default();
		let first = apply(&mut regime, Point::Settle);
		assert!(matches!(first.control, Some(RegimeControl::Retry)));
		assert!(matches!(first.effects.as_slice(), [RegimeEffect::AppendContext(_)]));
		assert!(matches!(apply(&mut regime, Point::Stream).control, Some(RegimeControl::Cancel(_))));
		assert!(
			matches!(apply(&mut regime, Point::ToolChoice).effects.as_slice(), [RegimeEffect::RequireTool(tool)] if tool == "yield")
		);
		assert!(matches!(apply(&mut regime, Point::Settle).control, Some(RegimeControl::Fail(_))));
	}

	#[test]
	fn quiescence_holds_until_the_settled_result_is_consumed() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = Arc::new(JobBoard::new(env, mailbox.sender()));
		assert!(board.register(JobRef {
			id:       Str::new_static("job-1"),
			owner:    JobOwner::AgentLoop { agent_id: Str::new_static("agent") },
			metadata: Arc::default(),
			artifact: ExpectedArtifact {
				description: Str::new_static("test"),
				media_type:  None,
				lifetime:    ArtifactLifetime::Session,
			},
		}));
		let mut regime = QuiescenceBarrier::new(Arc::clone(&board));
		assert!(matches!(apply(&mut regime, Point::Settle).control, Some(RegimeControl::Reject(_))));
		board.settle("job-1", Item::default()).unwrap();
		// A settled body is retained until a consumer claims it; the barrier
		// must keep vetoing stop so the result cannot be dropped.
		assert!(matches!(apply(&mut regime, Point::Settle).control, Some(RegimeControl::Reject(_))));
		let _ = board.snapshot_consuming();
		assert!(matches!(apply(&mut regime, Point::Settle).control, Some(RegimeControl::Complete)));
	}

	#[test]
	fn goal_state_emits_threshold_once_and_fails_at_budget() {
		let mut regime = GoalRegime::default();
		let at_half = serde_json::to_vec(&GoalRegimeState {
			objective:          Str::new_static("ship"),
			budget_tokens:      Some(100),
			spent_tokens:       50,
			thresholds_crossed: 0,
		})
		.unwrap();
		regime.update(&at_half).unwrap();
		assert!(matches!(apply(&mut regime, Point::Context).effects.as_slice(), [
			RegimeEffect::AppendContext(_)
		]));
		assert!(apply(&mut regime, Point::Context).effects.is_empty());
		let exhausted = serde_json::to_vec(&GoalRegimeState {
			objective:          Str::new_static("ship"),
			budget_tokens:      Some(100),
			spent_tokens:       100,
			thresholds_crossed: 1,
		})
		.unwrap();
		regime.update(&exhausted).unwrap();
		assert!(matches!(apply(&mut regime, Point::Context).control, Some(RegimeControl::Fail(_))));
	}

	#[test]
	fn session_stop_completes_after_eight_committed_retries() {
		let mut regime = SessionStopRegime::default();
		for _ in 0..8 {
			assert!(matches!(apply(&mut regime, Point::Settle).control, Some(RegimeControl::Retry)));
		}
		assert!(matches!(apply(&mut regime, Point::Settle).control, Some(RegimeControl::Complete)));
	}
	#[tokio::test]
	async fn expired_wait_is_retired_before_the_next_turn() {
		let waits = WaitSet::default();
		waits
			.insert(WaitTicket {
				id:          Str::new_static("expired"),
				deadline_ms: now_ms(),
				reason:      Str::new_static("first turn"),
			})
			.expect("insert expired ticket");
		let (_abort_tx, abort_rx) = tokio::sync::watch::channel(0);
		assert_eq!(waits.wait_empty(abort_rx.clone()).await, Err(WaitError::Deadline));
		assert!(!waits.resolve("expired"));

		waits
			.insert(WaitTicket {
				id:          Str::new_static("fresh"),
				deadline_ms: now_ms().saturating_add(1_000),
				reason:      Str::new_static("next turn"),
			})
			.expect("insert fresh ticket");
		assert!(waits.resolve("fresh"));
		assert_eq!(waits.wait_empty(abort_rx).await, Ok(()));
	}
	#[derive(Clone, Copy)]
	enum TestBehavior {
		RetryAppend(&'static str),
		Reject(&'static str),
		Wait,
		Tool(&'static str),
		FailAfterEffect,
	}

	struct TestRegime(TestBehavior);

	impl Regime for TestRegime {
		fn apply(&mut self, ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
			match self.0 {
				TestBehavior::RetryAppend(text) => {
					ctx.append_context(vec![regime_message(text)]);
					next.retry();
				},
				TestBehavior::Reject(reason) => next.reject(reason),
				TestBehavior::Wait => next.wait(WaitTicket {
					id:          Str::new_static("wait"),
					deadline_ms: 10,
					reason:      Str::new_static("pending"),
				}),
				TestBehavior::Tool(tool) => ctx.require_tool(tool),
				TestBehavior::FailAfterEffect => {
					ctx.append_context(vec![regime_message("discard")]);
					return Err(RegimeError::InvalidEffect);
				},
			}
			Ok(())
		}

		fn on_limit(&mut self, _: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
			next.fail("step limit reached");
			Ok(())
		}

		fn state(&self) -> Str {
			Str::new_static("{}")
		}

		fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
			if payload == "{}" {
				Ok(())
			} else {
				Err(RegimeStateError::InvalidPayload)
			}
		}
	}

	fn test_spec(id: &'static str, point: Point, precedence: i16) -> Arc<RegimeSpec> {
		Arc::new(RegimeSpec {
			id: Str::new_static(id),
			events: point.set(),
			precedence,
			max_steps: None,
			committed_step_interval_ms: None,
			on_limit: false,
			lifetime: RegimeLifetime::Run,
			family_rev: Str::new_static("test@1"),
			when: None,
			owns: Arc::from([]),
			sets: Arc::from([]),
			minimum_duration_ms: None,
		})
	}

	#[test]
	fn handler_error_discards_its_isolated_effects() {
		let mut set = RegimeSet::new();
		set.start(
			test_spec("error", Point::Context, 0),
			Box::new(TestRegime(TestBehavior::FailAfterEffect)),
			StartOptions::default(),
		)
		.unwrap();
		let resolution = set.resolve(Point::Context, &PointCx::default(), None);
		assert!(resolution.injects.is_empty());
		assert_eq!(resolution.control, ResolutionKind::Fail);
		assert!(set.is_empty());
	}
	#[test]
	fn structured_failure_payloads_are_not_rendered_to_strings() {
		let payload = Bytes::from_static(br#"{"code":"limit"}"#);
		let draft =
			evaluate_regime(Point::Settle, &PointCx::default(), "typed-failure", 0, |_, next| {
				next.fail(RegimeFailure::structured(payload.clone()));
				Ok(())
			})
			.unwrap();
		assert!(matches!(
			draft.control,
			Some(RegimeControl::Fail(RegimeFailure::Structured(actual))) if actual == payload
		));
	}

	#[test]
	fn precedence_orders_combined_rejections() {
		let mut set = RegimeSet::new();
		set.start(
			test_spec("low", Point::Admission, 1),
			Box::new(TestRegime(TestBehavior::Reject("low"))),
			StartOptions::default(),
		)
		.unwrap();
		set.start(
			test_spec("high", Point::Admission, 10),
			Box::new(TestRegime(TestBehavior::Reject("high"))),
			StartOptions::default(),
		)
		.unwrap();
		let resolution = set.resolve(Point::Admission, &PointCx::default(), None);
		assert_eq!(resolution.control, ResolutionKind::Reject);
		assert_eq!(resolution.denials, [Str::new_static("high"), Str::new_static("low")]);
	}

	#[test]
	fn waits_pause_committed_step_accounting() {
		let mut set = RegimeSet::new();
		let mut spec = (*test_spec("wait", Point::Admission, 0)).clone();
		spec.max_steps = Some(1);
		let receipt = set
			.start(Arc::new(spec), Box::new(TestRegime(TestBehavior::Wait)), StartOptions::default())
			.unwrap();
		let resolution =
			set.resolve(Point::Admission, &PointCx { delivered: true, ..PointCx::default() }, None);
		assert_eq!(resolution.control, ResolutionKind::Wait);
		let record = set
			.records()
			.into_iter()
			.find(|record| record.activation == receipt.activation)
			.unwrap();
		assert_eq!(record.committed_steps, 0);
	}

	#[test]
	fn committed_bound_invokes_same_handler_instance_on_limit() {
		let mut set = RegimeSet::new();
		let mut spec = (*test_spec("bounded", Point::Settle, 0)).clone();
		spec.max_steps = Some(1);
		spec.on_limit = true;
		set.start(
			Arc::new(spec),
			Box::new(TestRegime(TestBehavior::RetryAppend("retry"))),
			StartOptions::default(),
		)
		.unwrap();
		let first =
			set.resolve(Point::Settle, &PointCx { delivered: true, ..PointCx::default() }, None);
		assert_eq!(first.control, ResolutionKind::Retry);
		assert_eq!(set.records()[0].committed_steps, 1);
		let limited = set.resolve(Point::Settle, &PointCx::default(), None);
		assert_eq!(limited.control, ResolutionKind::Fail);
		assert!(set.is_empty());
	}

	#[test]
	fn simultaneous_tool_requirements_preserve_fifo_without_spending_bounds() {
		let mut set = RegimeSet::new();
		let mut first_spec = (*test_spec("first", Point::ToolChoice, 10)).clone();
		first_spec.max_steps = Some(1);
		let mut second_spec = (*test_spec("second", Point::ToolChoice, 1)).clone();
		second_spec.max_steps = Some(1);
		set.start(
			Arc::new(first_spec),
			Box::new(TestRegime(TestBehavior::Tool("first-tool"))),
			StartOptions::default(),
		)
		.unwrap();
		set.start(
			Arc::new(second_spec),
			Box::new(TestRegime(TestBehavior::Tool("second-tool"))),
			StartOptions::default(),
		)
		.unwrap();
		let mut queue = ToolChoiceQueue::new();
		let resolution = set.resolve(
			Point::ToolChoice,
			&PointCx { delivered: true, ..PointCx::default() },
			Some(&mut queue),
		);
		assert_eq!(resolution.control, ResolutionKind::Tool);
		assert!(
			set.records()
				.iter()
				.all(|record| record.committed_steps == 0)
		);
		assert!(matches!(
			queue.claim_next(),
			Some(ToolChoice::Named(tool)) if tool == "first-tool"
		));
		queue.resolve();
		assert!(matches!(
			queue.claim_next(),
			Some(ToolChoice::Named(tool)) if tool == "second-tool"
		));
	}
}
