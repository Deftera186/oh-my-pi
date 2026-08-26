//! Signed native package-registry inspection and rollback-safe self-update.

use std::{
	cmp,
	env::{self, consts},
	fs::{self, OpenOptions},
	io,
	io::Write as _,
	path::{Path, PathBuf},
	process::{self, Command},
	time::{SystemTime, UNIX_EPOCH},
};

use futures::StreamExt as _;
use miette::{IntoDiagnostic as _, miette};
use omp_core::{Str, encoding::hex};
use omp_ext::{
	index::{IndexArtifact, IndexExtension, IndexRelease, SignedIndex},
	trust::{KeysFile, verify_artifact_signature},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
	cli::{RegistryArgs, UpdateArgs},
	ext_cli,
};

const CORE_PACKAGE: &str = "omp-cli";
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;
const GITHUB_LATEST_RELEASE: &str = "https://api.github.com/repos/can1357/oh-my-pi/releases/latest";
const GITHUB_USER_AGENT: &str = concat!("omp/", env!("CARGO_PKG_VERSION"));
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum InstallManager {
	Native,
	Npm,
	Homebrew,
	Mise,
	Nix,
}

#[derive(Serialize)]
struct RegistryView<'a> {
	package:  &'a str,
	target:   String,
	manager:  InstallManager,
	releases: Vec<ReleaseView<'a>>,
}

#[derive(Serialize)]
struct ReleaseView<'a> {
	version:  &'a str,
	attested: bool,
	yanked:   bool,
	assets:   Vec<AssetView<'a>>,
}

#[derive(Serialize)]
struct AssetView<'a> {
	target: &'a str,
	file:   &'a str,
	size:   u64,
	sha256: &'a str,
}

struct Selected<'a> {
	issued_at: &'a Str,
	extension: &'a IndexExtension,
	release:   &'a IndexRelease,
	artifact:  &'a IndexArtifact,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct GithubRelease {
	tag_name: Str,
	assets:   Vec<GithubAsset>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GithubAsset {
	name:                 Str,
	browser_download_url: String,
	size:                 u64,
	digest:               Option<Str>,
}

#[must_use]
struct UpdateLock(PathBuf);

impl Drop for UpdateLock {
	fn drop(&mut self) {
		let _ = fs::remove_file(&self.0);
	}
}

/// Runs the signed core updater or explicitly delegates extension upgrades.
pub async fn run(args: UpdateArgs) -> miette::Result<()> {
	if args.plugins {
		if args.check || args.force || args.index.is_some() || args.index_key.is_some() {
			return Err(miette!(
				"--plugins is exactly `omp ext upgrade` and cannot be combined with core update \
				 options"
			));
		}
		return upgrade_extensions().await;
	}
	if !release_override_requested(&args) {
		return run_github_update(args).await;
	}
	let (index, _) = load_index(args.index.as_deref(), args.index_key.as_deref())?;
	let target = platform_target();
	let selected = select(&index, CORE_PACKAGE, &target)?;
	let manager = classify_installation(&env::current_exe().into_diagnostic()?);
	let current = env!("CARGO_PKG_VERSION");
	let newer = compare_versions(selected.release.version.as_str(), current).is_gt();
	if args.check || (!newer && !args.force) {
		println!(
			"current={current}\tlatest={}\ttarget={target}\tmanager={manager:?}\\
			 tupdate_available={newer}",
			selected.release.version
		);
		return Ok(());
	}
	if manager != InstallManager::Native {
		return Err(miette!("{}", manager_instruction(manager)));
	}
	let version = selected.release.version.clone();
	install(selected).await?;
	println!("updated omp to {version} ({target})");
	Ok(())
}
fn release_override_requested(args: &UpdateArgs) -> bool {
	args.index.is_some()
		|| args.index_key.is_some()
		|| env::var_os("OMP_RELEASE_INDEX").is_some()
		|| env::var_os("OMP_RELEASE_INDEX_KEY").is_some()
}

async fn run_github_update(args: UpdateArgs) -> miette::Result<()> {
	let release = fetch_github_release(std::time::Duration::from_secs(15)).await?;
	let target = platform_target();
	let asset_name = github_asset_name();
	let asset = release
		.assets
		.iter()
		.find(|asset| asset.name.as_str() == asset_name)
		.ok_or_else(|| miette!("latest GitHub release has no exact `{asset_name}` asset"))?;
	let digest = github_sha256(asset)?;
	let version = release.tag_name.as_str().trim_start_matches('v');
	if version.is_empty() {
		return Err(miette!("latest GitHub release has an empty version tag"));
	}
	let manager = classify_installation(&env::current_exe().into_diagnostic()?);
	let current = env!("CARGO_PKG_VERSION");
	let newer = compare_versions(version, current).is_gt();
	if args.check || (!newer && !args.force) {
		println!(
			"current={current}\tlatest={version}\ttarget={target}\tmanager={manager:?}\\
			 tupdate_available={newer}"
		);
		return Ok(());
	}
	if manager != InstallManager::Native {
		return Err(miette!("{}", manager_instruction(manager)));
	}
	install_github_asset(asset, digest, version).await?;
	println!("updated omp to {version} ({target})");
	Ok(())
}

async fn fetch_github_release(timeout: std::time::Duration) -> miette::Result<GithubRelease> {
	tokio::time::timeout(timeout, async {
		let response = omp_http::default_client()
			.get(GITHUB_LATEST_RELEASE)
			.header("User-Agent", GITHUB_USER_AGENT)
			.header("Accept", "application/vnd.github+json")
			.send()
			.await
			.into_diagnostic()?;
		if !response.status().is_success() {
			return Err(miette!("GitHub release lookup returned HTTP {}", response.status()));
		}
		response.json::<GithubRelease>().await.into_diagnostic()
	})
	.await
	.map_err(|_| miette!("GitHub release lookup timed out"))?
}

fn github_sha256(asset: &GithubAsset) -> miette::Result<&str> {
	let digest = asset
		.digest
		.as_deref()
		.and_then(|digest| digest.strip_prefix("sha256:"))
		.ok_or_else(|| miette!("GitHub release asset `{}` has no SHA-256 digest", asset.name))?;
	if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
		return Err(miette!("GitHub release asset `{}` has a malformed SHA-256 digest", asset.name));
	}
	Ok(digest)
}

async fn install_github_asset(
	asset: &GithubAsset,
	expected_sha256: &str,
	version: &str,
) -> miette::Result<()> {
	if asset.size > MAX_ASSET_BYTES {
		return Err(miette!("GitHub update asset exceeds the 256 MiB safety ceiling"));
	}
	let cache = update_cache_dir()?;
	fs::create_dir_all(&cache).into_diagnostic()?;
	let lock_path = cache.join("update.lock");
	OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(&lock_path)
		.map_err(|error| miette!("another updater owns {}: {error}", lock_path.display()))?;
	let _lock = UpdateLock(lock_path);
	let bytes = fetch_github_asset(asset).await?;
	if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != asset.size {
		return Err(miette!("GitHub update asset size differs from release metadata"));
	}
	let actual = hex::encode(&Sha256::digest(&bytes)).to_string();
	if !actual.eq_ignore_ascii_case(expected_sha256) {
		return Err(miette!("GitHub update asset SHA-256 differs from release metadata"));
	}
	let current = env::current_exe().into_diagnostic()?;
	let destination = renamed_destination(&current);
	let install_dir = destination.parent().unwrap_or_else(|| Path::new("."));
	prune_stale(install_dir)?;
	let timestamp = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.into_diagnostic()?
		.as_millis();
	let attempt = format!("{timestamp}.{}", process::id());
	let (staged, backup) = update_artifact_paths(&destination, &attempt)?;
	write_executable(&staged, &bytes)?;
	atomic_replace(&staged, &destination, &backup, version)?;
	retire_renamed_source(&current, &destination, &attempt)
}

async fn fetch_github_asset(asset: &GithubAsset) -> miette::Result<Vec<u8>> {
	if !asset.browser_download_url.starts_with("https://") {
		return Err(miette!("GitHub update asset URL must use HTTPS"));
	}
	let response = omp_http::default_client()
		.get(&asset.browser_download_url)
		.header("User-Agent", GITHUB_USER_AGENT)
		.send()
		.await
		.into_diagnostic()?;
	if !response.status().is_success() {
		return Err(miette!("update download returned HTTP {}", response.status()));
	}
	let mut bytes = Vec::with_capacity(usize::try_from(asset.size).unwrap_or_default());
	let mut stream = response.bytes_stream();
	while let Some(chunk) = stream.next().await {
		let chunk = chunk.into_diagnostic()?;
		if bytes.len().saturating_add(chunk.len())
			> usize::try_from(MAX_ASSET_BYTES).unwrap_or(usize::MAX)
		{
			return Err(miette!("update download exceeded the 256 MiB safety ceiling"));
		}
		bytes.extend_from_slice(&chunk);
	}
	Ok(bytes)
}

fn update_cache_dir() -> miette::Result<PathBuf> {
	if let Some(cache) = env::var_os("OMP_CACHE_DIR").filter(|value| !value.is_empty()) {
		return Ok(PathBuf::from(cache).join("updates"));
	}
	let home = env::var_os("HOME")
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
		.ok_or_else(|| miette!("HOME or OMP_CACHE_DIR must be set for native update staging"))?;
	Ok(omp_core::dirs::native_directories(&home)
		.cache
		.join("updates"))
}

/// Inspects the verified package registry without mutating locks or TOFU pins.
pub fn registry(args: RegistryArgs) -> miette::Result<()> {
	let (index, _) = load_index(args.index.as_deref(), args.index_key.as_deref())?;
	let target = platform_target();
	let package = index
		.extensions
		.iter()
		.find(|package| package.id == args.package)
		.ok_or_else(|| miette!("signed registry has no package `{}`", args.package))?;
	let manager = classify_installation(&env::current_exe().into_diagnostic()?);
	let view = RegistryView {
		package: package.id.as_str(),
		target,
		manager,
		releases: package
			.releases
			.iter()
			.map(|release| ReleaseView {
				version:  release.version.as_str(),
				attested: release.attested,
				yanked:   release.yanked,
				assets:   release
					.artifacts
					.iter()
					.map(|asset| AssetView {
						target: asset.target.as_str(),
						file:   asset.file.as_str(),
						size:   asset.size,
						sha256: asset.sha256.as_str(),
					})
					.collect(),
			})
			.collect(),
	};
	if args.json {
		println!("{}", serde_json::to_string_pretty(&view).into_diagnostic()?);
	} else {
		println!("package\t{}", view.package);
		println!("target\t{}", view.target);
		println!("manager\t{:?}", view.manager);
		for release in &view.releases {
			for asset in &release.assets {
				println!(
					"{}\t{}\t{}\t{}\tattested={}\tyanked={}",
					release.version,
					asset.target,
					asset.file,
					asset.sha256,
					release.attested,
					release.yanked
				);
			}
		}
	}
	Ok(())
}

fn load_index(index: Option<&Path>, key: Option<&Path>) -> miette::Result<(SignedIndex, String)> {
	let index = configured_path(index, "OMP_RELEASE_INDEX", "signed release index")?;
	let key = configured_path(key, "OMP_RELEASE_INDEX_KEY", "release index key")?;
	let key = fs::read_to_string(key).into_diagnostic()?;
	let key = key.trim().to_owned();
	let index = SignedIndex::read(&index, &key).into_diagnostic()?;
	Ok((index, key))
}

fn configured_path(
	explicit: Option<&Path>,
	variable: &str,
	label: &str,
) -> miette::Result<PathBuf> {
	explicit
		.map(Path::to_path_buf)
		.or_else(|| {
			env::var_os(variable)
				.filter(|value| !value.is_empty())
				.map(PathBuf::from)
		})
		.ok_or_else(|| miette!("{label} is required; pass its option or set {variable}"))
}

fn select<'a>(index: &'a SignedIndex, package: &str, target: &str) -> miette::Result<Selected<'a>> {
	let extension = index
		.extensions
		.iter()
		.find(|extension| extension.id.as_str() == package)
		.ok_or_else(|| miette!("signed registry has no package `{package}`"))?;
	let (release, artifact) = extension
		.releases
		.iter()
		.filter(|release| release.attested && !release.yanked)
		.filter_map(|release| target_artifact(release, target).map(|artifact| (release, artifact)))
		.max_by(|(left, _), (right, _)| {
			compare_versions(left.version.as_str(), right.version.as_str())
		})
		.ok_or_else(|| miette!("signed registry has no attested `{target}` asset for `{package}`"))?;
	verify_artifact_signature(
		extension.publisher_key.as_str(),
		artifact.blake3.as_str(),
		artifact.sha256.as_str(),
		release.capability_digest.as_str(),
		artifact.signature.as_str(),
	)
	.into_diagnostic()?;
	Ok(Selected { issued_at: &index.issued_at, extension, release, artifact })
}
fn target_artifact<'a>(release: &'a IndexRelease, target: &str) -> Option<&'a IndexArtifact> {
	release
		.artifacts
		.iter()
		.find(|artifact| artifact.target.as_str() == target)
}

async fn install(selected: Selected<'_>) -> miette::Result<()> {
	if selected.artifact.size > MAX_ASSET_BYTES {
		return Err(miette!("signed update asset exceeds the 256 MiB safety ceiling"));
	}
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let cache =
		if let Some(cache) = env::var_os("OMP_CACHE_DIR").filter(|value| !value.is_empty()) {
			PathBuf::from(cache)
		} else {
			let home = env::var_os("HOME")
				.filter(|value| !value.is_empty())
				.map(PathBuf::from)
				.ok_or_else(|| {
					miette!("HOME or OMP_CACHE_DIR must be set for native update staging")
				})?;
			omp_core::dirs::native_directories(&home).cache
		}
		.join("updates");
	fs::create_dir_all(&cache).into_diagnostic()?;
	let lock_path = cache.join("update.lock");
	OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(&lock_path)
		.map_err(|error| miette!("another updater owns {}: {error}", lock_path.display()))?;
	let _lock = UpdateLock(lock_path);

	let bytes = fetch_asset(selected.artifact).await?;
	verify_bytes(&bytes, selected.artifact)?;
	let executable = extract_executable(&bytes, selected.artifact.file.as_str())?;
	let current = env::current_exe().into_diagnostic()?;
	let destination = renamed_destination(&current);
	let install_dir = destination.parent().unwrap_or_else(|| Path::new("."));
	prune_stale(install_dir)?;
	let timestamp = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.into_diagnostic()?
		.as_millis();
	let attempt = format!("{timestamp}.{}", process::id());
	let (staged, backup) = update_artifact_paths(&destination, &attempt)?;
	write_executable(&staged, &executable)?;
	let mut keys =
		KeysFile::read(&data_dir.join("ext/keys.toml")).map_err(|error| miette!("{error}"))?;
	keys
		.verify_or_pin(
			&selected.extension.id,
			&selected.extension.publisher_key,
			&selected.release.version,
			selected.issued_at,
			None,
		)
		.map_err(|error| miette!("{error}"))?;
	keys
		.write(&data_dir.join("ext/keys.toml"))
		.into_diagnostic()?;
	atomic_replace(&staged, &destination, &backup, selected.release.version.as_str())?;
	retire_renamed_source(&current, &destination, &attempt)?;
	Ok(())
}

async fn fetch_asset(asset: &IndexArtifact) -> miette::Result<Vec<u8>> {
	if let Some(path) = asset.url.strip_prefix("file://") {
		return fs::read(path).into_diagnostic();
	}
	if !asset.url.starts_with("https://") {
		return Err(miette!("signed update asset URL must use HTTPS"));
	}
	let response = omp_http::default_client()
		.get(&asset.url)
		.send()
		.await
		.into_diagnostic()?;
	if !response.status().is_success() {
		return Err(miette!("update download returned HTTP {}", response.status()));
	}
	let mut bytes = Vec::with_capacity(usize::try_from(asset.size).unwrap_or_default());
	let mut stream = response.bytes_stream();
	while let Some(chunk) = stream.next().await {
		let chunk = chunk.into_diagnostic()?;
		if bytes.len().saturating_add(chunk.len())
			> usize::try_from(MAX_ASSET_BYTES).unwrap_or(usize::MAX)
		{
			return Err(miette!("update download exceeded the 256 MiB safety ceiling"));
		}
		bytes.extend_from_slice(&chunk);
	}
	Ok(bytes)
}

fn verify_bytes(bytes: &[u8], asset: &IndexArtifact) -> miette::Result<()> {
	if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != asset.size {
		return Err(miette!("update asset size differs from signed registry metadata"));
	}
	let sha256 = format!("sha256:{}", hex::encode(&Sha256::digest(bytes)));
	if sha256 != asset.sha256.as_str() {
		return Err(miette!("update asset SHA-256 differs from signed registry metadata"));
	}
	let blake3 = format!("b3:{}", blake3::hash(bytes).to_hex());
	if blake3 != asset.blake3.as_str() {
		return Err(miette!("update asset BLAKE3 differs from signed registry metadata"));
	}
	Ok(())
}

fn extract_executable(bytes: &[u8], filename: &str) -> miette::Result<Vec<u8>> {
	if matches!(filename, "omp" | "omp.exe") {
		return Ok(bytes.to_vec());
	}
	let files =
		omp_ar::unpack(bytes).map_err(|error| miette!("update archive is invalid: {error}"))?;
	let executable_name = if cfg!(windows) { "omp.exe" } else { "omp" };
	let mut matches = files.into_iter().filter(|(path, _)| {
		Path::new(path.as_str())
			.file_name()
			.is_some_and(|name| name == executable_name)
	});
	let (_, executable) = matches
		.next()
		.ok_or_else(|| miette!("update archive contains no `{executable_name}` executable"))?;
	if matches.next().is_some() {
		return Err(miette!("update archive contains multiple `{executable_name}` executables"));
	}
	Ok(executable)
}

fn write_executable(path: &Path, bytes: &[u8]) -> miette::Result<()> {
	let mut file = OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(path)
		.into_diagnostic()?;
	file.write_all(bytes).into_diagnostic()?;
	file.sync_all().into_diagnostic()?;
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt as _;
		fs::set_permissions(path, fs::Permissions::from_mode(0o755)).into_diagnostic()?;
	}
	Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenameFailureKind {
	Denied,
	Other,
}

fn classify_rename_failure(error: &io::Error) -> RenameFailureKind {
	if error.kind() == io::ErrorKind::PermissionDenied
		|| matches!(error.raw_os_error(), Some(5 | 32 | 33))
	{
		RenameFailureKind::Denied
	} else {
		RenameFailureKind::Other
	}
}

fn update_artifact_paths(destination: &Path, attempt: &str) -> miette::Result<(PathBuf, PathBuf)> {
	let file = destination
		.file_name()
		.ok_or_else(|| miette!("update destination has no filename"))?
		.to_string_lossy();
	let parent = destination.parent().unwrap_or_else(|| Path::new("."));
	Ok((parent.join(format!("{file}.{attempt}.new")), parent.join(format!("{file}.{attempt}.bak"))))
}

fn atomic_replace(
	staged: &Path,
	destination: &Path,
	backup: &Path,
	expected_version: &str,
) -> miette::Result<()> {
	let had_destination = destination.exists();
	if had_destination {
		if let Err(error) = fs::rename(destination, backup) {
			return match classify_rename_failure(&error) {
				RenameFailureKind::Denied => Err(miette!(
					"running omp executable could not be renamed; the existing installation was left \
					 untouched"
				)),
				RenameFailureKind::Other => Err(error).into_diagnostic(),
			};
		}
	}
	if let Err(error) = fs::rename(staged, destination) {
		if had_destination {
			let _ = fs::rename(backup, destination);
		}
		return Err(error).into_diagnostic();
	}
	let verified = Command::new(destination)
		.arg("--version")
		.output()
		.ok()
		.filter(|output| output.status.success())
		.is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains(expected_version));
	if !verified {
		let failed = destination.with_extension("failed-update");
		let _ = fs::rename(destination, &failed);
		if had_destination {
			fs::rename(backup, destination).into_diagnostic()?;
		}
		let _ = fs::remove_file(failed);
		return Err(miette!("installed omp failed version verification; previous binary restored"));
	}
	if had_destination {
		// Windows keeps the renamed process image mapped until this updater
		// exits. A failed unlink does not invalidate a verified replacement;
		// the next locked update reclaims the numeric `.bak` sidecar.
		let _ = fs::remove_file(backup);
	}
	Ok(())
}

fn retire_renamed_source(current: &Path, destination: &Path, attempt: &str) -> miette::Result<()> {
	if current == destination || !current.exists() {
		return Ok(());
	}
	let (_, backup) = update_artifact_paths(current, attempt)?;
	fs::rename(current, &backup).into_diagnostic()?;
	let _ = fs::remove_file(backup);
	Ok(())
}

fn renamed_destination(current: &Path) -> PathBuf {
	renamed_destination_for(current, cfg!(windows))
}

fn renamed_destination_for(current: &Path, windows: bool) -> PathBuf {
	if current
		.file_stem()
		.and_then(|name| name.to_str())
		.is_some_and(|name| matches!(name, "pi" | "oh-my-pi"))
	{
		return current.with_file_name(if windows { "omp.exe" } else { "omp" });
	}
	current.to_path_buf()
}
fn is_update_artifact_name(name: &str) -> bool {
	const BASES: [&str; 6] = ["omp.exe", "oh-my-pi.exe", "pi.exe", "omp", "oh-my-pi", "pi"];
	for base in BASES {
		let Some(rest) = name.strip_prefix(base) else {
			continue;
		};
		let middle = if let Some(middle) = rest.strip_suffix(".bak") {
			middle
		} else if let Some(middle) = rest.strip_suffix(".new") {
			middle
		} else {
			continue;
		};
		if middle.is_empty()
			|| middle.strip_prefix('.').is_some_and(|numeric| {
				!numeric.is_empty()
					&& numeric
						.split('.')
						.all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
			}) {
			return true;
		}
	}
	false
}

fn prune_stale(directory: &Path) -> miette::Result<()> {
	for entry in fs::read_dir(directory).into_diagnostic()? {
		let entry = entry.into_diagnostic()?;
		let name = entry.file_name();
		if is_update_artifact_name(&name.to_string_lossy()) {
			// A mapped Windows backup may still belong to a running older
			// updater. Deletion remains best-effort just like pi.
			let _ = fs::remove_file(entry.path());
		}
	}
	Ok(())
}

fn classify_installation(executable: &Path) -> InstallManager {
	if let Some(value) = env::var_os("OMP_INSTALL_MANAGER") {
		return match value.to_string_lossy().to_ascii_lowercase().as_str() {
			"npm" => InstallManager::Npm,
			"homebrew" | "brew" => InstallManager::Homebrew,
			"mise" => InstallManager::Mise,
			"nix" => InstallManager::Nix,
			_ => InstallManager::Native,
		};
	}
	let path = executable.to_string_lossy().to_ascii_lowercase();
	if path.contains("/nix/store/") {
		InstallManager::Nix
	} else if path.contains("/.local/share/mise/") || path.contains("/.mise/") {
		InstallManager::Mise
	} else if path.contains("/cellar/") || path.contains("/homebrew/") || path.contains("linuxbrew")
	{
		InstallManager::Homebrew
	} else if path.contains("node_modules") || path.contains("/npm/") {
		InstallManager::Npm
	} else {
		InstallManager::Native
	}
}

const fn manager_instruction(manager: InstallManager) -> &'static str {
	match manager {
		InstallManager::Native => "native installation can update in place",
		InstallManager::Npm => "npm owns this installation; run `npm update -g @oh-my-pi/omp`",
		InstallManager::Homebrew => "Homebrew owns this installation; run `brew upgrade omp`",
		InstallManager::Mise => "Mise owns this installation; run `mise upgrade omp`",
		InstallManager::Nix => "Nix owns this installation; update the pinned Nix input",
	}
}

fn github_asset_name() -> String {
	let arch = match consts::ARCH {
		"x86_64" => "x64",
		"aarch64" => "arm64",
		other => other,
	};
	match consts::OS {
		"macos" => format!("omp-darwin-{arch}"),
		"windows" => format!("omp-windows-{arch}.exe"),
		"linux" if cfg!(target_env = "musl") => format!("omp-linux-musl-{arch}"),
		"linux" => format!("omp-linux-{arch}"),
		other => format!("omp-{other}-{arch}"),
	}
}

fn platform_target() -> String {
	let arch = match consts::ARCH {
		"x86_64" => "x86_64",
		"aarch64" => "aarch64",
		other => other,
	};
	match consts::OS {
		"macos" => format!("{arch}-apple-darwin"),
		"windows" => format!("{arch}-pc-windows-msvc"),
		"linux" if cfg!(target_env = "musl") => format!("{arch}-unknown-linux-musl"),
		"linux" => format!("{arch}-unknown-linux-gnu"),
		other => format!("{arch}-unknown-{other}"),
	}
}

fn compare_versions(left: &str, right: &str) -> cmp::Ordering {
	let mut left = left.trim_start_matches('v').split(['.', '-', '+']);
	let mut right = right.trim_start_matches('v').split(['.', '-', '+']);
	loop {
		match (left.next(), right.next()) {
			(None, None) => return cmp::Ordering::Equal,
			(Some(_), None) => return cmp::Ordering::Greater,
			(None, Some(_)) => return cmp::Ordering::Less,
			(Some(left), Some(right)) => {
				let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
					(Ok(left), Ok(right)) => left.cmp(&right),
					_ => left.cmp(right),
				};
				if !ordering.is_eq() {
					return ordering;
				}
			},
		}
	}
}

async fn upgrade_extensions() -> miette::Result<()> {
	use crate::ext_cli::{ExtArgs, ExtCommand, ExtUpgradeArgs, Scope};
	ext_cli::run(ExtArgs {
		project:       PathBuf::from("."),
		data_dir:      None,
		store:         None,
		cache:         None,
		index:         Vec::new(),
		index_keys:    None,
		offline:       false,
		locked:        false,
		exclude_newer: None,
		disable:       Vec::new(),
		grant:         None,
		allow_build:   false,
		sign_key:      None,
		uv:            None,
		targets:       Vec::new(),
		trace:         false,
		env_socket:    None,
		layer:         None,
		scope:         Scope::User,
		json:          false,
		verbose:       false,
		command:       ExtCommand::Upgrade(ExtUpgradeArgs {
			ids: Vec::new(),
			to: None,
			dry_run: false,
			allow_capability_widening: false,
			rollback: None,
		}),
	})
	.await
}
#[cfg(test)]
mod tests {
	use super::*;

	fn artifact(target: &'static str, file: &'static str) -> IndexArtifact {
		IndexArtifact {
			target:    Str::new_static(target),
			url:       format!("https://releases.example/{file}"),
			file:      Str::new_static(file),
			tag:       Str::new_static("native"),
			size:      1,
			blake3:    Str::new_static("b3:00"),
			sha256:    Str::new_static("sha256:00"),
			signature: Str::new_static("signature"),
		}
	}

	#[test]
	fn windows_release_selects_its_attested_target_asset() {
		let asset = GithubAsset {
			name:                 Str::new_static("omp-x86_64-apple-darwin"),
			browser_download_url: "https://example.invalid/omp".to_owned(),
			size:                 1,
			digest:               Some(Str::new_static(
				"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
			)),
		};
		assert_eq!(
			github_sha256(&asset).expect("digest"),
			"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
		);
		let mut missing = asset;
		missing.digest = None;
		assert!(github_sha256(&missing).is_err());

		let windows = "x86_64-pc-windows-msvc";
		let release = IndexRelease {
			version:           Str::new_static("18.0.0"),
			manifest_digest:   Str::new_static("b3:manifest"),
			capability_digest: Str::new_static("b3:capabilities"),
			attested:          true,
			yanked:            false,
			shadows:           Vec::new(),
			artifacts:         vec![
				artifact("aarch64-apple-darwin", "omp-darwin"),
				artifact(windows, "omp.exe"),
			],
		};
		assert_eq!(target_artifact(&release, windows).unwrap().file, "omp.exe");
	}

	#[test]
	fn rename_denial_is_classified_without_a_helper_route() {
		let portable = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
		assert_eq!(classify_rename_failure(&portable), RenameFailureKind::Denied);
		let windows_sharing_violation = io::Error::from_raw_os_error(32);
		assert_eq!(classify_rename_failure(&windows_sharing_violation), RenameFailureKind::Denied);
		let missing = io::Error::new(io::ErrorKind::NotFound, "missing");
		assert_eq!(classify_rename_failure(&missing), RenameFailureKind::Other);
	}

	#[test]
	fn stale_numeric_backups_and_downloads_are_pruned() {
		let root = tempfile::tempdir().unwrap();
		for name in [
			"omp.100.42.bak",
			"omp.exe.101.43.new",
			"pi.102.44.bak",
			"oh-my-pi.exe.103.45.bak",
			"omp.bak",
		] {
			fs::write(root.path().join(name), b"stale").unwrap();
		}
		for name in ["omp.notes.bak", "company.bak", "omp.100.42.txt"] {
			fs::write(root.path().join(name), b"keep").unwrap();
		}
		prune_stale(root.path()).unwrap();
		assert!(!root.path().join("omp.100.42.bak").exists());
		assert!(!root.path().join("omp.exe.101.43.new").exists());
		assert!(!root.path().join("pi.102.44.bak").exists());
		assert!(!root.path().join("oh-my-pi.exe.103.45.bak").exists());
		assert!(!root.path().join("omp.bak").exists());
		assert!(root.path().join("omp.notes.bak").exists());
		assert!(root.path().join("company.bak").exists());
		assert!(root.path().join("omp.100.42.txt").exists());
	}
	#[cfg(unix)]
	#[test]
	fn atomic_replace_verifies_and_removes_backup() {
		let root = tempfile::tempdir().unwrap();
		let destination = root.path().join("omp");
		let staged = root.path().join("omp.100.42.new");
		let backup = root.path().join("omp.100.42.bak");
		write_executable(&destination, b"#!/bin/sh\necho 'omp 17.0.0'\n").unwrap();
		write_executable(&staged, b"#!/bin/sh\necho 'omp 18.0.0'\n").unwrap();
		atomic_replace(&staged, &destination, &backup, "18.0.0").unwrap();
		assert!(!backup.exists());
		assert_eq!(fs::read_to_string(destination).unwrap(), "#!/bin/sh\necho 'omp 18.0.0'\n");
	}

	#[cfg(unix)]
	#[test]
	fn atomic_replace_restores_previous_binary_after_failed_verification() {
		let root = tempfile::tempdir().unwrap();
		let destination = root.path().join("omp");
		let staged = root.path().join("omp.100.42.new");
		let backup = root.path().join("omp.100.42.bak");
		let old = b"#!/bin/sh\necho 'omp 17.0.0'\n";
		write_executable(&destination, old).unwrap();
		write_executable(&staged, b"#!/bin/sh\necho 'not omp'\n").unwrap();
		assert!(atomic_replace(&staged, &destination, &backup, "18.0.0").is_err());
		assert_eq!(fs::read(destination).unwrap(), old);
		assert!(!backup.exists());
	}

	#[test]
	fn legacy_windows_name_migrates_to_omp_exe() {
		assert_eq!(
			renamed_destination_for(Path::new("/tools/pi.exe"), true),
			PathBuf::from("/tools/omp.exe")
		);
	}
}
