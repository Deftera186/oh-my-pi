//! Authoritative property-key vocabulary shared by prompt producers and
//! templates.

/// Current workspace directory.
pub const CWD: &str = "cwd";
/// Source-control identity map.
pub const VCS: &str = "vcs";
/// Bounded workstation facts map.
pub const HOST: &str = "host";
/// Selected model identity map.
pub const MODEL: &str = "model";
/// Repository fact maps.
pub const REPOSITORIES: &str = "repositories";
/// Workspace-root authority map.
pub const ROOTS: &str = "roots";
/// Additional non-primary workspace roots.
pub const ADDITIONAL_ROOTS: &str = "additional_roots";
/// Nested active repository map.
pub const ACTIVE_REPOSITORY: &str = "active_repository";
/// Workspace context-file maps.
pub const CONTEXT_FILES: &str = "context_files";
/// Deeper directory-context pointers.
pub const DIRECTORY_CONTEXT: &str = "directory_context";
/// Bounded workspace-tree maps.
pub const WORKSPACE_TREES: &str = "workspace_trees";
/// Enabled skill maps.
pub const SKILLS: &str = "skills";
/// Standing rule maps.
pub const RULES: &str = "rules";
/// Resolved personality text.
pub const PERSONALITY: &str = "personality";
/// Mermaid-rendering capability.
pub const RENDER_MERMAID: &str = "render_mermaid";
/// Workstation inclusion setting.
pub const INCLUDE_WORKSTATION: &str = "include_workstation";
/// Model inclusion setting.
pub const INCLUDE_MODEL: &str = "include_model";
/// Workspace-tree inclusion setting.
pub const INCLUDE_WORKSPACE_TREE: &str = "include_workspace_tree";
/// Skill inclusion setting.
pub const INCLUDE_SKILLS: &str = "include_skills";
/// Reversible-secret-token setting.
pub const SECRETS_ENABLED: &str = "secrets_enabled";
/// Explicit empty-prompt bypass.
pub const NULL_PROMPT: &str = "null_prompt";
/// Optional tool-intent field name.
pub const INTENT_FIELD: &str = "intent_field";
/// Optional role-replacing custom prompt.
pub const CUSTOM_PROMPT: &str = "custom_prompt";
/// Optional appended guidance prompt.
pub const APPEND_PROMPT: &str = "append_prompt";
/// Pre-rendered tool inventory.
pub const TOOL_INVENTORY: &str = "tool_inventory";
/// Callable tool wire names.
pub const TOOLS: &str = "tools";
/// Internal-resource scheme maps.
pub const SCHEMES: &str = "schemes";
/// Whether any advertised internal-resource scheme accepts selectors.
pub const SCHEME_SELECTORS: &str = "scheme_selectors";
/// Computer-use capability.
pub const COMPUTER: &str = "computer";
/// Dynamic-device guidance.
pub const DEVICE_GUIDANCE: &str = "device_guidance";
/// AutoQA filing guidance.
pub const AUTO_QA_GUIDANCE: &str = "auto_qa_guidance";
/// Delegation policy map.
pub const DELEGATION: &str = "delegation";
/// Mutation convenience map.
pub const MUTATIONS: &str = "mutations";
/// Hashline edit dialect availability.
pub const EDIT_HASHLINE: &str = "edit_hashline";
/// Apply-patch or unified edit dialect availability.
pub const EDIT_APPLY_PATCH: &str = "edit_apply_patch";
/// Sloppy edit dialect availability.
pub const EDIT_SLOPPY: &str = "edit_sloppy";
/// Memory-slot map.
pub const MEMORY: &str = "memory";
/// Subagent display name.
pub const AGENT_NAME: &str = "agent_name";
/// Subagent definition description.
pub const AGENT_DESCRIPTION: &str = "agent_description";
/// Subagent definition prompt.
pub const AGENT_PROMPT: &str = "agent_prompt";
/// Shared batch context.
pub const SHARED_CONTEXT: &str = "shared_context";
/// Active plan path.
pub const PLAN_PATH: &str = "plan_path";
/// Active plan content.
pub const PLAN_CONTENT: &str = "plan_content";
/// Effective child workspace root.
pub const WORKSPACE_ROOT: &str = "workspace_root";
/// Effective structured output schema.
pub const OUTPUT_SCHEMA: &str = "output_schema";
/// Pre-rendered output-schema TypeScript.
pub const OUTPUT_SCHEMA_TS: &str = "output_schema_ts";
/// Child IRC display alias.
pub const SELF_NAME: &str = "self_name";
/// Child IRC role.
pub const SELF_ROLE: &str = "self_role";
/// IRC availability flag.
pub const IRC_ENABLED: &str = "irc_enabled";
/// IRC roster generation.
pub const ROSTER_GENERATION: &str = "roster_generation";
/// Visible IRC peer maps.
pub const PEERS: &str = "peers";
/// Current-root parked peer count omitted from the initial prompt.
pub const PARKED_COUNT: &str = "parked_count";
/// Live peer count dropped by the initial prompt bound.
pub const OMITTED_COUNT: &str = "omitted_count";
/// Model-family capability map.
pub const CAPS: &str = "caps";
/// Read-only plan-mode flag.
pub const PLAN_MODE: &str = "plan_mode";
/// Eager-delegation mode name.
pub const EAGER: &str = "eager";
/// Current recovery retry count.
pub const RETRY_COUNT: &str = "retry_count";
/// Maximum recovery retries.
pub const MAX_RETRIES: &str = "max_retries";
/// Steering message sender.
pub const FROM: &str = "from";
/// Steering message body.
pub const MESSAGE: &str = "message";
/// Recovery tool name.
pub const TOOL_NAME: &str = "tool_name";
/// Recovery repetition count.
pub const COUNT: &str = "count";
/// Recovery tool-argument summary.
pub const ARGUMENTS_SUMMARY: &str = "arguments_summary";
/// Recovery tool-result summary.
pub const RESULT_SUMMARY: &str = "result_summary";
/// Compaction summary body.
pub const SUMMARY: &str = "summary";
/// Voice user's first name.
pub const FIRST_NAME: &str = "first_name";
/// Voice operating-system username.
pub const USERNAME: &str = "username";
/// Raw command argument tail.
pub const ARGS: &str = "args";
/// Tokenized command arguments.
pub const ARGUMENTS: &str = "arguments";

// Nested field identifiers remain registry entries because template member
// access uses the same identifier grammar as top-level lookups.
/// VCS or repository root field.
pub const ROOT: &str = "root";
/// VCS or repository head field.
pub const HEAD: &str = "head";
/// Host operating-system field.
pub const OS: &str = "os";
/// Host distribution field.
pub const DISTRO: &str = "distro";
/// Host kernel field.
pub const KERNEL: &str = "kernel";
/// Host architecture field.
pub const ARCH: &str = "arch";
/// Host CPU field.
pub const CPU: &str = "cpu";
/// Host terminal field.
pub const TERMINAL: &str = "terminal";
/// Host GPU-list field.
pub const GPUS: &str = "gpus";
/// Model identifier field.
pub const IDENTIFIER: &str = "identifier";
/// Codex task-policy field.
pub const CODEX_TASK_POLICY: &str = "codex_task_policy";
/// Repository or workspace-tree root URI field.
pub const ROOT_URI: &str = "root_uri";
/// Repository worktree-root URI field.
pub const WORKTREE_ROOT_URI: &str = "worktree_root_uri";
/// Repository primary-root URI field.
pub const PRIMARY_ROOT_URI: &str = "primary_root_uri";
/// Active repository relative-root field.
pub const RELATIVE_ROOT: &str = "relative_root";
/// Repository branch field.
pub const BRANCH: &str = "branch";
/// Repository staged-count field.
pub const STAGED: &str = "staged";
/// Repository unstaged-count field.
pub const UNSTAGED: &str = "unstaged";
/// Repository untracked-count field.
pub const UNTRACKED: &str = "untracked";
/// Monotone revision field.
pub const REVISION: &str = "revision";
/// Truncation field.
pub const TRUNCATED: &str = "truncated";
/// Primary workspace-root field.
pub const PRIMARY: &str = "primary";
/// Canonical workspace-root URI field.
pub const CANONICAL_URI: &str = "canonical_uri";
/// Opaque workspace-root grant field.
pub const GRANT_ID: &str = "grant_id";
/// Context-file path field.
pub const PATH: &str = "path";
/// Context-file or named-source origin field.
pub const ORIGIN: &str = "origin";
/// Context-file or named-source content field.
pub const CONTENT: &str = "content";
/// Rendered workspace-tree field.
pub const RENDERED: &str = "rendered";
/// Named skill or rule name field.
pub const NAME: &str = "name";
/// Named skill or rule description field.
pub const DESCRIPTION: &str = "description";
/// Scheme readable field.
pub const READABLE: &str = "readable";
/// Scheme mintable field.
pub const MINTABLE: &str = "mintable";
/// Scheme selector-support field.
pub const SELECTORS: &str = "selectors";
/// Delegation enabled field.
pub const ENABLED: &str = "enabled";
/// Delegation batch field.
pub const BATCH: &str = "batch";
/// Delegation concurrency field.
pub const CONCURRENCY: &str = "concurrency";
/// Delegation queued-count field.
pub const QUEUED: &str = "queued";
/// Delegation scout-availability field.
pub const SCOUT_AVAILABLE: &str = "scout_available";
/// Delegation coordination field.
pub const COORDINATION: &str = "coordination";
/// Mutation format-on-write field.
pub const FORMAT_ON_WRITE: &str = "format_on_write";
/// Mutation fetch field.
pub const FETCH: &str = "fetch";
/// Mutation editor field.
pub const EDITOR: &str = "editor";
/// Mutation escalation field.
pub const ESCALATION: &str = "escalation";
/// Standing-memory slot field.
pub const STANDING: &str = "standing";
/// Recall-memory slot field.
pub const RECALL: &str = "recall";
/// Peer role field.
pub const ROLE: &str = "role";
/// Peer lifecycle status field.
pub const STATUS: &str = "status";
/// Peer activity field.
pub const ACTIVITY: &str = "activity";
/// Codex-style capability field.
pub const CODEX_STYLE: &str = "codex_style";
/// Parallel-tool-call capability field.
pub const PARALLEL_TOOL_CALLS: &str = "parallel_tool_calls";
/// Structured-yield capability field.
pub const STRUCTURED_YIELD: &str = "structured_yield";

/// Every legal prompt property and nested member key.
pub const ALL: &[&str] = &[
	CWD,
	VCS,
	HOST,
	MODEL,
	REPOSITORIES,
	ROOTS,
	ADDITIONAL_ROOTS,
	ACTIVE_REPOSITORY,
	CONTEXT_FILES,
	DIRECTORY_CONTEXT,
	WORKSPACE_TREES,
	SKILLS,
	RULES,
	PERSONALITY,
	RENDER_MERMAID,
	INCLUDE_WORKSTATION,
	INCLUDE_MODEL,
	INCLUDE_WORKSPACE_TREE,
	INCLUDE_SKILLS,
	SECRETS_ENABLED,
	NULL_PROMPT,
	INTENT_FIELD,
	CUSTOM_PROMPT,
	APPEND_PROMPT,
	TOOL_INVENTORY,
	TOOLS,
	SCHEMES,
	SCHEME_SELECTORS,
	COMPUTER,
	DEVICE_GUIDANCE,
	AUTO_QA_GUIDANCE,
	DELEGATION,
	MUTATIONS,
	EDIT_HASHLINE,
	EDIT_APPLY_PATCH,
	EDIT_SLOPPY,
	MEMORY,
	AGENT_NAME,
	AGENT_DESCRIPTION,
	AGENT_PROMPT,
	SHARED_CONTEXT,
	PLAN_PATH,
	PLAN_CONTENT,
	WORKSPACE_ROOT,
	OUTPUT_SCHEMA,
	OUTPUT_SCHEMA_TS,
	SELF_NAME,
	SELF_ROLE,
	IRC_ENABLED,
	ROSTER_GENERATION,
	PEERS,
	PARKED_COUNT,
	OMITTED_COUNT,
	CAPS,
	PLAN_MODE,
	EAGER,
	RETRY_COUNT,
	MAX_RETRIES,
	FROM,
	MESSAGE,
	TOOL_NAME,
	COUNT,
	ARGUMENTS_SUMMARY,
	RESULT_SUMMARY,
	SUMMARY,
	FIRST_NAME,
	USERNAME,
	ARGS,
	ARGUMENTS,
	ROOT,
	HEAD,
	OS,
	DISTRO,
	KERNEL,
	ARCH,
	CPU,
	TERMINAL,
	GPUS,
	IDENTIFIER,
	CODEX_TASK_POLICY,
	ROOT_URI,
	WORKTREE_ROOT_URI,
	PRIMARY_ROOT_URI,
	RELATIVE_ROOT,
	BRANCH,
	STAGED,
	UNSTAGED,
	UNTRACKED,
	REVISION,
	TRUNCATED,
	PRIMARY,
	CANONICAL_URI,
	GRANT_ID,
	PATH,
	ORIGIN,
	CONTENT,
	RENDERED,
	NAME,
	DESCRIPTION,
	READABLE,
	MINTABLE,
	SELECTORS,
	ENABLED,
	BATCH,
	CONCURRENCY,
	QUEUED,
	SCOUT_AVAILABLE,
	COORDINATION,
	FORMAT_ON_WRITE,
	FETCH,
	EDITOR,
	ESCALATION,
	STANDING,
	RECALL,
	ROLE,
	STATUS,
	ACTIVITY,
	CODEX_STYLE,
	PARALLEL_TOOL_CALLS,
	STRUCTURED_YIELD,
];

#[cfg(test)]
mod tests {
	use std::collections::HashSet;

	use super::ALL;

	#[test]
	fn registry_is_unique_and_disjoint_from_reserved_words() {
		let keys = ALL.iter().copied().collect::<HashSet<_>>();
		assert_eq!(keys.len(), ALL.len(), "prompt key registry contains duplicates");
		const RESERVED: &[&str] = &[
			"if", "elif", "else", "endif", "for", "endfor", "in", "and", "or", "not", "set", "raw",
			"endraw", "true", "false", "none", "loop",
		];
		assert!(keys.is_disjoint(&RESERVED.iter().copied().collect()));
	}
}
