//! Bundled task-agent materialization command.

use std::{collections::BTreeMap, env, fs, path::PathBuf};

use miette::{IntoDiagnostic as _, miette};
use omp_agent::{AgentDefinition, SpawnPolicy};
use serde::Serialize;

use crate::cli::AgentsArgs;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UnpackResult {
	target_dir: PathBuf,
	total:      usize,
	written:    Vec<PathBuf>,
	skipped:    Vec<PathBuf>,
}

/// Writes the build-time bundled agent definitions to the selected discovery
/// layer.
pub(crate) fn run(args: AgentsArgs) -> miette::Result<()> {
	match args.action {
		crate::cli::AgentsAction::Unpack => {},
	}
	let target = target_dir(&args)?;
	fs::create_dir_all(&target).into_diagnostic()?;
	let definitions = omp_driver::chat::agents::bundled();
	let mut written = Vec::new();
	let mut skipped = Vec::new();
	for (name, definition) in definitions.iter() {
		let path = target.join(format!("{name}.md"));
		if path.exists() && !args.force {
			skipped.push(path);
			continue;
		}
		fs::write(&path, serialize_agent(definition)?).into_diagnostic()?;
		written.push(path);
	}
	let result = UnpackResult { target_dir: target, total: definitions.len(), written, skipped };
	if args.json {
		serde_json::to_writer_pretty(std::io::stdout().lock(), &result).into_diagnostic()?;
		println!();
	} else {
		println!("Bundled agents: {}", result.total);
		println!("Target directory: {}", result.target_dir.display());
		println!("Written: {}", result.written.len());
		if !result.skipped.is_empty() {
			println!("Skipped existing: {} (use --force to overwrite)", result.skipped.len());
		}
		for path in &result.written {
			println!("  + {}", path.display());
		}
		for path in &result.skipped {
			println!("  = {}", path.display());
		}
	}
	Ok(())
}

fn target_dir(args: &AgentsArgs) -> miette::Result<PathBuf> {
	if args.user && args.project {
		return Err(miette!("choose either --user or --project, not both"));
	}
	let cwd = env::current_dir().into_diagnostic()?;
	if let Some(path) = &args.dir {
		return Ok(if path.is_absolute() {
			path.clone()
		} else {
			cwd.join(path)
		});
	}
	if args.project {
		return Ok(cwd.join(".omp/agents"));
	}
	Ok(omp_core::dirs::data_dir(None)
		.into_diagnostic()?
		.join("agents"))
}

fn serialize_agent(agent: &AgentDefinition) -> miette::Result<String> {
	let mut fields = BTreeMap::<String, serde_yaml::Value>::new();
	fields.insert("name".into(), serde_yaml::to_value(agent.name.as_str()).into_diagnostic()?);
	fields.insert(
		"description".into(),
		serde_yaml::to_value(agent.description.as_str()).into_diagnostic()?,
	);
	if !agent.tools.is_empty() {
		fields.insert("tools".into(), serde_yaml::to_value(&agent.tools).into_diagnostic()?);
	}
	match &agent.spawns {
		SpawnPolicy::Disabled => {},
		SpawnPolicy::Any => {
			fields.insert("spawns".into(), serde_yaml::to_value("*").into_diagnostic()?);
		},
		SpawnPolicy::Only(names) => {
			fields.insert("spawns".into(), serde_yaml::to_value(names).into_diagnostic()?);
		},
	}
	if let Some(model) = &agent.model {
		fields.insert("model".into(), serde_yaml::to_value(model.as_str()).into_diagnostic()?);
	}
	if let Some(level) = &agent.thinking_level {
		fields
			.insert("thinkingLevel".into(), serde_yaml::to_value(level.as_str()).into_diagnostic()?);
	}
	if let Some(schema) = &agent.output_schema {
		fields.insert("output".into(), serde_yaml::to_value(schema).into_diagnostic()?);
	}
	if agent.blocking {
		fields.insert("blocking".into(), serde_yaml::Value::Bool(true));
	}
	let frontmatter = serde_yaml::to_string(&fields).into_diagnostic()?;
	Ok(format!(
		"---\n{}\n---\n\n{}\n",
		frontmatter.trim_start_matches("---\n").trim_end(),
		agent.prompt.trim()
	))
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn serialized_bundled_agent_round_trips() {
		let definitions = omp_driver::chat::agents::bundled();
		let (name, definition) = definitions.iter().next().unwrap();
		let markdown = serialize_agent(definition).unwrap();
		let parsed =
			AgentDefinition::parse_markdown(omp_core::Str::new(name.as_str()), &markdown).unwrap();
		assert_eq!(parsed.name, definition.name);
		assert_eq!(parsed.prompt.trim(), definition.prompt.trim());
	}
}
