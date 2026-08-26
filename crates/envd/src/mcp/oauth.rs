//! Combined MCP OAuth discovery, authorization, and refresh coordination.

use std::{
	fmt,
	future::Future,
	pin::Pin,
	sync::{Arc, atomic},
	time::{SystemTime, UNIX_EPOCH},
};

use http::HeaderMap;
use omp_core::{ExposeSecret as _, SecretString, Str};
use omp_inference::id::PrincipalId;
use omp_oauth::{
	AuthChallenge, AuthorizationRequest, CallbackBindError, CallbackError, ClientConfiguration,
	ClientRegistrationError, CompleteAuthorizationError, LoopbackCallback, MetadataError,
	OAuthHttpClient, SystemEntropy, TokenError, TokenGrant, TokenRequest, begin_authorization,
	complete_authorization, discover_authorization_server_metadata,
	discover_protected_resource_metadata, generate_pkce, refresh_token, resolve_client,
	validate_redirect_pair,
};
use parking_lot::RwLock;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;
use zeroize::Zeroizing;

use super::{
	auth_authority::{
		AuthAffinity, CombinedAuthAuthority, McpOAuthStoreError, StoredMcpOAuthCredential,
	},
	config::{McpServerConfig, OauthConfig},
	http::RefreshableHeaders,
};

/// Live Streamable-HTTP header adapter backed by one encrypted renewable
/// credential record.
pub struct AuthorityHeaders {
	flow:               Arc<McpOAuth>,
	state:              Mutex<OAuthCredentialState>,
	headers:            RwLock<HeaderMap>,
	reauthorize_needed: atomic::AtomicBool,
}

impl AuthorityHeaders {
	/// Acquires the current sealed generation and materializes only a sensitive
	/// Authorization header for the HTTP transport.
	pub async fn new(
		flow: Arc<McpOAuth>,
		state: OAuthCredentialState,
	) -> Result<Arc<Self>, OAuthFlowError> {
		let headers = bearer_headers(&state.access_token)?;
		Ok(Arc::new(Self {
			flow,
			state: Mutex::new(state),
			headers: RwLock::new(headers),
			reauthorize_needed: atomic::AtomicBool::new(false),
		}))
	}
}

impl RefreshableHeaders for AuthorityHeaders {
	fn current(&self) -> HeaderMap {
		self.headers.read().clone()
	}

	fn refresh(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
		Box::pin(async move {
			let mut state = self.state.lock().await;
			if let Err(error) = self.flow.refresh(&mut state).await {
				if error.class() == OAuthFailureClass::Definitive
					|| matches!(error, OAuthFlowError::NotRefreshable)
				{
					let _ = self.flow.authority.delete_mcp(&state.affinity);
					state.refresh_token.take();
					self
						.reauthorize_needed
						.store(true, atomic::Ordering::Release);
				}
				return false;
			}
			let Ok(headers) = bearer_headers(&state.access_token) else {
				return false;
			};
			*self.headers.write() = headers;
			self
				.reauthorize_needed
				.store(false, atomic::Ordering::Release);
			true
		})
	}

	fn should_reauthorize(&self) -> bool {
		self.reauthorize_needed.load(atomic::Ordering::Acquire)
	}
}

fn bearer_headers(token: &SecretString) -> Result<HeaderMap, OAuthFlowError> {
	let mut material =
		Zeroizing::new(String::with_capacity("Bearer ".len() + token.expose_secret().len()));
	material.push_str("Bearer ");
	material.push_str(token.expose_secret());
	let mut value =
		http::HeaderValue::from_str(&material).map_err(|_| OAuthFlowError::InvalidBearerToken)?;
	value.set_sensitive(true);
	let mut headers = HeaderMap::new();
	headers.insert(http::header::AUTHORIZATION, value);
	Ok(headers)
}

/// Cold browser-opening boundary owned by the application shell.
pub trait BrowserLauncher: Send + Sync {
	/// Opens one validated authorization URL.
	fn open<'a>(
		&'a self,
		url: &'a str,
	) -> Pin<Box<dyn Future<Output = Result<(), BrowserError>> + Send + 'a>>;
}

/// Secret-free browser launch failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("OAuth authorization URL could not be opened")]
pub struct BrowserError;

/// Production launcher using the application's platform-safe opener.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemBrowserLauncher;

impl BrowserLauncher for SystemBrowserLauncher {
	fn open<'a>(
		&'a self,
		url: &'a str,
	) -> Pin<Box<dyn Future<Output = Result<(), BrowserError>> + Send + 'a>> {
		Box::pin(async move {
			omp_core::open::open_path(url);
			Ok(())
		})
	}
}

/// Retained OAuth protocol state. Secret fields remain authority-owned and are
/// never serialized into MCP definitions, UI, or journal events.
pub struct OAuthCredentialState {
	/// Opaque affinity used for encrypted persistence.
	pub affinity:       AuthAffinity,
	/// Current access token.
	pub access_token:   SecretString,
	/// Absolute access-token expiration.
	pub expires_at_ms:  Option<u64>,
	/// Token endpoint.
	pub token_endpoint: Str,
	/// Client identity.
	pub client_id:      Str,
	/// Optional confidential client secret.
	pub client_secret:  Option<SecretString>,
	/// RFC 8707 resource indicator.
	pub resource:       Option<Str>,
	/// Refresh material retained when a refresh response omits rotation.
	pub refresh_token:  Option<SecretString>,
	/// Current encrypted-store generation.
	pub generation:     u64,
}

impl fmt::Debug for OAuthCredentialState {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("OAuthCredentialState")
			.field("affinity", &self.affinity)
			.field("access_token", &"[REDACTED]")
			.field("expires_at_ms", &self.expires_at_ms)
			.field("token_endpoint", &self.token_endpoint)
			.field("client_id", &self.client_id)
			.field("client_secret", &self.client_secret.as_ref().map(|_| "[REDACTED]"))
			.field("resource", &self.resource)
			.field("refresh_token", &self.refresh_token.as_ref().map(|_| "[REDACTED]"))
			.field("generation", &self.generation)
			.finish()
	}
}

/// Inputs resolved from a mount and its authentication challenge.
pub struct OAuthAttempt<'a> {
	/// OMP profile identity.
	pub profile:      &'a str,
	/// Stable MCP server identity.
	pub server:       &'a str,
	/// Configured server URL.
	pub server_url:   &'a str,
	/// Validated mount configuration.
	pub config:       &'a McpServerConfig,
	/// HTTP rejection discovery evidence.
	pub challenge:    &'a AuthChallenge,
	/// Local HTTP listener URI behind any TLS terminator.
	pub listener_uri: &'a str,
	/// Cancellation for discovery, browser, callback, and token exchange.
	pub cancel:       CancellationToken,
}

/// Combined OAuth coordinator over the one encrypted credential authority.
pub struct McpOAuth {
	http:      Arc<dyn OAuthHttpClient>,
	authority: Arc<CombinedAuthAuthority>,
	browser:   Arc<dyn BrowserLauncher>,
}

impl McpOAuth {
	/// Creates an Environment-owned OAuth coordinator.
	pub fn new(
		http: Arc<dyn OAuthHttpClient>,
		authority: Arc<CombinedAuthAuthority>,
		browser: Arc<dyn BrowserLauncher>,
	) -> Self {
		Self { http, authority, browser }
	}

	/// Creates live authorization headers for a previously persisted OAuth
	/// grant. A missing lease is intentionally returned to the caller so an
	/// unauthenticated probe can discover the server's challenge.
	pub async fn authority_headers(
		self: &Arc<Self>,
		profile: &str,
		server: &str,
		config: &McpServerConfig,
	) -> Result<Arc<dyn RefreshableHeaders>, OAuthFlowError> {
		let _auth = config
			.auth
			.as_ref()
			.ok_or(OAuthFlowError::MissingAuthorizationServer)?;
		let affinity =
			CombinedAuthAuthority::mcp_affinity(profile, server, PrincipalId::from(profile));
		let persisted = self
			.authority
			.load_mcp_oauth(&affinity)?
			.ok_or(OAuthFlowError::CredentialUnavailable)?;
		let state = OAuthCredentialState {
			affinity,
			access_token: persisted.access_token,
			expires_at_ms: persisted.expires_at_ms,
			token_endpoint: persisted.token_endpoint,
			client_id: persisted.client_id,
			client_secret: persisted.client_secret,
			resource: persisted.resource,
			refresh_token: persisted.refresh_token,
			generation: persisted.generation,
		};
		Ok(AuthorityHeaders::new(Arc::clone(self), state).await?)
	}

	/// Runs discovery, explicit-client/DCR selection, browser authorization,
	/// callback validation, token exchange, and atomic encrypted grant rotation.
	pub async fn authorize(
		&self,
		attempt: OAuthAttempt<'_>,
	) -> Result<OAuthCredentialState, OAuthFlowError> {
		self.authorize_presented(attempt, None).await
	}

	/// Runs authorization while presenting the complete browser URL before the
	/// platform opener is invoked.
	pub async fn authorize_presented(
		&self,
		attempt: OAuthAttempt<'_>,
		present: Option<&(dyn Fn(&str) + Send + Sync)>,
	) -> Result<OAuthCredentialState, OAuthFlowError> {
		if attempt.challenge.kind != omp_oauth::ChallengeKind::OAuth {
			return Err(OAuthFlowError::UnsupportedChallenge);
		}
		let protected = discover_protected_resource_metadata(
			self.http.as_ref(),
			attempt.server_url,
			attempt.challenge.resource_metadata.as_deref(),
		)
		.await
		.ok();
		let discovered = if attempt.challenge.authorization_endpoint.is_some()
			&& attempt.challenge.token_endpoint.is_some()
		{
			None
		} else {
			let issuer = attempt
				.challenge
				.auth_server
				.as_deref()
				.or_else(|| {
					protected
						.as_ref()
						.and_then(|metadata| metadata.authorization_servers.first().map(Str::as_str))
				})
				.ok_or(OAuthFlowError::MissingAuthorizationServer)?;
			Some(discover_authorization_server_metadata(self.http.as_ref(), issuer).await?)
		};
		let authorization_endpoint = attempt
			.challenge
			.authorization_endpoint
			.clone()
			.or_else(|| {
				discovered
					.as_ref()
					.map(|metadata| metadata.authorization_endpoint.clone())
			})
			.ok_or(OAuthFlowError::MissingAuthorizationServer)?;
		let token_endpoint = attempt
			.challenge
			.token_endpoint
			.clone()
			.or_else(|| {
				discovered
					.as_ref()
					.map(|metadata| metadata.token_endpoint.clone())
			})
			.ok_or(OAuthFlowError::MissingAuthorizationServer)?;
		let registration_endpoint =
			attempt
				.challenge
				.registration_endpoint
				.as_deref()
				.or_else(|| {
					discovered
						.as_ref()
						.and_then(|metadata| metadata.registration_endpoint.as_deref())
				});
		let overrides = attempt.config.oauth.as_ref();
		let configured_auth = attempt.config.auth.as_ref();
		let listener_uri = callback_listener_uri(attempt.listener_uri, overrides)?;
		let redirect_uri = overrides
			.and_then(|oauth| oauth.redirect_uri.as_deref())
			.unwrap_or(listener_uri.as_str());
		validate_redirect_pair(redirect_uri, listener_uri.as_str())?;
		let explicit_client = overrides
			.and_then(|oauth| oauth.client_id.as_deref())
			.or_else(|| configured_auth.and_then(|auth| auth.client_id.as_deref()))
			.or(attempt.challenge.client_id.as_deref());
		let redirect_uris = [redirect_uri];
		let client = resolve_client(self.http.as_ref(), ClientConfiguration {
			client_id: explicit_client,
			client_secret: None,
			registration_endpoint,
			redirect_uris: &redirect_uris,
			client_name: "OMP MCP client",
		})
		.await?;
		let scopes = preferred_authorization_scopes(
			protected
				.as_ref()
				.map_or(&[][..], |metadata| metadata.scopes.as_ref()),
			attempt.challenge.scopes.as_ref(),
			discovered
				.as_ref()
				.map_or(&[][..], |metadata| metadata.scopes_supported.as_ref()),
		);
		let resource = configured_auth
			.and_then(|auth| auth.resource.as_deref())
			.or(attempt.challenge.resource.as_deref());
		let pkce = generate_pkce(|bytes| SystemEntropy.fill(bytes))?;
		let pending = begin_authorization(
			AuthorizationRequest {
				authorization_endpoint: authorization_endpoint.as_str(),
				client_id: client.client_id.as_str(),
				redirect_uri,
				scopes: &scopes,
				resource,
				prompt: overrides.and_then(|oauth| oauth.prompt.as_deref()),
			},
			pkce,
		)?;
		let callback = LoopbackCallback::bind(listener_uri.as_str(), pending.pkce.state()).await?;
		if let Some(present) = present {
			present(pending.browser_url.as_str());
		}
		self.browser.open(pending.browser_url.as_str()).await?;
		let grant = callback.receive(&attempt.cancel).await?;
		let grant = complete_authorization(
			self.http.as_ref(),
			token_endpoint.as_str(),
			client.client_id.as_str(),
			client.client_secret.as_ref(),
			pending,
			grant.code,
			grant.state.as_str(),
		)
		.await?;
		let affinity = CombinedAuthAuthority::mcp_affinity(
			attempt.profile,
			attempt.server,
			PrincipalId::from(attempt.profile),
		);
		self.persist_grant(
			affinity,
			token_endpoint,
			client.client_id,
			client.client_secret,
			resource.map(Str::from),
			grant,
			None,
			None,
		)
	}

	/// Refreshes an access token, preserving the previous refresh token when the
	/// token endpoint omits rotation, and updates the encrypted-store
	/// generation.
	pub async fn refresh(&self, state: &mut OAuthCredentialState) -> Result<(), OAuthFlowError> {
		let refresh = state
			.refresh_token
			.as_ref()
			.cloned()
			.ok_or(OAuthFlowError::NotRefreshable)?;
		let previous_refresh = refresh.clone();
		let grant = refresh_token(
			self.http.as_ref(),
			&TokenRequest {
				endpoint:      state.token_endpoint.as_str(),
				client_id:     Some(state.client_id.as_str()),
				client_secret: state.client_secret.as_ref(),
				resource:      state.resource.as_deref(),
			},
			refresh,
		)
		.await?;
		let replacement = self.persist_grant(
			state.affinity.clone(),
			state.token_endpoint.clone(),
			state.client_id.clone(),
			state.client_secret.clone(),
			state.resource.clone(),
			grant,
			Some(previous_refresh),
			Some(state.generation),
		)?;
		*state = replacement;
		Ok(())
	}

	fn persist_grant(
		&self,
		affinity: AuthAffinity,
		token_endpoint: Str,
		client_id: Str,
		client_secret: Option<SecretString>,
		resource: Option<Str>,
		grant: TokenGrant,
		previous_refresh_token: Option<SecretString>,
		expected_generation: Option<u64>,
	) -> Result<OAuthCredentialState, OAuthFlowError> {
		let expires_in = grant.expires_in();
		let (access, refresh_token, token_type, _) = grant.into_parts();
		let refresh_token = refresh_token.or(previous_refresh_token);
		if !token_type.eq_ignore_ascii_case("bearer") {
			return Err(OAuthFlowError::UnsupportedTokenType);
		}
		let now_ms = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis() as u64;
		let expires_at_ms =
			expires_in.map(|duration| now_ms.saturating_add(duration.as_millis() as u64));
		let mut state = OAuthCredentialState {
			affinity,
			access_token: access,
			expires_at_ms,
			token_endpoint,
			client_id,
			client_secret,
			resource,
			refresh_token,
			generation: 0,
		};
		state.generation = self.authority.persist_mcp_oauth(
			&state.affinity,
			&StoredMcpOAuthCredential {
				access_token:   state.access_token.clone(),
				refresh_token:  state.refresh_token.clone(),
				token_endpoint: state.token_endpoint.clone(),
				client_id:      state.client_id.clone(),
				client_secret:  state.client_secret.clone(),
				resource:       state.resource.clone(),
				expires_at_ms:  state.expires_at_ms,
				generation:     0,
			},
			now_ms,
			expected_generation,
		)?;
		Ok(state)
	}
}

/// Whether a failed grant should be cleared or retained for retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OAuthFailureClass {
	/// Authorization or refresh was conclusively rejected.
	Definitive,
	/// Network, cancellation, browser, or storage failure may succeed later.
	Transient,
}

fn callback_listener_uri(
	default_uri: &str,
	overrides: Option<&OauthConfig>,
) -> Result<Str, OAuthFlowError> {
	let mut url = Url::parse(default_uri).map_err(|_| OAuthFlowError::InvalidCallbackConfig)?;
	if let Some(port) = overrides.and_then(|oauth| oauth.callback_port) {
		url.set_port(Some(port))
			.map_err(|()| OAuthFlowError::InvalidCallbackConfig)?;
	}
	if let Some(path) = overrides.and_then(|oauth| oauth.callback_path.as_deref()) {
		let path = if path.starts_with('/') {
			path.to_owned()
		} else {
			format!("/{path}")
		};
		url.set_path(&path);
	}
	Ok(Str::from(url.as_str()))
}

/// MCP OAuth flow failure with secret-free diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum OAuthFlowError {
	/// Challenge requires an API key or unknown mechanism rather than OAuth.
	#[error("MCP authentication challenge is not OAuth")]
	UnsupportedChallenge,
	/// Callback port/path overrides were invalid.
	#[error("MCP OAuth callback configuration is invalid")]
	InvalidCallbackConfig,
	/// Discovery did not identify an authorization server.
	#[error("MCP OAuth challenge did not identify an authorization server")]
	MissingAuthorizationServer,
	/// Grant has no refresh token.
	#[error("MCP OAuth grant is not refreshable")]
	NotRefreshable,
	/// Token endpoint returned a non-bearer token.
	#[error("MCP OAuth token type is unsupported")]
	UnsupportedTokenType,
	/// Persisted grant did not contain a usable access token.
	#[error("MCP OAuth bearer token is invalid")]
	InvalidBearerToken,
	/// No persisted renewable grant exists for this mount.
	#[error("MCP OAuth credential is unavailable")]
	CredentialUnavailable,
	/// Metadata discovery failed.
	#[error(transparent)]
	Metadata(#[from] MetadataError),
	/// Client selection or DCR failed.
	#[error(transparent)]
	Registration(#[from] ClientRegistrationError),
	/// PKCE entropy was unavailable.
	#[error(transparent)]
	Entropy(#[from] omp_oauth::EntropyError),
	/// Authorization request was invalid.
	#[error(transparent)]
	Authorization(#[from] omp_oauth::AuthorizationError),
	/// Callback could not bind or redirect validation failed.
	#[error(transparent)]
	CallbackBind(#[from] CallbackBindError),
	/// Callback did not complete.
	#[error(transparent)]
	Callback(#[from] CallbackError),
	/// Authorization exchange failed.
	#[error(transparent)]
	Complete(#[from] CompleteAuthorizationError),
	/// Refresh failed.
	#[error(transparent)]
	Token(#[from] TokenError),
	/// Browser could not open.
	#[error(transparent)]
	Browser(#[from] BrowserError),
	/// Complete encrypted OAuth record persistence failed.
	#[error(transparent)]
	Store(#[from] McpOAuthStoreError),
}
fn preferred_authorization_scopes(
	protected: &[Str],
	challenge: &[Str],
	authorization_server: &[Str],
) -> Vec<Str> {
	// Resource and RFC 6750 challenge scopes describe this grant; the
	// authorization-server list is only a broad fallback catalogue.
	let source = if !protected.is_empty() {
		protected
	} else if !challenge.is_empty() {
		challenge
	} else {
		authorization_server
	};
	let mut scopes = source.to_vec();
	scopes.sort_unstable();
	scopes.dedup();
	scopes
}

impl OAuthFlowError {
	/// Classifies whether retained refresh material remains eligible for retry.
	pub const fn class(&self) -> OAuthFailureClass {
		match self {
			Self::UnsupportedChallenge
			| Self::InvalidCallbackConfig
			| Self::MissingAuthorizationServer
			| Self::NotRefreshable
			| Self::UnsupportedTokenType
			| Self::InvalidBearerToken
			| Self::CredentialUnavailable
			| Self::Registration(ClientRegistrationError::Rejected { .. })
			| Self::Registration(ClientRegistrationError::Malformed)
			| Self::Registration(ClientRegistrationError::RegistrationUnavailable)
			| Self::Registration(ClientRegistrationError::InvalidRedirect)
			| Self::Authorization(_)
			| Self::CallbackBind(_)
			| Self::Complete(CompleteAuthorizationError::StateMismatch)
			| Self::Complete(CompleteAuthorizationError::Token(TokenError::Rejected { .. }))
			| Self::Complete(CompleteAuthorizationError::Token(TokenError::Provider { .. }))
			| Self::Token(TokenError::Rejected { .. })
			| Self::Token(TokenError::Provider { .. })
			| Self::Token(TokenError::Malformed) => OAuthFailureClass::Definitive,
			Self::Metadata(_)
			| Self::Registration(_)
			| Self::Entropy(_)
			| Self::Callback(_)
			| Self::Complete(_)
			| Self::Token(_)
			| Self::Browser(_)
			| Self::Store(_) => OAuthFailureClass::Transient,
		}
	}
}
#[cfg(test)]
mod tests {
	use omp_core::Str;

	use super::preferred_authorization_scopes;

	#[test]
	fn protected_and_challenge_scopes_precede_authorization_server_catalogue() {
		let protected = [Str::from("offline_access"), Str::from("genie")];
		let challenge = [Str::from("challenge.read")];
		let catalogue =
			[Str::from("email"), Str::from("openid"), Str::from("profile"), Str::from("workspace")];

		assert_eq!(preferred_authorization_scopes(&protected, &challenge, &catalogue), vec![
			Str::from("genie"),
			Str::from("offline_access")
		],);
		assert_eq!(preferred_authorization_scopes(&[], &challenge, &catalogue), vec![Str::from(
			"challenge.read"
		)],);
		assert_eq!(preferred_authorization_scopes(&[], &[], &catalogue), vec![
			Str::from("email"),
			Str::from("openid"),
			Str::from("profile"),
			Str::from("workspace"),
		],);
	}
}
