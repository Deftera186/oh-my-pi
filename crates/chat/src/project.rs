//! Pure projection from an actor-owned session DOM replica to transcript
//! blocks.

use omp_core::{Str, StrMut};
use omp_dom::{Dom, Handle, KnownTag, Node, PropId, Tag, Value};
use omp_tui::{IntoComponent, UiContext, dom, slots::Mode};

use crate::{
	cards::{CardRegistry, CardStatus, CardView, Component},
	notices::{cache, divider, error, misc, usage},
	thinking,
	transcript::{Local, REVEAL_HORIZON},
};

/// Semantic transcript block class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockKind {
	/// Host-owned welcome banner shown before the first turn.
	Welcome,
	/// User-authored message.
	User,
	/// Assistant reasoning, controlled by the observer-local reveal setting.
	Thinking,
	/// Visible assistant answer.
	Assistant,
	/// Tool element rendered by the card registry.
	Tool,
	/// Controller notice.
	Notice,
	/// Turn receipt.
	Usage,
	/// Maintenance divider: compaction, handoff, branch summary, or a
	/// prompt-cache miss marker.
	Divider,
}

/// Test- and status-facing description of one projected block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockView {
	/// Stable observer-local identity derived from the DOM handle and block
	/// kind.
	pub key:       u64,
	/// Semantic block class.
	pub kind:      BlockKind,
	/// Plain semantic text represented by this block.
	pub text:      Str,
	/// Slot update mode.
	pub mode:      Mode,
	/// Whether the block may retire into history.
	pub finalized: bool,
}

/// One rendered block ready for admission to the slot engine.
pub(crate) struct RenderedBlock {
	pub view:      BlockView,
	pub component: Component,
	/// Streamed text owned by the component's [`omp_tui::slots::STREAM_ID`]
	/// child. A later projection whose stream extends this one is applied in
	/// place, keeping the reveal cursor and animation phase.
	pub stream:    Option<Str>,
}

/// Observer-local switches the projection reads (never DOM state).
#[derive(Clone, Copy)]
pub struct Options<'a> {
	/// Reveal reasoning text (`cl_showthinking`).
	pub show_thinking: bool,
	/// Expand tool cards (`cl_tools_expanded`).
	pub expanded:      bool,
	/// Type streamed text out at the reveal cadence (`cl_smooth_streaming`).
	pub smooth:        bool,
	/// Collapse fenced code in reasoning to an ellipsis
	/// (`cl_thinking_prose_only`).
	pub prose_only:    bool,
	/// Tool start instants, speed gauge, and reset banner.
	pub local:         &'a Local,
}

impl<'a> Options<'a> {
	/// pi's defaults: thinking shown, cards collapsed, smooth streaming and
	/// prose-only reasoning on.
	#[must_use]
	pub const fn new(local: &'a Local) -> Self {
		Self { show_thinking: true, expanded: false, smooth: true, prose_only: true, local }
	}
}

/// Projects descriptors without constructing terminal components.
#[must_use]
pub fn block_views(dom: &Dom, show_thinking: bool) -> Vec<BlockView> {
	let local = Local::default();
	let options = Options { show_thinking, ..Options::new(&local) };
	project(dom, &CardRegistry::standard(), &UiContext::default(), &options)
		.into_iter()
		.map(|block| block.view)
		.collect()
}

pub(crate) fn project(
	dom: &Dom,
	cards: &CardRegistry,
	ui: &UiContext,
	options: &Options<'_>,
) -> Vec<RenderedBlock> {
	let mut blocks = Vec::new();
	if let Some(banner) = options.local.banner() {
		blocks.push(banner_block(banner.key, banner.text.clone()));
	}
	let turns = dom.children(dom.body());
	let cache_misses = cache::cache_invalidations(dom);
	for (index, turn) in turns.iter().enumerate() {
		let Some(turn_node) = dom.get(*turn) else {
			continue;
		};
		if turn_node.tag != Tag::Known(KnownTag::Turn) {
			continue;
		}
		let start = blocks.len();
		let last_turn = index + 1 == turns.len();
		for handle in dom.children(*turn) {
			let Some(node) = dom.get(*handle) else {
				continue;
			};
			match &node.tag {
				Tag::Known(KnownTag::User) => {
					let text = node.content.clone().unwrap_or_default();
					let component: Component = if crate::notices::prop_bool(node, PropId::Synthetic) {
						misc::synthetic_row(text.as_str(), options.expanded)
					} else if let Some(author) = crate::notices::prop_text(node, PropId::Author) {
						misc::guest_bubble(author.as_str(), text.clone())
					} else {
						user_bubble(text.clone()).into_component()
					};
					blocks.push(rendered(
						*handle,
						BlockKind::User,
						text,
						Mode::Mutable,
						true,
						component,
					));
				},
				Tag::Known(KnownTag::Assistant) => {
					assistant_blocks(dom, *handle, node, options, &mut blocks);
				},
				Tag::Known(KnownTag::Notice) => {
					let text = node.content.clone().unwrap_or_default();
					let kind = prop_text(node, PropId::Kind).unwrap_or_else(|| Str::new_static("info"));
					// ERR-06: while the identical error is pinned above the editor the
					// inline copy is suppressed; ctrl+o draws it in full anyway.
					if !options.expanded && error::suppressed_inline(dom, *handle) {
						continue;
					}
					let component = misc::custom_notice(kind.as_str(), node)
						.unwrap_or_else(|| error::notice_card(kind.as_str(), text.clone(), options.expanded));
					blocks.push(rendered(*handle, BlockKind::Notice, text, Mode::Mutable, true, component));
				},
				Tag::Known(KnownTag::Usage) => {
					let facts = usage::usage_facts(dom, *handle);
					let text = usage::usage_line(&facts, ui);
					blocks.push(rendered(
						*handle,
						BlockKind::Usage,
						text,
						Mode::Mutable,
						true,
						usage::usage_block(&facts, ui),
					));
					// CSH-01: the cache-miss marker trails the turn whose request
					// lost the prompt cache.
					if let Some((_, miss)) = cache_misses.iter().find(|(usage, _)| usage == handle) {
						blocks.push(rendered(
							*handle,
							BlockKind::Divider,
							Str::new(format!("cache miss · {} tokens", miss.reprocessed_tokens)),
							Mode::Mutable,
							true,
							cache::cache_miss_marker(miss),
						));
					}
				},
				Tag::Custom(tool) => {
					if let Some(block) = tool_block(dom, *handle, node, tool, cards, ui, options) {
						blocks.push(block);
					}
				},
				_ => {},
			}
		}
		displace_cards(dom, &mut blocks, start, last_turn);
		group_reads(dom, cards, ui, options, &mut blocks, start);
		// CMP-01..06: maintenance dividers land after the turn holding their
		// boundary entry.
		for (compaction, component) in divider::compaction_dividers(dom, *turn, options.expanded) {
			let label = dom
				.get(compaction)
				.map(|node| divider::SummaryDivider::compaction(node, options.expanded).label)
				.unwrap_or_default();
			blocks.push(rendered(compaction, BlockKind::Divider, label, Mode::Mutable, true, component));
		}
	}
	blocks
}

/// Assistant reasoning and answer blocks (pi `AssistantMessageComponent`).
///
/// Reasoning shown: an append-only head (`<text reveal>`) whose stable
/// prefix may retire into scrollback mid-stream; reasoning hidden while the
/// model is still reasoning: the breathing starburst pulse with the speed
/// badge; the answer: a mutable markdown block typed out through the reveal
/// cursor, snapped to its full text once a tool call starts (a transcript
/// order boundary) or the message ends.
fn assistant_blocks(
	dom: &Dom,
	handle: Handle,
	node: &Node,
	options: &Options<'_>,
	blocks: &mut Vec<RenderedBlock>,
) {
	let finalized = node.prop(&PropId::StopReason.into()).is_some();
	let raw_thinking = live_text(dom, handle, node, PropId::Thinking).unwrap_or_default();
	let text = live_text(dom, handle, node, PropId::Text).unwrap_or_default();
	let thinking = thinking::display_thinking(&raw_thinking, options.prose_only);
	let thinking = Str::new(thinking.as_str().trim());
	let has_thinking = thinking::is_displayable(raw_thinking.as_str(), thinking.as_str());
	let tool_started = has_tool_calls(dom, handle);
	let reveal = options.smooth && !finalized && !tool_started;
	if options.show_thinking && has_thinking {
		let component = if reveal {
			dom! { <text id={omp_tui::slots::STREAM_ID} reveal={REVEAL_HORIZON_PROP} fg=muted italic pad-x=1>{thinking.clone()}</text> }
		} else {
			dom! { <text id={omp_tui::slots::STREAM_ID} fg=muted italic pad-x=1>{thinking.clone()}</text> }
		};
		let mut block = rendered(
			handle,
			BlockKind::Thinking,
			thinking.clone(),
			Mode::AppendOnly,
			finalized,
			component,
		);
		block.stream = Some(thinking.clone());
		blocks.push(block);
	} else if !options.show_thinking
		&& !finalized
		&& !tool_started
		&& thinking::has_content(raw_thinking.as_str())
		&& !thinking::has_content(text.as_str())
	{
		let local = options.local;
		let pulse = omp_tui::components::Pulse::new()
			.label(" Thinking")
			.count(local.thinking_tokens())
			.gauge(local.gauge().clone(), "toks/s")
			.with(omp_tui::Prop::Fg, "secondary");
		blocks.push(rendered(
			handle,
			BlockKind::Thinking,
			Str::new_static("Thinking"),
			Mode::Mutable,
			false,
			dom! { <row pad-x=1>{pulse}</row> },
		));
	}
	if !text.is_empty() || finalized {
		let component = if reveal {
			dom! { <md id={omp_tui::slots::STREAM_ID} reveal={REVEAL_HORIZON_PROP} pad-x=1>{text.clone()}</md> }
		} else {
			dom! { <md id={omp_tui::slots::STREAM_ID} pad-x=1>{text.clone()}</md> }
		};
		let mut block =
			rendered(handle, BlockKind::Assistant, text.clone(), Mode::Mutable, finalized, component);
		block.stream = Some(text);
		blocks.push(block);
	}
}

/// `reveal` prop spelling of [`REVEAL_HORIZON`].
const REVEAL_HORIZON_PROP: &str = "264ms";
const _: () = assert!(REVEAL_HORIZON.as_millis() == 264);

/// Whether a tool element follows this assistant message in its turn (pi
/// `splitAssistantMessageToolTimeline().hasToolCalls`).
fn has_tool_calls(dom: &Dom, assistant: Handle) -> bool {
	let Some(turn) = dom.parent(assistant) else {
		return false;
	};
	let siblings = dom.children(turn);
	let Some(position) = siblings.iter().position(|handle| *handle == assistant) else {
		return false;
	};
	siblings[position + 1..]
		.iter()
		.filter_map(|handle| dom.get(*handle))
		.any(|node| matches!(node.tag, Tag::Custom(_)))
}

/// Observer-local transcript row after a session reset (pi `present([new
/// Spacer(1), new Text(success + label, 1, 1)])`).
fn banner_block(key: u64, text: Str) -> RenderedBlock {
	let component = dom! {
		<row gap=1 pad-x=1 fg=accent><icon name="success"/><text>{text.clone()}</text></row>
	};
	RenderedBlock {
		view:      BlockView {
			key,
			kind: BlockKind::Notice,
			text,
			mode: Mode::Mutable,
			finalized: true,
		},
		component: component.into_component(),
		stream:    None,
	}
}

/// pi `displaceableByToolName`: a waiting `hub` poll card is displaced by
/// the next `hub` call, and a `todo` snapshot card by the next `todo` call
/// or the next user prompt, so the transcript keeps one live copy.
fn displace_cards(dom: &Dom, blocks: &mut Vec<RenderedBlock>, start: usize, last_turn: bool) {
	let displaceable = |block: &RenderedBlock| -> Option<&'static str> {
		if block.view.kind != BlockKind::Tool {
			return None;
		}
		let handle = Handle::new(block.view.key / 8)?;
		let node = dom.get(handle)?;
		let Tag::Custom(tool) = &node.tag else {
			return None;
		};
		match tool.as_str() {
			"todo" => Some("todo"),
			"hub" if hub_is_wait(dom, handle, node) => Some("hub"),
			_ => None,
		}
	};
	let mut keep = vec![true; blocks.len()];
	let mut latest: [Option<usize>; 2] = [None, None];
	for index in start..blocks.len() {
		let Some(name) = displaceable(&blocks[index]) else {
			continue;
		};
		let slot = usize::from(name == "hub");
		if let Some(previous) = latest[slot].replace(index) {
			keep[previous] = false;
		}
	}
	// A todo snapshot is also displaced by the next user prompt: only the
	// newest turn keeps its last one.
	if !last_turn && let Some(index) = latest[0] {
		keep[index] = false;
	}
	let mut position = 0;
	blocks.retain(|_| {
		let kept = keep[position];
		position += 1;
		kept
	});
}

/// Whether a `hub` call is a waiting poll (`op` = `wait`).
fn hub_is_wait(dom: &Dom, handle: Handle, _node: &Node) -> bool {
	child(dom, handle, KnownTag::Input)
		.and_then(|input| {
			let raw = match input.prop(&PropId::Data.into()) {
				Some(Value::Json(value)) => value.get().to_owned(),
				_ => input.prop(&PropId::Text.into()).and_then(Value::as_str)?.to_owned(),
			};
			serde_json::from_str::<serde_json::Value>(&raw).ok()
		})
		.and_then(|args| args.get("op").and_then(serde_json::Value::as_str).map(str::to_owned))
		.is_some_and(|op| op == "wait")
}

/// pi `read-tool-group.ts`: consecutive `read` calls in one turn collapse
/// into one compact tree block, and when the turn contains only reads the
/// turn's usage row attaches to the group instead of standing alone.
fn group_reads(
	dom: &Dom,
	cards: &CardRegistry,
	ui: &UiContext,
	options: &Options<'_>,
	blocks: &mut Vec<RenderedBlock>,
	start: usize,
) {
	let is_read = |block: &RenderedBlock| {
		block.view.kind == BlockKind::Tool
			&& Handle::new(block.view.key / 8)
				.and_then(|handle| dom.get(handle))
				.is_some_and(|node| matches!(&node.tag, Tag::Custom(tool) if tool.as_str() == "read"))
	};
	let reads_only = blocks[start..]
		.iter()
		.all(|block| is_read(block) || matches!(block.view.kind, BlockKind::User | BlockKind::Usage));
	let mut index = start;
	while index < blocks.len() {
		if !is_read(&blocks[index]) {
			index += 1;
			continue;
		}
		let mut end = index + 1;
		while end < blocks.len() && is_read(&blocks[end]) {
			end += 1;
		}
		if end - index < 2 {
			index = end;
			continue;
		}
		let handles = blocks[index..end]
			.iter()
			.filter_map(|block| Handle::new(block.view.key / 8))
			.collect::<Vec<_>>();
		let usage = if reads_only {
			blocks[end..]
				.iter()
				.position(|block| block.view.kind == BlockKind::Usage)
				.map(|offset| end + offset)
		} else {
			None
		};
		let usage_line = usage.map(|at| blocks[at].view.text.clone());
		let group = read_group_block(dom, cards, ui, options, &handles, usage_line);
		let mut text = StrMut::new("");
		for block in &blocks[index..end] {
			text.push_str(block.view.text.as_str());
			text.push_str("\n");
		}
		let finalized = blocks[index..end].iter().all(|block| block.view.finalized);
		let view = BlockView {
			key: blocks[index].view.key,
			kind: BlockKind::Tool,
			text: text.freeze(),
			mode: Mode::Mutable,
			finalized,
		};
		if let Some(at) = usage {
			blocks.remove(at);
		}
		blocks.splice(index..end, [RenderedBlock { view, component: group, stream: None }]);
		index += 1;
	}
}

fn read_group_block(
	dom: &Dom,
	cards: &CardRegistry,
	ui: &UiContext,
	options: &Options<'_>,
	handles: &[Handle],
	usage: Option<Str>,
) -> Component {
	let views = handles
		.iter()
		.filter_map(|handle| {
			let node = dom.get(*handle)?;
			card_view(dom, *handle, node, options)
		})
		.collect::<Vec<_>>();
	let _ = cards;
	crate::cards::read::render_calls_group(&views, options.expanded, usage, ui)
}

fn card_view<'a>(
	dom: &'a Dom,
	handle: Handle,
	node: &'a Node,
	options: &Options<'_>,
) -> Option<CardView<'a>> {
	let input = child(dom, handle, KnownTag::Input)?;
	let status = node
		.prop(&PropId::Status.into())
		.and_then(Value::as_str)
		.unwrap_or("running");
	let card_status = CardStatus::from_dom(status);
	let started = (card_status == CardStatus::InProgress)
		.then(|| options.local.started(block_key(handle, BlockKind::Tool)))
		.flatten();
	let result = dom
		.children(handle)
		.iter()
		.copied()
		.find(|child| dom.get(*child).is_some_and(|node| node.tag == Tag::Known(KnownTag::Result)));
	Some(CardView {
		input,
		result: result.and_then(|handle| dom.get(handle)),
		diag: child(dom, handle, KnownTag::Diag),
		usage: child(dom, handle, KnownTag::Usage),
		status: card_status,
		output: result.and_then(|handle| dom.stream_text(handle, &PropId::Text.into())),
		started,
	})
}

/// User message: pi paints the text on the `userMessageBg` tint with one
/// cell of padding on every side (`new Markdown(text, 1, 1, …)` in
/// `user-message.ts`: a tinted blank row above and below) and no border.
fn user_bubble(text: Str) -> impl IntoComponent {
	dom! { <text zone=prompt bg=surface pad="1 1">{text}</text> }
}

fn rendered(
	handle: Handle,
	kind: BlockKind,
	text: Str,
	mode: Mode,
	finalized: bool,
	component: impl IntoComponent,
) -> RenderedBlock {
	RenderedBlock {
		view:      BlockView { key: block_key(handle, kind), kind, text, mode, finalized },
		component: component.into_component(),
		stream:    None,
	}
}

/// Stable observer-local block identity: the DOM handle times eight plus a
/// kind suffix, so the handle is recoverable as `key / 8`.
pub(crate) const fn block_key(handle: Handle, kind: BlockKind) -> u64 {
	let suffix = match kind {
		BlockKind::Welcome | BlockKind::User => 0,
		BlockKind::Thinking => 1,
		BlockKind::Assistant => 2,
		BlockKind::Tool => 3,
		BlockKind::Notice => 4,
		BlockKind::Usage => 5,
		BlockKind::Divider => 6,
	};
	handle.get().saturating_mul(8).saturating_add(suffix)
}

fn tool_block(
	dom: &Dom,
	handle: Handle,
	node: &Node,
	tool: &Str,
	cards: &CardRegistry,
	ui: &UiContext,
	options: &Options<'_>,
) -> Option<RenderedBlock> {
	let view = card_view(dom, handle, node, options)?;
	let status = prop_text(node, PropId::Status).unwrap_or_else(|| Str::new_static("running"));
	let component = cards.render(tool.as_str(), &view, options.expanded, ui);
	let mut text = StrMut::new(tool.as_str());
	text.push_str(" ");
	text.push_str(status.as_str());
	if let Some(result) = view.result.and_then(node_text).filter(|text| !text.is_empty()) {
		text.push_str("\n");
		text.push_str(result.as_str());
	}
	if let Some(diag) = view.diag.and_then(node_text).filter(|text| !text.is_empty()) {
		text.push_str("\n");
		text.push_str(diag.as_str());
	}
	let finalized = matches!(status.as_str(), "ok" | "error" | "cancelled" | "aborted");
	Some(rendered(handle, BlockKind::Tool, text.freeze(), Mode::Mutable, finalized, component))
}

fn child(dom: &Dom, parent: Handle, tag: KnownTag) -> Option<&Node> {
	dom.children(parent)
		.iter()
		.filter_map(|handle| dom.get(*handle))
		.find(|node| node.tag == Tag::Known(tag))
}

/// The property's text, preferring an open stream buffer so streaming
/// content projects before the stream closes.
fn live_text(dom: &Dom, handle: Handle, node: &Node, prop: PropId) -> Option<Str> {
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

fn node_text(node: &Node) -> Option<Str> {
	node
		.content
		.clone()
		.or_else(|| prop_text(node, PropId::Text))
}
