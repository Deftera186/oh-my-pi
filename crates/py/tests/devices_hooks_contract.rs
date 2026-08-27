//! Focused contract proof for dynamic devices and host hook callbacks.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn devices_and_hooks_use_the_control_contract() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio

import omp
from omp import hooks as hook_module
from omp._registry import registry
from omp.devices import _dispatch_device
from omp.events import spec as event_spec

registry.configure_manifest(
    tools=(("dynamic", "integration", 1),),
    hooks=(("tool_call", "review"), ("user_bash", "transform")),
    extension="acme/devices-hooks",
)

parent = omp.devices.parent(
    "dynamic", family="integration", rev=1, place="host"
)

@omp.hook(
    "tool_call",
    phase=omp.HookPhase.REVIEW,
    on_failure=omp.OnFailure.DENY,
    name="acme.review",
)
async def review(event, ctx):
    assert isinstance(event, omp.ToolCallEvent)
    assert isinstance(event.target, omp.DeviceCall)
    assert ctx.extension == "acme/devices-hooks"
    return hook_module.Deny("blocked by subscribed review", code="ACME_POLICY")


@omp.hook(
    "user_bash",
    phase=omp.HookPhase.TRANSFORM,
    name="acme.shell-env-second",
    order=20,
)
def shell_env_second(event, ctx):
    assert isinstance(event, omp.UserBashEvent)
    assert event.env_overrides == {
        "BASELINE": "base",
        "TOKEN_FILE": "/run/token",
        "REMOVE_ME": None,
    }
    return hook_module.Defer()


registry.freeze()

user_bash_spec = event_spec("user_bash")
assert user_bash_spec.payload is omp.UserBashEvent
assert user_bash_spec.on_failure is omp.OnFailure.DENY
assert user_bash_spec.fields["env_overrides"] is omp.Composition.REPLACE


round_trip_decisions = (
    hook_module.Allow("accepted"),
    hook_module.Allow(),
    hook_module.Deny("blocked", fatal=True, code="POLICY"),
    hook_module.Deny("ordinary denial"),
    hook_module.Modify(
        target=hook_module.CoreTool(
            "bash", "2", {"command": "printf core"}
        ),
        args={"command": "printf replaced"},
        reason="replace core args",
    ),
    hook_module.Modify(
        target=hook_module.DeviceCall(
            "dynamic/echo",
            "integration",
            "1",
            {"value": "device"},
        ),
        patch={"value": "patched", "obsolete": hook_module.UNSET},
        reason="patch device args",
    ),
    hook_module.Modify(
        target=hook_module.McpCall(
            "filesystem", "read_file", {"path": "/workspace/input"}
        ),
        patch={},
    ),
    hook_module.Modify(
        env_overrides={"TOKEN_FILE": "/run/token", "REMOVE_ME": None}
    ),
    hook_module.Modify(),
    hook_module.Defer("no opinion"),
    hook_module.Defer(),
    hook_module.RequireApproval(
        hook_module.ApprovalSpec(
            title="Run command",
            body="The extension wants to run a command.",
            subject="bash",
            kind=hook_module.ApprovalKind.PRIVILEGE,
            scopes=(
                hook_module.PolicyScope.CALL,
                hook_module.PolicyScope.SESSION,
            ),
            default=False,
            route=hook_module.ApprovalRoute.EXTERNAL,
            approver="security",
            timeout=omp.Duration("30s"),
            unreachable=hook_module.Unreachable.ESCALATE_LOCAL,
            require_human=True,
            pattern="bash:*",
            evidence=("policy:restricted", "caller:model"),
        )
    ),
    hook_module.RequireApproval(
        hook_module.ApprovalSpec(
            title="Default approval",
            body="Use every documented default.",
            subject="default",
        )
    ),
)

for decision in round_trip_decisions:
    encoded = hook_module._decision_to_wire(decision)
    decoded = hook_module._decision_from_wire(encoded)
    assert decoded == decision, (decision, encoded, decoded)

malformed_decisions = (
    None,
    {},
    {"kind": "unknown"},
    {"kind": "allow", "reason": 1},
    {"kind": "deny"},
    {"kind": "deny", "reason": "no", "fatal": 1},
    {"kind": "deny", "reason": "no", "code": 1},
    {"kind": "modify", "args": []},
    {"kind": "modify", "patch": []},
    {"kind": "modify", "unset": "field"},
    {"kind": "modify", "reason": 1},
    {"kind": "modify", "patch": {1: "value"}},
    {
        "kind": "modify",
        "patch": {"field": "value"},
        "unset": ["field"],
    },
    {
        "kind": "modify",
        "target": {
            "kind": "core",
            "name": 1,
            "rev": "2",
            "args": {},
        },
    },
    {
        "kind": "modify",
        "target": {
            "kind": "mcp",
            "server": "filesystem",
            "tool": "read_file",
            "args": {1: "value"},
        },
    },
    {
        "kind": "modify",
        "args": {"value": 1},
        "patch": {"value": 2},
    },
    {"kind": "defer", "note": False},
    {"kind": "require_approval"},
    {
        "kind": "require_approval",
        "spec": {"title": 1, "body": "body", "subject": "subject"},
    },
    {
        "kind": "require_approval",
        "spec": {
            "title": "title",
            "body": "body",
            "subject": "subject",
            "scopes": "session",
        },
    },
    {
        "kind": "require_approval",
        "spec": {
            "title": "title",
            "body": "body",
            "subject": "subject",
            "require_human": 1,
        },
    },
)

for malformed in malformed_decisions:
    try:
        hook_module._decision_from_wire(malformed)
    except hook_module.HookContractError:
        pass
    else:
        raise AssertionError(
            f"malformed decision did not fail typed: {malformed!r}"
        )


def catalog_row(mounted=True, reason=None):
    return {
        "name": "dynamic/echo",
        "family": "integration",
        "rev": 1,
        "identity": "dynamic/echo@integration/1",
        "claimant": "acme/devices-hooks",
        "path": "dynamic/echo",
        "summary": "Echo one value",
        "place": "host",
        "precedence": 0,
        "tier": "write",
        "effects": None,
        "mounted": mounted,
        "enabled": mounted,
        "available": mounted,
        "reason": reason,
        "shadowed_by": None,
        "source": "acme/devices-hooks",
        "provenance": {
            "publisher": "acme",
            "extension_id": "devices-hooks",
            "version": "1.0.0",
            "artifact_digest": "sha256:test",
            "layer": "project",
            "tier": "extension",
            "generation": 7,
        },
        "slotted": False,
        "schema_bytes": 28,
        "schema_tokens": 7,
    }


class Backend:
    def __init__(self):
        self.calls = []
        self.mounted = True

    async def request(self, operation, arguments):
        self.calls.append((operation, arguments))
        if operation == "omp.devices.dynamic_mount":
            assert arguments == {
                "parent": {
                    "name": "dynamic",
                    "family": "integration",
                    "rev": 1,
                    "place": "host",
                },
                "specs": [{
                    "path": "dynamic/echo",
                    "subpath": "echo",
                    "schema": {
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                    },
                    "summary": "Echo one value",
                    "docs": "Returns the supplied value.",
                }],
            }
            return {
                "paths": ["dynamic/echo"],
                "catalog": [catalog_row()],
            }
        if operation == "omp.devices.set_availability":
            delta, = arguments["deltas"]
            self.mounted = delta["mounted"]
            return {
                "catalog": [
                    catalog_row(self.mounted, delta["reason"])
                ]
            }
        if operation == "omp.devices.refresh":
            return {"catalog": [catalog_row(self.mounted)]}
        if operation == "omp.devices.invoke":
            assert arguments == {
                "path": "dynamic/echo",
                "args": {"value": "nested"},
                "deadline": "2s",
            }
            return {"value": "nested", "admitted": True}
        if operation == "omp.hooks.dispatch":
            assert arguments["event"] == "tool_call"
            assert arguments["event_rev"] == 1
            return {
                "kind": "deny",
                "reason": "composed denial",
                "fatal": False,
                "code": "COMPOSED",
            }
        raise AssertionError(f"unexpected CONTROL operation {operation!r}")


backend = Backend()
omp._install_control_backend(backend)


async def exercise():
    async def echo(*, value):
        return {"value": value}

    spec = omp.MountSpec(
        "echo",
        echo,
        {
            "type": "object",
            "properties": {"value": {"type": "string"}},
        },
        "Echo one value",
        "Returns the supplied value.",
    )
    assert await parent.mount(spec) == "dynamic/echo"
    row, = omp.devices.list()
    assert isinstance(row, omp.DeviceInfo)
    assert str(row.path) == "dynamic/echo"
    assert row.available
    assert await _dispatch_device(
        "dynamic/echo", {"value": "callback"}
    ) == {"value": "callback"}

    await omp.devices.disable("dynamic/echo", reason="maintenance")
    row, = omp.devices.list(mounted_only=False)
    assert not row.mounted
    assert row.reason == "maintenance"
    assert omp.devices.list() == ()

    await omp.devices.enable("dynamic/echo")
    row, = await omp.devices.refresh()
    assert row.mounted

    nested = await omp.devices.invoke(
        "dynamic/echo", {"value": "nested"}, deadline=omp.Duration("2s")
    )
    assert nested == {"value": "nested", "admitted": True}

    payload = {
        "call_id": "call-1",
        "invocation_id": "inv-1",
        "target": {
            "kind": "device",
            "name": "dynamic/echo",
            "family": "integration",
            "rev": "1",
            "args": {"value": "callback"},
        },
        "kind": "device",
        "args": {"value": "callback"},
        "raw_args": "{\"value\":\"callback\"}",
        "repaired": False,
        "turn_id": "turn-1",
        "session_id": "session-1",
        "cwd": "/workspace",
        "origin": "model",
        "batch": [],
        "deadline": None,
        "bash": None,
    }
    context = {
        "extension": "acme/devices-hooks",
        "session": "session-1",
        "invocation": "hook-1",
        "principal": "extension:acme/devices-hooks",
        "generation": 7,
    }
    callback = await hook_module._dispatch_hook_callback(
        "tool_call", "review", "acme.review", payload, context
    )
    assert callback == {
        "kind": "deny",
        "reason": "blocked by subscribed review",
        "fatal": False,
        "code": "ACME_POLICY",
    }

    user_bash = {
        "command": "printf env",
        "cwd": "/workspace",
        "exclude_from_context": True,
        "bash": None,
        "env_overrides": {"BASELINE": "base"},
    }
    first = hook_module._decision_to_wire(
        hook_module.Modify(
            env_overrides={
                **user_bash["env_overrides"],
                "TOKEN_FILE": "/run/token",
                "REMOVE_ME": None,
            }
        )
    )
    assert first == {
        "kind": "modify",
        "target": None,
        "args": None,
        "patch": {
            "env_overrides": {
                "BASELINE": "base",
                "TOKEN_FILE": "/run/token",
                "REMOVE_ME": None,
            }
        },
        "unset": [],
        "reason": None,
    }
    user_bash["env_overrides"] = first["patch"]["env_overrides"]
    second = await hook_module._dispatch_hook_callback(
        "user_bash",
        "transform",
        "acme.shell-env-second",
        user_bash,
        context,
    )
    assert second == {
        "kind": "defer",
        "note": None,
    }

    composed = await omp.hooks.dispatch_hook("tool_call", payload)
    assert composed == hook_module.Deny(
        "composed denial", fatal=False, code="COMPOSED"
    )


asyncio.run(exercise())
"#
				),
				None,
				None,
			)
		})
		.expect("dynamic device and hook CONTROL contract");
}
