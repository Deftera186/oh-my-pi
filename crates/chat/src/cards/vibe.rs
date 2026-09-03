//! Typed card for the five persistent vibe worker controls.

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{Border, IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{Card, CardStatus, CardView, Component};

#[derive(Clone, Copy)]
enum VibeOp {
	Spawn,
	Send,
	Wait,
	Kill,
	List,
}

impl VibeOp {
	fn from_view(view: &CardView<'_>) -> Self {
		let args = view.args_json();
		let op = args
			.as_ref()
			.and_then(|value| value.get("op"))
			.and_then(Value::as_str)
			.or_else(|| partial_string(view.args_text().unwrap_or_default(), "op"));
		match op {
			Some("spawn") => Self::Spawn,
			Some("send") => Self::Send,
			Some("wait") => Self::Wait,
			Some("kill") => Self::Kill,
			_ => Self::List,
		}
	}

	const fn verb(self) -> &'static str {
		match self {
			Self::Spawn => "spawn",
			Self::Send => "send",
			Self::Wait => "wait",
			Self::Kill => "kill",
			Self::List => "list",
		}
	}

	fn title(self, ui: &UiContext) -> Str {
		match self {
			Self::Spawn => Str::new_static("vibe spawn ?"),
			Self::Send => {
				sf!("vibe send {} ?", ui.charset.icon_named("vibe-send-arrow").unwrap_or("->"))
			},
			Self::Wait => Str::new_static("vibe wait on running sessions"),
			Self::Kill => Str::new_static("vibe kill ?"),
			Self::List => Str::new_static("vibe sessions"),
		}
	}
}

/// Shared card for the `vibe_*` tool identities normalized to `vibe` by the
/// host.
pub struct VibeCard;

impl Card for VibeCard {
	fn tool(&self) -> &'static str {
		"vibe"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		let op = VibeOp::from_view(view);
		let title = op.title(ui);
		match view.status {
			CardStatus::StreamingArgs | CardStatus::InProgress
				if matches!(op, VibeOp::Spawn | VibeOp::Send) =>
			{
				let (top_left, _, bottom_left, _, horizontal, vertical) =
					ui.charset.border(Border::Round);
				let top = sf!("{top_left}{horizontal} {title}");
				let middle = sf!("{vertical} >");
				let bottom = sf!(
					"{bottom_left}{horizontal} {}",
					if matches!(op, VibeOp::Spawn) {
						"booting CLI…"
					} else {
						"delivering…"
					}
				);
				dom! {
					<col>
						<text>{top}</text>
						<row gap=1><text>{middle}</text> if !expanded { <i:stream-cursor/> }</row>
						<text>{bottom}</text>
					</col>
				}
				.into_component()
			},
			CardStatus::StreamingArgs | CardStatus::InProgress => dom! {
				<row gap=1><i:pending/><text>{title}</text></row>
			}
			.into_component(),
			CardStatus::Done => {
				let completed = sf!("vibe_{} completed", op.verb());
				dom! {
					<col>
						<row gap=1><i:done/><text>{title}</text></row>
						<text pad-x=2 fg=muted>{completed}</text>
					</col>
				}
				.into_component()
			},
			CardStatus::Failed => {
				let fault = diag_text(view.diag).unwrap_or_else(|| Str::new_static("operation failed"));
				dom! {
					<col>
						<row gap=1><i:error/><text>{title}</text></row>
						<row pad-x=2 gap=1><text fg=err>{"Error:"}</text><text fg=err>{fault}</text></row>
					</col>
				}
				.into_component()
			},
		}
	}
}

fn partial_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
	let marker = sf!("\"{key}\":\"");
	let start = json.find(marker.as_str())? + marker.len();
	let rest = &json[start..];
	Some(rest.split('"').next().unwrap_or(rest))
}

fn diag_text(node: Option<&Node>) -> Option<Str> {
	node.and_then(|node| {
		node.content.clone().or_else(|| {
			node
				.prop(&PropId::Text.into())
				.and_then(|value| value.as_str())
				.map(Str::new)
		})
	})
}
