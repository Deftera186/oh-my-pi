//! Direct GitHub API device with isolated worktree mutation operations.

use std::{
	error,
	fmt::{self, Display},
	sync::Arc,
};

use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, ExecEffects,
	IncomingParams, ParamError, Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// GitHub operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Operation {
	/// Read repository metadata.
	RepoView,
	/// Read a repository file.
	FileRead,
	/// Create a pull request.
	PrCreate,
	/// Check out pull request heads into isolated worktrees.
	PrCheckout,
	/// Push a previously checked-out pull request branch.
	PrPush,
	/// Search issues.
	SearchIssues,
	/// Search pull requests.
	SearchPrs,
	/// Search code.
	SearchCode,
	/// Search commits.
	SearchCommits,
	/// Search repositories.
	SearchRepos,
	/// Watch Actions runs and jobs.
	RunWatch,
}
/// Pull request selector accepted as either one value or a batch.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(untagged)]
pub enum PrSelector {
	/// One pull request number, URL, or branch name.
	One(Str),
	/// Multiple pull request numbers, URLs, or branch names.
	Many(Vec<Str>),
}

impl PrSelector {
	/// Returns the selectors as a borrowed slice.
	pub fn as_slice(&self) -> &[Str] {
		match self {
			Self::One(value) => std::slice::from_ref(value),
			Self::Many(values) => values,
		}
	}
}

/// Date field supported by GitHub issue, pull-request, and repository search.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DateField {
	/// Resource creation time.
	Created,
	/// Resource update time; repository search maps this to push time.
	Updated,
}

/// Flat GitHub operation arguments.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Params {
	/// Operation selector.
	pub op:               Operation,
	/// `owner/repo`; omitted operations resolve the current checkout.
	pub repo:             Option<Str>,
	/// Repository-relative file path.
	pub path:             Option<Str>,
	/// Branch, ref, or watched commit.
	pub branch:           Option<Str>,
	/// Pull request number, URL, or branch; arrays batch checkout.
	pub pr:               Option<PrSelector>,
	/// Search query.
	pub query:            Option<Str>,
	/// Lower date bound.
	pub since:            Option<Str>,
	/// Upper date bound.
	pub until:            Option<Str>,
	/// Search date field.
	pub date_field:       Option<DateField>,
	/// Maximum returned rows.
	pub limit:            Option<u32>,
	/// Pull request title.
	pub title:            Option<Str>,
	/// Pull request body.
	pub body:             Option<Str>,
	/// Pull request base branch.
	pub base:             Option<Str>,
	/// Pull request head branch.
	pub head:             Option<Str>,
	/// Actions run id or URL.
	pub run:              Option<Str>,
	/// Open a draft pull request.
	#[serde(default)]
	pub draft:            bool,
	/// Reset an existing local pull-request branch to the remote head.
	#[serde(default)]
	pub force:            bool,
	/// Force-with-lease a PR push.
	#[serde(default)]
	pub force_with_lease: bool,
}

impl Params {
	/// Whether this operation mutates GitHub or local worktree state.
	pub const fn mutates(&self) -> bool {
		matches!(self.op, Operation::PrCreate | Operation::PrCheckout | Operation::PrPush)
	}
}

/// Direct API response plus rate-limit receipt.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Payload {
	/// Completed operation.
	pub op:                   Operation,
	/// Structured operation result.
	pub result:               Value,
	/// Remaining GitHub API requests, when reported.
	pub rate_limit_remaining: Option<u64>,
	/// Rate-limit reset Unix timestamp, when reported.
	pub rate_limit_reset:     Option<u64>,
}

/// GitHub service failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fault {
	/// Stable failure category.
	pub code:    Str,
	/// Secret-free diagnostic.
	pub message: Str,
}
impl Display for Fault {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.message)
	}
}
impl error::Error for Fault {}

/// GitHub operations currently settle as one bounded result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// Harness-owned direct GitHub service.
#[async_trait]
pub trait GithubHost: Send + Sync + 'static {
	/// Execute one API/worktree operation.
	async fn execute(
		&self,
		params: Params,
		cancellation: CancellationToken,
	) -> Result<Payload, Fault>;
}

/// GitHub tool.
pub struct Github {
	host: Arc<dyn GithubHost>,
	spec: ToolSpec,
}

/// Creates `github@1`.
pub fn tool(host: Arc<dyn GithubHost>) -> Github {
	Github {
		host,
		spec: ToolSpec {
			name:            sf!("github"),
			rev:             Rev { family: Str::default(), n: 1 },
			description:     sf!(
				"Uses GitHub's direct API for repository, file, search, pull-request worktree, push, \
				 and Actions operations. No gh process or commit automation is used."
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects {
				documents: None,
				exec:      Some(ExecEffects { commands: Arc::from([sf!("git")]), network: true }),
				inference: None,
				desktop:   None,
				subagents: 0,
			},
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("github.rs"),
			)
			.into(),
		},
	}
}

impl Tool for Github {
	type Fault = Fault;
	type Params = Params;
	type Payload = Payload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, Payload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<Params>().await { Ok(params) => params, Err(error) => { yield param_event(error); return; } };
			if let Err(error) = incoming.interruptable().committed().await { yield commit_event(error); return; }
			let cancellation = CancellationToken::new();
			let execution = self.host.execute(params, cancellation.clone());
			tokio::pin!(execution);
			tokio::select! {
				result = &mut execution => {
					yield Ev::Done(ToolTerminal::Done { result, useless: false });
				},
				interrupt = incoming.next_interrupt() => {
					cancellation.cancel();
					if let Ok(interrupt) = interrupt {
						yield Ev::Aborted(Abort::Interrupted { reason: interrupt.reason });
					} else {
						yield Ev::Aborted(Abort::InputDropped);
					}
				},
			}
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) => {
					Str::new(serde_json::to_string(payload).expect("GitHub payload serializes"))
				},
				Err(fault) => fault.message.clone(),
			},
		}]
	}
}

fn param_event(error: ParamError) -> Ev<Update, Payload, Fault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}
fn commit_event(error: CommitError) -> Ev<Update, Payload, Fault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}
fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed GitHub argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  None,
		found:    Some(message),
	}
}
#[cfg(test)]
mod tests {
	use super::{DateField, Params, PrSelector};

	#[test]
	fn pr_selector_accepts_scalar_and_list_forms() {
		let scalar: Params =
			serde_json::from_value(serde_json::json!({ "op": "pr_checkout", "pr": "feature/foo" }))
				.expect("scalar selector");
		assert!(
			matches!(scalar.pr, Some(PrSelector::One(value)) if value == "feature/foo"),
			"scalar branch selector must survive schema decoding",
		);

		let list: Params = serde_json::from_value(
			serde_json::json!({ "op": "pr_checkout", "pr": ["17", "feature/foo"] }),
		)
		.expect("selector list");
		assert!(
			matches!(list.pr, Some(PrSelector::Many(values)) if values.len() == 2),
			"selector arrays must remain batch inputs",
		);
	}

	#[test]
	fn date_field_is_a_closed_enum() {
		let updated: Params = serde_json::from_value(
			serde_json::json!({ "op": "search_repos", "dateField": "updated" }),
		)
		.expect("updated date field");
		assert_eq!(updated.date_field, Some(DateField::Updated));
		assert!(
			serde_json::from_value::<Params>(
				serde_json::json!({ "op": "search_repos", "dateField": "pushed" }),
			)
			.is_err(),
			"undeclared date fields must fail before dispatch",
		);
	}
}
