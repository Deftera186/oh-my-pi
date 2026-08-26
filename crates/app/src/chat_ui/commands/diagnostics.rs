//! Project cleansing and local diagnostic command routes.

use std::{fmt::Write as _, future::Future, path::Path, pin::Pin, sync::Arc};

use miette::IntoDiagnostic as _;
use omp_core::{Str, sf};
use omp_driver::cleanse::{
	Checker, CleanseArgs, CleanseStatus, TargetChoice,
	production::{CleansePresentation, PresentationError, ProductionCleanseHost},
};
use tokio_util::sync::CancellationToken;

use super::command;

command!(cleanse, 545, "cleanse", icon: Stethoscope, [], "Detect and fix project diagnostics with weighted parallel subagents", [Workspace, Execution, Owner], false, typed("[request] [--all] [--tests] [-n <agents>] [-m <model>]", ["--all", "--tests", "--agents", "--model"], parse_cleanse) => |host, args| host.cleanse(args));
command!(debug, 84, "debug", icon: Bug, [], "Open debug tools selector", [Session, Owner], false, optional("[raw-stream|logs|paths|system]") => |host, inspector| host.debug(inspector));

struct InSessionCleansePresentation;

impl CleansePresentation for InSessionCleansePresentation {
	fn pick_target<'a>(
		&'a self,
		_checkers: &'a [Checker],
		cancel: &'a CancellationToken,
	) -> Pin<Box<dyn Future<Output = Result<TargetChoice, PresentationError>> + 'a>> {
		Box::pin(async move {
			if cancel.is_cancelled() {
				Ok(TargetChoice::Cancel)
			} else {
				Ok(TargetChoice::All)
			}
		})
	}

	fn prompt_request<'a>(
		&'a self,
		_cancel: &'a CancellationToken,
	) -> Pin<Box<dyn Future<Output = Result<Option<Str>, PresentationError>> + 'a>> {
		Box::pin(async { Ok(None) })
	}
}

pub(crate) async fn run_cleanse(
	root: &Path,
	data_dir: &Path,
	args: CleanseArgs,
	cancel: &CancellationToken,
) -> miette::Result<Str> {
	let root = root.to_path_buf();
	let data_dir = data_dir.to_path_buf();
	let cancel = cancel.clone();
	tokio::task::spawn_blocking(move || {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.into_diagnostic()?;
		runtime.block_on(run_cleanse_local(&root, &data_dir, args, &cancel))
	})
	.await
	.into_diagnostic()?
}

async fn run_cleanse_local(
	root: &Path,
	data_dir: &Path,
	args: CleanseArgs,
	cancel: &CancellationToken,
) -> miette::Result<Str> {
	let host = ProductionCleanseHost::open(
		root.to_path_buf(),
		data_dir.to_path_buf(),
		Arc::new(InSessionCleansePresentation),
	)
	.into_diagnostic()?;
	let exit = omp_driver::cleanse::run(&args, &host, cancel)
		.await
		.into_diagnostic()?;
	let checks = exit.report.checks.len();
	let diagnostics = exit.report.diagnostics.len();
	Ok(match exit.status {
		CleanseStatus::Clean => {
			sf!("Cleanse completed: {checks} checker(s) ran and no diagnostics remain.")
		},
		CleanseStatus::Unresolved => sf!(
			"Cleanse completed with {} unresolved file group(s) ({} diagnostics{}).",
			exit.remainder.len(),
			diagnostics,
			if exit.omitted_files == 0 {
				String::new()
			} else {
				format!(", {} more file group(s) omitted", exit.omitted_files)
			},
		),
		CleanseStatus::Unsupported => {
			sf!("No supported cleanse checker was discovered for this workspace.")
		},
		CleanseStatus::Cancelled => sf!("Cleanse cancelled."),
	})
}

pub(crate) fn render_debug(
	inspector: Option<&str>,
	data_dir: &Path,
	workspace_root: &str,
	session_id: &str,
	journal: &Path,
) -> miette::Result<Str> {
	let project_dir = omp_env::project_state::directory(data_dir, Path::new(workspace_root))
		.map_err(|error| miette::miette!("could not resolve session paths: {error}"))?;
	let artifacts = journal
		.parent()
		.unwrap_or(project_dir.as_path())
		.join(session_id);
	let logs = data_dir.join("logs");
	match inspector.map(str::trim).filter(|value| !value.is_empty()) {
		None => Ok(sf!(
			"## Debug tools\n\n| Command | Purpose |\n|---|---|\n| `/debug raw-stream` | View this \
			 session's bounded, always-redacted provider capture |\n| `/debug logs` | Show the \
			 process log location |\n| `/debug paths` | Show session journal and artifact paths |\n| \
			 `/debug system` | Show redacted host and process facts |\n\n**Logs:** `{}`  \n**Session \
			 journal:** `{}`  \n**Session artifacts:** `{}`\n\nReport bundles, performance/memory \
			 capture, terminal protocol probes, transcript export, and artifact GC remain available \
			 only through omp's native debug surfaces; this slash-command inspector does not expose \
			 them.",
			logs.display(),
			journal.display(),
			artifacts.display(),
		)),
		Some("raw-stream") => render_raw_stream(session_id),
		Some("logs") => Ok(sf!(
			"## Debug logs\n\nProcess logs are stored under `{}`. The native debug log viewer reads \
			 `.log` files from this directory and one bounded child-directory level.",
			logs.display(),
		)),
		Some("paths" | "session") => Ok(sf!(
			"## Session debug paths\n\n- Journal: `{}`\n- Session artifacts: `{}`\n- Project state: \
			 `{}`\n- Process logs: `{}`",
			journal.display(),
			artifacts.display(),
			project_dir.display(),
			logs.display(),
		)),
		Some("system") => {
			let facts = crate::debug::collect_system_facts();
			let json = serde_json::to_string_pretty(&facts)
				.map_err(|error| miette::miette!("could not render system facts: {error}"))?;
			Ok(sf!("## System information\n\n```json\n{json}\n```"))
		},
		Some(other) => Err(miette::miette!(
			"unknown debug inspector `{other}`; expected raw-stream, logs, paths, or system"
		)),
	}
}

fn render_raw_stream(session_id: &str) -> miette::Result<Str> {
	let snapshot = omp_inference::transport::global_provider_capture().snapshot(Some(session_id));
	let mut text = format!(
		"## Raw provider stream\n\nSession `{session_id}` has {} retained frame(s). The process \
		 ring currently retains {} frame(s), with {} eviction(s) and {} subscriber drop(s). \
		 Payloads are irreversibly redacted before capture.\n",
		snapshot.frames.len(),
		snapshot.summary.retained,
		snapshot.summary.evicted,
		snapshot.summary.subscriber_drops,
	);
	for frame in &snapshot.frames {
		let _ = write!(
			text,
			"\n### #{} `{}`\n\n```text\n{}\n```\n",
			frame.sequence,
			frame.event,
			frame.payload.replace("```", "` ` `"),
		);
	}
	Ok(Str::from(text))
}

fn parse_cleanse(args: &str) -> miette::Result<CleanseArgs> {
	let mut parsed = CleanseArgs::default();
	let mut request = Vec::new();
	let mut words = args.split_whitespace();
	while let Some(word) = words.next() {
		match word {
			"--all" | "-a" => parsed.all = true,
			"--tests" | "-t" => parsed.tests = true,
			"--agents" | "-n" => {
				let value = words.next().ok_or_else(cleanse_usage)?;
				parsed.agents = value
					.parse()
					.ok()
					.filter(|value| *value > 0)
					.ok_or_else(cleanse_usage)?;
			},
			"--model" | "-m" => {
				parsed.model = Str::new(words.next().ok_or_else(cleanse_usage)?);
			},
			flag if flag.starts_with('-') => return Err(cleanse_usage()),
			part => request.push(part),
		}
	}
	if !request.is_empty() {
		parsed.request = Some(Str::new(request.join(" ")));
	}
	Ok(parsed)
}

fn cleanse_usage() -> miette::Report {
	miette::miette!("usage: /cleanse [request] [--all] [--tests] [-n <agents>] [-m <model>]")
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn diagnostic_commands_are_registered_at_stable_orders() {
		let mut declarations = inventory::iter::<super::super::registry::BuiltinRegistration>
			.into_iter()
			.map(|registration| (registration.declaration)())
			.filter(|declaration| {
				matches!(declaration.name.as_str(), "cleanse" | "debug" | "extended-context")
			})
			.map(|declaration| (declaration.name, declaration.order))
			.collect::<Vec<_>>();
		declarations.sort_unstable_by_key(|(_, order)| *order);
		assert_eq!(declarations, vec![
			(Str::new("debug"), 84),
			(Str::new("extended-context"), 215),
			(Str::new("cleanse"), 545),
		],);
	}

	#[test]
	fn cleanse_parser_preserves_cli_options_and_request() {
		let parsed =
			parse_cleanse("fix type errors --tests -n 3 -m @slow").expect("valid cleanse arguments");
		assert_eq!(parsed.request.as_deref(), Some("fix type errors"));
		assert!(parsed.tests);
		assert_eq!(parsed.agents, 3);
		assert_eq!(parsed.model.as_str(), "@slow");
	}
	#[test]
	fn debug_menu_and_raw_stream_expose_live_diagnostics() {
		let root = tempfile::tempdir().expect("temporary workspace");
		let journal = root.path().join("session.jsonl");
		let menu = render_debug(
			None,
			root.path(),
			root.path().to_str().expect("UTF-8 path"),
			"session",
			&journal,
		)
		.expect("debug menu renders");
		assert!(menu.contains("/debug raw-stream"));
		assert!(menu.contains("Session journal"));

		let session = "debug-command-test-session";
		omp_inference::transport::global_provider_capture().capture(
			Some(session),
			"test-frame",
			"bounded payload",
		);
		let raw = render_debug(
			Some("raw-stream"),
			root.path(),
			root.path().to_str().expect("UTF-8 path"),
			session,
			&journal,
		)
		.expect("raw stream renders");
		assert!(raw.contains("test-frame"));
		assert!(raw.contains("irreversibly redacted"));
	}
}
