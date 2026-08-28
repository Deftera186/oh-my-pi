//! Direct default-device audio backends.
//!
//! Backends invoke callbacks on their own realtime threads, and guarantee that
//! an externally initiated `stop` waits out any in-flight callback. Queue depth
//! varies by backend and stream configuration; [`playback_drain_periods`]
//! reports the bound used by playback drain accounting.

#[cfg(all(feature = "native-audio", target_os = "macos"))]
mod coreaudio;
#[cfg(all(feature = "native-audio", target_os = "macos"))]
use coreaudio as imp;

#[cfg(all(feature = "native-audio", target_os = "windows"))]
mod wasapi;
#[cfg(all(feature = "native-audio", target_os = "windows"))]
use wasapi as imp;

#[cfg(all(feature = "native-audio", target_os = "linux"))]
mod linux;
#[cfg(all(feature = "native-audio", target_os = "linux"))]
use linux as imp;

#[cfg(not(all(
	feature = "native-audio",
	any(target_os = "macos", target_os = "windows", target_os = "linux")
)))]
mod unsupported {

	use std::env::consts;

	use super::{CaptureSink, DeviceConfig, PlaybackFill};
	use crate::{VoiceError, VoiceResult};

	pub(super) struct PlaybackDevice;

	impl PlaybackDevice {
		pub(super) fn start(config: DeviceConfig, _fill: PlaybackFill) -> VoiceResult<Self> {
			let _ = config.period_samples();

			Err(VoiceError::UnsupportedPlatform { platform: consts::OS })
		}

		pub(super) fn stop(&mut self) -> VoiceResult<()> {
			Ok(())
		}
	}

	pub(super) struct CaptureDevice;

	impl CaptureDevice {
		pub(super) fn start(config: DeviceConfig, _sink: CaptureSink) -> VoiceResult<Self> {
			let _ = config.period_samples();

			Err(VoiceError::UnsupportedPlatform { platform: consts::OS })
		}

		pub(super) fn stop(&mut self) -> VoiceResult<()> {
			Ok(())
		}
	}
}
#[cfg(not(all(
	feature = "native-audio",
	any(target_os = "macos", target_os = "windows", target_os = "linux")
)))]
use unsupported as imp;

#[cfg(feature = "native-audio")]
use crate::VoiceError;
use crate::VoiceResult;

#[cfg(all(
	feature = "native-audio",
	any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
pub(super) type BackendResult<T> = Result<T, String>;

pub(super) type PlaybackFill = Box<dyn FnMut(&mut [f32]) + Send + 'static>;
pub(super) type CaptureSink = Box<dyn FnMut(&[f32]) + Send + 'static>;

#[derive(Clone, Copy)]
pub(super) struct DeviceConfig {
	pub(super) sample_rate: u32,
	pub(super) period_ms:   u32,
}

impl DeviceConfig {
	pub(super) fn period_samples(self) -> usize {
		((self.sample_rate as usize * self.period_ms as usize) / 1000).max(1)
	}
}

pub(super) struct PlaybackDevice {
	inner: imp::PlaybackDevice,
}

impl PlaybackDevice {
	pub(super) fn start(config: DeviceConfig, fill: PlaybackFill) -> VoiceResult<Self> {
		#[cfg(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		))]
		let inner = imp::PlaybackDevice::start(config, fill).map_err(VoiceError::backend)?;
		#[cfg(not(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		)))]
		let inner = imp::PlaybackDevice::start(config, fill)?;
		Ok(Self { inner })
	}

	pub(super) fn stop(&mut self) -> VoiceResult<()> {
		#[cfg(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		))]
		return self.inner.stop().map_err(VoiceError::backend);
		#[cfg(not(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		)))]
		self.inner.stop()
	}
}

/// Returns the maximum number of callback periods queued by the playback
/// backend for this stream configuration.
#[cfg(all(feature = "native-audio", target_os = "linux"))]
pub(super) fn playback_drain_periods(config: DeviceConfig) -> u32 {
	imp::playback_drain_periods(config)
}

/// Returns the fixed playback queue depth used by non-PulseAudio backends.
#[cfg(not(all(feature = "native-audio", target_os = "linux")))]
pub(super) const fn playback_drain_periods(_config: DeviceConfig) -> u32 {
	3
}

pub(super) struct CaptureDevice {
	inner: imp::CaptureDevice,
}

impl CaptureDevice {
	pub(super) fn start(config: DeviceConfig, sink: CaptureSink) -> VoiceResult<Self> {
		#[cfg(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		))]
		let inner = imp::CaptureDevice::start(config, sink).map_err(VoiceError::backend)?;
		#[cfg(not(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		)))]
		let inner = imp::CaptureDevice::start(config, sink)?;
		Ok(Self { inner })
	}

	pub(super) fn stop(&mut self) -> VoiceResult<()> {
		#[cfg(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		))]
		return self.inner.stop().map_err(VoiceError::backend);
		#[cfg(not(all(
			feature = "native-audio",
			any(target_os = "macos", target_os = "windows", target_os = "linux")
		)))]
		self.inner.stop()
	}
}
