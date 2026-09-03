//! Typed card for `computer@1`.

use omp_tui::{IntoComponent as _, UiContext, dom};

use super::{Card, CardStatus, CardView, Component};

/// Native-desktop status card.
pub struct ComputerCard;

impl Card for ComputerCard {
	fn tool(&self) -> &'static str {
		"computer"
	}

	fn render(&self, view: &CardView<'_>, _expanded: bool, _ui: &UiContext) -> Component {
		let _input = view.input::<omp_tools::computer::Params>();
		let _result = view.result::<omp_tools::computer::Payload>();
		let _fault = view.fault::<omp_tools::computer::Fault>();
		dom! {
			<col>
				<text fg=muted>{" "}</text>
				<row gap=1>
					match view.status {
						CardStatus::StreamingArgs | CardStatus::InProgress => <i:pending/>,
						CardStatus::Done => <i:success/>,
						CardStatus::Failed => <i:error/>,
					}
					<text>{if view.status == CardStatus::Failed { "Computer: error" } else { "Computer" }}</text>
				</row>
				<text fg=muted>{" "}</text>
			</col>
		}
		.into_component()
	}
}
