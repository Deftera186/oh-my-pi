//! Typed settings owned by the voice runtime.

use omp_core::Str;
use omp_settings::{
	DomainRegistration, FieldDescriptor, OptionProvider, SettingKind, SettingOption, SettingScope,
	SettingsContribution, SettingsDomain,
};
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

const STT_MODELS: &[&str] = &["parakeet", "fast", "balanced", "turbo"];
const STT_MODEL_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "parakeet",
		label:       "Parakeet TDT v3",
		description: Some("SoTA English speech recognition through Candle Parakeet"),
	},
	SettingOption {
		value:       "fast",
		label:       "Whisper Base",
		description: Some("Small, fast multilingual Whisper model"),
	},
	SettingOption {
		value:       "balanced",
		label:       "Whisper Small",
		description: Some("Balanced multilingual Whisper model"),
	},
	SettingOption {
		value:       "turbo",
		label:       "Whisper Large v3 Turbo",
		description: Some("Highest-quality multilingual Whisper model"),
	},
];
const STT_SUBMIT_TRIGGERS: &[&str] = &["never", "release", "release-complete", "say-submit"];
const STT_SUBMIT_TRIGGER_OPTIONS: &[SettingOption] = &[
	SettingOption { value: "never", label: "Never", description: None },
	SettingOption {
		value:       "release",
		label:       "Release",
		description: Some("Submit an utterance containing at least two words when capture stops"),
	},
	SettingOption {
		value:       "release-complete",
		label:       "Release with complete sentence",
		description: Some("Submit a complete sentence when capture stops"),
	},
	SettingOption {
		value:       "say-submit",
		label:       "When I Say Submit",
		description: Some("Submit after the spoken submit trigger"),
	},
];
const TTS_MODELS: &[&str] = &["kokoro"];
const TTS_MODEL_OPTIONS: &[SettingOption] = &[SettingOption {
	value:       "kokoro",
	label:       "Kokoro-82M",
	description: Some("Kokoro-82M neural TTS, fully on-device"),
}];
const KOKORO_VOICES: &[&str] = &[
	"af_heart",
	"af_bella",
	"af_nicole",
	"af_aoede",
	"af_kore",
	"af_sarah",
	"am_michael",
	"am_fenrir",
	"am_puck",
	"bf_emma",
	"bm_george",
	"bm_fable",
];
const KOKORO_VOICE_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "af_heart",
		label:       "Heart (American female)",
		description: None,
	},
	SettingOption {
		value:       "af_bella",
		label:       "Bella (American female)",
		description: None,
	},
	SettingOption {
		value:       "af_nicole",
		label:       "Nicole (American female)",
		description: None,
	},
	SettingOption {
		value:       "af_aoede",
		label:       "Aoede (American female)",
		description: None,
	},
	SettingOption {
		value:       "af_kore",
		label:       "Kore (American female)",
		description: None,
	},
	SettingOption {
		value:       "af_sarah",
		label:       "Sarah (American female)",
		description: None,
	},
	SettingOption {
		value:       "am_michael",
		label:       "Michael (American male)",
		description: None,
	},
	SettingOption {
		value:       "am_fenrir",
		label:       "Fenrir (American male)",
		description: None,
	},
	SettingOption { value: "am_puck", label: "Puck (American male)", description: None },
	SettingOption {
		value:       "bf_emma",
		label:       "Emma (British female)",
		description: None,
	},
	SettingOption {
		value:       "bm_george",
		label:       "George (British male)",
		description: None,
	},
	SettingOption {
		value:       "bm_fable",
		label:       "Fable (British male)",
		description: None,
	},
];
const LIVE_VOICES: &[&str] =
	&["arbor", "breeze", "cove", "ember", "juniper", "maple", "sol", "spruce", "vale"];
const LIVE_VOICE_OPTIONS: &[SettingOption] = &[
	SettingOption { value: "arbor", label: "Arbor", description: None },
	SettingOption { value: "breeze", label: "Breeze", description: None },
	SettingOption { value: "cove", label: "Cove", description: None },
	SettingOption { value: "ember", label: "Ember", description: None },
	SettingOption { value: "juniper", label: "Juniper", description: None },
	SettingOption { value: "maple", label: "Maple", description: None },
	SettingOption { value: "sol", label: "Sol", description: None },
	SettingOption { value: "spruce", label: "Spruce", description: None },
	SettingOption { value: "vale", label: "Vale", description: None },
];
const SPEECH_MODES: &[&str] = &["all", "assistant", "yield"];
const SPEECH_MODE_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "all",
		label:       "All (messages + thinking)",
		description: None,
	},
	SettingOption { value: "assistant", label: "Assistant messages", description: None },
	SettingOption { value: "yield", label: "Final message only", description: None },
];
const TTS_PROVIDERS: &[&str] = &["auto", "local", "xai"];
const TTS_PROVIDER_OPTIONS: &[SettingOption] = &[
	SettingOption {
		value:       "auto",
		label:       "Auto",
		description: Some("Prefer local TTS; route MP3 to xAI when credentials are available"),
	},
	SettingOption {
		value:       "local",
		label:       "Local",
		description: Some("On-device Kokoro-82M with WAV/PCM16 output"),
	},
	SettingOption {
		value:       "xai",
		label:       "xAI Grok Voice",
		description: Some("Hosted xAI speech generation"),
	},
];

/// When dictated speech should be submitted to the active composer.
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
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum TtsProvider {
	/// Prefer local synthesis while allowing credentialed hosted routing when
	/// appropriate.
	#[default]
	Auto,
	/// Require local Kokoro synthesis.
	Local,
	/// Require hosted xAI Grok Voice synthesis.
	Xai,
}

/// Local speech-to-text preferences.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SttSettings {
	/// Whether microphone dictation is enabled.
	pub enabled:        bool,
	/// Language hint passed to speech recognition.
	pub language:       Str,
	/// Stable local speech model identifier.
	pub model_name:     Str,
	/// Automatic composer-submission policy.
	pub submit_trigger: SttSubmitTrigger,
}

impl Default for SttSettings {
	fn default() -> Self {
		Self {
			enabled:        false,
			language:       Str::new_static("en"),
			model_name:     Str::new_static("parakeet"),
			submit_trigger: SttSubmitTrigger::Never,
		}
	}
}

/// Local text-to-speech artifact and voice preferences.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TtsSettings {
	/// Stable local synthesis model identifier.
	pub local_model: Str,
	/// Voice used by direct local synthesis.
	pub local_voice: Str,
}

impl Default for TtsSettings {
	fn default() -> Self {
		Self { local_model: Str::new_static("kokoro"), local_voice: Str::new_static("af_heart") }
	}
}

/// Generated-speech tool availability.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct SpeechGenerationSettings {
	/// Whether the `tts` tool may synthesize speech files.
	pub enabled: bool,
}

/// Streaming assistant vocalization preferences.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct SpeechSettings {
	/// Whether assistant output is spoken aloud.
	pub enabled:  bool,
	/// Which assistant channels are spoken.
	pub mode:     SpeechMode,
	/// Whether a small model rewrites output into natural spoken prose.
	pub enhanced: bool,
	/// Kokoro voice used for assistant vocalization.
	pub voice:    Str,
}

impl Default for SpeechSettings {
	fn default() -> Self {
		Self {
			enabled:  false,
			mode:     SpeechMode::Assistant,
			enhanced: false,
			voice:    Str::new_static("af_heart"),
		}
	}
}

/// Realtime voice-session preferences.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct LiveSettings {
	/// Voice used by Codex-backed realtime sessions.
	pub voice: Str,
}

impl Default for LiveSettings {
	fn default() -> Self {
		Self { voice: Str::new_static("sol") }
	}
}

/// Provider routing preferences owned by speech generation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ProviderSpeechRouting {
	/// Text-to-speech backend preference.
	pub tts: TtsProvider,
}

/// Complete typed projection consumed by the voice runtime.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct VoiceSettings {
	/// Speech-to-text configuration.
	pub stt:       SttSettings,
	/// Local text-to-speech configuration.
	pub tts:       TtsSettings,
	/// Generated-speech tool configuration.
	pub speechgen: SpeechGenerationSettings,
	/// Assistant vocalization configuration.
	pub speech:    SpeechSettings,
	/// Realtime voice configuration.
	pub live:      LiveSettings,
	/// Speech-related provider routing.
	pub providers: ProviderSpeechRouting,
}

impl SettingsDomain for VoiceSettings {
	const DOMAIN: &'static str = "voice";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "stt.enabled",
			label:       "Speech-to-Text",
			description: "Enable speech-to-text input through the microphone.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "stt.language",
			label:       "Speech Language",
			description: "Language hint used for local speech recognition.",
			kind:        SettingKind::String,
			scopes:      PERSISTED,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "stt.modelName",
			label:       "Speech Model",
			description: "Local speech recognition model downloaded on first use.",
			kind:        SettingKind::Enum(STT_MODELS),
			scopes:      PERSISTED,
			order:       30,
			options:     Some(OptionProvider::Static(STT_MODEL_OPTIONS)),
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "stt.submitTrigger",
			label:       "Speech-to-Text Submit Trigger",
			description: "Choose when a completed dictation submits automatically.",
			kind:        SettingKind::Enum(STT_SUBMIT_TRIGGERS),
			scopes:      PERSISTED,
			order:       40,
			options:     Some(OptionProvider::Static(STT_SUBMIT_TRIGGER_OPTIONS)),
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "tts.localModel",
			label:       "Local TTS Model",
			description: "On-device neural speech synthesis model.",
			kind:        SettingKind::Enum(TTS_MODELS),
			scopes:      PERSISTED,
			order:       50,
			options:     Some(OptionProvider::Static(TTS_MODEL_OPTIONS)),
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "tts.localVoice",
			label:       "Local TTS Voice",
			description: "Kokoro voice used by direct local synthesis.",
			kind:        SettingKind::Enum(KOKORO_VOICES),
			scopes:      PERSISTED,
			order:       60,
			options:     Some(OptionProvider::Static(KOKORO_VOICE_OPTIONS)),
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "speechgen.enabled",
			label:       "Speech Generation",
			description: "Enable the tts tool for local or hosted speech-file synthesis.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       70,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "speech.enabled",
			label:       "Speech Vocalization",
			description: "Speak the assistant output aloud as it streams.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       80,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "speech.mode",
			label:       "Speech Vocalization Mode",
			description: "Choose which assistant output is spoken.",
			kind:        SettingKind::Enum(SPEECH_MODES),
			scopes:      PERSISTED,
			order:       90,
			options:     Some(OptionProvider::Static(SPEECH_MODE_OPTIONS)),
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "speech.enhanced",
			label:       "Enhanced Speech Rewriting",
			description: "Rewrite assistant output into natural spoken prose before synthesis.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       100,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "speech.voice",
			label:       "Speech Vocalization Voice",
			description: "Kokoro voice used for assistant vocalization.",
			kind:        SettingKind::Enum(KOKORO_VOICES),
			scopes:      PERSISTED,
			order:       110,
			options:     Some(OptionProvider::Static(KOKORO_VOICE_OPTIONS)),
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "live.voice",
			label:       "Live Voice",
			description: "Voice used by Codex-backed realtime voice sessions.",
			kind:        SettingKind::Enum(LIVE_VOICES),
			scopes:      PERSISTED,
			order:       120,
			options:     Some(OptionProvider::Static(LIVE_VOICE_OPTIONS)),
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "providers.tts",
			label:       "Text-to-Speech Provider",
			description: "Backend used for generated speech files.",
			kind:        SettingKind::Enum(TTS_PROVIDERS),
			scopes:      PERSISTED,
			order:       130,
			options:     Some(OptionProvider::Static(TTS_PROVIDER_OPTIONS)),
			condition:   None,
			secret:      false,
		},
	];
	const PREFIX: Option<&'static str> = None;
}

/// Settings domains and normalizers owned by the application crate.
pub const SETTINGS_CONTRIBUTION: SettingsContribution = SettingsContribution {
	domains:     &[DomainRegistration::of::<VoiceSettings>()],
	normalizers: &[],
};

#[cfg(test)]
mod tests {
	use omp_settings::{SettingsCatalog, SettingsSnapshot};

	use super::*;

	#[test]
	fn defaults_match_pi_voice_settings() {
		let settings = VoiceSettings::default();
		assert!(!settings.stt.enabled);
		assert_eq!(settings.stt.language, "en");
		assert_eq!(settings.stt.model_name, "parakeet");
		assert_eq!(settings.stt.submit_trigger, SttSubmitTrigger::Never);
		assert_eq!(settings.tts.local_model, "kokoro");
		assert_eq!(settings.tts.local_voice, "af_heart");
		assert!(!settings.speechgen.enabled);
		assert!(!settings.speech.enabled);
		assert_eq!(settings.speech.mode, SpeechMode::Assistant);
		assert!(!settings.speech.enhanced);
		assert_eq!(settings.speech.voice, "af_heart");
		assert_eq!(settings.live.voice, "sol");
		assert_eq!(settings.providers.tts, TtsProvider::Auto);
	}

	#[test]
	fn serde_uses_pi_dotted_key_segments() {
		let encoded =
			toml::Value::try_from(VoiceSettings::default()).expect("serialize voice settings");
		let root = encoded.as_table().expect("settings root table");
		let stt = root
			.get("stt")
			.and_then(toml::Value::as_table)
			.expect("stt table");
		assert!(stt.contains_key("modelName"));
		assert!(stt.contains_key("submitTrigger"));
		assert!(!stt.contains_key("model_name"));
		let tts = root
			.get("tts")
			.and_then(toml::Value::as_table)
			.expect("tts table");
		assert!(tts.contains_key("localModel"));
		assert!(tts.contains_key("localVoice"));
		assert_eq!(root["speechgen"]["enabled"].as_bool(), Some(false));
		assert_eq!(root["providers"]["tts"].as_str(), Some("auto"));

		let decoded: VoiceSettings = toml::from_str(
			r#"
			[stt]
			modelName = "turbo"
			submitTrigger = "release-complete"
			[tts]
			localVoice = "bf_emma"
			[speech]
			mode = "yield"
			[live]
			voice = "maple"
			[providers]
			tts = "xai"
			"#,
		)
		.expect("deserialize voice settings");
		assert_eq!(decoded.stt.model_name, "turbo");
		assert_eq!(decoded.stt.submit_trigger, SttSubmitTrigger::ReleaseComplete);
		assert_eq!(decoded.tts.local_voice, "bf_emma");
		assert_eq!(decoded.speech.mode, SpeechMode::Yield);
		assert_eq!(decoded.live.voice, "maple");
		assert_eq!(decoded.providers.tts, TtsProvider::Xai);
		assert_eq!(decoded.stt.language, "en");
	}

	#[test]
	fn isolated_projection_and_registration_are_typed() {
		let expected = VoiceSettings {
			stt: SttSettings { enabled: true, ..SttSettings::default() },
			..VoiceSettings::default()
		};
		let snapshot = SettingsSnapshot::isolated(
			expected.clone(),
			SettingsCatalog::new(&[&SETTINGS_CONTRIBUTION]),
		)
		.expect("isolated snapshot");
		assert_eq!(
			snapshot
				.project::<VoiceSettings>()
				.expect("voice projection")
				.get(),
			&expected
		);
		assert!(
			SETTINGS_CONTRIBUTION
				.domains
				.iter()
				.map(|domain| domain.descriptor().name)
				.eq([VoiceSettings::DOMAIN])
		);
	}
}
