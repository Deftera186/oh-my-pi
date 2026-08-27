//! Authenticated Codex SDP signaling and realtime sideband coordination.

use std::{
	collections::{HashSet, VecDeque},
	error,
	future::Future,
	io,
	sync::Arc,
	time::Duration,
};

use futures::{SinkExt as _, StreamExt as _};
use http::{
	Request,
	header::{HeaderName, HeaderValue},
};
use omp_core::{Str, sf};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{net::TcpStream, time};
use tokio_tungstenite::{
	MaybeTlsStream, WebSocketStream, connect_async,
	tungstenite::{self, Message, client::IntoClientRequest as _},
};
use url::Url;

use crate::{
	VoiceError,
	attestation::generate_codex_attestation,
	coordinator::AudioCoordinator,
	live::{DEFAULT_OPEN_TIMEOUT_MS, LiveCallbacks, LiveMediaSession},
};

/// Codex live-call signaling endpoint.
pub const SIGNALING_URL: &str =
	"https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas";
const SIDEBAND_ATTEMPTS: usize = 5;
const CONTEXT_CHUNK_BYTES: usize = 500;
const DEDUP_WINDOW: usize = 4_096;

/// OAuth access leased by the application credential authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveOAuthAccess {
	/// Bearer access token; never serialized into receipts.
	pub access_token: Str,
	/// Optional ChatGPT account identity.
	pub account_id:   Option<Str>,
}

/// Inputs required to establish one live transport.
#[derive(Clone, Debug)]
pub struct LiveTransportOptions {
	/// Durable OMP session identity.
	pub session_id:       Str,
	/// Per-connection realtime identity.
	pub realtime_session: Str,
	/// Voice-friendly system instructions.
	pub instructions:     Str,
	/// Stable live voice identifier.
	pub voice:            Str,
	/// Codex client version header.
	pub client_version:   Str,
	/// Optional proxy selected by the network authority.
	pub proxy:            Option<Url>,
	/// OAuth lease.
	pub access:           LiveOAuthAccess,
	/// Data-channel open timeout.
	pub open_timeout:     Duration,
}

impl LiveTransportOptions {
	/// Creates options with the data-channel timeout.
	pub fn new(
		session_id: Str,
		instructions: Str,
		voice: Str,
		client_version: Str,
		access: LiveOAuthAccess,
	) -> Self {
		Self {
			session_id,
			realtime_session: Str::from(omp_core::Ulid::generate().to_string()),
			instructions,
			voice,
			client_version,
			proxy: None,
			access,
			open_timeout: Duration::from_millis(u64::from(DEFAULT_OPEN_TIMEOUT_MS)),
		}
	}
}

/// Complete authenticated signaling request produced by the voice domain.
#[derive(Clone, Debug)]
pub struct LiveSignalingRequest {
	/// Fixed Codex endpoint.
	pub url:     &'static str,
	/// Secret-bearing headers for the credential-aware HTTP boundary.
	pub headers: Vec<(Str, Str)>,
	/// JSON request body containing SDP and the session payload.
	pub body:    Vec<u8>,
	/// Proxy selected for this provider, if any.
	pub proxy:   Option<Url>,
}

/// Accepted SDP answer and server-assigned `rtc_*` call identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveSignalingResponse {
	/// Remote SDP answer.
	pub answer:   Str,
	/// HTTP Location header.
	pub location: Str,
}

/// Unboxed signaling boundary implemented by the application's authenticated
/// HTTP transport (including its proxy and OAuth refresh policy).
pub trait LiveSignalingClient {
	/// Typed signaling error.
	type Error: error::Error + Send + Sync + 'static;

	/// Posts one SDP offer.
	fn signal(
		&mut self,
		request: LiveSignalingRequest,
	) -> impl Future<Output = Result<LiveSignalingResponse, Self::Error>> + Send;
}

/// Sideband connection abstraction. The default implementation is direct;
/// application network composition can replace it with an HTTP/SOCKS proxy
/// connector while preserving the exact request and retry policy.
pub trait SidebandConnector {
	/// Connected websocket type.
	type Socket;
	/// Typed connection failure.
	type Error: error::Error + Send + Sync + 'static;

	/// Connects the authenticated sideband request through `proxy` when present.
	fn connect(
		&mut self,
		request: Request<()>,
		proxy: Option<&Url>,
	) -> impl Future<Output = Result<Self::Socket, Self::Error>> + Send;
}

/// Direct rustls sideband connector.
#[derive(Clone, Copy, Debug, Default)]
pub struct DirectSidebandConnector;

/// Direct connector failures.
#[derive(Debug, Error)]
pub enum DirectSidebandError {
	/// A proxy requires the application's proxy-capable network connector.
	#[error("a direct sideband connector cannot satisfy a proxy route")]
	ProxyRequired,
	/// Websocket connection failed.
	#[error("live sideband websocket failed")]
	WebSocket {
		/// Typed tungstenite source.
		#[source]
		source: tungstenite::Error,
	},
}

impl SidebandConnector for DirectSidebandConnector {
	type Error = DirectSidebandError;
	type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

	fn connect(
		&mut self,
		request: Request<()>,
		proxy: Option<&Url>,
	) -> impl Future<Output = Result<Self::Socket, Self::Error>> + Send {
		async move {
			if proxy.is_some() {
				return Err(DirectSidebandError::ProxyRequired);
			}
			connect_async(request)
				.await
				.map(|(socket, _)| socket)
				.map_err(|source| DirectSidebandError::WebSocket { source })
		}
	}
}

/// Realtime transport establishment failures.
#[derive(Debug, Error)]
pub enum LiveTransportError<S, W>
where
	S: error::Error + Send + Sync + 'static,
	W: error::Error + Send + Sync + 'static,
{
	/// Native media initialization or SDP answer failed.
	#[error(transparent)]
	Media {
		/// Typed media error.
		#[from]
		source: VoiceError,
	},
	/// Signaling HTTP transport failed.
	#[error("Codex live signaling failed")]
	Signaling {
		/// Typed HTTP source.
		#[source]
		source: S,
	},
	/// Signaling response omitted a valid call ID.
	#[error("Codex live signaling returned no valid rtc call ID")]
	MissingCallId,
	/// Signaling request body could not be serialized.
	#[error("Codex live session payload could not be serialized")]
	Payload {
		/// Typed JSON source.
		#[source]
		source: serde_json::Error,
	},
	/// Sideband request headers were invalid.
	#[error("Codex live sideband request contains an invalid header")]
	Header,
	/// Every exponential-backoff sideband attempt failed.
	#[error("Codex live sideband connection failed after five attempts")]
	Sideband {
		/// Final typed websocket source.
		#[source]
		source: W,
	},
}

/// Established media and sideband channels. Dropping does not block; callers
/// must invoke [`Self::close`] for deterministic coordinator restoration.
pub struct EstablishedLiveTransport<W> {
	media:    Arc<LiveMediaSession>,
	sideband: W,
}

impl<W> EstablishedLiveTransport<W> {
	/// Borrows the coordinator-owned media session.
	pub const fn media(&self) -> &Arc<LiveMediaSession> {
		&self.media
	}

	/// Borrows the sideband socket/connector result.
	pub const fn sideband(&self) -> &W {
		&self.sideband
	}

	/// Mutably borrows the sideband socket.
	pub const fn sideband_mut(&mut self) -> &mut W {
		&mut self.sideband
	}

	/// Closes media and restores microphone/TTS ownership exactly once.
	pub async fn close(self) {
		self.media.close().await;
	}
}

/// Creates the WebRTC offer, signals it with OAuth/attestation headers, accepts
/// the answer, waits for the data channel, then opens the sideband with bounded
/// exponential backoff.
pub async fn establish_live_transport<S, C>(
	coordinator: &AudioCoordinator,
	callbacks: LiveCallbacks,
	options: &LiveTransportOptions,
	signaling: &mut S,
	connector: &mut C,
) -> Result<EstablishedLiveTransport<C::Socket>, LiveTransportError<S::Error, C::Error>>
where
	S: LiveSignalingClient,
	C: SidebandConnector,
{
	let (media, offer) = LiveMediaSession::start(coordinator, callbacks).await?;
	let result = establish_after_media(&media, offer, options, signaling, connector).await;
	match result {
		Ok(sideband) => Ok(EstablishedLiveTransport { media, sideband }),
		Err(error) => {
			media.close().await;
			Err(error)
		},
	}
}

async fn establish_after_media<S, C>(
	media: &Arc<LiveMediaSession>,
	offer: String,
	options: &LiveTransportOptions,
	signaling: &mut S,
	connector: &mut C,
) -> Result<C::Socket, LiveTransportError<S::Error, C::Error>>
where
	S: LiveSignalingClient,
	C: SidebandConnector,
{
	let attestation = generate_codex_attestation().await;
	let request = signaling_request(options, &offer, attestation.as_deref())
		.map_err(|source| LiveTransportError::Payload { source })?;
	let response = signaling
		.signal(request)
		.await
		.map_err(|source| LiveTransportError::Signaling { source })?;
	let call_id =
		parse_live_call_id(response.location.as_str()).ok_or(LiveTransportError::MissingCallId)?;
	media
		.peer()
		.accept_answer(response.answer.to_string())
		.await?;
	let timeout_ms = options.open_timeout.as_millis().min(u128::from(u32::MAX)) as u32;
	media.peer().wait_for_open(timeout_ms).await?;
	let mut delay = Duration::from_millis(200);
	let mut last = None;
	for attempt in 0..SIDEBAND_ATTEMPTS {
		let request = sideband_request(options, call_id, attestation.as_deref())?;
		match connector.connect(request, options.proxy.as_ref()).await {
			Ok(socket) => return Ok(socket),
			Err(error) => last = Some(error),
		}
		if attempt + 1 < SIDEBAND_ATTEMPTS {
			time::sleep(delay).await;
			delay = delay.saturating_mul(2);
		}
	}
	Err(LiveTransportError::Sideband { source: last.expect("sideband attempt count is non-zero") })
}

fn signaling_request(
	options: &LiveTransportOptions,
	offer: &str,
	attestation: Option<&str>,
) -> Result<LiveSignalingRequest, serde_json::Error> {
	let body = serde_json::to_vec(&json!({
		"sdp": offer,
		"session": {
			"type": "realtime",
			"model": "gpt-realtime",
			"instructions": options.instructions,
			"audio": {
				"input": { "format": { "type": "audio/pcm", "rate": 24000 }, "turn_detection": { "type": "semantic_vad" } },
				"output": { "format": { "type": "audio/pcm", "rate": 24000 }, "voice": options.voice },
			},
		},
	}))?;
	Ok(LiveSignalingRequest {
		url: SIGNALING_URL,
		headers: session_headers(options, attestation),
		body,
		proxy: options.proxy.clone(),
	})
}

fn sideband_request<S, W>(
	options: &LiveTransportOptions,
	call_id: &str,
	attestation: Option<&str>,
) -> Result<Request<()>, LiveTransportError<S, W>>
where
	S: error::Error + Send + Sync + 'static,
	W: error::Error + Send + Sync + 'static,
{
	let url = format!("wss://api.openai.com/v1/live/{call_id}");
	let mut request = url
		.into_client_request()
		.map_err(|_| LiveTransportError::Header)?;
	for (name, value) in session_headers(options, attestation) {
		let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| LiveTransportError::Header)?;
		let value = HeaderValue::from_str(value.as_str()).map_err(|_| LiveTransportError::Header)?;
		request.headers_mut().insert(name, value);
	}
	Ok(request)
}

fn session_headers(options: &LiveTransportOptions, attestation: Option<&str>) -> Vec<(Str, Str)> {
	let mut headers = vec![
		(Str::new_static("authorization"), sf!("Bearer {}", options.access.access_token)),
		(Str::new_static("OpenAI-Alpha"), Str::new_static("quicksilver=v2")),
		(Str::new_static("user-agent"), sf!("Codex Desktop/{}", options.client_version)),
		(Str::new_static("x-session-id"), options.realtime_session.clone()),
		(Str::new_static("originator"), Str::new_static("Codex Desktop")),
		(Str::new_static("x-codex-version"), options.client_version.clone()),
		(Str::new_static("x-codex-session-id"), options.session_id.clone()),
		(Str::new_static("x-codex-thread-id"), options.session_id.clone()),
	];
	if let Some(account) = options.access.account_id.as_ref() {
		headers.push((Str::new_static("ChatGPT-Account-Id"), account.clone()));
	}
	if let Some(attestation) = attestation {
		headers.push((Str::new_static("x-oai-attestation"), Str::from(attestation)));
	}
	headers
}

/// Extracts a validated server-assigned `rtc_*` call ID from Location.
pub fn parse_live_call_id(location: &str) -> Option<&str> {
	location
		.split_once('?')
		.map_or(location, |(path, _)| path)
		.split('/')
		.find(|segment| {
			segment.starts_with("rtc_")
				&& segment.len() > 4
				&& segment[4..]
					.bytes()
					.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
		})
}

/// Splits context into UTF-8-safe chunks no larger than 500 bytes.
pub fn chunk_live_context(text: &str) -> impl Iterator<Item = &str> {
	ContextChunks { remaining: text }
}

struct ContextChunks<'a> {
	remaining: &'a str,
}

impl<'a> Iterator for ContextChunks<'a> {
	type Item = &'a str;

	fn next(&mut self) -> Option<Self::Item> {
		if self.remaining.is_empty() {
			return None;
		}
		let mut end = self.remaining.len().min(CONTEXT_CHUNK_BYTES);
		while !self.remaining.is_char_boundary(end) {
			end -= 1;
		}
		let (chunk, remaining) = self.remaining.split_at(end);
		self.remaining = remaining;
		Some(chunk)
	}
}

/// Bounded cross-channel event-ID deduplicator for data-channel and sideband
/// deliveries. Events without IDs remain deliverable.
#[derive(Debug)]
pub struct EventDeduplicator {
	seen:  HashSet<Str>,
	order: VecDeque<Str>,
}

impl Default for EventDeduplicator {
	fn default() -> Self {
		Self { seen: HashSet::with_capacity(DEDUP_WINDOW), order: VecDeque::new() }
	}
}

impl EventDeduplicator {
	/// Returns `true` exactly once for each event ID in the bounded window.
	pub fn admit(&mut self, payload: &str) -> bool {
		let Some(id) = event_id(payload) else {
			return true;
		};
		if !self.seen.insert(id.clone()) {
			return false;
		}
		self.order.push_back(id);
		if self.order.len() > DEDUP_WINDOW
			&& let Some(expired) = self.order.pop_front()
		{
			self.seen.remove(&expired);
		}
		true
	}
}

fn event_id(payload: &str) -> Option<Str> {
	let value: Value = serde_json::from_str(payload).ok()?;
	value
		.get("event_id")
		.or_else(|| value.get("id"))
		.and_then(Value::as_str)
		.filter(|id| !id.is_empty())
		.map(Str::from)
}

/// Sends one JSON event over a connected direct sideband socket.
pub async fn send_sideband(
	socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
	value: &impl Serialize,
) -> Result<(), tungstenite::Error> {
	let payload = serde_json::to_string(value)
		.map_err(|error| tungstenite::Error::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?;
	socket.send(Message::Text(payload.into())).await
}

/// Receives the next text sideband event, ignoring ping/pong/binary frames.
pub async fn receive_sideband(
	socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<Option<Str>, tungstenite::Error> {
	while let Some(message) = socket.next().await {
		match message? {
			Message::Text(text) => return Ok(Some(Str::from(text.as_str()))),
			Message::Close(_) => return Ok(None),
			_ => {},
		}
	}
	Ok(None)
}
