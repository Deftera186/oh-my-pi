//! Integrity and runtime-health diagnostics for `omp ext doctor`.

use std::{
	fs,
	path::{Path, PathBuf},
};

use omp_core::{Str, encoding::hex};
use sha2::{Digest as _, Sha256};

use super::{
	ExtensionCode, Layer,
	lock::{InstalledRecord, LockFile, LockedExtension},
	trust::{KeysFile, RevocationsFile, verify_artifact_signature},
};

/// Diagnostic severity emitted by the extension doctor.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DoctorSeverity {
	/// Healthy evidence.
	Ok,
	/// Degraded or mechanically repairable evidence.
	Warning,
	/// Fail-closed integrity or runtime prerequisite failure.
	Error,
}

/// One stable doctor finding with repair evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorFinding {
	/// Stable extension diagnostic code when applicable.
	pub code:         Option<ExtensionCode>,
	/// Finding severity.
	pub severity:     DoctorSeverity,
	/// Extension identity, when scoped to one extension.
	pub extension_id: Option<Str>,
	/// Human-readable evidence.
	pub detail:       Str,
	/// Whether this invocation repaired deterministic local state.
	pub repaired:     bool,
}

/// Paths and policy consumed by one doctor pass.
#[derive(Clone, Debug)]
pub struct DoctorRequest<'a> {
	/// Owning lock layer.
	pub layer:            Layer,
	/// Portable lock path.
	pub lock_path:        &'a Path,
	/// Local install-record path.
	pub installed_path:   &'a Path,
	/// Local TOFU key path.
	pub keys_path:        &'a Path,
	/// Optional signed revocation snapshot.
	pub revocations_path: Option<&'a Path>,
	/// Managed site tree root.
	pub site_root:        &'a Path,
	/// Content-addressed artifact cache.
	pub artifact_cache:   &'a Path,
	/// Whether deterministic local repairs are allowed.
	pub fix:              bool,
}

/// Runtime health facts supplied by the Environment and inference authorities.
pub trait RuntimeHealth {
	/// Returns whether the Environment worker boundary is reachable.
	fn environment_ready(&self) -> bool;
	/// Returns a credential-health diagnostic for one installed extension.
	fn credential_health(&self, extension_id: &str) -> CredentialHealth;
}

/// Credential readiness without exposing credential material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialHealth {
	/// No credential is required.
	NotRequired,
	/// Required credential is available through inference authority.
	Ready,
	/// Required credential is missing or disabled.
	Unavailable(Str),
}

/// Runs integrity, ownership, ABI, revocation, Environment, and credential
/// checks. `fix` may remove stale staging paths and disable an installed entry
/// with no lock; it never selects versions or mutates publisher trust.
pub fn diagnose(request: &DoctorRequest<'_>, health: &impl RuntimeHealth) -> Vec<DoctorFinding> {
	let mut findings = Vec::new();
	let lock = match LockFile::read(request.lock_path, request.layer) {
		Ok(lock) => Some(lock),
		Err(error) => {
			findings.push(finding(Some(error.code), DoctorSeverity::Error, None, error.detail, false));
			None
		},
	};
	let mut installed = match InstalledRecord::read(request.installed_path) {
		Ok(installed) => installed,
		Err(error) => {
			findings.push(finding(Some(error.code), DoctorSeverity::Error, None, error.detail, false));
			InstalledRecord::default()
		},
	};
	let keys = match KeysFile::read(request.keys_path) {
		Ok(keys) => Some(keys),
		Err(error) => {
			findings.push(finding(Some(error.code), DoctorSeverity::Error, None, error.detail, false));
			None
		},
	};
	if !health.environment_ready() {
		findings.push(finding(
			Some(ExtensionCode::EOffline),
			DoctorSeverity::Error,
			None,
			Str::new_static("Environment extension boundary is unavailable"),
			false,
		));
	}

	let mut installed_changed = false;
	for entry in &mut installed.extensions {
		let Some(locked) = lock
			.as_ref()
			.and_then(|lock| lock.extensions.iter().find(|locked| locked.id == entry.id))
		else {
			if entry
				.source
				.as_table()
				.is_some_and(|source| source.contains_key("link") || source.contains_key("path"))
			{
				continue;
			}
			let repaired = request.fix && entry.enabled;
			if repaired {
				entry.enabled = false;
				installed_changed = true;
			}
			findings.push(finding(
				Some(ExtensionCode::WNoLock),
				DoctorSeverity::Warning,
				Some(entry.id.clone()),
				Str::new_static("installed extension has no reproducible lock entry"),
				repaired,
			));
			continue;
		};
		if !keys.as_ref().is_some_and(|keys| {
			keys
				.keys
				.iter()
				.any(|pin| pin.id == entry.id && pin.key == locked.publisher)
		}) {
			findings.push(finding(
				Some(ExtensionCode::EKeyChanged),
				DoctorSeverity::Error,
				Some(entry.id.clone()),
				Str::new_static("lock publisher does not match the local TOFU pin"),
				false,
			));
		}
		if let Some(revocations) = request
			.revocations_path
			.and_then(|path| RevocationsFile::read(path).ok())
			&& revocations.predicate_for(&entry.id).is_some()
		{
			findings.push(finding(
				Some(ExtensionCode::ERevoked),
				DoctorSeverity::Error,
				Some(entry.id.clone()),
				Str::new_static("installed extension matches the signed revocation set"),
				false,
			));
		}
		let artifact = request
			.artifact_cache
			.join(locked.wheel.blake3.as_str().trim_start_matches("b3:"));
		match verify_artifact(&artifact, locked) {
			Ok(()) => {},
			Err(detail) => findings.push(finding(
				Some(ExtensionCode::EIntegrity),
				DoctorSeverity::Error,
				Some(entry.id.clone()),
				detail,
				false,
			)),
		}
		if let CredentialHealth::Unavailable(detail) = health.credential_health(&entry.id) {
			findings.push(finding(
				None,
				DoctorSeverity::Warning,
				Some(entry.id.clone()),
				detail,
				false,
			));
		}
	}
	if installed_changed {
		match installed.write(request.installed_path) {
			Ok(()) => {},
			Err(error) => findings.push(finding(
				Some(ExtensionCode::EIntegrity),
				DoctorSeverity::Error,
				None,
				Str::new(error.to_string()),
				false,
			)),
		}
	}
	inspect_site(request, &mut findings);
	if findings.is_empty() {
		findings.push(finding(
			None,
			DoctorSeverity::Ok,
			None,
			Str::new_static("extension state is healthy"),
			false,
		));
	}
	findings
}

fn verify_artifact(path: &Path, locked: &LockedExtension) -> Result<(), Str> {
	let bytes = fs::read(path).map_err(|error| Str::new(error.to_string()))?;
	if bytes.len() as u64 != locked.wheel.size {
		return Err(Str::new_static("artifact byte length differs from lock"));
	}
	let blake3 = format!("b3:{}", blake3::hash(&bytes).to_hex());
	if blake3 != locked.wheel.blake3.as_str() {
		return Err(Str::new_static("artifact BLAKE3 differs from lock"));
	}
	let sha256 = format!("sha256:{}", hex::encode(&Sha256::digest(&bytes)));
	if sha256 != locked.wheel.sha256.as_str() {
		return Err(Str::new_static("artifact SHA-256 differs from lock"));
	}
	verify_artifact_signature(
		locked.publisher.as_str(),
		locked.wheel.blake3.as_str(),
		locked.wheel.sha256.as_str(),
		locked.capability_digest.as_str(),
		locked.signature.as_str(),
	)
	.map_err(|error| error.detail)
}

fn inspect_site(request: &DoctorRequest<'_>, findings: &mut Vec<DoctorFinding>) {
	let staging = request.site_root.join(".staging");
	if !staging.exists() {
		return;
	}
	let repaired = request.fix && fs::remove_dir_all(&staging).is_ok();
	findings.push(finding(
		Some(ExtensionCode::WSiteExtra),
		DoctorSeverity::Warning,
		None,
		Str::new_static("stale site materialization staging tree exists"),
		repaired,
	));
}

fn finding(
	code: Option<ExtensionCode>,
	severity: DoctorSeverity,
	extension_id: Option<Str>,
	detail: Str,
	repaired: bool,
) -> DoctorFinding {
	DoctorFinding { code, severity, extension_id, detail, repaired }
}

/// Returns paths referenced by the active lock/install generation. GC callers
/// retain these even when their version cache is otherwise unreachable.
pub fn active_paths(request: &DoctorRequest<'_>) -> Vec<PathBuf> {
	vec![
		request.lock_path.to_path_buf(),
		request.installed_path.to_path_buf(),
		request.site_root.to_path_buf(),
	]
}
