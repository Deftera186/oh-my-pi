//! Runtime-owned display, appearance, interaction, and lifecycle settings.

use std::collections::{BTreeMap, HashSet};

pub use omp_agent::UnexpectedStopMode;
use omp_core::Str;
use omp_settings::{
	DomainRegistration, FieldDescriptor, OptionProvider, SettingKind, SettingOption, SettingScope,
	SettingsDomain,
};
use serde::{Deserialize, Serialize};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const GLOBAL: &[SettingScope] = &[SettingScope::Global];

const fn field(
	path: &'static str,
	label: &'static str,
	description: &'static str,
	kind: SettingKind,
	order: u16,
) -> FieldDescriptor {
	FieldDescriptor {
		path,
		label,
		description,
		kind,
		scopes: PERSISTED,
		order,
		options: None,
		condition: None,
		secret: false,
	}
}

const fn default_max_inline_images() -> u16 {
	8
}

const fn default_paste_threshold() -> usize {
	100
}

/// Terminal hyperlink emission policy.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum HyperlinkMode {
	/// Never emit OSC 8 hyperlinks.
	Off,
	/// Emit hyperlinks when terminal capability detection permits.
	#[default]
	Auto,
	/// Emit hyperlinks unconditionally.
	Always,
}

/// Pending-content animation style.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum ShimmerMode {
	/// Soft cosine highlight.
	#[default]
	Classic,
	/// KITT-style scanning highlight.
	Kitt,
	/// Disable pending-content animation.
	Disabled,
}

/// Terminal scrollback refresh policy after a settled resize.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum ResizeScrollbackMode {
	/// Append the resized transcript after existing scrollback rows.
	Append,
	/// Rebuild retained transcript rows at the settled terminal width.
	#[default]
	Rebuild,
	/// Preserve existing terminal scrollback rows without replaying them.
	Preserve,
}

/// TUI-specific rendering and input behavior.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TuiSettings {
	/// Terminal hyperlink emission policy.
	pub hyperlinks:        HyperlinkMode,
	/// Render Mermaid diagrams as terminal graphics.
	pub render_mermaid:    bool,
	/// Celebrate newly available Codex reset capacity.
	pub codex_fireworks:   bool,
	/// Use compact horizontal spacing.
	pub tight:             bool,
	/// Maximum live inline terminal images; zero is unlimited.
	pub max_inline_images: u16,
	/// Keep prompt chrome stable during IME preedit.
	pub ime_safe_cursor:   bool,
	/// How a settled terminal resize refreshes transcript rows retained in
	/// terminal scrollback.
	pub resize_scrollback: ResizeScrollbackMode,
}

impl Default for TuiSettings {
	fn default() -> Self {
		Self {
			hyperlinks:        HyperlinkMode::Auto,
			render_mermaid:    true,
			codex_fireworks:   false,
			tight:             false,
			max_inline_images: default_max_inline_images(),
			ime_safe_cursor:   false,
			resize_scrollback: ResizeScrollbackMode::Rebuild,
		}
	}
}

impl SettingsDomain for TuiSettings {
	const DOMAIN: &'static str = "tui";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"tui.hyperlinks",
			"Hyperlinks",
			"Terminal hyperlink emission policy.",
			SettingKind::Enum(&["off", "auto", "always"]),
			10,
		),
		field("tui.renderMermaid", "Mermaid", "Render Mermaid diagrams.", SettingKind::Boolean, 20),
		field(
			"tui.codexFireworks",
			"Codex Fireworks",
			"Celebrate available Codex reset capacity.",
			SettingKind::Boolean,
			30,
		),
		field(
			"tui.tight",
			"Tight Layout",
			"Use compact horizontal spacing.",
			SettingKind::Boolean,
			40,
		),
		field(
			"tui.maxInlineImages",
			"Inline Image Limit",
			"Maximum live inline terminal images; zero is unlimited.",
			SettingKind::Integer,
			50,
		),
		field(
			"tui.imeSafeCursor",
			"IME-safe Cursor",
			"Keep prompt chrome stable during IME preedit.",
			SettingKind::Boolean,
			60,
		),
		field(
			"tui.resizeScrollback",
			"Resize Scrollback",
			"How a settled terminal resize refreshes transcript rows retained in terminal scrollback",
			SettingKind::Enum(&["append", "rebuild", "preserve"]),
			70,
		),
	];
}

/// Root-level terminal display switches retained at their Pi-compatible paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RootDisplaySettings {
	/// Show the terminal hardware cursor for IME support.
	pub show_hardware_cursor: bool,
	/// Use blue rather than green for diff additions.
	pub color_blind_mode:     bool,
}

impl Default for RootDisplaySettings {
	fn default() -> Self {
		Self { show_hardware_cursor: true, color_blind_mode: false }
	}
}

impl SettingsDomain for RootDisplaySettings {
	const DOMAIN: &'static str = "root-display";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"showHardwareCursor",
			"Hardware Cursor",
			"Show the terminal hardware cursor.",
			SettingKind::Boolean,
			10,
		),
		field(
			"colorBlindMode",
			"Colorblind Diff",
			"Use colorblind-safe diff additions.",
			SettingKind::Boolean,
			20,
		),
	];
	const PREFIX: Option<&'static str> = None;
}

/// Persisted on/off notification selection.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum NotifyToggle {
	/// Send the notification.
	#[default]
	On,
	/// Suppress the notification.
	Off,
}

impl NotifyToggle {
	/// Returns whether the notification is enabled.
	pub const fn enabled(self) -> bool {
		matches!(self, Self::On)
	}
}

/// Successful-turn notification policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompletionSettings {
	/// Whether completed turns trigger a desktop notification.
	pub notify: NotifyToggle,
}

impl Default for CompletionSettings {
	fn default() -> Self {
		Self { notify: NotifyToggle::On }
	}
}

impl SettingsDomain for CompletionSettings {
	const DOMAIN: &'static str = "completion";
	const FIELDS: &'static [FieldDescriptor] = &[field(
		"completion.notify",
		"Completion Notification",
		"Notify when a turn completes.",
		SettingKind::Enum(&["on", "off"]),
		10,
	)];
}

/// Failed-turn notification policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ErrorNotificationSettings {
	/// Whether failed turns trigger a desktop notification.
	pub notify: NotifyToggle,
}

impl Default for ErrorNotificationSettings {
	fn default() -> Self {
		Self { notify: NotifyToggle::Off }
	}
}

impl SettingsDomain for ErrorNotificationSettings {
	const DOMAIN: &'static str = "error";
	const FIELDS: &'static [FieldDescriptor] = &[field(
		"error.notify",
		"Error Notification",
		"Notify when a turn fails.",
		SettingKind::Enum(&["on", "off"]),
		10,
	)];
}

/// Terminal presentation and rendering behavior.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DisplaySettings {
	/// Pending-content animation style.
	pub shimmer:            ShimmerMode,
	/// Smoothly reveal streamed text.
	pub smooth_streaming:   bool,
	/// Show per-turn token usage.
	pub show_token_usage:   bool,
	/// Hide model-initiated tool calls and results.
	pub hide_tool_activity: bool,
}

impl Default for DisplaySettings {
	fn default() -> Self {
		Self {
			shimmer:            ShimmerMode::Classic,
			smooth_streaming:   true,
			show_token_usage:   false,
			hide_tool_activity: false,
		}
	}
}

impl SettingsDomain for DisplaySettings {
	const DOMAIN: &'static str = "display";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"display.shimmer",
			"Shimmer",
			"Animate pending assistant content.",
			SettingKind::Enum(&["classic", "kitt", "disabled"]),
			10,
		),
		field(
			"display.smoothStreaming",
			"Smooth Streaming",
			"Smoothly reveal streamed text.",
			SettingKind::Boolean,
			20,
		),
		field(
			"display.showTokenUsage",
			"Token Usage",
			"Show per-turn token usage.",
			SettingKind::Boolean,
			30,
		),
		field(
			"display.hideToolActivity",
			"Hide Tool Activity",
			"Hide model-initiated tool calls and results.",
			SettingKind::Boolean,
			40,
		),
	];
}

/// Theme, status-line, icon, and image presentation choices.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppearanceSettings {
	/// Theme family name.
	pub theme:           Str,
	/// Optional theme variant within the selected family.
	pub theme_variant:   Option<Str>,
	/// Status-line preset name.
	pub status_preset:   Str,
	/// Ordered status-line segment identifiers.
	pub status_segments: Vec<Str>,
	/// Optional theme accent override.
	pub accent:          Option<Str>,
	/// Icon/character preset name.
	pub icon_preset:     Str,
	/// Permit terminal image rendering.
	pub render_images:   bool,
}

impl Default for AppearanceSettings {
	fn default() -> Self {
		Self {
			theme:           Str::new_static("default"),
			theme_variant:   None,
			status_preset:   Str::new_static("default"),
			status_segments: Vec::new(),
			accent:          None,
			icon_preset:     Str::new_static("unicode"),
			render_images:   true,
		}
	}
}

impl SettingsDomain for AppearanceSettings {
	const DOMAIN: &'static str = "appearance";
	const FIELDS: &'static [FieldDescriptor] = &[
		field("appearance.theme", "Theme", "Theme family name.", SettingKind::String, 10),
		field(
			"appearance.themeVariant",
			"Theme Variant",
			"Variant within the selected theme.",
			SettingKind::String,
			20,
		),
		field(
			"appearance.statusPreset",
			"Status Preset",
			"Status-line preset.",
			SettingKind::String,
			30,
		),
		field(
			"appearance.statusSegments",
			"Status Segments",
			"Ordered status-line segments.",
			SettingKind::Array,
			40,
		),
		field("appearance.accent", "Accent", "Theme accent override.", SettingKind::String, 50),
		field(
			"appearance.iconPreset",
			"Icon Preset",
			"Icon and character preset.",
			SettingKind::String,
			60,
		),
		field(
			"appearance.renderImages",
			"Render Images",
			"Permit terminal image rendering.",
			SettingKind::Boolean,
			70,
		),
	];
}

/// Queued steering delivery policy.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum SteeringMode {
	/// Deliver every queued steering message at the next safe boundary.
	All,
	/// Deliver one queued steering message per safe boundary.
	#[default]
	OneAtATime,
}

impl From<SteeringMode> for omp_agent::SteeringMode {
	fn from(mode: SteeringMode) -> Self {
		match mode {
			SteeringMode::All => Self::All,
			SteeringMode::OneAtATime => Self::OneAtATime,
		}
	}
}

/// Interactive notification, voice, loop, and input behavior.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InteractionSettings {
	/// Queued steering delivery policy.
	pub steering_mode:             SteeringMode,
	/// Approval timeout for an unanswered interactive question, in seconds.
	pub ask_timeout_seconds:       u64,
	/// Per-event notification overrides.
	pub notifications:             BTreeMap<Str, bool>,
	/// Enable speech output.
	pub tts_enabled:               bool,
	/// Enable speech-to-text input.
	pub stt_enabled:               bool,
	/// Enable live duplex voice mode.
	pub live_voice_enabled:        bool,
	/// Optional speech voice identifier.
	pub voice:                     Option<Str>,
	/// Automatic agent loop mode.
	pub loop_mode:                 bool,
	/// Unexpected assistant-stop recovery policy.
	pub unexpected_stop_detection: UnexpectedStopMode,
	/// Line threshold above which the large-paste menu is offered.
	pub paste_threshold:           usize,
	/// User-defined composer keyword expansions.
	pub magic_keywords:            BTreeMap<Str, Str>,
	/// Whether follow-up submissions enter the pending queue.
	pub queue_follow_ups:          bool,
	/// Whether microphone input is pushed to talk rather than always live.
	pub push_to_talk:              bool,
}

impl Default for InteractionSettings {
	fn default() -> Self {
		Self {
			steering_mode:             SteeringMode::OneAtATime,
			ask_timeout_seconds:       0,
			notifications:             BTreeMap::new(),
			tts_enabled:               false,
			stt_enabled:               false,
			live_voice_enabled:        false,
			voice:                     None,
			loop_mode:                 false,
			unexpected_stop_detection: UnexpectedStopMode::Mechanical,
			paste_threshold:           default_paste_threshold(),
			magic_keywords:            BTreeMap::new(),
			queue_follow_ups:          true,
			push_to_talk:              true,
		}
	}
}

impl SettingsDomain for InteractionSettings {
	const DOMAIN: &'static str = "interaction";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"interaction.steeringMode",
			"Steering Mode",
			"How queued steering messages are delivered at safe boundaries.",
			SettingKind::Enum(&["all", "one-at-a-time"]),
			5,
		),
		field(
			"interaction.askTimeoutSeconds",
			"Ask Timeout",
			"Seconds to wait for an interactive answer.",
			SettingKind::Integer,
			10,
		),
		field(
			"interaction.notifications",
			"Notifications",
			"Per-event notification overrides.",
			SettingKind::Table,
			20,
		),
		field(
			"interaction.ttsEnabled",
			"Text to Speech",
			"Enable speech output.",
			SettingKind::Boolean,
			30,
		),
		field(
			"interaction.sttEnabled",
			"Speech to Text",
			"Enable microphone transcription.",
			SettingKind::Boolean,
			40,
		),
		field(
			"interaction.liveVoiceEnabled",
			"Live Voice",
			"Enable duplex live voice.",
			SettingKind::Boolean,
			50,
		),
		field("interaction.voice", "Voice", "Speech voice identifier.", SettingKind::String, 60),
		field(
			"interaction.loopMode",
			"Loop Mode",
			"Continue autonomous turns when supported.",
			SettingKind::Boolean,
			70,
		),
		field(
			"interaction.unexpectedStopDetection",
			"Unexpected Stops",
			"Recover no-message stops mechanically or classify text-only stops with a small model.",
			SettingKind::Enum(&["none", "mechanical", "smart"]),
			71,
		),
		field(
			"interaction.pasteThreshold",
			"Paste Threshold",
			"Lines before the large-paste menu is offered.",
			SettingKind::Integer,
			80,
		),
		field(
			"interaction.magicKeywords",
			"Magic Keywords",
			"Composer keyword expansions.",
			SettingKind::Table,
			90,
		),
		field(
			"interaction.queueFollowUps",
			"Queue Follow-ups",
			"Queue follow-up submissions during an active turn.",
			SettingKind::Boolean,
			110,
		),
		field(
			"interaction.pushToTalk",
			"Push to Talk",
			"Require explicit microphone activation.",
			SettingKind::Boolean,
			120,
		),
	];
}

/// Partial-output treatment for a time-traveling stream rule match.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum TtsrContextMode {
	/// Remove abandoned partial output before replay.
	#[default]
	Discard,
	/// Preserve abandoned partial output in replay context.
	Keep,
}

/// Stream classes on which a TTSR match interrupts generation.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum TtsrInterruptMode {
	/// Never interrupt generation.
	Never,
	/// Interrupt prose and reasoning only.
	ProseOnly,
	/// Interrupt tool arguments only.
	ToolOnly,
	/// Interrupt every matched stream.
	#[default]
	Always,
}

/// Persisted time-traveling stream-rule policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TtsrSettings {
	/// Whether stream rules are enabled.
	pub enabled:        bool,
	/// Partial-output treatment after interruption.
	pub context_mode:   TtsrContextMode,
	/// Default interruption policy.
	pub interrupt_mode: TtsrInterruptMode,
	/// Whether bundled rules participate beneath user rules.
	pub builtin_rules:  bool,
	/// Rule names disabled before precedence resolution.
	pub disabled_rules: Vec<Str>,
}

impl Default for TtsrSettings {
	fn default() -> Self {
		Self {
			enabled:        true,
			context_mode:   TtsrContextMode::Discard,
			interrupt_mode: TtsrInterruptMode::Always,
			builtin_rules:  true,
			disabled_rules: Vec::new(),
		}
	}
}

impl TtsrSettings {
	/// Freezes this persisted projection for the agent matcher, retaining the
	/// agent's repeat policy defaults.
	pub fn for_agent(&self) -> omp_agent::TtsrSettings {
		let defaults = omp_agent::TtsrSettings::default();
		omp_agent::TtsrSettings {
			enabled:        self.enabled,
			context_mode:   match self.context_mode {
				TtsrContextMode::Discard => omp_agent::TtsrContextMode::Discard,
				TtsrContextMode::Keep => omp_agent::TtsrContextMode::Keep,
			},
			interrupt_mode: match self.interrupt_mode {
				TtsrInterruptMode::Never => omp_agent::TtsrInterruptMode::Never,
				TtsrInterruptMode::ProseOnly => omp_agent::TtsrInterruptMode::ProseOnly,
				TtsrInterruptMode::ToolOnly => omp_agent::TtsrInterruptMode::ToolOnly,
				TtsrInterruptMode::Always => omp_agent::TtsrInterruptMode::Always,
			},
			repeat_mode:    defaults.repeat_mode,
			repeat_gap:     defaults.repeat_gap,
			builtin_rules:  self.builtin_rules,
			disabled_rules: self.disabled_rules.iter().cloned().collect::<HashSet<_>>(),
		}
	}
}

impl SettingsDomain for TtsrSettings {
	const DOMAIN: &'static str = "ttsr";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"ttsr.enabled",
			"TTSR",
			"Enable time-traveling stream rules.",
			SettingKind::Boolean,
			10,
		),
		field(
			"ttsr.contextMode",
			"TTSR Context",
			"Partial-output treatment after interruption.",
			SettingKind::Enum(&["discard", "keep"]),
			20,
		),
		field(
			"ttsr.interruptMode",
			"TTSR Interrupt",
			"Streams on which a rule interrupts generation.",
			SettingKind::Enum(&["never", "prose-only", "tool-only", "always"]),
			30,
		),
		field(
			"ttsr.builtinRules",
			"Bundled TTSR Rules",
			"Enable bundled stream rules.",
			SettingKind::Boolean,
			40,
		),
		field(
			"ttsr.disabledRules",
			"Disabled TTSR Rules",
			"Bundled or user rule names to disable.",
			SettingKind::Array,
			50,
		),
	];
}

/// Durable encrypted-share transport.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum ShareStore {
	/// Upload directly to the configured encrypted blob endpoint.
	#[default]
	Http,
	/// Upload to a secret GitHub gist, with HTTP fallback.
	Gist,
}

/// Encrypted session-sharing endpoint and backing store.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ShareSettings {
	/// Viewer/upload base used for encrypted share links.
	pub server_url: Str,
	/// Preferred ciphertext store.
	pub store:      ShareStore,
}

impl Default for ShareSettings {
	fn default() -> Self {
		Self { server_url: Str::new_static("https://omp.dev/share"), store: ShareStore::Http }
	}
}

impl SettingsDomain for ShareSettings {
	const DOMAIN: &'static str = "share";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"share.serverUrl",
			"Share Server",
			"Encrypted share viewer and upload base URL.",
			SettingKind::String,
			10,
		),
		field(
			"share.store",
			"Share Store",
			"Preferred encrypted share backing store.",
			SettingKind::Enum(&["http", "gist"]),
			20,
		),
	];
}

/// Session title generation policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TitleSettings {
	/// Refresh an automatically generated title when the plan is replaced.
	pub refresh_on_replan: bool,
}

impl Default for TitleSettings {
	fn default() -> Self {
		Self { refresh_on_replan: true }
	}
}

impl SettingsDomain for TitleSettings {
	const DOMAIN: &'static str = "title";
	const FIELDS: &'static [FieldDescriptor] = &[field(
		"title.refreshOnReplan",
		"Refresh Title on Replan",
		"Regenerate an automatic session title after plan replacement.",
		SettingKind::Boolean,
		10,
	)];
}

/// Idle recap generation policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RecapSettings {
	/// Generate an ephemeral recap after the session becomes idle.
	pub enabled:      bool,
	/// Seconds to wait while idle before requesting the recap.
	pub idle_seconds: u64,
}

impl Default for RecapSettings {
	fn default() -> Self {
		Self { enabled: true, idle_seconds: 240 }
	}
}

impl SettingsDomain for RecapSettings {
	const DOMAIN: &'static str = "recap";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"recap.enabled",
			"Idle Recap",
			"Generate a brief LLM recap of where things stand after the terminal has been idle",
			SettingKind::Boolean,
			10,
		),
		FieldDescriptor {
			path:        "recap.idleSeconds",
			label:       "Idle Recap Delay",
			description: "Seconds to wait while idle before showing the recap",
			kind:        SettingKind::Integer,
			scopes:      PERSISTED,
			order:       20,
			options:     Some(OptionProvider::Static(&[
				SettingOption { value: "60", label: "1 minute", description: None },
				SettingOption { value: "120", label: "2 minutes", description: None },
				SettingOption { value: "240", label: "4 minutes", description: None },
				SettingOption { value: "300", label: "5 minutes", description: None },
				SettingOption { value: "600", label: "10 minutes", description: None },
			])),
			condition:   None,
			secret:      false,
		},
	];
}

/// Marketplace catalog refresh and update policy.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum MarketplaceUpdateMode {
	/// Never refresh or install marketplace packages automatically.
	Off,
	/// Refresh stale catalogs and report available updates.
	Notify,
	/// Refresh stale catalogs and install eligible updates.
	#[default]
	Auto,
}

/// Credential database encryption-key source selected deliberately at startup.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum CredentialKeySourceSetting {
	/// Owner-only local file when the process is interactive; otherwise fail
	/// closed.
	#[default]
	Auto,
	/// Refuse durable secret reads and writes.
	Unavailable,
	/// Use an owner-only file beside the credential database.
	LocalFile,
	/// Use the operating-system credential service.
	OsKeychain,
}

/// Miscellaneous startup, retention, gateway, and workspace policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LifecycleSettings {
	/// Credential database encryption-key source.
	pub credential_key_source:   CredentialKeySourceSetting,
	/// Optional authentication broker gateway origin.
	pub auth_broker_gateway:     Option<Str>,
	/// Submit anonymized automatic QA failure reports.
	pub autoqa_reporting:        bool,
	/// Redeem an available Codex reset automatically.
	pub codex_reset_auto_redeem: bool,
	/// Materialize handoff children as durable indexed journals.
	pub handoff_save_to_disk:    bool,
	/// Number of days durable garbage remains eligible for recovery.
	pub gc_retention_days:       u32,
	/// Show the interactive startup splash.
	pub startup_splash:          bool,
	/// Run the first-use setup wizard when required.
	pub startup_wizard:          bool,
	/// Check for releases at startup.
	pub startup_update_check:    bool,
	/// Changelog display policy.
	pub changelog_mode:          Str,
	/// Marketplace catalog refresh and package update policy.
	pub marketplace_auto_update: MarketplaceUpdateMode,
	/// Enabled extension identifiers.
	pub extensions:              Vec<Str>,
	/// Permit session sharing.
	pub share_enabled:           bool,
	/// Enable prompt-injection scanning.
	pub prompt_injection_scan:   bool,
	/// Additional workspace roots.
	pub workspace_roots:         Vec<Str>,
	/// Optional personality preset.
	pub personality:             Option<Str>,
	/// Include Git context in workspace prompts.
	pub git_context:             bool,
	/// Include shell context in workspace prompts.
	pub shell_context:           bool,
	/// Apply configured tree filters during discovery.
	pub tree_filter:             bool,
}

impl Default for LifecycleSettings {
	fn default() -> Self {
		Self {
			credential_key_source:   CredentialKeySourceSetting::Auto,
			auth_broker_gateway:     None,
			autoqa_reporting:        false,
			codex_reset_auto_redeem: false,
			handoff_save_to_disk:    true,
			gc_retention_days:       30,
			startup_splash:          true,
			startup_wizard:          true,
			startup_update_check:    true,
			changelog_mode:          Str::new_static("unread"),
			marketplace_auto_update: MarketplaceUpdateMode::Auto,
			extensions:              Vec::new(),
			share_enabled:           true,
			prompt_injection_scan:   true,
			workspace_roots:         Vec::new(),
			personality:             None,
			git_context:             true,
			shell_context:           true,
			tree_filter:             true,
		}
	}
}

impl SettingsDomain for LifecycleSettings {
	const DOMAIN: &'static str = "lifecycle";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "lifecycle.credentialKeySource",
			label:       "Credential Key Source",
			description: "Global durable credential encryption source; auto uses an owner-only local \
			              file for interactive processes and otherwise fails closed.",
			kind:        SettingKind::Enum(&["auto", "unavailable", "local-file", "os-keychain"]),
			scopes:      GLOBAL,
			order:       5,
			options:     None,
			condition:   None,
			secret:      false,
		},
		field(
			"lifecycle.authBrokerGateway",
			"Auth Broker Gateway",
			"Authentication broker gateway origin.",
			SettingKind::String,
			10,
		),
		field(
			"lifecycle.autoqaReporting",
			"AutoQA Reporting",
			"Submit anonymized automatic QA reports.",
			SettingKind::Boolean,
			20,
		),
		field(
			"lifecycle.codexResetAutoRedeem",
			"Codex Reset Redemption",
			"Redeem available Codex resets automatically.",
			SettingKind::Boolean,
			30,
		),
		field(
			"lifecycle.handoffSaveToDisk",
			"Save Handoff Sessions",
			"Materialize handoff children as durable indexed journals.",
			SettingKind::Boolean,
			35,
		),
		field(
			"lifecycle.gcRetentionDays",
			"GC Retention",
			"Recovery retention in days.",
			SettingKind::Integer,
			40,
		),
		field(
			"lifecycle.startupSplash",
			"Startup Splash",
			"Show the startup splash.",
			SettingKind::Boolean,
			50,
		),
		field(
			"lifecycle.startupWizard",
			"Startup Wizard",
			"Run first-use setup when needed.",
			SettingKind::Boolean,
			60,
		),
		field(
			"lifecycle.startupUpdateCheck",
			"Startup Update Check",
			"Check for releases at startup.",
			SettingKind::Boolean,
			70,
		),
		field(
			"lifecycle.changelogMode",
			"Changelog Mode",
			"Changelog display policy.",
			SettingKind::Enum(&["never", "unread", "always"]),
			80,
		),
		field(
			"lifecycle.marketplaceAutoUpdate",
			"Marketplace Auto-update",
			"Refresh stale catalogs and optionally install eligible updates.",
			SettingKind::Enum(&["off", "notify", "auto"]),
			90,
		),
		field(
			"lifecycle.extensions",
			"Extensions",
			"Enabled extension identifiers.",
			SettingKind::Array,
			100,
		),
		field(
			"lifecycle.shareEnabled",
			"Session Sharing",
			"Permit session sharing.",
			SettingKind::Boolean,
			110,
		),
		field(
			"lifecycle.promptInjectionScan",
			"Prompt Injection Scan",
			"Scan untrusted prompt contributions.",
			SettingKind::Boolean,
			120,
		),
		field(
			"lifecycle.workspaceRoots",
			"Workspace Roots",
			"Additional workspace roots.",
			SettingKind::Array,
			130,
		),
		field(
			"lifecycle.personality",
			"Personality",
			"Optional personality preset.",
			SettingKind::String,
			140,
		),
		field(
			"lifecycle.gitContext",
			"Git Context",
			"Include Git context in workspace prompts.",
			SettingKind::Boolean,
			150,
		),
		field(
			"lifecycle.shellContext",
			"Shell Context",
			"Include shell context in workspace prompts.",
			SettingKind::Boolean,
			160,
		),
		field(
			"lifecycle.treeFilter",
			"Tree Filter",
			"Apply configured tree filters.",
			SettingKind::Boolean,
			170,
		),
	];
}

omp_settings::inventory::submit! { DomainRegistration::of::<DisplaySettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<AppearanceSettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<TuiSettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<RootDisplaySettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<CompletionSettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<ErrorNotificationSettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<InteractionSettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<TtsrSettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<ShareSettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<TitleSettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<RecapSettings>() }
omp_settings::inventory::submit! { DomainRegistration::of::<LifecycleSettings>() }
