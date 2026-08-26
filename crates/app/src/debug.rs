//! Native debug action catalog and diagnostic fact collectors.

use std::{
	env::{self, consts},
	fs, io,
	path::{Path, PathBuf},
	process, thread, time,
	time::Duration,
};

use omp_core::Str;
use omp_storage::{blob::BlobStore, gc};
use serde::Serialize;
use strum::{Display, EnumString, IntoStaticStr};
use url::Url;

/// Stable action identifier selected by the debug overlay.
#[derive(Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "kebab-case")]
pub enum DebugAction {
	/// Open the session artifact directory.
	OpenArtifacts,
	/// Create an immediate diagnostic bundle.
	ReportSession,
	/// Capture native CPU/work diagnostics before bundling.
	ReportPerformance,
	/// Create a memory-focused diagnostic bundle.
	ReportMemory,
	/// Inspect bounded process logs.
	ViewLogs,
	/// Inspect the sanitized provider stream ring.
	ViewRawStream,
	/// Show host operating-system and process facts.
	ViewSystem,
	/// Show negotiated terminal capabilities.
	ViewTerminal,
	/// Exercise supported terminal protocols.
	ProbeProtocols,
	/// Export the visible transcript to a temporary artifact.
	ExportTranscript,
	/// Run reachability-based artifact garbage collection.
	GarbageCollect,
}

/// Human-facing immutable action metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DebugActionInfo {
	/// Stable selector value.
	pub action:      DebugAction,
	/// Compact action label.
	pub label:       &'static str,
	/// One-line consequence description.
	pub description: &'static str,
}

/// Ordered action catalog used by native selectors and non-interactive clients.
pub const ACTIONS: &[DebugActionInfo] = &[
	DebugActionInfo {
		action:      DebugAction::OpenArtifacts,
		label:       "Open artifact folder",
		description: "Open session artifacts in the file manager",
	},
	DebugActionInfo {
		action:      DebugAction::ReportSession,
		label:       "Report session",
		description: "Create a bounded diagnostic bundle now",
	},
	DebugActionInfo {
		action:      DebugAction::ReportPerformance,
		label:       "Report performance issue",
		description: "Attach native CPU/work captures and bundle",
	},
	DebugActionInfo {
		action:      DebugAction::ReportMemory,
		label:       "Report memory issue",
		description: "Attach allocator summary and bundle",
	},
	DebugActionInfo {
		action:      DebugAction::ViewLogs,
		label:       "View recent logs",
		description: "Search bounded dated process logs",
	},
	DebugActionInfo {
		action:      DebugAction::ViewRawStream,
		label:       "View raw provider stream",
		description: "Inspect the always-redacted bounded capture",
	},
	DebugActionInfo {
		action:      DebugAction::ViewSystem,
		label:       "View system information",
		description: "OS, CPU, memory, version, shell, and cwd",
	},
	DebugActionInfo {
		action:      DebugAction::ViewTerminal,
		label:       "View terminal state",
		description: "Protocols, geometry, multiplexer, and scrollback",
	},
	DebugActionInfo {
		action:      DebugAction::ProbeProtocols,
		label:       "Test terminal protocols",
		description: "Styles, links, sizing, graphics, and notifications",
	},
	DebugActionInfo {
		action:      DebugAction::ExportTranscript,
		label:       "Export visible transcript",
		description: "Write the current TUI transcript to a temporary text artifact",
	},
	DebugActionInfo {
		action:      DebugAction::GarbageCollect,
		label:       "Garbage collect artifacts",
		description: "Sweep unreachable eligible blobs and report reclaimed bytes",
	},
];

/// Concrete app/UI surface launched by one catalog action.
#[derive(Clone, Debug)]
pub enum DebugTarget {
	/// Open the artifact directory through the app opener.
	ArtifactFolder,
	/// Build a diagnostic report with the selected native attachments.
	Report(ReportKind),
	/// Open the bounded dated-log overlay.
	Logs,
	/// Subscribe the raw-stream overlay to inference.
	RawStream,
	/// Render collected system facts.
	System(SystemFacts),
	/// Render negotiated terminal facts.
	Terminal(TerminalFacts),
	/// Open the active terminal protocol probe.
	ProtocolProbe,
	/// Export the currently visible transcript.
	TranscriptExport,
	/// Invoke reachability-based storage garbage collection.
	GarbageCollect,
}

/// Optional native attachments selected for a diagnostic report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportKind {
	/// Session, logs, settings, system, and environment only.
	Session,
	/// Native CPU/work samples, folded stacks, and SVG flamegraph.
	Performance,
	/// Native allocator summary when the platform runtime exposes one.
	Memory,
}

/// Resolves every stable catalog action to one concrete launcher target.
pub fn target(action: DebugAction, caps: omp_tui::TerminalCaps) -> DebugTarget {
	match action {
		DebugAction::OpenArtifacts => DebugTarget::ArtifactFolder,
		DebugAction::ReportSession => DebugTarget::Report(ReportKind::Session),
		DebugAction::ReportPerformance => DebugTarget::Report(ReportKind::Performance),
		DebugAction::ReportMemory => DebugTarget::Report(ReportKind::Memory),
		DebugAction::ViewLogs => DebugTarget::Logs,
		DebugAction::ViewRawStream => DebugTarget::RawStream,
		DebugAction::ViewSystem => DebugTarget::System(collect_system_facts()),
		DebugAction::ViewTerminal => DebugTarget::Terminal(collect_terminal_facts(caps)),
		DebugAction::ProbeProtocols => DebugTarget::ProtocolProbe,
		DebugAction::ExportTranscript => DebugTarget::TranscriptExport,
		DebugAction::GarbageCollect => DebugTarget::GarbageCollect,
	}
}

/// Sanitized host facts suitable for an overlay or diagnostic archive.
#[derive(Clone, Debug, Serialize)]
pub struct SystemFacts {
	/// Operating-system family.
	pub os:            &'static str,
	/// CPU architecture.
	pub architecture:  &'static str,
	/// Logical processors visible to the process.
	pub logical_cpus:  usize,
	/// Physical memory in bytes when cheaply available.
	pub memory_bytes:  Option<u64>,
	/// OMP package version.
	pub omp_version:   &'static str,
	/// Rust target family.
	pub target_family: &'static str,
	/// User shell executable after mandatory credential masking.
	pub shell:         String,
	/// Current working directory after mandatory credential masking.
	pub cwd:           String,
}

/// Collects bounded host facts without invoking platform debuggers.
pub fn collect_system_facts() -> SystemFacts {
	let shell = env::var("SHELL")
		.or_else(|_| env::var("COMSPEC"))
		.unwrap_or_default();
	let cwd = env::current_dir()
		.unwrap_or_else(|_| PathBuf::from("."))
		.display()
		.to_string();
	SystemFacts {
		os:            consts::OS,
		architecture:  consts::ARCH,
		logical_cpus:  thread::available_parallelism().map_or(1, usize::from),
		memory_bytes:  platform_memory_bytes(),
		omp_version:   env!("CARGO_PKG_VERSION"),
		target_family: consts::FAMILY,
		shell:         omp_telemetry::redact::redact_sensitive_credentials(&shell),
		cwd:           omp_telemetry::redact::redact_sensitive_credentials(&cwd),
	}
}

/// Negotiated terminal facts suitable for the terminal-state overlay.
#[derive(Clone, Debug, Serialize)]
pub struct TerminalFacts {
	/// Terminal emulator identity.
	pub terminal:            String,
	/// Character-set tier.
	pub charset:             String,
	/// Graphics protocol tier.
	pub graphics:            String,
	/// Cell pixel geometry if negotiated.
	pub cell_pixels:         Option<(u16, u16)>,
	/// Whether OSC 8 hyperlinks are supported.
	pub hyperlinks:          bool,
	/// Whether OSC 66 text sizing is supported.
	pub text_sizing:         bool,
	/// Notification protocol.
	pub notifications:       String,
	/// Whether execution is nested in a terminal multiplexer.
	pub multiplexer:         bool,
	/// Native scrollback strategy description.
	pub scrollback:          &'static str,
	/// Synchronized-output support.
	pub synchronized_output: bool,
	/// Kitty keyboard flags, when negotiated.
	pub kitty_keyboard:      Option<u8>,
}

/// Projects already-negotiated TUI capabilities without probing a second time.
pub fn collect_terminal_facts(caps: omp_tui::TerminalCaps) -> TerminalFacts {
	let scrollback = if caps.margin_scrollback {
		"margin scrollback"
	} else if caps.screen_to_scrollback {
		"screen transfer"
	} else {
		"viewport only"
	};
	TerminalFacts {
		terminal: format!("{:?}", caps.id),
		charset: format!("{:?}", caps.charset),
		graphics: format!("{:?}", caps.graphics),
		cell_pixels: caps.cell_px,
		hyperlinks: caps.hyperlinks,
		text_sizing: caps.text_sizing,
		notifications: format!("{:?}", caps.notify),
		multiplexer: caps.inside_multiplexer,
		scrollback,
		synchronized_output: caps.sync_output,
		kitty_keyboard: caps.kitty_keyboard,
	}
}

/// Writes a redacted visible transcript into an environment-created temporary
/// artifact.
pub fn export_transcript(directory: &Path, text: &str) -> io::Result<PathBuf> {
	fs::create_dir_all(directory)?;
	let nonce = time::SystemTime::now()
		.duration_since(time::UNIX_EPOCH)
		.unwrap_or(Duration::ZERO)
		.as_nanos();
	let path = directory.join(format!("omp-transcript-{}-{nonce}.txt", process::id()));
	let redacted = omp_telemetry::redact::redact_sensitive_credentials(text);
	fs::write(&path, redacted)?;
	Ok(path)
}

/// Converts a generated artifact path into a safe OSC 8/open-action URL.
pub fn artifact_url(path: &Path) -> Option<Url> {
	Url::from_file_path(path).ok()
}

/// Sweeps only blobs unreachable from the authoritative profile session roots.
pub fn garbage_collect(
	store: &BlobStore,
	min_age: Duration,
) -> Result<omp_storage::gc::SweepReport, gc::Error> {
	let roots = omp_storage::gc::SessionRoots::discover(store, &[])?;
	omp_storage::gc::sweep(store, &roots, min_age)
}

#[cfg(target_os = "linux")]
fn platform_memory_bytes() -> Option<u64> {
	let text = fs::read_to_string("/proc/meminfo").ok()?;
	let kb = text.lines().find_map(|line| {
		line
			.strip_prefix("MemTotal:")?
			.split_ascii_whitespace()
			.next()?
			.parse::<u64>()
			.ok()
	})?;
	kb.checked_mul(1024)
}
#[cfg(not(target_os = "linux"))]
const fn platform_memory_bytes() -> Option<u64> {
	None
}

/// Stable string form used by selector rows.
pub fn action_key(action: DebugAction) -> Str {
	let value: &'static str = action.into();
	Str::new_static(value)
}
/// Projects the app-owned catalog into the host-agnostic selector model.
pub fn selector_rows() -> Vec<omp_chat_ui::debug_selector::DebugActionRow> {
	ACTIONS
		.iter()
		.map(|info| omp_chat_ui::debug_selector::DebugActionRow {
			key:         action_key(info.action),
			label:       Str::new_static(info.label),
			description: Str::new_static(info.description),
		})
		.collect()
}
