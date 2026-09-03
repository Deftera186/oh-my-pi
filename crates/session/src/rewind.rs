//! Lifecycle work derived from two authoritative DOM snapshots.

use omp_core::{FastHashMap, Str};
use omp_dom::{Handle, KnownTag, PropId, PropKey, Snapshot, Tag, Value};

/// Spawn and termination work implied by a session-tree transition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LifecycleWork {
	/// Removed `<subagent>` and `<job>` handles to terminate.
	pub terminate: Vec<Handle>,
	/// Added `<subagent>` and `<job>` handles to spawn or resume.
	pub spawn:     Vec<Handle>,
	/// Durable identities retained across re-derivation as `(old, new)` handles.
	pub retained:  Vec<(Handle, Handle)>,
}

/// Diffs lifecycle-bearing elements between two snapshots by durable identity.
///
/// Reminting a handle during re-derivation does not terminate and respawn the
/// underlying job or subagent. Such nodes appear in [`LifecycleWork::retained`]
/// instead. Nodes without `id` or `cause` use their handle as a last-resort
/// identity and therefore cannot be recognized across reminting.
#[must_use]
pub fn diff(before: &Snapshot, after: &Snapshot) -> LifecycleWork {
	let before_live = lifecycle_nodes(before);
	let mut after_live = lifecycle_nodes(after);
	let mut terminate = Vec::new();
	let mut retained = Vec::new();
	for (identity, old_handle) in before_live {
		if let Some(new_handle) = after_live.remove(&identity) {
			retained.push((old_handle, new_handle));
		} else {
			terminate.push(old_handle);
		}
	}
	let mut spawn: Vec<_> = after_live.into_values().collect();
	terminate.sort_unstable();
	spawn.sort_unstable();
	retained.sort_unstable();
	LifecycleWork { terminate, spawn, retained }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LifecycleId {
	tag: KnownTag,
	id:  Str,
}

fn lifecycle_nodes(snapshot: &Snapshot) -> FastHashMap<LifecycleId, Handle> {
	snapshot
		.handles()
		.filter_map(|handle| {
			let node = snapshot.get(handle)?;
			let tag = match node.tag {
				Tag::Known(tag @ (KnownTag::Subagent | KnownTag::Job)) => tag,
				_ => return None,
			};
			let id = node
				.prop(&PropKey::from(PropId::Id))
				.or_else(|| node.prop(&PropKey::from(PropId::Cause)))
				.and_then(Value::as_str)
				.map(Str::new)
				.unwrap_or_else(|| Str::new(handle.to_string()));
			Some((LifecycleId { tag, id }, handle))
		})
		.collect()
}
