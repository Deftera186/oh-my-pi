//! Claude Code and Codex transcript import into native `.oms` journals.

use std::{
	fs,
	io::{BufRead as _, BufReader},
	path::{Path, PathBuf},
	time::SystemTime,
};

use miette::{IntoDiagnostic as _, miette};
use omp_core::Str;
use omp_dom::PropId;
use omp_session::{ComponentRegistry, Session};
use serde_json::Value;

use crate::cli::ChatArgs;

/// Foreign transcript dialect accepted by the one-shot importer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForeignFormat {
	/// Claude Code JSON-line events.
	Claude,
	/// Codex CLI rollout JSON-line events.
	Codex,
}

/// Resolves the newest requested foreign session, imports it, and rewrites the
/// launch to resume the resulting native journal.
pub(crate) fn prepare(args: &mut ChatArgs) -> miette::Result<()> {
	let format = if args.from_claude {
		ForeignFormat::Claude
	} else {
		ForeignFormat::Codex
	};
	let home = std::env::var_os("HOME")
		.map(PathBuf::from)
		.ok_or_else(|| miette!("HOME is unset"))?;
	let root = match format {
		ForeignFormat::Claude => home.join(".claude/projects"),
		ForeignFormat::Codex => home.join(".codex/sessions"),
	};
	let source = newest_jsonl(&root)?.ok_or_else(|| {
		miette!(
			"no importable {} sessions were found under {}",
			match format {
				ForeignFormat::Claude => "Claude Code",
				ForeignFormat::Codex => "Codex",
			},
			root.display(),
		)
	})?;
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let project = fs::canonicalize(&args.project).into_diagnostic()?;
	let state_dir =
		omp_env::project_state::directory(&data_dir, &project).map_err(|source| miette!(source))?;
	let sessions = args
		.session_dir
		.clone()
		.unwrap_or_else(|| state_dir.join("sessions"));
	fs::create_dir_all(&sessions).into_diagnostic()?;
	let destination = sessions.join(format!("{}.oms", omp_core::Ulid::generate()));
	let count = import_file(format, &source, &destination)?;
	if count == 0 {
		return Err(miette!(
			"{} contains no importable user or assistant messages",
			source.display()
		));
	}
	eprintln!(
		"Imported {} messages from {} into {}.",
		count,
		source.display(),
		destination.display()
	);
	args.resume = Some(Str::new(destination.to_string_lossy()));
	args.session_dir = Some(sessions);
	args.from_claude = false;
	args.from_codex = false;
	Ok(())
}

/// Imports one foreign JSONL fixture into a replayable `.oms` journal.
pub fn import_file(
	format: ForeignFormat,
	source: &Path,
	destination: &Path,
) -> miette::Result<usize> {
	if destination.extension().and_then(|value| value.to_str()) != Some("oms") {
		return Err(miette!("native session destination must use the .oms extension"));
	}
	if let Some(parent) = destination.parent() {
		fs::create_dir_all(parent).into_diagnostic()?;
	}
	let input = BufReader::new(fs::File::open(source).into_diagnostic()?);
	let mut messages = Vec::new();
	for (line_number, line) in input.lines().enumerate() {
		let line = line.into_diagnostic()?;
		if line.trim().is_empty() {
			continue;
		}
		let value: Value = serde_json::from_str(&line)
			.map_err(|source| miette!("invalid JSON on line {}: {source}", line_number + 1))?;
		if let Some(message) = foreign_message(format, &value) {
			messages.push(message);
		}
	}
	let mut session =
		Session::create(destination, ComponentRegistry::standard()).into_diagnostic()?;
	let mut turn_open = false;
	for (role, text) in &messages {
		match *role {
			"user" => {
				session.begin_turn().into_diagnostic()?;
				session.user(text.as_str(), Vec::new()).into_diagnostic()?;
				turn_open = true;
			},
			"assistant" => {
				if !turn_open {
					session.begin_turn().into_diagnostic()?;
					session.user("", Vec::new()).into_diagnostic()?;
				}
				session
					.assistant_start("imported", "foreign", "foreign/imported")
					.into_diagnostic()?;
				let turn = *session
					.dom()
					.children(session.dom().body())
					.last()
					.ok_or_else(|| miette!("imported turn is absent"))?;
				let assistant = session
					.dom()
					.children(turn)
					.iter()
					.copied()
					.find(|handle| {
						session.dom().get(*handle).is_some_and(|node| {
							node.tag == omp_dom::Tag::Known(omp_dom::KnownTag::Assistant)
						})
					})
					.ok_or_else(|| miette!("imported assistant node is absent"))?;
				let sid = session
					.stream_open(assistant, PropId::Text.into())
					.into_diagnostic()?;
				session
					.stream_append(sid, text.as_str())
					.into_diagnostic()?;
				session.stream_close(sid).into_diagnostic()?;
				session.assistant_end("imported").into_diagnostic()?;
				turn_open = false;
			},
			_ => {},
		}
	}
	session.process_exit().into_diagnostic()?;
	Ok(messages.len())
}

fn foreign_message(format: ForeignFormat, value: &Value) -> Option<(&'static str, Str)> {
	match format {
		ForeignFormat::Claude => {
			let role = value
				.get("type")
				.and_then(Value::as_str)
				.or_else(|| value.pointer("/message/role").and_then(Value::as_str))?;
			let role = match role {
				"user" | "human" => "user",
				"assistant" => "assistant",
				_ => return None,
			};
			let content = value
				.pointer("/message/content")
				.or_else(|| value.get("content"))?;
			text_content(content).map(|text| (role, text))
		},
		ForeignFormat::Codex => {
			let payload = value.get("payload").unwrap_or(value);
			if payload
				.get("type")
				.and_then(Value::as_str)
				.is_some_and(|kind| !matches!(kind, "message" | "user_message" | "assistant_message"))
			{
				return None;
			}
			let role = payload.get("role").and_then(Value::as_str).or_else(|| {
				match payload.get("type").and_then(Value::as_str) {
					Some("user_message") => Some("user"),
					Some("assistant_message") => Some("assistant"),
					_ => None,
				}
			})?;
			let role = match role {
				"user" => "user",
				"assistant" => "assistant",
				_ => return None,
			};
			let content = payload.get("content").or_else(|| payload.get("message"))?;
			text_content(content).map(|text| (role, text))
		},
	}
}

fn text_content(value: &Value) -> Option<Str> {
	if let Some(text) = value.as_str() {
		return Some(Str::new(text));
	}
	let parts = value.as_array()?;
	let mut text = String::new();
	for part in parts {
		if let Some(value) = part
			.as_str()
			.or_else(|| part.get("text").and_then(Value::as_str))
		{
			text.push_str(value);
		}
	}
	(!text.is_empty()).then(|| Str::new(text))
}

fn newest_jsonl(root: &Path) -> miette::Result<Option<PathBuf>> {
	let mut stack = vec![root.to_path_buf()];
	let mut newest: Option<(SystemTime, PathBuf)> = None;
	while let Some(directory) = stack.pop() {
		let entries = match fs::read_dir(&directory) {
			Ok(entries) => entries,
			Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
			Err(source) => return Err(source).into_diagnostic(),
		};
		for entry in entries {
			let entry = entry.into_diagnostic()?;
			let path = entry.path();
			let metadata = entry.metadata().into_diagnostic()?;
			if metadata.is_dir() {
				stack.push(path);
			} else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
				let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
				if newest
					.as_ref()
					.is_none_or(|(current, _)| modified > *current)
				{
					newest = Some((modified, path));
				}
			}
		}
	}
	Ok(newest.map(|(_, path)| path))
}
