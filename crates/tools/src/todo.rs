//! Phased session task tracking with deterministic state transitions.

use std::{error, fmt, fmt::Display, sync::Arc};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	ArgIssue, ArgIssueKind, Constraint, Effects, Ev, IncomingParams, ParamError, Part, PromptCaps,
	Rev, Tool, ToolSpec, ToolTerminal,
};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Model arguments for `todo@1`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// State transition to perform.
	pub op:     Op,
	/// Complete phased list for multi-phase `init`.
	#[schemars(default, skip_serializing_if = "Option::is_none")]
	pub list:   Option<Vec<Phase>>,
	/// Phase name for item operations, `append`, and single-phase `init`.
	#[schemars(default, skip_serializing_if = "Option::is_none")]
	pub phase:  Option<Str>,
	/// Item text for single-item operations.
	#[schemars(default, skip_serializing_if = "Option::is_none")]
	pub item:   Option<Str>,
	/// Tasks for single-phase `init` or `append`.
	#[schemars(
		default,
		skip_serializing_if = "Option::is_none",
		description = "tasks for single-phase init or append"
	)]
	pub items:  Option<Vec<Str>>,
	/// Required explanation when blocking an item.
	#[schemars(default, skip_serializing_if = "Option::is_none")]
	pub reason: Option<Str>,
}

/// Supported todo operations.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	JsonSchema,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
pub enum Op {
	/// Replaces the complete phased list.
	Init,
	/// Marks one item as in progress.
	Start,
	/// Marks one item completed.
	Done,
	/// Removes one item.
	Rm,
	/// Marks one item abandoned.
	Drop,
	/// Marks one item blocked with a reason.
	Block,
	/// Returns a blocked item to pending.
	Unblock,
	/// Adds pending items to an existing phase.
	Append,
	/// Returns the current state without changing it.
	View,
}

/// One named phase and its ordered items.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Phase {
	/// Stable phase label.
	pub phase: Str,
	/// Items in their user-defined order.
	pub items: Vec<Item>,
}

/// One task item.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Item {
	/// User-visible task text.
	pub text:   Str,
	/// Current lifecycle state.
	#[serde(default)]
	pub status: Status,
	/// Block explanation, only present while blocked.
	#[schemars(default, skip_serializing_if = "Option::is_none")]
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub reason: Option<Str>,
}

/// Durable task state.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Eq,
	JsonSchema,
	PartialEq,
	Serialize,
	strum::AsRefStr,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Status {
	/// Not yet started.
	#[default]
	Pending,
	/// Actively being worked.
	InProgress,
	/// Finished successfully.
	Completed,
	/// Intentionally abandoned.
	Abandoned,
	/// Waiting on an external dependency.
	Blocked,
}

/// Successful todo state after an operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Payload {
	/// Durable phase tree after the requested operation.
	pub phases:   Vec<Phase>,
	/// Markdown projection of the phase tree.
	pub rendered: Str,
}
/// Todo does not stream progress updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}
/// A rejected todo transition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// An operation's arguments or state transition is invalid.
	Invalid {
		/// Stable validation explanation.
		message: Str,
	},
	/// A named phase or item does not exist.
	Missing {
		/// Stable lookup explanation.
		message: Str,
	},
}
impl Fault {
	/// Stable transition failure explanation.
	pub(crate) fn message(&self) -> &str {
		match self {
			Self::Invalid { message } | Self::Missing { message } => message,
		}
	}
}
impl Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Invalid { message } | Self::Missing { message } => f.write_str(message),
		}
	}
}
impl error::Error for Fault {}

/// In-memory todo executor. Session hosts may snapshot `Payload::phases` into
/// their journal.
pub struct Todo {
	phases: Arc<Mutex<Vec<Phase>>>,
	spec:   ToolSpec,
}
/// Creates the core todo slot tool.
pub fn tool() -> Todo {
	Todo {
		phases: Arc::new(Mutex::new(Vec::new())),
		spec:   ToolSpec {
			name:            sf!("todo"),
			rev:             Rev { family: Str::new(""), n: 1 },
			description:     sf!(
				"Tracks a phased task list. `items` supplies tasks for single-phase `init` or \
				 `append`. After each successful state-changing op, if nothing is `in_progress`, the \
				 earliest `pending` task in phase order auto-promotes; if several are `in_progress`, \
				 only the earliest stays. Blocked tasks never auto-promote: `unblock` first. \
				 Read-only `view` and failed operations never normalize state.",
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects::default(),
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("todo.rs"),
			)
			.into(),
		},
	}
}

impl Tool for Todo {
	type Fault = Fault;
	type Params = Params;
	type Payload = Payload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut params: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, Payload, Fault>> + Send + 'c {
		stream! {
			let arguments = match params.whole::<Params>().await { Ok(value) => value, Err(error) => { yield param_event(error); return; } };
			if let Err(error) = params.interruptable().committed().await { yield commit_event(error); return; }
			let result = apply(&mut self.phases.lock(), arguments).map(|phases| Payload { rendered: Str::new(render(&phases)), phases });
			yield done(result);
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: Str::new(match view {
				Ok(payload) => payload.rendered.to_string(),
				Err(fault) => fault.to_string(),
			}),
		}]
	}
}

/// Applies a transition to a phased list.
pub fn apply(phases: &mut Vec<Phase>, params: Params) -> Result<Vec<Phase>, Fault> {
	let state_changing = params.op != Op::View;
	apply_mut(phases, params)?;
	if state_changing {
		normalize_in_progress(phases);
	}
	Ok(phases.clone())
}

fn apply_mut(phases: &mut Vec<Phase>, params: Params) -> Result<(), Fault> {
	match params.op {
		Op::Init => {
			*phases = if let Some(list) = params.list {
				list
			} else {
				let items = params
					.items
					.ok_or_else(|| invalid("`list` or `items` is required for init"))?;
				if items.is_empty() {
					return Err(invalid("`items` must not be empty for init"));
				}
				vec![Phase {
					phase: params
						.phase
						.map_or_else(|| sf!("Todos"), |phase| title_case(&phase)),
					items: items
						.into_iter()
						.map(|text| Item { text, status: Status::Pending, reason: None })
						.collect(),
				}]
			};
		},
		Op::View => {},
		Op::Append => {
			let phase = required(params.phase, "phase")?;
			let items = params
				.items
				.ok_or_else(|| invalid("`items` is required for append"))?;
			if items.is_empty() {
				return Err(invalid("`items` must not be empty for append"));
			}
			let target = match resolve_phase_index(phases, &phase) {
				Some(index) => &mut phases[index],
				None => {
					phases.push(Phase { phase: title_case(&phase), items: Vec::new() });
					phases.last_mut().expect("phase was appended")
				},
			};
			target.items.extend(items.into_iter().map(|text| Item {
				text,
				status: Status::Pending,
				reason: None,
			}));
		},
		op => {
			if params.phase.is_none() && params.item.is_none() {
				if op == Op::Rm {
					phases.clear();
				} else if matches!(op, Op::Done | Op::Drop) {
					for item in phases.iter_mut().flat_map(|phase| &mut phase.items) {
						set_status(item, op, params.reason.as_ref())?;
					}
				} else {
					return Err(invalid("this operation requires an item"));
				}
				return Ok(());
			}
			let phase_index = match params.phase.as_ref() {
				Some(phase) => {
					resolve_phase_index(phases, phase).ok_or_else(|| missing("phase", phase))?
				},
				None => resolve_item(phases, params.item.as_deref().unwrap_or_default())
					.map(|(phase, _)| phase)
					.ok_or_else(|| missing("item", params.item.as_deref().unwrap_or_default()))?,
			};
			if params.item.is_none() {
				if op == Op::Rm {
					phases.remove(phase_index);
				} else if matches!(op, Op::Done | Op::Drop) {
					for item in &mut phases[phase_index].items {
						set_status(item, op, params.reason.as_ref())?;
					}
				} else {
					return Err(invalid("this operation requires an item"));
				}
				return Ok(());
			}
			let item = params.item.as_ref().expect("item presence checked");
			let item_index = resolve_item_in_phase(&phases[phase_index].items, item)
				.ok_or_else(|| missing("item", item))?;
			if op == Op::Rm {
				phases[phase_index].items.remove(item_index);
				return Ok(());
			}
			if op == Op::Block && params.reason.is_none() {
				return Err(invalid("`reason` is required for block"));
			}
			if op == Op::Start {
				for candidate in phases.iter_mut().flat_map(|phase| &mut phase.items) {
					if candidate.status == Status::InProgress {
						candidate.status = Status::Pending;
					}
				}
			}
			set_status(&mut phases[phase_index].items[item_index], op, params.reason.as_ref())?;
		},
	}
	Ok(())
}

fn normalize_in_progress(phases: &mut [Phase]) {
	let mut found_active = false;
	for item in phases.iter_mut().flat_map(|phase| &mut phase.items) {
		if item.status != Status::InProgress {
			continue;
		}
		if found_active {
			item.status = Status::Pending;
		} else {
			found_active = true;
		}
	}
	if found_active {
		return;
	}
	if let Some(item) = phases
		.iter_mut()
		.flat_map(|phase| &mut phase.items)
		.find(|item| item.status == Status::Pending)
	{
		item.status = Status::InProgress;
	}
}

fn set_status(item: &mut Item, op: Op, reason: Option<&Str>) -> Result<(), Fault> {
	match op {
		Op::Start => {
			item.status = Status::InProgress;
			item.reason = None;
		},
		Op::Done => {
			item.status = Status::Completed;
			item.reason = None;
		},
		Op::Drop => {
			item.status = Status::Abandoned;
			item.reason = None;
		},
		Op::Block => {
			item.status = Status::Blocked;
			item.reason = Some(
				reason
					.cloned()
					.ok_or_else(|| invalid("`reason` is required for block"))?,
			);
		},
		Op::Unblock => {
			if item.status != Status::Blocked {
				return Err(invalid("only blocked items can be unblocked"));
			}
			item.status = Status::Pending;
			item.reason = None;
		},
		Op::Init | Op::Rm | Op::Append | Op::View => unreachable!(),
	}
	Ok(())
}

/// Resolves a phase by case-insensitive exact, unique prefix, then unique
/// substring match.
pub fn resolve_phase_index(phases: &[Phase], query: &str) -> Option<usize> {
	let query = query.trim().to_ascii_lowercase();
	if query.is_empty() {
		return None;
	}
	phases
		.iter()
		.position(|phase| phase.phase.to_ascii_lowercase() == query)
		.or_else(|| {
			unique_index(phases.iter().map(|phase| phase.phase.as_str()), |name| {
				name.to_ascii_lowercase().starts_with(&query)
			})
		})
		.or_else(|| {
			unique_index(phases.iter().map(|phase| phase.phase.as_str()), |name| {
				name.to_ascii_lowercase().contains(&query)
			})
		})
}

/// Resolves one task across phases, preferring a unique actionable match.
pub fn resolve_item(phases: &[Phase], query: &str) -> Option<(usize, usize)> {
	let query = query.trim().to_ascii_lowercase();
	if query.is_empty() {
		return None;
	}
	for (phase_index, phase) in phases.iter().enumerate() {
		if let Some(item_index) = phase
			.items
			.iter()
			.position(|item| item.text.to_ascii_lowercase() == query)
		{
			return Some((phase_index, item_index));
		}
	}
	let matches = phases
		.iter()
		.enumerate()
		.flat_map(|(phase_index, phase)| {
			let query = &query;
			phase
				.items
				.iter()
				.enumerate()
				.filter_map(move |(item_index, item)| {
					item.text.to_ascii_lowercase().contains(query).then_some((
						phase_index,
						item_index,
						matches!(item.status, Status::Pending | Status::InProgress),
					))
				})
		})
		.collect::<Vec<_>>();
	if matches.len() == 1 {
		return matches.first().map(|&(phase, item, _)| (phase, item));
	}
	let mut active = matches.iter().filter(|(_, _, active)| *active);
	let first = active.next()?;
	active.next().is_none().then_some((first.0, first.1))
}

fn resolve_item_in_phase(items: &[Item], query: &str) -> Option<usize> {
	let query = query.trim().to_ascii_lowercase();
	items
		.iter()
		.position(|item| item.text.to_ascii_lowercase() == query)
		.or_else(|| {
			unique_index(items.iter().map(|item| item.text.as_str()), |text| {
				text.to_ascii_lowercase().contains(&query)
			})
		})
}

fn unique_index<'a>(
	values: impl Iterator<Item = &'a str>,
	matches: impl Fn(&str) -> bool,
) -> Option<usize> {
	let mut indexes = values
		.enumerate()
		.filter_map(|(index, value)| matches(value).then_some(index));
	let first = indexes.next()?;
	indexes.next().is_none().then_some(first)
}

fn title_case(value: &str) -> Str {
	let mut output = String::with_capacity(value.len());
	for (index, word) in value.split_whitespace().enumerate() {
		if index > 0 {
			output.push(' ');
		}
		let mut graphemes = xutf::graphemes_str(word);
		if let Some(first) = graphemes.next() {
			output.extend(first.chars().flat_map(char::to_uppercase));
		}
		for grapheme in graphemes {
			output.push_str(grapheme);
		}
	}
	Str::from(output)
}
fn required(value: Option<Str>, name: &str) -> Result<Str, Fault> {
	value.ok_or_else(|| invalid(&format!("`{name}` is required")))
}
fn invalid(message: &str) -> Fault {
	Fault::Invalid { message: Str::new(message) }
}
fn missing(kind: &str, value: &str) -> Fault {
	Fault::Missing { message: sf!("{kind} not found: {value}") }
}
/// Formats the durable state as editable Markdown.
pub fn render(phases: &[Phase]) -> String {
	if phases.is_empty() {
		return "# Todos\n".to_owned();
	}
	let mut output = String::new();
	for (phase_index, phase) in phases.iter().enumerate() {
		if phase_index != 0 {
			output.push('\n');
		}
		output.push_str("# ");
		output.push_str(&phase.phase);
		output.push('\n');
		for item in &phase.items {
			let marker = match item.status {
				Status::Pending => ' ',
				Status::InProgress => '/',
				Status::Completed => 'x',
				Status::Abandoned => '-',
				Status::Blocked => '!',
			};
			output.push_str("- [");
			output.push(marker);
			output.push_str("] ");
			output.push_str(&item.text);
			if item.status == Status::Blocked
				&& let Some(reason) = &item.reason
			{
				output.push_str(" <!-- blocker: ");
				output.push_str(reason);
				output.push_str(" -->");
			}
			output.push('\n');
		}
	}
	output
}

/// Parses an editable Markdown checklist into canonical phased todo state.
pub fn parse_markdown(markdown: &str) -> Result<Vec<Phase>, Fault> {
	let mut phases = Vec::<Phase>::new();
	for (line_index, raw) in markdown.lines().enumerate() {
		let line = raw.trim();
		if line.is_empty() {
			continue;
		}
		if let Some(phase) = parse_heading(line) {
			phases.push(Phase { phase: Str::new(phase), items: Vec::new() });
			continue;
		}
		let Some((marker, content)) = parse_checklist(line) else {
			return Err(invalid(&format!(
				"Line {}: unrecognized todo Markdown syntax",
				line_index + 1
			)));
		};
		let status = match marker {
			' ' => Status::Pending,
			'/' | '>' => Status::InProgress,
			'x' | 'X' => Status::Completed,
			'-' | '~' => Status::Abandoned,
			'!' => Status::Blocked,
			_ => {
				return Err(invalid(&format!(
					"Line {}: unknown status marker `[{marker}]`",
					line_index + 1
				)));
			},
		};
		if phases.is_empty() {
			phases.push(Phase { phase: sf!("Todos"), items: Vec::new() });
		}
		let (text, reason) = if status == Status::Blocked {
			parse_blocker(content)
		} else {
			(content.trim(), None)
		};
		if text.is_empty() {
			return Err(invalid(&format!("Line {}: todo text is empty", line_index + 1)));
		}
		phases
			.last_mut()
			.expect("a default phase was inserted")
			.items
			.push(Item { text: Str::new(text), status, reason: reason.map(Str::new) });
	}
	Ok(phases)
}

fn parse_heading(line: &str) -> Option<&str> {
	let depth = line.bytes().take_while(|byte| *byte == b'#').count();
	if !(1..=6).contains(&depth) {
		return None;
	}
	line
		.get(depth..)
		.map(str::trim)
		.filter(|heading| !heading.is_empty())
}

fn parse_checklist(line: &str) -> Option<(char, &str)> {
	if !matches!(line.as_bytes().first(), Some(b'-' | b'*' | b'+')) {
		return None;
	}
	let mut rest = line.get(1..)?.trim_start();
	rest = rest.strip_prefix('\\').unwrap_or(rest);
	rest = rest.strip_prefix('[')?;
	let marker = rest.chars().next()?;
	rest = rest.get(marker.len_utf8()..)?;
	rest = rest.strip_prefix('\\').unwrap_or(rest);
	rest = rest.strip_prefix(']')?;
	let content = rest.trim_start();
	(!content.is_empty()).then_some((marker, content))
}

fn parse_blocker(content: &str) -> (&str, Option<&str>) {
	let Some(comment) = content.rfind("<!--") else {
		return (content.trim(), None);
	};
	let Some(body) = content
		.get(comment + 4..)
		.and_then(|rest| rest.strip_suffix("-->"))
	else {
		return (content.trim(), None);
	};
	let Some(reason) = body.trim().strip_prefix("blocker:") else {
		return (content.trim(), None);
	};
	(content[..comment].trim(), Some(reason.trim()))
}
const fn done(result: Result<Payload, Fault>) -> Ev<Update, Payload, Fault> {
	Ev::Done(ToolTerminal::Done { result, useless: false })
}
fn param_event(error: ParamError) -> Ev<Update, Payload, Fault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(omp_tool::Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}
fn commit_event(error: omp_tool::CommitError) -> Ev<Update, Payload, Fault> {
	match error {
		omp_tool::CommitError::Aborted => Ev::Aborted(omp_tool::Abort::InputDropped),
		omp_tool::CommitError::Interrupted(interrupt) => {
			Ev::Aborted(omp_tool::Abort::Interrupted { reason: interrupt.reason })
		},
		omp_tool::CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}
fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed JSON argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"op":"view"}}"#)),
		found:    Some(message),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	fn init() -> Vec<Phase> {
		vec![Phase {
			phase: sf!("Build"),
			items: vec![Item { text: sf!("port"), status: Status::Pending, reason: None }],
		}]
	}
	#[test]
	fn transitions_and_append_preserve_phase_order() {
		let mut phases = Vec::new();
		apply(&mut phases, Params {
			op:     Op::Init,
			list:   Some(init()),
			phase:  None,
			item:   None,
			items:  None,
			reason: None,
		})
		.unwrap();
		apply(&mut phases, Params {
			op:     Op::Start,
			list:   None,
			phase:  Some(sf!("Build")),
			item:   Some(sf!("port")),
			items:  None,
			reason: None,
		})
		.unwrap();
		apply(&mut phases, Params {
			op:     Op::Append,
			list:   None,
			phase:  Some(sf!("Build")),
			item:   None,
			items:  Some(vec![sf!("test")]),
			reason: None,
		})
		.unwrap();
		assert_eq!(phases[0].items[0].status, Status::InProgress);
		assert_eq!(phases[0].items[1].text, "test");
	}
	#[test]
	fn block_requires_reason_and_unblock_promotes_the_item() {
		let mut phases = init();
		assert!(
			apply(&mut phases, Params {
				op:     Op::Block,
				list:   None,
				phase:  Some(sf!("Build")),
				item:   Some(sf!("port")),
				items:  None,
				reason: None,
			})
			.is_err()
		);
		apply(&mut phases, Params {
			op:     Op::Block,
			list:   None,
			phase:  Some(sf!("Build")),
			item:   Some(sf!("port")),
			items:  None,
			reason: Some(sf!("blocked")),
		})
		.unwrap();
		apply(&mut phases, Params {
			op:     Op::Unblock,
			list:   None,
			phase:  Some(sf!("Build")),
			item:   Some(sf!("port")),
			items:  None,
			reason: None,
		})
		.unwrap();
		assert_eq!(phases[0].items[0].status, Status::InProgress);
	}

	#[test]
	fn single_phase_init_promotes_only_the_earliest_pending_item() {
		let mut phases = Vec::new();
		apply(&mut phases, Params {
			op:     Op::Init,
			list:   None,
			phase:  Some(sf!("release")),
			item:   None,
			items:  Some(vec![sf!("build"), sf!("ship")]),
			reason: None,
		})
		.expect("single-phase init");
		assert_eq!(phases[0].phase, "Release");
		assert_eq!(
			phases[0]
				.items
				.iter()
				.map(|item| item.status)
				.collect::<Vec<_>>(),
			vec![Status::InProgress, Status::Pending]
		);
	}

	#[test]
	fn normalization_is_pending_only_and_runs_only_after_successful_mutations() {
		let mut phases = vec![Phase {
			phase: sf!("Work"),
			items: vec![
				Item { text: sf!("first"), status: Status::InProgress, reason: None },
				Item { text: sf!("second"), status: Status::InProgress, reason: None },
				Item { text: sf!("blocked"), status: Status::Blocked, reason: Some(sf!("waiting")) },
			],
		}];
		let original = phases.clone();
		apply(&mut phases, Params {
			op:     Op::View,
			list:   None,
			phase:  None,
			item:   None,
			items:  None,
			reason: None,
		})
		.expect("view");
		assert_eq!(phases, original);
		assert!(
			apply(&mut phases, Params {
				op:     Op::Block,
				list:   None,
				phase:  Some(sf!("Work")),
				item:   Some(sf!("first")),
				items:  None,
				reason: None,
			})
			.is_err()
		);
		assert_eq!(phases, original);

		apply(&mut phases, Params {
			op:     Op::Append,
			list:   None,
			phase:  Some(sf!("Work")),
			item:   None,
			items:  Some(vec![sf!("later")]),
			reason: None,
		})
		.expect("successful mutation normalizes duplicate active items");
		assert_eq!(phases[0].items[0].status, Status::InProgress);
		assert_eq!(phases[0].items[1].status, Status::Pending);
		assert_eq!(phases[0].items[2].status, Status::Blocked);

		apply(&mut phases, Params {
			op:     Op::Done,
			list:   None,
			phase:  Some(sf!("Work")),
			item:   Some(sf!("first")),
			items:  None,
			reason: None,
		})
		.expect("state-changing operation");
		assert_eq!(phases[0].items[0].status, Status::Completed);
		assert_eq!(phases[0].items[1].status, Status::InProgress);
		assert_eq!(phases[0].items[2].status, Status::Blocked);
	}

	#[test]
	fn schema_and_guidance_describe_single_phase_items_and_normalization_scope() {
		let todo = tool();
		let schema = String::from_utf8(todo.spec().schema.to_vec()).expect("UTF-8 schema");
		assert!(schema.contains("tasks for single-phase init or append"));
		assert!(
			todo
				.spec()
				.description
				.contains("After each successful state-changing op")
		);
		assert!(
			todo
				.spec()
				.description
				.contains("Blocked tasks never auto-promote")
		);
		assert!(
			todo
				.spec()
				.description
				.contains("Read-only `view` and failed operations never normalize state")
		);
	}
	#[test]
	fn fuzzy_resolution_prefers_exact_and_unique_actionable_matches() {
		let mut phases = vec![
			Phase {
				phase: sf!("Build Runtime"),
				items: vec![
					Item { text: sf!("Port router"), status: Status::Completed, reason: None },
					Item { text: sf!("Test router"), status: Status::Pending, reason: None },
				],
			},
			Phase {
				phase: sf!("Build UI"),
				items: vec![Item {
					text:   sf!("Render router"),
					status: Status::Completed,
					reason: None,
				}],
			},
		];
		assert_eq!(resolve_phase_index(&phases, "runtime"), Some(0));
		assert_eq!(resolve_item(&phases, "test"), Some((0, 1)));
		apply(&mut phases, Params {
			op:     Op::Done,
			list:   None,
			phase:  Some(sf!("runtime")),
			item:   Some(sf!("test")),
			items:  None,
			reason: None,
		})
		.expect("fuzzy transition");
		assert_eq!(phases[0].items[1].status, Status::Completed);
	}

	#[test]
	fn phase_and_all_mutations_are_supported() {
		let mut phases = init();
		apply(&mut phases, Params {
			op:     Op::Done,
			list:   None,
			phase:  Some(sf!("bui")),
			item:   None,
			items:  None,
			reason: None,
		})
		.expect("phase complete");
		assert_eq!(phases[0].items[0].status, Status::Completed);
		apply(&mut phases, Params {
			op:     Op::Rm,
			list:   None,
			phase:  None,
			item:   None,
			items:  None,
			reason: None,
		})
		.expect("clear");
		assert!(phases.is_empty());
	}
	#[test]
	fn editable_markdown_round_trips_every_status_and_block_reason() {
		let phases = vec![Phase {
			phase: sf!("Build"),
			items: vec![
				Item { text: sf!("pending"), status: Status::Pending, reason: None },
				Item { text: sf!("active"), status: Status::InProgress, reason: None },
				Item { text: sf!("done"), status: Status::Completed, reason: None },
				Item { text: sf!("dropped"), status: Status::Abandoned, reason: None },
				Item {
					text:   sf!("blocked"),
					status: Status::Blocked,
					reason: Some(sf!("waiting for owner")),
				},
			],
		}];
		let markdown = render(&phases);
		assert_eq!(parse_markdown(&markdown).expect("round-trip"), phases);
		assert_eq!(
			parse_markdown("# Imported\n* \\[>\\] active\n+ [~] dropped\n")
				.expect("aliases")
				.first()
				.expect("phase")
				.items
				.iter()
				.map(|item| item.status)
				.collect::<Vec<_>>(),
			vec![Status::InProgress, Status::Abandoned]
		);
	}
}
