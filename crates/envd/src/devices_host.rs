//! Envd-owned authority bridges behind the embedded shell's `dyn` builtin.

use std::{collections::BTreeSet, fmt::Write as _, sync::Arc};

use bytes::Bytes;
use futures::StreamExt as _;
use omp_agent::{GateEvent, GateOutcome, HookGate};
use omp_core::{Duration, DurationUnit, Str, sf};
use omp_proto::toolhost::v1::HookEventId;
use omp_shell_builtins::{
	DynDevice, DynFault, DynFuture, DynHost as ShellDynHost, DynOutput, DynSchema,
};
use omp_tool::{
	DevicePath, ErasedEv, ErasedOutcome, ErasedStream, IncomingParams, Part, PromptCaps, Registry,
	RegistryError, ToolIdentity, ToolRoute,
};
use omp_tools::{
	device::{DeviceCatalog, DeviceInvokeRequest, ErasedDeviceInvoker},
	staging::{ProposalDecision, ProposalRejection, StagedProposalRegistry},
};
use serde_json::{Map, Value, json};

/// Envd-owned loopback bridge behind the `dyn` shell builtin.
pub struct DynHost {
	catalog:            DeviceCatalog,
	invoker:            Arc<dyn ErasedDeviceInvoker>,
	proposals:          StagedProposalRegistry,
	hooks:              Arc<HookGate>,
	next_invocation_id: std::sync::atomic::AtomicU64,
}

impl DynHost {
	/// Binds one live device catalog, worker dispatcher, proposal registry, and
	/// session hook gate.
	pub fn new(
		catalog: DeviceCatalog,
		invoker: Arc<dyn ErasedDeviceInvoker>,
		proposals: StagedProposalRegistry,
		hooks: Arc<HookGate>,
	) -> Self {
		Self {
			catalog,
			invoker,
			proposals,
			hooks,
			next_invocation_id: std::sync::atomic::AtomicU64::new(1),
		}
	}

	fn invocation_id(&self) -> Str {
		let sequence = self
			.next_invocation_id
			.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		sf!("dyn-{sequence}")
	}

	async fn visible_names(&self, registry: &Registry) -> Result<Option<BTreeSet<Str>>, DynFault> {
		if !self.hooks.subscribed(HookEventId::HookEventDeviceList) {
			return Ok(None);
		}
		let device_hash = registry.device_hash();
		let devices = registry
			.devices()
			.map(device_event_json)
			.collect::<Vec<_>>();
		let payload = serde_json::to_vec(&json!({ "devices": devices, "turn_id": null }))
			.map(Bytes::from)
			.map_err(|_| DynFault::new("failed to encode the dynamic-device catalog"))?;
		let outcome = self
			.hooks
			.gate(
				HookEventId::HookEventDeviceList,
				GateEvent::new(sf!("device_list:{}", device_hash.to_hex()), payload),
			)
			.await;
		let effective = match outcome {
			GateOutcome::Allow { event, .. } => event.effective_args,
			GateOutcome::Deny { reason, .. } => return Err(DynFault::new(reason)),
			GateOutcome::Approval { .. } => {
				return Err(DynFault::new("device listing cannot require approval"));
			},
		};
		let effective: Value = serde_json::from_slice(&effective)
			.map_err(|_| DynFault::new("device-list hook returned malformed JSON"))?;
		let devices = effective
			.get("devices")
			.and_then(Value::as_array)
			.ok_or_else(|| DynFault::new("device-list hook omitted its effective devices"))?;
		Ok(Some(
			devices
				.iter()
				.filter_map(|device| device.get("name").and_then(Value::as_str).map(Str::new))
				.collect(),
		))
	}

	fn proposal_schema(name: &str) -> Option<DynSchema> {
		matches!(name, "resolve" | "reject").then(|| DynSchema {
			name:        Str::new(name),
			description: Some(Str::new_static("Finalize the latest staged proposal.")),
			schema:      json!({
				"type": "object",
				"properties": {
					"reason": {
						"type": "string",
						"description": "One-sentence decision reason."
					}
				},
				"required": ["reason"]
			}),
		})
	}

	fn finalize_proposal(&self, name: &str, args: &Value) -> Result<DynOutput, DynFault> {
		let reason = args
			.get("reason")
			.and_then(Value::as_str)
			.map(str::trim)
			.filter(|reason| !reason.is_empty())
			.ok_or_else(|| DynFault::new("a one-sentence reason is required"))?;
		let id = self
			.proposals
			.latest_pending()
			.ok_or_else(|| DynFault::new("no staged proposal is pending"))?;
		let decision = if name == "resolve" {
			ProposalDecision::Resolve { reason: Str::new(reason) }
		} else {
			ProposalDecision::Reject(ProposalRejection::Requested { reason: Str::new(reason) })
		};
		let outcome = self
			.proposals
			.finalize(id.as_str(), decision)
			.map_err(|_| DynFault::new("failed to finalize the staged proposal"))?;
		Ok(DynOutput::Json(outcome.payload))
	}
}

impl ShellDynHost for DynHost {
	fn list(&self) -> DynFuture<'_, Vec<DynDevice>> {
		Box::pin(async move {
			let registry = self
				.catalog
				.registry()
				.ok_or_else(|| DynFault::new("device catalog is not available in this session"))?;
			let visible = self.visible_names(&registry).await?;
			Ok(registry
				.devices()
				.filter(|device| {
					visible
						.as_ref()
						.is_none_or(|names| names.contains(device.name.as_str()))
				})
				.map(|device| DynDevice {
					name:        device.name.clone(),
					description: Some(device.summary.clone()),
				})
				.collect())
		})
	}

	fn schema(&self, name: &str) -> DynFuture<'_, DynSchema> {
		let name = Str::new(name);
		Box::pin(async move {
			if let Some(schema) = Self::proposal_schema(name.as_str()) {
				return Ok(schema);
			}
			let registry = self
				.catalog
				.registry()
				.ok_or_else(|| DynFault::new("device catalog is not available in this session"))?;
			let path = DevicePath::parse(name.as_str())
				.map_err(|_| DynFault::new(format!("unknown device `{name}`")))?;
			let device = registry
				.devices()
				.find(|device| device.name.as_str() == path.root())
				.ok_or_else(|| DynFault::new(format!("unknown device `{name}`")))?;
			let schema = serde_json::from_slice(device.schema)
				.map_err(|_| DynFault::new(format!("device `{name}` has an invalid JSON schema")))?;
			Ok(DynSchema { name, description: Some(device.summary.clone()), schema })
		})
	}

	fn call(&self, name: &str, args: Value) -> DynFuture<'_, DynOutput> {
		let name = Str::new(name);
		Box::pin(async move {
			if matches!(name.as_str(), "resolve" | "reject") {
				return self.finalize_proposal(name.as_str(), &args);
			}
			let registry = self
				.catalog
				.registry()
				.ok_or_else(|| DynFault::new("device catalog is not available in this session"))?;
			let path = DevicePath::parse(name.as_str())
				.map_err(|_| DynFault::new(format!("unknown device `{name}`")))?;
			let target = registry
				.resolve_device(&path)
				.map_err(|_| DynFault::new(format!("device `{name}` rejected its path arguments")))?;
			let identity = target.identity();
			let raw = Str::new(args.to_string());
			let args_json = Bytes::from(raw.clone());
			let mut stream = match target.route.clone() {
				ToolRoute::Native => {
					let (feed, params) = IncomingParams::channel();
					feed
						.args_committed(raw)
						.map_err(|_| DynFault::new("device argument channel closed before dispatch"))?;
					registry
						.invoke_device(&path, params)
						.map_err(|error| DynFault::new(format!("device dispatch failed: {error}")))?
				},
				ToolRoute::Remote => {
					return Err(DynFault::new("device is owned by the remote environment host"));
				},
				ToolRoute::Worker { site, name: worker } => {
					self
						.invoker
						.invoke(DeviceInvokeRequest {
							path,
							name: target.name.clone(),
							rev: Str::from(target.rev.to_string()),
							owner: Some(target.claimant.clone()),
							site: Some(site),
							worker: Some(worker),
							invocation_id: self.invocation_id(),
							deadline: Duration::new(5, DurationUnit::Minutes),
							args_json,
						})
						.await
				},
			};
			consume(&registry, &identity, &mut stream).await
		})
	}
}

async fn consume(
	registry: &Registry,
	identity: &ToolIdentity,
	stream: &mut ErasedStream<'_>,
) -> Result<DynOutput, DynFault> {
	loop {
		match stream.next().await {
			Some(Ok(ErasedEv::Update(_))) => {},
			Some(Ok(ErasedEv::Done(ErasedOutcome::Done { verdict, .. }))) => {
				return project_result(registry, identity, &verdict);
			},
			Some(Ok(ErasedEv::Done(ErasedOutcome::Detached(job)))) => {
				return Ok(DynOutput::Text(sf!("detached job: {}", job.id)));
			},
			Some(Err(error)) => {
				return Err(DynFault::new(format!("device dispatch failed: {error}")));
			},
			None => return Err(DynFault::new("device dispatch ended without an outcome")),
		}
	}
}

fn project_result(
	registry: &Registry,
	identity: &ToolIdentity,
	verdict: &[u8],
) -> Result<DynOutput, DynFault> {
	let caps = PromptCaps {
		maximum_parts:      16,
		maximum_text_bytes: 262_144,
		media:              false,
		dialect:            Default::default(),
		model_class:        Default::default(),
	};
	let mut rendered = String::new();
	match registry.prompt(identity, verdict, &caps) {
		Ok(Some(parts)) => {
			for text in parts.iter().filter_map(|part| match part {
				Part::Text { text } => Some(text.as_str()),
				Part::Json { .. } | Part::Blob { .. } => None,
			}) {
				if !rendered.is_empty() {
					rendered.push('\n');
				}
				rendered.push_str(text);
			}
		},
		Ok(None) => {},
		Err(RegistryError::UnsupportedExternal { .. }) => {
			render_external_verdict(verdict, &mut rendered);
		},
		Err(error) => {
			return Err(DynFault::new(format!("device result projection failed: {error}")));
		},
	}
	if rendered.is_empty() {
		rendered.push_str("(device returned non-text output)");
	}
	if faulted(verdict) {
		Err(DynFault::new(rendered))
	} else {
		Ok(DynOutput::Text(Str::new(rendered)))
	}
}

fn render_external_verdict(verdict: &[u8], rendered: &mut String) {
	let Ok(verdict) = serde_json::from_slice::<Value>(verdict) else {
		return;
	};
	let Some(value) = verdict.get("value") else {
		return;
	};
	match value {
		Value::String(text) => rendered.push_str(text),
		other => write!(rendered, "{other}").expect("writing JSON into a string cannot fail"),
	}
}

fn faulted(verdict: &[u8]) -> bool {
	serde_json::from_slice::<Value>(verdict)
		.ok()
		.is_some_and(|value| {
			value
				.get("kind")
				.and_then(Value::as_str)
				.is_some_and(|kind| matches!(kind, "fault" | "faulted"))
		})
}

fn device_event_json(device: omp_tool::MountedDevice<'_>) -> Value {
	let place = match device.route {
		ToolRoute::Native => String::from("env"),
		ToolRoute::Remote => String::from("remote"),
		ToolRoute::Worker { name, .. } => format!("worker:{name}"),
	};
	let mut row = Map::from_iter([
		("name".to_owned(), Value::String(device.name.to_string())),
		("family".to_owned(), Value::String(device.rev.family.to_string())),
		("rev".to_owned(), Value::from(device.rev.n)),
		("claimant".to_owned(), Value::String(device.claimant.to_string())),
		("path".to_owned(), Value::String(device.name.to_string())),
		("summary".to_owned(), Value::String(device.summary.to_string())),
		("place".to_owned(), Value::String(place)),
		("mounted".to_owned(), Value::Bool(true)),
		("enabled".to_owned(), Value::Bool(true)),
		("available".to_owned(), Value::Bool(true)),
	]);
	if let Some(metadata) = device.metadata {
		let mut provenance = Map::new();
		for (name, value) in [
			("publisher", metadata.publisher.as_ref()),
			("extension_id", metadata.extension_id.as_ref()),
			("version", metadata.version.as_ref()),
			("artifact_digest", metadata.artifact_digest.as_ref()),
			("layer", metadata.layer.as_ref()),
			("tier", metadata.tier.as_ref()),
		] {
			if let Some(value) = value {
				provenance.insert(name.to_owned(), Value::String(value.to_string()));
			}
		}
		if let Some(generation) = metadata.generation {
			provenance.insert("generation".to_owned(), Value::from(generation));
		}
		if !provenance.is_empty() {
			row.insert("provenance".to_owned(), Value::Object(provenance));
		}
	}
	Value::Object(row)
}
