//! Native-window adapter for the detached journal-first chat actor.

use std::{cell::RefCell, rc::Rc, time::Duration};

use omp_chat::{HostOptions, NativeEffect, NativeHost};
use omp_gui::{Effect, HostConfig, Scene, SceneFrame};
use omp_tui::{Dim, Key, Layer, MouseReport, OverlayOptions, Size};
use smallvec::SmallVec;

/// Runs the production chat projection in a native GPU window.
///
/// The scene receives only the detached `Snapshot + Event` actor contract;
/// kernel/session ownership stays in the application controller task.
pub(crate) fn run(options: HostOptions) -> miette::Result<()> {
	let options = Rc::new(RefCell::new(Some(options)));
	let result = Rc::new(RefCell::new(None));
	let build_options = Rc::clone(&options);
	let build_result = Rc::clone(&result);
	omp_gui::run(HostConfig { multiplex: false, ..HostConfig::default() }, move |_ui| {
		let options = build_options
			.borrow_mut()
			.take()
			.expect("single-window GUI builds one chat scene");
		GuiScene {
			host:             NativeHost::new(options, Size::new(100, 32)),
			viewport:         Size::new(100, 32),
			result:           Rc::clone(&build_result),
			approval_options: OverlayOptions::default().width(Dim::Pct(80)).z(30),
		}
	});
	result.borrow_mut().take().unwrap_or(Ok(()))
}

struct GuiScene {
	host:             NativeHost,
	viewport:         Size,
	result:           Rc<RefCell<Option<miette::Result<()>>>>,
	approval_options: OverlayOptions,
}

impl GuiScene {
	fn effect(&mut self, result: Result<NativeEffect, omp_chat::HostError>) -> Effect {
		match result {
			Ok(NativeEffect::Ignored) => Effect::Ignored,
			Ok(NativeEffect::Consumed) => Effect::Consumed,
			Ok(NativeEffect::Quit) => Effect::Quit,
			Err(error) => {
				*self.result.borrow_mut() = Some(Err(miette::miette!(error)));
				Effect::Quit
			},
		}
	}
}

impl Scene for GuiScene {
	fn resize(&mut self, viewport: Size, _settled: bool) {
		self.viewport = viewport;
		self.host.resize(viewport);
	}

	fn render(&mut self) -> SceneFrame<'_> {
		let mut layers = SmallVec::new();
		if let Some(frame) = self.host.approval_frame() {
			layers.push(Layer { frame, options: &self.approval_options, active: true });
		}
		SceneFrame {
			frame: self.host.frame(),
			viewport: self.viewport,
			editor_rows: self.host.editor_rows(),
			layers,
		}
	}

	fn key(&mut self, key: Key) -> Effect {
		let result = self.host.key(key);
		self.effect(result)
	}

	fn mouse(&mut self, _report: MouseReport) -> Effect {
		Effect::Ignored
	}

	fn paste(&mut self, text: &str, _raw: bool) -> Effect {
		match self.host.paste(text) {
			NativeEffect::Ignored => Effect::Ignored,
			NativeEffect::Consumed => Effect::Consumed,
			NativeEffect::Quit => Effect::Quit,
		}
	}

	fn poll(&mut self) -> Effect {
		let result = self.host.poll();
		self.effect(result)
	}

	fn tick(&self) -> Duration {
		Duration::from_millis(16)
	}
}

impl Drop for GuiScene {
	fn drop(&mut self) {
		if self.result.borrow().is_none() {
			*self.result.borrow_mut() = Some(Ok(()));
		}
	}
}
