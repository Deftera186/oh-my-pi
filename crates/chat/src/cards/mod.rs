//! Typed tool-card registry over materialized tool element state.

pub mod apply_patch;
pub mod ask;
pub mod ast_edit;
pub mod ast_grep;
pub mod bash;
pub mod browser;
pub mod computer;
pub mod context_gauge;
pub mod debug;
pub mod edit;
pub mod eval;
pub(crate) mod fixtures;
mod generic;
pub mod github;
pub mod glob;
pub mod goal;
pub mod grep;
pub mod hub;
pub mod inspect_image;
pub mod lsp;
pub mod memory;
pub mod read;
pub mod report_issue;
pub mod resolve;
pub mod task;
pub mod think;
pub mod todo;
pub mod vibe;
pub mod web_search;
pub mod write;

use std::{collections::BTreeMap, sync::Arc};

pub use generic::GenericCard;
use omp_dom::{Node, PropId};
use omp_tui::UiContext;
use serde::de::DeserializeOwned;

/// A boxed retained TUI component.
pub type Component = Box<dyn omp_tui::Component>;

/// Tool lifecycle state derived from the tool element's `status` property.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CardStatus {
	/// The provider is still streaming tool arguments.
	StreamingArgs,
	/// The tool is executing.
	InProgress,
	/// The tool settled successfully.
	Done,
	/// The tool faulted or was aborted.
	Failed,
}

impl CardStatus {
	/// Derives a card status from the session-DOM lifecycle spelling.
	#[must_use]
	pub fn from_dom(status: &str) -> Self {
		match status.as_bytes() {
			b"arguments" => Self::StreamingArgs,
			b"ok" => Self::Done,
			b"error" | b"cancelled" | b"aborted" => Self::Failed,
			_ => Self::InProgress,
		}
	}

	/// Returns the canonical session-DOM spelling.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::StreamingArgs => "arguments",
			Self::InProgress => "running",
			Self::Done => "ok",
			Self::Failed => "error",
		}
	}
}

/// Borrowed state of one tool element and its standard child elements.
pub struct CardView<'a> {
	/// Tool input state.
	pub input:  &'a Node,
	/// Successful result state, when present.
	pub result: Option<&'a Node>,
	/// Diagnostic state, when present.
	pub diag:   Option<&'a Node>,
	/// Usage state, when present.
	pub usage:  Option<&'a Node>,
	/// Tool lifecycle status.
	pub status: CardStatus,
}

impl CardView<'_> {
	/// Returns the streamed or committed argument text.
	#[must_use]
	pub fn args_text(&self) -> Option<&str> {
		node_text(self.input)
	}

	/// Deserializes the streamed or committed arguments into the tool's
	/// canonical parameter type.
	#[must_use]
	pub fn input<P: DeserializeOwned>(&self) -> Option<P> {
		let raw = node_data(self.input).or_else(|| self.args_text())?;
		let mut value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
		value.as_object_mut()?.remove("i");
		serde_json::from_value(value).ok()
	}

	/// Parses the streamed or committed arguments as JSON.
	#[must_use]
	pub fn args_json(&self) -> Option<serde_json::Value> {
		serde_json::from_str(self.args_text()?).ok()
	}

	/// Returns the successful result's model-facing text.
	#[must_use]
	pub fn result_text(&self) -> Option<&str> {
		self.result.and_then(node_text)
	}

	/// Deserializes the successful result into the tool's canonical payload
	/// type.
	#[must_use]
	pub fn result<T: DeserializeOwned>(&self) -> Option<T> {
		let node = self.result?;
		serde_json::from_str(node_data(node).or_else(|| node_text(node))?).ok()
	}

	/// Parses the successful result as JSON.
	#[must_use]
	pub fn result_json(&self) -> Option<serde_json::Value> {
		serde_json::from_str(self.result_text()?).ok()
	}

	/// Deserializes the terminal diagnostic into the tool's canonical fault
	/// type.
	#[must_use]
	pub fn fault<F: DeserializeOwned>(&self) -> Option<F> {
		let node = self.diag?;
		serde_json::from_str(node_data(node).or_else(|| node_text(node))?).ok()
	}
}

pub(crate) fn typed_input<P>(view: &CardView<'_>) -> Option<serde_json::Value>
where
	P: DeserializeOwned + serde::Serialize,
{
	view
		.input::<P>()
		.and_then(|value| serde_json::to_value(value).ok())
		.or_else(|| view.args_json())
}

pub(crate) fn typed_result<T>(view: &CardView<'_>) -> Option<serde_json::Value>
where
	T: DeserializeOwned + serde::Serialize,
{
	view
		.result::<T>()
		.and_then(|value| serde_json::to_value(value).ok())
		.or_else(|| view.result_json())
}

pub(crate) fn typed_fault<F>(view: &CardView<'_>) -> Option<omp_core::Str>
where
	F: DeserializeOwned + serde::Serialize,
{
	let value = serde_json::to_value(view.fault::<F>()?).ok()?;
	let text = value
		.get("message")
		.and_then(serde_json::Value::as_str)
		.map(str::to_owned)
		.unwrap_or_else(|| serde_json::to_string(&value).unwrap_or_default());
	Some(omp_core::Str::new(text))
}

fn node_data(node: &Node) -> Option<&str> {
	match node.prop(&PropId::Data.into())? {
		omp_dom::Value::Json(value) => Some(value.get()),
		_ => None,
	}
}

fn node_text(node: &Node) -> Option<&str> {
	node
		.prop(&PropId::Text.into())
		.and_then(omp_dom::Value::as_str)
		.filter(|text| !text.is_empty())
		.or(node.content.as_deref())
}

/// One typed renderer for a tool identity.
pub trait Card: Send + Sync {
	/// Tool name handled by this renderer.
	fn tool(&self) -> &'static str;

	/// Builds retained semantic markup for the current element state.
	fn render(&self, el: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component;
}

/// Tool-identity keyed card renderer registry with a generic fallback.
#[derive(Clone)]
pub struct CardRegistry {
	cards:    BTreeMap<&'static str, Arc<dyn Card>>,
	fallback: Arc<GenericCard>,
}

impl CardRegistry {
	/// Builds the standard registry. Tool-specific cards extend this seam.
	#[must_use]
	pub fn standard() -> Self {
		let mut registry = Self { cards: BTreeMap::new(), fallback: Arc::new(GenericCard) };
		registry.register(apply_patch::ApplyPatchCard);
		registry.register(ask::AskCard);
		registry.register(ast_edit::AstEditCard);
		registry.register(ast_grep::AstGrepCard);
		registry.register(bash::BashCard);
		registry.register(browser::BrowserCard);
		registry.register(computer::ComputerCard);
		registry.register(context_gauge::ContextGaugeCard);
		registry.register(debug::DebugCard);
		registry.register(edit::EditCard);
		registry.register(eval::EvalCard);
		registry.register(github::GithubCard);
		registry.register(glob::GlobCard);
		registry.register(goal::GoalCard);
		registry.register(grep::GrepCard);
		registry.register(hub::HubCard);
		registry.register(inspect_image::InspectImageCard);
		registry.register(lsp::LspCard);
		registry.register(memory::RecallCard);
		registry.register(memory::ReflectCard);
		registry.register(memory::RetainCard);
		registry.register(read::ReadCard);
		registry.register(report_issue::ReportIssueCard);
		registry.register(resolve::RejectCard);
		registry.register(resolve::ResolveCard);
		registry.register(task::TaskCard);
		registry.register(think::ThinkCard);
		registry.register(todo::TodoCard);
		registry.register(vibe::VibeCard);
		registry.register(web_search::WebSearchCard);
		registry.register(write::WriteCard);
		registry
	}

	/// Registers or replaces one typed card.
	pub fn register<C: Card + 'static>(&mut self, card: C) {
		self.cards.insert(card.tool(), Arc::new(card));
	}

	/// Returns whether a tool identity has a dedicated typed card.
	#[must_use]
	pub fn contains(&self, tool: &str) -> bool {
		self.cards.contains_key(tool)
	}

	/// Renders one tool, falling back to the generic element-state card.
	#[must_use]
	pub fn render(
		&self,
		tool: &str,
		view: &CardView<'_>,
		expanded: bool,
		ui: &UiContext,
	) -> Component {
		self.cards.get(tool).map_or_else(
			|| self.fallback.render_named(tool, view, expanded, ui),
			|card| card.render(view, expanded, ui),
		)
	}
}

impl Default for CardRegistry {
	fn default() -> Self {
		Self::standard()
	}
}
