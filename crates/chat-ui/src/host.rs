//! Interactive chat state and its terminal host for the host-agnostic
//! immediate-mode chat scene.

use std::{
	collections::{BTreeMap, VecDeque},
	future,
	io::{self, Write},
	time::{Duration, Instant},
};

use flume::{Receiver, Sender};
use omp_core::{Str, sf};
use omp_tui::{
	AltScreenUse, Chord, CursorStyle, DebugOp, DebugQuery, Frame, HistoryReplay, Icon, InputEvent,
	Key, Keymap, Layer, Mods, Mouse, MouseReport, Notification, Pasted, Renderer, Size, Terminal,
	TerminalEvent, TerminalOptions, TtyOut, UiContext, Urgency, detect,
	paste::{self, Clipboard, ClipboardRead},
};
use smallvec::SmallVec;

use crate::{
	AgentHub, AgentHubEvent, ApprovalAction, ApprovalTicketView, BackendEvent, Chat, ChatKey,
	CommandPalette, ExtensionDialog, ExtensionInspector, ExtensionInspectorEvent,
	ExtensionModalEvent, ExtensionOverlay, GitIntent, GitWorkbench, GitWorkbenchEvent,
	HistoryInspector, HistoryInspectorEvent, ImageOverlay, ImageOverlayEvent, Intent, ListPicker,
	ListRow, ModelHub, ModelHubEvent, ModelHubIntent, ModelPicker, ModelRow, PaletteAction,
	PaletteEntry, PaletteEvent, PickerEvent, PromptEvent, PromptOverlay, ProviderPicker, PtyEvent,
	PtyOverlay, RawStreamEvent, RawStreamViewer, RewindTargetRow, SelectionPurpose, SessionRow,
	SettingChange, SettingRow, Sidebar, SubmitMode, Welcome, WelcomeEvent,
	approval::{ApprovalEvent, ApprovalOverlay},
	ask::{self, AskDialog, AskDialogEvent, AskRequest},
	autoqa::{AutoQaConsent, ConsentRequest, Decision},
	login_panel::{LoginPanel, LoginPanelEvent},
	modes::{GuidedGoalEvent, GuidedGoalInterview},
	plan_review::{PlanReviewEvent, PlanReviewOverlay, PlanReviewSection},
	selection_overlay::{SelectionEvent, SelectionOverlay},
	settings_overlay::{SettingsEvent, SettingsOverlay},
};

const RESIZE_SETTLE: Duration = Duration::from_millis(120);
const DOUBLE_ESC: Duration = Duration::from_millis(500);
const DOUBLE_CLEAR: Duration = Duration::from_millis(500);
const PASTE_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest interval before a retained host observes a background event.
const BACKEND_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Answers the chat-owned half of the terminal debug protocol.
fn answer_debug(query: DebugQuery, chat: &mut Chat) {
	if query.op != DebugOp::Slots {
		return;
	}
	let slots: Vec<_> = chat
		.slots_mut()
		.debug_mounts()
		.into_iter()
		.map(|mount| {
			serde_json::json!({
				"key": mount.key,
				"placement": mount.placement,
				"rect": {
					"x": mount.rect.x,
					"y": mount.rect.y,
					"width": mount.rect.width,
					"height": mount.rect.height,
				},
			})
		})
		.collect();
	omp_tui::respond_debug_query(query.id, serde_json::json!({ "ok": true, "slots": slots }));
}

mod paste_read {
	use tokio::sync::oneshot::Receiver;

	use super::{Clipboard, ClipboardRead, Instant, PASTE_READ_TIMEOUT, paste};

	pub(super) struct PasteRead {
		pub(super) clipboard:  Receiver<Option<Clipboard>>,
		pub(super) scope:      ClipboardRead,
		pub(super) abandon_at: Instant,
	}

	impl PasteRead {
		pub(super) fn start(scope: ClipboardRead) -> Self {
			Self {
				clipboard: paste::spawn_clipboard_read(scope),
				scope,
				abandon_at: Instant::now() + PASTE_READ_TIMEOUT,
			}
		}
	}
}
use paste_read::PasteRead;

/// Interactive chat-host lifecycle controls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostOptions {
	/// Whether to show the welcome session index before entering chat.
	pub welcome:                bool,
	/// Whether session-changing actions return to the caller for reconstruction.
	pub exit_on_session_change: bool,
	/// Notify when a non-interrupted turn settles.
	pub completion_notify:      bool,
	/// Notify and retain an attention title for backend errors.
	pub error_notify:           bool,
	/// Permit generated terminal-title escape sequences.
	pub title_enabled:          bool,
	/// How a settled terminal width change refreshes retired scrollback rows.
	pub resize_scrollback:      ResizeScrollback,
	/// Effective configured application bindings, consulted before legacy
	/// fallback keys.
	pub input_actions:          Vec<InputBinding>,
	/// Resolved dequeue chord label rendered in pending queued-row hints;
	/// `None` keeps the scene's platform default.
	pub dequeue_hint:           Option<Str>,
}

impl Default for HostOptions {
	fn default() -> Self {
		Self {
			welcome:                true,
			exit_on_session_change: true,
			completion_notify:      true,
			error_notify:           true,
			title_enabled:          true,
			resize_scrollback:      ResizeScrollback::Rebuild,
			input_actions:          Vec::new(),
			dequeue_hint:           None,
		}
	}
}
/// How a settled terminal width change refreshes transcript rows already
/// retired into native scrollback (written at the old width).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResizeScrollback {
	/// Replay the logical transcript at the new width below retained history
	/// in one buffered transaction.
	Append,
	/// Erase native scrollback and replay one current-width transcript in the
	/// same buffered transaction.
	#[default]
	Rebuild,
	/// Repaint only the mutable viewport; scrollback keeps its old width.
	Preserve,
}

const DOUBLE_LEFT_MIN: Duration = Duration::from_millis(40);
const DOUBLE_LEFT_MAX: Duration = Duration::from_millis(500);

/// Reason an interactive chat host returned to its production caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostExit {
	/// The user or backend closed the host.
	Quit,
	/// Rebuild the agent around this session.
	Resume(Str),
	/// Build a fresh agent session.
	NewSession,
	/// Terminal modes were restored so the owner can launch an external editor
	/// and reconstruct the host with its successful replacement.
	ExternalEditor,
	/// Terminal modes were restored so the process group can be suspended.
	Suspend,
}

/// Interactive host result with the final unsent composer draft.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostOutcome {
	/// Why the host returned.
	pub exit:  HostExit,
	/// Unsent composer text at the exact exit boundary.
	pub draft: Str,
}

/// Runs the example-style terminal host, handling session choices in-band.
#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
pub async fn run(
	chat: Chat,
	ctx: UiContext,
	events: Receiver<BackendEvent>,
	intents: Sender<Intent>,
) -> io::Result<()> {
	run_with_options(chat, ctx, events, intents, HostOptions {
		welcome:                true,
		exit_on_session_change: false,
		completion_notify:      true,
		error_notify:           true,
		title_enabled:          true,
		resize_scrollback:      ResizeScrollback::Rebuild,
		input_actions:          Vec::new(),
		dequeue_hint:           None,
	})
	.await
	.map(|_| ())
}

/// Runs the terminal host with explicit boot and session-handoff behavior.
#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
pub async fn run_with_options(
	chat: Chat,
	ctx: UiContext,
	events: Receiver<BackendEvent>,
	intents: Sender<Intent>,
	options: HostOptions,
) -> io::Result<HostExit> {
	run_with_draft(chat, ctx, events, intents, options, Str::default())
		.await
		.map(|outcome| outcome.exit)
}

/// Runs the terminal host with an owner-supplied draft and returns the final
/// unsent composer text without persisting it in the UI crate.
#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
pub async fn run_with_draft(
	mut chat: Chat,
	ctx: UiContext,
	events: Receiver<BackendEvent>,
	intents: Sender<Intent>,
	options: HostOptions,
	initial_draft: Str,
) -> io::Result<HostOutcome> {
	chat.set_composer_text(initial_draft.as_str());
	let caps = detect();
	let terminal_options = TerminalOptions::new(caps).cursor_style(CursorStyle::BlinkingBar);
	let mut terminal = Terminal::enter(terminal_options)?;
	let mut renderer = Renderer::new(TtyOut::new()?);
	renderer.apply_caps(&caps)?;
	let result =
		run_with_terminal(&mut terminal, &mut renderer, chat, &ctx, &events, &intents, options).await;
	let scrub = terminal.leave_alt().and_then(|()| renderer.clear_layers());
	match (result, scrub) {
		(Err(error), _) | (Ok(_), Err(error)) => Err(error),
		(Ok(exit), Ok(())) => Ok(exit),
	}
}

#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
async fn run_with_terminal(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	mut chat: Chat,
	ctx: &UiContext,
	events: &Receiver<BackendEvent>,
	intents: &Sender<Intent>,
	options: HostOptions,
) -> io::Result<HostOutcome> {
	let mut viewport = terminal.size()?;
	let mut models = Vec::new();
	let mut current_model = 0;
	if options.welcome {
		match run_welcome(
			terminal,
			renderer,
			ctx,
			&mut viewport,
			&mut chat,
			events,
			intents,
			&mut models,
			&mut current_model,
			options.exit_on_session_change,
		)
		.await?
		{
			WelcomeOutcome::Proceed => terminal.leave_alt()?,
			WelcomeOutcome::Exit(exit) => {
				return Ok(HostOutcome { exit, draft: Str::from(chat.composer_text()) });
			},
		}
	}
	run_chat(
		terminal,
		renderer,
		ctx,
		viewport,
		chat,
		models,
		current_model,
		events,
		intents,
		options,
	)
	.await
}

enum WelcomeOutcome {
	Proceed,
	Exit(HostExit),
}

#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
async fn run_welcome(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	ctx: &UiContext,
	viewport: &mut Size,
	chat: &mut Chat,
	events: &Receiver<BackendEvent>,
	intents: &Sender<Intent>,
	models: &mut Vec<ModelRow>,
	current_model: &mut usize,
	exit_on_session_change: bool,
) -> io::Result<WelcomeOutcome> {
	let mut alt_enter = terminal.stage_alt_enter(AltScreenUse::Interactive);
	let mut welcome = Welcome::new(ctx, Vec::new());
	let started = Instant::now();
	loop {
		if let Some(size) = terminal.take_resize()? {
			*viewport = size;
		}
		renderer.repaint(
			alt_enter.take().as_deref().unwrap_or(""),
			welcome.render(*viewport, started.elapsed()).clone(),
			viewport.height,
			&[],
		)?;
		tokio::select! {
					event = terminal.next() => match event? {
						TerminalEvent::Resize => if let Some(size) = terminal.take_resize()? { *viewport = size; },
						TerminalEvent::Debug(query) => answer_debug(query, chat),
						TerminalEvent::Effect(effect) => {
							let _ = chat.slots_mut().apply_serialized(effect);
						},
						TerminalEvent::Closed => return Ok(WelcomeOutcome::Exit(HostExit::Quit)),
						TerminalEvent::Input(event) => {
							let Some(event) = user_event(terminal, renderer, event)? else { continue };
							match event {
								InputEvent::Key(key) => match welcome.handle_key(key) {
									WelcomeEvent::Consumed => {},
									WelcomeEvent::NewSession => {
										send(intents, Intent::NewSession);
										return Ok(if exit_on_session_change {
											WelcomeOutcome::Exit(HostExit::NewSession)
										} else {
											WelcomeOutcome::Proceed
										});
									},
									WelcomeEvent::Resume(id) => {
										send(intents, Intent::Resume(Some(id.clone())));
										return Ok(if exit_on_session_change {
											WelcomeOutcome::Exit(HostExit::Resume(id))
										} else {
											WelcomeOutcome::Proceed
										});
									},
									WelcomeEvent::Quit => {
										send(intents, Intent::Quit);
										return Ok(WelcomeOutcome::Exit(HostExit::Quit));
									},
								},
								InputEvent::Mouse(report) if matches!(report.kind, Mouse::Move | Mouse::Drag) => {
									welcome.point_at(report.col, report.row);
								},
								InputEvent::Mouse(_) | InputEvent::Paste(_) | InputEvent::Focus(_)
								| InputEvent::Response(_) => {},
							}
						},
					},
			backend = events.recv_async() => match backend {
						Ok(BackendEvent::Sessions(rows)) => welcome.set_sessions(rows),
						Ok(BackendEvent::WelcomeLspServers(servers)) => {
							welcome.set_lsp_servers(servers);
						},
						Ok(BackendEvent::ModelDownloadProgress(progress)) => {
							welcome.set_download_progress(progress, started.elapsed());
						},
		Ok(BackendEvent::OpenModelPicker { rows, current }
							| BackendEvent::ModelsUpdated { rows, current }) => {
							*models = rows;
							*current_model = current.min(models.len().saturating_sub(1));
						},
						Ok(event) => { let _ = chat.apply_backend_event(event); },
						Err(_) => return Ok(WelcomeOutcome::Exit(HostExit::Quit)),
					},
				}
	}
}

enum PendingModal {
	Ask(AskRequest),
	ExtensionDialog { correlation: Str, dialog: ExtensionDialog },
	ExtensionOverlay { correlation: Str, overlay: ExtensionOverlay },
	ResumeOverlay(ExtensionOverlay),
}

struct ChatHost {
	chat:                    Chat,
	session_title:           Str,
	sidebar:                 Sidebar,
	overlay:                 Option<Overlay>,
	models:                  Vec<ModelRow>,
	current_model:           usize,
	last_esc:                Option<Instant>,
	last_clear:              Option<Instant>,
	last_left:               Option<Instant>,
	left_taps:               u8,
	pending_approvals:       usize,
	active_approval:         Option<Str>,
	approval_queue:          VecDeque<ApprovalTicketView>,
	autoqa_queue:            VecDeque<ConsentRequest>,
	modal_queue:             VecDeque<PendingModal>,
	modal_guard_visible:     bool,
	pending_ui_intents:      VecDeque<Intent>,
	hidden_overlays:         BTreeMap<Str, ExtensionOverlay>,
	suppress_history_replay: bool,
	saved_git_keymap:        Option<Keymap>,
}

impl ChatHost {
	fn new(
		mut chat: Chat,
		ctx: &UiContext,
		viewport: Size,
		models: Vec<ModelRow>,
		current_model: usize,
	) -> Self {
		let status = chat.status();
		let sidebar = Sidebar::new_hidden(&status, ctx);
		chat.set_right_inset(sidebar.reserved(viewport));
		Self {
			chat,
			session_title: sf!("omp"),
			sidebar,
			overlay: None,
			models,
			current_model,
			last_esc: None,
			last_clear: None,
			last_left: None,
			left_taps: 0,
			pending_approvals: 0,
			active_approval: None,
			approval_queue: VecDeque::new(),
			autoqa_queue: VecDeque::new(),
			modal_queue: VecDeque::new(),
			modal_guard_visible: false,
			pending_ui_intents: VecDeque::new(),
			hidden_overlays: BTreeMap::new(),
			suppress_history_replay: false,
			saved_git_keymap: None,
		}
	}

	fn open_models(&mut self, ctx: &UiContext) {
		if !self.models.is_empty() {
			self.overlay = Some(Overlay::Models(ModelPicker::open(
				&self.models,
				self.current_model.min(self.models.len() - 1),
				ctx,
			)));
		}
	}

	fn cycle_model(&mut self, backward: bool, intents: &Sender<Intent>) {
		if self.models.is_empty() {
			return;
		}
		self.current_model = if backward {
			(self.current_model + self.models.len() - 1) % self.models.len()
		} else {
			(self.current_model + 1) % self.models.len()
		};
		send(intents, Intent::SwitchModel(self.models[self.current_model].key.clone()));
	}

	fn apply_chat_key(
		&mut self,
		result: ChatKey,
		now: Instant,
		intents: &Sender<Intent>,
		ctx: &UiContext,
	) -> Option<HostExit> {
		if let Some(action) = self.chat.take_live_voice_action() {
			send(intents, Intent::LiveVoice(action));
		}
		match result {
			ChatKey::Clear => {
				if self
					.last_clear
					.is_some_and(|last| now.duration_since(last) <= DOUBLE_CLEAR)
				{
					self.last_clear = None;
					send(intents, Intent::Quit);
					return Some(HostExit::Quit);
				}
				self.chat.clear_composer();
				self.last_clear = Some(now);
				self.open_next_modal(ctx);
			},
			ChatKey::Exit => {
				self.last_clear = None;
				send(intents, Intent::Quit);
				return Some(HostExit::Quit);
			},
			ChatKey::ToggleLive => {
				self.last_clear = None;
				send(intents, Intent::ToggleLive);
			},
			ChatKey::Consumed | ChatKey::Ignored => {},
		}
		self.open_next_modal(ctx);
		None
	}

	fn enqueue_ask(&mut self, request: AskRequest, ctx: &UiContext) -> bool {
		self.modal_queue.push_back(PendingModal::Ask(request));
		self.open_next_modal(ctx)
	}

	fn enqueue_ui_request(
		&mut self,
		correlation: Str,
		request: omp_proto::omp::ui::v1::UiRequest,
		ctx: &UiContext,
	) -> bool {
		use omp_proto::omp::ui::v1::{Text, UiResponse, ui_request, ui_response};
		match request.kind {
			Some(ui_request::Kind::Dialog(dialog)) => match ExtensionDialog::open(&dialog, ctx) {
				Ok(dialog) => self
					.modal_queue
					.push_back(PendingModal::ExtensionDialog { correlation, dialog }),
				Err(error) => self.pending_ui_intents.push_back(Intent::UiResponse {
					correlation,
					response: crate::extension_ui::error_response(error),
				}),
			},
			Some(ui_request::Kind::ShowOverlay(show)) => {
				let id = sf!("extension-overlay-{correlation}");
				match ExtensionOverlay::open(id, &show, ctx) {
					Ok(overlay) => self
						.modal_queue
						.push_back(PendingModal::ExtensionOverlay { correlation, overlay }),
					Err(error) => self.pending_ui_intents.push_back(Intent::UiResponse {
						correlation,
						response: crate::extension_ui::error_response(error),
					}),
				}
			},
			Some(ui_request::Kind::OverlayValues(query)) => {
				let values = self
					.overlay
					.as_ref()
					.and_then(|overlay| match overlay {
						Overlay::ExtensionOverlay { overlay }
							if overlay.id().as_str() == query.overlay_id =>
						{
							Some(overlay.values())
						},
						_ => None,
					})
					.or_else(|| {
						self
							.hidden_overlays
							.get(query.overlay_id.as_str())
							.map(ExtensionOverlay::values)
					});
				let response = values.map_or_else(
					|| {
						crate::extension_ui::error_response(crate::extension_ui::ui_error(
							"overlay_not_found",
							"overlay is not active",
						))
					},
					crate::extension_ui::values_response,
				);
				self
					.pending_ui_intents
					.push_back(Intent::UiResponse { correlation, response });
			},
			Some(ui_request::Kind::CloseOverlay(close)) => {
				let active = matches!(
					self.overlay.as_ref(),
					Some(Overlay::ExtensionOverlay { overlay })
						if overlay.id().as_str() == close.overlay_id.as_str()
				);
				if active {
					self.overlay = None;
				}
				let hidden = self
					.hidden_overlays
					.remove(close.overlay_id.as_str())
					.is_some();
				if active || hidden {
					self.pending_ui_intents.push_back(Intent::UiOverlayEvent(
						omp_proto::omp::ui::v1::OverlayEvent {
							overlay_id: close.overlay_id.clone(),
							kind:       "close".to_owned(),
							value:      None,
						},
					));
				}
				let response = crate::extension_ui::values_response(Vec::new());
				self
					.pending_ui_intents
					.push_back(Intent::UiResponse { correlation, response });
			},
			Some(ui_request::Kind::ComposerText(_)) => {
				self.pending_ui_intents.push_back(Intent::UiResponse {
					correlation,
					response: UiResponse {
						kind:  Some(ui_response::Kind::Text(Text { value: self.chat.composer_text() })),
						props: None,
					},
				});
			},
			Some(ui_request::Kind::Presentation(_)) | Some(ui_request::Kind::IconList(_)) | None => {
				self.pending_ui_intents.push_back(Intent::UiResponse {
					correlation,
					response: crate::extension_ui::error_response(crate::extension_ui::ui_error(
						"unsupported_request",
						"request belongs to the application presentation authority",
					)),
				});
			},
		}
		self.open_next_modal(ctx)
	}

	fn apply_extension_effect(
		&mut self,
		effect: &omp_proto::omp::ui::v1::UiEffect,
		ctx: &UiContext,
	) -> bool {
		use omp_proto::omp::ui::v1::ui_effect;
		match effect.kind.as_ref() {
			Some(ui_effect::Kind::PatchNode(patch)) => {
				let visibility = patch
					.props
					.get("visible")
					.and_then(|value| value.value.as_ref())
					.and_then(|value| match value {
						omp_proto::omp::ui::v1::prop_value::Value::BoolValue(visible) => Some(*visible),
						_ => None,
					});
				if visibility == Some(false)
					&& matches!(
						self.overlay.as_ref(),
						Some(Overlay::ExtensionOverlay { overlay })
							if overlay.id().as_str() == patch.key.as_str()
					) && let Some(Overlay::ExtensionOverlay { mut overlay }) = self.overlay.take()
				{
					overlay.blur();
					self.hidden_overlays.insert(overlay.id().clone(), overlay);
					return true;
				}
				if visibility == Some(true)
					&& let Some(mut overlay) = self.hidden_overlays.remove(patch.key.as_str())
				{
					overlay.focus();
					if self.overlay.is_none() {
						self.overlay = Some(Overlay::ExtensionOverlay { overlay });
					} else {
						self
							.modal_queue
							.push_front(PendingModal::ResumeOverlay(overlay));
					}
					return true;
				}
				if let Some(Overlay::ExtensionOverlay { overlay }) = self.overlay.as_mut()
					&& patch.key.as_str() == overlay.id().as_str()
				{
					if let Some(text) = patch.text.as_ref() {
						overlay.patch_text(patch.node_id.as_str(), &text.source);
					}
					overlay.patch_props(patch.node_id.as_str(), &patch.props);
					return true;
				}
				if let Some(overlay) = self.hidden_overlays.get_mut(patch.key.as_str()) {
					if let Some(text) = patch.text.as_ref() {
						overlay.patch_text(patch.node_id.as_str(), &text.source);
					}
					overlay.patch_props(patch.node_id.as_str(), &patch.props);
					return true;
				}
				false
			},
			Some(ui_effect::Kind::FocusSlot(focus)) if focus.key.is_empty() => {
				let active = matches!(self.overlay, Some(Overlay::ExtensionOverlay { .. }));
				if active && let Some(Overlay::ExtensionOverlay { mut overlay }) = self.overlay.take() {
					overlay.blur();
					self.hidden_overlays.insert(overlay.id().clone(), overlay);
				}
				active
			},
			Some(ui_effect::Kind::FocusSlot(focus)) => {
				if let Some(Overlay::ExtensionOverlay { overlay }) = self.overlay.as_mut()
					&& focus.key.as_str() == overlay.id().as_str()
				{
					overlay.focus();
					return true;
				}
				let Some(mut overlay) = self.hidden_overlays.remove(focus.key.as_str()) else {
					return false;
				};
				overlay.focus();
				if self.overlay.is_none() {
					self.overlay = Some(Overlay::ExtensionOverlay { overlay });
				} else {
					self
						.modal_queue
						.push_front(PendingModal::ResumeOverlay(overlay));
				}
				true
			},
			Some(ui_effect::Kind::UnmountSlot(unmount)) => {
				let active = matches!(
					self.overlay.as_ref(),
					Some(Overlay::ExtensionOverlay { overlay })
						if overlay.id().as_str() == unmount.key.as_str()
				);
				if active {
					self.overlay = None;
				}
				active || self.hidden_overlays.remove(unmount.key.as_str()).is_some()
			},
			Some(ui_effect::Kind::MountSlot(mount))
				if mount
					.options
					.as_ref()
					.is_some_and(|options| !options.visible) =>
			{
				let active = matches!(
					self.overlay.as_ref(),
					Some(Overlay::ExtensionOverlay { overlay })
						if overlay.id().as_str() == mount.key.as_str()
				);
				if active && let Some(Overlay::ExtensionOverlay { mut overlay }) = self.overlay.take() {
					overlay.blur();
					self.hidden_overlays.insert(overlay.id().clone(), overlay);
				}
				active
			},
			Some(ui_effect::Kind::MountSlot(mount)) => {
				let known = matches!(
					self.overlay.as_ref(),
					Some(Overlay::ExtensionOverlay { overlay })
						if overlay.id().as_str() == mount.key.as_str()
				) || self.hidden_overlays.contains_key(mount.key.as_str());
				if !known {
					return false;
				}
				let show = omp_proto::omp::ui::v1::ShowOverlay {
					kind:    "custom".to_owned(),
					content: mount.content.clone(),
					options: None,
					props:   None,
				};
				let Ok(overlay) = ExtensionOverlay::open(Str::new(mount.key.as_str()), &show, ctx)
				else {
					return true;
				};
				if matches!(
					self.overlay.as_ref(),
					Some(Overlay::ExtensionOverlay { overlay: active })
						if active.id().as_str() == mount.key.as_str()
				) {
					self.overlay = Some(Overlay::ExtensionOverlay { overlay });
				} else {
					self.hidden_overlays.insert(overlay.id().clone(), overlay);
				}
				true
			},
			_ => false,
		}
	}

	fn open_next_modal(&mut self, ctx: &UiContext) -> bool {
		while self.modal_queue.front().is_some_and(
			|pending| matches!(pending, PendingModal::Ask(request) if request.is_cancelled()),
		) {
			self.modal_queue.pop_front();
		}
		if self.overlay.is_some()
			|| self.active_approval.is_some()
			|| !self.autoqa_queue.is_empty()
			|| self.modal_queue.is_empty()
		{
			return false;
		}
		if !self.chat.composer_empty() {
			if !self.modal_guard_visible {
				self
					.chat
					.push_notice("Finish or clear the current prompt to answer");
				self.modal_guard_visible = true;
			}
			return false;
		}
		match self
			.modal_queue
			.pop_front()
			.expect("queue checked non-empty")
		{
			PendingModal::Ask(request) => {
				let dialog = AskDialog::open(request.question.clone(), ctx);
				self.overlay = Some(Overlay::Ask { dialog, request });
			},
			PendingModal::ExtensionDialog { correlation, dialog } => {
				self.overlay = Some(Overlay::ExtensionDialog { correlation, dialog });
			},
			PendingModal::ExtensionOverlay { correlation, overlay } => {
				use omp_proto::omp::ui::v1::{OverlayOpened, UiResponse, ui_response};
				let overlay_id = overlay.id().to_string();
				self.overlay = Some(Overlay::ExtensionOverlay { overlay });
				self.pending_ui_intents.push_back(Intent::UiResponse {
					correlation,
					response: UiResponse {
						kind:  Some(ui_response::Kind::OverlayOpened(OverlayOpened { overlay_id })),
						props: None,
					},
				});
			},
			PendingModal::ResumeOverlay(overlay) => {
				self.overlay = Some(Overlay::ExtensionOverlay { overlay });
			},
		}
		self.modal_guard_visible = false;
		true
	}

	fn fail_pending_modals(&mut self) {
		let active = self.overlay.take();
		self.overlay = match active {
			Some(Overlay::Ask { request, .. }) => {
				request.fail("interactive UI disconnected");
				None
			},
			Some(Overlay::ExtensionDialog { correlation, .. }) => {
				self.pending_ui_intents.push_back(Intent::UiResponse {
					correlation,
					response: crate::extension_ui::error_response(crate::extension_ui::ui_error(
						"ui_disconnected",
						"interactive UI disconnected",
					)),
				});
				None
			},
			Some(Overlay::ExtensionOverlay { overlay }) => {
				self.pending_ui_intents.push_back(Intent::UiOverlayEvent(
					omp_proto::omp::ui::v1::OverlayEvent {
						overlay_id: overlay.id().to_string(),
						kind:       "close".to_owned(),
						value:      None,
					},
				));
				None
			},
			Some(Overlay::RawStream(_)) => {
				self.pending_ui_intents.push_back(Intent::CloseRawStream);
				None
			},
			overlay => overlay,
		};
		for pending in self.modal_queue.drain(..) {
			match pending {
				PendingModal::Ask(request) => request.fail("interactive UI disconnected"),
				PendingModal::ExtensionDialog { correlation, .. }
				| PendingModal::ExtensionOverlay { correlation, .. } => {
					self.pending_ui_intents.push_back(Intent::UiResponse {
						correlation,
						response: crate::extension_ui::error_response(crate::extension_ui::ui_error(
							"ui_disconnected",
							"interactive UI disconnected",
						)),
					});
				},
				PendingModal::ResumeOverlay(_) => {},
			}
		}
		for (_, overlay) in std::mem::take(&mut self.hidden_overlays) {
			self.pending_ui_intents.push_back(Intent::UiOverlayEvent(
				omp_proto::omp::ui::v1::OverlayEvent {
					overlay_id: overlay.id().to_string(),
					kind:       "close".to_owned(),
					value:      None,
				},
			));
		}
	}

	fn left_double_tap(&mut self) -> bool {
		let now = Instant::now();
		let Some(last) = self.last_left.replace(now) else {
			self.left_taps = 1;
			return false;
		};
		let gap = now.duration_since(last);
		if gap >= DOUBLE_LEFT_MAX {
			self.left_taps = 1;
			return false;
		}
		self.left_taps = self.left_taps.saturating_add(1);
		if self.left_taps == 2 && gap >= DOUBLE_LEFT_MIN {
			self.last_left = None;
			self.left_taps = 0;
			return true;
		}
		false
	}
}

/// Host-neutral retained chat state for native application hosts.
///
/// It owns the same overlays, input routing, backend protocol, and draft
/// boundary as the terminal host while exposing retained frames directly.
pub struct RetainedChat {
	host:                   ChatHost,
	ctx:                    UiContext,
	events:                 Receiver<BackendEvent>,
	intents:                Sender<Intent>,
	exit_on_session_change: bool,
	ask_binding:            ask::AskBinding,
	viewport:               Size,
	pending_exit:           Option<HostExit>,
	pending_clipboard:      Option<Str>,
}

/// One retained chat paint for a non-terminal host.
pub struct RetainedChatFrame<'a> {
	/// Exactly viewport-sized live presentation grid.
	pub frame:       &'a Frame,
	/// Live viewport dimensions in cells.
	pub viewport:    Size,
	/// Viewport rows reserved for the composer.
	pub editor_rows: u16,
	/// Viewport-anchored overlays in paint order.
	pub layers:      SmallVec<Layer<'a>, 4>,
}

/// One resolved configured chord and its semantic chat action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputBinding {
	/// Key emitted by the terminal's canonical default chord resolver.
	pub key:    Key,
	/// Semantic application action.
	pub action: InputAction,
}

impl InputBinding {
	/// Resolves one canonical chord spelling using the TUI's chord parser and
	/// default key normalization.
	pub fn parse(chord: &str, action: InputAction) -> Option<Self> {
		let chord = Chord::parse(chord).ok()?;
		let key = Keymap::default().resolve(chord)?;
		Some(Self { key, action })
	}
}

/// Semantic application input accepted by the retained chat host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputAction {
	/// Abort active work.
	Interrupt,
	/// Clear the current draft, or exit on a rapid second activation.
	Clear,
	/// Exit immediately through orderly host teardown.
	Exit,
	/// Cycle reasoning effort.
	CycleThinking,
	/// Toggle thinking-block visibility.
	ToggleThinking,
	/// Cycle the model roster forward.
	CycleModelForward,
	/// Cycle the model roster backward.
	CycleModelBackward,
	/// Open model selection.
	SelectModel,
	/// Open the fullscreen models hub.
	OpenModelHub,
	/// Toggle the latest tool tree.
	ToggleToolTree,
	/// Temporarily hand the expanded composer draft to an external editor.
	ExternalEditor,
	/// Stage the composer as a follow-up.
	FollowUp,
	/// Retry the latest durable user turn.
	Retry,
	/// Restore queued prompts.
	Dequeue,
	/// Toggle plan mode.
	TogglePlan,
	/// Open the prompt-history search selector.
	HistorySearch,
	/// Toggle speech-to-text capture.
	ToggleVoice,
	/// Toggle realtime voice.
	ToggleLiveVoice,
	/// Open Agent Hub.
	AgentHub,
	/// Dispatch one app-owned extension shortcut after local chord matching.
	ExtensionShortcut(Str),
}

impl InputAction {
	fn host_key(self) -> Key {
		match self {
			Self::Interrupt => Key::JumpPrevious,
			Self::Clear => Key::Ctrl('c'),
			Self::Exit => Key::Ctrl('d'),
			Self::CycleThinking => Key::BackTab,
			Self::ToggleThinking => Key::Ctrl('t'),
			Self::CycleModelForward => Key::Ctrl('p'),
			Self::CycleModelBackward => Key::CyclePrevious,
			Self::SelectModel => Key::Alt('p'),
			Self::OpenModelHub => Key::Alt('m'),
			Self::ToggleToolTree => Key::Ctrl('o'),
			Self::ExternalEditor => Key::JumpNext,
			Self::FollowUp => Key::FollowUp,
			Self::Retry => Key::Alt('r'),
			Self::Dequeue => Key::RestoreQueue,
			Self::TogglePlan => Key::PlanToggle,
			Self::HistorySearch => Key::Ctrl('r'),
			Self::ToggleVoice => Key::CtrlAlt('s'),
			Self::ToggleLiveVoice => Key::CtrlAlt('l'),
			Self::AgentHub => Key::Alt('a'),
			Self::ExtensionShortcut(_) => Key::Function(0),
		}
	}

	/// Projects one canonical application action id into chat-owned behavior.
	pub fn from_action_id(id: &str) -> Option<Self> {
		Some(match id {
			"app.interrupt" => Self::Interrupt,
			"app.clear" => Self::Clear,
			"app.exit" => Self::Exit,
			"app.thinking.cycle" => Self::CycleThinking,
			"app.thinking.toggle" => Self::ToggleThinking,
			"app.model.cycle_forward" => Self::CycleModelForward,
			"app.model.cycle_backward" => Self::CycleModelBackward,
			"app.model.select" => Self::SelectModel,
			"app.model.hub" => Self::OpenModelHub,
			"app.tools.toggle_tree" => Self::ToggleToolTree,
			"app.editor.external" => Self::ExternalEditor,
			"app.message.follow_up" => Self::FollowUp,
			"app.retry" => Self::Retry,
			"app.message.dequeue" => Self::Dequeue,
			"app.plan.toggle" => Self::TogglePlan,
			"app.history.search" => Self::HistorySearch,
			"app.voice.toggle" => Self::ToggleVoice,
			"app.voice.live_toggle" => Self::ToggleLiveVoice,
			"app.agent_hub" => Self::AgentHub,
			_ => return None,
		})
	}
}

/// A host operation requested by retained chat state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetainedChatEffect {
	/// No host action is needed.
	Ignored,
	/// State changed and the host should repaint.
	Consumed,
	/// Close the active chat surface with this lifecycle result.
	Quit(HostExit),
	/// Read a matching system clipboard representation.
	Clipboard(ClipboardRead),
	/// Copy text through the host's clipboard authority.
	SetClipboard(Str),
	/// Suspend terminal presentation, edit this exact draft externally, then
	/// return a successful replacement as [`BackendEvent::ComposerReplaced`].
	ExternalEditor(Str),
}

impl Drop for ChatHost {
	fn drop(&mut self) {
		self.fail_pending_modals();
	}
}

impl Drop for RetainedChat {
	fn drop(&mut self) {
		self.host.fail_pending_modals();
		drain_ui_intents(&mut self.host, &self.intents);
	}
}

impl RetainedChat {
	/// Creates an active chat surface after session selection.
	pub fn new(
		mut chat: Chat,
		ctx: UiContext,
		events: Receiver<BackendEvent>,
		intents: Sender<Intent>,
		options: HostOptions,
		initial_draft: Str,
	) -> Self {
		chat.set_composer_text(initial_draft.as_str());
		let viewport = Size::new(0, 0);
		Self {
			host: ChatHost::new(chat, &ctx, viewport, Vec::new(), 0),
			ctx,
			events,
			intents,
			exit_on_session_change: options.exit_on_session_change,
			ask_binding: ask::bind(),
			viewport,
			pending_exit: None,
			pending_clipboard: None,
		}
	}

	/// Replaces the composer after a host-owned editor succeeds.
	pub fn replace_composer(&mut self, text: Str) {
		self.host.chat.set_composer_text(text.as_str());
		open_next_queued_overlay(&mut self.host, &self.ctx);
	}

	/// Updates the fixed cell viewport.
	pub fn resize(&mut self, viewport: Size, _settled: bool) {
		self.viewport = viewport;
		self
			.host
			.chat
			.set_right_inset(self.host.sidebar.reserved(viewport));
		send_pty_resize(&mut self.host, viewport, &self.intents);
	}

	/// Pumps backend and dialog events, returning any required host operation.
	pub fn poll(&mut self) -> RetainedChatEffect {
		let changed = self.drain();
		if let Some(exit) = self.pending_exit.take() {
			return RetainedChatEffect::Quit(exit);
		}
		if let Some(text) = self.pending_clipboard.take() {
			return RetainedChatEffect::SetClipboard(text);
		}
		if changed {
			RetainedChatEffect::Consumed
		} else {
			RetainedChatEffect::Ignored
		}
	}

	/// Renders the active chat and its viewport overlays.
	pub fn render(&mut self) -> RetainedChatFrame<'_> {
		let viewport = self.viewport;
		let editor_rows = self.host.chat.composer_rows();
		let rendered = self.host.chat.render(viewport);
		let mut layers = rail_layers(&mut self.host.sidebar, viewport);
		if let Some(overlay) = self.host.overlay.as_mut() {
			layers.push(overlay.layer(viewport));
		}
		RetainedChatFrame { frame: rendered.frame, viewport, editor_rows, layers }
	}

	/// Routes one keyboard event through the active overlay or chat composer.
	pub fn key(&mut self, key: Key) -> RetainedChatEffect {
		if let Some(overlay) = self.host.overlay.as_mut() {
			let key = if key == Key::Ctrl('c') && !matches!(overlay, Overlay::RawStream(_)) {
				Key::Esc
			} else {
				key
			};
			let event = overlay.handle_key(key);
			return self.apply_overlay(event);
		}
		if key == Key::Ctrl('b') {
			self.host.sidebar.toggle();
			self
				.host
				.chat
				.set_right_inset(self.host.sidebar.reserved(self.viewport));
			return RetainedChatEffect::Consumed;
		}
		if key == Key::RestoreQueue {
			send(&self.intents, Intent::Dequeue);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::CyclePrevious {
			self.host.cycle_model(true, &self.intents);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Ctrl('p') {
			self.host.cycle_model(false, &self.intents);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::BackTab {
			send(&self.intents, Intent::CycleThinking);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Alt('r') {
			send(&self.intents, Intent::Retry);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::PlanToggle {
			send(&self.intents, Intent::TogglePlan);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::CtrlAlt('l') {
			send(&self.intents, Intent::ToggleLive);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::CtrlAlt('s') {
			send(&self.intents, Intent::ToggleStt);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Alt('h') {
			send(&self.intents, Intent::InspectHistory);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Ctrl('r') {
			send(&self.intents, Intent::SearchHistory);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Ctrl('k') {
			self.host.overlay =
				Some(Overlay::Palette(CommandPalette::open(palette_entries(), &self.ctx)));
			return RetainedChatEffect::Consumed;
		}
		if self.host.sidebar.focused() {
			if key == Key::Ctrl('c') {
				send(&self.intents, Intent::Quit);
				return RetainedChatEffect::Quit(HostExit::Quit);
			}
			self.host.sidebar.handle_key(key);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Alt('a') || key == Key::Ctrl('s') {
			open_agents(&mut self.host, &self.ctx);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Alt('m') || key == Key::Alt('p') {
			self.host.open_models(&self.ctx);
			return RetainedChatEffect::Consumed;
		}
		if let Some(scope) = ClipboardRead::for_key(key) {
			return RetainedChatEffect::Clipboard(scope);
		}
		if key == Key::Esc && !self.host.chat.live_voice_active() && self.host.chat.is_working() {
			self.host.last_esc = None;
			send(&self.intents, Intent::Abort);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Esc && !self.host.chat.live_voice_active() && self.host.chat.composer_empty() {
			let now = Instant::now();
			if self
				.host
				.last_esc
				.is_some_and(|last| now.duration_since(last) <= DOUBLE_ESC)
			{
				self.host.last_esc = None;
				send(&self.intents, Intent::RewindRequest);
			} else {
				self.host.last_esc = Some(now);
			}
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Left
			&& self.host.chat.composer_empty()
			&& !self.host.chat.agent_roster().is_empty()
			&& self.host.left_double_tap()
		{
			open_agents_armed(&mut self.host, &self.ctx);
			return RetainedChatEffect::Consumed;
		}
		self.host.last_esc = None;
		let result = self.host.chat.handle_key(key);
		let copied = self.host.chat.take_copied();
		while let Some((text, attachments, mode)) = self.host.chat.take_submission() {
			if text.trim() == "/images" {
				self.host.overlay = Some(Overlay::Images(ImageOverlay::open(
					&self.host.chat.composer_attachments(),
					&self.ctx,
				)));
				continue;
			}
			send(&self.intents, Intent::Submit { text, attachments, mode });
		}
		if let Some(exit) = self
			.host
			.apply_chat_key(result, Instant::now(), &self.intents, &self.ctx)
		{
			return RetainedChatEffect::Quit(exit);
		}
		if let Some(text) = copied {
			return RetainedChatEffect::SetClipboard(text);
		}
		match result {
			ChatKey::Ignored => RetainedChatEffect::Ignored,
			ChatKey::Consumed | ChatKey::Clear | ChatKey::Exit | ChatKey::ToggleLive => {
				RetainedChatEffect::Consumed
			},
		}
	}

	/// Applies one configured semantic application action.
	pub fn action(&mut self, action: InputAction) -> RetainedChatEffect {
		match action {
			InputAction::Interrupt => send(&self.intents, Intent::Abort),
			InputAction::Clear => {
				if let Some(exit) =
					self
						.host
						.apply_chat_key(ChatKey::Clear, Instant::now(), &self.intents, &self.ctx)
				{
					return RetainedChatEffect::Quit(exit);
				}
			},
			InputAction::Exit => {
				if let Some(exit) =
					self
						.host
						.apply_chat_key(ChatKey::Exit, Instant::now(), &self.intents, &self.ctx)
				{
					return RetainedChatEffect::Quit(exit);
				}
			},
			InputAction::CycleThinking => send(&self.intents, Intent::CycleThinking),
			InputAction::ToggleThinking => {
				let _ = self.host.chat.handle_key(Key::Ctrl('t'));
			},
			InputAction::CycleModelForward => self.host.cycle_model(false, &self.intents),
			InputAction::CycleModelBackward => self.host.cycle_model(true, &self.intents),
			InputAction::SelectModel => self.host.open_models(&self.ctx),
			InputAction::OpenModelHub => send(&self.intents, Intent::OpenModelHub),
			InputAction::ToggleToolTree => {
				let _ = self.host.chat.handle_key(Key::Ctrl('o'));
			},
			InputAction::ExternalEditor => {
				return RetainedChatEffect::ExternalEditor(Str::new(self.host.chat.composer_text()));
			},
			InputAction::FollowUp => return self.key(Key::FollowUp),
			InputAction::Retry => send(&self.intents, Intent::Retry),
			InputAction::Dequeue => send(&self.intents, Intent::Dequeue),
			InputAction::TogglePlan => send(&self.intents, Intent::TogglePlan),
			InputAction::HistorySearch => send(&self.intents, Intent::SearchHistory),
			InputAction::ToggleVoice => send(&self.intents, Intent::ToggleStt),
			InputAction::ToggleLiveVoice => send(&self.intents, Intent::ToggleLive),
			InputAction::AgentHub => open_agents(&mut self.host, &self.ctx),
			InputAction::ExtensionShortcut(chord) => {
				send(&self.intents, Intent::ExtensionShortcut(chord))
			},
		}
		RetainedChatEffect::Consumed
	}

	/// Routes one pointer event through the active overlay or chat surface.
	pub fn mouse(&mut self, report: MouseReport) -> RetainedChatEffect {
		if let Some(overlay) = self.host.overlay.as_mut() {
			let event = overlay.handle_mouse(report.col, report.row, report.kind, self.viewport);
			return self.apply_overlay(event);
		}
		if !self
			.host
			.sidebar
			.handle_mouse(report.col, report.row, report.kind, self.viewport)
		{
			self.host.chat.handle_mouse(&report);
		}
		RetainedChatEffect::Consumed
	}

	/// Routes clipboard text into the active overlay or chat composer.
	pub fn paste(&mut self, text: &str, raw: bool) -> RetainedChatEffect {
		if let Some(overlay) = self.host.overlay.as_mut() {
			let event = overlay.handle_paste(text);
			return self.apply_overlay(event);
		}
		if !self.host.sidebar.focused() {
			if raw {
				self.host.chat.handle_paste_raw(text);
			} else {
				self.host.chat.handle_paste(text);
			}
		}
		RetainedChatEffect::Consumed
	}

	/// Returns the lifecycle outcome with the current unsent composer draft.
	pub fn outcome(&self, exit: HostExit) -> HostOutcome {
		host_outcome(&self.host, exit)
	}

	/// Returns the next paint deadline while preserving background event
	/// latency.
	pub fn tick(&self) -> Duration {
		self
			.host
			.chat
			.next_wake()
			.map_or(BACKEND_POLL_INTERVAL, |wake| wake.min(BACKEND_POLL_INTERVAL))
	}

	fn apply_overlay(&mut self, event: OverlayEvent) -> RetainedChatEffect {
		match apply_overlay_event(
			&mut self.host,
			event,
			&self.ctx,
			self.viewport,
			&self.intents,
			self.exit_on_session_change,
		) {
			Some(exit) => RetainedChatEffect::Quit(exit),
			None => RetainedChatEffect::Consumed,
		}
	}

	fn drain(&mut self) -> bool {
		let mut changed = false;
		loop {
			match self.events.try_recv() {
				Ok(BackendEvent::NewSessionRequested) if self.exit_on_session_change => {
					self.pending_exit = Some(HostExit::NewSession);
					changed = true;
				},
				Ok(BackendEvent::SessionResumeRequested(session)) if self.exit_on_session_change => {
					self.pending_exit = Some(HostExit::Resume(session));
					changed = true;
				},
				Ok(BackendEvent::CopyToClipboard(text)) => {
					self.pending_clipboard = Some(text);
					changed = true;
				},
				Ok(event) => {
					if let Some(intent) = apply_backend(&mut self.host, event, &self.ctx) {
						send(&self.intents, Intent::Git(intent));
					}
					send_pty_resize(&mut self.host, self.viewport, &self.intents);
					changed = true;
				},
				Err(flume::TryRecvError::Empty) => break,
				Err(flume::TryRecvError::Disconnected) => {
					self.pending_exit = Some(HostExit::Quit);
					changed = true;
					break;
				},
			}
		}
		while let Some(request) = self.ask_binding.try_recv() {
			self.host.enqueue_ask(request, &self.ctx);
			changed = true;
		}
		changed |= drain_ui_intents(&mut self.host, &self.intents);
		changed
	}
}

fn rail_layers(sidebar: &mut Sidebar, viewport: Size) -> SmallVec<Layer<'_>, 4> {
	sidebar
		.layer(viewport, Instant::now().into())
		.into_iter()
		.collect()
}
enum ListPurpose {
	Resume,
	Rewind,
	Logout,
	Pause,
}

enum Overlay {
	Git(GitWorkbench),
	Models(ModelPicker),
	ModelHub(ModelHub),
	GuidedGoal(GuidedGoalInterview),
	PlanReview(PlanReviewOverlay),
	PlanSave { prompt: PromptOverlay, content: Str },
	Extensions(ExtensionInspector),
	Pty(PtyOverlay),
	Palette(CommandPalette),
	List { picker: ListPicker, rows: Vec<ListRow>, prefill: Vec<Str>, purpose: ListPurpose },
	AgentHub(AgentHub),
	Settings(SettingsOverlay),
	Selection(SelectionOverlay),
	Images(ImageOverlay),
	History(HistoryInspector),
	RawStream(RawStreamViewer),
	AgentPrompt { prompt: PromptOverlay, agent_id: Str, revive: bool },
	Providers(ProviderPicker),
	Approval(ApprovalOverlay),
	ApprovalAmend { prompt: PromptOverlay, ticket: ApprovalTicketView },
	Ask { dialog: AskDialog, request: AskRequest },
	ExtensionDialog { correlation: Str, dialog: ExtensionDialog },
	ExtensionOverlay { overlay: ExtensionOverlay },
	AutoQaConsent { dialog: AskDialog, consent: AutoQaConsent },
	Login { panel: LoginPanel },
}

enum OverlayEvent {
	Consumed,
	Git(GitIntent),
	LoginCancel,
	LoginSubmit(Str),
	GoalComplete { objective: Str, token_budget: Option<u64> },
	PlanReviewComplete(Str),
	PlanSavePathRequest(Str),
	PlanSaveSubmit { path: Str, content: Str },
	ExtensionToggle { id: Str, enabled: bool },
	PtyInput { id: Str, data: bytes::Bytes },
	PtyKill { id: Str },
	Close,
	Pick(usize),
	ModelHub(ModelHubIntent),
	LoginRequest(Str),
	Palette(PaletteAction),
	PromptCancel,
	Prompt(Str),
	OpenAgentPrompt(Str),
	OpenAgentRevivePrompt(Str),
	AgentSteerPrompt { agent_id: Str, prompt: Str },
	AgentRevivePrompt { agent_id: Str, prompt: Str },
	AgentKill(Str),
	ApprovalCancel,
	ApprovalDecide { ticket_id: Str, action: ApprovalAction },
	ApprovalAmend(Str),
	AskCancel,
	AskSubmit { selected: Vec<Str>, custom_input: Option<Str> },
	ExtensionDialog { correlation: Str, response: omp_proto::omp::ui::v1::UiResponse },
	ExtensionOverlayEvent(omp_proto::omp::ui::v1::OverlayEvent),
	ExtensionOverlayClose(Str),
	RawStreamClose,
	RawStreamCopy(Str),
	AutoQaConsent(Decision),
	SettingsPreview(Vec<SettingChange>),
	SettingsCommit(Vec<SettingChange>),
	Selection(SelectionPurpose, Str),
}

impl Overlay {
	const fn kind(&self) -> &'static str {
		match self {
			Self::Git(_) => "git",
			Self::Models(_) => "models",
			Self::ModelHub(_) => "model_hub",
			Self::GuidedGoal(_) => "guided_goal",
			Self::PlanReview(_) => "plan_review",
			Self::PlanSave { .. } => "plan_save",
			Self::Extensions(_) => "extensions",
			Self::Pty(_) => "pty",
			Self::Palette(_) => "palette",
			Self::List { .. } => "list",
			Self::AgentHub(_) => "agent_hub",
			Self::Settings(_) => "settings",
			Self::Selection(_) => "selection",
			Self::Images(_) => "images",
			Self::History(_) => "history",
			Self::RawStream(_) => "raw_stream",
			Self::AgentPrompt { .. } => "agent_prompt",
			Self::Providers(_) => "providers",
			Self::Approval(_) => "approval",
			Self::ApprovalAmend { .. } => "approval_amend",
			Self::Ask { .. } => "ask",
			Self::ExtensionDialog { .. } => "extension_dialog",
			Self::ExtensionOverlay { .. } => "extension_overlay",
			Self::AutoQaConsent { .. } => "auto_qa_consent",
			Self::Login { .. } => "login",
		}
	}

	fn handle_key(&mut self, key: Key) -> OverlayEvent {
		match self {
			Self::Git(workbench) => git_workbench_event(workbench.handle_key(key)),
			Self::Models(picker) => picker_event(picker.handle_key(key)),
			Self::GuidedGoal(interview) => guided_goal_event(interview.handle_key(key)),
			Self::PlanReview(review) => {
				let event = review.handle_key(key);
				plan_review_event(event, review.sections())
			},
			Self::PlanSave { prompt, content } => match prompt_event(prompt.handle_key(key)) {
				OverlayEvent::Prompt(path) => {
					OverlayEvent::PlanSaveSubmit { path, content: content.clone() }
				},
				OverlayEvent::PromptCancel => OverlayEvent::Close,
				event => event,
			},
			Self::Extensions(inspector) => extension_inspector_event(inspector.handle_key(key)),
			Self::Pty(pty) => match pty.handle_key(key) {
				PtyEvent::Input(data) => OverlayEvent::PtyInput { id: pty.id().clone(), data },
				PtyEvent::ForceKill => OverlayEvent::PtyKill { id: pty.id().clone() },
				PtyEvent::Close => OverlayEvent::Close,
				PtyEvent::Consumed => OverlayEvent::Consumed,
			},
			Self::Palette(palette) => palette_event(palette.handle_key(key)),
			Self::List { picker, .. } => picker_event(picker.handle_key(key)),
			Self::ModelHub(hub) => model_hub_event(hub.handle_key(key)),
			Self::AgentHub(hub) => agent_hub_event(hub.handle_key(key)),
			Self::Settings(settings) => settings_event(settings.handle_key(key)),
			Self::Selection(selection) => selection_event(selection.handle_key(key)),
			Self::Images(images) => image_overlay_event(images.handle_key(key)),
			Self::History(inspector) => history_inspector_event(inspector.handle_key(key)),
			Self::RawStream(viewer) => raw_stream_event(viewer.handle_key(key)),
			Self::AgentPrompt { prompt, agent_id, revive } => {
				match prompt_event(prompt.handle_key(key)) {
					OverlayEvent::Prompt(value) if *revive => {
						OverlayEvent::AgentRevivePrompt { agent_id: agent_id.clone(), prompt: value }
					},
					OverlayEvent::Prompt(value) => {
						OverlayEvent::AgentSteerPrompt { agent_id: agent_id.clone(), prompt: value }
					},
					OverlayEvent::PromptCancel => OverlayEvent::Close,
					event => event,
				}
			},
			Self::Providers(picker) => picker_event(picker.handle_key(key)),
			Self::Approval(approval) => {
				approval_event(approval.ticket_id().clone(), approval.handle_key(key))
			},
			Self::ApprovalAmend { prompt, .. } => match prompt_event(prompt.handle_key(key)) {
				OverlayEvent::Prompt(value) => OverlayEvent::ApprovalAmend(value),
				OverlayEvent::PromptCancel => OverlayEvent::ApprovalCancel,
				event => event,
			},
			Self::Ask { dialog, .. } => ask_event(dialog.handle_key(key)),
			Self::ExtensionDialog { correlation, dialog } => {
				extension_modal_event(Some(correlation.clone()), dialog.handle_key(key))
			},
			Self::ExtensionOverlay { overlay } => extension_modal_event(None, overlay.handle_key(key)),
			Self::AutoQaConsent { dialog, .. } => autoqa_event(dialog.handle_key(key)),
			Self::Login { panel } => login_event(panel.handle_key(key)),
		}
	}

	fn handle_paste(&mut self, text: &str) -> OverlayEvent {
		match self {
			Self::Git(workbench) => git_workbench_event(workbench.handle_paste(text)),
			Self::Models(picker) => picker_event(picker.handle_paste(text)),
			Self::ModelHub(hub) => model_hub_event(hub.handle_paste(text)),
			Self::GuidedGoal(interview) => guided_goal_event(interview.handle_paste(text)),
			Self::PlanReview(review) => {
				let event = review.handle_paste(text);
				plan_review_event(event, review.sections())
			},
			Self::PlanSave { prompt, content } => match prompt_event(prompt.handle_paste(text)) {
				OverlayEvent::Prompt(path) => {
					OverlayEvent::PlanSaveSubmit { path, content: content.clone() }
				},
				OverlayEvent::PromptCancel => OverlayEvent::Close,
				event => event,
			},
			Self::Extensions(_) => OverlayEvent::Consumed,
			Self::Pty(pty) => match pty.handle_paste(text) {
				PtyEvent::Input(data) => OverlayEvent::PtyInput { id: pty.id().clone(), data },
				PtyEvent::ForceKill => OverlayEvent::PtyKill { id: pty.id().clone() },
				PtyEvent::Close => OverlayEvent::Close,
				PtyEvent::Consumed => OverlayEvent::Consumed,
			},
			Self::Palette(palette) => palette_event(palette.handle_paste(text)),
			Self::List { picker, .. } => picker_event(picker.handle_paste(text)),
			Self::Settings(settings) => settings_event(settings.handle_paste(text)),
			Self::Selection(selection) => selection_event(selection.handle_paste(text)),
			Self::AgentHub(_) | Self::Images(_) => OverlayEvent::Consumed,
			Self::History(_) | Self::RawStream(_) => OverlayEvent::Consumed,
			Self::AgentPrompt { prompt, agent_id, revive } => {
				match prompt_event(prompt.handle_paste(text)) {
					OverlayEvent::Prompt(value) if *revive => {
						OverlayEvent::AgentRevivePrompt { agent_id: agent_id.clone(), prompt: value }
					},
					OverlayEvent::Prompt(value) => {
						OverlayEvent::AgentSteerPrompt { agent_id: agent_id.clone(), prompt: value }
					},
					OverlayEvent::PromptCancel => OverlayEvent::Close,
					event => event,
				}
			},
			Self::Providers(picker) => picker_event(picker.handle_paste(text)),
			Self::Approval(approval) => {
				approval_event(approval.ticket_id().clone(), approval.handle_paste(text))
			},
			Self::ApprovalAmend { prompt, .. } => match prompt_event(prompt.handle_paste(text)) {
				OverlayEvent::Prompt(value) => OverlayEvent::ApprovalAmend(value),
				OverlayEvent::PromptCancel => OverlayEvent::ApprovalCancel,
				event => event,
			},
			Self::Ask { dialog, .. } => ask_event(dialog.handle_paste(text)),
			Self::ExtensionDialog { correlation, dialog } => {
				extension_modal_event(Some(correlation.clone()), dialog.handle_paste(text))
			},
			Self::ExtensionOverlay { overlay } => {
				extension_modal_event(None, overlay.handle_paste(text))
			},
			Self::AutoQaConsent { dialog, .. } => autoqa_event(dialog.handle_paste(text)),
			Self::Login { panel } => login_event(panel.handle_paste(text)),
		}
	}

	fn handle_mouse(&mut self, col: u16, row: u16, kind: Mouse, viewport: Size) -> OverlayEvent {
		match self {
			Self::Git(workbench) => {
				git_workbench_event(workbench.handle_mouse(col, row, kind, viewport))
			},
			Self::Models(picker) => picker_event(picker.handle_mouse(col, row, kind, viewport)),
			Self::ModelHub(hub) => model_hub_event(hub.handle_mouse(col, row, kind, viewport)),
			Self::GuidedGoal(interview) => {
				guided_goal_event(interview.handle_mouse(col, row, kind, viewport))
			},
			Self::PlanReview(review) => {
				let event = review.handle_mouse(col, row, kind, viewport);
				plan_review_event(event, review.sections())
			},
			Self::PlanSave { prompt, content } => {
				match prompt_event(prompt.handle_mouse(col, row, kind, viewport)) {
					OverlayEvent::Prompt(path) => {
						OverlayEvent::PlanSaveSubmit { path, content: content.clone() }
					},
					OverlayEvent::PromptCancel => OverlayEvent::Close,
					event => event,
				}
			},
			Self::Extensions(inspector) => {
				extension_inspector_event(inspector.handle_mouse(col, row, kind, viewport))
			},
			Self::Pty(_) => OverlayEvent::Consumed,
			Self::Palette(palette) => palette_event(palette.handle_mouse(col, row, kind, viewport)),
			Self::List { picker, .. } => picker_event(picker.handle_mouse(col, row, kind, viewport)),
			Self::AgentHub(hub) => agent_hub_event(hub.handle_mouse(col, row, kind, viewport)),
			Self::Settings(settings) => {
				settings_event(settings.handle_mouse(col, row, kind, viewport))
			},
			Self::Selection(selection) => {
				selection_event(selection.handle_mouse(col, row, kind, viewport))
			},
			Self::Images(images) => image_overlay_event(images.handle_mouse(col, row, kind, viewport)),
			Self::History(inspector) => history_inspector_event(inspector.handle_mouse(kind)),
			Self::RawStream(viewer) => raw_stream_event(viewer.handle_mouse(kind)),
			Self::AgentPrompt { .. } => OverlayEvent::Consumed,
			Self::Providers(picker) => picker_event(picker.handle_mouse(col, row, kind, viewport)),
			Self::Approval(approval) => approval_event(
				approval.ticket_id().clone(),
				approval.handle_mouse(col, row, kind, viewport),
			),
			Self::ApprovalAmend { prompt, .. } => {
				match prompt_event(prompt.handle_mouse(col, row, kind, viewport)) {
					OverlayEvent::Prompt(value) => OverlayEvent::ApprovalAmend(value),
					OverlayEvent::PromptCancel => OverlayEvent::ApprovalCancel,
					event => event,
				}
			},
			Self::Ask { dialog, .. } => ask_event(dialog.handle_mouse(col, row, kind, viewport)),
			Self::ExtensionDialog { correlation, dialog } => extension_modal_event(
				Some(correlation.clone()),
				dialog.handle_mouse(col, row, kind, viewport),
			),
			Self::ExtensionOverlay { overlay } => {
				extension_modal_event(None, overlay.handle_mouse(col, row, kind, viewport))
			},
			Self::AutoQaConsent { dialog, .. } => {
				autoqa_event(dialog.handle_mouse(col, row, kind, viewport))
			},
			Self::Login { panel } => login_event(panel.handle_mouse(col, row, kind, viewport)),
		}
	}

	fn layer(&mut self, viewport: Size) -> Layer<'_> {
		match self {
			Self::Git(workbench) => workbench.layer(viewport),
			Self::Models(picker) => picker.layer(viewport),
			Self::ModelHub(hub) => hub.layer(viewport),
			Self::GuidedGoal(interview) => interview.layer(viewport),
			Self::PlanReview(review) => review.layer(viewport),
			Self::PlanSave { prompt, .. } => prompt.layer(viewport),
			Self::Extensions(inspector) => inspector.layer(viewport),
			Self::Pty(pty) => pty.layer(viewport),
			Self::Palette(palette) => palette.layer(viewport),
			Self::List { picker, .. } => picker.layer(viewport),
			Self::AgentHub(hub) => hub.layer(viewport),
			Self::Settings(settings) => settings.layer(viewport),
			Self::Selection(selection) => selection.layer(viewport),
			Self::Images(images) => images.layer(viewport),
			Self::History(inspector) => inspector.layer(viewport),
			Self::RawStream(viewer) => viewer.layer(viewport),
			Self::AgentPrompt { prompt, .. } => prompt.layer(viewport),
			Self::Providers(picker) => picker.layer(viewport),
			Self::Approval(approval) => approval.layer(viewport),
			Self::ApprovalAmend { prompt, .. } => prompt.layer(viewport),
			Self::Ask { dialog, .. } => dialog.layer(viewport),
			Self::ExtensionDialog { dialog, .. } => dialog.layer(viewport),
			Self::ExtensionOverlay { overlay } => overlay.layer(viewport),
			Self::AutoQaConsent { dialog, .. } => dialog.layer(viewport),
			Self::Login { panel } => panel.layer(viewport),
		}
	}
}

fn git_workbench_event(event: GitWorkbenchEvent) -> OverlayEvent {
	match event {
		GitWorkbenchEvent::Consumed => OverlayEvent::Consumed,
		GitWorkbenchEvent::Intent(intent) => OverlayEvent::Git(intent),
		GitWorkbenchEvent::Close => OverlayEvent::Close,
	}
}

const fn picker_event(event: PickerEvent) -> OverlayEvent {
	match event {
		PickerEvent::Consumed => OverlayEvent::Consumed,
		PickerEvent::Close => OverlayEvent::Close,
		PickerEvent::Pick(index) => OverlayEvent::Pick(index),
	}
}
fn model_hub_event(event: ModelHubEvent) -> OverlayEvent {
	match event {
		ModelHubEvent::Consumed => OverlayEvent::Consumed,
		ModelHubEvent::Close => OverlayEvent::Close,
		ModelHubEvent::AssignRole { role, selector, thinking, scope } => {
			OverlayEvent::ModelHub(ModelHubIntent::AssignRole { role, selector, thinking, scope })
		},
		ModelHubEvent::UnassignRole { role, scope } => {
			OverlayEvent::ModelHub(ModelHubIntent::UnassignRole { role, scope })
		},
		ModelHubEvent::SetFallbackChain { key, chain } => {
			OverlayEvent::ModelHub(ModelHubIntent::SetFallbackChain { key, chain })
		},
		ModelHubEvent::SetCycleOrder { order } => {
			OverlayEvent::ModelHub(ModelHubIntent::SetCycleOrder { order })
		},
		ModelHubEvent::Login(provider) => OverlayEvent::LoginRequest(provider),
	}
}
fn guided_goal_event(event: GuidedGoalEvent) -> OverlayEvent {
	match event {
		GuidedGoalEvent::Consumed => OverlayEvent::Consumed,
		GuidedGoalEvent::Cancel => OverlayEvent::Close,
		GuidedGoalEvent::Complete(values) => OverlayEvent::GoalComplete {
			objective:    values.objective,
			token_budget: values.token_budget,
		},
	}
}

fn plan_review_event(event: PlanReviewEvent, sections: &[PlanReviewSection]) -> OverlayEvent {
	match event {
		PlanReviewEvent::Consumed
		| PlanReviewEvent::SectionChanged(_)
		| PlanReviewEvent::AnnotationsChanged(_) => OverlayEvent::Consumed,
		PlanReviewEvent::Submit(annotations) => {
			OverlayEvent::PlanReviewComplete(annotations.prompt(sections))
		},
		PlanReviewEvent::SaveAndQuit(content) => OverlayEvent::PlanSavePathRequest(content),
		PlanReviewEvent::Cancel => OverlayEvent::Close,
	}
}
fn extension_inspector_event(event: ExtensionInspectorEvent) -> OverlayEvent {
	match event {
		ExtensionInspectorEvent::Consumed => OverlayEvent::Consumed,
		ExtensionInspectorEvent::Close => OverlayEvent::Close,
		ExtensionInspectorEvent::Toggle { id, enabled } => {
			OverlayEvent::ExtensionToggle { id, enabled }
		},
	}
}

fn agent_hub_event(event: AgentHubEvent) -> OverlayEvent {
	match event {
		AgentHubEvent::Consumed => OverlayEvent::Consumed,
		AgentHubEvent::Close => OverlayEvent::Close,
		AgentHubEvent::Steer(id) => OverlayEvent::OpenAgentPrompt(id),
		AgentHubEvent::Revive(id) => OverlayEvent::OpenAgentRevivePrompt(id),
		AgentHubEvent::Kill(id) => OverlayEvent::AgentKill(id),
	}
}
const fn image_overlay_event(event: ImageOverlayEvent) -> OverlayEvent {
	match event {
		ImageOverlayEvent::Consumed => OverlayEvent::Consumed,
		ImageOverlayEvent::Close => OverlayEvent::Close,
	}
}
fn raw_stream_event(event: RawStreamEvent) -> OverlayEvent {
	match event {
		RawStreamEvent::Consumed => OverlayEvent::Consumed,
		RawStreamEvent::Close => OverlayEvent::RawStreamClose,
		RawStreamEvent::Copy(text) => OverlayEvent::RawStreamCopy(Str::new(text)),
	}
}

const fn history_inspector_event(event: HistoryInspectorEvent) -> OverlayEvent {
	match event {
		HistoryInspectorEvent::Consumed => OverlayEvent::Consumed,
		HistoryInspectorEvent::Close => OverlayEvent::Close,
	}
}

fn settings_event(event: SettingsEvent) -> OverlayEvent {
	match event {
		SettingsEvent::Consumed => OverlayEvent::Consumed,
		SettingsEvent::Close => OverlayEvent::Close,
		SettingsEvent::Preview(changes) => OverlayEvent::SettingsPreview(changes),
		SettingsEvent::Commit(changes) => OverlayEvent::SettingsCommit(changes),
	}
}

fn selection_event(event: SelectionEvent) -> OverlayEvent {
	match event {
		SelectionEvent::Consumed => OverlayEvent::Consumed,
		SelectionEvent::Close => OverlayEvent::Close,
		SelectionEvent::Pick { purpose, key } => OverlayEvent::Selection(purpose, key),
	}
}

fn palette_event(event: PaletteEvent) -> OverlayEvent {
	match event {
		PaletteEvent::Consumed => OverlayEvent::Consumed,
		PaletteEvent::Close => OverlayEvent::Close,
		PaletteEvent::Run(action) => OverlayEvent::Palette(action),
	}
}

fn prompt_event(event: PromptEvent) -> OverlayEvent {
	match event {
		PromptEvent::Consumed => OverlayEvent::Consumed,
		PromptEvent::Cancel => OverlayEvent::PromptCancel,
		PromptEvent::Submit(value) => OverlayEvent::Prompt(value),
	}
}

fn approval_event(ticket_id: Str, event: ApprovalEvent) -> OverlayEvent {
	match event {
		ApprovalEvent::Consumed => OverlayEvent::Consumed,
		ApprovalEvent::Decide(action) => OverlayEvent::ApprovalDecide { ticket_id, action },
		ApprovalEvent::Amend => OverlayEvent::ApprovalAmend(ticket_id),
	}
}
fn extension_modal_event(correlation: Option<Str>, event: ExtensionModalEvent) -> OverlayEvent {
	match event {
		ExtensionModalEvent::Consumed => OverlayEvent::Consumed,
		ExtensionModalEvent::Dialog(response) => OverlayEvent::ExtensionDialog {
			correlation: correlation.expect("dialog events retain correlation"),
			response,
		},
		ExtensionModalEvent::Overlay(event) => OverlayEvent::ExtensionOverlayEvent(event),
		ExtensionModalEvent::CloseOverlay(id) => OverlayEvent::ExtensionOverlayClose(id),
	}
}

fn ask_event(event: AskDialogEvent) -> OverlayEvent {
	match event {
		AskDialogEvent::Consumed => OverlayEvent::Consumed,
		AskDialogEvent::Cancel => OverlayEvent::AskCancel,
		AskDialogEvent::Submit { selected, custom_input } => {
			OverlayEvent::AskSubmit { selected, custom_input }
		},
	}
}
fn autoqa_event(event: AskDialogEvent) -> OverlayEvent {
	match event {
		AskDialogEvent::Consumed => OverlayEvent::Consumed,
		AskDialogEvent::Cancel => OverlayEvent::AutoQaConsent(Decision::LocalOnly),
		AskDialogEvent::Submit { selected, .. } => {
			OverlayEvent::AutoQaConsent(if selected.iter().any(|value| value == "Upload") {
				Decision::Upload
			} else {
				Decision::LocalOnly
			})
		},
	}
}

fn login_event(event: LoginPanelEvent) -> OverlayEvent {
	match event {
		LoginPanelEvent::Consumed => OverlayEvent::Consumed,
		LoginPanelEvent::Cancel => OverlayEvent::LoginCancel,
		LoginPanelEvent::Submit(value) => OverlayEvent::LoginSubmit(value),
	}
}

#[derive(Clone, Copy)]
struct ResizeState {
	last_event: Instant,
}

impl ResizeState {
	const fn new(last_event: Instant) -> Self {
		Self { last_event }
	}

	const fn observe(&mut self, observed_at: Instant) {
		self.last_event = observed_at;
	}

	fn deadline(self) -> Instant {
		self.last_event + RESIZE_SETTLE
	}

	fn settled(self, now: Instant) -> bool {
		now >= self.deadline()
	}
}

#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
async fn run_chat(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	ctx: &UiContext,
	mut viewport: Size,
	chat: Chat,
	models: Vec<ModelRow>,
	current_model: usize,
	events: &Receiver<BackendEvent>,
	intents: &Sender<Intent>,
	options: HostOptions,
) -> io::Result<HostOutcome> {
	let mut host = ChatHost::new(chat, ctx, viewport, models, current_model);
	let ask_binding = ask::bind();
	paint_host(renderer, &mut host, viewport, Retirement::Disabled)?;

	let mut resize = None;
	let mut settled_width = viewport.width;
	let mut pending_replay: Option<ResizeScrollback> = None;
	let mut paste_read: Option<PasteRead> = None;
	let mut next_frame = chat_deadline(&host.chat);
	let mut recap_policy: Option<(bool, u32)> = None;
	let mut recap_at: Option<Instant> = None;
	let mut requested_exit = HostExit::Quit;
	let input_actions = options.input_actions.clone();
	if let Some(hint) = options.dequeue_hint.clone() {
		host.chat.set_dequeue_hint(hint);
	}
	let HostOptions {
		exit_on_session_change,
		completion_notify,
		error_notify,
		title_enabled,
		resize_scrollback,
		..
	} = options;
	loop {
		let paste_deadline = paste_read.as_ref().map(|read| read.abandon_at);
		tokio::select! {
					event = terminal.next(), if paste_read.is_none() => match event? {
						TerminalEvent::Resize => {
							observe_resize(terminal, &mut viewport, &mut resize, Instant::now())?;
							host.chat.set_right_inset(host.sidebar.reserved(viewport));
							send_pty_resize(&mut host, viewport, intents);
							next_frame = Some(Instant::now());
						},
						TerminalEvent::Debug(query) => answer_debug(query, &mut host.chat),
						TerminalEvent::Effect(effect) => {
							let _ = host.chat.slots_mut().apply_serialized(effect);
						},
						TerminalEvent::Closed => return Ok(host_outcome(&host, HostExit::Quit)),
						TerminalEvent::Input(event) => {
							let Some(event) = user_event(terminal, renderer, event)? else { continue };
							match event {
								InputEvent::Key(key) => {
									let mapped_action = (host.overlay.is_none()
										&& !host.sidebar.focused())
										.then(|| {
											input_actions
												.iter()
												.find(|binding| binding.key == key)
												.map(|binding| binding.action.clone())
										})
										.flatten();
									if let Some(InputAction::ExtensionShortcut(chord)) =
										mapped_action.as_ref()
									{
										send(intents, Intent::ExtensionShortcut(chord.clone()));
										continue;
									}
									let key = mapped_action.clone().map_or(key, InputAction::host_key);
									let action_enabled = |action| {
										mapped_action.as_ref() == Some(&action)
											|| !input_actions
												.iter()
												.any(|binding| &binding.action == &action)
									};
									if host.overlay.is_some() {
										let overlay = host.overlay.as_mut().expect("overlay present");
										let key = if key == Key::Ctrl('c')
											&& !matches!(overlay, Overlay::RawStream(_))
										{
											Key::Esc
										} else {
											key
										};
										let event = overlay.handle_key(key);
										if let Some(exit) = apply_overlay_event(
											&mut host,
											event,
											ctx,
											viewport,
											intents,
											exit_on_session_change,
										) {
											requested_exit = exit;
											break;
										}
										if host.overlay.is_none() {
											close_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
										}
									} else if key == Key::JumpPrevious
										&& action_enabled(InputAction::Interrupt)
									{
										send(intents, Intent::Abort);
									} else if key == Key::JumpNext
										&& action_enabled(InputAction::ExternalEditor)
									{
										requested_exit = HostExit::ExternalEditor;
										break;
									} else if key == Key::Ctrl('b') {
										host.sidebar.toggle();
										host.chat.set_right_inset(host.sidebar.reserved(viewport));
																		} else if key == Key::Ctrl('z') {
										requested_exit = HostExit::Suspend;
										break;
									} else if key == Key::Alt('l') {
										terminal.refresh_appearance()?;
										// Rebuild directly, or append inside multiplexers where
										// ED3 would irreversibly erase pane history.
										pending_replay = Some(if terminal.inside_multiplexer() {
											ResizeScrollback::Append
										} else {
											ResizeScrollback::Rebuild
										});
										start_pending_replay(
											renderer,
											&mut host,
											&mut pending_replay,
										)?;
									} else if key == Key::RestoreQueue
										&& action_enabled(InputAction::Dequeue)
									{
										send(intents, Intent::Dequeue);
									} else if key == Key::CyclePrevious
										&& action_enabled(InputAction::CycleModelBackward)
									{
										host.cycle_model(true, intents);
									} else if key == Key::Ctrl('p')
										&& action_enabled(InputAction::CycleModelForward)
									{
										host.cycle_model(false, intents);
									} else if key == Key::BackTab
										&& action_enabled(InputAction::CycleThinking)
									{
										send(intents, Intent::CycleThinking);
									} else if key == Key::Alt('r')
										&& action_enabled(InputAction::Retry)
									{
										send(intents, Intent::Retry);
									} else if key == Key::PlanToggle
										&& action_enabled(InputAction::TogglePlan)
									{
										send(intents, Intent::TogglePlan);
									} else if key == Key::CtrlAlt('l')
										&& action_enabled(InputAction::ToggleLiveVoice)
									{
										send(intents, Intent::ToggleLive);
									} else if key == Key::CtrlAlt('s')
										&& action_enabled(InputAction::ToggleVoice)
									{
										send(intents, Intent::ToggleStt);
									} else if key == Key::Alt('h') {
										send(intents, Intent::InspectHistory);
									} else if key == Key::Ctrl('r')
										&& action_enabled(InputAction::HistorySearch)
									{
										send(intents, Intent::SearchHistory);
									} else if key == Key::Ctrl('k') {
										host.overlay = Some(Overlay::Palette(CommandPalette::open(palette_entries(), ctx)));
										open_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
									} else if host.sidebar.focused() {
										if key == Key::Ctrl('c') {
											send(intents, Intent::Quit);
											break;
										}
										host.sidebar.handle_key(key);
									} else if (key == Key::Alt('a') || key == Key::Ctrl('s'))
										&& action_enabled(InputAction::AgentHub)
									{
										open_agents(&mut host, ctx);
										open_overlay(
											terminal,
											renderer,
											&mut host,
											viewport,
											&mut resize,
										)?;
									} else if key == Key::Alt('p')
										&& action_enabled(InputAction::SelectModel)
									{
										host.open_models(ctx);
										if host.overlay.is_some() {
											open_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
										}
									} else if key == Key::Alt('m')
										&& action_enabled(InputAction::OpenModelHub)
									{
										send(intents, Intent::OpenModelHub);
									} else if let Some(scope) = ClipboardRead::for_key(key) {
										paste_read = Some(PasteRead::start(scope));
									} else if key == Key::Esc
										&& action_enabled(InputAction::Interrupt)
										&& !host.chat.live_voice_active()
										&& host.chat.is_working()
									{
										host.last_esc = None;
										send(intents, Intent::Abort);
									} else if key == Key::Esc
										&& !host.chat.live_voice_active()
										&& host.chat.composer_empty()
									{
										let now = Instant::now();
										if host.last_esc.is_some_and(|last| now.duration_since(last) <= DOUBLE_ESC) {
											host.last_esc = None;
											send(intents, Intent::RewindRequest);
										} else {
											host.last_esc = Some(now);
										}
																		} else if key == Key::Left
										&& host.chat.composer_empty()
										&& !host.chat.agent_roster().is_empty()
										&& host.left_double_tap()
									{
										open_agents_armed(&mut host, ctx);
										open_overlay(
											terminal,
											renderer,
											&mut host,
											viewport,
											&mut resize,
										)?;
									} else if (key == Key::Ctrl('c')
										&& !action_enabled(InputAction::Clear))
										|| (key == Key::Ctrl('d')
											&& !action_enabled(InputAction::Exit))
										|| (key == Key::Ctrl('o')
											&& !action_enabled(InputAction::ToggleToolTree))
										|| (key == Key::Ctrl('t')
											&& !action_enabled(InputAction::ToggleThinking))
										|| (key == Key::FollowUp
											&& !action_enabled(InputAction::FollowUp))
									{
										// An explicit binding replaces its legacy fallback.
									} else {
										host.last_esc = None;
										recap_at = None;
										let result = host.chat.handle_key(key);
										if let Some(text) = host.chat.take_copied() { terminal.copy_to_clipboard(&text)?; }
										let mut submitted = false;
										while let Some((text, attachments, mode)) = host.chat.take_submission() {
											submitted = true;
																				if text.trim() == "/images" {
												host.overlay = Some(Overlay::Images(ImageOverlay::open(
													&host.chat.composer_attachments(),
													ctx,
												)));
												open_overlay(
													terminal,
													renderer,
													&mut host,
													viewport,
													&mut resize,
												)?;
												continue;
											}
		send(intents, Intent::Submit { text, attachments, mode });
										}
										if let Some(exit) =
											host.apply_chat_key(result, Instant::now(), intents, ctx)
										{
											requested_exit = exit;
											break;
										}
										if !submitted {
											recap_at = idle_recap_deadline(recap_policy, &host.chat);
										}
									}
									next_frame = Some(Instant::now());
								},
								InputEvent::Paste(text) => {
									if let Some(active) = host.overlay.as_mut() {
										let event = active.handle_paste(&text);
										if let Some(exit) = apply_overlay_event(
											&mut host,
											event,
											ctx,
											viewport,
											intents,
											exit_on_session_change,
										) {
											requested_exit = exit;
											break;
										}
										if host.overlay.is_none() {
											close_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
										}
									} else if !host.sidebar.focused() {
										host.chat.handle_paste(&text);
										recap_at = idle_recap_deadline(recap_policy, &host.chat);
									}
									next_frame = Some(Instant::now());
								},
								InputEvent::Mouse(report) => {
									if let Some(active) = host.overlay.as_mut() {
										let event = active.handle_mouse(report.col, report.row, report.kind, viewport);
										if let Some(exit) = apply_overlay_event(
											&mut host,
											event,
											ctx,
											viewport,
											intents,
											exit_on_session_change,
										) {
											requested_exit = exit;
											break;
										}
										if host.overlay.is_none() {
											close_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
										}
									} else if !host.sidebar.handle_mouse(report.col, report.row, report.kind, viewport) {
										host.chat.handle_mouse(&report);
										recap_at = idle_recap_deadline(recap_policy, &host.chat);
									}
									next_frame = Some(Instant::now());
								},
								InputEvent::Focus(_) | InputEvent::Response(_) => {},
							}
						},
					},
					request = ask_binding.recv() => {
						if let Ok(request) = request {
							if host.enqueue_ask(request, ctx) {
								open_overlay(
									terminal,
									renderer,
									&mut host,
									viewport,
									&mut resize,
								)?;
							}
							drain_ui_intents(&mut host, intents);
							next_frame = Some(Instant::now());
						}
					},
					backend = events.recv_async() => match backend {
						Ok(BackendEvent::NewSessionRequested) if exit_on_session_change => {
							requested_exit = HostExit::NewSession;
							break;
						},
						Ok(BackendEvent::SessionResumeRequested(session)) if exit_on_session_change => {
							requested_exit = HostExit::Resume(session);
							break;
						},
						Ok(event) => {
							let was_working = host.chat.is_working();
							let arm_recap = matches!(
								&event,
								BackendEvent::Status(facts) if was_working && !facts.working
							);
							match &event {
								BackendEvent::RecapPolicy { enabled, idle_seconds } => {
									recap_policy = Some((*enabled, *idle_seconds));
									if !*enabled {
										recap_at = None;
									}
								},
								BackendEvent::Status(facts) if facts.working => {
									recap_at = None;
								},
								BackendEvent::ApprovalPending(_) if title_enabled => {
									terminal.set_title("Approval required · omp")?;
								},
								BackendEvent::ApprovalSettled { .. }
									if title_enabled && host.pending_approvals <= 1 =>
								{
									terminal.set_title(host.session_title.as_str())?;
								},
								BackendEvent::Error(message) => {
									if title_enabled {
										terminal.set_title("Error · omp")?;
									}
									if error_notify {
										terminal.notify(
											&Notification::builder()
												.title("omp error")
												.body(message.clone())
												.id("omp-error")
												.urgency(Urgency::Critical)
												.build(),
										)?;
									}
								},
								BackendEvent::Ack { interrupted: false } => {
									if title_enabled {
										terminal.set_title(host.session_title.as_str())?;
									}
									if completion_notify {
										terminal.notify(
											&Notification::builder()
												.title("omp")
												.body("Turn complete")
												.id("omp-complete")
												.build(),
										)?;
									}
								},
								BackendEvent::SessionTitle(title) => {
									host.session_title = sf!("{title} · omp");
									if title_enabled && host.pending_approvals == 0 {
										terminal.set_title(host.session_title.as_str())?;
									}
								},
								BackendEvent::CopyToClipboard(text) => {
									terminal.copy_to_clipboard(text)?;
								},
								BackendEvent::TerminalNotification(notification) => {
									terminal.notify(notification)?;
								},
								BackendEvent::TerminalProgress(progress) => {
									terminal.set_progress(*progress)?;
								},
								_ => {},
							}
							let had_overlay = host.overlay.is_some();
							if let Some(intent) = apply_terminal_backend(&mut host, event, ctx) {
								send(intents, Intent::Git(intent));
							}
							if arm_recap {
								recap_at = idle_recap_deadline(recap_policy, &host.chat);
							}
							send_pty_resize(&mut host, viewport, intents);
							drain_ui_intents(&mut host, intents);
							if !had_overlay && host.overlay.is_some() {
								open_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
							} else if had_overlay && host.overlay.is_none() {
								close_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
							}
							next_frame = Some(Instant::now());
						},
						Err(_) => break,
					},
					clipboard = async { (&mut paste_read.as_mut().expect("branch gated").clipboard).await }, if paste_read.is_some() => {
						let read = paste_read.take().expect("branch gated");
						if let Ok(Some(clipboard)) = clipboard
							&& let Some(text) = clipboard_paste_text(clipboard)
							&& host.overlay.is_none()
							&& !host.sidebar.focused()
						{
							match read.scope {
								ClipboardRead::Text => host.chat.handle_paste_raw(&text),
								ClipboardRead::Smart => host.chat.handle_paste(&text),
							}
							recap_at = idle_recap_deadline(recap_policy, &host.chat);
							next_frame = Some(Instant::now());
						}
					},
					() = deadline(paste_deadline) => paste_read = None,
					() = deadline(recap_at) => {
						recap_at = None;
						if matches!(recap_policy, Some((true, _)))
							&& !host.chat.is_working()
							&& host.chat.composer_empty()
						{
							send(intents, Intent::IdleRecap);
						}
					},
					() = deadline(next_frame) => {
						observe_resize(terminal, &mut viewport, &mut resize, Instant::now())?;
						host.chat.set_right_inset(host.sidebar.reserved(viewport));
						start_pending_replay(renderer, &mut host, &mut pending_replay)?;
						// A retired batch may leave further finalized prefixes
						// (or replay batches) ready: repaint immediately to
						// drain them instead of waiting for the next event.
						next_frame = match paint_host(renderer, &mut host, viewport, Retirement::Pressure)? {
							PaintKind::Retired | PaintKind::Deferred => Some(Instant::now()),
							PaintKind::Presented => chat_deadline(&host.chat),
						};
					},
					() = deadline(resize.map(ResizeState::deadline)) => {
						let now = Instant::now();
						if !resize.is_some_and(|state| state.settled(now)) { continue; }
						host.chat.set_right_inset(host.sidebar.reserved(viewport));
						// A settled width change leaves native scrollback rows
						// wrapped at the old width; refresh them through one
						// buffered replay without changing commit state.
						if viewport.width != settled_width {
							settled_width = viewport.width;
							let mode = if resize_scrollback == ResizeScrollback::Rebuild
								&& terminal.inside_multiplexer()
							{
								// ED3 wipes multiplexer pane history irrecoverably;
								// degrade to an append replay.
								ResizeScrollback::Append
							} else {
								resize_scrollback
							};
							pending_replay = (mode != ResizeScrollback::Preserve).then_some(mode);
						}
						resize = None;
						start_pending_replay(renderer, &mut host, &mut pending_replay)?;
						next_frame = match paint_host(renderer, &mut host, viewport, Retirement::Pressure)? {
							PaintKind::Retired | PaintKind::Deferred => Some(now),
							PaintKind::Presented => chat_deadline(&host.chat),
						};
					},
				}
	}
	host.fail_pending_modals();
	drain_ui_intents(&mut host, intents);
	if host.overlay.take().is_some() {
		close_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
	}
	if requested_exit == HostExit::Suspend {
		start_pending_replay(renderer, &mut host, &mut pending_replay)?;
	} else {
		// A latched rebuild replay would destructively clear native history
		// during teardown. Discard it and flush only genuinely unretired rows.
		let _ = pending_replay.take();
		host.chat.begin_history_flush();
		if requested_exit == HostExit::Quit {
			host.chat.cancel_active("Host closed");
		}
		loop {
			match paint_host(renderer, &mut host, viewport, Retirement::Flush)? {
				PaintKind::Retired | PaintKind::Deferred => {},
				PaintKind::Presented => break,
			}
		}
	}
	renderer.repaint("", Frame::new(viewport), viewport.height, &[])?;
	Ok(host_outcome(&host, requested_exit))
}

fn host_outcome(host: &ChatHost, exit: HostExit) -> HostOutcome {
	HostOutcome { exit, draft: Str::from(host.chat.composer_text()) }
}

fn apply_terminal_backend(
	host: &mut ChatHost,
	event: BackendEvent,
	ctx: &UiContext,
) -> Option<GitIntent> {
	match event {
		BackendEvent::HistoryRewind { user_index, text, attachments } => {
			let _ = host.chat.rewind_user(user_index, text.as_str());
			host.chat.stage_attachments(attachments);
			host.suppress_history_replay = true;
			None
		},
		BackendEvent::HistoryReplayFinished => {
			host.suppress_history_replay = false;
			None
		},
		BackendEvent::HistoryCleared => {
			host.chat.clear_history();
			None
		},
		_ if host.suppress_history_replay => None,
		event => apply_backend(host, event, ctx),
	}
}

fn apply_backend(host: &mut ChatHost, event: BackendEvent, ctx: &UiContext) -> Option<GitIntent> {
	match event {
		BackendEvent::UiRequest { correlation, request } => {
			host.enqueue_ui_request(correlation, request, ctx);
		},
		BackendEvent::ApplyUiEffect(effect) if host.apply_extension_effect(&effect, ctx) => {},
		BackendEvent::OpenRawStream { frames, summary } => {
			host.overlay = Some(Overlay::RawStream(RawStreamViewer::open(frames, summary, ctx)));
		},
		BackendEvent::RawStreamFrame { frame, summary } => {
			if let Some(Overlay::RawStream(viewer)) = host.overlay.as_mut() {
				viewer.push(frame, summary);
			}
		},
		BackendEvent::RawStreamSnapshot { frames, summary } => {
			if let Some(Overlay::RawStream(viewer)) = host.overlay.as_mut() {
				viewer.replace(frames, summary);
			}
		},
		BackendEvent::RawStreamClosed => {
			if matches!(host.overlay, Some(Overlay::RawStream(_))) {
				host.overlay = None;
			}
		},
		BackendEvent::OpenGitWorkbench(snapshot) => {
			let mut workbench = GitWorkbench::open(snapshot, ctx);
			let intent = workbench.initial_intent();
			host.overlay = Some(Overlay::Git(workbench));
			return intent;
		},
		BackendEvent::Git(update) => {
			return match host.overlay.as_mut() {
				Some(Overlay::Git(workbench)) => workbench.apply(update),
				_ => None,
			};
		},
		BackendEvent::OpenGuidedGoal => {
			host.overlay = Some(Overlay::GuidedGoal(GuidedGoalInterview::open(ctx)));
		},
		BackendEvent::OpenPlanReview { content } => {
			host.overlay = Some(Overlay::PlanReview(PlanReviewOverlay::open(
				content.as_str(),
				Default::default(),
				ctx,
			)));
		},
		BackendEvent::OpenPlanSavePrompt { content, suggested_path } => {
			host.overlay = Some(Overlay::PlanSave {
				prompt: PromptOverlay::open_prefilled("Save plan and quit", suggested_path, ctx),
				content,
			});
		},
		BackendEvent::OpenExtensionInspector(snapshot) => {
			host.overlay = Some(Overlay::Extensions(ExtensionInspector::open(snapshot, ctx)));
		},
		BackendEvent::ExtensionSnapshotUpdated(snapshot) => {
			if let Some(Overlay::Extensions(inspector)) = host.overlay.as_mut() {
				inspector.update_snapshot(snapshot);
			}
		},
		BackendEvent::ExtensionMcpUpdated(snapshot) => {
			if let Some(Overlay::Extensions(inspector)) = host.overlay.as_mut() {
				inspector.update_mcp(snapshot);
			}
		},
		BackendEvent::ExtensionProviderDisabled(provider_id) => {
			if let Some(Overlay::Extensions(inspector)) = host.overlay.as_mut() {
				inspector.provider_disabled(provider_id.as_str());
			}
		},
		BackendEvent::HistoryInspect { frame } => {
			host.overlay = Some(Overlay::History(HistoryInspector::open(frame)));
		},
		BackendEvent::ApprovalPending(ticket) => {
			host.pending_approvals = host.pending_approvals.saturating_add(1);
			host.approval_queue.push_back(ticket);
			open_next_queued_overlay(host, ctx);
		},
		BackendEvent::AutoQaConsent(request) => {
			if host.overlay.is_some() {
				host.autoqa_queue.push_back(request);
			} else {
				open_autoqa_consent(host, request, ctx);
			}
		},
		BackendEvent::ApprovalSettled { ticket_id } => {
			host.pending_approvals = host.pending_approvals.saturating_sub(1);
			let mounted = matches!(
				&host.overlay,
				Some(Overlay::Approval(approval)) if approval.ticket_id() == &ticket_id
			) || matches!(
				&host.overlay,
				Some(Overlay::ApprovalAmend { ticket, .. })
					if ticket.ticket_id.as_str() == ticket_id.as_str()
			);
			if mounted {
				host.overlay = None;
			}
			if host.active_approval.as_ref() == Some(&ticket_id) {
				host.active_approval = None;
			}
			host
				.approval_queue
				.retain(|ticket| ticket.ticket_id != ticket_id);
			open_next_queued_overlay(host, ctx);
		},
		BackendEvent::PtyStarted { id, command } => {
			host.overlay = Some(Overlay::Pty(PtyOverlay::open(id, command, ctx)));
		},
		BackendEvent::PtyOutput { id, chunk } => {
			if let Some(Overlay::Pty(pty)) = &mut host.overlay
				&& pty.id() == &id
			{
				pty.append_output(chunk);
			}
		},
		BackendEvent::PtyFinished { id, status, exit_code } => {
			if let Some(Overlay::Pty(pty)) = &mut host.overlay
				&& pty.id() == &id
			{
				pty.finish(status, exit_code);
			}
		},
		BackendEvent::Status(facts) => {
			host.sidebar.set_status(&facts);
			let _ = host.chat.apply_backend_event(BackendEvent::Status(facts));
		},
		BackendEvent::OpenModelPicker { rows, current } => {
			update_models(host, rows, current);
			host.open_models(ctx);
		},
		BackendEvent::ModelsUpdated { rows, current } => {
			update_models(host, rows, current);
		},
		BackendEvent::OpenModelHub(data) => {
			host.overlay = Some(Overlay::ModelHub(ModelHub::open(data, ctx)));
		},
		BackendEvent::ModelHubUpdated(data) => {
			if let Some(Overlay::ModelHub(hub)) = &mut host.overlay {
				hub.update(data);
			}
		},
		BackendEvent::Sessions(rows) => open_sessions(host, rows, ctx),
		BackendEvent::WelcomeLspServers(_) => {},
		BackendEvent::LoginProviders(rows) => open_login_providers(host, rows, ctx),
		BackendEvent::LogoutChoices { title, rows } => open_logout_choices(host, title, rows, ctx),
		BackendEvent::RewindTargets(rows) => open_rewind(host, rows, ctx),
		BackendEvent::AgentRoster(rows) => {
			if let Some(Overlay::AgentHub(hub)) = &mut host.overlay {
				hub.update_rows(&rows);
			}
			host.chat.set_agent_roster(rows);
		},
		BackendEvent::SettingsSchema(rows) => open_settings(host, rows, ctx),
		BackendEvent::OpenSelection { title, purpose, rows } => {
			host.overlay = Some(Overlay::Selection(SelectionOverlay::open(title, purpose, rows, ctx)));
		},
		BackendEvent::OpenAgentTree => open_agents(host, ctx),
		BackendEvent::Pause => open_pause(host, ctx),
		BackendEvent::NewSessionRequested => {},
		BackendEvent::SessionResumeRequested(_) => {},
		BackendEvent::LoginPanel { provider, event } => match &mut host.overlay {
			Some(Overlay::Login { panel }) => panel.update(event),
			_ => {
				let mut panel = LoginPanel::open(provider, ctx);
				panel.update(event);
				host.overlay = Some(Overlay::Login { panel });
			},
		},
		BackendEvent::LoginPanelClose => {
			if matches!(host.overlay, Some(Overlay::Login { .. })) {
				host.overlay = None;
			}
			let _ = host.chat.apply_backend_event(BackendEvent::LoginPanelClose);
		},
		event => {
			let _ = host.chat.apply_backend_event(event);
		},
	}
	open_next_queued_overlay(host, ctx);
	None
}
fn open_next_queued_overlay(host: &mut ChatHost, ctx: &UiContext) {
	if host.overlay.is_none()
		&& host.active_approval.is_none()
		&& let Some(ticket) = host.approval_queue.pop_front()
	{
		host.active_approval = Some(ticket.ticket_id.clone());
		host.overlay = Some(Overlay::Approval(ApprovalOverlay::open(ticket, ctx)));
	}
	if host.overlay.is_none()
		&& let Some(request) = host.autoqa_queue.pop_front()
	{
		open_autoqa_consent(host, request, ctx);
	}
	host.open_next_modal(ctx);
}

fn open_autoqa_consent(host: &mut ChatHost, request: ConsentRequest, ctx: &UiContext) {
	let consent = AutoQaConsent::new(request);
	let dialog = AskDialog::open(consent.question(), ctx);
	host.overlay = Some(Overlay::AutoQaConsent { dialog, consent });
}

fn update_models(host: &mut ChatHost, rows: Vec<ModelRow>, current: usize) {
	host.current_model = current.min(rows.len().saturating_sub(1));
	if let Some(Overlay::Models(picker)) = &mut host.overlay {
		picker.update_rows(&rows, host.current_model);
	}
	host.models = rows;
	if let Some(model) = host.models.get(host.current_model) {
		let mut facts = host.chat.status();
		facts.model = if model.name.is_empty() {
			model.key.clone()
		} else {
			model.name.clone()
		};
		host.sidebar.set_status(&facts);
		host.chat.set_status(facts);
	}
}

fn open_sessions(host: &mut ChatHost, sessions: Vec<SessionRow>, ctx: &UiContext) {
	let rows: Vec<ListRow> = sessions
		.into_iter()
		.map(|row| ListRow {
			key:    row.id,
			label:  if row.pinned {
				sf!("{} {}", ctx.charset.icon(Icon::Pin), row.label)
			} else {
				row.label
			},
			detail: row.detail,
		})
		.collect();
	let picker = ListPicker::open("Resume session", &rows, 0, ctx);
	host.overlay =
		Some(Overlay::List { picker, rows, prefill: Vec::new(), purpose: ListPurpose::Resume });
}

fn open_login_providers(host: &mut ChatHost, providers: Vec<SessionRow>, ctx: &UiContext) {
	host.overlay = Some(Overlay::Providers(ProviderPicker::open(providers, ctx)));
}

fn open_logout_choices(host: &mut ChatHost, title: Str, choices: Vec<SessionRow>, ctx: &UiContext) {
	let rows = choices
		.into_iter()
		.map(|row| ListRow { key: row.id, label: row.label, detail: row.detail })
		.collect::<Vec<_>>();
	let picker = ListPicker::open(title.as_str(), &rows, 0, ctx);
	host.overlay =
		Some(Overlay::List { picker, rows, prefill: Vec::new(), purpose: ListPurpose::Logout });
}

fn open_settings(host: &mut ChatHost, fields: Vec<SettingRow>, ctx: &UiContext) {
	host.overlay = Some(Overlay::Settings(SettingsOverlay::open(fields, ctx)));
}

fn open_agents(host: &mut ChatHost, ctx: &UiContext) {
	host.overlay = Some(Overlay::AgentHub(AgentHub::open(host.chat.agent_roster(), ctx)));
}
fn open_agents_armed(host: &mut ChatHost, ctx: &UiContext) {
	let mut hub = AgentHub::open(host.chat.agent_roster(), ctx);
	hub.arm_close_tap();
	host.overlay = Some(Overlay::AgentHub(hub));
}

fn open_pause(host: &mut ChatHost, ctx: &UiContext) {
	let rows = vec![ListRow {
		key:    sf!("resume"),
		label:  sf!("Resume"),
		detail: sf!("Press Enter or Esc to return to the session"),
	}];
	let picker = ListPicker::open("Paused", &rows, 0, ctx);
	host.overlay =
		Some(Overlay::List { picker, rows, prefill: Vec::new(), purpose: ListPurpose::Pause });
}

fn open_rewind(host: &mut ChatHost, targets: Vec<RewindTargetRow>, ctx: &UiContext) {
	let mut prefill = Vec::with_capacity(targets.len());
	let rows: Vec<ListRow> = targets
		.into_iter()
		.rev()
		.map(|row| {
			prefill.push(row.text.clone());
			ListRow {
				key:    Str::new(row.event.to_string()),
				label:  Str::new(row.text.lines().next().unwrap_or("")),
				detail: sf!("rewind here"),
			}
		})
		.collect();
	let picker = ListPicker::open("Rewind history", &rows, 0, ctx);
	host.overlay = Some(Overlay::List { picker, rows, prefill, purpose: ListPurpose::Rewind });
}

fn drain_ui_intents(host: &mut ChatHost, intents: &Sender<Intent>) -> bool {
	let mut changed = false;
	while let Some(intent) = host.pending_ui_intents.pop_front() {
		send(intents, intent);
		changed = true;
	}
	changed
}

fn send_pty_resize(host: &mut ChatHost, viewport: Size, intents: &Sender<Intent>) {
	let Some(Overlay::Pty(pty)) = &mut host.overlay else {
		return;
	};
	let _ = pty.layer(viewport);
	let (rows, columns) = pty.dimensions();
	send(intents, Intent::PtyResize { id: pty.id().clone(), rows, columns });
}

fn apply_overlay_event(
	host: &mut ChatHost,
	event: OverlayEvent,
	ctx: &UiContext,
	viewport: Size,
	intents: &Sender<Intent>,
	exit_on_session_change: bool,
) -> Option<HostExit> {
	match event {
		OverlayEvent::Consumed => {},
		OverlayEvent::Git(intent) => send(intents, Intent::Git(intent)),
		OverlayEvent::GoalComplete { objective, token_budget } => {
			send(intents, Intent::SetGoal { objective, token_budget });
			host.overlay = None;
		},
		OverlayEvent::PlanReviewComplete(feedback) => {
			send(intents, Intent::Submit {
				text:        feedback.to_string(),
				attachments: Vec::new(),
				mode:        SubmitMode::Steer,
			});
			host.overlay = None;
		},
		OverlayEvent::PlanSavePathRequest(content) => {
			send(intents, Intent::PlanSavePathRequest { content });
			host.overlay = None;
		},
		OverlayEvent::PlanSaveSubmit { path, content } => {
			send(intents, Intent::SavePlanAndQuit { path, content });
			host.overlay = None;
		},
		OverlayEvent::ExtensionToggle { id, enabled } => {
			send(intents, Intent::ToggleExtension { id, enabled });
		},
		OverlayEvent::PtyInput { id, data } => send(intents, Intent::PtyInput { id, data }),
		OverlayEvent::PtyKill { id } => send(intents, Intent::PtyKill { id }),
		OverlayEvent::ModelHub(intent) => send(intents, Intent::ModelHub(intent)),
		OverlayEvent::LoginRequest(provider) => {
			send(intents, Intent::Login(Some(provider)));
			host.overlay = None;
		},
		OverlayEvent::Close => {
			if matches!(host.overlay, Some(Overlay::Extensions(_))) {
				send(intents, Intent::CloseExtensionInspector);
			}
			if matches!(host.overlay, Some(Overlay::Git(_))) {
				send(intents, Intent::Git(GitIntent::Close));
			}
			host.overlay = None;
		},
		OverlayEvent::OpenAgentPrompt(agent_id) => {
			host.overlay = Some(Overlay::AgentPrompt {
				prompt: PromptOverlay::open("Steer selected agent", false, ctx),
				agent_id,
				revive: false,
			});
		},
		OverlayEvent::OpenAgentRevivePrompt(agent_id) => {
			host.overlay = Some(Overlay::AgentPrompt {
				prompt: PromptOverlay::open("Revive selected agent", false, ctx),
				agent_id,
				revive: true,
			});
		},
		OverlayEvent::AgentSteerPrompt { agent_id, prompt } => {
			send(intents, Intent::AgentSteer { id: agent_id, prompt });
			host.overlay = None;
		},
		OverlayEvent::AgentRevivePrompt { agent_id, prompt } => {
			send(intents, Intent::AgentRevive { id: agent_id, prompt });
			host.overlay = None;
		},
		OverlayEvent::AgentKill(id) => {
			send(intents, Intent::AgentKill { id });
			host.overlay = None;
		},
		OverlayEvent::Pick(index) => match host.overlay.as_ref() {
			Some(Overlay::Models(_)) => {
				if let Some(model) = host.models.get(index) {
					host.current_model = index;
					send(intents, Intent::SwitchModel(model.key.clone()));
				}
				host.overlay = None;
			},
			Some(Overlay::List { rows, prefill, purpose, .. }) => {
				if let Some(row) = rows.get(index) {
					match purpose {
						ListPurpose::Resume => {
							let id = row.key.clone();
							send(intents, Intent::Resume(Some(id.clone())));
							if exit_on_session_change {
								return Some(HostExit::Resume(id));
							}
						},
						ListPurpose::Rewind => {
							if let Ok(event) = row.key.parse::<u64>() {
								if let Some(text) = prefill.get(index) {
									host.chat.set_composer_text(text);
								}
								send(intents, Intent::Rewind { event });
							}
						},
						ListPurpose::Pause => {},
						ListPurpose::Logout => {
							send(intents, Intent::Logout(Some(row.key.clone())));
						},
					}
				}
				host.overlay = None;
			},
			Some(Overlay::Providers(picker)) => {
				if let Some(provider) = picker.key(index) {
					send(intents, Intent::Login(Some(provider.clone())));
				}
				host.overlay = None;
			},
			_ => {},
		},
		OverlayEvent::Palette(action) => match action {
			PaletteAction::Intent(intent) => {
				let exit = match &intent {
					Intent::Quit => Some(HostExit::Quit),
					Intent::Resume(Some(id)) if exit_on_session_change => {
						Some(HostExit::Resume(id.clone()))
					},
					Intent::NewSession if exit_on_session_change => Some(HostExit::NewSession),
					_ => None,
				};
				send(intents, intent);
				host.overlay = None;
				if exit.is_some() {
					return exit;
				}
			},
			PaletteAction::OpenModelPicker => host.open_models(ctx),
			PaletteAction::ToggleSidebar => {
				host.sidebar.toggle();
				host.chat.set_right_inset(host.sidebar.reserved(viewport));
				host.overlay = None;
			},
			PaletteAction::Insert(text) => {
				host.chat.set_composer_text(&text);
				host.overlay = None;
			},
		},
		OverlayEvent::Prompt(value) | OverlayEvent::LoginSubmit(value) => {
			send(intents, Intent::AuthAnswer { value: value.to_string() });
			host.overlay = None;
		},
		OverlayEvent::PromptCancel | OverlayEvent::LoginCancel => {
			send(intents, Intent::AuthCancel);
			host.overlay = None;
		},
		OverlayEvent::ApprovalCancel => match host.overlay.take() {
			Some(Overlay::ApprovalAmend { ticket, .. }) => {
				host.overlay = Some(Overlay::Approval(ApprovalOverlay::open(ticket, ctx)));
			},
			overlay => host.overlay = overlay,
		},
		OverlayEvent::ApprovalDecide { ticket_id, action } => {
			send(intents, Intent::Approval { ticket_id, action });
			host.overlay = None;
		},
		OverlayEvent::ApprovalAmend(value) => match host.overlay.take() {
			Some(Overlay::Approval(approval)) => {
				let ticket = approval.ticket().clone();
				host.overlay = Some(Overlay::ApprovalAmend {
					prompt: PromptOverlay::open("Amended exact command or subject", false, ctx),
					ticket,
				});
			},
			Some(Overlay::ApprovalAmend { ticket, .. }) => {
				send(intents, Intent::Approval {
					ticket_id: ticket.ticket_id,
					action:    ApprovalAction::Amend(value),
				});
			},
			overlay => host.overlay = overlay,
		},
		OverlayEvent::AskSubmit { selected, custom_input } => {
			if let Some(Overlay::Ask { request, .. }) = host.overlay.take() {
				let id = request.question.id.clone();
				request.answer(omp_tools::ask::Answer {
					id,
					selected,
					custom_input,
					note: None,
					timed_out: false,
				});
			}
		},
		OverlayEvent::AskCancel => {
			if let Some(Overlay::Ask { request, .. }) = host.overlay.take() {
				request.fail("Ask dialog cancelled");
			}
		},
		OverlayEvent::ExtensionDialog { correlation, response } => {
			if matches!(host.overlay, Some(Overlay::ExtensionDialog { .. })) {
				host.overlay = None;
			}
			send(intents, Intent::UiResponse { correlation, response });
		},
		OverlayEvent::ExtensionOverlayEvent(event) => {
			send(intents, Intent::UiOverlayEvent(event));
		},
		OverlayEvent::ExtensionOverlayClose(id) => {
			if matches!(
				host.overlay.as_ref(),
				Some(Overlay::ExtensionOverlay { overlay }) if overlay.id() == &id
			) {
				host.overlay = None;
			}
			send(
				intents,
				Intent::UiOverlayEvent(omp_proto::omp::ui::v1::OverlayEvent {
					overlay_id: id.to_string(),
					kind:       "close".to_owned(),
					value:      None,
				}),
			);
		},
		OverlayEvent::RawStreamClose => {
			send(intents, Intent::CloseRawStream);
			host.overlay = None;
		},
		OverlayEvent::RawStreamCopy(text) => {
			send(intents, Intent::CopyToClipboard(text));
		},
		OverlayEvent::AutoQaConsent(decision) => {
			if let Some(Overlay::AutoQaConsent { consent, .. }) = host.overlay.take() {
				send(intents, Intent::AutoQaConsent(consent.decide(decision)));
			}
		},
		OverlayEvent::SettingsPreview(changes) => {
			send(intents, Intent::ApplySettings { changes, commit: false });
		},
		OverlayEvent::SettingsCommit(changes) => {
			send(intents, Intent::ApplySettings { changes, commit: true });
			host.overlay = None;
		},
		OverlayEvent::Selection(purpose, key) => {
			send(intents, Intent::Select { purpose, key });
			host.overlay = None;
		},
	}
	open_next_queued_overlay(host, ctx);
	None
}

fn palette_entries() -> Vec<PaletteEntry> {
	vec![
		PaletteEntry::new(
			"Switch model",
			"Choose the model for the next turn",
			PaletteAction::OpenModelPicker,
		)
		.key("Alt+P"),
		PaletteEntry::new(
			"Configure models",
			"Assign model roles and retry fallbacks",
			PaletteAction::Intent(Intent::OpenModelHub),
		)
		.key("Alt+M"),
		PaletteEntry::new(
			"Toggle sidebar",
			"Show or hide session facts",
			PaletteAction::ToggleSidebar,
		)
		.key("Ctrl+B"),
		PaletteEntry::new(
			"Resume session",
			"Open recent sessions",
			PaletteAction::Intent(Intent::Resume(None)),
		),
		PaletteEntry::new(
			"Login",
			"Authenticate a provider",
			PaletteAction::Intent(Intent::Login(None)),
		),
		PaletteEntry::new(
			"Inspect history",
			"Search or scroll canonical committed history",
			PaletteAction::Intent(Intent::InspectHistory),
		)
		.key("Alt+H"),
		PaletteEntry::new(
			"Search prompt history",
			"Reuse a previously submitted prompt",
			PaletteAction::Intent(Intent::SearchHistory),
		)
		.key("Ctrl+R"),
		PaletteEntry::new("Help", "Show chat controls", PaletteAction::Intent(Intent::Help)),
		PaletteEntry::new("Quit", "Leave chat", PaletteAction::Intent(Intent::Quit)),
	]
}

fn send(intents: &Sender<Intent>, intent: Intent) {
	let _ = intents.send(intent);
}

fn observe_resize(
	terminal: &mut Terminal,
	viewport: &mut Size,
	resize: &mut Option<ResizeState>,
	observed_at: Instant,
) -> io::Result<()> {
	let Some(size) = terminal.take_resize()? else {
		return Ok(());
	};
	if size == *viewport && resize.is_none() {
		return Ok(());
	}
	*viewport = size;
	match resize {
		Some(state) => state.observe(observed_at),
		None => *resize = Some(ResizeState::new(observed_at)),
	}
	Ok(())
}

fn user_event(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	event: InputEvent,
) -> io::Result<Option<InputEvent>> {
	if terminal.handle_input_event(&event, renderer)? {
		return Ok(terminal.take_paste().and_then(|pasted| {
			let text = match pasted {
				Pasted::Text(text) => text,
				Pasted::Image(image) => image.persist().ok()?.display().to_string().into(),
			};
			Some(InputEvent::Paste(text))
		}));
	}
	Ok(Some(event))
}

fn clipboard_paste_text(clipboard: Clipboard) -> Option<String> {
	match clipboard {
		Clipboard::Text(text) => Some(text),
		Clipboard::Image(image) => Some(image.persist().ok()?.display().to_string()),
		Clipboard::Paths(paths) => Some(
			paths
				.iter()
				.map(|path| format!("\"{path}\""))
				.collect::<Vec<_>>()
				.join(" "),
		),
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaintKind {
	Presented,
	Retired,
	/// A geometry change forced a history-neutral present before the pending
	/// retirement; repaint immediately to retire.
	Deferred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Retirement {
	Disabled,
	Pressure,
	Flush,
}

fn start_pending_replay<W: Write>(
	_renderer: &mut Renderer<W>,
	host: &mut ChatHost,
	pending: &mut Option<ResizeScrollback>,
) -> io::Result<()> {
	if host.overlay.is_some() {
		return Ok(());
	}
	let Some(mode) = pending.take() else {
		return Ok(());
	};
	let mode = match mode {
		ResizeScrollback::Append => HistoryReplay::Append,
		ResizeScrollback::Rebuild => HistoryReplay::Rebuild,
		ResizeScrollback::Preserve => return Ok(()),
	};
	host.chat.begin_history_replay(mode);
	Ok(())
}

fn paint_host<W: Write>(
	renderer: &mut Renderer<W>,
	host: &mut ChatHost,
	viewport: Size,
	retirement: Retirement,
) -> io::Result<PaintKind> {
	let may_retire = retirement != Retirement::Disabled && host.overlay.is_none();
	let geometry_gate =
		may_retire && renderer.retire_requires_present(viewport.width, viewport.height);
	let batch = if geometry_gate {
		None
	} else {
		match retirement {
			Retirement::Disabled => None,
			Retirement::Pressure if host.overlay.is_none() => host.chat.retirement_batch(viewport),
			Retirement::Flush if host.overlay.is_none() => host.chat.flush_retirement_batch(viewport),
			Retirement::Pressure | Retirement::Flush => None,
		}
	};
	let rendered = match batch.as_ref() {
		Some(batch) => host.chat.render_after_retirement(viewport, batch),
		None => host.chat.render(viewport),
	};
	let mut layers = rail_layers(&mut host.sidebar, viewport);
	if let Some(overlay) = host.overlay.as_mut() {
		layers.push(overlay.layer(viewport));
	}
	let Some(batch) = batch else {
		renderer.present_damaged(
			rendered.frame,
			rendered.damage.as_slice(),
			viewport.height,
			&layers,
		)?;
		return Ok(if geometry_gate {
			PaintKind::Deferred
		} else {
			PaintKind::Presented
		});
	};
	let replayed = if let Some((mode, frames)) = batch.replay_plan() {
		renderer.replay_frames(frames, rendered.frame, viewport.height, &layers, mode)?;
		true
	} else if batch.frame.size().height == 0 {
		host.chat.mark_retired(&batch);
		return Ok(PaintKind::Retired);
	} else {
		renderer.retire(&batch.frame, rendered.frame, viewport.height, &layers)?;
		false
	};
	host.chat.mark_retired(&batch);
	Ok(if replayed {
		PaintKind::Presented
	} else {
		PaintKind::Retired
	})
}

fn open_overlay(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	host: &mut ChatHost,
	viewport: Size,
	_resize: &mut Option<ResizeState>,
) -> io::Result<()> {
	let overlay_kind = host.overlay.as_ref().expect("overlay opened").kind();
	if matches!(host.overlay.as_ref(), Some(Overlay::Git(_))) && host.saved_git_keymap.is_none() {
		host.saved_git_keymap = Some(terminal.keymap().clone());
		terminal.edit_keymap(|keymap| {
			for mods in [Mods { alt: true, ..Mods::default() }, Mods {
				alt: true,
				super_key: true,
				..Mods::default()
			}] {
				keymap.bind(Chord::new(Key::Up, mods), Key::JumpPrevious);
				keymap.bind(Chord::new(Key::Down, mods), Key::JumpNext);
			}
		});
	}
	let alt_enter = terminal.stage_alt_enter(AltScreenUse::Interactive);
	let rendered = host.chat.render(viewport);
	let mut layers = rail_layers(&mut host.sidebar, viewport);
	layers.push(
		host
			.overlay
			.as_mut()
			.expect("overlay opened")
			.layer(viewport),
	);
	renderer.repaint(
		alt_enter.as_deref().unwrap_or(""),
		rendered.frame.clone(),
		viewport.height,
		&layers,
	)?;
	tracing::debug!(overlay.kind = overlay_kind, "chat overlay opened");
	Ok(())
}

fn close_overlay(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	host: &mut ChatHost,
	viewport: Size,
	_resize: &mut Option<ResizeState>,
) -> io::Result<()> {
	if let Some(saved) = host.saved_git_keymap.take() {
		terminal.edit_keymap(|keymap| *keymap = saved);
	}
	let rendered = host.chat.render(viewport);
	let layers = rail_layers(&mut host.sidebar, viewport);
	let alt_exit = terminal.stage_alt_leave().unwrap_or("");
	renderer.repaint(alt_exit, rendered.frame.clone(), viewport.height, &layers)?;
	terminal.commit_alt_leave();
	tracing::debug!("chat overlay closed");
	Ok(())
}

fn chat_deadline(chat: &Chat) -> Option<Instant> {
	chat.next_wake().map(|delay| Instant::now() + delay)
}

fn idle_recap_deadline(policy: Option<(bool, u32)>, chat: &Chat) -> Option<Instant> {
	let Some((true, idle_seconds)) = policy else {
		return None;
	};
	if chat.is_working() || !chat.composer_empty() {
		return None;
	}
	let seconds = idle_seconds.clamp(1, 3600);
	Some(Instant::now() + Duration::from_secs(u64::from(seconds)))
}

async fn deadline(at: Option<Instant>) {
	match at {
		Some(at) => tokio::time::sleep_until(at.into()).await,
		None => future::pending().await,
	}
}

#[cfg(test)]
mod tests {
	use std::{
		cell::Cell,
		io::{self, Write},
		rc::Rc,
	};

	use bytes::Bytes;
	use omp_core::{Str, sf};
	use omp_proto::omp::{
		inference::v1::{Value as ProtoValue, ValueMap, value},
		ui::v1::{Dialog, DialogOutcome, ShowOverlay, Tml, UiRequest, ui_request, ui_response},
	};
	use omp_tools::ask::{OptionItem, Question};
	use omp_tui::{Frame, Key, Renderer, Size, UiContext, test_support::frame_row_text};

	use super::{
		ChatHost, Duration, HostExit, HostOptions, InputAction, InputBinding, Instant, Overlay,
		OverlayEvent, PaintKind, ResizeScrollback, ResizeState, RetainedChat, RetainedChatEffect,
		Retirement, apply_backend, apply_overlay_event, drain_ui_intents, paint_host,
		start_pending_replay,
	};
	use crate::{
		ApprovalAction, ApprovalTicketView, BackendEvent, Chat, ChatKey, HistoryInspector, Intent,
		LiveVoiceAction, ModelRow, ask,
	};
	fn ask_question(id: &'static str) -> Question {
		Question {
			id:          Str::new_static(id),
			question:    sf!("Choose"),
			header:      None,
			options:     vec![OptionItem {
				label:       sf!("Yes"),
				description: None,
				preview:     None,
			}],
			multi:       false,
			recommended: Some(0),
		}
	}

	fn approval_ticket(id: &'static str) -> ApprovalTicketView {
		ApprovalTicketView {
			ticket_id:     id.into(),
			invocation_id: Some(format!("invocation-{id}").into()),
			title:         "Approval required".into(),
			detail:        "Policy requires approval".into(),
			subject:       format!("command-{id}").into(),
			always_scope:  None,
			evidence:      Vec::new(),
		}
	}

	#[test]
	fn correlated_extension_dialog_settles_through_typed_intent() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (intents, received) = flume::unbounded();
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		let request = UiRequest {
			owner_invocation: 7,
			kind:             Some(ui_request::Kind::Dialog(Dialog {
				kind:    "select".to_owned(),
				title:   "Database".to_owned(),
				content: None,
				choices: vec!["Postgres".to_owned(), "SQLite".to_owned()],
				props:   None,
			})),
			props:            None,
		};

		let _ = apply_backend(
			&mut host,
			BackendEvent::UiRequest { correlation: sf!("dialog-7"), request },
			&ctx,
		);
		let event = host
			.overlay
			.as_mut()
			.expect("dialog opens")
			.handle_key(Key::Enter);
		let _ = apply_overlay_event(&mut host, event, &ctx, viewport, &intents, false);

		let Intent::UiResponse { correlation, response } =
			received.try_recv().expect("correlated response")
		else {
			panic!("wrong intent")
		};
		assert_eq!(correlation, "dialog-7");
		assert!(matches!(
			response.kind,
			Some(ui_response::Kind::DialogOutcome(outcome))
				if outcome.accepted && outcome.value.as_deref() == Some("Postgres")
		));
		assert!(host.overlay.is_none());
	}

	fn dialog_test_props(value: serde_json::Value) -> Option<ValueMap> {
		let serde_json::Value::Object(fields) = value else {
			return None;
		};
		Some(ValueMap {
			fields: fields
				.into_iter()
				.map(|(name, value)| (name, dialog_test_value(value)))
				.collect(),
		})
	}

	fn dialog_test_value(value: serde_json::Value) -> ProtoValue {
		let kind = match value {
			serde_json::Value::Null => value::Kind::Null(true),
			serde_json::Value::Bool(value) => value::Kind::Bool(value),
			serde_json::Value::String(value) => value::Kind::String(value),
			serde_json::Value::Number(value) => value.as_i64().map_or_else(
				|| {
					value.as_u64().map_or_else(
						|| value::Kind::Double(value.as_f64().unwrap_or_default()),
						value::Kind::Uint,
					)
				},
				value::Kind::Int,
			),
			serde_json::Value::Array(values) => {
				value::Kind::List(omp_proto::omp::inference::v1::ValueList {
					values: values.into_iter().map(dialog_test_value).collect(),
				})
			},
			serde_json::Value::Object(fields) => value::Kind::Map(ValueMap {
				fields: fields
					.into_iter()
					.map(|(name, value)| (name, dialog_test_value(value)))
					.collect(),
			}),
		};
		ProtoValue { kind: Some(kind) }
	}

	fn settle_extension_dialog(
		kind: &str,
		props: serde_json::Value,
		choices: Vec<String>,
		keys: &[Key],
	) -> DialogOutcome {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (intents, received) = flume::unbounded();
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		let request = UiRequest {
			owner_invocation: 17,
			kind:             Some(ui_request::Kind::Dialog(Dialog {
				kind: kind.to_owned(),
				title: "Extension dialog".to_owned(),
				content: None,
				choices,
				props: dialog_test_props(props),
			})),
			props:            None,
		};
		let _ = apply_backend(
			&mut host,
			BackendEvent::UiRequest { correlation: sf!("dialog-{kind}"), request },
			&ctx,
		);
		let mut event = OverlayEvent::Consumed;
		for &key in keys {
			event = host
				.overlay
				.as_mut()
				.expect("dialog remains open until settlement")
				.handle_key(key);
		}
		let _ = apply_overlay_event(&mut host, event, &ctx, viewport, &intents, false);
		let Intent::UiResponse { response, .. } = received
			.try_recv()
			.unwrap_or_else(|error| panic!("{kind} dialog response: {error}"))
		else {
			panic!("wrong intent")
		};
		let Some(ui_response::Kind::DialogOutcome(outcome)) = response.kind else {
			panic!("wrong response kind")
		};
		assert!(host.overlay.is_none());
		outcome
	}

	#[test]
	fn extension_dialog_kinds_accept_through_native_controls() {
		let confirm = settle_extension_dialog(
			"confirm",
			serde_json::json!({"message": "Proceed?"}),
			Vec::new(),
			&[Key::Enter],
		);
		assert!(confirm.accepted);
		assert!(!confirm.cancelled);
		let declined = settle_extension_dialog(
			"confirm",
			serde_json::json!({"message": "Proceed?"}),
			Vec::new(),
			&[Key::Tab, Key::Enter],
		);
		assert!(!declined.accepted);
		assert!(!declined.cancelled);
		assert!(declined.reason.is_none());

		let select = settle_extension_dialog(
			"select",
			serde_json::json!({}),
			vec!["alpha".to_owned(), "beta".to_owned()],
			&[Key::Enter],
		);
		assert_eq!(select.value.as_deref(), Some("alpha"));

		let multi = settle_extension_dialog(
			"multi_select",
			serde_json::json!({"checked": ["beta"]}),
			vec!["alpha".to_owned(), "beta".to_owned()],
			&[Key::Enter],
		);
		assert_eq!(multi.values, ["beta"]);

		let input = settle_extension_dialog(
			"input",
			serde_json::json!({
				"prefill": "valid-name",
				"placeholder": "name",
				"match": "[a-z-]+",
			}),
			Vec::new(),
			&[Key::Tab, Key::Right, Key::Enter],
		);
		assert_eq!(input.value.as_deref(), Some("valid-name"));

		let editor = settle_extension_dialog(
			"editor",
			serde_json::json!({"prefill": "let answer = 42;", "syntax": "rust"}),
			Vec::new(),
			&[Key::Tab, Key::Right, Key::Enter],
		);
		assert_eq!(editor.value.as_deref(), Some("let answer = 42;"));

		let form = settle_extension_dialog(
			"form",
			serde_json::json!({
				"fields": [
					{
						"id": "name",
						"kind": "text",
						"label": "Name",
						"value": "omp",
						"required": true,
					},
					{"id": "enabled", "kind": "bool", "label": "Enabled", "value": true},
					{
						"id": "region",
						"kind": "enum",
						"label": "Region",
						"options": ["us", "eu"],
						"value": "eu",
					},
					{
						"id": "scopes",
						"kind": "multi",
						"label": "Scopes",
						"options": ["repo", "issues"],
						"value": ["repo", "issues"],
					},
					{
						"id": "replicas",
						"kind": "number",
						"label": "Replicas",
						"value": 3,
						"min": 1,
						"max": 5,
					},
				],
			}),
			Vec::new(),
			&[Key::Tab, Key::Right, Key::Enter],
		);
		let fields = &form.answers.as_ref().expect("form answers").fields;
		assert!(matches!(
			fields.get("name").and_then(|value| value.kind.as_ref()),
			Some(value::Kind::String(value)) if value == "omp"
		));
		assert!(matches!(
			fields.get("enabled").and_then(|value| value.kind.as_ref()),
			Some(value::Kind::Bool(true))
		));
		assert!(matches!(
			fields.get("replicas").and_then(|value| value.kind.as_ref()),
			Some(value::Kind::Int(3))
		));
		assert!(matches!(
			fields.get("scopes").and_then(|value| value.kind.as_ref()),
			Some(value::Kind::List(values)) if values.values.len() == 2
		));

		let ask = settle_extension_dialog(
			"ask_user",
			serde_json::json!({
				"questions": [{
					"id": "database",
					"question": "Database?",
					"options": [{"value": "postgres", "label": "Postgres"}],
					"allow_freeform": true,
					"allow_note": true,
				}],
			}),
			Vec::new(),
			&[
				Key::Tab,
				Key::Char('o'),
				Key::Char('t'),
				Key::Char('h'),
				Key::Char('e'),
				Key::Char('r'),
				Key::Tab,
				Key::Char('n'),
				Key::Char('o'),
				Key::Char('t'),
				Key::Char('e'),
				Key::Tab,
				Key::Right,
				Key::Enter,
			],
		);
		let answer = ask
			.answers
			.as_ref()
			.and_then(|answers| answers.fields.get("database"))
			.and_then(|answer| answer.kind.as_ref());
		let Some(value::Kind::Map(answer)) = answer else {
			panic!("ask answer is an object")
		};
		assert!(matches!(
			answer.fields.get("freeform").and_then(|value| value.kind.as_ref()),
			Some(value::Kind::String(value)) if value == "other"
		));
		assert!(matches!(
			answer.fields.get("note").and_then(|value| value.kind.as_ref()),
			Some(value::Kind::String(value)) if value == "note"
		));
	}

	#[test]
	fn extension_confirm_renders_requested_countdown() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		let request = UiRequest {
			owner_invocation: 19,
			kind:             Some(ui_request::Kind::Dialog(Dialog {
				kind:    "confirm".to_owned(),
				title:   "Confirm".to_owned(),
				content: None,
				choices: Vec::new(),
				props:   dialog_test_props(serde_json::json!({
					"message": "Proceed?",
					"options": {"timeout": "3s", "countdown": true},
				})),
			})),
			props:            None,
		};
		let _ = apply_backend(
			&mut host,
			BackendEvent::UiRequest { correlation: sf!("dialog-countdown"), request },
			&ctx,
		);
		let layer = host
			.overlay
			.as_mut()
			.expect("confirm opens")
			.layer(viewport);
		let painted = (0..layer.frame.size().height)
			.map(|row| frame_row_text(layer.frame, row))
			.collect::<Vec<_>>()
			.join("\n");
		assert!(painted.contains("Proceed?"), "{painted}");
		assert!(painted.contains("Time remaining"), "{painted}");
		assert!(painted.contains("3s"), "{painted}");
	}

	#[test]
	fn extension_input_match_blocks_accept_until_valid() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (intents, received) = flume::unbounded();
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		let request = UiRequest {
			owner_invocation: 18,
			kind:             Some(ui_request::Kind::Dialog(Dialog {
				kind:    "input".to_owned(),
				title:   "Name".to_owned(),
				content: None,
				choices: Vec::new(),
				props:   dialog_test_props(serde_json::json!({
					"prefill": "Bad!",
					"match": "[a-z-]+",
				})),
			})),
			props:            None,
		};
		let _ = apply_backend(
			&mut host,
			BackendEvent::UiRequest { correlation: sf!("dialog-validation"), request },
			&ctx,
		);
		let dialog = host.overlay.as_mut().expect("input dialog opens");
		assert!(matches!(dialog.handle_key(Key::Tab), OverlayEvent::Consumed));
		assert!(matches!(dialog.handle_key(Key::Right), OverlayEvent::Consumed));
		assert!(matches!(dialog.handle_key(Key::Enter), OverlayEvent::Consumed));
		assert!(host.overlay.is_some());
		assert!(received.try_recv().is_err());

		let dialog = host.overlay.as_mut().expect("invalid dialog remains");
		for key in [
			Key::BackTab,
			Key::Ctrl('u'),
			Key::Char('v'),
			Key::Char('a'),
			Key::Char('l'),
			Key::Char('i'),
			Key::Char('d'),
			Key::Tab,
			Key::Right,
		] {
			assert!(matches!(dialog.handle_key(key), OverlayEvent::Consumed));
		}
		let event = dialog.handle_key(Key::Enter);
		let _ = apply_overlay_event(&mut host, event, &ctx, viewport, &intents, false);
		let Intent::UiResponse { response, .. } = received.try_recv().expect("valid input settles")
		else {
			panic!("wrong intent")
		};
		assert!(matches!(
			response.kind,
			Some(ui_response::Kind::DialogOutcome(outcome))
				if outcome.accepted && outcome.value.as_deref() == Some("valid")
		));
	}

	#[test]
	fn every_extension_dialog_kind_dismisses_with_reason() {
		for kind in ["confirm", "select", "multi_select", "input", "editor", "form", "ask_user"] {
			let outcome = settle_extension_dialog(
				kind,
				if kind == "ask_user" {
					serde_json::json!({
						"questions": [{"id": "q", "question": "Question?"}],
					})
				} else {
					serde_json::json!({})
				},
				vec!["choice".to_owned()],
				&[Key::Esc],
			);
			assert!(!outcome.accepted, "{kind}");
			assert!(outcome.cancelled, "{kind}");
			assert_eq!(outcome.reason.as_deref(), Some("dismissed"), "{kind}");
		}
	}

	#[test]
	fn retained_extension_overlay_opens_queries_and_closes_idempotently() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (intents, received) = flume::unbounded();
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		let request = UiRequest {
			owner_invocation: 8,
			kind:             Some(ui_request::Kind::ShowOverlay(ShowOverlay {
				kind:    "custom".to_owned(),
				content: Some(Tml {
					source: Bytes::from_static(b"<col><input id=value submit value=initial/></col>"),
					hash:   0,
				}),
				options: None,
				props:   None,
			})),
			props:            None,
		};
		let _ = apply_backend(
			&mut host,
			BackendEvent::UiRequest { correlation: sf!("overlay-8"), request },
			&ctx,
		);
		assert!(drain_ui_intents(&mut host, &intents));
		let Intent::UiResponse { response, .. } = received.try_recv().expect("opened response")
		else {
			panic!("wrong intent")
		};
		let Some(ui_response::Kind::OverlayOpened(opened)) = response.kind else {
			panic!("overlay did not open")
		};

		let values = UiRequest {
			owner_invocation: 8,
			kind:             Some(ui_request::Kind::OverlayValues(
				omp_proto::omp::ui::v1::OverlayValues {
					overlay_id: opened.overlay_id.clone(),
					values:     Vec::new(),
				},
			)),
			props:            None,
		};
		let _ = apply_backend(
			&mut host,
			BackendEvent::UiRequest { correlation: sf!("values"), request: values },
			&ctx,
		);
		assert!(drain_ui_intents(&mut host, &intents));
		assert!(matches!(
			received.try_recv(),
			Ok(Intent::UiResponse {
				response: omp_proto::omp::ui::v1::UiResponse {
					kind: Some(ui_response::Kind::Values(_)),
					..
				},
				..
			})
		));

		for correlation in ["close", "close-again"] {
			let close = UiRequest {
				owner_invocation: 8,
				kind:             Some(ui_request::Kind::CloseOverlay(
					omp_proto::omp::ui::v1::CloseOverlay { overlay_id: opened.overlay_id.clone() },
				)),
				props:            None,
			};
			let _ = apply_backend(
				&mut host,
				BackendEvent::UiRequest { correlation: Str::new(correlation), request: close },
				&ctx,
			);
			assert!(drain_ui_intents(&mut host, &intents));
			if correlation == "close" {
				assert!(matches!(received.try_recv(), Ok(Intent::UiOverlayEvent(_))));
			}
			assert!(matches!(received.try_recv(), Ok(Intent::UiResponse { .. })));
		}
		assert!(host.overlay.is_none());
	}

	#[test]
	fn ask_requests_settle_fifo_without_busy_failures() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (intents, _received) = flume::unbounded();
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		let (first, first_result) = ask::test_request(ask_question("first"));
		let (second, second_result) = ask::test_request(ask_question("second"));

		assert!(host.enqueue_ask(first, &ctx));
		assert!(!host.enqueue_ask(second, &ctx));
		assert_eq!(host.modal_queue.len(), 1);
		let _ = apply_overlay_event(
			&mut host,
			OverlayEvent::AskSubmit { selected: vec![sf!("Yes")], custom_input: None },
			&ctx,
			viewport,
			&intents,
			false,
		);

		assert!(matches!(
			host.overlay,
			Some(Overlay::Ask { ref request, .. }) if request.question.id == "second"
		));
		assert_eq!(
			first_result
				.try_recv()
				.expect("first answer")
				.expect("first succeeds")
				.selected,
			["Yes"]
		);
		assert!(second_result.try_recv().is_err());
	}

	#[test]
	fn pending_draft_keeps_focus_until_clear_then_opens_ask() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (intents, _received) = flume::unbounded();
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		host.chat.set_composer_text("Actually use SQLite");
		let (request, _result) = ask::test_request(ask_question("guarded"));

		assert!(!host.enqueue_ask(request, &ctx));
		assert!(host.overlay.is_none());
		assert_eq!(host.chat.composer_text(), "Actually use SQLite");
		assert_eq!(host.modal_queue.len(), 1);

		assert_eq!(host.apply_chat_key(ChatKey::Clear, Instant::now(), &intents, &ctx), None);
		assert!(host.chat.composer_empty());
		assert!(matches!(host.overlay, Some(Overlay::Ask { .. })));
	}

	#[test]
	fn clear_exit_ladder_preserves_orderly_exit_draft() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (intents, received) = flume::unbounded();
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		let now = Instant::now();
		host.chat.set_composer_text("draft");

		assert_eq!(host.apply_chat_key(ChatKey::Clear, now, &intents, &ctx), None);
		assert!(host.chat.composer_empty());
		assert!(received.try_recv().is_err());
		assert_eq!(
			host.apply_chat_key(ChatKey::Clear, now + Duration::from_millis(499), &intents, &ctx,),
			Some(HostExit::Quit)
		);
		assert!(matches!(received.try_recv(), Ok(Intent::Quit)));

		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		host.chat.set_composer_text("snapshot me");
		assert_eq!(host.apply_chat_key(ChatKey::Exit, now, &intents, &ctx), Some(HostExit::Quit));
		assert_eq!(super::host_outcome(&host, HostExit::Quit).draft, "snapshot me");
	}

	#[test]
	fn live_voice_keys_emit_session_actions_before_abort_routing() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (_events, receiver) = flume::unbounded();
		let (intents, requests) = flume::unbounded();
		let mut chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			Default::default(),
		);
		chat.resize(viewport, true);
		chat.host.chat.start_live_voice();

		assert_eq!(chat.key(Key::Char(' ')), RetainedChatEffect::Consumed);
		assert!(matches!(
			requests.try_recv(),
			Ok(Intent::LiveVoice(LiveVoiceAction::SetMuted(true)))
		));
		assert_eq!(chat.key(Key::Esc), RetainedChatEffect::Consumed);
		assert!(matches!(requests.try_recv(), Ok(Intent::LiveVoice(LiveVoiceAction::Close))));
	}

	#[test]
	fn ctrl_r_requests_prompt_history_before_the_composer_sees_it() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (_events, receiver) = flume::unbounded();
		let (intents, requests) = flume::unbounded();
		let mut chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			Default::default(),
		);
		chat.resize(viewport, true);
		chat.host.chat.set_composer_text("draft survives");

		assert_eq!(chat.key(Key::Ctrl('r')), RetainedChatEffect::Consumed);
		assert!(matches!(requests.try_recv(), Ok(Intent::SearchHistory)));
		assert_eq!(chat.host.chat.composer_text(), "draft survives");
	}

	#[test]
	fn approval_waits_behind_an_unrelated_modal_without_destroying_it() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		host.overlay = Some(Overlay::History(HistoryInspector::open(Frame::new(viewport))));

		let _ = apply_backend(&mut host, BackendEvent::ApprovalPending(approval_ticket("a")), &ctx);

		assert!(matches!(host.overlay, Some(Overlay::History(_))));
		assert_eq!(host.approval_queue.len(), 1);
		assert!(host.active_approval.is_none());
	}

	#[test]
	fn decided_approval_advances_queue_only_after_settlement() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (intents, received) = flume::unbounded();
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		let _ = apply_backend(&mut host, BackendEvent::ApprovalPending(approval_ticket("a")), &ctx);
		let _ = apply_backend(&mut host, BackendEvent::ApprovalPending(approval_ticket("b")), &ctx);

		let _ = apply_overlay_event(
			&mut host,
			OverlayEvent::ApprovalDecide {
				ticket_id: "a".into(),
				action:    ApprovalAction::AllowOnce,
			},
			&ctx,
			viewport,
			&intents,
			false,
		);
		assert!(host.overlay.is_none());
		assert_eq!(host.active_approval.as_deref(), Some("a"));
		assert!(matches!(
			received.try_recv(),
			Ok(Intent::Approval { ticket_id, .. }) if ticket_id == "a"
		));

		let _ =
			apply_backend(&mut host, BackendEvent::ApprovalSettled { ticket_id: "a".into() }, &ctx);
		assert_eq!(host.active_approval.as_deref(), Some("b"));
		assert!(matches!(host.overlay, Some(Overlay::Approval(_))));
		assert!(host.approval_queue.is_empty());
	}

	#[test]
	fn approval_cancel_keeps_the_only_decision_surface_reachable() {
		let ctx = UiContext::default();
		let viewport = Size::new(80, 24);
		let (intents, _received) = flume::unbounded();
		let mut host = ChatHost::new(Chat::new(&ctx), &ctx, viewport, Vec::new(), 0);
		let _ = apply_backend(&mut host, BackendEvent::ApprovalPending(approval_ticket("a")), &ctx);

		let _ = apply_overlay_event(
			&mut host,
			OverlayEvent::ApprovalCancel,
			&ctx,
			viewport,
			&intents,
			false,
		);

		assert_eq!(host.active_approval.as_deref(), Some("a"));
		assert!(matches!(host.overlay, Some(Overlay::Approval(_))));
	}

	#[test]
	fn resize_settle_window_restarts_at_each_event() {
		let started_at = Instant::now();
		let mut state = ResizeState::new(started_at);
		state.observe(started_at + Duration::from_millis(100));
		assert!(!state.settled(started_at + Duration::from_millis(219)));
		assert!(state.settled(started_at + Duration::from_millis(220)));
	}
	#[test]
	fn configured_actions_parse_and_external_editor_returns_exact_draft() {
		let binding = InputBinding::parse(
			"ctrl+x",
			InputAction::from_action_id("app.editor.external").expect("known action"),
		)
		.expect("canonical chord");
		assert_eq!(binding.key, Key::Ctrl('x'));

		let ctx = UiContext::default();
		let (_events, receiver) = flume::unbounded();
		let (intents, _requests) = flume::unbounded();
		let mut chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			sf!("expanded draft"),
		);
		assert_eq!(
			chat.action(InputAction::ExternalEditor),
			RetainedChatEffect::ExternalEditor(sf!("expanded draft"))
		);
	}

	#[test]
	fn retained_chat_exits_for_a_backend_session_transition() {
		let ctx = UiContext::default();
		let (events, receiver) = flume::unbounded();
		let (intents, _requests) = flume::unbounded();
		let mut chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			Default::default(),
		);
		events
			.send(BackendEvent::NewSessionRequested)
			.expect("retained chat receiver remains connected");

		assert_eq!(chat.poll(), RetainedChatEffect::Quit(HostExit::NewSession));
	}
	#[test]
	fn retained_chat_opens_its_sidebar_only_on_ctrl_b() {
		let ctx = UiContext::default();
		let (_events, receiver) = flume::unbounded();
		let (intents, _requests) = flume::unbounded();
		let mut chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			Default::default(),
		);
		chat.resize(Size::new(120, 30), true);

		assert!(chat.render().layers.is_empty());
		assert_eq!(chat.key(Key::Ctrl('b')), RetainedChatEffect::Consumed);
		assert_eq!(chat.render().layers.len(), 1);
	}

	#[test]
	fn retained_model_picker_commits_the_next_model() {
		let ctx = UiContext::default();
		let (events, receiver) = flume::unbounded();
		let (intents, requests) = flume::unbounded();
		let mut chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			Default::default(),
		);
		let row = |key: &'static str, name: &'static str| ModelRow {
			key:         sf!(key),
			name:        sf!(name),
			color:       None,
			provider_id: sf!("provider"),
			provider:    sf!("Provider"),
			context:     None,
			input_mtok:  None,
			output_mtok: None,
			efforts:     std::sync::Arc::from([]),
		};
		events
			.send(BackendEvent::ModelsUpdated {
				rows:    vec![row("provider/first", "First"), row("provider/second", "Second")],
				current: 0,
			})
			.expect("retained chat receiver remains connected");

		assert_eq!(chat.poll(), RetainedChatEffect::Consumed);
		assert_eq!(chat.key(Key::Alt('p')), RetainedChatEffect::Consumed);
		assert_eq!(chat.key(Key::Down), RetainedChatEffect::Consumed);
		assert_eq!(chat.key(Key::Enter), RetainedChatEffect::Consumed);

		let intent = requests.try_recv().expect("model pick emits an intent");
		let Intent::SwitchModel(model) = intent else {
			panic!("model pick emitted the wrong intent");
		};
		assert_eq!(model, "provider/second");
		assert!(chat.render().layers.is_empty());
	}

	fn finalized_host(ctx: &UiContext, viewport: Size) -> ChatHost {
		let mut chat = Chat::new(ctx);
		// Enough finalized rows to overflow the 40x8 live region, forcing a
		// capacity-pressure retirement offer.
		for index in 0..6 {
			chat.push_notice(format!("finalized {index}"));
		}
		ChatHost::new(chat, ctx, viewport, Vec::new(), 0)
	}

	#[test]
	fn startup_and_resumed_rows_do_not_issue_a_second_scrollback_clear() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut chat = Chat::new(&ctx);
		chat.push_notice("resumed transcript row");
		let mut host = ChatHost::new(chat, &ctx, viewport, Vec::new(), 0);
		let mut renderer = Renderer::new(Vec::new());

		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();
		while matches!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Flush).unwrap(),
			PaintKind::Retired | PaintKind::Deferred
		) {}
		let output = String::from_utf8(renderer.into_inner()).unwrap();
		assert!(!output.contains("\x1b[3J"), "{output:?}");
	}

	#[test]
	fn present_only_tick_leaves_finalized_blocks_pending() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut host = finalized_host(&ctx, viewport);
		let mut renderer = Renderer::new(Vec::new());

		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap(),
			PaintKind::Presented
		);
		assert!(host.chat.retirement_batch(viewport).is_some());
	}

	#[test]
	fn width_change_defers_retirement_until_the_viewport_repaints() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut host = finalized_host(&ctx, viewport);
		let mut renderer = Renderer::new(Vec::new());
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();

		// Retirement scrolls relative to the painted viewport, so a geometry
		// change presents once before the pending batch retires.
		let resized = Size::new(60, 8);
		assert_eq!(
			paint_host(&mut renderer, &mut host, resized, Retirement::Pressure).unwrap(),
			PaintKind::Deferred
		);
		assert_eq!(
			paint_host(&mut renderer, &mut host, resized, Retirement::Pressure).unwrap(),
			PaintKind::Retired
		);
	}

	#[test]
	fn finalized_prefix_retires_once_and_advances_frontier() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut host = finalized_host(&ctx, viewport);
		let mut renderer = Renderer::new(Vec::new());
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();

		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Retired
		);
		assert!(host.chat.retirement_batch(viewport).is_none());
		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Presented
		);
	}

	#[test]
	fn flush_retires_a_finalized_tail_without_pressure() {
		let viewport = Size::new(40, 20);
		let ctx = UiContext::default();
		let mut chat = Chat::new(&ctx);
		chat.push_notice("fits in the viewport");
		let mut host = ChatHost::new(chat, &ctx, viewport, Vec::new(), 0);
		let mut renderer = Renderer::new(Vec::new());
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();

		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Presented
		);
		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Flush).unwrap(),
			PaintKind::Retired
		);
	}

	#[test]
	fn replay_request_survives_an_overlay_without_requesting_another_pump() {
		let viewport = Size::new(40, 20);
		let ctx = UiContext::default();
		let mut chat = Chat::new(&ctx);
		chat.push_notice("committed row");
		let mut host = ChatHost::new(chat, &ctx, viewport, Vec::new(), 0);
		let mut renderer = Renderer::new(Vec::new());
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();
		paint_host(&mut renderer, &mut host, viewport, Retirement::Flush).unwrap();
		host.overlay = Some(Overlay::History(HistoryInspector::open(Frame::new(viewport))));
		let mut pending = Some(ResizeScrollback::Append);

		start_pending_replay(&mut renderer, &mut host, &mut pending).unwrap();
		assert_eq!(pending, Some(ResizeScrollback::Append));
		host.overlay = None;
		start_pending_replay(&mut renderer, &mut host, &mut pending).unwrap();
		assert_eq!(pending, None);
		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Presented
		);
	}

	#[test]
	fn retirement_waits_for_alt_overlay_and_runs_after_close() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut host = finalized_host(&ctx, viewport);
		let mut renderer = Renderer::new(Vec::new());
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();
		host.overlay = Some(Overlay::History(HistoryInspector::open(Frame::new(viewport))));

		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Presented
		);
		assert!(host.chat.retirement_batch(viewport).is_some());

		host.overlay = None;
		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Retired
		);
		assert!(host.chat.retirement_batch(viewport).is_none());
	}

	#[derive(Clone, Default)]
	struct WriteControl {
		fail:   Rc<Cell<bool>>,
		writes: Rc<Cell<usize>>,
	}

	struct SwitchWriter(WriteControl);

	impl Write for SwitchWriter {
		fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
			self.0.writes.set(self.0.writes.get() + 1);
			if self.0.fail.get() {
				Err(io::Error::other("surface lost"))
			} else {
				Ok(bytes.len())
			}
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	#[test]
	fn retirement_write_error_is_fatal_without_advancing_or_retrying() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut host = finalized_host(&ctx, viewport);
		let control = WriteControl::default();
		let mut renderer = Renderer::new(SwitchWriter(control.clone()));
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();
		control.fail.set(true);
		let writes_before = control.writes.get();

		let error = paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure)
			.expect_err("retirement write failure must escape the host coordinator");

		assert_eq!(error.kind(), io::ErrorKind::Other);
		assert_eq!(control.writes.get(), writes_before + 1);
		assert!(host.chat.retirement_batch(viewport).is_some());
	}
}
