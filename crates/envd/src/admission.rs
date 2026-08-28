//! Per-invocation admission gate between finalized arguments and authorization.

use std::{io::Cursor, path::Path, time::Duration};

use bytes::{Bytes, BytesMut};
use flume::Receiver;
use omp_core::Str;
use omp_proto::{
	env::v1::{Admission, AdmitInvocation},
	policy::v1::{BashIr, EffectEnvelope, PolicyDenied},
};
use omp_shell_engine::{
	analysis,
	parser::{Parser, ParserOptions},
};
use omp_tool::Effects;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{time, time::Instant};

/// Default approval posture applied before one invocation reaches interactive
/// admission.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Eq,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum ApprovalMode {
	/// Read-only effects proceed; writes and execution require confirmation.
	AlwaysAsk,
	/// Read and workspace-write effects proceed; execution requires
	/// confirmation.
	Write,
	/// Every declared tier proceeds unless a per-tool policy overrides it.
	#[default]
	Yolo,
}

/// User policy for one named tool.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ApprovalPolicy {
	/// Proceed without an interactive decision.
	Allow,
	/// Refuse the invocation.
	Deny,
	/// Require an interactive durable decision.
	Prompt,
}

/// Conservative capability tier derived from a tool's declared [`Effects`].
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
	strum::Display,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ApprovalTier {
	/// No mutation, process, inference, or subagent effects.
	Read,
	/// Declared document mutation without execution-class effects.
	Write,
	/// Process, network, inference, or subagent authority.
	Exec,
}

impl ApprovalTier {
	/// Resolves the highest approval tier present in `effects`.
	pub fn from_effects(effects: &Effects) -> Self {
		if effects.subagents != 0
			|| effects
				.exec
				.as_ref()
				.is_some_and(|effect| !effect.is_empty())
			|| effects
				.inference
				.as_ref()
				.is_some_and(|effect| !effect.is_empty())
			|| effects.desktop.as_ref().is_some_and(|effect| effect.input)
		{
			Self::Exec
		} else if effects
			.documents
			.as_ref()
			.is_some_and(|effect| !effect.write_globs.is_empty())
		{
			Self::Write
		} else {
			Self::Read
		}
	}
}

/// Authority that selected one durable approval policy.
#[derive(
	Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, strum::Display, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ApprovalSource {
	/// The active approval mode's tier ceiling.
	Mode,
	/// A named per-tool user override.
	User,
}

/// Stable per-invocation approval outcome suitable for the admission receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedApproval {
	/// Stable invocation identity.
	pub invocation_id: Str,
	/// Exact live tool name evaluated.
	pub tool_name:     Str,
	/// Tier derived from the live revision's declared effects.
	pub tier:          ApprovalTier,
	/// Effective policy.
	pub policy:        ApprovalPolicy,
	/// Authority which selected the policy.
	pub source:        ApprovalSource,
	/// User policy key, present only for a per-tool override.
	pub policy_key:    Option<Str>,
}

/// Resolves a durable invocation decision from the declared effect ceiling.
///
/// Per-tool overrides remain authoritative in every mode. Without one, modes
/// approve tiers up to `read`, `write`, and `exec`, respectively.
pub fn resolve_approval(
	invocation_id: impl Into<Str>,
	tool_name: impl Into<Str>,
	effects: &Effects,
	mode: ApprovalMode,
	override_policy: Option<ApprovalPolicy>,
) -> ResolvedApproval {
	let invocation_id = invocation_id.into();
	let tool_name = tool_name.into();
	let tier = ApprovalTier::from_effects(effects);
	let (policy, source, policy_key) = override_policy.map_or_else(
		|| {
			let allowed = match mode {
				ApprovalMode::AlwaysAsk => tier <= ApprovalTier::Read,
				ApprovalMode::Write => tier <= ApprovalTier::Write,
				ApprovalMode::Yolo => true,
			};
			(
				if allowed {
					ApprovalPolicy::Allow
				} else {
					ApprovalPolicy::Prompt
				},
				ApprovalSource::Mode,
				None,
			)
		},
		|policy| (policy, ApprovalSource::User, Some(tool_name.clone())),
	);
	ResolvedApproval { invocation_id, tool_name, tier, policy, source, policy_key }
}

/// A finalized admission result, with policy transformation applied before the
/// executor can observe arguments.
pub enum AdmissionDecision {
	/// The effective canonical arguments and regenerated shell facts.
	Allowed {
		/// RFC 8785-compatible serde canonical argument bytes for the executor.
		raw:  Bytes,
		/// Shell facts regenerated from the effective command, when applicable.
		bash: Option<BashIr>,
	},
	/// A refusal carrying the one wire vocabulary for policy denial.
	Denied(PolicyDenied),
}

/// A protocol or transformation failure while admitting an invocation.
#[derive(Debug, Error)]
pub enum AdmissionError {
	/// Arguments ended in a value that cannot be transformed as an object.
	#[error("finalized invocation arguments must be a JSON object")]
	ArgumentsNotObject,
	/// A policy transform was not a valid JSON merge patch.
	#[error("admission argument patch is not valid JSON")]
	InvalidPatch,
	/// The admission response belonged to another invocation.
	#[error("admission response invocation id did not match the pending invocation")]
	WrongInvocation,
	/// No admission query has reached the finalized-arguments transition.
	#[error("admission response arrived before finalized arguments")]
	NotPending,
	/// The query's single response has already been accepted.
	#[error("admission response was already supplied")]
	AlreadyAnswered,
}

/// One cache invalidation target derived from a mutating GitHub CLI command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GithubMutationTarget {
	/// Explicit `owner/repo`, absent when the command targets the active
	/// repository.
	pub(crate) repo:   Option<Str>,
	/// Resource family (`issue` or `pr`).
	pub(crate) kind:   Str,
	/// Explicit issue or pull-request number when statically known.
	pub(crate) number: Option<u64>,
}

/// Derives only mutating issue/PR operations from admitted BashIR.
///
/// Dynamic words are ignored rather than guessed, and read-only `gh` actions
/// never invalidate cache entries.
pub(crate) fn github_mutation_targets(bash: &BashIr) -> Vec<GithubMutationTarget> {
	let mut targets = Vec::new();
	for command in &bash.commands {
		if command.name.as_deref() != Some("gh")
			|| command.argv.iter().any(|argument| argument.dynamic)
		{
			continue;
		}
		let argv = command
			.argv
			.iter()
			.map(|argument| argument.text.as_str())
			.collect::<Vec<_>>();
		let Some(kind_index) = argv
			.iter()
			.position(|argument| matches!(*argument, "issue" | "pr"))
		else {
			continue;
		};
		let Some(action) = argv.get(kind_index + 1).copied() else {
			continue;
		};
		if !matches!(
			action,
			"create"
				| "edit" | "close"
				| "reopen"
				| "delete"
				| "comment"
				| "lock" | "unlock"
				| "pin" | "unpin"
				| "transfer"
				| "merge"
				| "ready"
				| "review"
		) {
			continue;
		}
		let repo = option_value(&argv, "-R")
			.or_else(|| option_value(&argv, "--repo"))
			.or_else(|| inline_option(&argv, "--repo="))
			.map(Str::new);
		let number = argv
			.iter()
			.skip(kind_index + 2)
			.find_map(|argument| argument.trim_start_matches('#').parse::<u64>().ok());
		targets.push(GithubMutationTarget { repo, kind: Str::new(argv[kind_index]), number });
	}
	targets
}

fn option_value<'a>(arguments: &[&'a str], option: &str) -> Option<&'a str> {
	arguments
		.windows(2)
		.find_map(|pair| (pair[0] == option).then_some(pair[1]))
}

fn inline_option<'a>(arguments: &[&'a str], prefix: &str) -> Option<&'a str> {
	arguments
		.iter()
		.find_map(|argument| argument.strip_prefix(prefix))
}

/// Env-owned one-shot admission state for one invocation.
pub struct AdmissionGate {
	invocation_id: Str,
	tool_name:     Str,
	deadline:      Instant,
	fragments:     BytesMut,
	requested:     Option<Value>,
	query_emitted: bool,
	policy:        ApprovalPolicy,
	answer_tx:     Option<flume::Sender<Admission>>,
	answer_rx:     Receiver<Admission>,
}

impl AdmissionGate {
	/// Starts an OPEN invocation gate whose deadline is enforced by env.
	#[cfg(test)]
	pub(crate) fn new(invocation_id: Str, tool_name: Str, deadline: Duration) -> Self {
		Self::with_policy(invocation_id, tool_name, deadline, ApprovalPolicy::Prompt)
	}

	/// Starts an OPEN invocation gate with its resolved admission policy.
	pub(crate) fn with_policy(
		invocation_id: Str,
		tool_name: Str,
		deadline: Duration,
		policy: ApprovalPolicy,
	) -> Self {
		let (answer_tx, answer_rx) = flume::bounded(1);
		Self {
			invocation_id,
			tool_name,
			deadline: Instant::now() + deadline,
			fragments: BytesMut::new(),
			requested: None,
			query_emitted: false,
			policy,
			answer_tx: Some(answer_tx),
			answer_rx,
		}
	}

	/// Appends one raw argument fragment and emits a query once it is one JSON
	/// document. Further fragments remain the caller's protocol violation.
	pub(crate) fn push_fragment(
		&mut self,
		fragment: &str,
		cwd: &Path,
		root: &Path,
	) -> Option<AdmitInvocation> {
		if self.query_emitted {
			return None;
		}
		self.fragments.extend_from_slice(fragment.as_bytes());
		let value = serde_json::from_slice::<Value>(&self.fragments).ok()?;
		if !value.is_object() {
			return None;
		}
		self.finish_query(value, cwd, root)
	}

	/// Finalizes a call that supplied its complete arguments only with
	/// `ArgsCommitted`, replacing any incomplete speculative fragments.
	pub(crate) fn finalize(
		&mut self,
		raw: &[u8],
		cwd: &Path,
		root: &Path,
	) -> Result<Option<AdmitInvocation>, AdmissionError> {
		if self.query_emitted {
			return Ok(None);
		}
		let value =
			serde_json::from_slice::<Value>(raw).map_err(|_| AdmissionError::ArgumentsNotObject)?;
		if !value.is_object() {
			return Err(AdmissionError::ArgumentsNotObject);
		}
		self.fragments.clear();
		self.fragments.extend_from_slice(raw);
		Ok(self.finish_query(value, cwd, root))
	}

	fn finish_query(&mut self, value: Value, cwd: &Path, root: &Path) -> Option<AdmitInvocation> {
		let bash = bash_ir(&self.tool_name, &value, cwd, root);
		self.requested = Some(value);
		self.query_emitted = true;
		let query = AdmitInvocation {
			invocation_id: self.invocation_id.to_string(),
			bash,
			deadline_ms: self
				.deadline
				.saturating_duration_since(Instant::now())
				.as_millis()
				.try_into()
				.unwrap_or(u64::MAX),
			props: Default::default(),
		};
		match self.policy {
			ApprovalPolicy::Prompt => Some(query),
			ApprovalPolicy::Allow => {
				self
					.answer(Admission {
						invocation_id: self.invocation_id.to_string(),
						allow: true,
						..Admission::default()
					})
					.expect("an internally resolved admission is answered exactly once");
				None
			},
			ApprovalPolicy::Deny => {
				self
					.answer(Admission {
						invocation_id: self.invocation_id.to_string(),
						allow: false,
						denied: Some(approval_denial(&self.invocation_id, &self.tool_name)),
						..Admission::default()
					})
					.expect("an internally resolved admission is answered exactly once");
				None
			},
		}
	}

	/// Accepts Core's one answer without allowing it to block the dispatcher.
	pub(crate) fn answer(&mut self, admission: Admission) -> Result<(), AdmissionError> {
		if !self.query_emitted {
			return Err(AdmissionError::NotPending);
		}
		if admission.invocation_id != self.invocation_id {
			return Err(AdmissionError::WrongInvocation);
		}
		let Some(answer_tx) = self.answer_tx.take() else {
			return Err(AdmissionError::AlreadyAnswered);
		};
		answer_tx
			.send(admission)
			.map_err(|_| AdmissionError::AlreadyAnswered)
	}

	/// Reports whether Core's one admission answer has arrived.
	pub(crate) const fn is_answered(&self) -> bool {
		self.query_emitted && self.answer_tx.is_none()
	}

	/// Returns the deadline for a query that is waiting on Core.
	pub(crate) fn pending_deadline(&self) -> Option<Instant> {
		(self.query_emitted && self.answer_tx.is_some()).then_some(self.deadline)
	}

	/// Converts an unanswered query whose deadline has elapsed into env's
	/// synthetic denial. The connection loop owns this transition.
	pub(crate) fn expire(&mut self, now: Instant) -> Option<PolicyDenied> {
		(self.query_emitted && self.answer_tx.is_some() && now >= self.deadline).then(|| {
			self.answer_tx.take();
			timeout_denial(&self.invocation_id)
		})
	}

	/// Waits for Core's answer through the env-owned deadline, synthesizing the
	/// structured fail-closed denial when it expires or the relay closes.
	pub(crate) async fn decide(&self, cwd: &Path, root: &Path) -> AdmissionDecision {
		let admission = match self.answer_rx.try_recv() {
			Ok(admission) => admission,
			Err(flume::TryRecvError::Empty) => {
				let answer = time::timeout_at(self.deadline, self.answer_rx.recv_async()).await;
				let Ok(Ok(admission)) = answer else {
					return AdmissionDecision::Denied(timeout_denial(&self.invocation_id));
				};
				admission
			},
			Err(flume::TryRecvError::Disconnected) => {
				return AdmissionDecision::Denied(timeout_denial(&self.invocation_id));
			},
		};
		if !admission.allow {
			return AdmissionDecision::Denied(
				admission
					.denied
					.unwrap_or_else(|| timeout_denial(&self.invocation_id)),
			);
		}
		let Some(requested) = self.requested.as_ref() else {
			return AdmissionDecision::Denied(timeout_denial(&self.invocation_id));
		};
		let Ok((raw, bash)) =
			apply_admission_patch(requested, &admission.args_patch, &self.tool_name, cwd, root)
		else {
			return AdmissionDecision::Denied(invalid_patch_denial(&self.invocation_id));
		};
		AdmissionDecision::Allowed { raw, bash }
	}
}

/// Refuses an envelope that would widen the resolved tool declaration.
pub fn effects_narrow_or_refuse(
	requested: Option<&EffectEnvelope>,
	maximum: &Effects,
) -> Option<Effects> {
	let requested = requested.map(Effects::try_from).transpose().ok()?;
	match requested {
		Some(requested) => maximum.narrow(requested),
		None => Some(maximum.clone()),
	}
}

fn apply_admission_patch(
	requested: &Value,
	patch: &[u8],
	tool_name: &str,
	cwd: &Path,
	root: &Path,
) -> Result<(Bytes, Option<BashIr>), AdmissionError> {
	let mut effective = requested.clone();
	if !patch.is_empty() {
		let patch = serde_json::from_slice(patch).map_err(|_| AdmissionError::InvalidPatch)?;
		merge_patch(&mut effective, patch);
	}
	if !effective.is_object() {
		return Err(AdmissionError::ArgumentsNotObject);
	}
	let raw = serde_json::to_vec(&effective).map_err(|_| AdmissionError::InvalidPatch)?;
	let bash = bash_ir(tool_name, &effective, cwd, root);
	Ok((Bytes::from(raw), bash))
}

fn merge_patch(target: &mut Value, patch: Value) {
	let Value::Object(patch) = patch else {
		*target = patch;
		return;
	};
	if !target.is_object() {
		*target = Value::Object(Default::default());
	}
	let Value::Object(target) = target else {
		return;
	};
	for (key, value) in patch {
		if value.is_null() {
			target.remove(&key);
		} else {
			merge_patch(target.entry(key).or_insert(Value::Null), value);
		}
	}
}

pub(crate) fn bash_ir(tool_name: &str, args: &Value, cwd: &Path, root: &Path) -> Option<BashIr> {
	if tool_name != "bash" {
		return None;
	}
	let command = args.get("command")?.as_str()?;
	let mut parser = Parser::new(Cursor::new(command), &ParserOptions::default());
	Some(match parser.parse_program() {
		Ok(program) => BashIr::from(&analysis::analyze(
			&program,
			cwd.to_string_lossy().as_ref(),
			root.to_string_lossy().as_ref(),
		)),
		Err(error) => BashIr {
			source: command.to_owned(),
			parse_ok: false,
			parse_error: Some(error.to_string()),
			..BashIr::default()
		},
	})
}

fn timeout_denial(invocation_id: &str) -> PolicyDenied {
	PolicyDenied {
		reason:      "admission deadline elapsed".into(),
		code:        "admission_timeout".into(),
		decision_id: invocation_id.to_owned(),
		rules:       Vec::new(),
		props:       Default::default(),
	}
}

fn invalid_patch_denial(invocation_id: &str) -> PolicyDenied {
	PolicyDenied {
		reason:      "admission transformation was invalid".into(),
		code:        "admission_invalid_patch".into(),
		decision_id: invocation_id.to_owned(),
		rules:       Vec::new(),
		props:       Default::default(),
	}
}

fn approval_denial(invocation_id: &str, tool_name: &str) -> PolicyDenied {
	PolicyDenied {
		reason:      format!("tool `{tool_name}` is denied by approval policy"),
		code:        "approval_policy_denied".into(),
		decision_id: invocation_id.to_owned(),
		rules:       vec![format!("tools.approval.{tool_name}")],
		props:       Default::default(),
	}
}

#[cfg(test)]
mod tests {
	use std::{path::Path, sync::Arc, time::Duration};

	use bytes::Bytes;
	use omp_core::sf;
	use omp_proto::{
		env::v1::Admission,
		policy::v1::{EffectEnvelope, ExecEffects},
	};
	use omp_tool::{
		DesktopEffects, DocEffects, Effects, ExecEffects as ToolExecEffects, InferenceEffects, Usd,
	};
	use tokio::time;

	use super::{
		AdmissionDecision, AdmissionGate, ApprovalMode, ApprovalPolicy, ApprovalSource, ApprovalTier,
		apply_admission_patch, bash_ir, effects_narrow_or_refuse, github_mutation_targets,
		resolve_approval,
	};

	#[tokio::test]
	async fn deadline_synthesizes_a_structured_denial() {
		let gate = AdmissionGate::new(sf!("call"), sf!("bash"), Duration::ZERO);
		let AdmissionDecision::Denied(denied) =
			gate.decide(Path::new("/work"), Path::new("/work")).await
		else {
			panic!("elapsed admission deadline must deny");
		};
		assert_eq!(denied.code, "admission_timeout");
	}

	#[tokio::test]
	async fn allow_policy_finalizes_without_emitting_a_query() {
		let mut gate = AdmissionGate::with_policy(
			sf!("call"),
			sf!("bash"),
			Duration::ZERO,
			ApprovalPolicy::Allow,
		);
		assert!(
			gate
				.finalize(br#"{"command":"echo allowed"}"#, Path::new("/work"), Path::new("/work"))
				.expect("valid arguments")
				.is_none()
		);
		let AdmissionDecision::Allowed { raw, .. } =
			gate.decide(Path::new("/work"), Path::new("/work")).await
		else {
			panic!("allow policy must admit");
		};
		assert_eq!(raw, Bytes::from_static(br#"{"command":"echo allowed"}"#));
	}

	#[tokio::test]
	async fn deny_policy_finalizes_without_emitting_a_query() {
		let mut gate =
			AdmissionGate::with_policy(sf!("call"), sf!("bash"), Duration::ZERO, ApprovalPolicy::Deny);
		assert!(
			gate
				.finalize(br#"{"command":"echo denied"}"#, Path::new("/work"), Path::new("/work"))
				.expect("valid arguments")
				.is_none()
		);
		let AdmissionDecision::Denied(denied) =
			gate.decide(Path::new("/work"), Path::new("/work")).await
		else {
			panic!("deny policy must refuse");
		};
		assert_eq!(denied.code, "approval_policy_denied");
		assert_eq!(denied.decision_id, "call");
		assert_eq!(denied.rules, ["tools.approval.bash"]);
	}

	#[tokio::test]
	async fn prompt_policy_emits_a_query_and_waits_for_an_answer() {
		let mut gate = AdmissionGate::new(sf!("call"), sf!("bash"), Duration::from_secs(1));
		assert!(
			gate
				.finalize(br#"{"command":"echo prompt"}"#, Path::new("/work"), Path::new("/work"))
				.expect("valid arguments")
				.is_some()
		);
		assert!(
			time::timeout(
				Duration::from_millis(10),
				gate.decide(Path::new("/work"), Path::new("/work")),
			)
			.await
			.is_err(),
			"prompt policy must wait for the client admission answer"
		);
		gate
			.answer(Admission { invocation_id: "call".into(), allow: true, ..Admission::default() })
			.expect("prompt answer");
		assert!(matches!(
			gate.decide(Path::new("/work"), Path::new("/work")).await,
			AdmissionDecision::Allowed { .. }
		));
	}

	#[test]
	fn patch_changes_effective_args_and_regenerates_shell_facts() {
		let requested = serde_json::json!({"command": "echo requested"});
		let (raw, bash) = apply_admission_patch(
			&requested,
			br#"{"command":"echo effective"}"#,
			"bash",
			Path::new("/work"),
			Path::new("/work"),
		)
		.expect("valid merge patch");
		assert_eq!(raw, Bytes::from_static(br#"{"command":"echo effective"}"#));
		assert_eq!(bash.expect("shell facts").source, "echo effective");
	}

	#[test]
	fn approval_modes_resolve_from_declared_effects() {
		let read = Effects {
			documents: Some(DocEffects { read: true, write_globs: Arc::from([]) }),
			..Effects::empty()
		};
		let write = Effects {
			documents: Some(DocEffects { read: true, write_globs: Arc::from([sf!("**")]) }),
			..Effects::empty()
		};
		let exec = Effects {
			inference: Some(InferenceEffects { max_requests: 1, max_usd: Usd::from_nanos(1) }),
			..Effects::empty()
		};

		let desktop_read = Effects {
			desktop: Some(DesktopEffects {
				capture:       true,
				accessibility: true,
				input:         false,
			}),
			..Effects::empty()
		};
		let desktop_input = Effects {
			desktop: Some(DesktopEffects {
				capture:       false,
				accessibility: false,
				input:         true,
			}),
			..Effects::empty()
		};
		let read_decision = resolve_approval("read-1", "read", &read, ApprovalMode::AlwaysAsk, None);
		assert_eq!(read_decision.tier, ApprovalTier::Read);
		assert_eq!(read_decision.policy, ApprovalPolicy::Allow);
		assert_eq!(read_decision.source, ApprovalSource::Mode);

		let write_prompt =
			resolve_approval("write-1", "write", &write, ApprovalMode::AlwaysAsk, None);
		assert_eq!(write_prompt.tier, ApprovalTier::Write);
		assert_eq!(write_prompt.policy, ApprovalPolicy::Prompt);

		let write_allowed = resolve_approval("write-2", "write", &write, ApprovalMode::Write, None);
		assert_eq!(write_allowed.policy, ApprovalPolicy::Allow);

		let exec_prompt = resolve_approval("eval-1", "eval", &exec, ApprovalMode::Write, None);
		assert_eq!(exec_prompt.tier, ApprovalTier::Exec);
		assert_eq!(exec_prompt.policy, ApprovalPolicy::Prompt);
		assert_eq!(ApprovalTier::from_effects(&desktop_read), ApprovalTier::Read);
		assert_eq!(ApprovalTier::from_effects(&desktop_input), ApprovalTier::Exec);
	}

	#[test]
	fn per_tool_override_is_authoritative_and_receipted() {
		let effects = Effects {
			exec: Some(ToolExecEffects { commands: Arc::from([sf!("*")]), network: true }),
			..Effects::empty()
		};
		let decision = resolve_approval(
			"shell-7",
			"bash",
			&effects,
			ApprovalMode::Yolo,
			Some(ApprovalPolicy::Deny),
		);
		assert_eq!(decision.policy, ApprovalPolicy::Deny);
		assert_eq!(decision.source, ApprovalSource::User);
		assert_eq!(decision.policy_key.as_deref(), Some("bash"));
		assert_eq!(
			serde_json::to_value(&decision).expect("durable approval receipt serializes"),
			serde_json::json!({
				"invocation_id": "shell-7",
				"tool_name": "bash",
				"tier": "exec",
				"policy": "deny",
				"source": "user",
				"policy_key": "bash"
			})
		);
	}

	#[test]
	fn derives_only_static_mutating_github_targets() {
		let bash = bash_ir(
			"bash",
			&serde_json::json!({
				"command": "gh issue edit 42 --repo Owner/Repo && gh pr view 7"
			}),
			Path::new("/work"),
			Path::new("/work"),
		)
		.unwrap();
		let targets = github_mutation_targets(&bash);
		assert_eq!(targets.len(), 1);
		assert_eq!(targets[0].repo.as_deref(), Some("Owner/Repo"));
		assert_eq!(targets[0].kind, "issue");
		assert_eq!(targets[0].number, Some(42));
	}

	#[test]
	fn widened_effect_envelope_is_refused() {
		let maximum = Effects {
			exec: Some(ToolExecEffects { commands: [sf!("git")].into(), network: false }),
			..Effects::empty()
		};
		let requested = EffectEnvelope {
			exec: Some(ExecEffects {
				commands: vec!["git".into(), "curl".into()],
				network:  false,
				props:    None,
			}),
			..EffectEnvelope::default()
		};
		assert!(effects_narrow_or_refuse(Some(&requested), &maximum).is_none());
	}

	#[test]
	fn absent_effect_envelope_retains_declared_maximum() {
		let maximum = Effects {
			exec: Some(ToolExecEffects { commands: [sf!("git")].into(), network: false }),
			..Effects::empty()
		};
		assert_eq!(effects_narrow_or_refuse(None, &maximum), Some(maximum));
	}
}
