//! Error taxonomy shared by every engine backend.

use std::{io, path::PathBuf, result};

use omp_core::Str;
use png::DecodingError;
use tokio_tungstenite::tungstenite;

use crate::SurfaceKind;

/// Crate-wide result alias.
pub type Result<T, E = Error> = result::Result<T, E>;

/// Everything that can go wrong while creating or driving a web surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
	/// No installed browser satisfies the requested engine/surface combination.
	#[error("no usable browser engine found for `{0}` surface")]
	NoEngine(SurfaceKind),

	/// The engine binary failed to start.
	#[error("failed to launch `{binary}`: {source}")]
	Launch {
		/// Underlying spawn failure.
		source: io::Error,
		/// Binary that was being launched.
		binary: PathBuf,
	},

	/// The engine process exited or the automation socket closed.
	#[error("engine connection closed")]
	Closed,

	/// The engine sent traffic the driver could not interpret, or answered a
	/// command with an error.
	#[error("protocol error: {0}")]
	Protocol(Str),

	/// A Chromium screencast frame contained invalid base64.
	#[error("screencast frame base64: {source}")]
	ScreencastFrameBase64 {
		/// Underlying base64 decoding failure.
		#[source]
		source: omp_core::encoding::DecodeError,
	},

	/// A Firefox screenshot contained invalid base64.
	#[error("screenshot base64: {source}")]
	ScreenshotBase64 {
		/// Underlying base64 decoding failure.
		#[source]
		source: omp_core::encoding::DecodeError,
	},

	/// A JPEG frame could not be decoded.
	#[error("jpeg: {0}")]
	Jpeg(#[source] zune_jpeg::errors::DecodeErrors),

	/// A PNG frame could not be decoded.
	#[error("png: {0}")]
	Png(#[source] DecodingError),

	/// A captured RGBA frame could not be encoded as PNG.
	#[error("png encode: {0}")]
	PngEncode(#[source] png::EncodingError),
	/// A websocket message contained malformed JSON.
	#[error("malformed message: {0}")]
	MalformedMessage(#[from] serde_json::Error),

	/// Websocket transport failure while talking to a remote engine.
	#[error("websocket error: {0}")]
	WebSocket(#[from] tungstenite::Error),

	/// The operation is not supported by this engine/surface combination.
	/// See the capability matrix in the crate docs.
	#[error("unsupported operation: {0}")]
	Unsupported(&'static str),

	/// The engine did not reach the expected state in time.
	#[error("timed out while {0}")]
	Timeout(&'static str),

	/// A system-webview operation was invoked off the main thread.
	#[error("system webview operations require the main thread")]
	MainThread,

	/// The host window handle is not usable on this platform.
	#[error("unsupported window handle for this platform")]
	WindowHandle,

	/// Filesystem or process I/O failure (profile dirs, port files, ...).
	#[error(transparent)]
	Io(#[from] io::Error),
}
impl Error {
	pub(crate) const fn kind(&self) -> &'static str {
		match self {
			Self::NoEngine(_) => "no_engine",
			Self::Launch { .. } => "launch",
			Self::Closed => "closed",
			Self::Protocol(_) => "protocol",
			Self::ScreencastFrameBase64 { .. } => "screencast_frame_base64",
			Self::ScreenshotBase64 { .. } => "screenshot_base64",
			Self::Jpeg(_) => "jpeg",
			Self::Png(_) => "png",
			Self::PngEncode(_) => "png_encode",
			Self::MalformedMessage(_) => "malformed_message",
			Self::WebSocket(_) => "websocket",
			Self::Unsupported(_) => "unsupported",
			Self::Timeout(_) => "timeout",
			Self::MainThread => "main_thread",
			Self::WindowHandle => "window_handle",
			Self::Io(_) => "io",
		}
	}
}
