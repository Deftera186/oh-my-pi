//! Typed cards for approval resolution tools.

use omp_tui::{IntoComponent as _, UiContext, dom};

use super::{Card, CardView, Component};

/// Renders accepted approval resolutions.
pub struct ResolveCard;
/// Renders rejected approval resolutions.
pub struct RejectCard;

impl Card for ResolveCard {
	fn tool(&self) -> &'static str {
		"resolve"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		render_resolution(view, expanded, ui)
	}
}
impl Card for RejectCard {
	fn tool(&self) -> &'static str {
		"reject"
	}

	fn render(&self, view: &CardView<'_>, expanded: bool, ui: &UiContext) -> Component {
		render_resolution(view, expanded, ui)
	}
}

fn render_resolution(view: &CardView<'_>, _expanded: bool, _ui: &UiContext) -> Component {
	match view.status.as_str() {
		"ok" => dom! {
			<col gap=1>
				<row gap=1 pad-x=1><i:resolve/><text>{"Accept: pending action"}</text></row>
				<text pad-x=1>{"No reason provided"}</text>
			</col>
		}
		.into_component(),
		"error" => dom! {
			<col gap=1>
				<row gap=1 pad-x=1><i:error/><text>{"Failed: pending action"}</text></row>
				<text pad-x=1>{"No reason provided"}</text>
			</col>
		}
		.into_component(),
		_ => dom! { <row gap=1><i:pending/><text>{"Resolve ⟨proposed -> rejected⟩"}</text></row> }
			.into_component(),
	}
}
