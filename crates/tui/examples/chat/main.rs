//! Thin terminal host for the designed chat scene with a scripted backend.

mod mock;

use std::io;

use omp_chat_ui::Chat;
use omp_tui::{UiContext, detect};

#[tokio::main]
async fn main() -> io::Result<()> {
	let caps = detect();
	let ctx = UiContext::default().with_terminal_caps(&caps);
	let chat = Chat::new(&ctx);
	let (events, intents) = mock::start();
	omp_chat_ui::host::run(chat, ctx, events, intents).await
}
