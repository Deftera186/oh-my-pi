//! Production speech synthesis for the chat vocalizer: local Kokoro (or the
//! configured provider) through the Environment's media bridge, decoded to
//! the mono `f32` contract `omp-voice` plays.

use std::{future::Future, pin::Pin, sync::Arc};

use omp_chat::notices::voice::{SpeechSynth, SynthAudio};
use omp_core::Str;
use omp_envd::search_backend::SearchBridgeHost;
use omp_proto::inference::v1 as inference_pb;

/// Sample rate requested from the synthesizer and handed to playback.
const SAMPLE_RATE_HZ: u32 = 24_000;

/// Synthesizes speech through the environment's inference facade.
pub struct EnvSpeechSynth {
	bridge: Arc<SearchBridgeHost>,
	con:    Arc<omp_con::Ctx>,
}

impl EnvSpeechSynth {
	/// Creates a synthesizer over the environment bridge; the voice comes from
	/// `cl_speech_voice` at each request so a mid-session change applies.
	#[must_use]
	pub fn new(bridge: Arc<SearchBridgeHost>, con: Arc<omp_con::Ctx>) -> Self {
		Self { bridge, con }
	}
}

impl SpeechSynth for EnvSpeechSynth {
	fn synthesize(
		&self,
		text: Str,
	) -> Pin<Box<dyn Future<Output = Result<SynthAudio, Str>> + Send + '_>> {
		let voice = <&'static str>::from(super::settings::CL_SPEECH_VOICE.get(&self.con));
		let model = <&'static str>::from(super::settings::CL_TTS_MODEL.get(&self.con));
		let request = inference_pb::SpeakRequest {
			model: model.to_owned(),
			text: text.to_string(),
			voice: voice.to_owned(),
			encoding: inference_pb::AudioEncoding::Pcm16 as i32,
			sample_rate_hz: Some(SAMPLE_RATE_HZ),
			speed: None,
			instructions: String::new(),
			clone: None,
			props: None,
		};
		Box::pin(async move {
			let audio = self
				.bridge
				.speak(request)
				.await
				.map_err(|error| error.message)?;
			if audio.len() % 2 != 0 {
				return Err(Str::new_static("speech synthesis returned malformed PCM16 audio"));
			}
			let samples = audio
				.chunks_exact(2)
				.map(|sample| {
					f32::from(i16::from_le_bytes([sample[0], sample[1]])) / f32::from(i16::MAX)
				})
				.collect();
			Ok(SynthAudio { sample_rate: SAMPLE_RATE_HZ, samples })
		})
	}
}
