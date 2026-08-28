//! Full-screen bounded log viewer model and overlay.

use std::collections::BTreeSet;

use omp_core::{Str, sf};
use omp_tui::{Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, dom};

use crate::{OverlayPanel, panel_divider};

/// One sanitized process log entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogEntry {
	/// Complete raw line.
	pub line: Str,
	/// Parsed process id when present.
	pub pid: Option<u32>,
	/// Whether this is the first line from the current process lifetime.
	pub current_process_boundary: bool,
}

/// Result of routing log-viewer input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogViewerEvent {
	/// Viewer state changed and remains open.
	Consumed,
	/// Close the overlay.
	Close,
	/// Host should load the next backwards chunk.
	LoadOlder,
	/// Sanitized selected rows should be copied.
	Copy(String),
}

/// Searchable, expandable, range-selecting log viewer.
pub struct LogViewer {
	entries:     Vec<LogEntry>,
	visible:     Vec<usize>,
	query:       String,
	current_pid: u32,
	pid_only:    bool,
	cursor:      usize,
	anchor:      Option<usize>,
	expanded:    BTreeSet<usize>,
	has_older:   bool,
	ui:          Ui,
	ctx:         UiContext,
	options:     OverlayOptions,
	width:       u16,
	rows:        u16,
}

impl LogViewer {
	/// Opens the viewer on a newest bounded chunk.
	pub fn open(entries: Vec<LogEntry>, current_pid: u32, has_older: bool, ctx: &UiContext) -> Self {
		let mut viewer = Self {
			entries,
			visible: Vec::new(),
			query: String::new(),
			current_pid,
			pid_only: false,
			cursor: 0,
			anchor: None,
			expanded: BTreeSet::new(),
			has_older,
			ui: Ui::from_root(dom! { <text/> }, 1, ctx.clone()),
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(80))
				.z(20),
			width: 80,
			rows: 16,
		};
		viewer.rebuild();
		viewer
	}

	/// Prepends an older chunk while preserving the selected logical entry.
	pub fn prepend(&mut self, mut older: Vec<LogEntry>, has_older: bool) {
		let added = older.len();
		older.append(&mut self.entries);
		self.entries = older;
		self.cursor = self.cursor.saturating_add(added);
		self.expanded = self.expanded.iter().map(|index| index + added).collect();
		self.has_older = has_older;
		self.rebuild();
	}

	/// Routes keyboard search, selection, expansion, copy, and paging.
	pub fn handle_key(&mut self, key: Key) -> LogViewerEvent {
		match key {
			Key::Esc => return LogViewerEvent::Close,
			Key::Up => self.move_cursor(-1, false),
			Key::Down => self.move_cursor(1, false),
			Key::SelectUp => self.move_cursor(-1, true),
			Key::SelectDown => self.move_cursor(1, true),
			Key::Home => {
				self.cursor = 0;
				self.anchor = None;
			},
			Key::End => {
				self.cursor = self.visible.len().saturating_sub(1);
				self.anchor = None;
			},
			Key::PageUp => self.move_cursor(-(self.rows as isize), false),
			Key::PageDown => self.move_cursor(self.rows as isize, false),
			Key::Left => self.collapse_selected(),
			Key::Right => {
				self.expand_selected();
				self.rebuild_ui();
				return self.older_event();
			},
			Key::Enter | Key::Space => self.toggle_expanded(),
			Key::Ctrl('o') => return LogViewerEvent::LoadOlder,
			Key::Ctrl('p') => {
				self.pid_only = !self.pid_only;
				self.rebuild();
			},
			Key::Ctrl('a') | Key::SelectAll => {
				if !self.visible.is_empty() {
					self.anchor = Some(0);
					self.cursor = self.visible.len() - 1;
				}
			},
			Key::Ctrl('e') => {
				self.expanded.extend(self.visible.iter().copied());
			},
			Key::Ctrl('l') => self.expanded.clear(),
			Key::Copy | Key::Ctrl('c') => return LogViewerEvent::Copy(self.copy_payload()),
			Key::Backspace => {
				self.query.pop();
				self.rebuild();
			},
			Key::Char(character) if !character.is_control() => {
				self.query.push(character);
				self.rebuild();
			},
			_ => return LogViewerEvent::Consumed,
		}
		self.rebuild_ui();
		LogViewerEvent::Consumed
	}

	/// Routes wheel and click row hit-testing.
	pub fn handle_mouse(
		&mut self,
		_col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> LogViewerEvent {
		match kind {
			Mouse::WheelUp => self.move_cursor(-3, false),
			Mouse::WheelDown => self.move_cursor(3, false),
			Mouse::Click => {
				let top = viewport
					.height
					.saturating_sub(self.rows)
					.saturating_div(2)
					.saturating_add(3);
				let relative = usize::from(row.saturating_sub(top));
				if relative < self.visible.len() {
					self.cursor = relative;
					self.anchor = None;
					self.toggle_expanded();
				}
			},
			_ => return LogViewerEvent::Consumed,
		}
		self.rebuild_ui();
		LogViewerEvent::Consumed
	}

	/// Requests older data when the cursor is at the oldest visible entry.
	pub fn older_event(&self) -> LogViewerEvent {
		if self.has_older && self.cursor == 0 {
			LogViewerEvent::LoadOlder
		} else {
			LogViewerEvent::Consumed
		}
	}

	/// Returns the responsive full-screen overlay layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(2).max(32);
		let rows = viewport.height.saturating_sub(6).max(5);
		if width != self.width || rows != self.rows {
			self.width = width;
			self.rows = rows;
			self.rebuild_ui();
		}
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn rebuild(&mut self) {
		let query = self.query.to_lowercase();
		self.visible = self
			.entries
			.iter()
			.enumerate()
			.filter_map(|(index, entry)| {
				if self.pid_only && entry.pid != Some(self.current_pid) {
					return None;
				}
				(query.is_empty() || entry.line.to_lowercase().contains(&query)).then_some(index)
			})
			.collect();
		self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
		self.rebuild_ui();
	}

	fn move_cursor(&mut self, delta: isize, extend: bool) {
		if self.visible.is_empty() {
			return;
		}
		if extend && self.anchor.is_none() {
			self.anchor = Some(self.cursor);
		}
		if !extend {
			self.anchor = None;
		}
		self.cursor = self
			.cursor
			.saturating_add_signed(delta)
			.min(self.visible.len() - 1);
	}

	fn toggle_expanded(&mut self) {
		let Some(index) = self.selected_index() else {
			return;
		};
		if !self.expanded.remove(&index) {
			self.expanded.insert(index);
		}
	}

	fn collapse_selected(&mut self) {
		if let Some(index) = self.selected_index() {
			self.expanded.remove(&index);
		}
	}

	fn expand_selected(&mut self) {
		if let Some(index) = self.selected_index() {
			self.expanded.insert(index);
		}
	}

	fn selected_index(&self) -> Option<usize> {
		self.visible.get(self.cursor).copied()
	}

	fn selected(&self) -> impl Iterator<Item = usize> + '_ {
		let start = self.anchor.unwrap_or(self.cursor).min(self.cursor);
		let end = self.anchor.unwrap_or(self.cursor).max(self.cursor);
		self.visible.get(start..=end).into_iter().flatten().copied()
	}

	fn copy_payload(&self) -> String {
		self
			.selected()
			.filter_map(|index| self.entries.get(index))
			.map(|entry| sanitize_line(&entry.line))
			.filter(|line| !line.is_empty())
			.collect::<Vec<_>>()
			.join("\n")
	}

	fn rebuild_ui(&mut self) {
		let selected = self.selected().collect::<BTreeSet<_>>();
		let start = self.cursor.saturating_sub(self.rows as usize / 2);
		let rows = self
			.visible
			.iter()
			.skip(start)
			.take(self.rows as usize)
			.filter_map(|index| {
				let entry = self.entries.get(*index)?;
				let prefix = if selected.contains(index) {
					"› "
				} else {
					"  "
				};
				let boundary = entry
					.current_process_boundary
					.then_some("Current process ─ ")
					.unwrap_or("");
				let text = if self.expanded.contains(index) {
					sf!("{prefix}{boundary}{}", entry.line)
				} else {
					let first = entry.line.lines().next().unwrap_or_default();
					sf!("{prefix}{boundary}{first}")
				};
				Some(text)
			})
			.collect::<Vec<_>>();
		let summary = sf!(
			"{} shown · filter: {} · PID {}",
			self.visible.len(),
			if self.query.is_empty() {
				"none"
			} else {
				&self.query
			},
			if self.pid_only { "current" } else { "all" }
		);
		let height = self.rows;
		self.ui = Ui::from_root(OverlayPanel::new("Debug logs").child(dom! {
			<col gap=1>
				<text dim truncate>{summary}</text>
				<scroll id="debug-log-scroll" h={height}>
					<col>
						for row in rows {
							<text wrap>{row}</text>
						}
					</col>
				</scroll>
				{panel_divider()}
				<text dim truncate>{"Type search · Ctrl+O older · ← collapse · → expand · Shift+↑/↓ select · Enter toggle · Ctrl+C copy · Esc close"}</text>
			</col>
		}), self.width, self.ctx.clone());
	}
}

fn sanitize_line(line: &str) -> String {
	let mut output = String::with_capacity(line.len());
	let mut chars = line.chars().peekable();
	while let Some(character) = chars.next() {
		if character == '\u{1b}' {
			match chars.next() {
				Some('[') => {
					for tail in chars.by_ref() {
						if ('@'..='~').contains(&tail) {
							break;
						}
					}
				},
				Some(']') => {
					let mut escape = false;
					for tail in chars.by_ref() {
						if tail == '\u{7}' || (escape && tail == '\\') {
							break;
						}
						escape = tail == '\u{1b}';
					}
				},
				_ => {},
			}
		} else if matches!(character, '\t' | ' ') || !character.is_control() {
			output.push(character);
		}
	}
	output
}

#[cfg(test)]
mod tests {
	use super::*;

	fn entry(line: &str) -> LogEntry {
		LogEntry { line: Str::new(line), pid: None, current_process_boundary: false }
	}

	#[test]
	fn directional_expansion_and_older_loading_match_keyboard_contract() {
		let mut viewer =
			LogViewer::open(vec![entry("oldest"), entry("newest")], 0, true, &UiContext::default());

		assert_eq!(viewer.handle_key(Key::Right), LogViewerEvent::LoadOlder);
		assert!(viewer.expanded.contains(&0));
		assert_eq!(viewer.handle_key(Key::Left), LogViewerEvent::Consumed);
		assert!(!viewer.expanded.contains(&0));

		viewer.cursor = 1;
		assert_eq!(viewer.handle_key(Key::Ctrl('o')), LogViewerEvent::LoadOlder);
	}
}
