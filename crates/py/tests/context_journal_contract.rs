//! Focused CONTROL contract for context projection and authoritative journals.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn context_and_journal_control_contract() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import asyncio
import base64
import dataclasses
import json

import omp
from _omp import _principal_from_host


@dataclasses.dataclass(frozen=True, slots=True)
class ContextResult(omp.Payload):
    count: int


@omp.entry_kind("dev.example.context-journal", rev="v.1", spill=False)
@dataclasses.dataclass(frozen=True, slots=True)
class Observation:
    text: str
    rank: int


usage = {
    "total_tokens": 10,
    "context_window": 100,
    "reserve_tokens": 10,
    "usable_tokens": 90,
    "fraction": 1 / 9,
    "prompt_head_tokens": 1,
    "device_catalog_tokens": 2,
    "message_tokens": 7,
    "catalog_notice_tokens": 0,
    "media_tokens": 0,
    "compaction_epoch": 7,
    "threshold_fraction": 0.8,
    "in_flight": False,
}
message = {
    "id": "item-4",
    "event": 4,
    "seq": 3,
    "kind": "tool_result",
    "role": "tool",
    "turn_id": "turn-1",
    "created_at_ms": 1234,
    "tokens": 5,
    "byte_len": 12,
    "part_count": 1,
    "media_count": 0,
    "tool": {"name": "read", "family": "hl", "rev": 1},
    "is_error": False,
    "useless": False,
    "pinned": False,
    "elided": False,
    "superseded_by": None,
    "artifacts": [],
    "preview": "result",
}
row = {
    "id": {"session": "session-1", "index": 12},
    "kind": "dev.example.context-journal",
    "rev": "v.1",
    "ts": 99,
    "principal": _principal_from_host("principal-1", "Test"),
    "provenance": omp.Provenance(
        publisher="dev.example",
        extension_id="context-journal",
        version="1.0.0",
        artifact_digest="abc",
        layer="project",
        tier="trusted",
        generation=2,
    ),
    "raw": '{"rank":2,"text":"durable"}',
    "display": False,
    "in_context": True,
    "artifact": None,
}


class Backend:
    def __init__(self):
        self.calls = []
        self.label = None
        self.next_label_index = 17

    async def request(self, operation, arguments):
        # Every domain arm must remain valid on the JSON CONTROL transport.
        json.dumps(arguments, allow_nan=False)
        self.calls.append((operation, arguments))
        if operation == "omp.context.view":
            return {"schema": "omp.context.view.v1", "result": {
                "session_id": "session-1",
                "turn_id": "turn-1",
                "model": "model-1",
                "provider": "provider-1",
                "epoch": 7,
                "messages": [message],
                "usage": usage,
                "prompt_hash": "deadbeef",
                "reset_event": None,
            }}
        if operation == "omp.context.usage":
            return {"schema": "omp.context.usage.v1", "result": usage}
        if operation == "omp.context.epoch":
            return {"schema": "omp.context.epoch.v1", "result": 7}
        if operation == "omp.context.message.parts":
            return {"schema": "omp.context.message.parts.v1", "result": [
                {"kind": "text", "text": "durable part"},
            ]}
        if operation == "omp.context.message.verdict":
            return {"schema": "omp.context.message.verdict.v1", "result": ContextResult(3)}
        if operation == "omp.context.message.raw_args":
            return {"schema": "omp.context.message.raw_args.v1", "result": {
                "base64": base64.b64encode(b'{"path":"x"}').decode(),
            }}
        if operation == "omp.context.pin":
            return {"schema": "omp.context.pin.v1", "result": len(arguments["ids"])}
        if operation == "omp.context.unpin":
            return {"schema": "omp.context.unpin.v1", "result": len(arguments["ids"])}
        if operation == "omp.context.compact":
            return {"schema": "omp.context.compact.v1", "result": {
                "preparation_id": "compact-1",
                "tiers_run": ["prune", "local"],
                "from_extension": None,
                "tokens_before": 90,
                "tokens_after": 40,
                "first_kept_id": "item-4",
                "epoch": 8,
                "summary_bytes": 20,
                "warning": None,
            }}
        if operation == "omp.journal.append":
            return {"schema": "omp.journal.append.v1", "result": {
                "session": "session-1", "index": 12,
            }}
        if operation == "omp.journal.append_many":
            return {"schema": "omp.journal.append_many.v1", "result": [
                {"session": "session-1", "index": 13},
                {"session": "session-1", "index": 14},
            ]}
        if operation == "omp.journal.append_atomic":
            return {"schema": "omp.journal.append_atomic.v1", "result": [
                {"session": "session-1", "index": 15},
                {"session": "session-1", "index": 16},
            ]}
        if operation == "omp.journal.entries":
            return {"schema": "omp.journal.entries.v1", "result": [row]}
        if operation == "omp.journal.latest":
            return {"schema": "omp.journal.latest.v1", "result": row}
        if operation == "omp.journal.label":
            self.label = arguments["label"]
            index = self.next_label_index
            self.next_label_index += 1
            return {"schema": "omp.journal.label.v1", "result": {
                "session": "session-1", "index": index,
            }}
        if operation == "omp.journal.label_of":
            return {"schema": "omp.journal.label_of.v1", "result": self.label}
        raise AssertionError(operation)


async def exercise():
    backend = Backend()
    token = omp._control_backend.set(backend)
    try:
        projected = await omp.context.view()
        assert projected.messages[0].kind is omp.MessageKind.TOOL_RESULT
        assert projected.messages[0].tool == omp.ToolRef("read", "hl", 1)
        assert projected.usage.compaction_epoch == 7
        assert (await projected.messages[0].parts())[0].text == "durable part"
        assert await projected.messages[0].verdict() == ContextResult(3)
        assert await projected.messages[0].raw_args() == b'{"path":"x"}'
        assert (await omp.context.usage()).usable_tokens == 90
        assert await omp.context.pin(("item-4",), reason="memory") == 1
        assert await omp.context.unpin(("item-4",)) == 1
        compacted = await omp.context.compact(tier=omp.CompactionTier.LOCAL, focus="facts")
        assert compacted.epoch == 8

        appended = await omp.journal.append(
            Observation("durable", 2), idempotency_key="append-1"
        )
        assert str(appended) == "session-1:12"
        append_call = next(args for op, args in backend.calls if op == "omp.journal.append")
        assert append_call["entry"] == {
            "schema": "omp.journal.entry.v1",
            "kind": "dev.example.context-journal",
            "rev": "v.1",
            "data": '{"rank":2,"text":"durable"}',
            "display": None,
            "spill": False,
        }
        assert append_call["idempotency_key"] == "append-1"
        assert append_call["expected_context_epoch"] is None

        many = await omp.journal.append_many(
            (Observation("a", 1), Observation("b", 2)),
            idempotency_key="many-1",
        )
        assert [item.index for item in many] == [13, 14]
        atomic = await omp.journal.append_atomic(
            (Observation("a", 1), Observation("b", 2)),
            idempotency_key="atomic-1",
        )
        assert [item.index for item in atomic] == [15, 16]

        records = await omp.journal.entries(Observation)
        assert len(records) == 1 and records[0].value == Observation("durable", 2)
        assert records[0].raw == b'{"rank":2,"text":"durable"}'
        assert (await omp.journal.latest(Observation)).id.index == 12
        total, watermark = await omp.journal.fold(
            Observation, lambda acc, entry: acc + entry.value.rank, 0
        )
        assert total == 2 and watermark.index == 12
        assert (await omp.journal.label(appended, "memory")).index == 17
        assert await omp.journal.label_of(appended) == "memory"
        assert (await omp.journal.label(appended, None)).index == 18
        assert await omp.journal.label_of(appended) is None

        async with omp.context.lane(strict_epoch=True):
            await omp.journal.append(
                Observation("fenced", 3), idempotency_key="append-fenced"
            )
        fenced = [
            args for op, args in backend.calls
            if op == "omp.journal.append" and args["idempotency_key"] == "append-fenced"
        ][0]
        assert fenced["expected_context_epoch"] == 7
    finally:
        omp._control_backend.reset(token)


asyncio.run(exercise())
"#
				),
				None,
				None,
			)
		})
		.expect("context and journal CONTROL contract");
}
