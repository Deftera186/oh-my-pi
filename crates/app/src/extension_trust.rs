//! Core-owned interactive extension grant dialog.

use std::{path::Path, sync::Arc, time::SystemTime};

use miette::{IntoDiagnostic as _, miette};
use omp_agent::{
	ApprovalBook, ApprovalDecision, ApprovalRoute, ApprovalSource, ApprovalSpec, TicketState,
};
use omp_chat_ui::OverlayPanel;
use omp_core::{Str, sf};
use omp_driver::discovery::ExtensionGrantRequest;
use omp_ext::trust::{Grant, GrantsFile};
use omp_tui::{
	AppEvent, AppOptions, Dim, Key, OverlayAnchor, OverlayMargin, OverlayOptions, Prop, Size, Ui,
	components::{Button, Col, Markdown, Shader, TextLeaf},
	shader::Eclipse,
};
use strum::IntoStaticStr;

const ALLOW_ONCE: &str = "extension-trust-once";
const ALLOW_SESSION: &str = "extension-trust-session";
const ALLOW_PERSIST: &str = "extension-trust-persist";
const ALLOW_SUBTREE: &str = "extension-trust-subtree";
const DENY: &str = "extension-trust-deny";
/// Completed interactive admission decisions carried into the first session
/// journal before the first agent turn.
pub struct PromptOutcome {
	/// Grants valid for the current process session.
	pub session_grants: Vec<Grant>,
	/// Core-owned terminal tickets to append to the durable session journal.
	pub tickets:        Vec<omp_agent::ApprovalTicket>,
}

/// Presents each pending install grant through a native, Core-owned approval
/// ticket and returns the grants admitted for this session.
#[expect(clippy::future_not_send, reason = "the trust dialog owns a thread-confined omp_tui::App")]
pub async fn prompt(
	requests: &[ExtensionGrantRequest],
	grant_path: &Path,
) -> miette::Result<PromptOutcome> {
	if requests.is_empty() {
		return Ok(PromptOutcome { session_grants: Vec::new(), tickets: Vec::new() });
	}
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
	let book = Arc::new(ApprovalBook::with_prefix("extension-approval"));
	let (route, inbox) = ApprovalRoute::new(book, None);
	let mut session = Vec::new();
	let mut tickets = Vec::with_capacity(requests.len());
	for request in requests {
		let spec = approval_spec(request);
		let route = route.clone();
		let filed = tokio::spawn(async move { route.request(None, vec![spec], now_ms()).await });
		let pending = inbox.recv().await.into_diagnostic()?;
		show_dialog(app.ui_mut(), request, &pending.ticket);
		let action = loop {
			match app.next().await.into_diagnostic()? {
				Some(AppEvent::Pressed(id)) if id.as_str() == ALLOW_ONCE => break GrantAction::Once,
				Some(AppEvent::Pressed(id)) if id.as_str() == ALLOW_SESSION => {
					break GrantAction::Session;
				},
				Some(AppEvent::Pressed(id)) if id.as_str() == ALLOW_PERSIST => {
					break GrantAction::Persist;
				},
				Some(AppEvent::Pressed(id))
					if id.as_str() == ALLOW_SUBTREE && request.grant.workspace.is_some() =>
				{
					break GrantAction::Subtree;
				},
				Some(AppEvent::Pressed(id)) if id.as_str() == DENY => break GrantAction::Deny,
				Some(AppEvent::Key(Key::Esc)) | None => break GrantAction::Deny,
				_ => {},
			}
		};
		let decision = action.decision();
		pending
			.respond(decision)
			.map_err(|_| miette!("extension approval ticket was already settled"))?;
		let ticket = filed.await.into_diagnostic()?;
		if ticket.state != TicketState::Decided {
			return Err(miette!("extension approval ticket did not reach a decision"));
		}
		let _ = app.ui_mut().close_top_overlay();
		tickets.push(ticket);
		match action {
			GrantAction::Once => session.push(completed_grant(
				&request.grant,
				omp_ext::trust::GrantDuration::Once,
				omp_ext::trust::GrantScope::Exact,
			)),
			GrantAction::Session => session.push(completed_grant(
				&request.grant,
				omp_ext::trust::GrantDuration::Session,
				omp_ext::trust::GrantScope::Exact,
			)),
			GrantAction::Persist | GrantAction::Subtree => {
				let scope = if action == GrantAction::Subtree {
					omp_ext::trust::GrantScope::Subtree
				} else {
					omp_ext::trust::GrantScope::Exact
				};
				let grant =
					completed_grant(&request.grant, omp_ext::trust::GrantDuration::Persistent, scope);
				GrantsFile::persist(grant_path, grant.clone()).into_diagnostic()?;
				session.push(grant);
			},
			GrantAction::Deny => {},
		}
	}
	Ok(PromptOutcome { session_grants: session, tickets })
}

#[derive(Clone, Copy, Debug, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "snake_case")]
enum GrantAction {
	Once,
	Session,
	Persist,
	Subtree,
	Deny,
}

impl GrantAction {
	fn decision(self) -> ApprovalDecision {
		let approved = self != Self::Deny;
		ApprovalDecision {
			approved,
			scope: Str::new_static(self.into()),
			source: ApprovalSource::User,
			decided_by: None,
			reason: (!approved).then(|| sf!("extension grant denied by user")),
			audited: false,
		}
	}
}

fn approval_spec(request: &ExtensionGrantRequest) -> ApprovalSpec {
	let requested = joined(request.requested_capabilities.as_ref(), "none");
	let granted = joined(request.granted_capabilities.as_ref(), "none");
	ApprovalSpec {
		title:         sf!("Approve extension {}", request.grant.id),
		body:          sf!(
			"Publisher: `{}`\n\nRequested capabilities: {}\n\nCurrently granted: {}",
			request.grant.publisher,
			requested,
			granted
		),
		subject:       request.grant.id.clone(),
		kind:          sf!("extension_trust"),
		scopes:        vec![sf!("once"), sf!("session"), sf!("persist"), sf!("subtree")],
		default:       Some(false),
		route:         sf!("local"),
		approver:      None,
		timeout_ms:    0,
		unreachable:   sf!("deny"),
		require_human: true,
		pattern:       None,
		evidence:      request
			.requested_capabilities
			.iter()
			.map(|capability| sf!("requested:{capability}"))
			.chain(
				request
					.granted_capabilities
					.iter()
					.map(|capability| sf!("granted:{capability}")),
			)
			.collect(),
	}
}

fn joined(capabilities: &[Str], fallback: &'static str) -> String {
	if capabilities.is_empty() {
		return fallback.to_owned();
	}
	capabilities
		.iter()
		.map(|capability| format!("`{capability}`"))
		.collect::<Vec<_>>()
		.join(", ")
}

fn completed_grant(
	template: &Grant,
	duration: omp_ext::trust::GrantDuration,
	scope: omp_ext::trust::GrantScope,
) -> Grant {
	Grant {
		granted_at: Str::new(jiff::Timestamp::now().to_string()),
		granted_by: sf!("interactive"),
		duration,
		scope,
		..template.clone()
	}
}

fn show_dialog(ui: &mut Ui, request: &ExtensionGrantRequest, ticket: &omp_agent::ApprovalTicket) {
	let reason = ticket
		.reasons
		.first()
		.expect("extension ticket has one Core-owned reason");
	let mut actions = Col::new()
		.with(Prop::Gap, 1_u16)
		.child(Button::new().with(Prop::Id, ALLOW_ONCE).child("Allow once"))
		.child(
			Button::new()
				.with(Prop::Id, ALLOW_SESSION)
				.child("Allow for this session"),
		)
		.child(
			Button::new()
				.with(Prop::Id, ALLOW_PERSIST)
				.child("Allow and remember"),
		);
	if request.grant.workspace.is_some() {
		actions = actions.child(
			Button::new()
				.with(Prop::Id, ALLOW_SUBTREE)
				.child("Trust this folder and everything under it"),
		);
	}
	let actions = actions.child(Button::new().with(Prop::Id, DENY).child("Deny"));
	let content = Col::new()
		.with(Prop::Gap, 1_u16)
		.child(Markdown::new().text(reason.body.clone()))
		.child(actions)
		.child(
			TextLeaf::new()
				.with(Prop::Dim, true)
				.text(sf!("Extension code has not started · Esc denies · {}", request.grant.layer)),
		);
	ui.show_overlay(
		OverlayPanel::new(reason.title.clone()).child(content),
		OverlayOptions::default()
			.anchor(OverlayAnchor::Center)
			.width(Dim::Pct(65))
			.min_width(48)
			.max_height(Dim::Pct(70))
			.margin(OverlayMargin::uniform(1))
			.min_viewport(Size::new(24, 8)),
	);
	ui.focus_first();
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(SystemTime::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

#[cfg(test)]
mod tests {
	use omp_ext::{Layer, TrustTier};

	use super::*;

	fn request() -> ExtensionGrantRequest {
		ExtensionGrantRequest {
			grant:                  Grant {
				id:                sf!("acme.reviewer"),
				publisher:         sf!("ed25519:publisher"),
				layer:             Layer::Client,
				workspace:         None,
				scope:             omp_ext::trust::GrantScope::Exact,
				capability_digest: sf!("b3:capabilities"),
				tier:              TrustTier::Sandboxed,
				ship:              sf!("installed"),
				granted_at:        sf!(""),
				granted_by:        sf!("interactive"),
				duration:          omp_ext::trust::GrantDuration::Persistent,
			},
			requested_capabilities: Arc::from([sf!("env.fs.read"), sf!("env.exec")]),
			granted_capabilities:   Arc::from([sf!("env.fs.read")]),
		}
	}

	#[test]
	fn core_ticket_carries_identity_publisher_and_capability_diff() {
		let spec = approval_spec(&request());
		assert_eq!(spec.kind.as_str(), "extension_trust");
		assert_eq!(spec.subject.as_str(), "acme.reviewer");
		assert!(spec.require_human);
		assert!(spec.body.contains("ed25519:publisher"));
		assert!(spec.body.contains("env.exec"));
		assert!(spec.body.contains("Currently granted"));
		assert_eq!(spec.scopes, [sf!("once"), sf!("session"), sf!("persist"), sf!("subtree")]);
	}

	#[test]
	fn actions_map_to_once_persist_and_deny_ticket_decisions() {
		let once = GrantAction::Once.decision();
		assert!(once.approved);
		assert_eq!(once.scope.as_str(), "once");
		let persist = GrantAction::Persist.decision();
		assert!(persist.approved);
		assert_eq!(persist.scope.as_str(), "persist");
		let session = GrantAction::Session.decision();
		assert!(session.approved);
		assert_eq!(session.scope.as_str(), "session");
		let subtree = GrantAction::Subtree.decision();
		assert!(subtree.approved);
		assert_eq!(subtree.scope.as_str(), "subtree");
		let deny = GrantAction::Deny.decision();
		assert!(!deny.approved);
		assert_eq!(deny.source, ApprovalSource::User);
	}
}
