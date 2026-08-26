//! Proves gateway provider mutations reject stale catalogs and incomplete
//! caller identities.

use std::sync::Arc;

use omp_catalog::snapshot;
use omp_driver::{
	discovery::runtime::gateway_provider_rpc_authority,
	model_controls::ProductionProviderApplicationOwner,
};
use omp_inference::layer::stack::{BuiltinConfig, RouteComposer, RouteProviderService};
use omp_proto::inference::v1 as pb;
use omp_storage::blob::BlobStore;
use tonic::Code;

struct UnusedComposer;

impl RouteComposer for UnusedComposer {
	fn compose(
		&self,
		_catalog: &snapshot::Catalog,
		_route: &omp_catalog::RouteDef,
	) -> Result<RouteProviderService, omp_inference::RouteUnavailable> {
		panic!("stale and unauthenticated requests never compose a route")
	}
}

fn authority() -> Arc<dyn omp_serve::inference::ProviderGatewayAuthority> {
	let catalog = Arc::new(snapshot::Catalog::embedded().clone());
	let registry = omp_inference::Registry::builder(catalog).build_catalog_projection();
	let blobs = BlobStore::open(tempfile::tempdir().expect("temporary blob root").keep())
		.expect("blob store");
	gateway_provider_rpc_authority(Arc::new(ProductionProviderApplicationOwner::new(
		registry,
		BuiltinConfig::new(Arc::new(UnusedComposer)),
		blobs,
	)))
}

#[tokio::test]
async fn provider_mutations_refuse_stale_catalog_and_incomplete_identity() {
	let authority = authority();
	let catalog = authority
		.catalog(pb::ProviderCatalogRequest { provider: None })
		.await
		.expect("authoritative catalog");
	let generation = catalog.cursor.expect("catalog cursor").generation;

	let stale = authority
		.declare(pb::ProviderDeclarationRequest {
			caller:              Some(pb::ProviderCaller {
				extension:          "dev.example.provider".into(),
				artifact_digest:    "sha256:fixture".into(),
				host_generation:    7,
				session_generation: 11,
				principal_id:       "principal".into(),
				principal_display:  "Principal".into(),
				layer:              "user".into(),
				tier:               "trusted".into(),
				trust:              "trusted".into(),
				capabilities:       vec!["provider".into()],
			}),
			provider:            "acme".into(),
			document_json:       b"{}".to_vec().into(),
			expected_generation: generation.saturating_add(1),
		})
		.await
		.expect_err("stale generation must be refused");
	assert_eq!(stale.code(), Code::Aborted);

	let unauthenticated = authority
		.retract(pb::RetractProviderRequest {
			caller:              Some(pb::ProviderCaller {
				extension:          "dev.example.provider".into(),
				artifact_digest:    String::new(),
				host_generation:    7,
				session_generation: 11,
				principal_id:       "principal".into(),
				principal_display:  "Principal".into(),
				layer:              "user".into(),
				tier:               "trusted".into(),
				trust:              "trusted".into(),
				capabilities:       vec!["provider".into()],
			}),
			provider:            "acme".into(),
			expected_generation: generation,
		})
		.await
		.expect_err("incomplete authenticated identity must be refused");
	assert_eq!(unauthenticated.code(), Code::Unauthenticated);

	let unchanged = authority
		.catalog(pb::ProviderCatalogRequest { provider: None })
		.await
		.expect("catalog remains readable")
		.cursor
		.expect("catalog cursor")
		.generation;
	assert_eq!(unchanged, generation);
}
