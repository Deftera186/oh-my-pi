//! Journal-derived environment-state restoration hooks.
//!
//! Some tool state lives in the project environment rather than the journal
//! (the todo slot executor, for example). Whenever durable history is
//! rewritten — rewind, reset — or a session resumes, that state must be
//! re-seeded from journal truth. Components implementing
//! [`StatefulComponent`] are registered on the agent by the composition
//! layer; the loop itself stays a generic hook surface with no per-feature
//! knowledge.

use std::{future::Future, pin::Pin};

use omp_env::EnvClient;

use crate::Journal;

/// Boxed restore future.
///
/// Restoration is a cold, environment-I/O-dominated path (at most once per
/// history rewrite or resume), so the one allocation at this `dyn` boundary
/// is sanctioned.
pub type RestoreFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// One environment-state slot whose live effects exist outside the journal
/// and must be re-seeded from journal truth.
///
/// The agent loop invokes every registered component after each history
/// rewrite (user rewind, retry, checkpoint rewind, `omp.agents.rewind`,
/// reset) and hosts invoke the same set once on session resume. Components
/// are best-effort: they log their own failures and never fail the loop.
pub trait StatefulComponent: Send + Sync {
	/// Stable component name for diagnostics.
	fn name(&self) -> &'static str;

	/// Recomputes journal-derived state and drives the environment to match.
	fn restore<'a>(&'a self, journal: &'a Journal, env: &'a EnvClient) -> RestoreFuture<'a>;
}
