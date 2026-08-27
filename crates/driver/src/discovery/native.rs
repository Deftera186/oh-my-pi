//! Native OMP filesystem discovery with explicit, bounded ancestor walks.

use std::{
	collections::{BTreeMap, BTreeSet},
	env, fs,
	io::Read,
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_core::Str;
use omp_ext::lock::{InstalledRecord, LockFile};
use omp_walker::WalkRequest;
use serde::Deserialize;

use super::{
	containment::contained_existing,
	manifest::{
		CapabilityPayload, ContextPayload, DiscoveredCapability, ExtensionGrantFacts,
		ExtensionPayload, HookPayload, HookPhase, InstructionPayload, PromptPayload,
		PythonWorkerDeclaration, SettingsPayload, SourceProvenance, SourceScope, SystemPromptPayload,
		ToolHandlerDeclaration, ToolPayload,
	},
	mcp_ssh::{parse_mcp_file, parse_ssh_file},
	packages::{self, ExtensionRootMode},
	rules::{self, RuleSource},
	skills::{self, SkillDiscoverySettings, SkillSource},
	slash_commands,
};

/// A native OMP configuration root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigRoot {
	/// Root directory containing the configuration.
	pub path:     PathBuf,
	/// `true` for a user-home root, `false` for a project root.
	pub user:     bool,
	/// Stable native precedence (project before user).
	pub priority: u8,
}

/// Returns only native `.omp` roots eligible for config/model/settings loads.
/// Foreign roots are intentionally excluded from this authority.
pub fn config_roots(cwd: &Path, home: &Path, max_depth: usize) -> Vec<ConfigRoot> {
	let mut roots = Vec::new();
	let mut current = cwd;
	for _ in 0..=max_depth {
		let path = current.join(".omp");
		if directory_has_entries(&path) {
			roots.push(ConfigRoot { path, user: false, priority: 2 });
			break;
		}
		let Some(parent) = current.parent() else {
			break;
		};
		if parent == current || current == home {
			break;
		}
		current = parent;
	}
	let user = user_config_root(home);
	if user.is_dir() {
		roots.push(ConfigRoot { path: user, user: true, priority: 1 });
	}
	roots
}

/// A read-only foreign repository content root. It is never eligible for
/// settings, models, keybindings, commands, plugins, or MCP decoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForeignContentRoot {
	/// Foreign family label retained as provenance for content assets.
	pub label: &'static str,
	/// Existing repository-local content directory.
	pub path:  PathBuf,
}

/// Finds labeled foreign repository content roots for discovery owners that
/// consume prompt/instruction assets. This does not inspect user-home roots.
pub fn foreign_content_roots(cwd: &Path, home: &Path, max_depth: usize) -> Vec<ForeignContentRoot> {
	let mut roots = Vec::new();
	let mut current = cwd;
	for _ in 0..=max_depth {
		for (name, label) in
			[(".claude", "claude-content"), (".codex", "codex-content"), (".gemini", "gemini-content")]
		{
			let path = current.join(name);
			if path.is_dir() {
				roots.push(ForeignContentRoot { label, path });
			}
		}
		if current == home {
			break;
		}
		let Some(parent) = current.parent() else {
			break;
		};
		if parent == current {
			break;
		}
		current = parent;
	}
	roots
}

/// Native configuration roots ordered from highest to lowest precedence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRoots {
	/// Profile-scoped user agent directory.
	pub user:    PathBuf,
	/// Nearest-first `.omp` directories between the cwd and filesystem root.
	pub project: Vec<PathBuf>,
	/// Nearest-first standalone instruction files.
	pub agents:  Vec<PathBuf>,
}

/// Resolves the native user config root. `OMP_PROFILE` scopes profiles without
/// changing the project `.omp` convention.
pub fn user_config_root(home: &Path) -> PathBuf {
	let base = env::var_os("OMP_CONFIG_DIR")
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
		.unwrap_or_else(|| home.join(".omp"));
	let profile = omp_core::dirs::selected_profile()
		.map(str::to_owned)
		.or_else(|| {
			env::var("OMP_PROFILE")
				.ok()
				.filter(|profile| !profile.is_empty())
		});
	match profile {
		Some(profile) => base.join("profiles").join(profile).join("agent"),
		None => base.join("agent"),
	}
}

/// Collects native `.omp` and standalone `AGENTS.md` walk-ups. The cap is an
/// I/O bound as well as a cycle guard for malformed synthetic test paths.
pub fn discover_roots(cwd: &Path, home: &Path, max_depth: usize) -> NativeRoots {
	let mut project = Vec::new();
	let mut agents = Vec::new();
	let mut native_owner_found = false;
	let mut current = cwd;
	for _ in 0..=max_depth {
		let omp = current.join(".omp");
		if !native_owner_found && directory_has_entries(&omp) {
			project.push(omp);
			native_owner_found = true;
		}
		let agents_file = current.join("AGENTS.md");
		if agents_file.is_file() {
			agents.push(agents_file);
		}
		if current == home {
			break;
		}
		let Some(parent) = current.parent() else {
			break;
		};
		if parent == current {
			break;
		}
		current = parent;
	}
	NativeRoots { user: user_config_root(home), project, agents }
}

fn directory_has_entries(path: &Path) -> bool {
	fs::read_dir(path)
		.ok()
		.and_then(|mut entries| entries.next())
		.is_some()
}

/// Scans one capability directory without recursive imports, hidden entries,
/// or ignored files. `omp-walker` owns full gitignore semantics.
pub fn scan_capability_dir(root: &Path) -> Vec<PathBuf> {
	WalkRequest::new(root)
		.hidden(false)
		.gitignore(true)
		.skip_git(true)
		.depth(1, 1)
		.collect_files()
		.unwrap_or_default()
		.into_iter()
		.map(|entry| entry.absolute_path(root))
		.collect()
}

/// Explicit native-root composition policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NativeRootMode {
	/// Explicit roots precede normally discovered project/user roots.
	#[default]
	Merge,
	/// Only explicit roots participate.
	ExplicitOnly,
}

/// Native static capability discovery options.
#[derive(Clone, Debug)]
pub struct NativeDiscoveryOptions {
	/// Ordered explicit `.omp`/agent roots.
	pub explicit_roots:     Vec<PathBuf>,
	/// Explicit-root merge behavior.
	pub root_mode:          NativeRootMode,
	/// Skill source, name, and custom-directory policy for every native root.
	pub skill_settings:     SkillDiscoverySettings,
	/// Whether implicit project roots and standalone project files participate.
	pub include_workspace:  bool,
	/// Authoritative selected-profile installed extension record.
	pub client_installed:   Option<PathBuf>,
	/// Canonical workspace identity used only for workspace-layer grant keys.
	pub workspace_identity: Option<omp_ext::WorkspaceUri>,
}
impl Default for NativeDiscoveryOptions {
	fn default() -> Self {
		Self {
			explicit_roots:     Vec::new(),
			root_mode:          NativeRootMode::Merge,
			skill_settings:     SkillDiscoverySettings::default(),
			include_workspace:  true,
			client_installed:   None,
			workspace_identity: None,
		}
	}
}

/// Complete native static provider output.
#[derive(Clone, Debug, Default)]
pub struct NativeDiscovery {
	/// Typed declarations. Discovery never executes tool or hook paths.
	pub declarations: Vec<DiscoveredCapability>,
	/// Bounded non-fatal source diagnostics.
	pub warnings:     Vec<Str>,
}

/// Discovers the canonical native content surface from realpath-deduplicated
/// roots. Roots are ordered explicit, nearest project, then user unless
/// explicit-only mode is selected.
pub fn discover_capabilities(
	cwd: &Path,
	home: &Path,
	max_depth: usize,
	options: &NativeDiscoveryOptions,
) -> NativeDiscovery {
	let discovered = discover_roots(cwd, home, max_depth);
	let standalone_agents = discovered.agents.clone();
	let mut roots = options
		.explicit_roots
		.iter()
		.map(|path| (path.clone(), SourceScope::Project))
		.collect::<Vec<_>>();
	if options.root_mode == NativeRootMode::Merge {
		if options.include_workspace {
			roots.extend(
				discovered
					.project
					.into_iter()
					.map(|path| (path, SourceScope::Project)),
			);
		}
		roots.push((discovered.user, SourceScope::User));
	}
	let mut seen = BTreeSet::new();
	roots.retain_mut(|(path, _)| {
		let canonical = fs::canonicalize(&*path).unwrap_or_else(|_| path.clone());
		*path = canonical.clone();
		canonical.is_dir() && seen.insert(canonical)
	});

	let mut output = NativeDiscovery::default();
	let custom_skills = skills::discover(&[], &options.skill_settings);
	output.declarations.extend(custom_skills.declarations);
	output.warnings.extend(
		custom_skills
			.warnings
			.into_iter()
			.map(|warning| warning.message),
	);
	let mut root_skill_settings = options.skill_settings.clone();
	root_skill_settings.custom_directories.clear();
	let mut install_records = roots
		.iter()
		.map(|(root, _)| root.join("installed.toml"))
		.collect::<Vec<_>>();
	if let Some(installed) = &options.client_installed
		&& !install_records.contains(installed)
	{
		install_records.push(installed.clone());
	}
	for (root, scope) in roots {
		load_root(&root, scope, &root_skill_settings, None, &mut output);
	}
	let mut extension_roots = BTreeSet::new();
	for installed_path in install_records {
		let installed = match InstalledRecord::read(&installed_path) {
			Ok(installed) => installed,
			Err(error) => {
				output.warnings.push(Str::from(format!(
					"ignored native extension record {}: {error}",
					installed_path.display()
				)));
				continue;
			},
		};
		let layer = if options.client_installed.as_ref() == Some(&installed_path) {
			omp_ext::Layer::Client
		} else {
			omp_ext::Layer::Workspace
		};
		let lock = LockFile::read(&installed_path.with_file_name("omp.lock"), layer).ok();
		let packages = packages::discover(&installed, &[], ExtensionRootMode::Merge);
		output.warnings.extend(packages.warnings);
		for extension in packages.roots {
			if extension_roots.insert(extension.path.clone()) {
				let grant = lock
					.as_ref()
					.and_then(|lock| {
						lock
							.extensions
							.iter()
							.find(|locked| locked.id == extension.id)
					})
					.map(|locked| {
						Arc::new(ExtensionGrantFacts {
							id: locked.id.clone(),
							publisher: locked.publisher.clone(),
							layer,
							workspace: (layer == omp_ext::Layer::Workspace)
								.then(|| options.workspace_identity.clone())
								.flatten(),
							capability_digest: locked.capability_digest.clone(),
							tier: locked.tier,
							ship: locked.ship.clone(),
						})
					})
					.or_else(|| {
						installed
							.extensions
							.iter()
							.find(|installed| {
								installed.id == extension.id
									&& installed
										.source
										.as_table()
										.is_some_and(|source| source.contains_key("link"))
							})
							.map(|_| {
								let publisher = Str::from(format!(
									"unsigned:link:{}",
									omp_core::Hash32::sum(extension.path.to_string_lossy().as_bytes())
										.to_hex()
								));
								Arc::new(ExtensionGrantFacts {
									id: extension.id.clone(),
									publisher,
									layer,
									workspace: (layer == omp_ext::Layer::Workspace)
										.then(|| options.workspace_identity.clone())
										.flatten(),
									capability_digest: Str::new_static("unsigned-link"),
									tier: omp_ext::TrustTier::Sandboxed,
									ship: Str::new_static("link"),
								})
							})
					});
				load_root(
					&extension.path,
					SourceScope::Package,
					&root_skill_settings,
					grant,
					&mut output,
				);
			}
		}
	}
	for path in standalone_agents
		.into_iter()
		.filter(|_| options.root_mode == NativeRootMode::Merge && options.include_workspace)
	{
		let Ok(content) = fs::read_to_string(&path) else {
			continue;
		};
		if content.trim().is_empty() {
			continue;
		}
		let key = Str::from(path.to_string_lossy().as_ref());
		output.declarations.push(DiscoveredCapability::keyed(
			key,
			CapabilityPayload::ContextFiles(ContextPayload {
				path:    path.clone(),
				content: Str::from(content),
				depth:   None,
			}),
			SourceProvenance::native("native-project-context", path, SourceScope::Project),
		));
	}
	if options.root_mode == NativeRootMode::Merge && options.include_workspace {
		let mut current = cwd;
		for _ in 0..=max_depth {
			load_standalone(current, &mut output);
			if current == home {
				break;
			}
			let Some(parent) = current.parent() else {
				break;
			};
			if parent == current {
				break;
			}
			current = parent;
		}
	}
	output
}

fn load_standalone(root: &Path, output: &mut NativeDiscovery) {
	for filename in ["mcp.json", ".mcp.json"] {
		let path = root.join(filename);
		if !path.is_file() {
			continue;
		}
		match parse_mcp_file(&path, None) {
			Ok(servers) => output
				.declarations
				.extend(servers.into_iter().map(|server| {
					let key = server.name.clone();
					DiscoveredCapability::keyed(
						key,
						CapabilityPayload::Mcps(server),
						SourceProvenance::native(
							"native-project-root",
							path.clone(),
							SourceScope::Project,
						),
					)
				})),
			Err(_) => output
				.warnings
				.push(Str::from(format!("failed to load {}", path.display()))),
		}
	}
	let path = root.join("ssh.json");
	if path.is_file() {
		match parse_ssh_file(&path, None) {
			Ok(hosts) => output.declarations.extend(hosts.into_iter().map(|host| {
				let key = host.name.clone();
				DiscoveredCapability::keyed(
					key,
					CapabilityPayload::Ssh(host),
					SourceProvenance::native("native-project-root", path.clone(), SourceScope::Project),
				)
			})),
			Err(_) => output
				.warnings
				.push(Str::from(format!("failed to load {}", path.display()))),
		}
	}
}

fn load_root(
	root: &Path,
	scope: SourceScope,
	skill_settings: &SkillDiscoverySettings,
	grant: Option<Arc<ExtensionGrantFacts>>,
	output: &mut NativeDiscovery,
) {
	let source_id = match scope {
		SourceScope::Project => Str::from("native-project"),
		SourceScope::User => Str::from("native-user"),
		_ => Str::from("native"),
	};
	let skill_result = skills::discover(
		&[SkillSource {
			id: source_id.clone(),
			root: root.join("skills"),
			scope,
			include_root: false,
			require_description: true,
			contain_root: None,
			read_only: false,
			kind: skills::SkillSourceKind::Native,
		}],
		skill_settings,
	);
	output.declarations.extend(skill_result.declarations);
	output.warnings.extend(
		skill_result
			.warnings
			.into_iter()
			.map(|warning| warning.message),
	);

	let mut rule_sources =
		vec![RuleSource { id: source_id.clone(), root: root.join("rules"), scope, read_only: false }];
	if root.join("RULES.md").is_file() {
		rule_sources.push(RuleSource {
			id: source_id.clone(),
			root: root.join("RULES.md"),
			scope,
			read_only: false,
		});
	}
	let rule_result = rules::discover(&rule_sources);
	output.declarations.extend(rule_result.declarations);
	output.warnings.extend(
		rule_result
			.warnings
			.into_iter()
			.map(|warning| warning.message),
	);

	for (directory, kind) in [
		("prompts", CapabilityFileKind::Prompt),
		("instructions", CapabilityFileKind::Instruction),
		("commands", CapabilityFileKind::Command),
	] {
		load_markdown_dir(root, directory, kind, source_id.clone(), scope, output);
	}
	load_hooks(root, source_id.clone(), scope, output);
	load_tools(root, source_id.clone(), scope, output);
	load_settings(root, source_id.clone(), scope, output);
	load_extension(root, source_id.clone(), scope, grant, output);

	for filename in ["mcp.json", ".mcp.json"] {
		let path = root.join(filename);
		if !path.is_file() {
			continue;
		}
		match parse_mcp_file(&path, None) {
			Ok(servers) => output
				.declarations
				.extend(servers.into_iter().map(|server| {
					let key = server.name.clone();
					DiscoveredCapability::keyed(
						key,
						CapabilityPayload::Mcps(server),
						SourceProvenance::native(source_id.clone(), path.clone(), scope),
					)
				})),
			Err(_) => output
				.warnings
				.push(Str::from(format!("failed to load {}", path.display()))),
		}
	}
	let ssh = root.join("ssh.json");
	if ssh.is_file() {
		match parse_ssh_file(&ssh, None) {
			Ok(hosts) => output.declarations.extend(hosts.into_iter().map(|host| {
				let key = host.name.clone();
				DiscoveredCapability::keyed(
					key,
					CapabilityPayload::Ssh(host),
					SourceProvenance::native(source_id.clone(), ssh.clone(), scope),
				)
			})),
			Err(_) => output
				.warnings
				.push(Str::from(format!("failed to load {}", ssh.display()))),
		}
	}
	for filename in ["SYSTEM.md", "AGENTS.md"] {
		let path = root.join(filename);
		let Ok(content) = fs::read_to_string(&path) else {
			continue;
		};
		if content.trim().is_empty() {
			continue;
		}
		let payload = if filename == "SYSTEM.md" {
			CapabilityPayload::SystemPrompt(SystemPromptPayload {
				path:    path.clone(),
				content: Str::from(content),
			})
		} else {
			CapabilityPayload::ContextFiles(ContextPayload {
				path:    path.clone(),
				content: Str::from(content),
				depth:   None,
			})
		};
		output.declarations.push(DiscoveredCapability::keyed(
			filename,
			payload,
			SourceProvenance::native(source_id.clone(), path, scope),
		));
	}
}

#[derive(Clone, Copy)]
enum CapabilityFileKind {
	Prompt,
	Instruction,
	Command,
}

fn load_markdown_dir(
	root: &Path,
	directory: &str,
	kind: CapabilityFileKind,
	source_id: Str,
	scope: SourceScope,
	output: &mut NativeDiscovery,
) {
	let capability_root = root.join(directory);
	let paths = if matches!(kind, CapabilityFileKind::Command) {
		scan_command_dir(&capability_root)
	} else {
		scan_capability_dir(&capability_root)
	};
	for path in paths.into_iter().filter(|path| {
		path
			.extension()
			.is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
	}) {
		let Ok(content) = fs::read_to_string(&path) else {
			continue;
		};
		let name = if matches!(kind, CapabilityFileKind::Command) {
			let Some(name) = command_name(&capability_root, &path) else {
				output
					.warnings
					.push(Str::from(format!("ignored invalid command path {}", path.display())));
				continue;
			};
			name
		} else {
			Str::from(
				path
					.file_stem()
					.and_then(|name| name.to_str())
					.unwrap_or("declaration"),
			)
		};
		let payload = match kind {
			CapabilityFileKind::Prompt => CapabilityPayload::Prompts(PromptPayload {
				name:    name.clone(),
				path:    path.clone(),
				content: Str::from(content),
			}),
			CapabilityFileKind::Instruction => CapabilityPayload::Instructions(InstructionPayload {
				name:     name.clone(),
				path:     path.clone(),
				content:  Str::from(content),
				apply_to: None,
			}),
			CapabilityFileKind::Command => {
				let command = match slash_commands::parse_markdown(name.clone(), path.clone(), &content)
				{
					Ok(command) => command,
					Err(error) => {
						output
							.warnings
							.push(Str::from(format!("ignored /{name} from {}: {error}", path.display(),)));
						continue;
					},
				};
				CapabilityPayload::SlashCommands(command)
			},
		};
		output.declarations.push(DiscoveredCapability::keyed(
			name,
			payload,
			SourceProvenance::native(source_id.clone(), path, scope),
		));
	}
}

fn scan_command_dir(root: &Path) -> Vec<PathBuf> {
	WalkRequest::new(root)
		.hidden(false)
		.gitignore(true)
		.skip_git(true)
		.depth(1, 16)
		.limit(1_024)
		.collect_files()
		.unwrap_or_default()
		.into_iter()
		.map(|entry| entry.absolute_path(root))
		.filter(|path| contained_existing(root, path).is_ok())
		.collect()
}

fn command_name(root: &Path, path: &Path) -> Option<Str> {
	let relative = path.strip_prefix(root).ok()?;
	let count = relative.components().count();
	let mut name = String::new();
	for (index, component) in relative.components().enumerate() {
		let component = component.as_os_str().to_str()?;
		let component = if index + 1 == count {
			component.strip_suffix(".md")?
		} else {
			component
		};
		if component.is_empty() || component.starts_with('.') || component.contains(['/', '\\', ':'])
		{
			return None;
		}
		if !name.is_empty() {
			name.push(':');
		}
		name.push_str(component);
	}
	(!name.is_empty()).then(|| Str::from(name))
}

fn load_hooks(root: &Path, source_id: Str, scope: SourceScope, output: &mut NativeDiscovery) {
	for (directory, phase) in [("pre", HookPhase::Pre), ("post", HookPhase::Post)] {
		for path in scan_capability_dir(&root.join("hooks").join(directory)) {
			let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
				continue;
			};
			let tool = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
			let payload = HookPayload {
				name: Str::from(filename),
				path: path.clone(),
				phase,
				tool: Str::from(tool),
			};
			output.declarations.push(DiscoveredCapability::keyed(
				format!("{directory}:{filename}"),
				CapabilityPayload::Hooks(payload),
				SourceProvenance::native(source_id.clone(), path, scope),
			));
		}
	}
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolHeader {
	name:         Option<String>,
	description:  Option<String>,
	input_schema: Option<serde_json::Value>,
}

fn script_tool_header(path: &Path) -> ToolHeader {
	const HEADER_BYTES: u64 = 4096;
	let mut source = String::new();
	let Some(mut file) = fs::File::open(path)
		.ok()
		.map(|file| file.take(HEADER_BYTES))
	else {
		return ToolHeader::default();
	};
	if file.read_to_string(&mut source).is_err() {
		return ToolHeader::default();
	}
	let Some(start) = source.find("/**").map(|start| start.saturating_add(3)) else {
		return ToolHeader::default();
	};
	let Some(end) = source[start..]
		.find("*/")
		.map(|end| end.saturating_add(start))
	else {
		return ToolHeader::default();
	};
	let description = source[start..end]
		.lines()
		.map(|line| line.trim().trim_start_matches('*').trim())
		.filter(|line| !line.is_empty() && !line.to_ascii_lowercase().starts_with("symlink:"))
		.collect::<Vec<_>>()
		.join("\n");
	ToolHeader {
		description: (!description.is_empty()).then_some(description),
		..ToolHeader::default()
	}
}

fn load_tools(root: &Path, source_id: Str, scope: SourceScope, output: &mut NativeDiscovery) {
	for path in scan_capability_dir(&root.join("tools")) {
		let extension = path
			.extension()
			.and_then(|ext| ext.to_str())
			.unwrap_or_default();
		if !matches!(extension, "json" | "md" | "py" | "sh" | "bash" | "js" | "ts") {
			continue;
		}
		let fallback = path
			.file_stem()
			.and_then(|name| name.to_str())
			.unwrap_or("tool")
			.to_owned();
		let header = match extension {
			"json" => fs::read_to_string(&path)
				.ok()
				.and_then(|source| serde_json::from_str::<ToolHeader>(&source).ok())
				.unwrap_or_default(),
			"md" => fs::read_to_string(&path)
				.ok()
				.and_then(|source| {
					let rest = source.strip_prefix("---\n")?;
					let (header, _) = rest.split_once("\n---\n")?;
					serde_yaml::from_str::<ToolHeader>(header).ok()
				})
				.unwrap_or_default(),
			_ => script_tool_header(&path),
		};
		let name = header
			.name
			.as_deref()
			.map(str::trim)
			.filter(|value| !value.is_empty())
			.unwrap_or(fallback.as_str());
		let name = Str::from(name);
		let description = header
			.description
			.filter(|value| !value.trim().is_empty())
			.unwrap_or_else(|| format!("{name} custom tool"));
		let payload = ToolPayload {
			name:         name.clone(),
			path:         path.clone(),
			description:  Str::from(description),
			input_schema: header
				.input_schema
				.unwrap_or_else(|| serde_json::json!({"type":"object","additionalProperties":true})),
			handler:      ToolHandlerDeclaration::Process {
				program: path.clone(),
				args:    Vec::new(),
			},
		};
		output.declarations.push(DiscoveredCapability::keyed(
			name,
			CapabilityPayload::Tools(payload),
			SourceProvenance::native(source_id.clone(), path, scope),
		));
	}
}

fn load_settings(root: &Path, source_id: Str, scope: SourceScope, output: &mut NativeDiscovery) {
	for filename in ["settings.toml", "config.toml"] {
		let path = root.join(filename);
		let Some(table) = fs::read_to_string(&path)
			.ok()
			.and_then(|source| toml::from_str::<toml::Table>(&source).ok())
		else {
			continue;
		};
		output.declarations.push(DiscoveredCapability::unkeyed(
			CapabilityPayload::Settings(SettingsPayload { path: path.clone(), data: table }),
			SourceProvenance::native(source_id.clone(), path, scope),
		));
	}
}

#[derive(Default, Deserialize)]
struct ExtensionManifest {
	name:              Option<String>,
	description:       Option<String>,
	worker:            Option<PythonWorkerDeclaration>,
	#[serde(default)]
	cli:               Vec<omp_ext::config::CliContribution>,
	#[serde(default)]
	selected_features: Box<[Str]>,
	#[serde(flatten)]
	extra:             BTreeMap<Str, serde_json::Value>,
}

fn load_extension(
	root: &Path,
	source_id: Str,
	scope: SourceScope,
	grant: Option<Arc<ExtensionGrantFacts>>,
	output: &mut NativeDiscovery,
) {
	let legacy_path = root.join("extension.json");
	let deployment_path = root.join("omp.toml");
	let (path, manifest) = if let Some(manifest) = fs::read_to_string(&legacy_path)
		.ok()
		.and_then(|source| serde_json::from_str::<ExtensionManifest>(&source).ok())
	{
		(legacy_path, manifest)
	} else if let Some(manifest) = fs::read_to_string(&deployment_path)
		.ok()
		.and_then(|source| omp_ext::config::DeploymentManifest::parse(&source).ok())
		.filter(|manifest| manifest.validate().is_ok())
	{
		let mut extra = BTreeMap::new();
		extra.insert(
			Str::new_static("declarations"),
			serde_json::to_value(&manifest.declarations).unwrap_or_default(),
		);
		extra.insert(
			Str::new_static("settings"),
			serde_json::to_value(&manifest.settings).unwrap_or_default(),
		);
		(deployment_path, ExtensionManifest {
			name: Some(manifest.id.to_string()),
			description: None,
			worker: Some(PythonWorkerDeclaration { module: manifest.entry.clone(), entry: None }),
			cli: Vec::new(),
			selected_features: Box::new([]),
			extra,
		})
	} else {
		return;
	};
	let name = manifest.name.as_deref().unwrap_or_else(|| {
		root
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("extension")
	});
	let payload = ExtensionPayload {
		name: Str::from(name),
		root: root.to_path_buf(),
		description: manifest.description.map(Str::from),
		worker: manifest.worker,
		cli: manifest.cli,
		selected_features: manifest.selected_features,
		grant,
		manifest: manifest.extra,
	};
	output.declarations.push(DiscoveredCapability::keyed(
		name,
		CapabilityPayload::Extensions(payload),
		SourceProvenance::native(source_id, path, scope),
	));
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;
	#[test]
	fn config_roots_exclude_foreign_families() {
		let tree = tempfile::tempdir().expect("tree");
		let home = tree.path().join("home");
		let cwd = tree.path().join("repo/work");
		fs::create_dir_all(cwd.join(".claude")).expect("project");
		fs::create_dir_all(home.join(".omp/agent")).expect("user");
		let roots = config_roots(&cwd, &home, 3);
		assert_eq!(roots, vec![ConfigRoot {
			path:     home.join(".omp/agent"),
			user:     true,
			priority: 1,
		}]);
		assert_eq!(foreign_content_roots(&cwd, &home, 3), vec![ForeignContentRoot {
			label: "claude-content",
			path:  cwd.join(".claude"),
		}],);
	}
	#[test]
	fn nearest_non_empty_native_root_owns_project_discovery() {
		let tree = tempfile::tempdir().expect("tree");
		let root = tree.path();
		let cwd = root.join("a/b/c");
		fs::create_dir_all(cwd.join(".omp")).expect("nested");
		fs::create_dir_all(root.join("a/.omp")).expect("parent");
		fs::write(cwd.join(".omp/AGENTS.md"), "nearest native").expect("nearest native");
		fs::write(root.join("a/.omp/AGENTS.md"), "far native").expect("far native");
		fs::write(root.join("a/AGENTS.md"), "parent").expect("agents");
		let roots = discover_roots(&cwd, root, 2);
		assert_eq!(roots.project, vec![cwd.join(".omp")]);
		assert_eq!(roots.agents, vec![root.join("a/AGENTS.md")]);
	}
	#[test]
	fn scan_respects_gitignore_and_is_non_recursive() {
		let tree = tempfile::tempdir().expect("tree");
		let root = tree.path();
		fs::write(root.join(".gitignore"), "ignored.md\n").expect("ignore");
		fs::write(root.join("kept.md"), "x").expect("kept");
		fs::write(root.join("ignored.md"), "x").expect("ignored");
		fs::create_dir(root.join("nested")).expect("nested");
		fs::write(root.join("nested/child.md"), "x").expect("child");
		assert_eq!(scan_capability_dir(root), vec![root.join("kept.md")]);
	}
	#[test]
	fn nested_command_paths_become_colon_names_and_parse_failures_warn() {
		let tree = tempfile::tempdir().expect("tree");
		let root = tree.path().join(".omp");
		fs::create_dir_all(root.join("commands/git")).expect("commands");
		fs::write(
			root.join("commands/git/commit.md"),
			"---\ndescription: Commit staged changes\n---\nReview and commit $ARGUMENTS",
		)
		.expect("command");
		fs::write(root.join("commands/broken.md"), "---\ndescription: broken").expect("broken");
		let mut output = NativeDiscovery::default();
		load_root(&root, SourceScope::Project, &SkillDiscoverySettings::default(), None, &mut output);
		assert!(output.declarations.iter().any(|declaration| {
			matches!(
				&declaration.payload,
				CapabilityPayload::SlashCommands(command) if command.name == "git:commit"
			)
		}));
		assert!(
			output
				.warnings
				.iter()
				.any(|warning| warning.contains("/broken"))
		);
	}
	#[test]
	fn script_tool_header_keeps_jsdoc_and_drops_symlink_footnotes() {
		let tree = tempfile::tempdir().expect("tree");
		let path = tree.path().join("bundle.ts");
		fs::write(
			&path,
			"/**\n * Inspect and control system services.\n *\n * Symlink: ~/.omp/tools/bundle.ts\n \
			 */\nexport default {};\n",
		)
		.expect("tool");
		let header = script_tool_header(&path);
		assert_eq!(header.description.as_deref(), Some("Inspect and control system services."));
	}
}
