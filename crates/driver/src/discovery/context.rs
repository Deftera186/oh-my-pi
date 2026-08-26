//! Granted-root context-file discovery and immutable prompt projection.

use std::{
	cmp::Reverse,
	collections::{BTreeMap, BTreeSet},
	fs,
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_agent::{ContextFile, dedupe_context_file_indices};
use omp_core::Str;

use super::{
	at_path::expand_at_paths,
	manifest::{
		CapabilityPayload, ContextPayload, DiscoveredCapability, SourceProvenance, SourceScope,
	},
};

/// Standalone repository context filenames accepted during ancestor discovery.
pub const REPO_SURFACE_CONTEXT_FILES: &[&str] =
	&["AGENTS.md", "CLAUDE.md", "GEMINI.md", ".cursorrules"];

/// One Environment-granted root and the working directory whose ancestor chain
/// is eligible within that root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantedContextRoot {
	/// Canonical grant boundary.
	pub root:  PathBuf,
	/// Canonical starting directory, normally the per-root cwd.
	pub start: PathBuf,
}

/// Context discovery configuration frozen with a prompt snapshot.
#[derive(Clone, Debug)]
pub struct ContextDiscoveryOptions {
	/// Manifest-declared native context names. Foreign imports remain restricted
	/// to `REPO_SURFACE_CONTEXT_FILES` regardless of this list.
	pub filenames:          Arc<[Str]>,
	/// Maximum ancestor edges per root.
	pub max_depth:          usize,
	/// Explicit user home used for user-scope provider conventions. `None`
	/// disables user-scope discovery for deterministic embedded callers.
	pub home:               Option<PathBuf>,
	/// Discovery provider ids disabled by the effective model/settings policy.
	pub disabled_providers: Arc<[Str]>,
}

impl Default for ContextDiscoveryOptions {
	fn default() -> Self {
		Self {
			filenames:          REPO_SURFACE_CONTEXT_FILES
				.iter()
				.copied()
				.map(Str::from)
				.collect::<Vec<_>>()
				.into(),
			max_depth:          64,
			home:               None,
			disabled_providers: Arc::from([]),
		}
	}
}

/// Immutable context item retaining root, depth, source and exact bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextItem {
	/// Root ordinal from the Environment snapshot.
	pub root_index: usize,
	/// Canonical source path.
	pub path:       PathBuf,
	/// Ancestor distance from the root's start directory; zero is closest.
	pub depth:      u16,
	/// Expanded content bytes.
	pub content:    Str,
}

/// Immutable discovered context snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextSnapshot {
	/// Deterministically merged sources ordered from least to most
	/// authoritative, with normalized paragraph-contained sources removed.
	pub items:       Arc<[ContextItem]>,
	/// Bounded non-fatal diagnostics.
	pub diagnostics: Arc<[ContextDiagnostic]>,
}

/// Context discovery diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextDiagnostic {
	/// An ungranted start path was refused.
	OutsideGrant(PathBuf),
	/// A source could not be read.
	Unreadable(PathBuf),
	/// Ancestor scanning hit its configured bound.
	Truncated(PathBuf),
}

/// Discovers context only beneath explicit grant boundaries. Paths are
/// ancestor-walked, `@path` expansion reuses the canonical context importer,
/// and normalized paragraph containment favors sources closest to the workspace
/// directory.
pub fn discover(
	roots: &[GrantedContextRoot],
	options: &ContextDiscoveryOptions,
) -> ContextSnapshot {
	let allowed = options
		.filenames
		.iter()
		.map(|name| name.as_str())
		.filter(|name| REPO_SURFACE_CONTEXT_FILES.contains(name))
		.collect::<BTreeSet<_>>();
	let disabled = options
		.disabled_providers
		.iter()
		.map(Str::as_str)
		.collect::<BTreeSet<_>>();
	let mut diagnostics = Vec::new();
	let mut project_winners = BTreeMap::<(usize, u16), ContextCandidate>::new();
	for (root_index, grant) in roots.iter().enumerate() {
		let root = fs::canonicalize(&grant.root).unwrap_or_else(|_| grant.root.clone());
		let start = fs::canonicalize(&grant.start).unwrap_or_else(|_| grant.start.clone());
		if !start.starts_with(&root) {
			diagnostics.push(ContextDiagnostic::OutsideGrant(start));
			continue;
		}
		let mut current = start.as_path();
		let mut reached_root = false;
		let mut native_owner_found = false;
		for depth in 0..=options.max_depth {
			let depth = u16::try_from(depth).unwrap_or(u16::MAX);
			if !native_owner_found && !disabled.contains("native") {
				let native = current.join(".omp");
				if directory_has_entries(&native) {
					native_owner_found = true;
					admit_candidate(
						&mut project_winners,
						(root_index, depth),
						ContextCandidate::project(root_index, depth, 100, native.join("AGENTS.md")),
						&mut diagnostics,
					);
				}
			}
			for name in &allowed {
				let (provider, priority) = match *name {
					"CLAUDE.md" => ("claude", 80),
					"GEMINI.md" => ("gemini", 60),
					".cursorrules" => ("cursor", 50),
					_ => ("agents-md", 10),
				};
				if disabled.contains(provider) {
					continue;
				}
				admit_candidate(
					&mut project_winners,
					(root_index, depth),
					ContextCandidate::project(root_index, depth, priority, current.join(name)),
					&mut diagnostics,
				);
			}
			for (relative, priority) in [(".agent/AGENTS.md", 70), (".agents/AGENTS.md", 70)] {
				if disabled.contains("agents") {
					continue;
				}
				admit_candidate(
					&mut project_winners,
					(root_index, depth),
					ContextCandidate::project(root_index, depth, priority, current.join(relative)),
					&mut diagnostics,
				);
			}
			if depth == 0 {
				for (provider, relative, priority) in [
					("claude", ".claude/CLAUDE.md", 80),
					("gemini", ".gemini/GEMINI.md", 60),
					("github", ".github/copilot-instructions.md", 30),
				] {
					if disabled.contains(provider) {
						continue;
					}
					admit_candidate(
						&mut project_winners,
						(root_index, depth),
						ContextCandidate::project(root_index, depth, priority, current.join(relative)),
						&mut diagnostics,
					);
				}
			}
			if current == root {
				reached_root = true;
				break;
			}
			let Some(parent) = current.parent() else {
				break;
			};
			if parent == current || !parent.starts_with(&root) {
				break;
			}
			current = parent;
		}
		if !reached_root {
			diagnostics.push(ContextDiagnostic::Truncated(grant.start.clone()));
		}
	}

	let mut candidates = project_winners.into_values().collect::<Vec<_>>();
	candidates.sort_by_key(|candidate| {
		(candidate.item.root_index, Reverse(candidate.item.depth), Reverse(candidate.priority))
	});
	if let Some(home) = options.home.as_deref() {
		let mut user_winner = None;
		let native = super::native::user_config_root(home).join("AGENTS.md");
		for (provider, path, priority) in [
			("native", native, 100),
			("claude", home.join(".claude/CLAUDE.md"), 80),
			("codex", home.join(".codex/AGENTS.md"), 70),
			("agents", home.join(".agent/AGENTS.md"), 70),
			("agents", home.join(".agents/AGENTS.md"), 70),
			("gemini", home.join(".gemini/GEMINI.md"), 60),
			("opencode", home.join(".config/opencode/AGENTS.md"), 55),
			("github", home.join(".copilot/copilot-instructions.md"), 30),
		] {
			if disabled.contains(provider) {
				continue;
			}
			admit_user_candidate(&mut user_winner, path, priority, &mut diagnostics);
		}
		if !disabled.contains("github")
			&& let Some(copilot_home) =
				std::env::var_os("COPILOT_HOME").filter(|value| !value.is_empty())
		{
			admit_user_candidate(
				&mut user_winner,
				PathBuf::from(copilot_home).join("copilot-instructions.md"),
				30,
				&mut diagnostics,
			);
		}
		if let Some(candidate) = user_winner {
			candidates.push(candidate);
		}
	}
	let mut items = candidates
		.into_iter()
		.map(|candidate| candidate.item)
		.collect::<Vec<_>>();
	let mut exact = BTreeSet::new();
	let mut keep = vec![false; items.len()];
	for (index, item) in items.iter().enumerate().rev() {
		if exact.insert(item.content.clone()) {
			keep[index] = true;
		}
	}
	items = items
		.into_iter()
		.enumerate()
		.filter_map(|(index, item)| keep[index].then_some(item))
		.collect();
	let comparable = items
		.iter()
		.map(|item| {
			ContextFile::new(item.path.clone(), item.content.as_bytes().to_vec())
				.with_depth(item.depth)
		})
		.collect::<Vec<_>>();
	items = dedupe_context_file_indices(&comparable)
		.into_iter()
		.map(|index| items[index].clone())
		.collect();
	ContextSnapshot { items: items.into(), diagnostics: diagnostics.into() }
}

#[derive(Clone, Debug)]
struct ContextCandidate {
	item:     ContextItem,
	priority: u8,
}

impl ContextCandidate {
	fn project(root_index: usize, depth: u16, priority: u8, path: PathBuf) -> Self {
		Self { item: ContextItem { root_index, path, depth, content: Str::new_static("") }, priority }
	}
}

fn directory_has_entries(path: &Path) -> bool {
	fs::read_dir(path)
		.ok()
		.and_then(|mut entries| entries.next())
		.is_some()
}

fn read_candidate(
	mut candidate: ContextCandidate,
	diagnostics: &mut Vec<ContextDiagnostic>,
) -> Option<ContextCandidate> {
	if !candidate.item.path.is_file() {
		return None;
	}
	match expand_at_paths(&candidate.item.path) {
		Ok(content) if !content.trim().is_empty() => {
			candidate.item.path =
				fs::canonicalize(&candidate.item.path).unwrap_or(candidate.item.path);
			candidate.item.content = Str::from(content);
			Some(candidate)
		},
		Ok(_) => None,
		Err(_) => {
			diagnostics.push(ContextDiagnostic::Unreadable(candidate.item.path));
			None
		},
	}
}

fn admit_candidate(
	winners: &mut BTreeMap<(usize, u16), ContextCandidate>,
	key: (usize, u16),
	candidate: ContextCandidate,
	diagnostics: &mut Vec<ContextDiagnostic>,
) {
	let Some(candidate) = read_candidate(candidate, diagnostics) else {
		return;
	};
	if winners
		.get(&key)
		.is_none_or(|current| candidate.priority > current.priority)
	{
		winners.insert(key, candidate);
	}
}

fn admit_user_candidate(
	winner: &mut Option<ContextCandidate>,
	path: PathBuf,
	priority: u8,
	diagnostics: &mut Vec<ContextDiagnostic>,
) {
	let candidate = ContextCandidate::project(usize::MAX, 0, priority, path);
	let Some(candidate) = read_candidate(candidate, diagnostics) else {
		return;
	};
	if winner
		.as_ref()
		.is_none_or(|current| candidate.priority > current.priority)
	{
		*winner = Some(candidate);
	}
}

/// Projects exact context bytes into the agent prompt contract without
/// filesystem access.
pub fn prompt_files(snapshot: &ContextSnapshot) -> Arc<[ContextFile]> {
	snapshot
		.items
		.iter()
		.map(|item| {
			ContextFile::new(item.path.clone(), item.content.as_bytes().to_vec())
				.with_origin(Str::from(item.path.to_string_lossy().as_ref()))
				.with_depth(item.depth)
		})
		.collect::<Vec<_>>()
		.into()
}

/// Lowers an immutable context snapshot into registry declarations without
/// re-reading the filesystem.
pub fn declarations(snapshot: &ContextSnapshot) -> Vec<DiscoveredCapability> {
	snapshot
		.items
		.iter()
		.map(|item| {
			let payload = ContextPayload {
				path:    item.path.clone(),
				content: item.content.clone(),
				depth:   Some(item.depth),
			};
			let source = SourceProvenance::native("context", item.path.clone(), SourceScope::Project);
			DiscoveredCapability::keyed(
				item.path.to_string_lossy().as_ref(),
				CapabilityPayload::ContextFiles(payload),
				source,
			)
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn granted_walk_is_depth_sorted_and_dedupes_contained_context() {
		let tree = tempfile::tempdir().unwrap();
		let root = tree.path().join("repo");
		let nested = root.join("a/b");
		fs::create_dir_all(&nested).unwrap();
		fs::write(root.join("AGENTS.md"), "shared").unwrap();
		fs::write(root.join("a/CLAUDE.md"), "shared\n\nnear-only").unwrap();
		fs::write(nested.join(".cursorrules"), "closest").unwrap();
		let snapshot = discover(
			&[GrantedContextRoot { root, start: nested }],
			&ContextDiscoveryOptions::default(),
		);
		assert_eq!(snapshot.items.len(), 2);
		assert!(snapshot.items[0].path.ends_with("CLAUDE.md"));
		assert!(snapshot.items[1].path.ends_with(".cursorrules"));
	}

	#[test]
	fn rejects_starts_outside_grant() {
		let tree = tempfile::tempdir().unwrap();
		let snapshot = discover(
			&[GrantedContextRoot { root: tree.path().join("a"), start: tree.path().join("b") }],
			&ContextDiscoveryOptions::default(),
		);
		assert!(matches!(snapshot.diagnostics.first(), Some(ContextDiagnostic::OutsideGrant(_))));
	}
	#[test]
	fn provider_priority_and_nearest_non_empty_native_owner_are_enforced() {
		let tree = tempfile::tempdir().expect("tree");
		let root = tree.path().join("repo");
		let cwd = root.join("packages/api");
		fs::create_dir_all(cwd.join(".omp")).expect("nearest native");
		fs::create_dir_all(cwd.join(".github")).expect("github");
		fs::create_dir_all(root.join(".omp")).expect("far native");
		fs::write(cwd.join("AGENTS.md"), "standalone").expect("standalone");
		fs::write(cwd.join(".github/copilot-instructions.md"), "github").expect("github");
		fs::write(cwd.join(".omp/AGENTS.md"), "nearest-native").expect("native");
		fs::write(root.join(".omp/AGENTS.md"), "far-native").expect("far native");
		fs::write(root.join("AGENTS.md"), "root-standalone").expect("root standalone");
		let snapshot =
			discover(&[GrantedContextRoot { root, start: cwd }], &ContextDiscoveryOptions::default());
		assert_eq!(snapshot.items.len(), 2);
		assert_eq!(snapshot.items[0].content.as_str(), "root-standalone");
		assert_eq!(snapshot.items[1].content.as_str(), "nearest-native");
		assert!(snapshot.items.iter().all(|item| {
			item.content.as_str() != "github" && item.content.as_str() != "far-native"
		}));
	}

	#[test]
	fn highest_priority_user_context_sorts_after_project_context() {
		let tree = tempfile::tempdir().expect("tree");
		let home = tree.path().join("home");
		let root = tree.path().join("repo");
		fs::create_dir_all(home.join(".omp/agent")).expect("user native");
		fs::create_dir_all(home.join(".claude")).expect("user claude");
		fs::create_dir_all(&root).expect("project");
		fs::write(home.join(".omp/agent/AGENTS.md"), "user-native").expect("user native");
		fs::write(home.join(".claude/CLAUDE.md"), "user-claude").expect("user claude");
		fs::write(root.join("AGENTS.md"), "project").expect("project");
		let snapshot = discover(
			&[GrantedContextRoot { root: root.clone(), start: root }],
			&ContextDiscoveryOptions { home: Some(home), ..ContextDiscoveryOptions::default() },
		);
		assert_eq!(
			snapshot
				.items
				.iter()
				.map(|item| item.content.as_str())
				.collect::<Vec<_>>(),
			["project", "user-native"],
		);
	}
	#[test]
	fn disabled_provider_releases_its_shadowed_context_scope() {
		let tree = tempfile::tempdir().expect("tree");
		let root = tree.path().join("repo");
		fs::create_dir_all(root.join(".github")).expect("github");
		fs::write(root.join("AGENTS.md"), "standalone").expect("standalone");
		fs::write(root.join(".github/copilot-instructions.md"), "github").expect("github");
		let snapshot = discover(
			&[GrantedContextRoot { root: root.clone(), start: root }],
			&ContextDiscoveryOptions {
				disabled_providers: Arc::from([Str::new_static("github")]),
				..ContextDiscoveryOptions::default()
			},
		);
		assert_eq!(snapshot.items.len(), 1);
		assert_eq!(snapshot.items[0].content.as_str(), "standalone");
	}
}
