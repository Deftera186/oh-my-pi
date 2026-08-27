//! Compile-time prompt assets used by live auxiliary prompt consumers.

use std::sync::OnceLock;

use omp_scribe::{Props, Template};

use crate::{SlotClass, SlotId, prompt_engine, prompt_keys};

/// Semantic family of an immutable prompt asset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptAssetFamily {
	/// Configured communication personality.
	Personality,
	/// Agent lifecycle continuation text.
	Lifecycle,
	/// Parent or user steering text.
	Steering,
	/// Provider-loop recovery text.
	Recovery,
	/// Auxiliary title completion text.
	Title,
	/// Built-in agent definition.
	Agent,
	/// Built-in execution mode.
	Mode,
}

/// Prompt behavior activated by an explicit user keyword.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptKeywordBehavior {
	/// Requests extended reasoning.
	ExtendedThinking,
	/// Requests multi-agent orchestration.
	Orchestration,
	/// Requests the guided workflow policy.
	Workflow,
}

/// One canonical user keyword and the prompt behavior it activates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptKeyword {
	/// ASCII keyword matched case-insensitively at word boundaries.
	pub text:     &'static str,
	/// Policy behavior selected by the keyword.
	pub behavior: PromptKeywordBehavior,
}

/// Canonical user-keyword policy consumed by prompt and presentation layers.
pub const PROMPT_KEYWORDS: &[PromptKeyword] = &[
	PromptKeyword { text: "ultrathink", behavior: PromptKeywordBehavior::ExtendedThinking },
	PromptKeyword { text: "orchestrate", behavior: PromptKeywordBehavior::Orchestration },
	PromptKeyword { text: "workflowz", behavior: PromptKeywordBehavior::Workflow },
];

/// Typed identity of a compile-time prompt asset.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PromptAssetId {
	/// Default personality.
	PersonalityDefault,
	/// Friendly personality.
	PersonalityFriendly,
	/// Pragmatic personality.
	PersonalityPragmatic,
	/// Automatic continuation.
	AutoContinue,
	/// User steering.
	UserInterjection,
	/// Parent IRC steering.
	ParentIrc,
	/// Empty-stop recovery.
	EmptyStopRetry,
	/// Unexpected-stop recovery.
	UnexpectedStopRetry,
	/// Tool-loop redirect.
	ToolCallLoopRedirect,
	/// Thinking-loop redirect.
	ThinkingLoopRedirect,
	/// Gemini tool reminder.
	GeminiToolCallReminder,
	/// Title prompt.
	TitleSystem,
	/// Required tagged-output instruction for title generation.
	TitleMarkerInstruction,
	/// Plan filename topic prompt.
	PlanFilename,
	/// Dynamic user prompt for an idle session recap.
	RecapUser,
	/// Scout definition.
	AgentScout,
	/// Reviewer definition.
	AgentReviewer,
	/// Security reviewer definition.
	AgentSecurityReviewer,
	/// Task definition.
	AgentTask,
	/// Librarian definition.
	AgentLibrarian,
	/// Designer definition.
	AgentDesigner,
	/// Initializer definition.
	AgentInit,
	/// Plan mode.
	ModePlan,
	/// Prewalk mode.
	ModePrewalk,
	/// Goal mode.
	ModeGoal,
	/// Vibe mode.
	ModeVibe,
	/// Memory pipeline mode.
	ModeMemoryPipeline,
	/// Advisor mode.
	ModeAdvisor,
	/// Autoresearch mode.
	ModeAutoresearch,
	/// Security audit mode.
	ModeSecurityAudit,
	/// Benchmark mode.
	ModeBench,
	/// Review mode.
	ModeReview,
	/// Cleanse mode.
	ModeCleanse,
	/// Compression mode.
	ModeCompress,
	/// Live collaboration mode.
	ModeLiveCollab,
}

/// Immutable asset bytes and declared prompt placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptAsset {
	/// Typed identity.
	pub id:      PromptAssetId,
	/// Semantic family.
	pub family:  PromptAssetFamily,
	/// Destination slot.
	pub slot:    SlotId,
	/// Stability band.
	pub class:   SlotClass,
	/// Immutable UTF-8 source.
	pub content: &'static str,
}

macro_rules! asset {
	($id:ident, $family:ident, $slot:ident, $class:ident, $path:literal) => {
		PromptAsset {
			id:      PromptAssetId::$id,
			family:  PromptAssetFamily::$family,
			slot:    SlotId::$slot,
			class:   SlotClass::$class,
			content: include_str!($path),
		}
	};
}

const ASSETS: [PromptAsset; 35] = [
	asset!(PersonalityDefault, Personality, Runtime, Stable, "../prompts/personality/default.md"),
	asset!(PersonalityFriendly, Personality, Runtime, Stable, "../prompts/personality/friendly.md"),
	asset!(
		PersonalityPragmatic,
		Personality,
		Runtime,
		Stable,
		"../prompts/personality/pragmatic.md"
	),
	asset!(AutoContinue, Lifecycle, Status, Volatile, "../prompts/lifecycle/auto-continue.md"),
	asset!(UserInterjection, Steering, Status, Volatile, "../prompts/steering/user-interjection.md"),
	asset!(ParentIrc, Steering, Status, Volatile, "../prompts/steering/parent-irc.md"),
	asset!(EmptyStopRetry, Recovery, Status, Volatile, "../prompts/recovery/empty-stop-retry.md"),
	asset!(
		UnexpectedStopRetry,
		Recovery,
		Status,
		Volatile,
		"../prompts/recovery/unexpected-stop-retry.md"
	),
	asset!(
		ToolCallLoopRedirect,
		Recovery,
		Status,
		Volatile,
		"../prompts/recovery/tool-call-loop-redirect.md"
	),
	asset!(
		ThinkingLoopRedirect,
		Recovery,
		Status,
		Volatile,
		"../prompts/recovery/thinking-loop-redirect.md"
	),
	asset!(
		GeminiToolCallReminder,
		Recovery,
		Status,
		Volatile,
		"../prompts/recovery/gemini-tool-call-reminder.md"
	),
	asset!(TitleSystem, Title, Guidance, Stable, "../prompts/title/system.md"),
	asset!(
		TitleMarkerInstruction,
		Title,
		Guidance,
		Stable,
		"../prompts/title/marker-instruction.md"
	),
	asset!(PlanFilename, Title, Guidance, Stable, "../prompts/system/plan-filename.md"),
	asset!(RecapUser, Steering, Status, Volatile, "../prompts/recap/user.md"),
	asset!(AgentScout, Agent, Role, Frozen, "../prompts/roles/scout.md"),
	asset!(AgentReviewer, Agent, Role, Frozen, "../prompts/roles/reviewer.md"),
	asset!(AgentSecurityReviewer, Agent, Role, Frozen, "../prompts/roles/security-reviewer.md"),
	asset!(AgentTask, Agent, Role, Frozen, "../prompts/roles/task.md"),
	asset!(AgentLibrarian, Agent, Role, Frozen, "../prompts/roles/librarian.md"),
	asset!(AgentDesigner, Agent, Role, Frozen, "../prompts/roles/designer.md"),
	asset!(AgentInit, Agent, Role, Frozen, "../prompts/roles/init.md"),
	asset!(ModePlan, Mode, Status, Volatile, "../prompts/modes/plan.md"),
	asset!(ModePrewalk, Mode, Status, Volatile, "../prompts/modes/prewalk.md"),
	asset!(ModeGoal, Mode, Status, Volatile, "../prompts/modes/goal.md"),
	asset!(ModeVibe, Mode, Status, Volatile, "../prompts/modes/vibe.md"),
	asset!(ModeMemoryPipeline, Mode, Status, Volatile, "../prompts/modes/memory-pipeline.md"),
	asset!(ModeAdvisor, Mode, Status, Volatile, "../prompts/modes/advisor.md"),
	asset!(ModeAutoresearch, Mode, Status, Volatile, "../prompts/modes/autoresearch.md"),
	asset!(ModeSecurityAudit, Mode, Status, Volatile, "../prompts/modes/security-audit.md"),
	asset!(ModeBench, Mode, Status, Volatile, "../prompts/modes/bench.md"),
	asset!(ModeReview, Mode, Status, Volatile, "../prompts/modes/review.md"),
	asset!(ModeCleanse, Mode, Status, Volatile, "../prompts/modes/cleanse.md"),
	asset!(ModeCompress, Mode, Status, Volatile, "../prompts/modes/compress.md"),
	asset!(ModeLiveCollab, Mode, Status, Volatile, "../prompts/modes/live-collab.md"),
];

/// Returns one immutable asset without allocation.
pub const fn prompt_asset(id: PromptAssetId) -> &'static PromptAsset {
	&ASSETS[id as usize]
}

/// Iterates over the deterministic built-in catalog.
pub fn prompt_assets() -> impl ExactSizeIterator<Item = &'static PromptAsset> + Clone {
	ASSETS.iter()
}

/// Returns the lazily compiled scribe template for one catalog asset.
pub fn prompt_template(id: PromptAssetId) -> &'static Template {
	static TEMPLATES: [OnceLock<Template>; ASSETS.len()] = [const { OnceLock::new() }; ASSETS.len()];
	TEMPLATES[id as usize].get_or_init(|| {
		let asset = prompt_asset(id);
		prompt_engine::engine()
			.compile(asset_name(id), asset.content)
			.unwrap_or_else(|error| panic!("invalid embedded prompt asset: {error}"))
	})
}

fn asset_name(id: PromptAssetId) -> &'static str {
	const NAMES: [&str; 35] = [
		"personality/default",
		"personality/friendly",
		"personality/pragmatic",
		"lifecycle/auto-continue",
		"steering/user-interjection",
		"steering/parent-irc",
		"recovery/empty-stop-retry",
		"recovery/unexpected-stop-retry",
		"recovery/tool-call-loop-redirect",
		"recovery/thinking-loop-redirect",
		"recovery/gemini-tool-call-reminder",
		"title/system",
		"title/marker-instruction",
		"system/plan-filename",
		"recap/user",
		"roles/scout",
		"roles/reviewer",
		"roles/security-reviewer",
		"roles/task",
		"roles/librarian",
		"roles/designer",
		"roles/init",
		"modes/plan",
		"modes/prewalk",
		"modes/goal",
		"modes/vibe",
		"modes/memory-pipeline",
		"modes/advisor",
		"modes/autoresearch",
		"modes/security-audit",
		"modes/bench",
		"modes/review",
		"modes/cleanse",
		"modes/compress",
		"modes/live-collab",
	];
	NAMES[id as usize]
}

/// Returns the rich asset selected by a regime-scoped prompt setting.
pub fn prompt_slot_asset(slot: &str) -> Option<&'static PromptAsset> {
	let id = match slot {
		"plan" | "plan-yolo" => PromptAssetId::ModePlan,
		"prewalk" => PromptAssetId::ModePrewalk,
		"goal" => PromptAssetId::ModeGoal,
		"vibe" => PromptAssetId::ModeVibe,
		"memory-pipeline" => PromptAssetId::ModeMemoryPipeline,
		"advisor" => PromptAssetId::ModeAdvisor,
		"autoresearch" => PromptAssetId::ModeAutoresearch,
		"security-audit" => PromptAssetId::ModeSecurityAudit,
		"bench" => PromptAssetId::ModeBench,
		"review" => PromptAssetId::ModeReview,
		"cleanse" => PromptAssetId::ModeCleanse,
		"compress" => PromptAssetId::ModeCompress,
		"live-collab" => PromptAssetId::ModeLiveCollab,
		_ => return None,
	};
	Some(prompt_asset(id))
}

/// Renders the typed retry count into the empty-stop template.
pub fn render_empty_stop_retry(out: &mut String, retry_count: usize, max_retries: usize) {
	let mut props = Props::new();
	props.set(prompt_keys::RETRY_COUNT, retry_count);
	props.set(prompt_keys::MAX_RETRIES, max_retries);
	render_into(PromptAssetId::EmptyStopRetry, &props, out);
}

/// Renders one parent-agent steering notice into an existing buffer.
pub fn render_parent_irc(out: &mut String, from: &str, message: &str) {
	let mut props = Props::new();
	props.set(prompt_keys::FROM, from.to_owned());
	props.set(prompt_keys::MESSAGE, message.to_owned());
	render_into(PromptAssetId::ParentIrc, &props, out);
}

/// Renders bounded loop evidence into the repeated-tool-call redirect.
pub fn render_tool_call_loop_redirect(out: &mut String, count: u32, digest: &str) {
	let mut props = Props::new();
	props.set(prompt_keys::TOOL_NAME, "the same tool");
	props.set(prompt_keys::COUNT, count);
	props.set(prompt_keys::ARGUMENTS_SUMMARY, digest.to_owned());
	props.set(prompt_keys::RESULT_SUMMARY, "See the immediately preceding tool result.");
	render_into(PromptAssetId::ToolCallLoopRedirect, &props, out);
}

fn render_into(id: PromptAssetId, props: &Props, out: &mut String) {
	prompt_template(id)
		.render(prompt_engine::engine(), props, out)
		.expect("typed prompt props satisfy the embedded template");
}

#[cfg(test)]
mod tests {
	use std::collections::HashSet;

	use super::*;
	use crate::prompt_keys::ALL;

	#[test]
	fn catalog_templates_parse_and_reference_registered_keys() {
		let keys = ALL.iter().copied().collect::<HashSet<_>>();
		for asset in prompt_assets().filter(|asset| asset.id != PromptAssetId::ModeCompress) {
			let template = prompt_template(asset.id);
			for key in template.referenced_keys() {
				let per_use_recap_key =
					asset.id == PromptAssetId::RecapUser && matches!(key, "goal" | "task");
				assert!(
					keys.contains(key) || per_use_recap_key,
					"{} references unregistered key {key}",
					template.name()
				);
			}
		}
	}

	#[test]
	fn dynamic_assets_replace_every_typed_slot() {
		let mut parent = String::new();
		render_parent_irc(&mut parent, "parent", "steer now");
		assert!(parent.contains("parent"));
		assert!(parent.contains("steer now"));

		let mut redirect = String::new();
		render_tool_call_loop_redirect(&mut redirect, 3, "digest:abc");
		assert!(redirect.contains("3 consecutive"));
		assert!(redirect.contains("digest:abc"));
	}
}
