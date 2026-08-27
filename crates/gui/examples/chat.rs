//! The designed chat scene hosted in a GPU window.
//!
//! Run: `cargo run -p omp-gui --example chat`
//! Shots: `cargo run -p omp-gui --example chat -- --shot welcome|chat|picker
//! OUT.png`

use std::{
	env, fs, io,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	thread, time,
	time::{Duration, Instant},
};

use flume::{Receiver, Sender};
use omp_chat_ui::{
	BackendEvent, Chat, ChatKey, CommandPalette, CompactionBoundaries, GitFacts, Intent,
	ModelPicker, ModelRow, PaletteAction, PaletteEntry, PaletteEvent, PickerEvent, SessionRow,
	Sidebar, StatusFacts, ToolTerminal, ToolViewContent, Welcome, WelcomeEvent,
};
use omp_core::{Str, sf};
use omp_gui::{Effect, HostConfig, Scene, SceneFrame};
use omp_tui::{Graphics, Key, Layer, Mouse, MouseReport, Size, UiContext, paste::ClipboardRead};
use smallvec::SmallVec;

const FRAME_INTERVAL: Duration = Duration::from_millis(33);

fn main() {
	let mut args = env::args().skip(1);
	if args.next().as_deref() == Some("--shot") {
		let scene = args.next().unwrap_or_else(|| "chat".to_string());
		let out = args
			.next()
			.unwrap_or_else(|| "/tmp/omp-gui.png".to_string());
		shot(&scene, &out);
		return;
	}
	omp_gui::run(
		HostConfig { title: "omp — chat".to_string(), ..HostConfig::default() },
		ChatScene::new,
	);
}

struct ChatScene {
	ctx:      UiContext,
	started:  Instant,
	phase:    Phase,
	viewport: Size,
	events:   Receiver<BackendEvent>,
	intents:  Sender<Intent>,
	models:   Vec<ModelRow>,
	current:  usize,
}

enum Phase {
	Welcome(Box<Welcome>),
	Chat(Box<ChatState>),
}

struct ChatState {
	chat:    Chat,
	sidebar: Sidebar,
	overlay: Option<Overlay>,
}

impl ChatState {
	fn new(ctx: &UiContext, viewport: Size, facts: &StatusFacts) -> Self {
		let sidebar = Sidebar::new_hidden(facts, ctx);
		let mut chat = Chat::new(ctx);
		chat.set_status(facts.clone());
		chat.set_right_inset(sidebar.reserved(viewport));
		Self { chat, sidebar, overlay: None }
	}
}

enum Overlay {
	Models(ModelPicker),
	Palette(CommandPalette),
}

enum OverlayEvent {
	Consumed,
	Close,
	Pick(usize),
	Palette(PaletteAction),
}

impl Overlay {
	fn handle_key(&mut self, key: Key) -> OverlayEvent {
		match self {
			Self::Models(picker) => picker_event(picker.handle_key(key)),
			Self::Palette(palette) => palette_event(palette.handle_key(key)),
		}
	}

	fn handle_paste(&mut self, text: &str) -> OverlayEvent {
		match self {
			Self::Models(picker) => picker_event(picker.handle_paste(text)),
			Self::Palette(palette) => palette_event(palette.handle_paste(text)),
		}
	}

	fn handle_mouse(&mut self, col: u16, row: u16, kind: Mouse, viewport: Size) -> OverlayEvent {
		match self {
			Self::Models(picker) => picker_event(picker.handle_mouse(col, row, kind, viewport)),
			Self::Palette(palette) => palette_event(palette.handle_mouse(col, row, kind, viewport)),
		}
	}

	fn layer(&mut self, viewport: Size) -> Layer<'_> {
		match self {
			Self::Models(picker) => picker.layer(viewport),
			Self::Palette(palette) => palette.layer(viewport),
		}
	}
}

const fn picker_event(event: PickerEvent) -> OverlayEvent {
	match event {
		PickerEvent::Consumed => OverlayEvent::Consumed,
		PickerEvent::Close => OverlayEvent::Close,
		PickerEvent::Pick(index) => OverlayEvent::Pick(index),
	}
}

fn palette_event(event: PaletteEvent) -> OverlayEvent {
	match event {
		PaletteEvent::Consumed => OverlayEvent::Consumed,
		PaletteEvent::Close => OverlayEvent::Close,
		PaletteEvent::Run(action) => OverlayEvent::Palette(action),
	}
}

impl ChatScene {
	fn new(ctx: &UiContext) -> Self {
		let (events, intents) = mock_backend();
		Self {
			ctx: ctx.clone(),
			started: Instant::now(),
			phase: Phase::Welcome(Box::new(Welcome::new(ctx, Vec::new()))),
			viewport: Size::new(0, 0),
			events,
			intents,
			models: Vec::new(),
			current: 0,
		}
	}

	fn drain_backend(&mut self) {
		while let Ok(event) = self.events.try_recv() {
			match event {
				BackendEvent::OpenModelPicker { rows, current } => {
					self.models = rows;
					self.current = current.min(self.models.len().saturating_sub(1));
					if let Phase::Chat(state) = &mut self.phase
						&& !self.models.is_empty()
					{
						state.overlay = Some(Overlay::Models(ModelPicker::open(
							&self.models,
							self.current,
							&self.ctx,
						)));
					}
				},
				BackendEvent::ModelsUpdated { rows, current } => {
					self.models = rows;
					self.current = current.min(self.models.len().saturating_sub(1));
				},
				BackendEvent::Sessions(rows) => {
					if let Phase::Welcome(welcome) = &mut self.phase {
						welcome.set_sessions(rows);
					}
				},
				BackendEvent::Status(facts) => {
					if let Phase::Chat(state) = &mut self.phase {
						state.sidebar.set_status(&facts);
						state.chat.set_status(facts);
					}
				},
				event => {
					if let Phase::Chat(state) = &mut self.phase {
						let _ = state.chat.apply_backend_event(event);
					}
				},
			}
		}
	}

	fn apply_overlay(&mut self, event: OverlayEvent) -> Effect {
		let Phase::Chat(state) = &mut self.phase else {
			return Effect::Ignored;
		};
		match event {
			OverlayEvent::Consumed => Effect::Consumed,
			OverlayEvent::Close => {
				state.overlay = None;
				Effect::Consumed
			},
			OverlayEvent::Pick(index) => {
				if let Some(model) = self.models.get(index) {
					self.current = index;
					let _ = self.intents.send(Intent::SwitchModel(model.key.clone()));
				}
				state.overlay = None;
				Effect::Consumed
			},
			OverlayEvent::Palette(action) => match action {
				PaletteAction::Intent(intent) => {
					let quit = matches!(&intent, Intent::Quit);
					let _ = self.intents.send(intent);
					state.overlay = None;
					if quit { Effect::Quit } else { Effect::Consumed }
				},
				PaletteAction::OpenModelPicker => {
					state.overlay =
						Some(Overlay::Models(ModelPicker::open(&self.models, self.current, &self.ctx)));
					Effect::Consumed
				},
				PaletteAction::ToggleSidebar => {
					state.sidebar.toggle();
					state
						.chat
						.set_right_inset(state.sidebar.reserved(self.viewport));
					state.overlay = None;
					Effect::Consumed
				},
				PaletteAction::Insert(text) => {
					state.chat.set_composer_text(&text);
					state.overlay = None;
					Effect::Consumed
				},
			},
		}
	}
}

impl Scene for ChatScene {
	fn resize(&mut self, viewport: Size, _settled: bool) {
		self.viewport = viewport;
		if let Phase::Chat(state) = &mut self.phase {
			state.chat.set_right_inset(state.sidebar.reserved(viewport));
		}
	}

	fn render(&mut self) -> SceneFrame<'_> {
		self.drain_backend();
		let viewport = self.viewport;
		match &mut self.phase {
			Phase::Welcome(welcome) => SceneFrame {
				frame: welcome.render(viewport, self.started.elapsed()),
				viewport,
				editor_rows: viewport.height,
				layers: SmallVec::new(),
			},
			Phase::Chat(state) => {
				let mut layers = SmallVec::new();
				if let Some(layer) = state.sidebar.layer(viewport, Instant::now()) {
					layers.push(layer);
				}
				if let Some(overlay) = state.overlay.as_mut() {
					layers.push(overlay.layer(viewport));
				}
				let editor_rows = state.chat.composer_rows();
				let rendered = state.chat.render(viewport);
				SceneFrame { frame: rendered.frame, viewport, editor_rows, layers }
			},
		}
	}

	fn key(&mut self, key: Key) -> Effect {
		if let Phase::Welcome(welcome) = &mut self.phase {
			return match welcome.handle_key(key) {
				WelcomeEvent::Consumed => Effect::Consumed,
				WelcomeEvent::NewSession => {
					let _ = self.intents.send(Intent::NewSession);
					let facts = mock_status("Claude Sonnet", false);
					self.phase = Phase::Chat(Box::new(ChatState::new(&self.ctx, self.viewport, &facts)));
					Effect::Consumed
				},
				WelcomeEvent::Resume(id) => {
					let _ = self.intents.send(Intent::Resume(Some(id)));
					let facts = mock_status("Claude Sonnet", false);
					self.phase = Phase::Chat(Box::new(ChatState::new(&self.ctx, self.viewport, &facts)));
					Effect::Consumed
				},
				WelcomeEvent::Quit => Effect::Quit,
			};
		}
		let Phase::Chat(state) = &mut self.phase else {
			return Effect::Ignored;
		};
		if let Some(overlay) = state.overlay.as_mut() {
			let event = overlay.handle_key(key);
			return self.apply_overlay(event);
		}
		if key == Key::Ctrl('b') {
			state.sidebar.toggle();
			state
				.chat
				.set_right_inset(state.sidebar.reserved(self.viewport));
			return Effect::Consumed;
		}
		if key == Key::Ctrl('k') {
			state.overlay = Some(Overlay::Palette(CommandPalette::open(palette_entries(), &self.ctx)));
			return Effect::Consumed;
		}
		if state.sidebar.focused() {
			state.sidebar.handle_key(key);
			return Effect::Consumed;
		}
		if key == Key::Ctrl('p') || key == Key::Alt('p') {
			state.overlay =
				Some(Overlay::Models(ModelPicker::open(&self.models, self.current, &self.ctx)));
			return Effect::Consumed;
		}
		if let Some(scope) = ClipboardRead::for_key(key) {
			return Effect::Clipboard(scope);
		}
		if key == Key::Esc && state.chat.is_working() {
			let _ = self.intents.send(Intent::Abort);
			return Effect::Consumed;
		}
		let effect = match state.chat.handle_key(key) {
			ChatKey::Consumed => Effect::Consumed,
			ChatKey::Ignored => Effect::Ignored,
			ChatKey::Clear => {
				state.chat.clear_composer();
				Effect::Consumed
			},
			ChatKey::Exit => Effect::Quit,
			ChatKey::ToggleLive => {
				let _ = self.intents.send(Intent::ToggleLive);
				Effect::Consumed
			},
		};
		if let Some((text, attachments, mode)) = state.chat.take_submission() {
			let _ = self
				.intents
				.send(Intent::Submit { text, attachments, mode });
		}
		if let Some(text) = state.chat.take_copied() {
			return Effect::SetClipboard(text);
		}
		effect
	}

	fn mouse(&mut self, report: MouseReport) -> Effect {
		if let Phase::Welcome(welcome) = &mut self.phase {
			if matches!(report.kind, Mouse::Move | Mouse::Drag) {
				welcome.point_at(report.col, report.row);
			}
			return Effect::Consumed;
		}
		let Phase::Chat(state) = &mut self.phase else {
			return Effect::Consumed;
		};
		if let Some(overlay) = state.overlay.as_mut() {
			let event = overlay.handle_mouse(report.col, report.row, report.kind, self.viewport);
			return self.apply_overlay(event);
		}
		if !state
			.sidebar
			.handle_mouse(report.col, report.row, report.kind, self.viewport)
		{
			state.chat.handle_mouse(&report);
		}
		Effect::Consumed
	}

	fn paste(&mut self, text: &str, raw: bool) -> Effect {
		let Phase::Chat(state) = &mut self.phase else {
			return Effect::Consumed;
		};
		if let Some(overlay) = state.overlay.as_mut() {
			let event = overlay.handle_paste(text);
			return self.apply_overlay(event);
		}
		if !state.sidebar.focused() {
			if raw {
				state.chat.handle_paste_raw(text);
			} else {
				state.chat.handle_paste(text);
			}
		}
		Effect::Consumed
	}

	fn tick(&self) -> Duration {
		FRAME_INTERVAL
	}
}

fn palette_entries() -> Vec<PaletteEntry> {
	vec![
		PaletteEntry::new("Switch model", "Choose a model", PaletteAction::OpenModelPicker)
			.key("Ctrl+P"),
		PaletteEntry::new("Toggle sidebar", "Show session facts", PaletteAction::ToggleSidebar)
			.key("Ctrl+B"),
		PaletteEntry::new("Help", "Show controls", PaletteAction::Intent(Intent::Help)),
		PaletteEntry::new("Quit", "Leave chat", PaletteAction::Intent(Intent::Quit)),
	]
}

fn mock_backend() -> (Receiver<BackendEvent>, Sender<Intent>) {
	let (event_tx, event_rx) = flume::unbounded();
	let (intent_tx, intent_rx) = flume::unbounded();
	thread::spawn(move || run_mock(event_tx, intent_rx));
	(event_rx, intent_tx)
}

fn run_mock(events: Sender<BackendEvent>, intents: Receiver<Intent>) {
	let models = mock_models();
	let generation = Arc::new(AtomicU64::new(0));
	let mut current = 0;
	let _ = events.send(BackendEvent::Sessions(mock_sessions()));
	let _ = events.send(BackendEvent::ModelsUpdated { rows: models.clone(), current });
	while let Ok(intent) = intents.recv() {
		match intent {
			Intent::ExtensionShortcut(_) => {},
			Intent::Submit { text, attachments, mode: _ } => {
				let turn = generation.fetch_add(1, Ordering::SeqCst) + 1;
				let _ = events.send(BackendEvent::UserReplayed {
					text:  Str::from(text),
					chips: attachments
						.iter()
						.enumerate()
						.map(|(i, _)| Str::from(format!("attachment {}", i + 1)))
						.collect(),
				});
				let _ = events.send(BackendEvent::Status(mock_status(&models[current].name, true)));
				let id = Str::from(format!("assistant-{turn}"));
				let _ =
					events.send(BackendEvent::AssistantBegin { id: id.clone(), thinking: false });
				for text in ["Inspecting the scene… ", "preserving stable rows… ", "done."] {
					if generation.load(Ordering::SeqCst) != turn {
						break;
					}
					let _ =
						events.send(BackendEvent::AssistantDelta { id: id.clone(), text: sf!(text) });
					thread::sleep(Duration::from_millis(120));
				}
				let tool = Str::from(format!("tool-{turn}"));
				if generation.load(Ordering::SeqCst) == turn {
					let _ = events.send(BackendEvent::ToolStarted {
						id:    tool.clone(),
						name:  sf!("shell"),
						rev:   sf!("r0"),
						title: sf!("Inspect chat scene"),
					});
					let _ = events.send(BackendEvent::ToolOutput {
						id:    tool.clone(),
						chunk: sf!("checking damage ranges\n"),
					});
					let _ = events.send(BackendEvent::ToolFinished {
						id:       tool,
						terminal: ToolTerminal::Succeeded,
						view:     ToolViewContent::Plain(sf!("Host seam verified")),
					});
				}
				if generation.load(Ordering::SeqCst) == turn {
					let _ = events.send(BackendEvent::AssistantEnd { id });
					let _ = events.send(BackendEvent::Ack { interrupted: false });
					let _ = events.send(BackendEvent::Status(mock_status(&models[current].name, false)));
				}
			},
			Intent::Abort => {
				generation.fetch_add(1, Ordering::SeqCst);
				let _ = events.send(BackendEvent::Ack { interrupted: true });
				let _ = events.send(BackendEvent::Status(mock_status(&models[current].name, false)));
			},
			Intent::SwitchModel(key) => {
				if let Some(index) = models.iter().position(|row| row.key == key) {
					current = index;
				}
				let _ = events.send(BackendEvent::Status(mock_status(&models[current].name, false)));
			},
			Intent::NewSession => {
				let _ = events.send(BackendEvent::HistoryCleared);
			},
			Intent::Resume(_) => {
				let _ = events.send(BackendEvent::UserReplayed {
					text:  sf!("Continue the previous session."),
					chips: Vec::new(),
				});
			},
			Intent::Help => {
				let _ = events.send(BackendEvent::Notice(sf!(
					"Ctrl+P models · Ctrl+K commands · Ctrl+B sidebar",
				)));
			},
			Intent::Quit => break,
			_ => {},
		}
	}
}

fn mock_models() -> Vec<ModelRow> {
	[
		("anthropic/claude-sonnet", "Claude Sonnet", "anthropic", "Anthropic"),
		("openai/gpt-5", "GPT-5", "openai", "OpenAI"),
		("google/gemini-pro", "Gemini Pro", "google", "Google"),
	]
	.into_iter()
	.map(|(key, name, provider_id, provider)| ModelRow {
		key:         Str::from(key),
		name:        Str::from(name),
		color:       None,
		provider_id: Str::from(provider_id),
		provider:    Str::from(provider),
		context:     Some(200_000),
		input_mtok:  Some(3.0),
		output_mtok: Some(15.0),
	})
	.collect()
}

fn mock_sessions() -> Vec<SessionRow> {
	[
		("one", "Optimize custom status widget rendering", "NOW"),
		("two", "Check Unicode character display", "01m"),
		("three", "Add cursor shift", "02m"),
	]
	.into_iter()
	.map(|(id, label, detail)| SessionRow {
		id:     Str::from(id),
		label:  Str::from(label),
		detail: Str::from(detail),
		pinned: false,
	})
	.collect()
}

fn mock_status(model: &str, working: bool) -> StatusFacts {
	StatusFacts {
		model: Str::from(model),
		working,
		turn_started: working.then(Instant::now),
		context_tokens: 391_000,
		context_window: Some(1_000_000),
		cost_nanos: 8_650_000_000,
		queued: 0,
		jobs: usize::from(working),
		attempt: 0,
		dropped: 0,
		git: Some(GitFacts { branch: sf!("main"), dirty: 5, staged: 9 }),
		compaction_boundaries: Some(CompactionBoundaries {
			threshold_percent:   80.0,
			speculation_percent: Some(70.0),
		}),
		..StatusFacts::default()
	}
}
/// Renders one scripted scene to a PNG without a window, for pixel-level
/// verification (`--shot welcome|chat|picker OUT.png`).
fn shot(name: &str, out: &str) {
	use omp_gui::{Compositor, Fonts, Gpu, GuiTheme, Painter, View};
	use omp_tui::Charset;

	let gpu = Gpu::new(None).expect("gpu");
	let format = wgpu::TextureFormat::Rgba8Unorm;
	let mut painter = Painter::new(&gpu, format);
	let mut fonts = Fonts::new().expect("fonts");
	println!(
		"family: {} nerd: {} italic: {}",
		fonts.family(),
		fonts.has_nerd_font(),
		fonts.has_italic()
	);
	let scale = 2.0_f32;
	let px = 14.0 * scale;
	let ctx = UiContext {
		charset: if fonts.has_nerd_font() {
			Charset::NerdFont
		} else {
			Charset::Unicode
		},
		graphics: Graphics::KittyPlaceholders,
		native_decor: true,
		..UiContext::default()
	};
	// Opaque backdrop so shots read on any viewer.
	let theme = GuiTheme::from_ctx(&ctx, 1.0);
	let mut scene = ChatScene::new(&ctx);
	let viewport = Size::new(104, 30);
	scene.resize(viewport, true);

	let metrics = fonts.cell_metrics(px);
	let margin = 10.0 * scale;
	let width = margin
		.mul_add(2.0, f32::from(viewport.width) * metrics.advance)
		.ceil() as u32;
	let height = margin
		.mul_add(2.0, f32::from(viewport.height) * metrics.line_height)
		.ceil() as u32;

	let wait = match name {
		"welcome" => Duration::from_millis(1600),
		_ => Duration::from_millis(5200),
	};
	if name != "welcome" {
		scene.key(Key::Enter);
		scene.paste("Show the designed chat scene", false);
		scene.key(Key::Enter);
	}
	thread::sleep(wait);
	if name == "picker" {
		scene.key(Key::Ctrl('p'));
	}

	// Wall-clock seed so successive shots capture different shimmer phases;
	// the scene's own animations still run off `scene.started`.
	let now = time::SystemTime::now()
		.duration_since(time::UNIX_EPOCH)
		.unwrap_or_else(|_| scene.started.elapsed());
	let frame = scene.render();
	let view = View {
		window: [width as f32, height as f32],
		origin: [margin, margin],
		scroll: 0.0,
		cursor_on: true,
		selection: None,
		now,
	};
	let mut compositor = Compositor::default();
	let instances = compositor.build(&frame, &mut fonts, &theme, &view, px);
	let (mask, color) = fonts.take_uploads();
	painter.upload_atlas(&gpu, &mask, &color);

	let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
		label: Some("shot-target"),
		size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
		mip_level_count: 1,
		sample_count: 1,
		dimension: wgpu::TextureDimension::D2,
		format,
		usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
		view_formats: &[],
	});
	let target_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
	painter.draw(
		&gpu,
		&target_view,
		width,
		height,
		&instances.batches,
		&instances.rects,
		&instances.glyphs,
	);

	let row_bytes = width * 4;
	let aligned = row_bytes.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
	let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
		label:              Some("shot-readback"),
		size:               u64::from(aligned) * u64::from(height),
		usage:              wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
		mapped_at_creation: false,
	});
	let mut encoder = gpu
		.device
		.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("shot-copy") });
	encoder.copy_texture_to_buffer(
		wgpu::TexelCopyTextureInfo {
			texture:   &texture,
			mip_level: 0,
			origin:    wgpu::Origin3d::ZERO,
			aspect:    wgpu::TextureAspect::All,
		},
		wgpu::TexelCopyBufferInfo {
			buffer: &buffer,
			layout: wgpu::TexelCopyBufferLayout {
				offset:         0,
				bytes_per_row:  Some(aligned),
				rows_per_image: Some(height),
			},
		},
		wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
	);
	gpu.queue.submit([encoder.finish()]);

	let slice = buffer.slice(..);
	let (sender, receiver) = flume::bounded(1);
	slice.map_async(wgpu::MapMode::Read, move |result| {
		sender.send(result).expect("map channel open");
	});
	gpu.device
		.poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
		.expect("device poll");
	receiver
		.recv()
		.expect("map channel result")
		.expect("buffer map");

	let mut rgba = vec![0_u8; (row_bytes * height) as usize];
	{
		let data = slice.get_mapped_range().expect("mapped range");
		for row in 0..height as usize {
			let src = &data[row * aligned as usize..][..row_bytes as usize];
			rgba[row * row_bytes as usize..][..row_bytes as usize].copy_from_slice(src);
		}
	}
	let file = fs::File::create(out).expect("create output png");
	let mut encoder = png::Encoder::new(io::BufWriter::new(file), width, height);
	encoder.set_color(png::ColorType::Rgba);
	encoder.set_depth(png::BitDepth::Eight);
	encoder
		.write_header()
		.expect("png header")
		.write_image_data(&rgba)
		.expect("png pixels");
	println!("wrote {out} ({width}x{height})");
}
