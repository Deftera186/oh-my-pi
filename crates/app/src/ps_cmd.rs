//! Native project-environment process inspection and control.

use std::{
	io::{self, Write as _},
	time::Duration,
};

use miette::{IntoDiagnostic as _, miette};
use omp_env::{EnvClient, ProcessAttachmentEvent};
use omp_proto::env::v1::{
	AttachOutput, ListProcesses, ProcessInfo, ProcessState, RestartProcess, SignalProcess,
	StopProcess,
};

use crate::{
	cli::{PsAction, PsArgs},
	standalone_tool_cmd,
};

/// Lists or controls Environment-supervised project processes.
pub(crate) async fn run(args: PsArgs) -> miette::Result<()> {
	if let Some(scope) = &args.global {
		return Err(miette!("global service scope `{scope}` is not owned by a project Environment"));
	}
	let session = standalone_tool_cmd::session_at(args.dir.clone()).await?;
	let env = session.env();
	let processes = env
		.list_processes(ListProcesses::default())
		.await
		.into_diagnostic()?
		.processes;
	if args.action == PsAction::List {
		if args.json {
			let values = processes.iter().map(process_json).collect::<Vec<_>>();
			serde_json::to_writer_pretty(io::stdout().lock(), &values).into_diagnostic()?;
			println!();
		} else if processes.is_empty() {
			println!("No supervised processes.");
		} else {
			for process in processes {
				print_process(&process);
			}
		}
		return Ok(());
	}
	let name = args
		.name
		.as_deref()
		.ok_or_else(|| miette!("{} requires a process name", args.action.as_str()))?;
	let process = processes
		.iter()
		.find(|process| process.name == name)
		.ok_or_else(|| miette!("unknown process `{name}`"))?;
	match args.action {
		PsAction::Info => {
			if args.json {
				serde_json::to_writer_pretty(io::stdout().lock(), &process_json(process))
					.into_diagnostic()?;
				println!();
			} else {
				print_process(process);
			}
		},
		PsAction::Logs => logs(env, process, &args).await?,
		PsAction::Stop => {
			env.stop_process(StopProcess {
				name:       name.into(),
				grace_ms:   args.timeout.unwrap_or(5).saturating_mul(1000),
				generation: process.generation,
				props:      None,
			})
			.await
			.into_diagnostic()?;
			println!("Stopped {name}");
		},
		PsAction::Kill => {
			env.signal_process(SignalProcess {
				name:       name.into(),
				signal:     "SIGKILL".into(),
				generation: process.generation,
				props:      None,
			})
			.await
			.into_diagnostic()?;
			println!("Killed {name}");
		},
		PsAction::Restart => {
			env.restart_process(RestartProcess {
				name:          name.into(),
				generation:    process.generation,
				wire_revision: omp_proto::SCHEMA_REV,
				props:         None,
			})
			.await
			.into_diagnostic()?;
			println!("Restarted {name}");
		},
		PsAction::List => unreachable!(),
	}
	Ok(())
}

fn process_json(process: &ProcessInfo) -> serde_json::Value {
	serde_json::json!({
		"name": process.name,
		"generation": process.generation,
		"state": process.state().as_str_name().to_ascii_lowercase(),
		"pid": process.identity.as_ref().map(|identity| identity.pid),
		"logStart": process.log_start_offset,
		"logEnd": process.log_end_offset,
		"restartCount": process.restart_count,
		"consecutiveFailures": process.consecutive_failures,
		"endpoint": process.endpoint,
	})
}

fn print_process(process: &ProcessInfo) {
	println!(
		"{}\t{}\tgeneration={}\trestarts={}",
		process.name,
		process.state().as_str_name(),
		process.generation,
		process.restart_count
	);
}

async fn logs(env: &EnvClient, process: &ProcessInfo, args: &PsArgs) -> miette::Result<()> {
	let mut attachment = env
		.attach_output(AttachOutput {
			name:             process.name.clone(),
			after_sequence:   0,
			generation:       process.generation,
			max_bytes:        1024 * 1024,
			terminal_text:    true,
			terminal_columns: 120,
			terminal_rows:    args.lines,
			props:            None,
		})
		.await
		.into_diagnostic()?;
	let filter = args
		.grep
		.as_deref()
		.map(regex::Regex::new)
		.transpose()
		.into_diagnostic()?;
	let mut saw_output = false;
	loop {
		let next = if args.follow {
			attachment.next_event().await.into_diagnostic()?
		} else {
			match tokio::time::timeout(
				Duration::from_millis(if saw_output { 50 } else { 1_000 }),
				attachment.next_event(),
			)
			.await
			{
				Ok(event) => event.into_diagnostic()?,
				Err(_) => break,
			}
		};
		match next {
			Some(ProcessAttachmentEvent::Output(output)) => {
				if let Some(filter) = &filter {
					let text = String::from_utf8_lossy(&output.data);
					for line in text.lines().filter(|line| filter.is_match(line)) {
						writeln!(io::stdout(), "{line}").into_diagnostic()?;
					}
				} else {
					io::stdout().write_all(&output.data).into_diagnostic()?;
				}
				saw_output = true;
			},
			Some(ProcessAttachmentEvent::State(state))
				if matches!(
					state.process.as_ref().map(|process| process.state()),
					Some(ProcessState::Exited | ProcessState::Stopped | ProcessState::Failed)
				) =>
			{
				break;
			},
			Some(_) => {},
			None => break,
		}
	}
	Ok(())
}
#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn process_json_keeps_generation_and_restart_state() {
		let process = ProcessInfo {
			name: "worker".into(),
			generation: 7,
			state: ProcessState::Running as i32,
			restart_count: 2,
			..ProcessInfo::default()
		};
		let value = process_json(&process);
		assert_eq!(value["name"], "worker");
		assert_eq!(value["generation"], 7);
		assert_eq!(value["restartCount"], 2);
	}
}
