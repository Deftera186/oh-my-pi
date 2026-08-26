from __future__ import annotations

import ast
import io
import json
import os
import queue
import select
import socket
import sys
import threading
import time
import traceback

_MAX_FRAME_BYTES = 64 * 1024 * 1024
_connect_address = None
_connect_secret = None
if len(sys.argv) == 4 and sys.argv[1] == "--omp-connect":
    _connect_address = sys.argv[2]
    _connect_secret = sys.argv[3]
    del sys.argv[1:]
_protocol_socket = None
if _connect_address is not None and _connect_secret is not None:
    host, port = _connect_address.rsplit(":", 1)
    _protocol_socket = socket.create_connection((host, int(port)), timeout=30)
    _protocol_socket.settimeout(None)
    _protocol_in = _protocol_socket.makefile("rb", buffering=0)
    _protocol_out = _protocol_socket.makefile("wb", buffering=0)
else:
    _protocol_in = os.fdopen(os.dup(0), "rb", buffering=0)
    _protocol_out = os.fdopen(os.dup(1), "wb", buffering=0)
_null_fd = os.open(os.devnull, os.O_RDONLY)
os.dup2(_null_fd, 0)
os.close(_null_fd)
_stdout_read, _stdout_write = os.pipe()
_stderr_read, _stderr_write = os.pipe()
os.dup2(_stdout_write, 1)
os.dup2(_stderr_write, 2)
os.close(_stdout_write)
os.close(_stderr_write)
_write_lock = threading.Lock()
_commands: queue.Queue[dict] = queue.Queue()
_pending_lock = threading.Lock()
_pending: dict[int, queue.Queue[dict]] = {}
_capture_queues = (queue.Queue(), queue.Queue())
_cell_cancelled = threading.Event()
_active_run = 0
_runtime_cwd = None
_token = ""


def _write(frame: dict) -> None:
    encoded = json.dumps(frame, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    if not encoded or len(encoded) > _MAX_FRAME_BYTES:
        raise RuntimeError("eval child frame exceeds protocol limit")
    with _write_lock:
        _protocol_out.write(encoded + b"\n")
        _protocol_out.flush()


if _connect_secret is not None:
    _write({"secret": _connect_secret})


def _read() -> dict | None:
    encoded = _protocol_in.readline(_MAX_FRAME_BYTES + 2)
    if not encoded:
        return None
    if len(encoded) > _MAX_FRAME_BYTES + 1 or not encoded.endswith(b"\n"):
        raise RuntimeError("invalid eval parent frame")
    return json.loads(encoded)


def _reader() -> None:
    try:
        while True:
            frame = _read()
            if frame is None:
                os._exit(0)
            kind = frame.get("kind")
            if kind == "cancel":
                if frame.get("run_id") == _active_run:
                    with _pending_lock:
                        _cell_cancelled.set()
            elif kind in ("bridge_progress", "bridge_response"):
                request_id = frame.get("request_id")
                with _pending_lock:
                    target = _pending.get(request_id)
                if target is not None:
                    target.put(frame)
            else:
                _commands.put(frame)
    except BaseException as error:
        try:
            _write({"kind": "fatal", "message": f"external Python protocol reader failed: {error}"})
        finally:
            os._exit(1)


def _capture(descriptor: int, channel: str, commands: queue.Queue) -> None:
    while True:
        readable, _, _ = select.select((descriptor,), (), (), 0.001)
        if readable:
            data = os.read(descriptor, 16 * 1024)
            if not data:
                return
            if _active_run:
                _write({"kind": channel, "run_id": _active_run, "update": {
                    "channel": channel, "data": list(data), "sequence": 0
                }})
            continue
        try:
            while True:
                commands.get_nowait().set()
        except queue.Empty:
            pass


def _drain_capture() -> None:
    acknowledgements = (threading.Event(), threading.Event())
    for commands, acknowledgement in zip(_capture_queues, acknowledgements):
        commands.put(acknowledgement)
    for acknowledgement in acknowledgements:
        acknowledgement.wait()


def _bridge_call(name: str, args):
    global _next_request
    with _pending_lock:
        if _cell_cancelled.is_set():
            raise RuntimeError("eval cell cancelled")
        request_id = _next_request
        _next_request += 1
        responses: queue.Queue[dict] = queue.Queue()
        _pending[request_id] = responses
        _write({
            "kind": "bridge_call",
            "run_id": _active_run,
            "request_id": request_id,
            "token": _token,
            "name": name,
            "args": args,
        })
    updates = []
    try:
        while True:
            frame = responses.get()
            if frame["kind"] == "bridge_progress":
                updates.append(frame.get("event"))
                continue
            error = frame.get("error")
            if error is not None:
                raise RuntimeError(error)
            return {
                "__omp_bridge_value__": frame.get("value"),
                "__omp_bridge_updates__": updates,
            }
    finally:
        with _pending_lock:
            _pending.pop(request_id, None)


def _json_value(value):
    try:
        json.dumps(value)
        return value
    except (TypeError, ValueError, OverflowError):
        return None


def _display(value, raw=False):
    if raw and isinstance(value, dict):
        if "application/x-omp-status" in value:
            _write({"kind": "display", "run_id": _active_run, "output": {
                "type": "status", "event": value["application/x-omp-status"]
            }})
            return
        if "text/markdown" in value:
            _write({"kind": "display", "run_id": _active_run, "output": {
                "type": "markdown", "text": str(value["text/markdown"])
            }})
            return
        if "application/json" in value:
            value = value["application/json"]
    encoded = _json_value(value)
    if encoded is not None:
        _write({"kind": "display", "run_id": _active_run, "output": {
            "type": "json", "data": encoded
        }})


class _Stream(io.TextIOBase):
    def __init__(self, channel: str):
        self._channel = channel

    @property
    def encoding(self):
        return "utf-8"

    def writable(self):
        return True

    def write(self, text):
        if not isinstance(text, str):
            text = str(text)
        if text:
            data = list(text.encode("utf-8", errors="replace"))
            _write({"kind": self._channel, "run_id": _active_run, "update": {
                "channel": self._channel, "data": data, "sequence": 0
            }})
        return len(text)

    def flush(self):
        return None


def _apply_runtime(runtime: dict) -> None:
    global _runtime_cwd
    cwd = runtime.get("cwd")
    if not isinstance(cwd, str) or not os.path.isabs(cwd):
        raise RuntimeError("eval runtime omitted an absolute working directory")
    os.chdir(cwd)
    if _runtime_cwd is not None:
        sys.path[:] = [entry for entry in sys.path if entry != _runtime_cwd]
    sys.path.insert(0, cwd)
    _runtime_cwd = cwd
    managed = runtime.get("managed_env")
    if not isinstance(managed, dict):
        raise RuntimeError("eval runtime omitted managed environment")
    for key in ("OMP_ARTIFACTS_DIR", "OMP_EVAL_LOCAL_ROOTS", "OMP_SESSION_FILE"):
        value = managed.get(key)
        if value is None:
            os.environ.pop(key, None)
        elif isinstance(value, str):
            os.environ[key] = value
        else:
            raise RuntimeError(f"invalid managed environment value for {key}")


def _execute(code: str, namespace: dict):
    module = ast.parse(code, mode="exec")
    if module.body and isinstance(module.body[-1], ast.Expr):
        prefix = ast.Module(body=module.body[:-1], type_ignores=module.type_ignores)
        if prefix.body:
            exec(compile(prefix, "<omp-eval>", "exec"), namespace, namespace)
        expression = ast.Expression(module.body[-1].value)
        return True, eval(compile(expression, "<omp-eval>", "eval"), namespace, namespace)
    exec(compile(module, "<omp-eval>", "exec"), namespace, namespace)
    return False, None


def _exception(error: BaseException) -> dict:
    return {
        "name": type(error).__name__,
        "message": str(error),
        "traceback": traceback.format_exception(type(error), error, error.__traceback__),
    }


def _done(run_id: int, outcome: str, started: float) -> None:
    _drain_capture()
    _write({
        "kind": "done",
        "run_id": run_id,
        "status": {
            "outcome": outcome,
            "exit_code": 0 if outcome == "complete" else 1,
            "duration_ms": int((time.monotonic() - started) * 1000),
            "exception": None,
        },
        "truncated": False,
        "spilled_output": None,
        "total_lines": 0,
        "total_bytes": 0,
    })


def main() -> None:
    global _active_run, _next_request, _token
    init = _read()
    if init is None or init.get("kind") != "init":
        raise RuntimeError("Init must be the first eval child frame")
    parent_pid = init.get("parent_pid")
    if not isinstance(parent_pid, int) or parent_pid <= 1 or os.getppid() != parent_pid:
        raise RuntimeError("eval child parent identity is invalid")
    _token = init["token"]
    _next_request = 1
    namespace = {
        "__name__": "__main__",
        "__omp_bridge_call__": _bridge_call,
        "__omp_bridge_session__": init.get("session_id"),
        "__omp_display": _display,
    }
    exec(init["python_prelude"], namespace, namespace)
    namespace["__omp_install_prelude_helpers__"](init.get("prelude", []))
    sys.stdin = open(os.devnull, "r", encoding="utf-8")
    sys.stdout = _Stream("stdout")
    sys.stderr = _Stream("stderr")
    threading.Thread(target=_reader, name="omp-eval-protocol", daemon=True).start()
    threading.Thread(
        target=_capture,
        args=(_stdout_read, "stdout", _capture_queues[0]),
        name="omp-eval-stdout",
        daemon=True,
    ).start()
    threading.Thread(
        target=_capture,
        args=(_stderr_read, "stderr", _capture_queues[1]),
        name="omp-eval-stderr",
        daemon=True,
    ).start()
    _write({"kind": "ready"})
    while True:
        frame = _commands.get()
        kind = frame.get("kind")
        if kind == "exit":
            return
        if kind != "run" or _active_run:
            raise RuntimeError("eval child received an invalid or overlapping run")
        run_id = frame["run_id"]
        _active_run = run_id
        _cell_cancelled.clear()
        started = time.monotonic()
        try:
            _apply_runtime(frame["runtime"])
            if frame.get("reset"):
                preserved = {
                    key: namespace[key]
                    for key in tuple(namespace)
                    if (key.startswith("__omp_") and key != "__omp_prelude_loaded__")
                    or key in ("__name__",)
                }
                namespace.clear()
                namespace.update(preserved)
                exec(init["python_prelude"], namespace, namespace)
                namespace["__omp_install_prelude_helpers__"](init.get("prelude", []))
            _write({"kind": "started", "run_id": run_id, "cell_id": frame["cell_id"]})
            has_result, result = _execute(frame["code"], namespace)
            if has_result:
                _write({"kind": "result", "run_id": run_id, "value": {
                    "text": repr(result), "json": _json_value(result)
                }})
            _done(run_id, "complete", started)
        except KeyboardInterrupt as error:
            _write({"kind": "error", "run_id": run_id, "value": _exception(error)})
            _done(run_id, "cancelled", started)
        except BaseException as error:
            _write({"kind": "error", "run_id": run_id, "value": _exception(error)})
            _done(run_id, "error", started)
        finally:
            _active_run = 0


if __name__ == "__main__":
    try:
        main()
    except BaseException as error:
        _write({"kind": "fatal", "message": f"external Python runner failed: {error}"})
        raise
