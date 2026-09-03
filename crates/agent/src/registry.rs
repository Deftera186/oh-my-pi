//! Host-owned registry seam for routing between live session kernels.

use std::sync::Arc;

use omp_core::Str;
use omp_dom::Snapshot;
use parking_lot::RwLock;

use crate::Up;

/// Cloneable control and projection endpoint for one live session.
///
/// The host may cache this disposable endpoint, but durable identity and state
/// remain in the session journal and DOM.
#[derive(Clone)]
pub struct SessionEndpoint {
	/// Stable session identity.
	pub id:       Str,
	/// Human-readable session name.
	pub name:     Str,
	/// The kernel's sole upward control mailbox.
	pub up:       flume::Sender<Up>,
	/// Latest detached DOM snapshot published by the controller.
	pub snapshot: Arc<RwLock<Snapshot>>,
}

/// Read-only routing authority injected by the host composition.
///
/// Implementations are runtime indexes only. They must be rebuilt from live
/// sessions and must not become a second durable source of truth.
pub trait SessionAuthority: Send + Sync {
	/// Looks up a live session by stable id or name.
	fn lookup(&self, id_or_name: &str) -> Option<SessionEndpoint>;

	/// Lists all currently addressable live sessions.
	fn list(&self) -> Vec<SessionEndpoint>;
}
