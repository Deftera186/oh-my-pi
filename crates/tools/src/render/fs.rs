//! Native read and write renderers.

use std::fmt::Write as _;

use omp_core::Str;
use omp_tool::{CallOutcome, ToolIdentity, render::RenderFold};

use super::{debug_label, fault_view, live_view, push_text};
use crate::{
	gallery::RendererGalleryFixture,
	read::{Fault as ReadFault, Payload as ReadPayload, PayloadPart, Update as ReadUpdate},
	write::{Fault as WriteFault, Payload as WritePayload, Update as WriteUpdate},
};

pub(super) struct WriteRenderer;

impl RenderFold for WriteRenderer {
	type Outcome = CallOutcome<WritePayload, WriteFault>;
	type State = ();
	type Update = WriteUpdate;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn view(&self, _state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(live_view("write", "writing")),
			Some(CallOutcome::Ok(payload)) => Some(render_write_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("write", &fault.to_string())),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

#[derive(Default)]
pub(super) struct ReadState {
	phase: Option<Str>,
}

pub(super) struct ReadRenderer;

impl RenderFold for ReadRenderer {
	type Outcome = CallOutcome<ReadPayload, ReadFault>;
	type State = ReadState;
	type Update = ReadUpdate;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.phase = Some(update.phase);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(live_view("read", state.phase.as_deref().unwrap_or("reading"))),
			Some(CallOutcome::Ok(payload)) => Some(render_read_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("read", fault.message())),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

fn render_write_payload(payload: &WritePayload) -> Str {
	let disposition = debug_label(payload.disposition);
	let mut output = String::from("<row gap=1><text bold>write</text><text>");
	push_text(&mut output, &disposition);
	output.push(' ');
	push_text(&mut output, &payload.display_path);
	output.push_str("</text><text fg=muted>");
	write!(output, "{} bytes", payload.byte_len).expect("writing to String cannot fail");
	if payload.made_executable {
		output.push_str(" · executable");
	}
	if payload.stripped_wrapper {
		output.push_str(" · stripped wrapper");
	}
	output.push_str("</text></row>");
	Str::new(output)
}

fn render_read_payload(payload: &ReadPayload) -> Str {
	let mut text_bytes = 0usize;
	let mut blobs = 0usize;
	let mut blob_bytes = 0u64;
	for part in &payload.parts {
		match part {
			PayloadPart::Text { text } => {
				text_bytes = text_bytes.saturating_add(text.len());
			},
			PayloadPart::Blob { blob, .. } => {
				blobs = blobs.saturating_add(1);
				blob_bytes = blob_bytes.saturating_add(blob.byte_len);
			},
		}
	}
	let mut output = String::from("<row gap=1><text bold>read</text><text>");
	write!(output, "{} parts · {text_bytes} text bytes", payload.parts.len())
		.expect("writing to String cannot fail");
	if blobs != 0 {
		write!(output, " · {blobs} blobs · {blob_bytes} blob bytes")
			.expect("writing to String cannot fail");
	}
	output.push_str("</text></row>");
	Str::new(output)
}

/// Native write and read renderer lifecycle fixtures for the visual QA gallery.
pub(crate) fn gallery_fixtures(
	write: ToolIdentity,
	read: ToolIdentity,
) -> Vec<RendererGalleryFixture> {
	vec![
	RendererGalleryFixture {
		identity: write,
		title: "write gallery.txt",
		progress_update: None,
		success_outcome: br#"{"kind":"ok","value":{"resolved_path":"/tmp/gallery.txt","display_path":"gallery.txt","byte_len":7,"reported_len":7,"disposition":"created","stripped_wrapper":false,"made_executable":false,"snapshot_tag":"ABCD","operation":{"kind":"plain"}}}"#,
		error_outcome: br#"{"kind":"faulted","value":{"kind":"document","message":"sample write failure"}}"#,
	},
	RendererGalleryFixture {
		identity: read,
		title: "read src/gallery.rs",
		progress_update: Some(br#"{"phase":"reading source"}"#),
		success_outcome: br#"{"kind":"ok","value":{"parts":[{"kind":"text","text":"1: gallery fixture"}]}}"#,
		error_outcome: br#"{"kind":"faulted","value":{"kind":"source","message":"sample source failure"}}"#,
	},
	]
}

#[cfg(test)]
mod tests {
	use omp_core::sf;
	use omp_tool::{Abort, ArgIssue, ArgIssueKind, CallOutcome, render::ViewState};

	use crate::{
		read::{Fault as ReadFault, Payload as ReadPayload},
		render::test_support::{identities, registry},
		write::{Fault as WriteFault, Payload as WritePayload, WriteDisposition, WriteOperation},
	};

	#[test]
	fn typed_fault_renders_while_args_and_abort_use_generic_facts() {
		let (registry, identities) = registry(identities());
		let state = ViewState::new();
		let fault = CallOutcome::<ReadPayload, ReadFault>::Faulted(ReadFault::Source {
			message: sf!("missing <file> & owner"),
		});
		let encoded_fault = serde_json::to_vec(&fault).expect("fault serializes");
		assert_eq!(
			registry
				.view(identities.read.as_ref().unwrap(), &state, Some(&encoded_fault))
				.expect("typed fault renders")
				.as_str(),
			"<row gap=1><text bold fg=error>read</text><text fg=error>missing &lt;file&gt; &amp; \
			 owner</text></row>",
		);

		let args = CallOutcome::<ReadPayload, ReadFault>::ArgsRejected(ArgIssue {
			path:     Vec::new(),
			expected: sf!("path"),
			kind:     ArgIssueKind::Missing,
			example:  Some(sf!(r#"{{"path":"src/lib.rs"}}"#)),
			found:    None,
		});
		let encoded_args = serde_json::to_vec(&args).expect("argument issue serializes");
		assert_eq!(
			registry
				.view(identities.read.as_ref().unwrap(), &state, Some(&encoded_args))
				.expect("argument fallback renders")
				.as_str(),
			std::str::from_utf8(&encoded_args).expect("JSON is UTF-8"),
		);

		let abort = CallOutcome::<ReadPayload, ReadFault>::aborted(Abort::Interrupted {
			reason: sf!("cancelled"),
		});
		let encoded_abort = serde_json::to_vec(&abort).expect("abort serializes");
		assert_eq!(
			registry
				.view(identities.read.as_ref().unwrap(), &state, Some(&encoded_abort))
				.expect("abort fallback renders")
				.as_str(),
			std::str::from_utf8(&encoded_abort).expect("JSON is UTF-8"),
		);
	}

	#[test]
	fn settled_output_is_deterministic_and_escapes_payload_text() {
		let (registry, identities) = registry(identities());
		let outcome = CallOutcome::<WritePayload, WriteFault>::Ok(WritePayload {
			resolved_path:      sf!("/tmp/a<&.txt"),
			display_path:       sf!("a<&.txt"),
			canonical_recovery: None,
			byte_len:           9,
			reported_len:       9,
			disposition:        WriteDisposition::Created,
			stripped_wrapper:   false,
			made_executable:    true,
			snapshot_tag:       Some(sf!("ABCD")),
			operation:          WriteOperation::Plain,
		});
		let encoded = serde_json::to_vec(&outcome).expect("outcome serializes");
		let state = ViewState::new();
		let write_identity = identities
			.write
			.as_ref()
			.expect("write identity registered");
		let first = registry
			.view(write_identity, &state, Some(&encoded))
			.expect("write renders");
		let second = registry
			.view(write_identity, &state, Some(&encoded))
			.expect("write rerenders");
		assert_eq!(first, second);
		assert_eq!(
			first.as_str(),
			"<row gap=1><text bold>write</text><text>created a&lt;&amp;.txt</text><text fg=muted>9 \
			 bytes · executable</text></row>",
		);
	}
}
