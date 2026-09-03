#![recursion_limit = "256"]

//! Production application CLI, TUI, and command dispatch.

pub mod acp_mode;
pub mod audio_coordinator;
/// Push-to-talk capture and recognition for `omp chat`.
pub mod chat_voice;
pub mod auth_broker_cmd;
pub mod auth_cli;
pub mod auth_gateway_cmd;
pub mod bench_cmd;
pub mod browser_relay_cmd;
pub mod chat_cmd;
/// Session-owning controller behind `omp chat`.
#[cfg(any(unix, windows))]
pub(crate) mod chat_control;
/// Application feeds behind the chat host's dashboards and account commands.
pub mod chat_services;
pub mod cleanse_cmd;
pub mod cli;
pub mod commit_cmd;
pub mod complete_cmd;
pub mod completions;
pub mod compress_cmd;
pub mod config_cmd;
pub mod cursor_bridge;
pub mod daemon;
pub mod debug;
pub mod debug_logs;
pub mod diagnostics;
pub mod dry_balance_cmd;
pub mod endpoint;
pub mod ext_cli;
pub mod gallery_cmd;
pub mod gateway_rpc;
pub mod gc_cmd;
pub mod git_cmd;
pub mod grep_cmd;
pub mod grievances_cmd;
#[cfg(feature = "gui")]
mod gui;
pub mod help_extra;
pub mod image_attachment;
pub mod images_cmd;
pub mod keybindings;
pub mod models_cmd;
pub(crate) mod pickers;
pub mod print_mode;
pub mod profile_alias;
pub mod progress_reporter;
pub mod ps_cmd;
pub mod render_cmd;
pub mod rpc_mode;
#[cfg(feature = "local-tts")]
pub mod say_cmd;
/// Feature-disabled local speech command.
#[cfg(not(feature = "local-tts"))]
pub mod say_cmd {
	use crate::cli::SayArgs;

	/// Reports that local speech synthesis was excluded from this build.
	pub async fn run(_args: SayArgs) -> miette::Result<()> {
		Err(miette::miette!("local speech synthesis is not built; rerun with `--features local-tts`"))
	}
}
pub mod session_import;
pub mod setup_cmd;
pub mod shell_cmd;
pub mod smoke_test;
pub mod spec;
pub mod ssh_cmd;
pub mod standalone_tool_cmd;
pub mod startup_notice;
pub mod theme_watcher;
pub mod tiny_models_cmd;
pub mod token_cmd;
pub mod tool_installer;
pub mod update_cmd;
pub mod usage_cmd;
pub mod usage_error;
pub mod voice;
pub mod welcome_facts;
pub mod worktree_cmd;

use std::{
	fs,
	path::{Path, PathBuf},
};

pub use miette::{IntoDiagnostic, Report, Result};

/// Returns the archived command-stream configuration path: `<config
/// dir>/config.cfg` (`~/.o2/config.cfg` by default, `OMP_CONFIG_DIR`
/// overrides).
///
/// # Errors
///
/// [`omp_core::dirs::DataDirError::HomeUnset`] when no home directory is set.
pub fn config_path() -> std::result::Result<PathBuf, omp_core::dirs::DataDirError> {
	let home = omp_core::dirs::home_dir().ok_or(omp_core::dirs::DataDirError::HomeUnset)?;
	Ok(omp_core::dirs::config_dir(&home).join("config.cfg"))
}

/// Builds the process control context from user and exact-project cfg files.
///
/// The default bind cfg ([`keybindings::DEFAULT_BINDS`]) executes first, then
/// user configuration, then `<project>/.omp/config.cfg` overlays it.
pub fn process_ctx(project_root: &Path) -> Result<omp_con::Ctx> {
	process_ctx_with(project_root, omp_con::Ctx::builder())
}

/// [`process_ctx`] over a caller-prepared builder (reply sink, user objects).
pub fn process_ctx_with(project_root: &Path, builder: omp_con::CtxBuilder) -> Result<omp_con::Ctx> {
	let user = config_path().into_diagnostic()?;
	let project = project_root.join(".omp/config.cfg");
	let mut script = String::new();
	for path in [&user, &project] {
		if path.is_file() {
			if !script.is_empty() {
				script.push('\n');
			}
			script.push_str(&keybindings::migrate_generated_preamble(
				&fs::read_to_string(path).into_diagnostic()?,
			));
		}
	}
	let ctx = builder.build();
	ctx.exec(
		keybindings::DEFAULT_BINDS,
		omp_con::Source::Config(omp_core::Str::new_static(keybindings::DEFAULT_BINDS_NAME)),
	)
	.into_diagnostic()?;
	ctx.seal_bind_defaults();
	let outcome = ctx.exec_configs(
		&|name: &str| {
			(name == "config.cfg" && !script.is_empty()).then(|| omp_core::Str::new(script.as_str()))
		},
		None,
	);
	if outcome.failed > 0 {
		tracing::warn!(
			failed = outcome.failed,
			ran = outcome.ran,
			"config.cfg contained statements this build does not understand; they were skipped"
		);
	}
	Ok(ctx)
}

/// Parses process arguments and runs the selected production operation.
pub async fn run() -> Result<()> {
	cli::run().await
}
