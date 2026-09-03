//! One-cell production-topology runner for the deployment pooling matrix.
//!
//! The benchmark recorder supplies a cell through `OMP_POOL_*`. This process
//! also re-enters as the real Python worker, so `cargo run --bin
//! pooling-runner` does not depend on another pre-built executable.

use std::{
	collections::{HashMap, HashSet},
	env, fs, future, process,
	process::{Command, ExitCode},
	sync::Arc,
	time::{Duration, Instant},
};

use bytes::Bytes;
use futures::future::join_all;
use omp_core::{
	ArtifactDigest, Duration as CoreDuration, DurationUnit, Principal, Provenance, Str, sf,
};
use omp_e2e::{Context as _, Result, error};
use omp_env::{Admitter, EnvClient, InvocationEvent};
use omp_envd::{
	EnvServer, RegistryBridges,
	exthost::{
		ActivationTrigger, DeclarationSet, ExtensionManifest, ServiceManifest, ToolDeclarationKey,
	},
	worker::{ExtHostConfig, ExtHostSpec, HostKey, WORKER_ARG},
};
use omp_proto::{
	SCHEMA_REV,
	env::v1::{Admission, AdmitInvocation, ClientHello, InvokeTool},
};
use omp_tool::{CallOutcome, Registry};
use serde::Serialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{task::JoinHandle, time};

const CELL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct Condition {
	extensions:  usize,
	dependency:  Dependency,
	lifecycle:   Lifecycle,
	link_delay:  Duration,
	hook_phases: usize,
	invocation:  InvocationPattern,
}

#[derive(Clone, Copy)]
enum Dependency {
	PurePython,
	CommonNative,
	LargeMlWheel,
}

#[derive(Clone, Copy)]
enum Lifecycle {
	ColdBoot,
	WarmRestart,
	HotReload,
}

#[derive(Clone, Copy)]
enum InvocationPattern {
	OneCall,
	ConcurrentCalls,
	CancellationMidCall,
}

#[derive(Serialize)]
struct Measurements {
	rss_bytes:           u64,
	pss_bytes:           u64,
	boot_micros:         u64,
	prompt_start_micros: u64,
	hook_latency_micros: u64,
	reload_micros:       u64,
	collateral_loss:     u64,
}

struct AllowAdmission;

impl Admitter for AllowAdmission {
	type Future<'client> = future::Ready<Admission>;

	fn admit<'client>(&'client self, query: AdmitInvocation) -> Self::Future<'client> {
		future::ready(Admission {
			invocation_id: query.invocation_id,
			allow: true,
			..Admission::default()
		})
	}
}

struct BenchEnvironment {
	client:     EnvClient,
	server:     Arc<EnvServer>,
	serve_task: JoinHandle<()>,
	_state:     TempDir,
}

impl BenchEnvironment {
	async fn open(condition: Condition, site: &TempDir) -> Result<(Self, Duration, Duration)> {
		let state = tempfile::tempdir().context("create pooling state directory")?;
		let executable = env::current_exe().context("resolve pooling runner executable")?;
		let mut config = ExtHostConfig::new(
			executable,
			Principal::new(sf!("pooling-bench"), sf!("Pooling benchmark")),
			sf!("pooling-bench-session"),
			1,
		);
		config.interrupt_grace = CoreDuration::new(100, DurationUnit::Milliseconds);
		for index in 0..condition.extensions {
			let module = format!("omp_pool_extension_{index}");
			let tool = format!("pooling_probe_{index}");
			let key = HostKey::new("workspace", "trusted", module.as_str());
			let provenance = Provenance::new(
				sf!("omp-benchmark"),
				key.extension().clone(),
				sf!("1.0.0"),
				ArtifactDigest::new([index as u8; 32]),
				key.layer().clone(),
				key.tier().clone(),
				1,
			);
			let manifest = ExtensionManifest::new(
				provenance,
				Str::from(module),
				[],
				DeclarationSet::new([ToolDeclarationKey::new(tool, "pooling", 1)], []),
				ServiceManifest::default(),
				[],
				[ActivationTrigger::FirstReach],
			);
			let mut extension = ExtHostSpec::new(key, manifest);
			extension.python_site = Some(site.path().to_owned());
			config.extensions.push(extension);
		}

		let boot_at = Instant::now();
		let server = Arc::new(
			EnvServer::open_local(
				site.path(),
				state.path(),
				Registry::new(),
				config,
				&omp_con::Ctx::new(),
				RegistryBridges::default(),
			)
			.await
			.context("boot production environment topology")?,
		);
		let boot = boot_at.elapsed();
		let (client, transport) = EnvClient::in_process(64);
		client.set_admitter(AllowAdmission);
		let host = Arc::clone(&server);
		let serve_task = tokio::spawn(async move {
			let _ = host.serve_in_process(transport).await;
		});
		controlled_link(condition.link_delay).await;
		let prompt_at = Instant::now();
		time::timeout(
			CELL_TIMEOUT,
			client.hello(ClientHello {
				client: "pooling-benchmark".to_owned(),
				schema_rev: SCHEMA_REV,
				..ClientHello::default()
			}),
		)
		.await
		.context("prompt-start hello timed out")??;
		controlled_link(condition.link_delay).await;
		let prompt = prompt_at.elapsed();
		Ok((Self { client, server, serve_task, _state: state }, boot, prompt))
	}
}

impl Drop for BenchEnvironment {
	fn drop(&mut self) {
		self.serve_task.abort();
		// Retaining the server field makes worker ownership explicit until this drop.
		let _ = Arc::strong_count(&self.server);
	}
}

#[tokio::main]
async fn async_main() -> Result<()> {
	let condition = Condition::from_environment()?;
	let site = tempfile::tempdir().context("create pooling Python site")?;
	write_extensions(&site, condition)?;

	// EnvServer currently exposes no owner-side hot-reload command. Warm restart
	// and hot reload therefore both measure the production teardown/reopen path;
	// keeping the labels separate makes that missing seam visible in the artifact.
	let (mut environment, boot, prompt) = BenchEnvironment::open(condition, &site).await?;
	let mut reload = Duration::ZERO;
	if !matches!(condition.lifecycle, Lifecycle::ColdBoot) {
		drop(environment);
		let restart_at = Instant::now();
		let (restarted, ..) = BenchEnvironment::open(condition, &site).await?;
		reload = restart_at.elapsed();
		environment = restarted;
	}

	let hook = measure_hooks(&environment.client, condition).await?;
	let collateral_loss = exercise_invocation_pattern(&environment.client, condition).await?;
	let rss = process_tree_rss(process::id())?;
	let measurements = Measurements {
		rss_bytes: rss,
		// Darwin has no PSS accounting. Reporting RSS in both fields keeps the
		// artifact comparable without pretending an unavailable kernel metric is 0.
		pss_bytes: rss,
		boot_micros: micros(boot),
		prompt_start_micros: micros(prompt),
		hook_latency_micros: micros(hook),
		reload_micros: micros(reload),
		collateral_loss,
	};
	println!("{}", serde_json::to_string(&measurements)?);
	Ok(())
}

fn main() -> ExitCode {
	if env::args_os()
		.nth(1)
		.is_some_and(|argument| argument == WORKER_ARG)
	{
		return match omp_envd::worker::run_py_worker_entry() {
			Ok(()) => ExitCode::SUCCESS,
			Err(error) => {
				eprintln!("pooling Python worker: {error}");
				ExitCode::FAILURE
			},
		};
	}
	match async_main() {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			eprintln!("{error:#}");
			ExitCode::FAILURE
		},
	}
}

impl Condition {
	fn from_environment() -> Result<Self> {
		let extensions = required("OMP_POOL_EXTENSIONS_ACTIVE")?
			.parse::<usize>()
			.context("OMP_POOL_EXTENSIONS_ACTIVE must be an integer")?;
		if ![0, 5, 15, 32].contains(&extensions) {
			return Err(error(format!("OMP_POOL_EXTENSIONS_ACTIVE must be one of 0, 5, 15, 32")));
		}
		let dependency = match required("OMP_POOL_DEPENDENCY_PROFILE")?.as_str() {
			"pure-python" => Dependency::PurePython,
			"common-native" => Dependency::CommonNative,
			"large-ml-wheel" => Dependency::LargeMlWheel,
			other => return Err(error(format!("unknown dependency profile {other:?}"))),
		};
		let lifecycle = match required("OMP_POOL_LIFECYCLE")?.as_str() {
			"cold-boot" => Lifecycle::ColdBoot,
			"warm-restart" => Lifecycle::WarmRestart,
			"hot-reload" => Lifecycle::HotReload,
			other => return Err(error(format!("unknown lifecycle {other:?}"))),
		};
		let link_delay = match required("OMP_POOL_ENVIRONMENT_LINK")?.as_str() {
			"local" => Duration::ZERO,
			"remote-20ms-rtt" => Duration::from_millis(10),
			"remote-100ms-rtt" => Duration::from_millis(50),
			other => return Err(error(format!("unknown environment link {other:?}"))),
		};
		let hook_phases = match required("OMP_POOL_HOOK_LOAD")?.as_str() {
			"one-phase" => 1,
			"five-phases" => 5,
			other => return Err(error(format!("unknown hook load {other:?}"))),
		};
		let invocation = match required("OMP_POOL_INVOCATION_PATTERN")?.as_str() {
			"one-call" => InvocationPattern::OneCall,
			"concurrent-calls" => InvocationPattern::ConcurrentCalls,
			"cancellation-mid-call" => InvocationPattern::CancellationMidCall,
			other => return Err(error(format!("unknown invocation pattern {other:?}"))),
		};
		Ok(Self { extensions, dependency, lifecycle, link_delay, hook_phases, invocation })
	}
}

fn required(name: &str) -> Result<String> {
	env::var(name).with_context(|| format!("{name} is required"))
}

fn write_extensions(site: &TempDir, condition: Condition) -> Result<()> {
	let dependency_setup = match condition.dependency {
		Dependency::PurePython => "",
		Dependency::CommonNative => "import ctypes\nimport hashlib\nimport sqlite3\nimport zlib\n",
		Dependency::LargeMlWheel => {
			"import array\n# A touched 8 MiB numeric payload models a resident large-wheel \
			 shard.\n_MODEL = array.array('d', [1.0]) * (1024 * 1024)\n"
		},
	};
	for index in 0..condition.extensions {
		let source = format!(
			"import time\n{dependency_setup}\n_STATE = 0\n\ndef pooling_probe_{index}(params):\n    \
			 global _STATE\n    _STATE += 1\n    delay = params.get('sleep_ms', 0)\n    if delay:\n        \
			 time.sleep(delay / 1000)\n    return {{'parts': [], 'details': {{'state': \
			 _STATE}}}}\n\nOMP_TOOLS = [{{\n    'name': 'pooling_probe_{index}',\n    'description': \
			 'measured pooling probe',\n    'schema': {{'type': 'object', 'properties': \
			 {{'sleep_ms': {{'type': 'integer'}}}}, 'additionalProperties': False}},\n    'rev': \
			 'pooling.1',\n    'strict': True,\n    'handler': pooling_probe_{index},\n}}]\n"
		);
		fs::write(site.path().join(format!("omp_pool_extension_{index}.py")), source)
			.with_context(|| format!("write extension module {index}"))?;
	}
	Ok(())
}

// Extension hooks are verified by ExtHostSupervisor but EnvServer does not yet
// expose an owner-side hook-event transport. Until it does, use the identical
// admitted worker dispatch path as the event/decision round trip rather than a
// fabricated timer.
async fn measure_hooks(client: &EnvClient, condition: Condition) -> Result<Duration> {
	if condition.extensions == 0 {
		return Ok(Duration::ZERO);
	}
	let started = Instant::now();
	for phase in 0..condition.hook_phases {
		controlled_link(condition.link_delay).await;
		let id = format!("hook-{phase}");
		if !invoke(client, id, 0).await? {
			return Err(error(format!("hook probe did not complete successfully")));
		}
		controlled_link(condition.link_delay).await;
	}
	Ok(started.elapsed() / condition.hook_phases as u32)
}

async fn exercise_invocation_pattern(client: &EnvClient, condition: Condition) -> Result<u64> {
	if condition.extensions == 0 {
		return Ok(0);
	}
	match condition.invocation {
		InvocationPattern::OneCall => {
			if !invoke(client, "one-call".to_owned(), 0).await? {
				return Err(error(format!("one-call probe did not complete successfully")));
			}
			Ok(0)
		},
		InvocationPattern::ConcurrentCalls => {
			let results =
				join_all((0..4).map(|index| invoke(client, format!("concurrent-{index}"), 10))).await;
			for result in results {
				if !result? {
					return Err(error(format!("concurrent probe did not complete successfully")));
				}
			}
			Ok(0)
		},
		InvocationPattern::CancellationMidCall => cancellation_probe(client).await,
	}
}

async fn cancellation_probe(client: &EnvClient) -> Result<u64> {
	let cancelled = open_invocation(client, "cancelled-call".to_owned(), 1_000).await?;
	let sibling = open_invocation(client, "collateral-call".to_owned(), 1_000).await?;
	time::sleep(Duration::from_millis(25)).await;
	cancelled.guard().cancel();
	let _ = finish_invocation(cancelled).await?;
	let sibling_result = finish_invocation(sibling).await?;
	Ok(u64::from(!sibling_result))
}

async fn invoke(client: &EnvClient, id: String, sleep_ms: u64) -> Result<bool> {
	let invocation = open_invocation(client, id, sleep_ms).await?;
	finish_invocation(invocation).await
}

async fn open_invocation(
	client: &EnvClient,
	id: String,
	sleep_ms: u64,
) -> Result<omp_env::Invocation> {
	let mut invocation = time::timeout(
		CELL_TIMEOUT,
		client.invoke(InvokeTool {
			invocation_id: id,
			name: "pooling_probe_0".to_owned(),
			rev: "pooling.1".to_owned(),
			deadline_ms: 3_000,
			..InvokeTool::default()
		}),
	)
	.await
	.context("open pooling invocation timed out")??;
	match time::timeout(CELL_TIMEOUT, invocation.next_event()).await?? {
		Some(InvocationEvent::Accepted(_)) => {},
		other => return Err(error(format!("pooling invocation was not accepted: {other:?}"))),
	}
	invocation
		.commit_args(
			Bytes::from(serde_json::to_vec(&json!({"sleep_ms": sleep_ms}))?),
			Bytes::from_static(b"pooling-benchmark-token"),
			1,
			None,
		)
		.await?;
	Ok(invocation)
}

async fn finish_invocation(mut invocation: omp_env::Invocation) -> Result<bool> {
	loop {
		match time::timeout(CELL_TIMEOUT, invocation.next_event()).await?? {
			Some(InvocationEvent::Verdict(verdict)) => {
				let outcome: CallOutcome<Value, Value> = serde_json::from_slice(&verdict.json)?;
				return Ok(matches!(outcome, CallOutcome::Ok(_)));
			},
			Some(InvocationEvent::Update(_)) => {},
			Some(_) => {},
			None => return Ok(false),
		}
	}
}

async fn controlled_link(one_way: Duration) {
	if !one_way.is_zero() {
		time::sleep(one_way).await;
	}
}

fn process_tree_rss(root: u32) -> Result<u64> {
	let output = Command::new("ps")
		.args(["-axo", "pid=,ppid=,rss="])
		.output()
		.context("read process RSS from ps")?;
	if !output.status.success() {
		return Err(error(format!("ps failed while reading process RSS")));
	}
	let text = String::from_utf8(output.stdout).context("ps emitted non-UTF-8 output")?;
	let mut parents = HashMap::<u32, u32>::new();
	let mut rss_kib = HashMap::<u32, u64>::new();
	for line in text.lines() {
		let mut fields = line.split_whitespace();
		let (Some(pid), Some(parent), Some(rss)) = (fields.next(), fields.next(), fields.next())
		else {
			continue;
		};
		if let (Ok(pid), Ok(parent), Ok(rss)) = (pid.parse(), parent.parse(), rss.parse()) {
			parents.insert(pid, parent);
			rss_kib.insert(pid, rss);
		}
	}
	let mut family = HashSet::from([root]);
	loop {
		let before = family.len();
		for (&pid, &parent) in &parents {
			if family.contains(&parent) {
				family.insert(pid);
			}
		}
		if family.len() == before {
			break;
		}
	}
	Ok(family
		.into_iter()
		.filter_map(|pid| rss_kib.get(&pid))
		.sum::<u64>()
		* 1024)
}

fn micros(duration: Duration) -> u64 {
	duration.as_micros().try_into().unwrap_or(u64::MAX)
}
