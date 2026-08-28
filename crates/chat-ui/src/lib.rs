//! Host-agnostic immediate-mode chat scene and matching overlays.
//!
//! The crate owns presentation state only. A host forwards [`Intent`] values
//! to its backend and applies [`BackendEvent`] values to [`Chat`].

#![forbid(unsafe_code)]

pub mod actions;
pub mod advisor_config;
pub mod agent_hub;
mod approval;
pub mod ask;
pub mod autoqa;
pub mod blocks;
pub mod completion;
pub mod debug_selector;
pub mod extension_inspector;
pub mod extension_ui;
pub mod frame;
pub mod git;
pub mod gradient;
pub mod host;
pub mod inspector;
pub mod log_viewer;
pub mod login_panel;
pub mod modes;
mod overlays;
pub mod palette;
pub mod picker;
pub mod plan_review;
pub mod protocol_probe;
pub mod provider_picker;
pub mod pty;
pub mod queue;
pub mod raw_stream;
pub mod scene;
pub mod selection_overlay;
pub mod settings_overlay;
pub mod sidebar;
pub mod status_line;
pub mod vibe_wall;
pub mod welcome;

pub mod slots;
use std::{sync::Arc, time::Instant};

pub use agent_hub::{AgentHub, AgentHubEvent};
pub use blocks::{BlockOrdinal, BlockPhase, BlockTarget, Blocks, Overflow, Plan};
pub mod image_overlay;
pub use extension_inspector::{
	ExtensionCatalogSource, ExtensionDetail, ExtensionDisposition, ExtensionInspector,
	ExtensionInspectorEvent, ExtensionKind, ExtensionOrigin, ExtensionRow, ExtensionSnapshot,
	LiveToolView, McpCatalogEntry, McpHealth, McpLiveSnapshot, McpToolView,
};
pub use extension_ui::{ExtensionDialog, ExtensionModalEvent, ExtensionOverlay};
pub use git::{
	GitArea, GitChangeKind, GitCommitInfo, GitFileContents, GitFileRow, GitIntent, GitPatchOp,
	GitSnapshot, GitUpdate, GitWorkbench, GitWorkbenchEvent,
};
pub use gradient::{EditorGradient, EditorHighlight, GradientStop};
pub use image_overlay::{ImageOverlay, ImageOverlayEvent};
pub use inspector::{HistoryInspector, HistoryInspectorEvent};
use omp_core::Str;
use omp_proto::omp::ui::v1;
pub use omp_tui::components::{Attachment, CompactionBoundaries, ComposerStyle};
pub use overlays::{ListPicker, ListRow, OverlayPanel, PromptEvent, PromptOverlay, panel_divider};
pub use palette::{CommandPalette, PaletteAction, PaletteEntry, PaletteEvent};
pub use picker::{ModelPicker, PickerEvent};
pub use provider_picker::ProviderPicker;
pub use pty::{PtyEvent, PtyOutputQueue, PtyOverlay, PtyStatus, TerminalState};
pub use raw_stream::{RawFrame, RawStreamEvent, RawStreamViewer, StreamSummary};
pub use scene::{
	Chat, ChatKey, LiveVoiceAction, LiveVoicePhase, LiveVoiceVisualizer, RetirementBatch,
	ToolPresentation, ViewportFrame,
};
pub use selection_overlay::{SelectionEvent, SelectionOverlay, SelectionPurpose};
pub use settings_overlay::{SettingChange, SettingsEvent, SettingsOverlay};
pub use sidebar::Sidebar;
pub use welcome::{Welcome, WelcomeEvent};

/// One model shown by the model picker.
#[derive(Clone, Debug)]
pub struct ModelRow {
	/// Stable backend model key.
	pub key:         Str,
	/// Human-readable model name.
	pub name:        Str,
	/// Optional configured role color token.
	pub color:       Option<Str>,
	/// Stable provider identifier used to resolve its packaged logo.
	pub provider_id: Str,
	/// Human-readable provider name.
	pub provider:    Str,
	/// Context-window size in tokens, when known.
	pub context:     Option<u64>,
	/// Input price in dollars per million tokens, when known.
	pub input_mtok:  Option<f64>,
	/// Output price in dollars per million tokens, when known.
	pub output_mtok: Option<f64>,
}
/// Immutable projection of the environment-owned todo tree for the sticky HUD.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TodoHud {
	/// Pre-rendered Markdown lines from the canonical todo tool result.
	pub lines:       Vec<Str>,
	/// Number of task rows represented by `lines`.
	pub total_tasks: usize,
}

/// One reflected setting shown by schema-driven TUI surfaces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingRow {
	/// Owning settings domain.
	pub domain:      Str,
	/// Stable dotted settings path.
	pub path:        Str,
	/// Human-readable field label.
	pub label:       Str,
	/// User-facing description.
	pub description: Str,
	/// Reflected widget kind.
	pub kind:        Str,
	/// Stable settings panel identifier.
	pub panel:       Str,
	/// Whether the value must never be projected into the UI.
	pub secret:      bool,
	/// Current merged value, masked by the settings authority when secret.
	pub value:       Option<Str>,
	/// Current dynamic or static option labels.
	pub options:     Vec<Str>,
	/// Whether descriptor conditions currently expose this field.
	pub visible:     bool,
}

/// One resumable session shown by a list picker.
#[derive(Clone, Debug)]
pub struct SessionRow {
	/// Stable session identifier.
	pub id:     Str,
	/// Primary display label.
	pub label:  Str,
	/// Secondary display detail.
	pub detail: Str,
	/// Whether the session is pinned above ordinary recency ordering.
	pub pinned: bool,
}
/// One language server shown on the welcome card.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WelcomeLspServer {
	/// Declared server name.
	pub name:        Str,
	/// Lowercase lifecycle label, optionally followed by file types.
	pub stage_label: Str,
	/// Whether the lifecycle ended in failure.
	pub failed:      bool,
}
/// One live node projected from the core-owned `AgentTree` roster.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRow {
	/// Stable agent identity.
	pub id:               Str,
	/// User-facing agent name.
	pub name:             Str,
	/// Parent identity, absent for a root.
	pub parent:           Option<Str>,
	/// Hierarchy depth.
	pub depth:            u16,
	/// Allocation-free lifecycle status snapshot.
	pub status:           Str,
	/// Currently executing tool, when known.
	pub tool:             Option<Str>,
	/// Token consumption, when known.
	pub tokens:           Option<u64>,
	/// Resolved delegated-agent definition badge.
	pub definition:       Option<Str>,
	/// Requested model selector badge.
	pub model:            Option<Str>,
	/// Model that served the latest request.
	pub serving_model:    Option<Str>,
	/// Bounded transcript/activity preview supplied by the tree owner.
	pub transcript:       Str,
	/// Deterministic assignment brief recovered from the child journal.
	pub assignment:       Option<Str>,
	/// Assistant requests committed in the current generation.
	pub requests:         u32,
	/// Tool calls committed in the current generation.
	pub tool_calls:       u32,
	/// Latest provider context size.
	pub context_tokens:   u64,
	/// Durable cost attributed to the current generation, in micro-USD.
	pub cost_micros:      u64,
	/// Structured terminal verdict, when the generation has settled.
	pub terminal_kind:    Option<Str>,
	/// Retained terminal summary.
	pub terminal_summary: Option<Str>,
	/// Durable full-output artifact URI, when available.
	pub artifact_uri:     Option<Str>,
	/// Whether this row is an immutable terminal snapshot retained for
	/// scrollback.
	pub frozen:           bool,
	/// Whether the backend currently accepts steering for this node.
	pub can_steer:        bool,
	/// Whether the backend currently accepts cold revival for this node.
	pub can_revive:       bool,
	/// Whether the backend currently accepts cancellation for this node.
	pub can_kill:         bool,
}

/// Core-owned transcript frame category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptFrameKind {
	/// Context compaction summary.
	Compaction,
	/// Session branch boundary.
	Branch,
	/// Session handoff boundary.
	Handoff,
	/// Prompt-cache invalidation.
	CacheBreak,
	/// Automatic context recovery.
	Recovery,
	/// Display-only peer-to-peer coordination observation.
	Peer,
	/// Turn-ending error.
	Error,
}

/// One backend-authored transcript frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptFrame {
	/// Semantic frame category.
	pub kind:   TranscriptFrameKind,
	/// Compact frame heading.
	pub title:  Str,
	/// Optional explanatory body.
	pub detail: Option<Str>,
}

/// Optional repository facts for the status line.
#[derive(Clone, Debug)]
pub struct GitFacts {
	/// Current branch name.
	pub branch: Str,
	/// Number of dirty paths.
	pub dirty:  u32,
	/// Number of staged paths.
	pub staged: u32,
}

/// Background compaction state rendered by status surfaces.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CompactionSpeculationStatus {
	/// No background compaction exists.
	#[default]
	Idle,
	/// A detached snapshot is being compacted.
	Running,
	/// A speculative summary is ready for the threshold boundary.
	Armed,
}

/// Fixed-capacity activity bands rendered by the `/live` status sparkline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivityWaveform {
	bands: [u8; 24],
	len:   u8,
}

impl ActivityWaveform {
	/// Creates an empty activity history.
	pub const fn new() -> Self {
		Self { bands: [0; 24], len: 0 }
	}

	/// Appends one normalized activity band, dropping the oldest when full.
	pub fn push(&mut self, band: u8) {
		let band = band.min(4);
		let len = usize::from(self.len);
		if len < self.bands.len() {
			self.bands[len] = band;
			self.len = self.len.saturating_add(1);
		} else {
			self.bands.copy_within(1.., 0);
			let last = self.bands.len() - 1;
			self.bands[last] = band;
		}
	}

	/// Borrows populated bands from oldest to newest.
	pub fn bands(&self) -> &[u8] {
		&self.bands[..usize::from(self.len)]
	}
}

impl Default for ActivityWaveform {
	fn default() -> Self {
		Self::new()
	}
}

/// One visible resource owner projected by the backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleResourceFacts {
	/// Canonical resource name.
	pub resource:    Str,
	/// Regime currently owning the resource.
	pub owner:       Str,
	/// Durable FIFO activations waiting behind the owner.
	pub queue_depth: usize,
}

/// Complete host-supplied status snapshot.
#[derive(Clone, Debug, Default)]
pub struct StatusFacts {
	/// Model label shown in the status line.
	pub model:                  Str,
	/// Whether the primary model uses subscription billing.
	pub model_subscription:     bool,
	/// Advisor model label, when an advisor is configured or active.
	pub advisor_model:          Option<Str>,
	/// Whether the advisor model uses subscription billing.
	pub advisor_subscription:   bool,
	/// Whether a backend turn is active.
	pub working:                bool,
	/// Wall-clock start of the active turn, when available.
	pub turn_started:           Option<Instant>,
	/// Context tokens currently in use.
	pub context_tokens:         u64,
	/// Model context window, when known.
	pub context_window:         Option<u64>,
	/// Background speculative-compaction lifecycle.
	pub compaction_speculation: CompactionSpeculationStatus,
	/// Auto-compaction boundary percents for the embedded context gauge,
	/// absent when compaction is disabled or the window is unknown.
	pub compaction_boundaries:  Option<CompactionBoundaries>,
	/// Accumulated cost in billionths of a dollar.
	pub cost_nanos:             u64,
	/// Accumulated advisor-model cost in billionths of a dollar.
	pub advisor_cost_nanos:     u64,
	/// Number of queued user submissions.
	pub queued:                 usize,
	/// Resources whose owning regimes opt into user-facing status.
	pub visible_resources:      Arc<[VisibleResourceFacts]>,
	/// Number of active background jobs.
	pub jobs:                   usize,
	/// Current retry attempt.
	pub attempt:                u32,
	/// Number of dropped backend events.
	pub dropped:                u64,
	/// Repository facts, omitted when unavailable.
	pub git:                    Option<GitFacts>,
	/// `/live` firehose activity history, absent when live display is disabled.
	pub live_activity:          Option<ActivityWaveform>,
	/// Smoothed provider output velocity, in tokens per second.
	pub tokens_per_second:      Option<u64>,
	/// Current Environment working directory.
	pub cwd:                    Option<Str>,
	/// Active worktree label when distinct from `cwd`.
	pub worktree:               Option<Str>,
	/// Effective reasoning effort rendered as the model-segment glyph.
	pub thinking:               Option<ThinkingLevel>,
	/// Number of active hook facts.
	pub hooks:                  usize,
	/// Number of active durable tasks.
	pub tasks:                  usize,
	/// Number of connected collaboration peers.
	pub collab_peers:           usize,
	/// Opaque account-override display label; never a credential.
	pub account_override:       Option<Str>,
	/// One-shot quota-reset edge emitted by the provider usage authority.
	pub quota_reset:            bool,
	/// Disable non-essential retained animation.
	pub reduced_motion:         bool,
	/// Responsive status shedding policy.
	pub layout:                 StatusLayout,
	/// Separator used between visible status segments.
	pub separator:              StatusSeparator,
}

/// Core-timed working indicator supplied by one extension command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingIndicator {
	/// Ordered animation frames, already display-width validated by the host.
	pub frames:      Box<[Str]>,
	/// Frame interval in milliseconds.
	pub interval_ms: u64,
}

/// Reasoning effort represented by the model-status glyph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThinkingLevel {
	/// Minimal reasoning effort.
	Minimal,
	/// Low reasoning effort.
	Low,
	/// Medium reasoning effort.
	Medium,
	/// High reasoning effort.
	High,
	/// Extra-high reasoning effort.
	Xhigh,
	/// Maximum reasoning effort.
	Max,
}
/// Responsive status-segment shedding policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StatusLayout {
	/// Balanced defaults with secondary facts shed first.
	#[default]
	Compact,
	/// Show every available owner-facing fact.
	Full,
	/// Prefer diagnostics, timing, and throughput.
	Developer,
	/// Keep only model, working state, and context.
	Minimal,
}

/// Visual separator between status segments.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StatusSeparator {
	/// Centered dot on Unicode terminals, a period in ASCII.
	#[default]
	Dot,
	/// Put each segment in square brackets.
	Bracket,
}

/// How a composer submission interacts with an active turn.
///
/// Idle backends treat both modes as a plain submission; the distinction
/// only matters while a turn is running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitMode {
	/// Enter: steer the active turn by delivering the message immediately.
	Steer,
	/// Alt+Enter: queue the message as a follow-up after the active turn.
	FollowUp,
}
/// Provenance-typed content for a tool card.
///
/// Only renderer-authored markup is parsed. Generic output and fallback text
/// remain literal even when they contain valid TML tags.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolViewContent {
	/// Trusted core or exact-revision renderer markup.
	Markup(Str),
	/// Untrusted generic output rendered as literal text.
	Plain(Str),
}
impl ToolViewContent {
	/// Borrows the underlying source without changing its provenance.
	pub fn as_str(&self) -> &str {
		match self {
			Self::Markup(source) | Self::Plain(source) => source.as_str(),
		}
	}
}

/// Semantic terminal branch of a tool invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolTerminal {
	/// The tool produced a successful durable result.
	Succeeded,
	/// Execution faulted after accepting its arguments.
	Failed,
	/// Execution was cancelled or ended without a result.
	Aborted,
	/// Execution was intentionally skipped before effects began.
	Skipped,
	/// The tool rejected its arguments without executing.
	ArgsRejected,
}

/// Host-agnostic projection of one pending durable approval ticket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalTicketView {
	/// Stable idempotency key.
	pub ticket_id:     Str,
	/// Invocation blocked by this ticket, when present.
	pub invocation_id: Option<Str>,
	/// Compact merged-reason title.
	pub title:         Str,
	/// TML-safe merged-reason detail.
	pub detail:        Str,
	/// Exact command, path, or device subject.
	pub subject:       Str,
	/// Narrowest persistent scope offered by policy.
	pub always_scope:  Option<Str>,
	/// Rule and derived-fact evidence.
	pub evidence:      Vec<Str>,
}

/// One user decision for a pending approval ticket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalAction {
	/// Approve only the blocked invocation.
	AllowOnce,
	/// Approve and persist the narrowest offered policy scope.
	AllowAlways,
	/// Deny the invocation.
	Reject,
	/// Replace the exact subject and approve only that amended invocation.
	Amend(Str),
}
/// Outbound intent for the host to forward to its backend.
#[derive(Clone)]
pub enum Intent {
	/// Forward one interactive Git workbench request.
	Git(GitIntent),
	/// Submit composer text and staged attachments.
	Submit {
		/// User-authored composer text.
		text:        String,
		/// Attachments staged with the submission.
		attachments: Vec<Attachment>,
		/// Active-turn delivery discipline for this submission.
		mode:        SubmitMode,
	},
	/// Abort the active turn.
	Abort,
	/// Invoke one app-owned extension shortcut already matched by the host.
	ExtensionShortcut(Str),
	/// Request one transient recap after the idle deadline expires.
	IdleRecap,
	/// Create a goal from the retained guided interview.
	SetGoal {
		/// Interview objective.
		objective:    Str,
		/// Optional positive hard token budget.
		token_budget: Option<u64>,
	},
	/// Steer one selected child through the core-owned agent authority.
	AgentSteer {
		/// Stable agent identity.
		id:     Str,
		/// User-authored steering prompt.
		prompt: Str,
	},
	/// Revive one selected cold child through the core-owned agent authority.
	AgentRevive {
		/// Stable agent identity.
		id:     Str,
		/// User-authored follow-up prompt.
		prompt: Str,
	},
	/// Cancel one selected live child through the core-owned agent authority.
	AgentKill {
		/// Stable agent identity.
		id: Str,
	},
	/// Write exact bytes to an interactive PTY execution.
	PtyInput {
		/// Stable tool-call identity.
		id:   Str,
		/// Bytes translated by the terminal overlay.
		data: bytes::Bytes,
	},
	/// Resize an interactive PTY execution.
	PtyResize {
		/// Stable tool-call identity.
		id:      Str,
		/// Terminal rows.
		rows:    u16,
		/// Terminal columns.
		columns: u16,
	},
	/// Force-kill an interactive PTY execution.
	PtyKill {
		/// Stable tool-call identity.
		id: Str,
	},
	/// Settle one durable approval ticket exactly once.
	Approval {
		/// Stable ticket idempotency key.
		ticket_id: Str,
		/// User-selected action.
		action:    ApprovalAction,
	},
	/// Apply one explicit AutoQA upload decision.
	AutoQaConsent(autoqa::ConsentIntent),
	/// Ask the backend for rewind targets.
	RewindRequest,
	/// Open the alternate-screen inspector over canonical conversation history.
	///
	/// The terminal host binds this to Alt+H because legacy terminal input
	/// decodes Ctrl+H as Backspace.
	InspectHistory,
	/// Ask the backend for prompt-history rows to open the history selector.
	SearchHistory,
	/// Rewind the durable transcript to an event.
	Rewind {
		/// Event to keep as the new live-chain tail.
		event: u64,
	},
	/// Switch the active model.
	SwitchModel(Str),
	/// Start login, optionally for a specific provider.
	Login(Option<Str>),
	/// Continue provider/account selection or remove one stored credential.
	Logout(Option<Str>),
	/// Answer the active authentication prompt.
	AuthAnswer {
		/// Unmasked value entered by the user.
		value: String,
	},
	/// Cancel the active authentication prompt.
	AuthCancel,
	/// Resume a session, or request the session picker when absent.
	Resume(Option<Str>),
	/// Start a fresh session.
	NewSession,
	/// Show help.
	Help,
	/// Quit the host.
	Quit,
	/// Restore every producer-authored input that has not started to the
	/// composer.
	Dequeue,
	/// Retry the latest durable user turn.
	Retry,
	/// Cycle forward or backward through the active model roster.
	CycleModel {
		/// `true` cycles to the previous roster entry.
		backward: bool,
	},
	/// Cycle to the next supported reasoning effort.
	CycleThinking,
	/// Request an enablement change from the extension configuration owner.
	ToggleExtension {
		/// Stable inspector row identity.
		id:      Str,
		/// Requested enabled state.
		enabled: bool,
	},
	/// Release the live MCP subscription owned for the extension inspector.
	CloseExtensionInspector,
	/// Toggle planning mode through the backend mode authority.
	TogglePlan,
	/// Generate a safe suggested destination for reviewed plan Markdown.
	PlanSavePathRequest {
		/// Exact approved Markdown shown by the review overlay.
		content: Str,
	},
	/// Persist approved Markdown through the environment authority, exit plan
	/// mode, and start a fresh session.
	SavePlanAndQuit {
		/// User-selected environment-relative destination.
		path:    Str,
		/// Exact approved Markdown shown by the review overlay.
		content: Str,
	},
	/// Toggle real-time voice mode through the backend session coordinator.
	ToggleLive,
	/// Apply one action to the active realtime voice session.
	LiveVoice(LiveVoiceAction),
	/// Toggle speech-to-text capture through the backend audio coordinator.
	ToggleStt,
	/// Suspend the terminal application after restoring terminal modes.
	Suspend,
	/// Re-query terminal appearance and force a complete repaint.
	ResetDisplay,
	/// Settle one correlated extension UI request.
	UiResponse {
		/// Opaque application-owned request correlation.
		correlation: Str,
		/// Canonical protocol response.
		response:    v1::UiResponse,
	},
	/// Forward one interaction from a retained extension overlay.
	UiOverlayEvent(v1::OverlayEvent),
	/// Release the inference-owned raw-stream subscription.
	CloseRawStream,
	/// Copy complete viewer text through the host clipboard authority.
	CopyToClipboard(Str),
	/// Apply schema-driven settings mutations as a preview or persistent commit.
	ApplySettings {
		/// Typed reflected field changes.
		changes: Vec<SettingChange>,
		/// Whether to persist the generation.
		commit:  bool,
	},
	/// Commit one retained copy, hook, advisor, or history selection.
	Select {
		/// Backend-owned workflow.
		purpose: SelectionPurpose,
		/// Stable selected key.
		key:     Str,
	},
}

/// One queued composer submission returned by the backend before it starts.
#[derive(Clone)]
pub struct QueuedPrompt {
	/// Exact user-authored text.
	pub text:        Str,
	/// Attachments staged with the text.
	pub attachments: Vec<Attachment>,
}

/// One user-message target offered by history rewind.
#[derive(Clone, Debug)]
pub struct RewindTargetRow {
	/// Durable event index to keep.
	pub event: u64,
	/// Full user message text.
	pub text:  Str,
}

/// Inbound mutation emitted by a backend.
#[derive(Clone)]
pub enum BackendEvent {
	/// Open the fullscreen Git workbench over one repository snapshot.
	OpenGitWorkbench(GitSnapshot),
	/// Apply one update to the open Git workbench.
	Git(GitUpdate),
	/// Replay a user message from durable history.
	/// Open the retained guided-goal interview.
	OpenGuidedGoal,
	/// Open the retained plan review surface over resolved Markdown.
	OpenPlanReview {
		/// Exact approved or proposed plan Markdown.
		content: Str,
	},
	/// Open destination selection after the backend generated a safe topic name.
	OpenPlanSavePrompt {
		/// Exact approved Markdown to persist after confirmation.
		content:        Str,
		/// Editable safe destination suggestion.
		suggested_path: Str,
	},
	/// Replace the sticky todo HUD from one canonical environment result.
	TodoHud(TodoHud),
	/// Deterministically select the full or bounded todo HUD presentation.
	TodoExpanded(bool),
	/// Present one pending durable approval ticket.
	ApprovalPending(ApprovalTicketView),
	/// Present explicit upload consent for one redacted AutoQA report.
	AutoQaConsent(autoqa::ConsentRequest),
	/// Remove a settled or withdrawn approval ticket.
	ApprovalSettled {
		/// Stable ticket idempotency key.
		ticket_id: Str,
	},
	/// Replay a user message from durable history.
	UserReplayed {
		/// Message text.
		text:  Str,
		/// Display labels for replayed attachments.
		chips: Vec<Str>,
	},
	/// Replay one assistant reasoning block from durable history as a
	/// settled dim-italic thinking entry.
	ThinkingReplayed {
		/// Reasoning text.
		text: Str,
	},
	/// Return a user prompt that was dropped before the first turn committed.
	PromptDropped {
		/// Prompt text exactly as submitted.
		text:        Str,
		/// Attachments submitted with the prompt.
		attachments: Vec<Attachment>,
	},
	/// Restore a batch of unstarted queued prompts to the composer.
	QueuedPromptsRestored(Vec<QueuedPrompt>),
	/// Begin a streamed assistant message.
	AssistantBegin {
		/// Stable message identifier.
		id:       Str,
		/// Whether the stream carries provider reasoning rather than prose.
		thinking: bool,
	},
	/// Append text to a streamed assistant message.
	AssistantDelta {
		/// Stable message identifier.
		id:   Str,
		/// Delta text.
		text: Str,
	},
	/// Atomically replace a streamed assistant message before settlement.
	AssistantReplace {
		/// Stable message identifier.
		id:   Str,
		/// Complete transformed markdown.
		text: Str,
	},
	/// Finish a streamed assistant message.
	AssistantEnd {
		/// Stable message identifier.
		id: Str,
	},
	/// Discard a streamed assistant message whose content a retry attempt is
	/// about to re-stream from the start.
	AssistantAbandoned {
		/// Stable message identifier.
		id: Str,
	},
	/// Begin a streamed tool invocation.
	ToolStarted {
		/// Stable tool-call identifier.
		id:    Str,
		/// Backend tool name.
		name:  Str,
		/// Exact argument/rendering revision.
		rev:   Str,
		/// Human-readable tool title.
		title: Str,
	},
	/// Append output to a live tool invocation.
	ToolOutput {
		/// Stable tool-call identifier.
		id:    Str,
		/// Output chunk.
		chunk: Str,
	},
	/// Open an interactive PTY overlay for a live shell invocation.
	PtyStarted {
		/// Stable tool-call identifier.
		id:      Str,
		/// Exact command displayed in the overlay chrome.
		command: Str,
	},
	/// Append raw output to the active PTY overlay.
	PtyOutput {
		/// Stable tool-call identifier.
		id:    Str,
		/// Raw PTY bytes.
		chunk: bytes::Bytes,
	},
	/// Mark the active PTY overlay terminal.
	PtyFinished {
		/// Stable tool-call identifier.
		id:        Str,
		/// Terminal lifecycle.
		status:    pty::PtyStatus,
		/// Process exit code when reported.
		exit_code: Option<i32>,
	},
	/// Replace the retained structured view of a live tool invocation.
	ToolView {
		/// Stable tool-call identifier.
		id:   Str,
		/// Provenance-typed renderer markup or literal fallback text.
		view: ToolViewContent,
	},
	/// Attach an inline image to a live tool invocation.
	///
	/// `source` is a filesystem path to persisted PNG bytes; the scene
	/// renders it inline in the committed card on graphics-capable
	/// terminals and ignores undecodable sources.
	ToolImage {
		/// Stable tool-call identifier.
		id:     Str,
		/// Path to the persisted PNG payload.
		source: Str,
	},
	/// Finish a tool invocation.
	ToolFinished {
		/// Stable tool-call identifier.
		id:       Str,
		/// Semantic durable terminal branch.
		terminal: ToolTerminal,
		/// Provenance-typed renderer markup or literal fallback text.
		view:     ToolViewContent,
	},
	/// Append an in-place compaction summary divider.
	Compacted {
		/// Full summary used when no short preview title was recorded.
		summary:       Str,
		/// Optional short preview title.
		title:         Option<Str>,
		/// Ladder method that produced the entry.
		method:        Option<Str>,
		/// Context tokens before the rewrite.
		tokens_before: u64,
		/// Estimated context tokens after the rewrite.
		tokens_after:  Option<u64>,
	},
	/// Append a semantic transcript boundary or error frame.
	TranscriptFrame(TranscriptFrame),
	/// Replace the live `AgentTree` roster projection.
	AgentRoster(Vec<AgentRow>),
	/// Apply schema-driven settings mutations as a preview or persistent commit.
	ApplySettings {
		/// Typed reflected field changes.
		changes: Vec<SettingChange>,
		/// Whether to persist the generation.
		commit:  bool,
	},
	/// Commit one retained copy, hook, advisor, or history selection.
	Select {
		/// Backend-owned workflow.
		purpose: SelectionPurpose,
		/// Stable selected key.
		key:     Str,
	},
	/// Replace the reflected settings schema and open its TUI surface.
	SettingsSchema(Vec<SettingRow>),
	/// Open a generic workflow selector over backend-projected rows.
	OpenSelection {
		/// Overlay title.
		title:   Str,
		/// Backend-owned workflow.
		purpose: SelectionPurpose,
		/// Stable selector rows.
		rows:    Vec<ListRow>,
	},
	/// Replace role-filtered slash-command completion data.
	SlashCommands(Vec<omp_tui::Command>),
	/// Request the live agent hierarchy overlay.
	OpenAgentTree,
	/// Open the retained extension inspector over one immutable catalog
	/// generation.
	OpenExtensionInspector(ExtensionSnapshot),
	/// Atomically replace the declared extension catalog while preserving the
	/// overlay.
	ExtensionSnapshotUpdated(ExtensionSnapshot),
	/// Merge one live MCP server generation into the open extension inspector.
	ExtensionMcpUpdated(McpLiveSnapshot),
	/// Drop live MCP catalogs after one discovery provider is disabled.
	ExtensionProviderDisabled(Str),
	/// Copy backend-produced text through the terminal host clipboard authority.
	CopyToClipboard(Str),
	/// Route backend-produced text through the composer's ordinary paste path.
	ComposerPaste(Str),
	/// Deliver one structured desktop notification through the terminal host.
	TerminalNotification(omp_tui::Notification),
	/// Update the terminal's OSC 9;4 taskbar progress state.
	TerminalProgress(omp_tui::Progress),
	/// Request the pause overlay.
	Pause,
	/// Request a host-level fresh session transition.
	NewSessionRequested,
	/// Request a host-level switch to an already-durable session.
	SessionResumeRequested(Str),
	/// Append an informational notice.
	Notice(Str),
	/// Append an error notice.
	Error(Str),
	/// Apply one bounded exact-key retained transcript frame.
	RetainedFrame(v1::RetainedFrameEnvelope),
	/// Replace status facts.
	Status(StatusFacts),
	/// Replace the active turn's core-timed working indicator.
	WorkingIndicator(WorkingIndicator),
	/// Set every live and settled tool card's disclosure state.
	ToolsExpanded(bool),
	/// Replace the label used for hidden reasoning blocks.
	HiddenThinkingLabel(Option<Str>),
	/// Paint one ephemeral idle recap without adding it to transcript history.
	Recap(Str),
	/// Replace the idle recap scheduling policy.
	RecapPolicy {
		/// Whether idle recaps are enabled.
		enabled:      bool,
		/// Idle delay in seconds before requesting a recap.
		idle_seconds: u32,
	},
	/// Preview a parsed theme without committing settings.
	ThemePreview(omp_tui::Theme),
	/// Replace the composer only after a host-owned external editor succeeds.
	ComposerReplaced(Str),
	/// Present or query one correlated extension UI request.
	UiRequest {
		/// Opaque application-owned request correlation.
		correlation: Str,
		/// Canonical protocol request.
		request:     v1::UiRequest,
	},
	/// Apply one typed retained extension UI effect.
	ApplyUiEffect(v1::UiEffect),
	/// Open the subscribed raw provider stream viewer.
	OpenRawStream {
		/// Complete initial bounded ring snapshot.
		frames:  Vec<RawFrame>,
		/// Initial ring/drop counters.
		summary: StreamSummary,
	},
	/// Append one subscribed raw provider stream frame.
	RawStreamFrame {
		/// Newly delivered sanitized frame.
		frame:   RawFrame,
		/// Updated ring/drop counters.
		summary: StreamSummary,
	},
	/// Atomically replace the raw provider stream snapshot after eviction.
	RawStreamSnapshot {
		/// Complete current bounded ring snapshot.
		frames:  Vec<RawFrame>,
		/// Updated ring/drop counters.
		summary: StreamSummary,
	},
	/// Close the raw provider stream viewer after subscription teardown.
	RawStreamClosed,
	/// Replace the composer's live chrome without reconstructing the chat.
	ComposerStyleChanged(ComposerStyle),
	/// Replace the editor's live spelling feature policy.
	SpellingFeaturesChanged(omp_tui::SpellingFeatures),
	/// Enable or disable paced grapheme reveal for streamed assistant text.
	SmoothStreamingChanged(bool),
	/// Update tiny-title model download activity.
	ModelDownloadProgress(ModelDownloadProgress),
	/// Start realtime voice composer takeover.
	LiveVoiceStarted,
	/// Update realtime voice phase, levels, and volatile user transcript.
	LiveVoiceUpdated {
		/// Current provider/controller phase.
		phase:        LiveVoicePhase,
		/// Microphone input RMS level.
		input_level:  f32,
		/// Speaker output RMS level.
		output_level: f32,
		/// Latest volatile user transcript.
		transcript:   Str,
	},
	/// Stop realtime voice and restore the ordinary composer.
	LiveVoiceStopped,
	/// Replace the session title.
	SessionTitle(Str),
	/// Open the model picker with these rows and current selection.
	OpenModelPicker {
		/// Available models.
		rows:    Vec<ModelRow>,
		/// Current model index.
		current: usize,
	},
	/// Silently refresh cached model rows and the current selection.
	ModelsUpdated {
		/// Available models.
		rows:    Vec<ModelRow>,
		/// Current model index.
		current: usize,
	},
	/// Replace resumable sessions.
	Sessions(Vec<SessionRow>),
	/// Replace language server facts shown while the welcome card is open.
	WelcomeLspServers(Vec<WelcomeLspServer>),
	/// Replace provider-login choices; each row's `id` is the provider key.
	LoginProviders(Vec<SessionRow>),
	/// Open one stage of provider logout selection.
	LogoutChoices {
		/// Picker title describing the current selection stage.
		title: Str,
		/// Providers or accounts; each row's `id` is returned through
		/// [`Intent::Logout`].
		rows:  Vec<SessionRow>,
	},
	/// Replace rewind choices.
	RewindTargets(Vec<RewindTargetRow>),
	/// Open or update the interactive login panel.
	LoginPanel {
		/// Provider being authenticated; identifies the panel on open.
		provider: Str,
		/// Update applied to the panel.
		event:    login_panel::LoginEvent,
	},
	/// Close the active login panel.
	LoginPanelClose,
	/// Begin a rewind replay, identifying the selected user-message boundary.
	HistoryRewind {
		/// Chronological user-message index on the current branch.
		user_index: usize,
		/// Exact selected user-authored text.
		text:       Str,
	},
	/// Finish the replay bracket opened by [`BackendEvent::HistoryRewind`].
	HistoryReplayFinished,
	/// Remove all transcript history.
	HistoryCleared,
	/// Open canonical pre-rendered history in the alternate-screen inspector.
	HistoryInspect {
		/// History rendered through the app's headless canonical formatter.
		frame: omp_tui::Frame,
	},
	/// Acknowledge the active submission.
	Ack {
		/// Whether the submission ended by interruption.
		interrupted: bool,
	},
}
/// Retained tiny-title model download activity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelDownloadProgress {
	/// Stable model or artifact label.
	pub label:      Str,
	/// Downloaded bytes.
	pub downloaded: u64,
	/// Total expected bytes when known.
	pub total:      Option<u64>,
	/// Whether the download has reached a terminal success state.
	pub complete:   bool,
}
