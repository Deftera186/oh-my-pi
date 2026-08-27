//! Focused proof that sealed decorator declarations reach the worker
//! projection.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn decorated_tools_project_as_runnable_worker_declarations() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio
import dataclasses
import enum
import json

import omp
import omp._registry as registry_module

omp.packages._install_snapshot(
    [{
        "name": "registry-contract",
        "version": "1.0.0",
        "extension_id": "registry-contract",
        "root": "/registry-contract",
        "files": (),
    }],
    own="registry-contract",
)
skill_metadata = {
    "name": "review",
    "description": "Review a change.",
    "hidden": False,
    "disable_model_invocation": False,
    "autoload": False,
}
registry_module.configure_manifest(
    extension="registry-contract",
    tools=(
        ("contract_device", "wire", 7),
        ("contract_tool", "registry-contract", 3),
    ),
    services=(("acme.contract", 2),),
    declarations=(
        {
            "kind": "skills",
            "path": "extension/.omp-generated/skills/review/SKILL.md",
            "metadata": skill_metadata,
        },
        {
            "id": "contract-device",
            "kind": "soft",
            "module": "__main__",
            "key": "contract_device@wire.7",
            "trigger": "lazy",
            "api": 1,
            "failure": "fault",
        },
        {
            "id": "contract-tool",
            "kind": "hard",
            "module": "__main__",
            "key": "contract_tool@registry-contract.3",
            "trigger": "lazy",
            "api": 1,
            "failure": "fault",
        },
        {
            "id": "contract-service",
            "kind": "service",
            "module": "__main__",
            "key": "acme.contract",
            "trigger": "lazy",
            "api": 1,
            "failure": "fault",
        },
    ),
)
skill_evaluations = 0
lowering_evaluations = 0


def deterministic_skill():
    global lowering_evaluations
    lowering_evaluations += 1
    return "Review\n\nInspect the change."


lowering_args = {
    "name": "review",
    "description": "Review a change.",
    "hidden": False,
    "disable_model_invocation": False,
    "autoload": False,
    "contain_root": None,
}
first_lowering = registry_module._lower_skill_declaration(
    deterministic_skill, **lowering_args
)
second_lowering = registry_module._lower_skill_declaration(
    deterministic_skill, **lowering_args
)
assert first_lowering == second_lowering
assert lowering_evaluations == 2
try:
    registry_module._lower_skill_declaration(
        lambda: "x" * 64_000,
        name="oversized",
        description="Oversized.",
        hidden=False,
        disable_model_invocation=False,
        autoload=False,
        contain_root=None,
    )
except ValueError:
    pass
else:
    raise AssertionError("oversized generated skill was accepted")


@omp.skill("review", description="Review a change.")
def review_skill():
    global skill_evaluations
    skill_evaluations += 1
    return "Review\n\nInspect the change."


assert skill_evaluations == 1


@omp.device(
    "contract_device",
    family="wire",
    rev=7,
    summary="Run the decorated contract device.",
    schema={
        "type": "object",
        "properties": {"value": {"type": "integer"}},
        "required": ["value"],
        "additionalProperties": False,
    },
)
async def contract_device(args, ctx):
    return {"details": {"value": args["value"], "has_context": ctx is marker}}


@omp.tool("contract_tool", kind="hard", rev=3)
async def contract_tool(count: int, ctx: omp.Context) -> dict[str, int]:
    return {"details": {"count": count}}


@omp.service("acme.contract", rev=2)
class ContractService:
    async def ping(self, value: int) -> dict[str, int]:
        return {"value": value}


marker = object()
snapshot = registry_module.freeze_declarations()
tools, metadata_json = registry_module.project_worker_registry()
assert skill_evaluations == 1
assert len(snapshot.skills) == 1
assert snapshot.skills[0].declaration.path == (
    "extension/.omp-generated/skills/review/SKILL.md"
)
assert snapshot.skills[0].content == (
    b"---\nname: review\ndescription: 'Review a change.'\n---\n\n"
    b"Review\n\nInspect the change.\n"
)
try:
    omp.skill("late", description="late")(lambda: "late")
except omp.DeclarationSealed:
    pass
else:
    raise AssertionError("late @omp.skill declaration was accepted")
assert snapshot.tools == frozenset({
    ("contract_device", "wire", 7),
    ("contract_tool", "registry-contract", 3),
})
assert [(tool.name, tool.family, tool.rev) for tool in tools] == [
    ("contract_device", "wire", 7),
    ("contract_tool", "registry-contract", 3),
]

device_row, tool_row = tools
assert device_row.description == "Run the decorated contract device."
assert device_row.schema["properties"]["value"] == {"type": "integer"}
assert device_row.strict is None
assert tool_row.kind == "hard"
assert tool_row.strict is True
assert tool_row.schema == {
    "type": "object",
    "properties": {"count": {"type": "integer"}},
    "additionalProperties": False,
    "required": ["count"],
}
assert asyncio.run(device_row.handler({"value": 11}, marker)) == {
    "details": {"value": 11, "has_context": True}
}
assert asyncio.run(tool_row.handler({"count": 4}, marker)) == {
    "details": {"count": 4}
}

metadata = json.loads(metadata_json)
assert metadata["skills"] == [{
    "metadata": skill_metadata,
    "path": "extension/.omp-generated/skills/review/SKILL.md",
}]
assert metadata["tools"][0]["rev"] == 7
assert metadata["services"] == [{
    "methods": [{
        "input_schema": {
            "additionalProperties": False,
            "properties": {"value": {"type": "integer"}},
            "required": ["value"],
            "type": "object",
        },
        "name": "ping",
        "result_schema": {
            "additionalProperties": {"type": "integer"},
            "type": "object",
        },
    }],
    "name": "acme.contract",
    "rev": 2,
    "source_module": "__main__",
}]
class ContractKind(enum.Enum):
    OK = "ok"


@dataclasses.dataclass(frozen=True)
class ContractResult:
    kind: ContractKind
    count: int


assert registry_module.service_json_value(ContractResult(ContractKind.OK, 2)) == {
    "$omp.type": "__main__.ContractResult",
    "$omp.fields": {
        "kind": {"$omp.enum": "__main__.ContractKind", "value": "ok"},
        "count": 2,
    },
}
try:
    registry_module.service_json_value(object())
except TypeError:
    pass
else:
    raise AssertionError("unsupported service result was silently stringified")

# Manifest drift seals the candidate and rejects it before any projection can run.
drift = registry_module.DeclarationRegistry()
drift.configure_manifest(tools=(("manifest_only", "", 1),))
drift.register_tool("decorator_only", "", 1, lambda args: args)
try:
    drift.freeze()
except omp.DeclarationDrift as error:
    assert error.missing_tools == frozenset({("manifest_only", "", 1)})
    assert error.undeclared_tools == frozenset({("decorator_only", "", 1)})
else:
    raise AssertionError("manifest drift activated")
"#
				),
				None,
				None,
			)
		})
		.expect("registry worker contract");
}
