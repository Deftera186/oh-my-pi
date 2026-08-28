use omp_chat_ui::{
	ExtensionDetail, ExtensionDisposition, ExtensionKind, ExtensionOrigin, ExtensionRow,
	ExtensionSnapshot, LiveToolView, McpCatalogEntry, McpHealth, McpLiveSnapshot, McpToolView,
};
use omp_core::{Str, sf};
use omp_driver::discovery::manifest::{
	CapabilityPayload, DiscoveredCapability, McpTransport, SourceScope,
};

pub(crate) fn snapshot_live_mcp(
	environment: &omp_envd::McpInspectorHandle,
) -> Vec<McpLiveSnapshot> {
	environment
		.snapshots()
		.into_iter()
		.map(|snapshot| McpLiveSnapshot {
			server:           snapshot.server,
			health:           match snapshot.health {
				omp_envd::mcp::manager::McpInspectorHealth::Connecting => McpHealth::Connecting,
				omp_envd::mcp::manager::McpInspectorHealth::Connected => McpHealth::Connected,
				omp_envd::mcp::manager::McpInspectorHealth::Disconnected => McpHealth::Disconnected,
				omp_envd::mcp::manager::McpInspectorHealth::Failed => McpHealth::Failed,
			},
			generation:       snapshot.generation,
			definition_epoch: snapshot.definition_epoch,
			implementation:   snapshot.implementation,
			version:          snapshot.version,
			title:            snapshot.title,
			description:      snapshot.description,
			instructions:     snapshot.instructions,
			tools:            snapshot
				.tools
				.iter()
				.filter_map(|tool| {
					let name = tool.get("name")?.as_str()?;
					let title = tool
						.get("title")
						.and_then(serde_json::Value::as_str)
						.or_else(|| {
							tool
								.get("annotations")
								.and_then(|value| value.get("title"))
								.and_then(serde_json::Value::as_str)
						})
						.map(Str::new);
					Some(McpToolView {
						name: Str::new(name),
						title,
						description: tool
							.get("description")
							.and_then(serde_json::Value::as_str)
							.map(Str::new),
						input_schema: tool
							.get("inputSchema")
							.cloned()
							.unwrap_or_else(|| serde_json::json!({"type":"object","properties":{}})),
					})
				})
				.collect(),
			resources:        snapshot
				.resources
				.iter()
				.map(|resource| McpCatalogEntry {
					name:        resource.name.clone(),
					title:       None,
					description: resource.description.clone(),
				})
				.collect(),
			prompts:          snapshot
				.prompts
				.iter()
				.map(|prompt| McpCatalogEntry {
					name:        prompt.name.clone(),
					title:       None,
					description: prompt.description.clone(),
				})
				.collect(),
		})
		.collect()
}

pub(crate) fn build_inspector_snapshot_from_declarations(
	declarations: &[DiscoveredCapability],
	live_tools: &[LiveToolView],
	live_mcp: &[McpLiveSnapshot],
	generation: u64,
) -> ExtensionSnapshot {
	let mut snapshot = ExtensionSnapshot { rows: Vec::new(), generation };
	let mut claims = std::collections::BTreeMap::<(ExtensionKind, Str), Str>::new();
	for declaration in declarations {
		let Some((kind, name, description, detail)) = payload_view(&declaration.payload) else {
			continue;
		};
		let path = Str::from(declaration.source.path.to_string_lossy().as_ref());
		let disposition = if let Some(key) = declaration.key.as_ref() {
			let claim_key = (kind, key.clone());
			if let Some(by) = claims.get(&claim_key) {
				ExtensionDisposition::Shadowed { by: by.clone() }
			} else {
				claims.insert(claim_key, path.clone());
				if declaration.enabled {
					ExtensionDisposition::Winner
				} else {
					ExtensionDisposition::Disabled {
						reason: Str::new_static("disabled by source policy"),
					}
				}
			}
		} else if declaration.enabled {
			ExtensionDisposition::Winner
		} else {
			ExtensionDisposition::Disabled { reason: Str::new_static("disabled by source policy") }
		};
		snapshot.rows.push(ExtensionRow {
			id: sf!("{kind}:{name}:{}", path),
			kind,
			name,
			description,
			origin: ExtensionOrigin {
				provider_id: declaration.source.source_id.clone(),
				provider_name: declaration.source.source_id.clone(),
				project: project_label(path.as_str(), declaration.source.scope),
				path,
				scope: Str::from(declaration.source.scope.to_string()),
				read_only: declaration.source.read_only,
			},
			disposition,
			detail,
			live_tools: Vec::new(),
			mcp: None,
		});
	}
	snapshot.join_live_tools(live_tools);
	for live in live_mcp {
		snapshot.merge_mcp(live.clone());
	}
	snapshot
}

fn payload_view(
	payload: &CapabilityPayload,
) -> Option<(ExtensionKind, Str, Option<Str>, ExtensionDetail)> {
	match payload {
		CapabilityPayload::Extensions(value) => Some((
			ExtensionKind::Extension,
			value.name.clone(),
			value.description.clone(),
			ExtensionDetail::None,
		)),
		CapabilityPayload::Tools(value) => Some((
			ExtensionKind::Tool,
			value.name.clone(),
			Some(value.description.clone()),
			ExtensionDetail::Tool {
				description:  Some(value.description.clone()),
				input_schema: value.input_schema.clone(),
			},
		)),
		CapabilityPayload::Mcps(value) => {
			let connection = &value.connection;
			let endpoint = connection.command.as_ref().map_or_else(
				|| connection.url.clone(),
				|command| Some(Str::from(command.to_string_lossy().as_ref())),
			);
			Some((ExtensionKind::Mcp, value.name.clone(), None, ExtensionDetail::Mcp {
				transport: Str::from(connection.transport.map_or_else(
					|| {
						if connection.command.is_some() {
							"stdio"
						} else {
							"http"
						}
					},
					|value| match value {
						McpTransport::Stdio => "stdio",
						McpTransport::Sse => "sse",
						McpTransport::Http => "http",
					},
				)),
				endpoint,
				args: connection.args.clone(),
				env_count: connection.env.len(),
			}))
		},
		CapabilityPayload::Skills(value) => {
			let mut facts = Vec::new();
			if value.frontmatter.hidden || value.frontmatter.disable_model_invocation {
				facts.push((Str::new_static("discovery"), Str::new_static("hidden")));
			}
			if value.frontmatter.always_apply {
				facts.push((Str::new_static("applies"), Str::new_static("always")));
			}
			if !value.frontmatter.globs.is_empty() {
				facts.push((Str::new_static("globs"), join(&value.frontmatter.globs)));
			}
			Some((
				ExtensionKind::Skill,
				value.name.clone(),
				value.frontmatter.description.clone(),
				ExtensionDetail::Document {
					heading: Str::new_static("Instruction"),
					body: value.content.clone(),
					facts,
				},
			))
		},
		CapabilityPayload::Rules(value) => {
			let mut facts = Vec::new();
			if value.always_apply {
				facts.push((Str::new_static("applies"), Str::new_static("always")));
			}
			for (label, values) in [
				("globs", value.globs.as_slice()),
				("condition", value.conditions.as_slice()),
				("ast", value.ast_conditions.as_slice()),
				("scope", value.scopes.as_slice()),
			] {
				if !values.is_empty() {
					facts.push((Str::new(label), join(values)));
				}
			}
			if let Some(mode) = value.interrupt_mode {
				facts.push((Str::new_static("interrupt"), Str::from(mode.to_string())));
			}
			Some((
				ExtensionKind::Rule,
				value.name.clone(),
				value.description.clone(),
				ExtensionDetail::Document {
					heading: Str::new_static("Rule"),
					body: value.content.clone(),
					facts,
				},
			))
		},
		CapabilityPayload::SlashCommands(value) => Some((
			ExtensionKind::SlashCommand,
			value.name.clone(),
			Some(value.description.clone()),
			ExtensionDetail::SlashCommand {
				description:   Some(value.description.clone()),
				argument_hint: value.argument_hint.clone(),
				body:          value.content.clone(),
			},
		)),
		CapabilityPayload::Hooks(value) => {
			Some((ExtensionKind::Hook, value.name.clone(), None, ExtensionDetail::Hook {
				phase: Str::from(value.phase.to_string()),
				tool:  value.tool.clone(),
			}))
		},
		CapabilityPayload::Prompts(value) => {
			Some((ExtensionKind::Prompt, value.name.clone(), None, ExtensionDetail::Document {
				heading: Str::new_static("Prompt"),
				body:    value.content.clone(),
				facts:   Vec::new(),
			}))
		},
		CapabilityPayload::ContextFiles(value) => Some((
			ExtensionKind::ContextFile,
			Str::from(
				value
					.path
					.file_name()
					.and_then(|name| name.to_str())
					.unwrap_or("context"),
			),
			None,
			ExtensionDetail::Document {
				heading: Str::new_static("Preview"),
				body:    value.content.clone(),
				facts:   Vec::new(),
			},
		)),
		CapabilityPayload::Instructions(value) => {
			let facts = value
				.apply_to
				.as_ref()
				.map(|apply| vec![(Str::new_static("files"), apply.clone())])
				.unwrap_or_default();
			Some((ExtensionKind::Instruction, value.name.clone(), None, ExtensionDetail::Document {
				heading: Str::new_static("Instruction"),
				body: value.content.clone(),
				facts,
			}))
		},
		CapabilityPayload::Settings(_)
		| CapabilityPayload::Themes(_)
		| CapabilityPayload::Ssh(_)
		| CapabilityPayload::SystemPrompt(_)
		| CapabilityPayload::Agents(_) => None,
	}
}

fn join(values: &[Str]) -> Str {
	Str::from(
		values
			.iter()
			.map(Str::as_str)
			.collect::<Vec<_>>()
			.join(", "),
	)
}

fn project_label(path: &str, scope: SourceScope) -> Option<Str> {
	if scope != SourceScope::Project {
		return None;
	}
	let normalized = path.replace('\\', "/");
	let parts = normalized
		.split('/')
		.filter(|part| !part.is_empty())
		.collect::<Vec<_>>();
	let index = parts.iter().rposition(|part| *part == ".omp")?;
	index
		.checked_sub(1)
		.and_then(|index| parts.get(index))
		.map(|part| Str::new(*part))
}
