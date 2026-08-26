//! Durable approval-ticket presentation and exact-once action routing.

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, UiEvent, dom,
};

use crate::{ApprovalAction, ApprovalTicketView, OverlayPanel, panel_divider};

const DIALOG_WIDTH: u16 = 78;
const SUBJECT_MARGIN: u16 = 10;

/// Result of routing input through an approval dialog.
pub enum ApprovalEvent {
	/// Event consumed while the dialog remains open.
	Consumed,
	/// The user selected a terminal action.
	Decide(ApprovalAction),
	/// The user requested an amended subject.
	Amend,
}

/// Centered durable approval dialog.
pub struct ApprovalOverlay {
	ticket:  ApprovalTicketView,
	ui:      Ui,
	options: OverlayOptions,
	width:   u16,
}

impl ApprovalOverlay {
	/// Opens one pending ticket with all merged reasons visible.
	pub fn open(ticket: ApprovalTicketView, ctx: &UiContext) -> Self {
		let width = DIALOG_WIDTH;
		let ui = build(&ticket, width, ctx);
		Self {
			ticket,
			ui,
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.z(40),
			width,
		}
	}

	/// Returns the stable durable ticket identity.
	pub fn ticket_id(&self) -> &Str {
		&self.ticket.ticket_id
	}

	/// Borrows the complete durable ticket projection.
	pub const fn ticket(&self) -> &ApprovalTicketView {
		&self.ticket
	}

	/// Routes one key through the dialog.
	pub fn handle_key(&mut self, key: Key) -> ApprovalEvent {
		let event = self.ui.handle_key(key);
		Self::route(event)
	}

	/// Routes pasted input through the dialog.
	pub fn handle_paste(&mut self, text: &str) -> ApprovalEvent {
		let event = self.ui.handle_paste(text);
		Self::route(event)
	}

	/// Routes pointer input; an outside click leaves the ticket pending.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> ApprovalEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => Self::route(event),
			None if kind == Mouse::Click => ApprovalEvent::Consumed,
			None => ApprovalEvent::Consumed,
		}
	}

	/// Returns the centered composited layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = self.width.min(viewport.width);
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn route(event: UiEvent) -> ApprovalEvent {
		match event {
			UiEvent::Cancel => ApprovalEvent::Consumed,
			UiEvent::Changed { value, .. } => match value.as_str() {
				"once" => ApprovalEvent::Decide(ApprovalAction::AllowOnce),
				"always" => ApprovalEvent::Decide(ApprovalAction::AllowAlways),
				"reject" => ApprovalEvent::Decide(ApprovalAction::Reject),
				"amend" => ApprovalEvent::Amend,
				_ => ApprovalEvent::Consumed,
			},
			UiEvent::None
			| UiEvent::Submit
			| UiEvent::Filtered { .. }
			| UiEvent::Highlighted { .. }
			| UiEvent::Pressed(_)
			| UiEvent::Copied(_)
			| UiEvent::TreeActivated { .. }
			| UiEvent::TreeToggled { .. }
			| UiEvent::TreeAction { .. }
			| UiEvent::DiffAction { .. } => ApprovalEvent::Consumed,
		}
	}
}

fn build(ticket: &ApprovalTicketView, width: u16, ctx: &UiContext) -> Ui {
	let subject_width = usize::from(width.saturating_sub(SUBJECT_MARGIN));
	let subject = middle_elide(ticket.subject.as_str(), subject_width);
	let detail = ticket.detail.clone();
	let evidence = if ticket.evidence.is_empty() {
		sf!("No additional policy evidence")
	} else {
		Str::new(
			ticket
				.evidence
				.iter()
				.map(Str::as_str)
				.collect::<Vec<_>>()
				.join(" · "),
		)
	};
	let always_detail = ticket.always_scope.as_ref().map_or_else(
		|| sf!("Persist the narrowest offered policy scope"),
		|scope| sf!("Persist scope: {scope}"),
	);
	Ui::from_root(
		OverlayPanel::new(sf!("Approval · {}", ticket.title)).child(dom! {
			<col>
				<text bold>{subject}</text>
				<markdown>{detail}</markdown>
				<text dim>{evidence}</text>
				{panel_divider()}
				<select id="approval" h={4u16}>
					<option value="once" label="Allow once"><text dim>{"Approve only this exact invocation"}</text></option>
					<option value="always" label="Always allow"><text dim>{always_detail}</text></option>
					<option value="amend" label="Amend"><text dim>{"Edit the exact command or subject, then approve once"}</text></option>
					<option value="reject" label="Reject"><text dim>{"Deny this invocation"}</text></option>
				</select>
				{panel_divider()}
				<text dim>{"Enter choose · Esc keep pending"}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

/// Elides the middle of an exact command while preserving both
/// authority-bearing ends.
pub fn middle_elide(text: &str, max_chars: usize) -> Str {
	if text.chars().count() <= max_chars {
		return Str::new(text);
	}
	if max_chars <= 3 {
		return Str::new("...".chars().take(max_chars).collect::<String>());
	}
	let kept = max_chars - 3;
	let left = kept.div_ceil(2);
	let right = kept / 2;
	let prefix: String = text.chars().take(left).collect();
	let suffix: String = text
		.chars()
		.rev()
		.take(right)
		.collect::<String>()
		.chars()
		.rev()
		.collect();
	Str::new(format!("{prefix}...{suffix}"))
}
#[cfg(test)]
mod tests {
	use omp_tui::{Key, Mouse, Size, UiContext};

	use super::{ApprovalEvent, ApprovalOverlay};
	use crate::ApprovalTicketView;

	fn ticket() -> ApprovalTicketView {
		ApprovalTicketView {
			ticket_id:     "ticket".into(),
			invocation_id: Some("invocation".into()),
			title:         "Approval".into(),
			detail:        "Policy detail".into(),
			subject:       "bash command".into(),
			always_scope:  None,
			evidence:      Vec::new(),
		}
	}

	#[test]
	fn escape_and_outside_click_keep_durable_ticket_mounted() {
		let mut overlay = ApprovalOverlay::open(ticket(), &UiContext::default());
		assert!(matches!(overlay.handle_key(Key::Esc), ApprovalEvent::Consumed));
		assert!(matches!(
			overlay.handle_mouse(0, 0, Mouse::Click, Size::new(120, 40)),
			ApprovalEvent::Consumed
		));
	}
}
