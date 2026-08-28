//! Fail-closed classification of explicitly read-only agent definitions.

use crate::{AgentDefinition, SpawnPolicy};

const READ_ONLY_TOOLS: &[&str] = &[
	"read",
	"grep",
	"glob",
	"web_search",
	"lsp",
	"ast_grep",
	"dyn",
	"task",
	"yield",
	"hub",
	"ask",
	"todo",
	"recall",
	"reflect",
	"retain",
	"memory_edit",
	"inspect_image",
	"checkpoint",
	"rewind",
];

/// Reports whether every explicitly declared tool is in the read-only
/// whitelist.
///
/// A definition exposing `task` is read-only only when it may recursively
/// spawn its own definition; ordinary or caller-selected child profiles could
/// widen the inherited authority and therefore fail closed.
///
/// Empty declarations inherit their parent's tools and are therefore never
/// classified as read-only. Unknown names fail closed and this function never
/// adds tools to the definition.
pub fn is_read_only_agent(definition: &AgentDefinition) -> bool {
	!definition.tools.is_empty()
		&& definition
			.tools
			.iter()
			.all(|tool| READ_ONLY_TOOLS.contains(&tool.as_str()))
		&& (!definition.tools.iter().any(|tool| tool == "task")
			|| matches!(
				&definition.spawns,
				SpawnPolicy::Only(allowed)
					if !allowed.is_empty()
						&& allowed.iter().all(|name| name == &definition.name)
			))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn definition(tools: &str) -> AgentDefinition {
		AgentDefinition::parse_markdown(
			"scout",
			&format!("---\ndescription: Read only\ntools: [{tools}]\n---\nInspect."),
		)
		.expect("valid agent definition")
	}

	#[test]
	fn dynamic_device_transport_preserves_read_only_classification() {
		assert!(is_read_only_agent(&definition("read, grep, glob, dyn")));
	}

	#[test]
	fn mutating_top_level_tools_remain_rejected() {
		assert!(!is_read_only_agent(&definition("read, dyn, write")));
		assert!(!is_read_only_agent(&definition("read, dyn, edit")));
	}
}
