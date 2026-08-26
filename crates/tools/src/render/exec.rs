//! Native shell and eval renderers with bounded streaming tails.

use std::fmt::Write as _;

use omp_core::Str;
use omp_tool::{CallOutcome, ToolIdentity, render::RenderFold};
use serde::Deserialize;

use super::{debug_label, fault_view, live_view, push_text};
use crate::{
	eval::{Fault as EvalFault, Payload as EvalPayload, Update as EvalUpdate},
	gallery::RendererGalleryFixture,
	shell::{
		ExecOutcome, Fault as ShellFault, Payload as ShellPayload, TranscriptFrame,
		Update as ShellUpdate,
	},
};

#[derive(Default)]
pub(super) struct StreamState {
	bytes:         u64,
	last_sequence: Option<u64>,
	tail:          Vec<u8>,
	cached:        Option<Str>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum ShellRenderOutcome {
	Call(CallOutcome<ShellPayload, ShellFault>),
	Terminal(omp_tool::ToolTerminal<ShellPayload, ShellFault>),
}

pub(super) struct ShellRenderer;

impl RenderFold for ShellRenderer {
	type Outcome = ShellRenderOutcome;
	type State = StreamState;
	type Update = ShellUpdate;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.bytes = state
			.bytes
			.saturating_add(u64::try_from(update.data.len()).unwrap_or(u64::MAX));
		state.last_sequence = Some(update.sequence);
		append_bounded_tail(&mut state.tail, update.data.as_ref());
		state.cached = Some(render_shell_live(state));
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(
				state
					.cached
					.clone()
					.unwrap_or_else(|| render_shell_live(state)),
			),
			Some(ShellRenderOutcome::Call(CallOutcome::Ok(payload)))
			| Some(ShellRenderOutcome::Terminal(omp_tool::ToolTerminal::Done {
				result: Ok(payload),
				..
			})) => Some(render_shell_payload(payload)),
			Some(ShellRenderOutcome::Call(CallOutcome::Faulted(fault)))
			| Some(ShellRenderOutcome::Terminal(omp_tool::ToolTerminal::Done {
				result: Err(fault),
				..
			})) => Some(fault_view("shell", &shell_fault(fault))),
			Some(ShellRenderOutcome::Terminal(omp_tool::ToolTerminal::Detached(job))) => {
				Some(render_shell_detached(job))
			},
			Some(ShellRenderOutcome::Call(
				CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. },
			)) => None,
		}
	}
}

pub(super) struct EvalRenderer;

impl RenderFold for EvalRenderer {
	type Outcome = CallOutcome<EvalPayload, EvalFault>;
	type State = StreamState;
	type Update = EvalUpdate;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.bytes = state
			.bytes
			.saturating_add(u64::try_from(update.data.len()).unwrap_or(u64::MAX));
		state.last_sequence = Some(update.sequence);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(stream_live_view("eval", state)),
			Some(CallOutcome::Ok(payload)) => Some(render_eval_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("eval", &eval_fault(fault))),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

fn stream_live_view(name: &str, state: &StreamState) -> Str {
	let status = if state.last_sequence.is_some() {
		format!("running · {} bytes", state.bytes)
	} else {
		String::from("running")
	};
	live_view(name, &status)
}

fn append_bounded_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
	const MAX_LIVE_OUTPUT_BYTES: usize = 16 * 1024;
	if chunk.len() >= MAX_LIVE_OUTPUT_BYTES {
		tail.clear();
		tail.extend_from_slice(&chunk[chunk.len() - MAX_LIVE_OUTPUT_BYTES..]);
		return;
	}
	let overflow = tail
		.len()
		.saturating_add(chunk.len())
		.saturating_sub(MAX_LIVE_OUTPUT_BYTES);
	if overflow > 0 {
		tail.drain(..overflow);
	}
	tail.extend_from_slice(chunk);
}

fn render_shell_live(state: &StreamState) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=accent><col gap=0><row gap=1><text bold \
		 fg=accent>$</text><text bold>shell</text>",
	);
	if state.last_sequence.is_some() {
		output.push_str("<spinner>running</spinner><text fg=muted>");
		write!(output, "{} bytes", state.bytes).expect("writing to String cannot fail");
		output.push_str("</text>");
	} else {
		output.push_str("<spinner>starting</spinner>");
	}
	output.push_str("</row>");
	if !state.tail.is_empty() {
		output.push_str("<pre fg=muted>");
		push_text(&mut output, &String::from_utf8_lossy(&state.tail));
		output.push_str("</pre><text fg=muted>streaming tail · ctrl+o to expand</text>");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn shell_fault(fault: &ShellFault) -> String {
	match fault {
		ShellFault::Resource { operation, message } => format!("{operation}: {message}"),
		ShellFault::PtyDenied => String::from("PTY allocation denied by invocation scope"),
		ShellFault::InvalidEnvironmentKey { key } => {
			format!("invalid shell environment key {key:?}")
		},
		ShellFault::AsyncNameRequired => String::from("async shell execution requires a name"),
	}
}

fn render_shell_detached(job: &omp_tool::JobRef) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>$</text><text bold>shell detached</text><spinner>running</spinner></row><row \
		 gap=1><text fg=muted>job</text><text bold>",
	);
	push_text(&mut output, &job.id);
	output.push_str("</text></row><text>");
	push_text(&mut output, &job.metadata.label);
	output.push_str("</text><text fg=muted>completion will be delivered by the job board</text>");
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_shell_payload(payload: &ShellPayload) -> Str {
	const PREVIEW_LINES: usize = 20;
	let retained = payload
		.transcript
		.iter()
		.map(|frame| frame.data.len())
		.sum::<usize>();
	let outcome = debug_label(payload.status.outcome);
	let color = match payload.status.outcome {
		ExecOutcome::Exited if payload.status.exit_code.unwrap_or_default() == 0 => "success",
		ExecOutcome::Timeout => "warning",
		ExecOutcome::Exited | ExecOutcome::Failed | ExecOutcome::Cancelled | ExecOutcome::Denied => {
			"error"
		},
	};
	let mut output = String::from("<box border=round pad=\"0 1\" bc=");
	output.push_str(color);
	output.push_str(
		"><col gap=0><row gap=1><text bold fg=accent>$</text><text bold>shell</text><text fg=",
	);
	output.push_str(color);
	output.push('>');
	push_text(&mut output, &outcome);
	output.push_str("</text>");
	if let Some(code) = payload.status.exit_code {
		output.push_str("<text fg=");
		output.push_str(color);
		output.push('>');
		write!(output, "exit {code}").expect("writing to String cannot fail");
		output.push_str("</text>");
	}
	if let Some(signal) = &payload.status.signal {
		output.push_str("<text fg=error>");
		push_text(&mut output, signal);
		output.push_str("</text>");
	}
	output.push_str("<text fg=muted>");
	write!(output, "{} ms · {retained} bytes", payload.status.wall_clock_ms)
		.expect("writing to String cannot fail");
	output.push_str("</text></row><pre fg=accent>");
	push_text(&mut output, "$ ");
	push_text(&mut output, &payload.command);
	output.push_str("</pre>");
	if let Some(cwd) = &payload.status.final_cwd_uri {
		output.push_str("<row gap=1><text fg=muted>cwd</text><text truncate>");
		push_text(&mut output, cwd);
		output.push_str("</text></row>");
	}
	let contains_sixel = payload.transcript.iter().any(|frame| {
		frame.data.as_ref().contains(&0x90)
			|| frame
				.data
				.as_ref()
				.windows(2)
				.any(|window| window == b"\x1bP")
	});
	let transcript = bounded_transcript_tail(&payload.transcript, contains_sixel);
	if !transcript.is_empty() {
		let text = String::from_utf8_lossy(&transcript);
		let lines = text.lines().collect::<Vec<_>>();
		let preview_start = lines.len().saturating_sub(PREVIEW_LINES);
		output.push_str("<pre fg=muted>");
		for (index, line) in lines[preview_start..].iter().enumerate() {
			if index > 0 {
				output.push('\n');
			}
			push_text(&mut output, line);
		}
		output.push_str("</pre>");
		if preview_start > 0 {
			write!(
				output,
				"<text fg=muted>{preview_start} earlier lines hidden · ctrl+o to expand</text>"
			)
			.expect("writing to String cannot fail");
		}
	}
	if payload.status.spilled_output.is_some() {
		output.push_str("<text fg=muted>full output stored as blob</text>");
	}
	if payload.status.effects_unknown {
		output.push_str("<text fg=warning>final effect state is unknown</text>");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn bounded_transcript_tail(transcript: &[TranscriptFrame], retain_all: bool) -> Vec<u8> {
	const MAX_RENDER_BYTES: usize = 64 * 1024;
	let total = transcript
		.iter()
		.map(|frame| frame.data.len())
		.sum::<usize>();
	let retain = if retain_all {
		total
	} else {
		total.min(MAX_RENDER_BYTES)
	};
	let skip = total.saturating_sub(retain);
	let mut output = Vec::with_capacity(retain);
	let mut offset = 0usize;
	for frame in transcript {
		let bytes = frame.data.as_ref();
		let frame_end = offset.saturating_add(bytes.len());
		if frame_end > skip {
			let start = skip.saturating_sub(offset);
			output.extend_from_slice(&bytes[start..]);
		}
		offset = frame_end;
	}
	output
}

fn eval_fault(fault: &EvalFault) -> String {
	match fault {
		EvalFault::InvalidTimeout => String::from("timeout must be non-negative and finite"),
		EvalFault::Resource { operation, message } => {
			format!("{operation}: {message}")
		},
		EvalFault::SessionLost { message } => message.to_string(),
	}
}

fn render_eval_payload(payload: &EvalPayload) -> Str {
	let mut status = debug_label(payload.status.outcome);
	if let Some(code) = payload.status.exit_code {
		write!(status, " · exit {code}").expect("writing to String cannot fail");
	}
	let retained = payload
		.frames
		.iter()
		.map(|frame| frame.data.len())
		.sum::<usize>();
	let mut output = String::from("<col gap=0><row gap=1><text bold>eval</text><text>");
	push_text(&mut output, &status);
	output.push_str("</text><text fg=muted>");
	write!(
		output,
		"{retained} retained bytes · {} total bytes · {} ms",
		payload.total_bytes, payload.status.duration_ms
	)
	.expect("writing to String cannot fail");
	output.push_str("</text></row>");
	if let Some(title) = &payload.title {
		output.push_str("<text bold>");
		push_text(&mut output, title);
		output.push_str("</text>");
	}
	if let Some(exception) = &payload.status.exception {
		output.push_str("<text fg=error>");
		push_text(&mut output, &exception.name);
		if !exception.message.is_empty() {
			output.push_str(": ");
			push_text(&mut output, &exception.message);
		}
		output.push_str("</text>");
	}
	if payload.truncated {
		output.push_str("<text fg=muted>output truncated</text>");
	}
	output.push_str("</col>");
	Str::new(output)
}

/// Native shell and eval renderer lifecycle fixtures for the visual QA gallery.
pub(crate) fn gallery_fixtures(
	shell: ToolIdentity,
	eval: ToolIdentity,
) -> Vec<RendererGalleryFixture> {
	vec![
	RendererGalleryFixture {
		identity: shell,
		title: "shell printf gallery",
		progress_update: Some(
		br#"{"channel":"stdout","data":[103,97,108,108,101,114,121,10],"sequence":1,"exec_id":[1],"started":true,"terminal":false}"#,
		),
		success_outcome: br#"{"kind":"ok","value":{"session_id":[1],"exec_id":[1],"command":"printf gallery","transcript":[{"channel":"stdout","data":[103,97,108,108,101,114,121,10],"sequence":1}],"adjustments":[],"status":{"outcome":"exited","exit_code":0,"signal":null,"wall_clock_ms":12,"spilled_output":null,"aborted":false,"effects_unknown":false,"final_cwd_uri":null,"final_cwd_revision":0}}}"#,
		error_outcome: br#"{"kind":"faulted","value":{"kind":"resource","operation":"execute","message":"sample process failure"}}"#,
	},
	RendererGalleryFixture {
		identity: eval,
		title: "eval gallery",
		progress_update: Some(
		br#"{"channel":"stdout","data":[103,97,108,108,101,114,121,10],"sequence":1}"#,
		),
		success_outcome: br#"{"kind":"ok","value":{"session_id":[1],"cell_id":[1],"language":"py","title":"gallery","code":"print('gallery')","reset":false,"frames":[{"channel":"stdout","data":[103,97,108,108,101,114,121,10],"sequence":1}],"result":null,"display_outputs":[],"status":{"outcome":"complete","exit_code":0,"duration_ms":4,"exception":null},"truncated":false,"spilled_output":null,"total_lines":1,"total_bytes":8}}"#,
		error_outcome: br#"{"kind":"faulted","value":{"kind":"resource","operation":"run","message":"sample eval failure"}}"#,
	},
	]
}
