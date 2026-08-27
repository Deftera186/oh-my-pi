//! Welcome card shown while the chat demo "boots".
//!
//! The stippled-eclipse landing shader ([`omp_tui::shader::Eclipse`])
//! resolves out of black as a full-viewport backdrop. A raytraced orbital
//! aperture hangs over a dithered light floor: live violet core, hard cyan
//! Fresnel edge, smoked-aqua glass ring with a solid rim, vertical calibration
//! pole, and three orbiting probes. The wide layout sets that hero against a
//! compact, ruled session index. [`omp_tui::scene`]'s braille rasterizer traces
//! it per frame inside an ordinary text grid. The card shrinks to a narrow
//! instrument panel, then to the bare aperture, as the viewport tightens.
//!
//! Hovering the card rides the same primitive the retained pipeline's
//! `hover` prop uses: an [`omp_tui::anim::Tween`] eases a pointer-tracking
//! cyan edge while the pointer directly orbits the aperture camera.
#![allow(
	clippy::suboptimal_flops,
	reason = "straight port of the omp-welcome-term prototype; mul_add obscures the math"
)]

use std::{
	f32::consts::{PI, TAU},
	fmt::Write as _,
	iter,
	time::Duration,
};

use omp_core::{Str, sf};
use omp_tui::{
	Border, Charset,
	Color::{self, Rgb as RgbColor},
	Frame, Icon, Key, Rect, Size, Style, Theme, UiContext,
	anim::{Easing, Lerp, Tween},
	scene::{self, Camera, Ray, Trace, Vec3, vec3},
	shader::{Eclipse, Surface},
};

use crate::{ModelDownloadProgress, SessionRow, WelcomeLspServer};

/// Wide card: logo on the left, recent sessions on the right.
const CARD_COLS: u16 = 98;
const CARD_ROWS: u16 = 17;
/// Narrow card: centered aperture, condensed key hints.
const SMOL_COLS: u16 = 37;

/// Hover transition length; the same feel as the pipeline's `anim` default.
const HOVER_EASE: Duration = Duration::from_millis(160);
/// Ambient-scene cadence: preserve the boot reveal, then spend less of the
/// input thread on motion that remains legible at a lower rate.
const AMBIENT_REVEAL_INTERVAL: Duration = Duration::from_millis(40);
const LOGO_IDLE_INTERVAL: Duration = Duration::from_millis(120);
const BACKDROP_IDLE_INTERVAL: Duration = Duration::from_millis(80);
const AMBIENT_REVEAL_END: Duration = Duration::from_millis(1_200);
/// Backdrop fade-in horizon (seconds): the eclipse resolves out of black
/// while the aperture completes its reveal orbit.
const BACKDROP_FADE: f32 = 1.2;

fn ambient_interval(clock: Duration, idle: Duration) -> Duration {
	if clock < AMBIENT_REVEAL_END {
		AMBIENT_REVEAL_INTERVAL
	} else {
		idle
	}
}

fn perimeter_angle(rect: Rect, column: u16, row: u16) -> f32 {
	let right = rect.x.saturating_add(rect.width.saturating_sub(1));
	let bottom = rect.y.saturating_add(rect.height.saturating_sub(1));
	let x = column.clamp(rect.x, right);
	let y = row.clamp(rect.y, bottom);
	let width = f32::from(right.saturating_sub(rect.x)).max(1.0);
	let height = f32::from(bottom.saturating_sub(rect.y)).max(1.0);
	let x_fraction = f32::from(x.saturating_sub(rect.x)) / width;
	let y_fraction = f32::from(y.saturating_sub(rect.y)) / height;

	// Terminal cells are roughly twice as tall as they are wide. Compare
	// physical edge distances, then give every side one 90-degree quadrant.
	let left_distance = f32::from(x.saturating_sub(rect.x)) * 0.5;
	let right_distance = f32::from(right.saturating_sub(x)) * 0.5;
	let top_distance = f32::from(y.saturating_sub(rect.y));
	let bottom_distance = f32::from(bottom.saturating_sub(y));
	let quarter = TAU * 0.25;
	let angle = if left_distance <= bottom_distance
		&& left_distance <= right_distance
		&& left_distance <= top_distance
	{
		quarter * y_fraction
	} else if bottom_distance <= right_distance && bottom_distance <= top_distance {
		quarter * (1.0 + x_fraction)
	} else if right_distance <= top_distance {
		quarter * (3.0 - y_fraction)
	} else {
		quarter * (4.0 - x_fraction)
	};
	angle.rem_euclid(TAU)
}

fn shortest_angle_delta(from: f32, to: f32) -> f32 {
	(to - from + PI).rem_euclid(TAU) - PI
}

// ── scene constants ─────────────────────────────────────────────────────────
const LOGO_COLS: usize = 29;
const LOGO_ROWS: usize = 11;

const CORE_CENTER: Vec3 = vec3(0.0, 0.12, 0.0);
const CORE_RADIUS: f32 = 0.47;
const RING_OUTER_RADIUS: f32 = 1.05;
const RING_INNER_RADIUS: f32 = 0.68;
const RING_RIM_WIDTH: f32 = 0.09;
/// Fixed surface opacity of the smoked-aqua glass inside the solid rim.
const RING_GLASS_OPACITY: f32 = 0.50;
const SPARK_RADIUS: f32 = 0.055;
const POLE_RADIUS: f32 = 0.035;
const POLE_CAP_RADIUS: f32 = 0.065;
const POLE_BOTTOM: f32 = -0.70;
const POLE_TOP: f32 = 1.08;
const FLOOR_Y: f32 = -0.78;

/// Orbit camera tuned for a wide, low instrument silhouette.
const CAMERA_DISTANCE: f32 = 4.05;
const CAMERA_PITCH: f32 = 0.36;
const CAMERA_FOCAL: f32 = 2.82;

/// Theme-derived colors sampled by the aperture raytracer.
#[derive(Clone, Copy)]
struct AperturePalette {
	plate:       Color,
	fade_origin: Color,
	cyan:        [Vec3; 4],
	background:  Vec3,
	white:       Vec3,
	ink:         Vec3,
	live:        Vec3,
	glass:       Vec3,
	tone_lift:   Vec3,
}

impl AperturePalette {
	fn new(theme: Theme) -> Self {
		let panel = color_vec(theme.panel);
		let shadow = color_vec(theme.shadow);
		let info = color_vec(theme.info);
		let accent = color_vec(theme.accent);
		let fg = color_vec(theme.fg);
		let secondary = color_vec(theme.secondary);

		Self {
			plate:       scaled_color(theme.panel, vec3(0.825_585_1, 0.635_398_7, 0.607_804_4)),
			fade_origin: theme.shadow,
			cyan:        [
				info * vec3(0.065, 0.223_642_81, 0.360_751_12),
				accent * vec3(0.483_571_95, 1.455_503_2, 1.158_537_5),
				fg * vec3(1.370_022, 1.583_189_9, 1.518_891_6),
				accent * vec3(0.483_571_95, 1.455_503_2, 1.158_537_5),
			],
			background:  panel * vec3(0.825_585_1, 0.635_398_7, 0.607_804_4),
			white:       fg * vec3(1.370_022, 1.583_189_9, 1.518_891_6),
			ink:         shadow * 0.0,
			live:        secondary * vec3(0.961_527_05, 0.781_29, 1.143_262_4),
			glass:       info * vec3(1.946_768_4, 1.827_751_6, 1.299_589_5),
			tone_lift:   panel * vec3(0.247_675_54, 0.190_619_6, 0.200_717_71),
		}
	}
}

const fn color_vec(color: Color) -> Vec3 {
	match color {
		RgbColor(red, green, blue) => Vec3::rgb(red, green, blue),
		Color::Indexed(value) => Vec3::rgb(value, value, value),
		Color::Default => Vec3::ZERO,
	}
}

fn scaled_color(color: Color, scale: Vec3) -> Color {
	let value = color_vec(color) * scale;
	Color::from(value)
}

type LogoGrid = [[Option<(char, Color)>; LOGO_COLS]; LOGO_ROWS];

/// Result of routing a key through the welcome session index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WelcomeEvent {
	/// The session index consumed the key without committing a row.
	Consumed,
	/// Start a fresh session.
	NewSession,
	/// Resume the selected recent session.
	Resume(Str),
	/// Leave the terminal host.
	Quit,
}

/// Animated welcome screen with a host-provided recent-session index.
struct WelcomeDownload {
	progress:  ModelDownloadProgress,
	received:  Duration,
	completed: Option<Duration>,
}
/// Animated launch surface for starting or resuming a chat session.

pub struct Welcome {
	frame:          Frame,
	title:          Str,
	/// Detected glyph tier for the card chrome.
	charset:        Charset,
	theme:          Theme,
	palette:        AperturePalette,
	sessions:       Vec<SessionRow>,
	selected:       usize,
	lsp_servers:    Vec<WelcomeLspServer>,
	camera:         (f32, f32),
	camera_target:  (f32, f32),
	last_elapsed:   f32,
	/// Last visible card or bare-logo bounds used for pointer orbit mapping.
	orbit_rect:     Rect,
	logo:           LogoGrid,
	logo_at:        Option<Duration>,
	backdrop_frame: Frame,
	backdrop_at:    Option<Duration>,
	/// Eclipse backdrop program and its reusable half-block render target.
	backdrop:       Eclipse,
	surface:        Surface,
	/// Last pointer cell reported by the host loop.
	pointer:        Option<(u16, u16)>,
	/// Eased hover amount driving the border glow.
	hover:          Tween<f32>,
	download:       Option<WelcomeDownload>,
}

impl Welcome {
	/// Creates the animated welcome card with host-provided recent sessions.
	pub fn new(ctx: &UiContext, sessions: Vec<SessionRow>) -> Self {
		Self {
			charset:        ctx.charset,
			theme:          ctx.theme,
			palette:        AperturePalette::new(ctx.theme),
			frame:          Frame::new(Size::new(0, 0)),
			title:          sf!(
				" {} omp v{} ",
				ctx.charset.icon(Icon::Omp),
				env!("CARGO_PKG_VERSION")
			),
			sessions:       sessions.into_iter().take(3).collect(),
			selected:       0,
			lsp_servers:    Vec::new(),
			camera:         (0.0, 0.0),
			camera_target:  (0.0, 0.0),
			last_elapsed:   0.0,
			orbit_rect:     Rect::new(0, 0, 1, 1),
			logo:           [[None; LOGO_COLS]; LOGO_ROWS],
			logo_at:        None,
			backdrop_frame: Frame::new(Size::new(0, 0)),
			backdrop_at:    None,
			backdrop:       Eclipse::default(),
			surface:        Surface::new(),
			pointer:        None,
			hover:          Tween::settled(0.0),
			download:       None,
		}
	}

	/// Updates model download progress retained by the welcome activity row.
	pub fn set_download_progress(&mut self, progress: ModelDownloadProgress, now: Duration) {
		self.download = Some(WelcomeDownload {
			completed: progress.complete.then_some(now),
			progress,
			received: now,
		});
	}

	/// Replaces the recent-session index while preserving a visible selection.
	pub fn set_sessions(&mut self, mut sessions: Vec<SessionRow>) {
		sessions.truncate(3);
		self.sessions = sessions;
		self.selected = self.selected.min(self.sessions.len());
	}

	/// Replaces the compact language-server facts shown on the wide card.
	pub fn set_lsp_servers(&mut self, mut servers: Vec<WelcomeLspServer>) {
		servers.truncate(3);
		self.lsp_servers = servers;
	}

	/// Routes navigation and commit keys through the session index.
	pub fn handle_key(&mut self, key: Key) -> WelcomeEvent {
		match key {
			Key::Up => {
				self.selected = self.selected.saturating_sub(1);
				WelcomeEvent::Consumed
			},
			Key::Down => {
				self.selected = (self.selected + 1).min(self.sessions.len());
				WelcomeEvent::Consumed
			},
			Key::Enter if self.selected == 0 => WelcomeEvent::NewSession,
			Key::Enter => self
				.sessions
				.get(self.selected - 1)
				.map_or(WelcomeEvent::Consumed, |session| WelcomeEvent::Resume(session.id.clone())),
			Key::Esc | Key::Ctrl('c') => WelcomeEvent::Quit,
			_ => WelcomeEvent::Consumed,
		}
	}

	/// The perimeter runs clockwise: top-left `0°`, bottom-left `90°`,
	/// bottom-right `180°`, and top-right `270°`. Points inside use their
	/// physically nearest edge; points outside clamp to the card first.
	pub fn point_at(&mut self, column: u16, row: u16) {
		self.pointer = Some((column, row));
		self.logo_at = None;
		let bottom = self
			.orbit_rect
			.y
			.saturating_add(self.orbit_rect.height.saturating_sub(1));
		let center_y = f32::from(self.orbit_rect.y) + f32::from(bottom - self.orbit_rect.y) * 0.5;
		let half_height = (f32::from(bottom - self.orbit_rect.y) * 0.5).max(1.0);
		let clamped_row = row.clamp(self.orbit_rect.y, bottom);
		let vertical = (f32::from(clamped_row) - center_y) / half_height;
		self.camera_target = (-vertical * 0.42, perimeter_angle(self.orbit_rect, column, row));
	}

	/// Paints the card centered in `viewport` at `elapsed` since boot and
	/// returns the full-viewport frame (no stable rows, everything damaged).
	pub fn render(&mut self, viewport: Size, elapsed: Duration) -> &Frame {
		if self.frame.size() != viewport {
			self.frame = Frame::new(viewport);
			self.backdrop_frame = Frame::new(viewport);
			self.backdrop_at = None;
		}
		let clock = elapsed;
		let elapsed = elapsed.as_secs_f32();
		// Exponential pointer chase, frame-rate independent (~100ms lag).
		let delta = (elapsed - self.last_elapsed).max(0.0);
		self.last_elapsed = elapsed;
		let response = 1.0 - (-delta * 10.0).exp();
		self.camera.0 += (self.camera_target.0 - self.camera.0) * response;
		let yaw_delta = shortest_angle_delta(self.camera.1, self.camera_target.1);
		self.camera.1 = (self.camera.1 + yaw_delta * response).rem_euclid(TAU);
		self.draw_backdrop(viewport, clock, elapsed);

		let logo_interval = ambient_interval(clock, LOGO_IDLE_INTERVAL);
		if self
			.logo_at
			.is_none_or(|rendered_at| clock.saturating_sub(rendered_at) >= logo_interval)
		{
			self.logo = logo_cells(elapsed, self.camera, self.palette);
			self.logo_at = Some(clock);
		}
		let cols = if viewport.width >= CARD_COLS && viewport.height >= CARD_ROWS {
			Some(CARD_COLS)
		} else if viewport.width >= SMOL_COLS && viewport.height >= CARD_ROWS {
			Some(SMOL_COLS)
		} else {
			None
		};
		let Some(cols) = cols else {
			let left = viewport.width.saturating_sub(LOGO_COLS as u16) / 2;
			let top = viewport.height.saturating_sub(LOGO_ROWS as u16) / 2;
			self.orbit_rect = Rect::new(
				left,
				top,
				(LOGO_COLS as u16).min(viewport.width),
				(LOGO_ROWS as u16).min(viewport.height),
			);
			blit_logo(&mut self.frame, &self.logo, left, top, self.palette.plate);
			return &self.frame;
		};

		let left = (viewport.width - cols) / 2;
		let top = (viewport.height - CARD_ROWS) / 2;
		self.orbit_rect = Rect::new(left, top, cols, CARD_ROWS);
		let hovered = self.pointer.is_some_and(|(x, y)| {
			(left..left + cols).contains(&x) && (top..top + CARD_ROWS).contains(&y)
		});
		self
			.hover
			.retarget(clock, if hovered { 1.0 } else { 0.0 }, HOVER_EASE, Easing::EaseOut);
		let hover = self.hover.sample(clock).clamp(0.0, 1.0);
		self.draw_card(cols, left, top, elapsed, hover);
		if let Some(download) = &self.download
			&& clock >= download.received.saturating_add(Duration::from_secs(1))
			&& download
				.completed
				.is_none_or(|completed| clock < completed.saturating_add(Duration::from_secs(3)))
		{
			let mut label =
				format!(" {} · {} bytes", download.progress.label, download.progress.downloaded);
			if let Some(total) = download.progress.total.filter(|total| *total > 0) {
				let percent = download.progress.downloaded.saturating_mul(100) / total;
				let _ = write!(label, " / {total} · {percent}%");
			}
			if download.progress.complete {
				label.push_str(" · ready");
			}
			self.frame.fill(
				Rect::new(left + 1, top + CARD_ROWS - 2, cols - 2, 1),
				on_footer(self.theme, self.theme.fg),
			);
			self.frame.put(
				left + 3,
				top + CARD_ROWS - 2,
				&label,
				on_footer(self.theme, self.theme.info),
			);
		}
		&self.frame
	}

	/// Paints the eclipse across the whole viewport, resolving out of
	/// black over the first [`BACKDROP_FADE`] seconds of boot.
	fn draw_backdrop(&mut self, viewport: Size, clock: Duration, elapsed: f32) {
		let interval = ambient_interval(clock, BACKDROP_IDLE_INTERVAL);
		if self
			.backdrop_at
			.is_none_or(|rendered_at| clock.saturating_sub(rendered_at) >= interval)
		{
			let fade = smooth((elapsed / BACKDROP_FADE).clamp(0.0, 1.0));
			self
				.backdrop_frame
				.fill(Rect::new(0, 0, viewport.width, viewport.height), Style::default());
			let frame = &mut self.backdrop_frame;
			let mut buffer = [0_u8; 4];
			let dim = |color: Color| self.palette.fade_origin.lerp(color, fade);
			self.surface.render(
				&mut self.backdrop,
				clock,
				viewport.width,
				viewport.height,
				|x, y, glyph, fg, bg| {
					let style = Style::new().fg(dim(fg));
					let style = match bg {
						Some(bg) => style.bg(dim(bg)),
						None => style,
					};
					frame.put(x, y, glyph.encode_utf8(&mut buffer), style);
				},
			);
			self.backdrop_at = Some(clock);
		}
		self.frame.clone_from(&self.backdrop_frame);
	}

	fn draw_card(&mut self, cols: u16, left: u16, top: u16, elapsed: f32, hover: f32) {
		let full = cols == CARD_COLS;
		let logo_left = if full {
			left + 4
		} else {
			left + (cols - LOGO_COLS as u16) / 2
		};
		let theme = self.theme;

		let pointer = self.pointer;
		let center =
			(f32::from(left) + f32::from(cols) / 2.0, f32::from(top) + f32::from(CARD_ROWS) / 2.0);
		let edge_at = move |x: u16, y: u16| -> Style {
			let Some((px, py)) = pointer.filter(|_| hover > 0.02) else {
				return on_card(theme, theme.border);
			};
			let dx = (f32::from(x) - f32::from(px)) * 0.5;
			let dy = f32::from(y) - f32::from(py);
			let glow = hover * (-(dx * dx + dy * dy) / 34.0).exp();
			if glow < 0.02 {
				return on_card(theme, theme.border);
			}
			let angle = (f32::from(y) - center.1).atan2((f32::from(x) - center.0) * 0.5);
			let glint = (0.5 + 0.5 * (elapsed * 2.4 - angle * 2.0).sin()) * 0.38;
			let edge = theme.info.lerp(theme.fg, glint);
			on_card(theme, theme.border.lerp(edge, glow))
		};
		let frame = &mut self.frame;
		frame.fill(Rect::new(left, top, cols, CARD_ROWS), on_card(theme, theme.fg));

		let right = left + cols - 1;
		let bottom = top + CARD_ROWS - 1;
		let divider = bottom - 2;
		let (tl, tr, bl, br, horizontal, vertical) = self.charset.border(Border::Square);
		let grid = self.charset.grid();
		let mut glyph = [0_u8; 4];
		frame.put(left, top, tl.encode_utf8(&mut glyph), edge_at(left, top));
		frame.put(right, top, tr.encode_utf8(&mut glyph), edge_at(right, top));
		frame.put(left, divider, grid.middle.0.encode_utf8(&mut glyph), edge_at(left, divider));
		frame.put(right, divider, grid.middle.2.encode_utf8(&mut glyph), edge_at(right, divider));
		frame.put(left, bottom, bl.encode_utf8(&mut glyph), edge_at(left, bottom));
		frame.put(right, bottom, br.encode_utf8(&mut glyph), edge_at(right, bottom));
		for x in left + 1..right {
			frame.put(x, top, horizontal.encode_utf8(&mut glyph), edge_at(x, top));
			frame.put(x, divider, horizontal.encode_utf8(&mut glyph), edge_at(x, divider));
			frame.put(x, bottom, horizontal.encode_utf8(&mut glyph), edge_at(x, bottom));
		}
		for y in top + 1..bottom {
			if y != divider {
				frame.put(left, y, vertical.encode_utf8(&mut glyph), edge_at(left, y));
				frame.put(right, y, vertical.encode_utf8(&mut glyph), edge_at(right, y));
			}
		}

		frame.put(left + 2, top, self.title.as_str(), on_card(theme, theme.fg).bold());
		if full {
			frame.put(left + 39, top, " SESSION INDEX ", on_card(theme, theme.border));
			let live = sf!(" {} LIVE ", self.charset.icon(Icon::Enabled));
			frame.put(left + cols - 11, top, &live, on_card(theme, theme.secondary));
			draw_sessions(frame, left, top, self.charset, theme, &self.sessions, self.selected);
			draw_lsp_servers(frame, left, top, self.charset, theme, &self.lsp_servers);
		}

		draw_instrument_hud(frame, logo_left, top, theme);
		blit_logo(frame, &self.logo, logo_left, top + 2, theme.panel);

		let footer = divider + 1;
		frame.fill(Rect::new(left + 1, footer, cols - 2, 1), on_footer(theme, theme.fg));
		if full {
			frame.put(left + 3, divider, " COMMAND DECK ", on_card(theme, theme.border));
			draw_full_hints(frame, left, footer, theme);
		} else {
			draw_smol_hints(frame, left, cols, footer, theme);
		}
	}
}

impl Default for Welcome {
	fn default() -> Self {
		Self::new(&UiContext::default(), Vec::new())
	}
}

fn draw_instrument_hud(frame: &mut Frame, logo_left: u16, top: u16, theme: Theme) {
	frame.put(logo_left, top + 1, "00 / APERTURE", on_card(theme, theme.border));
	frame.put(logo_left + 25, top + 1, "Y+", on_card(theme, theme.border));
}

fn draw_sessions(
	frame: &mut Frame,
	left: u16,
	top: u16,
	charset: Charset,
	theme: Theme,
	sessions: &[SessionRow],
	selected: usize,
) {
	let (_, _, _, _, _, vertical) = charset.border(Border::Square);
	let mut glyph = [0_u8; 4];
	let panel_x = left + 40;
	frame.put(panel_x, top + 2, "RECENT SESSIONS", on_card(theme, theme.muted));
	let count = sf!("{:02} / LOCAL", sessions.len());
	frame.put(left + CARD_COLS - 15, top + 2, &count, on_card(theme, theme.border));
	for y in top + 4..=top + 11 {
		frame.put(panel_x, y, vertical.encode_utf8(&mut glyph), on_card(theme, theme.border));
	}
	let rows =
		iter::once((sf!("New session"), sf!("NEW"))).chain(sessions.iter().take(3).map(|row| {
			(
				if row.pinned {
					sf!("{} {}", charset.icon(Icon::Pin), row.label)
				} else {
					row.label.clone()
				},
				row.detail.clone(),
			)
		}));
	for (index, (label, detail)) in rows.enumerate() {
		let y = top + 4 + index as u16 * 2;
		if index == selected {
			frame.fill(Rect::new(panel_x - 2, y, CARD_COLS - 39, 1), on_selected(theme, theme.fg));
			frame.put(panel_x - 2, y, charset.rail(), on_selected(theme, theme.info));
			frame.put(panel_x, y, charset.radio(true), on_selected(theme, theme.info));
			frame.put(panel_x + 2, y, detail.as_str(), on_selected(theme, theme.info));
			frame.put(panel_x + 7, y, label.as_str(), on_selected(theme, theme.fg).bold());
		} else {
			frame.put(panel_x, y, charset.radio(false), on_card(theme, theme.border));
			frame.put(panel_x + 2, y, detail.as_str(), on_card(theme, theme.border));
			frame.put(panel_x + 7, y, label.as_str(), on_card(theme, theme.muted));
		}
	}
	frame.put(panel_x, top + 12, "INDEXED / LOCAL", on_card(theme, theme.border));
}

fn draw_lsp_servers(
	frame: &mut Frame,
	left: u16,
	top: u16,
	charset: Charset,
	theme: Theme,
	servers: &[WelcomeLspServer],
) {
	if servers.is_empty() {
		return;
	}

	let mut x = left + 40;
	let right = left + CARD_COLS - 2;
	frame.fill(Rect::new(x, top + 12, right - x, 1), on_card(theme, theme.fg));
	frame.put(x, top + 12, "LSP", on_card(theme, theme.border).bold());
	x += 4;
	for server in servers.iter().take(3) {
		let (glyph, color) = if server.failed {
			(charset.icon(Icon::Error), theme.err)
		} else if server.stage_label.starts_with("ready") {
			(charset.check(), theme.ok)
		} else if server.stage_label.starts_with("available") {
			(charset.radio(false), theme.muted)
		} else {
			(charset.icon(Icon::Warning), theme.warn)
		};
		let row = sf!("{glyph} {} {}", server.name, server.stage_label);
		let width = row.chars().count() as u16;
		if x.saturating_add(width) > right {
			break;
		}
		frame.put(x, top + 12, row.as_str(), on_card(theme, color));
		x += width + 2;
	}
}

fn draw_full_hints(frame: &mut Frame, left: u16, y: u16, theme: Theme) {
	frame.put(left + 3, y, "#", on_footer(theme, theme.info));
	frame.put(left + 5, y, "actions", on_footer(theme, theme.muted));
	frame.put(left + 14, y, "/", on_footer(theme, theme.border));
	frame.put(left + 16, y, "commands", on_footer(theme, theme.muted));
	frame.put(left + 27, y, "!", on_footer(theme, theme.warn));
	frame.put(left + 29, y, "shell", on_footer(theme, theme.muted));
	frame.put(left + 37, y, "$", on_footer(theme, theme.border));
	frame.put(left + 39, y, "python", on_footer(theme, theme.muted));
	frame.put(left + CARD_COLS - 26, y, "↑↓ select", on_footer(theme, theme.border));
	frame.put(left + CARD_COLS - 14, y, "↵ resume", on_footer(theme, theme.info).bold());
}

fn draw_smol_hints(frame: &mut Frame, left: u16, cols: u16, y: u16, theme: Theme) {
	frame.put(left + 3, y, "#", on_footer(theme, theme.info));
	frame.put(left + 5, y, "/", on_footer(theme, theme.border));
	frame.put(left + 7, y, "!", on_footer(theme, theme.warn));
	frame.put(left + 9, y, "$", on_footer(theme, theme.border));
	frame.put(left + cols - 14, y, "enter", on_footer(theme, theme.border));
	frame.put(left + cols - 8, y, "resume", on_footer(theme, theme.info).bold());
}

fn blit_logo(frame: &mut Frame, logo: &LogoGrid, left: u16, top: u16, background: Color) {
	let mut buffer = [0_u8; 4];
	for (row, cells) in logo.iter().enumerate() {
		for (column, cell) in cells.iter().enumerate() {
			let Some((glyph, color)) = cell else { continue };
			let style = Style::new().fg(*color).bg(background);
			frame.put(left + column as u16, top + row as u16, glyph.encode_utf8(&mut buffer), style);
		}
	}
}

const fn on_card(theme: Theme, fg: Color) -> Style {
	Style::new().fg(fg).bg(theme.panel)
}

const fn on_footer(theme: Theme, fg: Color) -> Style {
	Style::new().fg(fg).bg(theme.shadow)
}

const fn on_selected(theme: Theme, fg: Color) -> Style {
	Style::new().fg(fg).bg(theme.hover)
}

// ── raytraced logo ───────────────────────────────────────────────────────────

fn smooth(edge: f32) -> f32 {
	edge * edge * (3.0 - 2.0 * edge)
}

fn ease_in(progress: f32) -> f32 {
	let clamped = progress.clamp(0.0, 1.0);
	clamped * clamped
}

/// Raises the live core and cyan edge after the boot beat.
fn activation_curve(elapsed: f32) -> f32 {
	ease_in((elapsed - 0.18) / 0.72)
}

/// One decaying full camera orbit as the logo reveals.
fn reveal_orbit(elapsed: f32) -> f32 {
	let progress = ((elapsed - 0.18) / 0.64).clamp(0.0, 1.0);
	TAU * (1.0 - (1.0 - progress).powi(3))
}

/// Orbital motion: a measured ramp into a slow instrument rotation.
fn orbit_rotation(elapsed: f32) -> f32 {
	let spinning = (elapsed - 0.18).max(0.0);
	let ramp = 0.55;
	let angular_speed = TAU / 4.8;
	if spinning < ramp {
		angular_speed * spinning * spinning / (2.0 * ramp)
	} else {
		angular_speed * (spinning - ramp / 2.0)
	}
}

/// Samples the cyan instrument ramp by angle.
fn cyan_ramp(palette: AperturePalette, angle: f32) -> Vec3 {
	let position = (angle / TAU + 0.5).rem_euclid(1.0) * 3.0;
	let index = (position as usize).min(2);
	palette.cyan[index].lerp(palette.cyan[index + 1], position - index as f32)
}

/// Three soft light shafts crossing the floor in the key light's plane.
fn sun_rays(x: f32, z: f32) -> f32 {
	let length = 0.55_f32.hypot(0.38);
	let (hx, hz) = (-0.55 / length, 0.38 / length);
	let across = x * -hz + z * hx;
	let along = x * hx + z * hz;
	let rays = (-((across + 0.52) / 0.065).powi(2)).exp()
		+ (-((across + 0.02) / 0.045).powi(2)).exp()
		+ (-((across - 0.44) / 0.075).powi(2)).exp();
	let envelope = (-((along + 0.16) / 2.35).powi(4)).exp();
	(rays * envelope).clamp(0.0, 1.0)
}

fn sphere_depth(origin: Vec3, direction: Vec3, center: Vec3, radius: f32) -> f32 {
	let offset = origin - center;
	let projection = offset.dot(direction);
	let discriminant = projection * projection - (offset.dot(offset) - radius * radius);
	if discriminant < 0.0 {
		return f32::INFINITY;
	}
	let root = discriminant.sqrt();
	let near = -projection - root;
	if near > 0.0 {
		near
	} else {
		let far = -projection + root;
		if far > 0.0 { far } else { f32::INFINITY }
	}
}

fn ray_distance(origin: Vec3, direction: Vec3, center: Vec3) -> f32 {
	let along = (center - origin).dot(direction).max(0.0);
	(origin + direction * along - center).length()
}

const fn ring_basis() -> (Vec3, Vec3, Vec3) {
	(vec3(0.0, 1.0, 0.0), vec3(1.0, 0.0, 0.0), vec3(0.0, 0.0, 1.0))
}

fn ring_hit(
	origin: Vec3,
	direction: Vec3,
	normal: Vec3,
	tangent: Vec3,
	bitangent: Vec3,
) -> Option<(f32, Vec3, f32, f32, Vec3)> {
	let denominator = normal.dot(direction);
	if denominator.abs() <= 1.0e-5 {
		return None;
	}
	let depth = normal.dot(CORE_CENTER - origin) / denominator;
	if depth <= 0.0 {
		return None;
	}
	let point = origin + direction * depth;
	let offset = point - CORE_CENTER;
	let u = offset.dot(tangent);
	let v = offset.dot(bitangent);
	let radial = u.hypot(v);
	if !(RING_INNER_RADIUS..=RING_OUTER_RADIUS).contains(&radial) {
		return None;
	}
	let facing = if denominator < 0.0 {
		normal
	} else {
		normal * -1.0
	};
	Some((depth, point, radial, v.atan2(u), facing))
}

fn ring_opacity(radial: f32) -> f32 {
	if radial >= RING_OUTER_RADIUS - RING_RIM_WIDTH {
		1.0
	} else {
		RING_GLASS_OPACITY
	}
}

fn pole_hit(origin: Vec3, direction: Vec3) -> Option<(f32, Vec3, Vec3)> {
	let quadratic = direction.x * direction.x + direction.z * direction.z;
	let linear = 2.0 * (origin.x * direction.x + origin.z * direction.z);
	let constant = origin.x * origin.x + origin.z * origin.z - POLE_RADIUS * POLE_RADIUS;
	let mut cylinder_depth = f32::INFINITY;
	if quadratic > 1.0e-8 {
		let discriminant = linear * linear - 4.0 * quadratic * constant;
		if discriminant >= 0.0 {
			let root = discriminant.sqrt();
			let denominator = 2.0 * quadratic;
			for depth in [(-linear - root) / denominator, (-linear + root) / denominator] {
				let height = origin.y + direction.y * depth;
				if depth > 0.0 && (POLE_BOTTOM..=POLE_TOP).contains(&height) {
					cylinder_depth = depth;
					break;
				}
			}
		}
	}

	let top_center = vec3(0.0, POLE_TOP, 0.0);
	let bottom_center = vec3(0.0, POLE_BOTTOM, 0.0);
	let top_depth = sphere_depth(origin, direction, top_center, POLE_CAP_RADIUS);
	let bottom_depth = sphere_depth(origin, direction, bottom_center, POLE_CAP_RADIUS);
	let cap_depth = top_depth.min(bottom_depth);
	let depth = cylinder_depth.min(cap_depth);
	if !depth.is_finite() {
		return None;
	}
	let point = origin + direction * depth;
	let normal = if cap_depth < cylinder_depth {
		let center = if top_depth < bottom_depth {
			top_center
		} else {
			bottom_center
		};
		(point - center).normalize()
	} else {
		vec3(point.x, 0.0, point.z).normalize()
	};
	Some((depth, point, normal))
}

fn spark_position(index: usize, angle: f32) -> Vec3 {
	let phase = angle * 0.82 + index as f32 * (TAU / 3.0);
	vec3(1.11 * phase.cos(), CORE_CENTER.y + 0.24 * (phase * 2.0).sin(), 1.11 * phase.sin())
}

/// Terminal display lift in linear light so braille remains legible on the
/// black chassis.
fn tone(color: Vec3, lift: Vec3) -> Vec3 {
	color * 1.20 + lift
}

/// Per-frame orbital aperture state shared by every ray.
struct Aperture {
	sun:        Vec3,
	palette:    AperturePalette,
	angle:      f32,
	activation: f32,
	/// Smoothed pointer camera from [`Welcome`]: (lift, yaw).
	pointer:    (f32, f32),
}

impl Aperture {
	fn new(pointer: (f32, f32), palette: AperturePalette) -> Self {
		Self {
			sun: vec3(-0.55, 1.0, 0.38).normalize(),
			angle: 0.0,
			activation: 0.0,
			pointer,
			palette,
		}
	}

	/// Dithered floor aura, cast shafts, soft shadow, and core reflection.
	fn ground(&self, origin: Vec3, direction: Vec3) -> (Vec3, f32) {
		if direction.y >= 0.0 {
			return (self.palette.background, 0.0);
		}
		let depth = (FLOOR_Y - origin.y) / direction.y;
		if depth <= 0.0 {
			return (self.palette.background, 0.0);
		}
		let floor = origin + direction * depth;
		let stage =
			(-0.52 * ((floor.x.abs() / 2.20).powi(4) + ((floor.z + 0.12).abs() / 1.55).powi(4))).exp();
		let sun_depth = (CORE_CENTER.y - FLOOR_Y) / self.sun.y;
		let sunlit = floor + self.sun * sun_depth;
		let shadow_distance = (sunlit.x - CORE_CENTER.x).hypot(sunlit.z - CORE_CENTER.z);
		let occlusion = smooth(((CORE_RADIUS + 0.16 - shadow_distance) / 0.20).clamp(0.0, 1.0));
		let floor_radius = floor.x.hypot((floor.z + 0.10) * 1.28);
		let aura = (-(floor_radius / 0.94).powi(2)).exp() * self.activation;
		let dither = 0.72 + 0.28 * ((floor.x * 21.0).sin() * (floor.z * 17.0).sin()).max(0.0);
		let shafts = sun_rays(floor.x, floor.z) * (1.0 - occlusion);
		let cyan_alpha = stage * (shafts * 0.24 + aura * dither * 0.34);
		let live_alpha = stage * aura * occlusion * 0.30;
		let mut alpha = (cyan_alpha + live_alpha).clamp(0.0, 0.78);
		let live_mix = live_alpha / (cyan_alpha + live_alpha).max(1.0e-6);
		let mut color = self.palette.cyan[1]
			.lerp(self.palette.live, live_mix)
			.lerp(self.palette.white, shafts * 0.14);

		let mirrored = vec3(direction.x, -direction.y, direction.z);
		let reflected_depth =
			sphere_depth(floor + vec3(0.0, 1.0e-4, 0.0), mirrored, CORE_CENTER, CORE_RADIUS);
		if reflected_depth.is_finite() {
			let reflected_point = floor + mirrored * reflected_depth;
			let reflected_normal = (reflected_point - CORE_CENTER).normalize();
			let edge = (1.0 - reflected_normal.dot(mirrored * -1.0).abs()).powi(3);
			let reflection_alpha = self.activation * (0.18 + edge * 0.26);
			color = color.lerp(self.palette.cyan[1].lerp(self.palette.live, 0.28), reflection_alpha);
			alpha = reflection_alpha + alpha * (1.0 - reflection_alpha);
		}
		(color, alpha)
	}

	fn core_color(&self, point: Vec3, direction: Vec3) -> Vec3 {
		let normal = (point - CORE_CENTER).normalize();
		let view = direction * -1.0;
		let halfway = (self.sun + view).normalize();
		let diffuse = normal.dot(self.sun).max(0.0);
		let specular = normal.dot(halfway).max(0.0).powi(72);
		let fresnel = (1.0 - normal.dot(view).max(0.0)).powi(3);
		let longitude = normal.z.atan2(normal.x);
		let band_phase = normal.y * 7.0 + longitude * 2.0 - self.angle * 1.35;
		let scan = (-(band_phase.sin() / 0.13).powi(2)).exp();
		let pulse = 0.58 + 0.42 * (self.angle * 2.4).sin();
		let mass = self
			.palette
			.ink
			.lerp(self.palette.live, self.activation * (0.18 + 0.24 * pulse));
		let reflected = direction.reflect(normal);
		(mass * (0.54 + 0.28 * diffuse)
			+ cyan_ramp(self.palette, reflected.z.atan2(reflected.x)) * (0.18 + fresnel * 0.64)
			+ self.palette.cyan[1] * (scan * self.activation * 0.34)
			+ self.palette.white * (specular * 0.78 + fresnel * 0.34))
			.clamp01()
	}

	fn ring_color(&self, radial: f32, local_angle: f32, normal: Vec3, direction: Vec3) -> Vec3 {
		let view = direction * -1.0;
		let halfway = (self.sun + view).normalize();
		let diffuse = normal.dot(self.sun).max(0.0);
		let specular = normal.dot(halfway).max(0.0).powi(96);
		let fresnel = (1.0 - normal.dot(view).max(0.0)).powi(3);
		let outer_edge =
			smooth(((radial - (RING_OUTER_RADIUS - RING_RIM_WIDTH)) / RING_RIM_WIDTH).clamp(0.0, 1.0));
		let inner_edge = smooth(((RING_INNER_RADIUS + 0.045 - radial) / 0.045).clamp(0.0, 1.0));
		let edge = (outer_edge + inner_edge).clamp(0.0, 1.0);
		let angle = local_angle - self.angle;
		let ticks = (angle * 18.0).cos().abs().powi(24);
		let live_distance = (angle - 0.42 + PI).rem_euclid(TAU) - PI;
		let live_arc = (-(live_distance / 0.22).powi(2)).exp() * self.activation;
		let body = self
			.palette
			.background
			.lerp(self.palette.glass, 0.20 + 0.24 * diffuse);
		(body * (0.48 + 0.24 * diffuse)
			+ self.palette.glass * (edge * 0.62 + fresnel * 0.28)
			+ self.palette.cyan[1] * (ticks * 0.14)
			+ self.palette.live * (live_arc * 0.72)
			+ self.palette.white * (specular * 0.96 + live_arc * 0.18))
			.clamp01()
	}

	fn pole_color(&self, point: Vec3, normal: Vec3, direction: Vec3) -> Vec3 {
		let view = direction * -1.0;
		let halfway = (self.sun + view).normalize();
		let diffuse = normal.dot(self.sun).max(0.0);
		let specular = normal.dot(halfway).max(0.0).powi(64);
		let fresnel = (1.0 - normal.dot(view).max(0.0)).powi(3);
		let height = ((point.y - POLE_BOTTOM) / (POLE_TOP - POLE_BOTTOM)).clamp(0.0, 1.0);
		let pulse_height = 0.5 + 0.5 * (self.angle * 1.8).sin();
		let pulse = (-((height - pulse_height) / 0.075).powi(2)).exp() * self.activation;
		(self.palette.cyan[1] * (0.38 + 0.40 * diffuse + 0.22 * fresnel)
			+ self.palette.white * (0.20 + 0.72 * specular + 0.82 * pulse)
			+ self.palette.live * (0.20 * pulse))
			.clamp01()
	}

	fn spark_color(&self, index: usize, point: Vec3, center: Vec3, direction: Vec3) -> Vec3 {
		let normal = (point - center).normalize();
		let view = direction * -1.0;
		let halfway = (self.sun + view).normalize();
		let diffuse = normal.dot(self.sun).max(0.0);
		let specular = normal.dot(halfway).max(0.0).powi(48);
		let fresnel = (1.0 - normal.dot(view).max(0.0)).powi(3);
		let base = if index == 0 {
			self.palette.live
		} else {
			self.palette.cyan[1]
		};
		(base * (0.42 + 0.58 * diffuse) + self.palette.white * (specular * 0.80 + fresnel * 0.48))
			.clamp01()
	}
}

impl Trace for Aperture {
	fn advance(&mut self, now: Duration) -> Camera {
		let elapsed = now.as_secs_f32();
		self.angle = orbit_rotation(elapsed);
		self.activation = activation_curve(elapsed);
		Camera {
			target:   CORE_CENTER,
			yaw:      self.pointer.1 + reveal_orbit(elapsed) + (elapsed * 0.24).sin() * 0.024,
			pitch:    CAMERA_PITCH,
			distance: CAMERA_DISTANCE,
			lift:     self.pointer.0.clamp(-0.42, 0.42) + (elapsed * 0.42).sin() * 0.018,
			focal:    CAMERA_FOCAL,
		}
	}

	/// Shades one supersample of the core, ring, vertical pole, probes, halo,
	/// and floor.
	fn shade(&self, ray: Ray) -> (Vec3, f32) {
		let Ray { origin, dir: direction } = ray;
		let (ground_color, ground_alpha) = self.ground(origin, direction);
		let mut color = ground_color * ground_alpha + self.palette.background * (1.0 - ground_alpha);
		let mut alpha = ground_alpha;

		let core_depth = sphere_depth(origin, direction, CORE_CENTER, CORE_RADIUS);
		let halo_distance = ray_distance(origin, direction, CORE_CENTER);
		let halo = smooth(((CORE_RADIUS + 0.24 - halo_distance) / 0.24).clamp(0.0, 1.0));
		let halo_alpha = halo * self.activation * 0.44;
		if halo_alpha > 0.0 {
			let halo_color = self.palette.cyan[1].lerp(self.palette.white, halo * 0.22);
			color = halo_color * halo_alpha + color * (1.0 - halo_alpha);
			alpha = halo_alpha + alpha * (1.0 - halo_alpha);
		}

		let mut nearest_depth = f32::INFINITY;
		let mut nearest_color = if core_depth.is_finite() {
			let point = origin + direction * core_depth;
			nearest_depth = core_depth;
			self.core_color(point, direction)
		} else {
			Vec3::ZERO
		};

		let (ring_normal, tangent, bitangent) = ring_basis();
		let ring_sample = ring_hit(origin, direction, ring_normal, tangent, bitangent).map(
			|(depth, _, radial, local_angle, facing)| {
				(depth, self.ring_color(radial, local_angle, facing, direction), ring_opacity(radial))
			},
		);

		if let Some((depth, point, normal)) = pole_hit(origin, direction)
			&& depth < nearest_depth
		{
			nearest_depth = depth;
			nearest_color = self.pole_color(point, normal, direction);
		}

		for index in 0..3 {
			let center = spark_position(index, self.angle);
			let depth = sphere_depth(origin, direction, center, SPARK_RADIUS);
			if depth < nearest_depth {
				let point = origin + direction * depth;
				nearest_depth = depth;
				nearest_color = self.spark_color(index, point, center, direction);
			}
		}

		if nearest_depth.is_finite() {
			color = nearest_color;
			alpha = 1.0;
		}
		if let Some((ring_depth, ring_color, ring_opacity)) = ring_sample
			&& ring_depth < nearest_depth
		{
			color = ring_color * ring_opacity + color * (1.0 - ring_opacity);
			alpha = ring_opacity + alpha * (1.0 - ring_opacity);
		}
		(tone(color, self.palette.tone_lift), alpha)
	}
}

/// Renders the scene at `elapsed` under the pointer camera and packs it
/// into braille cells through [`scene::rasterize`].
fn logo_cells(elapsed: f32, pointer: (f32, f32), palette: AperturePalette) -> LogoGrid {
	let mut aperture = Aperture::new(pointer, palette);
	let camera = aperture.advance(Duration::from_secs_f32(elapsed));
	let mut grid: LogoGrid = [[None; LOGO_COLS]; LOGO_ROWS];
	scene::rasterize(
		&aperture,
		&camera,
		LOGO_COLS as u16,
		LOGO_ROWS as u16,
		|x, y, glyph, color| {
			grid[y as usize][x as usize] = Some((glyph, color));
		},
	);
	grid
}

#[cfg(test)]
mod tests {
	use omp_core::Str;
	use omp_tui::{
		scene::{Ray, Trace},
		test_support::{frame_cell_style, frame_row_text},
	};

	use super::{
		Aperture, AperturePalette, CARD_ROWS, CORE_CENTER, Charset, Color, Duration, PI,
		RING_GLASS_OPACITY, RING_INNER_RADIUS, RING_OUTER_RADIUS, RING_RIM_WIDTH, Rect, Size, TAU,
		Theme, UiContext, Welcome, perimeter_angle, ring_basis, shortest_angle_delta,
	};
	use crate::{SessionRow, WelcomeLspServer};

	fn context() -> UiContext {
		UiContext { charset: Charset::NerdFont, ..UiContext::default() }
	}

	fn sessions() -> Vec<SessionRow> {
		[
			("1", "Optimize custom status widget rendering", "NOW"),
			("2", "Check Unicode character display", "01m"),
			("3", "Add ╰─ cursor shift", "02m"),
			("4", "The Unicode Similarity", "18m"),
		]
		.into_iter()
		.map(|(id, label, detail)| SessionRow {
			id:     Str::new(id),
			label:  Str::new(label),
			detail: Str::new(detail),
			pinned: false,
		})
		.collect()
	}

	fn rows(viewport: Size, elapsed_ms: u64) -> Vec<String> {
		let mut welcome = Welcome::new(&context(), sessions());
		let frame = welcome.render(viewport, Duration::from_millis(elapsed_ms));
		(0..frame.size().height)
			.map(|row| frame_row_text(frame, row))
			.collect()
	}

	fn has_lit_braille(rows: &[String]) -> bool {
		rows.iter().any(|row| {
			row.chars()
				.any(|glyph| ('\u{2801}'..='\u{28ff}').contains(&glyph))
		})
	}

	fn assert_angle(actual: f32, expected: f32) {
		let delta = shortest_angle_delta(actual, expected).abs();
		assert!(delta < 1.0e-6, "angle {actual} differs from {expected} by {delta}");
	}

	#[test]
	fn full_card_centers_sessions_and_hints() {
		let rows = rows(Size::new(100, 21), 2_000);
		// Card spans rows 2..=18 when centered in a 100x21 viewport.
		assert!(rows[2].contains("omp v"), "title chip in the top border: {}", rows[2]);
		assert!(rows[4].contains("RECENT SESSIONS") && rows[4].contains("03 / LOCAL"));
		assert!(rows[6].contains("New session"));
		assert!(rows[8].contains("Optimize custom status widget rendering"));
		assert!(rows[16].contains("COMMAND DECK"), "divider labels the command rail");
		assert!(rows[17].contains("actions") && rows[17].contains("resume"));
		assert!(
			!rows[15].contains("CPU RT")
				&& !rows[15].contains("32 RAYS")
				&& !rows[15].contains("LIVE"),
			"aperture footer has no runtime status labels: {}",
			rows[15]
		);
		assert!(rows[18].contains('└'), "bottom border closes the card");
		assert!(has_lit_braille(&rows), "the aperture renders lit braille cells");
	}
	#[test]
	fn pinned_session_uses_the_active_charset_icon() {
		use omp_tui::Icon;
		let ctx = context();
		let mut sessions = sessions();
		sessions[0].pinned = true;
		let mut welcome = Welcome::new(&ctx, sessions);
		let frame = welcome.render(Size::new(100, 21), Duration::from_millis(2_000));
		let pin = ctx.charset.icon(Icon::Pin);
		assert!(
			(0..frame.size().height).any(|row| frame_row_text(frame, row).contains(pin)),
			"pinned resume row renders the themed pin icon"
		);
	}

	#[test]
	fn full_card_renders_compact_lsp_facts_with_themed_statuses() {
		let mut welcome = Welcome::new(&context(), sessions());
		welcome.set_lsp_servers(vec![
			WelcomeLspServer {
				name:        Str::new_static("rust"),
				stage_label: Str::new_static("ready (rs)"),
				failed:      false,
			},
			WelcomeLspServer {
				name:        Str::new_static("ts"),
				stage_label: Str::new_static("failed (ts)"),
				failed:      true,
			},
		]);
		let frame = welcome.render(Size::new(100, 21), Duration::from_millis(2_000));
		let row = frame_row_text(frame, 14);
		assert!(row.contains("LSP"));
		assert!(row.contains("rust ready (rs)"));
		assert!(row.contains("ts failed (ts)"));
	}

	#[test]
	fn smol_card_drops_the_session_panel() {
		let rows = rows(Size::new(40, CARD_ROWS), 2_000);
		assert!(rows[0].contains("omp v"));
		assert!(rows.iter().all(|row| !row.contains("RECENT SESSIONS")));
		assert!(rows[15].contains("resume"));
		assert!(has_lit_braille(&rows));
	}

	#[test]
	fn bare_logo_survives_a_tiny_viewport() {
		let rows = rows(Size::new(29, 11), 2_000);
		assert!(has_lit_braille(&rows), "the logo still renders without the card");
		assert!(rows.iter().all(|row| !row.contains('┌')), "no card chrome at this size");
	}

	#[test]
	fn eclipse_backdrop_fills_the_margins() {
		let rows = rows(Size::new(100, 21), 2_000);
		// Card spans rows 2..=18; everything outside is half-block pixels.
		assert!(rows[0].chars().all(|glyph| glyph == '▀'), "top margin is backdrop: {}", rows[0]);
		assert!(rows[20].chars().all(|glyph| glyph == '▀'), "bottom margin is backdrop");
	}

	#[test]
	fn pointer_orbit_changes_the_rendered_logo() {
		let viewport = Size::new(100, 21);
		let settle = Duration::from_millis(1_000);
		let sample = Duration::from_millis(2_000);

		let mut centered = Welcome::new(&context(), sessions());
		centered.render(viewport, settle);
		let baseline: Vec<String> = {
			let frame = centered.render(viewport, sample);
			(0..frame.size().height)
				.map(|row| frame_row_text(frame, row))
				.collect()
		};

		let mut orbited = Welcome::new(&context(), sessions());
		// First render fixes the card bounds the pointer maps against; the
		// second, a full second later, lets the exponential chase converge.
		orbited.render(viewport, settle);
		orbited.point_at(98, 18);
		let frame = orbited.render(viewport, sample);
		let pointed: Vec<String> = (0..frame.size().height)
			.map(|row| frame_row_text(frame, row))
			.collect();

		assert_ne!(baseline, pointed, "pointing away from the logo center orbits the camera");
	}

	#[test]
	fn pointer_perimeter_maps_clockwise_from_the_top_left() {
		let mut welcome = Welcome::new(&context(), sessions());
		welcome.render(Size::new(100, 21), Duration::from_secs(1));
		for ((column, row), expected) in
			[((1, 2), 0.0), ((1, 18), PI * 0.5), ((98, 18), PI), ((98, 2), PI * 1.5)]
		{
			welcome.point_at(column, row);
			assert_angle(welcome.camera_target.1, expected);
		}

		let rect = Rect::new(10, 20, 101, 51);
		assert_angle(perimeter_angle(rect, 60, 21), PI * 1.75);
		assert_angle(perimeter_angle(rect, 60, 69), PI * 0.75);
		assert_angle(perimeter_angle(rect, 11, 45), PI * 0.25);
		assert_angle(perimeter_angle(rect, 109, 45), PI * 1.25);
	}

	#[test]
	fn pointer_orbit_crosses_zero_by_the_shortest_arc() {
		let forward = shortest_angle_delta(TAU - 0.05, 0.05);
		let backward = shortest_angle_delta(0.05, TAU - 0.05);
		assert!((forward - 0.10).abs() < 1.0e-5);
		assert!((backward + 0.10).abs() < 1.0e-5);
	}

	#[test]
	fn hovering_the_card_glows_the_border_near_the_pointer() {
		let viewport = Size::new(100, 21);
		let context = context();
		let mut welcome = Welcome::new(&context, sessions());
		welcome.render(viewport, Duration::from_millis(1_000));

		// Hover just under a bare stretch of the top border (clear of the
		// title and panel labels): one frame starts the tween, a later
		// frame samples it settled.
		welcome.point_at(27, 3);
		welcome.render(viewport, Duration::from_millis(1_050));
		let frame = welcome.render(viewport, Duration::from_millis(2_000));
		let near_pointer = frame_cell_style(frame, 27, 2).foreground_color();
		let far_border = frame_cell_style(frame, 8, 18).foreground_color();
		assert!(frame_row_text(frame, 2).contains('┌'), "the card stays on row 2 while hovered");
		assert_ne!(near_pointer, context.theme.border, "border glows near the pointer");
		assert_eq!(far_border, context.theme.border, "the glow stays local to the pointer");
	}

	#[test]
	fn card_chrome_uses_the_supplied_theme() {
		let mut context = context();
		context.theme = Theme::for_appearance(omp_tui::Appearance::Light);
		let mut welcome = Welcome::new(&context, sessions());
		let frame = welcome.render(Size::new(100, 21), Duration::from_secs(2));

		assert_eq!(frame_cell_style(frame, 8, 18).foreground_color(), context.theme.border);
		assert_eq!(frame_cell_style(frame, 2, 3).background_color(), context.theme.panel);
		assert_eq!(frame_cell_style(frame, 2, 17).background_color(), context.theme.shadow);
		let mut bare = Welcome::new(&context, sessions());
		let plate = bare.palette.plate;
		let frame = bare.render(Size::new(29, 11), Duration::from_secs(2));
		assert!(
			(0..frame.size().height).any(|row| {
				(0..frame.size().width)
					.any(|column| frame_cell_style(frame, column, row).background_color() == plate)
			}),
			"bare aperture uses the theme-derived plate"
		);
		assert_ne!(plate, Color::Rgb(0, 0, 0));
	}

	#[test]
	fn glass_ring_keeps_a_half_transparent_interior_and_solid_rim() {
		let mut aperture = Aperture::new((0.0, 0.0), AperturePalette::new(Theme::default()));
		aperture.activation = 1.0;
		let (normal, tangent, _) = ring_basis();
		let coverage_at = |radial: f32| {
			let target = CORE_CENTER + tangent * radial;
			Trace::shade(&aperture, Ray::new(target - normal * 2.0, normal)).1
		};
		let glass_radius = (RING_INNER_RADIUS + RING_OUTER_RADIUS - RING_RIM_WIDTH) * 0.5;
		assert!((coverage_at(glass_radius) - RING_GLASS_OPACITY).abs() < 1.0e-6);
		let rim_radius = RING_OUTER_RADIUS - RING_RIM_WIDTH * 0.5;
		assert!((coverage_at(rim_radius) - 1.0).abs() < 1.0e-6);
	}
}
