//! Interactive retained diff-pane example.

use std::io;

use omp_tui::{
	AppEvent, AppOptions, DiffActionKind, DiffBuildOptions, DiffDocument, DiffPane, DiffPaneState,
	DiffPatchTarget, Key, Prop, Ui, UiContext, UiEvent, components::Col,
};

const OLD: &str = r#"use std::io;

fn greeting(name: &str) -> String {
	format!("Hello, {name}!")
}

fn punctuation() -> &'static str {
	"!"
}

fn audience() -> &'static str {
	"terminal"
}

fn main() -> io::Result<()> {
	println!("{}", greeting("world"));
	Ok(())
}
"#;

const NEW: &str = r#"use std::io;

fn greeting(name: &str) -> String {
	format!("Welcome, {name}!")
}

fn punctuation() -> &'static str {
	"!"
}

fn audience() -> &'static str {
	"terminal"
}

fn main() -> io::Result<()> {
	let audience = "terminal";
	println!("{}", greeting(audience));
	Ok(())
}
"#;

fn build(width: u16, height: u16, context: UiContext) -> Ui {
	let document =
		DiffDocument::build(OLD, NEW, "examples/greeting.rs", &DiffBuildOptions::default());
	let mut pane = DiffPane::new()
		.with(Prop::Id, "diff")
		.with(Prop::H, height.saturating_sub(1).max(1));
	pane.set_patch_target(Some(DiffPatchTarget::Stage));
	pane.set_document(Some(document), DiffPaneState::Ready);
	Ui::from_root(
		Col::new().child(pane).child(
			omp_tui::components::Pre::new()
				.with(Prop::Id, "status")
				.text("v mode  w wrap  n/p hunks  Shift+↑/↓ select  s/u/x actions"),
		),
		width,
		context,
	)
}

fn describe(event: &UiEvent) -> String {
	match event {
		UiEvent::DiffAction { action, target, .. } => format!("requested {action:?} on {target:?}"),
		_ => String::new(),
	}
}

#[tokio::main]
async fn main() -> io::Result<()> {
	let mut app = AppOptions::new()
		.mouse()
		.hotkeys([
			Key::Char('v'),
			Key::Char('w'),
			Key::Char('n'),
			Key::Char('p'),
			Key::Char('s'),
			Key::Char('u'),
			Key::Char('x'),
		])
		.quit([Key::Ctrl('c'), Key::Char('q')])
		.start(|env| build(env.viewport.width, env.viewport.height, env.ctx))
		.await?;

	while let Some(event) = app.next().await? {
		match event {
			AppEvent::Key(key) => {
				let action = app
					.ui_mut()
					.with_component_mut::<DiffPane, _>("diff", |pane| match key {
						Key::Char('v') => {
							pane.cycle_mode();
							None
						},
						Key::Char('w') => {
							pane.toggle_wrap();
							None
						},
						Key::Char('n') => {
							pane.jump_hunk(1);
							None
						},
						Key::Char('p') => {
							pane.jump_hunk(-1);
							None
						},
						Key::Char('s') => {
							pane.set_patch_target(Some(DiffPatchTarget::Stage));
							pane.request_action(DiffActionKind::Stage)
						},
						Key::Char('u') => {
							pane.set_patch_target(Some(DiffPatchTarget::Unstage));
							pane.request_action(DiffActionKind::Unstage)
						},
						Key::Char('x') => {
							pane.set_patch_target(Some(DiffPatchTarget::Stage));
							pane.request_action(DiffActionKind::Discard)
						},
						_ => None,
					});
				if let Some(Some(action)) = action {
					app.ui_mut().set_text("status", describe(&action));
				}
			},
			AppEvent::DiffAction { id, action, target } => {
				let event = UiEvent::DiffAction { id, action, target };
				app.ui_mut().set_text("status", describe(&event));
			},
			AppEvent::Resized(size) => {
				app.ui_mut()
					.set_height("diff", size.height.saturating_sub(1).max(1));
			},
			_ => {},
		}
	}
	Ok(())
}
