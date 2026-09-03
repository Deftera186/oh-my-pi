//! Dashboard and report slash commands (pi `builtin-session.ts`,
//! `builtin-collaboration.ts`, `builtin-lifecycle.ts`): `/usage`, `/stats`,
//! `/context`, `/trace`, `/changelog`, `/hotkeys`, `/debug`.
//!
//! `/usage` opens the full-screen dashboard; the report commands open a
//! [`ReportPanel`]; `/debug` opens the selector or one inspector. `/stats`
//! and `/trace` opened pi's local stats web dashboard, which has no seam on
//! this host, so they are registered for palette parity and answer with the
//! exact missing seam.

use omp_con::{ConError, ConResult, Ctx};
use omp_core::{Str, sf};
use omp_tui::Icon;

use super::{PaletteEntry, rest};
use crate::{
	actions::{HostAction, post},
	overlays::{
		PanelCall, PanelEvent, PanelOpener,
		info::{DebugSelector, changelog_report, context_report, debug_report, hotkeys_report},
		report::ReportPanel,
		services::ServiceError,
		usage::UsageDashboard,
	},
};

/// Palette icons for this module's commands.
pub const PALETTE: &[PaletteEntry] = &[
	PaletteEntry { name: "usage", icon: Icon::ChartBar },
	PaletteEntry { name: "stats", icon: Icon::Chart },
	PaletteEntry { name: "context", icon: Icon::Context },
	PaletteEntry { name: "trace", icon: Icon::Chart },
	PaletteEntry { name: "changelog", icon: Icon::Newspaper },
	PaletteEntry { name: "hotkeys", icon: Icon::Keyboard },
	PaletteEntry { name: "debug", icon: Icon::Bug },
];

const USAGE_USAGE: &str = "Usage: /usage [show|reset [account|active]]";
const DEFERRED_STATS: &str = "Stats dashboard is not available: the local stats web server \
                              (stats_cmd) was removed in d3d7c61fc4; use /usage";
const DEFERRED_TRACE: &str = "Trace viewer is not available: it opened the stats dashboard \
                              trace URL; the session replica does not carry the journal path";
const NO_CHANGELOG: &str = "No changelog entries found.";

fn usage(message: &'static str) -> ConError {
	ConError::Usage(Str::new_static(message))
}

fn open(ctx: &Ctx, opener: PanelOpener) -> ConResult<()> {
	post(ctx, HostAction::Open(opener))
}

fn call(ctx: &Ctx, call: PanelCall) -> ConResult<()> {
	post(ctx, HostAction::Call(call))
}

/// `/usage` subcommands (pi `parseSubcommand`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageOp {
	/// Open the dashboard.
	Show,
	/// Spend a saved rate-limit reset for `target` (empty lists them).
	Reset(Str),
}

/// Parses `/usage [show|reset [account|active]]`.
pub fn usage_op(words: Option<Str>) -> Result<UsageOp, ConError> {
	let Some(words) = words else {
		return Ok(UsageOp::Show);
	};
	let text = words.as_str().trim();
	let (verb, tail) = text
		.split_once(char::is_whitespace)
		.map_or((text, ""), |(verb, tail)| (verb, tail.trim()));
	match verb.to_ascii_lowercase().as_str() {
		"show" if tail.is_empty() => Ok(UsageOp::Show),
		"reset" => Ok(UsageOp::Reset(Str::new(tail))),
		_ => Err(usage(USAGE_USAGE)),
	}
}

omp_con::cmd! {
	/// Shows provider usage and limits: `/usage [show|reset [account|active]]`.
	usage(?op: Str, ?target: Str) = |ctx, args| match usage_op(rest(args, 0))? {
		UsageOp::Show => open(ctx, PanelOpener::new(|cx| {
			UsageDashboard::open(cx).map(|panel| Box::new(panel) as Box<_>)
		})),
		UsageOp::Reset(target) => call(ctx, PanelCall::new(move |cx| {
			match cx.services.reset_usage(&target) {
				Ok(line) => PanelEvent::Notice(line),
				Err(error) => PanelEvent::Notice(Str::new(error.to_string())),
			}
		})),
	};

	/// Launches the local stats dashboard.
	stats() = |ctx, _args| {
		call(ctx, PanelCall::new(|_cx| PanelEvent::Notice(Str::new_static(DEFERRED_STATS))))
	};

	/// Shows the estimated context usage breakdown.
	context() = |ctx, _args| open(ctx, PanelOpener::new(|cx| {
		let body = context_report(cx.dom, cx.con);
		Ok(Box::new(ReportPanel::new("context", "Context", body, cx.ui)) as Box<_>)
	}));

	/// Opens this session's trace in the stats dashboard.
	trace() = |ctx, _args| {
		call(ctx, PanelCall::new(|_cx| PanelEvent::Notice(Str::new_static(DEFERRED_TRACE))))
	};

	/// Shows changelog entries: `/changelog [full]`.
	changelog(?full: Str) = |ctx, args| {
		let full = rest(args, 0).is_some_and(|words| {
			words.as_str().split_whitespace().any(|word| word.eq_ignore_ascii_case("full"))
		});
		// An opener's `Err` is the host's status notice, so an empty or
		// unavailable changelog reads exactly as pi's one-liner.
		open(ctx, PanelOpener::new(move |cx| {
			let text = match cx.services.changelog() {
				Ok(text) => text,
				Err(ServiceError::Unavailable(_)) => return Err(Str::new_static(NO_CHANGELOG)),
				Err(error) => return Err(Str::new(error.to_string())),
			};
			let body = changelog_report(&text, full).ok_or_else(|| Str::new_static(NO_CHANGELOG))?;
			let title = if full { "Changelog" } else { "Changelog · recent" };
			Ok(Box::new(ReportPanel::new("changelog", title, body, cx.ui)) as Box<_>)
		}))
	};

	/// Shows all keyboard shortcuts.
	hotkeys() = |ctx, _args| open(ctx, PanelOpener::new(|cx| {
		Ok(Box::new(ReportPanel::new("hotkeys", "Hotkeys", hotkeys_report(cx.con), cx.ui)) as Box<_>)
	}));

	/// Opens the debug tools selector: `/debug [paths|system|values]`.
	debug(?inspector: Str) = |ctx, args| match rest(args, 0) {
		None => open(ctx, PanelOpener::new(|cx| {
			Ok(Box::new(DebugSelector::open(cx.ui, cx.viewport.width)) as Box<_>)
		})),
		Some(key) => open(ctx, PanelOpener::new(move |cx| {
			let body = debug_report(cx, key.as_str().trim())?;
			let title = sf!("Debug · {}", key.as_str().trim());
			Ok(Box::new(ReportPanel::new("debug", title, body, cx.ui)) as Box<_>)
		})),
	};
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn usage_words_parse_show_and_reset() {
		assert_eq!(usage_op(None).unwrap(), UsageOp::Show);
		assert_eq!(usage_op(Some(sf!("show"))).unwrap(), UsageOp::Show);
		assert_eq!(usage_op(Some(sf!("reset"))).unwrap(), UsageOp::Reset(Str::default()));
		assert_eq!(usage_op(Some(sf!("reset active"))).unwrap(), UsageOp::Reset(sf!("active")));
		assert_eq!(
			usage_op(Some(sf!("Reset me@example.com"))).unwrap(),
			UsageOp::Reset(sf!("me@example.com"))
		);
		assert!(matches!(usage_op(Some(sf!("show extra"))), Err(ConError::Usage(_))));
		assert!(matches!(usage_op(Some(sf!("bogus"))), Err(ConError::Usage(_))));
	}
}
