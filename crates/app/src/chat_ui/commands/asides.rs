//! Ephemeral asides, tangential agents, and generated TTSR rules.

use std::{fs, path::PathBuf, sync::Arc};

use miette::miette;
use omp_agent::{AgentEvent, TtsrRegistry, TtsrRule, TtsrSettings};
use omp_core::{Str, sf};
use omp_envd::eval::{NoopBridgeProgress, ParentSessionHost};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::BackendEvent;

const BTW_SYSTEM: &str = "You are answering an ephemeral side question for the current \
                          interactive session. Answer briefly and directly. Do not use tools and \
                          do not ask follow-up questions.";
const OMFG_SYSTEM: &str = r#"Generate one Time-Traveling Stream Rule (TTSR) from the user's complaint.

A TTSR rule is Markdown with YAML frontmatter. `condition` contains Rust-compatible regex patterns tested against streamed assistant output. `scope` is a narrow allowlist: `text`, `thinking`, `tool`, or `tool:<name>(<glob>)` such as `tool:edit(*.rs)`. When a condition matches within scope, the Markdown body is injected as correction guidance.

Return exactly the requested object. The name must be lowercase kebab-case. Keep regexes precise rather than using catch-alls. Prefer file-specific tool scopes for code complaints. The body must concisely state the correct behavior."#;

#[derive(Debug, Deserialize)]
struct GeneratedRule {
	name:        String,
	description: String,
	condition:   StringList,
	scope:       StringList,
	body:        String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringList {
	One(String),
	Many(Vec<String>),
}

impl StringList {
	fn normalized(self, field: &str) -> miette::Result<Vec<String>> {
		let values = match self {
			Self::One(value) => vec![value],
			Self::Many(values) => values,
		};
		let mut normalized = Vec::with_capacity(values.len());
		for value in values {
			let value = value.trim();
			if !value.is_empty() && !normalized.iter().any(|item| item == value) {
				normalized.push(value.to_owned());
			}
		}
		if normalized.is_empty() {
			return Err(miette!("generated TTSR rule has no {field}"));
		}
		Ok(normalized)
	}
}

#[derive(Serialize)]
struct RuleHeader<'a> {
	description: &'a str,
	condition:   &'a [String],
	scope:       &'a [String],
}

pub(crate) async fn ask_btw(parent: &dyn ParentSessionHost, question: &str) -> miette::Result<Str> {
	let response = parent
		.completion(
			json!({
				"prompt": format!("<btw>\nEphemeral side question for the current session:\n{}\n</btw>", question.trim()),
				"system": BTW_SYSTEM,
				"model": "default",
			}),
			&NoopBridgeProgress,
		)
		.await
		.map_err(|error| miette!(error.to_string()))?;
	response_text(&response)
}

pub(crate) fn spawn_tan(
	parent: Arc<dyn ParentSessionHost>,
	backend: flume::Sender<BackendEvent>,
	bus: omp_agent::EventBus,
	work: Str,
) -> Str {
	let job_id = sf!("Tan-{}", omp_core::Ulid::generate());
	bus.publish(AgentEvent::JobRegistered { job_id: job_id.clone() });
	let task_id = job_id.clone();
	drop(tokio::spawn(async move {
		let request = json!({
			"prompt": format!(
				"Work exclusively on this tangential request. Use the available tools when useful, then return a concise final answer for the parent user.\n\n{}",
				work.trim()
			),
			"agent": "task",
			"stableId": task_id.as_str(),
			"name": "Tan",
		});
		match parent.agent(request, &NoopBridgeProgress).await {
			Ok(value) => {
				let text = value
					.get("text")
					.and_then(Value::as_str)
					.map(str::trim)
					.filter(|text| !text.is_empty())
					.unwrap_or("(no output)");
				let _ = backend.send(BackendEvent::Notice(sf!(
					"**Background tan `{}` completed**\n\n{}\n\nResult: `agent://{}`",
					task_id,
					text,
					task_id
				)));
			},
			Err(error) => {
				let _ = backend.send(BackendEvent::Error(sf!(
					"Background tan `{}` failed: {}",
					task_id,
					error
				)));
			},
		}
		bus.publish(AgentEvent::JobSettled { job_id: task_id });
	}));
	job_id
}

pub(crate) async fn forge_ttsr(
	parent: &dyn ParentSessionHost,
	workspace_root: PathBuf,
	instruction: &str,
) -> miette::Result<PathBuf> {
	let schema = json!({
		"type": "object",
		"additionalProperties": false,
		"required": ["name", "description", "condition", "scope", "body"],
		"properties": {
			"name": { "type": "string" },
			"description": { "type": "string" },
			"condition": {
				"oneOf": [
					{ "type": "string" },
					{ "type": "array", "items": { "type": "string" }, "minItems": 1 }
				]
			},
			"scope": {
				"oneOf": [
					{ "type": "string" },
					{ "type": "array", "items": { "type": "string" }, "minItems": 1 }
				]
			},
			"body": { "type": "string" }
		}
	});
	let response = parent
		.completion(
			json!({
				"prompt": format!("Complaint or corrective instruction:\n{}", instruction.trim()),
				"system": OMFG_SYSTEM,
				"model": "default",
				"schema": schema,
				"max_output_tokens": 1200,
			}),
			&NoopBridgeProgress,
		)
		.await
		.map_err(|error| miette!(error.to_string()))?;
	let generated: GeneratedRule = match response.get("data") {
		Some(data) if !data.is_null() => serde_json::from_value(data.clone()),
		_ => serde_json::from_str(response_text(&response)?.as_str()),
	}
	.map_err(|error| miette!("model returned an invalid TTSR rule: {error}"))?;
	let name = generated.name.trim();
	if !valid_rule_name(name) {
		return Err(miette!("generated TTSR rule name is not lowercase kebab-case: {name}"));
	}
	let description = generated.description.trim();
	if description.is_empty() || description.contains('\n') {
		return Err(miette!("generated TTSR rule description must be one non-empty line"));
	}
	let body = generated.body.trim();
	if body.is_empty() {
		return Err(miette!("generated TTSR rule body is empty"));
	}
	let conditions = generated.condition.normalized("conditions")?;
	let scopes = generated.scope.normalized("scopes")?;
	let rule = TtsrRule {
		name:           Str::new(name),
		content:        Str::new(body),
		conditions:     conditions.iter().map(Str::from).collect(),
		ast_conditions: Vec::new(),
		scopes:         scopes.iter().map(Str::from).collect(),
		globs:          Vec::new(),
		interrupt_mode: None,
	};
	let (registry, diagnostics) = TtsrRegistry::from_layers(TtsrSettings::default(), [rule], []);
	if registry.rules().len() != 1 || !diagnostics.is_empty() {
		let detail = diagnostics
			.first()
			.map_or_else(|| "rule has no reachable stream scope".to_owned(), ToString::to_string);
		return Err(miette!("generated TTSR rule is invalid: {detail}"));
	}
	let header =
		serde_yaml::to_string(&RuleHeader { description, condition: &conditions, scope: &scopes })
			.map_err(|error| miette!("could not serialize generated TTSR rule: {error}"))?;
	let content = format!("---\n{header}---\n\n{body}\n");
	let target = workspace_root
		.join(".omp")
		.join("rules")
		.join(format!("{name}.md"));
	let written = target.clone();
	tokio::task::spawn_blocking(move || -> miette::Result<()> {
		let Some(directory) = written.parent() else {
			return Err(miette!("generated TTSR rule path has no parent"));
		};
		fs::create_dir_all(directory)
			.map_err(|error| miette!("could not create {}: {error}", directory.display()))?;
		fs::write(&written, content)
			.map_err(|error| miette!("could not write {}: {error}", written.display()))
	})
	.await
	.map_err(|error| miette!("TTSR rule write task failed: {error}"))??;
	Ok(target)
}

fn response_text(response: &Value) -> miette::Result<Str> {
	response
		.get("text")
		.and_then(Value::as_str)
		.map(str::trim)
		.filter(|text| !text.is_empty())
		.map(Str::from)
		.ok_or_else(|| miette!("model returned no text"))
}

fn valid_rule_name(name: &str) -> bool {
	!name.is_empty()
		&& name.len() <= 80
		&& name.split('-').all(|part| {
			!part.is_empty()
				&& part
					.bytes()
					.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
		})
}
