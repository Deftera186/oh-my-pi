//! Interactive `ask@1` presentation over the core `UiRequest` dialog path.

use std::{
	future::Future,
	mem,
	pin::Pin,
	sync::{
		Arc, LazyLock,
		atomic::{AtomicU64, Ordering},
	},
};

use flume::Receiver;
use omp_core::{IntoStr, Str, sf};
use omp_proto::omp::ui::v1::{Dialog, UiRequest, ui_request};
use omp_tools::ask::{Answer, AskPresenter, Fault, HeadlessPresenter, Presentation, Question};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, UiEvent, dom,
};
use parking_lot::Mutex;

use crate::{
	PromptEvent, PromptOverlay,
	overlays::{OverlayPanel, panel_divider},
};

const OTHER_VALUE: &str = "__omp_ask_other__";

struct ActiveBinding {
	generation: u64,
	sender:     flume::Sender<AskRequest>,
}

static ACTIVE: LazyLock<Mutex<Option<ActiveBinding>>> = LazyLock::new(|| Mutex::new(None));
static NEXT_BINDING: AtomicU64 = AtomicU64::new(1);
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

/// One typed ask question carried beside its canonical protobuf request.
pub struct AskRequest {
	/// Canonical environment/UI protocol request.
	pub request:  UiRequest,
	/// Typed question used by the core-owned renderer.
	pub question: Question,
	reply:        flume::Sender<Result<Answer, Fault>>,
}

impl AskRequest {
	/// Whether the blocked caller has cancelled its presentation future.
	pub fn is_cancelled(&self) -> bool {
		self.reply.is_disconnected()
	}

	/// Resolves the blocked tool invocation with a UI answer.
	pub fn answer(self, answer: Answer) {
		let _ = self.reply.send(Ok(answer));
	}

	/// Resolves the blocked tool invocation with a presentation fault.
	pub fn fail(self, message: impl IntoStr) {
		let _ = self
			.reply
			.send(Err(Fault::Presenter { message: message.into_str() }));
	}
}
#[cfg(test)]
pub(crate) fn test_request(question: Question) -> (AskRequest, Receiver<Result<Answer, Fault>>) {
	let (reply, result) = flume::bounded(1);
	let request = AskRequest { request: dialog_request(&question), question, reply };
	(request, result)
}

/// Exclusive binding installed by the active terminal host.
#[must_use]
pub struct AskBinding {
	receiver:   Receiver<AskRequest>,
	generation: u64,
}

impl AskBinding {
	/// Receives the next canonical dialog request.
	pub async fn recv(&self) -> Result<AskRequest, flume::RecvError> {
		self.receiver.recv_async().await
	}

	/// Receives a pending canonical dialog request without waiting.
	pub fn try_recv(&self) -> Option<AskRequest> {
		self.receiver.try_recv().ok()
	}
}

impl Drop for AskBinding {
	fn drop(&mut self) {
		let mut active = ACTIVE.lock();
		if active
			.as_ref()
			.is_some_and(|binding| binding.generation == self.generation)
		{
			*active = None;
		}
	}
}

/// Binds the process-wide Ask presenter to the currently active TUI host.
/// A later binding cleanly replaces a stale/disconnected host.
pub fn bind() -> AskBinding {
	let (sender, receiver) = flume::unbounded();
	let generation = NEXT_BINDING.fetch_add(1, Ordering::Relaxed);
	*ACTIVE.lock() = Some(ActiveBinding { generation, sender });
	AskBinding { receiver, generation }
}

/// Presenter registered with `ask@1` during production registry assembly.
#[derive(Default)]
pub struct UiRequestPresenter;

/// Creates the shared presenter used by environment tool registration.
///
/// When no interactive host is bound, it deliberately delegates to the
/// documented deterministic headless policy instead of inventing UI answers.
pub fn presenter() -> Arc<dyn AskPresenter> {
	Arc::new(UiRequestPresenter)
}

impl AskPresenter for UiRequestPresenter {
	fn present<'p>(
		&'p self,
		questions: &'p [Question],
	) -> Pin<Box<dyn Future<Output = Result<Presentation, Fault>> + Send + 'p>> {
		let Some(sender) = ACTIVE.lock().as_ref().map(|binding| binding.sender.clone()) else {
			return HeadlessPresenter.present(questions);
		};
		Box::pin(async move {
			let mut answers = Vec::with_capacity(questions.len());
			for question in questions {
				let (reply, result) = flume::bounded(1);
				let request =
					AskRequest { request: dialog_request(question), question: question.clone(), reply };
				sender
					.send(request)
					.map_err(|_| presenter_fault("interactive UI disconnected"))?;
				answers.push(
					result
						.recv_async()
						.await
						.map_err(|_| presenter_fault("Ask dialog was dismissed"))??,
				);
			}
			Ok(Presentation { answers, headless: false })
		})
	}
}

fn dialog_request(question: &Question) -> UiRequest {
	UiRequest {
		owner_invocation: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
		kind:             Some(ui_request::Kind::Dialog(Dialog {
			kind:    "ask".to_owned(),
			title:   question.header.as_deref().unwrap_or("Ask").to_owned(),
			content: None,
			choices: question
				.options
				.iter()
				.map(|option| option.label.to_string())
				.collect(),
		})),
		props:            None,
	}
}

const fn presenter_fault(message: &'static str) -> Fault {
	Fault::Presenter { message: sf!(message) }
}

/// Result of routing input through an Ask dialog.
#[derive(Debug, Eq, PartialEq)]
pub enum AskDialogEvent {
	/// Event consumed while the dialog remains open.
	Consumed,
	/// User cancelled the dialog.
	Cancel,
	/// User submitted fixed selections or one custom answer.
	Submit {
		/// Selected model-authored option labels.
		selected:     Vec<Str>,
		/// User-authored answer entered through the always-present Other row.
		custom_input: Option<Str>,
	},
}

/// Core-owned Ask dialog supporting single and multi selection.
pub struct AskDialog {
	ui:       Ui,
	options:  OverlayOptions,
	question: Question,
	selected: Vec<Str>,
	custom:   Option<PromptOverlay>,
	ctx:      UiContext,
	width:    u16,
}

impl AskDialog {
	/// Opens a typed Ask question.
	pub fn open(question: Question, ctx: &UiContext) -> Self {
		let width = 72;
		let mut ui = build_dialog(&question, width, ctx);
		ui.focus_first();
		Self {
			ui,
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.z(30),
			question,
			selected: Vec::new(),
			custom: None,
			ctx: ctx.clone(),
			width,
		}
	}

	/// Routes a key through the dialog.
	pub fn handle_key(&mut self, key: Key) -> AskDialogEvent {
		if let Some(custom) = self.custom.as_mut() {
			return match custom.handle_key(key) {
				PromptEvent::Consumed => AskDialogEvent::Consumed,
				PromptEvent::Cancel => {
					self.custom = None;
					AskDialogEvent::Consumed
				},
				PromptEvent::Submit(value) if value.trim().is_empty() => AskDialogEvent::Consumed,
				PromptEvent::Submit(value) => {
					AskDialogEvent::Submit { selected: Vec::new(), custom_input: Some(value) }
				},
			};
		}
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	/// Routes pasted custom input.
	pub fn handle_paste(&mut self, text: &str) -> AskDialogEvent {
		if let Some(custom) = self.custom.as_mut() {
			return match custom.handle_paste(text) {
				PromptEvent::Submit(value) if !value.trim().is_empty() => {
					AskDialogEvent::Submit { selected: Vec::new(), custom_input: Some(value) }
				},
				PromptEvent::Consumed | PromptEvent::Cancel | PromptEvent::Submit(_) => {
					AskDialogEvent::Consumed
				},
			};
		}
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	/// Routes pointer input; an outside click cancels.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> AskDialogEvent {
		if let Some(custom) = self.custom.as_mut() {
			return match custom.handle_mouse(col, row, kind, viewport) {
				PromptEvent::Consumed => AskDialogEvent::Consumed,
				PromptEvent::Cancel => {
					self.custom = None;
					AskDialogEvent::Consumed
				},
				PromptEvent::Submit(value) if value.trim().is_empty() => AskDialogEvent::Consumed,
				PromptEvent::Submit(value) => {
					AskDialogEvent::Submit { selected: Vec::new(), custom_input: Some(value) }
				},
			};
		}
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => self.route(event),
			None if kind == Mouse::Click => AskDialogEvent::Cancel,
			None => AskDialogEvent::Consumed,
		}
	}

	/// Returns the centered composited layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		if let Some(custom) = self.custom.as_mut() {
			return custom.layer(viewport);
		}
		let width = self.width.min(viewport.width);
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn route(&mut self, event: UiEvent) -> AskDialogEvent {
		match event {
			UiEvent::Cancel => AskDialogEvent::Cancel,
			UiEvent::Submit if self.question.multi => AskDialogEvent::Submit {
				selected:     mem::take(&mut self.selected),
				custom_input: None,
			},
			UiEvent::Changed { value, .. } if value == OTHER_VALUE => {
				self.custom = Some(PromptOverlay::open("Other answer", false, &self.ctx));
				AskDialogEvent::Consumed
			},
			UiEvent::Changed { value, .. } if self.question.multi => {
				if let Some(at) = self.selected.iter().position(|chosen| chosen == &value) {
					self.selected.remove(at);
				} else {
					self.selected.push(value);
				}
				AskDialogEvent::Consumed
			},
			UiEvent::Changed { value, .. } => {
				AskDialogEvent::Submit { selected: vec![value], custom_input: None }
			},
			UiEvent::None
			| UiEvent::Submit
			| UiEvent::Filtered { .. }
			| UiEvent::Highlighted { .. }
			| UiEvent::Pressed(_)
			| UiEvent::Copied(_)
			| UiEvent::TreeActivated { .. }
			| UiEvent::TreeToggled { .. }
			| UiEvent::TreeAction { .. }
			| UiEvent::DiffAction { .. } => AskDialogEvent::Consumed,
		}
	}
}

fn build_dialog(question: &Question, width: u16, ctx: &UiContext) -> Ui {
	let title = question.header.clone().unwrap_or_else(|| sf!("Ask"));
	let prompt = question.question.clone();
	let multi = question.multi;
	let recommended = question
		.recommended
		.filter(|index| *index < question.options.len());
	let rows = u16::try_from(question.options.len().saturating_add(1))
		.unwrap_or(u16::MAX)
		.clamp(3, 12);
	let choices = question
		.options
		.iter()
		.enumerate()
		.map(|(index, choice)| {
			let recommended = recommended == Some(index);
			let label = if recommended {
				sf!("{} (Recommended)", choice.label)
			} else {
				choice.label.clone()
			};
			(choice.clone(), label, recommended)
		})
		.collect::<Vec<_>>();
	Ui::from_root(
		OverlayPanel::new(title).child(dom! {
			<col>
				<markdown>{prompt}</markdown>
				{panel_divider()}
				<select id="ask" multi={multi} h={rows}>
					for (choice, label, is_recommended) in choices {
						<option value={choice.label.clone()} label={label.clone()} recommended={is_recommended}>
							<col>
								<text>{label}</text>
								if let Some(description) = choice.description { <text dim>{description}</text> }
								if let Some(preview) = choice.preview { <markdown dim>{preview}</markdown> }
							</col>
						</option>
					}
					<option value={OTHER_VALUE} label="Other">
						<text>{"Other…"}</text>
					</option>
				</select>
				{panel_divider()}
				<text dim>{if multi { "Space toggle · Enter confirm · Esc cancel" } else { "Enter choose · Esc cancel" }}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

#[cfg(test)]
mod tests {
	use omp_tools::ask::OptionItem;

	use super::*;

	fn question(multi: bool) -> Question {
		Question {
			id: sf!("language"),
			question: sf!("Choose a language"),
			header: Some(sf!("Language")),
			options: vec![
				OptionItem { label: sf!("Rust"), description: None, preview: None },
				OptionItem { label: sf!("Python"), description: None, preview: None },
			],
			multi,
			recommended: Some(0),
		}
	}

	#[tokio::test(flavor = "current_thread")]
	async fn presenter_uses_headless_policy_without_bound_host() {
		*ACTIVE.lock() = None;
		let result = UiRequestPresenter
			.present(&[question(false)])
			.await
			.unwrap();
		assert!(result.headless);
		assert_eq!(result.answers[0].selected, ["Rust"]);
	}

	#[test]
	fn dialog_request_uses_canonical_ui_protocol() {
		let request = dialog_request(&question(true));
		let Some(ui_request::Kind::Dialog(dialog)) = request.kind else {
			panic!("not dialog")
		};
		assert_eq!(dialog.kind, "ask");
		assert_eq!(dialog.choices, ["Rust", "Python"]);
	}
	#[test]
	fn recommended_option_owns_initial_cursor_and_submission() {
		let ctx = UiContext::default();
		let mut question = question(false);
		question.recommended = Some(1);
		let mut dialog = AskDialog::open(question, &ctx);

		assert_eq!(dialog.handle_key(Key::Enter), AskDialogEvent::Submit {
			selected:     vec![sf!("Python")],
			custom_input: None,
		});
	}

	#[test]
	fn other_opens_a_guarded_custom_answer_editor() {
		let ctx = UiContext::default();
		let mut dialog = AskDialog::open(question(false), &ctx);

		assert_eq!(dialog.handle_key(Key::Down), AskDialogEvent::Consumed);
		assert_eq!(dialog.handle_key(Key::Down), AskDialogEvent::Consumed);
		assert_eq!(dialog.handle_key(Key::Enter), AskDialogEvent::Consumed);
		assert!(dialog.custom.is_some());
		assert_eq!(dialog.handle_paste("DuckDB"), AskDialogEvent::Consumed);
		assert_eq!(dialog.handle_key(Key::Enter), AskDialogEvent::Submit {
			selected:     Vec::new(),
			custom_input: Some(sf!("DuckDB")),
		});
	}
}
