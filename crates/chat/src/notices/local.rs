//! Observer-local message blocks: host notices that never enter the session
//! journal (pi `hook-message.ts` / `message-frame.ts` for the framed
//! Markdown box; the plain `info | warn | error` kinds reuse the transcript
//! notice card).

use omp_core::{Str, sf};
use omp_tui::{IntoComponent as _, UiContext, dom};

use super::error::notice_card;
use crate::cards::Component;

/// Presentation class of one observer-local block.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "kebab-case")]
pub enum LocalBlockKind {
	/// Plain informational notice.
	Info,
	/// Cautionary notice.
	Warn,
	/// Error notice (capped through the inline error block).
	Error,
	/// Framed Markdown body without a hook identity.
	Markdown,
	/// Hook-authored message: framed Markdown with the hook icon and name.
	Hook,
}

/// Renders one observer-local block.
///
/// `Hook` and `Markdown` follow pi `renderFramedMessage`: a rounded,
/// muted-bordered box on the `surface` tint with one cell of padding, an
/// optional bold `[title]` header row (led by the hook icon for `Hook`)
/// followed by a blank row, and the Markdown body. `Info`, `Warn`, and
/// `Error` reuse [`notice_card`], with a title folded in as the first line.
#[must_use]
pub fn render_local_block(
	kind: LocalBlockKind,
	title: Option<Str>,
	body: Str,
	_ui: &UiContext,
) -> Component {
	let notice = match kind {
		LocalBlockKind::Info => "info",
		LocalBlockKind::Warn => "warn",
		LocalBlockKind::Error => "error",
		LocalBlockKind::Markdown | LocalBlockKind::Hook => {
			let header = title.map(|title| sf!("[{title}]"));
			let hook = kind == LocalBlockKind::Hook;
			return dom! {
				<box border=round bc=border bg=surface pad="1 1">
					if let Some(header) = header {
						<row gap=1>
							if hook { <icon name="hook" fg=accent/> }
							<text bold fg=accent>{header}</text>
						</row>
						<spacer/>
					}
					<md>{body}</md>
				</box>
			}
			.into_component();
		},
	};
	let text = match title {
		Some(title) => sf!("{title}\n{body}"),
		None => body,
	};
	notice_card(notice, text, false)
}

#[cfg(test)]
mod tests {
	use omp_tui::{Ui, frame_text};

	use super::*;

	fn rows(component: Component, width: u16) -> Vec<String> {
		let ui = Ui::from_root(component, width, UiContext::default());
		frame_text(ui.frame())
			.lines()
			.map(|row| row.trim_end().to_owned())
			.collect()
	}

	#[test]
	fn hook_box_renders_title_and_markdown() {
		let block = render_local_block(
			LocalBlockKind::Hook,
			Some(Str::new_static("pre-commit")),
			Str::new_static("Ran **3** checks\n\n- lint ok"),
			&UiContext::default(),
		);
		let rows = rows(block, 40);
		assert!(rows[0].starts_with('╭') && rows[0].ends_with('╮'), "{rows:?}");
		assert!(rows.last().is_some_and(|row| row.starts_with('╰')), "{rows:?}");
		let header = rows
			.iter()
			.position(|row| row.contains("[pre-commit]"))
			.expect("title row");
		assert!(rows[header].contains(omp_tui::Charset::default().icon(omp_tui::Icon::Hook)));
		assert!(rows[header + 1].trim_matches(|c| c == '│' || c == ' ').is_empty(), "blank row after title");
		let body = rows.iter().position(|row| row.contains("Ran 3 checks")).expect("markdown body");
		assert!(body > header, "{rows:?}");
		assert!(rows.iter().any(|row| row.contains("lint ok")), "{rows:?}");
		assert_eq!(LocalBlockKind::Hook.to_string(), "hook");
		assert_eq!("markdown".parse::<LocalBlockKind>(), Ok(LocalBlockKind::Markdown));
	}

	#[test]
	fn plain_kinds_reuse_the_notice_card() {
		let rows = rows(
			render_local_block(
				LocalBlockKind::Warn,
				Some(Str::new_static("Heads up")),
				Str::new_static("disk is nearly full"),
				&UiContext::default(),
			),
			40,
		);
		assert!(rows.iter().any(|row| row.contains("Heads up")), "{rows:?}");
		assert!(rows.iter().any(|row| row.contains("disk is nearly full")), "{rows:?}");
	}
}
