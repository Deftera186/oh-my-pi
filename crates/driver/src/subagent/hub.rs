//! Session-owned `hub@1` operations over live kernel mailboxes and the DOM.

use omp_agent::{JobBoard, SessionAuthority, SessionTool, SessionToolCx, SessionToolFuture, Up};
use omp_core::{Str, sf};
use omp_dom::{KnownTag, Op, Tag, Txn};
use omp_tool::{CallOutcome, ToolSpec};
use omp_tools::hub::{Fault, HubBackend, Op as HubOp, Params, Request, Response};

/// Declaration-only backend; kernel session routing intercepts every call.
pub struct HubDeclarationBackend;

impl HubBackend for HubDeclarationBackend {
	async fn execute<'a>(
		&'a self,
		_caller_id: &'a str,
		_request: Request,
		_updates: &'a flume::Sender<Response>,
	) -> Result<Response, Fault> {
		Err(Fault { message: sf!("hub session dispatcher is unavailable") })
	}
}

/// Stateless host operations shared by the model-facing hub tool and native
/// embeddings.
pub struct SessionHub;

impl SessionHub {
	/// Sends one steering item through the target kernel mailbox.
	pub fn send(
		authority: &dyn SessionAuthority,
		to: &str,
		message: Str,
	) -> Result<Response, omp_agent::SessionToolError> {
		send_to(authority, to, message)
	}

	/// Reads or drains the caller's journal-backed steering inbox.
	pub fn inbox(
		session: &mut omp_session::Session,
		peek: bool,
	) -> Result<Response, omp_agent::SessionToolError> {
		inbox(session, peek)
	}
}

/// Session-authority hub implementation.
pub struct HubSessionTool {
	spec: ToolSpec,
}

impl HubSessionTool {
	/// Creates the canonical session hub.
	#[must_use]
	pub fn new() -> Self {
		Self { spec: omp_tools::hub::spec() }
	}
}

impl Default for HubSessionTool {
	fn default() -> Self {
		Self::new()
	}
}

impl SessionTool for HubSessionTool {
	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'a>(
		&'a self,
		cx: SessionToolCx<'a>,
		args: Box<serde_json::value::RawValue>,
	) -> SessionToolFuture<'a> {
		Box::pin(async move {
			let mut value: serde_json::Value = serde_json::from_str(args.get())?;
			if let Some(object) = value.as_object_mut() {
				object.remove("i");
			}
			let params: Params = serde_json::from_value(value)?;
			cx.jobs.rebuild(cx.session);
			let response = match params.op {
				HubOp::Send => send(cx.authority, &params)?,
				HubOp::Inbox => inbox(cx.session, params.peek)?,
				HubOp::Wait => inbox(cx.session, params.peek)?,
				HubOp::List => list(cx.authority, params.limit)?,
				HubOp::Jobs | HubOp::Ps => roster(cx.jobs)?,
				HubOp::Cancel => cancel(cx.jobs, params.ids.as_deref().unwrap_or_default())?,
				HubOp::Start | HubOp::Logs | HubOp::Stop | HubOp::Restart | HubOp::Describe => {
					return typed_fault("named process authority is not attached");
				},
			};
			let payload = serde_json::value::to_raw_value(&response)?;
			Ok(CallOutcome::Ok(payload))
		})
	}
}

fn typed_fault(
	message: &'static str,
) -> Result<
	CallOutcome<Box<serde_json::value::RawValue>, Box<serde_json::value::RawValue>>,
	omp_agent::SessionToolError,
> {
	let fault = serde_json::value::to_raw_value(&Fault { message: Str::new_static(message) })?;
	Ok(CallOutcome::Faulted(fault))
}

fn send(
	authority: Option<&dyn SessionAuthority>,
	params: &Params,
) -> Result<Response, omp_agent::SessionToolError> {
	let authority = authority.ok_or_else(|| omp_agent::SessionToolError::Rejected {
		message: Str::new_static("live session authority is not attached"),
	})?;
	let target = params
		.to
		.as_deref()
		.ok_or_else(|| omp_agent::SessionToolError::Rejected {
			message: Str::new_static("hub send requires `to`"),
		})?;
	let message = params
		.message
		.clone()
		.ok_or_else(|| omp_agent::SessionToolError::Rejected {
			message: Str::new_static("hub send requires `message`"),
		})?;
	send_to(authority, target, message)
}

fn send_to(
	authority: &dyn SessionAuthority,
	target: &str,
	message: Str,
) -> Result<Response, omp_agent::SessionToolError> {
	let delivered = if target == "all" {
		authority
			.list()
			.into_iter()
			.filter(|endpoint| endpoint.up.send(Up::Steer(message.clone())).is_ok())
			.count()
	} else {
		usize::from(
			authority
				.lookup(target)
				.is_some_and(|endpoint| endpoint.up.send(Up::Steer(message)).is_ok()),
		)
	};
	if delivered == 0 {
		return Err(omp_agent::SessionToolError::Rejected {
			message: Str::new_static("target session is not live"),
		});
	}
	Ok(Response {
		text:    Str::new(serde_json::json!({ "delivered": delivered }).to_string()),
		useless: false,
	})
}

fn list(
	authority: Option<&dyn SessionAuthority>,
	limit: Option<u16>,
) -> Result<Response, omp_agent::SessionToolError> {
	let authority = authority.ok_or_else(|| omp_agent::SessionToolError::Rejected {
		message: Str::new_static("live session authority is not attached"),
	})?;
	let limit = usize::from(limit.unwrap_or(omp_tools::hub::DEFAULT_LIST_LIMIT as u16))
		.min(omp_tools::hub::MAX_LIST_LIMIT);
	let rows = authority
		.list()
		.into_iter()
		.take(limit)
		.map(|endpoint| serde_json::json!({ "id": endpoint.id, "name": endpoint.name }))
		.collect::<Vec<_>>();
	let useless = rows.is_empty();
	Ok(Response { text: Str::new(serde_json::json!({ "sessions": rows }).to_string()), useless })
}

fn inbox(
	session: &mut omp_session::Session,
	peek: bool,
) -> Result<Response, omp_agent::SessionToolError> {
	let steering = session
		.dom()
		.children(session.dom().queues())
		.iter()
		.copied()
		.find(|handle| {
			session
				.dom()
				.get(*handle)
				.is_some_and(|node| node.tag == Tag::Known(KnownTag::Steering))
		})
		.ok_or_else(|| omp_agent::SessionToolError::Rejected {
			message: Str::new_static("session steering queue is absent"),
		})?;
	let messages = session
		.dom()
		.children(steering)
		.iter()
		.filter_map(|handle| session.dom().get(*handle)?.content.clone())
		.collect::<Vec<_>>();
	let useless = messages.is_empty();
	if !peek && !useless {
		let cause = session
			.head()
			.ok_or_else(|| omp_agent::SessionToolError::Rejected {
				message: Str::new_static("session has no journal head"),
			})?;
		let ops = session
			.dom()
			.children(steering)
			.iter()
			.copied()
			.map(Op::Rm)
			.collect();
		session
			.patch(Txn { cause, label: Some(Str::new_static("hub.inbox")), ops })
			.map_err(|_| omp_agent::SessionToolError::Rejected {
				message: Str::new_static("failed to journal inbox drain"),
			})?;
	}
	Ok(Response { text: Str::new(serde_json::json!({ "messages": messages }).to_string()), useless })
}

fn roster(jobs: &JobBoard) -> Result<Response, omp_agent::SessionToolError> {
	let rows = jobs
		.list()
		.into_iter()
		.map(|job| {
			serde_json::json!({
				"id": job.id,
				"kind": job.kind.to_string(),
				"status": job.status,
				"owner": job.owner,
				"started": job.started,
			})
		})
		.collect::<Vec<_>>();
	let useless = rows.is_empty();
	Ok(Response { text: Str::new(serde_json::json!({ "jobs": rows }).to_string()), useless })
}

fn cancel(jobs: &JobBoard, ids: &[Str]) -> Result<Response, omp_agent::SessionToolError> {
	let handles = jobs
		.list()
		.into_iter()
		.filter(|job| ids.contains(&job.id))
		.map(|job| job.handle)
		.collect::<Vec<_>>();
	let cancelled = handles
		.into_iter()
		.filter(|handle| jobs.terminate(*handle))
		.count();
	Ok(Response {
		text:    Str::new(serde_json::json!({ "cancelled": cancelled }).to_string()),
		useless: false,
	})
}
