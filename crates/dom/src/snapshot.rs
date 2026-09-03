use serde::Serialize;

use crate::{Handle, Node, Sid, Value, stream::SnapshotStream};

#[derive(Serialize)]
struct Canonical<'a> {
	high_water: u64,
	next_sid:   Sid,
	nodes:      &'a [CanonicalNode],
	streams:    &'a [SnapshotStream],
}

#[derive(Serialize)]
struct CanonicalNode {
	handle: Handle,
	parent: Option<Handle>,
	node:   Node,
}

/// Deterministic, self-contained image of a materialized session tree.
///
/// Equality compares canonical bytes, including the handle high-water mark.
#[derive(Clone, Debug)]
pub struct Snapshot {
	pub(crate) high_water: u64,
	pub(crate) next_sid:   Sid,
	pub(crate) nodes:      Vec<Option<Node>>,
	pub(crate) parents:    Vec<Option<Handle>>,
	pub(crate) streams:    Vec<SnapshotStream>,
	bytes:                 Vec<u8>,
}

impl Snapshot {
	pub(crate) fn build(
		high_water: u64,
		next_sid: Sid,
		mut nodes: Vec<Option<Node>>,
		parents: Vec<Option<Handle>>,
		streams: Vec<SnapshotStream>,
	) -> Self {
		for stream in &streams {
			if let Some(node) = slot_mut(&mut nodes, stream.node) {
				node.set_prop(stream.prop.clone(), Value::Str(stream.text.clone()));
			}
		}
		let mut canonical_nodes = Vec::new();
		for raw in 1..=high_water {
			let Some(handle) = Handle::new(raw) else {
				continue;
			};
			let Some(mut node) = slot(&nodes, handle).cloned() else {
				continue;
			};
			node.props.sort_by(|left, right| left.0.cmp(&right.0));
			canonical_nodes.push(CanonicalNode {
				handle,
				parent: parents.get(raw as usize).copied().flatten(),
				node,
			});
		}
		let bytes = serde_json::to_vec(&Canonical {
			high_water,
			next_sid,
			nodes: &canonical_nodes,
			streams: &streams,
		})
		.expect("DOM snapshot values are always JSON serializable");
		Self { high_water, next_sid, nodes, parents, streams, bytes }
	}

	/// Returns the canonical serialized bytes.
	#[must_use]
	pub fn as_bytes(&self) -> &[u8] {
		&self.bytes
	}

	/// Consumes the snapshot and returns its canonical bytes.
	#[must_use]
	pub fn into_bytes(self) -> Vec<u8> {
		self.bytes
	}

	/// Returns the largest handle ever minted.
	#[must_use]
	pub const fn high_water(&self) -> u64 {
		self.high_water
	}

	/// Returns the next stream id that will be allocated.
	#[must_use]
	pub const fn next_sid(&self) -> Sid {
		self.next_sid
	}

	/// Looks up a materialized node.
	#[must_use]
	pub fn get(&self, handle: Handle) -> Option<&Node> {
		slot(&self.nodes, handle)
	}

	/// Returns a node's children, or an empty slice for an unknown handle.
	#[must_use]
	pub fn children(&self, handle: Handle) -> &[Handle] {
		self.get(handle).map_or(&[], |node| node.kids.as_slice())
	}

	/// Returns a node's parent.
	#[must_use]
	pub fn parent(&self, handle: Handle) -> Option<Handle> {
		self.parents.get(handle.get() as usize).copied().flatten()
	}

	/// Iterates live handles in numeric order.
	pub fn handles(&self) -> impl Iterator<Item = Handle> + '_ {
		self
			.nodes
			.iter()
			.enumerate()
			.skip(1)
			.filter_map(|(index, node)| node.as_ref().and_then(|_| Handle::new(index as u64)))
	}
}

impl PartialEq for Snapshot {
	fn eq(&self, other: &Self) -> bool {
		self.bytes == other.bytes
	}
}

impl Eq for Snapshot {}

pub(crate) fn slot(nodes: &[Option<Node>], handle: Handle) -> Option<&Node> {
	nodes.get(handle.get() as usize)?.as_ref()
}

pub(crate) fn slot_mut(nodes: &mut [Option<Node>], handle: Handle) -> Option<&mut Node> {
	nodes.get_mut(handle.get() as usize)?.as_mut()
}
