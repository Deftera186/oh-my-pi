//! Bounded discovery of native static assets.

use std::{
	fs, io,
	io::Read,
	path::{Path, PathBuf},
	str,
};

use omp_core::Str;
use omp_walker::WalkRequest;
use thiserror::Error;

/// Configuration scope that supplied an asset root.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DiscoveryScope {
	/// Process-level native configuration.
	Global,
	/// Canonical project configuration.
	Project,
	/// Explicit session-only input.
	Session,
}

/// Static native content understood by SDK discovery.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AssetKind {
	/// Native static extension manifest.
	Extension,
	/// Plugin package or Claude-compatible manifest.
	PluginManifest,
	/// Skill document.
	Skill,
	/// Plugin-provided agent definition.
	Agent,
	/// Persistent context file.
	Context,
	/// Rule document distinct from general persistent context.
	Rule,
	/// Reusable prompt template.
	Template,
	/// Markdown slash command.
	Command,
	/// Static MCP declaration.
	Mcp,
	/// Hook declaration or executable hook module.
	Hook,
	/// Native or JavaScript tool declaration.
	Tool,
	/// LSP server configuration.
	Lsp,
	/// DAP adapter configuration.
	Dap,
	/// JavaScript extension module declared by a plugin package.
	JavaScript,
}

/// One discovery root and the asset families accepted beneath it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryRequest {
	/// Authority scope of this root.
	pub root:         PathBuf,
	/// Scope precedence assigned by the host.
	pub source_scope: DiscoveryScope,
	/// Accepted asset families.
	pub kinds:        Box<[AssetKind]>,
}

/// One immutable bounded native asset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeAsset {
	/// Asset family.
	pub kind:         AssetKind,
	/// Configuration scope.
	pub source_scope: DiscoveryScope,
	/// Exact source path.
	pub path:         PathBuf,
	/// Validated UTF-8 source bytes.
	pub content:      Str,
}

/// Native asset discovery failure.
#[derive(Debug, Error)]
pub enum DiscoveryError {
	/// A source file could not be read.
	#[error("failed to read discovered asset {path:?}")]
	Read {
		/// Exact source path.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: io::Error,
	},
	/// A source file is not UTF-8.
	#[error("discovered asset is not UTF-8: {path:?}")]
	Encoding {
		/// Exact source path.
		path:   PathBuf,
		/// UTF-8 validation failure.
		#[source]
		source: str::Utf8Error,
	},
	/// One asset exceeds the per-file byte ceiling.
	#[error("discovered asset exceeds {limit} bytes: {path:?}")]
	AssetBudget {
		/// Exact source path.
		path:  PathBuf,
		/// Per-file ceiling.
		limit: usize,
	},
	/// All accepted assets exceed the aggregate byte ceiling.
	#[error("discovered assets exceed aggregate budget of {limit} bytes")]
	TotalBudget {
		/// Aggregate ceiling.
		limit: usize,
	},
	/// Native traversal failed.
	#[error("native asset traversal failed for {root:?}")]
	Walk {
		/// Discovery root.
		root: PathBuf,
	},
}

/// Deterministic bounded native asset loader.
#[derive(Clone, Debug)]
pub struct DiscoveryLoader {
	max_assets:          usize,
	max_asset_bytes:     usize,
	max_aggregate_bytes: usize,
}

impl DiscoveryLoader {
	/// Creates a loader with production-safe discovery bounds.
	pub const fn new() -> Self {
		Self {
			max_assets:          2_000,
			max_asset_bytes:     1024 * 1024,
			max_aggregate_bytes: 16 * 1024 * 1024,
		}
	}

	/// Loads all requested roots in precedence order.
	pub fn load(&self, requests: &[DiscoveryRequest]) -> Result<Vec<NativeAsset>, DiscoveryError> {
		let mut assets = Vec::new();
		let mut total_bytes = 0usize;
		for request in requests {
			let outcome = WalkRequest::new(&request.root)
				.hidden(true)
				.gitignore(false)
				.skip_git(true)
				.depth(1, 16)
				.limit(self.max_assets.saturating_sub(assets.len()))
				.collect()
				.map_err(|_| DiscoveryError::Walk { root: request.root.clone() })?;
			let mut candidates = outcome
				.entries
				.into_iter()
				.filter(|entry| entry.is_file())
				.filter_map(|entry| {
					classify(&entry.path, &request.kinds)
						.map(|kind| (kind, entry.absolute_path(&request.root)))
				})
				.collect::<Vec<_>>();
			candidates.sort_by(|left, right| left.1.cmp(&right.1));
			for (kind, path) in candidates {
				if assets.len() >= self.max_assets {
					break;
				}
				let content = read_bounded(&path, self.max_asset_bytes)?;
				total_bytes = total_bytes.saturating_add(content.len());
				if total_bytes > self.max_aggregate_bytes {
					return Err(DiscoveryError::TotalBudget { limit: self.max_aggregate_bytes });
				}
				assets.push(NativeAsset {
					kind,
					source_scope: request.source_scope,
					path,
					content: Str::from(content),
				});
			}
		}
		Ok(assets)
	}
}

impl Default for DiscoveryLoader {
	fn default() -> Self {
		Self::new()
	}
}

fn read_bounded(path: &Path, limit: usize) -> Result<String, DiscoveryError> {
	let file = fs::File::open(path)
		.map_err(|source| DiscoveryError::Read { path: path.to_owned(), source })?;
	let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
	file
		.take(limit as u64 + 1)
		.read_to_end(&mut bytes)
		.map_err(|source| DiscoveryError::Read { path: path.to_owned(), source })?;
	if bytes.len() > limit {
		return Err(DiscoveryError::AssetBudget { path: path.to_owned(), limit });
	}
	let content = str::from_utf8(&bytes)
		.map_err(|source| DiscoveryError::Encoding { path: path.to_owned(), source })?;
	Ok(content.to_owned())
}

fn classify(path: &str, accepted: &[AssetKind]) -> Option<AssetKind> {
	let file = path.rsplit('/').next().unwrap_or(path);
	let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
	let kind = if file == "extension.json" {
		AssetKind::Extension
	} else if matches!(file, "package.json" | "plugin.json") {
		AssetKind::PluginManifest
	} else if file == "SKILL.md" {
		AssetKind::Skill
	} else if directory_named(parent, "agents") && file.ends_with(".md") {
		AssetKind::Agent
	} else if file == "RULES.md" || directory_named(parent, "rules") && file.ends_with(".md") {
		AssetKind::Rule
	} else if file == "AGENTS.md"
		|| (directory_named(parent, "instructions") && file.ends_with(".md"))
	{
		AssetKind::Context
	} else if matches!(file, "mcp.json" | ".mcp.json") || directory_named(parent, "mcp") {
		AssetKind::Mcp
	} else if matches!(file, "hooks.json" | ".hooks.json") || directory_named(parent, "hooks") {
		AssetKind::Hook
	} else if matches!(file, "lsp.json" | ".lsp.json") || directory_named(parent, "lsp") {
		AssetKind::Lsp
	} else if matches!(
		file,
		"dap.json" | ".dap.json" | "dap.yaml" | ".dap.yaml" | "dap.yml" | ".dap.yml"
	) || directory_named(parent, "dap")
	{
		AssetKind::Dap
	} else if directory_named(parent, "tools")
		&& matches!(
			path.rsplit_once('.').map(|(_, extension)| extension),
			Some("js" | "mjs" | "cjs" | "ts")
		) {
		AssetKind::Tool
	} else if directory_named(parent, "extensions")
		&& matches!(
			path.rsplit_once('.').map(|(_, extension)| extension),
			Some("js" | "mjs" | "cjs" | "ts")
		) {
		AssetKind::JavaScript
	} else if (directory_named(parent, "prompts") || directory_named(parent, "templates"))
		&& file.ends_with(".md")
	{
		AssetKind::Template
	} else if directory_named(parent, "commands") && file.ends_with(".md") {
		AssetKind::Command
	} else {
		return None;
	};
	accepted.contains(&kind).then_some(kind)
}

fn directory_named(path: &str, name: &str) -> bool {
	path.rsplit('/').next() == Some(name)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn plugin_resource_families_remain_distinct() {
		let accepted = [
			AssetKind::Agent,
			AssetKind::Rule,
			AssetKind::Hook,
			AssetKind::Tool,
			AssetKind::Lsp,
			AssetKind::Dap,
			AssetKind::JavaScript,
		];
		assert_eq!(classify("agents/reviewer.md", &accepted), Some(AssetKind::Agent));
		assert_eq!(classify("rules/security.md", &accepted), Some(AssetKind::Rule));
		assert_eq!(classify("hooks/pre-tool.js", &accepted), Some(AssetKind::Hook));
		assert_eq!(classify("tools/custom.ts", &accepted), Some(AssetKind::Tool));
		assert_eq!(classify(".lsp.json", &accepted), Some(AssetKind::Lsp));
		assert_eq!(classify(".dap.yaml", &accepted), Some(AssetKind::Dap));
		assert_eq!(classify("extensions/provider.js", &accepted), Some(AssetKind::JavaScript));
	}
}
