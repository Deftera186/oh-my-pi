//! Credential-injecting gateway administration over the canonical daemon.

use std::{
	fs, io,
	net::SocketAddr,
	path::{Path, PathBuf},
};

use miette::{IntoDiagnostic as _, WrapErr as _, miette};
use omp_proto::{
	auth::v1::{ProbeCredentialsRequest, auth_client::AuthClient},
	gateway::v1::{HelloRequest, gateway_client::GatewayClient},
};
use ring::rand::{SecureRandom as _, SystemRandom};
use serde::Serialize;
use tonic::{Request, metadata::MetadataValue};
use zeroize::Zeroizing;

use crate::{
	auth_broker_cmd,
	cli::{AuthGatewayArgs, AuthGatewayCommand},
	daemon::{DaemonConfig, DaemonHandle},
	endpoint::LocalEndpoint,
};

const TOKEN_FILE: &str = "auth-gateway.token";

/// Starts, rotates, and health-checks the gateway without owning credentials.
pub async fn run(args: AuthGatewayArgs) -> miette::Result<()> {
	let data_dir = omp_core::dirs::data_dir(args.data_dir).into_diagnostic()?;
	fs::create_dir_all(&data_dir).into_diagnostic()?;
	match args.command {
		AuthGatewayCommand::Serve { bind } => {
			ensure_token(&data_dir, false)?;
			let config = DaemonConfig::tcp(bind, token_path(&data_dir));
			let handle = DaemonHandle::start(config.with_data_dir(data_dir.clone()))
				.await
				.into_diagnostic()?;
			println!("auth-gateway listening on http://{bind}");
			println!("bearer token: {}", token_path(&data_dir).display());
			handle.wait().await.into_diagnostic()
		},
		AuthGatewayCommand::Token { regenerate, json } => {
			let token = ensure_token(&data_dir, regenerate)?;
			let path = token_path(&data_dir);
			if json {
				#[derive(Serialize)]
				struct TokenOutput<'a> {
					token: &'a str,
					path:  &'a Path,
				}
				serde_json::to_writer(io::stdout().lock(), &TokenOutput {
					token: &token,
					path:  &path,
				})
				.into_diagnostic()?;
				println!();
			} else {
				println!("{}", token.as_str());
			}
			Ok(())
		},
		AuthGatewayCommand::Status { bind, json } => status(&data_dir, bind, json).await,
		AuthGatewayCommand::Check { bind, strict, json } => {
			check(&data_dir, bind, strict, json).await
		},
	}
}

async fn status(data_dir: &Path, bind: SocketAddr, json: bool) -> miette::Result<()> {
	let token = read_token(data_dir)?;
	let channel = LocalEndpoint::tcp(bind)
		.connect()
		.await
		.into_diagnostic()
		.wrap_err_with(|| format!("could not connect to tcp://{bind}"))?;
	let request = authenticated(
		HelloRequest {
			client:       "omp-auth-gateway-cli".to_owned(),
			schema_rev:   omp_proto::SCHEMA_REV,
			capabilities: vec!["auth".to_owned(), "gateway.forward".to_owned()],
		},
		&token,
	)?;
	let response = GatewayClient::new(channel)
		.hello(request)
		.await
		.into_diagnostic()?
		.into_inner();
	if response.schema_rev < omp_proto::SCHEMA_REV {
		return Err(miette!(
			"gateway schema {} is older than required {}",
			response.schema_rev,
			omp_proto::SCHEMA_REV,
		));
	}
	if json {
		serde_json::to_writer(io::stdout().lock(), &response).into_diagnostic()?;
		println!();
	} else {
		println!(
			"healthy: {} schema {} [{}]",
			response.server_version,
			response.schema_rev,
			response.capabilities.join(", "),
		);
	}
	Ok(())
}

async fn check(data_dir: &Path, bind: SocketAddr, strict: bool, json: bool) -> miette::Result<()> {
	let token = read_token(data_dir)?;
	let channel = LocalEndpoint::tcp(bind)
		.connect()
		.await
		.into_diagnostic()
		.wrap_err_with(|| format!("could not connect to tcp://{bind}"))?;
	let request =
		authenticated(ProbeCredentialsRequest { provider: String::new(), strict }, &token)?;
	let response = AuthClient::new(channel)
		.probe_credentials(request)
		.await
		.into_diagnostic()?
		.into_inner();
	let failed = response
		.credentials
		.iter()
		.filter(|health| !health.healthy)
		.count();
	if json {
		serde_json::to_writer(io::stdout().lock(), &response).into_diagnostic()?;
		println!();
	} else {
		for health in &response.credentials {
			let status = if health.healthy { "ok" } else { "FAIL" };
			let http = health
				.status_code
				.map_or_else(|| "-".to_owned(), |status| status.to_string());
			println!(
				"{status:4} provider={} credential={} http={} latency={}ms error_class={}",
				health.provider, health.credential_id, http, health.latency_ms, health.error_class,
			);
		}
		println!(
			"{} healthy, {failed} failed, {} total{}",
			response.credentials.len() - failed,
			response.credentials.len(),
			if strict { " [strict]" } else { "" },
		);
	}
	if failed > 0 {
		return Err(miette!("{failed} credential probe(s) failed"));
	}
	Ok(())
}

fn authenticated<T>(message: T, token: &str) -> miette::Result<Request<T>> {
	let encoded = Zeroizing::new(format!("Bearer {token}"));
	let mut value = MetadataValue::try_from(encoded.as_str()).into_diagnostic()?;
	value.set_sensitive(true);
	let mut request = Request::new(message);
	request.metadata_mut().insert("authorization", value);
	Ok(request)
}

fn token_path(data_dir: &Path) -> PathBuf {
	data_dir.join(TOKEN_FILE)
}

fn read_token(data_dir: &Path) -> miette::Result<Zeroizing<String>> {
	let token = Zeroizing::new(fs::read_to_string(token_path(data_dir)).into_diagnostic()?);
	if token.trim().is_empty() {
		return Err(miette!("gateway bearer token file is empty"));
	}
	Ok(Zeroizing::new(token.trim().to_owned()))
}

fn ensure_token(data_dir: &Path, regenerate: bool) -> miette::Result<Zeroizing<String>> {
	let path = token_path(data_dir);
	if !regenerate && path.is_file() {
		return read_token(data_dir);
	}
	let mut bytes = Zeroizing::new([0_u8; 32]);
	SystemRandom::new()
		.fill(bytes.as_mut())
		.map_err(|_| miette!("system random source failed"))?;
	let token = Zeroizing::new(omp_core::hex::encode(&*bytes).into_string());
	auth_broker_cmd::write_owner_only(&path, token.as_bytes())?;
	Ok(token)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn bearer_metadata_is_sensitive_and_does_not_debug_the_token() {
		let request = authenticated((), "gateway-secret-marker").expect("bearer request");
		let debug = format!("{:?}", request.metadata());
		assert!(!debug.contains("gateway-secret-marker"));
	}
}
