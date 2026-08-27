//! Bounded, gitignore-aware workspace tree formatting.

use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
	time::{Duration, Instant},
};

use omp_core::Str;
use omp_walker::{FileType, WalkDetail, WalkRequest};
use thiserror::Error;

const DEFAULT_DEPTH: usize = 3;
const DEFAULT_PER_DIRECTORY: usize = 12;
const DEFAULT_LINE_LIMIT: usize = 120;
const DEFAULT_BYTE_LIMIT: usize = 32 * 1024;
const DEFAULT_ENTRY_LIMIT: usize = 20_000;
const DEFAULT_DEADLINE: Duration = Duration::from_millis(250);

/// One stable formatted workspace tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormattedWorkspaceTree {
	/// Canonical root used for the walk.
	pub root:        PathBuf,
	/// Stable prompt-ready text.
	pub rendered:    Str,
	/// Number of rendered lines.
	pub total_lines: usize,
	/// Whether an entry, directory, line, byte, or deadline bound was reached.
	pub truncated:   bool,
}

/// Workspace tree construction failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum WorkspaceTreeError {
	/// The native walker could not complete the bounded traversal.
	#[error("workspace tree traversal failed or exceeded its deadline")]
	WalkFailed,
}

/// Bounded tree builder over the native walker.
#[derive(Clone, Debug)]
pub struct WorkspaceTreeBuilder {
	root:          PathBuf,
	depth:         usize,
	per_directory: usize,
	line_limit:    usize,
	byte_limit:    usize,
	entry_limit:   usize,
	deadline:      Duration,
}

impl WorkspaceTreeBuilder {
	/// Creates a builder with production prompt-tree bounds.
	pub fn new(root: impl Into<PathBuf>) -> Self {
		Self {
			root:          root.into(),
			depth:         DEFAULT_DEPTH,
			per_directory: DEFAULT_PER_DIRECTORY,
			line_limit:    DEFAULT_LINE_LIMIT,
			byte_limit:    DEFAULT_BYTE_LIMIT,
			entry_limit:   DEFAULT_ENTRY_LIMIT,
			deadline:      DEFAULT_DEADLINE,
		}
	}

	/// Formats the root using hidden-file exclusion and gitignore rules.
	pub fn build(&self) -> Result<FormattedWorkspaceTree, WorkspaceTreeError> {
		let request = WalkRequest::new(&self.root)
			.hidden(false)
			.gitignore(true)
			.skip_git(true)
			.depth(1, 64)
			.detail(WalkDetail::Full)
			.limit(self.entry_limit);
		let deadline = Instant::now() + self.deadline;
		let outcome = request
			.collect_with_heartbeat(|| {
				(Instant::now() <= deadline)
					.then_some(())
					.ok_or("workspace tree deadline")
			})
			.map_err(|_| WorkspaceTreeError::WalkFailed)?;

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
			.filter(|entry| entry.depth() <= self.depth)
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
		let mut truncated = outcome.stats.limited_entries > 0;
		while let Some(parent) = queue.pop() {
			let Some(all) = children.get(&parent) else {
				continue;
			};
			let selected = if all.len() > self.per_directory {
				truncated = true;
				let mut selected = all[..self.per_directory - 1].to_vec();
				selected.push(all[all.len() - 1].clone());
				selected
			} else {
				all.clone()
			};
			for (index, node) in selected.iter().enumerate() {
				if all.len() > self.per_directory && index == self.per_directory - 1 {
					lines.push(format!(
						"{}- ... {} more",
						"  ".repeat(node.depth),
						all.len() - self.per_directory
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
				.filter(|node| node.dir && node.depth < self.depth)
			{
				queue.push(node.path.clone());
			}
		}

		if lines.len() > self.line_limit {
			let removed = lines.len() - (self.line_limit - 1);
			lines.truncate(self.line_limit - 1);
			lines.push(format!("[...{removed} lines elided...]"));
			truncated = true;
		}
		let mut rendered = String::new();
		for line in lines {
			let separator = usize::from(!rendered.is_empty());
			if rendered.len() + separator + line.len() > self.byte_limit {
				truncated = true;
				break;
			}
			if separator != 0 {
				rendered.push('\n');
			}
			rendered.push_str(&line);
		}
		if truncated {
			let marker = "\n[...content elided...]";
			if rendered.len() + marker.len() <= self.byte_limit {
				rendered.push_str(marker);
			}
		}
		Ok(FormattedWorkspaceTree {
			root: self.root.clone(),
			total_lines: rendered.lines().count(),
			rendered: Str::from(rendered),
			truncated,
		})
	}

	/// Returns the walk root.
	pub fn root(&self) -> &Path {
		&self.root
	}
}
