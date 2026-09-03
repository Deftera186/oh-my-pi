//! Schema-derived `dyn` discovery and invocation builtin.

use std::{
	collections::{BTreeMap, BTreeSet},
	fmt::Write as _,
	fs,
	io::{self, Read, Write as _},
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_core::Str;
use omp_shell_engine::{
	ShellExtensions,
	builtins::{ContentOptions, ContentType, Registration},
	commands::{CommandArg, ExecutionContext},
	error,
	results::ExecutionResult,
};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::host::{DynDevice, DynFault, DynHost, DynOutput, DynSchema};

const HELP: &str = "dyn — discover and invoke live dynamic devices\n\nUsage:\n  dyn\n  dyn --q \
                    <TEXT>\n  dyn <NAMESPACE>/<TOOL> --help\n  dyn <NAMESPACE>/<TOOL> [--FLAG \
                    VALUE ...] [@FILE] [-]\n\n@FILE and - merge a JSON object from a file or \
                    stdin. Flags override merged values.\n";

/// Builds the dynamic-device builtin around one environment-owned host.
pub(crate) fn registration<SE: ShellExtensions>(host: Arc<dyn DynHost>) -> Registration<SE> {
	Registration {
		execute_func: Arc::new(move |context, args| {
			let host = Arc::clone(&host);
			Box::pin(async move { run(host, context, args).await })
		}),
		content_func: content,
		disabled: false,
		special_builtin: false,
		declaration_builtin: false,
		transparent_background_wrapper: false,
	}
}

fn content(
	_name: &str,
	_content_type: ContentType,
	_options: &ContentOptions,
) -> Result<String, error::Error> {
	Ok(HELP.to_owned())
}

async fn run<SE: ShellExtensions>(
	host: Arc<dyn DynHost>,
	context: ExecutionContext<'_, SE>,
	args: Vec<CommandArg>,
) -> Result<ExecutionResult, error::Error> {
	let argv = args
		.into_iter()
		.skip(1)
		.map(|argument| match argument {
			CommandArg::String(value) => Str::from(value),
			CommandArg::Assignment(value) => Str::from(value.to_string()),
		})
		.collect::<Vec<_>>();

	if argv.len() == 1 && is_help(argv[0].as_str()) {
		write_message(context.stdout(), HELP)?;
		return Ok(ExecutionResult::success());
	}

	let Some(first) = argv.first() else {
		let devices = match host.list().await {
			Ok(devices) => devices,
			Err(fault) => return host_fault(&context, &fault),
		};
		write_message(context.stdout(), &render_catalog(&devices))?;
		return Ok(ExecutionResult::success());
	};
	if first == "--q" {
		let devices = match host.list().await {
			Ok(devices) => devices,
			Err(fault) => return host_fault(&context, &fault),
		};
		let Some(query) = argv.get(1) else {
			write_error(&context, "--q requires search text")?;
			return Ok(ExecutionResult::new(2));
		};
		if argv.len() != 2 {
			write_error(&context, "--q accepts exactly one search string")?;
			return Ok(ExecutionResult::new(2));
		}
		write_message(context.stdout(), &render_search(&devices, query))?;
		return Ok(ExecutionResult::success());
	}
	if first.starts_with('-') {
		write_error(&context, "unknown option; run `dyn --help`")?;
		return Ok(ExecutionResult::new(2));
	}

	if argv[1..].iter().any(|argument| is_help(argument)) {
		if argv.len() != 2 {
			write_error(&context, "--help cannot be combined with invocation arguments")?;
			return Ok(ExecutionResult::new(2));
		}
		let schema = match host.schema(first).await {
			Ok(schema) => schema,
			Err(fault) => return host_fault(&context, &fault),
		};
		write_message(context.stdout(), &render_help(&schema))?;
		return Ok(ExecutionResult::success());
	}

	let schema = match host.schema(first).await {
		Ok(schema) => schema,
		Err(fault) => return host_fault(&context, &fault),
	};
	let mut stdin = context.stdin();
	let cwd = context.shell.working_dir();
	let parsed = match parse_args(
		&schema.schema,
		&argv[1..],
		cwd,
		context.params.path_policy().map(AsRef::as_ref),
		&mut stdin,
	) {
		Ok(parsed) => parsed,
		Err(parse_error) => {
			write_error(&context, &parse_error)?;
			write_error(&context, &format!("run `dyn {first} --help`"))?;
			return Ok(ExecutionResult::new(2));
		},
	};

	match host.call(first, Value::Object(parsed)).await {
		Ok(DynOutput::Text(text)) => write_message(context.stdout(), text.as_str())?,
		Ok(DynOutput::Json(value)) => write_message(context.stdout(), &value.to_string())?,
		Err(fault) => return host_fault(&context, &fault),
	}
	Ok(ExecutionResult::success())
}

fn host_fault<SE: ShellExtensions>(
	context: &ExecutionContext<'_, SE>,
	fault: &DynFault,
) -> Result<ExecutionResult, error::Error> {
	write_error(context, fault.message.as_str())?;
	Ok(ExecutionResult::general_error())
}

fn write_error<SE: ShellExtensions>(
	context: &ExecutionContext<'_, SE>,
	message: &(impl std::fmt::Display + ?Sized),
) -> Result<(), error::Error> {
	let mut stderr = context.stderr();
	writeln!(stderr, "dyn: {message}")?;
	Ok(())
}

fn write_message(mut output: impl io::Write, message: &str) -> io::Result<()> {
	output.write_all(message.as_bytes())?;
	if !message.ends_with('\n') {
		output.write_all(b"\n")?;
	}
	Ok(())
}

fn is_help(argument: &str) -> bool {
	matches!(argument, "-h" | "--help")
}

fn render_catalog(devices: &[DynDevice]) -> String {
	let mut namespaces = BTreeMap::<&str, Vec<&DynDevice>>::new();
	for device in devices {
		let namespace = device
			.name
			.split_once('/')
			.map_or("other", |(namespace, _)| namespace);
		namespaces.entry(namespace).or_default().push(device);
	}
	let mut rendered = String::new();
	for (namespace, mut members) in namespaces {
		members.sort_unstable_by(|left, right| left.name.cmp(&right.name));
		let _ = writeln!(rendered, "{namespace}/");
		for device in members {
			let leaf = device
				.name
				.rsplit('/')
				.next()
				.unwrap_or(device.name.as_str());
			let _ = write!(rendered, "  {leaf}");
			if let Some(description) = &device.description {
				let _ = write!(rendered, " — {description}");
			}
			rendered.push('\n');
		}
	}
	rendered
}

fn render_search(devices: &[DynDevice], query: &str) -> String {
	let query = query.trim().to_ascii_lowercase();
	let mut matches = devices
		.iter()
		.filter_map(|device| search_score(device, &query).map(|score| (score, device)))
		.collect::<Vec<_>>();
	matches.sort_unstable_by(|left, right| {
		left
			.0
			.cmp(&right.0)
			.then_with(|| left.1.name.cmp(&right.1.name))
	});
	let mut rendered = String::new();
	for (_, device) in matches {
		let _ = write!(rendered, "{}", device.name);
		if let Some(description) = &device.description {
			let _ = write!(rendered, " — {description}");
		}
		rendered.push('\n');
	}
	rendered
}

fn search_score(device: &DynDevice, query: &str) -> Option<(u8, usize)> {
	if query.is_empty() {
		return Some((0, 0));
	}
	let name = device.name.to_ascii_lowercase();
	let leaf = name.rsplit('/').next().unwrap_or(&name);
	if leaf == query || name == query {
		return Some((0, 0));
	}
	if leaf.starts_with(query) || name.starts_with(query) {
		return Some((1, 0));
	}
	if leaf.contains(query) || name.contains(query) {
		return Some((2, 0));
	}
	if device
		.description
		.as_ref()
		.is_some_and(|description| description.to_ascii_lowercase().contains(query))
	{
		return Some((3, 0));
	}
	let distance = levenshtein(query, leaf);
	(distance <= 3 || distance.saturating_mul(3) <= leaf.chars().count()).then_some((4, distance))
}

fn levenshtein(left: &str, right: &str) -> usize {
	let mut row = (0..=right.chars().count()).collect::<Vec<_>>();
	for (left_index, left_char) in left.chars().enumerate() {
		let mut diagonal = row[0];
		row[0] = left_index + 1;
		for (right_index, right_char) in right.chars().enumerate() {
			let above = row[right_index + 1];
			row[right_index + 1] = (above + 1)
				.min(row[right_index] + 1)
				.min(diagonal + usize::from(left_char != right_char));
			diagonal = above;
		}
	}
	row[right.chars().count()]
}

fn render_help(schema: &DynSchema) -> String {
	let leaves = schema_leaves(&schema.schema);
	let mut rendered = String::new();
	let _ = writeln!(
		rendered,
		"{} — {}",
		schema.name,
		schema.description.as_deref().unwrap_or("dynamic device")
	);
	let _ = writeln!(rendered, "\nUsage:\n  dyn {} [OPTIONS] [@FILE] [-]", schema.name);
	if !leaves.is_empty() {
		rendered.push_str("\nOptions:\n");
		for leaf in leaves {
			let flag = flag_name(&leaf.path);
			let _ = write!(rendered, "  --{flag}");
			if leaf.kind == ScalarKind::Boolean && leaf.values.is_none() {
				let _ = write!(rendered, " / --no-{flag}");
			} else {
				let _ = write!(rendered, " {}", value_usage(&leaf));
			}
			if let Some(description) = leaf.description {
				let _ = write!(rendered, "  {description}");
			}
			if leaf.required {
				rendered.push_str("  (required)");
			}
			if leaf.repeatable {
				rendered.push_str("  (repeatable)");
			}
			rendered.push('\n');
		}
	}
	rendered.push_str("  -j, --json <JSON>  Merge one raw JSON object.\n");
	rendered.push_str("  @FILE             Merge a JSON object from FILE.\n");
	rendered.push_str("  -                 Merge a JSON object from stdin.\n");
	rendered.push_str("  -h, --help        Show this help.\n");
	rendered
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScalarKind {
	String,
	Integer,
	Number,
	Boolean,
	Object,
	Fallback,
}

struct SchemaLeaf<'a> {
	path:        Vec<&'a str>,
	kind:        ScalarKind,
	values:      Option<&'a [Value]>,
	description: Option<&'a str>,
	required:    bool,
	repeatable:  bool,
	schema:      &'a Value,
}

fn schema_leaves(schema: &Value) -> Vec<SchemaLeaf<'_>> {
	let mut leaves = Vec::new();
	let mut path = Vec::new();
	collect_leaves(schema, true, &mut path, &mut leaves);
	leaves
}

fn collect_leaves<'a>(
	schema: &'a Value,
	parent_required: bool,
	path: &mut Vec<&'a str>,
	leaves: &mut Vec<SchemaLeaf<'a>>,
) {
	let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
		return;
	};
	let required = schema
		.get("required")
		.and_then(Value::as_array)
		.map(|items| {
			items
				.iter()
				.filter_map(Value::as_str)
				.collect::<BTreeSet<_>>()
		})
		.unwrap_or_default();
	for (name, child) in properties {
		let required_here = parent_required && required.contains(name.as_str());
		path.push(name);
		if child.get("properties").and_then(Value::as_object).is_some() {
			collect_leaves(child, required_here, path, leaves);
		} else {
			let kind = scalar_kind(child);
			leaves.push(SchemaLeaf {
				path: path.clone(),
				kind,
				values: child
					.get("enum")
					.and_then(Value::as_array)
					.map(Vec::as_slice),
				description: child.get("description").and_then(Value::as_str),
				required: required_here,
				repeatable: matches!(child.get("type").and_then(Value::as_str), Some("array")),
				schema: child,
			});
		}
		path.pop();
	}
}

fn scalar_kind(schema: &Value) -> ScalarKind {
	match schema.get("type").and_then(Value::as_str) {
		Some("string") => ScalarKind::String,
		Some("integer") => ScalarKind::Integer,
		Some("number") => ScalarKind::Number,
		Some("boolean") => ScalarKind::Boolean,
		Some("object") => ScalarKind::Object,
		Some("array") => schema
			.get("items")
			.map(scalar_kind)
			.unwrap_or(ScalarKind::Fallback),
		_ => ScalarKind::Fallback,
	}
}

fn flag_name(path: &[&str]) -> String {
	path
		.iter()
		.map(|segment| segment.replace('_', "-"))
		.collect::<Vec<_>>()
		.join(".")
}

fn value_usage(leaf: &SchemaLeaf<'_>) -> String {
	if let Some(values) = leaf.values {
		return format!(
			"{{{}}}",
			values
				.iter()
				.map(|value| value
					.as_str()
					.map_or_else(|| value.to_string(), str::to_owned))
				.collect::<Vec<_>>()
				.join("|")
		);
	}
	let kind = match leaf.kind {
		ScalarKind::String => "<STRING>",
		ScalarKind::Integer => "<INTEGER>",
		ScalarKind::Number => "<NUMBER>",
		ScalarKind::Boolean => "<BOOLEAN>",
		ScalarKind::Object => "<JSON_OBJECT>",
		ScalarKind::Fallback => "<JSON>",
	};
	if leaf.repeatable {
		format!("{kind}...")
	} else {
		kind.to_owned()
	}
}

#[derive(Debug, Error)]
enum ArgError {
	#[error("unknown flag `{flag}`")]
	UnknownFlag { flag: Str },
	#[error("flag `{flag}` requires a value")]
	MissingValue { flag: Str },
	#[error("invalid value for `{flag}`; expected {expected}")]
	InvalidValue { flag: Str, expected: &'static str },
	#[error("argument source `{origin}` is not a JSON object")]
	NotObject { origin: Str },
	#[error("failed to read argument source `{origin}`")]
	Read {
		origin: Str,
		#[source]
		error:  io::Error,
	},
	#[error("invalid JSON in argument source `{origin}`")]
	Json {
		origin: Str,
		#[source]
		error:  serde_json::Error,
	},
	#[error("missing required argument `{name}`")]
	MissingRequired { name: Str },
	#[error("unexpected argument `{argument}`")]
	Unexpected { argument: Str },
}

fn parse_args(
	schema: &Value,
	argv: &[Str],
	cwd: &Path,
	path_policy: Option<&dyn omp_shell_engine::PathPolicy>,
	stdin: &mut impl Read,
) -> Result<Map<String, Value>, ArgError> {
	let leaves = schema_leaves(schema);
	let mut output = Map::new();
	let mut index = 0;
	while index < argv.len() {
		let argument = argv[index].as_str();
		if argument == "-" || (argument.starts_with('@') && argument.len() > 1) {
			let source = if argument == "-" {
				let mut text = String::new();
				stdin
					.read_to_string(&mut text)
					.map_err(|error| ArgError::Read { origin: Str::new_static("<stdin>"), error })?;
				(Str::new_static("<stdin>"), text)
			} else {
				let raw_path = &argument[1..];
				let path = resolve(cwd, raw_path);
				if let Some(policy) = path_policy {
					policy.check_read(&path).map_err(|error| ArgError::Read {
						origin: Str::new(raw_path),
						error:  io::Error::other(error),
					})?;
				}
				let text = fs::read_to_string(&path)
					.map_err(|error| ArgError::Read { origin: Str::new(raw_path), error })?;
				(Str::new(raw_path), text)
			};
			merge_json_object(&mut output, &source.1, source.0)?;
			index += 1;
			continue;
		}
		if matches!(argument, "-j" | "--json") || argument.starts_with("--json=") {
			let (raw, consumed) = if let Some(raw) = argument.strip_prefix("--json=") {
				(raw, 1)
			} else {
				let raw = argv
					.get(index + 1)
					.ok_or_else(|| ArgError::MissingValue { flag: Str::new(argument) })?;
				(raw.as_str(), 2)
			};
			merge_json_object(&mut output, raw, Str::new_static("--json"))?;
			index += consumed;
			continue;
		}
		let Some(raw_flag) = argument.strip_prefix("--") else {
			return Err(ArgError::Unexpected { argument: Str::new(argument) });
		};
		let (raw_flag, inline) = raw_flag
			.split_once('=')
			.map_or((raw_flag, None), |(name, value)| (name, Some(value)));
		let (raw_flag, negative) = raw_flag
			.strip_prefix("no-")
			.map_or((raw_flag, false), |name| (name, true));
		let Some(leaf) = leaves.iter().find(|leaf| flag_name(&leaf.path) == raw_flag) else {
			return Err(ArgError::UnknownFlag { flag: Str::new(argument) });
		};
		if leaf.kind == ScalarKind::Boolean && leaf.values.is_none() {
			if inline.is_some() {
				return Err(ArgError::InvalidValue {
					flag:     Str::new(argument),
					expected: "a flag without a value",
				});
			}
			insert_value(&mut output, &leaf.path, Value::Bool(!negative), leaf.repeatable);
			index += 1;
			continue;
		}
		if negative {
			return Err(ArgError::UnknownFlag { flag: Str::new(argument) });
		}
		let raw = if let Some(raw) = inline {
			raw
		} else {
			index += 1;
			argv
				.get(index)
				.map(Str::as_str)
				.ok_or_else(|| ArgError::MissingValue { flag: Str::new(argument) })?
		};
		let value = coerce(leaf, raw, argument)?;
		insert_value(&mut output, &leaf.path, value, leaf.repeatable);
		index += 1;
	}
	validate_required(schema, &Value::Object(output.clone()), &mut Vec::new())?;
	Ok(output)
}

fn merge_json_object(
	output: &mut Map<String, Value>,
	raw: &str,
	source: Str,
) -> Result<(), ArgError> {
	let parsed: Value = serde_json::from_str(raw)
		.map_err(|error| ArgError::Json { origin: source.clone(), error })?;
	let Value::Object(values) = parsed else {
		return Err(ArgError::NotObject { origin: source });
	};
	merge_objects(output, values);
	Ok(())
}

fn merge_objects(target: &mut Map<String, Value>, source: Map<String, Value>) {
	for (name, value) in source {
		match (target.get_mut(&name), value) {
			(Some(Value::Object(existing)), Value::Object(incoming)) => {
				merge_objects(existing, incoming);
			},
			(_, value) => {
				target.insert(name, value);
			},
		}
	}
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
	let path = PathBuf::from(path);
	if path.is_absolute() {
		path
	} else {
		cwd.join(path)
	}
}

fn coerce(leaf: &SchemaLeaf<'_>, raw: &str, flag: &str) -> Result<Value, ArgError> {
	if let Some(values) = leaf.values {
		if let Some(value) = values.iter().find(|value| {
			value
				.as_str()
				.map_or_else(|| value.to_string() == raw, |value| value == raw)
		}) {
			return Ok(value.clone());
		}
		return Err(ArgError::InvalidValue { flag: Str::new(flag), expected: "an enum member" });
	}
	let kind_schema = if leaf.repeatable {
		leaf.schema.get("items").unwrap_or(&Value::Null)
	} else {
		leaf.schema
	};
	let value = match scalar_kind(kind_schema) {
		ScalarKind::String => Value::String(raw.to_owned()),
		ScalarKind::Integer => serde_json::from_str(raw)
			.ok()
			.filter(|value: &Value| value.as_i64().is_some() || value.as_u64().is_some())
			.ok_or_else(|| ArgError::InvalidValue {
				flag:     Str::new(flag),
				expected: "an integer",
			})?,
		ScalarKind::Number => serde_json::from_str(raw)
			.ok()
			.filter(Value::is_number)
			.ok_or_else(|| ArgError::InvalidValue {
				flag:     Str::new(flag),
				expected: "a number",
			})?,
		ScalarKind::Boolean => match raw {
			"true" => Value::Bool(true),
			"false" => Value::Bool(false),
			_ => {
				return Err(ArgError::InvalidValue {
					flag:     Str::new(flag),
					expected: "true or false",
				});
			},
		},
		ScalarKind::Object => serde_json::from_str(raw)
			.ok()
			.filter(Value::is_object)
			.ok_or_else(|| ArgError::InvalidValue {
				flag:     Str::new(flag),
				expected: "a JSON object",
			})?,
		ScalarKind::Fallback => {
			serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
		},
	};
	Ok(value)
}

fn insert_value(output: &mut Map<String, Value>, path: &[&str], value: Value, repeatable: bool) {
	let Some((last, parents)) = path.split_last() else {
		return;
	};
	let mut target = output;
	for parent in parents {
		let entry = target
			.entry((*parent).to_owned())
			.or_insert_with(|| Value::Object(Map::new()));
		if !entry.is_object() {
			*entry = Value::Object(Map::new());
		}
		let Value::Object(next) = entry else {
			unreachable!()
		};
		target = next;
	}
	if repeatable {
		let entry = target
			.entry((*last).to_owned())
			.or_insert_with(|| Value::Array(Vec::new()));
		if !entry.is_array() {
			*entry = Value::Array(Vec::new());
		}
		let Value::Array(values) = entry else {
			unreachable!()
		};
		values.push(value);
	} else {
		target.insert((*last).to_owned(), value);
	}
}

fn validate_required<'a>(
	schema: &'a Value,
	value: &Value,
	path: &mut Vec<&'a str>,
) -> Result<(), ArgError> {
	let Some(object) = value.as_object() else {
		return Ok(());
	};
	if let Some(required) = schema.get("required").and_then(Value::as_array) {
		for name in required.iter().filter_map(Value::as_str) {
			if !object.contains_key(name) {
				let mut full = path.join(".");
				if !full.is_empty() {
					full.push('.');
				}
				full.push_str(name);
				return Err(ArgError::MissingRequired { name: Str::new(full) });
			}
		}
	}
	if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
		for (name, child_schema) in properties {
			if let Some(child) = object.get(name) {
				path.push(name);
				validate_required(child_schema, child, path)?;
				path.pop();
			}
		}
	}
	Ok(())
}
