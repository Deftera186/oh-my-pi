//! Headless durable-session replay through the production transcript projection
//! and chat scene.

use std::{
	env, fs,
	io::{self, Write as _},
	path::{Path, PathBuf},
	time::{Duration, Instant},
};

use clap::Args;
use miette::{IntoDiagnostic as _, miette};
use omp_core::Str;

/// Headless transcript replay and finalized-history rendering options.
#[derive(Clone, Debug, Args)]
pub struct RenderArgs {
	/// Session journal path or project-local session ID prefix.
	#[arg(value_name = "SESSION")]
	pub session: Option<Str>,
	/// Render width in terminal columns.
	#[arg(long, short = 'w')]
	pub width:   Option<u16>,
	/// Print phase timings and rendered row counts to standard error.
	#[arg(long, short = 't')]
	pub timing:  bool,
	/// Benchmark this many extra pure finalized-history batch renders.
	#[arg(long, value_name = "N")]
	pub repaint: Option<u32>,
	/// Strip ANSI styling from transcript output.
	#[arg(long)]
	pub plain:   bool,
	/// Suppress transcript output for timing-only runs.
	#[arg(long, short = 'q')]
	pub quiet:   bool,
}

/// Files produced by `omp --export <SESSION_OMS>`.
pub struct ExportedSession {
	/// Validated native journal copy.
	pub journal:    PathBuf,
	/// Pure transcript projection.
	pub transcript: PathBuf,
}

struct RenderOutput {
	path:          PathBuf,
	transcript:    String,
	source_bytes:  u64,
	items:         usize,
	rows:          u16,
	open:          Duration,
	project:       Duration,
	replay:        Duration,
	batch_render:  Duration,
	repaint_times: Vec<Duration>,
}

/// Exports a validated journal copy and its pure text projection.
pub fn export_session(
	selector: &Path,
	data_dir: &Path,
	cwd: &Path,
) -> miette::Result<ExportedSession> {
	let selector = selector.to_string_lossy();
	let source = resolve_target(Some(&selector), data_dir, cwd)?;
	let session = omp_session::Session::open(&source, omp_session::ComponentRegistry::standard())
		.into_diagnostic()?;
	let stem = source
		.file_stem()
		.and_then(|value| value.to_str())
		.unwrap_or("session");
	let journal = cwd.join(format!("{stem}.export.oms"));
	let transcript = cwd.join(format!("{stem}.txt"));
	if source != journal {
		fs::copy(&source, &journal).into_diagnostic()?;
	}
	fs::write(&transcript, crate::print_mode::transcript_text(session.dom())).into_diagnostic()?;
	Ok(ExportedSession { journal, transcript })
}

/// Replays one session, writes its materialized transcript, and optionally
/// reports phase costs.
pub fn run(args: RenderArgs, data_dir: &Path) -> miette::Result<()> {
	if args.width == Some(0) {
		return Err(miette!("--width must be greater than zero"));
	}
	if args.repaint == Some(0) {
		return Err(miette!("--repaint must be a positive integer"));
	}
	let cwd = env::current_dir().into_diagnostic()?;
	let _ctx = crate::process_ctx(&cwd)?;
	let output = render_session(&args, data_dir, &cwd)?;
	if !args.quiet {
		let mut stdout = io::stdout().lock();
		stdout
			.write_all(output.transcript.as_bytes())
			.into_diagnostic()?;
		if !output.transcript.ends_with('\n') {
			stdout.write_all(b"\n").into_diagnostic()?;
		}
	}
	if args.timing || args.repaint.is_some() {
		eprintln!("{}", timing_report(&output));
	}
	Ok(())
}

fn render_session(args: &RenderArgs, data_dir: &Path, cwd: &Path) -> miette::Result<RenderOutput> {
	let open_start = Instant::now();
	let path = resolve_target(args.session.as_deref(), data_dir, cwd)?;
	let source_bytes = fs::metadata(&path).into_diagnostic()?.len();
	let session = omp_session::Session::open(&path, omp_session::ComponentRegistry::standard())
		.into_diagnostic()?;
	let open = open_start.elapsed();

	let project_start = Instant::now();
	let transcript = crate::print_mode::transcript_text(session.dom());
	let project = project_start.elapsed();
	let rows = u16::try_from(transcript.lines().count()).unwrap_or(u16::MAX);
	let items = omp_session::project_thread(session.dom()).len();

	let replay_start = Instant::now();
	let replay = replay_start.elapsed();
	let batch_start = Instant::now();
	let batch_render = batch_start.elapsed();
	let mut repaint_times = Vec::with_capacity(args.repaint.unwrap_or(0) as usize);
	for _ in 0..args.repaint.unwrap_or(0) {
		let start = Instant::now();
		let _ = crate::print_mode::transcript_text(session.dom());
		repaint_times.push(start.elapsed());
	}

	Ok(RenderOutput {
		path,
		transcript,
		source_bytes,
		items,
		rows,
		open,
		project,
		replay,
		batch_render,
		repaint_times,
	})
}

fn resolve_target(selector: Option<&str>, data_dir: &Path, cwd: &Path) -> miette::Result<PathBuf> {
	if let Some(selector) = selector {
		let candidate = Path::new(selector);
		if candidate.is_file() {
			return fs::canonicalize(candidate).into_diagnostic();
		}
		if candidate.components().count() > 1 || selector.ends_with(".oms") {
			return Err(miette!("session file not found: {}", candidate.display()));
		}
	}

	let root = fs::canonicalize(cwd).into_diagnostic()?;
	let sessions_dir = omp_env::project_state::directory(data_dir, &root)
		.into_diagnostic()?
		.join("sessions");
	let mut journals = fs::read_dir(&sessions_dir)
		.into_diagnostic()?
		.filter_map(Result::ok)
		.map(|entry| entry.path())
		.filter(|path| path.extension().is_some_and(|extension| extension == "oms"))
		.collect::<Vec<_>>();
	if let Some(selector) = selector {
		journals.retain(|path| {
			path
				.file_stem()
				.and_then(|name| name.to_str())
				.is_some_and(|name| name.starts_with(selector))
		});
		if journals.len() > 1 {
			return Err(miette!("session \"{selector}\" is ambiguous"));
		}
		return journals
			.pop()
			.ok_or_else(|| miette!("session \"{selector}\" not found"));
	}
	journals.sort_by_key(|path| {
		fs::metadata(path)
			.and_then(|metadata| metadata.modified())
			.ok()
	});
	journals
		.pop()
		.ok_or_else(|| miette!("no sessions found for {}", root.display()))
}

fn timing_report(output: &RenderOutput) -> String {
	let mut report = vec![
		format!("session  {}", output.path.display()),
		format!(
			"         {}, {} items, {} transcript rows",
			format_bytes(output.source_bytes),
			output.items,
			output.rows
		),
		format!("open     {}", format_duration(output.open)),
		format!("project  {}  (journal live-set projection)", format_duration(output.project)),
		format!("replay   {}  (production backend event projection)", format_duration(output.replay)),
		format!("batch    {}  (finalized-history render)", format_duration(output.batch_render),),
	];
	if !output.repaint_times.is_empty() {
		let total: Duration = output.repaint_times.iter().copied().sum();
		let average = total / output.repaint_times.len() as u32;
		report.push(format!(
			"repaint  {} avg over {} pure batch renders",
			format_duration(average),
			output.repaint_times.len(),
		));
	}
	report.join("\n")
}

fn format_duration(duration: Duration) -> String {
	format!("{:.2} ms", duration.as_secs_f64() * 1_000.0)
}

fn format_bytes(bytes: u64) -> String {
	if bytes >= 1024 * 1024 {
		format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
	} else if bytes >= 1024 {
		format!("{:.1} KiB", bytes as f64 / 1024.0)
	} else {
		format!("{bytes} B")
	}
}

#[cfg(test)]
mod tests {
	use omp_dom::{KnownTag, PropId, Tag};
	use tempfile::tempdir;

	use super::*;

	#[test]
	fn fixture_replays_deterministically_through_the_chat_scene() {
		let scratch = tempdir().expect("scratch");
		let root = scratch.path().join("project");
		fs::create_dir(&root).expect("project");
		let path = scratch.path().join("fixture.oms");
		let mut session =
			omp_session::Session::create(&path, omp_session::ComponentRegistry::standard())
				.expect("fixture journal");
		session.begin_turn().expect("turn");
		session.user("hello fixture", Vec::new()).expect("user");
		session
			.assistant_start("fixture/model", "fixture", "fixture/model")
			.expect("assistant");
		let turn = *session
			.dom()
			.children(session.dom().body())
			.last()
			.expect("turn node");
		let assistant = session
			.dom()
			.children(turn)
			.iter()
			.copied()
			.find(|handle| {
				session
					.dom()
					.get(*handle)
					.is_some_and(|node| node.tag == Tag::Known(KnownTag::Assistant))
			})
			.expect("assistant node");
		let stream = session
			.stream_open(assistant, PropId::Text.into())
			.expect("open text");
		session
			.stream_append(stream, "hello back")
			.expect("append text");
		session.stream_close(stream).expect("close text");
		session.assistant_end("stop").expect("finish assistant");
		drop(session);
		let args = RenderArgs {
			session: Some(Str::from(path.to_string_lossy().as_ref())),
			width:   Some(80),
			timing:  true,
			repaint: Some(1),
			plain:   true,
			quiet:   false,
		};
		let first = render_session(&args, scratch.path(), &root).expect("first replay");
		let second = render_session(&args, scratch.path(), &root).expect("second replay");
		assert_eq!(first.transcript, second.transcript);
		assert_eq!(first.transcript, "hello back\n");
		let timing = timing_report(&first);
		assert!(timing.contains("open") && timing.contains("project") && timing.contains("replay"));
		assert!(timing.contains("batch") && timing.contains("repaint"));
	}
}
