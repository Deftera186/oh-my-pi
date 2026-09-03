use omp_core::Str;
use omp_dom::{Dom, Handle, KnownTag, NodeSpec, Op, PropId, PropKey, Tag, Txn, Value};
use omp_journal::{Entry, EntryId, Kind, data::ToolResult, kind};

use crate::{Component, Draft};

/// Durable fields used to insert one member of the shared job primitive.
#[derive(Clone, Debug)]
pub struct JobSpec {
	/// Stable job identity.
	pub id:      Str,
	/// `tool`, `subagent`, or `process`.
	pub kind:    Str,
	/// Runtime owner identity.
	pub owner:   Str,
	/// Start timestamp represented by the caller.
	pub started: Str,
	/// Agent class for a subagent job.
	pub agent:   Option<Str>,
}

/// Builds the journalled patch that inserts a job under `<meta><jobs>`.
///
/// Subagents use the semantic `<subagent>` tag; every other kind uses `<job>`.
/// The returned transaction must be committed through
/// [`crate::Session::patch`].
pub fn insert(dom: &Dom, cause: EntryId, spec: JobSpec) -> Option<Txn> {
	let jobs = jobs_handle(dom)?;
	let tag = if spec.kind.as_str() == "subagent" {
		KnownTag::Subagent
	} else {
		KnownTag::Job
	};
	let mut node = NodeSpec::new(tag)
		.with_prop(PropId::Id, Value::Str(spec.id))
		.with_prop(PropId::Kind, Value::Str(spec.kind))
		.with_prop(PropId::Status, Value::Str(Str::new_static("running")))
		.with_prop(PropKey::Custom(Str::new_static("owner")), Value::Str(spec.owner))
		.with_prop(PropKey::Custom(Str::new_static("started")), Value::Str(spec.started));
	if let Some(agent) = spec.agent {
		node = node.with_prop(PropKey::Custom(Str::new_static("agent")), Value::Str(agent));
	}
	Some(Txn {
		cause,
		label: Some(Str::new_static("jobs.insert")),
		ops: vec![Op::Ins { parent: jobs, after: dom.children(jobs).last().copied(), node }],
	})
}

/// Builds a status update for one DOM job handle.
#[must_use]
pub fn set_status(cause: EntryId, handle: Handle, status: impl Into<Str>) -> Txn {
	Txn {
		cause,
		label: Some(Str::new_static("jobs.status")),
		ops: vec![Op::Set {
			h:     handle,
			prop:  PropId::Status.into(),
			value: Value::Str(status.into()),
		}],
	}
}

/// Finds the authoritative jobs component root.
#[must_use]
pub fn jobs_handle(dom: &Dom) -> Option<Handle> {
	find_tag(dom, dom.meta(), KnownTag::Jobs)
}

/// Projects detached tool terminals into `<meta><jobs><job>` elements.
pub struct JobsComponent;

impl Component for JobsComponent {
	fn interested(&self, kind: &Kind) -> bool {
		kind.rev == 1 && kind.name.as_str() == kind::TOOL_RESULT
	}

	fn apply(&mut self, entry: &Entry, dom: &Dom, draft: &mut Draft) {
		let Ok(ToolResult::Outcome { outcome, .. }) = serde_json::from_str(entry.data.as_str())
		else {
			return;
		};
		let Ok(value) = serde_json::from_str::<serde_json::Value>(outcome.get()) else {
			return;
		};
		let detached = value
			.get("kind")
			.and_then(serde_json::Value::as_str)
			.is_some_and(|kind| kind == "detached")
			.then_some(&value)
			.or_else(|| value.get("detached"));
		let Some(detached) = detached else { return };
		let Some(jobs) = jobs_handle(dom) else {
			return;
		};
		let id = detached
			.get("id")
			.and_then(serde_json::Value::as_str)
			.map(Str::new)
			.unwrap_or_else(|| Str::new(entry.id.to_string()));
		let Ok(raw) = serde_json::value::to_raw_value(detached) else {
			return;
		};
		let node = NodeSpec::new(KnownTag::Job)
			.with_prop(PropId::Id, Value::Str(id))
			.with_prop(PropId::Kind, Value::Str(Str::new_static("tool")))
			.with_prop(PropId::Status, Value::Str(Str::new_static("running")))
			.with_prop(PropId::Cause, Value::Str(Str::new(entry.id.to_string())))
			.with_prop(PropId::Data, Value::Json(raw));
		let after = dom.children(jobs).last().copied();
		draft.insert(jobs, after, node);
	}
}

fn find_tag(dom: &Dom, parent: Handle, tag: KnownTag) -> Option<Handle> {
	dom.children(parent).iter().copied().find(|handle| {
		dom.get(*handle)
			.is_some_and(|node| node.tag == Tag::Known(tag))
	})
}
