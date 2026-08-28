//! Joined-system proof for Python extension CONTROL and DATA authority wiring.

#![cfg(unix)]

use std::{fs, time::Duration};

use bytes::{BufMut as _, Bytes, BytesMut};
use flume::Receiver;
use omp_agent::{GateError, GateEvent, GateOutcome, HookEvent, HookPatch, HookPhase};
use omp_app::chat_ui::presentation_authority::PresentationEffect;
use omp_core::{ArtifactDigest, Principal, Provenance, Str, sf};
use omp_e2e::{
	Context as _, Result, error,
	support::{
		DEFAULT_TIMEOUT, ExtensionHarness, Scratch, install_omp_binary_env, omp_binary,
		recording_ui_factory, within,
	},
};
use omp_envd::{
	exthost::{
		ActivationTrigger, DeclarationSet, ExtensionManifest, HookDeclarationKey, QuotaBehavior,
		QuotaSpec, ServiceManifest, ToolDeclarationKey, quota::names,
	},
	policy::Grants,
	worker::{ExtHostConfig, ExtHostSpec, ExternalDomainControlFactories, HostKey},
};
use omp_ext::config::StaticDeclarations;
use omp_proto::toolhost::v1::HookEventId;
use serde_json::{Value, json};
use tokio::time;

const MODULE: &str = "p9_extension_control";
const SESSION: &str = "p9-extension-control-session";
const DEVICE: &str = "extension_probe";
const BLOCKER: &str = "extension_block";
const ACTIVATOR: &str = "extension_activate_probe";
const UNSUBSCRIBED_MODULE: &str = "p9_unsubscribed";
const UNSUBSCRIBED_DEVICE: &str = "unsubscribed_probe";
const HOOK_LOG: &str = "p9-hook-events.jsonl";
const PY_EXTENSION: &str = r#"
import asyncio
from dataclasses import dataclass, fields, is_dataclass
from datetime import datetime
from enum import Enum
import json

import omp


HOOK_LOG = __HOOK_LOG__


def _json_value(value):
    if is_dataclass(value):
        return {field.name: _json_value(getattr(value, field.name)) for field in fields(value)}
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    if isinstance(value, dict):
        return {str(key): _json_value(item) for key, item in value.items()}
    if isinstance(value, (tuple, list, set, frozenset)):
        return [_json_value(item) for item in value]
    if isinstance(value, datetime):
        return value.isoformat()
    return value if value is None or isinstance(value, (bool, int, float, str)) else str(value)


def _record(name, event):
    with open(HOOK_LOG, "a", encoding="utf-8") as stream:
        stream.write(json.dumps({"event": name, "payload": _json_value(event)}, sort_keys=True))
        stream.write("\n")
        stream.flush()


def _observe(name):
    async def handler(event, ctx):
        assert ctx.extension == "p9_extension_control"
        _record(name, event)
    options = {
        "phase": omp.HookPhase.OBSERVE,
        "name": "e2e.observe." + name,
    }
    if name.startswith("message_") or name == "call_open":
        options["coalesce"] = omp.Duration("16ms")
    return omp.hook(name, **options)(handler)


def _review(name):
    async def handler(event, ctx):
        assert ctx.extension == "p9_extension_control"
        _record(name, event)
        return omp.Defer()
    return omp.hook(
        name,
        phase=omp.HookPhase.REVIEW,
        name="e2e.review." + name,
    )(handler)


_FAMILY_OBSERVERS = tuple(_observe(name) for name in (
    "session_start",
    "session_shutdown",
    "session_branched",
    "session_rewound",
    "agent_start",
    "message_start",
    "message_update",
    "message_end",
    "call_open",
    "tool_execution_start",
    "tool_execution_end",
    "resources_changed",
    "provider_response",
    "job_registered",
    "job_settled",
    "extension_activate",
))
_FAMILY_REVIEWS = tuple(_review(name) for name in (
    "before_agent_start",
    "user_input",
    "user_bash",
    "resources_discover",
    "session_branch",
    "session_rewind",
))


@omp.entry_kind("e2e.extension_proof", rev="proof.1", display=False)
@dataclass(frozen=True)
class ExtensionProof:
    message: str


@omp.ui.command("extension-proof", description="E2E manifest verification command")
async def extension_proof_command(*_args, **_kwargs):
    return None


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.REVIEW,
    on_failure=omp.OnFailure.DENY,
    name="e2e.extension_review",
)
async def extension_review(event, ctx):
    assert isinstance(event, omp.ToolCallEvent)
    assert isinstance(event.target, omp.DeviceCall)
    assert ctx.extension == "p9_extension_control"
    _record("tool_call", event)
    if event.args.get("message") == "scripted allow":
        return omp.Defer()
    return omp.Deny("reviewed by Python extension", code="E2E_REVIEW")


@omp.hook(
    "subagent_spawn",
    phase=omp.HookPhase.REVIEW,
    on_failure=omp.OnFailure.DENY,
    name="e2e.subagent_review",
)
async def subagent_review(event, ctx):
    assert isinstance(event, omp.agents.SubagentSpec)
    assert ctx.extension == "p9_extension_control"
    _record("subagent_spawn", event)
    if event.task == "deny child":
        return omp.Deny("subagent denied by Python extension", code="E2E_SPAWN")
    return omp.Defer()


@omp.hook(
    "subagent_spawn",
    phase=omp.HookPhase.APPROVAL,
    on_failure=omp.OnFailure.DENY,
    name="e2e.subagent_approval",
)
async def subagent_approval(event, ctx):
    assert isinstance(event, omp.agents.SubagentSpec)
    if event.task != "approve child":
        return omp.Defer()
    return omp.RequireApproval(omp.ApprovalSpec(
        title="Approve delegated child",
        body="The extension requires explicit approval for this delegation.",
        subject=event.task,
        kind=omp.ApprovalKind.SPAWN,
        scopes=(omp.PolicyScope.ONCE,),
        route=omp.ApprovalRoute.LOCAL,
        require_human=True,
    ))


@omp.device(
    name="extension_activate_probe",
    family="proof",
    rev=1,
    schema={"type": "object", "additionalProperties": False},
)
async def extension_activate_probe():
    return {"details": {"active": True}}


@omp.device(
    name="extension_probe",
    family="proof",
    rev=1,
    schema={
        "type": "object",
        "properties": {"message": {"type": "string"}},
        "required": ["message"],
        "additionalProperties": False,
    },
    effects=omp.Effects(documents=omp.DocEffects(read=True)),
)
async def extension_probe(message: str):
    ctx = omp.Context.current()
    appended = await omp.journal.append(
        ExtensionProof(message), idempotency_key="p9-extension-proof"
    )
    latest = await omp.journal.latest(ExtensionProof)

    artifact_ref = await omp.artifacts.put(
        "artifact:" + message,
        media_type="text/plain",
        description="P9 extension DATA proof",
    )
    artifact_text = await omp.artifacts.read(artifact_ref)

    payload = {
        "call_id": "nested-hook-call",
        "invocation_id": ctx.invocation,
        "target": {
            "kind": "device",
            "name": "extension_probe",
            "family": "proof",
            "rev": "proof.1",
            "args": {"message": message},
        },
        "kind": "device",
        "args": {"message": message},
        "raw_args": "{\"message\":\"joined\"}",
        "repaired": False,
        "turn_id": "turn-p9",
        "session_id": ctx.session,
        "cwd": "/workspace",
        "origin": "model",
        "batch": [],
        "deadline": None,
        "bash": None,
    }
    decision = await omp.hooks.dispatch_hook("tool_call", payload)
    assert isinstance(decision, omp.Deny)

    denied_spawn = await omp.hooks.dispatch_hook(
        "subagent_spawn",
        {
            "task": "deny child",
            "max_depth": 1,
            "depth": 1,
            "remaining_concurrency": 7,
        },
    )
    assert isinstance(denied_spawn, omp.Deny)

    approved_spawn = await omp.hooks.dispatch_hook(
        "subagent_spawn",
        {
            "task": "approve child",
            "max_depth": 1,
            "depth": 1,
            "remaining_concurrency": 7,
        },
    )
    assert isinstance(approved_spawn, omp.RequireApproval)

    omp.ui.submit("extension authority is live")
    return {
        "parts": [],
        "details": {
            "entry_id": str(appended),
            "journal_message": latest.value.message,
            "artifact_id": artifact_ref.id,
            "artifact_text": artifact_text,
            "decision": {
                "kind": "deny",
                "reason": decision.reason,
                "code": decision.code,
            },
            "spawn_denial": {
                "kind": "deny",
                "reason": denied_spawn.reason,
                "code": denied_spawn.code,
            },
            "spawn_approval": {
                "kind": "require_approval",
                "title": approved_spawn.spec.title,
                "subject": approved_spawn.spec.subject,
                "kind_name": approved_spawn.spec.kind.value,
            },
        },
    }


@omp.device(
    name="extension_block",
    family="proof",
    rev=1,
    schema={
        "type": "object",
        "properties": {"started": {"type": "string"}},
        "required": ["started"],
        "additionalProperties": False,
    },
)
async def extension_block(started: str):
    ctx = omp.Context.current()
    with open(started, "w", encoding="utf-8") as marker:
        marker.write(ctx.invocation)
        marker.flush()
    await asyncio.Event().wait()
"#;

const PY_UNSUBSCRIBED_EXTENSION: &str = r#"
import json
import omp
import omp.hooks as _hooks


HOOK_LOG = __HOOK_LOG__
_original_dispatch = _hooks._dispatch_hook_callback


async def _count_hook_frame(event, phase, name, payload, context=None):
    with open(HOOK_LOG, "a", encoding="utf-8") as stream:
        stream.write(json.dumps({"event": event, "phase": phase}))
        stream.write("\n")
        stream.flush()
    return await _original_dispatch(event, phase, name, payload, context)


_hooks._dispatch_hook_callback = _count_hook_frame


@omp.device(
    name="unsubscribed_probe",
    family="proof",
    rev=1,
    schema={"type": "object", "additionalProperties": False},
)
async def unsubscribed_probe():
    return {"details": {"live": True}}
"#;

macro_rules! json_observation {
	($name:ident, $event:ident) => {
		struct $name(Value);

		impl HookEvent for $name {
			type Return = ();

			const ID: HookEventId = HookEventId::$event;
			const REV: u32 = 1;

			fn encode_into(&self, out: &mut BytesMut) {
				out.put_slice(&serde_json::to_vec(&self.0).expect("serializable scripted hook"));
			}

			fn apply(&mut self, _patch: &HookPatch) -> std::result::Result<(), GateError> {
				Ok(())
			}
		}
	};
}

json_observation!(SessionStartObservation, HookEventSessionStart);
json_observation!(SessionShutdownObservation, HookEventSessionShutdown);
json_observation!(SessionBranchedObservation, HookEventSessionBranched);
json_observation!(SessionRewoundObservation, HookEventSessionRewound);
json_observation!(AgentStartObservation, HookEventAgentStart);
json_observation!(MessageStartObservation, HookEventMessageStart);
json_observation!(MessageUpdateObservation, HookEventMessageUpdate);
json_observation!(MessageEndObservation, HookEventMessageEnd);
json_observation!(CallOpenObservation, HookEventCallOpen);
json_observation!(ToolExecutionStartObservation, HookEventToolExecutionStart);
json_observation!(ToolExecutionEndObservation, HookEventToolExecutionEnd);
json_observation!(ResourcesChangedObservation, HookEventResourcesChanged);
json_observation!(ProviderResponseObservation, HookEventProviderResponse);
json_observation!(JobRegisteredObservation, HookEventJobRegistered);
json_observation!(JobSettledObservation, HookEventJobSettled);
json_observation!(ExtensionActivateObservation, HookEventExtensionActivate);

fn extension_config(scratch: &Scratch) -> Result<(ExtHostConfig, Receiver<PresentationEffect>)> {
	install_omp_binary_env().context("exposing worker-capable e2e host")?;
	let mut config = ExtHostConfig::new(
		omp_binary().context("resolving worker-capable e2e host")?,
		Principal::new(sf!("p9-e2e"), sf!("P9 E2E")),
		sf!(SESSION),
		1,
	);
	let key = HostKey::new("workspace", "trusted", MODULE);
	let provenance = Provenance::new(
		sf!("omp-e2e"),
		key.extension().clone(),
		sf!("1.0.0"),
		ArtifactDigest::new([9; 32]),
		key.layer().clone(),
		key.tier().clone(),
		1,
	);
	let hook_rows = [
		("session_start", "observe", false),
		("session_shutdown", "observe", false),
		("session_branch", "review", false),
		("session_branched", "observe", false),
		("session_rewind", "review", true),
		("session_rewound", "observe", false),
		("before_agent_start", "review", false),
		("agent_start", "observe", false),
		("message_start", "observe", false),
		("message_update", "observe", false),
		("message_end", "observe", false),
		("call_open", "observe", false),
		("tool_call", "review", true),
		("tool_execution_start", "observe", false),
		("tool_execution_end", "observe", false),
		("user_input", "review", false),
		("user_bash", "review", true),
		("resources_discover", "review", true),
		("resources_changed", "observe", false),
		("provider_response", "observe", false),
		("subagent_spawn", "review", true),
		("subagent_spawn", "approval", true),
		("job_registered", "observe", false),
		("job_settled", "observe", false),
		("extension_activate", "observe", false),
	];
	let mut declaration_rows = vec![
		json!({
			"id": "extension-proof", "kind": "command", "module": MODULE,
			"key": "extension-proof", "trigger": "lazy", "api": 1, "failure": "fault",
			"description": "E2E manifest verification command",
			"callback": format!("{MODULE}.extension_proof_command"),
		}),
		json!({
			"id": DEVICE, "kind": "soft", "module": MODULE,
			"key": format!("{DEVICE}@proof.1"), "trigger": "lazy", "api": 1, "failure": "fault",
		}),
		json!({
			"id": BLOCKER, "kind": "soft", "module": MODULE,
			"key": format!("{BLOCKER}@proof.1"), "trigger": "lazy", "api": 1, "failure": "fault",
		}),
		json!({
			"id": ACTIVATOR, "kind": "soft", "module": MODULE,
			"key": format!("{ACTIVATOR}@proof.1"), "trigger": "lazy", "api": 1, "failure": "fault",
		}),
	];
	for &(event, phase, fail_closed) in &hook_rows {
		declaration_rows.push(json!({
			"id": format!("{event}-{phase}"),
			"kind": "hook",
			"module": MODULE,
			"key": format!("{event}/{phase}"),
			"trigger": if fail_closed { "eager-prompt" } else { "lazy" },
			"api": 1,
			"failure": if fail_closed { "fail-closed" } else { "fail-open" },
		}));
	}
	let properties = serde_json::from_value(json!({"declarations": declaration_rows}))?;
	let static_declarations = StaticDeclarations::from_properties(&properties)
		.map_err(|source| error(format!("building authenticated manifest declarations: {source}")))?;
	let manifest = ExtensionManifest::new_with_static(
		provenance,
		sf!(MODULE),
		[],
		DeclarationSet::new(
			[
				ToolDeclarationKey::new(DEVICE, "proof", 1),
				ToolDeclarationKey::new(BLOCKER, "proof", 1),
				ToolDeclarationKey::new(ACTIVATOR, "proof", 1),
			],
			hook_rows.into_iter().map(|(event, phase, _)| {
				HookDeclarationKey::new(event, match phase {
					"observe" => HookPhase::Observe,
					"review" => HookPhase::Review,
					"approval" => HookPhase::Approval,
					_ => unreachable!("fixture phase"),
				})
			}),
		),
		ServiceManifest::default(),
		static_declarations,
		[
			QuotaSpec::new(names::JOURNAL_APPENDS, 4, 4, None, QuotaBehavior::Hard),
			QuotaSpec::new(names::UI_EFFECTS, 4, 4, None, QuotaBehavior::Hard),
		],
		[ActivationTrigger::FirstReach],
	);
	let mut spec = ExtHostSpec::new(key, manifest);
	spec.python_site = Some(scratch.project().to_owned());
	spec.data_socket = Some(scratch.socket("p9-extension-data.sock"));
	spec.data_grants = Grants::supported(["env.blob"]);
	config.extensions.push(spec);
	let (ui, effects) = recording_ui_factory();
	config.bind_domain_control_factories(ExternalDomainControlFactories {
		ui: Some(ui),
		..ExternalDomainControlFactories::default()
	});
	Ok((config, effects))
}

fn unsubscribed_extension_config(scratch: &Scratch) -> Result<ExtHostConfig> {
	install_omp_binary_env().context("exposing worker-capable e2e host")?;
	let mut config = ExtHostConfig::new(
		omp_binary().context("resolving worker-capable e2e host")?,
		Principal::new(sf!("p9-e2e-unsubscribed"), sf!("P9 E2E Unsubscribed")),
		sf!(SESSION),
		1,
	);
	let key = HostKey::new("workspace", "trusted", UNSUBSCRIBED_MODULE);
	let provenance = Provenance::new(
		sf!("omp-e2e"),
		key.extension().clone(),
		sf!("1.0.0"),
		ArtifactDigest::new([10; 32]),
		key.layer().clone(),
		key.tier().clone(),
		1,
	);
	let properties = serde_json::from_value(json!({
		"declarations": [{
			"id": UNSUBSCRIBED_DEVICE,
			"kind": "soft",
			"module": UNSUBSCRIBED_MODULE,
			"key": format!("{UNSUBSCRIBED_DEVICE}@proof.1"),
			"trigger": "lazy",
			"api": 1,
			"failure": "fault",
		}]
	}))?;
	let static_declarations = StaticDeclarations::from_properties(&properties)
		.map_err(|source| error(format!("building empty manifest declarations: {source}")))?;
	let manifest = ExtensionManifest::new_with_static(
		provenance,
		sf!(UNSUBSCRIBED_MODULE),
		[],
		DeclarationSet::new([ToolDeclarationKey::new(UNSUBSCRIBED_DEVICE, "proof", 1)], []),
		ServiceManifest::default(),
		static_declarations,
		[],
		[ActivationTrigger::FirstReach],
	);
	let mut spec = ExtHostSpec::new(key, manifest);
	spec.python_site = Some(scratch.project().to_owned());
	spec.data_socket = Some(scratch.socket("p9-unsubscribed-data.sock"));
	config.extensions.push(spec);
	Ok(config)
}

fn subscribed_extension_source(scratch: &Scratch) -> Result<String> {
	let log = serde_json::to_string(&scratch.project().join(HOOK_LOG).to_string_lossy())?;
	Ok(PY_EXTENSION.replace("__HOOK_LOG__", &log))
}

fn unsubscribed_extension_source(scratch: &Scratch) -> Result<String> {
	let log = serde_json::to_string(&scratch.project().join(HOOK_LOG).to_string_lossy())?;
	Ok(PY_UNSUBSCRIBED_EXTENSION.replace("__HOOK_LOG__", &log))
}

async fn gate_allowed(
	gate: &omp_agent::HookGate,
	event: HookEventId,
	payload: Value,
) -> Result<()> {
	let outcome = gate
		.gate(event, GateEvent::new(Str::default(), Bytes::from(serde_json::to_vec(&payload)?)))
		.await;
	if matches!(outcome, GateOutcome::Allow { .. }) {
		Ok(())
	} else {
		Err(error(format!("scripted {event:?} hook did not allow: {outcome:?}")))
	}
}

async fn read_hook_log(scratch: &Scratch, final_event: &str) -> Result<Vec<Value>> {
	let path = scratch.project().join(HOOK_LOG);
	within("waiting for scripted hook observations", DEFAULT_TIMEOUT, async {
		loop {
			if let Ok(contents) = fs::read_to_string(&path) {
				let rows = contents
					.lines()
					.map(serde_json::from_str)
					.collect::<std::result::Result<Vec<Value>, _>>()?;
				if rows.iter().any(|row| row["event"] == final_event) {
					return Ok(rows);
				}
			}
			time::sleep(Duration::from_millis(10)).await;
		}
	})
	.await?
}

#[tokio::test]
async fn p9_python_extension_exercises_joined_control_and_data_authorities() -> Result<()> {
	let scratch = Scratch::new().context("creating extension control scratch project")?;
	scratch.write(format!("{MODULE}.py"), subscribed_extension_source(&scratch)?.as_bytes())?;
	let (config, _ui_effects) = extension_config(&scratch)?;
	let harness = ExtensionHarness::spawn(&scratch, config).await?;

	let declarations = harness
		.registry()
		.live_identities()
		.filter(|(name, _)| matches!(name.as_str(), DEVICE | BLOCKER))
		.map(|(name, rev)| (name.to_string(), rev.to_string()))
		.collect::<Vec<_>>();
	assert_eq!(
		declarations,
		vec![(BLOCKER.to_owned(), "proof.1".to_owned()), (DEVICE.to_owned(), "proof.1".to_owned())],
		"manifest-verified Python declarations were not published into the live registry",
	);

	let gate = harness.admission_gate();
	assert!(
		gate.subscribed(HookEventId::HookEventSubagentSpawn),
		"subagent hook subscription bit was not published"
	);
	let denied = gate
		.gate(
			HookEventId::HookEventSubagentSpawn,
			GateEvent::new(
				sf!("subagent_spawn"),
				Bytes::from(serde_json::to_vec(&json!({
					"task": "deny child",
					"max_depth": 1,
					"depth": 1,
					"remaining_concurrency": 7,
				}))?),
			),
		)
		.await;
	let GateOutcome::Deny { reason, policy, .. } = denied else {
		return Err(error("Python subagent review did not deny"));
	};
	assert_eq!(reason, "subagent denied by Python extension");
	assert_eq!(policy.as_deref().and_then(|denial| denial.code.as_deref()), Some("E2E_SPAWN"));

	let approval = gate
		.gate(
			HookEventId::HookEventSubagentSpawn,
			GateEvent::new(
				sf!("subagent_spawn"),
				Bytes::from(serde_json::to_vec(&json!({
					"task": "approve child",
					"max_depth": 1,
					"depth": 1,
					"remaining_concurrency": 7,
				}))?),
			),
		)
		.await;
	let GateOutcome::Approval { specs, .. } = approval else {
		return Err(error("Python subagent approval hook did not require approval"));
	};
	assert_eq!(specs.len(), 1);
	assert_eq!(specs[0].title, "Approve delegated child");
	assert_eq!(specs[0].subject, "approve child");
	assert_eq!(specs[0].kind, "spawn");

	harness.shutdown().await;
	Ok(())
}
#[tokio::test]
async fn p9_python_extension_observes_all_hook_families_in_script_order() -> Result<()> {
	let scratch = Scratch::new().context("creating all-families hook scratch project")?;
	scratch.write(format!("{MODULE}.py"), subscribed_extension_source(&scratch)?.as_bytes())?;
	let (config, _) = extension_config(&scratch)?;
	let harness = ExtensionHarness::spawn(&scratch, config).await?;
	let gate = harness.admission_gate();

	gate.notify(&ExtensionActivateObservation(json!({
		"extension": MODULE,
		"reason": "first_reach",
		"session_started_at": "2026-08-28T00:00:00Z",
		"generation": 1,
		"trigger": "p9_script",
	})));
	gate.notify(&SessionStartObservation(json!({
		"session_id": SESSION,
		"root": scratch.project(),
		"cwd": scratch.project(),
		"dirs": [scratch.project()],
		"resumed": false,
		"forked_from": null,
		"agent": null,
		"trust": "trusted",
		"head_event": 0,
		"prompt_rev": "p9",
		"previous_session": null,
	})));
	gate_allowed(
		gate.as_ref(),
		HookEventId::HookEventBeforeAgentStart,
		json!({
			"submission_id": "submit-p9",
			"text": "exercise hook families",
			"items": [],
			"source": "interactive",
			"prompt_rev": "p9",
			"staged_interrupts": 0,
			"resuming": false,
			"schedule_id": null,
		}),
	)
	.await?;
	gate.notify(&AgentStartObservation(json!({
		"submission_id": "submit-p9",
		"from_phase": "idle",
		"pending_items": 1,
	})));
	gate.notify(&MessageStartObservation(json!({
		"turn_id": "turn-p9",
		"item_id": "message-p9",
		"role": "assistant",
		"index": 0,
	})));
	gate.notify(&MessageUpdateObservation(json!({
		"turn_id": "turn-p9",
		"item_id": "message-p9",
		"part_index": 0,
		"kind": "text",
		"delta": "calling extension_probe",
		"coalesced": 1,
		"total_chars": 23,
	})));
	gate.notify(&MessageEndObservation(json!({
		"turn_id": "turn-p9",
		"item_id": "message-p9",
		"role": "assistant",
		"parts": 1,
		"finish": "complete",
	})));

	let target = json!({
		"kind": "device",
		"name": DEVICE,
		"family": "proof",
		"rev": "proof.1",
		"args": {"message": "scripted allow"},
	});
	gate.notify(&CallOpenObservation(json!({
		"call_id": "call-p9",
		"target": target,
		"kind": "device",
		"turn_id": "turn-p9",
		"place": {"kind": "host", "name": null},
	})));
	gate_allowed(
		gate.as_ref(),
		HookEventId::HookEventToolCall,
		json!({
			"call_id": "call-p9",
			"invocation_id": "invoke-p9",
			"target": target,
			"kind": "device",
			"args": {"message": "scripted allow"},
			"raw_args": "{\"message\":\"scripted allow\"}",
			"repaired": false,
			"turn_id": "turn-p9",
			"session_id": SESSION,
			"cwd": scratch.project(),
			"origin": "model",
			"batch": [],
			"deadline": null,
			"bash": null,
		}),
	)
	.await?;
	gate.notify(&ToolExecutionStartObservation(json!({
		"call_id": "call-p9",
		"invocation_id": "invoke-p9",
		"target": target,
		"place": {"kind": "host", "name": null},
		"deadline": null,
	})));
	gate.notify(&ToolExecutionEndObservation(json!({
		"call_id": "call-p9",
		"target": target,
		"outcome": "ok",
		"duration": "8ms",
		"spilled": false,
		"artifact": null,
		"effects_unknown": false,
	})));

	gate_allowed(
		gate.as_ref(),
		HookEventId::HookEventUserInput,
		json!({
			"text": "!printf p9",
			"images": [],
			"source": "interactive",
			"session_id": SESSION,
			"pasted": false,
		}),
	)
	.await?;
	gate_allowed(
		gate.as_ref(),
		HookEventId::HookEventUserBash,
		json!({
			"command": "printf p9",
			"cwd": scratch.project(),
			"exclude_from_context": false,
			"bash": null,
			"env_overrides": {},
		}),
	)
	.await?;
	gate_allowed(
		gate.as_ref(),
		HookEventId::HookEventResourcesDiscover,
		json!({
			"reason": "startup",
			"root": scratch.project(),
			"found": [],
			"add": [],
			"keep": null,
		}),
	)
	.await?;
	gate.notify(&ResourcesChangedObservation(json!({
		"added": [],
		"removed": [],
		"reason": "workspace_changed",
	})));
	gate.notify(&ProviderResponseObservation(json!({
		"provider": "p9-provider",
		"model": {"provider": "p9-provider", "api": "chat", "model": "p9-model"},
		"status": 200,
		"headers": {"x-request-id": "request-p9"},
		"request_id": "request-p9",
	})));

	let denied = gate
		.gate(
			HookEventId::HookEventSubagentSpawn,
			GateEvent::new(
				sf!("subagent_spawn"),
				Bytes::from(serde_json::to_vec(&json!({
					"task": "deny child",
					"max_depth": 2,
					"depth": 1,
					"remaining_concurrency": 1,
				}))?),
			),
		)
		.await;
	assert!(matches!(denied, GateOutcome::Deny { .. }), "first scripted spawn was not denied");
	gate_allowed(
		gate.as_ref(),
		HookEventId::HookEventSubagentSpawn,
		json!({
			"task": "allow child",
			"max_depth": 2,
			"depth": 1,
			"remaining_concurrency": 1,
		}),
	)
	.await?;
	gate.notify(&JobRegisteredObservation(json!({
		"job_id": "job-p9",
		"owner": "child-p9",
		"call_id": "call-p9",
		"lifetime": "session",
		"expected_artifact": null,
	})));
	gate.notify(&JobSettledObservation(json!({
		"job_id": "job-p9",
		"owner": "child-p9",
		"artifact": null,
		"failed": false,
		"duration": "12ms",
	})));
	gate_allowed(
		gate.as_ref(),
		HookEventId::HookEventSessionBranch,
		json!({"at_event": 8, "keep_event": 6, "reason": "user", "summarize": false}),
	)
	.await?;
	gate.notify(&SessionBranchedObservation(json!({
		"at_event": 8,
		"new_head": 9,
		"summary_event": null,
	})));
	gate_allowed(
		gate.as_ref(),
		HookEventId::HookEventSessionRewind,
		json!({
			"to_event": 6,
			"restore_workspace": false,
			"targets": [],
			"dropped_items": 2,
		}),
	)
	.await?;
	gate.notify(&SessionRewoundObservation(json!({
		"to_event": 6,
		"new_head": 10,
		"restored_workspace": false,
	})));
	gate.notify(&SessionShutdownObservation(json!({
		"session_id": SESSION,
		"reason": "user_exit",
		"budget": "2s",
		"target_session": null,
	})));

	let rows = read_hook_log(&scratch, "session_shutdown").await?;
	let names = rows
		.iter()
		.filter_map(|row| row["event"].as_str())
		.collect::<Vec<_>>();
	let expected = [
		"extension_activate",
		"session_start",
		"before_agent_start",
		"agent_start",
		"message_start",
		"message_update",
		"message_end",
		"call_open",
		"tool_call",
		"tool_execution_start",
		"tool_execution_end",
		"user_input",
		"user_bash",
		"resources_discover",
		"resources_changed",
		"provider_response",
		"subagent_spawn",
		"subagent_spawn",
		"job_registered",
		"job_settled",
		"session_branch",
		"session_branched",
		"session_rewind",
		"session_rewound",
		"session_shutdown",
	];
	let mut cursor = 0;
	for expected_name in expected {
		let Some(offset) = names[cursor..]
			.iter()
			.position(|name| *name == expected_name)
		else {
			return Err(error(format!(
				"missing ordered {expected_name} observation after {:?}: {names:?}",
				&names[..cursor]
			)));
		};
		cursor += offset + 1;
	}
	let payload = |name: &str| {
		rows
			.iter()
			.find(|row| row["event"] == name)
			.map(|row| &row["payload"])
			.expect("asserted event has a payload")
	};
	assert_eq!(payload("session_start")["session_id"], SESSION);
	assert_eq!(payload("before_agent_start")["text"], "exercise hook families");
	assert_eq!(payload("message_update")["delta"], "calling extension_probe");
	assert_eq!(payload("tool_call")["args"]["message"], "scripted allow");
	assert_eq!(payload("user_bash")["command"], "printf p9");
	assert_eq!(payload("resources_discover")["reason"], "startup");
	assert_eq!(payload("provider_response")["status"], 200);
	assert_eq!(
		rows
			.iter()
			.filter(|row| row["event"] == "subagent_spawn")
			.map(|row| row["payload"]["task"].as_str())
			.collect::<Vec<_>>(),
		vec![Some("deny child"), Some("allow child")],
	);
	assert_eq!(payload("job_settled")["failed"], false);
	assert_eq!(payload("session_branched")["new_head"], 9);
	assert_eq!(payload("session_rewound")["new_head"], 10);
	assert_eq!(payload("session_shutdown")["reason"], "user_exit");
	assert_eq!(gate.dropped_notifies(), 0, "script overflowed the hook observation mailbox");

	harness.shutdown().await;
	Ok(())
}

#[tokio::test]
async fn p9_unsubscribed_extension_receives_zero_hook_frames() -> Result<()> {
	let scratch = Scratch::new().context("creating unsubscribed hook scratch project")?;
	scratch.write(
		format!("{UNSUBSCRIBED_MODULE}.py"),
		unsubscribed_extension_source(&scratch)?.as_bytes(),
	)?;
	let harness =
		ExtensionHarness::spawn(&scratch, unsubscribed_extension_config(&scratch)?).await?;
	assert!(
		harness
			.registry()
			.live_identities()
			.any(|(name, rev)| name.as_str() == UNSUBSCRIBED_DEVICE && rev.to_string() == "proof.1"),
		"unsubscribed fixture declaration was not live",
	);
	let gate = harness.admission_gate();
	for ordinal in 1..=66 {
		if ordinal == 59 {
			continue;
		}
		let event = HookEventId::try_from(ordinal).map_err(|_| error("invalid hook ordinal"))?;
		assert!(!gate.subscribed(event), "unsubscribed fixture published bit {ordinal}");
	}
	gate.notify(&SessionStartObservation(json!({
		"session_id": SESSION,
		"root": scratch.project(),
		"cwd": scratch.project(),
		"dirs": [],
		"resumed": false,
		"forked_from": null,
		"agent": null,
		"trust": "trusted",
		"head_event": 0,
		"prompt_rev": "p9",
		"previous_session": null,
	})));
	gate.notify(&AgentStartObservation(json!({
		"submission_id": "unsubscribed",
		"from_phase": "idle",
		"pending_items": 1,
	})));
	gate.notify(&ProviderResponseObservation(json!({
		"provider": "p9-provider",
		"model": {"provider": "p9-provider", "api": "chat", "model": "p9-model"},
		"status": 200,
		"headers": {},
		"request_id": null,
	})));
	time::sleep(Duration::from_millis(100)).await;
	let hook_log = scratch.project().join(HOOK_LOG);
	assert!(
		!hook_log.exists() || fs::read(&hook_log)?.is_empty(),
		"unsubscribed fixture received a hook CONTROL frame"
	);
	assert_eq!(gate.dropped_notifies(), 0);

	harness.shutdown().await;
	Ok(())
}
