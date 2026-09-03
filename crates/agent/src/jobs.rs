//! Runtime supervision index rebuilt from the authoritative `<meta><jobs>`
//! tree.
//!
//! The board deliberately stores no durable job state.  Identities, kinds and
//! lifecycle status live in the session DOM; this module only connects those
//! elements to kill boundaries owned by the runtime.

use omp_core::{FastHashMap, Str};
use omp_dom::{Dom, Handle, KnownTag, PropId, PropKey, Tag, Value};
use omp_session::{LifecycleWork, Session};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};
use tokio_util::sync::CancellationToken;

/// The three execution shapes represented by the one job primitive.
#[derive(Clone, Copy, Debug, Deserialize, Display, EnumString, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum JobKind {
	/// A detached ordinary tool call.
	Tool,
	/// A child agent kernel.
	Subagent,
	/// A supervised process or daemon.
	Process,
}

/// Durable fields projected from one `<job>` or `<subagent>` element.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobRecord {
	/// Current DOM handle.
	pub handle:  Handle,
	/// Stable durable identity.
	pub id:      Str,
	/// Shared job kind.
	pub kind:    JobKind,
	/// Journal-derived lifecycle status.
	pub status:  Str,
	/// Owning session or agent identity, when present.
	pub owner:   Option<Str>,
	/// Start timestamp, when present.
	pub started: Option<Str>,
}

#[derive(Clone)]
struct RuntimeJob {
	record: JobRecord,
	cancel: CancellationToken,
}

/// A disposable runtime index over the authoritative jobs subtree.
///
/// Rebuilding preserves a live execution unit by durable `id`, remaps it to
/// the newly-derived handle, and cancels units absent from the new tree.
#[derive(Default)]
pub struct JobBoard {
	jobs: Mutex<FastHashMap<Handle, RuntimeJob>>,
}

impl JobBoard {
	/// Creates an empty runtime index.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Attaches the runtime kill boundary for a job already present in the DOM.
	/// Returns false when `handle` is not a lifecycle-bearing job element.
	pub fn attach(&self, dom: &Dom, handle: Handle, cancel: CancellationToken) -> bool {
		let Some(record) = record(dom, handle) else {
			return false;
		};
		self
			.jobs
			.lock()
			.insert(handle, RuntimeJob { record, cancel });
		true
	}

	/// Rebuilds the index after open or rewind. The DOM is the source of truth.
	pub fn rebuild(&self, session: &Session) {
		let records = records(session.dom());
		let mut jobs = self.jobs.lock();
		let mut by_id: FastHashMap<Str, RuntimeJob> = std::mem::take(&mut *jobs)
			.into_values()
			.map(|job| (job.record.id.clone(), job))
			.collect();
		for record in records {
			if let Some(mut job) = by_id.remove(&record.id) {
				job.record = record.clone();
				jobs.insert(record.handle, job);
			}
		}
		for job in by_id.into_values() {
			job.cancel.cancel();
		}
	}

	/// Applies lifecycle work returned by `Session::rewind`, then remaps
	/// retained handles. Removed executions are cancelled through their kill
	/// boundary.
	pub fn apply_lifecycle(&self, work: &LifecycleWork) {
		let mut jobs = self.jobs.lock();
		for handle in &work.terminate {
			if let Some(job) = jobs.remove(handle) {
				job.cancel.cancel();
			}
		}
		for (old, new) in &work.retained {
			if let Some(mut job) = jobs.remove(old) {
				job.record.handle = *new;
				jobs.insert(*new, job);
			}
		}
	}

	/// Terminates one execution unit by its current DOM handle.
	pub fn terminate(&self, handle: Handle) -> bool {
		let Some(job) = self.jobs.lock().remove(&handle) else {
			return false;
		};
		job.cancel.cancel();
		true
	}

	/// Returns the current DOM-derived roster.
	#[must_use]
	pub fn list(&self) -> Vec<JobRecord> {
		let mut records = self
			.jobs
			.lock()
			.values()
			.map(|job| job.record.clone())
			.collect::<Vec<_>>();
		records.sort_by(|left, right| left.id.cmp(&right.id));
		records
	}
}

fn records(dom: &Dom) -> Vec<JobRecord> {
	let Some(jobs) = dom.children(dom.meta()).iter().copied().find(|handle| {
		dom.get(*handle)
			.is_some_and(|node| node.tag == Tag::Known(KnownTag::Jobs))
	}) else {
		return Vec::new();
	};
	let mut out = Vec::new();
	collect(dom, jobs, &mut out);
	out
}

fn collect(dom: &Dom, parent: Handle, out: &mut Vec<JobRecord>) {
	for handle in dom.children(parent) {
		if let Some(record) = record(dom, *handle) {
			out.push(record);
		}
		collect(dom, *handle, out);
	}
}

fn record(dom: &Dom, handle: Handle) -> Option<JobRecord> {
	let node = dom.get(handle)?;
	let kind = match node.tag {
		Tag::Known(KnownTag::Job) => prop(node, PropId::Kind)
			.and_then(|value| value.parse().ok())
			.unwrap_or(JobKind::Tool),
		Tag::Known(KnownTag::Subagent) => JobKind::Subagent,
		_ => return None,
	};
	Some(JobRecord {
		handle,
		id: prop(node, PropId::Id)
			.map(Str::new)
			.unwrap_or_else(|| Str::new(handle.to_string())),
		kind,
		status: prop(node, PropId::Status)
			.map(Str::new)
			.unwrap_or_else(|| Str::new_static("running")),
		owner: custom(node, "owner").map(Str::new),
		started: custom(node, "started").map(Str::new),
	})
}

fn prop(node: &omp_dom::Node, id: PropId) -> Option<&str> {
	node.prop(&PropKey::from(id)).and_then(Value::as_str)
}

fn custom<'a>(node: &'a omp_dom::Node, key: &'static str) -> Option<&'a str> {
	node
		.prop(&PropKey::Custom(Str::new_static(key)))
		.and_then(Value::as_str)
}
