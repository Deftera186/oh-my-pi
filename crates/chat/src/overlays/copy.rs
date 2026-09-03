//! `/copy` picker (pi `modes/components/copy-selector.ts`): the transcript
//! itself, one message outlined, with `→` descending into that message's
//! inner blocks (fenced code, quotes, shell commands, tool output). Enter
//! copies and closes, exactly like pi's `onPick`; the close rides the
//! panel's `settled` hook so the host writes the clipboard first.
//!
//! `/copy code` and `/copy cmd` are one-shot host calls over the same
//! transcript walk ([`last_code_block`], [`last_command`]).

use std::time::Duration;

use omp_core::{Str, StrMut, sf};
use omp_dom::{Dom, KnownTag, Node, PropId, Tag, Value};
use omp_tui::{Frame, Icon, IntoComponent as _, Key, Prop, Size, Ui, UiContext, dom};

use super::{Panel, PanelAction, PanelAnchor, PanelEvent};
use crate::cards::Component;

/// Rows the frame chrome occupies: top rule, header, rule, footer hint,
/// bottom rule.
const CHROME_ROWS: u16 = 5;
/// Preview rows shown per block in the descended view; copy always takes
/// the full text.
const BLOCK_PREVIEW_LINES: usize = 12;
/// Result rows shown under a collapsed tool card.
const TOOL_PREVIEW_LINES: usize = 3;

/// One copyable inner block of a transcript message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyBlock {
	/// Short kind label (`rust code`, `bash command`, `read result`, …).
	pub label:    Str,
	/// Exact text placed on the clipboard.
	pub content:  Str,
	/// Highlight language for the block preview.
	pub language: Option<Str>,
}

/// What kind of command a `/copy cmd` hit came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandKind {
	/// A `bash` tool call.
	Bash,
	/// An `eval` tool call.
	Eval,
}

impl CommandKind {
	/// pi's status wording (`Copied bash command to clipboard`).
	#[must_use]
	pub const fn noun(self) -> &'static str {
		match self {
			Self::Bash => "bash command",
			Self::Eval => "eval code",
		}
	}
}

/// One rendered piece of a message.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Segment {
	User(Str),
	Thinking(Str),
	Assistant(Str),
	Tool { name: Str, status: Str, output: Str },
}

/// One selectable transcript message (pi `OutlineTarget`): a user prompt,
/// or an assistant message with the tool results it folded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyTarget {
	/// Clipboard label for the whole message.
	pub label:   Str,
	/// Clipboard text for the whole message.
	pub content: Str,
	/// Inner blocks reached with `→`.
	pub blocks:  Vec<CopyBlock>,
	segments:    Vec<Segment>,
}

/// Retained `/copy` picker.
pub struct CopySelector {
	ui:             Ui,
	ctx:            UiContext,
	targets:        Vec<CopyTarget>,
	selected:       usize,
	block_selected: Option<usize>,
	expanded:       bool,
	closing:        bool,
	width:          u16,
	rows:           u16,
}

impl CopySelector {
	/// Builds the picker over the session replica; `show_thinking` mirrors
	/// the transcript's reveal setting.
	#[must_use]
	pub fn open(dom: &Dom, show_thinking: bool, ctx: &UiContext) -> Self {
		let targets = collect_targets(dom, show_thinking);
		let mut panel = Self {
			ui: Ui::from_root(dom! { <col/> }, 80, ctx.clone()),
			ctx: ctx.clone(),
			selected: targets.len().saturating_sub(1),
			targets,
			block_selected: None,
			expanded: false,
			closing: false,
			width: 0,
			rows: 0,
		};
		panel.rebuild(80, 20);
		panel
	}

	/// Number of copyable messages; hosts skip mounting when zero.
	#[must_use]
	pub fn target_count(&self) -> usize {
		self.targets.len()
	}

	/// Footer hint as shown.
	#[must_use]
	pub fn hint(&self) -> Str {
		match self.block_selected {
			Some(index) => {
				let total = self.targets.get(self.selected).map_or(0, |target| target.blocks.len());
				sf!("{}/{total}  ↑/↓ block  ←/esc back  enter copy", index + 1)
			},
			None => {
				let blocks = self.targets.get(self.selected).map_or(0, |target| target.blocks.len());
				let mut hint = StrMut::new("");
				if !self.targets.is_empty() {
					hint.push_str(sf!("{}/{}  ", self.selected + 1, self.targets.len()).as_str());
				}
				hint.push_str("↑/↓ step  ");
				if blocks > 0 {
					hint.push_str("→ blocks  ");
				}
				hint.push_str("enter copy  ctrl+o expand  esc close");
				hint.freeze()
			},
		}
	}

	fn move_vertical(&mut self, delta: isize) -> PanelEvent {
		match self.block_selected {
			Some(index) => {
				let total = self.targets.get(self.selected).map_or(0, |target| target.blocks.len());
				if let Some(next) = index.checked_add_signed(delta).filter(|next| *next < total) {
					self.block_selected = Some(next);
					self.rebuild(self.width, self.rows);
				}
			},
			None => {
				if let Some(next) = self
					.selected
					.checked_add_signed(delta)
					.filter(|next| *next < self.targets.len())
				{
					self.selected = next;
					self.rebuild(self.width, self.rows);
				}
			},
		}
		PanelEvent::Consumed
	}

	fn descend(&mut self) -> PanelEvent {
		if self.block_selected.is_none()
			&& self
				.targets
				.get(self.selected)
				.is_some_and(|target| !target.blocks.is_empty())
		{
			self.block_selected = Some(0);
			self.rebuild(self.width, self.rows);
		}
		PanelEvent::Consumed
	}

	fn ascend(&mut self) -> PanelEvent {
		if self.block_selected.take().is_some() {
			self.rebuild(self.width, self.rows);
		}
		PanelEvent::Consumed
	}

	fn pick(&mut self) -> PanelEvent {
		let Some(target) = self.targets.get(self.selected) else {
			return PanelEvent::Consumed;
		};
		let content = match self.block_selected {
			Some(index) => match target.blocks.get(index) {
				Some(block) => block.content.clone(),
				None => return PanelEvent::Consumed,
			},
			None => target.content.clone(),
		};
		self.closing = true;
		PanelEvent::Copy(content)
	}

	fn rebuild(&mut self, width: u16, rows: u16) {
		self.width = width;
		self.rows = rows;
		let dot = self.ctx.charset.icon(Icon::Dot);
		let hint = self.hint();
		let expanded = self.expanded;
		let selected = self.selected;
		let block_selected = self.block_selected;
		let entries = self
			.targets
			.iter()
			.enumerate()
			.map(|(index, target)| {
				let id = sf!("turn-{index}");
				let descended = index == selected && block_selected.is_some();
				let outlined = index == selected && block_selected.is_none();
				let caption = if outlined && !target.blocks.is_empty() {
					let count = target.blocks.len();
					sf!("{count} block{} →", if count == 1 { "" } else { "s" })
				} else {
					Str::default()
				};
				let cards = descended
					.then(|| {
						let block = block_selected.unwrap_or_default();
						target
							.blocks
							.iter()
							.enumerate()
							.map(|(position, item)| {
								block_card(item, position, target.blocks.len(), position == block, dot)
							})
							.collect::<Vec<_>>()
					})
					.unwrap_or_default();
				let segments = target
					.segments
					.iter()
					.map(|segment| segment_view(segment, expanded))
					.collect::<Vec<_>>();
				(id, descended, outlined, caption, cards, segments)
			})
			.collect::<Vec<_>>();
		let tree = dom! {
			<box border=round pad-x=1>
				<col>
					<row gap=1>
						<icon name="copy"/>
						<text bold>{"Copy"}</text>
						<text fg=muted>{sf!("{dot}pick what to put on the clipboard")}</text>
					</row>
					<hr border=round/>
					<scroll id="copy" h={rows}>
						for (id, descended, outlined, caption, cards, segments) in entries {
							if descended {
								<col id={id} focus>
									for card in cards { {card} }
								</col>
							} else if outlined {
								<box id={id} focus border=round bc=ok title={caption}>
									<col>
										for segment in segments { {segment} }
									</col>
								</box>
							} else {
								<col id={id} focus pad-x=1>
									for segment in segments { {segment} }
								</col>
							}
						}
					</scroll>
					<text fg=muted truncate>{hint}</text>
				</col>
			</box>
		};
		self.ui = Ui::from_root(tree, width, self.ctx.clone());
		let _ = self.ui.focus_id(sf!("turn-{selected}").as_str());
	}
}

fn segment_view(segment: &Segment, expanded: bool) -> Component {
	match segment {
		Segment::User(text) => {
			let text = text.clone();
			dom! { <text bg=surface pad="1 1">{text}</text> }.into_component()
		},
		Segment::Thinking(text) => {
			let text = text.clone();
			dom! { <text fg=muted italic pad-x=1>{text}</text> }.into_component()
		},
		Segment::Assistant(text) => {
			let text = text.clone();
			dom! { <md pad-x=1>{text}</md> }.into_component()
		},
		Segment::Tool { name, status, output } => {
			let header = sf!("{name} {status}");
			let shown = if expanded {
				output.clone()
			} else {
				preview(output, TOOL_PREVIEW_LINES)
			};
			dom! {
				<col pad-x=1>
					<text fg=muted>{header}</text>
					if !shown.is_empty() { <pre fg=muted>{shown}</pre> }
				</col>
			}
			.into_component()
		},
	}
}

fn block_card(
	block: &CopyBlock,
	index: usize,
	total: usize,
	selected: bool,
	dot: &str,
) -> Component {
	let lines = block.content.lines().count().max(1);
	let caption = sf!(
		"{}/{total}{dot}{}{dot}{lines} line{}",
		index + 1,
		block.label,
		if lines == 1 { "" } else { "s" }
	);
	let shown = preview(&block.content, BLOCK_PREVIEW_LINES);
	if selected {
		dom! {
			<box border=round bc=ok title={caption}>
				<pre>{shown}</pre>
			</box>
		}
		.into_component()
	} else {
		dom! {
			<col pad-x=1>
				<text fg=muted>{caption}</text>
				<pre>{shown}</pre>
			</col>
		}
		.into_component()
	}
}

/// The first `limit` lines of `text` plus pi's `… +N more lines` tail.
fn preview(text: &str, limit: usize) -> Str {
	let total = text.lines().count();
	if total <= limit {
		return Str::new(text);
	}
	let mut out = StrMut::new("");
	for line in text.lines().take(limit) {
		out.push_str(line);
		out.push_str("\n");
	}
	out.push_str(sf!("… +{} more lines", total - limit).as_str());
	out.freeze()
}

impl Panel for CopySelector {
	fn id(&self) -> &'static str {
		"copy"
	}

	fn anchor(&self) -> PanelAnchor {
		PanelAnchor::Full
	}

	fn action(&mut self, action: PanelAction) -> PanelEvent {
		match action {
			PanelAction::Expand => {
				self.expanded = !self.expanded;
				self.rebuild(self.width, self.rows);
				PanelEvent::Consumed
			},
			_ => PanelEvent::Ignored,
		}
	}

	fn key(&mut self, key: Key) -> PanelEvent {
		match key {
			Key::Esc => {
				if self.block_selected.is_some() {
					self.ascend()
				} else {
					PanelEvent::Close
				}
			},
			Key::Up | Key::Char('k') => self.move_vertical(-1),
			Key::Down | Key::Char('j') => self.move_vertical(1),
			Key::Right => self.descend(),
			Key::Left => self.ascend(),
			Key::Enter => self.pick(),
			Key::PageUp | Key::PageDown | Key::Home | Key::End | Key::SelectUp | Key::SelectDown => {
				let _ = self.ui.handle_key(key);
				PanelEvent::Consumed
			},
			_ => PanelEvent::Consumed,
		}
	}

	fn frame(&mut self, viewport: Size) -> &Frame {
		let rows = viewport.height.saturating_sub(CHROME_ROWS).max(3);
		if viewport.width != self.width {
			self.rebuild(viewport.width, rows);
		} else if rows != self.rows {
			self.rows = rows;
			self.ui.set_prop("copy", Prop::H, rows);
		}
		self.ui.frame()
	}

	fn tick(&mut self, _now: Duration) -> bool {
		self.closing
	}

	fn next_wake(&self) -> Option<Duration> {
		self.closing.then_some(Duration::ZERO)
	}

	fn settled(&mut self) -> Option<PanelEvent> {
		self.closing.then_some(PanelEvent::Close)
	}
}

/// Walks the replica into pi's outline targets: every user prompt, and
/// every assistant message with the tool results it folded.
#[must_use]
pub fn collect_targets(dom: &Dom, show_thinking: bool) -> Vec<CopyTarget> {
	let mut targets = Vec::new();
	for turn in dom.children(dom.body()) {
		if dom.get(*turn).is_none_or(|node| node.tag != Tag::Known(KnownTag::Turn)) {
			continue;
		}
		let mut open: Option<CopyTarget> = None;
		for handle in dom.children(*turn) {
			let Some(node) = dom.get(*handle) else {
				continue;
			};
			match &node.tag {
				Tag::Known(KnownTag::User) => {
					targets.extend(open.take());
					let text = node.content.clone().unwrap_or_default();
					let mut blocks = Vec::new();
					push_markdown_blocks(&mut blocks, text.as_str());
					targets.push(CopyTarget {
						label: Str::new_static("user message"),
						content: text.clone(),
						blocks,
						segments: vec![Segment::User(text)],
					});
				},
				Tag::Known(KnownTag::Assistant) => {
					targets.extend(open.take());
					let text = live_text(dom, *handle, node, PropId::Text).unwrap_or_default();
					let mut segments = Vec::new();
					if show_thinking
						&& let Some(thinking) = live_text(dom, *handle, node, PropId::Thinking)
						&& !thinking.is_empty()
					{
						segments.push(Segment::Thinking(thinking));
					}
					if !text.is_empty() {
						segments.push(Segment::Assistant(text.clone()));
					}
					let mut blocks = Vec::new();
					push_markdown_blocks(&mut blocks, text.as_str());
					let trimmed = Str::new(text.trim());
					open = Some(CopyTarget {
						label: Str::new_static("assistant message"),
						content: trimmed,
						blocks,
						segments,
					});
				},
				Tag::Custom(tool) => {
					let Some(input) = child(dom, *handle, KnownTag::Input) else {
						continue;
					};
					let status = prop_text(node, PropId::Status).unwrap_or_else(|| Str::new_static("running"));
					let result = child(dom, *handle, KnownTag::Result).and_then(result_text);
					let target = open.get_or_insert_with(|| CopyTarget {
						label:    Str::new_static("turn content"),
						content:  Str::default(),
						blocks:   Vec::new(),
						segments: Vec::new(),
					});
					if let Some((kind, code, language)) = command_of(tool.as_str(), input) {
						target.blocks.push(CopyBlock {
							label: Str::new_static(kind.noun()),
							content: code,
							language,
						});
					}
					if let Some(result) = &result {
						target.blocks.push(CopyBlock {
							label:    sf!("{tool} result"),
							content:  result.clone(),
							language: None,
						});
					}
					target.segments.push(Segment::Tool {
						name:   tool.clone(),
						status: status.clone(),
						output: result.unwrap_or_default(),
					});
				},
				_ => {},
			}
		}
		targets.extend(open.take());
	}
	for target in &mut targets {
		if target.content.is_empty() {
			// No direct prose (a pure tool turn): fall back to its blocks joined.
			let mut joined = StrMut::new("");
			for (index, block) in target.blocks.iter().enumerate() {
				if index > 0 {
					joined.push_str("\n\n");
				}
				joined.push_str(block.content.as_str());
			}
			target.content = joined.freeze();
			target.label = Str::new_static("turn content");
		}
	}
	targets.retain(|target| !target.segments.is_empty());
	targets
}

/// The last fenced code block of any assistant message (pi
/// `extractLastCodeBlock`).
#[must_use]
pub fn last_code_block(dom: &Dom) -> Option<CopyBlock> {
	let mut last = None;
	for turn in dom.children(dom.body()) {
		for handle in dom.children(*turn) {
			let Some(node) = dom.get(*handle) else {
				continue;
			};
			if node.tag != Tag::Known(KnownTag::Assistant) {
				continue;
			}
			let Some(text) = live_text(dom, *handle, node, PropId::Text) else {
				continue;
			};
			let mut blocks = Vec::new();
			push_markdown_blocks(&mut blocks, text.as_str());
			if let Some(block) = blocks.into_iter().rev().find(|block| block.language.is_some() || block.label.ends_with("code")) {
				last = Some(block);
			}
		}
	}
	last
}

/// The last `bash`/`eval` tool call's command text (pi
/// `extractLastCommand`).
#[must_use]
pub fn last_command(dom: &Dom) -> Option<(CommandKind, Str)> {
	let mut last = None;
	for turn in dom.children(dom.body()) {
		for handle in dom.children(*turn) {
			let Some(node) = dom.get(*handle) else {
				continue;
			};
			let Tag::Custom(tool) = &node.tag else {
				continue;
			};
			let Some(input) = child(dom, *handle, KnownTag::Input) else {
				continue;
			};
			if let Some((kind, code, _)) = command_of(tool.as_str(), input) {
				last = Some((kind, code));
			}
		}
	}
	last
}

fn command_of(tool: &str, input: &Node) -> Option<(CommandKind, Str, Option<Str>)> {
	let args: serde_json::Value = serde_json::from_str(node_json(input)?).ok()?;
	match tool {
		"bash" => {
			let command = args.get("command")?.as_str()?;
			Some((CommandKind::Bash, Str::new(command), Some(Str::new_static("bash"))))
		},
		"eval" => {
			let code = args.get("code")?.as_str()?;
			let language = args
				.get("language")
				.and_then(serde_json::Value::as_str)
				.map(Str::new);
			Some((CommandKind::Eval, Str::new(code), language))
		},
		_ => None,
	}
}

/// Model-facing text of a tool result: a JSON `text`/`output` field when
/// the outcome is an object carrying one, else the raw outcome.
fn result_text(node: &Node) -> Option<Str> {
	let raw = node_json(node)?;
	let text = serde_json::from_str::<serde_json::Value>(raw)
		.ok()
		.and_then(|value| {
			value
				.get("text")
				.or_else(|| value.get("output"))
				.and_then(serde_json::Value::as_str)
				.map(str::to_owned)
		})
		.unwrap_or_else(|| raw.to_owned());
	let text = text.trim();
	(!text.is_empty()).then(|| Str::new(text))
}

fn node_json(node: &Node) -> Option<&str> {
	match node.prop(&PropId::Data.into()) {
		Some(Value::Json(value)) => Some(value.get()),
		_ => node
			.prop(&PropId::Text.into())
			.and_then(Value::as_str)
			.filter(|text| !text.is_empty())
			.or(node.content.as_deref()),
	}
}

/// pi `extractBlocks`: fenced code blocks and blockquotes, in order.
fn push_markdown_blocks(blocks: &mut Vec<CopyBlock>, text: &str) {
	let mut fence: Option<(Str, StrMut)> = None;
	let mut quote: Option<StrMut> = None;
	let flush_quote = |quote: &mut Option<StrMut>, blocks: &mut Vec<CopyBlock>| {
		if let Some(quote) = quote.take() {
			let content = quote.freeze();
			if !content.trim().is_empty() {
				blocks.push(CopyBlock {
					label:    Str::new_static("quote"),
					content:  Str::new(content.trim_end()),
					language: None,
				});
			}
		}
	};
	for line in text.lines() {
		if let Some((language, body)) = fence.as_mut() {
			if line.trim_start().starts_with("```") {
				let code = Str::new(body.as_str().trim_end_matches('\n'));
				let label = if language.is_empty() {
					Str::new_static("code")
				} else {
					sf!("{language} code")
				};
				blocks.push(CopyBlock {
					label,
					content: code,
					language: (!language.is_empty()).then(|| language.clone()),
				});
				fence = None;
			} else {
				body.push_str(line);
				body.push_str("\n");
			}
			continue;
		}
		if let Some(rest) = line.trim_start().strip_prefix("```") {
			flush_quote(&mut quote, blocks);
			let language = rest.trim().split_whitespace().next().unwrap_or_default();
			fence = Some((Str::new(language), StrMut::new("")));
			continue;
		}
		if let Some(rest) = line.trim_start().strip_prefix('>') {
			let body = quote.get_or_insert_with(|| StrMut::new(""));
			body.push_str(rest.strip_prefix(' ').unwrap_or(rest));
			body.push_str("\n");
			continue;
		}
		flush_quote(&mut quote, blocks);
	}
	flush_quote(&mut quote, blocks);
	if let Some((language, body)) = fence {
		let code = Str::new(body.as_str().trim_end_matches('\n'));
		if !code.is_empty() {
			let label = if language.is_empty() {
				Str::new_static("code")
			} else {
				sf!("{language} code")
			};
			blocks.push(CopyBlock {
				label,
				content: code,
				language: (!language.is_empty()).then(|| language),
			});
		}
	}
}

fn child<'a>(dom: &'a Dom, parent: omp_dom::Handle, tag: KnownTag) -> Option<&'a Node> {
	dom
		.children(parent)
		.iter()
		.filter_map(|handle| dom.get(*handle))
		.find(|node| node.tag == Tag::Known(tag))
}

fn live_text(dom: &Dom, handle: omp_dom::Handle, node: &Node, prop: PropId) -> Option<Str> {
	let key: omp_dom::PropKey = prop.into();
	match dom.stream_text(handle, &key) {
		Some(text) => Some(Str::new(text)),
		None => prop_text(node, prop),
	}
}

fn prop_text(node: &Node, prop: PropId) -> Option<Str> {
	node
		.prop(&prop.into())
		.and_then(Value::as_str)
		.map(Str::new)
}

#[cfg(test)]
mod tests {
	use omp_session::{ComponentRegistry, Session};
	use omp_tui::frame_text;

	use super::*;

	const FENCE: &str = "fn main() {\n    println!(\"hi\");\n}";

	fn session(with_bash: bool) -> Session {
		let directory = tempfile::tempdir().expect("temp directory");
		let path = directory.keep().join("copy.oms");
		let mut session = Session::create(path, ComponentRegistry::standard()).expect("session");
		session.begin_turn().expect("turn");
		session.user("show me main", Vec::new()).expect("user");
		session
			.assistant_start("test/model", "test", "test/model")
			.expect("assistant");
		let turn = *session
			.dom()
			.children(session.dom().body())
			.last()
			.expect("turn");
		let assistant = session
			.dom()
			.children(turn)
			.iter()
			.copied()
			.find(|handle| {
				session
					.dom()
					.get(*handle)
					.is_some_and(|node| node.tag == Tag::Known(KnownTag::Assistant))
			})
			.expect("assistant handle");
		let text = session
			.stream_open(assistant, PropId::Text.into())
			.expect("text stream");
		session
			.stream_append(text, &format!("Here it is:\n\n```rust\n{FENCE}\n```\n"))
			.expect("text");
		session.stream_close(text).expect("close");
		session
			.assistant_end(if with_bash { "tool_calls" } else { "stop" })
			.expect("end");
		if with_bash {
			let args = serde_json::value::to_raw_value(&serde_json::json!({"command":"cargo test"}))
				.expect("args");
			let call = session
				.call("bash", 1, "call-1", Some("run tests".into()), Some(args), None)
				.expect("call");
			let outcome = serde_json::value::to_raw_value(&serde_json::json!({"output":"ok"}))
				.expect("outcome");
			session.settle(call, outcome).expect("settle");
		}
		session
	}

	#[test]
	fn right_descends_into_the_code_block_and_enter_copies_it() {
		let session = session(false);
		let mut panel = CopySelector::open(session.dom(), true, &UiContext::default());
		assert_eq!(panel.id(), "copy");
		assert_eq!(panel.target_count(), 2);
		let text = frame_text(panel.frame(Size { width: 80, height: 24 }));
		assert!(text.contains("pick what to put on the clipboard"), "header missing:\n{text}");
		assert!(text.contains("2/2  ↑/↓ step  → blocks  enter copy  ctrl+o expand  esc close"), "hint:\n{text}");
		assert_eq!(panel.key(Key::Right), PanelEvent::Consumed);
		let text = frame_text(panel.frame(Size { width: 80, height: 24 }));
		assert!(text.contains("1/1 · rust code · 3 lines"), "block caption missing:\n{text}");
		assert!(text.contains("1/1  ↑/↓ block  ←/esc back  enter copy"), "block hint:\n{text}");
		assert_eq!(panel.key(Key::Enter), PanelEvent::Copy(Str::new_static(FENCE)));
		assert_eq!(panel.next_wake(), Some(Duration::ZERO));
		assert!(panel.tick(Duration::from_millis(1)));
		assert_eq!(panel.settled(), Some(PanelEvent::Close));
	}

	#[test]
	fn whole_message_copy_and_escape_ladder() {
		let session = session(true);
		let mut panel = CopySelector::open(session.dom(), true, &UiContext::default());
		assert_eq!(panel.key(Key::Up), PanelEvent::Consumed);
		assert_eq!(panel.key(Key::Enter), PanelEvent::Copy(Str::new_static("show me main")));
		let mut panel = CopySelector::open(session.dom(), true, &UiContext::default());
		assert_eq!(panel.key(Key::Right), PanelEvent::Consumed);
		assert_eq!(panel.key(Key::Esc), PanelEvent::Consumed);
		assert_eq!(panel.key(Key::Esc), PanelEvent::Close);
		let blocks = &panel.targets[1].blocks;
		assert_eq!(blocks.len(), 3, "{blocks:?}");
		assert_eq!(blocks[1].label, "bash command");
		assert_eq!(blocks[1].content, "cargo test");
		assert_eq!(blocks[2].label, "bash result");
		assert_eq!(blocks[2].content, "ok");
	}

	#[test]
	fn last_code_block_and_last_command_scan_the_transcript() {
		let with = session(true);
		let block = last_code_block(with.dom()).expect("code block");
		assert_eq!(block.content, FENCE);
		assert_eq!(block.language.as_deref(), Some("rust"));
		let (kind, code) = last_command(with.dom()).expect("command");
		assert_eq!(kind, CommandKind::Bash);
		assert_eq!(code, "cargo test");
		let without = session(false);
		assert_eq!(last_command(without.dom()), None);
	}

	#[test]
	fn markdown_blocks_extract_fences_and_quotes_in_order() {
		let mut blocks = Vec::new();
		push_markdown_blocks(&mut blocks, "> quoted\n> lines\n\n```\nplain\n```\n```py\nx = 1\n```");
		assert_eq!(blocks.iter().map(|block| block.label.as_str()).collect::<Vec<_>>(), [
			"quote", "code", "py code"
		]);
		assert_eq!(blocks[0].content, "quoted\nlines");
		assert_eq!(blocks[1].content, "plain");
		assert_eq!(blocks[2].content, "x = 1");
	}
}
