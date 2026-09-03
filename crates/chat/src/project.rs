//! Pure projection from an actor-owned session DOM replica to transcript
//! blocks.

use omp_core::{Str, StrMut};
use omp_dom::{Dom, Handle, KnownTag, Node, PropId, Tag, Value};
use omp_tui::{IntoComponent, UiContext, dom, slots::Mode};

use crate::cards::{CardRegistry, CardView, Component};

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
}

/// Projects descriptors without constructing terminal components.
#[must_use]
pub fn block_views(dom: &Dom, show_thinking: bool) -> Vec<BlockView> {
	project(dom, &CardRegistry::standard(), &UiContext::default(), show_thinking)
		.into_iter()
		.map(|block| block.view)
		.collect()
}

pub(crate) fn project(
	dom: &Dom,
	cards: &CardRegistry,
	ui: &UiContext,
	show_thinking: bool,
) -> Vec<RenderedBlock> {
	let mut blocks = Vec::new();
	for turn in dom.children(dom.body()) {
		let Some(turn_node) = dom.get(*turn) else {
			continue;
		};
		if turn_node.tag != Tag::Known(KnownTag::Turn) {
			continue;
		}
		for handle in dom.children(*turn) {
			let Some(node) = dom.get(*handle) else {
				continue;
			};
			match &node.tag {
				Tag::Known(KnownTag::User) => {
					let text = node.content.clone().unwrap_or_default();
					blocks.push(rendered(
						*handle,
						BlockKind::User,
						text.clone(),
						Mode::Mutable,
						true,
						user_bubble(text),
					));
				},
				Tag::Known(KnownTag::Assistant) => {
					let finalized = node.prop(&PropId::StopReason.into()).is_some();
					if show_thinking
						&& let Some(thinking) = live_text(dom, *handle, node, PropId::Thinking)
						&& !thinking.is_empty()
					{
						blocks.push(rendered(
							*handle,
							BlockKind::Thinking,
							thinking.clone(),
							Mode::Mutable,
							finalized,
							dom! { <text fg=muted italic pad-x=1>{thinking}</text> },
						));
					}
					if let Some(text) = live_text(dom, *handle, node, PropId::Text)
						&& (!text.is_empty() || finalized)
					{
						blocks.push(rendered(
							*handle,
							BlockKind::Assistant,
							text.clone(),
							Mode::Mutable,
							finalized,
							dom! { <md pad-x=1>{text}</md> },
						));
					}
				},
				Tag::Known(KnownTag::Notice) => {
					let text = node.content.clone().unwrap_or_default();
					let kind = prop_text(node, PropId::Kind).unwrap_or_else(|| Str::new_static("info"));
					blocks.push(rendered(
						*handle,
						BlockKind::Notice,
						text.clone(),
						Mode::Mutable,
						true,
						notice_card(kind.as_str(), text),
					));
				},
				Tag::Known(KnownTag::Usage) => {
					let text = usage_text(node);
					blocks.push(rendered(
						*handle,
						BlockKind::Usage,
						text.clone(),
						Mode::Mutable,
						true,
						dom! { <text fg=muted pad-x=1>{text}</text> },
					));
				},
				Tag::Custom(tool) => {
					if let Some(block) = tool_block(dom, *handle, node, tool, cards, ui) {
						blocks.push(block);
					}
				},
				_ => {},
			}
		}
	}
	blocks
}

/// User message: pi paints the text on the `userMessageBg` tint with one
/// cell of padding on every side (`new Markdown(text, 1, 1, …)` in
/// `user-message.ts`: a tinted blank row above and below) and no border.
fn user_bubble(text: Str) -> impl IntoComponent {
	dom! { <text bg=surface pad="1 1">{text}</text> }
}

/// Controller notice: rules above and below, an icon in the kind's color,
/// the message, and pi's dismissal hint for errors.
fn notice_card(kind: &str, text: Str) -> impl IntoComponent {
	let hint =
		(kind == "error").then(|| Str::new_static("Dismissed when you send your next message."));
	let glyph = match kind {
		"error" => dom! { <icon name="error" fg=error/> },
		"warn" | "warning" => dom! { <icon name="warning" fg=warning/> },
		"success" => dom! { <icon name="success" fg=success/> },
		_ => dom! { <icon name="info" fg=info/> },
	};
	dom! {
		<col>
			<hr fg=muted/>
			<row gap=1 pad-x=1>
				{glyph}
				<text grow>{text}</text>
			</row>
			if let Some(hint) = hint { <text fg=muted pad-x=1>{hint}</text> }
			<hr fg=muted/>
		</col>
	}
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
	}
}

const fn block_key(handle: Handle, kind: BlockKind) -> u64 {
	let suffix = match kind {
		BlockKind::Welcome | BlockKind::User => 0,
		BlockKind::Thinking => 1,
		BlockKind::Assistant => 2,
		BlockKind::Tool => 3,
		BlockKind::Notice => 4,
		BlockKind::Usage => 5,
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
) -> Option<RenderedBlock> {
	let input = child(dom, handle, KnownTag::Input)?;
	let result = child(dom, handle, KnownTag::Result);
	let diag = child(dom, handle, KnownTag::Diag);
	let usage = child(dom, handle, KnownTag::Usage);
	let status = prop_text(node, PropId::Status).unwrap_or_else(|| Str::new_static("running"));
	let card_status = crate::cards::CardStatus::from_dom(status.as_str());
	let view = CardView { input, result, diag, usage, status: card_status };
	let component = cards.render(tool.as_str(), &view, false, ui);
	let mut text = StrMut::new(tool.as_str());
	text.push_str(" ");
	text.push_str(status.as_str());
	if let Some(result) = result.and_then(node_text).filter(|text| !text.is_empty()) {
		text.push_str("\n");
		text.push_str(result.as_str());
	}
	if let Some(diag) = diag.and_then(node_text).filter(|text| !text.is_empty()) {
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

fn usage_text(node: &Node) -> Str {
	let input = prop_u64(node, PropId::TokensIn);
	let output = prop_u64(node, PropId::TokensOut);
	Str::new(format!("tokens {input} in / {output} out"))
}

fn prop_u64(node: &Node, prop: PropId) -> u64 {
	match node.prop(&prop.into()) {
		Some(Value::Int(value)) => u64::try_from(*value).unwrap_or_default(),
		_ => 0,
	}
}
