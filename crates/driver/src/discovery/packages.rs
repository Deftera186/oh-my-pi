//! Installed, linked, and explicitly scoped native OMP package roots.

use std::{collections::BTreeSet, fs, path::PathBuf};

use omp_core::Str;
use omp_ext::lock::InstalledRecord;

use super::manifest::{CapabilityDeclaration, CapabilityKind};
use crate::discovery::cache::DiscoveryCache;

/// How explicit `--extension` roots combine with installed packages.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ExtensionRootMode {
	/// Merge explicit roots ahead of installed roots.
	#[default]
	Merge,
	/// Discover only explicitly supplied roots.
	ExplicitOnly,
}

/// Scope from which a native package root was contributed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionRootScope {
	/// Installed native package selection.
	Installed,
	/// Command-line `--extension` contribution.
	Cli,
	/// SDK-scoped contribution for an embedded session.
	Sdk,
}

/// One explicit static package root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionRoot {
	/// Stable extension identity.
	pub id:    Str,
	/// Package directory.
	pub path:  PathBuf,
	/// Contribution scope.
	pub scope: ExtensionRootScope,
}

/// Package root discovery result.
#[derive(Clone, Debug, Default)]
pub struct PackageDiscovery {
	/// Declaration roots for sibling capability loaders.
	pub declarations: Vec<CapabilityDeclaration>,
	/// Package roots in effective precedence order.
	pub roots:        Vec<ExtensionRoot>,
	/// Rejected or unreadable package diagnostics.
	pub warnings:     Vec<Str>,
}

const CAPABILITY_SIBLINGS: &[(&str, CapabilityKind)] = &[
	("skills", CapabilityKind::Skills),
	("rules", CapabilityKind::Rules),
	("hooks", CapabilityKind::Hooks),
	("tools", CapabilityKind::Tools),
	("commands", CapabilityKind::SlashCommands),
	("prompts", CapabilityKind::Prompts),
	("instructions", CapabilityKind::Instructions),
	("agents", CapabilityKind::Agents),
	("settings", CapabilityKind::Settings),
];

/// Enumerates explicit and enabled installed/link package roots, then lowers
/// existing sibling capability paths. Canonical roots and declaration paths
/// are deduplicated so link aliases never execute or render twice.
pub fn discover(
	installed: &InstalledRecord,
	explicit: &[ExtensionRoot],
	mode: ExtensionRootMode,
) -> PackageDiscovery {
	let mut output = PackageDiscovery::default();
	let mut roots = explicit.to_vec();
	if mode == ExtensionRootMode::Merge {
		for extension in installed
			.extensions
			.iter()
			.filter(|extension| extension.enabled)
		{
			let Some(path) = source_path(&extension.source) else {
				continue;
			};
			roots.push(ExtensionRoot {
				id: extension.id.clone(),
				path,
				scope: ExtensionRootScope::Installed,
			});
		}
	}
	let mut canonical_roots = BTreeSet::new();
	let mut canonical_declarations = BTreeSet::new();
	for root in roots {
		let canonical = match fs::canonicalize(&root.path) {
			Ok(path) if path.is_dir() => path,
			_ => {
				output
					.warnings
					.push(Str::from(format!("extension root is unavailable: {}", root.path.display())));
				continue;
			},
		};
		if !canonical_roots.insert(canonical.clone()) {
			continue;
		}
		let priority = match root.scope {
			ExtensionRootScope::Cli | ExtensionRootScope::Sdk => 400,
			ExtensionRootScope::Installed => 300,
		};
		output
			.roots
			.push(ExtensionRoot { path: canonical.clone(), ..root.clone() });
		for (sibling, kind) in CAPABILITY_SIBLINGS {
			let path = canonical.join(sibling);
			if !path.exists() {
				continue;
			}
			let declaration_path = fs::canonicalize(&path).unwrap_or(path);
			if !declaration_path.starts_with(&canonical)
				|| !canonical_declarations.insert((*kind, declaration_path.clone()))
			{
				continue;
			}
			output.declarations.push(CapabilityDeclaration {
				id: Str::from(format!("{}:{sibling}", root.id)),
				kind: *kind,
				root: declaration_path,
				priority,
				extension_provenance: None,
			});
		}
		for filename in ["mcp.json", ".mcp.json"] {
			let path = canonical.join(filename);
			if !path.is_file() {
				continue;
			}
			let declaration_path = fs::canonicalize(&path).unwrap_or(path);
			if canonical_declarations.insert((CapabilityKind::Mcps, declaration_path.clone())) {
				output.declarations.push(CapabilityDeclaration {
					id: Str::from(format!("{}:{filename}", root.id)),
					kind: CapabilityKind::Mcps,
					root: declaration_path,
					priority,
					extension_provenance: None,
				});
			}
		}
	}
	output.declarations.sort_by(|left, right| {
		right
			.priority
			.cmp(&left.priority)
			.then_with(|| left.id.cmp(&right.id))
			.then_with(|| left.root.cmp(&right.root))
	});
	output
}

fn source_path(source: &toml::Value) -> Option<PathBuf> {
	if let Some(path) = source.as_str() {
		return Some(PathBuf::from(path));
	}
	["link", "path", "root"].into_iter().find_map(|key| {
		source
			.get(key)
			.and_then(toml::Value::as_str)
			.map(PathBuf::from)
	})
}

/// Invalidates only package-owned parsed declarations after a successful
/// lock/install transaction. Failed upgrade and rollback attempts preserve the
/// prior runnable discovery generation.
pub fn invalidate_after_transaction(
	cache: &DiscoveryCache,
	committed: Result<&[Str], &omp_ext::ExtensionError>,
) -> usize {
	let Ok(package_ids) = committed else {
		return 0;
	};
	package_ids
		.iter()
		.map(|id| cache.invalidate_installed_package(id))
		.sum()
}

/// Returns package roots suitable for scoped SDK composition without consulting
/// process-global state.
pub fn sdk_roots(paths: impl IntoIterator<Item = PathBuf>) -> Vec<ExtensionRoot> {
	paths
		.into_iter()
		.enumerate()
		.map(|(index, path)| ExtensionRoot {
			id: Str::from(format!("sdk-{index}")),
			path,
			scope: ExtensionRootScope::Sdk,
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use omp_ext::TrustTier;

	use super::*;

	#[test]
	fn explicit_only_and_realpath_dedup_are_enforced() {
		let tree = tempfile::tempdir().unwrap();
		let package = tree.path().join("package");
		fs::create_dir_all(package.join("skills")).unwrap();
		let installed = InstalledRecord {
			version:    2,
			extensions: vec![omp_ext::lock::InstalledExtension {
				id:       Str::from("installed"),
				features: Vec::new(),
				source:   toml::Value::Table(toml::Table::from_iter([(
					"link".to_owned(),
					toml::Value::String(package.to_string_lossy().into_owned()),
				)])),
				tier:     TrustTier::Sandboxed,
				enabled:  true,
			}],
		};
		let explicit = [ExtensionRoot {
			id:    Str::from("cli"),
			path:  package.clone(),
			scope: ExtensionRootScope::Cli,
		}];
		let only = discover(&installed, &explicit, ExtensionRootMode::ExplicitOnly);
		assert_eq!(only.roots.len(), 1);
		assert_eq!(only.declarations.len(), 1);
		let merged = discover(&installed, &explicit, ExtensionRootMode::Merge);
		assert_eq!(merged.roots.len(), 1);
	}
}
