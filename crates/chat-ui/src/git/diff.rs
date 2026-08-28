//! Git diff-pane chrome and workbench composition.

use std::borrow::Cow;

use omp_core::{Str, sf};
use omp_tui::{DiffPane, DiffWhitespaceMode, Icon, components::Col, dom};

use super::{Focus, GitArea, GitWorkbench, split_path};

pub(super) const DIFF_ID: &str = "git-diff";
pub(super) const VIEW_ID: &str = "git-diff-view";

/// Removes carriage returns before source lines reach tab expansion and
/// terminal rendering.
pub(super) fn strip_carriage_returns(text: &str) -> Cow<'_, str> {
	if text.contains('\r') {
		Cow::Owned(text.replace('\r', ""))
	} else {
		Cow::Borrowed(text)
	}
}

impl GitWorkbench {
	pub(super) fn root_component(
		&self,
		pane: DiffPane,
		sidebar: Col,
		sidebar_width: u16,
		content_rows: u16,
	) -> Col {
		let diff_width = self.width.saturating_sub(sidebar_width.saturating_add(1));
		let path = self.selected.as_ref().map_or("", |(_, path)| path.as_str());
		let (directory, basename) = split_path(path);
		let (additions, deletions) = self.current_counts();
		let middle = self
			.status
			.as_ref()
			.map_or_else(|| Str::new_static(self.focus.hint()), |(message, ..)| message.clone());
		let middle_color = self
			.status
			.as_ref()
			.map_or(self.ctx.theme.muted, |(_, color, ..)| *color);
		let encoding = self.contents.as_ref().map_or("UTF-8", |contents| {
			if contents.media.as_deref() == Some("binary") {
				"Binary"
			} else if contents.media.is_some() {
				"Media"
			} else if contents.binary {
				"Binary"
			} else {
				"UTF-8"
			}
		});
		let selected_area = self.selected.as_ref().map(|(area, _)| *area);
		let scope = self.scope_label();
		let scope_color = match selected_area {
			Some(GitArea::Staged) => "ok",
			Some(GitArea::Unstaged) => "warn",
			Some(GitArea::Commit) | None => "accent",
		};
		let mode_value: &'static str = self.pane_mode().into();
		let up_icon = self.ctx.charset.icon(Icon::Up);
		let down_icon = self.ctx.charset.icon(Icon::Down);
		let close_icon = self.ctx.charset.icon(Icon::Close);
		let whitespace_icon = self.ctx.charset.icon_named("whitespace").unwrap_or("¶");
		let whitespace_label = if self.whitespace == DiffWhitespaceMode::Formatting {
			sf!("{whitespace_icon}+")
		} else {
			Str::new(whitespace_icon)
		};
		let wrap_icon = self.ctx.charset.icon_named("word-wrap").unwrap_or("↩");
		let whitespace_active = self.whitespace != DiffWhitespaceMode::Off;
		let wraps = self.pane_wraps();
		let separator = if self.focus == Focus::Sidebar {
			self.ctx.theme.accent
		} else {
			self.ctx.theme.border
		};
		dom! {
			<col>
				<row h=1 bg=surface gap=1>
					<row grow truncate>
						if !directory.is_empty() { <text dim>{directory}</text> }
						<text bold>{basename}</text>
					</row>
					<text fg=ok>{sf!("+{additions}")}</text>
					<text fg=err>{sf!("−{deletions}")}</text>
					<spacer grow/>
					<text id="git-status" fg={middle_color} dim truncate>{middle}</text>
					<spacer grow/>
					<text dim>{encoding}</text>
					if selected_area == Some(GitArea::Unstaged) {
						<button id="git-stage-file" variant=pill color=ok active>{"Stage File"}</button>
					} else if selected_area == Some(GitArea::Staged) {
						<button id="git-unstage-file" variant=pill color=warn active>{"Unstage File"}</button>
					}
					<button id="git-close" variant=soft>{close_icon}</button>
				</row>
				<row h=1 bg=surface gap=1>
					<row w={diff_width} gap=1>
					<button variant=tint color={scope_color} active>{scope}</button>
					<spacer grow/>
					<button id="git-up" variant=ghost>{up_icon}</button>
					<button id="git-down" variant=ghost>{down_icon}</button>
					<segmented id={VIEW_ID} value={mode_value}>
						<option value="file" icon="file-diff"/>
						<option value="split" icon="split"/>
						<option value="inline" icon="inline"/>
						<option value="hunk" icon="hunk"/>
					</segmented>
					<spacer grow/>
					<button id="git-ws" variant=soft active={whitespace_active}>{whitespace_label}</button>
					<button id="git-wrap" variant=soft active={wraps}>{wrap_icon}</button>
					</row>
					<spacer grow/>
				</row>
				<row h={content_rows}>
					{pane}
					<pre id="git-separator" fg={separator}>{"│"}</pre>
					{sidebar}
				</row>
			</col>
		}
	}
}
