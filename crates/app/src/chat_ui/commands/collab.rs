use omp_collab::link::{CollabLink, RelayEndpoint, WebEndpoint};
use omp_core::Str;
use omp_driver::{
	collab::session::{CollabCommandResult, CollabOwnerCommand, HostOptions},
	settings::CollabSettings,
};

use super::{CollabRequest, ParsedFlags, command, parse_flags};

command!(collab, 700, "collab", icon: Broadcast, [], "Host or inspect live collaboration", [Session], true, typed("[start|view|status|stop] [--relay URL] [--web-url URL]", ["start", "view", "status", "stop", "--relay", "--web-url"], parse_collab) => |host, request| host.collab(request));
command!(join, 710, "join", icon: Input, [], "Join a live collaboration", [Session], true, required("<link>") => |host, link| host.join_collab(link));
command!(leave, 720, "leave", icon: Output, [], "Leave the active collaboration", [Session], true, none => |host| host.leave_collab());

fn parse_collab(raw: &str) -> miette::Result<CollabRequest> {
	let raw = raw.trim();
	if raw.is_empty() || raw == "status" {
		return Ok(CollabRequest::Status);
	}
	if raw == "view" {
		return Ok(CollabRequest::View);
	}
	if raw == "stop" {
		return Ok(CollabRequest::Stop);
	}
	let flags = raw
		.strip_prefix("start")
		.filter(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
		.ok_or_else(|| miette::miette!("usage: /collab [start|view|status|stop]"))?;
	let flags = parse_flags(flags.trim())?;
	for (flag, value) in &flags.0 {
		match flag.as_str() {
			"--relay" | "--web-url" if value.is_some() => {},
			"--relay" | "--web-url" => return Err(miette::miette!("{flag} requires a URL")),
			_ => return Err(miette::miette!("unknown collaboration option `{flag}`")),
		}
	}
	Ok(CollabRequest::Start(ParsedFlags(flags.0)))
}

pub(crate) fn owner_command(
	request: CollabRequest,
	settings: &CollabSettings,
) -> miette::Result<CollabOwnerCommand> {
	Ok(match request {
		CollabRequest::Status => CollabOwnerCommand::Status,
		CollabRequest::View => CollabOwnerCommand::View,
		CollabRequest::Stop => CollabOwnerCommand::Stop,
		CollabRequest::Start(flags) => {
			let mut relay = settings
				.relay_endpoint()
				.map_err(|error| miette::miette!(error))?;
			let mut web = settings
				.web_endpoint()
				.map_err(|error| miette::miette!(error))?;
			for (flag, value) in flags.0 {
				let value = value.expect("start options were validated by the command parser");
				match flag.as_str() {
					"--relay" => {
						relay = RelayEndpoint::parse(&value).map_err(|error| miette::miette!(error))?;
					},
					"--web-url" => {
						web = WebEndpoint::parse(&value).map_err(|error| miette::miette!(error))?;
					},
					_ => unreachable!("start options were validated by the command parser"),
				}
			}
			CollabOwnerCommand::Start(HostOptions { relay, web })
		},
	})
}

pub(crate) fn join_command(
	link: &str,
	settings: &CollabSettings,
) -> miette::Result<CollabOwnerCommand> {
	let link = CollabLink::parse(link).map_err(|error| miette::miette!(error))?;
	Ok(CollabOwnerCommand::Join { link, display_name: settings.resolved_display_name() })
}

pub(crate) fn render(result: CollabCommandResult) -> Str {
	let mut lines = Vec::new();
	let heading = if let Some(presence) = result.presence {
		format!(
			"**Collaboration** · {:?} · {:?} · {} participant{}{}",
			presence.role(),
			presence.connection(),
			presence.participant_count(),
			if presence.participant_count() == 1 {
				""
			} else {
				"s"
			},
			if presence.read_only() {
				" · read-only"
			} else {
				""
			},
		)
	} else {
		"**Collaboration inactive**".to_owned()
	};
	if let Some(link) = result.web_link.as_ref().or(result.web_view_link.as_ref()) {
		lines.push(format!("[Join in browser]({link})  {heading}"));
	} else {
		lines.push(heading);
	}
	for (label, link) in [
		("Join", result.full_link),
		("View", result.view_link),
		("Join in browser", result.web_link),
		("View in browser", result.web_view_link),
	] {
		if let Some(link) = link {
			lines.push(format!("- [{label}]({link})"));
		}
	}
	if lines.len() == 1 {
		Str::from(lines.remove(0))
	} else {
		Str::from(lines.join("\n"))
	}
}

#[cfg(test)]
mod tests {
	use omp_collab::presence::{ConnectionState, PresenceFacts};

	use super::*;

	#[test]
	fn browser_join_url_is_on_first_status_row() {
		let browser_url = Str::new_static("https://my.omp.sh/#a-very-long-collaboration-token");
		let rendered = render(CollabCommandResult {
			presence:      Some(PresenceFacts::host(ConnectionState::Connected, 0)),
			full_link:     Some(Str::new_static("native-link")),
			view_link:     None,
			web_link:      Some(browser_url.clone()),
			web_view_link: None,
		});
		let first = rendered.lines().next().expect("status has a heading");
		assert!(first.starts_with("[Join in browser]("), "{first}");
		assert!(first.contains(browser_url.as_str()), "{first}");
	}
}
