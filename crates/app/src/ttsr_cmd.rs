//! Standalone inspection and testing for the active TTSR registry.

use std::{env, fs, io, io::Read as _, path::Path};

use miette::{IntoDiagnostic as _, miette};
use omp_agent::{StreamSource, TtsrMatchContext, TtsrRegistry};
use omp_walker::WalkRequest;
use serde_json::json;

use crate::cli::{TtsrArgs, TtsrCommand, TtsrSourceArg};

/// Lists active rules or evaluates snippets/files without starting a session.
pub fn run(args: TtsrArgs) -> miette::Result<()> {
	let root = args.root.unwrap_or(env::current_dir().into_diagnostic()?);
	let content = omp_driver::discovery::active_content_snapshots(&root);
	let (mut registry, diagnostics) = omp_driver::rulebook::ttsr_registry(content.rules.as_ref());
	for diagnostic in diagnostics {
		eprintln!("warning: {diagnostic}");
	}
	match args.command.unwrap_or(TtsrCommand::List { json: false }) {
		TtsrCommand::List { json } => list(&registry, json),
		TtsrCommand::Test { snippet, file, rule, source, tool, path, verbose, json } => {
			let text = input(snippet, file.as_deref())?;
			let candidate = path
				.as_deref()
				.or(file.as_deref().and_then(Path::to_str))
				.unwrap_or("snippet.txt");
			evaluate(&mut registry, &text, candidate, &rule, source, &tool, verbose, json)
		},
		TtsrCommand::Scan { directory, rule, no_gitignore, max_bytes, json } => {
			scan(&mut registry, &directory, &rule, no_gitignore, max_bytes, json)
		},
	}
}

fn list(registry: &TtsrRegistry, json_output: bool) -> miette::Result<()> {
	let rows = registry
		.rules()
		.map(|rule| {
			json!({
				"name": rule.name,
				"conditions": rule.conditions,
				"astConditions": rule.ast_conditions,
				"scopes": rule.scopes,
				"globs": rule.globs,
			})
		})
		.collect::<Vec<_>>();
	if json_output {
		println!("{}", serde_json::to_string_pretty(&rows).into_diagnostic()?);
	} else if rows.is_empty() {
		println!("no active TTSR rules");
	} else {
		for row in rows {
			println!("{}", row["name"].as_str().unwrap_or_default());
		}
	}
	Ok(())
}

#[allow(clippy::too_many_arguments, reason = "CLI selectors project directly into matcher context")]
fn evaluate(
	registry: &mut TtsrRegistry,
	text: &str,
	path: &str,
	rule_filter: &Option<String>,
	source: TtsrSourceArg,
	tool: &str,
	verbose: bool,
	json_output: bool,
) -> miette::Result<()> {
	let paths = [path];
	let source = match source {
		TtsrSourceArg::Text => StreamSource::Text,
		TtsrSourceArg::Thinking => StreamSource::Thinking,
		TtsrSourceArg::Tool => StreamSource::Tool,
	};
	let context = TtsrMatchContext {
		source,
		tool_name: (source == StreamSource::Tool).then_some(tool),
		file_paths: &paths,
		stream_key: Some(path),
	};
	let mut matches = registry.check_snapshot(text, context).into_vec();
	if source == StreamSource::Tool && registry.has_ast_rules() {
		matches.extend(
			registry
				.check_ast_snapshot(text, context)
				.into_diagnostic()?,
		);
	}
	if let Some(filter) = rule_filter {
		matches.retain(|matched| matched.name.as_str() == filter);
	}
	let rows = matches
		.into_iter()
		.map(|matched| {
			json!({
				"rule": matched.name,
				"interruptMode": matched.interrupt_mode.to_string(),
				"content": verbose.then_some(matched.content),
				"path": path,
			})
		})
		.collect::<Vec<_>>();
	if json_output {
		println!("{}", serde_json::to_string_pretty(&rows).into_diagnostic()?);
	} else {
		for row in rows {
			println!("{}: {}", path, row["rule"].as_str().unwrap_or_default());
			if verbose && let Some(content) = row["content"].as_str() {
				println!("  {content}");
			}
		}
	}
	Ok(())
}

fn scan(
	registry: &mut TtsrRegistry,
	directory: &Path,
	rule: &Option<String>,
	no_gitignore: bool,
	max_bytes: u64,
	json_output: bool,
) -> miette::Result<()> {
	let candidates = WalkRequest::new(directory)
		.gitignore(!no_gitignore)
		.hidden(true)
		.collect_file_candidates()
		.map_err(|error| miette!(error))?;
	let mut rows = Vec::new();
	for candidate in candidates {
		if candidate.size.is_some_and(|size| size > max_bytes as f64) {
			continue;
		}
		let metadata = fs::metadata(&candidate.path).into_diagnostic()?;
		if metadata.len() > max_bytes {
			continue;
		}
		let Ok(text) = fs::read_to_string(&candidate.path) else {
			continue;
		};
		let path = candidate.path.to_string_lossy();
		let paths = [path.as_ref()];
		let context = TtsrMatchContext {
			source:     StreamSource::Tool,
			tool_name:  Some("edit"),
			file_paths: &paths,
			stream_key: Some(path.as_ref()),
		};
		let mut matches = registry.check_snapshot(&text, context).into_vec();
		if registry.has_ast_rules() {
			matches.extend(
				registry
					.check_ast_snapshot(&text, context)
					.into_diagnostic()?,
			);
		}
		rows.extend(
			matches
				.into_iter()
				.filter(|matched| {
					rule
						.as_ref()
						.is_none_or(|filter| matched.name.as_str() == filter)
				})
				.map(|matched| json!({ "path": path, "rule": matched.name })),
		);
	}
	if json_output {
		println!("{}", serde_json::to_string_pretty(&rows).into_diagnostic()?);
	} else {
		for row in rows {
			println!("{}: {}", row["path"].as_str().unwrap_or_default(), row["rule"]);
		}
	}
	Ok(())
}

fn input(snippet: Option<String>, file: Option<&Path>) -> miette::Result<String> {
	match (snippet, file) {
		(Some(_), Some(_)) => Err(miette!("snippet and --file are mutually exclusive")),
		(Some(snippet), None) => Ok(snippet),
		(None, Some(path)) if path == Path::new("-") => {
			let mut text = String::new();
			io::stdin().read_to_string(&mut text).into_diagnostic()?;
			Ok(text)
		},
		(None, Some(path)) => fs::read_to_string(path).into_diagnostic(),
		(None, None) => {
			let mut text = String::new();
			io::stdin().read_to_string(&mut text).into_diagnostic()?;
			Ok(text)
		},
	}
}
