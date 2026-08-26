//! Filesystem capability discovery and runtime model-discovery normalization.

pub mod active_repo;
pub mod at_path;
pub mod cache;
pub mod containment;
pub mod context;
pub mod custom_tools;
pub mod foreign;
pub mod managed_skills;
pub mod manifest;
pub mod mcp;
pub mod mcp_ssh;
pub mod models;
pub mod native;
pub mod packages;
pub mod project;
pub mod prompts;
pub mod registry;
pub mod roles;
pub mod rules;
pub mod runtime;
pub mod settings;
pub mod skills;
pub mod slash_commands;

use std::{
	collections::BTreeMap,
	env, iter,
	path::{Path, PathBuf},
	sync::Arc,
};

use futures::future::join_all;
use omp_catalog::{
	ContextStrategy, Pricing, RouteId, ThinkingPolicyId, WirePolicyId,
	discover::{DiscoveredModel, DiscoveryDefaults, DiscoveryNormalizer, NormalizedDiscovery},
};
use omp_core::{ArtifactDigest, Hash32, Provenance, Str};
use omp_envd::{
	exthost::{
		DeclarationSet, ExtensionManifest, HookDeclarationKey, ServiceManifest, ToolDeclarationKey,
	},
	worker::{ExtHostSpec, HostKey},
};

use self::{
	foreign::ForeignContentSettings,
	manifest::{
		CapabilityKind, CapabilityPayload, CapabilityRecord, DiscoveredCapability, SourceScope,
	},
	native::NativeDiscoveryOptions,
	registry::{CAPABILITY_KINDS, CapabilityResult, DiscoveryRegistry, LoadContext, LoadOptions},
	skills::SkillDiscoverySettings,
};
use crate::{
	rulebook::{RuleSnapshot, RulebookSettings},
	skills::SkillSnapshot,
};

/// Immutable settings authority consumed by one static discovery pass.
#[derive(Clone, Debug, Default)]
pub struct PromptDiscoverySettings {
	/// Model/provider admission and path-scoped provider exclusions.
	pub model:   omp_catalog::settings::ModelSettings,
	/// Skill source, name, and custom-directory policy.
	pub skills:  SkillDiscoverySettings,
	/// Read-only foreign content family policy.
	pub foreign: ForeignContentSettings,
	/// Built-in and blocked-rule policy.
	pub rules:   RulebookSettings,
	/// Invocation-local native root and installed-record admission policy.
	pub native:  NativeDiscoveryOptions,
}
/// One command contributed by native content discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandContribution {
	/// Primary spelling without `/`.
	pub name:        Str,
	/// Alternate spellings.
	pub aliases:     Vec<Str>,
	/// One-line description.
	pub description: Str,
	/// Inline argument hint.
	pub hint:        Option<Str>,
	/// Human-readable discovery source label.
	pub origin:      Str,
	/// Optional prompt template dispatched when this command is submitted.
	pub template:    Option<Str>,
}

const INIT_WORKFLOW_TEMPLATE: &str = r#"Use parallel `task` research agents for independent slices of the repository: core source, tests, configuration/build, and scripts/documentation. Synthesize their findings into one AGENTS.md.

The document MUST:
- be titled "Repository Guidelines" and use Markdown headings;
- concisely explain project purpose, architecture and data flow, key directories, development commands, code conventions, important files, runtime/tooling preferences, and testing/QA;
- include useful commands, paths, naming patterns, and architecture-specific guidance;
- omit facts that are obvious from the directory tree.

After analysis, write AGENTS.md to the project root."#;

fn embedded_workflow_commands() -> [CommandContribution; 1] {
	[CommandContribution {
		name:        omp_core::sf!("init"),
		aliases:     Vec::new(),
		description: omp_core::sf!("Generate AGENTS.md for the current codebase"),
		hint:        None,
		origin:      omp_core::sf!("Bundled OMP workflow"),
		template:    Some(omp_core::sf!(INIT_WORKFLOW_TEMPLATE)),
	}]
}

/// Immutable active content snapshots shared by prompt, UI, and internal URL
/// composition.
#[derive(Clone, Debug)]
pub struct ActiveContentSnapshots {
	/// Active skills.
	pub skills:        Arc<SkillSnapshot>,
	/// Active declarative rules.
	pub rules:         Arc<RuleSnapshot>,
	/// Active native Markdown slash commands in discovery precedence order.
	pub commands:      Arc<[CommandContribution]>,
	/// Bounded non-fatal diagnostics emitted while loading static content.
	pub warnings:      Arc<[Str]>,
	/// Frozen declarations from the same startup discovery pass.
	pub declarations:  Arc<[DiscoveredCapability]>,
	/// Authenticated native extension workers admitted from those declarations.
	pub extensions:    Arc<[ExtHostSpec]>,
	/// Environment-bound process custom tools admitted from those declarations.
	pub process_tools: Arc<custom_tools::ProcessToolFactory>,
}

/// Static prompt inputs frozen together for interactive and headless session
/// composition.
#[derive(Clone, Debug)]
pub struct ActivePromptSnapshots {
	/// Rules, skills, commands, declarations, and warnings from one pass.
	pub content: ActiveContentSnapshots,
	/// Context-file winners collated across every granted workspace root.
	pub context: context::ContextSnapshot,
}

/// Discovers the complete static prompt surface with identical provider,
/// priority, user-scope, and native-root semantics for every session mode.
pub fn active_prompt_snapshots(
	root: &Path,
	additional_roots: &[PathBuf],
	home: &Path,
	settings: &PromptDiscoverySettings,
) -> ActivePromptSnapshots {
	let disabled_providers = settings.model.resolved_disabled_providers(root, home);
	let content =
		active_content_snapshots_with_home(root, home, disabled_providers.as_ref(), settings);
	let roots = iter::once(root)
		.chain(additional_roots.iter().map(PathBuf::as_path))
		.map(|path| context::GrantedContextRoot {
			root:  context_repository_boundary(path, home),
			start: path.to_path_buf(),
		})
		.collect::<Vec<_>>();
	let context = context::discover(&roots, &context::ContextDiscoveryOptions {
		home: Some(home.to_path_buf()),
		disabled_providers,
		..context::ContextDiscoveryOptions::default()
	});
	ActivePromptSnapshots { content, context }
}

fn context_repository_boundary(start: &Path, home: &Path) -> PathBuf {
	start
		.ancestors()
		.find(|ancestor| ancestor.join(".git").exists())
		.map(Path::to_path_buf)
		.unwrap_or_else(|| {
			if start.starts_with(home) {
				home.to_path_buf()
			} else {
				start.to_path_buf()
			}
		})
}

/// Discovers native repository/user content once and freezes the skill/rule
/// winners used by a session composition.
pub fn active_content_snapshots(root: &Path) -> ActiveContentSnapshots {
	let home = env::var_os("HOME").map_or_else(|| root.to_path_buf(), PathBuf::from);
	active_content_snapshots_with_home(root, &home, &[], &PromptDiscoverySettings::default())
}

fn active_content_snapshots_with_home(
	root: &Path,
	home: &Path,
	disabled_providers: &[Str],
	settings: &PromptDiscoverySettings,
) -> ActiveContentSnapshots {
	let mut discovered = native::discover_capabilities(root, home, 64, &NativeDiscoveryOptions {
		skill_settings: settings.skills.clone(),
		..settings.native.clone()
	});
	let foreign = foreign::discover(root, &settings.foreign, &settings.skills);
	discovered.declarations.extend(foreign.skills);
	discovered.declarations.extend(foreign.rules);
	discovered.declarations.extend(foreign.prompts);
	discovered.declarations.extend(foreign.instructions);
	discovered.warnings.extend(foreign.warnings);
	let mut managed_skill_settings = settings.skills.clone();
	managed_skill_settings.custom_directories.clear();
	let managed =
		managed_skills::discover_dead_last(&native::user_config_root(home), &managed_skill_settings);
	discovered.declarations.extend(managed.declarations);
	discovered
		.warnings
		.extend(managed.warnings.into_iter().map(|warning| warning.message));
	discovered.declarations.retain(|declaration| {
		discovery_provider_id(declaration.source.source_id.as_str()).is_none_or(|provider| {
			!disabled_providers
				.iter()
				.any(|disabled| disabled == provider)
		})
	});
	let (extensions, extension_warnings) = admit_extension_specs(&discovered.declarations);
	discovered.warnings.extend(extension_warnings);
	let admitted_extensions = extensions
		.iter()
		.map(|spec| Str::new(spec.key.extension().as_str()))
		.collect::<std::collections::BTreeSet<_>>();
	discovered.declarations.retain(|declaration| {
		!matches!(
			&declaration.payload,
			CapabilityPayload::Extensions(extension)
				if extension.worker.is_some() && !admitted_extensions.contains(&extension.name)
		)
	});
	let mut process_names = std::collections::BTreeSet::new();
	let process_tools = custom_tools::ProcessToolFactory::new(
		discovered.declarations.iter().filter_map(|declaration| {
			let CapabilityPayload::Tools(tool) = &declaration.payload else {
				return None;
			};
			process_names
				.insert(tool.name.clone())
				.then(|| tool.clone())
		}),
	);
	let mut commands = discovered
		.declarations
		.iter()
		.filter_map(|declaration| {
			let CapabilityPayload::SlashCommands(command) = &declaration.payload else {
				return None;
			};
			let origin = match declaration.source.scope {
				SourceScope::Project => "Project .omp",
				SourceScope::User => "User .omp",
				_ => "OMP command",
			};
			Some(CommandContribution {
				name:        command.name.clone(),
				aliases:     Vec::new(),
				description: command.description.clone(),
				hint:        command
					.argument_hint
					.clone()
					.or_else(|| Some(Str::new_static("[arguments]"))),
				origin:      Str::new_static(origin),
				template:    Some(command.content.clone()),
			})
		})
		.collect::<Vec<_>>();
	if !commands
		.iter()
		.any(|command| command.name.as_str().eq_ignore_ascii_case("init"))
	{
		commands.extend(embedded_workflow_commands());
	}
	ActiveContentSnapshots {
		skills:        Arc::new(SkillSnapshot::from_declarations(&discovered.declarations)),
		rules:         Arc::new(RuleSnapshot::from_declarations(
			&discovered.declarations,
			&settings.rules,
		)),
		commands:      commands.into(),
		warnings:      discovered.warnings.into(),
		declarations:  discovered.declarations.into(),
		extensions:    extensions.into(),
		process_tools: Arc::new(process_tools),
	}
}
fn admit_extension_specs(declarations: &[DiscoveredCapability]) -> (Vec<ExtHostSpec>, Vec<Str>) {
	let mut seen = std::collections::BTreeSet::new();
	let mut specs = Vec::new();
	let mut warnings = Vec::new();
	for declaration in declarations {
		let CapabilityPayload::Extensions(extension) = &declaration.payload else {
			continue;
		};
		if !seen.insert(extension.name.clone()) {
			continue;
		}
		let Some(worker) = &extension.worker else {
			continue;
		};
		let static_declarations = match extension.static_declarations() {
			Ok(declarations) => declarations,
			Err(error) => {
				warnings.push(Str::from(format!(
					"extension {} was not admitted: invalid static declarations: {error}",
					extension.name
				)));
				continue;
			},
		};
		let tools = static_declarations
			.tools
			.iter()
			.filter_map(|row| {
				let name = if row.key.is_empty() {
					&row.id
				} else {
					&row.key
				};
				if name.is_empty() {
					return None;
				}
				let family = row
					.properties
					.get("family")
					.and_then(serde_json::Value::as_str)
					.unwrap_or(extension.name.as_str());
				let rev = row
					.properties
					.get("rev")
					.and_then(serde_json::Value::as_u64)
					.and_then(|rev| u16::try_from(rev).ok())
					.unwrap_or_else(|| u16::try_from(row.api).unwrap_or(1).max(1));
				Some(ToolDeclarationKey::new(name.clone(), family, rev))
			})
			.collect::<Vec<_>>();
		let hooks = static_declarations
			.hooks
			.iter()
			.filter_map(|row| {
				let event = row
					.properties
					.get("event")
					.and_then(serde_json::Value::as_str)
					.unwrap_or(row.key.as_str());
				let phase = row
					.properties
					.get("phase")
					.and_then(serde_json::Value::as_str)
					.unwrap_or("observe")
					.parse::<omp_agent::HookPhase>()
					.ok()?;
				(!event.is_empty()).then(|| HookDeclarationKey::new(event, phase))
			})
			.collect::<Vec<_>>();
		let declarations = DeclarationSet::new(tools, hooks);
		let requested_grants = static_declarations
			.capability_grants
			.values()
			.flat_map(|value| {
				value
					.as_array()
					.into_iter()
					.flatten()
					.filter_map(serde_json::Value::as_str)
					.map(Str::new)
			})
			.collect::<Vec<_>>();
		let data_grants = omp_envd::policy::Grants::supported(requested_grants);
		let declaration_modules = static_declarations
			.ordered
			.iter()
			.map(|row| row.module.clone())
			.filter(|module| !module.is_empty())
			.collect::<Vec<_>>();
		let manifest_bytes = std::fs::read(&declaration.source.path).unwrap_or_default();
		let digest = ArtifactDigest::new(Hash32::sum(&manifest_bytes).into_bytes());
		let layer = match declaration.source.scope {
			SourceScope::Project => Str::new_static("project"),
			SourceScope::User => Str::new_static("user"),
			SourceScope::Package => Str::new_static("package"),
			SourceScope::Native => Str::new_static("native"),
			SourceScope::BuiltIn => Str::new_static("builtin"),
		};
		let publisher = declaration
			.source
			.installed_package_id
			.clone()
			.unwrap_or_else(|| Str::new_static("local"));
		let version = extension
			.manifest
			.get("version")
			.and_then(serde_json::Value::as_str)
			.map_or_else(|| Str::new_static("0"), Str::new);
		let provenance = Provenance::new(
			publisher,
			extension.name.clone(),
			version,
			digest,
			layer.clone(),
			Str::new_static("native"),
			declaration
				.source
				.revision
				.as_ref()
				.map_or(1, |revision| revision.sequence.max(1)),
		);
		let manifest = ExtensionManifest::new_with_static(
			provenance,
			worker.module.clone(),
			declaration_modules,
			declarations,
			ServiceManifest::default(),
			static_declarations,
			[],
			[],
		);
		let key = HostKey::new(layer, "native", extension.name.clone());
		let mut spec = ExtHostSpec::new(key, manifest);
		spec.data_grants = data_grants;
		spec.python_site = Some(extension.root.clone());
		let entry = extension
			.root
			.join(worker.module.as_str().replace('.', "/"))
			.with_extension("py");
		if entry.is_file() {
			spec.entry_path = Some(entry);
		}
		match omp_ext::config::CliContributionSet::build(extension.cli.clone(), []) {
			Ok(cli) => spec.cli_contributions = cli,
			Err(error) => {
				warnings
					.push(Str::from(format!("extension {} was not admitted: {error}", extension.name)));
				continue;
			},
		}
		specs.push(spec);
	}
	(specs, warnings)
}

fn discovery_provider_id(source_id: &str) -> Option<&str> {
	if source_id == "native-project-context" {
		Some("agents-md")
	} else if let Some(provider) = source_id.strip_prefix("foreign-") {
		Some(provider)
	} else if source_id.starts_with("native") {
		Some("native")
	} else {
		None
	}
}

/// One data-only winning set ready for its owning runtime.
#[derive(Clone, Debug)]
pub struct WinningCapabilitySet {
	/// Capability family consumed by the domain owner.
	pub kind:    CapabilityKind,
	/// Immutable winning declarations.
	pub winners: Arc<[Arc<CapabilityRecord>]>,
}

/// Immutable, per-chat/session discovery result.
///
/// The snapshot contains only static declarations and diagnostics. Runtime
/// owners may project their winning set, but discovery never imports or
/// activates executable extension code.
#[derive(Clone, Debug)]
pub struct DiscoverySnapshot {
	results:      Arc<BTreeMap<CapabilityKind, CapabilityResult>>,
	winning_sets: Arc<[WinningCapabilitySet]>,
}

impl DiscoverySnapshot {
	/// Returns the complete diagnostics and claims for one capability family.
	pub fn result(&self, kind: CapabilityKind) -> Option<&CapabilityResult> {
		self.results.get(&kind)
	}

	/// Returns one immutable winning set for its domain owner.
	pub fn winning_set(&self, kind: CapabilityKind) -> Option<&[Arc<CapabilityRecord>]> {
		self
			.results
			.get(&kind)
			.map(|result| result.winners.as_ref())
	}

	/// Iterates complete results in canonical capability order.
	pub fn results(&self) -> impl ExactSizeIterator<Item = &CapabilityResult> + DoubleEndedIterator {
		self.results.values()
	}

	/// Returns data-only domain dispatch sets in canonical capability order.
	pub fn dispatch_sets(&self) -> &[WinningCapabilitySet] {
		&self.winning_sets
	}
}

/// Mutable discovery assembly consumed exactly once to freeze a session
/// snapshot. Consuming `self` prevents a chat from rediscovering beneath an
/// already composed prompt or runtime registry.
#[derive(Debug)]
pub struct DiscoveryComposition {
	registry: DiscoveryRegistry,
	context:  LoadContext,
}

impl DiscoveryComposition {
	/// Starts one session composition. The registry's cache is installed into
	/// the load context so no provider can accidentally use a process-global or
	/// sibling-session cache.
	pub fn new(registry: DiscoveryRegistry, mut context: LoadContext) -> Self {
		context.cache = Arc::clone(registry.cache());
		Self { registry, context }
	}

	/// Concurrently loads all canonical families and freezes their winners,
	/// suppressed claims, warnings, timings, and failures.
	pub async fn freeze(self, options: LoadOptions<'_>) -> DiscoverySnapshot {
		let loaded = join_all(
			CAPABILITY_KINDS
				.iter()
				.copied()
				.map(|kind| self.registry.load(kind, &self.context, options)),
		)
		.await;
		let results = loaded
			.into_iter()
			.map(|result| (result.kind, result))
			.collect::<BTreeMap<_, _>>();
		let winning_sets = results
			.iter()
			.map(|(kind, result)| WinningCapabilitySet {
				kind:    *kind,
				winners: Arc::clone(&result.winners),
			})
			.collect::<Vec<_>>()
			.into();
		DiscoverySnapshot { results: Arc::new(results), winning_sets }
	}
}

/// Normalizes provider-returned model rows conservatively before applying them
/// as runtime catalog overlays.
///
/// Missing evidence remains unknown; this module never infers capabilities from
/// provider or model names.
pub fn normalize(
	rows: &[DiscoveredModel],
	wire_policy: WirePolicyId,
	extended_wire_policy: Option<WirePolicyId>,
	thinking: Option<ThinkingPolicyId>,
) -> Result<Vec<NormalizedDiscovery>, Box<omp_catalog::discover::DiscoveryError>> {
	DiscoveryNormalizer::new(DiscoveryDefaults {
		wire_policy,
		extended_wire_policy,
		context: ContextStrategy::Replay,
		thinking,
		pricing: Pricing::default(),
	})
	.normalize_batch(rows)
	.map_err(Box::new)
}

/// Returns the route restriction carried by an authenticated discovery request.
pub const fn route_scope(route: RouteId) -> RouteId {
	route
}
#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	#[test]
	fn shared_prompt_snapshot_freezes_repo_walk_and_user_context() {
		let tree = tempfile::tempdir().expect("tree");
		let home = tree.path().join("home");
		let repo = home.join("repo");
		let cwd = repo.join("packages/api");
		fs::create_dir_all(home.join(".omp/agent")).expect("user native");
		fs::create_dir_all(repo.join(".git")).expect("repository");
		fs::create_dir_all(&cwd).expect("cwd");
		fs::write(home.join(".omp/agent/AGENTS.md"), "user").expect("user context");
		fs::write(repo.join("AGENTS.md"), "repository").expect("repo context");
		fs::write(cwd.join("AGENTS.md"), "package").expect("package context");
		let snapshot = active_prompt_snapshots(&cwd, &[], &home, &PromptDiscoverySettings::default());
		assert_eq!(
			snapshot
				.context
				.items
				.iter()
				.map(|item| item.content.as_str())
				.collect::<Vec<_>>(),
			["repository", "package", "user"],
		);
	}

	#[test]
	fn native_extension_workers_and_process_tools_are_admitted_from_one_snapshot() {
		let tree = tempfile::tempdir().expect("tree");
		let home = tree.path().join("home");
		let repo = home.join("repo");
		fs::create_dir_all(repo.join(".omp/tools")).expect("native root");
		fs::write(
			repo.join(".omp/extension.json"),
			r#"{"name":"demo","worker":{"module":"worker"}}"#,
		)
		.expect("manifest");
		fs::write(repo.join(".omp/worker.py"), "def activate(): pass\n").expect("worker");
		fs::write(repo.join(".omp/tools/check.sh"), "#!/bin/sh\ncat\n").expect("tool");
		let snapshot =
			active_prompt_snapshots(&repo, &[], &home, &PromptDiscoverySettings::default());
		assert_eq!(snapshot.content.extensions.len(), 1);
		assert_eq!(snapshot.content.extensions[0].key.extension().as_str(), "demo");
		assert!(!snapshot.content.process_tools.is_empty());
	}
}
