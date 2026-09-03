//! Voice command-stream variables and one-shot legacy migration keys.

use omp_core::Str;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

/// Dictation auto-submit policy.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
	strum::VariantNames,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum SttSubmitTrigger {
	/// Never submit automatically.
	#[default]
	Never,
	/// Submit a sufficiently long utterance when capture is released.
	Release,
	/// Submit only a complete sentence when capture is released.
	ReleaseComplete,
	/// Submit when the user speaks the submit trigger.
	SaySubmit,
}

/// Which assistant output is vocalized.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
	strum::VariantNames,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum SpeechMode {
	/// Speak assistant messages and thinking.
	All,
	/// Speak assistant messages without thinking.
	#[default]
	Assistant,
	/// Speak only the final message at turn completion.
	Yield,
}

/// Backend preference for generated speech files.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
	strum::VariantNames,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum TtsProvider {
	/// Prefer local synthesis.
	#[default]
	Auto,
	/// Require local Kokoro synthesis.
	Local,
	/// Require hosted xAI synthesis.
	Xai,
}

omp_con::con_enum!(SttSubmitTrigger);
omp_con::con_enum!(SpeechMode);
omp_con::con_enum!(TtsProvider);

/// Speech recognition model selection.
#[derive(
	Clone, Copy, Debug, Default, EnumString, Eq, IntoStaticStr, PartialEq, strum::VariantNames,
)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum SttModel {
	/// Parakeet TDT v3.
	#[default]
	Parakeet,
	/// Whisper Base.
	Fast,
	/// Whisper Small.
	Balanced,
	/// Whisper Large v3 Turbo.
	Turbo,
}

omp_con::con_enum!(SttModel);

/// Local speech synthesis model selection.
#[derive(
	Clone, Copy, Debug, Default, EnumString, Eq, IntoStaticStr, PartialEq, strum::VariantNames,
)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum TtsModel {
	/// Kokoro-82M.
	#[default]
	Kokoro,
}

omp_con::con_enum!(TtsModel);

/// Kokoro voice selection.
#[derive(
	Clone, Copy, Debug, Default, EnumString, Eq, IntoStaticStr, PartialEq, strum::VariantNames,
)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum KokoroVoice {
	/// Heart, American female.
	#[default]
	AfHeart,
	/// Bella, American female.
	AfBella,
	/// Nicole, American female.
	AfNicole,
	/// Aoede, American female.
	AfAoede,
	/// Kore, American female.
	AfKore,
	/// Sarah, American female.
	AfSarah,
	/// Michael, American male.
	AmMichael,
	/// Fenrir, American male.
	AmFenrir,
	/// Puck, American male.
	AmPuck,
	/// Emma, British female.
	BfEmma,
	/// George, British male.
	BmGeorge,
	/// Fable, British male.
	BmFable,
}

omp_con::con_enum!(KokoroVoice);

/// Realtime provider voice selection.
#[derive(
	Clone, Copy, Debug, Default, EnumString, Eq, IntoStaticStr, PartialEq, strum::VariantNames,
)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum LiveVoice {
	/// Arbor.
	Arbor,
	/// Breeze.
	Breeze,
	/// Cove.
	Cove,
	/// Ember.
	Ember,
	/// Juniper.
	Juniper,
	/// Maple.
	Maple,
	/// Sol.
	#[default]
	Sol,
	/// Spruce.
	Spruce,
	/// Vale.
	Vale,
}

omp_con::con_enum!(LiveVoice);

omp_con::var! {
	/// Enables microphone dictation.
	pub static CL_VOICE_STT_ENABLED = cl_voice_stt_enabled: bool { default: false, flags: archive };
	/// Speech recognition language hint.
	pub static CL_STT_LANGUAGE = cl_stt_language: Str { default: Str::new_static("en"), flags: archive };
	/// Local speech recognition model.
	pub static CL_STT_MODEL = cl_stt_model: SttModel { default: SttModel::Parakeet, flags: archive };
	/// Dictation submission policy.
	pub static CL_STT_SUBMIT_TRIGGER = cl_stt_submit_trigger: SttSubmitTrigger { default: SttSubmitTrigger::Never, flags: archive };
	/// Local synthesis model.
	pub static CL_TTS_MODEL = cl_tts_model: TtsModel { default: TtsModel::Kokoro, flags: archive };
	/// Direct local synthesis voice.
	pub static CL_TTS_VOICE = cl_tts_voice: KokoroVoice { default: KokoroVoice::AfHeart, flags: archive };
	/// Enables generated speech tools.
	pub static CL_SPEECHGEN_ENABLED = cl_speechgen_enabled: bool { default: false, flags: archive };
	/// Enables assistant vocalization.
	pub static CL_SPEECH_ENABLED = cl_speech_enabled: bool { default: false, flags: archive };
	/// Selects assistant channels to vocalize.
	pub static CL_SPEECH_MODE = cl_speech_mode: SpeechMode { default: SpeechMode::Assistant, flags: archive };
	/// Enables natural speech rewriting.
	pub static CL_SPEECH_ENHANCED = cl_speech_enhanced: bool { default: false, flags: archive };
	/// Assistant vocalization voice.
	pub static CL_SPEECH_VOICE = cl_speech_voice: KokoroVoice { default: KokoroVoice::AfHeart, flags: archive };
	/// Realtime provider voice.
	pub static CL_LIVE_VOICE = cl_live_voice: LiveVoice { default: LiveVoice::Sol, flags: archive };
	/// Generated-speech provider.
	pub static AI_TTS_PROVIDER = ai_tts_provider: TtsProvider { default: TtsProvider::Auto, flags: archive };
}

/// Legacy settings keys and their command-stream replacements.
pub const LEGACY_CONVAR_MAPPINGS: &[(&str, &str)] = &[
	("stt.enabled", "cl_voice_stt_enabled"),
	("stt.language", "cl_stt_language"),
	("stt.modelName", "cl_stt_model"),
	("stt.submitTrigger", "cl_stt_submit_trigger"),
	("tts.localModel", "cl_tts_model"),
	("tts.localVoice", "cl_tts_voice"),
	("speechgen.enabled", "cl_speechgen_enabled"),
	("speech.enabled", "cl_speech_enabled"),
	("speech.mode", "cl_speech_mode"),
	("speech.enhanced", "cl_speech_enhanced"),
	("speech.voice", "cl_speech_voice"),
	("live.voice", "cl_live_voice"),
	("providers.tts", "ai_tts_provider"),
];
