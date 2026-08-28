//! Durable approval-ticket presentation and exact-once action routing.

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, UiEvent, dom,
};

use crate::{ApprovalAction, ApprovalTicketView, OverlayPanel, panel_divider};

const DIALOG_WIDTH: u16 = 78;
const SUBJECT_MARGIN: u16 = 10;
#[derive(Clone, Copy)]
enum ApprovalChoiceAction {
	AllowOnce,
	AllowAlways,
	Amend,
	Reject,
}

#[derive(Clone, Copy)]
enum ApprovalChoiceDetail {
	Static(&'static str),
	AlwaysScope,
}

#[derive(Clone, Copy)]
struct ApprovalChoice {
	value:  &'static str,
	label:  &'static str,
	key:    char,
	help:   &'static str,
	detail: ApprovalChoiceDetail,
	action: ApprovalChoiceAction,
}

const APPROVAL_CHOICES: [ApprovalChoice; 4] = [
	ApprovalChoice {
		value:  "once",
		label:  "Allow once",
		key:    '1',
		help:   "Once",
		detail: ApprovalChoiceDetail::Static("Approve only this exact invocation"),
		action: ApprovalChoiceAction::AllowOnce,
	},
	ApprovalChoice {
		value:  "always",
		label:  "Always allow",
		key:    '2',
		help:   "always",
		detail: ApprovalChoiceDetail::AlwaysScope,
		action: ApprovalChoiceAction::AllowAlways,
	},
	ApprovalChoice {
		value:  "amend",
		label:  "Amend",
		key:    '3',
		help:   "amend",
		detail: ApprovalChoiceDetail::Static("Edit the exact command or subject, then approve once"),
		action: ApprovalChoiceAction::Amend,
	},
	ApprovalChoice {
		value:  "reject",
		label:  "Reject",
		key:    '4',
		help:   "reject",
		detail: ApprovalChoiceDetail::Static("Deny this invocation"),
		action: ApprovalChoiceAction::Reject,
	},
];
/// Iterates approval shortcuts and their action labels in dispatch order.
pub fn approval_hotkeys() -> impl ExactSizeIterator<Item = (char, &'static str)> + Clone {
	APPROVAL_CHOICES
		.iter()
		.map(|choice| (choice.key, choice.help))
}

impl ApprovalChoice {
	fn event(self) -> ApprovalEvent {
		match self.action {
			ApprovalChoiceAction::AllowOnce => ApprovalEvent::Decide(ApprovalAction::AllowOnce),
			ApprovalChoiceAction::AllowAlways => ApprovalEvent::Decide(ApprovalAction::AllowAlways),
			ApprovalChoiceAction::Amend => ApprovalEvent::Amend,
			ApprovalChoiceAction::Reject => ApprovalEvent::Decide(ApprovalAction::Reject),
		}
	}

	fn detail(self, ticket: &ApprovalTicketView) -> Str {
		match self.detail {
			ApprovalChoiceDetail::Static(detail) => Str::new(detail),
			ApprovalChoiceDetail::AlwaysScope => ticket.always_scope.as_ref().map_or_else(
				|| sf!("Persist the narrowest offered policy scope"),
				|scope| sf!("Persist scope: {scope}"),
			),
		}
	}
}

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
			UiEvent::Changed { value, .. } => APPROVAL_CHOICES
				.iter()
				.find(|choice| choice.value == value.as_str())
				.map_or(ApprovalEvent::Consumed, |choice| choice.event()),
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

fn choice_rows(ticket: &ApprovalTicketView) -> Vec<(Str, Str, Str)> {
	APPROVAL_CHOICES
		.iter()
		.map(|choice| {
			(Str::new(choice.value), sf!("{}. {}", choice.key, choice.label), choice.detail(ticket))
		})
		.collect()
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
	let choices = choice_rows(ticket);
	Ui::from_root(
		OverlayPanel::new(sf!("Approval · {}", ticket.title)).child(dom! {
			<col>
				<text bold>{subject}</text>
				<markdown>{detail}</markdown>
				<text dim>{evidence}</text>
				{panel_divider()}
				<select id="approval" h={4u16}>
					for (value, label, detail) in choices {
						<option value={value} label={label.clone()}>
							<text dim>{detail}</text>
						</option>
					}
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

	use super::{ApprovalEvent, ApprovalOverlay, choice_rows};
	use crate::{ApprovalAction, ApprovalTicketView};

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
	#[test]
	fn numbered_shortcuts_route_to_the_documented_actions() {
		let ctx = UiContext::default();

		let mut overlay = ApprovalOverlay::open(ticket(), &ctx);
		assert!(matches!(
			overlay.handle_key(Key::Char('1')),
			ApprovalEvent::Decide(ApprovalAction::AllowOnce)
		));
		let mut overlay = ApprovalOverlay::open(ticket(), &ctx);
		assert!(matches!(
			overlay.handle_key(Key::Char('2')),
			ApprovalEvent::Decide(ApprovalAction::AllowAlways)
		));
		let mut overlay = ApprovalOverlay::open(ticket(), &ctx);
		assert!(matches!(overlay.handle_key(Key::Char('3')), ApprovalEvent::Amend));
		let mut overlay = ApprovalOverlay::open(ticket(), &ctx);
		assert!(matches!(
			overlay.handle_key(Key::Char('4')),
			ApprovalEvent::Decide(ApprovalAction::Reject)
		));
	}

	#[test]
	fn numbered_labels_keep_choice_order_and_dynamic_always_scope() {
		let mut ticket = ticket();
		ticket.always_scope = Some("command prefix `cargo test`".into());
		let rows = choice_rows(&ticket);
		assert_eq!(
			rows
				.iter()
				.map(|(_, label, _)| label.as_str())
				.collect::<Vec<_>>(),
			["1. Allow once", "2. Always allow", "3. Amend", "4. Reject"]
		);
		assert_eq!(rows[1].2, "Persist scope: command prefix `cargo test`");
	}
}
