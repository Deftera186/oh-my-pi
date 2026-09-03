//! `/debug` tools selector (pi `showDebugSelector`) plus the pure report
//! builders behind `/context`, `/hotkeys`, `/changelog`, and the `/debug
//! <key>` inspectors. Every report is markdown for a
//! [`ReportPanel`](super::report::ReportPanel); nothing here touches the
//! session DOM beyond reading the replica.

use std::fmt::Write as _;

use omp_con::{AI_COMPACT_THRESHOLD, AI_MODEL, CL_CHARSET, CL_THEME, Ctx, DumpOptions};
use omp_core::{Str, sf};
use omp_dom::{Dom, PropKey, Value};
use omp_tui::{Frame, Key, Prop, Size, Ui, UiContext, UiEvent, dom};

use super::{Panel, PanelAnchor, PanelCx, PanelEvent};
use crate::status_line::StatusLine;

const DEBUG_HINT: &str = "↑/↓ choose · Enter select · Esc close";
/// Border, rule, hint, and blank rows around the select list.
const DEBUG_CHROME_ROWS: u16 = 5;
/// `## ` sections shown by `/changelog` without `full` (pi
/// `RECENT_CHANGELOG_ENTRY_LIMIT`).
const RECENT_CHANGELOG_ENTRIES: usize = 3;

/// One `/debug` inspector: stable key, label, and consequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DebugAction {
	/// Word passed back as `debug <key>`.
	pub key:         &'static str,
	/// Compact label.
	pub label:       &'static str,
	/// Consequence description.
	pub description: &'static str,
}

/// Inspectors offered by the selector, in menu order.
pub const DEBUG_ACTIONS: [DebugAction; 3] = [
	DebugAction { key: "paths", label: "paths", description: "Session and data paths" },
	DebugAction { key: "system", label: "system", description: "Process and terminal facts" },
	DebugAction { key: "values", label: "values", description: "Console variables (dump)" },
];

/// Retained `/debug` selector; Enter finishes with `debug <key>`.
pub struct DebugSelector {
	ui:    Ui,
	ctx:   UiContext,
	width: u16,
	rows:  u16,
}

impl DebugSelector {
	/// Opens the selector for a viewport width.
	#[must_use]
	pub fn open(ctx: &UiContext, width: u16) -> Self {
		let mut panel = Self { ui: Ui::from_root(dom! { <col/> }, width, ctx.clone()), ctx: ctx.clone(), width, rows: 0 };
		panel.rebuild(width, u16::try_from(DEBUG_ACTIONS.len()).unwrap_or(u16::MAX));
		panel
	}

	fn rebuild(&mut self, width: u16, rows: u16) {
		self.width = width;
		self.rows = rows;
		let height = rows.saturating_add(1);
		let tree = dom! {
			<box border=round title="Debug tools" pad-x=1>
				<col>
					<select id="actions" h={height}>
						for action in DEBUG_ACTIONS {
							<option value={action.key} label={action.label}>
								<td><pre bold>{action.label}</pre></td>
								<td truncate grow><pre fg=muted>{action.description}</pre></td>
							</option>
						}
					</select>
					<hr border=round/>
					<text fg=muted truncate>{DEBUG_HINT}</text>
				</col>
			</box>
		};
		self.ui = Ui::from_root(tree, width, self.ctx.clone());
	}
}

impl Panel for DebugSelector {
	fn id(&self) -> &'static str {
		"debug"
	}

	fn anchor(&self) -> PanelAnchor {
		PanelAnchor::Center
	}

	fn key(&mut self, key: Key) -> PanelEvent {
		if key == Key::Esc {
			return PanelEvent::Close;
		}
		match self.ui.handle_key(key) {
			UiEvent::Cancel => PanelEvent::Close,
			UiEvent::Changed { id, value } if id.as_str() == "actions" => {
				PanelEvent::Finish(sf!("debug {}", value.as_str()))
			},
			_ => PanelEvent::Consumed,
		}
	}

	fn frame(&mut self, viewport: Size) -> &Frame {
		let rows = viewport
			.height
			.saturating_sub(DEBUG_CHROME_ROWS)
			.clamp(1, u16::try_from(DEBUG_ACTIONS.len()).unwrap_or(u16::MAX));
		if viewport.width != self.width {
			self.rebuild(viewport.width, rows);
		} else if rows != self.rows {
			self.rows = rows;
			self.ui.set_prop("actions", Prop::H, rows.saturating_add(1));
		}
		self.ui.frame()
	}
}

/// Reads one `prompt-facts` value off `<meta>`; `inner` empty reads the
/// outer key itself.
fn prompt_fact(dom: &Dom, outer: &str, inner: &str) -> Option<Str> {
	let value = dom
		.get(dom.meta())?
		.prop(&PropKey::Custom(Str::new_static("prompt-facts")))?;
	let Value::Json(raw) = value else {
		return None;
	};
	let value: serde_json::Value = serde_json::from_str(raw.get()).ok()?;
	let selected = value.get(outer)?;
	let text = if inner.is_empty() {
		selected.as_str()?
	} else {
		selected.get(inner)?.as_str()?
	};
	Some(Str::new(text))
}

/// `/context`: estimated context usage from the replica's receipts.
#[must_use]
pub fn context_report(dom: &Dom, con: &Ctx) -> Str {
	let status = StatusLine::from_dom(dom);
	let live = AI_MODEL.get(con);
	let model = if live.is_empty() {
		status.model.clone()
	} else {
		live
	};
	let threshold = AI_COMPACT_THRESHOLD.get(con);
	let mut out = String::with_capacity(320);
	let _ = writeln!(out, "**Context · turn {}**\n", status.turns);
	let _ = writeln!(
		out,
		"- Model: {}",
		if model.is_empty() {
			"unknown"
		} else {
			model.as_str()
		}
	);
	let _ = writeln!(out, "- Input: {} tokens", status.context);
	let _ = writeln!(out, "- Window: unknown");
	let _ = writeln!(out, "- Compaction threshold: {}%", (threshold * 100.0).round());
	let _ = writeln!(out, "\n**Session totals**\n");
	let _ = writeln!(out, "- Turns: {}", status.turns);
	let _ = writeln!(out, "- Input: {} tokens", status.tokens_in);
	let _ = writeln!(out, "- Output: {} tokens", status.tokens_out);
	let _ = writeln!(out, "- Cache read: {} tokens", status.cache_read);
	let _ = writeln!(out, "- Cache write: {} tokens", status.cache_write);
	let _ = writeln!(
		out,
		"- Cost: ${}.{:04}",
		status.cost_nano_usd / 1_000_000_000,
		(status.cost_nano_usd % 1_000_000_000) / 100_000
	);
	if let Some(tps) = status.tokens_per_second {
		let _ = writeln!(out, "- Throughput: {tps:.1} tok/s");
	}
	Str::from(out)
}

/// `/hotkeys`: the fixed editor keys (pi `hotkeys-markdown.ts`) plus every
/// console bind, sorted by key.
#[must_use]
pub fn hotkeys_report(con: &Ctx) -> Str {
	let mac = cfg!(target_os = "macos");
	let alt = if mac { "Option" } else { "Alt" };
	let mut out = String::with_capacity(2048);
	out.push_str("**Navigation**\n| Key | Action |\n|-----|--------|\n");
	out.push_str("| `Arrow keys` | Move cursor / browse history (Up when empty) |\n");
	let _ = writeln!(out, "| `{alt}+Left/Right` | Move by word |");
	if mac {
		out.push_str("| `Ctrl+A` / `Home` / `Cmd+Left` | Start of line |\n");
		out.push_str("| `Ctrl+E` / `End` / `Cmd+Right` | End of line |\n");
	} else {
		out.push_str("| `Ctrl+A` / `Home` | Start of line |\n");
		out.push_str("| `Ctrl+E` / `End` | End of line |\n");
	}
	out.push_str("\n**Editing**\n| Key | Action |\n|-----|--------|\n");
	out.push_str("| `Enter` | Send message |\n");
	let _ = writeln!(out, "| `Shift+Enter` / `{alt}+Enter` | New line |");
	let _ = writeln!(out, "| `Ctrl+W` / `{alt}+Backspace` | Delete word backwards |");
	out.push_str("| `Ctrl+U` | Delete to start of line |\n");
	out.push_str("| `Ctrl+K` | Delete to end of line |\n");
	out.push_str("\n**Other**\n| Key | Action |\n|-----|--------|\n");
	out.push_str("| `Tab` | Path completion / accept autocomplete |\n");
	out.push_str("| `#<number>` | GitHub issue/PR reference (e.g. `#3164` → `pr://`/`issue://`) |\n");
	out.push_str("| `/` | Slash commands |\n");
	out.push_str("| `!` | Run bash command |\n");
	out.push_str("| `!!` | Run bash command (excluded from context) |\n");
	out.push_str("| `$` | Run Python in shared kernel |\n");
	out.push_str("| `$$` | Run Python (excluded from context) |\n");
	let binds = con.binds();
	out.push_str("\n**Bindings**\n");
	if binds.is_empty() {
		out.push_str("No keys are bound.\n");
	} else {
		out.push_str("| Key | Script |\n|-----|--------|\n");
		for (key, script) in binds {
			let _ = writeln!(out, "| `{key}` | `{}` |", script.replace('|', "\\|"));
		}
	}
	Str::from(out)
}

/// `/changelog [full]`: the first [`RECENT_CHANGELOG_ENTRIES`] `## `
/// sections unless `full`; `None` when the text has no entries.
#[must_use]
pub fn changelog_report(text: &str, full: bool) -> Option<Str> {
	let limit = if full {
		usize::MAX
	} else {
		RECENT_CHANGELOG_ENTRIES
	};
	let mut rendered = String::new();
	let mut shown = 0;
	for (index, entry) in text.split("\n## ").enumerate() {
		let entry = entry.trim();
		if entry.is_empty() || (index == 0 && !entry.starts_with("## ")) {
			continue;
		}
		if shown == limit {
			break;
		}
		if !rendered.is_empty() {
			rendered.push_str("\n\n");
		}
		if index > 0 {
			rendered.push_str("## ");
		}
		rendered.push_str(entry);
		shown += 1;
	}
	(!rendered.is_empty()).then(|| Str::from(rendered))
}

/// `/debug <key>` inspector report.
#[must_use]
pub fn debug_report(cx: &PanelCx<'_>, key: &str) -> Result<Str, Str> {
	match key {
		"paths" => Ok(paths_report(cx.dom)),
		"system" => Ok(system_report(cx)),
		"values" => Ok(sf!(
			"## Console values\n\n```\n{}\n```",
			cx.con.dump_with_options(DumpOptions { all_vars: true, include_defaults: true })
		)),
		other => Err(sf!("unknown debug inspector `{other}`; expected paths, system, or values")),
	}
}

fn paths_report(dom: &Dom) -> Str {
	let cwd = prompt_fact(dom, "cwd", "");
	let home = prompt_fact(dom, "home", "");
	let data = omp_core::dirs::data_dir(None)
		.map(|path| path.display().to_string())
		.unwrap_or_else(|error| format!("unavailable ({error})"));
	let process_cwd = std::env::current_dir()
		.map(|path| path.display().to_string())
		.unwrap_or_else(|error| format!("unavailable ({error})"));
	sf!(
		"## Session debug paths\n\n- Session cwd: `{}`\n- Process cwd: `{process_cwd}`\n- Home: \
		 `{}`\n- Data directory: `{data}`\n- Journal: unavailable (the session replica does not \
		 carry its journal path)",
		cwd.as_deref().unwrap_or("unknown"),
		home.as_deref().unwrap_or("unknown"),
	)
}

fn system_report(cx: &PanelCx<'_>) -> Str {
	sf!(
		"## System information\n\n- PID: {}\n- OS: {} ({})\n- Arch: {}\n- Viewport: {}×{}\n- \
		 Charset: {:?} (`cl_charset` = {})\n- Appearance: {:?} (`cl_theme` = {})\n- Graphics: {:?}",
		std::process::id(),
		std::env::consts::OS,
		std::env::consts::FAMILY,
		std::env::consts::ARCH,
		cx.viewport.width,
		cx.viewport.height,
		cx.ui.charset,
		CL_CHARSET.get(cx.con),
		cx.ui.appearance,
		CL_THEME.get(cx.con),
		cx.ui.graphics,
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn debug_selector_enter_finishes_with_the_chosen_key() {
		let ctx = UiContext::default();
		let mut panel = DebugSelector::open(&ctx, 60);
		let text = omp_tui::frame_text(panel.frame(Size { width: 60, height: 12 }));
		assert!(text.contains("Debug tools"), "title missing:\n{text}");
		assert!(text.contains("Session and data paths"), "row missing:\n{text}");
		assert!(text.contains(DEBUG_HINT), "hint missing:\n{text}");
		assert_eq!(panel.key(Key::Enter), PanelEvent::Finish(sf!("debug paths")));
		assert_eq!(panel.key(Key::Down), PanelEvent::Consumed);
		assert_eq!(panel.key(Key::Enter), PanelEvent::Finish(sf!("debug system")));
		assert_eq!(panel.key(Key::Esc), PanelEvent::Close);
	}

	#[test]
	fn hotkeys_report_lists_console_binds_sorted_by_key() {
		let con = Ctx::new();
		con.bind("alt+p", "cl_models").unwrap();
		con.bind("alt+a", "cl_agents").unwrap();
		let report = hotkeys_report(&con);
		assert!(report.contains("| `alt+p` | `cl_models` |"), "{report}");
		let a = report.find("`alt+a`").unwrap();
		let p = report.find("`alt+p`").unwrap();
		assert!(a < p, "binds sorted by key:\n{report}");
		assert!(report.contains("**Navigation**"));
		assert!(report.contains("| `/` | Slash commands |"));
	}

	#[test]
	fn changelog_report_limits_recent_sections_and_rejects_empty() {
		let text = "# Changelog\n\n## 1.3\n- c\n\n## 1.2\n- b\n\n## 1.1\n- a\n\n## 1.0\n- z\n";
		let recent = changelog_report(text, false).unwrap();
		assert_eq!(recent.lines().filter(|line| line.starts_with("## ")).count(), 3);
		assert!(recent.starts_with("## 1.3"));
		let full = changelog_report(text, true).unwrap();
		assert_eq!(full.lines().filter(|line| line.starts_with("## ")).count(), 4);
		assert_eq!(changelog_report("# Changelog\n\nnothing yet\n", true), None);
	}

	#[test]
	fn context_report_reads_the_compaction_threshold() {
		let con = Ctx::new();
		let dom = Dom::default();
		let report = context_report(&dom, &con);
		assert!(report.contains("Compaction threshold: 80%"), "{report}");
		assert!(report.contains("Window: unknown"), "{report}");
	}
}
