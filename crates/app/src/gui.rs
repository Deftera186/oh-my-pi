//! Native-window adapter for the production retained chat surface.

use std::{cell::RefCell, future::Future, rc::Rc, time::Duration};

use omp_chat_ui::host::{HostExit, HostOutcome, RetainedChat, RetainedChatEffect};
use omp_gui::{Effect, HostConfig, Scene, SceneFrame};
use omp_tui::{Key, Keymap, MouseReport, Size, UiContext};

use crate::editor::{EditorOptions, edit_draft_detached};

/// Runs one production chat scene in a single native GPU window.
pub(crate) fn run<F>(
	keymap: Keymap,
	build: impl FnOnce(&UiContext) -> RetainedChat,
	bridge: F,
) -> (HostOutcome, miette::Result<()>)
where
	F: Future<Output = miette::Result<()>> + Send + 'static,
{
	let build = Rc::new(RefCell::new(Some(build)));
	let outcome = Rc::new(RefCell::new(None));
	let (result_tx, result_rx) = flume::bounded(1);
	let bridge_task = tokio::spawn(async move {
		let _ = result_tx.send(bridge.await);
	});
	omp_gui::run(HostConfig { keymap, multiplex: false, ..HostConfig::default() }, {
		let build = Rc::clone(&build);
		let outcome = Rc::clone(&outcome);
		move |ctx| {
			let build = build
				.borrow_mut()
				.take()
				.expect("single-scene GUI host requested another chat scene");
			GuiScene::new(build(ctx), Rc::clone(&outcome))
		}
	});
	let outcome = outcome
		.borrow_mut()
		.take()
		.expect("closed GUI scene must report a chat outcome");
	let bridge = result_rx
		.recv()
		.expect("GUI bridge must complete after its scene drops");
	drop(bridge_task);
	(outcome, bridge)
}

struct GuiScene {
	chat:    RetainedChat,
	outcome: Rc<RefCell<Option<HostOutcome>>>,
}

impl GuiScene {
	fn new(chat: RetainedChat, outcome: Rc<RefCell<Option<HostOutcome>>>) -> Self {
		Self { chat, outcome }
	}

	fn apply(&mut self, effect: RetainedChatEffect) -> Effect {
		match effect {
			RetainedChatEffect::Ignored => Effect::Ignored,
			RetainedChatEffect::Consumed => Effect::Consumed,
			RetainedChatEffect::Quit(exit) => {
				self.record_exit(exit);
				Effect::Quit
			},
			RetainedChatEffect::Clipboard(scope) => Effect::Clipboard(scope),
			RetainedChatEffect::SetClipboard(text) => Effect::SetClipboard(text),
			RetainedChatEffect::ExternalEditor(draft) => {
				match edit_draft_detached(draft.as_str(), EditorOptions::default()) {
					Ok(Some(replacement)) => {
						self.chat.replace_composer(replacement.into());
					},
					Ok(None) => {},
					Err(error) => tracing::warn!(%error, "GUI external editor failed"),
				}
				Effect::Consumed
			},
		}
	}

	fn record_exit(&self, exit: HostExit) {
		let mut outcome = self.outcome.borrow_mut();
		if outcome.is_none() {
			*outcome = Some(self.chat.outcome(exit));
		}
	}
}

impl Drop for GuiScene {
	fn drop(&mut self) {
		self.record_exit(HostExit::Quit);
	}
}

impl Scene for GuiScene {
	fn resize(&mut self, viewport: Size, settled: bool) {
		self.chat.resize(viewport, settled);
	}

	fn render(&mut self) -> SceneFrame<'_> {
		let frame = self.chat.render();
		SceneFrame {
			frame:       frame.frame,
			viewport:    frame.viewport,
			editor_rows: frame.editor_rows,
			layers:      frame.layers,
		}
	}

	fn key(&mut self, key: Key) -> Effect {
		let effect = self.chat.key(key);
		self.apply(effect)
	}

	fn mouse(&mut self, report: MouseReport) -> Effect {
		let effect = self.chat.mouse(report);
		self.apply(effect)
	}

	fn paste(&mut self, text: &str, raw: bool) -> Effect {
		let effect = self.chat.paste(text, raw);
		self.apply(effect)
	}

	fn poll(&mut self) -> Effect {
		let effect = self.chat.poll();
		self.apply(effect)
	}

	fn tick(&self) -> Duration {
		self.chat.tick()
	}
}

#[cfg(test)]
mod tests {
	use omp_chat_ui::{BackendEvent, Chat, host::HostOptions};

	use super::*;

	#[test]
	fn backend_session_transition_closes_the_native_scene() {
		let ctx = UiContext::default();
		let (events, receiver) = flume::unbounded();
		let (intents, _requests) = flume::unbounded();
		let chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			Default::default(),
		);
		let outcome = Rc::new(RefCell::new(None));
		let mut scene = GuiScene::new(chat, Rc::clone(&outcome));
		events
			.send(BackendEvent::NewSessionRequested)
			.expect("retained chat receiver remains connected");

		assert_eq!(scene.poll(), Effect::Quit);
		let outcome = outcome.borrow();
		assert_eq!(outcome.as_ref().map(|outcome| &outcome.exit), Some(&HostExit::NewSession));
	}
}
