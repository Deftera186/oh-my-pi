//! Workspace-root command validation and rendering.

use std::{
	fmt::Write as _,
	io,
	path::{Path, PathBuf},
};

use omp_agent::WorkspaceRoots;
use omp_core::Str;

/// Resolves and canonicalizes one existing directory relative to the live root.
pub(crate) async fn canonical_directory(current: &Path, raw: &str) -> miette::Result<PathBuf> {
	let requested = PathBuf::from(raw.trim());
	let requested = if requested.is_absolute() {
		requested
	} else {
		current.join(requested)
	};
	let metadata = tokio::fs::metadata(&requested).await.map_err(|error| {
		if error.kind() == io::ErrorKind::NotFound {
			miette::miette!("Directory does not exist: {}", requested.display())
		} else {
			miette::miette!("Cannot inspect directory {}: {error}", requested.display())
		}
	})?;
	if !metadata.is_dir() {
		return Err(miette::miette!("Not a directory: {}", requested.display()));
	}
	tokio::fs::canonicalize(&requested).await.map_err(|error| {
		miette::miette!("Cannot canonicalize directory {}: {error}", requested.display())
	})
}

/// Renders the durable root projection without implying a live cwd change.
pub(crate) fn render(roots: &WorkspaceRoots, current: &Path) -> Str {
	let mut rendered = String::from("**Workspace directories**\n");
	let primary_label = if roots.primary() == current {
		"primary; current session root"
	} else {
		"future primary"
	};
	let _ = writeln!(rendered, "- `{}` ({primary_label})", roots.primary().display());
	for root in roots.secondary() {
		let label = if root == current {
			"current session root"
		} else {
			"additional root"
		};
		let _ = writeln!(rendered, "- `{}` ({label})", root.display());
	}
	Str::from(rendered)
}

/// Reports the absent Environment mutation authority precisely.
pub(crate) fn mutation_unavailable(command: &str) -> miette::Report {
	let rpc = match command {
		"dir add" => "AddWorkspaceRoot",
		"dir remove" => "RemoveWorkspaceRoot",
		_ => "MutateWorkspaceRoots",
	};
	miette::miette!(
		"`/{command}` requires the missing Environment `{rpc}` RPC; env/v1 currently exposes only \
		 read-only `WorkspaceRootSetRequest`/`WorkspaceRootSet`"
	)
}
#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	#[tokio::test]
	async fn canonical_directory_accepts_only_existing_directories() {
		let temp = tempfile::tempdir().unwrap();
		let directory = temp.path().join("nested");
		let file = temp.path().join("file");
		fs::create_dir(&directory).unwrap();
		fs::write(&file, "content").unwrap();

		assert_eq!(
			canonical_directory(temp.path(), "nested").await.unwrap(),
			directory.canonicalize().unwrap()
		);
		assert!(
			canonical_directory(temp.path(), "missing")
				.await
				.unwrap_err()
				.to_string()
				.contains("does not exist")
		);
		assert!(
			canonical_directory(temp.path(), "file")
				.await
				.unwrap_err()
				.to_string()
				.contains("Not a directory")
		);
	}
}
