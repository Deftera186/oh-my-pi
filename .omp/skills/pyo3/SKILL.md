---
name: pyo3
description: MUST use when touching any PyO3 boundary code.
---

# PyO3 (0.29, free-threaded embedding)

Repo context: pyo3 `0.29.2` (workspace-pinned), statically embedded CPython
**3.14t — free-threaded, GIL off**. Modules register via `append_to_inittab!`
before `Builder::init` (`crates/py/src/lib.rs`); bindings live in
`crates/py/src/bindings.rs` and are the house reference implementation.

## Non-negotiables

1. **0.29 names only.** `Python::attach` (not `with_gil`), `Python::detach`
   (not `allow_threads`), `Python::initialize` (not
   `prepare_freethreaded_python`), `PyOnceLock` (not `GILOnceCell`),
   `IntoPyObject`/`IntoPyObjectExt` (not `ToPyObject`/`IntoPy`). GIL-refs
   (`&PyAny`) are gone; everything is `Bound<'py, T>`.
2. **`Bound<'py, T>` in signatures and locals; `Py<T>` only for storage**
   (struct fields, statics, cross-thread). `py_obj.bind(py)` /
   `bound.unbind()` are free; `Py::clone_ref` is one atomic — don't clone
   when a borrow works. Get the token from `bound.py()`, never a nested
   `Python::attach`.
3. **`#[pyclass(frozen, module = "_omp")]` by default.** Frozen deletes the
   per-call atomic borrow-checker; unfrozen classes raise
   `RuntimeError: Already borrowed` under concurrent 3.14t callers.
   Mutability goes through atomics or short `parking_lot` locks inside a
   frozen class. `module = "…"` is required — omitting it breaks pickle and
   repr (`builtins.Foo`).
4. **Newtype wrappers, never `#[pyclass]` on domain types.**
   `struct PyFoo(Foo)` keeps pyo3 out of core crates; conversion is explicit
   at the boundary. See `PyDuration`, `PyEnvPath`, `PyPrincipal` in
   `bindings.rs`.
5. **Typed Rust errors inside, `PyErr` at the boundary.** thiserror enums in
   libraries; `impl From<MyError> for PyErr` mapping onto the
   `create_exception!` hierarchy rooted at `OmpError` (top of `bindings.rs`).
   `PyErr` construction is lazy (no Python objects until it crosses into
   Python) — never format/normalize eagerly. Chain causes with
   `err.set_cause(py, Some(root))`.
6. **`py.detach(|| …)` around anything blocking or >~1ms of pure Rust.**
   GIL-off does not remove this: an attached thread stalls stop-the-world
   events (GC, fork) for every Python thread. Never hold a `Bound` across
   `detach` — unbind or extract first.
7. **Zero-copy at the boundary.** `&str`/`PyString::to_str`,
   `PyBytes::as_bytes` borrow Python's buffers; `PyBytes::new_with` writes
   in place (no `Vec` + memcpy); `PyBackedStr`/`PyBackedBytes` when the data
   must outlive `'py` (Send+Sync, still zero-copy); `PyBuffer<T>` for
   bytearray/array/ndarray views. `extract::<String>`/`Vec<u8>` copies —
   only when ownership is genuinely needed.
   Treat borrowed, owned, and transferred foreign buffers as distinct states.
   Require unique transfer through vetted PyO3 or CPython APIs before taking
   mutable Rust ownership.
8. **`cast` for branching, `extract` for conversion.** `cast::<PyList>()`
   is a type-slot check; a failed `extract` materializes a full `PyErr`.
   Intern every repeated attr/method/dict-key literal:
   `obj.getattr(intern!(py, "name"))`.
   Never intern dynamic or user-derived strings.
9. **Statics that hold Python objects use `PyOnceLock`** (detaches while
   waiting → deadlock-free). Plain `LazyLock` + `parking_lot` is fine for
   pure-Rust state (see `RUNTIME` in `bindings.rs`) — but never call back
   into Python while holding such a lock; if a lock must be held attached
   and contended, `std::sync::Mutex` + `MutexExt::lock_py_attached`.
10. **`#[pymodule(gil_used = false)]`** stays explicit on every module
    (`gil_used = true` silently re-enables the GIL at import). A pyclass
    holding `Py<T>` needs `__traverse__`/`__clear__` or it leaks cycles;
    never touch the interpreter inside `__traverse__` (panics).

## References

- Performance & zero-copy — pointer cost model, extraction/call costs,
  `frozen`/`freelist`/`immutable_type` knobs, iteration fast paths, lazy
  `PyErr`: [references/performance.md](references/performance.md)
- Design & maintainability — module shape, submodule `sys.modules` gotcha,
  error taxonomy, `#[pymethods]` conventions, GC slots, embedding hygiene,
  testing: [references/design.md](references/design.md)

## House patterns (crates/py/src/bindings.rs)

- Enum vocab exposed to Python: `string_enum!` macro (classattr constants +
  `__str__`/`__repr__`/`__hash__`/`__richcmp__` from one table) — the
  sanctioned macro escape hatch; the inner Rust enum's strings still come
  from strum.
- Blocking bridge: one shared static tokio runtime; `#[pyfunction]` does
  `ASYNC_RUNTIME.block_on(...)` and maps `ClientError` → exception taxonomy
  (`_read_bytes_blocking`).
- Read-only mappings returned to Python are wrapped in
  `types.MappingProxyType` rather than exposing a mutable dict.
- Host→Python state pushes (`set_environment_root`, `set_resource_receipt`)
  do no Python work: they write `parking_lot`-guarded Rust state that
  pyfunctions read later.
