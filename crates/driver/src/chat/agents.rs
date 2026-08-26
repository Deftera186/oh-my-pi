//! Native OMP subagent catalog and discovery composition.

use std::{
	collections::BTreeMap,
	env, fs,
	path::{Path, PathBuf},
	sync::{Arc, LazyLock},
};

use omp_agent::{
	AgentDefinition,
	prompt_assets::{PromptAssetId, prompt_asset},
};
use omp_core::{Str, sf};

#[cfg(test)]
use crate::security_review::profile::PROFILE_ID;
use crate::{discovery::manifest, security_review::profile};

static BUNDLED: LazyLock<Arc<BTreeMap<Str, AgentDefinition>>> = LazyLock::new(|| {
	let definitions = [
		("task", TASK, PromptAssetId::AgentTask),
		("scout", SCOUT, PromptAssetId::AgentScout),
		("sonic", SONIC, PromptAssetId::AgentTask),
		("designer", DESIGNER, PromptAssetId::AgentDesigner),
		("reviewer", REVIEWER, PromptAssetId::AgentReviewer),
		("librarian", LIBRARIAN, PromptAssetId::AgentLibrarian),
	];
	Arc::new(
		definitions
			.into_iter()
			.map(|(name, frontmatter, asset)| {
				let content = prompt_asset(asset).content;
				let markdown = if let Some((_, body)) = content.split_once("\n---\n\n") {
					format!("{frontmatter}{body}")
				} else if let Some((_, body)) = content.split_once("\n---\n") {
					format!("{frontmatter}{body}")
				} else {
					format!("{frontmatter}{content}")
				};
				let definition = AgentDefinition::parse_markdown(name, &markdown)
					.expect("bundled agent definitions are build-time constants");
				(sf!(name), definition)
			})
			.collect(),
	)
});

/// Returns only the immutable build-time bundled agent definitions.
pub fn bundled() -> Arc<BTreeMap<Str, AgentDefinition>> {
	Arc::clone(&BUNDLED)
}

/// Returns the complete native catalog using project → user → extension →
/// bundled precedence.
pub fn discover(root: &Path, security_enabled: bool) -> Arc<BTreeMap<Str, AgentDefinition>> {
	let home = env::var_os("HOME").map(PathBuf::from);
	let extensions = extension_roots(root, home.as_deref());
	let declarations = manifest::agent_declarations(root, home.as_deref(), &extensions);
	let discovery = manifest::discover_agents(&declarations);
	for warning in &discovery.warnings {
		tracing::warn!(path = %warning.path.display(), error = %warning.kind, "skipping malformed agent definition");
	}
	let mut definitions = BUNDLED.as_ref().clone();
	for (name, definition) in discovery.definitions.into_iter().rev() {
		definitions.insert(name, definition);
	}
	definitions.retain(|name, _| !name.as_str().eq_ignore_ascii_case(profile::PROFILE_ID));
	if security_enabled {
		let definition = profile::definition();
		debug_assert!(crate::security_review::profile::is_canonical(&definition));
		definitions.insert(Str::new_static(profile::PROFILE_ID), definition);
	}
	Arc::new(definitions)
}

fn extension_roots(root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
	let mut roots = Vec::new();
	for extensions in
		[Some(root.join(".omp/extensions")), home.map(|home| home.join(".omp/extensions"))]
			.into_iter()
			.flatten()
	{
		let Ok(entries) = fs::read_dir(extensions) else {
			continue;
		};
		roots.extend(entries.filter_map(Result::ok).filter_map(|entry| {
			entry
				.file_type()
				.ok()
				.filter(|kind| kind.is_dir())
				.map(|_| entry.path())
		}));
	}
	roots.sort();
	roots.dedup();
	roots
}

const TASK: &str = r#"---
name: task
description: General-purpose subagent with full capabilities for delegated multi-step tasks
spawns: "*"
model: "@task"
thinkingLevel: medium
---
"#;

const SCOUT: &str = r#"---
name: scout
description: MUST be used for exploratory codebase research, rapid code analysis, and broad pattern searches. Fast read-only scout returning compressed context for handoff.
tools: [read, grep, glob, web_search]
model: "@smol"
thinkingLevel: medium
readSummarize: false
output:
  type: object
  required: [summary, files, architecture]
  properties:
    summary: { type: string }
    files: { type: array, items: { type: object } }
    architecture: { type: string }
---
"#;

const SONIC: &str = r#"---
name: sonic
description: Low-reasoning agent for strictly mechanical updates or data collection only
model: "@smol"
thinkingLevel: medium
---
"#;

const DESIGNER: &str = r#"---
name: designer
description: UI/UX specialist for design implementation, review, visual refinement
model: "@designer"
---
"#;

const REVIEWER: &str = r#"---
name: reviewer
description: Code review specialist for quality/security analysis
tools: [read, grep, glob, bash, lsp, web_search, ast_grep]
spawns: [scout]
model: "@slow"
output:
  type: object
  required: [overall_correctness, explanation, confidence]
  properties:
    overall_correctness: { enum: [correct, incorrect] }
    explanation: { type: string }
    confidence: { type: number }
    findings: { type: array, items: { type: object } }
---
"#;

const LIBRARIAN: &str = r#"---
name: librarian
description: Researches external libraries and APIs by reading source code. Returns definitive, source-verified answers.
tools: [read, grep, glob, bash, lsp, web_search, ast_grep]
model: "@smol"
thinkingLevel: minimal
readSummarize: false
output:
  type: object
  required: [answer, sources, api, version]
  properties:
    answer: { type: string }
    sources: { type: array, items: { type: object } }
    api: { type: array, items: { type: object } }
    version: { type: string }
    breaking_changes: { type: array, items: { type: string } }
    caveats: { type: array, items: { type: string } }
---
"#;
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn security_setting_solely_owns_canonical_profile_registration() {
		let root = tempfile::tempdir().unwrap();
		let disabled = discover(root.path(), false);
		assert!(
			disabled
				.keys()
				.all(|name| { !name.as_str().eq_ignore_ascii_case(PROFILE_ID) })
		);

		let enabled = discover(root.path(), true);
		let profile = enabled.get(PROFILE_ID).expect("enabled security reviewer");
		assert!(crate::security_review::profile::is_canonical(profile));
	}
}
