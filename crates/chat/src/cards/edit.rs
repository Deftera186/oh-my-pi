//! Typed card for update, delete, and move edit transactions.

use omp_core::{Str, sf};
use omp_dom::{Node, PropId};
use omp_tui::{IntoComponent as _, UiContext, dom};
use serde_json::Value;

use super::{
	Card, CardStatus, CardView, Component, elapsed_badge, typed_fault, typed_input, typed_result,
};

/// Card for `edit` calls.
pub struct EditCard;

impl Card for EditCard {
	fn tool(&self) -> &'static str {
		"edit"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		render_edit(view, expanded, false, ui)
	}
}

pub(crate) fn render_edit(
	view: &CardView<'_>,
	expanded: bool,
	patch: bool,
	ui: &UiContext,
) -> Component {
	let args = if patch {
		typed_input::<omp_tools::edit::apply_patch::FreeformEditParams>(view)
	} else {
		typed_input::<omp_tools::edit::Params>(view)
	}
	.unwrap_or(Value::Null);
	let result = typed_result::<omp_tools::edit::Payload>(view).unwrap_or(Value::Null);
	let section = result
		.get("sections")
		.and_then(Value::as_array)
		.and_then(|v| v.first());
	let input = string_at(&args, "input").unwrap_or_default();
	let source = section
		.and_then(|v| string_at(v, "source_path"))
		.or_else(|| string_at(&args, "file_path"))
		.or_else(|| string_at(&args, "path"))
		.or_else(|| hashline_path(input))
		.unwrap_or_default();
	let destination = section
		.and_then(|v| string_at(v, "path"))
		.or_else(|| string_at(&args, "rename"));
	let op = string_at(&args, "op")
		.or_else(|| section.and_then(|v| string_at(v, "op")))
		.unwrap_or_else(|| {
			if destination.is_some() {
				"move"
			} else {
				"update"
			}
		});
	if op == "delete" {
		return render_delete(view, source, ui);
	}
	if op == "move" || destination.is_some() && destination != Some(source) {
		return render_move(view, source, destination.unwrap_or_default(), ui);
	}
	let path = section.and_then(|v| string_at(v, "path")).unwrap_or(source);
	let diff = section
		.and_then(|v| string_at(v, "diff"))
		.or_else(|| string_at(&args, "previewDiff"))
		.or_else(|| string_at(&args, "preview_diff"))
		.unwrap_or_default();
	let (added, removed) = diff_stats(diff);
	let diff = presented_diff(view.status, diff);
	let fault = typed_fault::<omp_tools::edit::Fault>(view).or_else(|| diag_text(view.diag));
	let lead = if fault.is_some() {
		icon(ui, "error")
	} else if matches!(view.status, CardStatus::Done) {
		icon(ui, "edit")
	} else {
		""
	};
	let stats = if matches!(view.status, CardStatus::Done) && (added > 0 || removed > 0) {
		sf!(" ⟨+{added}/-{removed}⟩")
	} else {
		Str::default()
	};
	let title = sf!("Edit: {} {path}{stats}", icon(ui, "typescript"));
	dom! {
		<box border=round title_pad=3>
			<row kind=title gap=1 bold>
				if !lead.is_empty() { <text bold>{lead}</text> }
				<text bold>{title}</text>
				if let Some(badge) = elapsed_badge(view) { {badge} }
			</row>
			if let Some(fault) = fault {
				<text fg=err wrap=word>{fault}</text>
			} else {
				<diff path={path}>{diff}</diff>
				if matches!(view.status, CardStatus::StreamingArgs) {
					<row gap=1>
						<spinner kind=status/>
						if !expanded { <text fg=muted>{"(preview)"}</text> }
					</row>
				} else if matches!(view.status, CardStatus::InProgress) && !expanded {
					<row><text fg=muted>{"(preview)"}</text></row>
				}
			}
		</box>
	}
	.into_component()
}

fn render_delete(view: &CardView<'_>, path: &str, ui: &UiContext) -> Component {
	if matches!(view.status, CardStatus::Failed) {
		let fault = typed_fault::<omp_tools::edit::Fault>(view)
			.or_else(|| diag_text(view.diag))
			.unwrap_or_else(|| Str::new_static("delete failed"));
		let title = sf!("{} Delete: {} {path}", icon(ui, "error"), icon(ui, "typescript"));
		return dom! {
			<box border=round bc=err title={title} title_pad=3>
				<text fg=err wrap=word>{fault}</text>
			</box>
		}
		.into_component();
	}
	let done = matches!(view.status, CardStatus::Done);
	dom! {
		<row gap=1>
			if done { <i:delete/> } else { <i:pending/> }
			<text bold>{"Delete:"}</text><i:typescript/><text>{path}</text>
		</row>
	}
	.into_component()
}

fn render_move(view: &CardView<'_>, source: &str, destination: &str, ui: &UiContext) -> Component {
	if matches!(view.status, CardStatus::Failed) {
		let fault = typed_fault::<omp_tools::edit::Fault>(view)
			.or_else(|| diag_text(view.diag))
			.unwrap_or_else(|| Str::new_static("move failed"));
		let title =
			sf!("{} Edit: {} {source} → {destination}", icon(ui, "error"), icon(ui, "typescript"));
		return dom! {
			<box border=round bc=err title={title} title_pad=3>
				<text fg=err wrap=word>{fault}</text>
			</box>
		}
		.into_component();
	}
	let done = matches!(view.status, CardStatus::Done);
	dom! {
		<row gap=1>
			if done { <i:move/> } else { <i:pending/> }
			<text bold>{"Move:"}</text><i:typescript/><text>{source}</text>
			<text fg=muted>{"→"}</text><text>{destination}</text>
		</row>
	}
	.into_component()
}

/// The diff text a card paints for `status`: streaming previews drop
/// unbalanced trailing removals so the card does not jitter; settled cards
/// show the full diff.
fn presented_diff(status: CardStatus, diff: &str) -> &str {
	if matches!(status, CardStatus::StreamingArgs | CardStatus::InProgress) {
		strip_unbalanced_removals(diff)
	} else {
		diff
	}
}

/// Drops trailing `-` and `@@` rows that no `+` row has answered yet (pi
/// `stripTrailingUnbalancedRemoval`).
///
/// A streaming preview shows removals before the matching additions arrive;
/// without this the card would paint `-old` alone and then grow `+new`
/// beneath it. Once such a trailing row exists everything after the last
/// addition is cut, and a diff with no addition at all disappears until one
/// arrives.
pub(crate) fn strip_unbalanced_removals(diff: &str) -> &str {
	let mut last_add_end = None;
	let mut unbalanced_after = false;
	let mut offset = 0;
	for line in diff.split('\n') {
		if line.starts_with('+') {
			last_add_end = Some(offset + line.len());
			unbalanced_after = false;
		} else if line.starts_with('-') || line.starts_with("@@") {
			unbalanced_after = true;
		}
		offset += line.len() + 1;
	}
	if !unbalanced_after {
		return diff;
	}
	last_add_end.map_or("", |end| &diff[..end])
}

fn diff_stats(diff: &str) -> (u64, u64) {
	diff.lines().fold((0, 0), |(add, del), line| {
		if line.starts_with('+') && !line.starts_with("+++") {
			(add + 1, del)
		} else if line.starts_with('-') && !line.starts_with("---") {
			(add, del + 1)
		} else {
			(add, del)
		}
	})
}

fn string_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
	value.get(key).and_then(Value::as_str)
}

fn hashline_path(input: &str) -> Option<&str> {
	let line = input.lines().find(|line| line.starts_with('['))?;
	line.strip_prefix('[')?.split(['#', ']']).next()
}

fn icon<'a>(ui: &'a UiContext, name: &str) -> &'a str {
	ui.charset.icon_named(name).unwrap_or_default()
}

fn diag_text(node: Option<&Node>) -> Option<Str> {
	let raw = node.and_then(|node| {
		node.content.as_deref().or_else(|| {
			node
				.prop(&PropId::Text.into())
				.and_then(omp_dom::Value::as_str)
		})
	})?;
	let value: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()));
	fault_message(&value).map(Str::new)
}

fn fault_message(value: &Value) -> Option<&str> {
	value
		.as_str()
		.or_else(|| string_at(value, "error"))
		.or_else(|| string_at(value, "message"))
		.or_else(|| value.get("reason").and_then(fault_message))
}

#[cfg(test)]
mod tests {
	use super::{CardStatus, diff_stats, presented_diff, strip_unbalanced_removals};

	#[test]
	fn unbalanced_trailing_removals_are_stripped_while_streaming() {
		assert_eq!(strip_unbalanced_removals("-a\n+b\n-c"), "-a\n+b");
		assert_eq!(strip_unbalanced_removals("-a\n+b\n c\n@@ -5 +5 @@\n-d"), "-a\n+b");
		assert_eq!(strip_unbalanced_removals("-a\n+b\n c\n@@ -5 +5 @@"), "-a\n+b");
		assert_eq!(strip_unbalanced_removals("-a\n+b"), "-a\n+b", "balanced tail is untouched");
		assert_eq!(strip_unbalanced_removals("-a\n+b\n c"), "-a\n+b\n c", "context may trail");
		assert_eq!(strip_unbalanced_removals("-a"), "", "no addition yet hides the diff");
		assert_eq!(strip_unbalanced_removals("@@ -1 +1 @@\n-a"), "");
		assert_eq!(strip_unbalanced_removals(""), "");
		assert_eq!(strip_unbalanced_removals(" only\n context"), " only\n context");

		let diff = "@@ -1,2 +1,2 @@\n-old\n+new\n-gone";
		assert_eq!(presented_diff(CardStatus::StreamingArgs, diff), "@@ -1,2 +1,2 @@\n-old\n+new");
		assert_eq!(presented_diff(CardStatus::InProgress, diff), "@@ -1,2 +1,2 @@\n-old\n+new");
		assert_eq!(presented_diff(CardStatus::Done, diff), diff, "settled cards show everything");
		assert_eq!(presented_diff(CardStatus::Failed, diff), diff);
		assert_eq!(diff_stats(diff), (1, 2), "stats count the full diff");
	}
}
