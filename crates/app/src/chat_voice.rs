//! Push-to-talk for `omp chat`: the microphone lease, capture, and local
//! speech recognition behind the composer's space-hold gesture and
//! `cl_stt_toggle`.
//!
//! The chat actor only reports recording edges (`HostCommand::PushToTalk`);
//! everything that touches audio hardware lives here, composed over
//! [`InteractiveAudioController`] (shared microphone ownership) and
//! `omp_voice::audio::CaptureStream`. A finished recording is transcribed
//! off the controller thread and the text is posted back through the
//! console mailbox as `HostAction::InsertText`, which the actor pastes at
//! the caret (pi `onSpaceHoldEnd` → `sttController.stop()` → editor
//! insert).

use std::sync::{Arc, Mutex};

use omp_chat::{HostAction, HostMailbox};
use omp_con::Ctx;
use omp_core::Str;
use omp_voice::audio::CaptureStream;

use crate::audio_coordinator::InteractiveAudioController;

/// Mono capture rate the local recognizers consume.
const SAMPLE_RATE: u32 = 16_000;
/// Recordings shorter than this are noise from a mis-recognized hold.
const MIN_SAMPLES: usize = SAMPLE_RATE as usize / 4;

/// One session's push-to-talk recorder.
pub struct PushToTalk {
	audio:   InteractiveAudioController,
	capture: Option<CaptureStream>,
	buffer:  Arc<Mutex<Vec<f32>>>,
}

impl PushToTalk {
	/// Creates an idle recorder over the session's audio controller.
	#[must_use]
	pub fn new(audio: InteractiveAudioController) -> Self {
		Self { audio, capture: None, buffer: Arc::new(Mutex::new(Vec::new())) }
	}

	/// Whether a recording is in progress.
	#[must_use]
	pub const fn recording(&self) -> bool {
		self.capture.is_some()
	}

	/// Applies one recording edge from the host. Errors are reported to the
	/// host as a notice through `ctx`'s console mailbox, never raised.
	pub fn set_active(&mut self, active: bool, ctx: &Ctx) {
		if active {
			if let Err(error) = self.start() {
				reply(ctx, error);
			}
		} else {
			self.stop(ctx);
		}
	}

	/// Starts or stops the live-voice microphone lease (pi `/live`); the
	/// lease is exclusive with push-to-talk, so a competing owner reports
	/// instead of silently pretending.
	pub fn set_live(&mut self, active: bool, ctx: &Ctx) {
		if active {
			if self.capture.is_some() {
				self.stop(ctx);
			}
			match self.audio.start_live() {
				Ok(()) => {},
				Err(error) => reply(ctx, Str::new(format!("Live voice unavailable: {error}"))),
			}
		} else {
			self.audio.stop_live();
		}
	}

	fn start(&mut self) -> Result<(), Str> {
		if self.capture.is_some() {
			return Ok(());
		}
		if !self.audio.stt_active() {
			self
				.audio
				.toggle_stt()
				.map_err(|error| Str::new(format!("Microphone unavailable: {error}")))?;
		}
		self.buffer.lock().expect("capture buffer poisoned").clear();
		let buffer = Arc::clone(&self.buffer);
		let capture = CaptureStream::start(SAMPLE_RATE, move |samples| {
			if let Ok(mut buffer) = buffer.lock() {
				buffer.extend_from_slice(samples);
			}
		})
		.map_err(|error| {
			let _ = self.audio.toggle_stt();
			Str::new(format!("Microphone capture failed: {error}"))
		})?;
		self.capture = Some(capture);
		Ok(())
	}

	fn stop(&mut self, ctx: &Ctx) {
		let Some(mut capture) = self.capture.take() else {
			return;
		};
		let _ = capture.stop();
		if self.audio.stt_active() {
			let _ = self.audio.toggle_stt();
		}
		let samples = std::mem::take(&mut *self.buffer.lock().expect("capture buffer poisoned"));
		if samples.len() < MIN_SAMPLES {
			reply(ctx, Str::new_static("Nothing heard"));
			return;
		}
		let Some(mailbox) = ctx.user::<HostMailbox>() else {
			return;
		};
		let mailbox = Arc::clone(&mailbox);
		tokio::task::spawn_blocking(move || match transcribe(&samples) {
			Ok(text) if text.trim().is_empty() => {
				mailbox.post(HostAction::Reply {
					severity: omp_con::Severity::Info,
					text:     Str::new_static("Nothing recognized"),
				});
			},
			Ok(text) => mailbox.post(HostAction::InsertText(text)),
			Err(error) => mailbox.post(HostAction::Reply {
				severity: omp_con::Severity::Warn,
				text:     error,
			}),
		});
	}
}

fn reply(ctx: &Ctx, text: Str) {
	if let Some(mailbox) = ctx.user::<HostMailbox>() {
		mailbox.post(HostAction::Reply { severity: omp_con::Severity::Warn, text });
	}
}

/// Recognizes `samples` (mono 16 kHz) with the verified local recognizer
/// selected by `omp setup speech`.
#[cfg(feature = "local-stt")]
fn transcribe(samples: &[f32]) -> Result<Str, Str> {
	use std::{fs, sync::Arc, time::Duration};

	use omp_inference::local::{
		ArtifactStore, LocalCancellation, MemoryPool,
		speech_catalog::SpeechArtifactManifests,
		stt::{SpeechToTextAdapter, SttRuntimeOptions, TranscriptionOptions},
	};

	let fail = |error: &dyn std::fmt::Display| Str::new(format!("Speech recognition failed: {error}"));
	let data_dir = omp_core::dirs::data_dir(None).map_err(|error| fail(&error))?;
	let root = data_dir.join("models");
	fs::create_dir_all(&root).map_err(|error| fail(&error))?;
	let store = ArtifactStore::open(&root).map_err(|error| fail(&error))?;
	let artifacts = SpeechArtifactManifests::curated().map_err(|error| fail(&error))?;
	let cancel = LocalCancellation::new();
	let options = SttRuntimeOptions {
		threads:      std::thread::available_parallelism().map_or(4, usize::from),
		whisper_gpu:  true,
		idle_timeout: Duration::from_secs(120),
	};
	let memory = Arc::new(MemoryPool::new(2 * 1024 * 1024 * 1024));
	let adapter =
		SpeechToTextAdapter::from_verified_artifacts(&store, &artifacts, None, options, memory, &cancel)
			.map_err(|error| {
				Str::new(format!(
					"Speech recognition is not set up ({error}); run `omp setup speech` first"
				))
			})?;
	let transcription = adapter
		.transcribe_mono_16khz(samples, &TranscriptionOptions::default(), &cancel)
		.map_err(|error| fail(&error))?;
	Ok(Str::new(transcription.text.trim()))
}

/// Feature-disabled recognizer: the build carries no local speech models.
#[cfg(not(feature = "local-stt"))]
fn transcribe(_samples: &[f32]) -> Result<Str, Str> {
	Err(Str::new_static(
		"Speech-to-text is not built; rebuild omp with `--features local-stt`",
	))
}
