//! Cached commit-author avatar resolution for the Git workbench.

use std::{
	env,
	io::Cursor,
	path::{Path, PathBuf},
	sync::LazyLock,
	time::{Duration, SystemTime},
};

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use image::{GenericImageView as _, ImageFormat, imageops::FilterType};
use md5::{Digest as _, Md5};
use omp_core::Str;
use omp_envd::{
	exec::ExecHost,
	vcs::git::{commands::GitCommands, runner::GitRunner},
};
use regex::Regex;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

const AVATAR_PX: u32 = 64;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const NEGATIVE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_AVATAR_BYTES: usize = 8 * 1024 * 1024;

/// Non-blocking, disk-backed author avatar resolver.
#[derive(Clone)]
pub struct AvatarLoader {
	cache_dir: PathBuf,
	client:    reqwest::Client,
	commands:  GitCommands,
}

impl AvatarLoader {
	/// Creates a loader using the canonical OMP cache root.
	pub fn new() -> Option<Self> {
		let cache_dir = cache_root()?.join("avatars");
		Some(Self {
			cache_dir,
			client: omp_http::default_client(),
			commands: GitCommands::new(GitRunner::new(ExecHost::new())),
		})
	}

	/// Returns a cached or remotely resolved 64-pixel PNG, or `None` for a miss.
	pub async fn load(&self, email: &str, cwd: &Path, cancel: &CancellationToken) -> Option<Bytes> {
		let email = email.trim().to_ascii_lowercase();
		let key = md5_hex(&email);
		let png_path = self.cache_dir.join(format!("{key}.png"));
		let miss_path = self.cache_dir.join(format!("{key}.miss"));
		if let Ok(bytes) = tokio::fs::read(&png_path).await {
			return Some(Bytes::from(bytes));
		}
		if recent_miss(&miss_path).await {
			return None;
		}
		if cancel.is_cancelled() {
			return None;
		}
		let _ = tokio::fs::create_dir_all(&self.cache_dir).await;
		let mut candidates = noreply_urls(&email);
		candidates
			.push(format!("https://www.gravatar.com/avatar/{key}.png?d=404&s={}", AVATAR_PX * 2));
		let mut bytes = None;
		for url in candidates {
			bytes = self.fetch(&url, None).await;
			if bytes.is_some() || cancel.is_cancelled() {
				break;
			}
		}
		if bytes.is_none() && !cancel.is_cancelled() {
			if let Some(url) = self.github_api_avatar_url(cwd, &email, cancel).await {
				bytes = self.fetch(&url, None).await;
			}
		}
		let Some(bytes) = bytes.and_then(normalize_png) else {
			let _ = tokio::fs::write(&miss_path, []).await;
			return None;
		};
		let _ = tokio::fs::write(&png_path, &bytes).await;
		let _ = tokio::fs::remove_file(&miss_path).await;
		Some(bytes)
	}

	async fn fetch(&self, url: &str, token: Option<&str>) -> Option<Bytes> {
		let mut request = self
			.client
			.get(url)
			.header("Accept", "application/vnd.github+json")
			.header("User-Agent", "omp");
		if let Some(token) = token {
			request = request.bearer_auth(token);
		}
		tokio::time::timeout(FETCH_TIMEOUT, async move {
			let response = request.send().await.ok()?;
			if !response.status().is_success() {
				return None;
			}
			let mut stream = response.bytes_stream();
			let mut bytes = BytesMut::new();
			while let Some(chunk) = stream.next().await {
				let chunk = chunk.ok()?;
				if bytes.len().saturating_add(chunk.len()) > MAX_AVATAR_BYTES {
					return None;
				}
				bytes.extend_from_slice(&chunk);
			}
			Some(bytes.freeze())
		})
		.await
		.ok()
		.flatten()
	}

	async fn github_api_avatar_url(
		&self,
		cwd: &Path,
		email: &str,
		cancel: &CancellationToken,
	) -> Option<String> {
		let remote = self
			.commands
			.remote_url(cwd, "origin", cancel)
			.await
			.ok()??;
		let captures = github_remote_regex().captures(remote.as_str())?;
		let owner = captures.get(1)?.as_str();
		let repo = captures.get(2)?.as_str();
		let url = format!(
			"https://api.github.com/repos/{owner}/{repo}/commits?per_page=1&author={}",
			url::form_urlencoded::byte_serialize(email.as_bytes()).collect::<String>()
		);
		let token = env::var("GITHUB_TOKEN")
			.ok()
			.filter(|value| !value.is_empty())
			.or_else(|| env::var("GH_TOKEN").ok().filter(|value| !value.is_empty()));
		let response = self.fetch(&url, token.as_deref()).await?;
		let commits: Vec<GithubCommit> = serde_json::from_slice(&response).ok()?;
		let avatar = commits.first()?.author.as_ref()?.avatar_url.as_deref()?;
		Some(format!("{avatar}{}s={}", if avatar.contains('?') { "&" } else { "?" }, AVATAR_PX * 2))
	}
}

#[derive(Deserialize)]
struct GithubCommit {
	author: Option<GithubAuthor>,
}

#[derive(Deserialize)]
struct GithubAuthor {
	avatar_url: Option<Str>,
}

fn cache_root() -> Option<PathBuf> {
	if let Some(path) = env::var_os("OMP_CACHE_DIR").filter(|value| !value.is_empty()) {
		return Some(PathBuf::from(path));
	}
	let home = PathBuf::from(env::var_os("HOME")?);
	Some(omp_core::dirs::native_directories(&home).cache)
}

async fn recent_miss(path: &Path) -> bool {
	let Ok(metadata) = tokio::fs::metadata(path).await else {
		return false;
	};
	let Ok(modified) = metadata.modified() else {
		return false;
	};
	SystemTime::now()
		.duration_since(modified)
		.is_ok_and(|age| age < NEGATIVE_TTL)
}

fn md5_hex(text: &str) -> String {
	let digest: [u8; 16] = Md5::digest(text.as_bytes()).into();
	omp_core::hex::encode_n(&digest).as_str().to_owned()
}

fn noreply_urls(email: &str) -> Vec<String> {
	static WITH_ID: LazyLock<Regex> = LazyLock::new(|| {
		Regex::new(r"^(\d+)\+[^@]+@users\.noreply\.github\.com$").expect("valid regex")
	});
	static PLAIN: LazyLock<Regex> =
		LazyLock::new(|| Regex::new(r"^([^@+]+)@users\.noreply\.github\.com$").expect("valid regex"));
	let with_id = &*WITH_ID;
	if let Some(id) = with_id.captures(email).and_then(|captures| captures.get(1)) {
		return vec![format!(
			"https://avatars.githubusercontent.com/u/{}?s={}",
			id.as_str(),
			AVATAR_PX * 2
		)];
	}
	let plain = &*PLAIN;
	plain
		.captures(email)
		.and_then(|captures| captures.get(1))
		.map(|name| {
			vec![format!(
				"https://avatars.githubusercontent.com/{}?s={}",
				name.as_str(),
				AVATAR_PX * 2
			)]
		})
		.unwrap_or_default()
}

fn github_remote_regex() -> &'static Regex {
	static REMOTE: LazyLock<Regex> = LazyLock::new(|| {
		Regex::new(r"github\.com[/:]([^/]+)/([^/]+?)(?:\.git)?$").expect("valid regex")
	});
	&REMOTE
}

fn normalize_png(bytes: Bytes) -> Option<Bytes> {
	let image = image::load_from_memory(&bytes).ok()?;
	let (width, height) = image.dimensions();
	let resized = if width == AVATAR_PX && height == AVATAR_PX {
		image
	} else {
		image.resize_to_fill(AVATAR_PX, AVATAR_PX, FilterType::Lanczos3)
	};
	let mut output = Cursor::new(Vec::new());
	resized.write_to(&mut output, ImageFormat::Png).ok()?;
	Some(Bytes::from(output.into_inner()))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn github_noreply_candidates_cover_numeric_and_plain_addresses() {
		assert_eq!(noreply_urls("123+octocat@users.noreply.github.com"), [
			"https://avatars.githubusercontent.com/u/123?s=128"
		]);
		assert_eq!(noreply_urls("octocat@users.noreply.github.com"), [
			"https://avatars.githubusercontent.com/octocat?s=128"
		]);
		assert!(noreply_urls("person@example.com").is_empty());
	}

	#[test]
	fn md5_key_matches_gravatar_contract() {
		assert_eq!(md5_hex("test@example.com"), "55502f40dc8b7c769880b10874abc9d0");
	}
}
