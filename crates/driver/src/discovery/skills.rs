//! Deterministic, data-only skill discovery and admission.

use std::{
	collections::{BTreeMap, BTreeSet},
	fs,
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_core::Str;
use omp_settings::{
	DomainRegistration, FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError,
};
use omp_walker::{FollowLinks, WalkRequest};
use serde::{Deserialize, Serialize};

use super::{
	containment::contained_existing,
	manifest::{
		CapabilityPayload, DiscoveredCapability, SkillFrontmatter, SkillPayload, SourceProvenance,
		SourceScope,
	},
};

/// Provenance for a skill discovery root.
#[derive(Clone, Debug)]
pub enum SkillSourceKind {
	/// Native, user, foreign-adapter, or managed content.
	Native,
	/// Signed static content owned by one admitted extension package.
	Extension {
		/// Stable extension identity.
		extension_id:  Str,
		/// Canonical package containment root.
		package_root:  PathBuf,
		/// Signed distribution-relative path or glob.
		declared_path: Str,
	},
	/// Session-bound paths returned by an extension discovery hook.
	ExtensionDiscovery {
		/// Stable extension identity.
		extension_id: Str,
		/// Exact canonical path admitted from the hook result.
		path:         PathBuf,
	},
}

/// One skill source scanned in caller-defined precedence order.
#[derive(Clone, Debug)]
pub struct SkillSource {
	/// Stable source/provider identity used by settings.
	pub id:                  Str,
	/// Direct `SKILL.md` directory or parent of named skill directories.
	pub root:                PathBuf,
	/// Source scope.
	pub scope:               SourceScope,
	/// Whether the root itself may be a skill.
	pub include_root:        bool,
	/// Whether a description is mandatory for this source.
	pub require_description: bool,
	/// Optional package containment root.
	pub contain_root:        Option<PathBuf>,
	/// Read-only foreign/package content marker.
	pub read_only:           bool,
	/// Native or extension-package provenance.
	pub kind:                SkillSourceKind,
}

/// Settings projection applied before skill names claim precedence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SkillDiscoverySettings {
	/// Master enablement.
	pub enabled:             bool,
	/// Explicitly disabled source IDs.
	pub disabled_sources:    BTreeSet<Str>,
	/// Inclusion globs over skill names; empty includes every name.
	pub include:             Vec<Str>,
	/// Exclusion globs over skill names.
	pub ignore:              Vec<Str>,
	/// Explicit disabled skill names.
	pub disabled_skills:     BTreeSet<Str>,
	/// Fallback gate for repo-surface third-party families without a dedicated
	/// source toggle.
	pub third_party_enabled: bool,
	/// Additional authored skill directories.
	pub custom_directories:  Vec<PathBuf>,
}

impl Default for SkillDiscoverySettings {
	fn default() -> Self {
		Self {
			enabled:             true,
			disabled_sources:    BTreeSet::new(),
			include:             Vec::new(),
			ignore:              Vec::new(),
			disabled_skills:     BTreeSet::new(),
			third_party_enabled: true,
			custom_directories:  Vec::new(),
		}
	}
}

const SKILL_SCOPES: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

impl SettingsDomain for SkillDiscoverySettings {
	const DOMAIN: &'static str = "skills";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "skills.enabled",
			label:       "Skills",
			description: "Enable skill discovery and invocation.",
			kind:        SettingKind::Boolean,
			scopes:      SKILL_SCOPES,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.disabled_sources",
			label:       "Disabled skill sources",
			description: "Source IDs excluded before skill names claim precedence.",
			kind:        SettingKind::Array,
			scopes:      SKILL_SCOPES,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.include",
			label:       "Included skills",
			description: "Optional skill-name inclusion globs.",
			kind:        SettingKind::Array,
			scopes:      SKILL_SCOPES,
			order:       30,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.ignore",
			label:       "Ignored skills",
			description: "Skill-name exclusion globs.",
			kind:        SettingKind::Array,
			scopes:      SKILL_SCOPES,
			order:       40,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.disabled_skills",
			label:       "Disabled skills",
			description: "Explicit skill names disabled before collision handling.",
			kind:        SettingKind::Array,
			scopes:      SKILL_SCOPES,
			order:       50,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.third_party_enabled",
			label:       "Third-party content skills",
			description: "Enable repo-surface third-party skill families without a dedicated source \
			              toggle.",
			kind:        SettingKind::Boolean,
			scopes:      SKILL_SCOPES,
			order:       60,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.custom_directories",
			label:       "Custom skill directories",
			description: "Additional native authored skill roots.",
			kind:        SettingKind::Array,
			scopes:      SKILL_SCOPES,
			order:       70,
			options:     None,
			condition:   None,
			secret:      false,
		},
	];

	fn validate(&self) -> Result<(), ValidationError> {
		let valid = self.disabled_sources.iter().all(|value| !value.is_empty())
			&& self.disabled_skills.iter().all(|value| !value.is_empty())
			&& self
				.custom_directories
				.iter()
				.all(|path| !path.as_os_str().is_empty());
		if valid {
			Ok(())
		} else {
			Err(ValidationError::DomainInvariant { domain: Self::DOMAIN })
		}
	}
}

omp_settings::inventory::submit! {
	DomainRegistration::of::<SkillDiscoverySettings>()
}

/// Non-fatal skill discovery diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillWarning {
	/// Source which was skipped or suppressed.
	pub path:    PathBuf,
	/// Stable diagnostic text.
	pub message: Str,
}

/// Stable skill provider output.
#[derive(Clone, Debug, Default)]
pub struct SkillDiscovery {
	/// Winning declarations in case-insensitive name/path order.
	pub declarations: Vec<DiscoveredCapability>,
	/// Non-fatal malformed, duplicate, and collision diagnostics.
	pub warnings:     Vec<SkillWarning>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillHeader {
	name:                     Option<String>,
	description:              Option<String>,
	license:                  Option<String>,
	compatibility:            Option<String>,
	#[serde(default)]
	metadata:                 BTreeMap<Str, serde_json::Value>,
	#[serde(default, rename = "allowed-tools", alias = "allowedTools")]
	allowed_tools:            ToolList,
	#[serde(default)]
	globs:                    StringList,
	#[serde(default)]
	always_apply:             bool,
	#[serde(default)]
	enabled:                  Option<bool>,
	#[serde(default, alias = "hide")]
	hidden:                   bool,
	#[serde(default, alias = "disable-model-invocation")]
	disable_model_invocation: bool,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum ToolList {
	One(String),
	Many(Vec<String>),
	#[default]
	None,
}

impl ToolList {
	fn values(self) -> Vec<Str> {
		match self {
			Self::One(value) => value.split_whitespace().map(Str::from).collect(),
			Self::Many(values) => values
				.into_iter()
				.map(|value| value.trim().to_owned())
				.filter(|value| !value.is_empty())
				.map(Str::from)
				.collect(),
			Self::None => Vec::new(),
		}
	}
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum StringList {
	One(String),
	Many(Vec<String>),
	#[default]
	None,
}

impl StringList {
	fn values(self) -> Vec<Str> {
		match self {
			Self::One(value) => value
				.split(',')
				.map(str::trim)
				.filter(|s| !s.is_empty())
				.map(Str::from)
				.collect(),
			Self::Many(values) => values
				.into_iter()
				.map(|s| s.trim().to_owned())
				.filter(|s| !s.is_empty())
				.map(Str::from)
				.collect(),
			Self::None => Vec::new(),
		}
	}
}

/// Scans direct and nested `SKILL.md` declarations from ordered sources,
/// follows only contained symlinks, applies source/name gates before claiming
/// names, and realpath-deduplicates declarations.
#[tracing::instrument(
	level = "debug",
	skip_all,
	name = "skill_discovery",
	fields(source_count = sources.len(), enabled = settings.enabled)
)]
pub fn discover(sources: &[SkillSource], settings: &SkillDiscoverySettings) -> SkillDiscovery {
	if !settings.enabled {
		return SkillDiscovery::default();
	}
	let mut output = SkillDiscovery::default();
	let mut names = BTreeMap::<Str, PathBuf>::new();
	let mut realpaths = BTreeSet::new();
	let mut configured_sources = sources.to_vec();
	configured_sources.extend(
		settings
			.custom_directories
			.iter()
			.cloned()
			.map(|root| SkillSource {
				id: Str::from("custom"),
				root,
				scope: SourceScope::User,
				include_root: true,
				require_description: true,
				contain_root: None,
				read_only: false,
				kind: SkillSourceKind::Native,
			}),
	);
	for source in &configured_sources {
		if settings.disabled_sources.contains(&source.id)
			|| (!settings.third_party_enabled && source.id.starts_with("foreign-"))
		{
			continue;
		}
		let managed_source = source.id.as_str() == omp_envd::managed_skills_domain::PROVIDER_ID;
		for path in skill_files(source, &mut output.warnings) {
			if managed_source && !managed_path_safe(&path) {
				output.warnings.push(SkillWarning {
					path,
					message: Str::from("managed skill path is linked, oversized, or not a regular file"),
				});
				continue;
			}
			let canonical =
				match contained_existing(source.contain_root.as_deref().unwrap_or(&source.root), &path)
				{
					Ok(path) => path,
					Err(_) => {
						output.warnings.push(SkillWarning {
							path,
							message: Str::from("skill path escapes its source root"),
						});
						continue;
					},
				};
			if !realpaths.insert(canonical.clone()) {
				continue;
			}
			if !matches!(&source.kind, SkillSourceKind::Native)
				&& fs::metadata(&canonical).is_ok_and(|metadata| metadata.len() > 64_000)
			{
				output.warnings.push(SkillWarning {
					path:    canonical,
					message: Str::from("extension skill exceeds the 64,000-byte UTF-8 limit"),
				});
				continue;
			}
			let (header, content) = match parse_skill(&canonical) {
				Ok(value) => value,
				Err(_) => {
					output.warnings.push(SkillWarning {
						path:    canonical,
						message: Str::from("failed to parse SKILL.md frontmatter"),
					});
					continue;
				},
			};
			if header.enabled == Some(false) {
				continue;
			}
			let fallback = canonical
				.parent()
				.and_then(Path::file_name)
				.and_then(|name| name.to_str())
				.unwrap_or("skill");
			let name = header
				.name
				.as_deref()
				.map(str::trim)
				.filter(|name| !name.is_empty())
				.unwrap_or(fallback);
			if !safe_skill_name(name) {
				output.warnings.push(SkillWarning {
					path:    canonical,
					message: Str::from("skill name is not a safe directory-style identifier"),
				});
				continue;
			}
			let managed = managed_source;
			if managed && !omp_envd::managed_skills_domain::is_valid_name(name) {
				output.warnings.push(SkillWarning {
					path:    canonical,
					message: Str::from("managed skill name is not exact kebab-case"),
				});
				continue;
			}
			let managed_description = managed.then(|| {
				omp_envd::managed_skills_domain::sanitize_description(
					header.description.as_deref().unwrap_or_default(),
				)
			});
			if source.require_description
				&& header
					.description
					.as_deref()
					.map(str::trim)
					.filter(|v| !v.is_empty())
					.is_none()
			{
				continue;
			}
			if managed_description.as_ref().is_some_and(Str::is_empty) {
				continue;
			}
			if settings.disabled_skills.contains(name)
				|| settings
					.ignore
					.iter()
					.any(|pattern| glob_matches(pattern.as_str(), name))
				|| (!settings.include.is_empty()
					&& !settings
						.include
						.iter()
						.any(|pattern| glob_matches(pattern.as_str(), name)))
			{
				continue;
			}
			let key = Str::from(name);
			if let Some(winner) = names.get(&key) {
				output.warnings.push(SkillWarning {
					path:    canonical,
					message: Str::from(format!("skill name is already claimed by {}", winner.display())),
				});
				continue;
			}
			names.insert(key.clone(), canonical.clone());
			let mut payload = SkillPayload {
				name:         key.clone(),
				path:         canonical.clone(),
				content:      Str::from(content),
				frontmatter:  Arc::new(SkillFrontmatter {
					description:              header
						.description
						.map(|value| Str::from(value.trim().to_owned())),
					license:                  header
						.license
						.map(|value| Str::from(value.trim().to_owned())),
					compatibility:            header
						.compatibility
						.map(|value| Str::from(value.trim().to_owned())),
					metadata:                 header.metadata,
					allowed_tools:            header.allowed_tools.values(),
					globs:                    header.globs.values(),
					always_apply:             header.always_apply,
					hidden:                   header.hidden,
					disable_model_invocation: header.disable_model_invocation,
				}),
				contain_root: source.contain_root.clone(),
			};
			if let Some(description) = managed_description {
				Arc::make_mut(&mut payload.frontmatter).description = Some(description);
			}
			let mut provenance = SourceProvenance::native(source.id.clone(), canonical, source.scope);
			provenance.read_only = source.read_only;
			if let SkillSourceKind::Extension { extension_id, .. }
			| SkillSourceKind::ExtensionDiscovery { extension_id, .. } = &source.kind
			{
				provenance.installed_package_id = Some(extension_id.clone());
			}
			output.declarations.push(DiscoveredCapability::keyed(
				key,
				CapabilityPayload::Skills(payload),
				provenance,
			));
		}
	}
	output.declarations.sort_by(|left, right| {
		let left = match &left.payload {
			CapabilityPayload::Skills(skill) => skill,
			_ => unreachable!(),
		};
		let right = match &right.payload {
			CapabilityPayload::Skills(skill) => skill,
			_ => unreachable!(),
		};
		left
			.name
			.as_str()
			.to_ascii_lowercase()
			.cmp(&right.name.as_str().to_ascii_lowercase())
			.then_with(|| left.name.cmp(&right.name))
			.then_with(|| left.path.cmp(&right.path))
	});
	output
}

/// Lowers signed static `skills` rows to contained, read-only discovery roots.
pub fn extension_sources(
	extension_id: &Str,
	package_root: &Path,
	rows: &[omp_ext::config::StaticDeclaration],
) -> Vec<SkillSource> {
	rows
		.iter()
		.filter(|row| row.kind == "skills")
		.filter_map(|row| {
			let declared_path = row.path.as_deref()?;
			let relative = Path::new(declared_path);
			if declared_path.contains('\\')
				|| relative.is_absolute()
				|| relative.components().any(|component| {
					matches!(
						component,
						std::path::Component::ParentDir
							| std::path::Component::RootDir
							| std::path::Component::Prefix(_)
					)
				}) {
				return None;
			}
			let wildcard = declared_path
				.find(|character| matches!(character, '*' | '?'))
				.unwrap_or(declared_path.len());
			let prefix = Path::new(&declared_path[..wildcard]);
			let exact_file = wildcard == declared_path.len()
				&& prefix.file_name().is_some_and(|name| name == "SKILL.md");
			let root = if exact_file || prefix.extension().is_some() {
				prefix.parent().unwrap_or_else(|| Path::new(""))
			} else {
				prefix
			};
			let contain_root = row
				.metadata
				.get("contain_root")
				.and_then(serde_json::Value::as_str)
				.map(Path::new)
				.filter(|path| {
					!path.is_absolute()
						&& !path.components().any(|component| {
							matches!(
								component,
								std::path::Component::ParentDir
									| std::path::Component::RootDir
									| std::path::Component::Prefix(_)
							)
						})
				})
				.map_or_else(|| package_root.to_path_buf(), |path| package_root.join(path));
			Some(SkillSource {
				id:                  extension_id.clone(),
				root:                package_root.join(root),
				scope:               SourceScope::Package,
				include_root:        exact_file,
				require_description: true,
				contain_root:        Some(contain_root),
				read_only:           true,
				kind:                SkillSourceKind::Extension {
					extension_id:  extension_id.clone(),
					package_root:  package_root.to_path_buf(),
					declared_path: Str::new(declared_path),
				},
			})
		})
		.collect()
}

/// Converts already-admitted hook paths into driver discovery sources.
pub fn contributed_sources(
	extension_id: &Str,
	paths: impl IntoIterator<Item = (PathBuf, PathBuf)>,
) -> Vec<SkillSource> {
	paths
		.into_iter()
		.filter_map(|(path, contain_root)| {
			let root = path.parent()?.to_path_buf();
			Some(SkillSource {
				id: extension_id.clone(),
				root,
				scope: SourceScope::Package,
				include_root: true,
				require_description: true,
				contain_root: Some(contain_root),
				read_only: true,
				kind: SkillSourceKind::ExtensionDiscovery { extension_id: extension_id.clone(), path },
			})
		})
		.collect()
}

fn skill_files(source: &SkillSource, warnings: &mut Vec<SkillWarning>) -> Vec<PathBuf> {
	let mut files = Vec::new();
	if source.include_root && source.root.join("SKILL.md").is_file() {
		files.push(source.root.join("SKILL.md"));
	}
	let outcome = WalkRequest::new(&source.root)
		.hidden(false)
		.gitignore(true)
		.skip_git(true)
		.follow_links(FollowLinks::Always)
		.depth(2, 2)
		.limit(1024)
		.collect_files();
	match outcome {
		Ok(entries) => files.extend(
			entries
				.into_iter()
				.map(|entry| entry.absolute_path(&source.root))
				.filter(|path| path.file_name().is_some_and(|name| name == "SKILL.md")),
		),
		Err(_) if source.root.exists() => warnings.push(SkillWarning {
			path:    source.root.clone(),
			message: Str::from("failed to read skills directory"),
		}),
		Err(_) => {},
	}
	if let SkillSourceKind::Extension { package_root, declared_path, .. } = &source.kind {
		files.retain(|path| {
			path
				.strip_prefix(package_root)
				.ok()
				.and_then(Path::to_str)
				.is_some_and(|relative| glob_matches(declared_path, relative))
		});
	}
	if let SkillSourceKind::ExtensionDiscovery { path, .. } = &source.kind {
		files.retain(|candidate| candidate == path);
	}
	files.sort();
	files
}

fn parse_skill(path: &Path) -> Result<(SkillHeader, String), serde_yaml::Error> {
	let source = fs::read_to_string(path).unwrap_or_default();
	let Some(rest) = source.strip_prefix("---\n") else {
		return Ok((SkillHeader::default(), source));
	};
	let Some((header, body)) = rest.split_once("\n---\n") else {
		return Ok((SkillHeader::default(), source));
	};
	Ok((serde_yaml::from_str(header)?, body.trim().to_owned()))
}

/// Returns whether a skill name is a safe, URL-addressable identifier.
pub fn safe_skill_name(name: &str) -> bool {
	!name.is_empty()
		&& name != "."
		&& name != ".."
		&& name
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn managed_path_safe(path: &Path) -> bool {
	let Ok(file) = fs::symlink_metadata(path) else {
		return false;
	};
	let Some(directory) = path.parent() else {
		return false;
	};
	let Ok(directory) = fs::symlink_metadata(directory) else {
		return false;
	};
	!file.file_type().is_symlink()
		&& file.is_file()
		&& file.len() <= omp_envd::managed_skills_domain::MAX_SKILL_BYTES as u64
		&& managed_link_count(&file) == 1
		&& !directory.file_type().is_symlink()
		&& directory.is_dir()
}

#[cfg(unix)]
fn managed_link_count(metadata: &fs::Metadata) -> u64 {
	use std::os::unix::fs::MetadataExt as _;
	metadata.nlink()
}

#[cfg(windows)]
fn managed_link_count(metadata: &fs::Metadata) -> u64 {
	use std::os::windows::fs::MetadataExt as _;
	u64::from(metadata.number_of_links())
}

/// Small allocation-free wildcard matcher used for configuration globs.
/// `*` spans any bytes and `?` spans one byte; repeated stars naturally cover
/// `**` without introducing a second pattern dialect.
pub fn glob_matches(pattern: &str, candidate: &str) -> bool {
	let pattern = pattern.as_bytes();
	let candidate = candidate.as_bytes();
	let (mut p, mut c, mut star, mut retry) = (0, 0, None, 0);
	while c < candidate.len() {
		if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == candidate[c]) {
			p += 1;
			c += 1;
		} else if p < pattern.len() && pattern[p] == b'*' {
			star = Some(p);
			p += 1;
			retry = c;
		} else if let Some(index) = star {
			p = index + 1;
			retry += 1;
			c = retry;
		} else {
			return false;
		}
	}
	while p < pattern.len() && pattern[p] == b'*' {
		p += 1;
	}
	p == pattern.len()
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	#[test]
	fn pi_frontmatter_spelling_and_compatibility_fields_are_retained() {
		let tree = tempfile::tempdir().unwrap();
		let root = tree.path().join("frontmatter");
		let skill = root.join("compat/SKILL.md");
		fs::create_dir_all(skill.parent().unwrap()).unwrap();
		fs::write(
			&skill,
			"---\nname: compat\ndescription: compatible\nlicense: Apache-2.0\ncompatibility: \
			 Requires git\nmetadata:\n  author: pi\n  revision: 2\nallowed-tools: Read Grep \
			 Bash\ndisable-model-invocation: true\n---\nbody",
		)
		.unwrap();
		let result = discover(
			&[SkillSource {
				id: Str::new_static("test"),
				root,
				scope: SourceScope::Project,
				include_root: false,
				require_description: true,
				contain_root: None,
				read_only: false,
				kind: SkillSourceKind::Native,
			}],
			&SkillDiscoverySettings::default(),
		);
		let CapabilityPayload::Skills(skill) = &result.declarations[0].payload else {
			panic!("skill payload")
		};
		assert!(skill.frontmatter.disable_model_invocation);
		assert_eq!(skill.frontmatter.license.as_deref(), Some("Apache-2.0"));
		assert_eq!(skill.frontmatter.compatibility.as_deref(), Some("Requires git"));
		assert_eq!(skill.frontmatter.allowed_tools, [
			Str::new_static("Read"),
			Str::new_static("Grep"),
			Str::new_static("Bash")
		]);
		assert_eq!(skill.frontmatter.metadata["author"], "pi");
		assert_eq!(skill.frontmatter.metadata["revision"], 2);
	}

	#[test]
	fn scans_nested_skills_and_applies_gates_before_collision() {
		let tree = tempfile::tempdir().unwrap();
		let high = tree.path().join("high");
		let low = tree.path().join("low");
		fs::create_dir_all(high.join("alpha")).unwrap();
		fs::create_dir_all(low.join("alpha")).unwrap();
		fs::write(
			high.join("alpha/SKILL.md"),
			"---\nname: alpha\ndescription: hidden\nenabled: false\n---\nhigh",
		)
		.unwrap();
		fs::write(low.join("alpha/SKILL.md"), "---\ndescription: usable\n---\nlow").unwrap();
		let sources = [
			SkillSource {
				id:                  Str::from("high"),
				root:                high,
				scope:               SourceScope::Project,
				include_root:        false,
				require_description: true,
				contain_root:        None,
				read_only:           false,
				kind:                SkillSourceKind::Native,
			},
			SkillSource {
				id:                  Str::from("low"),
				root:                low,
				scope:               SourceScope::User,
				include_root:        false,
				require_description: true,
				contain_root:        None,
				read_only:           false,
				kind:                SkillSourceKind::Native,
			},
		];
		let result = discover(&sources, &SkillDiscoverySettings::default());
		assert_eq!(result.declarations.len(), 1);
		let CapabilityPayload::Skills(skill) = &result.declarations[0].payload else {
			panic!()
		};
		assert_eq!(skill.content, "low");
	}

	#[test]
	fn extension_contributors_use_existing_first_source_precedence() {
		let tree = tempfile::tempdir().expect("tree");
		let first_root = tree.path().join("first");
		let second_root = tree.path().join("second");
		let first = first_root.join("review/SKILL.md");
		let second = second_root.join("review/SKILL.md");
		fs::create_dir_all(first.parent().expect("first parent")).expect("first directory");
		fs::create_dir_all(second.parent().expect("second parent")).expect("second directory");
		fs::write(&first, "---\ndescription: first\n---\nfirst").expect("first skill");
		fs::write(&second, "---\ndescription: second\n---\nsecond").expect("second skill");
		let sources = [
			contributed_sources(&Str::from("publisher.first"), [(first.clone(), first_root)]),
			contributed_sources(&Str::from("publisher.second"), [(second, second_root)]),
		]
		.concat();
		let result = discover(&sources, &SkillDiscoverySettings::default());
		assert_eq!(result.declarations.len(), 1);
		let CapabilityPayload::Skills(skill) = &result.declarations[0].payload else {
			panic!("skill payload")
		};
		assert_eq!(skill.content, "first");
		assert_eq!(
			result.declarations[0]
				.source
				.installed_package_id
				.as_deref(),
			Some("publisher.first")
		);
		let frozen = crate::skills::SkillSnapshot::from_declarations(&result.declarations);
		fs::write(&first, "---\ndescription: first\n---\nupdated").expect("updated skill");
		assert_eq!(frozen.resolve_body("review"), Some("first"));
		let reloaded = discover(&sources, &SkillDiscoverySettings::default());
		let reloaded = crate::skills::SkillSnapshot::from_declarations(&reloaded.declarations);
		assert_eq!(reloaded.resolve_body("review"), Some("updated"));
	}

	#[test]
	fn wildcard_matching_is_deterministic() {
		assert!(glob_matches("rust-*", "rust-review"));
		assert!(glob_matches("*-review", "rust-review"));
		assert!(!glob_matches("go-*", "rust-review"));
	}
}
