use std::{
	collections::{BTreeMap, BTreeSet},
	env,
	future::{self, Future},
	path::{Path, PathBuf},
	pin::Pin,
	sync::Arc,
	time::Duration,
};

use bytes::Bytes;
use flume::Receiver;
use omp_core::{CowBytes, EnvPath, Str, encoding::hex, sf};
use omp_env::{EnvClient, ExecEvent as ClientExecEvent, ExecRun as ClientExecRun};
use omp_proto::env::{
	v1,
	v1::{
		CloseSessionRequest, EnvironmentDelta, ExecOutcome as EnvExecOutcome, ExecRequest,
		OpenSessionRequest, OutputChannel as EnvOutputChannel, ProcessSpec, PtySpec, RestartPolicy,
		RestartSpec, Script, ShellProfileInput, StartProcess,
	},
};
use omp_tool::{BlobRef, JobOwner};
use omp_tools::{
	auto_background::DetachedJob,
	read::{
		resolver::{ResolverTable, Scheme},
		selector::parse_uri,
	},
	shell::{
		DetachRequest, ExecOutcome, ExecStatus, Fault, OutputChannel, RunEvent, RunRequest, Session,
		SessionOptions, ShellExec, ShellRun, Update,
	},
	shell_uri::QuoteContext,
};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
	direnv::DirenvDelta,
	exec::{ExecError, ExecEvent, ExecHost, ExecRun, sandbox_denied_event_path},
	exec_settings::{DirenvMode, ExecSandboxMode, SandboxSettings, ShellProfile, ShellSettings},
	tool_url::UrlResolver,
	tools,
};

/// Session-scoped ACP terminal execution selected ahead of local shell
/// placement when a capable editor peer is attached.
pub trait AcpExecBackend: Send + Sync {
	/// Starts a foreground command and exposes its ordinary shell event stream.
	fn run(
		&self,
		request: AcpExecRequest,
	) -> Pin<Box<dyn Future<Output = Result<AcpExecRun, Fault>> + Send + '_>>;
}

/// One ACP terminal request after shell session option resolution.
pub struct AcpExecRequest {
	/// Shell command line to execute.
	pub command:    Str,
	/// Resolved local working-directory path, when one was requested.
	pub cwd:        Option<Str>,
	/// Resolved environment additions for the command.
	pub env:        BTreeMap<Str, Str>,
	/// Optional command timeout in milliseconds.
	pub timeout_ms: Option<u64>,
}

/// ACP terminal event handle consumed through the ordinary shell resource
/// contract.
pub struct AcpExecRun {
	/// Ordered execution events produced by the editor-owned terminal.
	pub events: Receiver<Result<RunEvent, Fault>>,
	/// Cancellation handle for the editor-owned terminal.
	pub cancel: CancellationToken,
}

/// Late-bound ACP backend capability shared with one Environment registry.
#[derive(Clone, Default)]
pub(crate) struct AcpExecSlot {
	backend: Arc<parking_lot::RwLock<Option<Arc<dyn AcpExecBackend>>>>,
}

impl AcpExecSlot {
	/// Replaces the session capability currently available to shell calls.
	pub(crate) fn bind(&self, backend: Option<Arc<dyn AcpExecBackend>>) {
		*self.backend.write() = backend;
	}

	fn backend(&self) -> Option<Arc<dyn AcpExecBackend>> {
		self.backend.read().clone()
	}
}

/// Shell resource adapter backed by either the local execution authority or a
/// retained remote Environment owner.
#[derive(Clone)]
pub struct ShellExecHost {
	backend:            ShellBackend,
	cwd_uri:            Str,
	resolvers:          Arc<ResolverTable<UrlResolver>>,
	settings:           ShellSettings,
	/// Sandbox posture compiled for external commands and in-process writes.
	pub(crate) sandbox: SandboxSettings,
	acp:                AcpExecSlot,
	acp_routing:        bool,
	acp_sessions:       Arc<Mutex<BTreeMap<Bytes, AcpSessionOptions>>>,
}
#[derive(Clone)]
enum ShellBackend {
	Local(ExecHost),
	Remote(EnvClient),
}
#[derive(Clone)]
struct AcpSessionOptions {
	cwd:   Option<Str>,
	env:   BTreeMap<Str, Str>,
	unset: Vec<String>,
}

impl ShellExecHost {
	/// Binds shell execution to the workspace root URI used for sessions and
	/// detached processes.
	pub(crate) fn new(
		host: ExecHost,
		cwd_uri: Str,
		resolvers: Arc<ResolverTable<UrlResolver>>,
		settings: ShellSettings,
		sandbox: SandboxSettings,
		acp: AcpExecSlot,
		acp_routing: bool,
	) -> Self {
		if let Ok(uri) = Url::parse(&cwd_uri)
			&& let Ok(root) = uri.to_file_path()
		{
			host.configure_sandbox(&sandbox, &root);
		}
		Self {
			backend: ShellBackend::Local(host),
			cwd_uri,
			resolvers,
			settings,
			sandbox,
			acp,
			acp_routing,
			acp_sessions: Arc::new(Mutex::new(BTreeMap::new())),
		}
	}

	/// Binds shell execution to a retained Environment owner connection while
	/// preserving this composition's URL resolvers, shell settings, and sandbox
	/// policy.
	pub(crate) fn new_remote(
		client: EnvClient,
		cwd_uri: Str,
		resolvers: Arc<ResolverTable<UrlResolver>>,
		settings: ShellSettings,
		sandbox: SandboxSettings,
		acp: AcpExecSlot,
		acp_routing: bool,
	) -> Self {
		Self {
			backend: ShellBackend::Remote(client),
			cwd_uri,
			resolvers,
			settings,
			sandbox,
			acp,
			acp_routing,
			acp_sessions: Arc::new(Mutex::new(BTreeMap::new())),
		}
	}
}
impl ShellExecHost {
	async fn shell_profile(&self) -> ShellProfileInput {
		use super::shell_profile::capture;
		let mut profile = self.settings.profile;
		let mut executable = self
			.settings
			.executable
			.as_deref()
			.unwrap_or_default()
			.to_owned();
		if profile == ShellProfile::User && executable.is_empty() {
			executable = env::var("SHELL")
				.ok()
				.filter(|shell| {
					let path = Path::new(shell);
					path.is_absolute()
						&& path.is_file()
						&& path
							.file_name()
							.and_then(|name| name.to_str())
							.is_some_and(|name| matches!(name, "bash" | "zsh" | "fish"))
				})
				.unwrap_or_default();
			if executable.is_empty() {
				profile = ShellProfile::Brush;
			}
		}
		if executable.is_empty() {
			executable = match profile {
				ShellProfile::Bash => String::from("bash"),
				ShellProfile::Zsh => String::from("zsh"),
				ShellProfile::Fish => String::from("fish"),
				ShellProfile::Brush | ShellProfile::User => String::new(),
			};
		}
		let profile_name: &'static str = profile.into();
		let args = self
			.settings
			.args
			.iter()
			.filter(|argument| {
				profile != ShellProfile::Fish || !matches!(argument.as_str(), "-l" | "--login")
			})
			.map(ToString::to_string)
			.collect();
		let snapshot_prefix =
			if matches!(profile, ShellProfile::Bash | ShellProfile::Zsh | ShellProfile::User) {
				let home = env::var_os("HOME").map(PathBuf::from);
				match home {
					Some(home) => capture(&executable, &home)
						.await
						.ok()
						.flatten()
						.map(|path| format!(". {} &&", shell_word(&path.to_string_lossy()))),
					None => None,
				}
			} else {
				None
			};
		let command_prefix = match (snapshot_prefix, self.settings.command_prefix.as_deref()) {
			(Some(snapshot), Some(prefix)) => format!("{snapshot} {prefix}"),
			(Some(snapshot), None) => snapshot,
			(None, Some(prefix)) => prefix.to_owned(),
			(None, None) => String::new(),
		};
		ShellProfileInput {
			profile: profile_name.to_owned(),
			executable,
			args,
			command_prefix,
			env_delta: None,
			login: self.settings.login && profile != ShellProfile::Fish,
			wire_revision: omp_proto::SCHEMA_REV,
		}
	}

	async fn detached_command(&self, command: &Str) -> String {
		let profile = self.shell_profile().await;
		let command = if profile.command_prefix.is_empty() {
			command.to_string()
		} else {
			format!("{} {command}", profile.command_prefix)
		};
		if matches!(profile.profile.as_str(), "" | "brush") {
			return command;
		}
		let mut rendered = shell_word(&profile.executable);
		for argument in profile.args {
			rendered.push(' ');
			rendered.push_str(&shell_word(&argument));
		}
		if profile.login {
			rendered.push_str(" -l");
		}
		rendered.push_str(" -c ");
		rendered.push_str(&shell_word(&command));
		rendered
	}

	async fn expand_internal_uris(&self, input: &str, shell_source: bool) -> Result<Str, Fault> {
		let mut paths = BTreeMap::new();
		for occurrence in omp_tools::shell_uri::scan(input) {
			if matches!(occurrence.quote, QuoteContext::Single | QuoteContext::Double)
				&& !occurrence.whole_quoted_token
			{
				continue;
			}
			if paths.contains_key(&occurrence.uri) {
				continue;
			}
			let parsed = parse_uri(occurrence.uri.as_str())
				.map_err(|_| Fault::Resource {
					operation: sf!("materialize"),
					message:   sf!("invalid internal resource URI: {}", occurrence.uri),
				})?
				.ok_or_else(|| Fault::Resource {
					operation: sf!("materialize"),
					message:   sf!("internal resource URI is missing a scheme"),
				})?;
			if parsed.scheme == Scheme::Unknown {
				continue;
			}
			let Some(resolved) = self.resolvers.path(parsed.scheme, parsed.resource).await else {
				continue;
			};
			let resolved = resolved.map_err(|_| Fault::Resource {
				operation: sf!("materialize"),
				message:   sf!("internal resource has no materializable path: {}", occurrence.uri),
			})?;
			let Some(path_uri) = resolved.canonical_path_uri else {
				continue;
			};
			let path = Url::parse(path_uri.as_str())
				.ok()
				.and_then(|uri| uri.to_file_path().ok())
				.ok_or_else(|| Fault::Resource {
					operation: sf!("materialize"),
					message:   sf!("internal resource path is not a local file URI"),
				})?;
			paths.insert(occurrence.uri, Str::from(path.to_string_lossy().as_ref()));
		}
		Ok(if shell_source {
			omp_tools::shell_uri::replace(input, &paths)
		} else {
			omp_tools::shell_uri::replace_plain(input, &paths)
		})
	}

	async fn expand_environment(
		&self,
		environment: BTreeMap<Str, Option<Str>>,
	) -> Result<BTreeMap<Str, Option<Str>>, Fault> {
		let mut expanded = BTreeMap::new();
		for (name, value) in environment {
			let value = match value {
				Some(value) => Some(self.expand_internal_uris(value.as_str(), false).await?),
				None => None,
			};
			expanded.insert(name, value);
		}
		Ok(expanded)
	}

	async fn resolve_cwd(&self, requested: Option<&str>) -> Result<Str, Fault> {
		let expanded;
		let requested = if let Some(value) = requested {
			expanded = self.expand_internal_uris(value, false).await?;
			Some(expanded.as_str())
		} else {
			None
		};
		let root = Url::parse(&self.cwd_uri)
			.map_err(|error| cwd_fault(format!("workspace root URI is invalid: {error}")))?;
		let root_path = root
			.to_file_path()
			.map_err(|()| cwd_fault("workspace root is not a local file URI"))?;
		let path = match requested {
			None => root_path,
			Some(value) if value.contains("://") => Url::parse(value)
				.map_err(|error| cwd_fault(format!("working-directory URI is invalid: {error}")))?
				.to_file_path()
				.map_err(|()| cwd_fault("working-directory URI is not a local file URI"))?,
			Some(value) => {
				let path = Path::new(value);
				if path.is_absolute() {
					path.into()
				} else {
					root_path.join(path)
				}
			},
		};
		if !path.is_dir() {
			return Err(cwd_fault(format!(
				"working directory is not an existing directory: {}",
				path.display()
			)));
		}
		let uri = Url::from_file_path(path)
			.map_err(|()| cwd_fault("working directory cannot be represented as a file URI"))?;
		Ok(Str::from(uri.to_string()))
	}

	async fn acp_command_prefix(&self, unset: &[String]) -> Str {
		let profile = self.shell_profile().await;
		let mut prefix = String::new();
		#[cfg(not(windows))]
		{
			let names = unset
				.iter()
				.filter(|name| valid_env_name(name))
				.map(String::as_str)
				.collect::<Vec<_>>();
			if !names.is_empty() {
				prefix.push_str("unset -v ");
				prefix.push_str(&names.join(" "));
				prefix.push_str("; ");
			}
		}
		#[cfg(windows)]
		for name in unset.iter().filter(|name| valid_env_name(name)) {
			prefix.push_str("set \"");
			prefix.push_str(name);
			prefix.push_str("=\" && ");
		}
		if !profile.command_prefix.is_empty() {
			prefix.push_str(&profile.command_prefix);
			prefix.push(' ');
		}
		Str::from(prefix)
	}

	async fn environment(
		&self,
		cwd_uri: &str,
		user: BTreeMap<Str, Option<Str>>,
		pty: bool,
	) -> EnvironmentDelta {
		use super::direnv::load;
		let direnv = if self.settings.direnv == DirenvMode::Auto {
			Url::parse(cwd_uri)
				.ok()
				.and_then(|url| url.to_file_path().ok())
				.map(|cwd| async move {
					load(&cwd, Duration::from_millis(self.settings.direnv_load_timeout_ms)).await
				})
		} else {
			None
		};
		let direnv = match direnv {
			Some(load) => load.await,
			None => None,
		};
		hardened_environment(user, pty, direnv)
	}
}

fn hardened_environment(
	user: BTreeMap<Str, Option<Str>>,
	pty: bool,
	direnv: Option<DirenvDelta>,
) -> EnvironmentDelta {
	let mut set: BTreeMap<String, String> = [
		("PAGER", "cat"),
		("GIT_PAGER", "cat"),
		("MANPAGER", "cat"),
		("SYSTEMD_PAGER", "cat"),
		("BAT_PAGER", "cat"),
		("DELTA_PAGER", "cat"),
		("GH_PAGER", "cat"),
		("GLAB_PAGER", "cat"),
		("AWS_PAGER", ""),
		("PSQL_PAGER", "cat"),
		("MYSQL_PAGER", "cat"),
		("HOMEBREW_PAGER", "cat"),
		("LESS", "FRX"),
		("NO_COLOR", "1"),
		("PYTHONUNBUFFERED", "1"),
		("GIT_EDITOR", "true"),
		("VISUAL", "true"),
		("EDITOR", "true"),
		("GIT_TERMINAL_PROMPT", "0"),
		("SSH_ASKPASS", "false"),
		("CI", "true"),
		("AGENT", "1"),
		("npm_config_yes", "true"),
		("npm_config_update_notifier", "false"),
		("npm_config_fund", "false"),
		("npm_config_audit", "false"),
		("PNPM_DISABLE_SELF_UPDATE_CHECK", "true"),
		("YARN_ENABLE_TELEMETRY", "0"),
		("PNPM_UPDATE_NOTIFIER", "false"),
		("YARN_ENABLE_PROGRESS_BARS", "0"),
		("CARGO_TERM_PROGRESS_WHEN", "never"),
		("PIP_NO_INPUT", "1"),
		("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
		("GH_PROMPT_DISABLED", "1"),
		("DEBIAN_FRONTEND", "noninteractive"),
		("TF_INPUT", "0"),
		("TF_IN_AUTOMATION", "1"),
		("COMPOSER_NO_INTERACTION", "1"),
		("CLOUDSDK_CORE_DISABLE_PROMPTS", "1"),
	]
	.into_iter()
	.map(|(key, value)| (String::from(key), String::from(value)))
	.collect();
	if let Some(direnv) = &direnv {
		set.extend(
			direnv
				.set
				.iter()
				.map(|(key, value)| (key.to_string(), value.to_string())),
		);
	}
	if !pty {
		set.insert(String::from("TERM"), String::from("dumb"));
	}
	if env::var_os("OMP_BASH_NO_CI").is_some_and(|value| {
		let value = value.to_string_lossy();
		!value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
	}) {
		set.remove("CI");
	}
	let mut unset = direnv
		.into_iter()
		.flat_map(|delta| delta.unset)
		.map(|key| key.to_string())
		.collect::<BTreeSet<_>>();
	for (key, value) in user {
		let key = key.to_string();
		match value {
			Some(value) => {
				unset.remove(&key);
				set.insert(key, value.to_string());
			},
			None => {
				set.remove(&key);
				unset.insert(key);
			},
		}
	}
	EnvironmentDelta { set, unset: unset.into_iter().collect(), props: None }
}

fn command_environment(environment: BTreeMap<Str, Option<Str>>) -> EnvironmentDelta {
	let mut set = BTreeMap::new();
	let mut unset = Vec::new();
	for (name, value) in environment {
		match value {
			Some(value) => {
				set.insert(name.to_string(), value.to_string());
			},
			None => unset.push(name.to_string()),
		}
	}
	EnvironmentDelta { set, unset, props: None }
}

fn shell_word(word: &str) -> String {
	format!("'{}'", word.replace('\'', "'\\''"))
}

fn valid_env_name(name: &str) -> bool {
	let mut bytes = name.bytes();
	bytes
		.next()
		.is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
		&& bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn named_process(started: v1::ProcessStarted) -> DetachedJob {
	let id = sf!("{}#{}", started.name, started.generation);
	DetachedJob {
		id,
		owner: JobOwner::NamedProcess {
			name:       Str::from(started.name),
			generation: started.generation,
		},
	}
}

fn cwd_fault(message: impl Into<Str>) -> Fault {
	Fault::Resource { operation: sf!("cwd"), message: message.into() }
}
fn env_path(cwd_uri: &str) -> Result<EnvPath, Fault> {
	let path = Url::parse(cwd_uri)
		.map_err(|error| cwd_fault(format!("working-directory URI is invalid: {error}")))?
		.to_file_path()
		.map_err(|()| cwd_fault("working-directory URI is not a local file URI"))?;
	EnvPath::new(Str::from(path.to_string_lossy().as_ref()))
		.map_err(|error| cwd_fault(format!("working-directory path is invalid: {error}")))
}
/// Foreground shell run retaining the concrete host's process-tree guard.
pub(crate) struct HostShellRun {
	host:            ExecHost,
	run:             ExecRun,
	rerun:           Option<SandboxRerun>,
	pending_denial:  Option<RunEvent>,
	approval:        tokio::sync::Mutex<Option<Pin<Box<dyn Future<Output = bool> + Send>>>>,
	sequence_offset: u64,
	last_sequence:   u64,
}

struct SandboxRerun {
	request: ExecRequest,
	timeout: Option<Duration>,
}

impl HostShellRun {
	fn new(host: ExecHost, run: ExecRun, request: ExecRequest, timeout: Option<Duration>) -> Self {
		Self {
			host,
			run,
			rerun: Some(SandboxRerun { request, timeout }),
			pending_denial: None,
			approval: tokio::sync::Mutex::new(None),
			sequence_offset: 0,
			last_sequence: 0,
		}
	}

	fn map_event(&mut self, event: ExecEvent) -> Result<RunEvent, Fault> {
		let mut event = map_event(event)?;
		if let RunEvent::Output(update) = &mut event {
			update.sequence = update.sequence.saturating_add(self.sequence_offset);
			self.last_sequence = self.last_sequence.max(update.sequence);
		}
		Ok(event)
	}
}

impl ShellRun for HostShellRun {
	async fn next_event(&mut self) -> Result<Option<RunEvent>, Fault> {
		loop {
			if let Some(approval) = self.approval.get_mut().as_mut() {
				let approved = approval.await;
				*self.approval.get_mut() = None;
				let denial = self
					.pending_denial
					.take()
					.expect("sandbox approval always retains its denied result");
				if !approved {
					return Ok(Some(denial));
				}
				let rerun = self
					.rerun
					.take()
					.expect("sandbox approval always retains its rerun request");
				let denied_exec = Bytes::copy_from_slice(self.run.id());
				let (_, run) = match self
					.host
					.exec_without_sandbox(rerun.request, rerun.timeout)
					.await
				{
					Ok(started) => started,
					Err(_) => return Ok(Some(denial)),
				};
				self.run = run;
				self.sequence_offset = self.last_sequence.saturating_add(1);
				let sequence = self.sequence_offset;
				self.last_sequence = sequence;
				return Ok(Some(RunEvent::Output(Update {
					channel: OutputChannel::Stderr,
					data: CowBytes::owned(Bytes::from_static(
						b"[sandbox] original run denied; rerun without sandbox after approval\n",
					)),
					sequence,
					exec_id: denied_exec,
					started: false,
					terminal: false,
				})));
			}

			let Some(event) = self.run.next_event().await else {
				return Ok(None);
			};
			let denied_path = self
				.rerun
				.as_ref()
				.and_then(|_| sandbox_denied_event_path(&event));
			let event = self.map_event(event)?;
			let Some(denied_path) = denied_path else {
				return Ok(Some(event));
			};
			let command = self
				.rerun
				.as_ref()
				.and_then(|rerun| rerun.request.source.as_ref())
				.map_or_else(Str::default, |source| Str::from(source.text.as_str()));
			let host = self.host.clone();
			self.pending_denial = Some(event);
			*self.approval.get_mut() = Some(Box::pin(async move {
				host
					.approve_sandbox_bypass(&command, &denied_path)
					.await
			}));
		}
	}

	fn cancel(&self) -> impl Future<Output = Result<(), Fault>> + Send + '_ {
		self.run.cancel();
		future::ready(Ok(()))
	}

	fn detach(&self, name: Str) -> impl Future<Output = Result<DetachedJob, Fault>> + Send + '_ {
		future::ready(
			self
				.host
				.detach_exec(self.run.id(), &name)
				.map(named_process)
				.map_err(|error| resource_fault("detach_running", error)),
		)
	}
}

struct RemoteShellRun {
	client: EnvClient,
	run:    tokio::sync::Mutex<Option<ClientExecRun>>,
	exec:   Mutex<Option<Bytes>>,
}

impl ShellRun for RemoteShellRun {
	async fn next_event(&mut self) -> Result<Option<RunEvent>, Fault> {
		let mut run = self.run.lock().await;
		let Some(run) = run.as_mut() else {
			return Ok(None);
		};
		let event = run
			.next_event()
			.await
			.map_err(|error| protocol_fault("run", sf!("{error}")))?;
		if let Some(ClientExecEvent::Started(started)) = &event {
			*self.exec.lock() = Some(started.exec.clone());
		}
		event.map(map_client_event).transpose()
	}

	async fn cancel(&self) -> Result<(), Fault> {
		if let Some(run) = self.run.lock().await.as_ref() {
			run.guard().cancel();
		}
		Ok(())
	}

	async fn detach(&self, name: Str) -> Result<DetachedJob, Fault> {
		let exec = self.exec.lock().clone().ok_or_else(|| Fault::Resource {
			operation: sf!("detach_running"),
			message:   sf!("remote execution has not started"),
		})?;
		let run = self
			.run
			.lock()
			.await
			.take()
			.ok_or_else(|| Fault::Resource {
				operation: sf!("detach_running"),
				message:   sf!("remote execution is no longer active"),
			})?;
		self
			.client
			.detach_exec(run, exec, name.to_string())
			.await
			.map(named_process)
			.map_err(|error| protocol_fault("detach_running", sf!("{error}")))
	}
}

/// Foreground run selected from the capability-advertised ACP backend or the
/// normal Environment host.
pub struct SelectedShellRun {
	kind: SelectedShellRunKind,
}

enum SelectedShellRunKind {
	Host(HostShellRun),
	Remote(RemoteShellRun),
	Acp(AcpExecRun),
}

impl ShellRun for SelectedShellRun {
	async fn next_event(&mut self) -> Result<Option<RunEvent>, Fault> {
		match &mut self.kind {
			SelectedShellRunKind::Host(run) => run.next_event().await,
			SelectedShellRunKind::Remote(run) => run.next_event().await,
			SelectedShellRunKind::Acp(run) => match run.events.recv_async().await {
				Ok(event) => event.map(Some),
				Err(_) => Ok(None),
			},
		}
	}

	async fn cancel(&self) -> Result<(), Fault> {
		match &self.kind {
			SelectedShellRunKind::Host(run) => {
				run.run.cancel();
				Ok(())
			},
			SelectedShellRunKind::Remote(run) => run.cancel().await,
			SelectedShellRunKind::Acp(run) => {
				run.cancel.cancel();
				Ok(())
			},
		}
	}

	async fn detach(&self, name: Str) -> Result<DetachedJob, Fault> {
		match &self.kind {
			SelectedShellRunKind::Host(run) => run
				.host
				.detach_exec(run.run.id(), &name)
				.map(named_process)
				.map_err(|error| resource_fault("detach_running", error)),
			SelectedShellRunKind::Remote(run) => run.detach(name).await,
			SelectedShellRunKind::Acp(_) => Err(Fault::Resource {
				operation: sf!("detach_running"),
				message:   sf!("ACP terminal runs remain foreground-owned by the editor"),
			}),
		}
	}
}

impl ShellExec for ShellExecHost {
	type Run = SelectedShellRun;

	async fn open_session(&self, options: SessionOptions) -> Result<Session, Fault> {
		if options.pty && tools::pty_denied() {
			return Err(Fault::PtyDenied);
		}
		let cwd_uri = self.resolve_cwd(options.cwd.as_deref()).await?;
		let pty = options.pty;
		let environment = self
			.environment(&cwd_uri, self.expand_environment(options.env).await?, pty)
			.await;
		if self.sandbox.mode == ExecSandboxMode::Off
			&& self.sandbox.environment_policy_is_default()
			&& self.acp_routing
			&& self.acp.backend().is_some()
			&& !pty
		{
			let cwd = Url::parse(&cwd_uri)
				.ok()
				.and_then(|uri| uri.to_file_path().ok())
				.map(|path| Str::from(path.to_string_lossy().as_ref()));
			let env = environment
				.set
				.iter()
				.map(|(name, value)| (Str::from(name.as_str()), Str::from(value.as_str())))
				.collect();
			let unset = environment.unset;
			let id = Bytes::from(format!("acp:{}", omp_core::Ulid::generate()));
			self
				.acp_sessions
				.lock()
				.insert(id.clone(), AcpSessionOptions { cwd, env, unset });
			return Ok(Session { id });
		}
		let request = OpenSessionRequest {
			cwd_uri: cwd_uri.to_string(),
			env_delta: Some(environment),
			pty: pty
				.then(|| PtySpec { terminal: String::from("xterm-256color"), ..Default::default() }),
			shell_profile: Some(self.shell_profile().await),
			..Default::default()
		};
		let opened =
			match &self.backend {
				ShellBackend::Local(host) => host
					.open_session(request)
					.await
					.map_err(|error| resource_fault("open_session", error))?,
				ShellBackend::Remote(client) => client
					.open_session(&env_path(&cwd_uri)?, request)
					.await
					.map_err(|error| protocol_fault("open_session", sf!("{error}")))?,
			};
		Ok(Session { id: opened.session })
	}

	fn close_session<'a>(
		&'a self,
		session: &'a Session,
	) -> impl Future<Output = Result<(), Fault>> + Send + 'a {
		async move {
			if self.acp_sessions.lock().remove(&session.id).is_some() {
				return Ok(());
			}
			match &self.backend {
				ShellBackend::Local(host) => host
					.close_session(&session.id)
					.map(|_| ())
					.map_err(|error| resource_fault("close_session", error)),
				ShellBackend::Remote(client) => client
					.close_session(CloseSessionRequest {
						session: session.id.clone(),
						..Default::default()
					})
					.await
					.map(|_| ())
					.map_err(|error| protocol_fault("close_session", sf!("{error}"))),
			}
		}
	}

	async fn run<'a>(
		&'a self,
		session: &'a Session,
		request: RunRequest,
	) -> Result<Self::Run, Fault> {
		let command = self
			.expand_internal_uris(request.command.as_str(), true)
			.await?;
		let environment = command_environment(self.expand_environment(request.environment).await?);
		let acp_options = self.acp_sessions.lock().get(&session.id).cloned();
		if self.sandbox.mode == ExecSandboxMode::Off
			&& self.sandbox.environment_policy_is_default()
			&& let Some(options) = acp_options
		{
			let backend = self.acp.backend().ok_or_else(|| Fault::Resource {
				operation: sf!("run"),
				message:   sf!("ACP terminal backend disconnected"),
			})?;
			let mut env = options.env;
			env.extend(
				environment
					.set
					.iter()
					.map(|(name, value)| (Str::from(name.as_str()), Str::from(value.as_str()))),
			);
			let mut unset = options.unset;
			unset.retain(|name| !environment.set.contains_key(name));
			unset.extend(environment.unset.iter().cloned());
			let command_prefix = self.acp_command_prefix(&unset).await;
			return backend
				.run(AcpExecRequest {
					command: if command_prefix.is_empty() {
						command
					} else {
						sf!("{}{}", command_prefix, command)
					},
					cwd: options.cwd,
					env,
					timeout_ms: request.timeout_ms,
				})
				.await
				.map(|run| SelectedShellRun { kind: SelectedShellRunKind::Acp(run) });
		}
		let mut exec_request = ExecRequest {
			session: session.id.clone(),
			source: Some(Script { text: command.to_string(), ..Default::default() }),
			..Default::default()
		};
		super::exec::set_run_environment(&mut exec_request, environment);
		match &self.backend {
			ShellBackend::Local(host) => {
				let (_, run) = host
					.exec(exec_request.clone(), request.timeout_ms.map(Duration::from_millis))
					.await
					.map_err(|error| resource_fault("run", error))?;
				Ok(SelectedShellRun {
					kind: SelectedShellRunKind::Host(HostShellRun::new(
						host.clone(),
						run,
						exec_request,
						request.timeout_ms.map(Duration::from_millis),
					)),
				})
			},
			ShellBackend::Remote(client) => {
				let run = client
					.exec(exec_request)
					.await
					.map_err(|error| protocol_fault("run", sf!("{error}")))?;
				Ok(SelectedShellRun {
					kind: SelectedShellRunKind::Remote(RemoteShellRun {
						client: client.clone(),
						run:    tokio::sync::Mutex::new(Some(run)),
						exec:   Mutex::new(None),
					}),
				})
			},
		}
	}

	async fn detach(&self, request: DetachRequest) -> Result<DetachedJob, Fault> {
		if request.options.pty && tools::pty_denied() {
			return Err(Fault::PtyDenied);
		}
		let cwd_uri = self.resolve_cwd(request.options.cwd.as_deref()).await?;
		let pty = request.options.pty;
		let environment = self
			.environment(&cwd_uri, self.expand_environment(request.options.env).await?, pty)
			.await;
		let command = self
			.expand_internal_uris(request.command.as_str(), true)
			.await?;
		let start = StartProcess {
			name: request.name.to_string(),
			spec: Some(ProcessSpec {
				source: Some(Script {
					text: self.detached_command(&command).await,
					..Default::default()
				}),
				cwd_uri: cwd_uri.to_string(),
				env_delta: Some(environment),
				pty: pty
					.then(|| PtySpec { terminal: String::from("xterm-256color"), ..Default::default() }),
				restart: Some(RestartSpec {
					policy: RestartPolicy::Never as i32,
					..Default::default()
				}),
				timeout_ms: request.timeout_ms.filter(|timeout| *timeout != 0),
				..Default::default()
			}),
			..Default::default()
		};
		let started = match &self.backend {
			ShellBackend::Local(host) => host
				.start_process(start)
				.await
				.map_err(|error| resource_fault("detach", error))?,
			ShellBackend::Remote(client) => client
				.start_process(&env_path(&cwd_uri)?, start)
				.await
				.map_err(|error| protocol_fault("detach", sf!("{error}")))?,
		};
		Ok(named_process(started))
	}
}

fn map_client_event(event: ClientExecEvent) -> Result<RunEvent, Fault> {
	match event {
		ClientExecEvent::Started(started) => map_event(ExecEvent::Started { exec_id: started.exec }),
		ClientExecEvent::Output(output) => map_event(ExecEvent::Output(output)),
		ClientExecEvent::Exit(exit) => map_event(ExecEvent::Exit(exit)),
	}
}

fn map_event(event: ExecEvent) -> Result<RunEvent, Fault> {
	match event {
		ExecEvent::Started { exec_id } => Ok(RunEvent::Started { exec_id }),
		ExecEvent::Output(frame) => {
			let channel = match EnvOutputChannel::try_from(frame.channel) {
				Ok(EnvOutputChannel::Stdout) => OutputChannel::Stdout,
				Ok(EnvOutputChannel::Stderr) => OutputChannel::Stderr,
				Ok(EnvOutputChannel::Pty) => OutputChannel::Pty,
				Ok(EnvOutputChannel::Unspecified) | Err(_) => {
					return Err(protocol_fault(
						"next_event",
						sf!("invalid output channel {}", frame.channel),
					));
				},
			};
			Ok(RunEvent::Output(Update {
				channel,
				data: CowBytes::owned(frame.data),
				sequence: frame.sequence,
				exec_id: frame.exec,
				started: false,
				terminal: channel == OutputChannel::Pty,
			}))
		},
		ExecEvent::Exit(event) => {
			let status = event
				.status
				.ok_or_else(|| protocol_fault("next_event", "terminal event omitted status"))?;
			let outcome = match EnvExecOutcome::try_from(status.outcome) {
				Ok(EnvExecOutcome::Exited) => ExecOutcome::Exited,
				Ok(EnvExecOutcome::Failed) => ExecOutcome::Failed,
				Ok(EnvExecOutcome::Timeout) => ExecOutcome::Timeout,
				Ok(EnvExecOutcome::Cancelled) => ExecOutcome::Cancelled,
				Ok(EnvExecOutcome::Denied) => ExecOutcome::Denied,
				Ok(EnvExecOutcome::Unspecified) | Err(_) => {
					return Err(protocol_fault(
						"next_event",
						sf!("invalid execution outcome {}", status.outcome),
					));
				},
			};
			let signal = (!status.signal.is_empty()).then(|| Str::from(status.signal));
			let spilled_output = status.spilled_output.map(|blob| BlobRef {
				hash:       Str::from(hex::encode(&blob.hash).into_string()),
				media_type: Str::from(blob.mime),
				byte_len:   blob.size,
			});
			Ok(RunEvent::Exit(ExecStatus {
				outcome,
				exit_code: status.exit_code,
				signal,
				wall_clock_ms: status.wall_clock_ms,
				spilled_output,
				aborted: status.aborted,
				effects_unknown: false,
				final_cwd_uri: (!event.final_cwd_uri.is_empty())
					.then(|| Str::from(event.final_cwd_uri)),
				final_cwd_revision: event.final_cwd_revision,
			}))
		},
	}
}

fn resource_fault(operation: &'static str, error: ExecError) -> Fault {
	protocol_fault(operation, sf!("{error}"))
}

fn protocol_fault(operation: &'static str, message: impl Into<Str>) -> Fault {
	Fault::Resource { operation: sf!(operation), message: message.into() }
}

#[cfg(test)]
mod tests {
	use super::*;

	fn test_host(root: &Path) -> ShellExecHost {
		let root_uri = Url::from_directory_path(root)
			.expect("workspace URI")
			.to_string();
		ShellExecHost::new(
			ExecHost::new(),
			Str::from(root_uri),
			Arc::new(ResolverTable::default()),
			ShellSettings::default(),
			SandboxSettings::default(),
			AcpExecSlot::default(),
			false,
		)
	}

	#[cfg(target_os = "macos")]
	#[tokio::test]
	async fn approved_sandbox_denial_reruns_exactly_once_without_hooks() {
		if !omp_sandbox::backend_status(omp_sandbox::Backend::Seatbelt).is_available() {
			return;
		}
		let root = tempfile::tempdir().expect("workspace");
		std::fs::create_dir(root.path().join(".git")).expect("git carve-out");
		let exec = ExecHost::new();
		let book = Arc::new(omp_agent::ApprovalBook::new());
		let (route, inbox) = omp_agent::ApprovalRoute::new(Arc::clone(&book), None);
		exec.bind_sandbox_approval_route(Some(route));
		let root_uri = Url::from_directory_path(root.path())
			.expect("workspace URI")
			.to_string();
		let host = ShellExecHost::new(
			exec,
			Str::from(root_uri),
			Arc::new(ResolverTable::default()),
			ShellSettings::default(),
			SandboxSettings {
				mode: ExecSandboxMode::WorkspaceWrite,
				..SandboxSettings::default()
			},
			AcpExecSlot::default(),
			false,
		);
		let approver = tokio::spawn(async move {
			let request = inbox.recv().await.expect("sandbox approval ticket");
			let reason = request
				.ticket
				.reasons
				.first()
				.expect("sandbox approval reason");
			assert_eq!(reason.kind, "sandbox_bypass");
			assert!(
				reason
					.pattern
					.as_deref()
					.is_some_and(|command| command == "echo approved > .git/approved.txt")
			);
			assert!(reason.subject.ends_with(".git/approved.txt"));
			request
				.respond(omp_agent::ApprovalDecision {
					approved:   true,
					scope:      sf!("once"),
					source:     omp_agent::ApprovalSource::User,
					decided_by: Some(sf!("test approver")),
					reason:     None,
					audited:    false,
				})
				.expect("approve sandbox bypass");
		});

		let session = host
			.open_session(SessionOptions::default())
			.await
			.expect("sandbox session");
		let mut run = host
			.run(&session, RunRequest {
				command:     sf!("echo approved > .git/approved.txt"),
				environment: BTreeMap::new(),
				timeout_ms:  Some(5_000),
			})
			.await
			.expect("sandboxed command starts");
		let mut output = Vec::new();
		let mut starts = 0;
		let status = loop {
			match run.next_event().await.expect("shell event") {
				Some(RunEvent::Started { .. }) => starts += 1,
				Some(RunEvent::Output(update)) => output.extend_from_slice(update.data.as_ref()),
				Some(RunEvent::Exit(status)) => break status,
				None => panic!("shell event stream closed before exit"),
			}
		};
		approver.await.expect("approver task");
		assert_eq!(starts, 2);
		assert_eq!(status.outcome, ExecOutcome::Exited);
		assert_eq!(status.exit_code, Some(0));
		let output = String::from_utf8_lossy(&output);
		assert!(output.contains("sandbox denied write"));
		assert!(output.contains("rerun without sandbox after approval"));
		assert_eq!(
			std::fs::read(root.path().join(".git/approved.txt")).expect("approved write"),
			b"approved\n"
		);
		host.close_session(&session).await.expect("close session");
	}

	#[tokio::test]
	async fn authenticated_pty_denial_is_invocation_local_and_plain_exec_still_runs() {
		use super::super::tools::with_invocation_scope;
		let root = tempfile::tempdir().expect("workspace");
		let host = test_host(root.path());
		let denied_host = host.clone();
		let allowed_host = host.clone();
		let denied = tokio::spawn(with_invocation_scope(true, async move {
			denied_host
				.open_session(SessionOptions { pty: true, ..SessionOptions::default() })
				.await
		}));
		let allowed = tokio::spawn(with_invocation_scope(false, async move {
			allowed_host
				.open_session(SessionOptions { pty: true, ..SessionOptions::default() })
				.await
		}));
		assert_eq!(denied.await.expect("denied scope task"), Err(Fault::PtyDenied));
		let allowed_session = allowed
			.await
			.expect("allowed scope task")
			.expect("unrestricted scope allocates a PTY");
		host
			.close_session(&allowed_session)
			.await
			.expect("close PTY session");

		let plain_session = with_invocation_scope(true, host.open_session(SessionOptions::default()))
			.await
			.expect("denied scope permits non-PTY session");
		let mut run = host
			.run(&plain_session, RunRequest {
				command:     sf!("printf scope-ok"),
				environment: BTreeMap::new(),
				timeout_ms:  Some(5_000),
			})
			.await
			.expect("plain execution starts");
		let mut exited = false;
		while let Some(event) = run.next_event().await.expect("plain execution event") {
			if let RunEvent::Exit(status) = event {
				assert_eq!(status.outcome, ExecOutcome::Exited);
				exited = true;
				break;
			}
		}
		assert!(exited, "plain execution must report terminal status");
		host
			.close_session(&plain_session)
			.await
			.expect("close plain session");
	}
}
