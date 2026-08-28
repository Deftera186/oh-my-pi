#![recursion_limit = "256"]

//! OMP command-line entry point.

use std::{env, panic, process::ExitCode};

#[cfg(windows)]
use windows_sys::Win32::System::Console;

fn process_bootstrap() {
	// Safety: this runs as the first statement in `main`, before OMP starts any
	// daemon, worker, or application thread that could concurrently read env.
	unsafe {
		env::remove_var("MallocStackLogging");
		env::remove_var("MallocStackLoggingNoCompact");
	}
	set_process_title();
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn set_process_title() {
	// Safety: `c\"omp\"` is static, NUL terminated, and valid for setprogname.
	unsafe { libc::setprogname(c"omp".as_ptr()) };
}

#[cfg(target_os = "linux")]
fn set_process_title() {
	// Safety: PR_SET_NAME reads at most sixteen bytes from this static C string.
	unsafe {
		libc::prctl(libc::PR_SET_NAME, c"omp".as_ptr());
	}
}

#[cfg(windows)]
fn set_process_title() {
	let title = "omp\0".encode_utf16().collect::<Vec<_>>();
	// Safety: `title` is NUL terminated and remains alive for the call.
	unsafe {
		Console::SetConsoleTitleW(title.as_ptr());
	}
}

#[cfg(not(any(
	target_os = "macos",
	target_os = "ios",
	target_os = "freebsd",
	target_os = "linux",
	windows
)))]
fn set_process_title() {}

fn install_panic_hook() {
	panic::set_hook(Box::new(|info| {
		tracing::error!(target: "omp", panic = %info, "panic");
		eprintln!("\x1b[31momp internal error:\x1b[0m {info}");
	}));
}

#[tokio::main]
async fn main() -> ExitCode {
	process_bootstrap();
	omp_observability::logging::init();
	install_panic_hook();
	if env::args_os()
		.nth(1)
		.is_some_and(|arg| arg == omp_sandbox::HIDDEN_CHILD_ARG)
	{
		return match omp_sandbox::run_child_entry() {
			Ok(()) => ExitCode::SUCCESS,
			Err(error) => {
				eprintln!("omp sandbox child: {error}");
				ExitCode::FAILURE
			},
		};
	}
	if env::args_os()
		.nth(1)
		.is_some_and(|arg| arg == omp_envd::EVAL_CHILD_ARG)
	{
		return match omp_envd::run_eval_child_entry().await {
			Ok(()) => ExitCode::SUCCESS,
			Err(error) => {
				eprintln!("omp eval child: {error}");
				ExitCode::FAILURE
			},
		};
	}
	if env::args_os()
		.nth(1)
		.is_some_and(|arg| arg == omp_envd::exthost::EXT_HOST_ARG)
	{
		return match omp_envd::exthost::run_ext_host_entry() {
			Ok(()) => ExitCode::SUCCESS,
			Err(error) => {
				eprintln!("omp extension host: {error}");
				ExitCode::FAILURE
			},
		};
	}
	if env::args_os()
		.nth(1)
		.is_some_and(|arg| arg == omp_envd::worker::WORKER_ARG)
	{
		return match omp_envd::worker::run_py_worker_entry() {
			Ok(()) => ExitCode::SUCCESS,
			Err(error) => {
				eprintln!("omp Python worker: {error}");
				ExitCode::FAILURE
			},
		};
	}
	omp_observability::export::init();
	omp_app::startup_notice::start_watchdog();
	let result = omp_app::run().await;
	omp_app::startup_notice::stop_watchdog();
	omp_observability::export::shutdown();
	match result {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			// Usage diagnostics are intentionally stack-free and follow the
			// conventional exit status 2; other execution failures remain 1.
			eprintln!("{error:?}");
			if error
				.downcast_ref::<omp_app::usage_error::CliUsageError>()
				.is_some()
			{
				ExitCode::from(2)
			} else {
				ExitCode::FAILURE
			}
		},
	}
}
