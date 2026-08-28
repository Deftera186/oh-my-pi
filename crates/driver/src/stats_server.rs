//! App-owned Hyper lifecycle and security boundary for local statistics.

use std::{
	convert::Infallible,
	env, io,
	net::{IpAddr, Ipv4Addr, SocketAddr},
	path::PathBuf,
	sync::Arc,
	time::Duration,
};

use bytes::Bytes;
use http::{
	Request, Response, StatusCode,
	header::{self, HeaderName, HeaderValue},
};
use http_body_util::Full;
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use omp_storage::index::SessionIndex;
use thiserror::Error;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	sync::{watch, watch::Receiver},
	task::{JoinHandle, JoinSet},
	time,
};

use crate::{
	stats_api::{Body, StatsApi},
	stats_dashboard,
};

/// Default loopback port used by `omp stats serve`.
pub const DEFAULT_PORT: u16 = 3847;

/// Stats service bind and access policy.
#[derive(Clone, Debug)]
pub struct Config {
	/// Socket address to bind.
	pub address:    SocketAddr,
	/// Bearer token required for every request, if configured.
	pub auth_token: Option<String>,
	/// Directory containing the cross-process synchronization lock.
	pub state_dir:  PathBuf,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			address:    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT),
			auth_token: None,
			state_dir:  PathBuf::new(),
		}
	}
}

/// Failure to securely start the statistics service.
#[derive(Debug, Error)]
pub enum Error {
	/// A public bind was requested without authentication.
	#[error("non-loopback stats binds require --auth-token")]
	NonLoopbackRequiresAuth,
	/// The requested address is already an OMP stats service.
	#[error("OMP statistics is already serving on http://{address}")]
	AlreadyRunning {
		/// Existing endpoint.
		address: SocketAddr,
	},
	/// The address is occupied by an unrelated process.
	#[error(
		"cannot bind statistics server to {address}; the address is occupied by another process and \
		 OMP will not terminate it"
	)]
	AddressOccupied {
		/// Occupied endpoint.
		address: SocketAddr,
	},
	/// Listener creation failed for another reason.
	#[error("cannot bind statistics server to {address}")]
	Bind {
		/// Requested endpoint.
		address: SocketAddr,
		/// Operating-system socket failure.
		#[source]
		source:  io::Error,
	},
}

/// Running service handle with explicit graceful shutdown.
pub struct RunningServer {
	address:  SocketAddr,
	shutdown: watch::Sender<bool>,
	task:     JoinHandle<()>,
}

impl RunningServer {
	/// Bound address, including the selected ephemeral port.
	pub const fn address(&self) -> SocketAddr {
		self.address
	}

	/// Stops accepting connections and gracefully drains active HTTP/1 requests.
	pub async fn shutdown(self) {
		let _ = self.shutdown.send(true);
		let _ = self.task.await;
	}

	/// Waits until the server task exits.
	pub async fn wait(self) {
		let _ = self.task.await;
	}
}

/// Starts the secure Hyper service and returns after the listener is bound.
pub async fn start(config: Config, index: Arc<SessionIndex>) -> Result<RunningServer, Error> {
	if !config.address.ip().is_loopback() && config.auth_token.is_none() {
		return Err(Error::NonLoopbackRequiresAuth);
	}
	let listener = match TcpListener::bind(config.address).await {
		Ok(listener) => listener,
		Err(source) if source.kind() == io::ErrorKind::AddrInUse => {
			return Err(if is_omp_server(config.address).await {
				Error::AlreadyRunning { address: config.address }
			} else {
				Error::AddressOccupied { address: config.address }
			});
		},
		Err(source) => return Err(Error::Bind { address: config.address, source }),
	};
	let address = listener
		.local_addr()
		.map_err(|source| Error::Bind { address: config.address, source })?;
	let api = Arc::new(StatsApi::new(index, config.state_dir.join("stats-sync.lock")));
	let security = Arc::new(Security::new(address, config.auth_token));
	let (shutdown, shutdown_rx) = watch::channel(false);
	let task = tokio::spawn(serve(listener, api, security, shutdown_rx));
	Ok(RunningServer { address, shutdown, task })
}

async fn serve(
	listener: TcpListener,
	api: Arc<StatsApi>,
	security: Arc<Security>,
	mut shutdown: Receiver<bool>,
) {
	let mut connections = JoinSet::new();
	loop {
		tokio::select! {
			biased;
			changed = shutdown.changed() => {
				if changed.is_err() || *shutdown.borrow() { break; }
			},
			accepted = listener.accept() => {
				let Ok((stream, _peer)) = accepted else { break; };
				let api = Arc::clone(&api);
				let security = Arc::clone(&security);
				let mut connection_shutdown = shutdown.clone();
				connections.spawn(async move {
					let service = service_fn(move |request| {
						let response = route(request, &api, &security);
						async move { Ok::<_, Infallible>(response) }
					});
					let connection = hyper::server::conn::http1::Builder::new()
						.serve_connection(TokioIo::new(stream), service);
					tokio::pin!(connection);
					tokio::select! {
						_ = &mut connection => {},
						_ = connection_shutdown.changed() => {
							connection.as_mut().graceful_shutdown();
							let _ = connection.await;
						},
					}
				});
			},
		}
	}
	while connections.join_next().await.is_some() {}
}

fn route(request: Request<Incoming>, api: &StatsApi, security: &Security) -> Response<Body> {
	if let Err(status) = security.authorize(&request, request.uri().path().starts_with("/api/")) {
		return secured(
			text_response(
				status,
				if status == StatusCode::UNAUTHORIZED {
					"authentication required"
				} else {
					"request origin refused"
				},
			),
			security,
		);
	}
	let response = if request.uri().path().starts_with("/api/") {
		api.handle(&request)
	} else if let Some(asset) = stats_dashboard::asset(request.uri().path()) {
		Response::builder()
			.status(StatusCode::OK)
			.header(header::CONTENT_TYPE, asset.content_type)
			.header(header::ETAG, asset.etag)
			.header(header::CACHE_CONTROL, "no-cache")
			.body(Full::new(Bytes::from_static(asset.bytes)))
			.unwrap_or_else(|_| {
				text_response(StatusCode::INTERNAL_SERVER_ERROR, "response construction failed")
			})
	} else {
		text_response(StatusCode::NOT_FOUND, "not found")
	};
	secured(response, security)
}

struct Security {
	origin:     String,
	host:       String,
	address:    SocketAddr,
	auth_token: Option<String>,
	hostname:   String,
}
impl Security {
	fn new(address: SocketAddr, auth_token: Option<String>) -> Self {
		let host = address.to_string();
		let hostname = env::var("HOSTNAME")
			.or_else(|_| env::var("COMPUTERNAME"))
			.unwrap_or_else(|_| "local".to_owned());
		Self {
			origin: format!("http://{host}"),
			host,
			address,
			auth_token,
			hostname: omp_observability::redact::redact_sensitive_credentials(&hostname),
		}
	}

	fn authorize(&self, request: &Request<Incoming>, protected: bool) -> Result<(), StatusCode> {
		let host = request
			.headers()
			.get(header::HOST)
			.and_then(|value| value.to_str().ok())
			.unwrap_or_default();
		let host_allowed = if self.address.ip().is_loopback() {
			host == self.host
				|| host == "localhost"
				|| host
					.strip_prefix("localhost:")
					.and_then(|port| port.parse::<u16>().ok())
					== Some(self.address.port())
		} else {
			host
				.parse::<SocketAddr>()
				.is_ok_and(|address| address.port() == self.address.port())
				|| host
					.strip_prefix("localhost:")
					.and_then(|port| port.parse::<u16>().ok())
					== Some(self.address.port())
		};
		if !host_allowed {
			return Err(StatusCode::FORBIDDEN);
		}
		if let Some(origin) = request
			.headers()
			.get(header::ORIGIN)
			.and_then(|value| value.to_str().ok())
		{
			if origin != self.origin && origin.strip_prefix("http://") != Some(host) {
				return Err(StatusCode::FORBIDDEN);
			}
		}
		if let Some(expected) = self.auth_token.as_deref() {
			if !protected {
				return Ok(());
			}
			let supplied = request
				.headers()
				.get(header::AUTHORIZATION)
				.and_then(|value| value.to_str().ok())
				.and_then(|value| value.strip_prefix("Bearer "))
				.unwrap_or_default();
			if !omp_core::ct_eq(supplied.as_bytes(), expected.as_bytes()) {
				return Err(StatusCode::UNAUTHORIZED);
			}
		}
		Ok(())
	}
}

fn secured(mut response: Response<Body>, security: &Security) -> Response<Body> {
	let headers = response.headers_mut();
	headers.insert("x-omp-stats-dashboard", HeaderValue::from_static("1"));
	if let Ok(value) = HeaderValue::from_str(&security.hostname) {
		headers.insert("x-omp-stats-hostname", value);
	}
	headers.insert(
		HeaderName::from_static("x-content-type-options"),
		HeaderValue::from_static("nosniff"),
	);
	headers
		.insert(HeaderName::from_static("referrer-policy"), HeaderValue::from_static("no-referrer"));
	headers.insert(
		HeaderName::from_static("content-security-policy"),
		HeaderValue::from_static(
			"default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' \
			 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'",
		),
	);
	headers.insert(
		HeaderName::from_static("permissions-policy"),
		HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
	);
	response
}
fn text_response(status: StatusCode, text: &'static str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
		.body(Full::new(Bytes::from_static(text.as_bytes())))
		.unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}
async fn is_omp_server(address: SocketAddr) -> bool {
	let probe = async {
		let Ok(mut stream) = TcpStream::connect(address).await else {
			return false;
		};
		let request =
			format!("GET /api/version HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
		if stream.write_all(request.as_bytes()).await.is_err() {
			return false;
		}
		let mut bytes = vec![0; 4096];
		let Ok(read) = stream.read(&mut bytes).await else {
			return false;
		};
		let response = String::from_utf8_lossy(&bytes[..read]);
		response.contains("x-omp-stats-dashboard: 1") && response.contains("omp.stats.v1")
	};
	time::timeout(Duration::from_millis(300), probe)
		.await
		.unwrap_or(false)
}
