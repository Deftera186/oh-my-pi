//! In-place provider-login overlay.
//!
//! One updating surface for the whole authentication dance — browser
//! authorization, device codes, progress notices, and manual input — mirroring
//! pi's login dialog instead of appending a transcript line per step. The
//! transcript receives only the final outcome, sent by the app bridge after
//! the panel closes.

use omp_core::{IntoStr, Str, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, UiEvent, dom,
};

use crate::overlays::{OverlayPanel, panel_divider};

const PANEL_WIDTH: u16 = 64;
const INPUT_ID: &str = "login-input";

/// One backend update applied to the open login panel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginEvent {
	/// Show the browser authorization location.
	Url {
		/// Full provider authorization URL.
		url:    Str,
		/// Short loopback launch URL (`http://localhost:<port>/launch`) when a
		/// callback server is bound.
		launch: Option<Str>,
	},
	/// Show a device code with its verification URL.
	DeviceCode {
		/// Short user code to enter at the verification URL.
		code: Str,
		/// Public verification URL.
		url:  Str,
	},
	/// Replace the status line in place.
	Notice(Str),
	/// Request manual input.
	Prompt {
		/// Prompt message shown above the input.
		message: Str,
		/// Whether typed input must be masked.
		masked:  bool,
	},
}

/// Where the user completes authorization in the browser.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Location {
	Url { url: Str, launch: Option<Str> },
	DeviceCode { code: Str, url: Str },
}

/// Result of routing input through a [`LoginPanel`].
pub enum LoginPanelEvent {
	/// Event consumed while the panel stays open.
	Consumed,
	/// Login cancelled by the user.
	Cancel,
	/// Manual input submitted with the unmasked value.
	Submit(Str),
}

/// Centered overlay hosting one provider login from start to outcome.
///
/// Updates mutate the retained status/location/prompt state and repaint in
/// place, so the authorization URL stays visible while the user answers a
/// paste prompt. Unlike [`crate::overlays::PromptOverlay`], an outside click
/// never cancels: a login waits on an external browser dance and a stray
/// click must not abort it — Esc is the cancel affordance.
pub struct LoginPanel {
	ui:       Ui,
	provider: Str,
	status:   Str,
	location: Option<Location>,
	prompt:   Option<(Str, bool)>,
	ctx:      UiContext,
	options:  OverlayOptions,
	value:    Str,
}

impl LoginPanel {
	/// Opens an empty panel for one provider login.
	pub fn open(provider: impl IntoStr, ctx: &UiContext) -> Self {
		let provider = provider.into_str();
		let status = Str::new_static("Contacting provider…");
		let ui = build_ui(&provider, &status, None, None, PANEL_WIDTH, ctx);
		Self {
			ui,
			provider,
			status,
			location: None,
			prompt: None,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(PANEL_WIDTH))
				.z(20),
			value: Str::default(),
		}
	}

	/// Applies a backend update, repainting the panel in place.
	pub fn update(&mut self, event: LoginEvent) {
		match event {
			LoginEvent::Url { url, launch } => {
				// Device flows follow the code with a pre-filled
				// `verification_uri_complete` OpenUrl; adopt the better link but
				// keep the code visible for browsers that do not carry it over.
				self.location = Some(match self.location.take() {
					Some(Location::DeviceCode { code, .. }) => Location::DeviceCode { code, url },
					_ => Location::Url { url, launch },
				});
				self.status = Str::new_static("Waiting for authorization…");
			},
			LoginEvent::DeviceCode { code, url } => {
				self.location = Some(Location::DeviceCode { code, url });
				self.status = Str::new_static("Waiting for authorization…");
			},
			LoginEvent::Notice(message) => self.status = message,
			LoginEvent::Prompt { message, masked } => {
				self.prompt = Some((message, masked));
				self.value = Str::default();
			},
		}
		self.rebuild(PANEL_WIDTH);
	}

	/// Routes a key into the panel.
	pub fn handle_key(&mut self, key: Key) -> LoginPanelEvent {
		if key == Key::Esc {
			return LoginPanelEvent::Cancel;
		}
		if key == Key::Enter && self.prompt.is_some() {
			return LoginPanelEvent::Submit(self.value.clone());
		}
		let event = self.ui.handle_key(key);
		self.sync_value();
		self.route(event)
	}

	/// Routes pasted text into the prompt input.
	pub fn handle_paste(&mut self, text: &str) -> LoginPanelEvent {
		let event = self.ui.handle_paste(text);
		self.sync_value();
		self.route(event)
	}

	/// Routes a pointer event; outside clicks are consumed, never a cancel.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> LoginPanelEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => {
				self.sync_value();
				self.route(event)
			},
			None => LoginPanelEvent::Consumed,
		}
	}

	/// Returns a centered rounded-box composited layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(4).clamp(1, PANEL_WIDTH);
		if self.ui.frame().size().width != width {
			self.rebuild(width);
		}
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn rebuild(&mut self, width: u16) {
		self.ui = build_ui(
			&self.provider,
			&self.status,
			self.location.as_ref(),
			self.prompt.as_ref(),
			width,
			&self.ctx,
		);
		if self.prompt.is_some() {
			self.ui.set_text(INPUT_ID, self.value.as_str());
			self.ui.focus_first();
		}
	}

	fn sync_value(&mut self) {
		if let Some(value) = self.ui.values()[INPUT_ID].as_str() {
			self.value = Str::new(value);
		}
	}

	fn route(&self, event: UiEvent) -> LoginPanelEvent {
		match event {
			UiEvent::Cancel => LoginPanelEvent::Cancel,
			UiEvent::Submit => LoginPanelEvent::Submit(self.value.clone()),
			_ => LoginPanelEvent::Consumed,
		}
	}
}

fn build_ui(
	provider: &str,
	status: &Str,
	location: Option<&Location>,
	prompt: Option<&(Str, bool)>,
	width: u16,
	ctx: &UiContext,
) -> Ui {
	let title = sf!("Log in to {provider}");
	let status = status.clone();
	let content = dom! {
		<col gap=1>
			<text fg=info wrap>{status}</text>
			if let Some(location) = location {
				{panel_divider()}
				match location {
					Location::Url { url, launch } => {
						<md>{launch.as_ref().unwrap_or(url).clone()}</md>
						<text dim wrap>
							if launch.is_some() {
								"If your browser didn't open, follow the link above."
							} else {
								"Authorize in your browser."
							}
						</text>
					},
					Location::DeviceCode { code, url } => {
						<row gap=1>
							<text fg=muted>{"code"}</text>
							<text bold fg=accent>{code.clone()}</text>
						</row>
						<md>{url.clone()}</md>
						<text dim wrap>{"Enter the code at the link above."}</text>
					},
				}
			}
			{panel_divider()}
			if let Some((message, masked)) = prompt {
				<text wrap>{message.clone()}</text>
				<input id="login-input" submit mask={*masked} placeholder="Paste code or URL"/>
				<text dim truncate>{"Enter submit · Esc cancel"}</text>
			} else {
				<text dim truncate>{"Esc cancel"}</text>
			}
		</col>
	};
	Ui::from_root(OverlayPanel::new(title).pad_y(1).child(content), width, ctx.clone())
}

#[cfg(test)]
mod tests {
	use omp_tui::Frame;

	use super::*;

	fn frame_text(frame: &Frame) -> String {
		(0..frame.size().height)
			.map(|row| omp_tui::test_support::frame_row_text(frame, row))
			.collect::<Vec<_>>()
			.join("\n")
	}

	fn text(panel: &mut LoginPanel) -> String {
		frame_text(panel.layer(Size::new(80, 24)).frame)
	}

	#[test]
	fn url_update_prefers_the_short_launch_link_and_repaints_in_place() {
		let mut panel = LoginPanel::open("kimi-code", &UiContext::default());
		assert!(text(&mut panel).contains("Log in to kimi-code"));

		panel.update(LoginEvent::Url {
			url:    Str::new_static("https://auth.example/authorize?challenge=pkce"),
			launch: Some(Str::new_static("http://localhost:1455/launch")),
		});
		let rendered = text(&mut panel);
		assert!(rendered.contains("http://localhost:1455/launch"));
		assert!(!rendered.contains("challenge=pkce"), "full URL yields to the short launch link");

		panel.update(LoginEvent::Notice(Str::new_static("Waiting for `kimi-code` authorization…")));
		let updated = text(&mut panel);
		assert!(updated.contains("Waiting for `kimi-code` authorization…"));
		assert!(
			updated.contains("http://localhost:1455/launch"),
			"a status update keeps the authorization location visible"
		);
	}

	#[test]
	fn device_code_stays_visible_and_prominent() {
		let mut panel = LoginPanel::open("kimi-code", &UiContext::default());
		panel.update(LoginEvent::DeviceCode {
			code: Str::new_static("WY7H-2AZ4"),
			url:  Str::new_static("https://www.kimi.com/code/authorize_device"),
		});
		let rendered = text(&mut panel);
		assert!(rendered.contains("WY7H-2AZ4"));
		assert!(rendered.contains("https://www.kimi.com/code/authorize_device"));
		panel.update(LoginEvent::Url {
			url:    Str::new_static("https://www.kimi.com/code/authorize_device?user_code=WY7H-2AZ4"),
			launch: None,
		});
		let followed = text(&mut panel);
		assert!(
			followed.contains("WY7H-2AZ4"),
			"the complete-URI follow-up keeps the device code visible"
		);
		assert!(
			followed
				.chars()
				.filter(|c| c.is_ascii() && !c.is_whitespace())
				.collect::<String>()
				.contains("user_code=WY7H-2AZ4"),
			"the pre-filled link replaces the bare one"
		);
	}

	#[test]
	fn prompt_round_trips_masked_input_and_keeps_the_location() {
		let mut panel = LoginPanel::open("anthropic", &UiContext::default());
		panel.update(LoginEvent::Url {
			url:    Str::new_static("https://auth.example/authorize"),
			launch: None,
		});
		panel.update(LoginEvent::Prompt {
			message: Str::new_static("Paste the authorization code"),
			masked:  true,
		});
		let rendered = text(&mut panel);
		assert!(rendered.contains("Paste the authorization code"));
		assert!(
			rendered.contains("https://auth.example/authorize"),
			"the prompt keeps the authorization URL visible for the paste flow"
		);

		for character in "secret".chars() {
			assert!(matches!(panel.handle_key(Key::Char(character)), LoginPanelEvent::Consumed));
		}
		match panel.handle_key(Key::Enter) {
			LoginPanelEvent::Submit(value) => assert_eq!(value.as_str(), "secret"),
			LoginPanelEvent::Consumed | LoginPanelEvent::Cancel => panic!("prompt did not submit"),
		}
	}

	#[test]
	fn escape_cancels_and_outside_clicks_do_not() {
		let mut panel = LoginPanel::open("kimi-code", &UiContext::default());
		panel.update(LoginEvent::Url {
			url:    Str::new_static("https://auth.example/authorize"),
			launch: None,
		});
		assert!(matches!(
			panel.handle_mouse(0, 0, Mouse::Click, Size::new(80, 24)),
			LoginPanelEvent::Consumed
		));
		assert!(matches!(panel.handle_key(Key::Esc), LoginPanelEvent::Cancel));
	}
}
