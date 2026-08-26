//! Native hub renderer: rosters, jobs, processes, logs, and messages.

use std::fmt::Write as _;

use omp_core::Str;
use omp_tool::{CallOutcome, ToolIdentity, render::RenderFold};

use super::{fault_view, live_view, push_text};
use crate::{
	gallery::RendererGalleryFixture,
	hub::{Fault as HubFault, Response as HubResponse},
};

#[derive(Default)]
pub(super) struct HubState {
	latest: Option<HubResponse>,
}

pub(super) struct HubRenderer;

impl RenderFold for HubRenderer {
	type Outcome = CallOutcome<HubResponse, HubFault>;
	type State = HubState;
	type Update = HubResponse;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.latest = Some(update);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => state
				.latest
				.as_ref()
				.and_then(render_hub_response)
				.or_else(|| Some(live_view("hub", "waiting for peer, job, or process activity"))),
			Some(CallOutcome::Ok(response)) => render_hub_response(response),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("hub", &fault.message)),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

fn render_hub_response(response: &HubResponse) -> Option<Str> {
	let value = serde_json::from_str::<serde_json::Value>(&response.text).ok()?;
	let object = value.as_object()?;
	if let Some(peers) = object.get("peers").and_then(serde_json::Value::as_array) {
		return Some(render_hub_roster(peers));
	}
	if let Some(jobs) = object.get("jobs").and_then(serde_json::Value::as_array) {
		return Some(render_hub_jobs(
			jobs,
			object.get("waitingMs").and_then(serde_json::Value::as_u64),
		));
	}
	if let Some(processes) = object
		.get("processes")
		.and_then(serde_json::Value::as_array)
	{
		return Some(render_hub_processes(processes));
	}
	if object.contains_key("lines") {
		return Some(render_hub_logs(object));
	}
	if object.contains_key("deliveries")
		|| object.contains_key("messages")
		|| object.contains_key("message")
	{
		return Some(render_hub_messages(object));
	}
	if object.contains_key("timeout")
		|| object.contains_key("waitedMs")
		|| object.contains_key("waitingMs")
	{
		return Some(render_hub_wait(object));
	}
	if object.contains_key("name") || object.contains_key("event") || object.contains_key("job") {
		return Some(render_hub_process_or_job(object));
	}
	None
}

fn render_hub_roster(peers: &[serde_json::Value]) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>@</text><text bold>Hub roster</text><text fg=muted>",
	);
	write!(output, "{} peers", peers.len()).expect("writing to String cannot fail");
	output.push_str("</text></row>");
	for peer in peers.iter().take(24) {
		let Some(peer) = peer.as_object() else {
			continue;
		};
		let name = json_string(peer, &["name", "callerName", "id"]).unwrap_or("unknown");
		let status = json_string(peer, &["status", "lifecycle"]).unwrap_or("unknown");
		let parent = json_string(peer, &["parent", "parentId"]);
		let unread = json_u64(peer, &["unread", "unreadCount"]).unwrap_or(0);
		let active = matches!(status, "running" | "active" | "reviving" | "queued");
		output.push_str("<row gap=1>");
		if active {
			output.push_str("<spinner></spinner>");
		} else {
			output.push_str("<text fg=muted>○</text>");
		}
		output.push_str("<text bold>");
		push_text(&mut output, name);
		output.push_str("</text><text fg=muted>");
		push_text(&mut output, status);
		if let Some(parent) = parent {
			output.push_str(" · child of ");
			push_text(&mut output, parent);
		}
		if unread > 0 {
			write!(output, " · {unread} unread").expect("writing to String cannot fail");
		}
		if let Some(activity) = json_u64(peer, &["lastActivityMs", "activityMs", "updatedAtMs"]) {
			write!(output, " · {activity} ms").expect("writing to String cannot fail");
		}
		output.push_str("</text></row>");
	}
	if peers.len() > 24 {
		write!(output, "<text fg=muted>+{} more peers</text>", peers.len() - 24)
			.expect("writing to String cannot fail");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_jobs(jobs: &[serde_json::Value], waiting_ms: Option<u64>) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>&amp;</text><text bold>Jobs</text><text fg=muted>",
	);
	write!(output, "{} tracked", jobs.len()).expect("writing to String cannot fail");
	if let Some(waiting_ms) = waiting_ms {
		output.push_str("</text><spinner>");
		write!(output, "waiting {waiting_ms} ms").expect("writing to String cannot fail");
		output.push_str("</spinner><text fg=muted>");
	}
	output.push_str("</text></row>");
	for job in jobs.iter().take(24) {
		let Some(job) = job.as_object() else {
			continue;
		};
		let id = json_string(job, &["id", "job", "name"]).unwrap_or("unknown");
		let status = json_string(job, &["status", "state", "lifecycle"]).unwrap_or("unknown");
		let running = matches!(status, "queued" | "running" | "active" | "waiting");
		output.push_str("<row gap=1>");
		if running {
			output.push_str("<spinner></spinner>");
		} else {
			output.push_str("<text fg=muted>└</text>");
		}
		output.push_str("<text bold>");
		push_text(&mut output, id);
		output.push_str("</text><text fg=muted>");
		push_text(&mut output, status);
		if let Some(kind) = json_string(job, &["kind", "model"]) {
			output.push_str(" · ");
			push_text(&mut output, kind);
		}
		if let Some(duration) = json_u64(job, &["durationMs", "elapsedMs"]) {
			write!(output, " · {duration} ms").expect("writing to String cannot fail");
		}
		output.push_str("</text></row>");
		if let Some(error) = json_string(job, &["error", "reason"]) {
			output.push_str("<text fg=error>  ");
			push_text(&mut output, error);
			output.push_str("</text>");
		}
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_processes(processes: &[serde_json::Value]) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=secondary><col gap=0><row gap=1><text bold \
		 fg=secondary>&gt;_</text><text bold>Processes</text><text fg=muted>",
	);
	write!(output, "{} supervised", processes.len()).expect("writing to String cannot fail");
	output.push_str("</text></row>");
	for process in processes.iter().take(24) {
		let Some(process) = process.as_object() else {
			continue;
		};
		let name = json_string(process, &["name"]).unwrap_or("unknown");
		let state = json_string(process, &["status", "state"]).unwrap_or("unknown");
		output.push_str("<row gap=1><text bold>");
		push_text(&mut output, name);
		output.push_str("</text><text fg=muted>");
		push_text(&mut output, state);
		if let Some(pid) = json_u64(process, &["pid"]) {
			write!(output, " · pid {pid}").expect("writing to String cannot fail");
		}
		if let Some(uptime) = json_u64(process, &["uptimeMs", "elapsedMs"]) {
			write!(output, " · up {uptime} ms").expect("writing to String cannot fail");
		}
		output.push_str("</text></row>");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_logs(object: &serde_json::Map<String, serde_json::Value>) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=secondary><col gap=0><row gap=1><text bold \
		 fg=secondary>&gt;_</text><text bold>Process log</text>",
	);
	if let Some(name) = json_string(object, &["name"]) {
		output.push_str("<text fg=muted>");
		push_text(&mut output, name);
		output.push_str("</text>");
	}
	output.push_str("</row><box border=round bc=muted><pre>");
	if let Some(lines) = object.get("lines").and_then(serde_json::Value::as_array) {
		for (index, line) in lines.iter().take(80).enumerate() {
			if index > 0 {
				output.push('\n');
			}
			push_text(&mut output, line.as_str().unwrap_or_default());
		}
	} else if let Some(lines) = object.get("lines").and_then(serde_json::Value::as_str) {
		push_text(&mut output, lines);
	}
	output.push_str("</pre></box>");
	if let Some(cursor) = json_u64(object, &["cursor"]) {
		write!(output, "<text fg=muted>cursor {cursor}</text>")
			.expect("writing to String cannot fail");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_messages(object: &serde_json::Map<String, serde_json::Value>) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>@</text><text bold>IRC</text></row>",
	);
	let rows = object
		.get("messages")
		.or_else(|| object.get("deliveries"))
		.and_then(serde_json::Value::as_array);
	if let Some(rows) = rows {
		for row in rows.iter().take(24) {
			render_hub_message_row(&mut output, row);
		}
	} else if let Some(message) = object.get("message") {
		if !message.is_null() {
			render_hub_message_row(&mut output, message);
		} else {
			output.push_str("<text fg=muted>no message received</text>");
		}
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_message_row(output: &mut String, value: &serde_json::Value) {
	let Some(message) = value.as_object() else {
		output.push_str("<text fg=muted>");
		push_text(output, value.as_str().unwrap_or_default());
		output.push_str("</text>");
		return;
	};
	let from = json_string(message, &["from", "sender"]).unwrap_or("me");
	let to = json_string(message, &["to", "recipient"]).unwrap_or("hub");
	let text = json_string(message, &["text", "message", "outcome", "status"]).unwrap_or_default();
	output.push_str("<row gap=1><text fg=info>");
	push_text(output, from);
	output.push_str(" → ");
	push_text(output, to);
	output.push_str("</text><text>");
	push_text(output, text);
	output.push_str("</text></row>");
}

fn render_hub_wait(object: &serde_json::Map<String, serde_json::Value>) -> Str {
	let waited = json_u64(object, &["waitingMs", "waitedMs"]).unwrap_or(0);
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><row gap=1><spinner>waiting</spinner><text fg=muted>",
	);
	write!(output, "{waited} ms elapsed").expect("writing to String cannot fail");
	if object.get("timeout").and_then(serde_json::Value::as_bool) == Some(true) {
		output.push_str(" · timeout");
	}
	output.push_str("</text></row></box>");
	Str::new(output)
}

fn render_hub_process_or_job(object: &serde_json::Map<String, serde_json::Value>) -> Str {
	let label = if object.contains_key("job") {
		"Job"
	} else {
		"Process"
	};
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=secondary><col gap=0><row gap=1><text bold fg=secondary>",
	);
	push_text(&mut output, label);
	output.push_str("</text><text bold>");
	if let Some(name) = json_string(object, &["name", "job"]) {
		push_text(&mut output, name);
	}
	output.push_str("</text></row>");
	for (key, value) in object {
		if matches!(key.as_str(), "name" | "job") {
			continue;
		}
		output.push_str("<row gap=1><text fg=muted>");
		push_text(&mut output, key);
		output.push_str("</text><text truncate>");
		push_text(&mut output, &json_compact(value));
		output.push_str("</text></row>");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn json_string<'a>(
	object: &'a serde_json::Map<String, serde_json::Value>,
	keys: &[&str],
) -> Option<&'a str> {
	keys
		.iter()
		.find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
}

fn json_u64(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<u64> {
	keys
		.iter()
		.find_map(|key| object.get(*key).and_then(serde_json::Value::as_u64))
}

fn json_compact(value: &serde_json::Value) -> String {
	match value {
		serde_json::Value::String(value) => value.clone(),
		_ => serde_json::to_string(value).unwrap_or_default(),
	}
}

/// Native hub renderer lifecycle fixtures for the visual QA gallery.
pub(crate) fn gallery_fixtures(hub: ToolIdentity) -> Vec<RendererGalleryFixture> {
	vec![
	RendererGalleryFixture {
		identity: hub,
		title: "hub jobs",
		progress_update: Some(
		br#"{"text":"{\"waitingMs\":250,\"jobs\":[]}","useless":true}"#,
		),
		success_outcome: br#"{"kind":"ok","value":{"text":"{\"peers\":[{\"name\":\"Gallery\",\"status\":\"running\",\"unreadCount\":0,\"parent\":\"Main\"}]}","useless":false}}"#,
		error_outcome: br#"{"kind":"faulted","value":{"message":"sample coordination failure"}}"#,
	},
	]
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use omp_core::Str;
	use omp_tool::{CallOutcome, render::ViewState};

	use crate::{
		hub::{Fault as HubFault, Response as HubResponse},
		render::test_support::{identities, registry},
	};

	#[test]
	fn hub_renderer_projects_wait_progress_roster_and_isolated_logs() {
		let (registry, identities) = registry(identities());
		let hub = identities.hub.as_ref().expect("hub identity");
		let mut state = ViewState::new();
		let progress =
			HubResponse { text: Str::from(r#"{"waitingMs":500,"jobs":[]}"#), useless: true };
		registry
			.fold(
				hub,
				&mut state,
				Bytes::from(serde_json::to_vec(&progress).expect("progress serializes")),
			)
			.expect("hub progress folds");
		let live = registry
			.view(hub, &state, None)
			.expect("hub progress renders");
		assert!(live.contains("<spinner>"));
		assert!(live.contains("waiting 500 ms"));

		let response = HubResponse {
			text:    Str::from(
				r#"{"peers":[{"name":"Scout","status":"running","unreadCount":2,"parent":"Main"}]}"#,
			),
			useless: false,
		};
		let encoded = serde_json::to_vec(&CallOutcome::<HubResponse, HubFault>::Ok(response))
			.expect("outcome serializes");
		let roster = registry
			.view(hub, &state, Some(&encoded))
			.expect("roster renders");
		assert!(roster.contains("Hub roster"));
		assert!(roster.contains("Scout"));
		assert!(roster.contains("2 unread"));
	}
}
