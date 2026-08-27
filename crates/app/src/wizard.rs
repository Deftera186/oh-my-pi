//! Retained first-run setup flow for interactive chat.

use std::{fs, path::Path, time::Duration};

use miette::{IntoDiagnostic as _, miette};
use omp_catalog::{ProviderDef, ProviderId, provider::AuthSpecKind, snapshot::Catalog};
use omp_chat_ui::{
	OverlayPanel, panel_divider,
	provider_picker::{ProviderCard, provider_card_grid},
};
use omp_core::{IntoStr, Str, sf};
use omp_driver::chat::ChatAuthWorker;
use omp_inference::{
	Client, Registry as InferenceRegistry,
	answer::{AccountState, AuthAnswer},
	call::{AuthRequest, CallMeta, Target},
	id::RequestId,
	receipt::ExecutionBudget,
	router::Router,
};
use omp_tui::{
	AppEvent, AppOptions, Dim, Key, OverlayAnchor, OverlayMargin, OverlayOptions, Prop, Size, Ui,
	components::{Button, Col, Input, Markdown, Select, SelectOption, Shader, TextLeaf},
	shader::Eclipse,
};

use crate::chat_ui::{
	AuthPromptKind, CREDENTIAL_STORAGE_LOCKED_MESSAGE, ChatAuthEvent, auth_input, prompt_masks_input,
};

const STATUS_ID: &str = "wizard-status";
/// Card-id namespace shared with the setup provider grid.
const PROVIDER_CARD_PREFIX: &str = "login-provider:";
const MODEL_SELECT_ID: &str = "model-picker";
const CONTINUE_ID: &str = "wizard-continue";

/// Setup scenes and their only legal exits.
///
/// `Welcome` continues to `Provider` (or `Model` for an existing account).
/// `Provider` starts `Authenticating`, while cancellation returns to `Welcome`.
/// `Authenticating` may open `Prompt`, complete into `Model`, fail back to
/// `Provider`, or stop at the blocking `CredentialStorageLocked` state.
/// `Prompt` submits back to `Authenticating` and cancellation/failure returns
/// to `Provider`. `CredentialStorageLocked` must be dismissed before returning
/// to `Welcome`. `Model` completes the wizard or cancels back to `Welcome`.
/// Every variant therefore owns a visible, keyboard-usable scene.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
	Welcome,
	Provider,
	Authenticating,
	CredentialStorageLocked,
	Prompt(AuthPromptKind),
	Model,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AuthLocation {
	Url { url: Str, launch: Option<Str> },
	DeviceCode { code: Str, url: Str },
}

/// Runs first-run setup and returns the persisted model selection.
///
/// A clean cancellation returns `None`; provider login and model selection are
/// completed inside this retained terminal host before it is dropped.
#[expect(clippy::future_not_send, reason = "the setup wizard owns a thread-confined omp_tui::App")]
pub async fn run(data_dir: &Path, catalog: &Catalog) -> miette::Result<Option<Str>> {
	fs::create_dir_all(data_dir).into_diagnostic()?;
	let store = omp_driver::registry::open_credential_store(data_dir.join("credentials.db"))
		.into_diagnostic()?;
	let registry = omp_driver::registry::production_registry(data_dir, store)
		.await
		.into_diagnostic()?;
	let has_account = has_active_account(&registry).await?;
	let worker = ChatAuthWorker::start(registry.clone());
	let auth = worker.ui();

	let mut app = AppOptions::new()
		.hold_alt()
		.keep_on_cancel()
		.start(|env: omp_tui::AppEnv| {
			Ui::from_root(
				Shader::new(Eclipse::default()).size(env.viewport.width, env.viewport.height),
				env.viewport.width,
				env.ctx,
			)
		})
		.await
		.into_diagnostic()?;
	show_welcome(app.ui_mut());
	let mut step = Step::Welcome;
	let mut auth_location = None;
	let mut auth_prompt_message = None;
	let mut active_provider: Option<Str> = None;

	let selected = 'wizard: loop {
		tokio::select! {
			event = app.next() => match event.into_diagnostic()? {
				None => break 'wizard None,
				Some(AppEvent::Pressed(id))
					if id.as_str() == CONTINUE_ID && step == Step::Welcome =>
				{
					let _ = app.ui_mut().close_top_overlay();
					if has_account {
						open_setup_model_step(app.ui_mut(), catalog, "", None);
						step = Step::Model;
					} else {
						open_setup_provider_step(app.ui_mut(), catalog);
						step = Step::Provider;
					}
				},
				Some(AppEvent::Submitted) => {
					if let Step::Prompt(kind) = step {
						let value = app.ui().values()["auth-secret"]
							.as_str()
							.unwrap_or("")
							.to_owned();
						let _ = app.ui_mut().close_top_overlay();
						match auth.answer(auth_input(kind, value)) {
							Ok(()) => {
								show_authenticating(app.ui_mut());
								set_status(app.ui_mut(), "Authenticating… Esc to cancel");
								auth_prompt_message = None;
								step = Step::Authenticating;
							},
							Err(error) => {
								open_setup_provider_step(app.ui_mut(), catalog);
								set_status(app.ui_mut(), sf!("Setup error: {error}"));
								auth_location = None;
								auth_prompt_message = None;
								step = Step::Provider;
							},
						}
					}
				},
				Some(AppEvent::Key(key)) if step == Step::Provider => {
					if key == omp_tui::Key::Esc {
						let _ = app.ui_mut().close_top_overlay();
						if has_active_account(&registry).await.unwrap_or(false) {
							break 'wizard None;
						}
						show_welcome(app.ui_mut());
						step = Step::Welcome;
					}
				},
				Some(AppEvent::Pressed(id))
					if step == Step::Provider
						&& id.as_str().starts_with(PROVIDER_CARD_PREFIX) =>
				{
					let value = Str::from(
						id.as_str()
							.strip_prefix(PROVIDER_CARD_PREFIX)
							.expect("guarded above"),
					);
					let _ = app.ui_mut().close_top_overlay();
					auth_location = None;
					auth_prompt_message = None;
					active_provider = Some(value.clone());
					match auth.start(value.clone()) {
						Ok(()) => {
							show_authenticating(app.ui_mut());
							set_status(
								app.ui_mut(),
								sf!("Authenticating `{value}`… Esc to cancel"),
							);
							step = Step::Authenticating;
						},
						Err(error) => {
							open_setup_provider_step(app.ui_mut(), catalog);
							set_status(app.ui_mut(), sf!("Setup error: {error}"));
							step = Step::Provider;
						},
					}
				},
				Some(AppEvent::Changed { id, value })
					if id.as_str() == MODEL_SELECT_ID && step == Step::Model =>
				{
					let manager = omp_settings::manager::SettingsManager::open(
						omp_settings::manager::SettingsPaths::discover(data_dir, None),
					)
					.into_diagnostic()?;
					let mut roles = manager
						.snapshot()
						.project::<omp_catalog::settings::ModelSettings>()
						.into_diagnostic()?
						.get()
						.roles
						.clone();
					roles.insert(Str::new_static("default"), value.clone());
					let encoded = serde_json::to_string(&roles).into_diagnostic()?;
					manager
						.set(
							omp_settings::manager::MutationScope::Global,
							"model.roles",
							&encoded,
						)
						.await
						.into_diagnostic()?;
					break 'wizard Some(value);
				},
				Some(AppEvent::OverlayClosed(_)) => match step {
					Step::Welcome => break 'wizard None,
					Step::Prompt(_) => {
						let _ = auth.cancel();
						open_setup_provider_step(app.ui_mut(), catalog);
						set_status(app.ui_mut(), "Authentication cancelled. Choose a provider.");
						auth_location = None;
						auth_prompt_message = None;
						step = Step::Provider;
					},
					Step::Provider | Step::Model => {
						show_welcome(app.ui_mut());
						step = Step::Welcome;
					},
					Step::Authenticating => {
						let _ = auth.cancel();
						open_setup_provider_step(app.ui_mut(), catalog);
						set_status(app.ui_mut(), "Authentication cancelled. Choose a provider.");
						auth_location = None;
						auth_prompt_message = None;
						step = Step::Provider;
					},
					Step::CredentialStorageLocked => {
						show_welcome(app.ui_mut());
						step = Step::Welcome;
					},
				},
				Some(AppEvent::Key(Key::Esc)) if step == Step::Authenticating => {
					let _ = auth.cancel();
					let _ = app.ui_mut().close_top_overlay();
					open_setup_provider_step(app.ui_mut(), catalog);
					set_status(app.ui_mut(), "Authentication cancelled. Choose a provider.");
					auth_location = None;
					auth_prompt_message = None;
					step = Step::Provider;
				},
				Some(AppEvent::Key(Key::Esc)) if step == Step::CredentialStorageLocked => {
					let _ = app.ui_mut().close_top_overlay();
					show_welcome(app.ui_mut());
					step = Step::Welcome;
				},
				Some(_) => {},
			},
							event = auth.next_event() => match event {
					Some(ChatAuthEvent::Url { url, launch })
						if matches!(step, Step::Authenticating | Step::Prompt(_)) =>
					{
						auth_location = Some(AuthLocation::Url { url, launch });
						if let Step::Prompt(kind) = step {
							if let Some(message) = auth_prompt_message.clone() {
								let _ = app.ui_mut().close_top_overlay();
								show_auth_prompt(app.ui_mut(), message, kind, auth_location.as_ref());
							}
						} else if let Some(AuthLocation::Url { url, launch }) = auth_location.as_ref() {
							let display_url = launch.as_ref().unwrap_or(url);
							set_status(
								app.ui_mut(),
								sf!("[Open to authorize]({display_url}) · Esc to cancel"),
							);
						}
					},
					Some(ChatAuthEvent::DeviceCode { code, url })
					if matches!(step, Step::Authenticating | Step::Prompt(_)) =>
				{
					auth_location = Some(AuthLocation::DeviceCode { code, url });
					if let Step::Prompt(kind) = step {
						if let Some(message) = auth_prompt_message.clone() {
							let _ = app.ui_mut().close_top_overlay();
							show_auth_prompt(app.ui_mut(), message, kind, auth_location.as_ref());
						}
					} else if let Some(AuthLocation::DeviceCode { code, url }) =
						auth_location.as_ref()
					{
						set_status(
							app.ui_mut(),
							sf!("Enter code `{code}` at [{url}]({url}) · Esc to cancel"),
						);
					}
				},
				Some(ChatAuthEvent::Prompt { message, kind })
					if step == Step::Authenticating =>
				{
					auth_prompt_message = Some(message.clone());
					let _ = app.ui_mut().close_top_overlay();
					show_auth_prompt(app.ui_mut(), message, kind, auth_location.as_ref());
					step = Step::Prompt(kind);
				},
				Some(ChatAuthEvent::Notice(message))
					if matches!(step, Step::Authenticating | Step::Prompt(_)) =>
				{
					set_status(app.ui_mut(), sf!("{message} · Esc to cancel"));
				},
				Some(ChatAuthEvent::Complete(_))
					if matches!(step, Step::Authenticating | Step::Prompt(_)) =>
				{
					close_auth_scene(app.ui_mut());
					open_setup_model_step(
						app.ui_mut(),
						catalog,
						"",
						active_provider.as_deref(),
					);
					auth_location = None;
					auth_prompt_message = None;
					step = Step::Model;
				},
				Some(ChatAuthEvent::CredentialStorageLocked)
					if matches!(step, Step::Authenticating | Step::Prompt(_)) =>
				{
					if matches!(step, Step::Prompt(_)) {
						let _ = app.ui_mut().close_top_overlay();
						show_authenticating(app.ui_mut());
					}
					set_status(app.ui_mut(), CREDENTIAL_STORAGE_LOCKED_MESSAGE);
					auth_location = None;
					auth_prompt_message = None;
					step = Step::CredentialStorageLocked;
				},
				Some(ChatAuthEvent::Failed(message)) => {
					if matches!(step, Step::Authenticating | Step::Prompt(_)) {
						close_auth_scene(app.ui_mut());
						open_setup_provider_step(app.ui_mut(), catalog);
						set_status(app.ui_mut(), sf!("Setup error: {message}"));
						auth_location = None;
						auth_prompt_message = None;
						step = Step::Provider;
					} else {
						set_status(app.ui_mut(), sf!("Setup error: {message}"));
					}
				},
				Some(_) => {},
				None => break 'wizard None,
			},
		}
	};

	worker.shutdown().await;
	drop(app);
	Ok(selected)
}

fn show_welcome(ui: &mut Ui) {
	let content = Col::new()
		.with(Prop::Gap, 1_u16)
		.child(
			TextLeaf::new()
				.with(Prop::Bold, true)
				.with(Prop::Align, "center")
				.text("oh my pi"),
		)
		.child(
			TextLeaf::new()
				.with(Prop::Align, "center")
				.text("A focused coding agent for this project."),
		)
		.child(
			Button::new()
				.with(Prop::Id, CONTINUE_ID)
				.with(Prop::Align, "center")
				.child("Continue"),
		)
		.child(
			TextLeaf::new()
				.with(Prop::Dim, true)
				.with(Prop::Align, "center")
				.text("Enter continue · Ctrl+C quit"),
		);
	let card = OverlayPanel::new("Welcome").pad_y(1).child(content);
	let scene = Col::new().with(Prop::Gap, 1_u16).child(card).child(
		Markdown::new()
			.with(Prop::Id, STATUS_ID)
			.with(Prop::Align, "center")
			.text(" "),
	);
	show_scene(ui, scene);
	ui.focus_first();
}

fn show_authenticating(ui: &mut Ui) {
	let content = Markdown::new()
		.with(Prop::Id, STATUS_ID)
		.with(Prop::Align, "center")
		.text("Authenticating… Esc to cancel");
	let card = OverlayPanel::new("Provider authentication")
		.pad_y(1)
		.child(content);
	show_scene(ui, card);
}

fn open_setup_provider_step(ui: &mut Ui, catalog: &Catalog) {
	let mut providers = catalog
		.providers()
		.iter()
		.filter(|provider| provider_supports_login(catalog, provider))
		.map(|provider| (provider, provider_uses_oauth(catalog, provider)))
		.collect::<Vec<_>>();
	providers.sort_by_key(|(_, oauth)| !*oauth);
	let count = providers.len();
	let cards: Vec<ProviderCard> = providers
		.into_iter()
		.map(|(provider, _)| ProviderCard {
			press_id:    sf!("{PROVIDER_CARD_PREFIX}{}", provider.id),
			provider_id: Str::from(provider.id.as_str()),
			label:       provider.name.clone(),
		})
		.collect();
	let counter = sf!("{count} providers");
	let picker = OverlayPanel::new("Provider Login").child(provider_card_grid(
		cards,
		counter,
		"↹/←→/↑↓ pick · ↵ login · Esc back",
		18,
	));
	let scene = Col::new()
		.with(Prop::Gap, 1_u16)
		.child(picker)
		.child(status_line());
	show_scene(ui, scene);
	ui.focus_first();
}

/// Opens the model picker, scoped to `provider`'s models when it names any;
/// falls back to the full catalog for providers without model entries.
fn open_setup_model_step(ui: &mut Ui, catalog: &Catalog, current: &str, provider: Option<&str>) {
	let mut scoped: Vec<&omp_inference::ModelSpec> = match provider {
		Some(provider) => catalog
			.models()
			.iter()
			.filter(|model| {
				model
					.key
					.as_str()
					.strip_prefix(provider)
					.is_some_and(|rest| rest.starts_with('/'))
			})
			.collect(),
		None => Vec::new(),
	};
	if scoped.is_empty() {
		scoped = catalog.models().iter().collect();
	}
	let mut select = Select::new()
		.with(Prop::Id, MODEL_SELECT_ID)
		.with(Prop::Filter, true)
		.with(Prop::H, u16::try_from(scoped.len()).unwrap_or(u16::MAX).min(12));
	for model in scoped {
		let key = model.key.to_string();
		let label = if key == current {
			format!("{key} (current)")
		} else {
			key.clone()
		};
		select = select.option(
			SelectOption::new()
				.with(Prop::Value, key)
				.label(label)
				.with_str(Prop::Desc, model.display_name.as_str()),
		);
	}
	let picker = OverlayPanel::new("Choose Model").child(
		Col::new().child(select).child(panel_divider()).child(
			TextLeaf::new()
				.with(Prop::Dim, true)
				.text("Type to filter · Enter select · Esc cancel"),
		),
	);
	let scene = Col::new()
		.with(Prop::Gap, 1_u16)
		.child(picker)
		.child(status_line());
	show_scene(ui, scene);
	ui.focus_first();
}

fn show_auth_prompt(
	ui: &mut Ui,
	message: Str,
	kind: AuthPromptKind,
	location: Option<&AuthLocation>,
) {
	let placeholder = match kind {
		AuthPromptKind::Confirmation => "Press Enter to confirm",
		AuthPromptKind::OptionalSecret => "Enter optional response or press Enter to skip",
		_ => "Enter provider response",
	};
	let mut content = Col::new()
		.with(Prop::Gap, 1_u16)
		.child(TextLeaf::new().text(message));
	if let Some(location) = location {
		let (authorization, instruction) = match location {
			AuthLocation::Url { url, launch } => {
				let display_url = launch.as_ref().unwrap_or(url);
				(
					sf!("[{display_url}]({display_url})"),
					"Authorize in your browser, then paste the redirect URL below.",
				)
			},
			AuthLocation::DeviceCode { code, url } => (
				sf!("Enter code `{code}` at [{url}]({url})"),
				"Complete authorization in your browser, then continue below.",
			),
		};
		content = content
			.child(Markdown::new().text(authorization))
			.child(TextLeaf::new().with(Prop::Dim, true).text(instruction));
	}
	content = content
		.child(
			Input::new()
				.with(Prop::Id, "auth-secret")
				.with(Prop::Placeholder, placeholder)
				.with(Prop::Mask, prompt_masks_input(kind))
				.with(Prop::Submit, true),
		)
		.child(panel_divider())
		.child(
			TextLeaf::new()
				.with(Prop::Dim, true)
				.text("Enter submit · Esc cancel"),
		);
	let prompt = OverlayPanel::new("Provider Authentication").child(content);
	let scene = Col::new()
		.with(Prop::Gap, 1_u16)
		.child(prompt)
		.child(status_line());
	show_scene(ui, scene);
	ui.focus_first();
}

fn provider_supports_login(catalog: &Catalog, provider: &ProviderDef) -> bool {
	provider
		.auth
		.iter()
		.filter_map(|auth_id| catalog.auth_spec(auth_id))
		.any(|auth| auth.kind != AuthSpecKind::None)
}

fn provider_uses_oauth(catalog: &Catalog, provider: &ProviderDef) -> bool {
	provider.auth.iter().any(|auth_id| {
		catalog
			.auth_spec(auth_id)
			.and_then(|auth| auth.oauth.as_ref())
			.is_some_and(|oauth_id| catalog.oauth_spec(oauth_id).is_some())
	})
}

fn show_scene(ui: &mut Ui, scene: impl omp_tui::IntoComponent) {
	ui.show_overlay(
		scene,
		OverlayOptions::default()
			.anchor(OverlayAnchor::Center)
			.width(Dim::Pct(60))
			.min_width(40)
			.max_height(Dim::Pct(60))
			.margin(OverlayMargin::uniform(1))
			.min_viewport(Size::new(24, 6)),
	);
}

fn close_auth_scene(ui: &mut Ui) {
	let _ = ui.close_top_overlay();
}

fn status_line() -> Markdown {
	Markdown::new()
		.with(Prop::Id, STATUS_ID)
		.with(Prop::Align, "center")
		.text(" ")
}

fn set_status(ui: &mut Ui, message: impl IntoStr) {
	ui.set_text(STATUS_ID, message.into_str());
}

async fn has_active_account(registry: &InferenceRegistry) -> miette::Result<bool> {
	let provider = registry
		.catalog()
		.providers()
		.first()
		.map(|provider| ProviderId::from(provider.id.as_str()))
		.ok_or_else(|| miette!("embedded catalog has no providers"))?;
	let planner = Router::new(registry.clone(), Duration::from_secs(30));
	let meta = CallMeta {
		id:       RequestId::from(format!("wizard-auth-{}", omp_core::Ulid::generate())),
		target:   Target::ProviderService(provider),
		deadline: None,
		budget:   ExecutionBudget::default(),
		session:  None,
	};
	let mut client = Client::new(registry.service(), planner, meta);
	let answer = client
		.execute(AuthRequest::ListAccounts { provider: None })
		.await
		.into_diagnostic()?;
	let AuthAnswer::Accounts(accounts) = answer else {
		return Err(miette!("account listing returned an unexpected response"));
	};
	Ok(accounts
		.iter()
		.any(|account| account.state == AccountState::Active))
}
