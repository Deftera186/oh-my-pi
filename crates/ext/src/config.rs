//! Layered extension configuration and environment overrides.

use std::{
	collections::{BTreeMap, BTreeSet},
	env,
	path::{Path, PathBuf},
};

use omp_core::{Str, sf};
use serde::{Deserialize, Serialize, de};

use super::{ExtensionCode, ExtensionError, Layer};

/// The ordered configuration scopes used for extension precedence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub enum Scope {
	/// The operator's client configuration.
	#[default]
	Client,
	/// The workspace's configuration, applied after the client scope.
	Workspace,
}

impl Scope {
	/// Returns the corresponding extension layer.
	pub const fn layer(self) -> Layer {
		match self {
			Self::Client => Layer::Client,
			Self::Workspace => Layer::Workspace,
		}
	}
}

/// Static extension CLI value shape.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CliValueKind {
	/// Presence-only flag.
	Boolean,
	/// Required string value.
	String,
	/// Optional string value; bare presence yields `true` at the sink.
	OptionalString,
}

/// One typed value delivered to an extension activation sink.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContributedValue {
	/// Presence-only value.
	Boolean(bool),
	/// String value.
	String(Str),
}

/// A declaration-linked contributed value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContributedCliValue {
	/// Qualified extension owner.
	pub owner: Str,
	/// Declared sink key.
	pub sink:  Str,
	/// Parsed typed value.
	pub value: ContributedValue,
}

/// Declaration-checked sink key exposed only to the owning extension.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CliValueSink {
	/// Stable key used by the extension activation payload.
	pub key: Str,
}

/// One static extension CLI contribution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CliContribution {
	/// TOFU-qualified publisher name.
	pub publisher:      Str,
	/// Extension id within the publisher namespace.
	pub extension:      Str,
	/// Long flag spelling without leading dashes.
	pub name:           Str,
	/// Human-readable help text.
	pub description:    Str,
	/// Typed value shape.
	pub kind:           CliValueKind,
	/// Optional typed default represented in manifest JSON.
	#[serde(default)]
	pub default:        Option<serde_json::Value>,
	/// Explicit operator-approved built-in shadow declaration.
	#[serde(default)]
	pub shadow_builtin: bool,
	/// Owning extension's activation sink.
	pub sink:           CliValueSink,
}

impl CliContribution {
	/// Publisher-qualified declaration identity.
	pub fn qualified_name(&self) -> Str {
		Str::from(format!("{}/{}:--{}", self.publisher, self.extension, self.name))
	}

	/// Validates static syntax and default type.
	pub fn validate(&self) -> Result<(), CliCollision> {
		if !qualified_component(&self.publisher)
			|| !qualified_component(&self.extension)
			|| !flag_name(&self.name)
			|| self.sink.key.is_empty()
		{
			return Err(CliCollision::Invalid(self.qualified_name()));
		}
		let valid_default = match (&self.kind, &self.default) {
			(_, None) => true,
			(CliValueKind::Boolean, Some(value)) => value.is_boolean(),
			(CliValueKind::String | CliValueKind::OptionalString, Some(value)) => value.is_string(),
		};
		if !valid_default {
			return Err(CliCollision::InvalidDefault(self.qualified_name()));
		}
		Ok(())
	}
}

/// Deterministic contribution collision diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CliCollision {
	/// Static contribution syntax is invalid.
	#[error("invalid extension CLI contribution `{0}`")]
	Invalid(Str),
	/// A typed default does not match the contribution kind.
	#[error("invalid default for extension CLI contribution `{0}`")]
	InvalidDefault(Str),
	/// Two extensions own one spelling.
	#[error("extension CLI flag `--{name}` is declared by both {first} and {second}")]
	Duplicate {
		/// Colliding long name.
		name:   Str,
		/// First qualified owner.
		first:  Str,
		/// Second qualified owner.
		second: Str,
	},
	/// A built-in collision lacked an explicit shadow declaration.
	#[error("extension CLI flag `{owner}` collides with built-in `--{name}` without shadow_builtin")]
	Builtin {
		/// Colliding long name.
		name:  Str,
		/// Qualified owner.
		owner: Str,
	},
}

/// Validated, name-sorted final contribution set.
#[derive(Clone, Debug, Default)]
pub struct CliContributionSet {
	entries: BTreeMap<Str, CliContribution>,
}

impl CliContributionSet {
	/// Validates declarations and configured built-in shadow precedence.
	pub fn build(
		contributions: impl IntoIterator<Item = CliContribution>,
		builtins: impl IntoIterator<Item = Str>,
	) -> Result<Self, CliCollision> {
		let builtins = builtins.into_iter().collect::<BTreeSet<_>>();
		let mut entries = BTreeMap::new();
		for contribution in contributions {
			contribution.validate()?;
			let owner = contribution.qualified_name();
			if builtins.contains(&contribution.name) && !contribution.shadow_builtin {
				return Err(CliCollision::Builtin { name: contribution.name, owner });
			}
			if let Some(first) = entries.insert(contribution.name.clone(), contribution.clone()) {
				return Err(CliCollision::Duplicate {
					name:   contribution.name,
					first:  first.qualified_name(),
					second: owner,
				});
			}
		}
		Ok(Self { entries })
	}

	/// Returns one contribution by long name.
	pub fn get(&self, name: &str) -> Option<&CliContribution> {
		self.entries.get(name)
	}

	/// Iterates in stable long-name order.
	pub fn iter(&self) -> impl ExactSizeIterator<Item = (&Str, &CliContribution)> {
		self.entries.iter()
	}
}

fn qualified_component(value: &str) -> bool {
	!value.is_empty()
		&& value
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn flag_name(value: &str) -> bool {
	qualified_component(value) && !value.starts_with('-')
}
/// A source specification accepted by extension discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceSpec {
	/// An omp extension index distribution.
	Index {
		/// Explicit index URL, empty when configured indexes select it.
		index:        String,
		/// Distribution name resolved from that index.
		distribution: Str,
	},
	/// A `PyPI` distribution.
	Pypi {
		/// Distribution name resolved through `PyPI`.
		distribution: Str,
	},
	/// A commit-pinned Git source.
	Git {
		/// Canonical Git repository URL.
		repository:   String,
		/// Immutable commit or annotated tag.
		revision:     Str,
		/// Optional contained repository subdirectory.
		subdirectory: Option<PathBuf>,
	},
	/// A local development source.
	Path(PathBuf),
	/// A hash-addressed archive URL.
	Url {
		/// HTTPS artifact URL.
		url:    String,
		/// Required SHA-256 digest.
		sha256: Str,
	},
}

impl SourceSpec {
	/// Parses the explicit source grammar. `link` is deliberately absent: links
	/// are local install-record overlays and can never be resolution sources.
	pub fn parse(value: &str) -> Result<Self, ExtensionError> {
		let (kind, rest) = value.split_once(':').ok_or_else(|| {
			ExtensionError::new(
				ExtensionCode::ENoManifest,
				"source must use index:, pypi:, git:, path:, or url:",
			)
		})?;
		match kind {
			"index" if !rest.is_empty() => {
				let (index, distribution) = rest.rsplit_once('/').unwrap_or(("", rest));
				Ok(Self::Index { index: index.to_owned(), distribution: Str::new(distribution) })
			},
			"pypi" if !rest.is_empty() => Ok(Self::Pypi { distribution: Str::new(rest) }),
			"git" => {
				let (source, subdirectory) = rest
					.split_once('#')
					.map_or((rest, None), |(source, subdirectory)| {
						(source, Some(PathBuf::from(subdirectory)))
					});
				let (repository, revision) = source.rsplit_once('@').ok_or_else(|| {
					ExtensionError::new(
						ExtensionCode::EGitFloating,
						"git source must name a commit or annotated tag",
					)
				})?;
				let pinned_commit = matches!(revision.len(), 40 | 64)
					&& revision.bytes().all(|byte| byte.is_ascii_hexdigit());
				let explicit_tag =
					revision.starts_with("refs/tags/") && revision.len() > "refs/tags/".len();
				if !pinned_commit && !explicit_tag {
					return Err(ExtensionError::new(
						ExtensionCode::EGitFloating,
						"git source revision must be a full commit or explicit refs/tags name",
					));
				}
				if subdirectory.as_ref().is_some_and(|path| {
					path.as_os_str().is_empty()
						|| path.is_absolute()
						|| path.components().any(|component| {
							matches!(
								component,
								std::path::Component::ParentDir
									| std::path::Component::RootDir
									| std::path::Component::Prefix(_)
							)
						})
				}) {
					return Err(ExtensionError::new(
						ExtensionCode::EIntegrity,
						"git subdirectory must be a contained relative path",
					));
				}
				Ok(Self::Git {
					repository: repository.to_owned(),
					revision: Str::new(revision),
					subdirectory,
				})
			},
			"path" if !rest.is_empty() => Ok(Self::Path(PathBuf::from(rest))),
			"url" if rest.starts_with("https://") => {
				let (url, sha256) = rest.rsplit_once("#sha256=").ok_or_else(|| {
					ExtensionError::new(
						ExtensionCode::EIntegrity,
						"url source must end with #sha256=<digest>",
					)
				})?;
				if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
					return Err(ExtensionError::new(
						ExtensionCode::EIntegrity,
						"url source has an invalid SHA-256 digest",
					));
				}
				Ok(Self::Url { url: url.to_owned(), sha256: Str::new(sha256) })
			},
			"link" => Err(ExtensionError::new(
				ExtensionCode::ELockLink,
				"link is an installed.toml development overlay, not a source",
			)),
			_ => Err(ExtensionError::new(ExtensionCode::ENoManifest, "unknown extension source")),
		}
	}
}

/// The `[extensions]` table for one precedence scope.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ExtensionOverlay {
	/// Extension ids enabled by this scope.
	#[serde(default)]
	pub enabled:  BTreeSet<Str>,
	/// Extension ids disabled by this scope; this is the negative P7 input.
	#[serde(default)]
	pub disabled: BTreeSet<Str>,
	/// Workspace-only replacement declarations.
	#[serde(default)]
	pub replace:  BTreeSet<Str>,
	/// Feature selections replacing the install-record feature selection.
	#[serde(default)]
	pub features: BTreeMap<Str, Vec<Str>>,
	/// Scalar, non-secret settings delivered to extensions.
	#[serde(default)]
	pub settings: BTreeMap<Str, BTreeMap<Str, toml::Value>>,
}

impl ExtensionOverlay {
	/// Validates scope-only and secret-handling invariants before the overlay is
	/// used.
	pub fn validate(&self, scope: Scope) -> Result<(), ExtensionError> {
		if scope == Scope::Client && !self.replace.is_empty() {
			return Err(ExtensionError::new(
				ExtensionCode::EReplaceScope,
				"[extensions].replace is workspace-only",
			));
		}
		for (extension, settings) in &self.settings {
			for (key, value) in settings {
				if !value.is_str() && !value.is_integer() && !value.is_float() && !value.is_bool() {
					return Err(ExtensionError::new(
						ExtensionCode::ESettingSecret,
						format!("{extension}.{key} is not a scalar setting"),
					));
				}
				if matches!(key.as_str(), "secret" | "password" | "token" | "api_key" | "key") {
					return Err(ExtensionError::new(
						ExtensionCode::ESettingSecret,
						format!("{extension}.{key} belongs in omp.creds"),
					));
				}
			}
		}
		Ok(())
	}
}

/// A parsed configuration scope and its P1/P2 position.
#[derive(Clone, Debug, Default)]
pub struct ScopedOverlay {
	/// Scope identity.
	pub scope:   Scope,
	/// Parsed overlay.
	pub overlay: ExtensionOverlay,
}

/// The result of applying P1–P7 to a specific extension id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectiveExtensionConfig {
	/// Whether P7 disabled the extension in any scope.
	pub disabled:         bool,
	/// Whether the latest non-negative scope enabled the extension.
	pub enabled:          bool,
	/// Latest feature selection, replacing rather than merging.
	pub features:         Vec<Str>,
	/// Later scalar settings override earlier settings.
	pub settings:         BTreeMap<Str, toml::Value>,
	/// Workspace replacement was explicitly declared.
	pub replace_declared: bool,
}

/// Folds ordered client then workspace overlays. P7 is represented directly as
/// the `disabled` accumulator so no caller can accidentally implement a
/// first-wins exception.
pub fn fold_extension(scopes: &[ScopedOverlay], id: &Str) -> EffectiveExtensionConfig {
	let mut result = EffectiveExtensionConfig::default();
	for scope in scopes {
		let overlay = &scope.overlay;
		result.disabled |= overlay.disabled.contains(id);
		if overlay.enabled.contains(id) {
			result.enabled = true;
		}
		if let Some(features) = overlay.features.get(id) {
			result.features.clone_from(features);
		}
		if let Some(settings) = overlay.settings.get(id) {
			for (key, value) in settings {
				result.settings.insert(key.clone(), value.clone());
			}
		}
		result.replace_declared |= scope.scope == Scope::Workspace && overlay.replace.contains(id);
	}
	if result.disabled {
		result.enabled = false;
	}
	result
}

/// Parses supported extension environment variables before CLI flag wiring.
#[derive(Clone, Debug, Default)]
pub struct ExtensionEnvironment {
	/// Content-addressed store root.
	pub store:         Option<PathBuf>,
	/// Artifact cache root.
	pub cache:         Option<PathBuf>,
	/// Ordered configured indexes.
	pub indexes:       Vec<String>,
	/// Index public-key path.
	pub index_keys:    Option<PathBuf>,
	/// Offline mode; `strict` also fails closed on stale revocations.
	pub offline:       OfflineMode,
	/// Lock mutation refusal.
	pub locked:        bool,
	/// R9 resolution clamp.
	pub exclude_newer: Option<Str>,
	/// Emergency negative admission set.
	pub disabled:      BTreeSet<Str>,
	/// Suppresses the workspace layer entirely.
	pub no_workspace:  bool,
	/// Noninteractive grants.
	pub grant:         Option<String>,
	/// Build allowance for path/git only.
	pub allow_build:   bool,
	/// Publisher signing key.
	pub sign_key:      Option<PathBuf>,
	/// `uv` executable.
	pub uv:            Option<PathBuf>,
	/// Target triples.
	pub targets:       Vec<Str>,
	/// Diagnostic resolution trace.
	pub trace:         bool,
	/// Ambient one-entry Python site override, reported as `W-SITE-OVERRIDE`.
	pub site_override: Option<PathBuf>,
	/// Per-host environment socket.
	pub env_socket:    Option<PathBuf>,
}

/// Offline policy derived from `OMP_EXT_OFFLINE`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OfflineMode {
	/// Network access is permitted.
	#[default]
	Online,
	/// Network is prohibited but stale revocation lists warn and proceed.
	Offline,
	/// Network is prohibited and stale revocation lists are refused.
	Strict,
}

impl ExtensionEnvironment {
	/// Reads the `OMP_EXT_*` configuration surface. Flag equivalence is wired by
	/// `ExtCli`; this type deliberately has no CLI dependency.
	pub fn from_environment() -> Self {
		let value = |name| env::var(name).ok().filter(|value| !value.is_empty());
		let comma = |name| {
			value(name).map_or_else(Vec::new, |value| {
				value
					.split(',')
					.filter(|entry| !entry.is_empty())
					.map(Str::new)
					.collect()
			})
		};
		let bool_value = |name| matches!(value(name).as_deref(), Some("1" | "true"));
		Self {
			store:         value("OMP_EXT_STORE").map(PathBuf::from),
			cache:         value("OMP_EXT_CACHE").map(PathBuf::from),
			indexes:       value("OMP_EXT_INDEX").map_or_else(Vec::new, |value| {
				value
					.split(',')
					.filter(|entry| !entry.is_empty())
					.map(str::to_owned)
					.collect()
			}),
			index_keys:    value("OMP_EXT_INDEX_KEYS").map(PathBuf::from),
			offline:       match value("OMP_EXT_OFFLINE").as_deref() {
				Some("strict") => OfflineMode::Strict,
				Some(_) => OfflineMode::Offline,
				None => OfflineMode::Online,
			},
			locked:        bool_value("OMP_EXT_LOCKED"),
			exclude_newer: value("OMP_EXT_EXCLUDE_NEWER").map(Str::new),
			disabled:      comma("OMP_EXT_DISABLE").into_iter().collect(),
			no_workspace:  bool_value("OMP_EXT_NO_WORKSPACE"),
			grant:         value("OMP_EXT_GRANT"),
			allow_build:   bool_value("OMP_EXT_ALLOW_BUILD"),
			sign_key:      value("OMP_EXT_SIGN_KEY").map(PathBuf::from),
			uv:            value("OMP_EXT_UV").map(PathBuf::from),
			targets:       comma("OMP_EXT_TARGETS"),
			trace:         bool_value("OMP_EXT_TRACE"),
			env_socket:    value("OMP_EXT_ENV_SOCKET").map(PathBuf::from),
			site_override: value("OMP_PY_SITE").map(PathBuf::from),
		}
	}

	/// Returns the diagnostic emitted when an ambient site override bypasses
	/// managed per-host site-tree selection.
	pub const fn site_override_warning(&self) -> Option<ExtensionCode> {
		if self.site_override.is_some() {
			Some(ExtensionCode::WSiteOverride)
		} else {
			None
		}
	}
}
/// Static discovery locations for one layer, ordered per P2.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmbientPaths {
	/// Directories containing manifests, in discovery order.
	pub manifest_roots:  Vec<PathBuf>,
	/// Config overlays, in discovery order.
	pub config_files:    Vec<PathBuf>,
	/// Local install records, in discovery order.
	pub install_records: Vec<PathBuf>,
	/// Compatibility roots that are reported but never loaded.
	pub foreign_roots:   Vec<PathBuf>,
}

/// Builds ambient discovery paths. Workspace paths are included on the
/// workspace side; callers do not invoke this for a remote workspace on the
/// client. Compatibility roots are diagnostic-only (`W-FOREIGN-ROOT`).
pub fn ambient_paths(data_dir: &Path, workspace: Option<&Path>) -> AmbientPaths {
	let mut paths = AmbientPaths {
		manifest_roots:  Vec::new(),
		config_files:    vec![data_dir.join("config.toml")],
		install_records: vec![data_dir.join("ext/installed.toml")],
		foreign_roots:   Vec::new(),
	};
	if let Some(workspace) = workspace {
		let root = workspace.join(".omp");
		paths.manifest_roots.push(root.join("extensions"));
		paths.config_files.push(root.join("config.toml"));
		paths.install_records.push(root.join("installed.toml"));
		for name in [".claude", ".codex", ".gemini"] {
			paths.foreign_roots.push(workspace.join(name));
		}
	}
	paths
}

/// Outcome of the P4 workspace replacement gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplacementDecision {
	/// The workspace instance is the sole active instance for this id.
	Replace,
	/// The client instance remains active and the workspace instance is omitted.
	Denied(ExtensionCode),
	/// No workspace replacement was requested.
	NotRequested,
}

/// Applies P4's declaration, publisher-match, and policy gates. A denial is
/// deterministic: callers retain or re-admit the client instance rather than
/// allowing both instances to coexist.
pub fn workspace_replacement(
	replace_declared: bool,
	client_publisher: &Str,
	workspace_publisher: &Str,
	policy_permits: bool,
) -> ReplacementDecision {
	if !replace_declared {
		return ReplacementDecision::NotRequested;
	}
	if client_publisher != workspace_publisher || !policy_permits {
		return ReplacementDecision::Denied(ExtensionCode::WReplaceDenied);
	}
	ReplacementDecision::Replace
}

/// The authoring intent of one `[[tools]]` entry.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Deserialize,
	Serialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum ToolIntent {
	/// Catalog-routed tool declaration.
	#[default]
	Soft,
	/// Model-slot-claiming tool declaration gated by `tools.hard`.
	Hard,
}

/// One source `[[tools]]` manifest entry.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ToolManifestEntry {
	/// Stable declaration id.
	pub id:     Str,
	/// Tool intent; defaults to soft.
	#[serde(default, rename = "kind")]
	pub intent: ToolIntent,
	/// Module imported when the tool activates.
	pub module: Str,
	/// Static route key.
	pub key:    Str,
	/// Required API level.
	pub api:    u32,
}

/// Uniform declaration consumed by static catalogs and lazy activation.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Declaration {
	/// Stable declaration id.
	pub id:      Str,
	/// Closed declaration kind (`soft` or `hard` for this lowering).
	pub kind:    ToolIntent,
	/// Module imported on activation.
	pub module:  Str,
	/// Static route key.
	pub key:     Str,
	/// Tools always activate lazily from their static declarations.
	pub trigger: Str,
	/// Required OMP API level.
	pub api:     u32,
}

/// Lowers authoring `[[tools]]` entries into the static declaration table.
pub fn lower_tools(tools: impl IntoIterator<Item = ToolManifestEntry>) -> Vec<Declaration> {
	tools
		.into_iter()
		.map(|tool| Declaration {
			id:      tool.id,
			kind:    tool.intent,
			module:  tool.module,
			key:     tool.key,
			trigger: sf!("lazy"),
			api:     tool.api,
		})
		.collect()
}

/// One sealed extension declaration retained before executable code is loaded.
///
/// The common routing fields are typed while class-specific signed properties
/// remain available verbatim. Permission is granted by membership in the
/// containing declaration table, never by a runtime callback.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct StaticDeclaration {
	/// Stable identity within its declaration class.
	#[serde(default)]
	pub id:         Str,
	/// Closed declaration kind from the deployment manifest.
	#[serde(default)]
	pub kind:       Str,
	/// Package-contained module that implements the declaration.
	#[serde(default)]
	pub module:     Str,
	/// Static activation trigger.
	#[serde(default)]
	pub trigger:    Str,
	/// Static class-specific route key.
	#[serde(default)]
	pub key:        Str,
	/// Required OMP API revision.
	#[serde(default)]
	pub api:        u32,
	/// Unavailability behavior fixed by the manifest.
	#[serde(default)]
	pub failure:    Str,
	/// Deployment-granted capability names.
	#[serde(default)]
	pub grants:     Box<[Str]>,
	/// Class-specific signed declaration properties.
	#[serde(flatten)]
	pub properties: BTreeMap<Str, serde_json::Value>,
}

/// Sealed interactive UI declaration tables.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct UiDeclarations {
	/// Namespaced command declarations.
	#[serde(default)]
	pub commands:          Box<[StaticDeclaration]>,
	/// High-level shortcut declarations.
	#[serde(default)]
	pub shortcuts:         Box<[StaticDeclaration]>,
	/// Versioned message renderer declarations.
	#[serde(default)]
	pub message_renderers: Box<[StaticDeclaration]>,
	/// Versioned verdict renderer declarations.
	#[serde(default)]
	pub verdict_renderers: Box<[StaticDeclaration]>,
	/// Typed completion source declarations.
	#[serde(default)]
	pub completions:       Box<[StaticDeclaration]>,
}

/// Sealed telemetry declaration tables.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct TelemetryDeclarations {
	/// Event subscriptions visible to the extension.
	#[serde(default)]
	pub subscriptions: Box<[StaticDeclaration]>,
	/// Consent-gated telemetry export declarations.
	#[serde(default)]
	pub exports:       Box<[StaticDeclaration]>,
}

/// Every statically declared extension CONTROL surface.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct StaticDeclarations {
	/// Exact deployment capability grant payload, grouped by authority domain.
	#[serde(default, rename = "capabilities")]
	pub capability_grants: BTreeMap<Str, serde_json::Value>,
	/// Uniform sealed declaration rows in deployment order.
	#[serde(default, rename = "declarations")]
	pub ordered:           Box<[StaticDeclaration]>,
	/// Soft and hard tool declarations.
	#[serde(default)]
	pub tools:             Box<[StaticDeclaration]>,
	/// Hook declarations.
	#[serde(default)]
	pub hooks:             Box<[StaticDeclaration]>,
	/// Inter-extension service declarations.
	#[serde(default)]
	pub services:          Box<[StaticDeclaration]>,
	/// Inference provider catalog declarations.
	#[serde(default)]
	pub providers:         Box<[StaticDeclaration]>,
	/// Session and turn regime declarations.
	#[serde(default)]
	pub regimes:           Box<[StaticDeclaration]>,
	/// Interactive presentation declarations.
	#[serde(default)]
	pub ui:                UiDeclarations,
	/// Telemetry observation and export declarations.
	#[serde(default)]
	pub telemetry:         TelemetryDeclarations,
	/// Typed system-prompt slot contributions.
	#[serde(default)]
	pub prompt_slots:      Box<[StaticDeclaration]>,
	/// Opaque credential-source declarations.
	#[serde(default)]
	pub credentials:       Box<[StaticDeclaration]>,
	/// Secret transform and reference declarations.
	#[serde(default)]
	pub secrets:           Box<[StaticDeclaration]>,
	/// Supervised Python worker declarations.
	#[serde(default)]
	pub workers:           Box<[StaticDeclaration]>,
	/// Worker placement constraints and affinity declarations.
	#[serde(default)]
	pub placement:         Box<[StaticDeclaration]>,
}

/// Closed class identity used by declaration/runtime drift reports.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StaticDeclarationClass {
	/// Soft or hard tool.
	Tool,
	/// Hook.
	Hook,
	/// Inter-extension service.
	Service,
	/// Inference provider.
	Provider,
	/// Regime.
	Regime,
	/// UI command.
	UiCommand,
	/// UI shortcut.
	UiShortcut,
	/// UI message renderer.
	UiMessageRenderer,
	/// UI verdict renderer.
	UiVerdictRenderer,
	/// UI completion source.
	UiCompletion,
	/// Telemetry subscription.
	TelemetrySubscription,
	/// Telemetry exporter.
	TelemetryExport,
	/// Prompt slot.
	PromptSlot,
	/// Credential source.
	Credential,
	/// Secret declaration.
	Secret,
	/// Supervised worker.
	Worker,
	/// Placement rule.
	Placement,
}

/// Exact identities missing from or unexpectedly reported by a frozen runtime.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StaticDeclarationDrift {
	/// Manifest rows absent from the runtime report.
	pub missing:    Box<[(StaticDeclarationClass, Str)]>,
	/// Runtime rows absent from the authenticated manifest.
	pub unexpected: Box<[(StaticDeclarationClass, Str)]>,
}

impl StaticDeclarationDrift {
	/// Returns whether static and runtime identities agree exactly.
	pub fn is_empty(&self) -> bool {
		self.missing.is_empty() && self.unexpected.is_empty()
	}
}

impl StaticDeclarations {
	/// Parses declaration tables from authenticated manifest properties.
	pub fn from_properties(
		properties: &BTreeMap<Str, serde_json::Value>,
	) -> Result<Self, serde_json::Error> {
		let mut parsed = serde_json::from_value::<Self>(serde_json::to_value(properties)?)?;
		let mut tools = Vec::from(parsed.tools);
		let mut hooks = Vec::from(parsed.hooks);
		let mut services = Vec::from(parsed.services);
		let mut providers = Vec::from(parsed.providers);
		let mut regimes = Vec::from(parsed.regimes);
		let mut commands = Vec::from(parsed.ui.commands);
		let mut shortcuts = Vec::from(parsed.ui.shortcuts);
		let mut message_renderers = Vec::from(parsed.ui.message_renderers);
		let mut verdict_renderers = Vec::from(parsed.ui.verdict_renderers);
		let mut completions = Vec::from(parsed.ui.completions);
		let mut subscriptions = Vec::from(parsed.telemetry.subscriptions);
		let mut exports = Vec::from(parsed.telemetry.exports);
		let mut prompt_slots = Vec::from(parsed.prompt_slots);
		let mut credentials = Vec::from(parsed.credentials);
		let mut secrets = Vec::from(parsed.secrets);
		let mut workers = Vec::from(parsed.workers);
		let mut placement = Vec::from(parsed.placement);
		for row in &parsed.ordered {
			if !matches!(
				row.trigger.as_str(),
				"" | "static"
					| "lazy" | "first_reach"
					| "eager-prompt"
					| "before_first_prompt"
					| "eager-ui"
					| "before_ui_input"
			) {
				return Err(de::Error::custom(format!("unknown activation trigger `{}`", row.trigger)));
			}
			match row.kind.as_str() {
				"soft" | "hard" | "tool" => tools.push(row.clone()),
				"hook" => hooks.push(row.clone()),
				"service" => services.push(row.clone()),
				"provider" => providers.push(row.clone()),
				"regime" => regimes.push(row.clone()),
				"command" => commands.push(row.clone()),
				"shortcut" => shortcuts.push(row.clone()),
				"message_renderer" => message_renderers.push(row.clone()),
				"verdict_renderer" | "renderer" => verdict_renderers.push(row.clone()),
				"completion" => completions.push(row.clone()),
				"telemetry" | "telemetry_subscription" => subscriptions.push(row.clone()),
				"telemetry_export" => exports.push(row.clone()),
				"prompt_slot" => prompt_slots.push(row.clone()),
				"credential" => credentials.push(row.clone()),
				"secret" => secrets.push(row.clone()),
				"worker" => workers.push(row.clone()),
				"placement" => placement.push(row.clone()),
				"skills" | "rules" | "context-files" | "prompts" => {},
				kind => {
					return Err(de::Error::custom(format!("unknown static declaration kind `{kind}`")));
				},
			}
		}
		parsed.tools = tools.into_boxed_slice();
		parsed.hooks = hooks.into_boxed_slice();
		parsed.services = services.into_boxed_slice();
		parsed.providers = providers.into_boxed_slice();
		parsed.regimes = regimes.into_boxed_slice();
		parsed.ui.commands = commands.into_boxed_slice();
		parsed.ui.shortcuts = shortcuts.into_boxed_slice();
		parsed.ui.message_renderers = message_renderers.into_boxed_slice();
		parsed.ui.verdict_renderers = verdict_renderers.into_boxed_slice();
		parsed.ui.completions = completions.into_boxed_slice();
		parsed.telemetry.subscriptions = subscriptions.into_boxed_slice();
		parsed.telemetry.exports = exports.into_boxed_slice();
		parsed.prompt_slots = prompt_slots.into_boxed_slice();
		parsed.credentials = credentials.into_boxed_slice();
		parsed.secrets = secrets.into_boxed_slice();
		parsed.workers = workers.into_boxed_slice();
		parsed.placement = placement.into_boxed_slice();
		Ok(parsed)
	}

	/// Visits every declaration row without changing manifest order within a
	/// declaration class.
	pub fn rows(&self) -> impl Iterator<Item = &StaticDeclaration> {
		self
			.tools
			.iter()
			.chain(self.hooks.iter())
			.chain(self.services.iter())
			.chain(self.providers.iter())
			.chain(self.regimes.iter())
			.chain(self.ui.commands.iter())
			.chain(self.ui.shortcuts.iter())
			.chain(self.ui.message_renderers.iter())
			.chain(self.ui.verdict_renderers.iter())
			.chain(self.ui.completions.iter())
			.chain(self.telemetry.subscriptions.iter())
			.chain(self.telemetry.exports.iter())
			.chain(self.prompt_slots.iter())
			.chain(self.credentials.iter())
			.chain(self.secrets.iter())
			.chain(self.workers.iter())
			.chain(self.placement.iter())
	}

	/// Visits every identity with its closed declaration class.
	pub fn identities(&self) -> impl Iterator<Item = (StaticDeclarationClass, &Str)> {
		self
			.tools
			.iter()
			.map(|row| (StaticDeclarationClass::Tool, &row.id))
			.chain(
				self
					.hooks
					.iter()
					.map(|row| (StaticDeclarationClass::Hook, &row.id)),
			)
			.chain(
				self
					.services
					.iter()
					.map(|row| (StaticDeclarationClass::Service, &row.id)),
			)
			.chain(
				self
					.providers
					.iter()
					.map(|row| (StaticDeclarationClass::Provider, &row.id)),
			)
			.chain(
				self
					.regimes
					.iter()
					.map(|row| (StaticDeclarationClass::Regime, &row.id)),
			)
			.chain(
				self
					.ui
					.commands
					.iter()
					.map(|row| (StaticDeclarationClass::UiCommand, &row.id)),
			)
			.chain(
				self
					.ui
					.shortcuts
					.iter()
					.map(|row| (StaticDeclarationClass::UiShortcut, &row.id)),
			)
			.chain(
				self
					.ui
					.message_renderers
					.iter()
					.map(|row| (StaticDeclarationClass::UiMessageRenderer, &row.id)),
			)
			.chain(
				self
					.ui
					.verdict_renderers
					.iter()
					.map(|row| (StaticDeclarationClass::UiVerdictRenderer, &row.id)),
			)
			.chain(
				self
					.ui
					.completions
					.iter()
					.map(|row| (StaticDeclarationClass::UiCompletion, &row.id)),
			)
			.chain(
				self
					.telemetry
					.subscriptions
					.iter()
					.map(|row| (StaticDeclarationClass::TelemetrySubscription, &row.id)),
			)
			.chain(
				self
					.telemetry
					.exports
					.iter()
					.map(|row| (StaticDeclarationClass::TelemetryExport, &row.id)),
			)
			.chain(
				self
					.prompt_slots
					.iter()
					.map(|row| (StaticDeclarationClass::PromptSlot, &row.id)),
			)
			.chain(
				self
					.credentials
					.iter()
					.map(|row| (StaticDeclarationClass::Credential, &row.id)),
			)
			.chain(
				self
					.secrets
					.iter()
					.map(|row| (StaticDeclarationClass::Secret, &row.id)),
			)
			.chain(
				self
					.workers
					.iter()
					.map(|row| (StaticDeclarationClass::Worker, &row.id)),
			)
			.chain(
				self
					.placement
					.iter()
					.map(|row| (StaticDeclarationClass::Placement, &row.id)),
			)
	}

	/// Compares a frozen runtime observation against this authenticated
	/// declaration snapshot. Runtime rows verify drift and never add authority.
	pub fn drift(&self, runtime: &Self) -> StaticDeclarationDrift {
		let expected = self
			.identities()
			.map(|(class, id)| (class, id.clone()))
			.collect::<BTreeSet<_>>();
		let actual = runtime
			.identities()
			.map(|(class, id)| (class, id.clone()))
			.collect::<BTreeSet<_>>();
		StaticDeclarationDrift {
			missing:    expected.difference(&actual).cloned().collect(),
			unexpected: actual.difference(&expected).cloned().collect(),
		}
	}

	/// Returns whether no static CONTROL declaration is present.
	pub fn is_empty(&self) -> bool {
		self.rows().next().is_none()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn p7_negative_dominates_later_positive() {
		let id = sf!("acme.reviewer");
		let client = ScopedOverlay {
			scope:   Scope::Client,
			overlay: ExtensionOverlay {
				disabled: [id.clone()].into_iter().collect(),
				..ExtensionOverlay::default()
			},
		};
		let workspace = ScopedOverlay {
			scope:   Scope::Workspace,
			overlay: ExtensionOverlay {
				enabled: [id.clone()].into_iter().collect(),
				..ExtensionOverlay::default()
			},
		};
		let effective = fold_extension(&[client, workspace], &id);
		assert!(effective.disabled);
		assert!(!effective.enabled);
	}

	#[test]
	fn regime_declarations_serialize_under_the_clean_class_name() {
		let declarations = StaticDeclarations {
			regimes: vec![StaticDeclaration {
				id: sf!("acme.goal-loop"),
				..StaticDeclaration::default()
			}]
			.into_boxed_slice(),
			..StaticDeclarations::default()
		};

		let encoded = serde_json::to_value(&declarations).expect("serialize declarations");
		assert_eq!(encoded["regimes"][0]["id"], "acme.goal-loop");
	}

	#[test]
	fn ordered_regime_declaration_lowers_and_unknown_kind_is_rejected() {
		let mut properties = BTreeMap::new();
		properties.insert(
			sf!("declarations"),
			serde_json::json!([{"id": "acme.goal-loop", "kind": "regime"}]),
		);
		let declarations =
			StaticDeclarations::from_properties(&properties).expect("lower regime declaration");
		assert_eq!(declarations.regimes.len(), 1);
		let (class, id) = declarations.identities().next().expect("regime identity");
		assert_eq!(class, StaticDeclarationClass::Regime);
		assert_eq!(id.as_str(), "acme.goal-loop");

		properties.insert(
			sf!("declarations"),
			serde_json::json!([{"id": "acme.legacy", "kind": "legacy_control"}]),
		);
		assert!(StaticDeclarations::from_properties(&properties).is_err());
	}
	#[test]
	fn manifest_admission_rejects_composer_shape_declaration_kind() {
		let mut properties = BTreeMap::new();
		properties.insert(
			sf!("declarations"),
			serde_json::json!([{"id": "acme.dock", "kind": "composer-shape"}]),
		);

		assert!(StaticDeclarations::from_properties(&properties).is_err());
	}
}
