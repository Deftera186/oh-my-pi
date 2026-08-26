//! Browser-relay command launcher.
//!
//! The relay and Chrome extension are shipped by the canonical Pi package;
//! this native entry point executes that exact implementation rather than
//! maintaining a second CDP-emulation protocol.

use std::process::Stdio;

use miette::{IntoDiagnostic as _, miette};
use tokio::process::Command;

use crate::cli::{BrowserRelayAction, BrowserRelayArgs};

/// Runs the canonical browser relay implementation and forwards its terminal.
pub(crate) async fn run(args: BrowserRelayArgs) -> miette::Result<()> {
	let argv = relay_arguments(&args);
	let package = "@oh-my-pi/pi-coding-agent";
	let status = Command::new("bunx")
		.arg("--bun")
		.arg(package)
		.args(argv)
		.stdin(Stdio::inherit())
		.stdout(Stdio::inherit())
		.stderr(Stdio::inherit())
		.status()
		.await
		.into_diagnostic()
		.map_err(|error| miette!("could not launch canonical browser relay with bunx: {error}"))?;
	if !status.success() {
		return Err(miette!("browser relay exited with {status}"));
	}
	Ok(())
}

fn relay_arguments(args: &BrowserRelayArgs) -> Vec<String> {
	let mut argv = vec!["browser-relay".into(), match args.action {
		BrowserRelayAction::Serve => "serve".into(),
		BrowserRelayAction::Install => "install".into(),
	}];
	if args.action == BrowserRelayAction::Serve {
		argv.extend(["--port".into(), args.port.to_string()]);
		if let Some(token) = &args.token {
			argv.extend(["--token".into(), token.to_string()]);
		}
		if args.no_group {
			argv.push("--no-group".into());
		}
		if args.verbose {
			argv.push("--verbose".into());
		}
	} else if let Some(dir) = &args.dir {
		argv.extend(["--dir".into(), dir.display().to_string()]);
	}
	argv
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn canonical_relay_flags_are_forwarded() {
		let args = BrowserRelayArgs {
			action:   BrowserRelayAction::Serve,
			port:     9333,
			token:    Some("secret".into()),
			dir:      None,
			no_group: true,
			verbose:  true,
		};
		assert_eq!(relay_arguments(&args), [
			"browser-relay",
			"serve",
			"--port",
			"9333",
			"--token",
			"secret",
			"--no-group",
			"--verbose"
		]);
	}
}
