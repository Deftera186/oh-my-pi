"""Typed declarations and CONTROL access for the authoritative session journal.

Importing this module performs no I/O. Operations resolve the active host bridge
at call time and never retain a Python-side copy of journal state.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass, fields, is_dataclass
from enum import Enum
import base64
import json
import uuid
from typing import Any, Generic, TypeVar

from _omp import OmpError, Principal

from ._verdicts import ArtifactRef
from .packages import Provenance
_T = TypeVar("_T")
_A = TypeVar("_A")

MAX_INLINE_BYTES = 65_536
"""Largest entry encoded inline before artifact spilling is required."""

MAX_ENTRY_BYTES = 16_777_216
"""Hard encoded-size ceiling for one journal entry."""

MAX_LABEL_BYTES = 256
"""Maximum UTF-8 byte length of a journal label."""

MAX_ATOMIC_ENTRIES = 1_024
"""Maximum number of entries accepted by one atomic append."""


@dataclass(frozen=True, slots=True, order=True)
class EntryId:
    """Opaque, totally ordered physical index within one session journal."""

    session: str
    index: int

    @classmethod
    def parse(cls, value: str) -> EntryId:
        """Parse the canonical ``<session_id>:<index>`` representation."""

        if not isinstance(value, str):
            raise TypeError("entry id must be a string")
        session, separator, raw_index = value.rpartition(":")
        if (
            not separator
            or not session
            or not raw_index.isascii()
            or not raw_index.isdecimal()
            or (len(raw_index) > 1 and raw_index.startswith("0"))
        ):
            raise ValueError(f"invalid entry id: {value!r}")
        return cls(session=session, index=int(raw_index))

    def __str__(self) -> str:
        """Render this id as ``<session_id>:<index>``."""

        return f"{self.session}:{self.index}"


class JournalError(OmpError):
    """Base error for journal operations and partial multi-entry appends."""

    def __init__(
        self,
        message: str,
        *,
        appended: Iterable[EntryId] = (),
    ) -> None:
        super().__init__(message)
        self.appended: list[EntryId] = list(appended)


class UnknownEntryKind(JournalError):
    """An append payload is not an instance of a declared entry kind."""

    def __init__(self, kind: object) -> None:
        self.kind = kind
        super().__init__(f"unknown journal entry kind: {kind!r}")


class EntryKindConflict(JournalError):
    """An entry-kind name is already owned by another declaration."""

    def __init__(self, name: str, owner: str | None = None) -> None:
        self.name = name
        self.owner = owner
        detail = f" by {owner!r}" if owner is not None else ""
        super().__init__(f"journal entry kind {name!r} is already owned{detail}")


class EntryTooLarge(JournalError):
    """A journal entry exceeds its applicable encoded-size ceiling."""

    def __init__(self, actual: int, limit: int) -> None:
        self.actual = actual
        self.limit = limit
        super().__init__(f"journal entry is {actual} bytes; limit is {limit}")


class EntryAccessDenied(JournalError):
    """The caller may not read the requested entry-kind namespace."""

    def __init__(self, kind: str) -> None:
        self.kind = kind
        super().__init__(f"journal entry kind {kind!r} is not readable")


class JournalIndeterminate(JournalError):
    """A journal mutation's durability could not be proven."""

    def __init__(
        self, operation: str = "journal mutation", *, appended: Iterable[EntryId] = ()
    ) -> None:
        self.operation = operation
        super().__init__(
            f"{operation} has an indeterminate durability outcome", appended=appended
        )


class EntryUndecodable(JournalError):
    """Canonical entry bytes could not be decoded without repair."""

    def __init__(self, raw: bytes, reason: str) -> None:
        self.raw = raw
        self.reason = reason
        super().__init__(f"journal entry bytes are not canonical: {reason}")


@dataclass(frozen=True, slots=True, order=True)
class StateEntryId:
    """Opaque, totally ordered physical index within one scoped state log."""

    scope: str
    index: int

    def __str__(self) -> str:
        """Render this id as ``<scope_instance>:<index>``."""

        return f"{self.scope}:{self.index}"


@dataclass(frozen=True, slots=True)
class JournalEntry(Generic[_T]):
    """Immutable decoded view of one durable session-journal record."""

    id: EntryId
    kind: str
    rev: str
    ts: int
    principal: Principal
    provenance: Provenance
    value: _T | None
    raw: bytes
    display: bool
    in_context: bool
    artifact: ArtifactRef | None = None


@dataclass(frozen=True, slots=True)
class StateEntry(Generic[_T]):
    """Immutable decoded view of one durable scoped-state record."""

    id: StateEntryId
    kind: str
    rev: str
    ts: int
    principal: Principal
    provenance: Provenance
    value: _T | None
    raw: bytes
    artifact: ArtifactRef | None = None


def _payload(response: object, schema: str) -> object:
    if isinstance(response, Mapping) and "schema" in response:
        if response["schema"] != schema:
            raise TypeError(
                f"expected {schema} response, got {response['schema']!r}"
            )
        if "result" in response:
            return response["result"]
    return response


def _json_value(value: object) -> object:
    if is_dataclass(value) and not isinstance(value, type):
        return {
            item.name: _json_value(getattr(value, item.name))
            for item in fields(value)
        }
    if isinstance(value, Enum):
        return _json_value(value.value)
    if isinstance(value, Mapping):
        result: dict[str, object] = {}
        for key, item in value.items():
            if not isinstance(key, str):
                raise TypeError("journal entry mappings require string keys")
            result[key] = _json_value(item)
        return result
    if isinstance(value, (list, tuple)):
        return [_json_value(item) for item in value]
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    raise TypeError(
        f"journal entry field {type(value).__qualname__} is not JSON serializable"
    )


def _entry_definitions() -> tuple[object, ...]:
    from ._registry import registry

    return registry.entry_kind_definitions()


def _definition_for_entry(entry: object) -> object:
    for definition in _entry_definitions():
        if type(entry) is definition.implementation:
            return definition
    raise UnknownEntryKind(type(entry).__qualname__)


def _kind_name(kind: str | type[object] | None) -> str | None:
    if kind is None:
        return None
    if isinstance(kind, str):
        if not kind:
            raise ValueError("journal kind must not be empty")
        return kind
    if isinstance(kind, type):
        for definition in _entry_definitions():
            if kind is definition.implementation:
                return definition.name
        raise UnknownEntryKind(kind.__qualname__)
    raise TypeError("journal kind must be a string, declared type, or None")


def _entry_wire(entry: object) -> dict[str, object]:
    definition = _definition_for_entry(entry)
    data = json.dumps(
        _json_value(entry),
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
        sort_keys=True,
    )
    encoded_length = len(data.encode("utf-8"))
    if encoded_length > MAX_ENTRY_BYTES:
        raise EntryTooLarge(encoded_length, MAX_ENTRY_BYTES)
    if encoded_length > MAX_INLINE_BYTES and not definition.spill:
        raise EntryTooLarge(encoded_length, MAX_INLINE_BYTES)
    return {
        "schema": "omp.journal.entry.v1",
        "kind": definition.name,
        "rev": definition.rev,
        "data": data,
        "display": definition.display,
        "spill": definition.spill,
    }


def _entry_id(value: object) -> EntryId:
    if isinstance(value, EntryId):
        return value
    if isinstance(value, str):
        return EntryId.parse(value)
    if not isinstance(value, Mapping):
        raise TypeError("journal entry id must be a string or mapping")
    session = value.get("session")
    index = value.get("index")
    if not isinstance(session, str) or not session:
        raise TypeError("journal entry id session must be a non-empty string")
    if not isinstance(index, int) or isinstance(index, bool) or index < 0:
        raise TypeError("journal entry id index must be a non-negative integer")
    return EntryId(session, index)


def _artifact(value: object) -> ArtifactRef | None:
    if value is None or isinstance(value, ArtifactRef):
        return value
    if not isinstance(value, Mapping):
        raise TypeError("journal artifact reference must be a mapping")
    return ArtifactRef(
        id=str(value["id"]),
        hash=str(value["hash"]),
        media_type=str(value["media_type"]),
        byte_len=int(value["byte_len"]),
    )


def _raw_bytes(row: Mapping[str, object]) -> bytes:
    raw = row.get("raw")
    if isinstance(raw, bytes):
        return raw
    if isinstance(raw, str):
        return raw.encode("utf-8")
    encoded = row.get("raw_base64")
    if isinstance(encoded, str):
        try:
            return base64.b64decode(encoded, validate=True)
        except ValueError as error:
            raise TypeError("journal raw_base64 is invalid") from error
    raise TypeError("journal entry must contain raw canonical JSON")


def _implementation(kind: str, rev: str) -> type | None:
    exact = None
    current = None
    for definition in _entry_definitions():
        if definition.name != kind:
            continue
        current = definition.implementation
        if definition.rev == rev:
            exact = definition.implementation
    return exact or current


def _entry_value(
    row: Mapping[str, object], kind: str, rev: str, raw: bytes
) -> object | None:
    implementation = _implementation(kind, rev)
    supplied = row.get("value")
    if implementation is not None and isinstance(supplied, implementation):
        return supplied
    try:
        value = decode(raw)
    except EntryUndecodable:
        return None
    if implementation is None:
        return supplied if "value" in row else value
    exact = any(
        definition.name == kind
        and definition.rev == rev
        and definition.implementation is implementation
        for definition in _entry_definitions()
    )
    if not exact:
        lift = getattr(implementation, "lift", None)
        if callable(lift):
            lifted = lift(rev, raw)
            if lifted is not None:
                return lifted
    if isinstance(value, Mapping):
        try:
            return implementation(**value)
        except (TypeError, ValueError):
            return None
    return None


def _journal_entry(value: object) -> JournalEntry[object]:
    if isinstance(value, JournalEntry):
        return value
    if not isinstance(value, Mapping):
        raise TypeError("journal entry response must be a mapping")
    entry_id = _entry_id(value["id"])
    kind = value.get("kind")
    rev = value.get("rev")
    if not isinstance(kind, str) or not isinstance(rev, str):
        raise TypeError("journal entry kind and rev must be strings")
    raw = _raw_bytes(value)
    ts = value.get("ts")
    if not isinstance(ts, int) or isinstance(ts, bool):
        raise TypeError("journal entry timestamp must be an integer")
    principal = value.get("principal")
    provenance = value.get("provenance")
    if not isinstance(principal, Principal):
        raise TypeError("journal entry principal must be core-authenticated")
    if not isinstance(provenance, Provenance):
        raise TypeError("journal entry provenance must be typed")
    display = value.get("display", False)
    in_context = value.get("in_context", False)
    if not isinstance(display, bool) or not isinstance(in_context, bool):
        raise TypeError("journal display and in_context must be booleans")
    return JournalEntry(
        id=entry_id,
        kind=kind,
        rev=rev,
        ts=ts,
        principal=principal,
        provenance=provenance,
        value=_entry_value(value, kind, rev, raw),
        raw=raw,
        display=display,
        in_context=in_context,
        artifact=_artifact(value.get("artifact")),
    )


def _idempotency_key(value: str | None) -> str:
    if value is None:
        return uuid.uuid4().hex
    if not isinstance(value, str) or not value:
        raise TypeError("idempotency_key must be a non-empty string or None")
    return value


def _epoch_fence() -> int | None:
    from .context import _journal_epoch_fence

    return _journal_epoch_fence()


def decode(raw: bytes) -> Any:
    """Decode only the exact canonical JSON encoding written by the host."""

    if not isinstance(raw, bytes):
        raise TypeError("journal entry data must be bytes")

    def reject_constant(value: str) -> None:
        raise ValueError(f"non-finite number {value!r}")

    try:
        value = json.loads(raw.decode("utf-8"), parse_constant=reject_constant)
        canonical = json.dumps(
            value,
            ensure_ascii=False,
            allow_nan=False,
            separators=(",", ":"),
            sort_keys=True,
        ).encode("utf-8")
    except (UnicodeError, ValueError, TypeError, OverflowError) as error:
        raise EntryUndecodable(raw, str(error)) from error
    if canonical != raw:
        raise EntryUndecodable(raw, "encoding differs from canonical JSON")
    return value


async def label(target: EntryId, label: str | None) -> EntryId:
    """Append a durable label event for an addressable journal entry."""

    from . import _control_request

    if not isinstance(target, EntryId):
        raise TypeError("target must be an EntryId")
    if label is not None:
        if not isinstance(label, str):
            raise TypeError("label must be a string or None")
        encoded_length = len(label.encode("utf-8"))
        if encoded_length > MAX_LABEL_BYTES:
            raise JournalError(
                f"journal label is {encoded_length} bytes; limit is {MAX_LABEL_BYTES}"
            )
    response = await _control_request(
        "omp.journal.label",
        target=str(target),
        label=label,
        idempotency_key=uuid.uuid4().hex,
        expected_context_epoch=_epoch_fence(),
    )
    return _entry_id(_payload(response, "omp.journal.label.v1"))


async def label_of(target: EntryId) -> str | None:
    """Return the latest live label assignment for a journal entry."""

    from . import _control_request

    if not isinstance(target, EntryId):
        raise TypeError("target must be an EntryId")
    response = _payload(
        await _control_request("omp.journal.label_of", target=str(target)),
        "omp.journal.label_of.v1",
    )
    if response is not None and not isinstance(response, str):
        raise TypeError("omp.journal.label_of returned a non-string label")
    return response


async def append(
    entry: object,
    *,
    display: bool | None = None,
    idempotency_key: str | None = None,
) -> EntryId:
    """Append one declared entry through the authoritative session journal."""

    from . import _control_request

    if display is not None and not isinstance(display, bool):
        raise TypeError("display must be bool or None")
    response = await _control_request(
        "omp.journal.append",
        entry=_entry_wire(entry),
        display=display,
        idempotency_key=_idempotency_key(idempotency_key),
        expected_context_epoch=_epoch_fence(),
    )
    return _entry_id(_payload(response, "omp.journal.append.v1"))


async def append_many(
    entries: Iterable[object], *, idempotency_key: str | None = None
) -> list[EntryId]:
    """Append an ordered, non-atomic group in one CONTROL round trip."""

    from . import _control_request

    batch = [_entry_wire(entry) for entry in entries]
    response = _payload(
        await _control_request(
            "omp.journal.append_many",
            entries=batch,
            idempotency_key=_idempotency_key(idempotency_key),
            expected_context_epoch=_epoch_fence(),
        ),
        "omp.journal.append_many.v1",
    )
    if not isinstance(response, Sequence) or isinstance(
        response, (str, bytes, bytearray)
    ):
        raise TypeError("omp.journal.append_many returned a non-sequence")
    return [_entry_id(value) for value in response]


async def append_atomic(
    entries: Iterable[object], *, idempotency_key: str
) -> list[EntryId]:
    """Append an idempotent group atomically through the authoritative journal."""

    from . import _control_request

    batch = [_entry_wire(entry) for entry in entries]
    if len(batch) > MAX_ATOMIC_ENTRIES:
        raise JournalError(
            f"atomic journal append has {len(batch)} entries; "
            f"limit is {MAX_ATOMIC_ENTRIES}"
        )
    response = _payload(
        await _control_request(
            "omp.journal.append_atomic",
            entries=batch,
            idempotency_key=_idempotency_key(idempotency_key),
            expected_context_epoch=_epoch_fence(),
        ),
        "omp.journal.append_atomic.v1",
    )
    if not isinstance(response, Sequence) or isinstance(
        response, (str, bytes, bytearray)
    ):
        raise TypeError("omp.journal.append_atomic returned a non-sequence")
    return [_entry_id(value) for value in response]


async def entries(
    kind: str | type[_T] | None = None,
    *,
    rev: str | None = None,
    since: EntryId | None = None,
    limit: int | None = None,
    live: bool = True,
) -> Sequence[JournalEntry[_T]]:
    """Read authoritative entries in ascending durable journal order."""

    from . import _control_request

    if rev is not None and (not isinstance(rev, str) or not rev):
        raise TypeError("rev must be a non-empty string or None")
    if since is not None and not isinstance(since, EntryId):
        raise TypeError("since must be an EntryId or None")
    if limit is not None and (
        not isinstance(limit, int) or isinstance(limit, bool) or limit < 0
    ):
        raise TypeError("limit must be a non-negative integer or None")
    if not isinstance(live, bool):
        raise TypeError("live must be bool")
    response = _payload(
        await _control_request(
            "omp.journal.entries",
            kind=_kind_name(kind),
            rev=rev,
            since=None if since is None else str(since),
            limit=limit,
            live=live,
        ),
        "omp.journal.entries.v1",
    )
    if not isinstance(response, Sequence) or isinstance(
        response, (str, bytes, bytearray)
    ):
        raise TypeError("omp.journal.entries returned a non-sequence")
    decoded = tuple(_journal_entry(value) for value in response)
    if any(left.id >= right.id for left, right in zip(decoded, decoded[1:])):
        raise JournalError("host returned journal entries outside durable order")
    return decoded


async def latest(kind: str | type[_T]) -> JournalEntry[_T] | None:
    """Return the highest-index live entry of one kind."""

    from . import _control_request

    response = _payload(
        await _control_request("omp.journal.latest", kind=_kind_name(kind)),
        "omp.journal.latest.v1",
    )
    if response is None:
        return None
    return _journal_entry(response)


async def fold(
    kind: str | type[_T],
    reducer: Callable[[_A, JournalEntry[_T]], _A],
    initial: _A,
    *,
    since: EntryId | None = None,
) -> tuple[_A, EntryId | None]:
    """Fold authoritative live entries left-to-right and return their watermark."""

    if not callable(reducer):
        raise TypeError("reducer must be callable")
    accumulator = initial
    watermark = None
    for entry in await entries(kind, since=since):
        accumulator = reducer(accumulator, entry)
        watermark = entry.id
    return accumulator, watermark


__all__ = (
    "EntryAccessDenied",
    "EntryId",
    "EntryKindConflict",
    "EntryTooLarge",
    "EntryUndecodable",
    "JournalError",
    "JournalIndeterminate",
    "JournalEntry",
    "MAX_ATOMIC_ENTRIES",
    "MAX_ENTRY_BYTES",
    "MAX_INLINE_BYTES",
    "MAX_LABEL_BYTES",
    "StateEntry",
    "StateEntryId",
    "UnknownEntryKind",
    "append",
    "append_atomic",
    "append_many",
    "decode",
    "entries",
    "fold",
    "label",
    "label_of",
    "latest",
)
