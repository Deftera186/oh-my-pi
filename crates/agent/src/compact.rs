//! Context compaction tiers, usage accounting, and deterministic hook verdicts.

use std::{mem, time};

use bytes::{Bytes, BytesMut};
use omp_core::{Str, sf};
use omp_proto::{
	prost::Message as _,
	toolhost::{
		v1,
		v1::{CompactionRequest, CompactionVerdict as WireCompactionVerdict, HookEventId},
	},
};
pub use omp_storage::transcript::SupersededCompaction;
use omp_storage::{
	blob::BlobRef,
	transcript::{Kind, ModelId, ModelRef, ProviderId, capsule::checkpoint_reusable},
};
use smallvec::SmallVec;

use crate::{
	hooks::{DomainReturn, GateError, HookEvent, HookGate, HookPatch, SourceRef},
	journal::Compact,
};

/// Fraction of an auto-compaction trigger below which the ladder is re-armed.
///
/// The band is agent-owned: extensions observe the triggering usage, never the
/// suppression state, so they cannot create a compact-on-every-turn loop.
pub const COMPACTION_RECOVERY_BAND: f64 = 0.8;
/// Prompt-cache suffix retained verbatim during lossless pruning.
pub const PROMPT_CACHE_WARM_SUFFIX_TOKENS: u64 = 8_192;
/// Idle duration after which lossless pruning is reconsidered.
pub const IDLE_PRUNE_AFTER: time::Duration = time::Duration::from_secs(90 * 60);

/// Context rescue rungs available to the ordered ladder.
///
/// [`CompactionTier::ALL`] is the default order. A resolved
/// [`CompactionMethodOrder`] may reorder or omit tiers.
#[derive(
	Clone,
	Copy,
	Debug,
	Eq,
	PartialEq,
	Ord,
	PartialOrd,
	Hash,
	serde::Serialize,
	serde::Deserialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum CompactionTier {
	/// Remove already-useless or superseded tool results without loss.
	Prune,
	/// Remove blob-backed historical content while retaining artifact
	/// references.
	DropMedia,
	/// Replace oversized historical tool results with bounded views.
	Elide,
	/// Produce an immediate portable summary from a local snapshot.
	Local,
	/// Render discarded history into bounded bitmap frames.
	Snapcompact,
	/// Request provider-native context management with replay checkpointing.
	Remote,
	/// Generate a handoff summary and continue from it in the same session.
	Handoff,
}

impl CompactionTier {
	/// The implemented rescue ladder in execution order.
	pub const ALL: [Self; 7] = [
		Self::Prune,
		Self::DropMedia,
		Self::Elide,
		Self::Snapcompact,
		Self::Local,
		Self::Remote,
		Self::Handoff,
	];

	/// Returns whether this rung preserves all non-targeted projection items.
	pub const fn is_lossless(self) -> bool {
		matches!(self, Self::Prune | Self::DropMedia)
	}

	/// Stable lower-case name used by settings and journal display metadata.
	pub const fn setting_name(self) -> &'static str {
		match self {
			Self::Prune => "prune",
			Self::DropMedia => "drop_media",
			Self::Elide => "elide",
			Self::Local => "local",
			Self::Snapcompact => "snapcompact",
			Self::Remote => "remote",
			Self::Handoff => "handoff",
		}
	}
}

/// Fraction of the threshold used as speculative-compaction lead.
pub const SPECULATION_LEAD_FRACTION: f64 = 0.125;
/// Minimum speculative lead and grace-band headroom.
pub const SPECULATION_LEAD_MIN_TOKENS: u64 = 8_192;
/// Maximum speculative lead, bounding how much new history an armed summary can
/// miss before it is committed.
pub const SPECULATION_LEAD_MAX_TOKENS: u64 = 32_000;

/// Returns the pre-threshold lead used for speculative compaction.
pub fn speculation_lead_tokens(threshold_tokens: u64) -> u64 {
	((threshold_tokens as f64 * SPECULATION_LEAD_FRACTION).floor() as u64)
		.clamp(SPECULATION_LEAD_MIN_TOKENS, SPECULATION_LEAD_MAX_TOKENS)
}

/// Resolved user preference for compaction ladder methods.
///
/// The first occurrence of every configured tier wins. An empty configured
/// list disables the ladder; [`Self::default`] supplies the current built-in
/// order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionMethodOrder {
	tiers: SmallVec<CompactionTier, 7>,
}

impl Default for CompactionMethodOrder {
	fn default() -> Self {
		Self { tiers: SmallVec::from_iter(CompactionTier::ALL) }
	}
}

impl CompactionMethodOrder {
	/// Resolves a configured list while preserving first-occurrence order.
	pub fn resolve(configured: &[CompactionTier]) -> Self {
		let mut tiers = SmallVec::new();
		for &tier in configured {
			if !tiers.contains(&tier) {
				tiers.push(tier);
			}
		}
		Self { tiers }
	}

	/// Returns the exact enabled fallback order.
	pub fn as_slice(&self) -> &[CompactionTier] {
		&self.tiers
	}

	/// Iterates enabled tiers in exact fallback order.
	pub fn iter(&self) -> impl ExactSizeIterator<Item = CompactionTier> + '_ {
		self.tiers.iter().copied()
	}

	/// Filters unsupported tiers without disturbing fallback order.
	pub fn available(&self, mut supported: impl FnMut(CompactionTier) -> bool) -> Self {
		Self { tiers: self.iter().filter(|&tier| supported(tier)).collect() }
	}

	/// Returns the first latency-bearing tier that can be speculated.
	///
	/// Mechanical lossless/elision rungs do not decide summary strategy. A
	/// local snapshot rung does: because it is immediate, it suppresses later
	/// remote or handoff speculation.
	pub fn speculation_tier(&self) -> Option<CompactionTier> {
		for &tier in &self.tiers {
			match tier {
				CompactionTier::Prune | CompactionTier::DropMedia | CompactionTier::Elide => {},
				CompactionTier::Local | CompactionTier::Snapcompact => return None,
				CompactionTier::Remote | CompactionTier::Handoff => return Some(tier),
			}
		}
		None
	}
}

/// Runtime options that govern speculative compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionSpeculationOptions {
	/// Whether background speculation may run.
	pub enabled:            bool,
	/// Recent-context budget used to refresh an armed summary after growth.
	pub keep_recent_tokens: u64,
}

impl Default for CompactionSpeculationOptions {
	fn default() -> Self {
		Self { enabled: true, keep_recent_tokens: 20_000 }
	}
}

/// User-visible state of the background speculation slot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpeculationState {
	/// No background work or result is retained.
	#[default]
	Idle,
	/// A detached snapshot is being compacted.
	Running,
	/// A result is ready to commit at the next threshold boundary.
	Armed,
}

/// Immutable state handed to a background compactor.
///
/// `isolated_session_id` is deliberately distinct from `session_id`, keeping
/// provider-native sticky state and journal writes away from the live session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeculationSnapshot {
	/// Live session whose history was snapshotted.
	pub session_id:          Str,
	/// Side-session identity used only by the speculative request.
	pub isolated_session_id: Str,
	/// Active branch leaf covered by the snapshot.
	pub branch_leaf:         u64,
	/// Durable compaction/reset epoch at snapshot time.
	pub compaction_epoch:    u64,
}

/// One detached speculative compaction launch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeculationRequest {
	/// Monotonic coordinator-local run identity.
	pub run_id:          u64,
	/// LLM-backed ladder method to execute.
	pub method:          CompactionTier,
	/// Immutable detached state; background work must consume only this value.
	pub snapshot:        SpeculationSnapshot,
	/// Context occupancy when this run began.
	pub tokens_at_start: u64,
}

/// Completed detached work offered back to the coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeculationResult {
	/// Exact launch that produced the result.
	pub request: SpeculationRequest,
	/// In-place journal entry prepared from the detached snapshot.
	pub compact: Compact,
}

/// Compaction work selected at one maintenance boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionDecision {
	/// No work is needed at this boundary.
	None,
	/// Launch detached work; `defer_blocking` is true when the live context has
	/// crossed the threshold but remains inside the grace band.
	Launch {
		/// Detached request to execute.
		request:        SpeculationRequest,
		/// Whether blocking compaction is deferred for this boundary.
		defer_blocking: bool,
		/// Superseded in-flight run the executor should abort.
		cancel_run:     Option<u64>,
	},
	/// Abort a detached run that settings, branch state, or growth superseded.
	Cancel {
		/// In-flight run the executor should abort.
		run_id: u64,
	},
	/// Keep serving the live turn while an existing run finishes inside the
	/// grace band.
	Defer,
	/// Commit an armed result in place without another summarizer call.
	Commit(Compact),
	/// Run the configured ladder synchronously.
	Block {
		/// Superseded in-flight run the executor should abort.
		cancel_run: Option<u64>,
	},
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SpeculationSlot {
	Idle,
	Running(SpeculationRequest),
	Armed(SpeculationResult),
}

/// Coordinates hysteresis with one running or armed speculative compaction.
///
/// The coordinator never owns live mutable session state. A launch contains an
/// isolated snapshot, completion only arms a result, and the journal is
/// rewritten exclusively when [`Self::evaluate`] returns
/// [`CompactionDecision::Commit`].
#[derive(Clone, Debug)]
pub struct CompactionCoordinator {
	hysteresis: CompactionHysteresis,
	slot:       SpeculationSlot,
	next_run:   u64,
}

impl Default for CompactionCoordinator {
	fn default() -> Self {
		Self {
			hysteresis: CompactionHysteresis::default(),
			slot:       SpeculationSlot::Idle,
			next_run:   1,
		}
	}
}

impl CompactionCoordinator {
	/// Returns the speculation slot state used by status surfaces.
	pub const fn speculation_state(&self) -> SpeculationState {
		match &self.slot {
			SpeculationSlot::Idle => SpeculationState::Idle,
			SpeculationSlot::Running(_) => SpeculationState::Running,
			SpeculationSlot::Armed(_) => SpeculationState::Armed,
		}
	}

	/// Discards running or armed work, returning an in-flight run to abort.
	pub fn cancel_speculation(&mut self) -> Option<u64> {
		let previous = mem::replace(&mut self.slot, SpeculationSlot::Idle);
		match previous {
			SpeculationSlot::Running(request) => Some(request.run_id),
			SpeculationSlot::Idle | SpeculationSlot::Armed(_) => None,
		}
	}

	/// Arms a completion only when it belongs to the current detached launch.
	///
	/// Late completions from cancelled, refreshed, or replaced runs are
	/// discarded without affecting the live session.
	pub fn arm(&mut self, mut result: SpeculationResult) -> bool {
		let SpeculationSlot::Running(request) = &self.slot else {
			return false;
		};
		if *request != result.request {
			return false;
		}
		result.compact.method = Some(sf!(result.request.method.setting_name()));
		self.slot = SpeculationSlot::Armed(result);
		true
	}

	/// Evaluates speculation, grace deferral, armed commit, and blocking
	/// hysteresis for one usage snapshot.
	///
	/// `snapshot_leaf_present` reports whether the leaf covered by a running or
	/// armed snapshot is still on the active branch. The caller must set it
	/// false after rewinds; `compaction_epoch` independently rejects reset or
	/// compact boundaries.
	pub fn evaluate(
		&mut self,
		usage: ContextUsage,
		order: &CompactionMethodOrder,
		options: CompactionSpeculationOptions,
		live_session_id: &Str,
		branch_leaf: u64,
		snapshot_leaf_present: bool,
	) -> CompactionDecision {
		if order.as_slice().is_empty() {
			return self
				.cancel_speculation()
				.map_or(CompactionDecision::None, |run_id| CompactionDecision::Cancel { run_id });
		}
		if !usage.over_threshold() {
			let _ = self.hysteresis.evaluate(usage);
			if !options.enabled {
				return self
					.cancel_speculation()
					.map_or(CompactionDecision::None, |run_id| CompactionDecision::Cancel { run_id });
			}
			let Some(method) = order.speculation_tier() else {
				return self
					.cancel_speculation()
					.map_or(CompactionDecision::None, |run_id| CompactionDecision::Cancel { run_id });
			};
			let mut cancel_run = None;
			let refresh_budget = options.keep_recent_tokens.max(SPECULATION_LEAD_MIN_TOKENS);
			let stale = match &self.slot {
				SpeculationSlot::Idle => false,
				SpeculationSlot::Running(request) => {
					request.snapshot.compaction_epoch != usage.compaction_epoch || !snapshot_leaf_present
				},
				SpeculationSlot::Armed(result) => {
					result.request.snapshot.compaction_epoch != usage.compaction_epoch
						|| !snapshot_leaf_present
						|| usage
							.total_tokens
							.saturating_sub(result.request.tokens_at_start)
							> refresh_budget
				},
			};
			if stale {
				cancel_run = self.cancel_speculation();
			}
			if !matches!(&self.slot, SpeculationSlot::Idle) {
				return CompactionDecision::None;
			}
			let threshold = usage.target_tokens();
			if threshold.saturating_sub(usage.total_tokens) > speculation_lead_tokens(threshold) {
				return cancel_run
					.map_or(CompactionDecision::None, |run_id| CompactionDecision::Cancel { run_id });
			}
			return self.launch(method, usage, live_session_id, branch_leaf, false, cancel_run);
		}

		let method = options.enabled.then(|| order.speculation_tier()).flatten();
		if matches!(&self.slot, SpeculationSlot::Armed(_)) {
			let armed = match mem::replace(&mut self.slot, SpeculationSlot::Idle) {
				SpeculationSlot::Armed(result) => result,
				SpeculationSlot::Idle | SpeculationSlot::Running(_) => unreachable!(),
			};
			let valid = method == Some(armed.request.method)
				&& armed.request.snapshot.compaction_epoch == usage.compaction_epoch
				&& snapshot_leaf_present;
			if valid {
				self.hysteresis.armed = false;
				return CompactionDecision::Commit(armed.compact);
			}
		}

		let threshold = usage.target_tokens();
		let grace_cap = threshold
			.saturating_add(speculation_lead_tokens(threshold))
			.min(
				usage
					.context_window
					.saturating_sub(SPECULATION_LEAD_MIN_TOKENS),
			);
		if usage.total_tokens < grace_cap
			&& let Some(method) = method
		{
			match &self.slot {
				SpeculationSlot::Running(request)
					if request.method == method
						&& request.snapshot.compaction_epoch == usage.compaction_epoch
						&& snapshot_leaf_present =>
				{
					return CompactionDecision::Defer;
				},
				SpeculationSlot::Idle => {
					return self.launch(method, usage, live_session_id, branch_leaf, true, None);
				},
				SpeculationSlot::Running(_) | SpeculationSlot::Armed(_) => {},
			}
		}

		let cancel_run = self.cancel_speculation();
		if self.hysteresis.evaluate(usage) {
			CompactionDecision::Block { cancel_run }
		} else {
			CompactionDecision::None
		}
	}

	fn launch(
		&mut self,
		method: CompactionTier,
		usage: ContextUsage,
		live_session_id: &Str,
		branch_leaf: u64,
		defer_blocking: bool,
		cancel_run: Option<u64>,
	) -> CompactionDecision {
		let run_id = self.next_run;
		self.next_run = self.next_run.wrapping_add(1).max(1);
		let request = SpeculationRequest {
			run_id,
			method,
			snapshot: SpeculationSnapshot {
				session_id: live_session_id.clone(),
				isolated_session_id: sf!("{live_session_id}:spec:{run_id}"),
				branch_leaf,
				compaction_epoch: usage.compaction_epoch,
			},
			tokens_at_start: usage.total_tokens,
		};
		self.slot = SpeculationSlot::Running(request.clone());
		CompactionDecision::Launch { request, defer_blocking, cancel_run }
	}
}

/// One-off method selected by `/compact`.
#[derive(
	Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString, strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum ManualCompactionMode {
	/// Summarize locally with the active model.
	Soft,
	/// Try provider-native compaction, then a local summary.
	Remote,
	/// Archive history into local bitmap frames.
	Snapcompact,
}

/// Parsed one-off compaction request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ManualCompactionRequest {
	/// Optional one-off mode; absent means configured preference order.
	pub mode:  Option<ManualCompactionMode>,
	/// Optional user focus text for summary-producing modes.
	pub focus: Option<Str>,
}
/// Receipt from a mechanical `/shake` rewrite.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManualShakeOutcome {
	/// Mechanical history rewrite mode applied.
	pub mode:             ManualShakeMode,
	/// Number of historical content regions replaced.
	pub replaced_regions: u64,
	/// Exact source bytes moved out of the live prompt.
	pub removed_bytes:    u64,
	/// Last materialized prompt-head item event.
	pub event:            u64,
}
/// One-off history rewrite selected by `/shake`.
#[derive(
	Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString, strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum ManualShakeMode {
	/// Spill replaceable historical content into recoverable artifacts.
	Elide,
	/// Spill historical media into recoverable artifacts.
	DropMedia,
	/// Remove every assistant thinking block, including redacted reasoning.
	Thinking,
}

/// Typed provenance for a cancelled manual compaction.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CompactionCancellation {
	/// The caller explicitly interrupted provider-backed compaction.
	#[error("compaction interrupted by user")]
	UserInterrupt,
	/// An extension vetoed compaction before it committed.
	#[error("compaction cancelled by extension: {reason}")]
	ExtensionVeto {
		/// Human-readable veto reason supplied by the extension.
		reason: Str,
	},
}

/// Invalid `/compact` arguments.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ManualCompactionError {
	/// Bitmap compaction has no summarizer and cannot consume focus text.
	#[error(
		"/compact snapcompact does not take focus instructions (it archives history without an LLM \
		 summary)"
	)]
	SnapcompactFocus,
}

impl ManualCompactionRequest {
	/// Parses the text following `/compact`, preserving legacy bare-focus input.
	pub fn parse(arguments: &str) -> Result<Self, ManualCompactionError> {
		let trimmed = arguments.trim();
		if trimmed.is_empty() {
			return Ok(Self::default());
		}
		let split = trimmed.find(char::is_whitespace);
		let (first, tail) =
			split.map_or((trimmed, ""), |index| (&trimmed[..index], trimmed[index..].trim()));
		let mode = first.parse::<ManualCompactionMode>().ok();
		if mode == Some(ManualCompactionMode::Snapcompact) && !tail.is_empty() {
			return Err(ManualCompactionError::SnapcompactFocus);
		}
		if mode.is_some() {
			return Ok(Self { mode, focus: (!tail.is_empty()).then(|| Str::from(tail)) });
		}
		Ok(Self { mode: None, focus: Some(Str::from(trimmed)) })
	}

	/// Resolves the one-off method order without mutating durable settings.
	pub fn method_order(&self, configured: &CompactionMethodOrder) -> CompactionMethodOrder {
		match self.mode {
			None => configured.clone(),
			Some(ManualCompactionMode::Soft) => {
				CompactionMethodOrder::resolve(&[CompactionTier::Local])
			},
			Some(ManualCompactionMode::Remote) => {
				CompactionMethodOrder::resolve(&[CompactionTier::Remote, CompactionTier::Local])
			},
			Some(ManualCompactionMode::Snapcompact) => {
				CompactionMethodOrder::resolve(&[CompactionTier::Snapcompact])
			},
		}
	}
}

/// Prepared bitmap compaction detached from mutable journal state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapcompactPreparation {
	/// Normalized oldest-to-newest history selected for imaging.
	pub text:            Str,
	/// Active-tokenizer measurement for `text`.
	pub source_tokens:   u64,
	/// Provider id used for image-count policy.
	pub provider:        Option<Str>,
	/// Wire API used for image billing.
	pub api:             Option<Str>,
	/// Reader model used for geometry selection.
	pub model_id:        Option<Str>,
	/// Images already occupying the rebuilt request.
	pub existing_images: usize,
	/// First durable event retained verbatim after the archive.
	pub first_kept:      u64,
	/// Complete live context usage before imaging.
	pub tokens_before:   u64,
}

/// Completed bitmap archive ready for blob persistence and one journal commit.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapcompactOutcome {
	/// Rendered frames and measured savings.
	pub archive: omp_snapcompact::archive::Archive,
	/// Textual journal boundary committed with the frame blob references.
	pub compact: Compact,
}

/// Runs the pure renderer against an immutable compaction preparation.
///
/// PNG frames remain byte values in this result; the journal owner must spill
/// each through its `BlobStore` before committing `compact`, preventing base64
/// request payloads from becoming transcript truth.
pub fn execute_snapcompact(
	preparation: &SnapcompactPreparation,
) -> Result<SnapcompactOutcome, omp_snapcompact::archive::ArchiveError> {
	let archive = omp_snapcompact::archive::render_archive(
		preparation.text.as_str(),
		preparation.source_tokens,
		omp_snapcompact::archive::ShapeTarget {
			api:      preparation.api.as_deref(),
			model_id: preparation.model_id.as_deref(),
		},
		preparation.provider.as_deref(),
		preparation.existing_images,
	)?;
	let frame_count = archive.frames.len();
	let tokens_after = archive.savings.image_tokens.saturating_add(
		preparation
			.tokens_before
			.saturating_sub(preparation.source_tokens),
	);
	Ok(SnapcompactOutcome {
		archive,
		compact: Compact {
			summary:       sf!(
				"Earlier conversation archived in {frame_count} Snapcompact frame{}.",
				if frame_count == 1 { "" } else { "s" }
			),
			short:         Some(sf!(
				"{frame_count} bitmap frame{}",
				if frame_count == 1 { "" } else { "s" }
			)),
			first_kept:    preparation.first_kept,
			tokens_before: preparation.tokens_before,
			tokens_after:  Some(tokens_after),
			method:        Some(sf!("snapcompact")),
			warning:       None,
			superseded:    Vec::new(),
			snapcompact:   None,
		},
	})
}

/// Observable result of one committed manual compaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManualCompactionOutcome {
	/// Stable correlation identifier shared with compaction hook events.
	pub preparation_id: Str,
	/// Method that produced the durable boundary.
	pub method:         ManualCompactionMode,
	/// Physical compact event index.
	pub event:          u64,
	/// First durable item retained verbatim after the summary.
	pub first_kept:     u64,
	/// Durable compaction epoch after the boundary.
	pub epoch:          u64,
	/// Context token estimate before compaction.
	pub tokens_before:  u64,
	/// Context token estimate after compaction.
	pub tokens_after:   u64,
	/// Exact byte length of the durable textual summary.
	pub summary_bytes:  usize,
	/// Extension that supplied the winning custom summary.
	pub from_extension: Option<Str>,
	/// Warning attached to a degraded but committed compaction.
	pub warning:        Option<Str>,
	/// Number of durable Snapcompact PNG frames.
	pub frame_count:    usize,
}

/// Work selected when a manual request takes ownership of the coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManualCompactionDecision {
	/// Ordered methods for this invocation.
	pub order:      CompactionMethodOrder,
	/// User summary focus, if present.
	pub focus:      Option<Str>,
	/// Detached speculation run that must be aborted before execution.
	pub cancel_run: Option<u64>,
}

impl CompactionCoordinator {
	/// Starts one manual run through the same cancellation and ordering
	/// authority as automatic, mid-turn, idle, and speculative compaction.
	pub fn begin_manual(
		&mut self,
		request: ManualCompactionRequest,
		configured: &CompactionMethodOrder,
	) -> ManualCompactionDecision {
		ManualCompactionDecision {
			order:      request.method_order(configured),
			focus:      request.focus,
			cancel_run: self.cancel_speculation(),
		}
	}
}

/// Classifies coordinator maintenance boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionBoundary {
	/// A completed turn crossed the configured threshold.
	Automatic,
	/// A streaming turn needs room before it can continue.
	MidTurn,
	/// The session became idle at the supplied instant.
	Idle {
		/// Time elapsed since the last activity.
		elapsed: time::Duration,
	},
}

/// Returns the ladder reason when a boundary should run maintenance.
///
/// Idle maintenance is deliberately lossless and begins only after ninety
/// minutes. Threshold and mid-turn boundaries retain their distinct durable
/// reasons while sharing the coordinator.
pub fn boundary_reason(
	boundary: CompactionBoundary,
	usage: ContextUsage,
) -> Option<CompactionReason> {
	match boundary {
		CompactionBoundary::Automatic => usage
			.over_threshold()
			.then_some(CompactionReason::Threshold),
		CompactionBoundary::MidTurn => Some(CompactionReason::MidTurn),
		CompactionBoundary::Idle { elapsed } => {
			(elapsed >= IDLE_PRUNE_AFTER).then_some(CompactionReason::Idle)
		},
	}
}

/// Stable textual name for the bitmap archive tier.
pub const SNAPCOMPACT_TIER: &str = "SNAPCOMPACT";

/// The current context budget used to decide whether compaction is necessary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextUsageBreakdown {
	/// System-policy and static instruction tokens.
	pub system_tokens:  u64,
	/// Tool descriptor and tool-choice tokens.
	pub tool_tokens:    u64,
	/// Installed skill/context-asset tokens.
	pub skill_tokens:   u64,
	/// Conversation message tokens.
	pub message_tokens: u64,
	/// Media token estimate.
	pub media_tokens:   u64,
}

impl ContextUsageBreakdown {
	/// Returns the saturating sum of independently measured categories.
	pub const fn total(self) -> u64 {
		self
			.system_tokens
			.saturating_add(self.tool_tokens)
			.saturating_add(self.skill_tokens)
			.saturating_add(self.message_tokens)
			.saturating_add(self.media_tokens)
	}
}

/// The current context budget used to decide whether compaction is necessary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextUsage {
	/// All input tokens currently occupying the provider context.
	pub total_tokens:          u64,
	/// Provider-advertised context window.
	pub context_window:        u64,
	/// Tokens reserved for the next completion.
	pub reserve_tokens:        u64,
	/// Context available to the prompt after the reserve.
	pub usable_tokens:         u64,
	/// Prompt-head tokens, accounted independently from message tokens.
	pub prompt_head_tokens:    u64,
	/// Device-catalog tokens included in the prompt head.
	pub device_catalog_tokens: u64,
	/// Message-body token estimate or back-projected provider usage.
	pub message_tokens:        u64,
	/// Media-token estimate kept beside exact byte lengths.
	pub media_tokens:          u64,
	/// Durable compact/reset epoch.
	pub compaction_epoch:      u64,
	/// Configured auto-compaction trigger fraction.
	pub threshold_fraction:    f64,
	/// Whether a streaming turn makes the total an extrapolation.
	pub in_flight:             bool,
	/// Independently measured prompt categories.
	pub breakdown:             ContextUsageBreakdown,
	/// Estimated tokens added by the currently streaming turn.
	pub in_flight_tokens:      u64,
	/// Prompt tokens removed by the most recent durable history rewrite.
	pub rewrite_savings:       u64,
}

impl ContextUsage {
	/// Creates usage while deriving the usable window from its reserve.
	pub const fn new(
		total_tokens: u64,
		context_window: u64,
		reserve_tokens: u64,
		threshold_fraction: f64,
	) -> Self {
		Self {
			total_tokens,
			context_window,
			reserve_tokens,
			usable_tokens: context_window.saturating_sub(reserve_tokens),
			prompt_head_tokens: 0,
			device_catalog_tokens: 0,
			message_tokens: 0,
			media_tokens: 0,
			compaction_epoch: 0,
			threshold_fraction,
			in_flight: false,
			breakdown: ContextUsageBreakdown {
				system_tokens:  0,
				tool_tokens:    0,
				skill_tokens:   0,
				message_tokens: 0,
				media_tokens:   0,
			},
			in_flight_tokens: 0,
			rewrite_savings: 0,
		}
	}

	/// Replaces the discrete category breakdown without changing authoritative
	/// provider total usage.
	pub const fn with_breakdown(mut self, breakdown: ContextUsageBreakdown) -> Self {
		self.breakdown = breakdown;
		self
	}

	/// Marks the usage as in-flight and includes a conservative current-turn
	/// token estimate in threshold calculations.
	pub const fn extrapolate_in_flight(mut self, added_tokens: u64) -> Self {
		self.in_flight = true;
		self.in_flight_tokens = added_tokens;
		self
	}

	/// Records the exact prompt delta across a committed history rewrite and
	/// advances the durable compaction epoch.
	pub const fn after_history_rewrite(
		mut self,
		before_prompt_tokens: u64,
		after_prompt_tokens: u64,
		compaction_epoch: u64,
	) -> Self {
		self.total_tokens = after_prompt_tokens;
		self.rewrite_savings = before_prompt_tokens.saturating_sub(after_prompt_tokens);
		self.compaction_epoch = compaction_epoch;
		self.in_flight = false;
		self.in_flight_tokens = 0;
		self
	}

	/// Returns prompt occupancy including the in-flight extrapolation.
	pub const fn effective_total_tokens(self) -> u64 {
		self.total_tokens.saturating_add(self.in_flight_tokens)
	}

	/// Returns occupancy of the usable context window.
	pub fn fraction(self) -> f64 {
		if self.usable_tokens == 0 {
			return f64::INFINITY;
		}
		self.effective_total_tokens() as f64 / self.usable_tokens as f64
	}

	/// Returns the target token count at the configured trigger threshold.
	pub fn target_tokens(self) -> u64 {
		(self.usable_tokens as f64 * self.threshold_fraction).floor() as u64
	}

	/// Returns whether occupancy reaches the configured auto-compaction trigger.
	pub fn over_threshold(self) -> bool {
		self.fraction() >= self.threshold_fraction
	}
}

/// Agent-owned state that prevents auto-compaction loops near the threshold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionHysteresis {
	armed: bool,
}

impl Default for CompactionHysteresis {
	fn default() -> Self {
		Self { armed: true }
	}
}

impl CompactionHysteresis {
	/// Evaluates auto-compaction and re-arms only below the recovery band edge.
	pub fn evaluate(&mut self, usage: ContextUsage) -> bool {
		if !self.armed {
			if usage.fraction() <= usage.threshold_fraction * COMPACTION_RECOVERY_BAND {
				self.armed = true;
			}
			return false;
		}
		if usage.over_threshold() {
			self.armed = false;
			return true;
		}
		false
	}

	/// Returns whether the next threshold crossing can trigger compaction.
	pub const fn armed(self) -> bool {
		self.armed
	}
}

/// Per-item accounting kept beside the exact stored byte length.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ItemUsage {
	/// Exact serialized content length in bytes.
	pub byte_len:        u64,
	/// Provider usage back-projected onto this item.
	pub provider_tokens: u64,
}

/// Back-projects reported provider usage across items proportional to byte
/// size.
///
/// The sum of returned item token counts is exactly `total_tokens`; exact bytes
/// remain untouched for reporting and later tokenizer replacement.
pub fn back_project_provider_usage(total_tokens: u64, items: &mut [ItemUsage]) {
	let total_bytes: u128 = items.iter().map(|item| u128::from(item.byte_len)).sum();
	if items.is_empty() {
		return;
	}
	if total_bytes == 0 {
		let len = u64::try_from(items.len()).expect("slice length fits in u64");
		let each = total_tokens / len;
		let mut remainder = total_tokens % len;
		for item in items {
			item.provider_tokens = each + u64::from(remainder > 0);
			remainder = remainder.saturating_sub(1);
		}
		return;
	}
	let mut assigned = 0_u64;
	for item in items.iter_mut() {
		item.provider_tokens = u64::try_from(
			u128::from(total_tokens).saturating_mul(u128::from(item.byte_len)) / total_bytes,
		)
		.expect("token share fits in u64");
		assigned = assigned.saturating_add(item.provider_tokens);
	}
	let mut remainder = total_tokens.saturating_sub(assigned);
	for item in items {
		if remainder == 0 {
			break;
		}
		item.provider_tokens = item.provider_tokens.saturating_add(1);
		remainder -= 1;
	}
}

/// One body-free projected item considered by lossless compaction planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionItem {
	/// Physical event index used by durable amendments.
	pub event:       u64,
	/// Whether the item is a tool result already marked useless.
	pub useless:     bool,
	/// Whether a later result superseded this item.
	pub superseded:  bool,
	/// Number of blob-backed parts in the item.
	pub media_parts: u32,
	/// Token and exact-byte accounting for this item.
	pub usage:       ItemUsage,
}

/// Pure lossless targets selected from one canonical projection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LosslessPlan {
	/// Tool-result events removed by the `PRUNE` rung.
	pub prune:      Vec<u64>,
	/// Historical events whose blob-backed parts are eligible for `DROP_MEDIA`.
	pub drop_media: Vec<u64>,
	/// Exact receipt for the selected lossless amendments.
	pub receipt:    LosslessReceipt,
}

/// Exact accounting for one lossless pruning plan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LosslessReceipt {
	/// Provider tokens removed by useless or superseded results.
	pub pruned_tokens:       u64,
	/// Exact serialized bytes removed by useless or superseded results.
	pub pruned_bytes:        u64,
	/// Provider tokens retained in the protected prompt-cache suffix.
	pub warm_suffix_tokens:  u64,
	/// First event protected by the warm suffix, if any.
	pub warm_suffix_event:   Option<u64>,
	/// Number of blob-backed media parts selected for removal.
	pub dropped_media_parts: u64,
}

/// Plans lossless `PRUNE` and `DROP_MEDIA` work without mutating the
/// projection.
pub fn plan_lossless(items: &[ProjectionItem]) -> LosslessPlan {
	plan_lossless_with_warm_suffix(items, PROMPT_CACHE_WARM_SUFFIX_TOKENS, |_| false)
}

/// Plans lossless work while protecting the newest tokenizer-measured suffix
/// and app-owned retained tool results.
///
/// Items are ordered oldest to newest. The complete item that crosses
/// `warm_suffix_tokens` is retained, so the protected suffix never undershoots
/// the configured prompt-cache budget. `retain_event` is consulted by prune,
/// shake/drop-media, and asynchronous callers using this shared planner; plan
/// file reads can therefore remain lossless without agent depending on app
/// artifact policy.
pub fn plan_lossless_with_warm_suffix(
	items: &[ProjectionItem],
	warm_suffix_tokens: u64,
	retain_event: impl Fn(u64) -> bool,
) -> LosslessPlan {
	let mut protected_start = items.len();
	let mut protected_tokens = 0_u64;
	for (index, item) in items.iter().enumerate().rev() {
		if protected_tokens >= warm_suffix_tokens {
			break;
		}
		protected_start = index;
		protected_tokens = protected_tokens.saturating_add(item.usage.provider_tokens);
	}
	let mut plan = LosslessPlan::default();
	plan.receipt.warm_suffix_tokens = protected_tokens;
	plan.receipt.warm_suffix_event = items.get(protected_start).map(|item| item.event);
	for item in &items[..protected_start] {
		if retain_event(item.event) {
			continue;
		}
		if item.useless || item.superseded {
			plan.prune.push(item.event);
			plan.receipt.pruned_tokens = plan
				.receipt
				.pruned_tokens
				.saturating_add(item.usage.provider_tokens);
			plan.receipt.pruned_bytes = plan
				.receipt
				.pruned_bytes
				.saturating_add(item.usage.byte_len);
		}
		if item.media_parts != 0 {
			plan.drop_media.push(item.event);
			plan.receipt.dropped_media_parts = plan
				.receipt
				.dropped_media_parts
				.saturating_add(u64::from(item.media_parts));
		}
	}
	plan
}

/// Why a compaction request entered the ladder.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum CompactionReason {
	/// Context occupancy crossed the configured automatic threshold.
	Threshold,
	/// The session was compacted while idle.
	Idle,
	/// A person explicitly requested compaction.
	Manual,
	/// A streaming turn requires prompt-space recovery.
	MidTurn,
	/// An extension initiated the request.
	Extension,
	/// A provider rejected a request for context length.
	Rescue,
}

/// The domain-return payload dispatched once before each ladder rung.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionEvent {
	/// Stable correlation identifier for this ladder attempt.
	pub preparation_id:       Str,
	/// Rung about to run.
	pub tier:                 CompactionTier,
	/// Why the ladder was entered.
	pub reason:               CompactionReason,
	/// Durable epoch before compaction.
	pub epoch:                u64,
	/// Current total token count.
	pub tokens_before:        u64,
	/// Target token count for this rung.
	pub target_tokens:        u64,
	/// Suggested first retained item id.
	pub suggested_first_kept: Str,
	/// Wire body-free refs selected for summarization.
	pub to_summarize:         Vec<v1::MessageRef>,
	/// Wire body-free refs retained verbatim.
	pub to_retain:            Vec<v1::MessageRef>,
	/// Whether the suggested cut divides a turn.
	pub split_turn:           bool,
	/// Text of the preceding durable compact summary.
	pub previous_summary:     Option<Str>,
	/// Opaque extension preserve payload from the preceding compaction.
	pub previous_preserve:    Option<bytes::Bytes>,
	/// User-supplied focus text.
	pub custom_instructions:  Option<Str>,
	/// Remaining hook deadline in milliseconds on the frozen context wire.
	pub deadline_ms:          u64,
}

/// Skip one compaction tier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelCompaction {
	/// Durable and displayable reason for the skip.
	pub reason:             Str,
	/// Number of subsequent turns for which the entire ladder is suppressed.
	pub suppress_for_turns: u64,
}

/// A textual summary supplied by an extension instead of a built-in summarizer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomSummary {
	/// Durable `Kind::Compact` payload; its summary is always textual.
	pub compact:  Compact,
	/// Extension-private JSON stored alongside the compaction record.
	pub details:  Option<bytes::Bytes>,
	/// Opaque state returned to the next compaction attempt.
	pub preserve: Option<bytes::Bytes>,
}

/// Adjustments to the built-in behavior of one compaction rung.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DelegateCompaction {
	/// Additional instructions appended to a summarization prompt.
	pub extra_instructions: Str,
	/// Stable item identifiers whose content should survive a summary.
	pub focus_ids:          SmallVec<Str, 2>,
	/// Optional model role override.
	pub role:               Option<Str>,
	/// Optional verbatim recent-history allowance.
	pub keep_recent_tokens: Option<u64>,
}

/// One non-empty domain verdict returned by a compaction handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionVerdict {
	/// Skip the current rung.
	Cancel(CancelCompaction),
	/// Use an extension-supplied durable textual summary.
	Custom(CustomSummary),
	/// Run the built-in rung with additional direction.
	Delegate(DelegateCompaction),
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireVerdictDetails {
	Cancel {
		suppress_for_turns: u64,
	},
	Custom {
		short:         Option<Str>,
		tokens_before: u64,
		warning:       Option<Str>,
		details:       Option<Bytes>,
		preserve:      Option<Bytes>,
	},
	Delegate {
		extra_instructions: Str,
		focus_ids:          SmallVec<Str, 2>,
		role:               Option<Str>,
		keep_recent_tokens: Option<u64>,
	},
}

impl HookEvent for CompactionEvent {
	type Return = Option<CompactionVerdict>;

	const ID: HookEventId = HookEventId::HookEventCompaction;
	const REV: u32 = 1;

	fn encode_into(&self, out: &mut BytesMut) {
		let event = CompactionRequest {
			preparation_id:         self.preparation_id.as_str().to_owned(),
			tier:                   self.tier.to_string(),
			reason:                 self.reason.to_string(),
			epoch:                  self.epoch,
			tokens_before:          self.tokens_before,
			target_tokens:          self.target_tokens,
			suggested_first_kept:   self.suggested_first_kept.as_str().to_owned(),
			to_summarize:           self.to_summarize.clone(),
			to_retain:              self.to_retain.clone(),
			split_turn:             self.split_turn,
			previous_summary:       self.previous_summary.as_ref().map(ToString::to_string),
			previous_preserve_json: self.previous_preserve.clone(),
			custom_instructions:    self.custom_instructions.as_ref().map(ToString::to_string),
			deadline_ms:            self.deadline_ms,
			props:                  None,
		};
		event
			.encode(out)
			.expect("bytes buffer cannot fail protobuf encoding");
	}

	fn apply(&mut self, _: &HookPatch) -> Result<(), GateError> {
		Ok(())
	}
}
/// Dispatches the domain-return `compaction` hook for one ladder rung.
///
/// Hook failures and malformed replies resolve to
/// [`CompactionResolution::Default`], so the caller runs that rung's built-in
/// behavior rather than leaving the session over budget. A rescue-time
/// `HANDOFF` cancellation is refused because the ladder has no remaining rung
/// that can make the next provider request fit.
pub async fn dispatch_tier(gate: &HookGate, event: &CompactionEvent) -> CompactionResolution {
	let mut outcome = gate.gate_domain(event).await;
	let resolution = if outcome.contributions.is_empty() {
		resolve_one(outcome.winner)
	} else {
		resolve_verdicts(&mut outcome.contributions)
	};
	if matches!(resolution, CompactionResolution::Cancel(_))
		&& event.tier == CompactionTier::Handoff
		&& event.reason == CompactionReason::Rescue
	{
		CompactionResolution::Default
	} else {
		resolution
	}
}

impl DomainReturn for Option<CompactionVerdict> {
	fn decode_domain(bytes: &[u8]) -> Option<Self> {
		let wire = WireCompactionVerdict::decode(bytes).ok()?;
		let details = wire
			.details_json
			.as_deref()
			.map(serde_json::from_slice::<WireVerdictDetails>)
			.transpose()
			.ok()?;
		match (wire.kind.as_str(), details) {
			("cancel", Some(WireVerdictDetails::Cancel { suppress_for_turns })) => {
				Some(Some(CompactionVerdict::Cancel(CancelCompaction {
					reason: Str::new(wire.reason?),
					suppress_for_turns,
				})))
			},
			(
				"custom_summary",
				Some(WireVerdictDetails::Custom { short, tokens_before, warning, details, preserve }),
			) => Some(Some(CompactionVerdict::Custom(CustomSummary {
				compact: Compact {
					summary: Str::new(wire.summary?),
					short,
					first_kept: wire.first_kept_id?.parse().ok()?,
					tokens_before,
					tokens_after: None,
					method: None,
					warning,
					superseded: Vec::new(),
					snapcompact: None,
				},
				details,
				preserve,
			}))),
			(
				"delegate",
				Some(WireVerdictDetails::Delegate {
					extra_instructions,
					focus_ids,
					role,
					keep_recent_tokens,
				}),
			) => Some(Some(CompactionVerdict::Delegate(DelegateCompaction {
				extra_instructions,
				focus_ids,
				role,
				keep_recent_tokens,
			}))),
			("none", None) => Some(None),
			_ => None,
		}
	}

	fn fail_open() -> Self {
		None
	}

	fn merge_domain(self, next: Self) -> Self {
		match (self, next) {
			(_, Some(CompactionVerdict::Cancel(cancel))) => Some(CompactionVerdict::Cancel(cancel)),
			(Some(CompactionVerdict::Cancel(cancel)), _) => Some(CompactionVerdict::Cancel(cancel)),
			(Some(CompactionVerdict::Custom(summary)), _) => Some(CompactionVerdict::Custom(summary)),
			(None, next) => next,
			(current, None) => current,
			(
				Some(CompactionVerdict::Delegate(mut current)),
				Some(CompactionVerdict::Delegate(next)),
			) => {
				compose_delegate(&mut current, &next);
				Some(CompactionVerdict::Delegate(current))
			},
			(Some(CompactionVerdict::Delegate(_)), Some(CompactionVerdict::Custom(summary))) => {
				Some(CompactionVerdict::Custom(summary))
			},
		}
	}
}

/// Encodes a domain compaction verdict for a `HookGate::gate_domain` reply.
pub fn encode_domain_verdict(verdict: Option<&CompactionVerdict>) -> Bytes {
	let wire = match verdict {
		None => WireCompactionVerdict { kind: "none".to_owned(), ..Default::default() },
		Some(CompactionVerdict::Cancel(cancel)) => WireCompactionVerdict {
			kind: "cancel".to_owned(),
			reason: Some(cancel.reason.as_str().to_owned()),
			details_json: Some(encode_details(&WireVerdictDetails::Cancel {
				suppress_for_turns: cancel.suppress_for_turns,
			})),
			..Default::default()
		},
		Some(CompactionVerdict::Custom(summary)) => WireCompactionVerdict {
			kind: "custom_summary".to_owned(),
			summary: Some(summary.compact.summary.as_str().to_owned()),
			first_kept_id: Some(summary.compact.first_kept.to_string()),
			details_json: Some(encode_details(&WireVerdictDetails::Custom {
				short:         summary.compact.short.clone(),
				tokens_before: summary.compact.tokens_before,
				warning:       summary.compact.warning.clone(),
				details:       summary.details.clone(),
				preserve:      summary.preserve.clone(),
			})),
			..Default::default()
		},
		Some(CompactionVerdict::Delegate(delegate)) => WireCompactionVerdict {
			kind: "delegate".to_owned(),
			details_json: Some(encode_details(&WireVerdictDetails::Delegate {
				extra_instructions: delegate.extra_instructions.clone(),
				focus_ids:          delegate.focus_ids.clone(),
				role:               delegate.role.clone(),
				keep_recent_tokens: delegate.keep_recent_tokens,
			})),
			..Default::default()
		},
	};
	let mut encoded = BytesMut::new();
	wire
		.encode(&mut encoded)
		.expect("bytes buffer cannot fail protobuf encoding");
	encoded.freeze()
}

fn encode_details(details: &WireVerdictDetails) -> Bytes {
	Bytes::from(serde_json::to_vec(details).expect("compaction verdict details are serializable"))
}

/// Deterministically composed result of one compaction hook dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionResolution {
	/// A handler cancelled this rung; later handlers are not consulted.
	Cancel(CancelCompaction),
	/// A custom textual summary won, with durable loser metadata.
	Custom {
		/// Winning extension summary.
		winner: CustomSummary,
		/// Extension that supplied the winner, when dispatch preserved
		/// attribution.
		source: Option<Str>,
		/// Ordered loser records to persist alongside `Kind::Compact`.
		losers: Vec<SupersededCompaction>,
	},
	/// Built-in behavior with all ordered delegate fields composed.
	Delegate(DelegateCompaction),
	/// No handler expressed an opinion.
	Default,
}

/// Composes built-in tier instructions in deterministic handler order.
fn compose_delegate(current: &mut DelegateCompaction, next: &DelegateCompaction) {
	if !next.extra_instructions.is_empty() {
		current.extra_instructions = if current.extra_instructions.is_empty() {
			next.extra_instructions.clone()
		} else {
			sf!("{}\n{}", current.extra_instructions.as_str(), next.extra_instructions.as_str())
		};
	}
	for id in &next.focus_ids {
		if !current.focus_ids.iter().any(|known| known == id) {
			current.focus_ids.push(id.clone());
		}
	}
	if current.role.is_none() {
		current.role.clone_from(&next.role);
	}
	if current.keep_recent_tokens.is_none() {
		current.keep_recent_tokens = next.keep_recent_tokens;
	}
}

impl CompactionResolution {
	/// Consumes a winning custom summary into the durable journal payload.
	///
	/// Only this path carries ordered superseded-summary metadata into
	/// `Journal::compact`; cancellation and delegated/default outcomes leave
	/// durable compaction to their respective built-in rungs.
	pub fn into_compact(self) -> Option<Compact> {
		let Self::Custom { mut winner, losers, .. } = self else {
			return None;
		};
		winner.compact.superseded = losers;
		Some(winner.compact)
	}
}

/// Resolves one fail-open domain result without source attribution.
fn resolve_one(verdict: Option<CompactionVerdict>) -> CompactionResolution {
	match verdict {
		Some(CompactionVerdict::Cancel(cancel)) => CompactionResolution::Cancel(cancel),
		Some(CompactionVerdict::Custom(winner)) => {
			CompactionResolution::Custom { winner, source: None, losers: Vec::new() }
		},
		Some(CompactionVerdict::Delegate(delegate)) => CompactionResolution::Delegate(delegate),
		None => CompactionResolution::Default,
	}
}

/// Resolves handler verdicts in `(layer, publisher, extension_id)` order.
///
/// The first cancellation wins immediately. Otherwise the first custom summary
/// wins and later custom summaries become ordered metadata; delegate fields
/// compose only when no custom summary replaces the rung.
pub fn resolve_verdicts(
	verdicts: &mut [(SourceRef, Option<CompactionVerdict>)],
) -> CompactionResolution {
	verdicts.sort_by(|left, right| left.0.cmp(&right.0));
	let mut winner: Option<(Str, CustomSummary)> = None;
	let mut losers = Vec::new();
	let mut delegate = DelegateCompaction::default();
	for (source, returned) in verdicts {
		let Some(verdict) = returned.as_ref() else {
			continue;
		};
		match verdict {
			CompactionVerdict::Cancel(cancel) => return CompactionResolution::Cancel(cancel.clone()),
			CompactionVerdict::Custom(summary) => {
				if winner.is_none() {
					winner = Some((source.extension_id.clone(), summary.clone()));
				} else {
					losers.push(SupersededCompaction {
						extension_id: source.extension_id.clone(),
						reason:       sf!("custom_summary_superseded"),
					});
				}
			},
			CompactionVerdict::Delegate(next) if winner.is_none() => {
				compose_delegate(&mut delegate, next);
			},
			CompactionVerdict::Delegate(_) => {},
		}
	}
	if let Some((source, winner)) = winner {
		CompactionResolution::Custom { winner, source: Some(source), losers }
	} else if delegate != DelegateCompaction::default() {
		CompactionResolution::Delegate(delegate)
	} else {
		CompactionResolution::Default
	}
}

/// Provider-native remote compaction checkpoint retained only for its origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteCheckpoint {
	/// Origin provider.
	pub provider: ProviderId,
	/// Origin model.
	pub model:    ModelId,
	/// Blob containing provider-native replay items.
	pub items:    BlobRef,
}

impl RemoteCheckpoint {
	/// Returns whether this opaque checkpoint is safe for the active model.
	pub fn reusable_for(&self, active: &ModelRef) -> bool {
		checkpoint_reusable(&self.provider, &self.model, active)
	}

	/// Converts this verified checkpoint into the existing durable transcript
	/// representation used by the `REMOTE` rung.
	pub fn into_event(self) -> Kind {
		Kind::NativeCheckpoint { provider: self.provider, model: self.model, items: self.items }
	}
}

#[cfg(test)]
mod tests {

	use omp_core::sf;

	use super::{
		COMPACTION_RECOVERY_BAND, Compact, CompactionCoordinator, CompactionDecision,
		CompactionHysteresis, CompactionMethodOrder, CompactionSpeculationOptions, CompactionTier,
		CompactionVerdict, ContextUsage, CustomSummary, ItemUsage, ProjectionItem,
		SPECULATION_LEAD_MIN_TOKENS, SpeculationRequest, SpeculationResult, SpeculationState,
		back_project_provider_usage, plan_lossless_with_warm_suffix, resolve_verdicts,
	};
	use crate::hooks::SourceRef;

	#[test]
	fn hysteresis_triggers_at_threshold_and_rearms_at_recovery_edge() {
		let mut hysteresis = CompactionHysteresis::default();
		let mut usage = ContextUsage::new(80, 100, 0, 0.8);
		assert!(hysteresis.evaluate(usage));
		assert!(!hysteresis.armed());
		assert!(!hysteresis.evaluate(usage));
		usage.total_tokens = (80.0 * COMPACTION_RECOVERY_BAND) as u64;
		assert!(!hysteresis.evaluate(usage));
		assert!(hysteresis.armed());
	}

	fn speculative_compact(
		request: &SpeculationRequest,
		summary: &'static str,
	) -> SpeculationResult {
		SpeculationResult {
			request: request.clone(),
			compact: Compact {
				summary:       sf!(summary),
				short:         Some(sf!("preview title")),
				first_kept:    1,
				tokens_before: request.tokens_at_start,
				tokens_after:  Some(20_000),
				method:        None,
				warning:       None,
				superseded:    Vec::new(),
				snapcompact:   None,
			},
		}
	}

	#[test]
	fn speculation_starts_at_lead_edge_on_an_isolated_session() {
		let mut coordinator = CompactionCoordinator::default();
		let order = CompactionMethodOrder::resolve(&[CompactionTier::Remote]);
		let session = sf!("live");
		let below = ContextUsage::new(41_807, 100_000, 0, 0.5);
		assert_eq!(
			coordinator.evaluate(
				below,
				&order,
				CompactionSpeculationOptions::default(),
				&session,
				7,
				true,
			),
			CompactionDecision::None,
		);
		let edge = ContextUsage::new(50_000 - SPECULATION_LEAD_MIN_TOKENS, 100_000, 0, 0.5);
		let CompactionDecision::Launch { request, defer_blocking, cancel_run } = coordinator
			.evaluate(edge, &order, CompactionSpeculationOptions::default(), &session, 7, true)
		else {
			panic!("lead edge launches speculation");
		};
		assert!(!defer_blocking);
		assert_eq!(cancel_run, None);
		assert_ne!(request.snapshot.isolated_session_id, request.snapshot.session_id);
		assert_eq!(coordinator.speculation_state(), SpeculationState::Running);
		assert_eq!(
			coordinator.evaluate(
				edge,
				&order,
				CompactionSpeculationOptions { enabled: false, keep_recent_tokens: 20_000 },
				&session,
				7,
				true,
			),
			CompactionDecision::Cancel { run_id: request.run_id },
		);
		assert_eq!(coordinator.speculation_state(), SpeculationState::Idle);
	}

	#[test]
	fn armed_summary_commits_in_place_with_method_and_token_metadata() {
		let mut coordinator = CompactionCoordinator::default();
		let order = CompactionMethodOrder::resolve(&[CompactionTier::Remote]);
		let session = sf!("live");
		let usage = ContextUsage::new(42_000, 100_000, 0, 0.5);
		let CompactionDecision::Launch { request, .. } = coordinator.evaluate(
			usage,
			&order,
			CompactionSpeculationOptions::default(),
			&session,
			7,
			true,
		) else {
			panic!("speculation launches");
		};
		assert!(coordinator.arm(speculative_compact(&request, "armed")));
		assert_eq!(coordinator.speculation_state(), SpeculationState::Armed);
		let threshold = ContextUsage::new(50_000, 100_000, 0, 0.5);
		let CompactionDecision::Commit(compact) = coordinator.evaluate(
			threshold,
			&order,
			CompactionSpeculationOptions::default(),
			&session,
			8,
			true,
		) else {
			panic!("armed summary commits");
		};
		assert_eq!(compact.summary.as_str(), "armed");
		assert_eq!(compact.short.as_deref(), Some("preview title"));
		assert_eq!(compact.tokens_before, 42_000);
		assert_eq!(compact.tokens_after, Some(20_000));
		assert_eq!(compact.method.as_deref(), Some("remote"));
		assert_eq!(coordinator.speculation_state(), SpeculationState::Idle);
	}

	#[test]
	fn local_snapshot_method_discards_an_armed_llm_summary() {
		let mut coordinator = CompactionCoordinator::default();
		let remote = CompactionMethodOrder::resolve(&[CompactionTier::Remote]);
		let session = sf!("live");
		let usage = ContextUsage::new(42_000, 100_000, 0, 0.5);
		let CompactionDecision::Launch { request, .. } = coordinator.evaluate(
			usage,
			&remote,
			CompactionSpeculationOptions::default(),
			&session,
			7,
			true,
		) else {
			panic!("speculation launches");
		};
		assert!(coordinator.arm(speculative_compact(&request, "discard me")));

		let local_first =
			CompactionMethodOrder::resolve(&[CompactionTier::Local, CompactionTier::Remote]);
		let threshold = ContextUsage::new(50_000, 100_000, 0, 0.5);
		assert_eq!(
			coordinator.evaluate(
				threshold,
				&local_first,
				CompactionSpeculationOptions::default(),
				&session,
				8,
				true,
			),
			CompactionDecision::Block { cancel_run: None },
		);
		assert_eq!(coordinator.speculation_state(), SpeculationState::Idle);
		assert!(!coordinator.arm(speculative_compact(&request, "late")));
	}

	#[test]
	fn reset_epoch_discards_armed_summary_and_launches_a_fresh_snapshot() {
		let mut coordinator = CompactionCoordinator::default();
		let order = CompactionMethodOrder::resolve(&[CompactionTier::Remote]);
		let session = sf!("live");
		let usage = ContextUsage::new(42_000, 100_000, 0, 0.5);
		let CompactionDecision::Launch { request, .. } = coordinator.evaluate(
			usage,
			&order,
			CompactionSpeculationOptions::default(),
			&session,
			7,
			true,
		) else {
			panic!("speculation launches");
		};
		assert!(coordinator.arm(speculative_compact(&request, "stale")));
		let mut reset_usage = ContextUsage::new(51_000, 100_000, 0, 0.5);
		reset_usage.compaction_epoch = 1;
		let CompactionDecision::Launch { request: refreshed, defer_blocking, .. } = coordinator
			.evaluate(
				reset_usage,
				&order,
				CompactionSpeculationOptions::default(),
				&session,
				9,
				false,
			)
		else {
			panic!("stale armed summary is replaced");
		};
		assert!(defer_blocking);
		assert_ne!(refreshed.run_id, request.run_id);
		assert_eq!(refreshed.snapshot.compaction_epoch, 1);
		assert_eq!(coordinator.speculation_state(), SpeculationState::Running);
	}

	#[test]
	fn grace_band_defers_running_work_then_rearms_blocking_at_cap() {
		let mut coordinator = CompactionCoordinator::default();
		let order = CompactionMethodOrder::resolve(&[CompactionTier::Remote]);
		let session = sf!("live");
		let jumped = ContextUsage::new(51_000, 100_000, 0, 0.5);
		let CompactionDecision::Launch { request, defer_blocking, .. } = coordinator.evaluate(
			jumped,
			&order,
			CompactionSpeculationOptions::default(),
			&session,
			7,
			true,
		) else {
			panic!("jump starts grace speculation");
		};
		assert!(defer_blocking);
		assert_eq!(
			coordinator.evaluate(
				ContextUsage::new(51_500, 100_000, 0, 0.5),
				&order,
				CompactionSpeculationOptions::default(),
				&session,
				8,
				true,
			),
			CompactionDecision::Defer,
		);
		assert_eq!(
			coordinator.evaluate(
				ContextUsage::new(58_192, 100_000, 0, 0.5),
				&order,
				CompactionSpeculationOptions::default(),
				&session,
				9,
				true,
			),
			CompactionDecision::Block { cancel_run: Some(request.run_id) },
		);
	}

	#[test]
	fn method_order_preserves_first_occurrence_and_empty_disables_ladder() {
		let order = CompactionMethodOrder::resolve(&[
			CompactionTier::Remote,
			CompactionTier::Local,
			CompactionTier::Remote,
			CompactionTier::Handoff,
		]);
		assert_eq!(order.as_slice(), &[
			CompactionTier::Remote,
			CompactionTier::Local,
			CompactionTier::Handoff
		],);
		assert_eq!(
			order
				.available(|tier| tier != CompactionTier::Remote)
				.as_slice(),
			&[CompactionTier::Local, CompactionTier::Handoff],
		);
		assert!(CompactionMethodOrder::resolve(&[]).as_slice().is_empty());
		let mut coordinator = CompactionCoordinator::default();
		assert_eq!(
			coordinator.evaluate(
				ContextUsage::new(90, 100, 0, 0.5),
				&CompactionMethodOrder::resolve(&[]),
				CompactionSpeculationOptions::default(),
				&sf!("live"),
				1,
				true,
			),
			CompactionDecision::None,
		);
		assert_eq!(CompactionMethodOrder::default().as_slice(), &CompactionTier::ALL);
	}

	#[test]
	fn lossless_prune_leaves_non_targets_identical() {
		let retained = ProjectionItem {
			event:       1,
			useless:     false,
			superseded:  false,
			media_parts: 0,
			usage:       ItemUsage { byte_len: 10, provider_tokens: 2 },
		};
		let pruned = ProjectionItem { event: 2, useless: true, ..retained };
		let projection = [retained, pruned];
		let plan = plan_lossless_with_warm_suffix(&projection, 0, |_| false);
		assert_eq!(plan.prune, vec![2]);
		assert_eq!(projection[0], retained);
	}

	#[test]
	fn provider_usage_back_projection_preserves_exact_total() {
		let mut usage = [ItemUsage { byte_len: 1, provider_tokens: 0 }, ItemUsage {
			byte_len:        3,
			provider_tokens: 0,
		}];
		back_project_provider_usage(7, &mut usage);
		assert_eq!(usage.iter().map(|item| item.provider_tokens).sum::<u64>(), 7);
	}

	#[test]
	fn custom_summary_winner_and_loser_metadata_follow_publisher_order() {
		let summary = |text: &'static str, first_kept| {
			CompactionVerdict::Custom(CustomSummary {
				compact:  Compact {
					summary: sf!(text),
					short: None,
					first_kept,
					tokens_before: 100,
					tokens_after: None,
					method: None,
					warning: None,
					superseded: Vec::new(),
					snapcompact: None,
				},
				details:  None,
				preserve: None,
			})
		};
		let mut verdicts = [
			(
				SourceRef { layer: 1, publisher: sf!("z"), extension_id: sf!("late") },
				Some(summary("late", 8)),
			),
			(
				SourceRef { layer: 1, publisher: sf!("a"), extension_id: sf!("early") },
				Some(summary("early", 4)),
			),
		];
		let resolution = resolve_verdicts(&mut verdicts);
		let compact = resolution.into_compact().expect("custom summary resolves");
		assert_eq!(compact.summary.as_str(), "early");
		assert_eq!(compact.superseded.len(), 1);
		assert_eq!(compact.superseded[0].extension_id.as_str(), "late");
	}
}
