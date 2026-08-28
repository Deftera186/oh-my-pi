//! Immutable prompt-input preparation at the application composition boundary.

use std::{
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use omp_agent::{
	ContextFile, EagerTaskPolicy, HostInfoInput, Journal, ModelPromptInput, MutationPromptInput,
	PromptCapabilitiesInput, PromptDelegationInput, PromptDeviceInput, PromptFacts,
	PromptNamedInput, PromptSchemeInput, PromptSettingsInput, PromptToolExampleInput,
	PromptToolInput, RepositoryInput,
};
use omp_core::{Hash32, Str};
use omp_env::{ClientError, EnvClient};
use omp_proto::{SCHEMA_REV, env::v1 as pb};
use omp_scribe::Props;
use omp_tool::Registry;
use thiserror::Error;
use tokio::{time, time::Instant};

use crate::{
	discovery::{
		ActiveContentSnapshots,
		context::{ContextSnapshot, prompt_files},
		project::{ProjectSnapshot, prompt_active_repository, prompt_repositories, prompt_trees},
	},
	rulebook,
	skills::prompt_inputs,
	workspace_roots::{WorkspaceRootDiagnostic, WorkspaceRootError, WorkspaceRootGuard},
};

pub mod settings;

const HOST_FIELD_BYTES: u32 = 4 * 1024;
const PROMPT_PREP_DEADLINE: Duration = Duration::from_millis(5_000);

/// Immutable policy-resolved registry catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryPromptInput {
	/// Stable digest of callable and dynamic-device declarations.
	pub digest:  Hash32,
	/// Callable declarations in deterministic wire-name order.
	pub tools:   Arc<[PromptToolInput]>,
	/// Mounted dynamic devices in deterministic name order.
	pub devices: Arc<[PromptDeviceInput]>,
}

/// Readability and mintability of one internal URL scheme.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemePromptInput {
	/// Scheme name without `://`.
	pub name:        Str,
	/// Whether prompt-advertised reads resolve.
	pub readable:    bool,
	/// Whether tools may mint links in this scheme.
	pub mintable:    bool,
	/// Whether read selectors are accepted.
	pub selectors:   bool,
	/// Live capability description.
	pub description: Str,
}

/// UI capabilities frozen for conditional prompt sections.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UiPromptInput {
	/// Computer-use device is mounted and granted.
	pub computer: bool,
	/// Image inspection is mounted and granted.
	pub images:   bool,
	/// Mermaid blocks can be rendered by the active UI.
	pub mermaid:  bool,
}

/// Delegation policy frozen for one turn.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DelegationPromptInput {
	/// Delegation is mounted and granted.
	pub enabled:         bool,
	/// Live child concurrency ceiling; zero means unlimited.
	pub concurrency:     u32,
	/// Number of requests already queued.
	pub queued:          u32,
	/// Whether a read-only scout is available.
	pub scout_available: bool,
	/// Eager delegation policy.
	pub eager:           EagerTaskPolicy,
	/// Whether one task call accepts a batch.
	pub batch:           bool,
	/// Whether peer coordination is available.
	pub coordination:    bool,
	/// Whether Codex-specific delegation wording applies.
	pub codex:           bool,
}

/// Structured preparation diagnostic retained with the snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptDiagnostic {
	/// Journal and Environment root authorities drifted.
	WorkspaceRoot(WorkspaceRootDiagnostic),
	/// Repository facts were unavailable for this root.
	RepositoryUnavailable(Str),
	/// A bounded preparation source was omitted.
	SourceUnavailable(Str),
}

/// Complete immutable input used by typed prompt rendering for one turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptSnapshot {
	/// Workspace, root, host, repository, model, capability, and settings facts.
	pub workspace:   PromptFacts,
	/// Policy-resolved tool registry identities.
	pub registry:    RegistryPromptInput,
	/// Internal URL scheme capabilities.
	pub schemes:     Arc<[SchemePromptInput]>,
	/// UI capabilities.
	pub ui:          UiPromptInput,
	/// Delegation policy and live queue facts.
	pub delegation:  DelegationPromptInput,
	/// Ordered standing rules.
	pub rules:       Arc<[PromptNamedInput]>,
	/// Ordered enabled skills.
	pub skills:      Arc<[PromptNamedInput]>,
	/// Structured preparation diagnostics.
	pub diagnostics: Arc<[PromptDiagnostic]>,
}
impl PromptSnapshot {
	/// Builds the immutable template property bag beside the typed prompt
	/// pipeline.
	///
	/// Optional and empty-suppressed values are absent. Prompt-source paragraphs
	/// share one canonical deduplication set in precedence order: custom
	/// prompt, append prompt, context files, then rules.
	pub fn props(&self) -> Props {
		self
			.workspace
			.props()
			.expect("frozen prompt tool metadata must be UTF-8")
	}

	/// Freezes every caller-owned input and derives the capability projection
	/// from the same exact selected registry and scheme snapshots.
	#[allow(
		clippy::too_many_arguments,
		reason = "the constructor names the required immutable prompt snapshot facets"
	)]
	pub fn freeze(
		mut workspace: PromptFacts,
		registry: &Registry,
		selected_tools: Option<&[Str]>,
		schemes: impl Into<Arc<[SchemePromptInput]>>,
		ui: UiPromptInput,
		delegation: DelegationPromptInput,
		mutations: MutationPromptInput,
		rules: impl Into<Arc<[PromptNamedInput]>>,
		skills: impl Into<Arc<[PromptNamedInput]>>,
		diagnostics: impl Into<Arc<[PromptDiagnostic]>>,
	) -> Self {
		let registry = freeze_registry(registry, selected_tools);
		let schemes = schemes.into();
		let rules: Arc<[PromptNamedInput]> = rules.into();
		let mut rules = rules.to_vec();
		let available = |name: &str| {
			registry.tools.iter().any(|tool| tool.name.as_str() == name)
				|| registry
					.devices
					.iter()
					.any(|device| device.name.as_str() == name)
		};
		if let Some(guidance) =
			omp_agent::standing_guidance(available("manage_skill"), available("learn"))
		{
			rules.push(guidance);
		}
		let rules: Arc<[PromptNamedInput]> = rules.into();
		let skills = skills.into();
		workspace.model.codex_task_policy |= delegation.codex;
		workspace.settings.render_mermaid &= ui.mermaid;
		workspace.rules = Arc::clone(&rules);
		workspace.skills = Arc::clone(&skills);
		let prompt_schemes = schemes
			.iter()
			.map(|scheme| PromptSchemeInput {
				name:        scheme.name.clone(),
				readable:    scheme.readable,
				mintable:    scheme.mintable,
				selectors:   scheme.selectors,
				description: scheme.description.clone(),
			})
			.collect::<Vec<_>>()
			.into();
		let has_xd =
			registry.tools.iter().any(|tool| tool.name == "shell") && !registry.devices.is_empty();
		let has_auto_qa = registry
			.devices
			.iter()
			.any(|device| device.name == "report_issue");
		workspace.capabilities = PromptCapabilitiesInput {
			registry_revision: u64::from_le_bytes(
				registry.digest.as_bytes()[..8]
					.try_into()
					.expect("eight digest bytes"),
			),
			tools: Arc::clone(&registry.tools),
			devices: Arc::clone(&registry.devices),
			schemes: prompt_schemes,
			computer: ui.computer,
			delegation: PromptDelegationInput {
				enabled:         delegation.enabled,
				eager:           delegation.eager,
				batch:           delegation.batch,
				concurrency:     delegation.concurrency,
				queued:          delegation.queued,
				scout_available: delegation.scout_available,
				coordination:    delegation.coordination,
			},
			mutations,
			device_guidance: has_xd.then(|| Str::new(omp_tools::device::PROMPT_GUIDANCE)),
			auto_qa_guidance: has_auto_qa
				.then(|| Str::new(omp_tools::device::AUTO_QA_PROMPT_GUIDANCE)),
		};
		Self {
			workspace,
			registry,
			schemes,
			ui,
			delegation,
			rules,
			skills,
			diagnostics: diagnostics.into(),
		}
	}
}

/// Environment-owned inputs consumed before the immutable snapshot is built.
pub struct EnvironmentPromptInputs {
	/// Bounded workstation facts.
	pub host:        HostInfoInput,
	/// Reconciled root provenance and revision.
	pub roots:       omp_agent::WorkspaceRootsInput,
	/// Structured root drift.
	pub diagnostics: Arc<[PromptDiagnostic]>,
}

/// Environment retrieval or root reconciliation failure.
#[derive(Debug, Error)]
pub enum PromptPrepError {
	/// Environment request failed.
	#[error(transparent)]
	Environment(#[from] ClientError),
	/// Environment root facts were invalid or journal projection failed.
	#[error(transparent)]
	WorkspaceRoots(#[from] WorkspaceRootError),
}

/// Retrieves host and ordered root facts concurrently, then intersects the
/// root grants with append-only journal truth.
pub async fn prepare_environment_inputs(
	env: &EnvClient,
	journal: &Journal,
	primary: &Path,
) -> Result<EnvironmentPromptInputs, PromptPrepError> {
	let (host, roots) = tokio::try_join!(
		env.host_info(pb::HostInfoRequest {
			wire_revision:   SCHEMA_REV,
			max_field_bytes: HOST_FIELD_BYTES,
		}),
		env.workspace_roots(pb::WorkspaceRootSetRequest { wire_revision: SCHEMA_REV }),
	)?;
	let roots = WorkspaceRootGuard::from_environment(roots)?.snapshot(journal, primary)?;
	let diagnostics = roots
		.diagnostics
		.iter()
		.cloned()
		.map(PromptDiagnostic::WorkspaceRoot)
		.collect::<Vec<_>>()
		.into();
	Ok(EnvironmentPromptInputs { host: host.into(), roots: roots.roots, diagnostics })
}

/// Retrieves environment prompt facets concurrently under the shared
/// five-second preparation deadline.
///
/// Timed-out requests are detached rather than aborted so Environment-owned
/// caches may warm for the next snapshot. The current snapshot is frozen with
/// deterministic empty fallbacks and structured diagnostics.
pub async fn prepare_environment_inputs_bounded(
	env: &EnvClient,
	journal: &Journal,
	primary: &Path,
) -> EnvironmentPromptInputs {
	let host_env = env.clone();
	let roots_env = env.clone();
	let mut host_task = tokio::spawn(async move {
		host_env
			.host_info(pb::HostInfoRequest {
				wire_revision:   SCHEMA_REV,
				max_field_bytes: HOST_FIELD_BYTES,
			})
			.await
	});
	let mut roots_task = tokio::spawn(async move {
		roots_env
			.workspace_roots(pb::WorkspaceRootSetRequest { wire_revision: SCHEMA_REV })
			.await
	});
	let deadline = Instant::now() + PROMPT_PREP_DEADLINE;
	let mut diagnostics = Vec::new();

	let host = match time::timeout_at(deadline, &mut host_task).await {
		Ok(Ok(Ok(host))) => host.into(),
		Ok(Ok(Err(_))) | Ok(Err(_)) => {
			warn_prep_fallback("host");
			diagnostics.push(PromptDiagnostic::SourceUnavailable(omp_core::sf!("host")));
			HostInfoInput::default()
		},
		Err(_) => {
			warn_prep_timeout("host");
			diagnostics.push(PromptDiagnostic::SourceUnavailable(omp_core::sf!("timeout:host")));
			HostInfoInput::default()
		},
	};
	let roots = match time::timeout_at(deadline, &mut roots_task).await {
		Ok(Ok(Ok(roots))) => match WorkspaceRootGuard::from_environment(roots)
			.and_then(|guard| guard.snapshot(journal, primary))
		{
			Ok(snapshot) => {
				diagnostics.extend(
					snapshot
						.diagnostics
						.iter()
						.cloned()
						.map(PromptDiagnostic::WorkspaceRoot),
				);
				snapshot.roots
			},
			Err(_) => {
				warn_prep_fallback("workspace-roots");
				diagnostics.push(PromptDiagnostic::SourceUnavailable(omp_core::sf!("workspace-roots")));
				Default::default()
			},
		},
		Ok(Ok(Err(_))) | Ok(Err(_)) => {
			warn_prep_fallback("workspace-roots");
			diagnostics.push(PromptDiagnostic::SourceUnavailable(omp_core::sf!("workspace-roots")));
			Default::default()
		},
		Err(_) => {
			warn_prep_timeout("workspace-roots");
			diagnostics
				.push(PromptDiagnostic::SourceUnavailable(omp_core::sf!("timeout:workspace-roots")));
			Default::default()
		},
	};
	EnvironmentPromptInputs { host, roots, diagnostics: diagnostics.into() }
}

fn warn_prep_timeout(step: &str) {
	tracing::warn!(
		step,
		timeout_ms = PROMPT_PREP_DEADLINE.as_millis() as u64,
		"system prompt preparation timed out; using minimal fallback"
	);
}

fn warn_prep_fallback(step: &str) {
	tracing::warn!(step, "system prompt preparation failed; using minimal fallback");
}

/// Creates the initial workspace input from already-frozen facets.
#[allow(clippy::too_many_arguments, reason = "the helper makes every PromptFacts facet explicit")]
pub fn workspace_input(
	cwd: impl Into<PathBuf>,
	context_files: impl Into<Arc<[ContextFile]>>,
	environment: EnvironmentPromptInputs,
	repositories: impl Into<Arc<[RepositoryInput]>>,
	model: ModelPromptInput,
	settings: PromptSettingsInput,
) -> PromptFacts {
	PromptFacts {
		cwd: cwd.into(),
		vcs: None,
		context_files: context_files.into(),
		roots: environment.roots,
		host: environment.host,
		repositories: repositories.into(),
		model,
		capabilities: PromptCapabilitiesInput::default(),
		settings,
		..Default::default()
	}
}

/// Applies already-frozen discovery facets to the immutable workspace input.
///
/// This projection performs no filesystem, process, registry, or model I/O.
pub fn apply_discovery_snapshots(
	workspace: &mut PromptFacts,
	context: &ContextSnapshot,
	project: &ProjectSnapshot,
	content: &ActiveContentSnapshots,
) {
	workspace.context_files = prompt_files(context);
	workspace.repositories = prompt_repositories(project);
	workspace.directory_context = Arc::clone(&project.directory_context);
	workspace.workspace_trees = prompt_trees(project);
	workspace.active_repository = prompt_active_repository(project);
	workspace.rules = rulebook::prompt_inputs(&content.rules);
	workspace.skills = prompt_inputs(&content.skills);
}

fn freeze_registry(registry: &Registry, selected_tools: Option<&[Str]>) -> RegistryPromptInput {
	let tools = registry
		.prompt_projection(selected_tools)
		.entries()
		.map(|tool| PromptToolInput {
			name:        tool.name.clone(),
			revision:    tool.revision.clone(),
			description: tool.description.clone(),
			schema:      tool.schema.clone(),
			examples:    tool
				.examples
				.iter()
				.map(|example| PromptToolExampleInput {
					label:     example.label.clone(),
					arguments: example.arguments.clone(),
				})
				.collect::<Vec<_>>()
				.into(),
			docs:        tool.docs.map(Str::new),
		})
		.collect::<Vec<_>>();
	let devices = registry
		.devices()
		.map(|device| PromptDeviceInput {
			name:        device.name.clone(),
			revision:    device.rev.clone(),
			description: device.summary.clone(),
		})
		.collect::<Vec<_>>();
	let mut hasher = Hash32::hasher();
	let mut hash_field = |bytes: &[u8]| {
		hasher.update((bytes.len() as u64).to_le_bytes());
		hasher.update(bytes);
	};
	for tool in &tools {
		hash_field(tool.name.as_bytes());
		hash_field(&[0]);
		hash_field(tool.revision.family.as_bytes());
		hash_field(&tool.revision.n.to_le_bytes());
		hash_field(tool.description.as_bytes());
		hash_field(&tool.schema);
		for example in tool.examples.iter() {
			if let Some(label) = &example.label {
				hash_field(label.as_bytes());
			}
			hash_field(&example.arguments);
		}
		if let Some(docs) = &tool.docs {
			hash_field(docs.as_bytes());
		}
	}
	for device in &devices {
		hash_field(device.name.as_bytes());
		hash_field(&[1]);
		hash_field(device.revision.family.as_bytes());
		hash_field(&device.revision.n.to_le_bytes());
		hash_field(device.description.as_bytes());
	}
	drop(hash_field);
	RegistryPromptInput {
		digest:  hasher.finalize(),
		tools:   tools.into(),
		devices: devices.into(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn prompt_snapshot_freezes_every_input_facet() {
		let workspace = PromptFacts {
			cwd: "/workspace".into(),
			model: ModelPromptInput {
				identifier:        Str::from("provider/model"),
				codex_task_policy: true,
			},
			settings: PromptSettingsInput::default(),
			..Default::default()
		};
		let schemes: Arc<[SchemePromptInput]> = vec![SchemePromptInput {
			name:        Str::from("artifact"),
			readable:    true,
			mintable:    true,
			selectors:   true,
			description: Str::from("durable artifact"),
		}]
		.into();
		let rules: Arc<[PromptNamedInput]> = vec![PromptNamedInput {
			id:      Str::from("rule"),
			origin:  Str::from("rule://rule"),
			content: Str::from("frozen"),
		}]
		.into();
		let snapshot = PromptSnapshot::freeze(
			workspace,
			&Registry::new(),
			None,
			Arc::clone(&schemes),
			UiPromptInput { computer: true, images: true, mermaid: true },
			DelegationPromptInput {
				enabled:         true,
				concurrency:     4,
				queued:          1,
				scout_available: true,
				eager:           EagerTaskPolicy::Preferred,
				batch:           true,
				coordination:    true,
				codex:           true,
			},
			MutationPromptInput { format_on_write: true, ..Default::default() },
			Arc::clone(&rules),
			Arc::<[PromptNamedInput]>::from([]),
			Arc::<[PromptDiagnostic]>::from([]),
		);
		drop(schemes);
		drop(rules);

		assert_eq!(snapshot.workspace.model.identifier, "provider/model");
		assert_eq!(snapshot.workspace.capabilities.schemes[0].name, "artifact");
		assert!(snapshot.workspace.capabilities.schemes[0].readable);
		assert!(snapshot.workspace.capabilities.computer);
		assert!(snapshot.workspace.capabilities.delegation.enabled);
		assert_eq!(snapshot.workspace.capabilities.delegation.eager, EagerTaskPolicy::Preferred);
		assert_eq!(snapshot.rules[0].content, "frozen");
		assert_eq!(snapshot.delegation.queued, 1);
		assert_eq!(snapshot.clone(), snapshot);
		let props = snapshot.props();
		assert_eq!(props.get("cwd").and_then(omp_scribe::Value::as_str), Some("/workspace"));
		assert_eq!(
			props.get("model"),
			Some(&omp_scribe::map! {
				"identifier" => "provider/model".to_owned(),
				"codex_task_policy" => true,
			})
		);
		assert!(props.get("custom_prompt").is_none());
	}
}
