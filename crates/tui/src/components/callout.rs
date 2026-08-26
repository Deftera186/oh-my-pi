use omp_core::{IntoStr, Str};

use super::text::{append, put_clipped, truncate_rich};
use crate::{
	Icon, UiContext,
	component::{Component, MemoKey, PaintCtx, Slot, next_slot},
	frame::{Color, Rect, Style},
	markdown,
	markdown::MdTheme,
	props::{Prop, PropValue, Props},
	rich::{RichText, cell_width},
};

#[derive(Clone, Copy)]
enum CalloutKind {
	Info,
	Warn,
	Error,
	Success,
}

impl CalloutKind {
	fn parse(value: &str) -> Option<Self> {
		match value {
			"info" => Some(Self::Info),
			"warn" => Some(Self::Warn),
			"error" => Some(Self::Error),
			"success" => Some(Self::Success),
			_ => None,
		}
	}

	const fn color(self, ctx: &UiContext) -> Color {
		match self {
			Self::Info => ctx.theme.info,
			Self::Warn => ctx.theme.warn,
			Self::Error => ctx.theme.err,
			Self::Success => ctx.theme.ok,
		}
	}

	const fn icon(self) -> Icon {
		match self {
			Self::Info => Icon::Info,
			Self::Warn => Icon::Warning,
			Self::Error => Icon::Error,
			Self::Success => Icon::Success,
		}
	}
}

/// A highlighted Markdown notice backing the `<callout>` markup tag.
pub struct Callout {
	props:        Props,
	slot:         Slot,
	text:         Str,
	rich:         RichText,
	version:      u64,
	cached_width: u16,
	cached:       Option<MemoKey>,
}

impl Callout {
	/// Creates an empty callout.
	pub fn new() -> Self {
		Self {
			props:        Props::new(),
			slot:         next_slot(),
			text:         Str::default(),
			rich:         RichText::default(),
			version:      1,
			cached_width: 0,
			cached:       None,
		}
	}

	/// Sets one callout property.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self.version = self.version.wrapping_add(1);
		self
	}

	/// Sets one callout property from a string.
	pub fn with_str(self, prop: Prop, value: &str) -> Self {
		self.with(prop, value)
	}

	/// Appends Markdown source text.
	pub fn text(mut self, text: impl IntoStr) -> Self {
		append(&mut self.text, text.into_str());
		self.version = self.version.wrapping_add(1);
		self
	}

	fn semantic_kind(&self) -> Option<CalloutKind> {
		self
			.props
			.str_of(Prop::Kind)
			.and_then(|kind| CalloutKind::parse(kind))
	}

	fn has_header(&self) -> bool {
		self.semantic_kind().is_some()
			|| self.props.title().is_some()
			|| self.props.str_of(Prop::Badge).is_some()
			|| self.props.str_of(Prop::Icon).is_some()
	}

	fn style(&self, ctx: &UiContext) -> Style {
		let style = self.props.style(&ctx.theme);
		if matches!(style.foreground_color(), Color::Default)
			&& let Some(kind) = self.semantic_kind()
		{
			return style.fg(kind.color(ctx));
		}
		style
	}

	fn accent(&self, ctx: &UiContext) -> Color {
		let color = self.style(ctx).foreground_color();
		if matches!(color, Color::Default) {
			ctx.theme.info
		} else {
			color
		}
	}

	fn icon<'a>(&'a self, ctx: &'a UiContext) -> &'a str {
		self.props.str_of(Prop::Icon).map_or_else(
			|| {
				self
					.semantic_kind()
					.map_or_else(|| ctx.charset.note_icon(), |kind| ctx.charset.icon(kind.icon()))
			},
			|name| ctx.charset.icon_named(name).unwrap_or(name),
		)
	}

	fn header_width(&self, ctx: &UiContext) -> u16 {
		if !self.has_header() {
			return 0;
		}
		let icon = cell_width(self.icon(ctx)).saturating_add(1);
		let title = self.props.title().map_or(0, |title| cell_width(title));
		let badge = self
			.props
			.str_of(Prop::Badge)
			.map_or(0, |badge| cell_width(badge).saturating_add(1));
		icon.saturating_add(title).saturating_add(badge)
	}

	fn render(&mut self, ctx: &UiContext, width: u16) {
		let width = width.saturating_sub(2).max(1);
		let key = MemoKey::new(self.version, ctx);
		if self.cached_width == width && self.cached == Some(key) {
			return;
		}
		let style = self.style(ctx);
		let theme = MdTheme::from_context(ctx).cascade(style);
		self.rich.clear();
		markdown::render(&self.text, width, &theme, &mut self.rich);
		truncate_rich(&mut self.rich, width, style, self.props.truncate());
		self.cached_width = width;
		self.cached = Some(key);
	}
}
#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		Charset,
		component::PaintCtx,
		frame::{Frame, Size},
		markup::Border,
		test_support::{frame_cell_style, frame_row_text},
	};

	fn paint(callout: &mut Callout, ctx: &UiContext, width: u16, height: u16) -> Frame {
		let mut frame = Frame::new(Size::new(width, height));
		let mut hits = Vec::new();
		let mut wakes = Vec::new();
		let mut pc = PaintCtx::new(&mut frame, ctx, &mut hits, &mut wakes);
		callout.paint(&mut pc, Rect::new(0, 0, width, height));
		frame
	}

	#[test]
	fn semantic_kinds_supply_their_icon_and_color() {
		for charset in [Charset::Unicode, Charset::NerdFont, Charset::Ascii] {
			let ctx = UiContext { charset, ..UiContext::default() };
			for (kind, color, icon) in [
				("info", ctx.theme.info, Icon::Info),
				("warn", ctx.theme.warn, Icon::Warning),
				("error", ctx.theme.err, Icon::Error),
				("success", ctx.theme.ok, Icon::Success),
			] {
				let mut callout = Callout::new().with(Prop::Kind, kind).text("body");
				let frame = paint(&mut callout, &ctx, 24, 2);
				assert!(
					frame_row_text(&frame, 0).starts_with(ctx.charset.icon(icon)),
					"{kind} should use its semantic icon in {charset:?}"
				);
				assert_eq!(frame_cell_style(&frame, 0, 0).foreground, color);
				assert_eq!(frame_cell_style(&frame, 0, 1).foreground, color);
				assert_eq!(frame_cell_style(&frame, 2, 1).foreground, color);
			}
		}
	}

	#[test]
	fn explicit_callout_props_override_semantic_defaults() {
		let ctx = UiContext::default();
		let foreground = Color::Rgb(1, 2, 3);
		let background = Color::Rgb(4, 5, 6);
		let mut callout = Callout::new()
			.with(Prop::Kind, "error")
			.with(Prop::Icon, "folder")
			.with(Prop::Fg, foreground)
			.with(Prop::Bg, background)
			.with(Prop::Border, Border::Round)
			.with(Prop::Badge, "manual")
			.with(Prop::Title, "Override")
			.text("body");

		assert_eq!(callout.accent(&ctx), foreground);
		assert_eq!(callout.icon(&ctx), ctx.charset.icon_named("folder").unwrap());
		assert_eq!(callout.props.border(), Some(Border::Round));
		let frame = paint(&mut callout, &ctx, 32, 2);
		let header = frame_row_text(&frame, 0);
		assert!(header.starts_with(ctx.charset.icon_named("folder").unwrap()));
		assert!(header.contains("Override"));
		assert!(header.contains("manual"));
		let body = frame_cell_style(&frame, 2, 1);
		assert_eq!(body.foreground, foreground);
		assert_eq!(body.background, background);
	}

	#[test]
	fn callouts_without_kind_keep_the_existing_presentation() {
		let ctx = UiContext::default();
		let mut body_only = Callout::new().text("body");
		let frame = paint(&mut body_only, &ctx, 20, 1);
		assert!(!body_only.has_header());
		assert_eq!(frame_cell_style(&frame, 0, 0).foreground, ctx.theme.info);
		assert_eq!(frame_cell_style(&frame, 2, 0).foreground, ctx.theme.fg);

		let titled = Callout::new().with(Prop::Title, "Note");
		assert_eq!(titled.icon(&ctx), ctx.charset.note_icon());
	}
}

impl Default for Callout {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for Callout {
	fn props(&self) -> &Props {
		&self.props
	}

	fn props_mut(&mut self) -> &mut Props {
		&mut self.props
	}

	fn slot(&self) -> Slot {
		self.slot
	}

	fn measure(&mut self, ctx: &UiContext) -> (u16, u16) {
		let body = self
			.text
			.lines()
			.map(cell_width)
			.max()
			.unwrap_or(0)
			.saturating_add(2);
		(14, body.max(self.header_width(ctx)).max(16))
	}

	fn height(&mut self, ctx: &UiContext, width: u16) -> u16 {
		self.render(ctx, width);
		u16::from(self.has_header()).saturating_add(RichText::rows(&self.rich))
	}

	fn place(&mut self, ctx: &UiContext, content: Rect) {
		self.render(ctx, content.width);
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		self.render(pc.ctx, rect.width);
		let accent = self.accent(pc.ctx);
		let clip = pc.clip.min(rect.y.saturating_add(rect.height));
		let mut y = rect.y;
		if self.has_header() {
			if y < clip {
				let mut x = pc
					.frame
					.put(rect.x, y, self.icon(pc.ctx), Style::new().fg(accent));
				x = pc.frame.put(x, y, " ", Style::new().fg(pc.ctx.theme.fg));
				if let Some(title) = self.props.title() {
					x = pc.frame.put(x, y, title, Style::new().fg(accent).bold());
				}
				if let Some(badge) = self.props.str_of(Prop::Badge) {
					x = pc.frame.put(x, y, " ", Style::new().fg(pc.ctx.theme.fg));
					pc.frame
						.put(x, y, badge, Style::new().fg(pc.ctx.theme.muted));
				}
			}
			y = y.saturating_add(1);
		}
		let right = rect.x.saturating_add(rect.width);
		for row in 0..RichText::rows(&self.rich) {
			let line_y = y.saturating_add(row);
			if line_y >= clip {
				break;
			}
			let mut x = pc
				.frame
				.put(rect.x, line_y, pc.ctx.charset.rail(), Style::new().fg(accent));
			for (style, text) in self.rich.row_runs(row) {
				x = put_clipped(pc.frame, x, line_y, right, text, style);
				if x >= right {
					break;
				}
			}
		}
	}

	fn set_text(&mut self, _ctx: &UiContext, text: Str) -> bool {
		if self.text == text {
			return false;
		}
		self.text = text;
		self.version = self.version.wrapping_add(1);
		true
	}
}
