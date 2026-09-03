//! Built-in Director specifications.

pub mod autoresearch;
pub mod compaction;
pub mod force_tool;
pub mod goal;
pub mod loop_mode;
pub mod plan;
pub mod todo_reminder;
pub mod vibe;

use crate::director::DirectorRegistry;

/// Registers every built-in Director constructor.
pub fn register_standard(registry: &mut DirectorRegistry) {
	registry.register("autoresearch", |node| Box::new(autoresearch::Autoresearch::from_node(node)));
	registry.register("compaction", |_| Box::new(compaction::CompactionDirector::new()));
	registry.register("force_tool", |node| Box::new(force_tool::ForceTool::from_node(node)));
	registry.register("goal", |node| Box::new(goal::Goal::from_node(node)));
	registry.register("loop_mode", |node| Box::new(loop_mode::LoopMode::from_node(node)));
	registry.register("plan", |node| Box::new(plan::Plan::from_node(node)));
	registry
		.register("todo_reminder", |node| Box::new(todo_reminder::TodoReminder::from_node(node)));
	registry.register("vibe", |node| Box::new(vibe::Vibe::from_node(node)));
}
