//! Shared outbound HTTP clients and process-wide TLS policy.
//!
//! Reqwest clients own connection pools. Callers clone one of the process-wide
//! clients instead of constructing a pool per request or host instance.

use std::sync::LazyLock;

use reqwest::{Client, ClientBuilder, redirect::Policy};

static DEFAULT_CLIENT: LazyLock<Client> = LazyLock::new(|| {
	client_builder()
		.build()
		.expect("default HTTP client configuration must be valid")
});
static NO_REDIRECT_CLIENT: LazyLock<Client> = LazyLock::new(|| {
	client_builder()
		.redirect(Policy::none())
		.build()
		.expect("redirect-disabled HTTP client configuration must be valid")
});

/// Clones the process-wide client using Reqwest's default redirect policy.
#[inline]
pub fn default_client() -> Client {
	DEFAULT_CLIENT.clone()
}

/// Clones the process-wide redirect-disabled client.
#[inline]
pub fn no_redirect_client() -> Client {
	NO_REDIRECT_CLIENT.clone()
}

/// Starts a client builder after installing the workspace Ring provider.
#[inline]
pub fn client_builder() -> ClientBuilder {
	let _ = rustls::crypto::ring::default_provider().install_default();
	Client::builder()
}
