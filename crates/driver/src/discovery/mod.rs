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
	collections::{BTreeMap, BTreeSet},
	env, iter,
	path::{Path, PathBuf},
	sync::Arc,
};

use bytes::{Bytes, BytesMut};
use futures::future::join_all;
use omp_agent::{GateError, GateEvent, GateOutcome, HookEvent, HookGate};
use omp_catalog::{
	ContextStrategy, Pricing, RouteId, ThinkingPolicyId, WirePolicyId,
	discover::{DiscoveredModel, DiscoveryDefaults, DiscoveryNormalizer, NormalizedDiscovery},
};
use omp_core::{ArtifactDigest, Hash32, Provenance, Str, sf};
use omp_envd::{
	exthost::{
		DeclarationSet, ExtensionManifest, HookDeclarationKey, ServiceManifest, ToolDeclarationKey,
	},
	worker::{ExtHostSpec, HostKey},
};
use omp_ext::{
	config::{
		CliSettingOverride, DeploymentManifest, ResourceFamily, ScopedOverlay, fold_extension,
		resolve_extension_settings,
	},
	trust::{Grant, GrantsFile, grant_covers},
};
use omp_proto::toolhost::v1::HookEventId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use self::native::{ConfiguredRootLevel, EffectiveExtensionRoots, NativeRootMode};
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
	pub model:               omp_catalog::settings::ModelSettings,
	/// Skill source, name, and custom-directory policy.
	pub skills:              SkillDiscoverySettings,
	/// Read-only foreign content family policy.
	pub foreign:             ForeignContentSettings,
	/// Built-in and blocked-rule policy.
	pub rules:               RulebookSettings,
	/// Invocation-local native root and installed-record admission policy.
	pub native:              NativeDiscoveryOptions,
	/// Durable and session-only operator grants used for installed extension
	/// admission. `None` preserves embedding callers that own admission.
	pub grants:              Option<ExtensionGrantSettings>,
	/// Ordered user then project extension configuration overlays.
	pub extension_scopes:    Vec<ScopedOverlay>,
	/// Inert command-line values validated only after the manifest is known.
	pub extension_overrides: Arc<[CliSettingOverride]>,
}

/// Grant sources consulted while admitting installed extension workers.
#[derive(Clone, Debug)]
pub struct ExtensionGrantSettings {
	/// Canonical client-side durable grant file.
	pub path:    PathBuf,
	/// Session-only grants accepted by the interactive core dialog.
	pub session: Arc<[Grant]>,
}

/// Stable resource families exposed at one discovery boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
	/// One bounded skill document.
	Skill,
	/// One reusable prompt template.
	Prompt,
	/// One terminal theme.
	Theme,
	/// One declarative rule document.
	Rule,
	/// One static subagent definition.
	Agent,
}

/// Reason one resource snapshot was assembled or refreshed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverReason {
	/// Initial session discovery.
	Startup,
	/// Explicit resource reload.
	Reload,
	/// Workspace files changed.
	WorkspaceChanged,
	/// An installed or linked extension changed.
	ExtensionChanged,
}

/// One file-backed resource visible to extension discovery hooks.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ResourceRef {
	/// Canonical Environment path.
	pub uri:    Str,
	/// Typed resource family.
	pub kind:   ResourceKind,
	/// Stable discovery source identity.
	pub origin: Str,
}

#[derive(Serialize)]
struct ResourcesDiscoverPayload<'a> {
	reason: DiscoverReason,
	root:   &'a str,
	found:  &'a [ResourceRef],
	add:    &'a [ResourceRef],
	keep:   Option<&'a BTreeSet<Str>>,
}

#[derive(Deserialize)]
struct EffectiveResourcesDiscoverPayload {
	#[serde(default)]
	add:  Vec<ResourceRef>,
	keep: Option<BTreeSet<Str>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ResourcesChangedEvent {
	added:   Box<[ResourceRef]>,
	removed: Box<[ResourceRef]>,
	reason:  DiscoverReason,
}

impl HookEvent for ResourcesChangedEvent {
	type Return = ();

	const ID: HookEventId = HookEventId::HookEventResourcesChanged;
	const REV: u32 = 1;

	fn encode_into(&self, out: &mut BytesMut) {
		out.extend_from_slice(b"\n");
		out.extend_from_slice(
			&serde_json::to_vec(self).expect("resource change payload must serialize to JSON"),
		);
	}

	fn apply(&mut self, _: &omp_agent::HookPatch) -> Result<(), GateError> {
		Ok(())
	}
}

/// Fail-closed refusal or malformed effective payload from resource discovery.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ResourceDiscoveryError {
	/// A discovery handler denied the refresh.
	#[error("resource discovery denied: {0}")]
	Denied(Str),
	/// A discovery handler returned a payload that did not match the event.
	#[error("resource discovery returned a malformed effective payload")]
	Malformed,
	/// A contributed skill path failed containment or bounded-file admission.
	#[error("resource discovery contribution was rejected: {0}")]
	Contribution(Str),
}

#[derive(Debug, Error)]
enum ExtensionAdmissionError {
	#[error("extension {extension} at {path} was not admitted: duplicate extension identity")]
	DuplicateIdentity { extension: Str, path: PathBuf },
	#[error(
		"extension {extension} was not admitted: entry module `{module}` was not found under {site}"
	)]
	MissingEntry { extension: Str, module: Str, site: PathBuf },
}

/// One installed extension omitted pending a Core-owned operator decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionGrantRequest {
	/// Exact grant record to persist or retain for this session.
	pub grant:                  Grant,
	/// Canonical capabilities requested by the active manifest.
	pub requested_capabilities: Arc<[Str]>,
	/// Capabilities covered by the currently matching durable grant.
	pub granted_capabilities:   Arc<[Str]>,
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
	pub skills:           Arc<SkillSnapshot>,
	/// Active declarative rules.
	pub rules:            Arc<RuleSnapshot>,
	/// Active native Markdown slash commands in discovery precedence order.
	pub commands:         Arc<[CommandContribution]>,
	/// Bounded non-fatal diagnostics emitted while loading static content.
	pub warnings:         Arc<[Str]>,
	/// Frozen declarations from the same startup discovery pass.
	pub declarations:     Arc<[DiscoveredCapability]>,
	/// Authenticated native extension workers admitted from those declarations.
	pub extensions:       Arc<[ExtHostSpec]>,
	/// Installed extension identities awaiting interactive operator consent.
	pub extension_grants: Arc<[ExtensionGrantRequest]>,
	/// Environment-bound process custom tools admitted from those declarations.
	pub process_tools:    Arc<custom_tools::ProcessToolFactory>,
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
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(root = %root.display(), additional_root_count = additional_roots.len())
)]
pub fn active_prompt_snapshots(
	root: &Path,
	additional_roots: &[PathBuf],
	home: &Path,
	settings: &PromptDiscoverySettings,
) -> ActivePromptSnapshots {
	let disabled_providers = settings.model.resolved_disabled_providers(root, home);
	let content =
		active_content_snapshots_with_home(root, home, disabled_providers.as_ref(), settings, &[]);
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
	tracing::debug!(
		declaration_count = content.declarations.len(),
		context_item_count = context.items.len(),
		context_diagnostic_count = context.diagnostics.len(),
		"active prompt snapshots discovered"
	);
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
	active_content_snapshots_with_home(root, &home, &[], &PromptDiscoverySettings::default(), &[])
}

/// Freezes admitted `resources_discover` skill paths into a new session
/// snapshot.
pub fn active_content_snapshots_with_skill_contributions(
	root: &Path,
	extension_id: &Str,
	contributions: &[omp_envd::exthost::dispatch::SkillPathContribution],
) -> ActiveContentSnapshots {
	let home = env::var_os("HOME").map_or_else(|| root.to_path_buf(), PathBuf::from);
	let sources = skills::contributed_sources(
		extension_id,
		contributions
			.iter()
			.map(|contribution| (contribution.path.clone(), contribution.contain_root.clone())),
	);
	active_content_snapshots_with_home(
		root,
		&home,
		&[],
		&PromptDiscoverySettings::default(),
		&sources,
	)
}

/// Runs the fail-closed resource discovery gate over one complete static pass.
///
/// The immutable snapshot is returned unchanged when ordinal 35 is not
/// subscribed, without constructing the resource inventory or a CONTROL frame.
pub async fn gate_resources_discover(
	gate: &HookGate,
	reason: DiscoverReason,
	root: &Path,
	allowed_roots: &[PathBuf],
	settings: &PromptDiscoverySettings,
	snapshot: ActiveContentSnapshots,
) -> Result<ActiveContentSnapshots, ResourceDiscoveryError> {
	if !gate.subscribed(HookEventId::HookEventResourcesDiscover) {
		return Ok(snapshot);
	}
	let found = resource_refs(snapshot.declarations.as_ref());
	let root_text = root.to_string_lossy();
	let requested = ResourcesDiscoverPayload {
		reason,
		root: root_text.as_ref(),
		found: &found,
		add: &[],
		keep: None,
	};
	let bytes =
		serde_json::to_vec(&requested).expect("resource discovery payload must serialize to JSON");
	let outcome = gate
		.gate(
			HookEventId::HookEventResourcesDiscover,
			GateEvent::new(sf!("resources_discover"), Bytes::from(bytes)),
		)
		.await;
	let effective = match outcome {
		GateOutcome::Allow { event, .. } => event.effective_args,
		GateOutcome::Deny { reason, .. } => return Err(ResourceDiscoveryError::Denied(reason)),
		GateOutcome::Approval { .. } => {
			return Err(ResourceDiscoveryError::Denied(sf!(
				"resource discovery cannot require approval"
			)));
		},
	};
	let effective: EffectiveResourcesDiscoverPayload =
		serde_json::from_slice(&effective).map_err(|_| ResourceDiscoveryError::Malformed)?;
	let before = snapshot.clone();
	let after = apply_resource_transform(snapshot, settings, allowed_roots, effective)?;
	notify_resources_changed(gate, reason, &before, &after);
	Ok(after)
}

/// Emits one observe notification when a committed discovery refresh changed
/// the representable resource set.
pub fn notify_resources_changed(
	gate: &HookGate,
	reason: DiscoverReason,
	before: &ActiveContentSnapshots,
	after: &ActiveContentSnapshots,
) {
	if !gate.subscribed(HookEventId::HookEventResourcesChanged) {
		return;
	}
	let before = resource_refs(before.declarations.as_ref())
		.into_iter()
		.collect::<BTreeSet<_>>();
	let after = resource_refs(after.declarations.as_ref())
		.into_iter()
		.collect::<BTreeSet<_>>();
	let added = after.difference(&before).cloned().collect::<Vec<_>>();
	let removed = before.difference(&after).cloned().collect::<Vec<_>>();
	if added.is_empty() && removed.is_empty() {
		return;
	}
	gate.notify(&ResourcesChangedEvent {
		added: added.into_boxed_slice(),
		removed: removed.into_boxed_slice(),
		reason,
	});
}

fn apply_resource_transform(
	mut snapshot: ActiveContentSnapshots,
	settings: &PromptDiscoverySettings,
	allowed_roots: &[PathBuf],
	effective: EffectiveResourcesDiscoverPayload,
) -> Result<ActiveContentSnapshots, ResourceDiscoveryError> {
	let mut declarations = snapshot.declarations.as_ref().to_vec();
	for addition in effective
		.add
		.iter()
		.filter(|addition| addition.kind == ResourceKind::Skill)
	{
		let composed = serde_json::json!({"add": [addition]});
		let admitted =
			omp_envd::exthost::dispatch::admit_skill_path_contributions(&composed, allowed_roots)
				.map_err(|error| ResourceDiscoveryError::Contribution(Str::from(error.to_string())))?;
		let sources = skills::contributed_sources(
			&addition.origin,
			admitted
				.into_iter()
				.map(|contribution| (contribution.path, contribution.contain_root)),
		);
		let discovered = skills::discover(&sources, &settings.skills);
		declarations.extend(discovered.declarations);
		let mut warnings = snapshot.warnings.as_ref().to_vec();
		warnings.extend(
			discovered
				.warnings
				.into_iter()
				.map(|warning| warning.message),
		);
		snapshot.warnings = warnings.into();
	}
	if let Some(keep) = effective.keep {
		declarations.retain(|declaration| {
			declaration_resource_ref(declaration).is_none_or(|resource| keep.contains(&resource.uri))
		});
	}
	snapshot.skills = Arc::new(SkillSnapshot::from_declarations(&declarations));
	snapshot.rules = Arc::new(RuleSnapshot::from_declarations(&declarations, &settings.rules));
	snapshot.declarations = declarations.into();
	Ok(snapshot)
}

fn resource_refs(declarations: &[DiscoveredCapability]) -> Vec<ResourceRef> {
	let mut resources = declarations
		.iter()
		.filter(|declaration| declaration.enabled)
		.filter_map(declaration_resource_ref)
		.collect::<Vec<_>>();
	resources.sort();
	resources.dedup();
	resources
}

fn declaration_resource_ref(declaration: &DiscoveredCapability) -> Option<ResourceRef> {
	let (kind, path) = match &declaration.payload {
		CapabilityPayload::Skills(value) => (ResourceKind::Skill, value.path.as_path()),
		CapabilityPayload::Themes(value) => (ResourceKind::Theme, value.path.as_path()),
		CapabilityPayload::Prompts(value) => (ResourceKind::Prompt, value.path.as_path()),
		CapabilityPayload::Rules(value) => (ResourceKind::Rule, value.path.as_path()),
		CapabilityPayload::Agents(value) => (ResourceKind::Agent, value.path.as_path()),
		_ => return None,
	};
	Some(ResourceRef {
		uri: Str::from(path.to_string_lossy().as_ref()),
		kind,
		origin: declaration.source.source_id.clone(),
	})
}

/// Derives the canonical local workspace grant identity used by extension
/// admission and extension management commands.
pub fn workspace_identity(root: &Path) -> omp_ext::WorkspaceUri {
	let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
	let uri = url::Url::from_directory_path(&canonical)
		.map(|url| url.to_string())
		.unwrap_or_else(|()| format!("file://{}", canonical.display()));
	omp_ext::WorkspaceUri {
		digest: omp_core::sf!("sha256:{}", Hash32::sum(uri.as_bytes()).to_hex()),
		uri:    Str::from(uri),
	}
}

fn active_content_snapshots_with_home(
	root: &Path,
	home: &Path,
	disabled_providers: &[Str],
	settings: &PromptDiscoverySettings,
	contributed_skill_sources: &[skills::SkillSource],
) -> ActiveContentSnapshots {
	let mut discovered = native::discover_capabilities(root, home, 64, &NativeDiscoveryOptions {
		skill_settings: settings.skills.clone(),
		..settings.native.clone()
	});
	let extension_skill_sources = discovered
		.declarations
		.iter()
		.filter_map(|declaration| {
			let CapabilityPayload::Extensions(extension) = &declaration.payload else {
				return None;
			};
			let static_declarations = extension.static_declarations().ok()?;
			Some(skills::extension_sources(
				&extension.name,
				&extension.root,
				&static_declarations.ordered,
			))
		})
		.flatten()
		.collect::<Vec<_>>();
	let extension_skills = skills::discover(&extension_skill_sources, &settings.skills);
	discovered
		.declarations
		.extend(extension_skills.declarations);
	discovered.warnings.extend(
		extension_skills
			.warnings
			.into_iter()
			.map(|warning| warning.message),
	);
	let extension_themes = discovered
		.declarations
		.iter()
		.filter_map(|declaration| {
			let CapabilityPayload::Extensions(extension) = &declaration.payload else {
				return None;
			};
			let static_declarations = extension.static_declarations().ok()?;
			Some(manifest::discover_manifest_themes(
				&extension.name,
				&extension.root,
				&static_declarations.ordered,
			))
		})
		.collect::<Vec<_>>();
	for themes in extension_themes {
		discovered.declarations.extend(themes.declarations);
		discovered.warnings.extend(
			themes.warnings.into_iter().map(|warning| {
				Str::from(format!("ignored extension theme {}", warning.path.display()))
			}),
		);
	}
	let contributed_skills = skills::discover(contributed_skill_sources, &settings.skills);
	discovered
		.declarations
		.extend(contributed_skills.declarations);
	discovered.warnings.extend(
		contributed_skills
			.warnings
			.into_iter()
			.map(|warning| warning.message),
	);
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
		let Some(package_id) = declaration.source.installed_package_id.as_ref() else {
			return true;
		};
		let Some((family, path)) = package_resource_path(declaration) else {
			return true;
		};
		fold_extension(&settings.extension_scopes, package_id).resource_enabled(
			family,
			path.as_str(),
			true,
		)
	});
	discovered.declarations.retain(|declaration| {
		discovery_provider_id(declaration.source.source_id.as_str()).is_none_or(|provider| {
			!disabled_providers
				.iter()
				.any(|disabled| disabled == provider)
		})
	});
	let (extensions, extension_grants, extension_warnings) = admit_extension_specs(
		&discovered.declarations,
		settings.grants.as_ref(),
		&settings.extension_scopes,
		&settings.extension_overrides,
	);
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
	if !discovered.warnings.is_empty() {
		tracing::warn!(
			warning_count = discovered.warnings.len(),
			"prompt discovery completed with warnings"
		);
	}
	ActiveContentSnapshots {
		skills:           Arc::new(SkillSnapshot::from_declarations(&discovered.declarations)),
		rules:            Arc::new(RuleSnapshot::from_declarations(
			&discovered.declarations,
			&settings.rules,
		)),
		commands:         commands.into(),
		warnings:         discovered.warnings.into(),
		declarations:     discovered.declarations.into(),
		extensions:       extensions.into(),
		extension_grants: extension_grants.into(),
		process_tools:    Arc::new(process_tools),
	}
}

fn package_resource_path(declaration: &DiscoveredCapability) -> Option<(ResourceFamily, String)> {
	let family = match &declaration.payload {
		CapabilityPayload::Extensions(_) => ResourceFamily::Extensions,
		CapabilityPayload::Skills(_) => ResourceFamily::Skills,
		CapabilityPayload::Prompts(_) => ResourceFamily::Prompts,
		CapabilityPayload::Themes(_) => ResourceFamily::Themes,
		_ => return None,
	};
	let directory = family.to_string();
	let components = declaration.source.path.components().collect::<Vec<_>>();
	let start = components
		.iter()
		.position(|component| component.as_os_str() == directory.as_str());
	let path = start.map_or_else(
		|| {
			declaration
				.source
				.path
				.file_name()
				.map_or_else(String::new, |name| name.to_string_lossy().into_owned())
		},
		|start| {
			components[start..]
				.iter()
				.map(|component| component.as_os_str().to_string_lossy())
				.collect::<Vec<_>>()
				.join("/")
		},
	);
	Some((family, path))
}

fn admit_extension_specs(
	declarations: &[DiscoveredCapability],
	grant_settings: Option<&ExtensionGrantSettings>,
	extension_scopes: &[ScopedOverlay],
	extension_overrides: &[CliSettingOverride],
) -> (Vec<ExtHostSpec>, Vec<ExtensionGrantRequest>, Vec<Str>) {
	let durable_grants = grant_settings
		.map(|settings| GrantsFile::read(&settings.path))
		.transpose();
	let durable_grants = match durable_grants {
		Ok(grants) => grants,
		Err(error) => {
			return (Vec::new(), Vec::new(), vec![Str::from(format!(
				"installed extensions were not admitted: {error}"
			))]);
		},
	};
	let session_grants = grant_settings
		.map(|settings| GrantsFile { version: 1, grants: settings.session.as_ref().to_vec() });
	let mut seen = std::collections::BTreeSet::new();
	let mut specs = Vec::new();
	let mut grant_requests = Vec::new();
	let mut warnings = Vec::new();
	for declaration in declarations {
		let CapabilityPayload::Extensions(extension) = &declaration.payload else {
			continue;
		};
		if !seen.insert(extension.name.clone()) {
			warnings.push(Str::from(
				ExtensionAdmissionError::DuplicateIdentity {
					extension: extension.name.clone(),
					path:      declaration.source.path.clone(),
				}
				.to_string(),
			));
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
		let mut admitted_tier = extension.grant.as_ref().map(|facts| facts.tier);
		let requested_capabilities = static_declarations
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
			.collect::<std::collections::BTreeSet<_>>();
		// An explicit invocation root is itself the operator's admission decision;
		// durable grants apply only to installed package records.
		if let Some(_settings) = grant_settings
			&& declaration.source.scope != SourceScope::Native
		{
			let Some(facts) = extension.grant.as_ref() else {
				warnings.push(Str::from(format!(
					"extension {} was not admitted: installed package has no locked grant identity",
					extension.name
				)));
				continue;
			};
			let trusted_link = facts.ship == "link"
				&& (durable_grants.as_ref().is_some_and(|durable| {
					grant_covers(
						durable,
						&facts.id,
						&facts.publisher,
						facts.layer,
						facts.workspace.as_ref(),
						&facts.capability_digest,
						omp_ext::TrustTier::Trusted,
						&facts.ship,
					)
				}) || session_grants.as_ref().is_some_and(|session| {
					grant_covers(
						session,
						&facts.id,
						&facts.publisher,
						facts.layer,
						facts.workspace.as_ref(),
						&facts.capability_digest,
						omp_ext::TrustTier::Trusted,
						&facts.ship,
					)
				}));
			if trusted_link {
				admitted_tier = Some(omp_ext::TrustTier::Trusted);
			}
			let covered = trusted_link
				|| durable_grants.as_ref().is_some_and(|durable| {
					grant_covers(
						durable,
						&facts.id,
						&facts.publisher,
						facts.layer,
						facts.workspace.as_ref(),
						&facts.capability_digest,
						facts.tier,
						&facts.ship,
					)
				}) || session_grants.as_ref().is_some_and(|session| {
				grant_covers(
					session,
					&facts.id,
					&facts.publisher,
					facts.layer,
					facts.workspace.as_ref(),
					&facts.capability_digest,
					facts.tier,
					&facts.ship,
				)
			});
			let unsigned_sandboxed_link = facts.ship == "link"
				&& facts.tier == omp_ext::TrustTier::Sandboxed
				&& requested_capabilities.is_empty();
			if !covered && !unsigned_sandboxed_link {
				grant_requests.push(ExtensionGrantRequest {
					grant:                  Grant {
						id:                facts.id.clone(),
						publisher:         facts.publisher.clone(),
						layer:             facts.layer,
						workspace:         facts.workspace.clone(),
						scope:             omp_ext::trust::GrantScope::Exact,
						capability_digest: facts.capability_digest.clone(),
						tier:              facts.tier,
						ship:              facts.ship.clone(),
						granted_at:        Str::new_static(""),
						granted_by:        Str::new_static("interactive"),
						duration:          omp_ext::trust::GrantDuration::Persistent,
					},
					requested_capabilities: requested_capabilities
						.iter()
						.cloned()
						.collect::<Vec<_>>()
						.into(),
					granted_capabilities:   Arc::from([]),
				});
				warnings.push(Str::from(format!(
					"extension {} was not admitted: no exact operator grant covers its capabilities",
					extension.name
				)));
				continue;
			}
		}
		let tools = static_declarations
			.tools
			.iter()
			.filter_map(|row| {
				let uniform_key = row.key.rsplit_once('@').and_then(|(name, revision)| {
					let (family, rev) = revision.rsplit_once('.')?;
					let rev = rev.parse::<u16>().ok()?;
					(!name.is_empty()).then_some((name, family, rev))
				});
				let (name, family, rev) = uniform_key.unwrap_or_else(|| {
					let name = if row.key.is_empty() {
						row.id.as_str()
					} else {
						row.key.as_str()
					};
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
					(name, family, rev)
				});
				(!name.is_empty()).then(|| ToolDeclarationKey::new(name, family, rev))
			})
			.collect::<Vec<_>>();
		let hooks = static_declarations
			.hooks
			.iter()
			.filter_map(|row| {
				let uniform_key = row.key.rsplit_once('/');
				let event = row
					.properties
					.get("event")
					.and_then(serde_json::Value::as_str)
					.or_else(|| uniform_key.map(|(event, _)| event))
					.unwrap_or(row.key.as_str());
				let phase = row
					.properties
					.get("phase")
					.and_then(serde_json::Value::as_str)
					.or_else(|| uniform_key.map(|(_, phase)| phase))
					.unwrap_or("observe")
					.parse::<omp_agent::HookPhase>()
					.ok()?;
				(!event.is_empty()).then(|| HookDeclarationKey::new(event, phase))
			})
			.collect::<Vec<_>>();
		let declarations = DeclarationSet::new(tools, hooks);
		let data_grants = omp_envd::policy::Grants::supported(requested_capabilities.iter().cloned());
		let declaration_modules = static_declarations
			.ordered
			.iter()
			.map(|row| row.module.clone())
			.filter(|module| !module.is_empty())
			.collect::<Vec<_>>();
		let manifest_bytes = std::fs::read(&declaration.source.path).unwrap_or_default();
		let digest = ArtifactDigest::new(Hash32::sum(&manifest_bytes).into_bytes());
		let layer = extension.grant.as_ref().map_or_else(
			|| match declaration.source.scope {
				SourceScope::Project => Str::new_static("project"),
				SourceScope::User => Str::new_static("user"),
				SourceScope::Package => Str::new_static("package"),
				SourceScope::Native => Str::new_static("native"),
				SourceScope::BuiltIn => Str::new_static("builtin"),
			},
			|facts| Str::from(facts.layer.to_string()),
		);
		let publisher = extension
			.grant
			.as_ref()
			.map(|facts| facts.publisher.clone())
			.or_else(|| declaration.source.installed_package_id.clone())
			.unwrap_or_else(|| Str::new_static("local"));
		let version = extension
			.manifest
			.get("version")
			.and_then(serde_json::Value::as_str)
			.map_or_else(|| Str::new_static("0"), Str::new);
		// Ungranted loads (one-invocation --plugin-dir, unsigned links) launch in
		// the default isolated tier; the spawn policy only knows sandboxed and
		// trusted.
		let tier = Str::from(admitted_tier.unwrap_or_default().to_string());
		let settings_schema = extension
			.manifest
			.get("settings")
			.cloned()
			.map(serde_json::from_value)
			.transpose();
		let settings_schema = match settings_schema {
			Ok(Some(settings)) => settings,
			Ok(None) => Default::default(),
			Err(error) => {
				warnings.push(Str::from(format!(
					"extension {} was not admitted: invalid settings schema: {error}",
					extension.name
				)));
				continue;
			},
		};
		let deployment_manifest = DeploymentManifest {
			id: extension.name.clone(),
			entry: worker.module.clone(),
			settings: settings_schema,
			..DeploymentManifest::default()
		};
		let configured = fold_extension(extension_scopes, &extension.name);
		let resolved_settings = match resolve_extension_settings(
			&deployment_manifest,
			&configured.settings,
			extension_overrides,
		) {
			Ok(settings) => settings,
			Err(error) => {
				warnings
					.push(Str::from(format!("extension {} was not admitted: {error}", extension.name)));
				continue;
			},
		};
		let provenance = Provenance::new(
			publisher,
			extension
				.grant
				.as_ref()
				.map_or_else(|| extension.name.clone(), |facts| facts.id.clone()),
			version,
			digest,
			layer.clone(),
			tier.clone(),
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
		let key = HostKey::new(layer, tier, extension.name.clone());
		let mut spec = ExtHostSpec::new(key, manifest);
		spec.data_grants = data_grants;
		spec.settings = resolved_settings;
		let python_site = if extension.root.join("src").is_dir() {
			extension.root.join("src")
		} else {
			extension.root.clone()
		};
		spec.python_site = Some(python_site.clone());
		if extension
			.grant
			.as_ref()
			.is_some_and(|facts| facts.ship == "link")
		{
			spec.watch_root = Some(extension.root.clone());
		}
		let module_path = python_site.join(worker.module.as_str().replace('.', "/"));
		let module_file = module_path.with_extension("py");
		let package_file = module_path.join("__init__.py");
		spec.entry_path = if module_file.is_file() {
			Some(module_file)
		} else if package_file.is_file() {
			Some(package_file)
		} else {
			warnings.push(Str::from(
				ExtensionAdmissionError::MissingEntry {
					extension: extension.name.clone(),
					module:    worker.module.clone(),
					site:      python_site,
				}
				.to_string(),
			));
			continue;
		};
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
	for value in extension_overrides {
		if !seen.contains(&value.extension) {
			warnings.push(Str::from(format!(
				"extension {} was not admitted: setting override targets an unknown extension",
				value.extension
			)));
		}
	}
	(specs, grant_requests, warnings)
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

	use omp_core::sf;
	use omp_tools::read::{resolver::Resolve as _, selector::ParsedSelector};

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
	fn packaged_theme_is_discovered_and_loadable() {
		let tree = tempfile::tempdir().expect("tree");
		let home = tree.path().join("home");
		let repo = home.join("repo");
		let theme = repo.join(".omp/demo/themes/ocean.json");
		fs::create_dir_all(theme.parent().expect("theme parent")).expect("theme directory");
		fs::write(&theme, r##"{"name":"ocean","dark":{"fg":"#dcecff","accent":"#32a8c6"}}"##)
			.expect("theme");
		fs::write(
			repo.join(".omp/omp.toml"),
			r#"
id = "demo"
entry = "demo"

[[declarations]]
kind = "themes"
path = "demo/themes/*.json"
"#,
		)
		.expect("manifest");
		let snapshot = active_content_snapshots_with_home(
			&repo,
			&home,
			&[],
			&PromptDiscoverySettings::default(),
			&[],
		);
		let declaration = snapshot
			.declarations
			.iter()
			.find(|declaration| matches!(&declaration.payload, CapabilityPayload::Themes(_)))
			.expect("packaged theme");
		let CapabilityPayload::Themes(theme) = &declaration.payload else {
			unreachable!()
		};
		assert_eq!(declaration.source.scope, SourceScope::Package);
		assert_eq!(declaration.source.installed_package_id.as_deref(), Some("demo"));
		assert_eq!(theme.name, "ocean");
		assert!(theme.path.ends_with("themes/ocean.json"));
		assert_eq!(
			omp_tui::JsonTheme::parse(theme.content.as_str())
				.expect("loadable theme")
				.name,
			"ocean"
		);
	}

	#[tokio::test]
	async fn static_extension_skill_is_frozen_until_the_next_snapshot() {
		let tree = tempfile::tempdir().expect("tree");
		let home = tree.path().join("home");
		let repo = home.join("repo");
		let skill = repo.join(".omp/demo/.omp-generated/skills/review/SKILL.md");
		fs::create_dir_all(skill.parent().expect("skill parent")).expect("skill directory");
		fs::write(&skill, "---\nname: review\ndescription: 'Review a change.'\n---\n\nfirst\n")
			.expect("skill");
		fs::write(
			repo.join(".omp/extension.json"),
			r#"{
				"name":"demo",
				"declarations":[{
					"kind":"skills",
					"path":"demo/.omp-generated/skills/review/SKILL.md",
					"metadata":{"name":"review","description":"Review a change."}
				}]
			}"#,
		)
		.expect("manifest");

		let first = active_content_snapshots_with_home(
			&repo,
			&home,
			&[],
			&PromptDiscoverySettings::default(),
			&[],
		);
		assert_eq!(first.skills.get("review").expect("static skill").body, "first");
		assert_eq!(first.skills.get("review").expect("static skill").source, "demo");
		let resolver = crate::skills::SkillResolver::new(Arc::clone(&first.skills));
		let resolved = resolver
			.read("review", &ParsedSelector::None)
			.await
			.expect("skill://review");
		assert_eq!(resolved.as_ref(), b"first");

		fs::write(&skill, "---\nname: review\ndescription: 'Review a change.'\n---\n\nsecond\n")
			.expect("updated skill");
		assert_eq!(first.skills.get("review").expect("frozen skill").body, "first");

		let reloaded = active_content_snapshots_with_home(
			&repo,
			&home,
			&[],
			&PromptDiscoverySettings::default(),
			&[],
		);
		assert_eq!(reloaded.skills.get("review").expect("reloaded skill").body, "second");
	}

	#[test]
	fn linked_omp_manifest_is_admitted_without_import_and_marks_watch_root() {
		let tree = tempfile::tempdir().expect("tree");
		let home = tree.path().join("home");
		let repo = home.join("repo");
		let linked = tree.path().join("demo");
		fs::create_dir_all(linked.join("src/demo")).expect("linked package");
		fs::create_dir_all(&repo).expect("repository");
		fs::write(
			linked.join("omp.toml"),
			r#"
id = "demo"
entry = "demo"

[[declarations]]
id = "hello"
kind = "soft"
module = "demo"
key = "hello@demo.1"
trigger = "lazy"
api = 1
failure = "fail-closed"
[[declarations]]
id = "activated"
kind = "hook"
module = "demo"
key = "extension_activate/observe"
trigger = "lazy"
api = 1
failure = "fail-open"
"#,
		)
		.expect("deployment manifest");
		fs::write(linked.join("src/demo/__init__.py"), "raise AssertionError('must not import')\n")
			.expect("extension source");
		let installed_path = tree.path().join("installed.toml");
		omp_ext::lock::InstalledRecord {
			version:    2,
			extensions: vec![omp_ext::lock::InstalledExtension {
				id:       sf!("demo"),
				features: Vec::new(),
				source:   toml::Value::Table(toml::Table::from_iter([(
					"link".to_owned(),
					toml::Value::String(linked.display().to_string()),
				)])),
				tier:     omp_ext::TrustTier::Sandboxed,
				enabled:  true,
			}],
		}
		.write(&installed_path)
		.expect("installed link");
		let settings = PromptDiscoverySettings {
			native: native::NativeDiscoveryOptions {
				client_installed: Some(installed_path),
				..native::NativeDiscoveryOptions::default()
			},
			grants: Some(ExtensionGrantSettings {
				path:    tree.path().join("grants.toml"),
				session: Arc::from([]),
			}),
			..PromptDiscoverySettings::default()
		};
		let snapshot = active_prompt_snapshots(&repo, &[], &home, &settings);
		assert_eq!(snapshot.content.extensions.len(), 1);
		let spec = &snapshot.content.extensions[0];
		assert_eq!(spec.key.extension(), "demo");
		assert_eq!(spec.key.tier(), "sandboxed");
		let linked = linked.canonicalize().expect("canonical linked root");
		assert_eq!(spec.watch_root.as_deref(), Some(linked.as_path()));
		let entry = linked.join("src/demo/__init__.py");
		assert_eq!(spec.entry_path.as_deref(), Some(entry.as_path()));
		let tool = spec
			.manifest
			.declarations
			.tools()
			.next()
			.expect("uniform tool declaration");
		assert_eq!(tool.name, "hello");
		assert_eq!(tool.family, "demo");
		assert_eq!(tool.rev, 1);
		let hook = spec
			.manifest
			.declarations
			.hooks()
			.next()
			.expect("uniform hook declaration");
		assert_eq!(hook.event, "extension_activate");
		assert_eq!(hook.phase, omp_agent::HookPhase::Observe);
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

	#[test]
	fn explicit_invocation_extension_does_not_require_an_installed_grant() {
		let tree = tempfile::tempdir().expect("tree");
		let home = tree.path().join("home");
		let repo = home.join("repo");
		let extension = tree.path().join("demo");
		fs::create_dir_all(&repo).expect("repository");
		fs::create_dir_all(extension.join("src/demo")).expect("extension package");
		fs::write(extension.join("omp.toml"), "id = \"demo\"\nentry = \"demo\"\n").expect("manifest");
		fs::write(extension.join("src/demo/__init__.py"), "def activate(): pass\n").expect("worker");
		let mut settings = PromptDiscoverySettings {
			native: native::NativeDiscoveryOptions {
				explicit_roots: vec![extension.clone()],
				root_mode: native::NativeRootMode::ExplicitOnly,
				..native::NativeDiscoveryOptions::default()
			},
			grants: Some(ExtensionGrantSettings {
				path:    tree.path().join("grants.toml"),
				session: Arc::from([]),
			}),
			..PromptDiscoverySettings::default()
		};
		let snapshot = active_prompt_snapshots(&repo, &[], &home, &settings);
		assert!(snapshot.content.warnings.is_empty(), "{:?}", snapshot.content.warnings);
		assert_eq!(snapshot.content.extensions.len(), 1);
		let spec = &snapshot.content.extensions[0];
		assert_eq!(spec.key.extension(), "demo");
		assert_eq!(spec.key.layer(), "native");
		let entry = fs::canonicalize(extension.join("src/demo/__init__.py")).expect("entry path");
		assert_eq!(spec.entry_path.as_deref(), Some(entry.as_path()));

		let duplicate = tree.path().join("duplicate");
		fs::create_dir_all(duplicate.join("src/demo")).expect("duplicate package");
		fs::write(duplicate.join("omp.toml"), "id = \"demo\"\nentry = \"demo\"\n")
			.expect("duplicate manifest");
		fs::write(duplicate.join("src/demo/__init__.py"), "def activate(): pass\n")
			.expect("duplicate worker");
		settings.native.explicit_roots.push(duplicate.clone());
		let duplicate_snapshot = active_prompt_snapshots(&repo, &[], &home, &settings);
		assert_eq!(duplicate_snapshot.content.extensions.len(), 1);
		assert!(duplicate_snapshot.content.warnings.iter().any(|warning| {
			warning.contains(duplicate.join("omp.toml").to_string_lossy().as_ref())
				&& warning.contains("duplicate extension identity")
		}));
	}

	fn installed_extension(root: &Path, capabilities: &[&str]) -> DiscoveredCapability {
		std::fs::write(root.join("worker.py"), "").expect("fixture entry module");
		let manifest = [(sf!("capabilities"), serde_json::json!({"data": capabilities}))]
			.into_iter()
			.collect();
		DiscoveredCapability::keyed(
			"acme.reviewer",
			CapabilityPayload::Extensions(manifest::ExtensionPayload {
				name: sf!("acme.reviewer"),
				root: root.to_path_buf(),
				description: None,
				worker: Some(manifest::PythonWorkerDeclaration { module: sf!("worker"), entry: None }),
				cli: Vec::new(),
				selected_features: Box::new([]),
				grant: Some(Arc::new(manifest::ExtensionGrantFacts {
					id:                sf!("acme.reviewer"),
					publisher:         sf!("ed25519:publisher"),
					layer:             omp_ext::Layer::Client,
					workspace:         None,
					capability_digest: sf!("b3:capabilities"),
					tier:              omp_ext::TrustTier::Sandboxed,
					ship:              sf!("installed"),
				})),
				manifest,
			}),
			manifest::SourceProvenance::native(
				"installed",
				root.join("extension.json"),
				SourceScope::Package,
			),
		)
	}

	fn grant_settings(path: &Path, session: Vec<Grant>) -> ExtensionGrantSettings {
		ExtensionGrantSettings { path: path.to_path_buf(), session: session.into() }
	}

	#[test]
	fn cli_override_is_validated_at_admit_and_frozen_into_host_settings() {
		let tree = tempfile::tempdir().expect("tree");
		let mut declaration = installed_extension(tree.path(), &[]);
		let CapabilityPayload::Extensions(extension) = &mut declaration.payload else {
			panic!("extension payload");
		};
		extension.manifest.insert(
			sf!("settings"),
			serde_json::json!({
				"verbose": {"type": "boolean", "default": false}
			}),
		);
		let overrides =
			[CliSettingOverride::parse("acme.reviewer.verbose=true").expect("generic override")];
		let (specs, requests, warnings) =
			admit_extension_specs(&[declaration.clone()], None, &[], &overrides);
		assert!(requests.is_empty());
		assert!(warnings.is_empty());
		assert_eq!(specs.len(), 1);
		assert_eq!(specs[0].settings["verbose"], serde_json::json!(true));

		let invalid =
			[CliSettingOverride::parse("acme.reviewer.unknown=true").expect("generic override")];
		let (specs, _, warnings) = admit_extension_specs(&[declaration], None, &[], &invalid);
		assert!(specs.is_empty());
		assert!(
			warnings
				.iter()
				.any(|warning| { warning.contains("acme.reviewer") && warning.contains("unknown") })
		);
	}

	#[test]
	fn non_interactive_ungranted_extension_is_omitted_with_the_existing_diagnostic() {
		let tree = tempfile::tempdir().expect("tree");
		let declarations = [installed_extension(tree.path(), &["env.fs.read"])];
		let settings = grant_settings(&tree.path().join("grants.toml"), Vec::new());
		let (specs, requests, warnings) =
			admit_extension_specs(&declarations, Some(&settings), &[], &[]);
		assert!(specs.is_empty());
		assert_eq!(requests.len(), 1);
		assert!(
			warnings
				.iter()
				.any(|warning| warning.contains("was not admitted"))
		);
		assert_eq!(requests[0].requested_capabilities.as_ref(), [sf!("env.fs.read")]);
	}

	#[test]
	fn allow_once_is_scoped_to_one_session_and_deny_stays_degraded() {
		let tree = tempfile::tempdir().expect("tree");
		let declarations = [installed_extension(tree.path(), &["env.fs.read"])];
		let empty = grant_settings(&tree.path().join("grants.toml"), Vec::new());
		let (_, requests, _) = admit_extension_specs(&declarations, Some(&empty), &[], &[]);
		let once = Grant {
			granted_at: sf!("now"),
			granted_by: sf!("interactive-once"),
			duration: omp_ext::trust::GrantDuration::Once,
			..requests[0].grant.clone()
		};
		let session = grant_settings(&tree.path().join("grants.toml"), vec![once]);
		let (admitted, pending, session_warnings) =
			admit_extension_specs(&declarations, Some(&session), &[], &[]);
		assert_eq!(admitted.len(), 1, "{session_warnings:?}");
		assert!(pending.is_empty());
		let (next_session, denied, warnings) =
			admit_extension_specs(&declarations, Some(&empty), &[], &[]);
		assert!(next_session.is_empty());
		assert_eq!(denied.len(), 1);
		assert!(
			warnings
				.iter()
				.any(|warning| warning.contains("no exact operator grant"))
		);
	}

	#[test]
	fn allow_persist_prevents_a_subsequent_admission_prompt() {
		let tree = tempfile::tempdir().expect("tree");
		let path = tree.path().join("grants.toml");
		let declarations = [installed_extension(tree.path(), &["env.fs.read", "env.exec"])];
		let settings = grant_settings(&path, Vec::new());
		let (_, requests, _) = admit_extension_specs(&declarations, Some(&settings), &[], &[]);
		let grant = Grant {
			granted_at: sf!("now"),
			granted_by: sf!("interactive"),
			..requests[0].grant.clone()
		};
		GrantsFile::persist(&path, grant).expect("persist grant");
		let (admitted, pending, warnings) =
			admit_extension_specs(&declarations, Some(&settings), &[], &[]);
		assert_eq!(admitted.len(), 1, "{warnings:?}");
		assert!(pending.is_empty());
		assert!(warnings.is_empty());
	}

	fn resource_subscription(
		event: HookEventId,
		phase: omp_agent::HookPhase,
		id: u32,
	) -> omp_agent::Subscription {
		omp_agent::Subscription {
			host: sf!("test"),
			source: omp_agent::SourceRef {
				layer:        0,
				publisher:    sf!("test"),
				extension_id: sf!("resource-hooks"),
			},
			id,
			event,
			phase,
			order: 0,
			on_failure: omp_agent::OnFailure::Deny,
			when: omp_agent::When::default(),
		}
	}

	fn write_skill(path: &Path, name: &str) {
		fs::create_dir_all(path.parent().expect("skill parent")).expect("skill directory");
		fs::write(
			path,
			format!("---\nname: {name}\ndescription: '{name} skill.'\n---\n\n# {name}\n"),
		)
		.expect("skill");
	}

	#[tokio::test]
	async fn resources_discover_appends_skill_and_intersects_existing_resources() {
		let tree = tempfile::tempdir().expect("tree");
		let repo = tree.path().join("repo");
		fs::create_dir_all(&repo).expect("repository");
		let keep = tree.path().join("existing/keep/SKILL.md");
		let drop_path = tree.path().join("existing/drop/SKILL.md");
		let added = tree.path().join("added/new/SKILL.md");
		write_skill(&keep, "keep");
		write_skill(&drop_path, "drop");
		write_skill(&added, "added");
		let contain_root = tree.path().canonicalize().expect("contain root");
		let before =
			active_content_snapshots_with_skill_contributions(&repo, &sf!("fixture.static"), &[
				omp_envd::exthost::dispatch::SkillPathContribution {
					path:         keep.canonicalize().expect("keep path"),
					contain_root: contain_root.clone(),
				},
				omp_envd::exthost::dispatch::SkillPathContribution {
					path:         drop_path.canonicalize().expect("drop path"),
					contain_root: contain_root.clone(),
				},
			]);
		let keep_uri = Str::from(
			keep
				.canonicalize()
				.expect("keep path")
				.to_string_lossy()
				.as_ref(),
		);
		let added_uri = Str::from(
			added
				.canonicalize()
				.expect("added path")
				.to_string_lossy()
				.as_ref(),
		);
		let (gate, dispatches) = HookGate::channel();
		gate
			.subscribe("test", [resource_subscription(
				HookEventId::HookEventResourcesDiscover,
				omp_agent::HookPhase::Transform,
				35,
			)])
			.expect("resource subscription");
		let gate = Arc::new(gate);
		let responder_gate = Arc::clone(&gate);
		let responder_keep = keep_uri.clone();
		let responder_added = added_uri.clone();
		let responder = tokio::spawn(async move {
			let dispatch = dispatches.recv_async().await.expect("resource dispatch");
			assert_eq!(dispatch.event, HookEventId::HookEventResourcesDiscover);
			let separator = dispatch
				.payload
				.iter()
				.position(|byte| *byte == b'\n')
				.expect("payload separator");
			let mut payload: serde_json::Value =
				serde_json::from_slice(&dispatch.payload[separator + 1..]).expect("payload");
			assert!(
				payload["found"]
					.as_array()
					.is_some_and(|found| found.len() >= 2)
			);
			payload["add"] = serde_json::json!([{
				"uri": responder_added,
				"kind": "skill",
				"origin": "fixture.dynamic",
			}]);
			payload["keep"] = serde_json::json!([responder_keep, responder_added]);
			responder_gate
				.answer(dispatch.dispatch_id, vec![(
					35,
					omp_agent::GateDecision::Modify(omp_agent::HookPatch {
						target: None,
						args:   Some(Bytes::from(
							serde_json::to_vec(&payload).expect("effective payload"),
						)),
					}),
				)])
				.expect("resource decision");
		});
		let after = gate_resources_discover(
			&gate,
			DiscoverReason::Startup,
			&repo,
			&[contain_root],
			&PromptDiscoverySettings::default(),
			before,
		)
		.await
		.expect("resource discovery");
		responder.await.expect("resource responder");
		let names = after
			.skills
			.all()
			.iter()
			.map(|skill| skill.name.as_str())
			.collect::<BTreeSet<_>>();
		assert!(names.contains("keep"));
		assert!(names.contains("added"));
		assert!(!names.contains("drop"));
	}

	#[test]
	fn resources_changed_emits_one_frame_for_one_committed_refresh() {
		let tree = tempfile::tempdir().expect("tree");
		let repo = tree.path().join("repo");
		fs::create_dir_all(&repo).expect("repository");
		let first = tree.path().join("skills/first/SKILL.md");
		let second = tree.path().join("skills/second/SKILL.md");
		write_skill(&first, "first");
		write_skill(&second, "second");
		let contain_root = tree.path().canonicalize().expect("contain root");
		let contribution = |path: &Path| omp_envd::exthost::dispatch::SkillPathContribution {
			path:         path.canonicalize().expect("skill path"),
			contain_root: contain_root.clone(),
		};
		let before =
			active_content_snapshots_with_skill_contributions(&repo, &sf!("fixture.static"), &[
				contribution(&first),
			]);
		let after =
			active_content_snapshots_with_skill_contributions(&repo, &sf!("fixture.static"), &[
				contribution(&first),
				contribution(&second),
			]);
		let (gate, dispatches) = HookGate::channel();
		gate
			.subscribe("test", [resource_subscription(
				HookEventId::HookEventResourcesChanged,
				omp_agent::HookPhase::Observe,
				36,
			)])
			.expect("resource changed subscription");
		notify_resources_changed(&gate, DiscoverReason::Reload, &before, &after);
		let dispatch = dispatches.try_recv().expect("one resource changed frame");
		assert_eq!(dispatch.event, HookEventId::HookEventResourcesChanged);
		let separator = dispatch
			.payload
			.iter()
			.position(|byte| *byte == b'\n')
			.expect("payload separator");
		let payload: serde_json::Value =
			serde_json::from_slice(&dispatch.payload[separator + 1..]).expect("payload");
		assert_eq!(payload["added"].as_array().map(Vec::len), Some(1));
		assert_eq!(payload["removed"].as_array().map(Vec::len), Some(0));
		assert!(dispatches.try_recv().is_err(), "refresh emitted more than one frame");
	}
}
