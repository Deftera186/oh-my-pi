//! Catalog-aware credential acquisition across typed source engines.

use std::{collections::BTreeMap, env, fmt, sync::Arc};

use futures::future::{BoxFuture, FutureExt as _};
use omp_catalog::{AuthSpecId, Catalog, provider::AuthSpecKind};
use omp_core::{SecretString, Str, sf};

use super::lease::{
	AuthRejection, CredentialError, CredentialKind, CredentialLease, CredentialNeed,
	CredentialSource, LeaseMeta,
};
use crate::{AccountId, PrincipalId};

const ENVIRONMENT_TAG: &str = "environment";
const STORED_TAG: &str = "stored";
const ADC_TAG: &str = "application-default";
const AWS_TAG: &str = "aws-chain";
const OAUTH_TAG: &str = "oauth";
const SESSION_TAG: &str = "session";
const INVOCATION_TAG: &str = "invocation";

/// Secret environment boundary used by [`CredentialBroker`].
pub trait CredentialEnvironment: Send + Sync {
	/// Reads one exact catalog-declared name into a zeroizing secret wrapper.
	fn read(&self, name: &str) -> Result<Option<SecretString>, CredentialError>;
}

/// Process environment implementation that performs no alias or fallback
/// lookup.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCredentialEnvironment;

impl CredentialEnvironment for SystemCredentialEnvironment {
	fn read(&self, name: &str) -> Result<Option<SecretString>, CredentialError> {
		if !name.starts_with("OMP_") {
			return Err(CredentialError::InvalidSource);
		}
		match env::var(name) {
			Ok(value) if value.is_empty() => Err(CredentialError::InvalidSource),
			Ok(value) => Ok(Some(SecretString::from(value))),
			Err(env::VarError::NotPresent) => Ok(None),
			Err(env::VarError::NotUnicode(_)) => Err(CredentialError::SourceFailure),
		}
	}
}

/// Optional typed engines used by the catalog credential broker.
#[derive(Clone, Default)]
pub struct CredentialBrokerEngines {
	/// Encrypted account-store engine.
	pub stored:              Option<Arc<dyn CredentialSource>>,
	/// Application-default credential engine.
	pub application_default: Option<Arc<dyn CredentialSource>>,
	/// AWS credential-chain engine.
	pub aws:                 Option<Arc<dyn CredentialSource>>,
	/// OAuth login/refresh engine.
	pub oauth:               Option<Arc<dyn CredentialSource>>,
	/// Interactive provider-session engine.
	pub session:             Option<Arc<dyn CredentialSource>>,
}

impl fmt::Debug for CredentialBrokerEngines {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("CredentialBrokerEngines")
			.field("stored", &self.stored.is_some())
			.field("application_default", &self.application_default.is_some())
			.field("aws", &self.aws.is_some())
			.field("oauth", &self.oauth.is_some())
			.field("session", &self.session.is_some())
			.finish()
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EngineKind {
	Stored,
	ApplicationDefault,
	Aws,
	OAuth,
	Session,
}

impl EngineKind {
	const fn tag(self) -> &'static str {
		match self {
			Self::Stored => STORED_TAG,
			Self::ApplicationDefault => ADC_TAG,
			Self::Aws => AWS_TAG,
			Self::OAuth => OAUTH_TAG,
			Self::Session => SESSION_TAG,
		}
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BrokerSource {
	Environment(Box<[Str]>),
	BasicEnvironment { username_names: Box<[Str]>, password_names: Box<[Str]> },
	Engine(EngineKind),
}
#[derive(Clone, Debug)]
struct InvocationOverride {
	specs:  Arc<BTreeMap<AuthSpecId, CredentialKind>>,
	secret: SecretString,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BrokerPlan {
	kind:    CredentialKind,
	sources: Box<[BrokerSource]>,
}

/// Catalog compilation failure for credential acquisition plans.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CredentialBrokerError {
	/// An authenticated catalog record has no declared acquisition source.
	#[error("catalog authentication specification has no credential source")]
	MissingSource(AuthSpecId),
	/// A credential environment source contains an empty or non-OMP name.
	#[error("catalog credential environment source is invalid")]
	InvalidEnvironment(AuthSpecId),
	/// The selected provider does not exist in the catalog.
	#[error("invocation credential override names an unknown provider")]
	UnknownProvider(omp_catalog::ProviderId),
	/// The selected provider has no scalar authentication compatible with a
	/// generic API key.
	#[error("selected provider does not accept a generic invocation API key")]
	UnsupportedOverride(omp_catalog::ProviderId),
}

/// Catalog-aware composite credential source.
///
/// Plans retain exact catalog source order. Only `Unavailable` advances to the
/// next source; cancellation, invalid source, expiry, staleness, and engine
/// failure remain typed terminal evidence.
#[derive(Clone)]
pub struct CredentialBroker {
	plans:       Arc<BTreeMap<AuthSpecId, BrokerPlan>>,
	environment: Arc<dyn CredentialEnvironment>,
	engines:     CredentialBrokerEngines,
	invocation:  Option<InvocationOverride>,
}

impl CredentialBroker {
	/// Compiles immutable acquisition plans from the canonical catalog.
	pub fn from_catalog(
		catalog: &Catalog,
		environment: Arc<dyn CredentialEnvironment>,
		engines: CredentialBrokerEngines,
	) -> Result<Self, CredentialBrokerError> {
		let mut plans = BTreeMap::new();
		for auth in catalog.auth_specs() {
			let Some(kind) = credential_kind(auth.kind) else {
				continue;
			};
			let mut sources = Vec::with_capacity(auth.credential_sources.len());
			for source in &auth.credential_sources {
				use omp_catalog::provider::CredentialSourceSpec as CatalogSource;
				let source = match source {
					CatalogSource::Environment { ordered_names } => {
						if ordered_names.is_empty()
							|| ordered_names.iter().any(|name| !name.starts_with("OMP_"))
						{
							return Err(CredentialBrokerError::InvalidEnvironment(auth.id.clone()));
						}
						BrokerSource::Environment(ordered_names.clone())
					},
					CatalogSource::BasicEnvironment { username_names, password_names } => {
						if username_names.is_empty()
							|| password_names.is_empty()
							|| username_names.iter().any(|name| !name.starts_with("OMP_"))
							|| password_names.iter().any(|name| !name.starts_with("OMP_"))
						{
							return Err(CredentialBrokerError::InvalidEnvironment(auth.id.clone()));
						}
						BrokerSource::BasicEnvironment {
							username_names: username_names.clone(),
							password_names: password_names.clone(),
						}
					},
					CatalogSource::Stored => BrokerSource::Engine(EngineKind::Stored),
					CatalogSource::ApplicationDefault { .. } => {
						BrokerSource::Engine(EngineKind::ApplicationDefault)
					},
					CatalogSource::AwsChain => BrokerSource::Engine(EngineKind::Aws),
					CatalogSource::Oauth { .. } => BrokerSource::Engine(EngineKind::OAuth),
					CatalogSource::Session => BrokerSource::Engine(EngineKind::Session),
				};
				sources.push(source);
			}
			if sources.is_empty() {
				return Err(CredentialBrokerError::MissingSource(auth.id.clone()));
			}
			plans.insert(auth.id.clone(), BrokerPlan { kind, sources: sources.into_boxed_slice() });
		}
		Ok(Self { plans: Arc::new(plans), environment, engines, invocation: None })
	}

	/// Uses the process environment without upstream aliases or fallbacks.
	pub fn system(
		catalog: &Catalog,
		engines: CredentialBrokerEngines,
	) -> Result<Self, CredentialBrokerError> {
		Self::from_catalog(catalog, Arc::new(SystemCredentialEnvironment), engines)
	}

	/// Returns a session-owned broker overlay for one selected provider.
	///
	/// The generic key is held only by the returned clone. It is never written
	/// to the process environment or delegated to a durable credential engine.
	pub fn with_api_key_override(
		&self,
		catalog: &Catalog,
		provider: &omp_catalog::ProviderId<str>,
		secret: SecretString,
	) -> Result<Self, CredentialBrokerError> {
		let provider = catalog
			.provider(provider)
			.ok_or_else(|| CredentialBrokerError::UnknownProvider(provider.to_owned()))?;
		let specs = provider
			.auth
			.iter()
			.filter_map(|id| {
				let kind = credential_kind(catalog.auth_spec(id)?.kind)?;
				matches!(
					kind,
					CredentialKind::ApiKey | CredentialKind::Bearer | CredentialKind::SessionToken
				)
				.then(|| (id.clone(), kind))
			})
			.collect::<BTreeMap<_, _>>();
		if specs.is_empty() {
			return Err(CredentialBrokerError::UnsupportedOverride(provider.id.clone()));
		}
		let mut broker = self.clone();
		broker.invocation = Some(InvocationOverride { specs: Arc::new(specs), secret });
		Ok(broker)
	}

	/// Refreshes the renewable engine for an exact account/spec selection.
	///
	/// Stored OAuth is authoritative when installed. Environment, invocation,
	/// ADC, AWS, and session engines are never considered and no ordinary
	/// source fallback occurs.
	pub fn refresh_account(
		&self,
		need: CredentialNeed,
	) -> BoxFuture<'_, Result<CredentialLease, CredentialError>> {
		async move {
			let plan = self
				.plans
				.get(&need.spec)
				.ok_or(CredentialError::InvalidSource)?;
			let selected = [EngineKind::Stored, EngineKind::OAuth]
				.into_iter()
				.find(|kind| {
					self.engine(*kind).is_some()
						&& plan
							.sources
							.iter()
							.any(|source| source == &BrokerSource::Engine(*kind))
				})
				.ok_or(CredentialError::Unavailable)?;
			self
				.engine(selected)
				.expect("selected installed renewable engine")
				.refresh_lease(need.clone())
				.await
				.and_then(|lease| Self::validate_lease(lease, &need, plan.kind, selected.tag()))
		}
		.boxed()
	}

	/// Refreshes the exact source that produced a rejected lease and returns
	/// its new generation once.
	///
	/// Environment and invocation credentials are nonrenewable and fail
	/// closed; no later source is tried.
	pub fn refresh_lease<'a>(
		&'a self,
		rejected: &'a CredentialLease,
		need: CredentialNeed,
	) -> BoxFuture<'a, Result<CredentialLease, CredentialError>> {
		async move {
			let Some(tag) = rejected.source_tag() else {
				return Err(CredentialError::InvalidSource);
			};
			let kind = match tag {
				STORED_TAG => EngineKind::Stored,
				OAUTH_TAG => EngineKind::OAuth,
				ENVIRONMENT_TAG | INVOCATION_TAG | ADC_TAG | AWS_TAG | SESSION_TAG => {
					return Err(CredentialError::Unavailable);
				},
				_ => return Err(CredentialError::InvalidSource),
			};
			let plan = self
				.plans
				.get(&need.spec)
				.ok_or(CredentialError::InvalidSource)?;
			self
				.engine(kind)
				.ok_or(CredentialError::Unavailable)?
				.refresh_lease(need.clone())
				.await
				.and_then(|lease| Self::validate_lease(lease, &need, plan.kind, kind.tag()))
		}
		.boxed()
	}

	fn invocation_lease(
		&self,
		need: &CredentialNeed,
	) -> Option<Result<CredentialLease, CredentialError>> {
		let invocation = self.invocation.as_ref()?;
		let kind = *invocation.specs.get(&need.spec)?;
		let account = need
			.account
			.clone()
			.unwrap_or_else(|| AccountId::from("invocation"));
		let principal = need
			.principal
			.clone()
			.unwrap_or_else(|| PrincipalId::from("invocation"));
		let meta = LeaseMeta { account, principal, generation: 0, expires_at: None };
		let lease = match kind {
			CredentialKind::ApiKey => CredentialLease::api_key(meta, invocation.secret.clone()),
			CredentialKind::Bearer => CredentialLease::bearer(meta, invocation.secret.clone()),
			CredentialKind::SessionToken => {
				CredentialLease::session_token(meta, invocation.secret.clone())
			},
			CredentialKind::Basic | CredentialKind::AwsSigV4 => {
				return Some(Err(CredentialError::InvalidSource));
			},
		};
		Some(Ok(lease.with_source_tag(sf!(INVOCATION_TAG))))
	}

	fn engine(&self, kind: EngineKind) -> Option<&Arc<dyn CredentialSource>> {
		match kind {
			EngineKind::Stored => self.engines.stored.as_ref(),
			EngineKind::ApplicationDefault => self.engines.application_default.as_ref(),
			EngineKind::Aws => self.engines.aws.as_ref(),
			EngineKind::OAuth => self.engines.oauth.as_ref(),
			EngineKind::Session => self.engines.session.as_ref(),
		}
	}

	fn environment_lease(
		&self,
		names: &[Str],
		need: &CredentialNeed,
		kind: CredentialKind,
	) -> Result<CredentialLease, CredentialError> {
		for name in names {
			let Some(secret) = self.environment.read(name)? else {
				continue;
			};
			let account = need.account.clone().ok_or(CredentialError::InvalidSource)?;
			let principal = need
				.principal
				.clone()
				.ok_or(CredentialError::InvalidSource)?;
			let meta = LeaseMeta { account, principal, generation: 0, expires_at: None };
			let lease = match kind {
				CredentialKind::ApiKey => CredentialLease::api_key(meta, secret),
				CredentialKind::Basic => return Err(CredentialError::InvalidSource),
				CredentialKind::Bearer => CredentialLease::bearer(meta, secret),
				CredentialKind::SessionToken => CredentialLease::session_token(meta, secret),
				CredentialKind::AwsSigV4 => return Err(CredentialError::InvalidSource),
			};
			return Ok(lease.with_source_tag(sf!(ENVIRONMENT_TAG)));
		}
		Err(CredentialError::Unavailable)
	}

	fn basic_environment_lease(
		&self,
		username_names: &[Str],
		password_names: &[Str],
		need: &CredentialNeed,
	) -> Result<CredentialLease, CredentialError> {
		let read_first = |names: &[Str]| {
			for name in names {
				if let Some(secret) = self.environment.read(name)? {
					return Ok(secret);
				}
			}
			Err(CredentialError::Unavailable)
		};
		let username = read_first(username_names)?;
		let password = read_first(password_names)?;
		let account = need.account.clone().ok_or(CredentialError::InvalidSource)?;
		let principal = need
			.principal
			.clone()
			.ok_or(CredentialError::InvalidSource)?;
		let meta = LeaseMeta { account, principal, generation: 0, expires_at: None };
		Ok(CredentialLease::basic(meta, username, password).with_source_tag(sf!(ENVIRONMENT_TAG)))
	}

	fn validate_lease(
		lease: CredentialLease,
		need: &CredentialNeed,
		expected: CredentialKind,
		tag: &'static str,
	) -> Result<CredentialLease, CredentialError> {
		if lease.kind() != expected {
			return Err(CredentialError::InvalidSource);
		}
		if need
			.account
			.as_ref()
			.is_some_and(|account| account != &lease.meta().account)
			|| need
				.principal
				.as_ref()
				.is_some_and(|principal| principal != &lease.meta().principal)
		{
			return Err(CredentialError::InvalidSource);
		}
		if lease.is_expired_at(need.valid_after) {
			return Err(CredentialError::Expired);
		}
		Ok(lease.with_source_tag(Str::new(tag)))
	}
}

impl fmt::Debug for CredentialBroker {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("CredentialBroker")
			.field("plans", &self.plans.len())
			.field("engines", &self.engines)
			.field("invocation", &self.invocation.is_some())
			.finish()
	}
}

impl CredentialSource for CredentialBroker {
	fn lease(
		&self,
		need: CredentialNeed,
	) -> BoxFuture<'_, Result<CredentialLease, CredentialError>> {
		async move {
			if let Some(lease) = self.invocation_lease(&need) {
				return lease;
			}
			let plan = self
				.plans
				.get(&need.spec)
				.ok_or(CredentialError::InvalidSource)?;
			for source in &plan.sources {
				let result = match source {
					BrokerSource::Environment(names) => self.environment_lease(names, &need, plan.kind),
					BrokerSource::BasicEnvironment { username_names, password_names } => {
						self.basic_environment_lease(username_names, password_names, &need)
					},
					BrokerSource::Engine(kind) => match self.engine(*kind) {
						Some(engine) => engine
							.lease(need.clone())
							.await
							.and_then(|lease| Self::validate_lease(lease, &need, plan.kind, kind.tag())),
						None => Err(CredentialError::Unavailable),
					},
				};
				if !matches!(&result, Err(CredentialError::Unavailable)) {
					return result;
				}
			}
			Err(CredentialError::Unavailable)
		}
		.boxed()
	}

	fn reject<'a>(
		&'a self,
		lease: &'a CredentialLease,
		evidence: AuthRejection,
	) -> BoxFuture<'a, Result<(), CredentialError>> {
		async move {
			let Some(tag) = lease.source_tag() else {
				return Err(CredentialError::InvalidSource);
			};
			if tag == ENVIRONMENT_TAG {
				return Ok(());
			}
			if tag == INVOCATION_TAG {
				return Ok(());
			}
			let kind = match tag {
				STORED_TAG => EngineKind::Stored,
				ADC_TAG => EngineKind::ApplicationDefault,
				AWS_TAG => EngineKind::Aws,
				OAUTH_TAG => EngineKind::OAuth,
				SESSION_TAG => EngineKind::Session,
				_ => return Err(CredentialError::InvalidSource),
			};
			self
				.engine(kind)
				.ok_or(CredentialError::Unavailable)?
				.reject(lease, evidence)
				.await
		}
		.boxed()
	}
}

const fn credential_kind(kind: AuthSpecKind) -> Option<CredentialKind> {
	match kind {
		AuthSpecKind::None => None,
		AuthSpecKind::ApiKey => Some(CredentialKind::ApiKey),
		AuthSpecKind::Basic => Some(CredentialKind::Basic),
		AuthSpecKind::Bearer
		| AuthSpecKind::OptionalBearer
		| AuthSpecKind::Oauth
		| AuthSpecKind::GcpAdc
		| AuthSpecKind::AzureAd
		| AuthSpecKind::GithubApp => Some(CredentialKind::Bearer),
		AuthSpecKind::AwsSigv4 => Some(CredentialKind::AwsSigV4),
		AuthSpecKind::OmpSession => Some(CredentialKind::SessionToken),
	}
}

#[cfg(test)]
mod tests {
	use std::{
		sync::atomic::{AtomicUsize, Ordering},
		time::SystemTime,
	};

	use omp_core::ExposeSecret as _;
	use parking_lot::Mutex;

	use super::*;
	use crate::id::{AccountId, PrincipalId};

	#[derive(Debug, Default)]
	struct EmptyEnvironment;

	impl CredentialEnvironment for EmptyEnvironment {
		fn read(&self, _: &str) -> Result<Option<SecretString>, CredentialError> {
			Ok(None)
		}
	}
	#[derive(Debug, Default)]
	struct TrackingEnvironment {
		reads: AtomicUsize,
	}

	impl CredentialEnvironment for TrackingEnvironment {
		fn read(&self, _: &str) -> Result<Option<SecretString>, CredentialError> {
			self.reads.fetch_add(1, Ordering::Relaxed);
			Ok(None)
		}
	}

	#[derive(Debug, Default)]
	struct TrackingStore {
		leases: AtomicUsize,
	}

	impl CredentialSource for TrackingStore {
		fn lease(
			&self,
			_: CredentialNeed,
		) -> BoxFuture<'_, Result<CredentialLease, CredentialError>> {
			self.leases.fetch_add(1, Ordering::Relaxed);
			futures::future::ready(Err(CredentialError::Unavailable)).boxed()
		}

		fn reject<'a>(
			&'a self,
			_: &'a CredentialLease,
			_: AuthRejection,
		) -> BoxFuture<'a, Result<(), CredentialError>> {
			futures::future::ready(Ok(())).boxed()
		}
	}

	#[tokio::test]
	async fn invocation_key_is_provider_scoped_and_bypasses_external_sources() {
		let catalog = Catalog::embedded();
		let selected = catalog
			.providers()
			.iter()
			.find_map(|provider| {
				provider.auth.iter().find_map(|spec| {
					let kind = credential_kind(catalog.auth_spec(spec)?.kind)?;
					matches!(
						kind,
						CredentialKind::ApiKey | CredentialKind::Bearer | CredentialKind::SessionToken
					)
					.then(|| (provider, spec.clone()))
				})
			})
			.expect("provider with scalar authentication");
		let selected_kind = credential_kind(
			catalog
				.auth_spec(&selected.1)
				.expect("selected authentication spec")
				.kind,
		)
		.expect("selected scalar authentication kind");
		let other = catalog
			.auth_specs()
			.iter()
			.find(|spec| {
				spec.id != selected.1
					&& credential_kind(spec.kind).is_some()
					&& !selected.0.auth.contains(&spec.id)
			})
			.expect("authentication outside selected provider");
		let environment = Arc::new(TrackingEnvironment::default());
		let store = Arc::new(TrackingStore::default());
		let broker =
			CredentialBroker::from_catalog(catalog, environment.clone(), CredentialBrokerEngines {
				stored: Some(store.clone()),
				..CredentialBrokerEngines::default()
			})
			.expect("base broker")
			.with_api_key_override(catalog, &selected.0.id, SecretString::from("invocation-only-key"))
			.expect("provider override");
		let need = |spec| CredentialNeed {
			spec,
			account: Some(AccountId::from("selected-account")),
			principal: Some(PrincipalId::from("selected-principal")),
			valid_after: SystemTime::UNIX_EPOCH,
		};

		let lease = broker
			.lease(need(selected.1.clone()))
			.await
			.expect("invocation lease");
		assert_eq!(lease.scalar_secret().expect("scalar key").expose_secret(), "invocation-only-key");
		assert_eq!(lease.kind(), selected_kind);
		assert_eq!(lease.meta().account.as_str(), "selected-account");
		assert_eq!(lease.meta().principal.as_str(), "selected-principal");
		assert_eq!(
			broker
				.refresh_lease(&lease, need(selected.1.clone()))
				.await
				.expect_err("invocation credentials are nonrenewable"),
			CredentialError::Unavailable,
		);
		assert_eq!(
			broker
				.refresh_account(need(selected.1.clone()))
				.await
				.expect_err("account has no renewable stored credential"),
			CredentialError::Unavailable,
		);
		assert_eq!(environment.reads.load(Ordering::Relaxed), 0);
		assert_eq!(store.leases.load(Ordering::Relaxed), 0);

		assert_eq!(
			broker.lease(need(other.id.clone())).await.unwrap_err(),
			CredentialError::Unavailable
		);
		assert!(
			environment.reads.load(Ordering::Relaxed) > 0 || store.leases.load(Ordering::Relaxed) > 0
		);
	}

	#[test]
	fn embedded_catalog_compiles_one_exact_plan_per_authenticated_spec() {
		let catalog = Catalog::embedded();
		let broker = CredentialBroker::from_catalog(
			catalog,
			Arc::new(EmptyEnvironment),
			CredentialBrokerEngines::default(),
		)
		.expect("credential plans");
		let authenticated = catalog
			.auth_specs()
			.iter()
			.filter(|auth| credential_kind(auth.kind).is_some())
			.count();
		assert_eq!(broker.plans.len(), authenticated);
		for auth in catalog
			.auth_specs()
			.iter()
			.filter(|auth| credential_kind(auth.kind).is_some())
		{
			let plan = broker
				.plans
				.get(&auth.id)
				.expect("plan by exact auth identity");
			assert_eq!(plan.sources.len(), auth.credential_sources.len());
		}
	}

	#[derive(Debug)]
	struct OrderedEnvironment {
		calls: Mutex<Vec<Str>>,
	}

	impl CredentialEnvironment for OrderedEnvironment {
		fn read(&self, name: &str) -> Result<Option<SecretString>, CredentialError> {
			self.calls.lock().push(name.into());
			Ok((name == "OMP_SECOND").then(|| SecretString::from("secret".to_owned())))
		}
	}

	#[tokio::test]
	async fn environment_names_are_tried_in_declared_order() {
		let spec = AuthSpecId::new("ordered");
		let environment = Arc::new(OrderedEnvironment { calls: Mutex::new(Vec::new()) });
		let broker = CredentialBroker {
			plans:       Arc::new(BTreeMap::from([(spec.clone(), BrokerPlan {
				kind:    CredentialKind::ApiKey,
				sources: vec![BrokerSource::Environment(
					vec![sf!("OMP_FIRST"), sf!("OMP_SECOND")].into_boxed_slice(),
				)]
				.into_boxed_slice(),
			})])),
			environment: environment.clone(),
			engines:     CredentialBrokerEngines::default(),
			invocation:  None,
		};
		let lease = broker
			.lease(CredentialNeed {
				spec,
				account: Some(AccountId::from("account")),
				principal: Some(PrincipalId::from("principal")),
				valid_after: SystemTime::UNIX_EPOCH,
			})
			.await
			.expect("second source");
		assert_eq!(lease.kind(), CredentialKind::ApiKey);
		assert_eq!(*environment.calls.lock(), vec![sf!("OMP_FIRST"), sf!("OMP_SECOND")]);
		assert!(!format!("{broker:?} {lease:?}").contains("secret"));
	}
}
