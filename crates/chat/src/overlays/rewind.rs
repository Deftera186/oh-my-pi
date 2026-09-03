//! `/branch` and `/rewind` selector: pick the user turn the session should
//! continue from (pi `rewind-selector.ts`).
//!
//! pi replays the whole transcript on the alternate screen and walks a
//! dotted outline over rendered blocks, sliding a camera between sibling
//! branches at a fork. This port keeps pi's framing — the `↶ Rewind · pick
//! the point to continue from` header, the `n/m  ↑/↓ step  ←/→ user turns
//! enter rewind  ctrl+o expand  esc cancel` hint — over a type-to-filter list
//! of user turns with a wrapped preview of the outlined message below it.
//! The branch strip and its eased camera slide are not ported: `/tree` owns
//! sibling navigation on this host.
//!
//! Rows are read from the detached session replica (ADR 0005); the choice
//! leaves as a `rewind <entry> "<text>"` console line (ADR 0014) so the
//! controller rewinds the head and the composer is prefilled with the
//! message the turn was started with.

use omp_core::{Str, StrMut, sf};
use omp_dom::{Dom, KnownTag, PropId, PropKey, Tag, Value};
use omp_tui::{Frame, Key, Prop, Size, Ui, UiContext, UiEvent, dom};

use super::{Panel, PanelAction, PanelAnchor, PanelCx, PanelEvent};

/// pi `rewind-selector.ts:407-409`: header row caption.
const HEADER_CAPTION: &str = "pick the point to continue from";
/// pi `rewind-selector.ts:412`: lateral hint when no branch strip is open.
const LATERAL_HINT: &str = "←/→ user turns";
/// Box border, header, two rules, and the hint row.
const CHROME_ROWS: u16 = 7;
/// Preview rows while collapsed (pi `ScrollView` default height is 10; the
/// list keeps the larger share of a bottom-anchored panel).
const PREVIEW_ROWS: u16 = 4;
/// Preview rows after Ctrl+O.
const PREVIEW_ROWS_EXPANDED: u16 = 12;
/// Smallest list the panel keeps when the viewport is short.
const MIN_LIST_ROWS: u16 = 3;

/// One selectable user turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindRow {
	/// Entry the rewind lands on: the turn's `cause` (the head before the
	/// turn started) so the turn itself is dropped.
	pub target: Str,
	/// Full user message text.
	pub text:   Str,
}

/// Retained rewind selector over the session's user turns.
pub struct RewindPanel {
	ui:       Ui,
	ctx:      UiContext,
	rows:     Vec<RewindRow>,
	/// Index into `rows` of the outlined turn.
	selected: usize,
	query:    Str,
	expanded: bool,
	width:    u16,
	height:   u16,
	list:     u16,
	preview:  u16,
}

impl RewindPanel {
	/// Reads every user turn from the session replica; `Err` when there is
	/// nothing to rewind to.
	pub fn open(cx: &PanelCx<'_>) -> Result<Self, Str> {
		let rows = rewind_rows(cx.dom);
		if rows.is_empty() {
			return Err(Str::new_static("No user messages to rewind to"));
		}
		let selected = rows.len() - 1;
		let mut panel = Self {
			ui: Ui::from_root(dom! { <col/> }, cx.viewport.width, cx.ui.clone()),
			ctx: cx.ui.clone(),
			rows,
			selected,
			query: Str::default(),
			expanded: false,
			width: cx.viewport.width,
			height: cx.viewport.height,
			list: 0,
			preview: PREVIEW_ROWS,
		};
		let (list, preview) = panel.split();
		panel.list = list;
		panel.preview = preview;
		panel.rebuild();
		Ok(panel)
	}

	/// Rows in list order (oldest first).
	#[must_use]
	pub fn rows(&self) -> &[RewindRow] {
		&self.rows
	}

	/// Index of the outlined row.
	#[must_use]
	pub fn selected(&self) -> usize {
		self.selected
	}

	/// The console line Enter would emit for the outlined row.
	#[must_use]
	pub fn line(&self) -> Str {
		let row = &self.rows[self.selected];
		sf!("rewind {} \"{}\"", row.target, escape_quoted(&row.text))
	}

	/// List and preview heights for the viewport: the list takes the picker
	/// share (40% of the viewport minus chrome), the preview its fixed rows
	/// capped by whatever the viewport has left.
	fn split(&self) -> (u16, u16) {
		let list = (self.height * 2 / 5)
			.saturating_sub(CHROME_ROWS)
			.max(MIN_LIST_ROWS);
		let wanted = if self.expanded {
			PREVIEW_ROWS_EXPANDED
		} else {
			PREVIEW_ROWS
		};
		let room = self
			.height
			.saturating_sub(list.saturating_add(CHROME_ROWS))
			.max(1);
		(list, wanted.min(room))
	}

	fn hint(&self) -> Str {
		sf!(
			"{}/{}  ↑/↓ step  {LATERAL_HINT}  enter rewind  ctrl+o expand  esc cancel",
			self.selected + 1,
			self.rows.len()
		)
	}

	fn rebuild(&mut self) {
		let seed = self.query.clone();
		let list = self.list.saturating_add(1);
		let preview_rows = self.preview;
		let options = self
			.rows
			.iter()
			.enumerate()
			.map(|(index, row)| {
				let first = row.text.lines().next().unwrap_or_default();
				(sf!("{index}"), sf!("#{}  {first}", index + 1), index == self.selected)
			})
			.collect::<Vec<_>>();
		let preview = self.rows[self.selected].text.clone();
		let hint = self.hint();
		let tree = dom! {
			<box border=round pad-x=1>
				<col>
					<row gap=1>
						<i:rewind/>
						<text bold>{"Rewind"}</text>
						<text fg=muted>{"·"}</text>
						<text fg=muted truncate>{HEADER_CAPTION}</text>
					</row>
					<hr border=round/>
					<select id="turns" filter={seed} h={list}>
						for (value, label, selected) in options {
							<option value={value} label={label.clone()} selected={selected}>
								<td truncate grow><pre>{label}</pre></td>
							</option>
						}
					</select>
					<hr border=round/>
					<scroll id="preview" h={preview_rows}>
						<md>{preview}</md>
					</scroll>
					<hr border=round/>
					<text id="hint" fg=muted truncate>{hint}</text>
				</col>
			</box>
		};
		self.ui = Ui::from_root(tree, self.width, self.ctx.clone());
	}

	fn route(&mut self, event: UiEvent) -> PanelEvent {
		match event {
			UiEvent::Cancel => PanelEvent::Close,
			UiEvent::Changed { id, value } if id.as_str() == "turns" => {
				match self.index_of(&value) {
					Some(index) => {
						self.selected = index;
						PanelEvent::Finish(self.line())
					},
					None => PanelEvent::Consumed,
				}
			},
			UiEvent::Highlighted { id, value } if id.as_str() == "turns" => {
				if let Some(index) = self.index_of(&value)
					&& index != self.selected
				{
					self.selected = index;
					self.rebuild();
				}
				PanelEvent::Consumed
			},
			UiEvent::Filtered { id, query, value } if id.as_str() == "turns" => {
				self.query = query;
				if let Some(index) = value.as_deref().and_then(|value| self.index_of(value))
					&& index != self.selected
				{
					self.selected = index;
					self.rebuild();
				}
				PanelEvent::Consumed
			},
			_ => PanelEvent::Consumed,
		}
	}

	fn index_of(&self, value: &str) -> Option<usize> {
		value
			.parse::<usize>()
			.ok()
			.filter(|index| *index < self.rows.len())
	}
}

impl Panel for RewindPanel {
	fn id(&self) -> &'static str {
		"rewind"
	}

	fn anchor(&self) -> PanelAnchor {
		PanelAnchor::Bottom
	}

	/// Ctrl+O (pi `app.tools.expand`): grow the preview pane.
	fn action(&mut self, action: PanelAction) -> PanelEvent {
		match action {
			PanelAction::Expand => {
				self.expanded = !self.expanded;
				let (list, preview) = self.split();
				self.list = list;
				self.preview = preview;
				self.rebuild();
				PanelEvent::Consumed
			},
			_ => PanelEvent::Ignored,
		}
	}

	/// Up/Down step, Left/Right jump between user turns (every row is one),
	/// Enter rewinds, Esc cancels; printable keys filter the list. Stepping
	/// clamps at the oldest and newest turn (pi `#move`) instead of the
	/// filterable select's wrap while the whole list is visible.
	fn key(&mut self, key: Key) -> PanelEvent {
		let key = match key {
			Key::Left => Key::Up,
			Key::Right => Key::Down,
			other => other,
		};
		if self.query.is_empty()
			&& ((key == Key::Up && self.selected == 0)
				|| (key == Key::Down && self.selected + 1 == self.rows.len()))
		{
			return PanelEvent::Consumed;
		}
		let event = self.ui.handle_key(key);
		self.route(event)
	}

	fn paste(&mut self, text: &str) -> PanelEvent {
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	fn frame(&mut self, viewport: Size) -> &Frame {
		self.height = viewport.height;
		let (list, preview) = self.split();
		if viewport.width != self.width {
			self.width = viewport.width;
			self.list = list;
			self.preview = preview;
			self.rebuild();
		} else {
			if list != self.list {
				self.list = list;
				self.ui.set_prop("turns", Prop::H, list.saturating_add(1));
			}
			if preview != self.preview {
				self.preview = preview;
				self.ui.set_prop("preview", Prop::H, preview);
			}
		}
		self.ui.frame()
	}
}

/// Every `<turn>` under the body with a `<user>` child, oldest first.
fn rewind_rows(dom: &Dom) -> Vec<RewindRow> {
	let mut rows = Vec::new();
	for turn in dom.children(dom.body()) {
		let Some(node) = dom.get(*turn) else {
			continue;
		};
		if node.tag != Tag::Known(KnownTag::Turn) {
			continue;
		}
		let Some(target) = node
			.prop(&PropKey::from(PropId::Cause))
			.and_then(Value::as_str)
			.or_else(|| node.prop(&PropKey::from(PropId::Id)).and_then(Value::as_str))
		else {
			continue;
		};
		let Some(text) = dom.children(*turn).iter().find_map(|child| {
			let child = dom.get(*child)?;
			(child.tag == Tag::Known(KnownTag::User)).then(|| {
				child
					.content
					.clone()
					.or_else(|| {
						child
							.prop(&PropKey::from(PropId::Text))
							.and_then(Value::as_str)
							.map(Str::new)
					})
					.unwrap_or_default()
			})
		}) else {
			continue;
		};
		rows.push(RewindRow { target: Str::new(target), text });
	}
	rows
}

/// Escapes text for a double-quoted console argument (`omp_con` script
/// quoting: `\"`, `\\`, `\n`, `\t`).
fn escape_quoted(text: &str) -> Str {
	let mut out = StrMut::with_capacity(text.len() + 8);
	for ch in text.chars() {
		match ch {
			'\\' => out.push_str("\\\\"),
			'"' => out.push_str("\\\""),
			'\n' => out.push_str("\\n"),
			'\t' => out.push_str("\\t"),
			'\r' => {},
			other => out.push(other),
		}
	}
	out.freeze()
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use omp_con::Ctx;
	use omp_session::{ComponentRegistry, Session};

	use super::super::{NoServices, Services};
	use super::*;

	fn session(prompts: &[&str]) -> Session {
		let directory = tempfile::tempdir().expect("temp directory");
		let mut session = Session::create(directory.keep().join("rewind.oms"), ComponentRegistry::standard())
			.expect("session");
		for prompt in prompts {
			session.begin_turn().expect("turn");
			session.user(*prompt, Vec::new()).expect("user");
		}
		session
	}

	fn open(session: &Session) -> Result<RewindPanel, Str> {
		let con = Ctx::default();
		let ui = UiContext::default();
		let services: Arc<dyn Services> = Arc::new(NoServices);
		let cx = PanelCx {
			dom:      session.dom(),
			con:      &con,
			ui:       &ui,
			viewport: Size { width: 100, height: 30 },
			services: &services,
		};
		RewindPanel::open(&cx)
	}

	/// `cause` of the `<turn>` at `index` under the body.
	fn turn_cause(dom: &Dom, index: usize) -> Str {
		let turn = dom.children(dom.body())[index];
		let node = dom.get(turn).expect("turn");
		assert_eq!(node.tag, Tag::Known(KnownTag::Turn));
		Str::new(
			node
				.prop(&PropKey::from(PropId::Cause))
				.and_then(Value::as_str)
				.expect("cause"),
		)
	}

	#[test]
	fn empty_body_has_nothing_to_rewind_to() {
		let session = session(&[]);
		let error = open(&session).err().expect("error");
		assert_eq!(error.as_str(), "No user messages to rewind to");
	}

	#[test]
	fn rows_are_chronological_and_the_newest_is_outlined() {
		let session = session(&["first prompt", "second prompt\nwith detail", "third \"quoted\""]);
		let mut panel = open(&session).expect("panel");
		assert_eq!(panel.rows().len(), 3);
		assert_eq!(panel.selected(), 2, "cursor starts on the newest turn");
		let text = omp_tui::frame_text(panel.frame(Size { width: 100, height: 30 }));
		let first = text.find("#1  first prompt").expect("row 1");
		let second = text.find("#2  second prompt").expect("row 2");
		let third = text.find("#3  third \"quoted\"").expect("row 3");
		assert!(first < second && second < third, "rows are oldest first:\n{text}");
		assert!(!text.contains("with detail"), "row shows only the first line:\n{text}");
		assert!(text.contains("Rewind"), "header missing:\n{text}");
		assert!(text.contains(HEADER_CAPTION), "caption missing:\n{text}");
		assert!(
			text.contains("3/3  ↑/↓ step  ←/→ user turns  enter rewind  ctrl+o expand  esc cancel"),
			"hint missing:\n{text}"
		);
		assert!(text.contains("third \"quoted\""), "preview shows the outlined message:\n{text}");
	}

	#[test]
	fn enter_rewinds_to_the_cause_of_the_outlined_turn() {
		let session = session(&["first", "second\nline two", "third"]);
		let mut panel = open(&session).expect("panel");
		assert_eq!(panel.key(Key::Up), PanelEvent::Consumed);
		assert_eq!(panel.selected(), 1);
		let text = omp_tui::frame_text(panel.frame(Size { width: 100, height: 30 }));
		assert!(text.contains("line two"), "preview follows the cursor:\n{text}");
		assert!(text.contains("2/3  ↑/↓ step"), "position follows the cursor:\n{text}");
		let cause = turn_cause(session.dom(), 1);
		assert_eq!(
			panel.key(Key::Enter),
			PanelEvent::Finish(sf!("rewind {cause} \"second\\nline two\""))
		);
	}

	#[test]
	fn cursor_clamps_at_the_edges_and_arrows_step_turns() {
		let session = session(&["first", "second"]);
		let mut panel = open(&session).expect("panel");
		panel.key(Key::Up);
		assert_eq!(panel.selected(), 0);
		panel.key(Key::Left);
		assert_eq!(panel.selected(), 0, "Left clamps at the oldest turn");
		panel.key(Key::Right);
		assert_eq!(panel.selected(), 1);
		panel.key(Key::Down);
		assert_eq!(panel.selected(), 1, "Down clamps at the newest turn");
		let cause = turn_cause(session.dom(), 1);
		assert_eq!(panel.key(Key::Enter), PanelEvent::Finish(sf!("rewind {cause} \"second\"")));
	}

	#[test]
	fn typing_filters_the_list_and_enter_picks_the_match() {
		let session = session(&["alpha", "bravo", "charlie"]);
		let mut panel = open(&session).expect("panel");
		for ch in "bra".chars() {
			assert_eq!(panel.key(Key::Char(ch)), PanelEvent::Consumed);
		}
		let text = omp_tui::frame_text(panel.frame(Size { width: 100, height: 30 }));
		assert!(!text.contains("#3  charlie"), "filtered rows are hidden:\n{text}");
		assert_eq!(panel.selected(), 1);
		let cause = turn_cause(session.dom(), 1);
		assert_eq!(panel.key(Key::Enter), PanelEvent::Finish(sf!("rewind {cause} \"bravo\"")));
	}

	#[test]
	fn escape_closes_and_expand_grows_the_preview() {
		let session = session(&["one"]);
		let mut panel = open(&session).expect("panel");
		let collapsed = panel.frame(Size { width: 100, height: 30 }).size().height;
		assert_eq!(panel.action(PanelAction::Expand), PanelEvent::Consumed);
		let expanded = panel.frame(Size { width: 100, height: 30 }).size().height;
		assert!(expanded > collapsed, "ctrl+o grows the preview: {collapsed} -> {expanded}");
		assert_eq!(panel.action(PanelAction::Rename), PanelEvent::Ignored);
		assert_eq!(panel.key(Key::Esc), PanelEvent::Close);
	}

	#[test]
	fn quoted_text_round_trips_through_the_console_parser() {
		let escaped = escape_quoted("say \"hi\"\n\tback\\slash");
		assert_eq!(escaped.as_str(), "say \\\"hi\\\"\\n\\tback\\\\slash");
	}
}
