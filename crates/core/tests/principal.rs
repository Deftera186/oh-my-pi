//! Contracts for authenticated principal and provenance identities.

use std::{
	collections::hash_map::DefaultHasher,
	hash::{Hash, Hasher},
	mem::size_of,
};

use omp_core::{ArtifactDigest, Provenance, sf};

fn provenance(generation: u64) -> Provenance {
	Provenance::new(
		sf!("publisher"),
		sf!("example.extension"),
		sf!("1.2.3"),
		ArtifactDigest::new([0xabu8; 32]),
		sf!("user"),
		sf!("trusted"),
		generation,
	)
}

#[test]
fn provenance_is_compact_and_serializes_as_its_fields() {
	let value = provenance(7);

	assert_eq!(size_of::<Provenance>(), 8);
	assert_eq!(
		serde_json::to_value(&value).expect("provenance serializes"),
		serde_json::json!({
			"publisher": "publisher",
			"extension_id": "example.extension",
			"version": "1.2.3",
			"artifact_digest": "sha256:abababababababababababababababababababababababababababababababab",
			"layer": "user",
			"tier": "trusted",
			"generation": 7,
		})
	);
	assert_eq!(
		serde_json::from_value::<Provenance>(
			serde_json::to_value(&value).expect("provenance serializes"),
		)
		.expect("provenance deserializes"),
		value
	);
}

#[test]
fn provenance_compares_and_hashes_by_value() {
	let value = provenance(7);
	let equal = provenance(7);
	let later = provenance(8);

	assert_eq!(value, equal);
	assert!(value < later);

	let mut value_hash = DefaultHasher::new();
	value.hash(&mut value_hash);
	let mut equal_hash = DefaultHasher::new();
	equal.hash(&mut equal_hash);
	assert_eq!(value_hash.finish(), equal_hash.finish());
}
