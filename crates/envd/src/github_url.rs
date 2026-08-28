//! Direct GitHub REST resolver for issue:// and pr:// resources.

use std::{
	fmt::{self, Display},
	fs,
	path::{Path, PathBuf},
	str,
	sync::{Arc, OnceLock},
	time::{SystemTime, UNIX_EPOCH},
};

use bytes::BytesMut;
use futures::{StreamExt as _, future::BoxFuture};
use http::{
	HeaderMap, HeaderValue,
	header::{ACCEPT, ETAG, IF_NONE_MATCH, USER_AGENT},
};
use omp_catalog::AuthSpecId;
use omp_core::{CowBytes, Str, sf};
use omp_inference::{
	auth::{CredentialError, CredentialLease, CredentialNeed, HeaderPlacement},
	id::{AccountId, PrincipalId},
};
use omp_storage::github_cache::{
	GithubCache, GithubCacheKey, GithubCacheStatus, GithubResourceKind,
};
use omp_tools::read::{Fault, resolver::Resolve, selector::ParsedSelector};
use serde_json::Value;

use super::tool_url;

const MAX_BODY: usize = 8 * 1024 * 1024;

/// Provider credential authority consumed by environment-owned GitHub
/// resources.
pub trait CredentialAuthority: Send + Sync + 'static {
	/// Issues one sealed provider lease.
	fn provider_lease(
		&self,
		need: CredentialNeed,
	) -> BoxFuture<'_, Result<CredentialLease, CredentialError>>;
}

/// Late-bound projection of the one combined daemon credential authority.
pub struct GithubCredentialBridge {
	authority: OnceLock<Arc<dyn CredentialAuthority>>,
}

impl GithubCredentialBridge {
	/// Creates an unbound credential projection.
	pub const fn new() -> Self {
		Self { authority: OnceLock::new() }
	}

	/// Installs the sole combined credential authority for this environment.
	pub fn bind(
		&self,
		authority: Arc<dyn CredentialAuthority>,
	) -> Result<(), Arc<dyn CredentialAuthority>> {
		self.authority.set(authority)
	}

	/// Leases the default GitHub provider credential when one is available.
	pub async fn lease(&self) -> Result<Option<CredentialLease>, Fault> {
		self.lease_for("github").await
	}

	/// Leases one named provider credential when one is available.
	pub async fn lease_for(&self, spec: &str) -> Result<Option<CredentialLease>, Fault> {
		let Some(authority) = self.authority.get() else {
			return Ok(None);
		};
		match authority
			.provider_lease(CredentialNeed {
				spec:        AuthSpecId::from(Str::new(spec)),
				account:     Some(AccountId::from(sf!("{spec}:environment"))),
				principal:   Some(PrincipalId::from("environment")),
				valid_after: SystemTime::now(),
			})
			.await
		{
			Ok(lease) => Ok(Some(lease)),
			Err(CredentialError::Unavailable) => Ok(None),
			Err(_) => Err(Fault::Source { message: sf!("credential lease failed.") }),
		}
	}
}

impl fmt::Debug for GithubCredentialBridge {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("GithubCredentialBridge")
			.field("bound", &self.authority.get().is_some())
			.finish()
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GithubScheme {
	Issue,
	PullRequest,
}

pub(super) struct GithubResolver {
	scheme:      GithubScheme,
	root:        PathBuf,
	cache:       Arc<GithubCache>,
	credentials: Arc<GithubCredentialBridge>,
	client:      omp_http::Client,
}

impl GithubResolver {
	pub(super) fn new(
		scheme: GithubScheme,
		root: PathBuf,
		cache: Arc<GithubCache>,
		credentials: Arc<GithubCredentialBridge>,
	) -> Self {
		Self { scheme, root, cache, credentials, client: omp_http::no_redirect_client() }
	}

	#[tracing::instrument(
		name = "github_resource_resolve",
		level = "debug",
		skip_all,
		fields(scheme = ?self.scheme, repo = tracing::field::Empty, number = tracing::field::Empty),
	)]
	async fn resolve(&self, resource: &str, query: Option<&str>) -> Result<Vec<u8>, Fault> {
		let target = Target::parse(self.scheme, resource, query, &self.root)?;
		tracing::Span::current().record("repo", target.repo.as_str());
		if let Some(number) = target.number {
			tracing::Span::current().record("number", number);
		}
		let key = target.cache_key()?;
		let now = now_ms();
		let cached = self.cache.get(&key, now).map_err(cache_fault)?;
		if cached
			.as_ref()
			.is_some_and(|entry| entry.status == GithubCacheStatus::Fresh)
		{
			return Ok(cached.expect("fresh entry").body.to_vec());
		}
		match self
			.fetch(&target, cached.as_ref().and_then(|entry| entry.etag.as_deref()))
			.await
		{
			Ok(Fetch::NotModified) => {
				self.cache.touch(&key, now).map_err(cache_fault)?;
				Ok(cached.expect("304 requires cached entity").body.to_vec())
			},
			Ok(Fetch::Body { body, etag }) => {
				let comments = if target.comments_enabled() {
					match self.fetch_comments(&target).await {
						Ok(comments) => Some(comments),
						Err(error) => {
							tracing::warn!(
								error = ?error,
								cached = cached.is_some(),
								"GitHub comments refresh failed",
							);
							if let Some(cached) = &cached {
								let mut warning =
									b"> WARNING: Live GitHub comments refresh failed; cached content may be stale.\n\n"
										.to_vec();
								warning.extend_from_slice(&cached.body);
								return Ok(warning);
							}
							return Err(error);
						},
					}
				} else {
					None
				};
				let rendered = target.render(&body, comments.as_deref())?;
				self
					.cache
					.put(&key, &rendered, etag.as_deref(), now)
					.map_err(cache_fault)?;
				Ok(rendered)
			},
			Err(error) => {
				tracing::warn!(
					error = ?error,
					cached = cached.is_some(),
					"GitHub resource refresh failed",
				);
				if let Some(cached) = cached {
					let mut warning =
						b"> WARNING: Live GitHub refresh failed; cached content may be stale.\n\n"
							.to_vec();
					warning.extend_from_slice(&cached.body);
					Ok(warning)
				} else {
					Err(error)
				}
			},
		}
	}

	#[tracing::instrument(
		name = "github_resource_fetch",
		level = "debug",
		skip_all,
		fields(scheme = ?target.scheme, repo = %target.repo, number = ?target.number),
	)]
	async fn fetch(&self, target: &Target, etag: Option<&str>) -> Result<Fetch, Fault> {
		let mut headers = HeaderMap::new();
		headers.insert(USER_AGENT, HeaderValue::from_static("omp/issue-pr-resolver"));
		headers.insert("x-github-api-version", HeaderValue::from_static("2022-11-28"));
		headers.insert(ACCEPT, HeaderValue::from_static(target.accept()));
		if let Some(lease) = self.credentials.lease().await? {
			lease
				.apply_header(&HeaderPlacement::bearer(), &mut headers)
				.map_err(|_| Fault::Source { message: sf!("GitHub credential projection failed.") })?;
		}
		if let Some(etag) = etag {
			headers.insert(
				IF_NONE_MATCH,
				HeaderValue::from_str(etag).map_err(|_| invalid("Invalid cached GitHub ETag."))?,
			);
		}
		let response = self
			.client
			.get(target.api_url())
			.headers(headers)
			.send()
			.await
			.map_err(http_fault)?;
		if response.status().as_u16() == 304 {
			return Ok(Fetch::NotModified);
		}
		let status = response.status().as_u16();
		let etag = response
			.headers()
			.get(ETAG)
			.and_then(|value| value.to_str().ok())
			.map(Str::new);
		let mut stream = response.bytes_stream();
		let mut bytes = BytesMut::new();
		while let Some(chunk) = stream.next().await {
			let chunk = chunk.map_err(http_fault)?;
			if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
				return Err(invalid("GitHub response exceeds 8 MiB."));
			}
			bytes.extend_from_slice(&chunk);
		}
		if !(200..300).contains(&status) {
			return Err(Fault::Source {
				message: Str::new(format!("GitHub API returned HTTP {status}.")),
			});
		}
		Ok(Fetch::Body { body: bytes.freeze().to_vec(), etag })
	}

	#[tracing::instrument(
		name = "github_comments_fetch",
		level = "debug",
		skip_all,
		fields(scheme = ?target.scheme, repo = %target.repo, number = ?target.number),
	)]
	async fn fetch_comments(&self, target: &Target) -> Result<Vec<Value>, Fault> {
		let mut comments = Vec::new();
		let mut retained_bytes = 0usize;
		let lease = self.credentials.lease().await?;
		for page in 1..=100u32 {
			let mut headers = HeaderMap::new();
			headers.insert(USER_AGENT, HeaderValue::from_static("omp/issue-pr-resolver"));
			headers.insert("x-github-api-version", HeaderValue::from_static("2022-11-28"));
			headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github+json"));
			if let Some(lease) = &lease {
				lease
					.apply_header(&HeaderPlacement::bearer(), &mut headers)
					.map_err(|_| Fault::Source {
						message: sf!("GitHub credential projection failed."),
					})?;
			}
			let response = self
				.client
				.get(target.comments_url(page))
				.headers(headers)
				.send()
				.await
				.map_err(http_fault)?;
			let status = response.status().as_u16();
			if !(200..300).contains(&status) {
				return Err(Fault::Source {
					message: Str::new(format!("GitHub comments API returned HTTP {status}.")),
				});
			}
			let mut stream = response.bytes_stream();
			let mut bytes = BytesMut::new();
			while let Some(chunk) = stream.next().await {
				let chunk = chunk.map_err(http_fault)?;
				retained_bytes = retained_bytes.saturating_add(chunk.len());
				if retained_bytes > MAX_BODY {
					return Err(invalid("GitHub comments exceed 8 MiB."));
				}
				bytes.extend_from_slice(&chunk);
			}
			let page_comments: Vec<Value> = serde_json::from_slice(&bytes).map_err(|error| {
				Fault::Invalid { message: Str::new(format!("Invalid GitHub comments JSON: {error}")) }
			})?;
			let complete = page_comments.len() < 100;
			comments.extend(page_comments);
			if complete {
				return Ok(comments);
			}
		}
		Err(invalid("GitHub comments exceed 100 pages."))
	}
}

impl Resolve for GithubResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let bytes = self.resolve(resource, None).await?;
		tool_url::select_bytes(&Default::default(), resource, CowBytes::from(bytes), selector)
	}

	async fn read_query<'a>(
		&'a self,
		resource: &'a str,
		query: Option<&'a str>,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let bytes = self.resolve(resource, query).await?;
		tool_url::select_bytes(&Default::default(), resource, CowBytes::from(bytes), selector)
	}
}

impl fmt::Debug for GithubResolver {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("GithubResolver(..)")
	}
}

enum Fetch {
	NotModified,
	Body { body: Vec<u8>, etag: Option<Str> },
}

#[derive(Clone, Debug)]
enum View {
	List { state: Str, limit: u32, author: Option<Str>, label: Option<Str> },
	Detail { comments: bool },
	Diff { mode: DiffMode },
}
#[derive(Clone, Debug)]
enum DiffMode {
	Index,
	All,
	Slice(usize),
}
#[derive(Clone, Debug)]
struct Target {
	scheme: GithubScheme,
	repo:   Str,
	number: Option<u64>,
	view:   View,
}

impl Target {
	fn parse(
		scheme: GithubScheme,
		resource: &str,
		query: Option<&str>,
		root: &Path,
	) -> Result<Self, Fault> {
		let parts = resource
			.split('/')
			.filter(|part| !part.is_empty())
			.collect::<Vec<_>>();
		if parts.iter().any(|part| matches!(*part, "." | "..")) {
			return Err(invalid("Unsafe GitHub resource path."));
		}
		let (repo, number, trailing) = match parts.as_slice() {
			[] => (infer_repo(root)?, None, &[][..]),
			[number] if number.bytes().all(|b| b.is_ascii_digit()) => {
				(infer_repo(root)?, Some(parse_number(number)?), &[][..])
			},
			[owner, repo] => (Str::new(format!("{owner}/{repo}")), None, &[][..]),
			[owner, repo, number, tail @ ..] => {
				(Str::new(format!("{owner}/{repo}")), Some(parse_number(number)?), tail)
			},
			_ => return Err(invalid("Expected issue://<n>, issue://owner/repo/<n>, or a repo list.")),
		};
		let params = query
			.map(|query| {
				url::form_urlencoded::parse(query.as_bytes())
					.into_owned()
					.collect::<Vec<_>>()
			})
			.unwrap_or_default();
		let get = |name: &str| {
			params
				.iter()
				.find(|(key, _)| key == name)
				.map(|(_, value)| value.as_str())
		};
		let view = if !trailing.is_empty() {
			if scheme != GithubScheme::PullRequest
				|| trailing.first() != Some(&"diff")
				|| trailing.len() > 2
			{
				return Err(invalid("Only pr:// resources support /diff[/all|<index>]."));
			}
			let mode = match trailing.get(1).copied() {
				None => DiffMode::Index,
				Some("all") => DiffMode::All,
				Some(value) => DiffMode::Slice(
					value
						.parse()
						.map_err(|_| invalid("Diff index must be positive."))?,
				),
			};
			View::Diff { mode }
		} else if number.is_some() {
			View::Detail { comments: !matches!(get("comments"), Some("0" | "false")) }
		} else {
			let state = Str::new(get("state").unwrap_or("open"));
			if !matches!(state.as_str(), "open" | "closed" | "merged" | "all") {
				return Err(invalid("Invalid GitHub list state."));
			}
			let limit = get("limit")
				.map_or(Ok(30), str::parse::<u32>)
				.map_err(|_| invalid("Invalid GitHub list limit."))?
				.clamp(1, 100);
			View::List {
				state,
				limit,
				author: get("author").map(Str::new),
				label: get("label").map(Str::new),
			}
		};
		Ok(Self { scheme, repo, number, view })
	}

	fn cache_key(&self) -> Result<GithubCacheKey, Fault> {
		GithubCacheKey::new(
			match &self.view {
				View::Diff { .. } => GithubResourceKind::Diff,
				_ if self.scheme == GithubScheme::Issue => GithubResourceKind::Issue,
				_ => GithubResourceKind::PullRequest,
			},
			self.repo.clone(),
			self.number,
			self.view_key(),
		)
		.map_err(cache_fault)
	}

	fn view_key(&self) -> Str {
		Str::new(format!("{:?}", self.view))
	}

	fn accept(&self) -> &'static str {
		if matches!(self.view, View::Diff { .. }) {
			"application/vnd.github.v3.diff"
		} else {
			"application/vnd.github+json"
		}
	}

	fn api_url(&self) -> String {
		let base = format!("https://api.github.com/repos/{}", self.repo);
		match &self.view {
			View::List { state, limit, author, label } => {
				let family = if self.scheme == GithubScheme::Issue {
					"issues"
				} else {
					"pulls"
				};
				let mut url = format!(
					"{base}/{family}?state={}&per_page={limit}",
					if state == "merged" {
						"closed"
					} else {
						state.as_str()
					}
				);
				if let Some(author) = author {
					url.push_str("&creator=");
					url.push_str(author);
				}
				if let Some(label) = label {
					url.push_str("&labels=");
					url.push_str(label);
				}
				url
			},
			View::Detail { .. } => format!(
				"{base}/{}/{}",
				if self.scheme == GithubScheme::Issue {
					"issues"
				} else {
					"pulls"
				},
				self.number.expect("detail number")
			),
			View::Diff { .. } => format!("{base}/pulls/{}", self.number.expect("diff number")),
		}
	}

	fn comments_enabled(&self) -> bool {
		matches!(self.view, View::Detail { comments: true })
	}

	fn comments_url(&self, page: u32) -> String {
		format!(
			"https://api.github.com/repos/{}/issues/{}/comments?per_page=100&page={page}",
			self.repo,
			self.number.expect("comments require a detail number"),
		)
	}

	fn render(&self, body: &[u8], comments: Option<&[Value]>) -> Result<Vec<u8>, Fault> {
		if let View::Diff { mode } = &self.view {
			let text = str::from_utf8(body).map_err(|_| invalid("GitHub diff is not UTF-8."))?;
			return render_diff(text, mode, &self.repo, self.number.expect("diff number"));
		}
		let value: Value = serde_json::from_slice(body).map_err(|error| Fault::Invalid {
			message: Str::new(format!("Invalid GitHub JSON: {error}")),
		})?;
		let mut out = String::new();
		match (&self.view, value) {
			(View::List { .. }, Value::Array(items)) => {
				out.push_str(if self.scheme == GithubScheme::Issue {
					"# Issues\n\n"
				} else {
					"# Pull Requests\n\n"
				});
				for item in items {
					if self.scheme == GithubScheme::Issue && item.get("pull_request").is_some() {
						continue;
					}
					render_item(&mut out, &item, &self.repo, self.scheme);
				}
			},
			(View::Detail { comments: include_comments }, item) => {
				out.push_str("# ");
				out.push_str(
					item
						.get("title")
						.and_then(Value::as_str)
						.unwrap_or("(no title)"),
				);
				out.push_str("\n\n");
				out.push_str(item.get("body").and_then(Value::as_str).unwrap_or(""));
				if !include_comments {
					out.push_str("\n\n_Comments disabled._");
				} else if let Some(comments) = comments {
					render_comments(&mut out, comments);
				}
			},
			_ => return Err(invalid("Unexpected GitHub response shape.")),
		}
		Ok(out.into_bytes())
	}
}

fn render_comments(out: &mut String, comments: &[Value]) {
	out.push_str("\n\n## Comments\n");
	if comments.is_empty() {
		out.push_str("\n_No comments._\n");
		return;
	}
	for comment in comments {
		let author = comment
			.get("user")
			.and_then(|user| user.get("login"))
			.and_then(Value::as_str)
			.unwrap_or("unknown");
		let created = comment
			.get("created_at")
			.and_then(Value::as_str)
			.unwrap_or("unknown time");
		out.push_str("\n### @");
		out.push_str(author);
		out.push_str(" — ");
		out.push_str(created);
		out.push_str("\n\n");
		out.push_str(comment.get("body").and_then(Value::as_str).unwrap_or(""));
		out.push('\n');
	}
}

fn render_item(out: &mut String, item: &Value, repo: &str, scheme: GithubScheme) {
	let number = item
		.get("number")
		.and_then(Value::as_u64)
		.unwrap_or_default();
	let state = item.get("state").and_then(Value::as_str).unwrap_or("?");
	let title = item
		.get("title")
		.and_then(Value::as_str)
		.unwrap_or("(no title)");
	out.push_str(&format!(
		"- [{state}] #{number} {title}\n  {}://{repo}/{number}\n",
		if scheme == GithubScheme::Issue {
			"issue"
		} else {
			"pr"
		}
	));
}
fn render_diff(text: &str, mode: &DiffMode, repo: &str, number: u64) -> Result<Vec<u8>, Fault> {
	if matches!(mode, DiffMode::All) {
		return Ok(text.as_bytes().to_vec());
	}
	let starts = text
		.match_indices("diff --git ")
		.map(|(index, _)| index)
		.collect::<Vec<_>>();
	if let DiffMode::Slice(index) = mode {
		if *index == 0 || *index > starts.len() {
			return Err(invalid("Diff file index is out of range."));
		}
		let start = starts[index - 1];
		let end = starts.get(*index).copied().unwrap_or(text.len());
		return Ok(text.as_bytes()[start..end].to_vec());
	}
	let mut out = format!("# Pull Request Diff: {repo}#{number}\n\n");
	for (index, start) in starts.iter().enumerate() {
		let line = text[*start..].lines().next().unwrap_or("diff --git");
		out.push_str(&format!(
			"{}. {}\n   pr://{repo}/{number}/diff/{}\n",
			index + 1,
			line.trim_start_matches("diff --git "),
			index + 1
		));
	}
	Ok(out.into_bytes())
}
pub(super) fn infer_repo(root: &Path) -> Result<Str, Fault> {
	let config = fs::read_to_string(root.join(".git/config"))
		.map_err(|_| invalid("Cannot infer GitHub repo; use owner/repo explicitly."))?;
	let url = config
		.lines()
		.map(str::trim)
		.find_map(|line| line.strip_prefix("url = "))
		.ok_or_else(|| invalid("Git origin URL is missing."))?;
	let path = url
		.strip_prefix("git@github.com:")
		.or_else(|| url.strip_prefix("https://github.com/"))
		.ok_or_else(|| invalid("Origin is not GitHub."))?
		.trim_end_matches(".git");
	if path.split('/').count() != 2 {
		return Err(invalid("GitHub origin is not owner/repo."));
	}
	Ok(Str::new(path))
}
fn parse_number(value: &str) -> Result<u64, Fault> {
	value
		.parse::<u64>()
		.ok()
		.filter(|n| *n > 0)
		.ok_or_else(|| invalid("GitHub number must be positive."))
}
fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
fn invalid(message: &'static str) -> Fault {
	Fault::Invalid { message: Str::new_static(message) }
}
fn cache_fault(error: impl Display) -> Fault {
	Fault::Source { message: Str::new(error.to_string()) }
}
fn http_fault(error: impl Display) -> Fault {
	Fault::Source { message: Str::new(format!("GitHub API request failed: {error}")) }
}
