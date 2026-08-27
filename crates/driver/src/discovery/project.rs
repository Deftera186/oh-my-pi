//! Immutable repository facts and bounded workspace-tree discovery.

use std::{
	collections::{BTreeMap, BTreeSet},
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, Instant},
};

use omp_agent::{ActiveRepositoryInput, RepositoryInput, WorkspaceTreeInput};
use omp_core::Str;
use omp_proto::env::v1::{RepositoryAvailability, RepositorySnapshot};
use omp_walker::{FileType, WalkDetail, WalkRequest};
use url::Url;

use super::active_repo::resolve_active_repo_context;

/// Maximum number of `AGENTS.md` sources retained from a tree.
pub const AGENTS_MD_LIMIT: usize = 200;
/// Prompt tree depth below each workspace root.
pub const WORKSPACE_TREE_DEPTH: usize = 3;
const PER_DIRECTORY_LIMIT: usize = 12;
const LINE_LIMIT: usize = 120;
const BYTE_LIMIT: usize = 32 * 1024;
const WALK_ENTRY_LIMIT: usize = 20_000;
const WALK_TIME_LIMIT: Duration = Duration::from_millis(250);

/// Environment-owned repository facts projected without running git.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryFacts {
	/// Granted workspace root this snapshot belongs to.
	pub workspace_root: PathBuf,
	/// Nested worktree root when distinct from the granted root.
	pub worktree_root:  PathBuf,
	/// Primary repository root identity.
	pub primary_root:   PathBuf,
	/// HEAD object identity.
	pub head:           Str,
	/// Current branch, if attached.
	pub branch:         Str,
	/// Changed-path counts from the same Environment revision.
	pub staged:         u32,
	/// Changed-path counts from the same Environment revision.
	pub unstaged:       u32,
	/// Changed-path counts from the same Environment revision.
	pub untracked:      u32,
	/// Environment repository revision.
	pub revision:       u64,
	/// Whether Environment bounded the changed-path projection.
	pub truncated:      bool,
}

/// One immutable, per-root workspace tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceTree {
	/// Canonical root.
	pub root:         PathBuf,
	/// Stable rendered tree.
	pub rendered:     Str,
	/// Number of rendered lines.
	pub total_lines:  usize,
	/// Whether native, per-directory, line, byte, or entry caps elided content.
	pub truncated:    bool,
	/// Deterministically sorted `AGENTS.md` files, capped at 200.
	pub agents_files: Arc<[PathBuf]>,
}

/// Frozen project discovery contract consumed by prompt composition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectSnapshot {
	/// Repository facts in granted-root order.
	pub repositories:      Arc<[RepositoryFacts]>,
	/// Workspace trees in granted-root order; empty when the default-off toggle
	/// is disabled.
	pub trees:             Arc<[WorkspaceTree]>,
	/// Exact scoped `AGENTS.md` contributions captured from the same bounded
	/// tree discovery generation.
	pub directory_context: Arc<[Str]>,
	/// Exactly-one direct child repository adopted during snapshot preparation
	/// when Environment reported no repository facts.
	pub active_repository: Option<PathBuf>,
}

/// Converts one Environment repository snapshot into stable facts. Non-repo
/// and unavailable snapshots intentionally produce no block.
pub fn repository_facts(
	workspace_root: &Path,
	snapshot: &RepositorySnapshot,
) -> Option<RepositoryFacts> {
	if snapshot.availability != RepositoryAvailability::Available as i32 {
		return None;
	}
	Some(RepositoryFacts {
		workspace_root: workspace_root.to_path_buf(),
		worktree_root:  uri_path(&snapshot.worktree_root_uri)?,
		primary_root:   uri_path(&snapshot.primary_root_uri)?,
		head:           Str::from(snapshot.head.as_str()),
		branch:         Str::from(snapshot.branch.as_str()),
		staged:         snapshot.staged,
		unstaged:       snapshot.unstaged,
		untracked:      snapshot.untracked,
		revision:       snapshot.revision,
		truncated:      snapshot.truncated,
	})
}

fn uri_path(uri: &str) -> Option<PathBuf> {
	Url::parse(uri).ok()?.to_file_path().ok()
}

/// Builds a depth-three, gitignore-aware, mtime-sorted prompt tree and collects
/// scoped `AGENTS.md` paths from the same bounded walker result. The caller
/// invokes this only when the default-off workspace-tree setting is enabled.
pub fn build_workspace_tree(root: &Path) -> WorkspaceTree {
	let request = WalkRequest::new(root)
		.hidden(false)
		.gitignore(true)
		.skip_git(true)
		.depth(1, 64)
		.detail(WalkDetail::Full)
		.limit(WALK_ENTRY_LIMIT);
	let deadline = Instant::now() + WALK_TIME_LIMIT;
	let outcome = request.collect_with_heartbeat(|| {
		(Instant::now() <= deadline)
			.then_some(())
			.ok_or("workspace tree deadline")
	});
	let Ok(outcome) = outcome else {
		return WorkspaceTree {
			root:         root.to_path_buf(),
			rendered:     Str::default(),
			total_lines:  0,
			truncated:    true,
			agents_files: Arc::from([]),
		};
	};
	let mut agents_files = outcome
		.entries
		.iter()
		.filter(|entry| entry.is_file() && entry.path.rsplit('/').next() == Some("AGENTS.md"))
		.map(|entry| entry.absolute_path(root))
		.collect::<Vec<_>>();
	agents_files.sort();
	agents_files.dedup();
	let agents_truncated = agents_files.len() > AGENTS_MD_LIMIT;
	agents_files.truncate(AGENTS_MD_LIMIT);

	#[derive(Clone)]
	struct Node {
		name:  String,
		path:  String,
		dir:   bool,
		depth: usize,
		mtime: u64,
		size:  u64,
	}
	let mut children = BTreeMap::<String, Vec<Node>>::new();
	for entry in outcome
		.entries
		.iter()
		.filter(|entry| entry.depth() <= WORKSPACE_TREE_DEPTH)
	{
		let (parent, name) = entry
			.path
			.rsplit_once('/')
			.unwrap_or(("", entry.path.as_str()));
		children.entry(parent.to_owned()).or_default().push(Node {
			name:  name.to_owned(),
			path:  entry.path.clone(),
			dir:   entry.file_type == FileType::Dir,
			depth: entry.depth(),
			mtime: entry.mtime.unwrap_or_default().max(0.0) as u64,
			size:  entry.size.unwrap_or_default().max(0.0) as u64,
		});
	}
	for values in children.values_mut() {
		values.sort_by(|left, right| {
			right
				.mtime
				.cmp(&left.mtime)
				.then_with(|| left.name.cmp(&right.name))
		});
	}
	let mut lines = vec![".".to_owned()];
	let mut queue = vec![String::new()];
	let mut truncated = agents_truncated || outcome.stats.limited_entries > 0;
	while let Some(parent) = queue.pop() {
		let Some(all) = children.get(&parent) else {
			continue;
		};
		let selected = if all.len() > PER_DIRECTORY_LIMIT {
			truncated = true;
			let mut selected = all[..PER_DIRECTORY_LIMIT - 1].to_vec();
			selected.push(all[all.len() - 1].clone());
			selected
		} else {
			all.clone()
		};
		for (index, node) in selected.iter().enumerate() {
			if all.len() > PER_DIRECTORY_LIMIT && index == PER_DIRECTORY_LIMIT - 1 {
				lines.push(format!(
					"{}- … {} more",
					"  ".repeat(node.depth),
					all.len() - PER_DIRECTORY_LIMIT
				));
			}
			let suffix = if node.dir { "/" } else { "" };
			let metadata = if node.dir {
				String::new()
			} else {
				format!("  {}B  {}ms", node.size, node.mtime)
			};
			lines.push(format!("{}- {}{}{}", "  ".repeat(node.depth), node.name, suffix, metadata));
		}
		for node in selected
			.iter()
			.rev()
			.filter(|node| node.dir && node.depth < WORKSPACE_TREE_DEPTH)
		{
			queue.push(node.path.clone());
		}
	}
	if lines.len() > LINE_LIMIT {
		let removed = lines.len() - (LINE_LIMIT - 1);
		lines.truncate(LINE_LIMIT - 1);
		lines.push(format!("[…{removed}ln elided…]"));
		truncated = true;
	}
	let mut rendered = lines.join("\n");
	if rendered.len() > BYTE_LIMIT {
		let boundary = rendered.floor_char_boundary(BYTE_LIMIT.saturating_sub(20));
		rendered.truncate(boundary);
		rendered.push_str("\n[…bytes elided…]");
		truncated = true;
	}
	WorkspaceTree {
		root: root.to_path_buf(),
		total_lines: rendered.lines().count(),
		rendered: Str::from(rendered),
		truncated,
		agents_files: agents_files.into(),
	}
}

/// Freezes repository facts and optional workspace trees in granted-root order.
pub fn freeze(
	roots: &[PathBuf],
	repository_snapshots: &[RepositorySnapshot],
	include_workspace_tree: bool,
) -> ProjectSnapshot {
	let repositories: Arc<[RepositoryFacts]> = roots
		.iter()
		.zip(repository_snapshots)
		.filter_map(|(root, snapshot)| repository_facts(root, snapshot))
		.collect::<Vec<_>>()
		.into();
	let scanned_trees = roots
		.iter()
		.map(|root| build_workspace_tree(root))
		.collect::<Vec<_>>();
	let directory_context: Arc<[Str]> = merged_agents_files(&scanned_trees)
		.iter()
		.map(|path| Str::from(path.to_string_lossy().as_ref()))
		.collect::<Vec<_>>()
		.into();
	let trees: Arc<[WorkspaceTree]> = if include_workspace_tree {
		scanned_trees.into()
	} else {
		Arc::from([])
	};
	let active_repository = repositories
		.is_empty()
		.then(|| roots.first())
		.flatten()
		.and_then(|root| resolve_active_repo_context(root).ok().flatten())
		.map(|context| context.relative_repo_root);
	ProjectSnapshot { repositories, trees, directory_context, active_repository }
}

/// Returns a stable nested-repository identity relative to its granted root.
pub fn nested_repository_identity(facts: &RepositoryFacts) -> Option<PathBuf> {
	(facts.worktree_root != facts.workspace_root).then(|| {
		facts
			.worktree_root
			.strip_prefix(&facts.workspace_root)
			.unwrap_or(&facts.worktree_root)
			.to_path_buf()
	})
}

/// Projects repository facts into the prompt contract.
pub fn prompt_repositories(snapshot: &ProjectSnapshot) -> Arc<[RepositoryInput]> {
	snapshot
		.repositories
		.iter()
		.map(|facts| RepositoryInput {
			root_uri:          path_uri(&facts.workspace_root),
			worktree_root_uri: path_uri(&facts.worktree_root),
			primary_root_uri:  path_uri(&facts.primary_root),
			head:              facts.head.clone(),
			branch:            facts.branch.clone(),
			staged:            facts.staged,
			unstaged:          facts.unstaged,
			untracked:         facts.untracked,
			revision:          facts.revision,
			truncated:         facts.truncated,
		})
		.collect::<Vec<_>>()
		.into()
}

/// Projects bounded per-root trees into the prompt contract.
pub fn prompt_trees(snapshot: &ProjectSnapshot) -> Arc<[WorkspaceTreeInput]> {
	snapshot
		.trees
		.iter()
		.map(|tree| WorkspaceTreeInput {
			root_uri:  path_uri(&tree.root),
			rendered:  tree.rendered.clone(),
			truncated: tree.truncated,
		})
		.collect::<Vec<_>>()
		.into()
}

/// Projects the first nested repository relative identity, if present.
pub fn prompt_active_repository(snapshot: &ProjectSnapshot) -> Option<ActiveRepositoryInput> {
	snapshot
		.active_repository
		.clone()
		.or_else(|| {
			snapshot
				.repositories
				.iter()
				.find_map(nested_repository_identity)
		})
		.map(|path| ActiveRepositoryInput {
			relative_root: Str::from(path.to_string_lossy().replace('\\', "/")),
		})
}

fn path_uri(path: &Path) -> Str {
	Url::from_file_path(path)
		.ok()
		.map_or_else(|| Str::from(path.to_string_lossy().as_ref()), |url| Str::from(url.as_str()))
}

/// Deduplicates `AGENTS.md` paths across per-root trees while retaining root
/// order and the global source cap.
pub fn merged_agents_files(trees: &[WorkspaceTree]) -> Arc<[PathBuf]> {
	let mut seen = BTreeSet::new();
	let mut output = Vec::new();
	for path in trees.iter().flat_map(|tree| tree.agents_files.iter()) {
		if seen.insert(path.clone()) {
			output.push(path.clone());
		}
		if output.len() == AGENTS_MD_LIMIT {
			break;
		}
	}
	output.into()
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	#[test]
	fn freeze_adopts_exactly_one_direct_child_repository_without_render_io() {
		let tree = tempfile::tempdir().unwrap();
		let child = tree.path().join("repo");
		fs::create_dir_all(child.join(".git")).unwrap();
		let snapshot = freeze(&[tree.path().to_path_buf()], &[], false);
		assert_eq!(snapshot.active_repository, Some(PathBuf::from("repo")));
		assert_eq!(
			prompt_active_repository(&snapshot),
			Some(ActiveRepositoryInput { relative_root: Str::from("repo") })
		);
	}

	#[test]
	fn tree_is_mtime_sorted_depth_bounded_and_default_off() {
		let tree = tempfile::tempdir().unwrap();
		fs::create_dir_all(tree.path().join("a/b/c/d")).unwrap();
		fs::write(tree.path().join("a/AGENTS.md"), "rules").unwrap();
		fs::write(tree.path().join("a/b/c/file"), "x").unwrap();
		fs::write(tree.path().join("a/b/c/d/too-deep"), "x").unwrap();
		let built = build_workspace_tree(tree.path());
		assert!(built.rendered.contains("AGENTS.md"));
		assert!(!built.rendered.contains("too-deep"));
		let frozen = freeze(&[tree.path().to_path_buf()], &[], false);
		assert!(frozen.trees.is_empty());
	}
}
