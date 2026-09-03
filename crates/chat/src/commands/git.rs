//! Git workbench and transcript copy commands (pi `builtin-session.ts`
//! `/git`, `builtin-collaboration.ts` `/copy`). `/git` opens the
//! fullscreen workbench over the session's project root; `/copy` opens the
//! transcript picker, or with `code`/`cmd` copies the last fenced block or
//! shell command straight from the replica through a host call.

use omp_con::ConError;
use omp_core::Str;
use omp_tui::Icon;

use super::{PaletteEntry, rest};
use crate::{
	actions::{HostAction, post},
	overlays::{
		Panel, PanelCall, PanelEvent, PanelOpener,
		copy::{CopySelector, last_code_block, last_command},
		git::GitWorkbench,
	},
};

/// Palette icons for this module's commands.
pub const PALETTE: &[PaletteEntry] = &[
	PaletteEntry { name: "git", icon: Icon::Branch },
	PaletteEntry { name: "copy", icon: Icon::Copy },
];

/// `/copy` argument forms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyOp {
	/// Open the transcript picker.
	Picker,
	/// Copy the last fenced code block.
	Code,
	/// Copy the last `bash`/`eval` command.
	Command,
}

/// Parses `/copy [code|cmd]`; pi rejects anything else with its usage line.
pub fn copy_op(words: Option<Str>) -> Result<CopyOp, ConError> {
	let arg = words.unwrap_or_default();
	let arg = arg.as_str().trim().to_ascii_lowercase();
	Ok(match arg.as_str() {
		"" => CopyOp::Picker,
		"code" => CopyOp::Code,
		"cmd" | "command" => CopyOp::Command,
		_ => return Err(ConError::Usage(Str::new_static("Usage: /copy [code|cmd]"))),
	})
}

omp_con::cmd! {
	/// Opens the git UI (split diff viewer, staging, commit composer); a revision pins the view to that commit.
	git(?revision: Str) = |ctx, args| {
		let revision = rest(args, 0);
		post(ctx, HostAction::Open(PanelOpener::new(move |cx| {
			GitWorkbench::open(cx, revision.clone()).map(|panel| Box::new(panel) as Box<dyn Panel>)
		})))
	};

	/// Picks text or code from the conversation to copy: `/copy [code|cmd]`.
	copy(?what: Str) = |ctx, args| {
		match copy_op(rest(args, 0))? {
			CopyOp::Picker => post(ctx, HostAction::Open(PanelOpener::new(|cx| {
				let show_thinking = omp_con::CL_SHOWTHINKING.try_get(cx.con).unwrap_or(true);
				let panel = CopySelector::open(cx.dom, show_thinking, cx.ui);
				if panel.target_count() == 0 {
					return Err(Str::new_static("Nothing to copy yet."));
				}
				Ok(Box::new(panel) as Box<dyn Panel>)
			}))),
			CopyOp::Code => post(ctx, HostAction::Call(PanelCall::new(|cx| {
				last_code_block(cx.dom).map_or_else(
					|| PanelEvent::Notice(Str::new_static("No code block to copy.")),
					|block| PanelEvent::Copy(block.content),
				)
			}))),
			CopyOp::Command => post(ctx, HostAction::Call(PanelCall::new(|cx| {
				last_command(cx.dom).map_or_else(
					|| PanelEvent::Notice(Str::new_static("No command to copy.")),
					|(_, code)| PanelEvent::Copy(code),
				)
			}))),
		}
	};
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn copy_words_select_the_picker_code_or_command() {
		assert_eq!(copy_op(None).unwrap(), CopyOp::Picker);
		assert_eq!(copy_op(Some(Str::new_static("code"))).unwrap(), CopyOp::Code);
		assert_eq!(copy_op(Some(Str::new_static("CMD"))).unwrap(), CopyOp::Command);
		assert_eq!(copy_op(Some(Str::new_static("command"))).unwrap(), CopyOp::Command);
		let error = copy_op(Some(Str::new_static("all"))).unwrap_err();
		assert!(error.to_string().contains("Usage: /copy [code|cmd]"), "{error}");
	}
}
