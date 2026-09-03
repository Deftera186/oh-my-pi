use omp_core::Str;
use omp_dom::{Dom, Handle, KnownTag, NodeSpec, PropId, PropKey, Tag, Value};
use omp_journal::{Entry, Kind, data::ToolResult, kind};

use crate::{Component, Draft};

/// Rebuilds `<meta><todo>` from successful `todo` tool snapshots.
pub struct TodoComponent;

impl Component for TodoComponent {
	fn interested(&self, kind: &Kind) -> bool {
		kind.rev == 1 && kind.name.as_str() == kind::TOOL_RESULT
	}

	fn apply(&mut self, entry: &Entry, dom: &Dom, draft: &mut Draft) {
		let Some(call_id) = entry.by else { return };
		if !is_todo_call(dom, call_id) {
			return;
		}
		let Ok(ToolResult::Outcome { outcome, .. }) = serde_json::from_str(entry.data.as_str())
		else {
			return;
		};
		let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(outcome.get()) else {
			return;
		};
		if payload.get("kind").and_then(serde_json::Value::as_str) == Some("ok") {
			payload = payload
				.get_mut("value")
				.map_or(serde_json::Value::Null, serde_json::Value::take);
		}
		let Some(phases) = payload.get("phases").and_then(serde_json::Value::as_array) else {
			return;
		};
		let Some(todo) = find_tag(dom, dom.meta(), KnownTag::Todo) else {
			return;
		};
		for child in dom.children(todo) {
			draft.remove(*child);
		}
		let mut after = None;
		let mut next = dom.high_water() + 1;
		for phase in phases {
			let phase_name = phase
				.get("phase")
				.and_then(serde_json::Value::as_str)
				.unwrap_or("");
			let Some(items) = phase.get("items").and_then(serde_json::Value::as_array) else {
				continue;
			};
			for item in items {
				let text = item
					.get("text")
					.and_then(serde_json::Value::as_str)
					.unwrap_or("");
				let status = item
					.get("status")
					.and_then(serde_json::Value::as_str)
					.unwrap_or("pending");
				let mut node = NodeSpec::new(KnownTag::Item)
					.with_prop(PropId::Label, Value::Str(Str::new(text)))
					.with_prop(PropId::Status, Value::Str(Str::new(status)))
					.with_prop(
						PropKey::Custom(Str::new_static("phase")),
						Value::Str(Str::new(phase_name)),
					);
				if let Some(reason) = item.get("reason").and_then(serde_json::Value::as_str) {
					node = node.with_prop(PropId::Detail, Value::Str(Str::new(reason)));
				}
				draft.insert(todo, after, node);
				after = Handle::new(next);
				next += 1;
			}
		}
	}
}

fn is_todo_call(dom: &Dom, entry_id: omp_journal::EntryId) -> bool {
	let wanted = entry_id.to_string();
	dom.handles().any(|handle| {
		dom.get(handle).is_some_and(|node| {
			node.tag == Tag::Custom(Str::new_static("todo"))
				&& node
					.prop(&PropKey::from(PropId::Cause))
					.and_then(Value::as_str)
					.is_some_and(|cause| cause == wanted)
		})
	})
}

fn find_tag(dom: &Dom, parent: Handle, tag: KnownTag) -> Option<Handle> {
	dom.children(parent).iter().copied().find(|handle| {
		dom.get(*handle)
			.is_some_and(|node| node.tag == Tag::Known(tag))
	})
}
