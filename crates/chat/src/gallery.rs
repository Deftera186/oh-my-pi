//! Deterministic, journal-derived tool-card gallery.

use omp_core::Str;
use omp_dom::{Handle, KnownTag, Node, Op, PropId, Snapshot, Tag, Txn, Value};
use omp_session::{ComponentRegistry, Session};
use omp_tui::{Charset, Frame, Ui, UiContext};
use serde_json::value::RawValue;
use thiserror::Error;

use crate::cards::{CardRegistry, CardStatus, CardView, fixtures::CardFixture};

/// Tool lifecycle states rendered by the gallery, in display order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GalleryState {
	/// Arguments are still streaming.
	StreamingArgs,
	/// The call is executing.
	InProgress,
	/// The call settled successfully.
	Done,
	/// The call faulted or returned an error-shaped outcome.
	Failed,
}

impl GalleryState {
	/// All states in reference-gallery order.
	pub const ALL: [Self; 4] = [Self::StreamingArgs, Self::InProgress, Self::Done, Self::Failed];

	/// Human-readable state label used by the captured references.
	#[must_use]
	pub const fn label(self) -> &'static str {
		match self {
			Self::StreamingArgs => "streaming args",
			Self::InProgress => "in progress",
			Self::Done => "done",
			Self::Failed => "failed",
		}
	}

	const fn index(self) -> usize {
		match self {
			Self::StreamingArgs => 0,
			Self::InProgress => 1,
			Self::Done => 2,
			Self::Failed => 3,
		}
	}
}

/// One rendered fixture state.
pub struct GallerySection {
	/// Gallery fixture identity.
	pub tool:  &'static str,
	/// Human-readable fixture title.
	pub title: &'static str,
	/// Lifecycle state represented by this frame.
	pub state: GalleryState,
	/// Fully laid-out card frame.
	pub frame: Frame,
}

/// Failure to materialize or render a gallery fixture.
#[derive(Debug, Error)]
pub enum GalleryError {
	/// A fixture payload was not valid complete JSON for its lifecycle state.
	#[error("gallery fixture JSON is invalid")]
	Json(#[from] serde_json::Error),
	/// A temporary journal could not be created.
	#[error("gallery temporary journal failed")]
	Temp(#[from] std::io::Error),
	/// The journal-to-DOM fold failed.
	#[error("gallery session fold failed")]
	Session(#[from] omp_session::SessionError),
	/// The folded call element or one of its mandatory children is absent.
	#[error("gallery fixture did not materialize {0}")]
	Missing(&'static str),
}

/// Returns gallery fixture names in stable reference order.
#[must_use]
pub fn fixture_names() -> Vec<&'static str> {
	let mut names = crate::cards::fixtures::all()
		.into_iter()
		.map(|fixture| fixture.tool)
		.collect::<Vec<_>>();
	names.sort_unstable();
	names
}

/// Materializes and renders selected card fixtures through real sessions.
///
/// `tool = None` renders every fixture in stable reference order.
pub fn render_sections(
	tool: Option<&str>,
	states: &[GalleryState],
	width: u16,
	expanded: bool,
) -> Result<Vec<GallerySection>, GalleryError> {
	let mut fixtures = crate::cards::fixtures::all();
	fixtures.sort_unstable_by_key(|fixture| fixture.tool);
	let registry = CardRegistry::standard();
	let mut sections = Vec::with_capacity(fixtures.len().saturating_mul(states.len()));
	for fixture in fixtures {
		if tool.is_some_and(|wanted| wanted != fixture.tool) {
			continue;
		}
		for &state in states {
			sections.push(render_fixture(&registry, fixture, state, width, expanded)?);
		}
	}
	Ok(sections)
}

fn render_fixture(
	registry: &CardRegistry,
	fixture: &'static CardFixture,
	state: GalleryState,
	width: u16,
	expanded: bool,
) -> Result<GallerySection, GalleryError> {
	let directory = tempfile::tempdir()?;
	let journal = directory.path().join("gallery.oms");
	let mut session = Session::create(journal, ComponentRegistry::standard())?;
	session.begin_turn()?;
	let state_fixture = fixture.states[state.index()];
	let call_id = format!("gallery-{}-{}", fixture.tool, state.index());
	let call = if state == GalleryState::StreamingArgs {
		let (call, sid) =
			session.call_streaming(card_tool(fixture.tool), 1, call_id.as_str(), None)?;
		if !state_fixture.args.is_empty() {
			session.stream_append(sid, state_fixture.args)?;
		}
		call
	} else {
		session.call(
			card_tool(fixture.tool),
			1,
			call_id.as_str(),
			None,
			Some(raw(state_fixture.args)?),
			None,
		)?
	};
	if state != GalleryState::StreamingArgs {
		if let Some(update) = state_fixture.update {
			session.call_update(call, raw(update)?)?;
		}
		match state {
			GalleryState::StreamingArgs | GalleryState::InProgress => {},
			GalleryState::Done => {
				session.settle(call, raw(state_fixture.result.unwrap_or("null"))?)?;
			},
			GalleryState::Failed => {
				if let Some(fault) = state_fixture.fault {
					session.fail(call, raw(fault)?)?;
				} else if let Some(result) = state_fixture.result {
					session.settle(call, raw(result)?)?;
					let tool = find_dom_call(session.dom(), call_id.as_str())
						.ok_or(GalleryError::Missing("tool element"))?;
					session.patch(Txn {
						cause: session
							.head()
							.ok_or(GalleryError::Missing("journal head"))?,
						label: Some(Str::new_static("gallery.failed-outcome")),
						ops:   vec![Op::Set {
							h:     tool,
							prop:  PropId::Status.into(),
							value: Value::Str(Str::new_static("error")),
						}],
					})?;
				} else {
					session.fail(call, raw(r#""operation failed""#)?)?;
				}
			},
		}
	}
	let snapshot = session.dom().snapshot();
	let tool = find_snapshot_call(&snapshot, call_id.as_str())
		.ok_or(GalleryError::Missing("tool element"))?;
	let node = snapshot
		.get(tool)
		.ok_or(GalleryError::Missing("tool element"))?;
	let input =
		child(&snapshot, tool, KnownTag::Input).ok_or(GalleryError::Missing("input element"))?;
	let status = node
		.prop(&PropId::Status.into())
		.and_then(Value::as_str)
		.map_or(CardStatus::InProgress, CardStatus::from_dom);
	let view = CardView {
		input,
		result: child(&snapshot, tool, KnownTag::Result),
		diag: child(&snapshot, tool, KnownTag::Diag),
		usage: child(&snapshot, tool, KnownTag::Usage),
		status,
		output: None,
		started: None,
	};
	let mut ui_context = UiContext::default();
	ui_context.charset = Charset::NerdFont;
	let component = registry.render(card_tool(fixture.tool), &view, expanded, &ui_context);
	let ui = Ui::from_root(component, width, ui_context);
	Ok(GallerySection { tool: fixture.tool, title: fixture.title, state, frame: ui.frame().clone() })
}

fn raw(text: &str) -> Result<Box<RawValue>, serde_json::Error> {
	let value: serde_json::Value = serde_json::from_str(text)?;
	serde_json::value::to_raw_value(&value)
}

fn find_dom_call(dom: &omp_dom::Dom, call_id: &str) -> Option<Handle> {
	dom.handles().find(|handle| {
		dom.get(*handle).is_some_and(|node| {
			matches!(&node.tag, Tag::Custom(_))
				&& node
					.prop(&PropId::Id.into())
					.and_then(Value::as_str)
					.is_some_and(|id| id == call_id)
		})
	})
}

fn find_snapshot_call(snapshot: &Snapshot, call_id: &str) -> Option<Handle> {
	snapshot.handles().find(|handle| {
		snapshot.get(*handle).is_some_and(|node| {
			matches!(&node.tag, Tag::Custom(_))
				&& node
					.prop(&PropId::Id.into())
					.and_then(Value::as_str)
					.is_some_and(|id| id == call_id)
		})
	})
}

fn child(snapshot: &Snapshot, parent: Handle, tag: KnownTag) -> Option<&Node> {
	snapshot
		.children(parent)
		.iter()
		.filter_map(|handle| snapshot.get(*handle))
		.find(|node| node.tag == Tag::Known(tag))
}

fn card_tool(tool: &str) -> &str {
	match tool {
		"read_group" => "read",
		"edit_delete" | "edit_move" => "edit",
		"hub_inbox" | "hub_jobs" | "hub_list" | "hub_logs" | "hub_send" | "hub_start"
		| "hub_wait" => "hub",
		"vibe_kill" | "vibe_list" | "vibe_send" | "vibe_spawn" | "vibe_wait" => "vibe",
		"custom" => "Custom Tool",
		other => other,
	}
}

#[cfg(test)]
mod tests {
	use omp_tui::frame_text;

	use super::{GalleryState, fixture_names, render_sections};

	#[test]
	fn gallery_fixture_inventory_is_complete() {
		assert_eq!(fixture_names(), [
			"apply_patch",
			"ask",
			"ast_edit",
			"ast_grep",
			"bash",
			"browser",
			"computer",
			"context_gauge",
			"custom",
			"debug",
			"edit",
			"edit_delete",
			"edit_move",
			"eval",
			"github",
			"glob",
			"goal",
			"grep",
			"hub",
			"hub_inbox",
			"hub_jobs",
			"hub_list",
			"hub_logs",
			"hub_send",
			"hub_start",
			"hub_wait",
			"inspect_image",
			"lsp",
			"read",
			"read_group",
			"recall",
			"reflect",
			"reject",
			"report_tool_issue",
			"resolve",
			"retain",
			"task",
			"think",
			"todo",
			"vibe_kill",
			"vibe_list",
			"vibe_send",
			"vibe_spawn",
			"vibe_wait",
			"web_search",
			"write",
		]);
	}

	#[test]
	fn gallery_materializes_every_read_lifecycle_through_session() {
		let sections = render_sections(Some("read"), &GalleryState::ALL, 100, false)
			.expect("read fixtures should fold and render");
		assert_eq!(sections.len(), GalleryState::ALL.len());
		for (section, state) in sections.iter().zip(GalleryState::ALL) {
			assert_eq!(section.state, state);
			assert_eq!(section.frame.size().width, 100);
			assert!(!frame_text(&section.frame).trim().is_empty());
		}
	}
}
