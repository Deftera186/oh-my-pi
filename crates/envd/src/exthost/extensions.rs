//! Python worker adapters for the engine Director and Component surfaces.

use std::{
	str::FromStr as _,
	sync::Arc,
	time::{Duration, Instant},
};

use omp_agent::{
	BindValue, BoxFut, Director, DirectorCx, DirectorEffect, DirectorError, ExtensionRegistrar,
	LiveComponent, LiveComponentError, MutDirectorCx, Prepared, Slot, StateUpdate, TurnView,
	Verdict,
};
use omp_core::{Str, sf};
use omp_dom::{Node, Op, Txn};
use omp_journal::{Entry, Kind};
use omp_session::{Component, Draft, SessionError};
use serde_json::{Map, Value as JsonValue, json};
use thiserror::Error;

use super::{
	CallbackConcurrency, EventDeadline,
	control::{ControlDispatch, ControlHandle, ControlInvocationAuthority},
};

/// Default bounded callback wait; composition may inject `sv_ext_hook_timeout`.
pub const DEFAULT_EXTENSION_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// A Python callback could not produce a valid engine registration result.
#[derive(Debug, Error)]
pub enum PyExtensionError {
	/// The extension callback timed out.
	#[error("Python extension callback timed out")]
	Timeout,
	/// No Tokio runtime was available to drive the CONTROL dispatch.
	#[error("Python extension callback has no runtime")]
	NoRuntime,
	/// The worker CONTROL dispatch failed.
	#[error(transparent)]
	Control(#[from] super::control::ControlRuntimeError),
	/// The callback returned an invalid result shape.
	#[error("Python extension callback returned an invalid result")]
	InvalidResult,
	/// A returned DOM patch was malformed.
	#[error("Python extension callback returned malformed DOM operations")]
	InvalidOps(#[source] serde_json::Error),
	/// Journaling the callback result failed.
	#[error(transparent)]
	Session(#[from] SessionError),
}

#[derive(Clone)]
struct PyCallback {
	control:   ControlHandle,
	authority: ControlInvocationAuthority,
	callable:  Str,
	timeout:   Duration,
}

impl PyCallback {
	fn dispatch(
		&self,
		operation: &'static str,
		mut arguments: Map<String, JsonValue>,
	) -> ControlDispatch {
		arguments.insert("callable".into(), JsonValue::String(self.callable.to_string()));
		ControlDispatch {
			operation: Str::new_static(operation),
			arguments,
			authority: self.authority.clone(),
			policy: CallbackConcurrency::Serialized,
			deadline: EventDeadline { at: Instant::now() + self.timeout },
		}
	}

	async fn call_async(
		&self,
		operation: &'static str,
		arguments: Map<String, JsonValue>,
	) -> Result<JsonValue, PyExtensionError> {
		Ok(self
			.control
			.dispatch(self.dispatch(operation, arguments))
			.await?)
	}

	fn call_sync(
		&self,
		operation: &'static str,
		arguments: Map<String, JsonValue>,
	) -> Result<JsonValue, PyExtensionError> {
		let runtime =
			tokio::runtime::Handle::try_current().map_err(|_| PyExtensionError::NoRuntime)?;
		let control = self.control.clone();
		let dispatch = self.dispatch(operation, arguments);
		let (tx, rx) = flume::bounded(1);
		std::thread::spawn(move || {
			let _ = tx.send(runtime.block_on(control.dispatch(dispatch)));
		});
		let result = rx
			.recv_timeout(self.timeout)
			.map_err(|_| PyExtensionError::Timeout)?;
		Ok(result?)
	}
}

/// Director backed by one callable in a killable Python extension worker.
///
/// The adapter retains only worker routing metadata. Durable callback state is
/// returned as `StateUpdate`s and committed on the Director element.
pub struct PyDirector {
	id:       Str,
	callback: PyCallback,
	claims:   Vec<Slot>,
	binds:    Vec<(Str, BindValue)>,
}

impl PyDirector {
	/// Creates an admitted Python Director.
	pub fn new(
		id: Str,
		callable: Str,
		control: ControlHandle,
		authority: ControlInvocationAuthority,
		claims: Vec<Slot>,
		binds: Vec<(Str, BindValue)>,
		timeout: Option<Duration>,
	) -> Self {
		Self {
			id,
			callback: PyCallback {
				control,
				authority,
				callable,
				timeout: timeout.unwrap_or(DEFAULT_EXTENSION_HOOK_TIMEOUT),
			},
			claims,
			binds,
		}
	}

	fn child(&self, result: &JsonValue) -> Option<Self> {
		let child = result.get("child")?.as_object()?;
		let id = required_str(child, "id").ok()?;
		let callable = callable_id(child).ok()?;
		let claims = child
			.get("claims")
			.and_then(JsonValue::as_array)
			.into_iter()
			.flatten()
			.map(|value| Slot::from_str(value.as_str()?).ok())
			.collect::<Option<Vec<_>>>()?;
		let binds = child
			.get("binds")
			.and_then(JsonValue::as_object)
			.into_iter()
			.flat_map(|values| values.iter())
			.map(|(name, value)| Some((Str::new(name), bind_value(value)?)))
			.collect::<Option<Vec<_>>>()?;
		Some(Self {
			id,
			callback: PyCallback {
				control: self.callback.control.clone(),
				authority: self.callback.authority.clone(),
				callable,
				timeout: self.callback.timeout,
			},
			claims,
			binds,
		})
	}
}

impl Director for PyDirector {
	fn id(&self) -> &str {
		self.id.as_str()
	}

	fn claims(&self) -> &[Slot] {
		&self.claims
	}

	fn binds(&self) -> &[(Str, BindValue)] {
		&self.binds
	}

	fn before_inference<'a>(
		&'a self,
		cx: &'a mut MutDirectorCx<'_>,
		req: &'a omp_inference::ChatRequest,
	) -> BoxFut<'a, Result<Prepared, DirectorError>> {
		Box::pin(async move {
			let mut arguments = Map::new();
			arguments.insert("director".into(), JsonValue::String(self.id.to_string()));
			arguments.insert("state".into(), director_state(cx.director_node()));
			arguments.insert(
				"request".into(),
				json!({
					"message_count": req.messages.len(),
					"tool_count": req.tools.len(),
					"max_output_tokens": req.max_output_tokens,
				}),
			);
			let result = self
				.callback
				.call_async("omp.extensions.director.before_inference", arguments)
				.await
				.map_err(|_| DirectorError::ExtensionCallback)?;
			if let Some(ops) = result.get("ops") {
				let ops: Vec<Op> =
					serde_json::from_value(ops.clone()).map_err(|_| DirectorError::ExtensionCallback)?;
				if !ops.is_empty() {
					let cause = cx.session.head().ok_or(DirectorError::MissingDirectors)?;
					cx.session.patch(Txn {
						cause,
						label: Some(sf!("extension.director.before_inference")),
						ops,
					})?;
				}
			}
			Ok(if result.get("prepared").and_then(JsonValue::as_str) == Some("rebuild") {
				Prepared::Rebuild
			} else {
				Prepared::Unchanged
			})
		})
	}

	fn evaluate(&self, _: &omp_dom::Dom, cx: &DirectorCx<'_>, turn: &TurnView) -> DirectorEffect {
		let arguments = json!({
			"director": self.id.as_str(),
			"state": director_state(cx.director_node()),
			"turn": {
				"had_tool_calls": turn.had_tool_calls,
				"assistant_text": turn.assistant_text.as_str(),
				"stop_reason": turn.stop_reason.as_str(),
			}
		})
		.as_object()
		.cloned()
		.unwrap_or_default();
		let Ok(result) = self
			.callback
			.call_sync("omp.extensions.director.on_yield", arguments)
		else {
			return DirectorEffect::new(Verdict::Fail(sf!(
				"Python extension Director callback failed"
			)));
		};
		let updates = result
			.get("updates")
			.and_then(JsonValue::as_object)
			.map(|updates| {
				updates
					.iter()
					.filter_map(|(key, value)| {
						bind_value(value).map(|value| StateUpdate::new(Str::new(key), value))
					})
					.collect()
			})
			.unwrap_or_default();
		let verdict = match result.get("verdict").and_then(JsonValue::as_str) {
			Some("pass") => Verdict::Pass,
			Some("continue") => Verdict::Continue {
				reminder: result
					.get("reminder")
					.and_then(JsonValue::as_str)
					.map(Str::new),
			},
			Some("yield") => Verdict::Yield,
			Some("done") => Verdict::Done,
			Some("push") => self.child(&result).map_or_else(
				|| Verdict::Fail(sf!("Python extension Director returned an invalid child")),
				|child| Verdict::Push(Box::new(child)),
			),
			Some("fail") => Verdict::Fail(
				result
					.get("reason")
					.and_then(JsonValue::as_str)
					.map_or_else(|| sf!("Python extension Director failed"), Str::new),
			),
			_ => Verdict::Fail(sf!("Python extension Director returned an invalid verdict")),
		};
		DirectorEffect { verdict, updates, asides: Vec::new() }
	}
}

/// Live Python Component adapter.
///
/// `reduce_live` invokes Python once and journals the returned operations as
/// `patch@1`. The `Component` implementation intentionally consumes no replay
/// entries: replay applies that patch directly and never calls Python again.
#[derive(Clone)]
pub struct PyComponent {
	id:         Str,
	callback:   PyCallback,
	interested: Arc<[Kind]>,
}

impl PyComponent {
	/// Creates an admitted journal-to-DOM Component adapter.
	pub fn new(
		id: Str,
		callable: Str,
		control: ControlHandle,
		authority: ControlInvocationAuthority,
		interested: Vec<Kind>,
		timeout: Option<Duration>,
	) -> Self {
		Self {
			id,
			callback: PyCallback {
				control,
				authority,
				callable,
				timeout: timeout.unwrap_or(DEFAULT_EXTENSION_HOOK_TIMEOUT),
			},
			interested: interested.into(),
		}
	}

	fn reduce_ops(&self, entry: &Entry) -> Result<Vec<Op>, PyExtensionError> {
		let arguments = json!({
			"component": self.id.as_str(),
			"entry": {
				"id": entry.id.to_string(),
				"kind": entry.kind.name.as_str(),
				"rev": entry.kind.rev,
				"by": entry.by.map(|id| id.to_string()),
				"prior": entry.prior.map(|id| id.to_string()),
				"label": entry.label.as_deref(),
				"data": entry.data.as_str(),
			},
		})
		.as_object()
		.cloned()
		.unwrap_or_default();
		let result = self
			.callback
			.call_sync("omp.extensions.component.apply", arguments)?;
		serde_json::from_value::<Vec<Op>>(result.get("ops").cloned().unwrap_or_else(|| json!([])))
			.map_err(PyExtensionError::InvalidOps)
	}
}

impl Component for PyComponent {
	fn interested(&self, _: &Kind) -> bool {
		false
	}

	fn apply(&mut self, _: &Entry, _: &omp_dom::Dom, _: &mut Draft) {}
}

impl LiveComponent for PyComponent {
	fn id(&self) -> &str {
		self.id.as_str()
	}

	fn interested(&self, kind: &Kind) -> bool {
		self.interested.iter().any(|candidate| candidate == kind)
	}

	fn reduce(&mut self, entry: &Entry, _: &omp_dom::Dom) -> Result<Vec<Op>, LiveComponentError> {
		self
			.reduce_ops(entry)
			.map_err(|_| LiveComponentError::Callback)
	}
}

/// Lowers frozen Python registry metadata into engine registrations.
///
/// The returned components are the live invocation handles. Register clones in
/// `ExtensionRegistrar` consume no replay entries; callers invoke
/// [`PyComponent::reduce_live`] at the journal append boundary.
pub fn register_python_extensions(
	registrar: &mut ExtensionRegistrar,
	directors: &[JsonValue],
	components: &[JsonValue],
	control: ControlHandle,
	authority: ControlInvocationAuthority,
	timeout: Option<Duration>,
) -> Result<Vec<PyComponent>, PyExtensionError> {
	for row in directors {
		let row = row.as_object().ok_or(PyExtensionError::InvalidResult)?;
		let id = required_str(row, "id")?;
		let callable = callable_id(row)?;
		let claims = row
			.get("claims")
			.and_then(JsonValue::as_array)
			.ok_or(PyExtensionError::InvalidResult)?
			.iter()
			.map(|value| {
				value
					.as_str()
					.ok_or(PyExtensionError::InvalidResult)
					.and_then(|value| Slot::from_str(value).map_err(|_| PyExtensionError::InvalidResult))
			})
			.collect::<Result<Vec<_>, _>>()?;
		let binds = row
			.get("binds")
			.and_then(JsonValue::as_object)
			.ok_or(PyExtensionError::InvalidResult)?
			.iter()
			.map(|(name, value)| {
				bind_value(value)
					.map(|value| (Str::new(name), value))
					.ok_or(PyExtensionError::InvalidResult)
			})
			.collect::<Result<Vec<_>, _>>()?;
		registrar.director(Box::new(PyDirector::new(
			id,
			callable,
			control.clone(),
			authority.clone(),
			claims,
			binds,
			timeout,
		)));
	}
	let mut live = Vec::with_capacity(components.len());
	for row in components {
		let row = row.as_object().ok_or(PyExtensionError::InvalidResult)?;
		let id = required_str(row, "id")?;
		let callable = callable_id(row)?;
		let interested = row
			.get("interested")
			.and_then(JsonValue::as_array)
			.ok_or(PyExtensionError::InvalidResult)?
			.iter()
			.map(|value| {
				let value = value.as_str().ok_or(PyExtensionError::InvalidResult)?;
				let (name, rev) = value
					.rsplit_once('@')
					.ok_or(PyExtensionError::InvalidResult)?;
				let rev = rev
					.parse::<u32>()
					.map_err(|_| PyExtensionError::InvalidResult)?;
				let kind = Kind::new(name, rev).map_err(|_| PyExtensionError::InvalidResult)?;
				kind
					.is_known()
					.then_some(kind)
					.ok_or(PyExtensionError::InvalidResult)
			})
			.collect::<Result<Vec<_>, _>>()?;
		let component =
			PyComponent::new(id, callable, control.clone(), authority.clone(), interested, timeout);
		registrar.component(Box::new(component.clone()));
		live.push(component);
	}
	Ok(live)
}

fn required_str(row: &Map<String, JsonValue>, key: &str) -> Result<Str, PyExtensionError> {
	row.get(key)
		.and_then(JsonValue::as_str)
		.filter(|value| !value.is_empty())
		.map(Str::new)
		.ok_or(PyExtensionError::InvalidResult)
}

fn callable_id(row: &Map<String, JsonValue>) -> Result<Str, PyExtensionError> {
	row.get("callable")
		.and_then(JsonValue::as_object)
		.and_then(|value| value.get("$omp.callable"))
		.and_then(JsonValue::as_str)
		.filter(|value| !value.is_empty())
		.map(Str::new)
		.ok_or(PyExtensionError::InvalidResult)
}

fn director_state(node: Option<&Node>) -> JsonValue {
	let Some(node) = node else {
		return JsonValue::Object(Map::new());
	};
	JsonValue::Object(
		node
			.props
			.iter()
			.filter_map(|(key, value)| {
				let key = key.as_str().strip_prefix("state/")?;
				serde_json::to_value(value)
					.ok()
					.map(|value| (key.to_owned(), value))
			})
			.collect(),
	)
}

fn bind_value(value: &JsonValue) -> Option<BindValue> {
	match value {
		JsonValue::Bool(value) => Some(BindValue::Bool(*value)),
		JsonValue::Number(value) => value
			.as_i64()
			.map(BindValue::Int)
			.or_else(|| value.as_f64().map(BindValue::Float)),
		JsonValue::String(value) => Some(BindValue::Str(Str::new(value))),
		_ => None,
	}
}
