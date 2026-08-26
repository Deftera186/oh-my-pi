//! Extension configuration, resolution, locking, and local trust state.
//!
//! The crate is intentionally CLI- and host-agnostic: argument parsing lives in
//! the application, Environment-backed materialization lives in the
//! environment host, and this surface owns deterministic data transformations
//! plus durable on-disk state.

pub mod config;
pub mod doctor;
pub mod index;
pub mod lock;
pub mod marketplace;
pub mod resolver;
pub mod trust;
pub mod upgrade;

use std::error::Error;

use omp_core::Str;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

/// The layer in which an extension is resolved and admitted.
#[derive(
	Clone, Copy, Debug, Default, Display, EnumString, Eq, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum Layer {
	/// Operator-owned client layer.
	#[default]
	Client,
	/// Workspace-owned layer.
	Workspace,
}

/// The requested trust tier for an extension host.
#[derive(
	Clone, Copy, Debug, Default, Display, EnumString, Eq, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum TrustTier {
	/// Isolated, policy-mediated extension host.
	#[default]
	Sandboxed,
	/// Operator-approved trusted extension host.
	Trusted,
}

/// The closed extension diagnostic vocabulary from deployment §3.16.
///
/// Every extension subsystem emits one of these values; callers should use
/// [`ExtensionCode::as_ref`] rather than inventing string codes.
#[derive(Clone, Copy, Debug, Display, EnumString, Eq, Hash, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "SCREAMING-KEBAB-CASE", ascii_case_insensitive)]
pub enum ExtensionCode {
	/// A named source had no extension manifest.
	ENoManifest,
	/// An extension manifest could not be parsed.
	EManifestParse,
	/// A capability lies outside the closed vocabulary.
	ECapUnknown,
	/// An executable capability was open-ended.
	ECapExecOpen,
	/// A layer declared the same id twice.
	EDupId,
	/// A declaration kind is unknown.
	EDeclKind,
	/// Replacement was declared outside workspace scope.
	EReplaceScope,
	/// An extension dependency crossed layers.
	EXlayerDep,
	/// Extension dependency edges form a cycle.
	EExtCycle,
	/// Skills declared requirements.
	ESkillsRequires,
	/// The resolver has no satisfying closure.
	EUnsat,
	/// A requirement conflicts with frozen runtime metadata.
	EFrozenConflict,
	/// A target lacks an installable wheel.
	ETargetMissing,
	/// A wheel ABI is not valid for `CPython` 3.14t.
	EAbiRejected,
	/// A direct URL occurred in a requirement.
	EUrlRequire,
	/// A git source was not pinned.
	EGitFloating,
	/// Locked index configuration drifted.
	EIndexDrift,
	/// A lock format is too new.
	ELockVersion,
	/// A lock was loaded in the wrong layer.
	ELockLayer,
	/// A lock targets a different Python runtime.
	ELockPython,
	/// A lock contains a duplicate extension id.
	ELockDup,
	/// A lock incorrectly contains a link source.
	ELockLink,
	/// A locked resolution no longer satisfies the request.
	ELockDrift,
	/// Artifact integrity verification failed.
	EIntegrity,
	/// A publisher signature failed verification.
	ESig,
	/// A TOFU publisher key changed without rotation.
	EKeyChanged,
	/// A package or extension was revoked.
	ERevoked,
	/// A binary has no target-specific artifact.
	EBinPlatform,
	/// Offline materialization lacks an artifact.
	EOffline,
	/// A vendored tree contained native code.
	EVendorNative,
	/// Operator consent was declined.
	EConsent,
	/// A requested grant named an unknown capability.
	EGrantUnknown,
	/// Extension settings attempted to carry a secret.
	ESettingSecret,
	/// A trusted extension failed to load.
	ETrustedLoad,
	/// A host binary does not export the `CPython` C API.
	EAbiExport,
	/// A lock references a yanked artifact.
	WYanked,
	/// An accepted publisher key rotation occurred.
	WKeyRotated,
	/// An offline revocation list was stale.
	WRevocationStale,
	/// A site tree contains an untracked entry.
	WSiteExtra,
	/// A vendored dependency duplicates a resolved one.
	WVendorDup,
	/// Resident host cost exceeded the configured budget.
	WPoolCount,
	/// Client and workspace hosts have different API admission sets.
	WApiSkew,
	/// A foreign extension-shaped root was ignored.
	WForeignRoot,
	/// A workspace identity could not be derived.
	WWorkspaceAnon,
	/// Workspace replacement failed a P4 gate.
	WReplaceDenied,
	/// An installed extension has no reproducible lock entry.
	WNoLock,
	/// Ambient `OMP_PY_SITE` bypassed managed site selection.
	WSiteOverride,
	/// A configured index list differs outside locked mode.
	WIndexDrift,
}

/// A structured extension failure or warning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionError {
	/// Stable diagnostic code.
	pub code:   ExtensionCode,
	/// Human-actionable detail.
	pub detail: Str,
}

impl ExtensionError {
	/// Creates a typed diagnostic.
	pub fn new(code: ExtensionCode, detail: impl AsRef<str>) -> Self {
		Self { code, detail: Str::new(detail) }
	}
}

mod extension_error_display {
	use std::fmt::{self, Display};

	use super::ExtensionError;

	impl Display for ExtensionError {
		fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
			write!(formatter, "{}: {}", self.code, self.detail)
		}
	}
}

impl Error for ExtensionError {}

/// Typed provenance fields stamped wherever an extension acts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
	/// TOFU-pinned publisher fingerprint.
	pub publisher:       Str,
	/// Publisher-scoped extension identity.
	pub extension_id:    Str,
	/// Exact extension version.
	pub version:         Str,
	/// Exact wheel artifact digest.
	pub artifact_digest: Str,
	/// Resolving layer.
	pub layer:           Layer,
	/// Granted trust tier.
	pub tier:            TrustTier,
	/// Host incarnation generation.
	pub generation:      u64,
}

/// A canonical workspace identity and its grant-key digest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceUri {
	/// Canonical URI identifying the workspace machine and root.
	pub uri:    Str,
	/// BLAKE3 workspace identity digest.
	pub digest: Str,
}
