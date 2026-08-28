//! Envd-owned loopback bridge behind the embedded shell's `dyn` builtin.

use std::{
	fmt::{self, Write as _},
	fs,
	io::{self, Read as _, Write as _},
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};

use bytes::Bytes;
use futures::StreamExt as _;
use omp_agent::{GateEvent, GateOutcome, HookGate};
use omp_core::{Duration, DurationUnit, Hash32, Str, sf};
use omp_proto::toolhost::v1::HookEventId;
use omp_shell_engine::{
	builtins::{ContentOptions, ContentType, Registration},
	commands::{CommandArg, ExecutionContext},
	error,
	extensions::ShellExtensions,
	results::ExecutionResult,
};
use omp_tool::{
	DevicePath, ErasedEv, ErasedOutcome, ErasedStream, IncomingParams, Part, PromptCaps, Registry,
	RegistryError, ToolIdentity, ToolRoute,
};
use omp_tools::{
	device::{
		CatalogQuery, DeviceCatalog, DeviceInvokeRequest, ErasedDeviceInvoker, render_catalog,
		render_catalog_query, render_device_docs, render_near_miss,
	},
	device_ctl::{BlobSource, CliParseError, DeviceCli},
	staging::{ProposalDecision, ProposalRejection, StagedProposalRegistry},
};
use parking_lot::Mutex;
use serde_json::{Map, Value};
use thiserror::Error;

const DYN_HELP: &str = "dyn — invoke dynamic devices through the embedded shell\n\nUsage:\ndyn \
                        [--q TEXT] [--tag TAG] [--provenance OWNER] [--offset N] [--limit N]\ndyn \
                        [--depth N] [--under SUBTREE]\ndyn <device> --help\ndyn <device> \
                        [args…]\ndyn <device> --json '<payload>'\ndyn resolve \"<one-sentence \
                        reason>\"\ndyn reject \"<one-sentence reason>\"\n\nThe first tokens \
                        `resolve`, `reject`, and `help` are reserved by this builtin.";

#[derive(Clone)]
struct CatalogCache {
	hash:     Hash32,
	rendered: Str,
}

/// Envd-owned loopback bridge behind the `dyn` shell builtin.
pub struct DynHost {
	catalog:            DeviceCatalog,
	invoker:            Arc<dyn ErasedDeviceInvoker>,
	proposals:          StagedProposalRegistry,
	hooks:              Arc<HookGate>,
	catalog_cache:      Mutex<Option<CatalogCache>>,
	next_invocation_id: AtomicU64,
}

impl DynHost {
	/// Binds one live device catalog, worker dispatcher, proposal registry, and
	/// session hook gate.
	pub fn new(
		catalog: DeviceCatalog,
		invoker: Arc<dyn ErasedDeviceInvoker>,
		proposals: StagedProposalRegistry,
		hooks: Arc<HookGate>,
	) -> Self {
		Self {
			catalog,
			invoker,
			proposals,
			hooks,
			catalog_cache: Mutex::new(None),
			next_invocation_id: AtomicU64::new(1),
		}
	}

	async fn catalog(
		&self,
		registry: &Registry,
		query: &CatalogQuery,
		subtree: Option<&str>,
	) -> Result<Str, Str> {
		if self.hooks.subscribed(HookEventId::HookEventDeviceList) {
			let device_hash = registry.device_hash();
			let devices = registry
				.devices()
				.map(device_event_json)
				.collect::<Vec<_>>();
			let payload = serde_json::to_vec(&serde_json::json!({
				"devices": devices,
				"turn_id": null,
			}))
			.expect("device list payload must serialize to JSON");
			let outcome = self
				.hooks
				.gate(
					HookEventId::HookEventDeviceList,
					GateEvent::new(sf!("device_list:{}", device_hash.to_hex()), Bytes::from(payload)),
				)
				.await;
			let effective = match outcome {
				GateOutcome::Allow { event, .. } => event.effective_args,
				GateOutcome::Deny { reason, .. } => return Err(reason),
				GateOutcome::Approval { .. } => {
					return Err(sf!("device list cannot require approval"));
				},
			};
			let effective: Value = serde_json::from_slice(&effective)
				.map_err(|_| sf!("device list returned a malformed effective payload"))?;
			let selected = effective
				.get("devices")
				.and_then(Value::as_array)
				.ok_or_else(|| sf!("device list omitted its effective devices"))?
				.iter()
				.filter_map(|device| device.get("name").and_then(Value::as_str))
				.collect::<std::collections::BTreeSet<_>>();
			return Ok(render_catalog_query(
				registry
					.devices()
					.filter(|device| selected.contains(device.name.as_str())),
				query,
				subtree,
			));
		}
		if *query != CatalogQuery::default() || subtree.is_some() {
			return Ok(render_catalog_query(registry.devices(), query, subtree));
		}
		let hash = registry.device_hash();
		if let Some(cached) = self
			.catalog_cache
			.lock()
			.as_ref()
			.filter(|cached| cached.hash == hash)
		{
			return Ok(cached.rendered.clone());
		}
		let rendered = render_catalog(registry.devices());
		*self.catalog_cache.lock() = Some(CatalogCache { hash, rendered: rendered.clone() });
		Ok(rendered)
	}

	fn invocation_id(&self) -> Str {
		let sequence = self.next_invocation_id.fetch_add(1, Ordering::Relaxed);
		sf!("dyn-{sequence}")
	}
}

fn device_event_json(device: omp_tool::MountedDevice<'_>) -> Value {
	let place = match device.route {
		ToolRoute::Native => String::from("env"),
		ToolRoute::Worker { name, .. } => format!("worker:{name}"),
	};
	let mut row = Map::from_iter([
		("name".to_owned(), Value::String(device.name.to_string())),
		("family".to_owned(), Value::String(device.rev.family.to_string())),
		("rev".to_owned(), Value::from(device.rev.n)),
		("identity".to_owned(), Value::String(format!("{}@{}", device.name, device.claimant))),
		("claimant".to_owned(), Value::String(device.claimant.to_string())),
		("path".to_owned(), Value::String(device.name.to_string())),
		("summary".to_owned(), Value::String(device.summary.to_string())),
		("place".to_owned(), Value::String(place)),
		("precedence".to_owned(), Value::from(device.precedence.0)),
		(
			"effects".to_owned(),
			serde_json::to_value(device.effects).expect("device effects must serialize to JSON"),
		),
		("mounted".to_owned(), Value::Bool(true)),
		("enabled".to_owned(), Value::Bool(true)),
		("available".to_owned(), Value::Bool(true)),
		("reason".to_owned(), Value::Null),
		("shadowed_by".to_owned(), Value::Null),
		("source".to_owned(), Value::String(device.claimant.to_string())),
		("slotted".to_owned(), Value::Bool(false)),
		("schema_bytes".to_owned(), Value::from(device.schema.len())),
	]);
	if let Some(metadata) = device.metadata {
		let mut provenance = Map::new();
		for (name, value) in [
			("publisher", metadata.publisher.as_ref()),
			("extension_id", metadata.extension_id.as_ref()),
			("version", metadata.version.as_ref()),
			("artifact_digest", metadata.artifact_digest.as_ref()),
			("layer", metadata.layer.as_ref()),
			("tier", metadata.tier.as_ref()),
		] {
			if let Some(value) = value {
				provenance.insert(name.to_owned(), Value::String(value.to_string()));
			}
		}
		if let Some(generation) = metadata.generation {
			provenance.insert("generation".to_owned(), Value::from(generation));
		}
		if !provenance.is_empty() {
			row.insert("provenance".to_owned(), Value::Object(provenance));
		}
		if let Some(tier) = &metadata.tier {
			row.insert("tier".to_owned(), Value::String(tier.to_string()));
		}
	}
	Value::Object(row)
}

/// Builds the `dyn` builtin registration bound to one live environment host.
pub fn registration<SE: ShellExtensions>(host: Arc<DynHost>) -> Registration<SE> {
	Registration {
		execute_func: Arc::new(move |context, args| {
			let host = Arc::clone(&host);
			Box::pin(async move { run(host, context, args).await })
		}),
		content_func: content,
		disabled: false,
		special_builtin: false,
		declaration_builtin: false,
		transparent_background_wrapper: false,
	}
}

fn content(
	_name: &str,
	_content_type: ContentType,
	_options: &ContentOptions,
) -> Result<String, error::Error> {
	Ok(DYN_HELP.to_owned())
}

// A device must not re-enter this ExecHost session while the builtin awaits it.
async fn run<SE: ShellExtensions>(
	host: Arc<DynHost>,
	context: ExecutionContext<'_, SE>,
	args: Vec<CommandArg>,
) -> Result<ExecutionResult, error::Error> {
	let argv = args
		.into_iter()
		.skip(1)
		.map(|argument| match argument {
			CommandArg::String(value) => Str::from(value),
			CommandArg::Assignment(value) => Str::from(value.to_string()),
		})
		.collect::<Vec<_>>();

	if argv.len() == 1 && is_help(argv[0].as_str()) {
		print_stdout(&context, DYN_HELP)?;
		return Ok(ExecutionResult::success());
	}
	let Some(registry) = host.catalog.registry() else {
		print_error(&context, "device catalog is not available in this session")?;
		return Ok(ExecutionResult::general_error());
	};

	let Some(first) = argv.first() else {
		let rendered = match host
			.catalog(&registry, &CatalogQuery::default(), None)
			.await
		{
			Ok(rendered) => rendered,
			Err(reason) => {
				print_error(&context, reason.as_str())?;
				return Ok(ExecutionResult::general_error());
			},
		};
		print_stdout(&context, rendered.as_str())?;
		return Ok(ExecutionResult::success());
	};
	if first.starts_with('-') {
		return catalog_command(&host, &registry, &context, &argv).await;
	}
	if matches!(first.as_str(), "resolve" | "reject") {
		return proposal_command(&host, &context, &argv);
	}
	if first == "help" {
		let Some(device) = argv.get(1) else {
			print_stdout(&context, DYN_HELP)?;
			return Ok(ExecutionResult::success());
		};
		return help_command(&registry, &context, device.as_str());
	}
	if argv[1..].iter().any(|argument| is_help(argument.as_str())) {
		return help_command(&registry, &context, first.as_str());
	}

	invoke_command(host, registry, context, first, &argv[1..]).await
}

async fn catalog_command<SE: ShellExtensions>(
	host: &DynHost,
	registry: &Registry,
	context: &ExecutionContext<'_, SE>,
	argv: &[Str],
) -> Result<ExecutionResult, error::Error> {
	let (query, subtree) = match parse_catalog(argv) {
		Ok(parsed) => parsed,
		Err(parse_error) => {
			print_error(context, &parse_error)?;
			return Ok(ExecutionResult::new(2));
		},
	};
	let rendered = match host.catalog(registry, &query, subtree.as_deref()).await {
		Ok(rendered) => rendered,
		Err(reason) => {
			print_error(context, reason.as_str())?;
			return Ok(ExecutionResult::general_error());
		},
	};
	print_stdout(context, rendered.as_str())?;
	Ok(ExecutionResult::success())
}

fn proposal_command<SE: ShellExtensions>(
	host: &DynHost,
	context: &ExecutionContext<'_, SE>,
	argv: &[Str],
) -> Result<ExecutionResult, error::Error> {
	let verb = argv[0].as_str();
	let reason = argv
		.get(1)
		.map(Str::as_str)
		.map(str::trim)
		.filter(|reason| !reason.is_empty());
	let Some(reason) = reason else {
		print_error(context, "a one-sentence reason is required")?;
		return Ok(ExecutionResult::new(2));
	};
	if argv.len() != 2 {
		print_error(context, "resolve and reject accept exactly one reason")?;
		return Ok(ExecutionResult::new(2));
	}
	let Some(id) = host.proposals.latest_pending() else {
		print_error(context, "no staged proposal is pending")?;
		return Ok(ExecutionResult::general_error());
	};
	let decision = if verb == "resolve" {
		ProposalDecision::Resolve { reason: Str::new(reason) }
	} else {
		ProposalDecision::Reject(ProposalRejection::Requested { reason: Str::new(reason) })
	};
	match host.proposals.finalize(id.as_str(), decision) {
		Ok(outcome) => {
			let payload = outcome.payload.to_string();
			print_stdout(context, &payload)?;
			Ok(ExecutionResult::success())
		},
		Err(proposal_error) => {
			print_error(context, &proposal_error)?;
			Ok(ExecutionResult::general_error())
		},
	}
}

fn help_command<SE: ShellExtensions>(
	registry: &Registry,
	context: &ExecutionContext<'_, SE>,
	raw: &str,
) -> Result<ExecutionResult, error::Error> {
	let Ok(path) = DevicePath::parse(raw) else {
		let near = render_near_miss(raw, registry.devices());
		print_stderr(context, near.as_str())?;
		return Ok(ExecutionResult::new(2));
	};
	let Some(device) = registry
		.devices()
		.find(|device| device.name.as_str() == path.root())
	else {
		let near = render_near_miss(raw, registry.devices());
		print_stderr(context, near.as_str())?;
		return Ok(ExecutionResult::new(2));
	};
	let mut rendered = render_device_docs(&device, raw);
	let invocation = format!("dyn {raw}");
	match serde_json::from_slice::<Value>(device.schema)
		.ok()
		.and_then(|schema| DeviceCli::compile(&schema).ok())
	{
		Some(cli) => {
			rendered.push_str("\n\nUsage:\n");
			rendered.push_str(&cli.usage(&invocation));
		},
		None => rendered.push_str("\n\nThis device accepts only `--json '<payload>'`."),
	}
	print_stdout(context, &rendered)?;
	Ok(ExecutionResult::success())
}

async fn invoke_command<SE: ShellExtensions>(
	host: Arc<DynHost>,
	registry: Arc<Registry>,
	context: ExecutionContext<'_, SE>,
	raw_path: &Str,
	argv: &[Str],
) -> Result<ExecutionResult, error::Error> {
	let Ok(path) = DevicePath::parse(raw_path.as_str()) else {
		let near = render_near_miss(raw_path, registry.devices());
		print_stderr(&context, near.as_str())?;
		return Ok(ExecutionResult::new(2));
	};
	let target = match registry.resolve_device(&path) {
		Ok(target) => target,
		Err(_) => {
			let near = render_near_miss(raw_path, registry.devices());
			print_stderr(&context, near.as_str())?;
			return Ok(ExecutionResult::new(2));
		},
	};
	let Some(device) = registry
		.devices()
		.find(|device| device.name.as_str() == path.root())
	else {
		let near = render_near_miss(raw_path, registry.devices());
		print_stderr(&context, near.as_str())?;
		return Ok(ExecutionResult::new(2));
	};

	let cwd = context.shell.working_dir().to_path_buf();
	let mut stdin = context.stdin();
	let mut blob = |source: BlobSource<'_>| match source {
		BlobSource::Literal(value) => Ok(value.to_owned()),
		BlobSource::File(path) => {
			let resolved = resolve_blob_path(&cwd, path);
			fs::read_to_string(&resolved)
				.map_err(|error| CliParseError::Blob { source: Str::new(path), error })
		},
		BlobSource::Stdin => {
			let mut text = String::new();
			stdin
				.read_to_string(&mut text)
				.map_err(|error| CliParseError::Blob { source: Str::new_static("<stdin>"), error })?;
			Ok(text)
		},
	};
	let parsed = match serde_json::from_slice::<Value>(device.schema)
		.ok()
		.and_then(|schema| DeviceCli::compile(&schema).ok())
	{
		Some(cli) => cli.parse(argv, &mut blob),
		None => parse_raw_json(argv),
	};
	let parsed = match parsed {
		Ok(parsed) => parsed,
		Err(parse_error) => {
			print_error(&context, &parse_error)?;
			let hint = format!("run `dyn {} --help`", raw_path.as_str());
			print_error(&context, &hint)?;
			return Ok(ExecutionResult::new(2));
		},
	};

	let raw = Str::from(Value::Object(parsed).to_string());
	let args_json = Bytes::from(raw.clone());
	let identity = target.identity();
	let claimant = target.claimant.clone();
	let revision = Str::from(target.rev.to_string());
	let route = target.route.clone();
	let name = target.name.clone();
	let mut stream = match route {
		ToolRoute::Native => {
			let (feed, params) = IncomingParams::channel();
			if feed.args_committed(raw).is_err() {
				print_error(&context, "device argument channel closed before dispatch")?;
				return Ok(ExecutionResult::general_error());
			}
			match registry.invoke_device(&path, params) {
				Ok(stream) => stream,
				Err(dispatch_error) => {
					print_dispatch_error(&context, &dispatch_error)?;
					return Ok(ExecutionResult::general_error());
				},
			}
		},
		ToolRoute::Worker { site, name: worker } => {
			host
				.invoker
				.invoke(DeviceInvokeRequest {
					path,
					name,
					rev: revision,
					owner: Some(claimant),
					site: Some(site),
					worker: Some(worker),
					invocation_id: host.invocation_id(),
					deadline: Duration::new(5, DurationUnit::Minutes),
					args_json,
				})
				.await
		},
	};

	consume(&registry, &identity, &context, &mut stream).await
}

async fn consume<SE: ShellExtensions>(
	registry: &Registry,
	identity: &ToolIdentity,
	context: &ExecutionContext<'_, SE>,
	stream: &mut ErasedStream<'_>,
) -> Result<ExecutionResult, error::Error> {
	let cancellation = context.cancel_token();
	loop {
		let event = if let Some(cancellation) = cancellation.as_ref() {
			tokio::select! {
				_ = cancellation.cancelled() => {
					print_error(context, "interrupted")?;
					return Ok(ExecutionResult::new(130));
				},
				event = stream.next() => event,
			}
		} else {
			stream.next().await
		};
		match event {
			Some(Ok(ErasedEv::Update(_))) => {},
			Some(Ok(ErasedEv::Done(ErasedOutcome::Done { verdict, .. }))) => {
				return project_result(registry, identity, context, &verdict);
			},
			Some(Ok(ErasedEv::Done(ErasedOutcome::Detached(job)))) => {
				let message = format!("detached job: {}", job.id);
				print_stdout(context, &message)?;
				return Ok(ExecutionResult::success());
			},
			Some(Err(dispatch_error)) => {
				print_dispatch_error(context, &dispatch_error)?;
				return Ok(ExecutionResult::general_error());
			},
			None => {
				print_error(context, "device dispatch ended without an outcome")?;
				return Ok(ExecutionResult::general_error());
			},
		}
	}
}

fn project_result<SE: ShellExtensions>(
	registry: &Registry,
	identity: &ToolIdentity,
	context: &ExecutionContext<'_, SE>,
	verdict: &[u8],
) -> Result<ExecutionResult, error::Error> {
	let caps = PromptCaps {
		maximum_parts:      16,
		maximum_text_bytes: 262_144,
		media:              false,
		dialect:            Default::default(),
		model_class:        Default::default(),
	};
	let mut rendered = String::new();
	match registry.prompt(identity, verdict, &caps) {
		Ok(Some(parts)) => {
			for text in parts.iter().filter_map(|part| match part {
				Part::Text { text } => Some(text.as_str()),
				Part::Json { .. } | Part::Blob { .. } => None,
			}) {
				if !rendered.is_empty() {
					rendered.push('\n');
				}
				rendered.push_str(text);
			}
		},
		Ok(None) => {},
		Err(RegistryError::UnsupportedExternal { .. }) => {
			render_external_verdict(verdict, &mut rendered);
		},
		Err(projection_error) => {
			print_dispatch_error(context, &projection_error)?;
			return Ok(ExecutionResult::general_error());
		},
	}
	if rendered.is_empty() {
		rendered.push_str("(device returned non-text output)");
	}
	if faulted(verdict) {
		print_stderr(context, &rendered)?;
		Ok(ExecutionResult::general_error())
	} else {
		print_stdout(context, &rendered)?;
		Ok(ExecutionResult::success())
	}
}

fn render_external_verdict(verdict: &[u8], rendered: &mut String) {
	let Ok(verdict) = serde_json::from_slice::<Value>(verdict) else {
		return;
	};
	let Some(value) = verdict.get("value") else {
		return;
	};
	match value {
		Value::String(text) => rendered.push_str(text),
		other => write!(rendered, "{other}").expect("writing JSON into a string cannot fail"),
	}
}

fn faulted(verdict: &[u8]) -> bool {
	serde_json::from_slice::<Value>(verdict)
		.ok()
		.is_some_and(|value| {
			value
				.get("kind")
				.and_then(Value::as_str)
				.is_some_and(|kind| matches!(kind, "fault" | "faulted"))
		})
}

fn resolve_blob_path(cwd: &Path, path: &str) -> PathBuf {
	let path = PathBuf::from(path);
	if path.is_absolute() {
		path
	} else {
		cwd.join(path)
	}
}

fn is_help(argument: &str) -> bool {
	matches!(argument, "--help" | "-h" | "help")
}

#[derive(Debug, Error)]
enum CatalogArgError {
	#[error("unknown catalog option `{flag}`")]
	Unknown { flag: Str },
	#[error("catalog option `{flag}` requires a value")]
	MissingValue { flag: Str },
	#[error("catalog option `{flag}` requires a non-negative integer, found `{value}`")]
	InvalidNumber { flag: Str, value: Str },
}

fn parse_catalog(argv: &[Str]) -> Result<(CatalogQuery, Option<Str>), CatalogArgError> {
	let mut query = CatalogQuery::default();
	let mut subtree = None;
	let mut index = 0;
	while index < argv.len() {
		let argument = argv[index].as_str();
		let (flag, inline) = argument
			.split_once('=')
			.map_or((argument, None), |(flag, value)| (flag, Some(value)));
		let value = |index: &mut usize| -> Result<&str, CatalogArgError> {
			if let Some(value) = inline {
				return Ok(value);
			}
			*index += 1;
			argv
				.get(*index)
				.map(Str::as_str)
				.ok_or_else(|| CatalogArgError::MissingValue { flag: Str::new(flag) })
		};
		match flag {
			"--q" => query.text = Some(Str::new(value(&mut index)?)),
			"--tag" => query.tags.push(Str::new(value(&mut index)?)),
			"--provenance" => query.provenance = Some(Str::new(value(&mut index)?)),
			"--under" => subtree = Some(Str::new(value(&mut index)?)),
			"--offset" => query.offset = parse_number(flag, value(&mut index)?)?,
			"--limit" => query.limit = Some(parse_number(flag, value(&mut index)?)?),
			"--depth" => query.depth = Some(parse_number(flag, value(&mut index)?)?),
			_ => return Err(CatalogArgError::Unknown { flag: Str::new(argument) }),
		}
		index += 1;
	}
	Ok((query, subtree))
}

fn parse_number(flag: &str, value: &str) -> Result<usize, CatalogArgError> {
	value
		.parse()
		.map_err(|_| CatalogArgError::InvalidNumber { flag: Str::new(flag), value: Str::new(value) })
}

fn parse_raw_json(argv: &[Str]) -> Result<Map<String, Value>, CliParseError> {
	let raw = match argv {
		[first, raw] if matches!(first.as_str(), "--json" | "-j") => raw.as_str(),
		[first, _, extra @ ..] if matches!(first.as_str(), "--json" | "-j") => {
			return Err(CliParseError::UnexpectedArgument {
				argument: extra.first().cloned().unwrap_or_else(|| argv[1].clone()),
			});
		},
		[first] if matches!(first.as_str(), "--json" | "-j") => {
			return Err(CliParseError::MissingValue { flag: first.clone() });
		},
		[first] => first
			.as_str()
			.strip_prefix("--json=")
			.or_else(|| first.as_str().strip_prefix("-j="))
			.ok_or_else(|| CliParseError::InvalidValue {
				flag:     Str::new_static("--json"),
				expected: Str::new_static("the only supported argument"),
				found:    first.clone(),
			})?,
		[first, ..] => {
			return Err(CliParseError::InvalidValue {
				flag:     Str::new_static("--json"),
				expected: Str::new_static("the only supported argument"),
				found:    first.clone(),
			});
		},
		[] => {
			return Err(CliParseError::MissingValue { flag: Str::new_static("--json") });
		},
	};
	let value: Value = serde_json::from_str(raw)?;
	value
		.as_object()
		.cloned()
		.ok_or_else(|| CliParseError::InvalidValue {
			flag:     Str::new_static("--json"),
			expected: Str::new_static("a JSON object"),
			found:    Str::new(raw),
		})
}

fn print_stdout<SE: ShellExtensions>(
	context: &ExecutionContext<'_, SE>,
	message: &str,
) -> Result<(), error::Error> {
	write_message(context.stdout(), message)?;
	Ok(())
}

fn print_stderr<SE: ShellExtensions>(
	context: &ExecutionContext<'_, SE>,
	message: &str,
) -> Result<(), error::Error> {
	write_message(context.stderr(), message)?;
	Ok(())
}

fn print_error<SE: ShellExtensions>(
	context: &ExecutionContext<'_, SE>,
	message: &(impl fmt::Display + ?Sized),
) -> Result<(), error::Error> {
	let mut stderr = context.stderr();
	writeln!(stderr, "dyn: {message}")?;
	Ok(())
}

fn print_dispatch_error<SE: ShellExtensions>(
	context: &ExecutionContext<'_, SE>,
	message: &(impl fmt::Display + ?Sized),
) -> Result<(), error::Error> {
	let mut stderr = context.stderr();
	writeln!(stderr, "dyn: device dispatch failed: {message}")?;
	Ok(())
}

fn write_message(mut writer: impl io::Write, message: &str) -> io::Result<()> {
	writer.write_all(message.as_bytes())?;
	if !message.ends_with('\n') {
		writer.write_all(b"\n")?;
	}
	Ok(())
}
#[cfg(test)]
mod tests {
	use std::{
		fs,
		future::{self, Future},
		sync::Arc,
	};

	use async_stream::stream;
	use bytes::Bytes;
	use futures::Stream;
	use omp_core::{Str, sf};
	use omp_proto::{
		SCHEMA_REV,
		env::v1::{self, ExecRequest, OpenSessionRequest, OutputChannel, Script},
	};
	use omp_shell_engine::{builtins::ContentType, extensions::DefaultShellExtensions};
	use omp_tool::{
		Claims, Constraint, Effects, ErasedStream, Ev, IncomingParams, Part, Precedence,
		Presentation, PromptCaps, Registry, Rev, Tool, ToolSpec, ToolTerminal, schema,
	};
	use omp_tools::{
		device::{DeviceCatalog, DeviceInvokeRequest, DeviceInvoker},
		staging::{
			ProposalActivationError, ProposalDecision, ProposalError, StagedProposalAction,
			StagedProposalRegistry,
		},
	};
	use parking_lot::Mutex;
	use schemars::JsonSchema;
	use serde::{Deserialize, Serialize};
	use serde_json::{Value, json};
	use tempfile::TempDir;
	use url::Url;

	use super::{
		CatalogQuery, DynHost, HookEventId, HookGate, registration, render_external_verdict,
	};
	use crate::exec::{ExecEvent, ExecHost};

	#[derive(Clone, Debug, Deserialize, JsonSchema)]
	struct FixtureParams {
		message: Str,
		#[serde(default)]
		flag:    Option<Str>,
	}

	#[derive(Clone, Debug, Deserialize, Serialize)]
	struct FixturePayload {
		message: Str,
		flag:    Option<Str>,
	}

	struct FixtureTool {
		spec: ToolSpec,
		seen: Arc<Mutex<Vec<Value>>>,
	}

	impl FixtureTool {
		fn new(seen: Arc<Mutex<Vec<Value>>>) -> Self {
			Self::named(sf!("fixture"), seen)
		}

		fn named(name: Str, seen: Arc<Mutex<Vec<Value>>>) -> Self {
			Self {
				spec: ToolSpec {
					name,
					rev: Rev { family: Str::default(), n: 1 },
					description: sf!("Echoes schema-derived arguments."),
					schema: schema::<FixtureParams>(),
					constraint: Constraint::None,
					effects: Effects::empty(),
					projection_code: [0; 32],
				},
				seen,
			}
		}
	}

	impl Tool for FixtureTool {
		type Fault = Value;
		type Params = FixtureParams;
		type Payload = FixturePayload;
		type Update = Value;

		fn spec(&self) -> &ToolSpec {
			&self.spec
		}

		fn call<'c>(
			&'c self,
			mut incoming: IncomingParams<'c>,
		) -> impl Stream<Item = Ev<Value, FixturePayload, Value>> + Send + 'c {
			stream! {
				let params = match incoming.whole::<FixtureParams>().await {
					Ok(params) => params,
					Err(error) => panic!("fixture arguments must decode: {error:?}"),
				};
				self.seen.lock().push(json!({
					"message": params.message,
					"flag": params.flag,
				}));
				yield Ev::Done(ToolTerminal::Done {
					result: Ok(FixturePayload { message: params.message, flag: params.flag }),
					useless: false,
				});
			}
		}

		fn prompt(&self, view: Result<&FixturePayload, &Value>, _: &PromptCaps) -> Vec<Part> {
			vec![Part::Text {
				text: match view {
					Ok(payload) => {
						sf!("fixture:{}:{}", payload.message, payload.flag.as_deref().unwrap_or("<none>"))
					},
					Err(fault) => Str::from(fault.to_string()),
				},
			}]
		}
	}

	#[derive(Clone)]
	struct UnusedInvoker;

	impl DeviceInvoker for UnusedInvoker {
		fn invoke(
			&self,
			_request: DeviceInvokeRequest,
		) -> impl Future<Output = ErasedStream<'static>> + Send {
			let stream: ErasedStream<'static> = Box::pin(futures::stream::empty());
			future::ready(stream)
		}
	}

	struct RecordingAction(Arc<Mutex<Vec<ProposalDecision>>>);

	impl StagedProposalAction for RecordingAction {
		fn finalize(&mut self, decision: &ProposalDecision) -> Result<Value, ProposalError> {
			self.0.lock().push(decision.clone());
			Ok(json!({ "settled": true }))
		}
	}

	struct CommandOutput {
		stdout: String,
		stderr: String,
		status: i32,
	}

	async fn host_fixture(
		proposals: StagedProposalRegistry,
	) -> (ExecHost, Bytes, Arc<Mutex<Vec<Value>>>, TempDir, Arc<Registry>) {
		let seen = Arc::new(Mutex::new(Vec::new()));
		let mut registry = Registry::default();
		registry
			.register(FixtureTool::new(Arc::clone(&seen)), Presentation::Device, Claims {
				precedence: Precedence::DEFAULT,
				claimant:   sf!("test/fixture"),
				replaces:   None,
			})
			.expect("fixture registers");
		let catalog = DeviceCatalog::default();
		let registry = Arc::new(registry);
		catalog
			.install_registry(Arc::clone(&registry))
			.expect("catalog installs");
		let host = ExecHost::new();
		host.install_devices(Arc::new(DynHost::new(
			catalog,
			Arc::new(UnusedInvoker),
			proposals,
			Arc::new(HookGate::channel().0),
		)));
		let root = tempfile::tempdir().expect("temp root");
		let opened = host
			.open_session(OpenSessionRequest {
				cwd_uri: Url::from_directory_path(root.path())
					.expect("temp path is a file URL")
					.to_string(),
				shell_profile: Some(v1::ShellProfileInput {
					profile: String::from("brush"),
					wire_revision: SCHEMA_REV,
					..Default::default()
				}),
				..Default::default()
			})
			.await
			.expect("session opens");
		(host, opened.session, seen, root, registry)
	}

	async fn execute(host: &ExecHost, session: &Bytes, command: &str) -> CommandOutput {
		let (_, run) = host
			.exec(
				ExecRequest {
					session: session.clone(),
					source: Some(Script { text: command.to_owned(), ..Default::default() }),
					..Default::default()
				},
				None,
			)
			.await
			.expect("command starts");
		let mut stdout = Vec::new();
		let mut stderr = Vec::new();
		let status = loop {
			match run.next_event().await {
				Some(ExecEvent::Output(frame)) if frame.channel == OutputChannel::Stdout as i32 => {
					stdout.extend_from_slice(&frame.data);
				},
				Some(ExecEvent::Output(frame)) if frame.channel == OutputChannel::Stderr as i32 => {
					stderr.extend_from_slice(&frame.data);
				},
				Some(ExecEvent::Exit(exit)) => break exit.status.expect("exit status"),
				Some(ExecEvent::Started { .. } | ExecEvent::Output(_)) => {},
				None => panic!("command stream ended before exit"),
			}
		};
		CommandOutput {
			stdout: String::from_utf8(stdout).expect("stdout is UTF-8"),
			stderr: String::from_utf8(stderr).expect("stderr is UTF-8"),
			status: status.exit_code.expect("exit code"),
		}
	}

	#[tokio::test]
	async fn real_session_lists_documents_invokes_and_reports_usage_errors() {
		let (host, session, seen, root, _registry) =
			host_fixture(StagedProposalRegistry::new()).await;

		let catalog = execute(&host, &session, "dyn").await;
		assert_eq!(catalog.status, 0, "stderr: {}", catalog.stderr);
		assert!(
			catalog
				.stdout
				.contains("fixture — Echoes schema-derived arguments.")
		);

		let help = execute(&host, &session, "dyn fixture --help").await;
		assert_eq!(help.status, 0);
		assert!(help.stdout.contains("fixture @ test/fixture"));
		assert!(help.stdout.contains("Usage:"));
		assert!(help.stdout.contains("<message>"));

		fs::write(root.path().join("message.txt"), "from file").expect("fixture file");
		let invoked = execute(&host, &session, "dyn fixture @message.txt --flag value").await;
		assert_eq!(invoked.status, 0);
		assert_eq!(invoked.stdout, "fixture:from file:value\n");
		assert_eq!(seen.lock().as_slice(), &[json!({ "message": "from file", "flag": "value" })],);

		let unknown = execute(&host, &session, "dyn nope").await;
		assert_eq!(unknown.status, 2);
		assert!(unknown.stderr.contains("Nearest:"));

		let bad_flag = execute(&host, &session, "dyn fixture --bogus").await;
		assert_eq!(bad_flag.status, 2);
		assert!(bad_flag.stderr.contains("unknown flag"));
		assert!(bad_flag.stderr.contains("run `dyn fixture --help`"));
	}

	#[tokio::test]
	async fn real_session_resolves_and_rejects_the_latest_proposal() {
		let proposals = StagedProposalRegistry::new();
		proposals.install_activation_observer(Arc::new(|_| {
			Box::pin(async { Ok::<(), ProposalActivationError>(()) })
		}));
		let decisions = Arc::new(Mutex::new(Vec::new()));
		let (host, session, _, _root, _registry) = host_fixture(proposals.clone()).await;

		let first = proposals
			.stage(sf!("ast_edit"), sf!("one file changed"), RecordingAction(Arc::clone(&decisions)))
			.await
			.expect("first proposal stages");
		let missing = execute(&host, &session, "dyn resolve").await;
		assert_eq!(missing.status, 2, "stderr: {}", missing.stderr);
		assert!(missing.stderr.contains("a one-sentence reason is required"));
		let resolved = execute(&host, &session, "dyn resolve applied").await;
		assert_eq!(resolved.status, 0);
		assert_eq!(resolved.stdout, "{\"settled\":true}\n");
		assert!(!proposals.is_pending(first.id.as_str()));

		let second = proposals
			.stage(sf!("edit"), sf!("one file changed"), RecordingAction(Arc::clone(&decisions)))
			.await
			.expect("second proposal stages");
		let rejected = execute(&host, &session, "dyn reject wrong").await;
		assert_eq!(rejected.status, 0);
		assert!(!proposals.is_pending(second.id.as_str()));
		assert!(matches!(decisions.lock().as_slice(), [
			ProposalDecision::Resolve { .. },
			ProposalDecision::Reject(_)
		]));
	}

	#[tokio::test]
	async fn device_list_intersection_narrows_catalog_without_changing_slot_hash() {
		let seen = Arc::new(Mutex::new(Vec::new()));
		let mut registry = Registry::new();
		let claims = |claimant: &'static str| Claims {
			precedence: Precedence::DEFAULT,
			claimant:   Str::new_static(claimant),
			replaces:   None,
		};
		registry
			.register(
				FixtureTool::named(sf!("fixture"), Arc::clone(&seen)),
				Presentation::Device,
				claims("fixture/owner"),
			)
			.expect("fixture device");
		registry
			.register(
				FixtureTool::named(sf!("hidden"), Arc::clone(&seen)),
				Presentation::Device,
				claims("hidden/owner"),
			)
			.expect("hidden device");
		registry
			.register(FixtureTool::named(sf!("slot"), seen), Presentation::Slot, Claims {
				precedence: Precedence::CORE,
				claimant:   sf!("omp/core"),
				replaces:   None,
			})
			.expect("slot");
		let slot_hash = registry.slot_hash();
		let registry = Arc::new(registry);
		let catalog = DeviceCatalog::default();
		catalog
			.install_registry(Arc::clone(&registry))
			.expect("catalog");
		let (gate, dispatches) = HookGate::channel();
		gate
			.subscribe("test", [omp_agent::Subscription {
				host:       sf!("test"),
				source:     omp_agent::SourceRef {
					layer:        0,
					publisher:    sf!("test"),
					extension_id: sf!("device-filter"),
				},
				id:         30,
				event:      HookEventId::HookEventDeviceList,
				phase:      omp_agent::HookPhase::Transform,
				order:      0,
				on_failure: omp_agent::OnFailure::Deny,
				when:       omp_agent::When::default(),
			}])
			.expect("device subscription");
		let gate = Arc::new(gate);
		let responder_gate = Arc::clone(&gate);
		let responder = tokio::spawn(async move {
			let dispatch = dispatches.recv_async().await.expect("device dispatch");
			assert_eq!(dispatch.event, HookEventId::HookEventDeviceList);
			let separator = dispatch
				.payload
				.iter()
				.position(|byte| *byte == b'\n')
				.expect("payload separator");
			let mut payload: Value =
				serde_json::from_slice(&dispatch.payload[separator + 1..]).expect("payload");
			payload["devices"]
				.as_array_mut()
				.expect("devices")
				.retain(|device| device["name"] == "fixture");
			responder_gate
				.answer(dispatch.dispatch_id, vec![(
					30,
					omp_agent::GateDecision::Modify(omp_agent::HookPatch {
						target: None,
						args:   Some(Bytes::from(
							serde_json::to_vec(&payload).expect("effective payload"),
						)),
					}),
				)])
				.expect("device decision");
		});
		let host =
			DynHost::new(catalog, Arc::new(UnusedInvoker), StagedProposalRegistry::new(), gate);
		let rendered = host
			.catalog(&registry, &CatalogQuery::default(), None)
			.await
			.expect("filtered catalog");
		responder.await.expect("device responder");
		assert!(rendered.contains("fixture"));
		assert!(!rendered.contains("hidden"));
		assert_eq!(registry.slot_hash(), slot_hash);
	}

	#[test]
	fn external_verdict_projection_preserves_structured_values() {
		let mut rendered = String::new();
		render_external_verdict(br#"{"kind":"ok","value":"placed"}"#, &mut rendered);
		assert_eq!(rendered, "placed");
		rendered.clear();
		render_external_verdict(br#"{"kind":"ok","value":{"placed":true}}"#, &mut rendered);
		assert_eq!(rendered, r#"{"placed":true}"#);
	}

	#[test]
	fn registration_help_is_static() {
		let host = Arc::new(DynHost::new(
			DeviceCatalog::default(),
			Arc::new(UnusedInvoker),
			StagedProposalRegistry::new(),
			Arc::new(HookGate::channel().0),
		));
		let registration = registration::<DefaultShellExtensions>(host);
		let help = (registration.content_func)("dyn", ContentType::DetailedHelp, &Default::default())
			.expect("help renders");
		assert!(help.contains("dyn <device> --help"));
	}
}
