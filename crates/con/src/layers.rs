//! Layered convar values and spawn seeds.

use omp_core::{FastHashMap, Str};
use serde::{Deserialize, Serialize};

use crate::Value;

/// Stable identity of an engagement layer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct LayerId(u64);

impl LayerId {
	/// Constructs an id from its wire value.
	#[must_use]
	pub const fn new(value: u64) -> Self {
		Self(value)
	}

	/// Returns the wire value.
	#[must_use]
	pub const fn get(self) -> u64 {
		self.0
	}
}

/// Provenance and destination of a convar write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Origin {
	/// Registration-time value.
	Default,
	/// Profile value loaded from `config.cfg`.
	Archive,
	/// Durable value in the session tree.
	Session,
	/// Command-stream value, committed to the session layer.
	Script(Str),
	/// Value supplied by an active director engagement.
	Engagement(LayerId),
	/// Trusted host write, committed to the session layer.
	Host,
}

/// Result of committing a convar write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetReport {
	/// Layer that received the write.
	pub committed_to: Origin,
	/// Innermost engagement which still shadows the committed value.
	pub shadowed_by:  Option<(LayerId, Str)>,
}

/// Values copied into a newly spawned child before cfg overrides.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seed {
	values: FastHashMap<Str, Value>,
}

impl Seed {
	/// Creates a seed from effective values.
	#[must_use]
	pub fn new(values: FastHashMap<Str, Value>) -> Self {
		Self { values }
	}

	/// Iterates seeded name/value pairs.
	pub fn iter(&self) -> impl Iterator<Item = (&Str, &Value)> {
		self.values.iter()
	}

	/// Returns a seeded value.
	#[must_use]
	pub fn get(&self, name: &str) -> Option<&Value> {
		self.values.get(name)
	}

	/// Consumes the seed.
	#[must_use]
	pub fn into_values(self) -> FastHashMap<Str, Value> {
		self.values
	}
}

#[derive(Clone, Debug)]
pub(crate) struct EngagementLayer {
	pub(crate) id:     LayerId,
	pub(crate) owner:  Str,
	pub(crate) values: FastHashMap<Str, Value>,
}

#[derive(Default)]
pub(crate) struct Layers {
	pub(crate) archive:     FastHashMap<Str, Value>,
	pub(crate) session:     FastHashMap<Str, Value>,
	pub(crate) engagements: Vec<EngagementLayer>,
	next_id:                u64,
}

impl Layers {
	pub(crate) fn push(&mut self, owner: Str, binds: &[(Str, Value)]) -> LayerId {
		self.next_id = self.next_id.saturating_add(1);
		let id = LayerId(self.next_id);
		let values = binds.iter().cloned().collect();
		self.engagements.push(EngagementLayer { id, owner, values });
		id
	}

	pub(crate) fn pop(&mut self, id: LayerId) -> Option<EngagementLayer> {
		let at = self.engagements.iter().position(|layer| layer.id == id)?;
		Some(self.engagements.remove(at))
	}

	pub(crate) fn engagement_value(&self, name: &str) -> Option<&Value> {
		self
			.engagements
			.iter()
			.rev()
			.find_map(|layer| layer.values.get(name))
	}

	pub(crate) fn shadow(&self, name: &str) -> Option<(LayerId, Str)> {
		self
			.engagements
			.iter()
			.rev()
			.find(|layer| layer.values.contains_key(name))
			.map(|layer| (layer.id, layer.owner.clone()))
	}
}
