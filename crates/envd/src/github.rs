//! Direct GitHub API and isolated pull-request worktree host.

use std::{
	fs, io,
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::BytesMut;
use futures::StreamExt as _;
use http::{
	HeaderMap, HeaderValue,
	header::{ACCEPT, USER_AGENT},
};
use omp_core::{Str, sf};
use omp_inference::auth::HeaderPlacement;
use omp_tools::github::{DateField, Fault, GithubHost, Operation, Params, Payload};
use omp_vcs::{PushOptions, git::GitRepo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{task, time};
use tokio_util::sync::CancellationToken;

use super::github_url::{self, GithubCredentialBridge, GithubRepo};

const MAX_BODY: usize = 16 * 1024 * 1024;

/// Combined-credential GitHub owner.
pub(crate) struct GithubService {
	root:        PathBuf,
	worktrees:   PathBuf,
	credentials: Arc<GithubCredentialBridge>,
	client:      omp_http::Client,
}

impl GithubService {
	pub(crate) fn new(
		root: PathBuf,
		state_dir: &Path,
		credentials: Arc<GithubCredentialBridge>,
	) -> Arc<Self> {
		Arc::new(Self {
			root,
			worktrees: state_dir.join("github-worktrees"),
			credentials,
			client: omp_http::no_redirect_client(),
		})
	}

	#[tracing::instrument(
		name = "github_api_request",
		level = "debug",
		skip_all,
		fields(
			method = ?method,
			path = %api_path(path),
		),
	)]
	async fn request(
		&self,
		host: &str,
		method: Method,
		path: &str,
		body: Option<&Value>,
		cancellation: &CancellationToken,
	) -> Result<ApiResponse, Fault> {
		let mut headers = HeaderMap::new();
		headers.insert(USER_AGENT, HeaderValue::from_static("omp-github-device"));
		headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github+json"));
		headers.insert("x-github-api-version", HeaderValue::from_static("2022-11-28"));
		if let Some(lease) = tokio::select! {
			result = self.credentials.lease() => result,
			() = cancellation.cancelled() => return Err(cancelled_fault()),
		}
		.map_err(|error| Fault {
			code:    sf!("github_credentials_failed"),
			message: Str::new(error.message().clone()),
		})? {
			lease
				.apply_header(&HeaderPlacement::bearer(), &mut headers)
				.map_err(|_| Fault {
					code:    sf!("github_credentials_failed"),
					message: sf!("GitHub credential projection failed"),
				})?;
		}
		let url = github_url::api_url_for_host(host, path);
		let request = match method {
			Method::Get => self.client.get(url),
			Method::Post => self.client.post(url),
		};
		let request = if let Some(body) = body {
			request.json(body)
		} else {
			request
		};
		let response = tokio::select! {
			result = request.headers(headers).send() => result.map_err(http_fault)?,
			() = cancellation.cancelled() => return Err(cancelled_fault()),
		};
		let status = response.status().as_u16();
		let remaining = response
			.headers()
			.get("x-ratelimit-remaining")
			.and_then(|value| value.to_str().ok())
			.and_then(|value| value.parse().ok());
		let reset = response
			.headers()
			.get("x-ratelimit-reset")
			.and_then(|value| value.to_str().ok())
			.and_then(|value| value.parse().ok());
		let mut bytes = BytesMut::new();
		let mut stream = response.bytes_stream();
		while let Some(chunk) = tokio::select! {
			chunk = stream.next() => chunk,
			() = cancellation.cancelled() => return Err(cancelled_fault()),
		} {
			let chunk = chunk.map_err(http_fault)?;
			if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
				return Err(fault("github_response_too_large", "GitHub response exceeds 16 MiB"));
			}
			bytes.extend_from_slice(&chunk);
		}
		if !(200..300).contains(&status) {
			let message = serde_json::from_slice::<Value>(&bytes)
				.ok()
				.and_then(|value| {
					value
						.get("message")
						.and_then(Value::as_str)
						.map(str::to_owned)
				})
				.unwrap_or_else(|| format!("GitHub API returned HTTP {status}"));
			return Err(Fault { code: sf!("github_http_error"), message: Str::new(message) });
		}
		let value = if bytes.is_empty() {
			Value::Null
		} else {
			serde_json::from_slice(&bytes)
				.map_err(|_| fault("github_invalid_response", "GitHub returned malformed JSON"))?
		};
		Ok(ApiResponse { value, remaining, reset })
	}

	fn repo(&self, requested: Option<&str>) -> Result<GithubRepo, Fault> {
		requested.map_or_else(
			|| {
				github_url::infer_repo(&self.root).map_err(|error| Fault {
					code:    sf!("github_repo_unresolved"),
					message: Str::new(error.message().clone()),
				})
			},
			|repo| {
				GithubRepo::parse(repo).map_err(|error| Fault {
					code:    sf!("github_invalid_repo"),
					message: Str::new(error.message().clone()),
				})
			},
		)
	}
}

#[async_trait]
impl GithubHost for GithubService {
	#[tracing::instrument(name = "github_operation", level = "debug", skip_all, fields(operation = ?params.op))]
	async fn execute(
		&self,
		params: Params,
		cancellation: CancellationToken,
	) -> Result<Payload, Fault> {
		let response = match params.op {
			Operation::RepoView => {
				let repo = self.repo(params.repo.as_deref())?;
				self
					.request(
						repo.host(),
						Method::Get,
						&format!("/repos/{}", repo.slug()),
						None,
						&cancellation,
					)
					.await?
			},
			Operation::FileRead => {
				let repo = self.repo(params.repo.as_deref())?;
				let path = required(params.path.as_deref(), "file_read requires `path`")?;
				let endpoint = file_endpoint(repo.slug(), path, params.branch.as_deref())?;
				let mut response = self
					.request(repo.host(), Method::Get, &endpoint, None, &cancellation)
					.await?;
				response.value = decode_file_response(&response.value, repo.identity(), path)?;
				response
			},
			Operation::PrCreate => {
				let repo = self.repo(params.repo.as_deref())?;
				let body = json!({
					"title": required(params.title.as_deref(), "pr_create requires `title`")?,
					"body": params.body.as_deref().unwrap_or(""),
					"head": required(params.head.as_deref(), "pr_create requires `head`")?,
					"base": params.base.as_deref().unwrap_or("main"),
					"draft": params.draft,
				});
				self
					.request(
						repo.host(),
						Method::Post,
						&format!("/repos/{}/pulls", repo.slug()),
						Some(&body),
						&cancellation,
					)
					.await?
			},
			Operation::PrCheckout => self.checkout(&params, &cancellation).await?,
			Operation::PrPush => self.push(&params, &cancellation).await?,
			Operation::SearchIssues
			| Operation::SearchPrs
			| Operation::SearchCode
			| Operation::SearchCommits
			| Operation::SearchRepos => self.search(&params, &cancellation).await?,
			Operation::RunWatch => self.watch(&params, &cancellation).await?,
		};
		Ok(Payload {
			op:                   params.op,
			result:               response.value,
			rate_limit_remaining: response.remaining,
			rate_limit_reset:     response.reset,
		})
	}
}

impl GithubService {
	async fn search(
		&self,
		params: &Params,
		cancellation: &CancellationToken,
	) -> Result<ApiResponse, Fault> {
		let (kind, tag) = match params.op {
			Operation::SearchIssues => ("issues", Some("is:issue")),
			Operation::SearchPrs => ("issues", Some("is:pr")),
			Operation::SearchCode => ("code", None),
			Operation::SearchCommits => ("commits", None),
			Operation::SearchRepos => ("repositories", None),
			_ => unreachable!(),
		};
		let repo = if params.op == Operation::SearchRepos && params.repo.is_none() {
			None
		} else {
			Some(self.repo(params.repo.as_deref())?)
		};
		if params.op == Operation::SearchCode
			&& (params.since.is_some() || params.until.is_some() || params.date_field.is_some())
		{
			return Err(fault(
				"github_invalid_search",
				"code search does not support date bounds or dateField",
			));
		}
		let mut query = params.query.as_deref().unwrap_or("").trim().to_owned();
		if let Some(tag) = tag {
			append_query_part(&mut query, tag);
		}
		if params.op != Operation::SearchRepos && !has_scope(&query) {
			append_query_part(
				&mut query,
				&format!("repo:{}", repo.as_ref().expect("scoped search repository").slug()),
			);
		}
		if let Some(qualifier) = search_date_qualifier(params, SystemTime::now())? {
			append_query_part(&mut query, &qualifier);
		}
		if query.is_empty() {
			return Err(fault("github_invalid_search", "search requires `query` or a date bound"));
		}
		let encoded = encode_query(&query);
		let limit = params.limit.unwrap_or(30).clamp(1, 100);
		self
			.request(
				repo
					.as_ref()
					.map_or(github_url::GITHUB_HOST, GithubRepo::host),
				Method::Get,
				&format!("/search/{kind}?q={encoded}&per_page={limit}"),
				None,
				cancellation,
			)
			.await
	}

	async fn resolve_pr(
		&self,
		repo: &GithubRepo,
		selector: &str,
		cancellation: &CancellationToken,
	) -> Result<(u64, ApiResponse), Fault> {
		let number = if let Some(number) = parse_pr_number(selector, repo)? {
			number
		} else {
			let branch = selector.trim();
			if branch.is_empty() || branch.starts_with('-') {
				return Err(fault(
					"github_invalid_pr",
					"pull request must be a positive number, URL, or branch name",
				));
			}
			let owner = repo.slug().split_once('/').expect("validated repository").0;
			let head = if branch.contains(':') {
				branch.to_owned()
			} else {
				format!("{owner}:{branch}")
			};
			let endpoint = pr_branch_endpoint(repo.slug(), &head);
			let matches = self
				.request(repo.host(), Method::Get, &endpoint, None, cancellation)
				.await?;
			let rows = matches.value.as_array().ok_or_else(|| {
				fault("github_invalid_response", "GitHub pull request lookup was not a list")
			})?;
			let mut numbers = rows
				.iter()
				.filter_map(|row| row.get("number").and_then(Value::as_u64))
				.collect::<Vec<_>>();
			if numbers.is_empty() {
				for page in 1..=100u32 {
					let response = self
						.request(
							repo.host(),
							Method::Get,
							&pr_list_endpoint(repo.slug(), page),
							None,
							cancellation,
						)
						.await?;
					let rows = response.value.as_array().ok_or_else(|| {
						fault("github_invalid_response", "GitHub pull request lookup was not a list")
					})?;
					numbers.extend(rows.iter().filter_map(|row| {
						let matches_branch = row.pointer("/head/ref").and_then(Value::as_str)
							== Some(branch)
							|| row.pointer("/head/label").and_then(Value::as_str) == Some(branch);
						matches_branch
							.then(|| row.get("number").and_then(Value::as_u64))
							.flatten()
					}));
					if rows.len() < 100 {
						break;
					}
				}
			}
			numbers.sort_unstable();
			numbers.dedup();
			let Some(number) = numbers.first().copied() else {
				return Err(fault(
					"github_invalid_pr",
					"no open pull request has the requested head branch",
				));
			};
			if numbers.len() > 1 {
				return Err(fault(
					"github_invalid_pr",
					"multiple open pull requests have the requested head branch",
				));
			}
			number
		};
		let response = self
			.request(
				repo.host(),
				Method::Get,
				&format!("/repos/{}/pulls/{number}", repo.slug()),
				None,
				cancellation,
			)
			.await?;
		Ok((number, response))
	}

	async fn checkout(
		&self,
		params: &Params,
		cancellation: &CancellationToken,
	) -> Result<ApiResponse, Fault> {
		let repo = self.repo(params.repo.as_deref())?;
		let prs = params
			.pr
			.as_ref()
			.map(omp_tools::github::PrSelector::as_slice)
			.ok_or_else(|| fault("github_invalid_pr", "pr_checkout requires `pr`"))?;
		if prs.is_empty() {
			return Err(fault("github_invalid_pr", "pr_checkout requires at least one `pr`"));
		}
		let mut checkouts = Vec::new();
		for value in prs {
			let (number, api) = self.resolve_pr(&repo, value, cancellation).await?;
			let clone_url = api
				.value
				.pointer("/head/repo/clone_url")
				.and_then(Value::as_str)
				.ok_or_else(|| {
					fault("github_invalid_response", "pull request head repository is unavailable")
				})?;
			let head = api
				.value
				.pointer("/head/ref")
				.and_then(Value::as_str)
				.ok_or_else(|| {
					fault("github_invalid_response", "pull request head branch is unavailable")
				})?;
			let path = self.worktrees.join(format!("pr-{number}"));
			let force = params.force;
			tokio::select! {
				result = checkout_git(
					&self.root,
					&path,
					number,
					clone_url,
					head,
					force,
					cancellation.clone(),
				) => result?,
				() = cancellation.cancelled() => return Err(cancelled_fault()),
			}
			fs::create_dir_all(&path).map_err(io_fault)?;
			let metadata = CheckoutMetadata {
				repo:      repo.identity().to_owned(),
				clone_url: clone_url.to_owned(),
				head:      head.to_owned(),
			};
			fs::write(
				path.join(".omp-pr-checkout.json"),
				serde_json::to_vec(&metadata).expect("metadata serializes"),
			)
			.map_err(io_fault)?;
			checkouts.push(json!({ "pr": number, "branch": format!("pr-{number}"), "path": path }));
		}
		Ok(ApiResponse {
			value:     json!({ "checkouts": checkouts }),
			remaining: None,
			reset:     None,
		})
	}

	async fn push(
		&self,
		params: &Params,
		cancellation: &CancellationToken,
	) -> Result<ApiResponse, Fault> {
		let repo = self.repo(params.repo.as_deref())?;
		let prs = params
			.pr
			.as_ref()
			.map(omp_tools::github::PrSelector::as_slice)
			.ok_or_else(|| {
				fault("github_invalid_pr", "pr_push requires `pr` from a prior checkout")
			})?;
		if prs.is_empty() {
			return Err(fault("github_invalid_pr", "pr_push requires at least one `pr`"));
		}
		let mut pushed = Vec::new();
		for value in prs {
			let number = if let Some(number) = parse_pr_number(value, &repo)? {
				number
			} else {
				self.resolve_pr(&repo, value, cancellation).await?.0
			};
			let path = self.worktrees.join(format!("pr-{number}"));
			let metadata: CheckoutMetadata = serde_json::from_slice(
				&fs::read(path.join(".omp-pr-checkout.json")).map_err(io_fault)?,
			)
			.map_err(|_| {
				fault("github_checkout_missing", "pull request checkout metadata is invalid")
			})?;
			let force = params.force_with_lease;
			tokio::select! {
				result = push_git(path.clone(), &metadata, force, cancellation.clone()) => result?,
				() = cancellation.cancelled() => return Err(cancelled_fault()),
			}
			pushed.push(json!({ "pr": number, "path": path }));
		}
		Ok(ApiResponse { value: json!({ "pushed": pushed }), remaining: None, reset: None })
	}

	async fn watch(
		&self,
		params: &Params,
		cancellation: &CancellationToken,
	) -> Result<ApiResponse, Fault> {
		let repo = self.repo(params.repo.as_deref())?;
		let target = if let Some(run) = &params.run {
			WatchTarget::Run(run_id(run)?)
		} else if let Some(branch) = &params.branch {
			let branch_endpoint = branch_endpoint(repo.slug(), branch);
			let response = self
				.request(repo.host(), Method::Get, &branch_endpoint, None, cancellation)
				.await?;
			let head_sha = response
				.value
				.pointer("/commit/sha")
				.and_then(Value::as_str)
				.ok_or_else(|| fault("github_invalid_response", "GitHub branch has no head commit"))?;
			WatchTarget::Commit { branch: branch.clone(), head_sha: Str::new(head_sha) }
		} else {
			let current_repo = self.repo(None)?;
			if !current_repo
				.identity()
				.eq_ignore_ascii_case(repo.identity())
			{
				return Err(fault(
					"github_repo_mismatch",
					"current checkout does not match `repo`; pass `branch` or `run`",
				));
			}
			let root = self.root.clone();
			let worker = task::spawn_blocking(move || current_git_snapshot(&root));
			let (branch, head_sha) = tokio::select! {
				result = worker => result
					.map_err(|_| fault("github_git_failed", "GitHub Git lookup worker failed"))??,
				() = cancellation.cancelled() => return Err(cancelled_fault()),
			};
			WatchTarget::Commit { branch, head_sha }
		};
		let mut receipt = None;
		for attempt in 0..100u32 {
			let mut response = match &target {
				WatchTarget::Run(id) => {
					let mut run = self
						.request(
							repo.host(),
							Method::Get,
							&format!("/repos/{}/actions/runs/{id}", repo.slug()),
							None,
							cancellation,
						)
						.await?;
					let jobs = self.fetch_run_jobs(&repo, *id, cancellation).await?;
					run.value = json!({ "run": run.value, "jobs": jobs });
					run
				},
				WatchTarget::Commit { branch, head_sha } => {
					let mut runs = self
						.request(
							repo.host(),
							Method::Get,
							&actions_runs_endpoint(repo.slug(), head_sha),
							None,
							cancellation,
						)
						.await?;
					self
						.attach_run_jobs(&repo, &mut runs.value, cancellation)
						.await?;
					if let Some(object) = runs.value.as_object_mut() {
						object.insert("branch".to_owned(), Value::String(branch.to_string()));
						object.insert("head_sha".to_owned(), Value::String(head_sha.to_string()));
					}
					runs
				},
			};
			let state = actions_state(&response.value);
			if let Some(object) = response.value.as_object_mut() {
				object.insert(
					"outcome".to_owned(),
					Value::String(
						match state {
							ActionsState::Pending => "pending",
							ActionsState::Success => "success",
							ActionsState::Failure => "failure",
						}
						.to_owned(),
					),
				);
			}
			receipt = Some(response);
			if state != ActionsState::Pending || attempt == 99 {
				break;
			}
			let delay = Duration::from_secs(if attempt < 20 { 3 } else { 15 });
			poll_sleep(delay, cancellation).await?;
		}
		receipt
			.ok_or_else(|| fault("github_actions_missing", "no GitHub Actions response was returned"))
	}

	async fn fetch_run_jobs(
		&self,
		repo: &GithubRepo,
		run_id: u64,
		cancellation: &CancellationToken,
	) -> Result<Value, Fault> {
		let mut jobs = Vec::new();
		for page in 1..=100u32 {
			let response = self
				.request(
					repo.host(),
					Method::Get,
					&run_jobs_endpoint(repo.slug(), run_id, page),
					None,
					cancellation,
				)
				.await?;
			let page_jobs = response
				.value
				.get("jobs")
				.and_then(Value::as_array)
				.ok_or_else(|| {
					fault("github_invalid_response", "GitHub Actions jobs were not a list")
				})?;
			let complete = page_jobs.len() < 100;
			jobs.extend(page_jobs.iter().cloned());
			if complete {
				return Ok(Value::Array(jobs));
			}
		}
		Err(fault("github_invalid_response", "GitHub Actions jobs exceeded 100 pages"))
	}

	async fn attach_run_jobs(
		&self,
		repo: &GithubRepo,
		value: &mut Value,
		cancellation: &CancellationToken,
	) -> Result<(), Fault> {
		let runs = value
			.get_mut("workflow_runs")
			.and_then(Value::as_array_mut)
			.ok_or_else(|| {
				fault("github_invalid_response", "GitHub Actions workflow runs were not a list")
			})?;
		for run in runs {
			let id = run.get("id").and_then(Value::as_u64).ok_or_else(|| {
				fault("github_invalid_response", "GitHub Actions run has no numeric id")
			})?;
			let jobs = self.fetch_run_jobs(repo, id, cancellation).await?;
			run.as_object_mut()
				.ok_or_else(|| {
					fault("github_invalid_response", "GitHub Actions run was not an object")
				})?
				.insert("jobs".to_owned(), jobs);
		}
		Ok(())
	}
}

#[derive(Clone, Copy, Debug)]
enum Method {
	Get,
	Post,
}
fn api_path(path: &str) -> &str {
	path.split_once('?').map_or(path, |(path, _)| path)
}
#[derive(Clone)]
enum WatchTarget {
	Run(u64),
	Commit { branch: Str, head_sha: Str },
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum ActionsState {
	Pending,
	Success,
	Failure,
}
struct ApiResponse {
	value:     Value,
	remaining: Option<u64>,
	reset:     Option<u64>,
}
#[derive(Deserialize, Serialize)]
struct CheckoutMetadata {
	repo:      String,
	clone_url: String,
	head:      String,
}

async fn checkout_git(
	root: &Path,
	path: &Path,
	number: u64,
	remote: &str,
	head: &str,
	force: bool,
	cancel: CancellationToken,
) -> Result<(), Fault> {
	let parent = path
		.parent()
		.ok_or_else(|| fault("github_git_failed", "invalid worktree path"))?;
	fs::create_dir_all(parent).map_err(io_fault)?;

	let root = root.to_owned();
	let repo = task::spawn_blocking(move || GitRepo::require(&root).map(Arc::new))
		.await
		.map_err(|_| fault("github_git_failed", "GitHub checkout worker failed"))?
		.map_err(git_fault)?;
	let fetch_branch = format!("omp/github-fetch/pr-{number}");
	let fetch_ref = format!("refs/heads/{fetch_branch}");
	repo
		.fetch(remote, head, &fetch_ref, None, Some(cancel))
		.await
		.map_err(git_fault)?;

	let path = path.to_owned();
	task::spawn_blocking(move || finish_checkout_git(repo, &path, number, &fetch_branch, force))
		.await
		.map_err(|_| fault("github_git_failed", "GitHub checkout worker failed"))?
}

fn finish_checkout_git(
	repo: Arc<GitRepo>,
	path: &Path,
	number: u64,
	fetch_branch: &str,
	force: bool,
) -> Result<(), Fault> {
	let branch = format!("pr-{number}");
	let fetch_ref = format!("refs/heads/{fetch_branch}");
	let fetched = repo
		.resolve_ref(&fetch_ref)
		.map_err(git_fault)?
		.ok_or_else(|| git_fault(omp_vcs::Error::RefNotFound { name: fetch_ref }))?;
	repo.delete_branch(fetch_branch, true).map_err(git_fault)?;

	if path.exists() {
		let active = GitRepo::require(path)
			.map_err(git_fault)?
			.current_branch()
			.map_err(git_fault)?;
		if active.as_deref() != Some(branch.as_str()) {
			return Err(fault(
				"github_git_failed",
				"existing pull-request worktree has an unexpected branch",
			));
		}
		return Ok(());
	}

	match repo
		.resolve_ref(&format!("refs/heads/{branch}"))
		.map_err(git_fault)?
	{
		Some(existing) if existing != fetched => {
			if !force {
				return Err(fault(
					"github_git_failed",
					"local pull-request branch differs from the remote head; pass force=true to reset \
					 it",
				));
			}
			repo
				.create_branch(&branch, &fetched, true)
				.map_err(git_fault)?;
		},
		None => repo
			.create_branch(&branch, &fetched, false)
			.map_err(git_fault)?,
		_ => {},
	}
	repo.worktree_add(path, &branch, false).map_err(git_fault)
}

async fn push_git(
	path: PathBuf,
	metadata: &CheckoutMetadata,
	force: bool,
	cancel: CancellationToken,
) -> Result<(), Fault> {
	let repo = task::spawn_blocking(move || GitRepo::require(&path).map(Arc::new))
		.await
		.map_err(|_| fault("github_git_failed", "GitHub push worker failed"))?
		.map_err(git_fault)?;
	repo
		.push(
			&PushOptions {
				remote:           Some(metadata.clone_url.clone()),
				refspec:          Some(format!("HEAD:{}", metadata.head)),
				force_with_lease: force,
			},
			Some(cancel),
		)
		.await
		.map_err(git_fault)
}

fn current_git_snapshot(root: &Path) -> Result<(Str, Str), Fault> {
	let repo = GitRepo::require(root).map_err(git_fault)?;
	let branch = repo.current_branch().map_err(git_fault)?.ok_or_else(|| {
		fault("github_git_failed", "current checkout is detached; pass `branch` or `run`")
	})?;
	if branch.is_empty() {
		return Err(fault(
			"github_git_failed",
			"current checkout is detached; pass `branch` or `run`",
		));
	}
	let head_sha = repo
		.head_sha()
		.map_err(git_fault)?
		.ok_or_else(|| git_fault(omp_vcs::Error::RefNotFound { name: "HEAD".to_owned() }))?;
	Ok((Str::new(branch), Str::new(head_sha)))
}
fn has_scope(query: &str) -> bool {
	query.split_whitespace().any(|part| {
		["repo:", "org:", "user:", "owner:"]
			.iter()
			.any(|prefix| part.starts_with(prefix))
	})
}
fn parse_pr_number(value: &str, repo: &GithubRepo) -> Result<Option<u64>, Fault> {
	let value = value.trim();
	if let Ok(number) = value.parse::<u64>() {
		return if number > 0 {
			Ok(Some(number))
		} else {
			Err(fault("github_invalid_pr", "pull request number must be positive"))
		};
	}
	if !value.contains("://") {
		return Ok(None);
	}
	let parsed = url::Url::parse(value)
		.map_err(|_| fault("github_invalid_pr", "pull request URL is not a valid GitHub URL"))?;
	let host = parsed.host_str().unwrap_or_default();
	let parts = parsed
		.path_segments()
		.map(|parts| parts.collect::<Vec<_>>())
		.unwrap_or_default();
	let (repo_host, owner, name, number) = match (host, parts.as_slice()) {
		("api.github.com", ["repos", owner, name, "pulls", number, ..]) => {
			("github.com", *owner, *name, *number)
		},
		(host, ["api", "v3", "repos", owner, name, "pulls", number, ..]) => {
			(host, *owner, *name, *number)
		},
		(host, [owner, name, "pull", number, ..]) if !host.is_empty() => {
			(host, *owner, *name, *number)
		},
		_ => {
			return Err(fault(
				"github_invalid_pr",
				"pull request URL must be a GitHub pull request URL",
			));
		},
	};
	let url_repo = GithubRepo::new(repo_host, owner, name)
		.map_err(|_| fault("github_invalid_pr", "pull request URL has an invalid repository"))?;
	if !url_repo.identity().eq_ignore_ascii_case(repo.identity()) {
		return Err(fault("github_invalid_pr", "pull request URL belongs to a different repository"));
	}
	number
		.parse::<u64>()
		.ok()
		.filter(|number| *number > 0)
		.map(Some)
		.ok_or_else(|| fault("github_invalid_pr", "pull request URL has no positive number"))
}
fn run_id(value: &str) -> Result<u64, Fault> {
	value
		.trim_end_matches('/')
		.rsplit('/')
		.next()
		.unwrap_or(value)
		.parse()
		.map_err(|_| fault("github_invalid_run", "Actions run must be an id or URL"))
}
fn actions_state(value: &Value) -> ActionsState {
	if let Some(run) = value.get("run") {
		return run_state(run, value.get("jobs").and_then(Value::as_array));
	}
	let Some(runs) = value.get("workflow_runs").and_then(Value::as_array) else {
		return ActionsState::Pending;
	};
	if runs.is_empty() {
		return ActionsState::Pending;
	}
	let mut pending = false;
	for run in runs {
		match run_state(run, run.get("jobs").and_then(Value::as_array)) {
			ActionsState::Failure => return ActionsState::Failure,
			ActionsState::Pending => pending = true,
			ActionsState::Success => {},
		}
	}
	if pending {
		ActionsState::Pending
	} else {
		ActionsState::Success
	}
}
fn run_state(run: &Value, jobs: Option<&Vec<Value>>) -> ActionsState {
	if let Some(jobs) = jobs {
		let mut pending = false;
		for job in jobs {
			if job.get("status").and_then(Value::as_str) != Some("completed") {
				pending = true;
				continue;
			}
			if conclusion_state(job.get("conclusion").and_then(Value::as_str)) == ActionsState::Failure
			{
				return ActionsState::Failure;
			}
		}
		if pending {
			return ActionsState::Pending;
		}
	}
	if run.get("status").and_then(Value::as_str) != Some("completed") {
		return ActionsState::Pending;
	}
	conclusion_state(run.get("conclusion").and_then(Value::as_str))
}
fn conclusion_state(conclusion: Option<&str>) -> ActionsState {
	match conclusion {
		Some("success" | "neutral" | "skipped") => ActionsState::Success,
		Some("failure" | "timed_out" | "cancelled" | "action_required" | "startup_failure") => {
			ActionsState::Failure
		},
		_ => ActionsState::Pending,
	}
}
fn file_endpoint(repo: &str, path: &str, branch: Option<&str>) -> Result<String, Fault> {
	if path.is_empty()
		|| path.starts_with('/')
		|| path
			.split('/')
			.any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
	{
		return Err(fault(
			"github_invalid_path",
			"file path must be a nonempty repository-relative file path",
		));
	}
	let encoded = path
		.split('/')
		.map(encode_path_segment)
		.collect::<Vec<_>>()
		.join("/");
	let mut endpoint = format!("/repos/{repo}/contents/{encoded}");
	if let Some(branch) = branch {
		endpoint.push_str("?ref=");
		endpoint.push_str(&encode_query(branch));
	}
	Ok(endpoint)
}
fn decode_file_response(value: &Value, repo: &str, path: &str) -> Result<Value, Fault> {
	if value.get("type").and_then(Value::as_str) != Some("file") {
		return Err(fault("github_invalid_content", "GitHub Contents response was not a file"));
	}
	if value.get("encoding").and_then(Value::as_str) != Some("base64") {
		return Err(fault(
			"github_invalid_content",
			"GitHub file content uses an unsupported encoding",
		));
	}
	let encoded = value
		.get("content")
		.and_then(Value::as_str)
		.ok_or_else(|| fault("github_invalid_content", "GitHub file response has no content"))?;
	let compact = encoded
		.bytes()
		.filter(|byte| !byte.is_ascii_whitespace())
		.collect::<Vec<_>>();
	let decoded = omp_core::base64::decode(&compact)
		.into_vec()
		.map_err(|_| fault("github_invalid_content", "GitHub file content was not valid base64"))?;
	Ok(json!({
		"repo": repo,
		"path": path,
		"content": String::from_utf8_lossy(&decoded),
		"size": decoded.len(),
	}))
}
fn pr_branch_endpoint(repo: &str, head: &str) -> String {
	format!("/repos/{repo}/pulls?state=open&head={}&per_page=100", encode_query(head),)
}
fn pr_list_endpoint(repo: &str, page: u32) -> String {
	format!("/repos/{repo}/pulls?state=open&per_page=100&page={page}")
}
fn branch_endpoint(repo: &str, branch: &str) -> String {
	format!("/repos/{repo}/branches/{}", encode_path_segment(branch))
}
fn actions_runs_endpoint(repo: &str, head_sha: &str) -> String {
	format!("/repos/{repo}/actions/runs?head_sha={}&per_page=100", encode_query(head_sha),)
}
async fn poll_sleep(delay: Duration, cancellation: &CancellationToken) -> Result<(), Fault> {
	tokio::select! {
		() = time::sleep(delay) => Ok(()),
		() = cancellation.cancelled() => Err(cancelled_fault()),
	}
}
fn run_jobs_endpoint(repo: &str, run_id: u64, page: u32) -> String {
	format!("/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100&page={page}")
}
fn encode_path_segment(value: &str) -> String {
	url::form_urlencoded::byte_serialize(value.as_bytes())
		.collect::<String>()
		.replace('+', "%20")
}
fn encode_query(value: &str) -> String {
	url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
fn append_query_part(query: &mut String, part: &str) {
	if !query.is_empty() {
		query.push(' ');
	}
	query.push_str(part);
}
fn search_date_qualifier(params: &Params, now: SystemTime) -> Result<Option<String>, Fault> {
	date_qualifier(
		params.op,
		params.date_field,
		params.since.as_deref(),
		params.until.as_deref(),
		now,
	)
}
fn date_qualifier(
	operation: Operation,
	requested: Option<DateField>,
	since: Option<&str>,
	until: Option<&str>,
	now: SystemTime,
) -> Result<Option<String>, Fault> {
	let field = match operation {
		Operation::SearchCommits => "committer-date",
		Operation::SearchRepos if requested == Some(DateField::Updated) => "pushed",
		_ if requested == Some(DateField::Updated) => "updated",
		_ => "created",
	};
	let since = since
		.map(|value| normalize_date_bound(value, now))
		.transpose()?;
	let until = until
		.map(|value| normalize_date_bound(value, now))
		.transpose()?;
	Ok(match (since, until) {
		(Some(since), Some(until)) => Some(format!("{field}:{since}..{until}")),
		(Some(since), None) => Some(format!("{field}:>={since}")),
		(None, Some(until)) => Some(format!("{field}:<={until}")),
		(None, None) => None,
	})
}
fn normalize_date_bound(raw: &str, now: SystemTime) -> Result<String, Fault> {
	let value = raw.trim();
	if value.is_empty() {
		return Err(fault("github_invalid_search", "date bound must not be empty"));
	}
	let digit_count = value
		.bytes()
		.take_while(|byte| byte.is_ascii_digit())
		.count();
	if digit_count > 0 {
		let count = value[..digit_count]
			.parse::<u64>()
			.map_err(|_| fault("github_invalid_search", "relative date bound is too large"))?;
		let unit = value[digit_count..].trim().to_ascii_lowercase();
		if matches!(unit.as_str(), "m" | "h" | "d" | "w") {
			let unit_seconds = match unit.as_str() {
				"m" => 60,
				"h" => 3_600,
				"d" => 86_400,
				"w" => 604_800,
				_ => unreachable!(),
			};
			let delta = count
				.checked_mul(unit_seconds)
				.ok_or_else(|| fault("github_invalid_search", "relative date bound is too large"))?;
			let now = epoch_seconds(now)?;
			let bound = now
				.checked_sub(
					i64::try_from(delta).map_err(|_| {
						fault("github_invalid_search", "relative date bound is too large")
					})?,
				)
				.ok_or_else(|| fault("github_invalid_search", "relative date bound is too large"))?;
			return format_epoch_date(bound);
		}
		if matches!(unit.as_str(), "mo" | "y") {
			let now = epoch_seconds(now)?;
			let (year, month, day) = civil_from_days(now.div_euclid(86_400));
			let count = i64::try_from(count)
				.map_err(|_| fault("github_invalid_search", "relative date bound is too large"))?;
			let target_days = if unit == "mo" {
				let month_index = i64::from(year)
					.checked_mul(12)
					.and_then(|value| value.checked_add(i64::from(month) - 1))
					.and_then(|value| value.checked_sub(count))
					.ok_or_else(|| fault("github_invalid_search", "relative date bound is too large"))?;
				let target_year = month_index.div_euclid(12);
				let target_month =
					u32::try_from(month_index.rem_euclid(12) + 1).expect("month is in range");
				days_from_civil(target_year, target_month, 1)
					.and_then(|value| value.checked_add(i64::from(day) - 1))
			} else {
				let target_year = i64::from(year)
					.checked_sub(count)
					.ok_or_else(|| fault("github_invalid_search", "relative date bound is too large"))?;
				days_from_civil(target_year, month, 1)
					.and_then(|value| value.checked_add(i64::from(day) - 1))
			}
			.ok_or_else(|| fault("github_invalid_search", "relative date bound is too large"))?;
			let (year, month, day) = civil_from_days(target_days);
			return format_date(year, month, day);
		}
	}
	if is_iso_date(value) {
		let (year, month, day) = parse_date(value)?;
		return format_date(year, month, day);
	}
	parse_iso_datetime(value)
}
fn epoch_seconds(time: SystemTime) -> Result<i64, Fault> {
	let seconds = time.duration_since(UNIX_EPOCH).map_err(|_| {
		fault("github_invalid_search", "date normalization time precedes the Unix epoch")
	})?;
	i64::try_from(seconds.as_secs())
		.map_err(|_| fault("github_invalid_search", "date normalization time is out of range"))
}
fn is_iso_date(value: &str) -> bool {
	value.len() == 10
		&& value.as_bytes()[4] == b'-'
		&& value.as_bytes()[7] == b'-'
		&& value
			.bytes()
			.enumerate()
			.all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}
fn parse_date(value: &str) -> Result<(i32, u32, u32), Fault> {
	if !is_iso_date(value) {
		return Err(invalid_date_bound());
	}
	let year = value[0..4]
		.parse::<i32>()
		.map_err(|_| invalid_date_bound())?;
	let month = value[5..7]
		.parse::<u32>()
		.map_err(|_| invalid_date_bound())?;
	let day = value[8..10]
		.parse::<u32>()
		.map_err(|_| invalid_date_bound())?;
	if year == 0 || day == 0 || day > days_in_month(year, month).unwrap_or(0) {
		return Err(invalid_date_bound());
	}
	Ok((year, month, day))
}
fn parse_iso_datetime(value: &str) -> Result<String, Fault> {
	if value.len() < 20 || value.as_bytes().get(10).copied() != Some(b'T') {
		return Err(invalid_date_bound());
	}
	let date = value.get(..10).ok_or_else(invalid_date_bound)?;
	let (year, month, day) = parse_date(date)?;
	let remainder = &value[11..];
	let (clock, offset_seconds) = if let Some(clock) = remainder
		.strip_suffix('Z')
		.or_else(|| remainder.strip_suffix('z'))
	{
		(clock, 0i64)
	} else {
		let split = remainder
			.rfind(|character| matches!(character, '+' | '-'))
			.ok_or_else(invalid_date_bound)?;
		let (clock, offset) = remainder.split_at(split);
		if offset.len() != 6 || offset.as_bytes()[3] != b':' {
			return Err(invalid_date_bound());
		}
		if !offset.as_bytes()[1..3]
			.iter()
			.chain(&offset.as_bytes()[4..6])
			.all(|byte| byte.is_ascii_digit())
		{
			return Err(invalid_date_bound());
		}
		let hours = offset[1..3]
			.parse::<i64>()
			.map_err(|_| invalid_date_bound())?;
		let minutes = offset[4..6]
			.parse::<i64>()
			.map_err(|_| invalid_date_bound())?;
		if hours > 23 || minutes > 59 {
			return Err(invalid_date_bound());
		}
		let seconds = hours * 3_600 + minutes * 60;
		(
			clock,
			if offset.starts_with('-') {
				-seconds
			} else {
				seconds
			},
		)
	};
	let mut clock_parts = clock.split(':');
	let hour = clock_parts
		.next()
		.and_then(|value| value.parse::<i64>().ok())
		.ok_or_else(invalid_date_bound)?;
	let minute = clock_parts
		.next()
		.and_then(|value| value.parse::<i64>().ok())
		.ok_or_else(invalid_date_bound)?;
	let seconds = clock_parts.next().ok_or_else(invalid_date_bound)?;
	if clock_parts.next().is_some() {
		return Err(invalid_date_bound());
	}
	let mut second_parts = seconds.split('.');
	let second = second_parts
		.next()
		.and_then(|value| value.parse::<i64>().ok())
		.ok_or_else(invalid_date_bound)?;
	if let Some(fraction) = second_parts.next() {
		if fraction.is_empty()
			|| !fraction.bytes().all(|byte| byte.is_ascii_digit())
			|| second_parts.next().is_some()
		{
			return Err(invalid_date_bound());
		}
	}
	if hour > 23 || minute > 59 || second > 59 {
		return Err(invalid_date_bound());
	}
	let days = days_from_civil(i64::from(year), month, day).ok_or_else(invalid_date_bound)?;
	let timestamp = days
		.checked_mul(86_400)
		.and_then(|value| value.checked_add(hour * 3_600 + minute * 60 + second))
		.and_then(|value| value.checked_sub(offset_seconds))
		.ok_or_else(invalid_date_bound)?;
	format_epoch_datetime(timestamp)
}
fn invalid_date_bound() -> Fault {
	fault(
		"github_invalid_search",
		"invalid date bound; expected a relative duration, ISO date, or ISO datetime",
	)
}
fn days_in_month(year: i32, month: u32) -> Option<u32> {
	Some(match month {
		1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
		4 | 6 | 9 | 11 => 30,
		2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
		2 => 28,
		_ => return None,
	})
}
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
	if !(1..=12).contains(&month) || day == 0 {
		return None;
	}
	let year = year - if month <= 2 { 1 } else { 0 };
	let era = if year >= 0 { year } else { year - 399 } / 400;
	let year_of_era = year - era * 400;
	let month = i64::from(month);
	let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
	let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
	era.checked_mul(146_097)?
		.checked_add(day_of_era)?
		.checked_sub(719_468)
}
fn civil_from_days(days: i64) -> (i32, u32, u32) {
	let days = days + 719_468;
	let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
	let day_of_era = days - era * 146_097;
	let year_of_era =
		(day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
	let mut year = year_of_era + era * 400;
	let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let month_prime = (5 * day_of_year + 2) / 153;
	let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
	let month = month_prime + if month_prime < 10 { 3 } else { -9 };
	year += if month <= 2 { 1 } else { 0 };
	(
		i32::try_from(year).unwrap_or(if year < 0 { i32::MIN } else { i32::MAX }),
		u32::try_from(month).expect("month is positive"),
		u32::try_from(day).expect("day is positive"),
	)
}
fn format_date(year: i32, month: u32, day: u32) -> Result<String, Fault> {
	if !(1..=9_999).contains(&year) {
		return Err(invalid_date_bound());
	}
	Ok(format!("{year:04}-{month:02}-{day:02}"))
}
fn format_epoch_date(seconds: i64) -> Result<String, Fault> {
	let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
	format_date(year, month, day)
}
fn format_epoch_datetime(seconds: i64) -> Result<String, Fault> {
	let days = seconds.div_euclid(86_400);
	let within_day = seconds.rem_euclid(86_400);
	let (year, month, day) = civil_from_days(days);
	if !(1..=9_999).contains(&year) {
		return Err(invalid_date_bound());
	}
	let hour = within_day / 3_600;
	let minute = within_day % 3_600 / 60;
	let second = within_day % 60;
	Ok(format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"))
}
fn required<'a>(value: Option<&'a str>, message: &'static str) -> Result<&'a str, Fault> {
	value.ok_or_else(|| fault("github_invalid_request", message))
}
fn fault(code: &'static str, message: &'static str) -> Fault {
	Fault { code: Str::new_static(code), message: Str::new_static(message) }
}
fn cancelled_fault() -> Fault {
	fault("github_cancelled", "GitHub operation was cancelled")
}
fn http_fault(error: reqwest::Error) -> Fault {
	Fault { code: sf!("github_transport_failed"), message: Str::new(error.to_string()) }
}
fn git_fault(error: omp_vcs::Error) -> Fault {
	Fault { code: sf!("github_git_failed"), message: Str::new(error.to_string()) }
}
fn io_fault(error: io::Error) -> Fault {
	Fault { code: sf!("github_io_failed"), message: Str::new(error.to_string()) }
}
#[cfg(test)]
mod tests {
	use std::time::Duration;

	use serde_json::json;
	use tokio_util::sync::CancellationToken;

	use super::{
		ActionsState, DateField, Operation, UNIX_EPOCH, actions_runs_endpoint, actions_state,
		branch_endpoint, date_qualifier, days_from_civil, decode_file_response, file_endpoint,
		normalize_date_bound, parse_pr_number, poll_sleep, pr_branch_endpoint, run_jobs_endpoint,
	};
	use crate::github_url::GithubRepo;

	fn instant(year: i64, month: u32, day: u32, hour: u64) -> std::time::SystemTime {
		let days = days_from_civil(year, month, day).expect("test date");
		UNIX_EPOCH
			+ Duration::from_secs(
				u64::try_from(days).expect("post-epoch date") * 86_400 + hour * 3_600,
			)
	}

	#[test]
	fn file_request_encodes_each_path_segment_once() {
		assert_eq!(
			file_endpoint("owner/repo", "docs/a#b?100% é.md", Some("feature/encoded"),)
				.expect("valid file path"),
			"/repos/owner/repo/contents/docs/a%23b%3F100%25%20%C3%A9.md?ref=feature%2Fencoded",
		);
		assert!(file_endpoint("owner/repo", "/etc/passwd", None).is_err());
		assert!(file_endpoint("owner/repo", "docs/../secret", None).is_err());
	}

	#[test]
	fn contents_response_must_be_a_base64_file() {
		let decoded = decode_file_response(
			&json!({ "type": "file", "encoding": "base64", "content": "aGVsbG8=" }),
			"owner/repo",
			"hello.txt",
		)
		.expect("file response");
		assert_eq!(decoded["content"], "hello");
		assert!(decode_file_response(&json!([]), "owner/repo", "dir").is_err());
		assert!(
			decode_file_response(
				&json!({ "type": "file", "encoding": "utf-8", "content": "hello" }),
				"owner/repo",
				"hello.txt",
			)
			.is_err(),
		);
	}

	#[test]
	fn branch_and_actions_requests_encode_and_commit_scope() {
		assert_eq!(
			pr_branch_endpoint("owner/repo", "owner:feature/foo"),
			"/repos/owner/repo/pulls?state=open&head=owner%3Afeature%2Ffoo&per_page=100",
		);
		assert_eq!(
			branch_endpoint("owner/repo", "feature/foo"),
			"/repos/owner/repo/branches/feature%2Ffoo",
		);
		assert_eq!(
			actions_runs_endpoint("owner/repo", "abc123"),
			"/repos/owner/repo/actions/runs?head_sha=abc123&per_page=100",
		);
		assert!(!actions_runs_endpoint("owner/repo", "abc123").contains("branch="));
		assert_eq!(
			run_jobs_endpoint("owner/repo", 42, 3),
			"/repos/owner/repo/actions/runs/42/jobs?per_page=100&page=3",
		);
	}

	#[test]
	fn pr_references_distinguish_numbers_urls_branches_and_hosts() {
		let default = GithubRepo::parse("owner/repo").expect("default repo");
		assert_eq!(parse_pr_number("17", &default).expect("number"), Some(17));
		assert_eq!(
			parse_pr_number("https://github.com/OWNER/REPO/pull/19/files", &default).expect("URL"),
			Some(19),
		);
		assert_eq!(parse_pr_number("feature/17", &default).expect("branch"), None);
		assert!(parse_pr_number("https://github.com/other/repo/pull/19", &default).is_err(),);

		let enterprise = GithubRepo::parse("ghe.example.com/owner/repo").expect("enterprise repo");
		assert_eq!(
			parse_pr_number("https://ghe.example.com/OWNER/REPO/pull/23", &enterprise)
				.expect("enterprise URL"),
			Some(23),
		);
		assert!(
			parse_pr_number("https://github.com/owner/repo/pull/23", &enterprise).is_err(),
			"a PR URL from another host must not drive the enterprise checkout",
		);
	}

	#[test]
	fn search_dates_normalize_and_use_operation_fields() {
		let now = instant(2026, 8, 26, 12);
		assert_eq!(normalize_date_bound("3d", now).expect("relative date"), "2026-08-23",);
		assert_eq!(
			date_qualifier(Operation::SearchCommits, None, Some("3d"), None, now)
				.expect("commit qualifier")
				.as_deref(),
			Some("committer-date:>=2026-08-23"),
		);
		assert_eq!(
			date_qualifier(
				Operation::SearchRepos,
				Some(DateField::Updated),
				Some("2026-08-01"),
				Some("2026-08-26"),
				now,
			)
			.expect("repository qualifier")
			.as_deref(),
			Some("pushed:2026-08-01..2026-08-26"),
		);
		assert_eq!(
			normalize_date_bound("2026-08-26T01:02:03.999+02:00", now).expect("ISO datetime"),
			"2026-08-25T23:02:03Z",
		);
		assert!(normalize_date_bound("next Thursday", now).is_err());
	}

	#[test]
	fn actions_outcome_inspects_job_conclusions() {
		let value = json!({
			"workflow_runs": [{
				"id": 1,
				"status": "in_progress",
				"conclusion": null,
				"jobs": [{ "status": "completed", "conclusion": "failure" }],
			}],
		});
		assert!(matches!(actions_state(&value), ActionsState::Failure));
		let pending = json!({ "workflow_runs": [] });
		assert!(matches!(actions_state(&pending), ActionsState::Pending));
	}
	#[tokio::test]
	async fn poll_sleep_stops_immediately_when_cancelled() {
		let cancellation = CancellationToken::new();
		cancellation.cancel();
		tokio::time::timeout(
			Duration::from_millis(50),
			poll_sleep(Duration::from_secs(3_600), &cancellation),
		)
		.await
		.expect("cancelled wait must not sleep")
		.expect_err("cancelled wait must fail");
	}
}
