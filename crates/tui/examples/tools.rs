//! Elastic `ToolCard` gallery driven by the retained App runtime.
//!
//! `[`/`]` apply and release height pressure, `f` finalizes/collapses/removes
//! the browser card out of order, and `m` toggles reduced motion.

use std::io;

use omp_tui::{
	AppEvent, AppOptions, Key, Prop, Ui, UiContext,
	components::{Col, TextLeaf, ToolCard, ToolState},
};

const CARD_IDS: &[&str] = &["read", "bash", "edit", "browser", "task"];
const BROWSER_ID: &str = "browser";
const STATUS_ID: &str = "status";

fn card(
	id: &'static str,
	name: &'static str,
	intent: &'static str,
	activity: &'static str,
	badge: &'static str,
	body: &'static str,
) -> ToolCard {
	ToolCard::new()
		.with(Prop::Id, id)
		.with(Prop::H, 3_u16)
		.with(Prop::Anim, "180ms")
		.with(Prop::Ease, "out")
		.name(name)
		.intent(intent)
		.activity(activity)
		.badge(badge)
		.child(TextLeaf::new().text(body))
}

fn build_ui(width: u16, ctx: UiContext) -> Ui {
	let root = Col::new()
		.child(
			TextLeaf::new()
				.with(Prop::Id, STATUS_ID)
				.with(Prop::Bold, true)
				.text("TOOLS · height 3 · motion on"),
		)
		.child(
			TextLeaf::new()
				.text("[ pressure  ] release  f finalize/hide browser  m reduced motion  q quit"),
		)
		.child(card(
			"read",
			"read",
			"crates/tui/src/components/tool_card.rs",
			"reading lines 225-390",
			"4.8K",
			"225 │ fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect)",
		))
		.child(card(
			"bash",
			"bash",
			"just test-pkg omp-tui",
			"running tool_card tests",
			"12s",
			"$ just test-pkg omp-tui\nrunning 214 tests",
		))
		.child(card(
			"edit",
			"edit",
			"tool_card.rs",
			"applying height-keyed paint",
			"+84",
			"@@ components/tool_card.rs\n+ paint from the granted rectangle",
		))
		.child(card(
			BROWSER_ID,
			"browser",
			"https://example.test/docs",
			"waiting for network idle",
			"200",
			"GET /docs\ncontent-type: text/html",
		))
		.child(card(
			"task",
			"task",
			"Wave 1 implementation",
			"SchedulerAgent running",
			"3/4",
			"RendererCore  done\nSchedulerAgent running",
		));
	Ui::from_root(root, width, ctx)
}

fn set_pressure(ui: &mut Ui, height: u16, browser_detached: bool) {
	for id in CARD_IDS {
		if browser_detached && *id == BROWSER_ID {
			continue;
		}
		ui.set_height(id, height);
	}
}

fn set_motion(ui: &mut Ui, reduced: bool) {
	let duration = if reduced { "0ms" } else { "180ms" };
	for id in CARD_IDS {
		ui.set_prop(id, Prop::Anim, duration);
	}
}

fn status(ui: &mut Ui, height: u16, reduced: bool, browser_hidden: bool) {
	let motion = if reduced { "reduced" } else { "on" };
	let browser = if browser_hidden {
		" · browser removed"
	} else {
		""
	};
	ui.set_text(STATUS_ID, format!("TOOLS · height {height} · motion {motion}{browser}"));
}

#[tokio::main]
async fn main() -> io::Result<()> {
	let hotkeys = [Key::Char('['), Key::Char(']'), Key::Char('f'), Key::Char('m')];
	let mut app = AppOptions::new()
		.quit([Key::Ctrl('c'), Key::Char('q'), Key::Esc])
		.hotkeys(hotkeys)
		.start(|env| build_ui(env.viewport.width, env.ctx))
		.await?;
	let mut height = 3_u16;
	let mut reduced = false;
	let mut browser_finalized = false;
	let mut browser_hidden = false;
	let mut browser_height = 3_u16;

	while let Some(event) = app.next().await? {
		if let AppEvent::Key(key) = event {
			match key {
				Key::Char('[') => {
					height = height.saturating_sub(1).max(1);
					set_pressure(app.ui_mut(), height, browser_finalized);
					if !browser_finalized {
						browser_height = height;
					}
				},
				Key::Char(']') => {
					height = height.saturating_add(1).min(3);
					set_pressure(app.ui_mut(), height, browser_finalized);
					if !browser_finalized {
						browser_height = height;
					}
				},
				Key::Char('f') if !browser_finalized => {
					browser_finalized = true;
					app.ui_mut()
						.update_component::<ToolCard>(BROWSER_ID, |card| {
							card.set_state(ToolState::Success)
						});
					if browser_height > 2 {
						browser_height = 2;
						app.ui_mut().set_height(BROWSER_ID, browser_height);
					}
				},
				Key::Char('f') if browser_height > 1 => {
					browser_height = 1;
					app.ui_mut().set_height(BROWSER_ID, browser_height);
				},
				Key::Char('f') if !browser_hidden => {
					browser_hidden = true;
					app.ui_mut()
						.set_prop(BROWSER_ID, Prop::Anim, if reduced { "0ms" } else { "100ms" });
					app.ui_mut().set_height(BROWSER_ID, 0);
				},
				Key::Char('m') => {
					reduced = !reduced;
					set_motion(app.ui_mut(), reduced);
				},
				_ => {},
			}
			status(app.ui_mut(), height, reduced, browser_hidden);
		}
	}
	Ok(())
}
