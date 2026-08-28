//! Interactive pseudo-terminal overlay and bounded terminal-state projection.

use std::{collections::VecDeque, mem, str};

use bytes::Bytes;
use omp_core::{Str, sf};
use omp_tui::{Dim, Key, Layer, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, dom};

use crate::{OverlayPanel, panel_divider};

const SCROLLBACK_LINES: usize = 10_000;
const MAX_LIVE_WRITE_QUEUE_CHUNKS: usize = 512;
const MIN_COLUMNS: u16 = 20;
const MIN_ROWS: u16 = 5;

/// Terminal lifecycle rendered in the overlay chrome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtyStatus {
	/// The child still accepts input.
	Running,
	/// The child exited normally or with a status code.
	Exited,
	/// The environment deadline elapsed.
	TimedOut,
	/// The user forced process-tree termination.
	Killed,
}

/// One action emitted by the PTY overlay.
pub enum PtyEvent {
	/// The key or paste was translated into exact PTY bytes.
	Input(Bytes),
	/// Escape requests immediate resource-owned force-kill.
	ForceKill,
	/// Escape closes a terminal overlay after the child exits.
	Close,
	/// No backend action is required.
	Consumed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StringKind {
	Osc,
	Dcs,
	Apc,
}

#[derive(Debug)]
enum ParseState {
	Ground,
	Escape,
	Csi(Vec<u8>),
	String { kind: StringKind, escape: bool },
}

/// Bounded queue between environment output and the terminal-state engine.
///
/// When producer overflow drops a middle span, an ST prefix is inserted before
/// the oldest retained chunk. ST is harmless in the ground state and closes a
/// DCS, OSC, or APC whose terminator was part of the dropped span.
#[derive(Debug, Default)]
pub struct PtyOutputQueue {
	chunks:  VecDeque<Bytes>,
	dropped: u64,
}

impl PtyOutputQueue {
	/// Appends one live chunk while retaining the newest live window.
	pub fn push(&mut self, chunk: Bytes) {
		self.chunks.push_back(chunk);
		if self.chunks.len() <= MAX_LIVE_WRITE_QUEUE_CHUNKS {
			return;
		}
		let overflow = self.chunks.len() - MAX_LIVE_WRITE_QUEUE_CHUNKS;
		for _ in 0..overflow {
			self.chunks.pop_front();
		}
		self.dropped = self.dropped.saturating_add(overflow as u64);
		if let Some(first) = self.chunks.pop_front() {
			let mut repaired = Vec::with_capacity(first.len().saturating_add(2));
			repaired.extend_from_slice(b"\x1b\\");
			repaired.extend_from_slice(&first);
			self.chunks.push_front(Bytes::from(repaired));
		}
	}

	/// Applies every retained chunk in order.
	pub fn drain_into(&mut self, terminal: &mut TerminalState) {
		while let Some(chunk) = self.chunks.pop_front() {
			terminal.write(&chunk);
		}
	}

	/// Returns the number of chunks dropped from the live projection.
	pub const fn dropped(&self) -> u64 {
		self.dropped
	}
}

/// Sans-I/O terminal-state engine used by the interactive overlay.
#[derive(Debug)]
pub struct TerminalState {
	rows:               u16,
	columns:            u16,
	cursor_row:         u16,
	cursor_column:      u16,
	screen:             Vec<Vec<char>>,
	scrollback:         VecDeque<String>,
	parse:              ParseState,
	text:               Vec<u8>,
	utf8_pending:       Vec<u8>,
	application_cursor: bool,
}

impl TerminalState {
	/// Creates a blank terminal with bounded scrollback capacity.
	pub fn new(rows: u16, columns: u16) -> Self {
		let rows = rows.max(1);
		let columns = columns.max(1);
		Self {
			rows,
			columns,
			cursor_row: 0,
			cursor_column: 0,
			screen: vec![vec![' '; usize::from(columns)]; usize::from(rows)],
			scrollback: VecDeque::with_capacity(SCROLLBACK_LINES),
			parse: ParseState::Ground,
			text: Vec::new(),
			utf8_pending: Vec::new(),
			application_cursor: false,
		}
	}

	/// Applies raw PTY bytes while retaining parser state across chunks.
	pub fn write(&mut self, bytes: &[u8]) {
		for &byte in bytes {
			match &mut self.parse {
				ParseState::Ground => match byte {
					0x1b => {
						self.flush_text();
						self.parse = ParseState::Escape;
					},
					b'\n' => {
						self.flush_text();
						self.line_feed();
					},
					b'\r' => {
						self.flush_text();
						self.cursor_column = 0;
					},
					0x08 => {
						self.flush_text();
						self.cursor_column = self.cursor_column.saturating_sub(1);
					},
					b'\t' => {
						self.flush_text();
						self.cursor_column = ((self.cursor_column / 8) + 1)
							.saturating_mul(8)
							.min(self.columns.saturating_sub(1));
					},
					0x00..=0x1f | 0x7f => self.flush_text(),
					_ => self.text.push(byte),
				},
				ParseState::Escape => {
					self.parse = match byte {
						b'[' => ParseState::Csi(Vec::new()),
						b']' => ParseState::String { kind: StringKind::Osc, escape: false },
						b'P' => ParseState::String { kind: StringKind::Dcs, escape: false },
						b'_' => ParseState::String { kind: StringKind::Apc, escape: false },
						b'D' => {
							self.line_feed();
							ParseState::Ground
						},
						b'E' => {
							self.cursor_column = 0;
							self.line_feed();
							ParseState::Ground
						},
						b'c' => {
							self.clear();
							ParseState::Ground
						},
						_ => ParseState::Ground,
					};
				},
				ParseState::Csi(sequence) => {
					if (0x40..=0x7e).contains(&byte) {
						let sequence = mem::take(sequence);
						self.parse = ParseState::Ground;
						self.apply_csi(&sequence, byte);
					} else if sequence.len() < 4096 {
						sequence.push(byte);
					} else {
						self.parse = ParseState::Ground;
					}
				},
				ParseState::String { kind, escape } => {
					if *escape && byte == b'\\' {
						self.parse = ParseState::Ground;
					} else if *kind == StringKind::Osc && byte == 0x07 {
						self.parse = ParseState::Ground;
					} else {
						*escape = byte == 0x1b;
					}
				},
			}
		}
		self.flush_text();
	}

	/// Resizes the virtual screen and preserves its newest visible rows.
	pub fn resize(&mut self, rows: u16, columns: u16) {
		let rows = rows.max(1);
		let columns = columns.max(1);
		if columns != self.columns {
			for row in &mut self.screen {
				row.resize(usize::from(columns), ' ');
			}
			self.columns = columns;
			self.cursor_column = self.cursor_column.min(columns.saturating_sub(1));
		}
		if rows < self.rows {
			let removed = usize::from(self.rows - rows);
			for _ in 0..removed {
				let line = self.screen.remove(0);
				self.push_scrollback(line);
			}
		} else {
			self
				.screen
				.resize_with(usize::from(rows), || vec![' '; usize::from(columns)]);
		}
		self.rows = rows;
		self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
	}

	/// Returns whether DEC application-cursor mode is active.
	pub const fn application_cursor(&self) -> bool {
		self.application_cursor
	}

	/// Copies the visible viewport as plain rows.
	pub fn visible_lines(&self) -> Vec<Str> {
		self
			.screen
			.iter()
			.map(|row| Str::from(row.iter().collect::<String>().trim_end()))
			.collect()
	}

	fn flush_text(&mut self) {
		if self.text.is_empty() && self.utf8_pending.is_empty() {
			return;
		}
		self.utf8_pending.append(&mut self.text);
		loop {
			match str::from_utf8(&self.utf8_pending) {
				Ok(text) => {
					let owned = text.to_owned();
					self.utf8_pending.clear();
					self.write_text(&owned);
					break;
				},
				Err(error) => {
					let valid = error.valid_up_to();
					if valid != 0 {
						let text = String::from_utf8_lossy(&self.utf8_pending[..valid]).into_owned();
						self.write_text(&text);
						self.utf8_pending.drain(..valid);
					}
					if let Some(length) = error.error_len() {
						self.write_char('\u{fffd}');
						self
							.utf8_pending
							.drain(..length.min(self.utf8_pending.len()));
						continue;
					}
					break;
				},
			}
		}
	}

	fn write_text(&mut self, text: &str) {
		for grapheme in xutf::graphemes_str(text) {
			let mut characters = grapheme.chars();
			let Some(character) = characters.next() else {
				continue;
			};
			self.write_char(character);
			for character in characters {
				self.write_char(character);
			}
		}
	}

	fn write_char(&mut self, character: char) {
		if self.cursor_column >= self.columns {
			self.cursor_column = 0;
			self.line_feed();
		}
		let row = usize::from(self.cursor_row);
		let column = usize::from(self.cursor_column);
		self.screen[row][column] = character;
		let width = u16::try_from(xutf::width_char(character))
			.unwrap_or(1)
			.max(1);
		for offset in 1..width {
			let continuation = self.cursor_column.saturating_add(offset);
			if continuation < self.columns {
				self.screen[row][usize::from(continuation)] = ' ';
			}
		}
		self.cursor_column = self.cursor_column.saturating_add(width);
	}

	fn line_feed(&mut self) {
		if self.cursor_row + 1 < self.rows {
			self.cursor_row += 1;
			return;
		}
		let line = self.screen.remove(0);
		self.push_scrollback(line);
		self.screen.push(vec![' '; usize::from(self.columns)]);
	}

	fn push_scrollback(&mut self, line: Vec<char>) {
		self
			.scrollback
			.push_back(line.into_iter().collect::<String>().trim_end().to_owned());
		while self.scrollback.len() > SCROLLBACK_LINES {
			self.scrollback.pop_front();
		}
	}

	fn clear(&mut self) {
		for row in &mut self.screen {
			row.fill(' ');
		}
		self.cursor_row = 0;
		self.cursor_column = 0;
		self.application_cursor = false;
	}

	fn apply_csi(&mut self, sequence: &[u8], final_byte: u8) {
		let private = sequence.first() == Some(&b'?');
		let body = if private { &sequence[1..] } else { sequence };
		let mut values = body.split(|byte| *byte == b';').map(|part| {
			str::from_utf8(part)
				.ok()
				.and_then(|part| part.parse::<u16>().ok())
		});
		let first = values.next().flatten().unwrap_or(1);
		match final_byte {
			b'A' => self.cursor_row = self.cursor_row.saturating_sub(first),
			b'B' => self.cursor_row = self.cursor_row.saturating_add(first).min(self.rows - 1),
			b'C' => {
				self.cursor_column = self
					.cursor_column
					.saturating_add(first)
					.min(self.columns - 1);
			},
			b'D' => self.cursor_column = self.cursor_column.saturating_sub(first),
			b'G' => self.cursor_column = first.saturating_sub(1).min(self.columns - 1),
			b'H' | b'f' => {
				let column = values.next().flatten().unwrap_or(1);
				self.cursor_row = first.saturating_sub(1).min(self.rows - 1);
				self.cursor_column = column.saturating_sub(1).min(self.columns - 1);
			},
			b'J' => match first {
				2 | 3 => self.clear(),
				_ => {
					for column in self.cursor_column..self.columns {
						self.screen[usize::from(self.cursor_row)][usize::from(column)] = ' ';
					}
					for row in self.cursor_row.saturating_add(1)..self.rows {
						self.screen[usize::from(row)].fill(' ');
					}
				},
			},
			b'K' => match first {
				1 => {
					for column in 0..=self.cursor_column {
						self.screen[usize::from(self.cursor_row)][usize::from(column)] = ' ';
					}
				},
				2 => self.screen[usize::from(self.cursor_row)].fill(' '),
				_ => {
					for column in self.cursor_column..self.columns {
						self.screen[usize::from(self.cursor_row)][usize::from(column)] = ' ';
					}
				},
			},
			b'h' if private && first == 1 => self.application_cursor = true,
			b'l' if private && first == 1 => self.application_cursor = false,
			_ => {},
		}
	}
}

/// Centered interactive terminal overlay.
pub struct PtyOverlay {
	id:        Str,
	command:   Str,
	status:    PtyStatus,
	exit_code: Option<i32>,
	terminal:  TerminalState,
	queue:     PtyOutputQueue,
	ui:        Ui,
	ctx:       UiContext,
	options:   OverlayOptions,
	width:     u16,
	height:    u16,
	dirty:     bool,
}

impl PtyOverlay {
	/// Opens a running terminal overlay.
	pub fn open(id: Str, command: Str, ctx: &UiContext) -> Self {
		let width = 80;
		let height = 24;
		let terminal = TerminalState::new(height - 4, width - 2);
		let ui = build(&command, PtyStatus::Running, None, &terminal, width, ctx);
		Self {
			id,
			command,
			status: PtyStatus::Running,
			exit_code: None,
			terminal,
			queue: PtyOutputQueue::default(),
			ui,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.z(50)
				.max_height(Dim::Cells(height)),
			width,
			height,
			dirty: false,
		}
	}

	/// Returns the stable tool-call identity owning this overlay.
	pub fn id(&self) -> &Str {
		&self.id
	}

	/// Queues terminal output and updates the retained screen state.
	pub fn append_output(&mut self, chunk: Bytes) {
		self.queue.push(chunk);
		self.queue.drain_into(&mut self.terminal);
		self.dirty = true;
	}

	/// Marks the execution terminal while keeping its final screen visible.
	pub fn finish(&mut self, status: PtyStatus, exit_code: Option<i32>) {
		self.status = status;
		self.exit_code = exit_code;
		self.dirty = true;
	}

	/// Routes a decoded key into PTY bytes or force-kill.
	pub fn handle_key(&mut self, key: Key) -> PtyEvent {
		if self.status != PtyStatus::Running {
			return if key == Key::Esc {
				PtyEvent::Close
			} else {
				PtyEvent::Consumed
			};
		}
		if key == Key::Esc {
			return PtyEvent::ForceKill;
		}
		encode_key(key, self.terminal.application_cursor())
			.map_or(PtyEvent::Consumed, PtyEvent::Input)
	}

	/// Forwards pasted text without shell reinterpretation.
	pub fn handle_paste(&mut self, text: &str) -> PtyEvent {
		if self.status == PtyStatus::Running && !text.is_empty() {
			PtyEvent::Input(Bytes::copy_from_slice(text.as_bytes()))
		} else {
			PtyEvent::Consumed
		}
	}

	/// Returns the responsive centered terminal layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(4).clamp(MIN_COLUMNS, 120);
		let height = viewport
			.height
			.saturating_mul(4)
			.checked_div(5)
			.unwrap_or(MIN_ROWS)
			.clamp(MIN_ROWS, viewport.height.max(MIN_ROWS));
		if width != self.width || height != self.height {
			self.width = width;
			self.height = height;
			self
				.terminal
				.resize(height.saturating_sub(4).max(1), width.saturating_sub(2).max(1));
			self.dirty = true;
		}
		if self.dirty {
			self.ui = build(
				&self.command,
				self.status,
				self.exit_code,
				&self.terminal,
				self.width,
				&self.ctx,
			);
			self.dirty = false;
		}
		self.options = self
			.options
			.clone()
			.width(Dim::Cells(self.width))
			.max_height(Dim::Cells(self.height));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	/// Returns the current PTY dimensions as `(rows, columns)`.
	pub fn dimensions(&self) -> (u16, u16) {
		(self.height.saturating_sub(4).max(1), self.width.saturating_sub(2).max(1))
	}
}

fn build(
	command: &Str,
	status: PtyStatus,
	exit_code: Option<i32>,
	terminal: &TerminalState,
	width: u16,
	ctx: &UiContext,
) -> Ui {
	let status = match (status, exit_code) {
		(PtyStatus::Running, _) => sf!("running"),
		(PtyStatus::TimedOut, _) => sf!("timed out"),
		(PtyStatus::Killed, _) => sf!("killed"),
		(PtyStatus::Exited, Some(code)) => sf!("exit {code}"),
		(PtyStatus::Exited, None) => sf!("exited"),
	};
	let lines = terminal.visible_lines();
	let dropped = terminal.scrollback.len().saturating_sub(SCROLLBACK_LINES);
	let footer = if status == "running" {
		sf!("Esc force-kill · input forwarded to PTY")
	} else {
		sf!("session finished")
	};
	let overflow = (dropped != 0).then(|| sf!("{dropped} old lines omitted"));
	Ui::from_root(
		OverlayPanel::new(sf!("Console · {status}")).child(dom! {
			<col>
				<text dim>{command.clone()}</text>
				{panel_divider()}
				for line in lines {
					<text>{line}</text>
				}
				if let Some(overflow) = overflow {
					<text dim>{overflow}</text>
				}
				{panel_divider()}
				<text dim>{footer}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

fn encode_key(key: Key, application_cursor: bool) -> Option<Bytes> {
	let sequence: &[u8] = match key {
		Key::Up => {
			if application_cursor {
				b"\x1bOA"
			} else {
				b"\x1b[A"
			}
		},
		Key::Down => {
			if application_cursor {
				b"\x1bOB"
			} else {
				b"\x1b[B"
			}
		},
		Key::Right => {
			if application_cursor {
				b"\x1bOC"
			} else {
				b"\x1b[C"
			}
		},
		Key::Left => {
			if application_cursor {
				b"\x1bOD"
			} else {
				b"\x1b[D"
			}
		},
		Key::Home => {
			if application_cursor {
				b"\x1bOH"
			} else {
				b"\x1b[H"
			}
		},
		Key::End => {
			if application_cursor {
				b"\x1bOF"
			} else {
				b"\x1b[F"
			}
		},
		Key::PageUp => b"\x1b[5~",
		Key::PageDown => b"\x1b[6~",
		Key::Insert => b"\x1b[2~",
		Key::Delete => b"\x1b[3~",
		Key::BackTab => b"\x1b[Z",
		Key::Enter | Key::ShiftEnter | Key::FollowUp => b"\r",
		Key::Tab => b"\t",
		Key::Space => b" ",
		Key::Backspace => b"\x7f",
		Key::RestoreQueue
		| Key::CyclePrevious
		| Key::PlanToggle
		| Key::DebugMenu
		| Key::ToggleToolVisibility
		| Key::CopyPrompt
		| Key::CopyLine => b"",
		Key::JumpPrevious | Key::JumpNext => return None,
		Key::Function(1) => b"\x1bOP",
		Key::Function(2) => b"\x1bOQ",
		Key::Function(3) => b"\x1bOR",
		Key::Function(4) => b"\x1bOS",
		Key::Function(5) => b"\x1b[15~",
		Key::Function(6) => b"\x1b[17~",
		Key::Function(7) => b"\x1b[18~",
		Key::Function(8) => b"\x1b[19~",
		Key::Function(9) => b"\x1b[20~",
		Key::Function(10) => b"\x1b[21~",
		Key::Function(11) => b"\x1b[23~",
		Key::Function(12) => b"\x1b[24~",
		Key::Ctrl(character) if character.is_ascii_lowercase() => {
			return Some(Bytes::from(vec![(character as u8).saturating_sub(b'a').saturating_add(1)]));
		},
		Key::Alt(character) if character.is_ascii() => {
			return Some(Bytes::from(vec![0x1b, character as u8]));
		},
		Key::CtrlAlt(character) if character.is_ascii_lowercase() => {
			return Some(Bytes::from(vec![
				0x1b,
				(character as u8).saturating_sub(b'a').saturating_add(1),
			]));
		},
		Key::Char(character) => {
			let mut encoded = [0_u8; 4];
			return Some(Bytes::copy_from_slice(character.encode_utf8(&mut encoded).as_bytes()));
		},
		Key::Esc
		| Key::SelectLeft
		| Key::SelectRight
		| Key::SelectUp
		| Key::SelectDown
		| Key::SelectHome
		| Key::SelectEnd
		| Key::WordLeft
		| Key::WordRight
		| Key::SelectWordLeft
		| Key::SelectWordRight
		| Key::SelectAll
		| Key::Copy
		| Key::Cut
		| Key::WordDelete
		| Key::Paste
		| Key::PasteRaw
		| Key::Function(_)
		| Key::Ctrl(_)
		| Key::Alt(_)
		| Key::CtrlAlt(_) => return None,
	};
	Some(Bytes::from_static(sequence))
}
