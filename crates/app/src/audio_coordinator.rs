//! Production composition boundary for shared audio ownership.
//!
//! The policy state machine remains in [`omp_voice::coordinator`]. This module
//! only adapts its suspension and gain transitions to the application's local
//! text-to-speech controller.

use std::sync::{
	Arc,
	atomic::{AtomicBool, AtomicU32, Ordering},
};

use omp_voice::coordinator::{AudioCoordinator, AudioEffects, MicrophoneLease};
use parking_lot::Mutex;

/// Application-side local text-to-speech controls consumed by the voice
/// coordinator adapter.
pub trait LocalTtsControl: Send + Sync + 'static {
	/// Suspend or resume creation and playback of local speech.
	fn set_suspended(&self, suspended: bool);

	/// Set render-time playback gain for current and future local speech.
	fn set_gain(&self, gain: f32);
}

struct ApplicationAudioEffects<C> {
	control: Arc<C>,
}

impl<C> AudioEffects for ApplicationAudioEffects<C>
where
	C: LocalTtsControl,
{
	fn set_tts_suspended(&self, suspended: bool) {
		self.control.set_suspended(suspended);
	}

	fn set_tts_gain(&self, gain: f32) {
		self.control.set_gain(gain);
	}
}

/// Application wrapper around the domain-owned audio coordinator.
#[derive(Clone)]
pub struct AppAudioCoordinator {
	domain: AudioCoordinator,
}

impl AppAudioCoordinator {
	/// Compose audio ownership policy with the production local-TTS controller.
	pub fn new<C>(control: Arc<C>) -> Self
	where
		C: LocalTtsControl,
	{
		let effects = Arc::new(ApplicationAudioEffects { control });
		Self { domain: AudioCoordinator::new(effects) }
	}

	/// Borrow the domain coordinator used by STT, live voice, and vocalization
	/// controllers to acquire their leases.
	pub fn domain(&self) -> &AudioCoordinator {
		&self.domain
	}
}

#[derive(Default)]
struct InteractiveTtsControl {
	suspended: AtomicBool,
	gain_bits: AtomicU32,
}

impl LocalTtsControl for InteractiveTtsControl {
	fn set_suspended(&self, suspended: bool) {
		self.suspended.store(suspended, Ordering::Release);
	}

	fn set_gain(&self, gain: f32) {
		self.gain_bits.store(gain.to_bits(), Ordering::Release);
	}
}

#[derive(Default)]
struct InteractiveAudioState {
	stt:        Option<MicrophoneLease>,
	live:       Option<MicrophoneLease>,
	live_muted: bool,
}

struct InteractiveAudioInner {
	coordinator: AppAudioCoordinator,
	state:       Mutex<InteractiveAudioState>,
}

/// Production session owner for interactive STT and realtime-voice microphone
/// leases.
///
/// A UI transition is acknowledged only after the shared audio authority grants
/// the requested lease. Competing microphone owners therefore fail instead of
/// presenting a synthetic enabled state.
#[derive(Clone)]
pub struct InteractiveAudioController {
	inner: Arc<InteractiveAudioInner>,
}

impl InteractiveAudioController {
	/// Creates one session-scoped controller over the production audio policy.
	pub fn new() -> Self {
		let control = Arc::new(InteractiveTtsControl {
			gain_bits: AtomicU32::new(1.0_f32.to_bits()),
			..InteractiveTtsControl::default()
		});
		Self {
			inner: Arc::new(InteractiveAudioInner {
				coordinator: AppAudioCoordinator::new(control),
				state:       Mutex::new(InteractiveAudioState::default()),
			}),
		}
	}

	/// Returns whether STT currently owns the microphone.
	pub fn stt_active(&self) -> bool {
		self.inner.state.lock().stt.is_some()
	}

	/// Returns whether live voice currently owns the microphone.
	pub fn live_active(&self) -> bool {
		self.inner.state.lock().live.is_some()
	}

	/// Toggles the real STT microphone lease and returns its new state.
	pub fn toggle_stt(&self) -> Result<bool, omp_voice::coordinator::CoordinatorError> {
		let mut state = self.inner.state.lock();
		if let Some(mut lease) = state.stt.take() {
			lease.release();
			return Ok(false);
		}
		state.stt = Some(self.inner.coordinator.domain().acquire_speech_to_text()?);
		Ok(true)
	}

	/// Starts live voice after acquiring exclusive microphone ownership.
	pub fn start_live(&self) -> Result<(), omp_voice::coordinator::CoordinatorError> {
		let mut state = self.inner.state.lock();
		if state.live.is_some() {
			return Ok(());
		}
		state.live = Some(self.inner.coordinator.domain().acquire_live()?);
		state.live_muted = false;
		Ok(())
	}

	/// Stops live voice and restores the prior TTS ownership state.
	pub fn stop_live(&self) {
		let mut state = self.inner.state.lock();
		if let Some(mut lease) = state.live.take() {
			lease.release();
		}
		state.live_muted = false;
	}

	/// Changes mute state only while a live session owns the microphone.
	pub fn set_live_muted(&self, muted: bool) -> Result<(), &'static str> {
		let mut state = self.inner.state.lock();
		if state.live.is_none() {
			return Err("live voice is not active");
		}
		state.live_muted = muted;
		Ok(())
	}
}

impl Default for InteractiveAudioController {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use super::InteractiveAudioController;

	#[test]
	fn interactive_controller_enforces_exclusive_microphone_ownership() {
		let audio = InteractiveAudioController::new();
		assert_eq!(audio.toggle_stt(), Ok(true));
		assert!(audio.stt_active());
		assert!(audio.start_live().is_err());
		assert_eq!(audio.toggle_stt(), Ok(false));
		audio.start_live().expect("live lease");
		assert!(audio.live_active());
		assert!(audio.set_live_muted(true).is_ok());
		audio.stop_live();
		assert!(!audio.live_active());
		assert!(audio.set_live_muted(false).is_err());
	}
}
