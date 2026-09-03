//! Streaming assistant speech (pi `tts/vocalizer.ts`, event-controller
//! routing at `modes/controllers/event-controller.ts:1135-1166` and
//! `:1340-1349`).
//!
//! The [`Vocalizer`] turns the assistant's streaming output into spoken audio
//! as a side effect of the turn. Deltas run through
//! [`SpeakableStream`] — which drops code, tables, and markup and cuts
//! speakable segments the moment a boundary appears — and every ready segment
//! is queued for a background worker that synthesizes it through the host's
//! [`SpeechSynth`] and plays it on one gapless [`PlaybackStream`] per
//! utterance. An idle timer speaks the buffered partial when generation
//! stalls (tool call, thinking block), and [`Vocalizer::clear`] stops
//! playback at once and drops everything queued — wired to a new user
//! message and to the Esc/Ctrl+C interrupt.
//!
//! Mode routing (pi `speech.mode`): `assistant` and `all` speak text deltas,
//! `all` also speaks thinking, `yield` speaks nothing live and the whole
//! final message at turn end, `off` speaks nothing. The host reads the mode
//! with [`Vocalizer::mode`] and passes it to every call so the app's
//! `cl_speech_mode` declaration stays the single owner of the setting.
//!
//! Synthesis and playback failures never reach the turn: they are
//! debug-logged and the audio is dropped (pi swallows them the same way).

use std::{
	future::Future,
	pin::Pin,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::Duration,
};

use flume::{Receiver, Sender};
use omp_con::{Ctx, Value};
use omp_core::Str;
use omp_voice::{audio::PlaybackStream, segmentation::SpeakableStream};
use parking_lot::Mutex;
use tokio::{sync::Notify, time::Instant};

/// Quiet time on the delta stream before the buffered partial is spoken
/// (pi `vocalizer.ts` `IDLE_FLUSH_MS`).
const IDLE_FLUSH: Duration = Duration::from_millis(1000);

/// Which assistant channels are vocalized (pi `speech.mode`, plus `off` for
/// the disabled state so a caller carries one value).
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
	strum::VariantNames,
)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum SpeechMode {
	/// Speak assistant text as it streams.
	Assistant,
	/// Speak assistant text and thinking as they stream.
	All,
	/// Speak only the final message once the turn ends.
	Yield,
	/// Speech disabled.
	#[default]
	Off,
}

omp_con::con_enum!(SpeechMode);

impl SpeechMode {
	/// Whether streamed text deltas are spoken live.
	const fn speaks_text(self) -> bool {
		matches!(self, Self::Assistant | Self::All)
	}
}

/// One synthesized utterance segment: mono PCM at `sample_rate`.
pub struct SynthAudio {
	/// Samples per second.
	pub sample_rate: u32,
	/// Mono `f32` samples in `[-1, 1]`.
	pub samples:     Vec<f32>,
}

/// Text-to-speech backend supplied by the application.
pub trait SpeechSynth: Send + Sync + 'static {
	/// Synthesizes one speakable segment.
	fn synthesize(
		&self,
		text: Str,
	) -> Pin<Box<dyn Future<Output = Result<SynthAudio, Str>> + Send + '_>>;
}

/// Worker queue entry; `generation` is compared against the live generation
/// so a [`Vocalizer::clear`] invalidates everything already queued.
enum Job {
	/// Synthesize and play one segment.
	Speak { generation: u64, text: Str },
	/// Utterance end: drain playback and release the speaker.
	End { generation: u64 },
}

impl Job {
	const fn generation(&self) -> u64 {
		match self {
			Self::Speak { generation, .. } | Self::End { generation } => *generation,
		}
	}
}

/// State shared between the host-side [`Vocalizer`], the synthesis worker,
/// and the idle-flush timer.
struct Shared {
	/// Bumped by `clear`; jobs from older generations are dropped unplayed.
	generation:    AtomicU64,
	/// Generation of the utterance whose playback session is open (`0` when
	/// none); `speaking` while it matches the live generation.
	open:          AtomicU64,
	/// Current playback session and its sample rate.
	playback:      Mutex<Option<(u32, PlaybackStream)>>,
	/// Markdown → speakable segment transform for the current utterance.
	speakable:     Mutex<SpeakableStream>,
	/// When the idle timer should speak the buffered partial.
	idle_deadline: Mutex<Option<Instant>>,
	/// Wakes the idle timer on re-arm and on shutdown.
	idle:          Notify,
	/// Set when the owning `Vocalizer` drops; the idle task exits.
	closed:        AtomicBool,
}

impl Shared {
	fn live(&self, generation: u64) -> bool {
		self.generation.load(Ordering::Acquire) == generation
	}

	/// Queues ready segments for the worker.
	fn enqueue(&self, tx: &Sender<Job>, segments: Vec<Str>) {
		if segments.is_empty() {
			return;
		}
		let generation = self.generation.load(Ordering::Acquire);
		for text in segments {
			let _ = tx.send(Job::Speak { generation, text });
		}
	}

	/// Appends synthesized audio to the open session, opening (or reopening
	/// at a new sample rate) the speaker on demand.
	async fn play(&self, generation: u64, audio: SynthAudio) {
		if audio.samples.is_empty() {
			return;
		}
		let stale = {
			let mut slot = self.playback.lock();
			match slot.as_mut() {
				Some((rate, stream)) if *rate != audio.sample_rate => {
					stream.finish_input();
					Some(stream.state())
				},
				_ => None,
			}
		};
		if let Some(state) = stale {
			state.wait_for_drain().await;
			self.playback.lock().take();
			if !self.live(generation) {
				return;
			}
		}
		let mut slot = self.playback.lock();
		if slot.is_none() {
			match PlaybackStream::start(audio.sample_rate) {
				Ok(stream) => *slot = Some((audio.sample_rate, stream)),
				Err(error) => {
					tracing::debug!(error = %error, "vocalizer playback unavailable; dropping audio");
					return;
				},
			}
		}
		let written = slot
			.as_ref()
			.map(|(_, stream)| stream.writer().and_then(|writer| writer.write_owned(audio.samples)));
		if let Some(Err(error)) = written {
			tracing::debug!(error = %error, "vocalizer playback write failed");
			slot.take();
		}
	}

	/// Finishes the open session and waits until its audio has reached the
	/// speaker (or `clear` aborted it), then releases the device.
	async fn drain(&self) {
		let state = {
			let mut slot = self.playback.lock();
			slot.as_mut().map(|(_, stream)| {
				stream.finish_input();
				stream.state()
			})
		};
		let Some(state) = state else { return };
		state.wait_for_drain().await;
		self.playback.lock().take();
	}

	/// Stops playback immediately and drops the open session.
	fn abort_playback(&self) {
		let open = self.playback.lock().take();
		if let Some((_, mut stream)) = open {
			let _ = stream.abort();
		}
	}
}

/// Synthesis worker: synthesizes queued segments in order and feeds one
/// gapless playback session per utterance, so sequential utterances never
/// overlap (pi `#chain`). Exits once every sender is gone.
async fn worker(rx: Receiver<Job>, synth: Arc<dyn SpeechSynth>, shared: Arc<Shared>) {
	while let Ok(job) = rx.recv_async().await {
		let generation = job.generation();
		if !shared.live(generation) {
			continue;
		}
		match job {
			Job::Speak { text, .. } => {
				shared.open.store(generation, Ordering::Release);
				let audio = synth.synthesize(text).await;
				if !shared.live(generation) {
					continue;
				}
				match audio {
					Ok(audio) => shared.play(generation, audio).await,
					Err(error) => tracing::debug!(error = %error, "vocalizer synthesis failed"),
				}
			},
			Job::End { .. } => {
				shared.drain().await;
				let _ = shared.open.compare_exchange(
					generation,
					0,
					Ordering::AcqRel,
					Ordering::Acquire,
				);
			},
		}
	}
}

/// Idle-flush timer (pi `#armIdle`): when no delta arrives for
/// [`IDLE_FLUSH`], speaks the buffered partial instead of holding it through
/// a tool call or thinking block.
async fn idle_flush(tx: Sender<Job>, shared: Arc<Shared>) {
	while !shared.closed.load(Ordering::Acquire) {
		let deadline = *shared.idle_deadline.lock();
		let Some(at) = deadline else {
			shared.idle.notified().await;
			continue;
		};
		tokio::select! {
			biased;
			() = shared.idle.notified() => {},
			() = tokio::time::sleep_until(at) => {
				let fire = {
					let mut slot = shared.idle_deadline.lock();
					let due = *slot == Some(at);
					if due {
						*slot = None;
					}
					due
				};
				if fire {
					let segments = shared.speakable.lock().flush_idle();
					shared.enqueue(&tx, segments);
				}
			},
		}
	}
}

/// Streaming assistant vocalizer (pi `Vocalizer`).
///
/// Every method is non-blocking on the host thread; synthesis and playback
/// run on a worker spawned onto the current tokio runtime, or onto a
/// dedicated thread with its own runtime when none is current.
pub struct Vocalizer {
	shared: Arc<Shared>,
	tx:     Sender<Job>,
	rx:     Receiver<Job>,
}

impl Vocalizer {
	/// Starts the synthesis worker over `synth`.
	#[must_use]
	pub fn new(synth: Arc<dyn SpeechSynth>) -> Self {
		let shared = Arc::new(Shared {
			generation:    AtomicU64::new(1),
			open:          AtomicU64::new(0),
			playback:      Mutex::new(None),
			speakable:     Mutex::new(SpeakableStream::new()),
			idle_deadline: Mutex::new(None),
			idle:          Notify::new(),
			closed:        AtomicBool::new(false),
		});
		let (tx, rx) = flume::unbounded();
		let work = worker(rx.clone(), synth, Arc::clone(&shared));
		let idle = idle_flush(tx.clone(), Arc::clone(&shared));
		if let Ok(runtime) = tokio::runtime::Handle::try_current() {
			runtime.spawn(work);
			runtime.spawn(idle);
		} else {
			let spawned = std::thread::Builder::new()
				.name("omp-vocalizer".to_owned())
				.spawn(move || {
					let runtime = tokio::runtime::Builder::new_current_thread()
						.enable_time()
						.build();
					match runtime {
						Ok(runtime) => runtime.block_on(async {
							tokio::join!(work, idle);
						}),
						Err(error) => {
							tracing::warn!(error = %error, "vocalizer runtime unavailable; speech disabled");
						},
					}
				});
			if let Err(error) = spawned {
				tracing::warn!(error = %error, "vocalizer thread unavailable; speech disabled");
			}
		}
		Self { shared, tx, rx }
	}

	/// Effective speech mode from the console: `cl_speech_enabled == false`
	/// or an absent/unknown `cl_speech_mode` reads as [`SpeechMode::Off`].
	#[must_use]
	pub fn mode(con: &Ctx) -> SpeechMode {
		if matches!(con.get("cl_speech_enabled"), Some(Value::Bool(false))) {
			return SpeechMode::Off;
		}
		con.get("cl_speech_mode")
			.and_then(|value| value.as_str()?.parse().ok())
			.unwrap_or(SpeechMode::Off)
	}

	/// Streams an assistant text delta (`assistant` and `all` modes; pi
	/// `event-controller.ts:1145`).
	pub fn push_text(&mut self, mode: SpeechMode, delta: &str) {
		if mode.speaks_text() {
			self.push_delta(delta);
		}
	}

	/// Streams a thinking delta (`all` mode only; pi
	/// `event-controller.ts:1147`).
	pub fn push_thinking(&mut self, mode: SpeechMode, delta: &str) {
		if mode == SpeechMode::All {
			self.push_delta(delta);
		}
	}

	/// Speaks the trailing partial sentence of a completed assistant message
	/// (pi `event-controller.ts:1346`); `yield` waits for
	/// [`turn_ended`](Self::turn_ended). An aborted message must go through
	/// [`clear`](Self::clear) instead, never here.
	pub fn message_completed(&mut self, mode: SpeechMode) {
		if mode.speaks_text() {
			self.flush();
		}
	}

	/// End of turn (pi `#handleTurnEnd`): `yield` speaks the whole final
	/// message in one shot; every other mode flushes the live buffer.
	pub fn turn_ended(&mut self, mode: SpeechMode, final_text: &str) {
		match mode {
			SpeechMode::Yield => {
				if !final_text.is_empty() {
					self.push_delta(final_text);
					self.flush();
				}
			},
			SpeechMode::Assistant | SpeechMode::All => self.flush(),
			SpeechMode::Off => {},
		}
	}

	/// Drops every queued segment, stops playback at once, and discards the
	/// buffered partial (pi `clear`: new user message, Esc interrupt).
	pub fn clear(&mut self) {
		self.shared.generation.fetch_add(1, Ordering::AcqRel);
		self.disarm_idle();
		*self.shared.speakable.lock() = SpeakableStream::new();
		self.rx.drain().for_each(drop);
		self.shared.abort_playback();
	}

	/// Silences playback immediately (Esc rung 4); identical to
	/// [`clear`](Self::clear).
	pub fn silence(&mut self) {
		self.clear();
	}

	/// Whether anything is queued, synthesizing, or audible.
	#[must_use]
	pub fn speaking(&self) -> bool {
		!self.rx.is_empty()
			|| self.shared.open.load(Ordering::Acquire) == self.shared.generation.load(Ordering::Acquire)
	}

	fn push_delta(&self, delta: &str) {
		if delta.is_empty() {
			return;
		}
		let segments = self.shared.speakable.lock().push(delta);
		self.shared.enqueue(&self.tx, segments);
		*self.shared.idle_deadline.lock() = Some(Instant::now() + IDLE_FLUSH);
		self.shared.idle.notify_one();
	}

	/// Closes the current utterance: drains the trailing partial and ends
	/// the playback session after it (pi `flush`).
	fn flush(&self) {
		self.disarm_idle();
		let segments = {
			let mut speakable = self.shared.speakable.lock();
			let segments = speakable.flush();
			*speakable = SpeakableStream::new();
			segments
		};
		self.shared.enqueue(&self.tx, segments);
		let generation = self.shared.generation.load(Ordering::Acquire);
		let _ = self.tx.send(Job::End { generation });
	}

	fn disarm_idle(&self) {
		self.shared.idle_deadline.lock().take();
		self.shared.idle.notify_one();
	}
}

impl Drop for Vocalizer {
	fn drop(&mut self) {
		self.shared.closed.store(true, Ordering::Release);
		self.clear();
	}
}

/// Console user slot holding the host's vocalizer for `cl_voice_silence`.
pub struct VoiceSlot(pub Arc<Mutex<Vocalizer>>);

/// Attaches `vocalizer` to `con` so `cl_voice_silence` can reach it.
pub fn install(con: &Ctx, vocalizer: Arc<Mutex<Vocalizer>>) {
	con.insert_user(VoiceSlot(vocalizer));
}

omp_con::cmd! {
	/// Silence the vocalizer immediately (Esc rung 4).
	cl_voice_silence() = |ctx, _args| {
		if let Some(slot) = ctx.user::<VoiceSlot>() {
			slot.0.lock().silence();
		}
		Ok(())
	};
}

#[cfg(test)]
mod tests {
	use omp_con::{DynamicVarSpec, TypeSpec, VarFlags};

	use super::*;

	struct FakeSynth {
		log: Mutex<Vec<Str>>,
	}

	impl FakeSynth {
		fn new() -> Arc<Self> {
			Arc::new(Self { log: Mutex::new(Vec::new()) })
		}

		fn spoken(&self) -> Vec<Str> {
			self.log.lock().clone()
		}

		/// Yields to the worker until `count` segments were synthesized.
		async fn wait_for(&self, count: usize) {
			let deadline = Instant::now() + Duration::from_secs(3);
			while self.log.lock().len() < count {
				assert!(Instant::now() < deadline, "synth log stalled at {:?}", self.spoken());
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		}
	}

	impl SpeechSynth for FakeSynth {
		fn synthesize(
			&self,
			text: Str,
		) -> Pin<Box<dyn Future<Output = Result<SynthAudio, Str>> + Send + '_>> {
			Box::pin(async move {
				self.log.lock().push(text);
				Ok(SynthAudio { sample_rate: 24_000, samples: vec![0.0; 240] })
			})
		}
	}

	/// Lets the worker run and playback settle.
	async fn settle(vocalizer: &Vocalizer) {
		let deadline = Instant::now() + Duration::from_secs(3);
		while vocalizer.speaking() && Instant::now() < deadline {
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}

	fn spoken_contains(spoken: &[Str], needle: &str) -> bool {
		spoken.iter().any(|segment| segment.as_str().contains(needle))
	}

	const TEXT: &str = "Hello there, this is a spoken sentence. ";
	const THINKING: &str = "Secret deliberation that stays private. ";

	#[tokio::test]
	async fn assistant_mode_speaks_text_not_thinking() {
		let synth = FakeSynth::new();
		let mut vocalizer = Vocalizer::new(synth.clone());
		vocalizer.push_text(SpeechMode::Assistant, TEXT);
		vocalizer.push_thinking(SpeechMode::Assistant, THINKING);
		vocalizer.message_completed(SpeechMode::Assistant);
		synth.wait_for(1).await;
		settle(&vocalizer).await;
		let spoken = synth.spoken();
		assert!(spoken_contains(&spoken, "Hello there"), "{spoken:?}");
		assert!(!spoken_contains(&spoken, "Secret"), "{spoken:?}");
	}

	#[tokio::test]
	async fn all_mode_speaks_thinking_too() {
		let synth = FakeSynth::new();
		let mut vocalizer = Vocalizer::new(synth.clone());
		vocalizer.push_thinking(SpeechMode::All, THINKING);
		vocalizer.push_text(SpeechMode::All, TEXT);
		vocalizer.message_completed(SpeechMode::All);
		synth.wait_for(2).await;
		settle(&vocalizer).await;
		let spoken = synth.spoken();
		assert!(spoken_contains(&spoken, "Secret deliberation"), "{spoken:?}");
		assert!(spoken_contains(&spoken, "Hello there"), "{spoken:?}");
		assert!(!vocalizer.speaking());
	}

	#[tokio::test]
	async fn yield_mode_speaks_only_at_turn_end() {
		let synth = FakeSynth::new();
		let mut vocalizer = Vocalizer::new(synth.clone());
		vocalizer.push_text(SpeechMode::Yield, TEXT);
		vocalizer.push_thinking(SpeechMode::Yield, THINKING);
		vocalizer.message_completed(SpeechMode::Yield);
		assert!(!vocalizer.speaking());
		settle(&vocalizer).await;
		assert!(synth.spoken().is_empty());
		vocalizer.turn_ended(SpeechMode::Yield, "The final answer is forty-two.");
		synth.wait_for(1).await;
		settle(&vocalizer).await;
		let spoken = synth.spoken();
		assert_eq!(spoken.len(), 1, "{spoken:?}");
		assert!(spoken_contains(&spoken, "final answer is forty-two"), "{spoken:?}");
	}

	#[tokio::test]
	async fn off_mode_speaks_nothing() {
		let synth = FakeSynth::new();
		let mut vocalizer = Vocalizer::new(synth.clone());
		vocalizer.push_text(SpeechMode::Off, TEXT);
		vocalizer.push_thinking(SpeechMode::Off, THINKING);
		vocalizer.message_completed(SpeechMode::Off);
		vocalizer.turn_ended(SpeechMode::Off, TEXT);
		assert!(!vocalizer.speaking());
		settle(&vocalizer).await;
		assert!(synth.spoken().is_empty());
	}

	#[tokio::test]
	async fn clear_drops_queued_segments_and_bumps_generation() {
		let synth = FakeSynth::new();
		let mut vocalizer = Vocalizer::new(synth.clone());
		let paragraph = "First sentence of a long reply. Second sentence follows it. \
		                 Third sentence keeps going. Fourth sentence is here too. \
		                 Fifth sentence ends the paragraph. ";
		vocalizer.push_text(SpeechMode::Assistant, paragraph);
		assert!(vocalizer.speaking(), "segments are queued before the worker runs");
		let before = vocalizer.shared.generation.load(Ordering::Acquire);
		vocalizer.clear();
		assert_eq!(vocalizer.shared.generation.load(Ordering::Acquire), before + 1);
		assert!(!vocalizer.speaking());
		settle(&vocalizer).await;
		assert!(synth.spoken().is_empty(), "{:?}", synth.spoken());

		vocalizer.push_text(SpeechMode::Assistant, "A fresh sentence after the clear. ");
		vocalizer.message_completed(SpeechMode::Assistant);
		synth.wait_for(1).await;
		settle(&vocalizer).await;
		let spoken = synth.spoken();
		assert_eq!(spoken.len(), 1, "{spoken:?}");
		assert!(spoken_contains(&spoken, "fresh sentence"), "{spoken:?}");
	}

	#[tokio::test]
	async fn message_completed_flushes_partial_sentence() {
		let synth = FakeSynth::new();
		let mut vocalizer = Vocalizer::new(synth.clone());
		vocalizer.push_text(SpeechMode::Assistant, "Trailing partial");
		tokio::time::sleep(Duration::from_millis(30)).await;
		assert!(synth.spoken().is_empty(), "no boundary yet");
		vocalizer.message_completed(SpeechMode::Assistant);
		synth.wait_for(1).await;
		settle(&vocalizer).await;
		let spoken = synth.spoken();
		assert_eq!(spoken.len(), 1, "{spoken:?}");
		assert!(spoken_contains(&spoken, "Trailing partial"), "{spoken:?}");
		assert!(!vocalizer.speaking());
	}

	#[test]
	fn works_without_a_current_runtime() {
		let synth = FakeSynth::new();
		let mut vocalizer = Vocalizer::new(synth.clone());
		vocalizer.push_text(SpeechMode::Assistant, TEXT);
		vocalizer.message_completed(SpeechMode::Assistant);
		let deadline = std::time::Instant::now() + Duration::from_secs(3);
		while synth.log.lock().is_empty() {
			assert!(std::time::Instant::now() < deadline, "dedicated worker thread never ran");
			std::thread::sleep(Duration::from_millis(5));
		}
		assert!(spoken_contains(&synth.spoken(), "Hello there"));
		while vocalizer.speaking() && std::time::Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(5));
		}
		assert!(!vocalizer.speaking());
	}

	#[test]
	fn mode_reads_console_var_by_name() {
		let ctx = Ctx::builder().isolated().build();
		assert_eq!(Vocalizer::mode(&ctx), SpeechMode::Off);
		ctx.register_dynamic_var(DynamicVarSpec {
			name:    Str::new_static("cl_speech_mode"),
			desc:    Str::new_static("test"),
			ty:      TypeSpec::STR,
			flags:   VarFlags::NONE,
			default: Value::Str(Str::new_static("all")),
		})
		.expect("registers");
		assert_eq!(Vocalizer::mode(&ctx), SpeechMode::All);
		ctx.register_dynamic_var(DynamicVarSpec {
			name:    Str::new_static("cl_speech_enabled"),
			desc:    Str::new_static("test"),
			ty:      TypeSpec::BOOL,
			flags:   VarFlags::NONE,
			default: Value::Bool(false),
		})
		.expect("registers");
		assert_eq!(Vocalizer::mode(&ctx), SpeechMode::Off);
	}

	#[tokio::test]
	async fn silence_command_is_registered() {
		let synth = FakeSynth::new();
		let vocalizer = Arc::new(Mutex::new(Vocalizer::new(synth.clone())));
		let ctx = Ctx::new();
		ctx.run("cl_voice_silence").expect("no-op without a slot");
		install(&ctx, Arc::clone(&vocalizer));
		vocalizer
			.lock()
			.push_text(SpeechMode::Assistant, "Queued sentence that will be silenced. ");
		assert!(vocalizer.lock().speaking());
		ctx.run("cl_voice_silence").expect("command runs");
		assert!(!vocalizer.lock().speaking());
		tokio::time::sleep(Duration::from_millis(30)).await;
		assert!(synth.spoken().is_empty());
	}
}
