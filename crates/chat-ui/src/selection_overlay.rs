//! Retained selector used by copy, hook, and advisor workflows.

use omp_core::{IntoStr, Str};
use omp_tui::{Key, Layer, Mouse, Size, UiContext};

use crate::{ListPicker, ListRow, PickerEvent};

/// Backend-owned workflow receiving the selected stable key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionPurpose {
	/// Copy one transcript selection, code block, or command.
	Copy,
	/// Select a configured hook before editing its input.
	Hook,
	/// Select an advisor configuration field.
	Advisor,
	/// Select a conversation branch or history result.
	History,
	/// Select a user message to branch the session from.
	Branch,
}

/// Action emitted by [`SelectionOverlay`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionEvent {
	/// Input was consumed while the selector remains open.
	Consumed,
	/// Dismiss without choosing.
	Close,
	/// Commit one stable backend key.
	Pick {
		/// Backend workflow that will consume the chosen key.
		purpose: SelectionPurpose,
		/// Stable backend identity of the selected row.
		key:     Str,
	},
}

/// Filterable retained selector that never owns workflow state.
pub struct SelectionOverlay {
	picker:  ListPicker,
	rows:    Vec<ListRow>,
	purpose: SelectionPurpose,
}

impl SelectionOverlay {
	/// Opens a selector over backend-projected rows.
	pub fn open(
		title: impl IntoStr,
		purpose: SelectionPurpose,
		rows: Vec<ListRow>,
		ctx: &UiContext,
	) -> Self {
		let picker = ListPicker::open(title, &rows, 0, ctx);
		Self { picker, rows, purpose }
	}

	/// Routes a key through the retained filter and selection.
	pub fn handle_key(&mut self, key: Key) -> SelectionEvent {
		let event = self.picker.handle_key(key);
		self.route(event)
	}

	/// Routes pasted search text.
	pub fn handle_paste(&mut self, text: &str) -> SelectionEvent {
		let event = self.picker.handle_paste(text);
		self.route(event)
	}

	/// Routes pointer interaction and outside-click dismissal.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> SelectionEvent {
		let event = self.picker.handle_mouse(col, row, kind, viewport);
		self.route(event)
	}

	/// Returns the composited retained layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		self.picker.layer(viewport)
	}

	fn route(&self, event: PickerEvent) -> SelectionEvent {
		match event {
			PickerEvent::Consumed => SelectionEvent::Consumed,
			PickerEvent::Close => SelectionEvent::Close,
			PickerEvent::Pick(index) | PickerEvent::PickTask(index) => {
				self
					.rows
					.get(index)
					.map_or(SelectionEvent::Consumed, |row| SelectionEvent::Pick {
						purpose: self.purpose,
						key:     row.key.clone(),
					})
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn digit_picks_numbered_label_after_unnumbered_row() {
		let rows = vec![
			ListRow {
				key:    "detected".into(),
				label:  "Detected item".into(),
				detail: Str::default(),
			},
			ListRow { key: "first".into(), label: "1. First".into(), detail: Str::default() },
			ListRow { key: "second".into(), label: "2. Second".into(), detail: Str::default() },
		];
		let mut overlay =
			SelectionOverlay::open("Pick one", SelectionPurpose::Hook, rows, &UiContext::default());

		assert_eq!(overlay.handle_key(Key::Char('2')), SelectionEvent::Pick {
			purpose: SelectionPurpose::Hook,
			key:     "second".into(),
		});
	}
}
