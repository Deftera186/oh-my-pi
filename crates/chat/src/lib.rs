#![warn(missing_docs)]
//! Projection-only interactive actor over the OMP session patch stream.

/// Console commands the interactive actor executes locally.
pub mod actions;
/// Composer autocomplete providers.
pub mod autocomplete;
/// Typed tool cards.
pub mod cards;
/// Boot chrome: welcome banner, status band, composer shell.
pub mod chrome;
/// Observer-local composer.
pub mod composer;
/// External editor resolution and temporary-draft round trips.
pub mod editor;
/// Journal-derived tool-card gallery.
pub mod gallery;
/// Interactive terminal actor.
pub mod host;
/// Terminal input and command bindings.
pub mod input;
/// Observer-local overlays.
pub mod overlays;
/// Pure session-DOM transcript projection.
pub mod project;
/// DOM-derived status values.
pub mod status_line;

pub use actions::{HostAction, HostMailbox};
pub use chrome::ModelBadge;
pub use host::{
	CtrlCAction, Host, HostCommand, HostError, HostOptions, LocalFacts, NativeEffect, NativeHost,
	UpEvent, ctrl_c_action, render_surface,
};
pub use overlays::{ModelRow, PickerEvent};
pub use project::{BlockKind, BlockView, block_views};
