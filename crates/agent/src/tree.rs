//! Session agent roster, recursive budget authority, and concurrency permits.

use std::{
	collections::{BTreeMap, HashMap, HashSet, VecDeque, btree_map},
	future::Future,
	mem,
	sync::{
		Arc, Weak,
		atomic::{AtomicU8, AtomicU64, Ordering},
	},
	time,
	time::Instant,
};

use omp_core::{AppendVec, Hash32, InvocationPhase, Str};
use omp_inference::recovery::tools::{ToolAssemblyLimits, validate_schema};
use omp_proto::{
	inference::v1::{self as pb, usage, value},
	prost::Message as _,
	thread::v1::{self as thread_pb, item},
};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, map};
use thiserror::Error;
use tokio::sync::{Notify, watch, watch::Receiver};

use crate::name::{AgentNameAllocator, AgentNameError};

/// Default tree-wide number of concurrently running agent turns.
pub const DEFAULT_MAX_CONCURRENCY: usize = 32;
/// Default number of whole spawn waves allowed to await admission.
pub const DEFAULT_MAX_ADMISSION_QUEUE: usize = 128;
/// Number of schema-correction attempts before permissive mode accepts the
/// caller-visible override.
pub const MAX_YIELD_SCHEMA_RETRIES: u8 = 2;

/// Validated terminal or incremental subagent yield.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct YieldPayload {
	/// Verbatim structured success payload after lossless string-container
	/// salvage, when present.
	pub data:              Option<Value>,
	/// Caller-reported terminal failure.
	pub error:             Option<Str>,
	/// String terminal label or array incremental section path.
	pub kind:              Option<Value>,
	/// Whether finalization should consume the child's last assistant turn.
	pub use_last_turn:     bool,
	/// Whether this call submitted an incremental section.
	pub incremental:       bool,
	/// Whether permissive mode accepted a payload after exhausting schema
	/// correction attempts.
	pub schema_overridden: bool,
}

/// Retryable malformed-yield reason returned in-band to the child.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum YieldPayloadError {
	/// `type` was neither a string nor a non-empty string array.
	#[error("type must be a string or non-empty array of strings")]
	InvalidType,
	/// No object-shaped result envelope could be recovered.
	#[error("result must be an object containing either data or error")]
	InvalidEnvelope,
	/// A success envelope carried explicit null data.
	#[error("data is required when yield indicates success")]
	MissingData,
	/// A failure envelope did not carry a string error.
	#[error("error must be a string when yield indicates failure")]
	InvalidError,
	/// Structured tasks cannot use prose-only last-turn finalization.
	#[error(
		"this task requires structured output matching the declared schema; submit the full object \
		 as result.data"
	)]
	SchemaBoundLastTurn,
	/// The recovered payload did not match the declared output schema.
	#[error("yield payload violates output schema at {path} ({rule})")]
	SchemaViolation {
		/// JSON Pointer-like failing payload path.
		path: Str,
		/// Stable schema rule identifier.
		rule: &'static str,
	},
}

/// Stateful, verbatim validator for one subagent's yield calls.
///
/// Generic argument coercion must never run before this validator: accepted
/// payloads are the child's deliverable, not tool plumbing. Only reversible
/// wrapper recovery and JSON-container parsing are performed here.
pub struct YieldPayloadValidator {
	schema:                   Option<Value>,
	strict:                   bool,
	has_incremental_sections: bool,
	schema_retries:           u8,
}

impl YieldPayloadValidator {
	/// Creates a validator for an optional declared output schema.
	pub const fn new(schema: Option<Value>, strict: bool) -> Self {
		Self { schema, strict, has_incremental_sections: false, schema_retries: 0 }
	}

	/// Validates and losslessly salvages one raw yield argument object.
	pub fn validate(&mut self, raw: &Value) -> Result<YieldPayload, YieldPayloadError> {
		let raw = raw.as_object().ok_or(YieldPayloadError::InvalidEnvelope)?;
		let kind = parse_yield_kind(raw.get("type"))?;
		let incremental = kind.as_ref().is_some_and(Value::is_array);
		let result =
			resolve_result_record(raw, kind.is_some()).ok_or(YieldPayloadError::InvalidEnvelope)?;
		let error = match result.get("error") {
			Some(Value::String(error)) => Some(Str::new(error.as_str())),
			Some(_) => return Err(YieldPayloadError::InvalidError),
			None => None,
		};
		let has_data = result.contains_key("data");
		let mut data = result.get("data").cloned();
		let use_last_turn = error.is_none() && !has_data && kind.is_some();
		if error.is_none() && matches!(data, Some(Value::Null)) {
			return Err(YieldPayloadError::MissingData);
		}
		if error.is_none() && !has_data && !use_last_turn {
			return Err(YieldPayloadError::InvalidEnvelope);
		}
		if use_last_turn && self.schema.is_some() && !self.has_incremental_sections && !incremental {
			return Err(YieldPayloadError::SchemaBoundLastTurn);
		}
		let mut schema_overridden = false;
		if error.is_none()
			&& !use_last_turn
			&& !incremental
			&& let Some(schema) = self.schema.as_ref()
			&& let Some(value) = data.as_mut()
			&& let Err(issue) =
				validate_schema(schema, value, self.strict, ToolAssemblyLimits::default())
		{
			let mut salvaged = false;
			if let Value::String(encoded) = value
				&& let Some(parsed) = parse_container_string(encoded)
				&& validate_schema(schema, &parsed, self.strict, ToolAssemblyLimits::default()).is_ok()
			{
				*value = parsed;
				salvaged = true;
			}
			if !salvaged {
				if self.strict || self.schema_retries < MAX_YIELD_SCHEMA_RETRIES {
					self.schema_retries = self.schema_retries.saturating_add(1);
					return Err(YieldPayloadError::SchemaViolation {
						path: issue.path,
						rule: issue.rule,
					});
				}
				schema_overridden = true;
			}
		}
		if error.is_none() && incremental {
			self.has_incremental_sections = true;
		}
		Ok(YieldPayload { data, error, kind, use_last_turn, incremental, schema_overridden })
	}

	/// Returns whether at least one incremental section was accepted.
	pub const fn has_incremental_sections(&self) -> bool {
		self.has_incremental_sections
	}
}

fn parse_yield_kind(kind: Option<&Value>) -> Result<Option<Value>, YieldPayloadError> {
	match kind {
		None => Ok(None),
		Some(Value::String(kind)) => Ok(Some(Value::String(kind.clone()))),
		Some(Value::Array(kinds)) if !kinds.is_empty() && kinds.iter().all(Value::is_string) => {
			Ok(Some(Value::Array(kinds.clone())))
		},
		Some(_) => Err(YieldPayloadError::InvalidType),
	}
}

fn resolve_result_record(
	raw: &serde_json::Map<String, Value>,
	has_kind: bool,
) -> Option<serde_json::Map<String, Value>> {
	let result = match raw.get("result") {
		Some(Value::String(encoded)) => parse_container_string(encoded),
		Some(value) => Some(value.clone()),
		None => None,
	};
	if let Some(Value::Object(result)) = result {
		return Some(result);
	}
	if raw.get("result").is_some_and(|result| !result.is_null()) {
		return None;
	}
	if raw.contains_key("data") || raw.contains_key("error") {
		let mut result = serde_json::Map::new();
		if let Some(data) = raw.get("data") {
			result.insert("data".to_owned(), data.clone());
		}
		if let Some(error) = raw.get("error") {
			result.insert("error".to_owned(), error.clone());
		}
		return Some(result);
	}
	has_kind.then(serde_json::Map::new)
}

fn parse_container_string(encoded: &str) -> Option<Value> {
	let encoded = encoded.trim();
	if !(encoded.starts_with('{') || encoded.starts_with('[')) {
		return None;
	}
	serde_json::from_str(encoded).ok()
}

/// Source which supplied a subagent's effective output schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum OutputSchemaSource {
	/// The spawn caller supplied the schema.
	Caller,
	/// The selected agent definition supplied the schema.
	Frontmatter,
	/// The parent session supplied the schema.
	Session,
	/// No schema applies.
	None,
}

/// Effective schema selected before a child is scheduled.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputSchemaResolution {
	/// Normalized JSON Schema, when any source supplied one.
	pub schema:          Option<Value>,
	/// Winning source.
	pub source:          OutputSchemaSource,
	/// Whether a caller schema replaced definition frontmatter.
	pub overrides_agent: bool,
}

/// Resolves schemas in caller → definition frontmatter → session order.
pub fn resolve_output_schema(
	caller: Option<&Value>,
	frontmatter: Option<&Value>,
	session: Option<&Value>,
) -> OutputSchemaResolution {
	if let Some(schema) = caller {
		return OutputSchemaResolution {
			schema:          Some(schema.clone()),
			source:          OutputSchemaSource::Caller,
			overrides_agent: frontmatter.is_some(),
		};
	}
	if let Some(schema) = frontmatter {
		return OutputSchemaResolution {
			schema:          Some(schema.clone()),
			source:          OutputSchemaSource::Frontmatter,
			overrides_agent: false,
		};
	}
	if let Some(schema) = session {
		return OutputSchemaResolution {
			schema:          Some(schema.clone()),
			source:          OutputSchemaSource::Session,
			overrides_agent: false,
		};
	}
	OutputSchemaResolution {
		schema:          None,
		source:          OutputSchemaSource::None,
		overrides_agent: false,
	}
}

/// Failure while folding incremental yield paths.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum YieldAssemblyError {
	/// An incremental path was empty or contained a blank component.
	#[error("incremental yield path must contain non-empty components")]
	InvalidPath,
	/// A nested section attempted to descend through an existing scalar or
	/// array.
	#[error("incremental yield path collides with a non-object at {path}")]
	ObjectCollision {
		/// JSON Pointer-like collision location.
		path: Str,
	},
}

/// Final folded yield result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AssembledYield {
	/// Explicit terminal data or assembled incremental sections.
	pub data:              Option<Value>,
	/// Explicit terminal error.
	pub error:             Option<Str>,
	/// Whether a data-less terminal used the last assistant turn.
	pub raw_text:          bool,
	/// Whether no usable data was supplied.
	pub missing_data:      bool,
	/// Whether permissive schema handling accepted invalid data.
	pub schema_overridden: bool,
}

/// Path-aware accumulator for a subagent generation's yield calls.
#[derive(Clone, Debug, Default)]
pub struct YieldAssembler {
	schema: Option<Value>,
	items:  Vec<YieldPayload>,
}

impl YieldAssembler {
	/// Creates an empty assembler using the effective schema for array hints.
	pub const fn new(schema: Option<Value>) -> Self {
		Self { schema, items: Vec::new() }
	}

	/// Retains one validated yield call in model-issued order.
	pub fn push(&mut self, payload: YieldPayload) {
		self.items.push(payload);
	}

	/// Folds explicit terminal data or incremental sections into one payload.
	pub fn finish(
		&self,
		last_assistant: Option<&str>,
	) -> Result<AssembledYield, YieldAssemblyError> {
		let terminal = self.items.iter().rev().find(|item| !item.incremental);
		if let Some(terminal) = terminal {
			if terminal.error.is_some() || terminal.data.is_some() {
				return Ok(AssembledYield {
					data:              terminal.data.clone(),
					error:             terminal.error.clone(),
					raw_text:          false,
					missing_data:      terminal.error.is_none() && terminal.data.is_none(),
					schema_overridden: terminal.schema_overridden,
				});
			}
		}

		let mut sections = serde_json::Map::new();
		let mut has_sections = false;
		let mut schema_overridden = false;
		let mut missing_data = false;
		for item in self.items.iter().filter(|item| item.incremental) {
			schema_overridden |= item.schema_overridden;
			let path = item
				.kind
				.as_ref()
				.and_then(Value::as_array)
				.ok_or(YieldAssemblyError::InvalidPath)?
				.iter()
				.map(|part| part.as_str().map(str::trim).filter(|part| !part.is_empty()))
				.collect::<Option<Vec<_>>>()
				.ok_or(YieldAssemblyError::InvalidPath)?;
			let value = item.data.clone().or_else(|| {
				item
					.use_last_turn
					.then(|| last_assistant.map_or(Value::Null, |text| Value::String(text.to_owned())))
			});
			let Some(value) = value else {
				missing_data = true;
				continue;
			};
			missing_data |= value.is_null();
			append_yield_path(&mut sections, &path, value, self.schema.as_ref())?;
			has_sections = true;
		}
		if has_sections {
			return Ok(AssembledYield {
				data: Some(Value::Object(sections)),
				error: None,
				raw_text: false,
				missing_data,
				schema_overridden,
			});
		}

		let Some(terminal) = terminal else {
			return Ok(AssembledYield { missing_data: true, ..AssembledYield::default() });
		};
		let data = terminal
			.use_last_turn
			.then(|| last_assistant.map(|text| Value::String(text.to_owned())))
			.flatten();
		Ok(AssembledYield {
			raw_text: data.is_some(),
			missing_data: data.is_none(),
			data,
			error: None,
			schema_overridden: terminal.schema_overridden,
		})
	}
}

fn append_yield_path(
	root: &mut serde_json::Map<String, Value>,
	path: &[&str],
	value: Value,
	schema: Option<&Value>,
) -> Result<(), YieldAssemblyError> {
	let (leaf, parents) = path.split_last().ok_or(YieldAssemblyError::InvalidPath)?;
	let mut current = root;
	for (index, component) in parents.iter().enumerate() {
		let entry = current
			.entry((*component).to_owned())
			.or_insert_with(|| Value::Object(serde_json::Map::new()));
		let Value::Object(next) = entry else {
			return Err(YieldAssemblyError::ObjectCollision {
				path: Str::from(format!("/{}", path[..=index].join("/"))),
			});
		};
		current = next;
	}
	let force_array = schema_path_is_array(schema, path);
	match current.entry((*leaf).to_owned()) {
		map::Entry::Vacant(entry) => {
			entry.insert(if force_array {
				Value::Array(vec![value])
			} else {
				value
			});
		},
		map::Entry::Occupied(mut entry) => match entry.get_mut() {
			Value::Array(values) => values.push(value),
			existing if !existing.is_object() => {
				let first = mem::replace(existing, Value::Null);
				*existing = Value::Array(vec![first, value]);
			},
			_ => {
				return Err(YieldAssemblyError::ObjectCollision {
					path: Str::from(format!("/{}", path.join("/"))),
				});
			},
		},
	}
	Ok(())
}

fn schema_path_is_array(mut schema: Option<&Value>, path: &[&str]) -> bool {
	for component in path {
		schema = schema
			.and_then(Value::as_object)
			.and_then(|object| {
				object
					.get("properties")
					.or_else(|| object.get("optionalProperties"))
			})
			.and_then(Value::as_object)
			.and_then(|properties| properties.get(*component));
	}
	schema.is_some_and(|schema| {
		schema.get("elements").is_some()
			|| schema.get("type").is_some_and(|kind| {
				kind == "array"
					|| kind
						.as_array()
						.is_some_and(|kinds| kinds.iter().any(|kind| kind == "array"))
			})
	})
}

/// CONTROL operation whose generated metadata requires effects authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum EffectsOperation {
	/// Starts a foreground or Core-owned child session.
	SpawnAgent,
	/// Creates or replaces a durable standing authorization.
	ScheduleUpsert,
	/// Requests paid constrained inference.
	Completion,
}

/// Enforces the shared `EFFECTS_AUTHORIZED` minimum phase for CONTROL effects.
///
/// Wire responders map [`SpawnRefusal::MinimumPhase`] to
/// `SPAWN_REFUSAL_MINIMUM_PHASE`; all three operations deliberately use the
/// same refusal so hooks cannot spend or spawn speculatively.
pub const fn enforce_minimum_phase(
	phase: InvocationPhase,
	_: EffectsOperation,
) -> Result<(), SpawnRefusal> {
	if phase.allows_operation(InvocationPhase::EffectsAuthorized) {
		Ok(())
	} else {
		Err(SpawnRefusal::MinimumPhase)
	}
}

/// Stable classification of a roster node.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum AgentKind {
	/// The interactive session root.
	Main,
	/// A child admitted through subagent spawning.
	Subagent,
	/// A passive observability transcript hidden from peer rosters.
	Advisor,
}

/// Lifecycle state stored in each roster node without allocating on reads.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum AgentStatus {
	/// Admitted but not currently submitting a turn.
	Pending   = 0,
	/// A turn is actively consuming a concurrency permit.
	Running   = 1,
	/// Idle and available for steering.
	Settled   = 2,
	/// Successfully terminal.
	Completed = 3,
	/// Terminal with an error.
	Failed    = 4,
	/// Terminal after cancellation.
	Cancelled = 5,
	/// Terminal after a hard budget or deadline ceiling.
	Exhausted = 6,
}

impl AgentStatus {
	/// Decodes the compact atomic representation, treating corrupt values as
	/// failed.
	pub const fn from_atomic(value: u8) -> Self {
		match value {
			0 => Self::Pending,
			1 => Self::Running,
			2 => Self::Settled,
			3 => Self::Completed,
			4 => Self::Failed,
			5 => Self::Cancelled,
			6 => Self::Exhausted,
			_ => Self::Failed,
		}
	}

	/// Reports whether this status cannot receive another turn.
	pub const fn terminal(self) -> bool {
		matches!(self, Self::Completed | Self::Failed | Self::Cancelled | Self::Exhausted)
	}
}

/// Frontmatter policy governing which definitions an agent may spawn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnPolicy {
	/// The `task` tool is unavailable.
	Disabled,
	/// Any discovered definition may be spawned.
	Any,
	/// Only the named definitions may be spawned.
	Only(Box<[Str]>),
}

impl SpawnPolicy {
	/// Reports whether `definition` is allowed by this exact policy.
	pub fn allows(&self, definition: &str) -> bool {
		match self {
			Self::Disabled => false,
			Self::Any => true,
			Self::Only(allowed) => allowed
				.iter()
				.any(|candidate| candidate.as_str().eq_ignore_ascii_case(definition)),
		}
	}

	/// Returns the inherited default definition for a child spawn.
	pub fn default_definition(&self) -> Option<&str> {
		match self {
			Self::Only(allowed) => allowed.first().map(Str::as_str),
			Self::Any => Some("task"),
			Self::Disabled => None,
		}
	}
}

/// Model-selection purpose for one inherited agent chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentModelPurpose {
	/// Ordinary agent turn.
	Agent,
	/// Prewalk planning pass.
	Prewalk,
	/// Passive advisor call.
	Advisor,
}

/// Optional prewalk or advisor activation declared by an agent definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentAuxiliary {
	/// Enable the session-default role.
	Default,
	/// Enable a specific model selector or role.
	Model(Str),
}

/// Static agent definition loaded through the discovery manifest table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDefinition {
	/// Stable discovery key, normally the file stem.
	pub name:            Str,
	/// Human-readable description used by dynamic task schemas.
	pub description:     Str,
	/// Exact child tool vocabulary. An empty list inherits the caller toolset.
	pub tools:           Box<[Str]>,
	/// Child-spawn capability and whitelist.
	pub spawns:          SpawnPolicy,
	/// Optional role or exact model selector.
	pub model:           Option<Str>,
	/// Optional prewalk-specific selector.
	pub prewalk_model:   Option<Str>,
	/// Ordered advisor selector chain.
	pub advisor_models:  Box<[Str]>,
	/// Optional typed thinking level name.
	pub thinking_level:  Option<Str>,
	/// Optional prewalk activation or selector.
	pub prewalk:         Option<AgentAuxiliary>,
	/// Optional advisor activation or selector.
	pub advisor:         Option<AgentAuxiliary>,
	/// Skills injected before the first delegated turn and restored on revival.
	pub autoload_skills: Box<[Str]>,
	/// Whether structural read summaries remain enabled for this definition.
	pub read_summarize:  Option<bool>,
	/// Definition-owned output schema, normalized from YAML frontmatter.
	pub output_schema:   Option<Value>,
	/// Whether execution must block the caller.
	pub blocking:        bool,
	/// Markdown body appended to the spawned system prompt.
	pub prompt:          Str,
}

/// Malformed agent discovery frontmatter.
#[derive(Debug, Error)]
pub enum AgentDefinitionError {
	/// The markdown document lacks a complete frontmatter fence.
	#[error("agent definition frontmatter is missing or unterminated")]
	MissingFrontmatter,
	/// YAML frontmatter could not be decoded.
	#[error("agent definition frontmatter is malformed")]
	Yaml(#[source] serde_yaml::Error),
	/// A supported field had an invalid value.
	#[error("invalid agent frontmatter field {0}")]
	InvalidField(&'static str),
}

impl AgentDefinition {
	/// Parses the portable frontmatter subset used by manifest-discovered
	/// definitions. Unknown keys remain forward-compatible and are ignored.
	pub fn parse_markdown(
		name: impl Into<Str>,
		markdown: &str,
	) -> Result<Self, AgentDefinitionError> {
		let name = name.into();
		let Some(rest) = markdown.strip_prefix("---\n") else {
			return Err(AgentDefinitionError::MissingFrontmatter);
		};
		let Some((frontmatter, prompt)) = rest.split_once("\n---") else {
			return Err(AgentDefinitionError::MissingFrontmatter);
		};
		let prompt = prompt.strip_prefix('\n').unwrap_or(prompt);
		let document: serde_yaml::Value =
			serde_yaml::from_str(frontmatter).map_err(AgentDefinitionError::Yaml)?;
		let fields = document
			.as_mapping()
			.ok_or(AgentDefinitionError::InvalidField("frontmatter"))?;
		let description = yaml_string(fields, &["description"])?.unwrap_or_default();
		let mut tools = yaml_string_list(fields, &["tools"], "tools")?;
		if !tools.is_empty() && !tools.iter().any(|tool| tool == "yield") {
			tools.push(Str::new_static("yield"));
		}
		let spawns =
			if yaml_get(fields, &["spawns"]).is_none() && tools.iter().any(|tool| tool == "task") {
				SpawnPolicy::Any
			} else {
				yaml_spawn_policy(fields)?
			};
		let tools = tools.into_boxed_slice();
		let model = yaml_string(fields, &["model"])?;
		let prewalk_model = yaml_string(fields, &["prewalkModel", "prewalk_model"])?;
		let advisor_models =
			yaml_string_list(fields, &["advisorModels", "advisor_models"], "advisorModels")?
				.into_boxed_slice();
		let thinking_level =
			yaml_string(fields, &["thinkingLevel", "thinking-level", "thinking_level"])?;
		let prewalk = yaml_auxiliary(fields, &["prewalk"], "prewalk")?;
		let advisor = yaml_auxiliary(fields, &["advisor"], "advisor")?;
		let autoload_skills = yaml_string_list(
			fields,
			&["autoloadSkills", "autoload-skills", "autoload_skills"],
			"autoloadSkills",
		)?
		.into_boxed_slice();
		let read_summarize = yaml_bool(
			fields,
			&["readSummarize", "read-summarize", "read_summarize"],
			"readSummarize",
		)?;
		let output_schema = yaml_get(fields, &["output", "outputSchema", "output_schema"])
			.map(|value| serde_yaml::from_value(value.clone()).map_err(AgentDefinitionError::Yaml))
			.transpose()?;
		let blocking = yaml_bool(fields, &["blocking"], "blocking")?.unwrap_or(false);
		Ok(Self {
			name,
			description,
			tools,
			spawns,
			model,
			prewalk_model,
			advisor_models,
			thinking_level,
			prewalk,
			advisor,
			autoload_skills,
			read_summarize,
			output_schema,
			blocking,
			prompt: Str::new(prompt),
		})
	}

	/// Builds an ordered inherited selector chain for the requested agent
	/// purpose.
	///
	/// The caller feeds this chain into the catalog's ordinary candidate
	/// planner, ensuring agent, prewalk, and advisor selection use identical
	/// glob/role/fallback semantics.
	pub fn effective_model_chain<'a>(
		&'a self,
		overrides: &'a BTreeMap<Str, Str>,
		purpose: AgentModelPurpose,
		parent: Option<&'a str>,
		session: Option<&'a str>,
	) -> Vec<&'a str> {
		let mut chain = Vec::new();
		match purpose {
			AgentModelPurpose::Agent => {},
			AgentModelPurpose::Prewalk => {
				if let Some(model) = self.prewalk_model.as_deref() {
					chain.push(model);
				}
			},
			AgentModelPurpose::Advisor => chain.extend(self.advisor_models.iter().map(Str::as_str)),
		}
		if let Some(model) = self.effective_model(overrides)
			&& !chain.contains(&model)
		{
			chain.push(model);
		}
		for inherited in [parent, session].into_iter().flatten() {
			if !chain.contains(&inherited) {
				chain.push(inherited);
			}
		}
		chain
	}

	/// Resolves a configured per-agent override ahead of frontmatter.
	pub fn effective_model<'a>(&'a self, overrides: &'a BTreeMap<Str, Str>) -> Option<&'a str> {
		overrides
			.iter()
			.find(|(name, _)| name.as_str().eq_ignore_ascii_case(self.name.as_str()))
			.map(|(_, model)| model.as_str())
			.or(self.model.as_deref())
	}
}

fn yaml_get<'a>(fields: &'a serde_yaml::Mapping, keys: &[&str]) -> Option<&'a serde_yaml::Value> {
	keys
		.iter()
		.find_map(|key| fields.get(serde_yaml::Value::String((*key).to_owned())))
}

fn yaml_string(
	fields: &serde_yaml::Mapping,
	keys: &[&'static str],
) -> Result<Option<Str>, AgentDefinitionError> {
	let Some(value) = yaml_get(fields, keys) else {
		return Ok(None);
	};
	match value {
		serde_yaml::Value::Null => Ok(None),
		serde_yaml::Value::String(value) => {
			let value = value.trim();
			Ok((!value.is_empty()).then(|| Str::new(value)))
		},
		serde_yaml::Value::Sequence(values) => values
			.first()
			.and_then(serde_yaml::Value::as_str)
			.map(|value| Some(Str::new(value.trim())))
			.ok_or(AgentDefinitionError::InvalidField(keys[0])),
		_ => Err(AgentDefinitionError::InvalidField(keys[0])),
	}
}

fn yaml_string_list(
	fields: &serde_yaml::Mapping,
	keys: &[&'static str],
	field: &'static str,
) -> Result<Vec<Str>, AgentDefinitionError> {
	let Some(value) = yaml_get(fields, keys) else {
		return Ok(Vec::new());
	};
	match value {
		serde_yaml::Value::Null => Ok(Vec::new()),
		serde_yaml::Value::String(value) => parse_string_list(field, value),
		serde_yaml::Value::Sequence(values) => values
			.iter()
			.map(|value| {
				value
					.as_str()
					.map(str::trim)
					.filter(|value| !value.is_empty())
					.map(Str::new)
					.ok_or(AgentDefinitionError::InvalidField(field))
			})
			.collect(),
		_ => Err(AgentDefinitionError::InvalidField(field)),
	}
}

fn yaml_bool(
	fields: &serde_yaml::Mapping,
	keys: &[&'static str],
	field: &'static str,
) -> Result<Option<bool>, AgentDefinitionError> {
	let Some(value) = yaml_get(fields, keys) else {
		return Ok(None);
	};
	value
		.as_bool()
		.map(Some)
		.ok_or(AgentDefinitionError::InvalidField(field))
}

fn yaml_auxiliary(
	fields: &serde_yaml::Mapping,
	keys: &[&'static str],
	field: &'static str,
) -> Result<Option<AgentAuxiliary>, AgentDefinitionError> {
	let Some(value) = yaml_get(fields, keys) else {
		return Ok(None);
	};
	match value {
		serde_yaml::Value::Null | serde_yaml::Value::Bool(false) => Ok(None),
		serde_yaml::Value::Bool(true) => Ok(Some(AgentAuxiliary::Default)),
		serde_yaml::Value::String(model) if !model.trim().is_empty() => {
			Ok(Some(AgentAuxiliary::Model(Str::new(model.trim()))))
		},
		_ => Err(AgentDefinitionError::InvalidField(field)),
	}
}

fn yaml_spawn_policy(fields: &serde_yaml::Mapping) -> Result<SpawnPolicy, AgentDefinitionError> {
	let Some(value) = yaml_get(fields, &["spawns"]) else {
		return Ok(SpawnPolicy::Disabled);
	};
	match value {
		serde_yaml::Value::Bool(true) => Ok(SpawnPolicy::Any),
		serde_yaml::Value::Bool(false) | serde_yaml::Value::Null => Ok(SpawnPolicy::Disabled),
		serde_yaml::Value::String(value) => parse_spawn_policy(value),
		serde_yaml::Value::Sequence(_) => {
			let allowed = yaml_string_list(fields, &["spawns"], "spawns")?;
			if allowed.is_empty() {
				Ok(SpawnPolicy::Disabled)
			} else {
				Ok(SpawnPolicy::Only(allowed.into_boxed_slice()))
			}
		},
		_ => Err(AgentDefinitionError::InvalidField("spawns")),
	}
}

fn parse_spawn_policy(value: &str) -> Result<SpawnPolicy, AgentDefinitionError> {
	match unquote(value) {
		"*" | "true" => Ok(SpawnPolicy::Any),
		"" | "false" => Ok(SpawnPolicy::Disabled),
		_ => {
			let allowed = parse_string_list("spawns", value)?;
			if allowed.is_empty() {
				Ok(SpawnPolicy::Disabled)
			} else {
				Ok(SpawnPolicy::Only(allowed.into_boxed_slice()))
			}
		},
	}
}

fn parse_string_list(field: &'static str, value: &str) -> Result<Vec<Str>, AgentDefinitionError> {
	let value = value.trim();
	let value = value
		.strip_prefix('[')
		.and_then(|value| value.strip_suffix(']'))
		.unwrap_or(value);
	if value.trim().is_empty() {
		return Ok(Vec::new());
	}
	let values = value
		.split(',')
		.map(|part| Str::new(unquote(part.trim())))
		.filter(|part| !part.is_empty())
		.collect::<Vec<_>>();
	if values.is_empty() {
		Err(AgentDefinitionError::InvalidField(field))
	} else {
		Ok(values)
	}
}

fn unquote(value: &str) -> &str {
	value
		.strip_prefix('"')
		.and_then(|value| value.strip_suffix('"'))
		.or_else(|| {
			value
				.strip_prefix('\'')
				.and_then(|value| value.strip_suffix('\''))
		})
		.unwrap_or(value)
}

/// Durable usage totals used for hard subtree budget checks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
	/// Submitted provider requests.
	pub requests:      u64,
	/// Metered input tokens, including the inference-owned cache policy.
	pub input_tokens:  u64,
	/// Output and reasoning tokens.
	pub output_tokens: u64,
	/// Cost in micros of USD from durable turn receipts only.
	pub usd_micros:    u64,
}

impl Usage {
	const fn saturating_add(self, right: Self) -> Self {
		Self {
			requests:      self.requests.saturating_add(right.requests),
			input_tokens:  self.input_tokens.saturating_add(right.input_tokens),
			output_tokens: self.output_tokens.saturating_add(right.output_tokens),
			usd_micros:    self.usd_micros.saturating_add(right.usd_micros),
		}
	}
}

/// Exact observable statistics recorded directly against durable receipts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeStatistics {
	/// User message items committed in outcomes.
	pub user_messages:      u64,
	/// Assistant message items committed in outcomes.
	pub assistant_messages: u64,
	/// System message items committed in outcomes.
	pub system_messages:    u64,
	/// Tool calls committed in outcomes.
	pub tool_calls:         u64,
	/// Tool results committed in outcomes.
	pub tool_results:       u64,
	/// Tool results whose canonical outcome is an error.
	pub tool_errors:        u64,
	/// Canonical inference usage, including optional provider fields.
	pub usage:              pb::Usage,
	/// Canonical cost, including every component field.
	pub cost:               pb::Cost,
	/// Distinct durable receipts.
	pub requests:           u64,
}

impl TreeStatistics {
	fn add_outcome(&mut self, outcome: &pb::Outcome) {
		for item in &outcome.output {
			match item.kind.as_ref() {
				Some(item::Kind::Message(message)) => {
					match thread_pb::Role::try_from(message.role).unwrap_or(thread_pb::Role::Unspecified)
					{
						thread_pb::Role::User => {
							self.user_messages = self.user_messages.saturating_add(1);
						},
						thread_pb::Role::Assistant => {
							self.assistant_messages = self.assistant_messages.saturating_add(1);
						},
						thread_pb::Role::System => {
							self.system_messages = self.system_messages.saturating_add(1);
						},
						thread_pb::Role::Unspecified => {},
					}
				},
				Some(item::Kind::ToolCall(_)) => {
					self.tool_calls = self.tool_calls.saturating_add(1);
				},
				Some(item::Kind::ToolResult(result)) => {
					self.tool_results = self.tool_results.saturating_add(1);
					self.tool_errors = self.tool_errors.saturating_add(u64::from(result.is_error));
				},
				None => {},
			}
		}
		if let Some(usage) = outcome.usage.as_ref() {
			merge_usage(&mut self.usage, usage);
		}
		if let Some(cost) = outcome.cost.as_ref() {
			merge_cost(&mut self.cost, cost);
		}
		self.requests = self.requests.saturating_add(1);
	}

	fn saturating_add(&mut self, source: &Self) {
		self.user_messages = self.user_messages.saturating_add(source.user_messages);
		self.assistant_messages = self
			.assistant_messages
			.saturating_add(source.assistant_messages);
		self.system_messages = self.system_messages.saturating_add(source.system_messages);
		self.tool_calls = self.tool_calls.saturating_add(source.tool_calls);
		self.tool_results = self.tool_results.saturating_add(source.tool_results);
		self.tool_errors = self.tool_errors.saturating_add(source.tool_errors);
		merge_usage(&mut self.usage, &source.usage);
		merge_cost(&mut self.cost, &source.cost);
		self.requests = self.requests.saturating_add(source.requests);
	}
}

fn merge_usage(target: &mut pb::Usage, source: &pb::Usage) {
	target.input_tokens = target.input_tokens.saturating_add(source.input_tokens);
	target.output_tokens = target.output_tokens.saturating_add(source.output_tokens);
	target.cache_read_tokens = target
		.cache_read_tokens
		.saturating_add(source.cache_read_tokens);
	target.cache_write_tokens = target
		.cache_write_tokens
		.saturating_add(source.cache_write_tokens);
	target.accuracy = match (target.accuracy, source.accuracy) {
		(0, right) => right,
		(left, 0) => left,
		(left, right) if left == right => left,
		_ => usage::Accuracy::Mixed as i32,
	};
	merge_usage_detail(&mut target.detail, source.detail.as_ref());
	target.total_tokens = sum_optional(target.total_tokens, source.total_tokens);
	target.context_tokens = sum_optional(target.context_tokens, source.context_tokens);
	merge_orchestration(&mut target.orchestration, source.orchestration.as_ref());
	target.premium_requests = sum_optional(target.premium_requests, source.premium_requests);
	target.reasoning_tokens = sum_optional(target.reasoning_tokens, source.reasoning_tokens);
	merge_cache_ttl(&mut target.cache_ttl, source.cache_ttl.as_ref());
	merge_server_tools(&mut target.server_tools, source.server_tools.as_ref());
}

const fn sum_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
	match (left, right) {
		(None, None) => None,
		(Some(left), Some(right)) => Some(left.saturating_add(right)),
		(Some(value), None) | (None, Some(value)) => Some(value),
	}
}

fn merge_orchestration(
	target: &mut Option<pb::OrchestrationUsage>,
	source: Option<&pb::OrchestrationUsage>,
) {
	let Some(source) = source else {
		return;
	};
	let target = target.get_or_insert_with(pb::OrchestrationUsage::default);
	target.input_tokens = sum_optional(target.input_tokens, source.input_tokens);
	target.cache_read_tokens = sum_optional(target.cache_read_tokens, source.cache_read_tokens);
	target.output_tokens = sum_optional(target.output_tokens, source.output_tokens);
}

fn merge_cache_ttl(target: &mut Option<pb::CacheTtlUsage>, source: Option<&pb::CacheTtlUsage>) {
	let Some(source) = source else {
		return;
	};
	let target = target.get_or_insert_with(pb::CacheTtlUsage::default);
	target.ephemeral_5m_tokens =
		sum_optional(target.ephemeral_5m_tokens, source.ephemeral_5m_tokens);
	target.ephemeral_1h_tokens =
		sum_optional(target.ephemeral_1h_tokens, source.ephemeral_1h_tokens);
}

fn merge_server_tools(
	target: &mut Option<pb::ServerToolUsage>,
	source: Option<&pb::ServerToolUsage>,
) {
	let Some(source) = source else {
		return;
	};
	let target = target.get_or_insert_with(pb::ServerToolUsage::default);
	target.web_search_requests =
		sum_optional(target.web_search_requests, source.web_search_requests);
	target.web_fetch_requests = sum_optional(target.web_fetch_requests, source.web_fetch_requests);
}

fn merge_cost(target: &mut pb::Cost, source: &pb::Cost) {
	target.nanos_usd = target.nanos_usd.saturating_add(source.nanos_usd);
	target.estimated |= source.estimated;
	target.input_nanos_usd = sum_optional(target.input_nanos_usd, source.input_nanos_usd);
	target.output_nanos_usd = sum_optional(target.output_nanos_usd, source.output_nanos_usd);
	target.cache_read_nanos_usd =
		sum_optional(target.cache_read_nanos_usd, source.cache_read_nanos_usd);
	target.cache_write_nanos_usd =
		sum_optional(target.cache_write_nanos_usd, source.cache_write_nanos_usd);
}

fn merge_usage_detail(target: &mut Option<pb::ValueMap>, source: Option<&pb::ValueMap>) {
	let Some(source) = source else {
		return;
	};
	let target = target.get_or_insert_with(pb::ValueMap::default);
	for (key, incoming) in &source.fields {
		match target.fields.entry(key.clone()) {
			btree_map::Entry::Vacant(entry) => {
				entry.insert(incoming.clone());
			},
			btree_map::Entry::Occupied(mut entry) => {
				let merged = match (entry.get().kind.as_ref(), incoming.kind.as_ref()) {
					(Some(value::Kind::Int(left)), Some(value::Kind::Int(right))) => {
						left.checked_add(*right).map(value::Kind::Int)
					},
					(Some(value::Kind::Uint(left)), Some(value::Kind::Uint(right))) => {
						left.checked_add(*right).map(value::Kind::Uint)
					},
					_ if entry.get() == incoming => continue,
					_ => None,
				};
				if let Some(kind) = merged {
					entry.get_mut().kind = Some(kind);
				} else {
					entry.remove();
				}
			},
		}
	}
}

/// Hard ceilings for an agent and every descendant.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Budget {
	/// Maximum subtree provider requests.
	pub max_requests:      Option<u64>,
	/// Maximum subtree metered input tokens.
	pub max_input_tokens:  Option<u64>,
	/// Maximum subtree output and reasoning tokens.
	pub max_output_tokens: Option<u64>,
	/// Maximum subtree durable receipt spend in micros of USD.
	pub max_usd_micros:    Option<u64>,
	/// Maximum duration from admission to settlement.
	pub max_wall:          Option<time::Duration>,
}

impl Budget {
	/// Clamps this budget to the unspent remainder represented by `parent`.
	pub fn clamped_to(self, parent: BudgetRemainder) -> Self {
		Self {
			max_requests:      clamp(self.max_requests, parent.requests),
			max_input_tokens:  clamp(self.max_input_tokens, parent.input_tokens),
			max_output_tokens: clamp(self.max_output_tokens, parent.output_tokens),
			max_usd_micros:    clamp(self.max_usd_micros, parent.usd_micros),
			max_wall:          match (self.max_wall, parent.wall) {
				(Some(child), Some(ancestor)) => Some(child.min(ancestor)),
				(None, value) => value,
				(value, None) => value,
			},
		}
	}
}

fn clamp(child: Option<u64>, parent: Option<u64>) -> Option<u64> {
	match (child, parent) {
		(Some(child), Some(parent)) => Some(child.min(parent)),
		(None, value) => value,
		(value, None) => value,
	}
}

/// Remaining capacity at one point in an ancestor chain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetRemainder {
	/// Remaining requests.
	pub requests:      Option<u64>,
	/// Remaining input tokens.
	pub input_tokens:  Option<u64>,
	/// Remaining output tokens.
	pub output_tokens: Option<u64>,
	/// Remaining durable-receipt spend.
	pub usd_micros:    Option<u64>,
	/// Remaining wall time.
	pub wall:          Option<time::Duration>,
}

#[derive(Debug)]
struct BudgetAccount {
	budget:       Budget,
	usage:        Usage,
	direct_stats: TreeStatistics,
	receipt_ids:  HashSet<Hash32>,
	admitted_at:  Instant,
}

impl BudgetAccount {
	fn remainder(&self) -> BudgetRemainder {
		BudgetRemainder {
			requests:      self
				.budget
				.max_requests
				.map(|cap| cap.saturating_sub(self.usage.requests)),
			input_tokens:  self
				.budget
				.max_input_tokens
				.map(|cap| cap.saturating_sub(self.usage.input_tokens)),
			output_tokens: self
				.budget
				.max_output_tokens
				.map(|cap| cap.saturating_sub(self.usage.output_tokens)),
			usd_micros:    self
				.budget
				.max_usd_micros
				.map(|cap| cap.saturating_sub(self.usage.usd_micros)),
			wall:          self
				.budget
				.max_wall
				.map(|cap| cap.saturating_sub(self.admitted_at.elapsed())),
		}
	}

	fn permits(&self, next: Usage) -> Result<(), BudgetCeiling> {
		let total = self.usage.saturating_add(next);
		if self
			.budget
			.max_requests
			.is_some_and(|cap| total.requests > cap)
		{
			return Err(BudgetCeiling::Requests);
		}
		if self
			.budget
			.max_input_tokens
			.is_some_and(|cap| total.input_tokens > cap)
		{
			return Err(BudgetCeiling::InputTokens);
		}
		if self
			.budget
			.max_output_tokens
			.is_some_and(|cap| total.output_tokens > cap)
		{
			return Err(BudgetCeiling::OutputTokens);
		}
		if self
			.budget
			.max_usd_micros
			.is_some_and(|cap| total.usd_micros > cap)
		{
			return Err(BudgetCeiling::Usd);
		}
		if self
			.budget
			.max_wall
			.is_some_and(|cap| self.admitted_at.elapsed() > cap)
		{
			return Err(BudgetCeiling::Wall);
		}
		Ok(())
	}
}

/// One roster node retained for the life of its session.
pub struct AgentNode {
	/// Stable agent identity.
	pub id:         Str,
	/// Session-unique display and routing name.
	pub name:       Str,
	/// Resolved definition identity used for recursion prevention.
	pub definition: Option<Str>,
	/// Whether this is the root or a spawned child.
	pub kind:       AgentKind,
	/// Parent identity, absent only for the root.
	pub parent:     Option<Str>,
	/// Tree depth, with root at zero.
	pub depth:      u16,
	/// Session identity owning this journal.
	pub session:    Str,
	status:         AtomicU8,
	activity:       Mutex<Str>,
	budget:         Mutex<BudgetAccount>,
}

impl AgentNode {
	/// Returns this node's allocation-free lifecycle state.
	pub fn status(&self) -> AgentStatus {
		AgentStatus::from_atomic(self.status.load(Ordering::Acquire))
	}

	/// Publishes a lifecycle state.
	pub fn set_status(&self, status: AgentStatus) {
		self.status.store(status as u8, Ordering::Release);
	}

	/// Replaces the short roster activity text.
	pub fn set_activity(&self, activity: Str) {
		*self.activity.lock() = activity;
	}

	/// Returns a clone of the latest roster activity text.
	pub fn activity(&self) -> Str {
		self.activity.lock().clone()
	}

	/// Returns subtree usage used by this node's inherited budget.
	pub fn usage(&self) -> Usage {
		self.budget.lock().usage
	}

	/// Returns statistics from receipts directly owned by this node.
	pub fn direct_statistics(&self) -> TreeStatistics {
		self.budget.lock().direct_stats.clone()
	}
}

/// Reason a spawn wave could not be admitted.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum SpawnRefusal {
	/// The requested parent was absent or terminal.
	#[error("parent agent is unavailable")]
	ParentGone,
	/// A durable identity was already registered.
	#[error("agent identity is already registered")]
	DuplicateIdentity,
	/// A definition attempted to spawn itself.
	#[error("an agent cannot recursively spawn its own resolved definition")]
	SelfRecursion,
	/// A caller display name was invalid.
	#[error(transparent)]
	InvalidName(#[from] AgentNameError),
	/// The requested child would exceed the tree depth ceiling.
	#[error("agent depth ceiling exceeded")]
	DepthExceeded,
	/// CONTROL effects were invoked before `EFFECTS_AUTHORIZED`.
	#[error("SPAWN_REFUSAL_MINIMUM_PHASE")]
	MinimumPhase,
	/// The whole spawn wave cannot fit in the bounded admission queue.
	#[error(
		"agent concurrency exhausted (running={running}, queued={queued}, max={max_concurrency})"
	)]
	ConcurrencyExhausted {
		/// Turns holding concurrency permits.
		running:         usize,
		/// Spawn-wave slots already awaiting permits.
		queued:          usize,
		/// Tree-wide concurrency ceiling.
		max_concurrency: usize,
	},
}

/// Ceiling which rejected a request before it reached a provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum BudgetCeiling {
	/// Request count would exceed its cap.
	Requests,
	/// Input tokens would exceed their cap.
	InputTokens,
	/// Output tokens would exceed their cap.
	OutputTokens,
	/// Durable receipt spend would exceed its cap.
	Usd,
	/// Admission-to-settlement duration exceeded its cap.
	Wall,
}

/// Budget pre-dispatch rejection for a node or any ancestor.
#[derive(Debug, Error, Eq, PartialEq)]
#[error("agent budget exhausted: {ceiling}")]
pub struct BudgetExceeded {
	/// The first ancestor ceiling crossed by the proposed request.
	pub ceiling: BudgetCeiling,
}

/// One queued concurrency acquisition.
struct ConcurrencyWaiter {
	ticket: u64,
	units:  usize,
	wake:   Arc<Notify>,
}

struct ConcurrencyState {
	limit:       usize,
	active:      usize,
	next_ticket: u64,
	waiters:     VecDeque<ConcurrencyWaiter>,
}
/// Live agent-tree admission limits and occupancy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentTreeLimits {
	/// Maximum admitted child depth.
	pub max_depth:       u16,
	/// Concurrent run ceiling; zero means unlimited.
	pub max_concurrency: usize,
	/// Runs currently holding admission permits.
	pub active:          usize,
	/// Run slots currently waiting for admission.
	pub queued:          usize,
	/// Maximum queued run slots.
	pub max_queue:       usize,
}

/// Race-safe, session-scoped resizable concurrency authority.
///
/// A zero limit is genuinely unlimited. Shrinking never revokes active runs;
/// it only delays later acquisitions until the active count falls below the
/// new ceiling.
struct ConcurrencyController {
	state:     Mutex<ConcurrencyState>,
	max_queue: usize,
}

impl ConcurrencyController {
	fn new(limit: usize, max_queue: usize) -> Arc<Self> {
		Arc::new(Self {
			state: Mutex::new(ConcurrencyState {
				limit,
				active: 0,
				next_ticket: 0,
				waiters: VecDeque::new(),
			}),
			max_queue,
		})
	}

	async fn acquire(self: &Arc<Self>, units: usize) -> Result<(), SpawnRefusal> {
		if units == 0 {
			return Err(self.refusal());
		}
		let (ticket, wake) = {
			let mut state = self.state.lock();
			if state.limit != 0 && units > state.limit {
				return Err(refusal_from(&state));
			}
			if state.waiters.is_empty()
				&& (state.limit == 0 || state.active.saturating_add(units) <= state.limit)
			{
				state.active = state.active.saturating_add(units);
				return Ok(());
			}
			let queued = state
				.waiters
				.iter()
				.fold(0_usize, |total, waiter| total.saturating_add(waiter.units));
			if queued.saturating_add(units) > self.max_queue {
				return Err(refusal_from(&state));
			}
			let ticket = state.next_ticket;
			state.next_ticket = state.next_ticket.wrapping_add(1);
			let wake = Arc::new(Notify::new());
			state
				.waiters
				.push_back(ConcurrencyWaiter { ticket, units, wake: Arc::clone(&wake) });
			(ticket, wake)
		};
		let mut registration =
			WaitRegistration { controller: Arc::downgrade(self), ticket: Some(ticket) };
		loop {
			let notified = wake.notified();
			{
				let mut state = self.state.lock();
				let position = state
					.waiters
					.iter()
					.position(|waiter| waiter.ticket == ticket);
				let Some(position) = position else {
					continue;
				};
				let may_run = state.limit == 0
					|| (position == 0 && state.active.saturating_add(units) <= state.limit);
				if may_run {
					state.waiters.remove(position);
					state.active = state.active.saturating_add(units);
					registration.ticket = None;
					wake_waiters(&state);
					return Ok(());
				}
			}
			notified.await;
		}
	}

	fn release(&self, units: usize) {
		let mut state = self.state.lock();
		state.active = state.active.saturating_sub(units);
		wake_waiters(&state);
	}

	fn resize(&self, limit: usize) {
		let mut state = self.state.lock();
		state.limit = limit;
		wake_waiters(&state);
	}

	fn limit(&self) -> usize {
		self.state.lock().limit
	}

	fn refusal(&self) -> SpawnRefusal {
		refusal_from(&self.state.lock())
	}

	fn cancel(&self, ticket: u64) {
		let mut state = self.state.lock();
		if let Some(position) = state
			.waiters
			.iter()
			.position(|waiter| waiter.ticket == ticket)
		{
			state.waiters.remove(position);
			wake_waiters(&state);
		}
	}
}

fn refusal_from(state: &ConcurrencyState) -> SpawnRefusal {
	SpawnRefusal::ConcurrencyExhausted {
		running:         state.active,
		queued:          state
			.waiters
			.iter()
			.fold(0_usize, |total, waiter| total.saturating_add(waiter.units)),
		max_concurrency: state.limit,
	}
}

fn wake_waiters(state: &ConcurrencyState) {
	for waiter in &state.waiters {
		waiter.wake.notify_one();
		if state.limit != 0 {
			break;
		}
	}
}

#[must_use]
struct WaitRegistration {
	controller: Weak<ConcurrencyController>,
	ticket:     Option<u64>,
}

impl Drop for WaitRegistration {
	fn drop(&mut self) {
		if let (Some(controller), Some(ticket)) = (self.controller.upgrade(), self.ticket) {
			controller.cancel(ticket);
		}
	}
}

/// RAII reservation for active agent runs.
///
/// Dropping it releases every held unit. A waiter must call
/// [`Self::release_for_wait`] before awaiting a child and [`Self::reacquire`]
/// afterwards; this is the release-while-waiting accounting rule.
#[must_use]
pub struct SpawnPermit {
	controller: Arc<ConcurrencyController>,
	held:       bool,
	units:      usize,
}

impl SpawnPermit {
	/// Releases this agent's active-turn capacity before waiting on a child.
	pub fn release_for_wait(&mut self) {
		if self.held {
			self.controller.release(self.units);
			self.held = false;
		}
	}

	/// Re-acquires the same capacity after a child wait completes.
	pub async fn reacquire(&mut self) {
		if !self.held {
			self
				.controller
				.acquire(self.units)
				.await
				.expect("a previously admitted run remains admissible");
			self.held = true;
		}
	}

	/// Runs `future` without holding this agent's turn permit, then restores it.
	pub async fn wait<F: Future>(&mut self, future: F) -> F::Output {
		self.release_for_wait();
		let output = future.await;
		self.reacquire().await;
		output
	}

	/// Returns how many concurrency units this reservation represents.
	pub const fn units(&self) -> usize {
		self.units
	}
}

impl Drop for SpawnPermit {
	fn drop(&mut self) {
		self.release_for_wait();
	}
}

/// Session-scoped append-only roster and resource authority.
pub struct AgentTree {
	nodes:             AppendVec<Arc<AgentNode>>,
	by_id:             RwLock<HashMap<Str, usize>>,
	by_name:           RwLock<HashMap<Str, usize>>,
	names:             AgentNameAllocator,
	concurrency:       Arc<ConcurrencyController>,
	max_depth:         u16,
	roster_generation: AtomicU64,
	roster_watch:      watch::Sender<u64>,
}

impl AgentTree {
	/// Creates an empty tree with explicit depth, concurrency, and queue
	/// ceilings.
	pub fn new(max_depth: u16, max_concurrency: usize, max_queue: usize) -> Self {
		let (roster_watch, _) = watch::channel(0_u64);
		Self {
			nodes: AppendVec::new(),
			by_id: RwLock::new(HashMap::new()),
			by_name: RwLock::new(HashMap::new()),
			names: AgentNameAllocator::new(),
			concurrency: ConcurrencyController::new(max_concurrency, max_queue),
			max_depth,
			roster_generation: AtomicU64::new(0),
			roster_watch,
		}
	}

	/// Creates a tree with the standard session ceilings.
	pub fn standard(max_depth: u16) -> Self {
		Self::new(max_depth, DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_ADMISSION_QUEUE)
	}

	/// Adds a root or already-resolved node to the append-only roster.
	pub fn register(
		&self,
		id: Str,
		name: Str,
		kind: AgentKind,
		parent: Option<Str>,
		session: Str,
		budget: Budget,
	) -> Result<Arc<AgentNode>, SpawnRefusal> {
		self.names.reserve(name.as_str());
		self.register_node(id, name, None, kind, parent, session, budget)
	}

	/// Resolves a child display name and atomically blocks self-recursion by
	/// definition identity before publishing the node.
	pub fn register_child(
		&self,
		id: Str,
		requested_name: Option<&str>,
		definition: &AgentDefinition,
		parent: Str,
		session: Str,
		budget: Budget,
	) -> Result<Arc<AgentNode>, SpawnRefusal> {
		let parent_node = self.node(parent.as_str()).ok_or(SpawnRefusal::ParentGone)?;
		if parent_node.status().terminal() {
			return Err(SpawnRefusal::ParentGone);
		}
		if parent_node
			.definition
			.as_ref()
			.is_some_and(|parent_definition| {
				parent_definition
					.as_str()
					.eq_ignore_ascii_case(definition.name.as_str())
			}) {
			return Err(SpawnRefusal::SelfRecursion);
		}
		let name =
			self
				.names
				.allocate(id.as_str(), Some(parent_node.name.as_str()), requested_name)?;
		self.register_node(
			id,
			name,
			Some(definition.name.clone()),
			AgentKind::Subagent,
			Some(parent),
			session,
			budget,
		)
	}

	/// Reserves a recovered artifact or journal stem before later allocation.
	pub fn reserve_historical_name(&self, name: &str) {
		self.names.reserve(name);
	}

	fn register_node(
		&self,
		id: Str,
		name: Str,
		definition: Option<Str>,
		kind: AgentKind,
		parent: Option<Str>,
		session: Str,
		budget: Budget,
	) -> Result<Arc<AgentNode>, SpawnRefusal> {
		if self.by_id.read().contains_key(&id) {
			return Err(SpawnRefusal::DuplicateIdentity);
		}
		let depth = match parent.as_ref() {
			Some(parent) => {
				let parent = self.node(parent).ok_or(SpawnRefusal::ParentGone)?;
				if parent.status().terminal() && kind == AgentKind::Subagent {
					return Err(SpawnRefusal::ParentGone);
				}
				parent.depth.saturating_add(1)
			},
			None => 0,
		};
		if depth > self.max_depth {
			return Err(SpawnRefusal::DepthExceeded);
		}
		let node = Arc::new(AgentNode {
			id: id.clone(),
			name: name.clone(),
			definition,
			kind,
			parent,
			depth,
			session,
			status: AtomicU8::new(AgentStatus::Pending as u8),
			activity: Mutex::new(Default::default()),
			budget: Mutex::new(BudgetAccount {
				budget,
				usage: Usage::default(),
				direct_stats: TreeStatistics::default(),
				receipt_ids: HashSet::new(),
				admitted_at: Instant::now(),
			}),
		});
		let index = self.nodes.push(Arc::clone(&node));
		self.by_id.write().insert(id, index);
		self
			.by_name
			.write()
			.insert(Str::from(name.as_str().to_ascii_lowercase()), index);
		self.publish_roster_change();
		if kind == AgentKind::Subagent {
			tracing::info!(
				agent_id = %node.id,
				agent_name = %node.name,
				parent_id = ?node.parent,
				depth = node.depth,
				"subagent admitted"
			);
		}
		Ok(node)
	}

	/// Returns a node by stable identity without scanning the roster.
	pub fn node(&self, id: &str) -> Option<Arc<AgentNode>> {
		let index = *self.by_id.read().get(id)?;
		self.nodes.get(index).cloned()
	}

	/// Returns a node by session-local name without scanning the roster.
	pub fn named(&self, name: &str) -> Option<Arc<AgentNode>> {
		let folded = name.to_ascii_lowercase();
		let index = *self.by_name.read().get(folded.as_str())?;
		self.nodes.get(index).cloned()
	}

	/// Iterates the append-only roster in admission order.
	pub fn roster(&self) -> impl Iterator<Item = &Arc<AgentNode>> {
		self.nodes.iter()
	}

	/// Returns a watch receiver that advances whenever a node is admitted.
	///
	/// Consumers obtain the allocation-free roster after `changed()`; this
	/// avoids UI polling while keeping node storage append-only.
	pub fn watch_roster(&self) -> Receiver<u64> {
		self.roster_watch.subscribe()
	}

	/// Returns the current monotonic roster generation.
	pub fn roster_generation(&self) -> u64 {
		self.roster_generation.load(Ordering::Acquire)
	}

	/// Returns one coherent snapshot of configured limits and live admission
	/// occupancy.
	pub fn limits(&self) -> AgentTreeLimits {
		let state = self.concurrency.state.lock();
		AgentTreeLimits {
			max_depth:       self.max_depth,
			max_concurrency: state.limit,
			active:          state.active,
			queued:          state
				.waiters
				.iter()
				.fold(0_usize, |total, waiter| total.saturating_add(waiter.units)),
			max_queue:       self.concurrency.max_queue,
		}
	}

	/// Reserves capacity for active agent runs.
	///
	/// Callers should acquire one unit per run. Queue overflow refuses the
	/// request before it can start.
	pub async fn admit(&self, count: usize) -> Result<SpawnPermit, SpawnRefusal> {
		if count != 1 {
			return Err(self.concurrency.refusal());
		}
		self.concurrency.acquire(count).await?;
		Ok(SpawnPermit {
			controller: Arc::clone(&self.concurrency),
			held:       true,
			units:      count,
		})
	}

	/// Checks all ancestor ceilings before dispatch and records receipt-backed
	/// usage.
	///
	/// Callers must pass only usage committed by a durable receipt; telemetry is
	/// intentionally not an input to this method.
	pub fn debit_receipt(&self, node_id: &str, usage: Usage) -> Result<(), BudgetExceeded> {
		let mut lineage = Vec::new();
		let mut current = self
			.node(node_id)
			.ok_or(BudgetExceeded { ceiling: BudgetCeiling::Requests })?;
		loop {
			lineage.push(Arc::clone(&current));
			let Some(parent) = current.parent.as_ref() else {
				break;
			};
			current = self
				.node(parent)
				.ok_or(BudgetExceeded { ceiling: BudgetCeiling::Requests })?;
		}
		lineage.reverse();
		let mut accounts = lineage
			.iter()
			.map(|node| node.budget.lock())
			.collect::<Vec<_>>();
		for account in &accounts {
			account
				.permits(usage)
				.map_err(|ceiling| BudgetExceeded { ceiling })?;
		}
		for account in &mut accounts {
			account.usage = account.usage.saturating_add(usage);
		}
		Ok(())
	}

	/// Debits one canonical durable outcome exactly once and records every usage
	/// and cost field on the owning node.
	///
	/// The canonical encoded outcome is its replay-stable identity. Replaying an
	/// identical receipt on the same node is accepted as an idempotent no-op.
	pub fn debit_outcome(
		&self,
		node_id: &str,
		outcome: &pb::Outcome,
	) -> Result<bool, BudgetExceeded> {
		let mut lineage = Vec::new();
		let mut current = self
			.node(node_id)
			.ok_or(BudgetExceeded { ceiling: BudgetCeiling::Requests })?;
		loop {
			lineage.push(Arc::clone(&current));
			let Some(parent) = current.parent.as_ref() else {
				break;
			};
			current = self
				.node(parent)
				.ok_or(BudgetExceeded { ceiling: BudgetCeiling::Requests })?;
		}
		lineage.reverse();
		let mut accounts = lineage
			.iter()
			.map(|node| node.budget.lock())
			.collect::<Vec<_>>();
		let Some(owner) = accounts.last() else {
			return Err(BudgetExceeded { ceiling: BudgetCeiling::Requests });
		};
		let receipt_id = Hash32::sum(outcome.encode_to_vec());
		if owner.receipt_ids.contains(&receipt_id) {
			return Ok(false);
		}
		let usage = outcome.usage.as_ref();
		let cost = outcome.cost.as_ref();
		let budget_usage = Usage {
			requests:      1,
			input_tokens:  usage.map_or(0, |usage| usage.input_tokens),
			output_tokens: usage.map_or(0, |usage| usage.output_tokens),
			usd_micros:    cost.map_or(0, |cost| cost.nanos_usd.saturating_add(999) / 1_000),
		};
		for account in &accounts {
			account
				.permits(budget_usage)
				.map_err(|ceiling| BudgetExceeded { ceiling })?;
		}
		for account in &mut accounts {
			account.usage = account.usage.saturating_add(budget_usage);
		}
		let Some(owner) = accounts.last_mut() else {
			return Err(BudgetExceeded { ceiling: BudgetCeiling::Requests });
		};
		owner.direct_stats.add_outcome(outcome);
		owner.receipt_ids.insert(receipt_id);
		Ok(true)
	}

	/// Returns direct or recursively rolled-up receipt statistics.
	///
	/// Recursive aggregation walks direct node receipts rather than adding the
	/// already inherited budget totals, so each descendant contributes once.
	pub fn statistics(&self, node_id: &str, recursive: bool) -> Option<TreeStatistics> {
		let root = self.node(node_id)?;
		if !recursive {
			return Some(root.direct_statistics());
		}
		let mut total = TreeStatistics::default();
		for candidate in self.roster() {
			let mut current = Some(Arc::clone(candidate));
			let include = loop {
				let Some(node) = current else {
					break false;
				};
				if node.id == root.id {
					break true;
				}
				current = node.parent.as_ref().and_then(|parent| self.node(parent));
			};
			if include {
				total.saturating_add(&candidate.direct_statistics());
			}
		}
		Some(total)
	}

	/// Clamps a child's requested budget against every ancestor's unspent
	/// remainder.
	pub fn clamp_budget(&self, parent_id: &str, requested: Budget) -> Result<Budget, SpawnRefusal> {
		let mut effective = requested;
		let mut current = self.node(parent_id).ok_or(SpawnRefusal::ParentGone)?;
		loop {
			effective = effective.clamped_to(current.budget.lock().remainder());
			let Some(parent) = current.parent.as_ref() else {
				break;
			};
			current = self.node(parent).ok_or(SpawnRefusal::ParentGone)?;
		}
		Ok(effective)
	}

	/// Returns the live tree-wide concurrency ceiling (`0` means unlimited).
	pub fn max_concurrency(&self) -> usize {
		self.concurrency.limit()
	}

	/// Applies a new concurrency ceiling without replacing the controller.
	///
	/// Active runs keep their permits while a shrink is applied. New runs
	/// observe the new ceiling immediately.
	pub fn resize_concurrency(&self, max_concurrency: usize) {
		self.concurrency.resize(max_concurrency);
	}

	fn publish_roster_change(&self) {
		let generation = self
			.roster_generation
			.fetch_add(1, Ordering::AcqRel)
			.wrapping_add(1);
		self.roster_watch.send_replace(generation);
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;
	use tokio::task;

	use super::*;

	#[tokio::test]
	async fn permit_is_released_while_waiting() {
		let tree = AgentTree::new(2, 1, 2);
		let mut parent = tree.admit(1).await.unwrap();
		parent
			.wait(async {
				drop(tree.admit(1).await.unwrap());
			})
			.await;
	}

	#[tokio::test]
	async fn zero_concurrency_is_unlimited() {
		let tree = AgentTree::new(2, 0, 2);
		let first = tree.admit(1).await.unwrap();
		let second = tree.admit(1).await.unwrap();
		assert_eq!(tree.max_concurrency(), 0);
		assert_eq!(first.units(), 1);
		assert_eq!(second.units(), 1);
	}

	#[tokio::test]
	async fn resize_applies_under_load_without_revoking_active_runs() {
		let tree = Arc::new(AgentTree::new(2, 2, 4));
		let first = tree.admit(1).await.unwrap();
		let second = tree.admit(1).await.unwrap();
		tree.resize_concurrency(1);
		let waiting_tree = Arc::clone(&tree);
		let waiter = tokio::spawn(async move { waiting_tree.admit(1).await.unwrap() });
		task::yield_now().await;
		assert!(!waiter.is_finished());
		drop(first);
		task::yield_now().await;
		assert!(!waiter.is_finished());
		drop(second);
		let third = waiter.await.unwrap();
		assert_eq!(third.units(), 1);
		tree.resize_concurrency(0);
		let fourth = tree.admit(1).await.unwrap();
		let fifth = tree.admit(1).await.unwrap();
		drop((third, fourth, fifth));
	}

	#[tokio::test]
	async fn cancelled_waiter_does_not_consume_capacity_or_block_the_queue() {
		let tree = Arc::new(AgentTree::new(2, 1, 4));
		let active = tree.admit(1).await.unwrap();
		let cancelled_tree = Arc::clone(&tree);
		let cancelled = tokio::spawn(async move { cancelled_tree.admit(1).await });
		task::yield_now().await;
		cancelled.abort();
		assert!(matches!(cancelled.await, Err(error) if error.is_cancelled()));

		let next_tree = Arc::clone(&tree);
		let next = tokio::spawn(async move { next_tree.admit(1).await.unwrap() });
		task::yield_now().await;
		assert!(!next.is_finished());
		drop(active);
		let permit = next.await.unwrap();
		assert_eq!(permit.units(), 1);
	}

	#[test]
	fn child_budget_clamps_to_ancestor_remainder() {
		let tree = AgentTree::standard(2);
		tree
			.register(sf!("root"), sf!("Main"), AgentKind::Main, None, sf!("s"), Budget {
				max_requests: Some(4),
				..Budget::default()
			})
			.unwrap();
		tree
			.debit_receipt("root", Usage { requests: 3, ..Usage::default() })
			.unwrap();
		assert_eq!(
			tree
				.clamp_budget("root", Budget { max_requests: Some(9), ..Budget::default() })
				.unwrap()
				.max_requests,
			Some(1)
		);
	}

	#[test]
	fn wrapperless_terminal_yield_uses_last_turn_without_schema() {
		let mut validator = YieldPayloadValidator::new(None, true);
		let payload = validator.validate(&json!({"type": "result"})).unwrap();
		assert!(payload.use_last_turn);
		assert_eq!(payload.kind, Some(json!("result")));
		assert!(payload.data.is_none());
	}

	#[test]
	fn schema_bound_last_turn_is_retryable_until_a_section_exists() {
		let schema = json!({
			"type": "object",
			"properties": {"summary": {"type": "string"}},
			"required": ["summary"]
		});
		let mut validator = YieldPayloadValidator::new(Some(schema), true);
		assert_eq!(
			validator.validate(&json!({"type": "result"})),
			Err(YieldPayloadError::SchemaBoundLastTurn)
		);
		validator
			.validate(&json!({
				"type": ["summary"],
				"result": {"data": "done"}
			}))
			.unwrap();
		assert!(validator.has_incremental_sections());
		assert!(
			validator
				.validate(&json!({"type": "result"}))
				.unwrap()
				.use_last_turn
		);
	}

	#[test]
	fn weak_yield_envelopes_are_salvaged_losslessly() {
		let mut validator = YieldPayloadValidator::new(None, true);
		assert_eq!(
			validator
				.validate(&json!({"data": {"ok": true}}))
				.unwrap()
				.data,
			Some(json!({"ok": true}))
		);
		assert_eq!(
			validator
				.validate(&json!({"error": "blocked"}))
				.unwrap()
				.error,
			Some(sf!("blocked"))
		);
		assert_eq!(
			validator
				.validate(&json!({"result": "{\"data\":{\"ok\":true}}"}))
				.unwrap()
				.data,
			Some(json!({"ok": true}))
		);
	}

	#[test]
	fn schema_payload_parses_container_string_but_never_stringifies_objects() {
		let object_schema = json!({
			"type": "object",
			"properties": {"n": {"type": "number"}},
			"required": ["n"],
			"additionalProperties": false
		});
		let mut validator = YieldPayloadValidator::new(Some(object_schema), true);
		assert_eq!(
			validator
				.validate(&json!({"result": {"data": "{\"n\":4}"}}))
				.unwrap()
				.data,
			Some(json!({"n": 4}))
		);

		let string_field_schema = json!({
			"type": "object",
			"properties": {"summary": {"type": "string"}},
			"required": ["summary"],
			"additionalProperties": false
		});
		let mut validator = YieldPayloadValidator::new(Some(string_field_schema), true);
		assert!(matches!(
			validator.validate(&json!({
				"result": {"data": {"summary": {"purge": 13, "keep": 20}}}
			})),
			Err(YieldPayloadError::SchemaViolation { path, rule: "type" })
				if path.as_str() == "/summary"
		));
	}

	#[test]
	fn permissive_yield_overrides_only_after_retry_budget() {
		let schema = json!({"type":"string"});
		let raw = json!({"result":{"data":7}});
		let mut permissive = YieldPayloadValidator::new(Some(schema.clone()), false);
		for _ in 0..MAX_YIELD_SCHEMA_RETRIES {
			assert!(matches!(
				permissive.validate(&raw),
				Err(YieldPayloadError::SchemaViolation { .. })
			));
		}
		assert!(permissive.validate(&raw).unwrap().schema_overridden);

		let mut strict = YieldPayloadValidator::new(Some(schema), true);
		for _ in 0..=MAX_YIELD_SCHEMA_RETRIES {
			assert!(matches!(strict.validate(&raw), Err(YieldPayloadError::SchemaViolation { .. })));
		}
	}

	#[test]
	fn discovered_agent_frontmatter_enforces_spawn_and_model_policy() {
		let definition = AgentDefinition::parse_markdown(
			"reviewer",
			"---\ndescription: Review code\ntools: [read, grep, hub]\nspawns: [scout, \
			 librarian]\nmodel: '@task'\nthinkingLevel: high\nblocking: true\n---\nReview carefully.",
		)
		.expect("definition");
		assert_eq!(definition.tools.as_ref(), ["read", "grep", "hub", "yield"]);
		assert!(definition.spawns.allows("SCOUT"));
		assert!(!definition.spawns.allows("task"));
		assert_eq!(definition.spawns.default_definition(), Some("scout"));
		assert_eq!(definition.thinking_level.as_deref(), Some("high"));
		assert!(definition.blocking);
		let overrides = BTreeMap::from([("Reviewer".into(), "@slow".into())]);
		assert_eq!(definition.effective_model(&overrides), Some("@slow"));
	}
}
