#![warn(missing_docs)]
//! Projection-only interactive actor over the OMP session patch stream.

/// Console commands the interactive actor executes locally.
pub mod actions;
/// Composer autocomplete providers.
pub mod autocomplete;
/// Typed tool cards.
pub mod cards;
/// Codex quota-reset celebration detector.
pub mod celebrate;
/// Boot chrome: welcome banner, status band, composer shell.
pub mod chrome;
/// Slash-command registry (`cmd!` declarations + palette metadata).
pub mod commands;
/// Observer-local composer.
pub mod composer;
/// External editor resolution and temporary-draft round trips.
pub mod editor;
/// Journal-derived tool-card gallery.
pub mod gallery;
/// Live git branch/dirty facts for the status band.
pub mod gitwatch;
/// Interactive terminal actor.
pub mod host;
/// Terminal input and command bindings.
pub mod input;
/// Transcript notices, maintenance dividers, usage rows, and the vocalizer.
pub mod notices;
/// Desktop notifications on turn completion, error, and pending questions.
pub mod notify;
/// Observer-local overlays.
pub mod overlays;
/// Pure session-DOM transcript projection.
pub mod project;
/// Composer status band.
pub mod status_band;
/// DOM-derived status values.
pub mod status_line;
/// Reasoning text prepared for display (prose-only filter).
pub mod thinking;
/// Retained transcript ledger and observer-local transcript facts.
pub mod transcript;
/// Welcome banner.
pub mod welcome;

pub use actions::{HostAction, HostMailbox};
pub use chrome::ModelBadge;
pub use host::{
	CtrlCAction, Host, HostCommand, HostError, HostOptions, LocalFacts, NativeEffect, NativeHost,
	UpEvent, ctrl_c_action, render_surface,
};
pub use overlays::{ModelRow, PickerEvent};
pub use project::{BlockKind, BlockView, block_views};
