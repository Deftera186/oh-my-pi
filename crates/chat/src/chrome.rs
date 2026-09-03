//! Boot chrome: the welcome banner, the composer status band, and the
//! composer shell. Every glyph and color comes from the ambient
//! [`UiContext`](omp_tui::UiContext); the shapes follow pi's `welcome.ts`
//! and the status-band composer.

use omp_core::Str;
use omp_tui::{
	Prop, Ui, UiContext,
	components::{Col, ComposerStyle, EditorPane, KeywordAccent, Spacer},
};

pub use crate::{
	status_band::{PathLabel, StatusBand, StatusFacts, display_path},
	welcome::{Welcome, tip_for},
};

/// Element id of the composer editor inside the chrome tree.
pub const COMPOSER_ID: &str = "composer";
/// Element id of the status band inside the chrome tree.
pub const STATUS_ID: &str = "status-band";
/// Element id of the one-row gap above the composer (pi `EditorTopGap`).
pub const GAP_ID: &str = "composer-gap";
/// Composer placeholder shared with the gallery composer previews.
pub const PLACEHOLDER: &str = "Ask anything, edit files, run tools";

/// Model facts the host learns once at launch and never journals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelBadge {
	/// Canonical `provider/model` identifier the session was launched with.
	pub identifier:     Str,
	/// Human-readable model name (catalog display name).
	pub name:           Str,
	/// Provider identifier.
	pub provider:       Str,
	/// Total context window in tokens when the catalog knows it.
	pub context_window: Option<u64>,
	/// Whether the model can reason (the band then shows the thinking level).
	pub reasoning:      bool,
}

impl ModelBadge {
	/// Derives a badge from a `provider/model` identifier when no catalog
	/// record is available.
	#[must_use]
	pub fn from_identifier(identifier: &str) -> Self {
		let (provider, name) = identifier.split_once('/').unwrap_or(("", identifier));
		Self {
			identifier:     Str::new(identifier),
			name:           Str::new(name),
			provider:       Str::new(provider),
			context_window: None,
			reasoning:      false,
		}
	}

	/// Model label for the status band: pi drops the `Claude ` prefix
	/// (`status-line/segments.ts` `modelSegment`).
	#[must_use]
	pub fn short_name(&self) -> Str {
		match self.name.as_str().strip_prefix("Claude ") {
			Some(short) => Str::new(short),
			None => self.name.clone(),
		}
	}
}

/// pi `EditorTopGap`: the one-row margin above the editor stays for every
/// shape except the band (omp's [`ComposerStyle::Borderless`]), whose status
/// band is designed to sit flush under an occupied status row — the notice
/// row the host paints directly above the composer. An empty status row
/// keeps the gap so the band never sits flush against the transcript.
#[must_use]
pub const fn top_gap_shown(shape: ComposerStyle, status_row_occupied: bool) -> bool {
	!matches!(shape, ComposerStyle::Borderless) || !status_row_occupied
}

/// Builds the composer chrome tree: the top-gap row, then the editor in
/// `shape` with its status band above the prompt and pi's magic-keyword
/// shimmer. Mount it with [`composer_ui`], which applies the gap rule.
#[must_use]
pub fn composer_root(facts: StatusFacts, shape: ComposerStyle) -> Col {
	let editor = EditorPane::new()
		.composer_style(shape)
		.keyword_accent(KeywordAccent::magic())
		.with(Prop::Id, COMPOSER_ID)
		.with(Prop::Submit, true)
		.with(Prop::Placeholder, PLACEHOLDER)
		.status(StatusBand::new(facts));
	Col::new()
		.child(Spacer::new().with(Prop::Id, GAP_ID))
		.child(editor)
}

/// Mounts [`composer_root`] as a retained tree at `width`, showing the top
/// gap per [`top_gap_shown`] for an unoccupied status row.
#[must_use]
pub fn composer_ui(facts: StatusFacts, shape: ComposerStyle, width: u16, ctx: UiContext) -> Ui {
	let mut ui = Ui::from_root(composer_root(facts, shape), width, ctx);
	ui.set_visible(GAP_ID, top_gap_shown(shape, false));
	ui
}

#[cfg(test)]
mod tests {
	use omp_tui::frame_text;

	use super::*;
	use crate::status_band::tests::facts;

	/// At rest (no notice above), pi's `EditorTopGap` keeps the blank row
	/// above the band, as in the boot reference capture.
	#[test]
	fn composer_root_paints_status_then_prompt_gutter() {
		let mut ui = composer_ui(
			StatusFacts { tokens: 0, ..facts() },
			ComposerStyle::Borderless,
			80,
			UiContext::default(),
		);
		ui.focus_first();
		let rows = frame_text(ui.frame())
			.lines()
			.map(str::to_owned)
			.collect::<Vec<_>>();
		assert_eq!(rows[0].trim(), "");
		assert!(rows[1].starts_with(" π  >"), "{}", rows[1]);
		assert!(rows[2].starts_with("╰─ Ask anything"), "{}", rows[2]);
		assert_eq!(ui.frame().cursor(), Some((3, 2)), "caret sits after the prompt gutter");
	}

	/// pi `EditorTopGap`: only the band over an occupied status row sits
	/// flush; every other shape keeps the one-row gap regardless.
	#[test]
	fn top_gap_collapses_only_for_the_band_over_an_occupied_status_row() {
		assert!(!top_gap_shown(ComposerStyle::Borderless, true));
		assert!(top_gap_shown(ComposerStyle::Borderless, false));
		assert!(top_gap_shown(ComposerStyle::Rail, true));
		assert!(top_gap_shown(ComposerStyle::Box, true));

		let mut ui = composer_ui(facts(), ComposerStyle::Rail, 80, UiContext::default());
		ui.focus_first();
		let rows = frame_text(ui.frame())
			.lines()
			.map(str::to_owned)
			.collect::<Vec<_>>();
		assert_eq!(rows[0].trim(), "", "rail shape keeps the top gap");
		assert_ne!(rows[1].trim(), "");
	}
}
