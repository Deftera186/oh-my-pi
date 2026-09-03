//! Centered scrollable report panel: the one presentation for slash
//! commands that answer with a multi-line markdown report (`/tools`,
//! `/security`, `/hotkeys`, `/changelog`, `/context`, …). pi renders these
//! as custom transcript messages; on this host they are observer-local
//! panels (ADR 0005) until the local-block seam lands, so they never enter
//! the journal either way.

use omp_core::Str;
use omp_tui::{Frame, Key, Prop, Size, Ui, UiContext, UiEvent, dom};

use super::{Panel, PanelAnchor, PanelEvent};

const HINT: &str = "↑/↓ scroll · PgUp/PgDn page · Esc close";
/// Border, title rule, hint, and blank rows around the scroll pane.
const CHROME_ROWS: u16 = 5;

/// Retained markdown report with a scroll pane.
pub struct ReportPanel {
	id:    &'static str,
	title: Str,
	body:  Str,
	ui:    Ui,
	ctx:   UiContext,
	width: u16,
	rows:  u16,
}

impl ReportPanel {
	/// Builds a report titled `title` over markdown `body`.
	#[must_use]
	pub fn new(id: &'static str, title: impl Into<Str>, body: impl Into<Str>, ctx: &UiContext) -> Self {
		let mut panel = Self {
			id,
			title: title.into(),
			body: body.into(),
			ui: Ui::from_root(dom! { <col/> }, 80, ctx.clone()),
			ctx: ctx.clone(),
			width: 0,
			rows: 0,
		};
		panel.rebuild(80, 20);
		panel
	}

	/// Replaces the body (live reports re-render in place).
	pub fn set_body(&mut self, body: impl Into<Str>) {
		self.body = body.into();
		self.rebuild(self.width, self.rows);
	}

	/// Report body as shown.
	#[must_use]
	pub fn body(&self) -> &str {
		&self.body
	}

	fn rebuild(&mut self, width: u16, rows: u16) {
		self.width = width;
		self.rows = rows;
		let title = self.title.clone();
		let body = self.body.clone();
		let tree = dom! {
			<box border=round title={title} pad-x=1>
				<col>
					<scroll id="report" h={rows} focus>
						<md>{body}</md>
					</scroll>
					<hr border=round/>
					<text fg=muted truncate>{HINT}</text>
				</col>
			</box>
		};
		self.ui = Ui::from_root(tree, width, self.ctx.clone());
	}
}

impl Panel for ReportPanel {
	fn id(&self) -> &'static str {
		self.id
	}

	fn anchor(&self) -> PanelAnchor {
		PanelAnchor::Center
	}

	fn key(&mut self, key: Key) -> PanelEvent {
		match key {
			Key::Esc | Key::Char('q') => PanelEvent::Close,
			_ => match self.ui.handle_key(key) {
				UiEvent::Cancel => PanelEvent::Close,
				_ => PanelEvent::Consumed,
			},
		}
	}

	fn frame(&mut self, viewport: Size) -> &Frame {
		let rows = viewport.height.saturating_sub(CHROME_ROWS).max(3);
		if viewport.width != self.width {
			self.rebuild(viewport.width, rows);
		} else if rows != self.rows {
			self.rows = rows;
			self.ui.set_prop("report", Prop::H, rows);
		}
		self.ui.frame()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn report_shows_title_body_and_hint_and_esc_closes() {
		let ctx = UiContext::default();
		let mut panel = ReportPanel::new("tools", "Tools", "- **read** · reads files", &ctx);
		let frame = panel.frame(Size { width: 60, height: 12 });
		let text = omp_tui::frame_text(frame);
		assert!(text.contains("Tools"), "title missing:\n{text}");
		assert!(text.contains("read"), "body missing:\n{text}");
		assert!(text.contains("Esc close"), "hint missing:\n{text}");
		assert_eq!(panel.key(Key::Esc), PanelEvent::Close);
		assert_eq!(panel.key(Key::Down), PanelEvent::Consumed);
	}
}
