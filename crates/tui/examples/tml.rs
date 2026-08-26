//! Renders a runtime-markup document and live-reloads it on save.
//!
//! ```sh
//! cargo run -p omp-tui --example tml -- example.tml
//! ```
//!
//! Edit the file in another window; the screen repaints on every save. A
//! parse error shows as an error card until the next good save. Quit with
//! `q`, Escape, or Ctrl-C.

use std::{env, fs, io, path::Path, time, time::Duration};

use omp_tui::{AppOptions, Key, Ui, UiContext, dom};

/// Parses `source`, degrading a [`omp_tui::ParseError`] to an error card so a
/// mid-edit typo never kills the session.
fn build(source: &str, width: u16, ctx: &UiContext) -> Ui {
	match Ui::from_markup(source, width, ctx.clone()) {
		Ok(ui) => ui,
		Err(error) => {
			let message = error.to_string();
			Ui::from_root(
				dom! {
					<box border=round bc=err title="parse error" pad="0 1">
						<text fg=err>{message}</text>
					</box>
				},
				width,
				ctx.clone(),
			)
		},
	}
}

fn modified(path: &Path) -> Option<time::SystemTime> {
	fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

#[tokio::main]
async fn main() -> io::Result<()> {
	let path = env::args().nth(1).unwrap_or_else(|| "example.tml".into());
	let source = fs::read_to_string(&path)?;

	let mut ctx = None;
	let mut app = AppOptions::new()
		.quit([Key::Ctrl('c'), Key::Char('q'), Key::Esc])
		.start(|env| {
			let ui = build(&source, env.viewport.width, &env.ctx);
			ctx = Some(env.ctx);
			ui
		})
		.await?;
	let ctx = ctx.expect("start ran the builder");

	let handle = app.handle();
	tokio::spawn(async move {
		let mut seen = modified(path.as_ref());
		loop {
			tokio::time::sleep(Duration::from_millis(150)).await;
			let stamp = modified(path.as_ref());
			if stamp == seen {
				continue;
			}
			seen = stamp;
			let Ok(source) = fs::read_to_string(&path) else {
				continue;
			};
			let ctx = ctx.clone();
			handle.update(move |ui| *ui = build(&source, ui.frame().size().width, &ctx));
		}
	});

	while app.next().await?.is_some() {}
	Ok(())
}
