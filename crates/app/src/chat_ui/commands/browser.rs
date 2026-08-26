//! Browser surface-mode command and private daemon restart invocation.

use futures::StreamExt as _;
use miette::IntoDiagnostic as _;
use omp_core::{Str, sf};
use omp_settings::BrowserSettings;
use omp_tool::{CallOutcome, ErasedEv, ErasedOutcome, Registry};
use omp_tools::browser::{Action, Fault, Params, Payload};

use super::{BrowserRequest, command};

command!(browser, 725, "browser", icon: Globe, [], "Toggle browser headless vs visible mode", [Execution, Owner], false, typed("[headless|visible]", [
	("headless", "Switch to headless mode"),
	("visible", "Switch to visible mode"),
], parse_browser) => |host, request| host.browser(request));

fn parse_browser(raw: &str) -> miette::Result<BrowserRequest> {
	match raw.trim().to_ascii_lowercase().as_str() {
		"" => Ok(BrowserRequest::Toggle),
		"headless" | "hidden" => Ok(BrowserRequest::Headless),
		"visible" | "show" | "headful" => Ok(BrowserRequest::Visible),
		_ => Err(miette::miette!("Usage: /browser [headless|visible]")),
	}
}

pub(crate) const fn autocomplete_description(settings: &BrowserSettings) -> &'static str {
	if !settings.enabled {
		"Browser: disabled"
	} else if settings.headless {
		"Browser: headless"
	} else {
		"Browser: visible"
	}
}

pub(crate) async fn restart_for_mode_change(
	registry: &Registry,
	headless: bool,
) -> miette::Result<()> {
	if !registry
		.devices()
		.any(|device| device.name.as_str() == "browser")
	{
		return Ok(());
	}
	let params = Params {
		action:                  Action::Close,
		name:                    None,
		url:                     None,
		operation:               None,
		code:                    None,
		selector:                None,
		target:                  None,
		value:                   None,
		values:                  None,
		width:                   None,
		height:                  None,
		scale:                   None,
		timeout:                 None,
		all:                     true,
		full_page:               false,
		restart_for_mode_change: Some(headless),
	};
	let raw = serde_json::to_string(&params).into_diagnostic()?;
	let (feed, incoming) = omp_tool::IncomingParams::owned_channel(sf!("slash-browser-mode"));
	feed.args_committed(Str::from(raw)).into_diagnostic()?;
	drop(feed);
	let mut stream = registry.invoke("browser", incoming).into_diagnostic()?;
	while let Some(event) = stream.next().await {
		match event.into_diagnostic()? {
			ErasedEv::Update(_) => {},
			ErasedEv::Done(ErasedOutcome::Detached(_)) => {
				return Err(miette::miette!("browser mode restart detached unexpectedly"));
			},
			ErasedEv::Done(ErasedOutcome::Done { verdict, .. }) => {
				let outcome =
					serde_json::from_slice::<CallOutcome<Payload, Fault>>(&verdict).into_diagnostic()?;
				return match outcome {
					CallOutcome::Ok(_) => Ok(()),
					CallOutcome::Faulted(fault) => Err(miette::miette!("{fault}")),
					CallOutcome::ArgsRejected(_) => {
						Err(miette::miette!("browser mode restart arguments were rejected"))
					},
					CallOutcome::Aborted { .. } => {
						Err(miette::miette!("browser mode restart was aborted"))
					},
				};
			},
		}
	}
	Err(miette::miette!("browser mode restart ended without a verdict"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_toggle_and_mode_aliases() {
		assert_eq!(parse_browser("").expect("toggle"), BrowserRequest::Toggle);
		assert_eq!(parse_browser("hidden").expect("hidden alias"), BrowserRequest::Headless);
		assert_eq!(parse_browser("SHOW").expect("visible alias"), BrowserRequest::Visible);
		assert_eq!(parse_browser("headful").expect("headful alias"), BrowserRequest::Visible);
	}

	#[test]
	fn rejects_unknown_mode_with_usage() {
		let error = parse_browser("sideways").expect_err("unknown browser mode");
		assert_eq!(error.to_string(), "Usage: /browser [headless|visible]");
	}

	#[test]
	fn declaration_uses_globe_and_described_canonical_modes() {
		let declaration = inventory::iter::<super::super::registry::BuiltinRegistration>
			.into_iter()
			.map(|registration| (registration.declaration)())
			.find(|declaration| declaration.name.as_str() == "browser")
			.expect("browser declaration");
		assert_eq!(declaration.icon, omp_tui::Icon::Globe);
		assert_eq!(
			declaration
				.hints
				.iter()
				.map(|hint| (hint.value.as_str(), hint.description.as_str()))
				.collect::<Vec<_>>(),
			vec![("headless", "Switch to headless mode"), ("visible", "Switch to visible mode"),]
		);
	}

	#[test]
	fn autocomplete_reflects_browser_state() {
		assert_eq!(
			autocomplete_description(&BrowserSettings { enabled: false, headless: true }),
			"Browser: disabled"
		);
		assert_eq!(
			autocomplete_description(&BrowserSettings { enabled: true, headless: true }),
			"Browser: headless"
		);
		assert_eq!(
			autocomplete_description(&BrowserSettings { enabled: true, headless: false }),
			"Browser: visible"
		);
	}
}
