//! Interactive rendering gallery: every showcase scene as one tabbed app.
//!
//! ```sh
//! cargo run -p omp-tui --example gallery
//! ```
//!
//! Tab/Shift-Tab moves focus, ←/→ switches tabs on the tab bar, ↑/↓ and
//! PageUp/PageDown scroll the active pane, and the Live tab re-renders the
//! preview as you type. The `Anim` tab autoplays its prop tweens and takes
//! scene keys, the `Overlay` tab opens modal layers (`Ctrl+K`/`Ctrl+G`),
//! and `Eclipse` runs the fullscreen shader. Ctrl-C or Ctrl-Q quits.

mod anim;
mod eclipse;
mod overlay;
mod render;

use std::{io, time::Instant};

use omp_tui::{AppEvent, AppOptions, Key, OverlayId, Size, Ui, UiContext, dom};

fn build_ui(viewport: Size, context: UiContext) -> Ui {
	// keep panes inside the viewport so switching tabs never strands
	// stale rows in scrollback
	let pane_height = render::pane_height(viewport);

	Ui::from_root(
		dom! {
			<col gap=1>
				<tabs id=view>
					<tab title="Markdown">
						<scroll id="pane-md" h={pane_height}>
							<md>{render::MARKDOWN_TAB}</md>
						</scroll>
					</tab>
					<tab title="Math">
						<scroll id="pane-math" h={pane_height}>
							<md>{render::MATH_TAB}</md>
						</scroll>
					</tab>
					<tab title="Mermaid">
						<scroll id="pane-mermaid" h={pane_height}>
							<md>{render::MERMAID_TAB}</md>
						</scroll>
					</tab>
					<tab title="Graphviz">
						<scroll id="pane-graphviz" h={pane_height}>
							<md>{render::GRAPHVIZ_TAB}</md>
						</scroll>
					</tab>
					<tab title="Macro">
						<box border=round title="Built with dom!">
							<col gap=1>
								<row gap=1>
									<i:info/>
									<text bold>{"Macro-built pane"}</text>
								</row>
								<gallery-note>
									<text dim>{format!("Interpolated at runtime: {} nested layout levels", 3)}</text>
								</gallery-note>
							</col>
						</box>
					</tab>
					<tab title="Live">
						<col gap=1>
							<editor id=src value={render::LIVE_PREFILL}/>
							<box border=round title="Preview">
								<md id=preview>{"..."}</md>
							</box>
						</col>
					</tab>
					<tab title="Anim">{anim::pane()}</tab>
					<tab title="Overlay">{overlay::pane()}</tab>
					<tab title="Eclipse">{eclipse::pane(viewport, pane_height)}</tab>
				</tabs>
				<text dim>{"Tab focus · ←/→ switch tabs · ↑/↓ PgUp/PgDn scroll · Ctrl-C quit"}</text>
			</col>
		},
		viewport.width,
		context,
	)
}

/// Title of the active tab, from the tabs component's reported value.
fn active_tab(ui: &Ui) -> String {
	ui.values()["view"].as_str().unwrap_or_default().to_owned()
}

/// Layers opened from the Overlay tab.
#[derive(Default)]
struct Layers {
	picker: Option<OverlayId>,
	help:   Option<OverlayId>,
}

#[tokio::main]
async fn main() -> io::Result<()> {
	let mut app = AppOptions::new()
		.mouse()
		.quit([Key::Ctrl('c'), Key::Ctrl('q')])
		.start(|env| build_ui(env.viewport, env.ctx))
		.await?;

	let mut synced = String::new();
	let mut lab = anim::Lab::new();
	let mut layers = Layers::default();
	let mut next_step = Instant::now() + anim::AUTOPLAY_STEP;

	loop {
		let event = tokio::select! {
			event = app.next() => match event? {
				Some(event) => event,
				None => break,
			},
			() = tokio::time::sleep_until(next_step.into()) => {
				if lab.autoplay && active_tab(app.ui()) == "Anim" {
					lab.advance(app.ui_mut());
				}
				next_step += anim::AUTOPLAY_STEP;
				continue;
			},
		};
		match event {
			AppEvent::Resized(viewport) => {
				for pane in render::PANE_IDS {
					app.ui_mut().set_height(pane, render::pane_height(viewport));
				}
			},
			AppEvent::Key(key) => match active_tab(app.ui()).as_str() {
				"Anim" => lab.handle_key(key, app.ui_mut()),
				"Overlay" => match key {
					Key::Ctrl('k') if layers.picker.is_none() => {
						layers.picker = Some(overlay::show_picker(app.ui_mut()));
					},
					Key::Ctrl('g') => match layers.help.take() {
						Some(id) => {
							app.ui_mut().close_overlay(id);
						},
						None => layers.help = Some(overlay::show_help(app.ui_mut())),
					},
					_ => {},
				},
				_ => {},
			},
			// The Overlay tab's modal select committed a model.
			AppEvent::Changed { id, value } if id == "model" => {
				if let Some(overlay) = layers.picker.take() {
					let label = overlay::MODELS
						.iter()
						.find(|(short, ..)| *short == value)
						.map_or(value.as_str(), |(_, label, _)| label);
					app.ui_mut().set_text("status", format!("model: {label}"));
					app.ui_mut().close_overlay(overlay);
				}
			},
			AppEvent::OverlayClosed(id) => {
				if layers.picker == Some(id) {
					layers.picker = None;
				}
				if layers.help == Some(id) {
					layers.help = None;
				}
			},
			_ => {},
		}
		render::sync_preview(app.ui_mut(), &mut synced);
		// Reserve the Overlay tab's chords only while it is showing, so the
		// focused composer can't spend Ctrl+K on kill-line — and the Live
		// tab's editor keeps it.
		let chords: &[Key] = if active_tab(app.ui()) == "Overlay" {
			&[Key::Ctrl('k'), Key::Ctrl('g')]
		} else {
			&[]
		};
		app.set_hotkeys(chords.iter().copied());
	}
	Ok(())
}
