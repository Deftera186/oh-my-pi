//! Production pooled HTTP/1.1 and HTTP/2 streaming transport over rustls.

use std::{
	collections::VecDeque,
	convert::Infallible,
	error,
	future::Future,
	io, mem,
	pin::Pin,
	str,
	sync::{Arc, LazyLock},
	task::{Context, Poll},
	time,
};

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt as _, future::poll_fn, stream};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Uri, header};
use http_body_util::{BodyExt as _, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::{Frame as BodyFrame, Incoming};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
	client::legacy::{Client, connect::HttpConnector},
	rt::{TokioExecutor, TokioIo},
};
use omp_core::Str;
use parking_lot::Mutex;
use rustls::crypto::ring;
use smallvec::SmallVec;
use tokio::{
	io::{AsyncReadExt as _, AsyncWriteExt as _},
	net::TcpStream,
	runtime,
	time::{Instant, sleep_until},
};
use tower::{Service, ServiceExt as _};
use url::Url;
use zeroize::Zeroizing;

use crate::{
	body::{
		AttemptBodyEvidence, AttemptEvidenceHandle, BodyFactoryHandle, BodyOpenError, BodyReader,
		BodySource, ByteStream, byte_stream,
	},
	catalog::OperationKind,
	codec::{
		Cancellation, DecoderState, HandshakeMeta, HandshakenResponse, RawEvent, RawEventStream,
		RequestHeader, RequestMethod, TransportAttempt, TransportRequest,
	},
	error::{
		Error, ErrorDetail, ErrorKind, ErrorPhase, RetryAction, classify_provider_rejection,
		is_transient_generation_fault,
	},
	receipt::{
		AttemptOutcome, AttemptReceipt, Cost, ExecutionReceipt, ProviderEvidence, ReasonId, Usage,
	},
	transport::{
		ConnectDecoder, CrcScope, EventStreamDecoder, Frame, FramingError, FramingProtocol,
		NdjsonDecoder, RawChunkFramer, SseDecoder,
		browser::{
			BrowserFetch, BrowserFetchError, BrowserFetchRequest, BrowserHeader,
			MAX_BROWSER_BODY_BYTES as BROWSER_MAX_BROWSER_BODY_BYTES, MAX_BROWSER_DEADLINE,
		},
		cassette::{CapturedFrame, capture_frame, is_commit_candidate},
		proxy,
	},
};

const MAX_CAPTURED_HEADERS: usize = 32;
const MAX_CAPTURED_HEADER_BYTES: usize = 32;
const MAX_PROVIDER_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_PROVIDER_MESSAGE_BYTES: usize = 1_024;
const MAX_PROVIDER_CODE_BYTES: usize = 128;
const PUBLIC_NUMERIC_HEADERS: [&str; 13] = [
	"content-length",
	"retry-after",
	"ratelimit-limit",
	"ratelimit-remaining",
	"ratelimit-reset",
	"x-ratelimit-limit",
	"x-ratelimit-remaining",
	"x-ratelimit-reset",
	"x-ratelimit-limit-requests",
	"x-ratelimit-remaining-requests",
	"x-ratelimit-limit-tokens",
	"x-ratelimit-remaining-tokens",
	"x-ratelimit-reset-tokens",
];

/// `LiteLLM` (and compatible proxies) shed over-concurrency requests *before*
/// the upstream call with an immediate HTTP 429 marked `rate_limit_type:
/// max_parallel_requests` — as a response header and/or a structured body
/// field (the codec classifies the body form). The admission failure never
/// reached a model, so honoring the proxy's `Retry-After`-scale hint on the
/// same route duplicates the backoff and model fallback the router already
/// owns, stalling one turn for minutes (pi #8854).
const CONCURRENCY_ADMISSION_HEADER: &str = "rate_limit_type";
const CONCURRENCY_ADMISSION_LIMITER: &[u8] = b"max_parallel_requests";

const MAX_REQUEST_ID_BYTES: usize = 128;
const PUBLIC_REQUEST_ID_HEADERS: [&str; 5] =
	["x-request-id", "request-id", "x-amzn-requestid", "x-goog-request-id", "cf-ray"];

type Connector = HttpsConnector<EnvProxyConnector>;
type RequestBody = UnsyncBoxBody<Bytes, Error>;
type PooledClient = Client<Connector, RequestBody>;
type ConnectorError = Box<dyn error::Error + Send + Sync>;

#[derive(Clone, Debug)]
struct EnvProxyConnector {
	direct: HttpConnector,
}

impl EnvProxyConnector {
	fn new() -> Self {
		let mut direct = HttpConnector::new();
		direct.enforce_http(false);
		Self { direct }
	}
}

impl Service<Uri> for EnvProxyConnector {
	type Error = ConnectorError;
	type Future =
		Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;
	type Response = TokioIo<TcpStream>;

	fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		match self.direct.poll_ready(context) {
			Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
			Poll::Ready(Err(error)) => Poll::Ready(Err(Box::new(error) as ConnectorError)),
			Poll::Pending => Poll::Pending,
		}
	}

	fn call(&mut self, destination: Uri) -> Self::Future {
		let mut direct = self.direct.clone();
		Box::pin(async move {
			let target = Url::parse(&destination.to_string()).ok();
			let selected = target.as_ref().and_then(proxy::for_url);
			let Some(proxy) = selected else {
				return direct
					.call(destination)
					.await
					.map_err(|error| Box::new(error) as ConnectorError);
			};
			if proxy.scheme() != "http" {
				return Err(connector_error("proxy URL must use http"));
			}
			let proxy_host = proxy
				.host_str()
				.ok_or_else(|| connector_error("proxy URL has no host"))?;
			let proxy_port = proxy.port().unwrap_or(80);
			let proxy_authority = if proxy_host.contains(':') {
				format!("[{proxy_host}]:{proxy_port}")
			} else {
				format!("{proxy_host}:{proxy_port}")
			};
			let proxy_uri = format!("http://{proxy_authority}")
				.parse::<Uri>()
				.map_err(|_| connector_error("proxy URL is invalid"))?;
			let mut stream = direct
				.call(proxy_uri)
				.await
				.map_err(|error| Box::new(error) as ConnectorError)?;
			let authorization = proxy_basic_authorization(&proxy)?;
			connect_tunnel(
				&mut stream,
				&destination,
				authorization.as_ref().map(|value| value.as_str()),
			)
			.await?;
			Ok(stream)
		})
	}
}

fn connector_error(message: &'static str) -> ConnectorError {
	Box::new(io::Error::new(io::ErrorKind::InvalidInput, message))
}

fn proxy_basic_authorization(proxy: &Url) -> Result<Option<Zeroizing<String>>, ConnectorError> {
	if proxy.username().is_empty() && proxy.password().is_none() {
		return Ok(None);
	}
	let username = Zeroizing::new(
		percent_decode(proxy.username())
			.ok_or_else(|| connector_error("proxy username has invalid percent encoding"))?,
	);
	let password = Zeroizing::new(
		percent_decode(proxy.password().unwrap_or_default())
			.ok_or_else(|| connector_error("proxy password has invalid percent encoding"))?,
	);
	let mut credentials = Zeroizing::new(Vec::with_capacity(username.len() + password.len() + 1));
	credentials.extend_from_slice(&username);
	credentials.push(b':');
	credentials.extend_from_slice(&password);
	Ok(Some(Zeroizing::new(base64(&credentials))))
}

fn percent_decode(value: &str) -> Option<Vec<u8>> {
	let bytes = value.as_bytes();
	let mut output = Vec::with_capacity(bytes.len());
	let mut index = 0;
	while index < bytes.len() {
		if bytes[index] != b'%' {
			output.push(bytes[index]);
			index += 1;
			continue;
		}
		let high = hex_digit(*bytes.get(index + 1)?)?;
		let low = hex_digit(*bytes.get(index + 2)?)?;
		output.push((high << 4) | low);
		index += 3;
	}
	Some(output)
}

const fn hex_digit(byte: u8) -> Option<u8> {
	match byte {
		b'0'..=b'9' => Some(byte - b'0'),
		b'a'..=b'f' => Some(byte - b'a' + 10),
		b'A'..=b'F' => Some(byte - b'A' + 10),
		_ => None,
	}
}

fn base64(input: &[u8]) -> String {
	const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
	for chunk in input.chunks(3) {
		let bits = (u32::from(chunk[0]) << 16)
			| (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
			| u32::from(*chunk.get(2).unwrap_or(&0));
		output.push(ALPHABET[((bits >> 18) & 0x3f) as usize] as char);
		output.push(ALPHABET[((bits >> 12) & 0x3f) as usize] as char);
		output.push(if chunk.len() > 1 {
			ALPHABET[((bits >> 6) & 0x3f) as usize] as char
		} else {
			'='
		});
		output.push(if chunk.len() > 2 {
			ALPHABET[(bits & 0x3f) as usize] as char
		} else {
			'='
		});
	}
	output
}

async fn connect_tunnel(
	stream: &mut TokioIo<TcpStream>,
	destination: &Uri,
	authorization: Option<&str>,
) -> Result<(), ConnectorError> {
	let host = destination
		.host()
		.ok_or_else(|| connector_error("proxy destination has no host"))?;
	let port = destination.port_u16().unwrap_or_else(|| {
		if destination.scheme_str() == Some("https") {
			443
		} else {
			80
		}
	});
	let authority = if host.contains(':') {
		format!("[{host}]:{port}")
	} else {
		format!("{host}:{port}")
	};
	let authorization = authorization
		.map_or_else(String::new, |value| format!("Proxy-Authorization: Basic {value}\r\n"));
	let request = Zeroizing::new(format!(
		"CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{authorization}Proxy-Connection: \
		 Keep-Alive\r\n\r\n"
	));
	stream
		.inner_mut()
		.write_all(request.as_bytes())
		.await
		.map_err(|error| Box::new(error) as ConnectorError)?;
	let mut response = Vec::with_capacity(256);
	let mut byte = [0_u8; 1];
	while response.len() < 8 * 1024 && !response.ends_with(b"\r\n\r\n") {
		stream
			.inner_mut()
			.read_exact(&mut byte)
			.await
			.map_err(|error| Box::new(error) as ConnectorError)?;
		response.push(byte[0]);
	}
	if !response.ends_with(b"\r\n\r\n") {
		return Err(connector_error("proxy CONNECT response exceeds header bound"));
	}
	let first_line = response
		.split(|byte| *byte == b'\n')
		.next()
		.and_then(|line| str::from_utf8(line).ok())
		.unwrap_or_default();
	let accepted = first_line
		.split_ascii_whitespace()
		.nth(1)
		.is_some_and(|status| status == "200");
	if !accepted {
		return Err(connector_error("proxy rejected CONNECT tunnel"));
	}
	Ok(())
}

static POOLED_CLIENT: LazyLock<PooledClient> = LazyLock::new(|| {
	let _ = ring::default_provider().install_default();
	let connector = HttpsConnectorBuilder::new()
		.with_webpki_roots()
		.https_or_http()
		.enable_http1()
		.enable_http2()
		.wrap_connector(EnvProxyConnector::new());
	Client::builder(TokioExecutor::new()).build(connector)
});

/// Outcome of scheduling a credential-free best-effort host preconnect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum PreconnectLaunch {
	/// A background DNS/TCP/TLS/HTTP handshake was scheduled.
	Scheduled,
	/// Construction happened outside a Tokio runtime.
	NoRuntime,
	/// The endpoint is not an HTTP(S) URL.
	UnsupportedEndpoint,
	/// The endpoint could not be represented as an HTTP URI.
	InvalidEndpoint,
}

fn pooled_client() -> PooledClient {
	POOLED_CLIENT.clone()
}

/// Sanitized bounded evidence retained from a live HTTP attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpCapture {
	/// Zero-based attempt index.
	pub attempt:             u32,
	/// HTTP response status.
	pub status:              u16,
	/// Provider request identifier, if present.
	pub provider_request_id: Option<Str>,
	/// Exact body evidence observed when response headers arrived.
	pub body:                AttemptBodyEvidence,
	/// Payload-free frame records retained within the attempt capture budget.
	pub frames:              Vec<CapturedFrame>,
}

struct HttpCaptureRecord {
	snapshot: Arc<Mutex<HttpCapture>>,
	evidence: AttemptEvidenceHandle,
}

/// Pooled production HTTP transport using the workspace rustls stack.
///
/// `poll_ready` is run on the same pooled client value moved into the following
/// `call`; a clone replaces it only for the next readiness cycle. Request-body
/// factories are opened inside that call, exactly once per attempt.
pub struct HttpTransport {
	inner:        Option<PooledClient>,
	ready_permit: bool,
	captures:     Arc<Mutex<Vec<HttpCaptureRecord>>>,
	browser:      Option<Arc<dyn BrowserFetch>>,
}

impl Clone for HttpTransport {
	fn clone(&self) -> Self {
		Self {
			inner:        self.inner.clone(),
			ready_permit: false,
			captures:     Arc::clone(&self.captures),
			browser:      self.browser.clone(),
		}
	}
}

impl Default for HttpTransport {
	fn default() -> Self {
		Self::new()
	}
}

/// Creates a bounded deferred HTTPS download stream for a provider-returned
/// artifact URL.
pub(crate) fn remote_artifact_stream(uri: &str, maximum_bytes: u64) -> Result<ByteStream, Error> {
	let parsed = Url::parse(uri)
		.map_err(|_| protocol_error(ErrorPhase::Encoding, false, "artifact-url-invalid"))?;
	if parsed.scheme() != "https"
		|| parsed.host_str().is_none()
		|| !parsed.username().is_empty()
		|| parsed.password().is_some()
		|| parsed.fragment().is_some()
	{
		return Err(protocol_error(ErrorPhase::Encoding, false, "artifact-url-untrusted"));
	}
	let uri = parsed
		.as_str()
		.parse::<Uri>()
		.map_err(|_| protocol_error(ErrorPhase::Encoding, false, "artifact-url-invalid"))?;
	Ok(Box::pin(stream::once(async move {
		let download = async move {
			let body = Full::new(Bytes::new())
				.map_err(|never: Infallible| match never {})
				.boxed_unsync();
			let request = Request::builder()
				.method(Method::GET)
				.uri(uri)
				.header(header::ACCEPT, "image/*")
				.body(body)
				.map_err(|_| protocol_error(ErrorPhase::Encoding, false, "artifact-request-invalid"))?;
			let mut response = pooled_client().request(request).await.map_err(|_| {
				connectivity(ErrorPhase::Connecting, false, "artifact-download-connect")
			})?;
			if !response.status().is_success() {
				return Err(protocol_error(ErrorPhase::Handshake, false, "artifact-download-status"));
			}
			if response
				.headers()
				.get(header::CONTENT_LENGTH)
				.and_then(|value| value.to_str().ok())
				.and_then(|value| value.parse::<u64>().ok())
				.is_some_and(|length| length > maximum_bytes)
			{
				return Err(protocol_error(ErrorPhase::Handshake, false, "artifact-download-limit"));
			}
			let mut bytes = BytesMut::new();
			while let Some(frame) = response.body_mut().frame().await {
				let frame = frame.map_err(|_| {
					protocol_error(ErrorPhase::Streaming, true, "artifact-download-body")
				})?;
				if let Ok(data) = frame.into_data() {
					let observed = (bytes.len() as u64).saturating_add(data.len() as u64);
					if observed > maximum_bytes {
						return Err(protocol_error(
							ErrorPhase::Streaming,
							true,
							"artifact-download-limit",
						));
					}
					bytes.extend_from_slice(&data);
				}
			}
			Ok(bytes.freeze())
		};
		tokio::time::timeout(time::Duration::from_secs(60), download)
			.await
			.map_err(|_| protocol_error(ErrorPhase::Streaming, true, "artifact-download-timeout"))?
	})))
}

impl HttpTransport {
	/// Constructs a pooled rustls client supporting HTTP/1.1 and HTTP/2.
	pub fn new() -> Self {
		Self {
			inner:        Some(pooled_client()),
			ready_permit: false,
			captures:     Arc::new(Mutex::new(Vec::new())),
			browser:      None,
		}
	}

	/// Installs the application-owned browser escalation boundary used only by
	/// credential-free decoders that explicitly request one replay.
	pub fn with_browser_fetch(mut self, browser: impl BrowserFetch) -> Self {
		self.browser = Some(Arc::new(browser));
		self
	}

	/// Starts a best-effort credential-free preconnect on the same process-wide
	/// pool used by production provider requests.
	///
	/// The HEAD exchange carries no request headers or body and is detached from
	/// session construction. Failure is intentionally unobservable beyond the
	/// typed scheduling result because preconnect is only a latency
	/// optimization.
	pub fn preconnect_host(base_url: &Url) -> PreconnectLaunch {
		if !matches!(base_url.scheme(), "http" | "https") {
			return PreconnectLaunch::UnsupportedEndpoint;
		}
		if !base_url.username().is_empty() || base_url.password().is_some() {
			return PreconnectLaunch::InvalidEndpoint;
		}
		let mut target = base_url.clone();
		target.set_query(None);
		target.set_fragment(None);
		let Ok(uri) = target.as_str().parse::<Uri>() else {
			return PreconnectLaunch::InvalidEndpoint;
		};
		let Ok(runtime) = runtime::Handle::try_current() else {
			return PreconnectLaunch::NoRuntime;
		};
		let body = Full::new(Bytes::new())
			.map_err(|never: Infallible| match never {})
			.boxed_unsync();
		let Ok(request) = Request::builder().method(Method::HEAD).uri(uri).body(body) else {
			return PreconnectLaunch::InvalidEndpoint;
		};
		let client = pooled_client();
		runtime.spawn(async move {
			let _ = client.request(request).await;
		});
		PreconnectLaunch::Scheduled
	}

	/// Returns deterministic snapshots of completed and in-flight sanitized
	/// captures.
	pub fn captures(&self) -> Vec<HttpCapture> {
		let mut captures: Vec<_> = self
			.captures
			.lock()
			.iter()
			.map(|record| {
				let mut capture = record.snapshot.lock().clone();
				capture.body = record.evidence.evidence();
				capture
			})
			.collect();
		captures.sort_by_key(|capture| capture.attempt);
		captures
	}
}

impl Service<TransportRequest> for HttpTransport {
	type Error = Error;
	type Response = HandshakenResponse;

	type Future = impl Future<Output = Result<Self::Response, Self::Error>> + Send + 'static;

	fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		let Some(client) = self.inner.as_mut() else {
			return Poll::Ready(Err(protocol_error(
				ErrorPhase::Readiness,
				false,
				"http-readiness-state",
			)));
		};
		match client.poll_ready(context) {
			Poll::Ready(Ok(())) => {
				self.ready_permit = true;
				Poll::Ready(Ok(()))
			},
			Poll::Ready(Err(_)) => {
				Poll::Ready(Err(connectivity(ErrorPhase::Readiness, false, "http-client-not-ready")))
			},
			Poll::Pending => Poll::Pending,
		}
	}

	fn call(&mut self, request: TransportRequest) -> Self::Future {
		let permit = mem::take(&mut self.ready_permit);
		let client = if permit { self.inner.take() } else { None };
		if let Some(client) = &client {
			self.inner = Some(client.clone());
		}
		let captures = Arc::clone(&self.captures);
		let browser = self.browser.clone();
		async move {
			let client = client.ok_or_else(|| {
				protocol_error(ErrorPhase::Readiness, false, "call-without-readiness")
			})?;
			execute(client, request, captures, browser).await
		}
	}
}

async fn execute(
	client: PooledClient,
	mut transport: TransportRequest,
	captures: Arc<Mutex<Vec<HttpCaptureRecord>>>,
	browser: Option<Arc<dyn BrowserFetch>>,
) -> Result<HandshakenResponse, Error> {
	let started = Instant::now();
	let attempt = transport.attempt.clone();
	let browser_retry = browser_retry_request(&transport, browser);
	let mut body_attempt = transport.encoded.body.begin_attempt();
	let mut evidence = body_attempt.evidence_handle();
	let deadline = Instant::now() + attempt.timeout;
	if !matches!((transport.decoder.is_some(), transport.realtime.is_some()), (true, false)) {
		return Err(record_failure(
			protocol_error(ErrorPhase::Handshake, false, "http-decoder-cardinality"),
			&attempt,
			&evidence,
			None,
			None,
			started,
			false,
		));
	}
	if transport.encoded.framing == FramingProtocol::WebSocket {
		return Err(record_failure(
			protocol_error(ErrorPhase::Connecting, false, "websocket-requires-socket-transport"),
			&attempt,
			&evidence,
			None,
			None,
			started,
			false,
		));
	}
	if transport.cancel.is_cancelled() {
		return Err(record_failure(
			cancelled(false),
			&attempt,
			&evidence,
			None,
			None,
			started,
			false,
		));
	}

	let mut sealed_bound = false;
	if let Some(template) = transport.encoded.take_sealed_body() {
		let Some(credentials) = transport.credentials.as_ref() else {
			return Err(record_failure(
				authentication_error("sealed-body-credentials-missing"),
				&attempt,
				&evidence,
				None,
				None,
				started,
				false,
			));
		};
		let bytes = credentials
			.finalize_sealed_body(template, &transport.cancel, transport.encoded.bounds.request_body)
			.map_err(|_| {
				let error = if transport.cancel.is_cancelled() {
					cancelled(false)
				} else {
					authentication_error("sealed-body-finalization")
				};
				record_failure(error, &attempt, &evidence, None, None, started, false)
			})?;
		let finalized = bytes;
		transport.encoded.body = BodySource::Factory(BodyFactoryHandle::new(move || {
			let bytes = finalized.clone();
			async move { Ok(byte_stream(bytes)) }
		}));
		body_attempt = transport.encoded.body.begin_attempt();
		evidence = body_attempt.evidence_handle();
		sealed_bound = true;
	} else if transport
		.credentials
		.as_ref()
		.is_some_and(|credentials| credentials.requires_sealed_body())
	{
		return Err(record_failure(
			authentication_error("sealed-body-template-missing"),
			&attempt,
			&evidence,
			None,
			None,
			started,
			false,
		));
	}

	let reader = tokio::select! {
		result = body_attempt.open() => result.map_err(|error| {
			record_failure(map_body_open_error(error), &attempt, &evidence, None, None, started, false)
		})?,
		() = poll_fn(|context| transport.cancel.poll_cancelled(context)) => {
			return Err(record_failure(cancelled(false), &attempt, &evidence, None, None, started, false));
		},
		() = sleep_until(deadline) => {
			transport.cancel.cancel();
			return Err(record_failure(deadline_exceeded(false), &attempt, &evidence, None, None, started, false));
		},
	};
	let request = if transport
		.credentials
		.as_ref()
		.is_some_and(|credentials| credentials.requires_buffered_body())
	{
		let bytes = collect_request_body(
			reader,
			transport.encoded.bounds.request_body,
			&transport.cancel,
			deadline,
		)
		.await
		.map_err(|error| record_failure(error, &attempt, &evidence, None, None, started, false))?;
		let mut request = build_request(&transport, bytes)
			.map_err(|error| record_failure(error, &attempt, &evidence, None, None, started, false))?;
		transport
			.credentials
			.as_ref()
			.expect("buffered credentials checked")
			.finalize_buffered(&mut request)
			.map_err(|_| {
				record_failure(
					authentication_error("credential-finalization"),
					&attempt,
					&evidence,
					None,
					None,
					started,
					false,
				)
			})?;
		let (parts, bytes) = request.into_parts();
		let body = Full::new(bytes)
			.map_err(|never: Infallible| -> Error { match never {} })
			.boxed_unsync();
		Request::from_parts(parts, body)
	} else {
		let body = StreamBody::new(LimitedBodyStream {
			reader,
			cancel: transport.cancel.clone(),
			seen: 0,
			limit: transport.encoded.bounds.request_body,
			done: false,
		})
		.boxed_unsync();
		let mut request = build_request(&transport, body)
			.map_err(|error| record_failure(error, &attempt, &evidence, None, None, started, false))?;
		if !sealed_bound
			&& let Some(credentials) = &transport.credentials
			&& credentials.finalize_streaming(&mut request).is_err()
		{
			drop(request);
			return Err(record_failure(
				authentication_error("credential-finalization"),
				&attempt,
				&evidence,
				None,
				None,
				started,
				false,
			));
		}
		request
	};
	let response = tokio::select! {
		result = client.oneshot(request) => result.map_err(|_| {
			record_failure(connectivity(ErrorPhase::Connecting, false, "http-dispatch"), &attempt, &evidence, None, None, started, false)
		})?,
		() = poll_fn(|context| transport.cancel.poll_cancelled(context)) => {
			return Err(record_failure(cancelled(false), &attempt, &evidence, None, None, started, false));
		},
		() = sleep_until(deadline) => {
			transport.cancel.cancel();
			return Err(record_failure(deadline_exceeded(false), &attempt, &evidence, None, None, started, false));
		},
	};
	let (parts, incoming) = response.into_parts();
	let status = parts.status.as_u16();
	let headers = sanitize_headers(&parts.headers);
	let provider_request_id = request_id(&parts.headers);
	let concurrency_admission = concurrency_admission_rejection(&parts.headers);
	let capture = Arc::new(Mutex::new(HttpCapture {
		attempt: transport.attempt.index,
		status,
		provider_request_id: provider_request_id.clone(),
		body: evidence.evidence(),
		frames: Vec::new(),
	}));
	captures
		.lock()
		.push(HttpCaptureRecord { snapshot: Arc::clone(&capture), evidence: evidence.clone() });
	let response_limit = transport.encoded.bounds.response;

	if !(200..300).contains(&status) {
		let body = collect_error_response_body(
			incoming,
			response_limit.min(MAX_PROVIDER_ERROR_BODY_BYTES as u64),
			&transport.cancel,
			deadline,
		)
		.await
		.map_err(|error| {
			record_failure(
				error,
				&attempt,
				&evidence,
				Some(status),
				provider_request_id.as_ref(),
				started,
				false,
			)
		})?;
		let mut capture_remaining = transport.attempt.capture_limit;
		capture_http_frame(&capture, 0, &Frame::Raw(body.clone()), &mut capture_remaining);
		let mut error = classify_http_error(status, &body);
		surface_concurrency_admission(&mut error, concurrency_admission);
		return Err(record_failure(
			error,
			&attempt,
			&evidence,
			Some(status),
			provider_request_id.as_ref(),
			started,
			false,
		));
	}

	let framing = transport.encoded.framing;
	let frame_limit = usize::try_from(transport.encoded.bounds.frame).unwrap_or(usize::MAX);
	let event_stream = decode_stream(
		incoming,
		framing,
		frame_limit,
		response_limit,
		transport.decoder.take().ok_or_else(|| {
			record_failure(
				protocol_error(ErrorPhase::Handshake, false, "ordinary-decoder-missing"),
				&attempt,
				&evidence,
				None,
				None,
				started,
				false,
			)
		})?,
		transport.cancel.clone(),
		transport.attempt.capture_limit,
		capture,
		attempt.clone(),
		evidence.clone(),
		status,
		provider_request_id.clone(),
		deadline,
		started,
		browser_retry,
	);
	let mut event_stream: RawEventStream = Box::pin(event_stream);
	let mut preamble = VecDeque::new();
	let first_visible = loop {
		match event_stream.next().await {
			Some(Ok(event)) if is_commit_candidate(&event) => break event,
			Some(Ok(event)) => preamble.push_back(event),
			Some(Err(error)) => {
				let mut error = error.status(Some(status)).committed(false);
				error.phase = ErrorPhase::Handshake;
				surface_concurrency_admission(&mut error, concurrency_admission);
				return Err(error);
			},
			None => {
				return Err(record_failure(
					protocol_error(ErrorPhase::Handshake, false, "response-ended-before-commit-event"),
					&attempt,
					&evidence,
					Some(status),
					provider_request_id.as_ref(),
					started,
					false,
				));
			},
		}
	};
	preamble.push_back(first_visible);
	let events: RawEventStream =
		Box::pin(stream::iter(preamble.into_iter().map(Ok)).chain(event_stream));
	Ok(HandshakenResponse {
		meta:     HandshakeMeta { status: Some(status), headers, provider_request_id },
		body:     evidence,
		events:   Some(events),
		control:  None,
		realtime: None,
	})
}

fn build_request<B>(transport: &TransportRequest, body: B) -> Result<Request<B>, Error> {
	let method = match transport.encoded.method {
		RequestMethod::Get => Method::GET,
		RequestMethod::Post => Method::POST,
		RequestMethod::Put => Method::PUT,
		RequestMethod::Patch => Method::PATCH,
		RequestMethod::Delete => Method::DELETE,
	};
	let uri = transport
		.encoded
		.uri
		.as_str()
		.parse::<Uri>()
		.map_err(|_| protocol_error(ErrorPhase::Encoding, false, "invalid-http-uri"))?;
	let mut request = Request::builder()
		.method(method)
		.uri(uri)
		.body(body)
		.map_err(|_| protocol_error(ErrorPhase::Encoding, false, "invalid-http-request"))?;
	for item in &transport.encoded.headers {
		insert_header(request.headers_mut(), item.name.as_str(), item.value.as_str())?;
	}
	request
		.headers_mut()
		.entry(header::USER_AGENT)
		.or_insert(HeaderValue::from_static(omp_core::USER_AGENT));
	Ok(request)
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), Error> {
	let name = HeaderName::from_bytes(name.as_bytes())
		.map_err(|_| protocol_error(ErrorPhase::Encoding, false, "invalid-header-name"))?;
	let value = HeaderValue::from_str(value)
		.map_err(|_| protocol_error(ErrorPhase::Encoding, false, "invalid-header-value"))?;
	headers.insert(name, value);
	Ok(())
}

fn map_body_open_error(error: BodyOpenError) -> Error {
	match error {
		BodyOpenError::Factory(error) => error,
		BodyOpenError::AttemptAlreadyOpened => {
			protocol_error(ErrorPhase::Connecting, false, "request-body-attempt-already-opened")
		},
		BodyOpenError::ConcurrentReader => {
			protocol_error(ErrorPhase::Connecting, false, "request-body-concurrent-reader")
		},
		BodyOpenError::Consumed => {
			protocol_error(ErrorPhase::Connecting, false, "request-body-consumed")
		},
		BodyOpenError::ReacquisitionUnavailable => {
			protocol_error(ErrorPhase::Connecting, false, "request-body-reacquisition-unavailable")
		},
	}
}

/// Extracts bounded provider-controlled correlation data from a closed header
/// allowlist. These values are never credential material or copied to outgoing
/// requests; the strict opaque-ID alphabet prevents header-shaped reflections.
pub(crate) fn request_id(headers: &HeaderMap) -> Option<Str> {
	let value = PUBLIC_REQUEST_ID_HEADERS
		.iter()
		.find_map(|name| headers.get(*name))?;
	let value = value.to_str().ok()?;
	(!value.is_empty()
		&& value.len() <= MAX_REQUEST_ID_BYTES
		&& value
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
	.then(|| Str::new(value))
}

pub(crate) fn sanitize_headers(headers: &HeaderMap) -> Box<[RequestHeader]> {
	headers
		.iter()
		.filter_map(|(name, value)| {
			if value.as_bytes().len() > MAX_CAPTURED_HEADER_BYTES
				|| !PUBLIC_NUMERIC_HEADERS.contains(&name.as_str())
			{
				return None;
			}
			let value = value.to_str().ok()?.trim().parse::<u64>().ok()?;
			Some(RequestHeader { name: Str::new(name.as_str()), value: Str::new(value.to_string()) })
		})
		.take(MAX_CAPTURED_HEADERS)
		.collect::<Vec<_>>()
		.into_boxed_slice()
}

/// `true` for a response carrying `LiteLLM`'s concurrency-admission marker
/// header. Only equality against the fixed limiter token is observed; the
/// provider-controlled value is never retained, preserving the numeric-only
/// header sanitization contract.
fn concurrency_admission_rejection(headers: &HeaderMap) -> bool {
	headers
		.get(CONCURRENCY_ADMISSION_HEADER)
		.is_some_and(|value| value.as_bytes().trim_ascii() == CONCURRENCY_ADMISSION_LIMITER)
}

/// Upgrades a pre-commit same-route rate-limit retry into immediate route
/// reselection when the response was a concurrency-admission shed, so the
/// preplanned fallback walk owns retry instead of a transport-level sleep.
fn surface_concurrency_admission(error: &mut Error, admission_marker: bool) {
	if admission_marker
		&& error.kind == ErrorKind::RateLimited
		&& matches!(error.action, RetryAction::SameRoute { .. })
	{
		error.action = RetryAction::ReselectRoute;
	}
}

struct LimitedBodyStream {
	reader: BodyReader,
	cancel: Cancellation,
	seen:   u64,
	limit:  u64,
	done:   bool,
}

impl Stream for LimitedBodyStream {
	type Item = Result<BodyFrame<Bytes>, Error>;

	fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		if self.done {
			return Poll::Ready(None);
		}
		if self.cancel.poll_cancelled(context).is_ready() {
			self.done = true;
			return Poll::Ready(Some(Err(cancelled(false))));
		}
		match Pin::new(&mut self.reader).poll_next(context) {
			Poll::Ready(Some(Ok(chunk))) => {
				self.seen = self.seen.saturating_add(chunk.len() as u64);
				if self.seen > self.limit {
					self.done = true;
					return Poll::Ready(Some(Err(protocol_error(
						ErrorPhase::Connecting,
						false,
						"request-body-limit",
					))));
				}
				Poll::Ready(Some(Ok(BodyFrame::data(chunk))))
			},
			Poll::Ready(Some(Err(error))) => {
				self.done = true;
				Poll::Ready(Some(Err(error)))
			},
			Poll::Ready(None) => {
				self.done = true;
				Poll::Ready(None)
			},
			Poll::Pending => Poll::Pending,
		}
	}
}

async fn collect_request_body(
	mut reader: BodyReader,
	limit: u64,
	cancel: &Cancellation,
	deadline: Instant,
) -> Result<Bytes, Error> {
	let capacity = usize::try_from(limit.min(64 * 1024)).unwrap_or(64 * 1024);
	let mut output = BytesMut::with_capacity(capacity);
	loop {
		let next = tokio::select! {
			next = reader.next() => next,
			() = poll_fn(|context| cancel.poll_cancelled(context)) => return Err(cancelled(false)),
			() = sleep_until(deadline) => {
				cancel.cancel();
				return Err(deadline_exceeded(false));
			},
		};
		match next {
			Some(Ok(chunk)) => {
				let observed = (output.len() as u64).saturating_add(chunk.len() as u64);
				if observed > limit {
					return Err(protocol_error(ErrorPhase::Connecting, false, "request-body-limit"));
				}
				output.extend_from_slice(&chunk);
			},
			Some(Err(error)) => return Err(error),
			None => return Ok(output.freeze()),
		}
	}
}

async fn collect_error_response_body(
	mut incoming: Incoming,
	limit: u64,
	cancel: &Cancellation,
	deadline: Instant,
) -> Result<Bytes, Error> {
	let limit = usize::try_from(limit).unwrap_or(usize::MAX);
	let mut output = BytesMut::with_capacity(limit.min(8 * 1024));
	while output.len() < limit {
		let next = tokio::select! {
			next = incoming.frame() => next,
			() = poll_fn(|context| cancel.poll_cancelled(context)) => return Err(cancelled(false)),
			() = sleep_until(deadline) => {
				cancel.cancel();
				return Err(deadline_exceeded(false));
			},
		};
		let Some(next) = next else {
			break;
		};
		let frame = next
			.map_err(|_| connectivity(ErrorPhase::Handshake, false, "http-error-response-body"))?;
		let Ok(chunk) = frame.into_data() else {
			continue;
		};
		let remaining = limit.saturating_sub(output.len());
		output.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
	}
	Ok(output.freeze())
}

fn classify_http_error(status: u16, body: &[u8]) -> Error {
	let (code, message) = provider_error_facts(body);
	let classified_rejection =
		classify_provider_rejection(Some(status), message.as_deref(), None, None);
	let transient_generation_fault = status == 400
		&& message
			.as_deref()
			.is_some_and(is_transient_generation_fault);
	let (kind, action) = if let Some(kind) = classified_rejection {
		(kind, RetryAction::Never)
	} else if transient_generation_fault {
		(ErrorKind::ResourceExhausted, RetryAction::SameRoute { after: time::Duration::ZERO })
	} else {
		match status {
			401 => (ErrorKind::Authentication, RetryAction::RefreshCredential),
			403 => (ErrorKind::Authorization, RetryAction::RotateAccount),
			408 => {
				(ErrorKind::DeadlineExceeded, RetryAction::SameRoute { after: time::Duration::ZERO })
			},
			429 => (ErrorKind::RateLimited, RetryAction::SameRoute {
				after: time::Duration::from_secs(30),
			}),
			400 | 404 | 405 | 422 => (ErrorKind::InvalidRequest, RetryAction::Never),
			402 => (ErrorKind::PaymentRequired, RetryAction::Never),
			409 => (ErrorKind::SessionConflict, RetryAction::Never),
			500..=599 => (ErrorKind::ResourceExhausted, RetryAction::SameRoute {
				after: time::Duration::from_millis(500),
			}),
			_ => (ErrorKind::ProviderContractMismatch, RetryAction::Never),
		}
	};
	Error::new(kind, ErrorPhase::Handshake, action, ExecutionReceipt::default())
		.status(Some(status))
		.optional_code(code)
		.detail(ErrorDetail::provider(
			message.unwrap_or_else(|| Str::new_static("Provider request failed")),
		))
}

fn provider_error_facts(body: &[u8]) -> (Option<Str>, Option<Str>) {
	let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
		return (
			None,
			str::from_utf8(body)
				.ok()
				.and_then(sanitize_provider_message),
		);
	};
	let facts = value.get("error").unwrap_or(&value);
	let code = ["code", "type", "status"]
		.into_iter()
		.find_map(|field| facts.get(field).and_then(provider_code));
	let message = ["message", "detail", "error"]
		.into_iter()
		.find_map(|field| facts.get(field).and_then(serde_json::Value::as_str))
		.and_then(sanitize_provider_message);
	(code, message)
}

fn provider_code(value: &serde_json::Value) -> Option<Str> {
	let text = match value {
		serde_json::Value::String(text) => text.clone(),
		serde_json::Value::Number(number) => number.to_string(),
		_ => return None,
	};
	(!text.is_empty()
		&& text.len() <= MAX_PROVIDER_CODE_BYTES
		&& text
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')))
	.then(|| Str::new(text))
}

fn sanitize_provider_message(message: &str) -> Option<Str> {
	let lowered = message.to_ascii_lowercase();
	if [
		"authorization:",
		"bearer ",
		"api_key",
		"api key",
		"access_token",
		"access token",
		"secret",
		"sk-",
	]
	.into_iter()
	.any(|marker| lowered.contains(marker))
	{
		return None;
	}
	let mut sanitized = String::with_capacity(message.len().min(MAX_PROVIDER_MESSAGE_BYTES));
	for word in message.split_whitespace() {
		let separator = usize::from(!sanitized.is_empty());
		if sanitized
			.len()
			.saturating_add(separator)
			.saturating_add(word.len())
			> MAX_PROVIDER_MESSAGE_BYTES
		{
			break;
		}
		if separator != 0 {
			sanitized.push(' ');
		}
		sanitized.push_str(word);
	}
	(!sanitized.is_empty()).then(|| Str::new(sanitized))
}

fn browser_retry_request(
	transport: &TransportRequest,
	browser: Option<Arc<dyn BrowserFetch>>,
) -> Option<(Arc<dyn BrowserFetch>, BrowserFetchRequest)> {
	let browser = browser?;
	if transport.credentials.is_some()
		|| transport.encoded.operation != OperationKind::Search
		|| transport.encoded.method != RequestMethod::Get
		|| transport.encoded.framing != FramingProtocol::Raw
	{
		return None;
	}
	let max_bytes = usize::try_from(transport.encoded.bounds.response)
		.ok()?
		.min(BROWSER_MAX_BROWSER_BODY_BYTES);
	let deadline = transport.attempt.timeout.min(MAX_BROWSER_DEADLINE);
	if max_bytes == 0 || deadline.is_zero() {
		return None;
	}
	let headers = transport
		.encoded
		.headers
		.iter()
		.map(|header| BrowserHeader { name: header.name.clone(), value: header.value.clone() })
		.collect::<Vec<_>>()
		.into_boxed_slice();
	Some((browser, BrowserFetchRequest {
		url: transport.encoded.uri.clone(),
		headers,
		max_bytes,
		deadline,
	}))
}

fn browser_fetch_error(error: BrowserFetchError) -> Error {
	match error {
		BrowserFetchError::Cancelled => cancelled(false),
		BrowserFetchError::TimedOut => deadline_exceeded(false),
		BrowserFetchError::Unavailable | BrowserFetchError::Navigation => {
			connectivity(ErrorPhase::Handshake, false, "browser-fetch-unavailable")
		},
		BrowserFetchError::InvalidUrl
		| BrowserFetchError::InvalidLimit
		| BrowserFetchError::InvalidDeadline
		| BrowserFetchError::ResponseTooLarge => {
			protocol_error(ErrorPhase::Handshake, false, "browser-fetch-contract")
		},
	}
}

fn decode_stream(
	mut incoming: Incoming,
	protocol: FramingProtocol,
	frame_limit: usize,
	response_limit: u64,
	mut decoder: DecoderState,
	cancel: Cancellation,
	capture_limit: u64,
	capture: Arc<Mutex<HttpCapture>>,
	attempt: TransportAttempt,
	evidence: AttemptEvidenceHandle,
	status: u16,
	provider_request_id: Option<Str>,
	deadline: Instant,
	started: Instant,
	browser_retry: Option<(Arc<dyn BrowserFetch>, BrowserFetchRequest)>,
) -> impl Stream<Item = Result<RawEvent, Error>> + Send + 'static {
	async_stream::stream! {
		let mut guard = CancelOnDrop::new(cancel.clone());
		let mut framer = ResponseFramer::new(protocol, frame_limit);
		let mut response_bytes = 0_u64;
		let mut capture_remaining = capture_limit;
		let mut ordinal = 0_u64;
		let mut emitted = false;
		'response: loop {
			let next = tokio::select! {
				next = incoming.frame() => next,
				() = poll_fn(|context| cancel.poll_cancelled(context)) => {
					yield Err(record_failure(cancelled(emitted), &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
					break;
				},
				() = sleep_until(deadline) => {
					cancel.cancel();
					yield Err(record_failure(deadline_exceeded(emitted), &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
					break;
				},
			};
			let Some(next) = next else {
				match framer.finish() {
					Ok(frames) => {
						for frame in frames {
							capture_http_frame(&capture, ordinal, &frame, &mut capture_remaining);
							ordinal += 1;
							let mut events = VecDeque::new();
							if let Err(error) = decoder.push(frame, &mut |event| events.push_back(event)) {
								yield Err(record_failure(error, &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
								return;
							}
							for event in events {
								match event {
									RawEvent::Failure(error) => {
										yield Err(record_failure(error, &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
										return;
									},
									event => {
										emitted |= is_commit_candidate(&event);
										yield Ok(event);
									},
								}
							}
						}
					},
					Err(error) => {
						yield Err(record_failure(framing_error(error, emitted), &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
						break;
					},
				}
				let mut events = VecDeque::new();
				match decoder.finish(&mut |event| events.push_back(event)) {
					Ok(()) => for event in events {
						match event {
							RawEvent::Failure(error) => {
								yield Err(record_failure(error, &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
								return;
							},
							event => {
								emitted |= is_commit_candidate(&event);
								yield Ok(event);
							},
						}
					},
					Err(error) => {
						let Some((browser, request)) =
							browser_retry.filter(|_| !emitted && decoder.prepare_browser_retry())
						else {
							yield Err(record_failure(error, &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
							guard.disarm();
							break;
						};
						let response = match browser.fetch(request, cancel.clone()).await {
							Ok(response) => response,
							Err(failure) => {
								yield Err(record_failure(browser_fetch_error(failure), &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
								guard.disarm();
								break;
							},
						};
						let frame = Frame::Raw(response.body);
						capture_http_frame(&capture, ordinal, &frame, &mut capture_remaining);
						let mut retried_events = VecDeque::new();
						if let Err(error) =
							decoder.push(frame, &mut |event| retried_events.push_back(event))
						{
							yield Err(record_failure(error, &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
							guard.disarm();
							break;
						}
						if let Err(error) =
							decoder.finish(&mut |event| retried_events.push_back(event))
						{
							yield Err(record_failure(error, &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
							guard.disarm();
							break;
						}
						for event in retried_events {
							match event {
								RawEvent::Failure(error) => {
									yield Err(record_failure(error, &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
									return;
								},
								event => {
									emitted |= is_commit_candidate(&event);
									yield Ok(event);
								},
							}
						}
					},
				}
				guard.disarm();
				break;
			};
			let Ok(body_frame) = next else {
				let error = connectivity(
					if emitted { ErrorPhase::Streaming } else { ErrorPhase::Handshake },
					emitted,
					"http-response-body",
				);
				yield Err(record_failure(
					error,
					&attempt,
					&evidence,
					Some(status),
					provider_request_id.as_ref(),
					started,
					emitted,
				));
				break;
			};
			let Ok(chunk) = body_frame.into_data() else { continue };
			response_bytes = response_bytes.saturating_add(chunk.len() as u64);
			if response_bytes > response_limit {
				let error = protocol_error(if emitted { ErrorPhase::Streaming } else { ErrorPhase::Handshake }, emitted, "response-body-limit");
				yield Err(record_failure(error, &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
				break;
			}
			let frames = match framer.push(chunk) {
				Ok(frames) => frames,
				Err(error) => {
					yield Err(record_failure(framing_error(error, emitted), &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
					break;
				},
			};
			for frame in frames {
				capture_http_frame(&capture, ordinal, &frame, &mut capture_remaining);
				ordinal += 1;
				let mut events = VecDeque::new();
				if let Err(error) = decoder.push(frame, &mut |event| events.push_back(event)) {
					yield Err(record_failure(error, &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
					return;
				}
				for event in events {
					match event {
						RawEvent::Failure(error) => {
							yield Err(record_failure(error, &attempt, &evidence, Some(status), provider_request_id.as_ref(), started, emitted));
							return;
						},
						event => {
							emitted |= is_commit_candidate(&event);
							yield Ok(event);
						},
					}
				}
				if decoder.is_complete() {
					// A provider terminal envelope owns response completion even
					// when a broken keep-alive leaves the HTTP body open.
					guard.disarm();
					break 'response;
				}
			}
		}
	}
}

fn capture_http_frame(
	capture: &Arc<Mutex<HttpCapture>>,
	ordinal: u64,
	frame: &Frame,
	remaining: &mut u64,
) {
	let mut capture = capture.lock();
	capture_frame(&mut capture.frames, ordinal, frame, remaining);
}

enum ResponseFramer {
	Raw { buffer: BytesMut, limit: usize },
	RawChunks(RawChunkFramer),
	Sse(SseDecoder),
	Ndjson(NdjsonDecoder),
	Connect(ConnectDecoder),
	EventStream(EventStreamDecoder),
}

impl ResponseFramer {
	fn new(protocol: FramingProtocol, limit: usize) -> Self {
		match protocol {
			FramingProtocol::Raw => Self::Raw { buffer: BytesMut::new(), limit },
			FramingProtocol::RawChunks => Self::RawChunks(RawChunkFramer::new(limit)),
			FramingProtocol::Sse => Self::Sse(SseDecoder::with_max_frame_bytes(limit)),
			FramingProtocol::Ndjson => Self::Ndjson(NdjsonDecoder::with_max_frame_bytes(limit)),
			FramingProtocol::Connect => Self::Connect(ConnectDecoder::with_max_payload_bytes(limit)),
			FramingProtocol::AwsEventStream => {
				Self::EventStream(EventStreamDecoder::with_limits(limit, limit.min(128 * 1024)))
			},
			FramingProtocol::WebSocket => unreachable!("rejected before response framing"),
		}
	}

	fn push(&mut self, chunk: Bytes) -> Result<SmallVec<Frame, 4>, FramingError> {
		match self {
			Self::Raw { buffer, limit } => {
				let observed = buffer.len().saturating_add(chunk.len());
				if observed > *limit {
					return Err(FramingError::LimitExceeded {
						protocol: FramingProtocol::Raw,
						limit: *limit,
						observed,
					});
				}
				buffer.extend_from_slice(&chunk);
				Ok(SmallVec::new())
			},
			Self::RawChunks(framer) => framer
				.push(chunk)
				.map(|frames| frames.into_iter().map(Frame::Raw).collect()),
			Self::Sse(framer) => {
				let was_done = framer.is_done();
				let mut frames: SmallVec<Frame, 4> =
					framer.push(chunk)?.into_iter().map(Frame::Sse).collect();
				if !was_done && framer.is_done() {
					frames.push(Frame::Sse(crate::transport::SseEvent {
						name: None,
						data: Bytes::from_static(b"[DONE]"),
					}));
				}
				Ok(frames)
			},
			Self::Ndjson(framer) => framer
				.push(chunk)
				.map(|frames| frames.into_iter().map(Frame::Ndjson).collect()),
			Self::Connect(framer) => framer
				.push(chunk)
				.map(|frames| frames.into_iter().map(Frame::Connect).collect()),
			Self::EventStream(framer) => framer.push(chunk).map(|frames| {
				frames
					.into_iter()
					.map(|frame| Frame::EventStream(Box::new(frame)))
					.collect()
			}),
		}
	}

	fn finish(&mut self) -> Result<SmallVec<Frame, 4>, FramingError> {
		match self {
			Self::Raw { buffer, .. } => {
				let mut frames = SmallVec::new();
				frames.push(Frame::Raw(buffer.split().freeze()));
				Ok(frames)
			},
			Self::RawChunks(framer) => framer
				.finish()
				.map(|frames| frames.into_iter().map(Frame::Raw).collect()),
			Self::Sse(framer) => framer
				.finish()
				.map(|frames| frames.into_iter().map(Frame::Sse).collect()),
			Self::Ndjson(framer) => framer
				.finish()
				.map(|frames| frames.into_iter().map(Frame::Ndjson).collect()),
			Self::Connect(framer) => framer
				.finish()
				.map(|frames| frames.into_iter().map(Frame::Connect).collect()),
			Self::EventStream(framer) => framer.finish().map(|frames| {
				frames
					.into_iter()
					.map(|frame| Frame::EventStream(Box::new(frame)))
					.collect()
			}),
		}
	}
}

struct CancelOnDrop {
	cancel: Cancellation,
	armed:  bool,
}

impl CancelOnDrop {
	const fn new(cancel: Cancellation) -> Self {
		Self { cancel, armed: true }
	}

	const fn disarm(&mut self) {
		self.armed = false;
	}
}

impl Drop for CancelOnDrop {
	fn drop(&mut self) {
		if self.armed {
			self.cancel.cancel();
		}
	}
}

fn framing_error(error: FramingError, committed: bool) -> Error {
	let (kind, reason) = match error {
		FramingError::AfterEnd { .. } => (ErrorKind::Protocol, "framing-after-end"),
		FramingError::Cancelled { .. } => (ErrorKind::Cancelled, "framing-cancelled"),
		FramingError::LimitExceeded { .. } => (ErrorKind::StreamCorruption, "framing-limit"),
		FramingError::UnexpectedEof { .. } => (ErrorKind::StreamCorruption, "framing-truncated"),
		FramingError::InvalidFlags { .. } => (ErrorKind::StreamCorruption, "framing-invalid-flags"),
		FramingError::InvalidWebSocketOpcode { .. } => {
			(ErrorKind::StreamCorruption, "websocket-opcode")
		},
		FramingError::NonCanonicalWebSocketLength { .. } => {
			(ErrorKind::StreamCorruption, "websocket-noncanonical-length")
		},
		FramingError::InvalidWebSocketControl => (ErrorKind::StreamCorruption, "websocket-control"),
		FramingError::InvalidWebSocketClose => (ErrorKind::StreamCorruption, "websocket-close"),
		FramingError::InvalidUtf8 { .. } => (ErrorKind::StreamCorruption, "framing-invalid-utf8"),
		FramingError::CrcMismatch { scope: CrcScope::Prelude, .. } => {
			(ErrorKind::StreamCorruption, "eventstream-prelude-crc")
		},
		FramingError::CrcMismatch { scope: CrcScope::Message, .. } => {
			(ErrorKind::StreamCorruption, "eventstream-message-crc")
		},
		FramingError::InvalidEventStreamLengths { .. } => {
			(ErrorKind::StreamCorruption, "eventstream-lengths")
		},
		FramingError::InvalidEventStreamHeader { .. } => {
			(ErrorKind::StreamCorruption, "eventstream-header")
		},
		FramingError::UnknownEventStreamHeaderType { .. } => {
			(ErrorKind::StreamCorruption, "eventstream-header-type")
		},
	};
	let phase = if committed {
		ErrorPhase::Streaming
	} else {
		ErrorPhase::Handshake
	};
	let mut error = structured_error(kind, phase, committed, reason).code(Str::new(reason));
	if !committed && error.kind != ErrorKind::Cancelled {
		error.action = RetryAction::SameRoute { after: time::Duration::ZERO };
	}
	error
}

pub(crate) fn record_failure(
	error: Error,
	attempt: &TransportAttempt,
	evidence: &AttemptEvidenceHandle,
	status: Option<u16>,
	provider_request_id: Option<&Str>,
	started: Instant,
	committed: bool,
) -> Error {
	let status = error.status.or(status);
	let mut error = error
		.provider(attempt.provider.clone())
		.route(attempt.route.clone())
		.request_id(attempt.request_id.clone())
		.status(status)
		.committed(committed);
	if committed {
		error.phase = ErrorPhase::Streaming;
		error.action = RetryAction::Never;
	}
	let outcome = if error.kind == ErrorKind::Cancelled {
		AttemptOutcome::Cancelled
	} else if committed {
		AttemptOutcome::FailedCommitted
	} else {
		AttemptOutcome::FailedPreCommit
	};
	let provider_evidence = ProviderEvidence {
		request_id: provider_request_id.cloned(),
		status:     error.status,
		code:       error.code.clone(),
		summary:    None,
	};
	error.receipt_mut().record_attempt(AttemptReceipt {
		index: attempt.index,
		hidden: attempt.provisional,
		provider: Some(attempt.provider.clone()),
		route: Some(attempt.route.clone()),
		account: attempt.account.clone(),
		principal: attempt.principal.clone(),
		body: evidence.evidence(),
		outcome,
		usage: Usage::default(),
		cost: Cost::default(),
		provider_evidence,
		elapsed: started.elapsed(),
	});
	error
}

fn deadline_exceeded(committed: bool) -> Error {
	Error::new(
		ErrorKind::DeadlineExceeded,
		if committed {
			ErrorPhase::Streaming
		} else {
			ErrorPhase::Handshake
		},
		RetryAction::Never,
		ExecutionReceipt::default(),
	)
	.committed(committed)
}

fn cancelled(committed: bool) -> Error {
	Error::new(
		ErrorKind::Cancelled,
		if committed {
			ErrorPhase::Streaming
		} else {
			ErrorPhase::Handshake
		},
		RetryAction::Never,
		ExecutionReceipt::default(),
	)
	.committed(committed)
}

fn authentication_error(reason: &'static str) -> Error {
	structured_error(ErrorKind::Authentication, ErrorPhase::Authentication, false, reason)
}

fn connectivity(phase: ErrorPhase, committed: bool, reason: &'static str) -> Error {
	let mut error = structured_error(ErrorKind::Connectivity, phase, committed, reason);
	if !committed {
		error.action = RetryAction::SameRoute { after: time::Duration::ZERO };
	}
	error
}

fn protocol_error(phase: ErrorPhase, committed: bool, reason: &'static str) -> Error {
	structured_error(ErrorKind::Protocol, phase, committed, reason)
}

fn structured_error(
	kind: ErrorKind,
	phase: ErrorPhase,
	committed: bool,
	reason: &'static str,
) -> Error {
	Error::new(kind, phase, RetryAction::Never, ExecutionReceipt::default())
		.committed(committed)
		.detail(ErrorDetail::protocol(ReasonId(Str::new(reason))))
}

#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;

	#[test]
	fn reflected_secret_is_rejected_for_every_public_header_surface() {
		const SECRET: &str = "Bearer reflected-super-secret";
		let mut headers = HeaderMap::new();
		for name in PUBLIC_NUMERIC_HEADERS {
			headers.insert(HeaderName::from_static(name), HeaderValue::from_static(SECRET));
		}
		for name in [
			"content-type",
			"date",
			"traceparent",
			"x-request-id",
			"x-amzn-requestid",
			"x-ratelimit-reflection",
		] {
			headers.insert(HeaderName::from_static(name), HeaderValue::from_static(SECRET));
		}
		let sanitized = sanitize_headers(&headers);
		assert!(sanitized.is_empty());
		assert!(!format!("{sanitized:?}").contains(SECRET));
	}

	#[test]
	fn request_id_uses_closed_names_and_bounded_opaque_values() {
		let mut headers = HeaderMap::new();
		headers.insert("x-request-id", HeaderValue::from_static("req_01HZX-abc.7"));
		assert_eq!(request_id(&headers).as_deref(), Some("req_01HZX-abc.7"));
		headers.insert("x-request-id", HeaderValue::from_static("Bearer reflected-super-secret"));
		assert!(request_id(&headers).is_none());
		headers.clear();
		headers.insert("x-provider-request-id", HeaderValue::from_static("looks-safe"));
		assert!(request_id(&headers).is_none());
	}

	#[test]
	fn only_closed_numeric_fields_survive_in_canonical_form() {
		let mut headers = HeaderMap::new();
		for name in PUBLIC_NUMERIC_HEADERS {
			headers.insert(HeaderName::from_static(name), HeaderValue::from_static("0007"));
		}
		let sanitized = sanitize_headers(&headers);
		assert_eq!(sanitized.len(), PUBLIC_NUMERIC_HEADERS.len());
		assert!(sanitized.iter().all(|header| header.value.as_str() == "7"));
	}

	#[test]
	fn request_id_uses_first_present_allowlisted_header_only() {
		let mut headers = HeaderMap::new();
		headers.insert("x-request-id", HeaderValue::from_static("invalid value"));
		headers.insert("request-id", HeaderValue::from_static("would-be-valid"));
		assert!(request_id(&headers).is_none());

		headers.clear();
		for name in PUBLIC_REQUEST_ID_HEADERS {
			headers
				.insert(HeaderName::from_static(name), HeaderValue::from_static("provider_01.test-id"));
			assert_eq!(request_id(&headers).as_deref(), Some("provider_01.test-id"));
			headers.clear();
		}

		let oversized = "a".repeat(MAX_REQUEST_ID_BYTES + 1);
		headers.insert("x-request-id", HeaderValue::from_str(&oversized).expect("valid header"));
		assert!(request_id(&headers).is_none());
	}

	#[test]
	fn non_success_statuses_are_typed_before_codec_decoding() {
		for (status, kind, retryable) in [
			(400, ErrorKind::InvalidRequest, false),
			(401, ErrorKind::Authentication, false),
			(403, ErrorKind::Authorization, false),
			(404, ErrorKind::InvalidRequest, false),
			(408, ErrorKind::DeadlineExceeded, true),
			(413, ErrorKind::PayloadRejected, false),
			(422, ErrorKind::InvalidRequest, false),
			(429, ErrorKind::RateLimited, true),
			(500, ErrorKind::ResourceExhausted, true),
			(503, ErrorKind::ResourceExhausted, true),
		] {
			let error = classify_http_error(status, br#"{"error":{"code":"upstream_code"}}"#);
			assert_eq!(error.kind, kind, "status {status}");
			assert_eq!(error.phase, ErrorPhase::Handshake);
			assert_eq!(error.status, Some(status));
			assert_eq!(error.code.as_deref(), Some("upstream_code"));
			assert_eq!(
				matches!(error.action, RetryAction::SameRoute { .. }),
				retryable,
				"status {status}",
			);
		}
		assert_eq!(classify_http_error(401, b"{}").action, RetryAction::RefreshCredential,);
		assert_eq!(classify_http_error(403, b"{}").action, RetryAction::RotateAccount,);
	}

	#[test]
	fn provider_error_facts_are_bounded_and_secret_free() {
		let token_overflow = classify_http_error(
			413,
			br#"{"error":{"message":"maximum context length is 128000 tokens"}}"#,
		);
		assert_eq!(token_overflow.kind, ErrorKind::ContextOverflow);
		assert_eq!(token_overflow.action, RetryAction::Never);

		let media_limit = classify_http_error(
			413,
			br#"{"error":{"message":"image count exceeds the limit of 20"}}"#,
		);
		assert_eq!(media_limit.kind, ErrorKind::PayloadRejected);
		assert_eq!(media_limit.action, RetryAction::Never);

		let wrapped = classify_http_error(
			413,
			br#"{"error":{"message":"Provider returned error: 413 Payload Too Large"}}"#,
		);
		assert_eq!(wrapped.kind, ErrorKind::PayloadRejected);
		assert_eq!(wrapped.action, RetryAction::Never);

		let generation_nan = classify_http_error(
			400,
			br#"{"error":{"type":"invalid_request_error","message":"Floating point NaN (not-a-number) is detected in generation"}}"#,
		);
		assert_eq!(generation_nan.kind, ErrorKind::ResourceExhausted);
		assert!(matches!(generation_nan.action, RetryAction::SameRoute { .. }));

		let error = classify_http_error(
			404,
			br#"{"error":{"code":"model_not_found","message":"Model zai-glm-4.7 was retired"}}"#,
		);
		assert_eq!(error.kind, ErrorKind::InvalidRequest);
		assert_eq!(error.status, Some(404));
		assert_eq!(error.code.as_deref(), Some("model_not_found"));
		assert!(matches!(
			error.detail_ref(),
			Some(ErrorDetail::Provider { sanitized_message })
				if sanitized_message.as_str() == "Model zai-glm-4.7 was retired"
		));

		let reflected = classify_http_error(
			401,
			br#"{"error":{"message":"Authorization: Bearer reflected-super-secret"}}"#,
		);
		assert!(matches!(
			reflected.detail_ref(),
			Some(ErrorDetail::Provider { sanitized_message })
				if sanitized_message.as_str() == "Provider request failed"
		));
		assert!(!format!("{reflected:?}").contains("reflected-super-secret"));

		let oversized = "x".repeat(MAX_PROVIDER_MESSAGE_BYTES + 20);
		let body = serde_json::json!({"error": {"message": oversized}});
		let encoded = serde_json::to_vec(&body).expect("provider error fixture");
		let bounded = classify_http_error(500, &encoded);
		assert!(matches!(
			bounded.detail_ref(),
			Some(ErrorDetail::Provider { sanitized_message })
				if sanitized_message.len() <= MAX_PROVIDER_MESSAGE_BYTES
		));
	}

	#[test]
	fn factory_open_error_preserves_typed_failure() {
		let inner = Error::new(
			ErrorKind::RateLimited,
			ErrorPhase::Admission,
			RetryAction::SameRoute { after: time::Duration::from_secs(3) },
			ExecutionReceipt::default(),
		)
		.detail(ErrorDetail::protocol(ReasonId(sf!("factory-rate-window"))));
		let mapped = map_body_open_error(BodyOpenError::Factory(inner.clone()));
		assert_eq!(mapped.kind, inner.kind);
		assert_eq!(mapped.phase, inner.phase);
		assert_eq!(mapped.action, inner.action);
		assert_eq!(mapped.detail_ref(), inner.detail_ref());
		assert_eq!(mapped.receipt(), inner.receipt());
	}

	/// Mid-stream transport interruptions (`HTTP/2` `RST_STREAM`, connection
	/// resets) are classified as transient connectivity: uncommitted attempts
	/// retry the same route immediately, while committed attempts keep the
	/// stable `Connectivity`/`Streaming`/`committed` signature session-level
	/// turn recovery keys on instead of being misfiled as corruption.
	#[test]
	fn stream_interruptions_classify_as_transient_connectivity() {
		let uncommitted = connectivity(ErrorPhase::Handshake, false, "http-response-body");
		assert_eq!(uncommitted.kind, ErrorKind::Connectivity);
		assert!(!uncommitted.committed);
		assert_eq!(uncommitted.action, RetryAction::SameRoute { after: std::time::Duration::ZERO });

		let committed = connectivity(ErrorPhase::Streaming, true, "http-response-body");
		assert_eq!(committed.kind, ErrorKind::Connectivity);
		assert_eq!(committed.phase, ErrorPhase::Streaming);
		assert!(committed.committed);
		assert_eq!(committed.action, RetryAction::Never, "committed output is never blind-retried");
		assert_eq!(
			committed.detail_ref(),
			Some(&ErrorDetail::protocol(ReasonId(sf!("http-response-body")))),
			"the stable reason survives for resume classification"
		);
	}

	#[test]
	fn concurrency_admission_marker_is_detected_only_for_the_exact_limiter_token() {
		let mut headers = HeaderMap::new();
		assert!(!concurrency_admission_rejection(&headers), "absent header is no marker");
		headers.insert(
			HeaderName::from_static(CONCURRENCY_ADMISSION_HEADER),
			HeaderValue::from_static("max_parallel_requests"),
		);
		assert!(concurrency_admission_rejection(&headers));
		headers.insert(
			HeaderName::from_static(CONCURRENCY_ADMISSION_HEADER),
			HeaderValue::from_static(" max_parallel_requests "),
		);
		assert!(concurrency_admission_rejection(&headers), "padded token still matches");
		headers.insert(
			HeaderName::from_static(CONCURRENCY_ADMISSION_HEADER),
			HeaderValue::from_static("tokens_per_minute"),
		);
		assert!(!concurrency_admission_rejection(&headers), "other limiter types are not sheds");
		// The marker never enters the sanitized public header surface: only
		// the boolean comparison against the fixed token is observed.
		assert!(sanitize_headers(&headers).is_empty());
	}

	/// A header-marked `LiteLLM` concurrency-admission 429 (`pi` #8854) must
	/// surface for immediate route reselection instead of sleeping through the
	/// transport's same-route retry lane; unmarked rate limits and every other
	/// classification keep their action.
	#[test]
	fn header_marked_admission_429_upgrades_same_route_retry_to_reselection() {
		let rate_limited = || {
			Error::new(
				ErrorKind::RateLimited,
				ErrorPhase::Handshake,
				RetryAction::SameRoute { after: time::Duration::from_secs(30) },
				ExecutionReceipt::default(),
			)
		};
		let mut marked = rate_limited();
		surface_concurrency_admission(&mut marked, true);
		assert_eq!(marked.action, RetryAction::ReselectRoute);

		let mut unmarked = rate_limited();
		surface_concurrency_admission(&mut unmarked, false);
		assert!(
			matches!(unmarked.action, RetryAction::SameRoute { .. }),
			"a plain 429 keeps honoring the same-route backoff"
		);

		let mut terminal = rate_limited();
		terminal.action = RetryAction::Never;
		surface_concurrency_admission(&mut terminal, true);
		assert_eq!(terminal.action, RetryAction::Never, "non-retryable classifications never widen");

		let mut transient = rate_limited();
		transient.kind = ErrorKind::ResourceExhausted;
		surface_concurrency_admission(&mut transient, true);
		assert!(
			matches!(transient.action, RetryAction::SameRoute { .. }),
			"only rate-limit classifications are admission sheds"
		);
	}
}
