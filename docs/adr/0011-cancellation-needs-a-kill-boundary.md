# 0011. Cancellation is a runtime guarantee, not cooperative etiquette

Status: accepted
Date: 2026-09-02
Area: runtime

## Context

In pi and omp v1, extensions and custom tools ran inside the engine's JavaScript isolate. Two things
followed. Hot reload was nearly impossible: there is no way to unload a module that owns live
closures and timers in the same heap. And a tool call could not be forcibly stopped: once it escaped
cooperative cancellation, the only lever left was killing the harness.

JavaScript's `AbortSignal` and Go's `context.Context` are protocols, not enforcement. The failure
modes are ordinary:

- the author forgets to thread the signal into one call;
- a dependency does not accept a signal at all;
- the work is synchronous and never yields to check;
- an infinite retry loop swallows the abort as one more transient error.

In every case a timeout tells the *agent* to move on while the *work* keeps burning CPU, file
handles, and network in the background. Under the Factorio row (0001) nobody is there to notice; under
the multiplexed row the leaked work competes with live agents.

The host/sandbox split (0006) already puts execution outside session authority. Cancellation is the
same boundary viewed from the stop side.

## Decision

- A tool, extension, or job MUST run in an execution unit the host can terminate: a process, a
  worker, a subinterpreter, a VM request, or an equivalent boundary. Termination MUST NOT be able to
  take session authority with it — the unit that dies never owns the journal or the session tree.
- Cancellation is part of the runtime contract. The host MUST first request cooperative settlement,
  then, after a bounded grace, MUST forcibly terminate the unit and record the outcome. A tool author
  who ignores the request cannot prevent the stop.
- After effects have been authorized, forced termination MUST be recorded as uncertainty
  (effects may or may not have happened), never as a silent success or a missing event.
- Extensions and custom tools MUST NEVER share the engine's own runtime heap. Hot reload is a
  consequence: replace the unit, not the module.
- The chosen way to make this boundary pleasant is Python with `@remote` (0036): the extension
  author writes a local-looking function, the runtime ships it to a unit it can kill.

## Consequences

- Every cancellation reaches the resources, not just the awaiting future. Leaked background work is
  a bug in the supervisor, not an expected outcome.
- Hot reload of extensions becomes ordinary process replacement.
- Prohibited: in-process extension execution; tools whose only cancellation is a signal they may
  forget to honour; cancellation semantics that differ by tool.
- Cost accepted: an extra process or worker per execution unit, with the IPC and supervision that
  implies. The supervision code is written once in the engine (0002).

## Status in omp

**Implemented.** Primary implementation: `crates/agent/src/cancel.rs` (scopes) and
`crates/agent/src/dispatch.rs` (the ladder). Every tool scope carries two views of one stop: the
*commit* token a foreground mutation observes (session-only, so a turn interrupt never tears an
atomic commit) and the *interrupt* token the host raises on turn interruption or session
cancellation. `Dispatcher::dispatch` answers the interrupt token with cooperative settlement (feed
interrupt for in-process tools; `ExternalDispatchRequest.cancellation` for worker/remote units,
which `crates/driver/src/headless/kernel.rs` forwards to envd where `ExecHost` applies TERM →
`sv_interrupt_grace` → KILL to the process group), waits `DispatchPolicy::interrupt_grace` for the
unit's own verdict, then forcibly terminates and journals `Abort::EffectsUnknown` — never a missing
`tool.result@1`. A cancelled turn is recorded in the tree (`msg.assistant.end@1 cancelled` when an
assistant is open, then `<notice kind=warn>`), so the chat host derives "turn over" from the DOM.
Proof: `crates/agent/tests/dispatch.rs::interrupt_kills_a_running_shell_tool_and_settles_aborted`,
`crates/agent/tests/cancel.rs`, `crates/e2e/tests/p7_tui.rs` (real PTY ctrl+c over a `sleep 30`).

## References

- The Harness Playbook, "The runtime" — "Cancellation requires a kill boundary", "Make the
  mandatory boundary pleasant"
- 0006 (host/sandbox split), 0010 (one job primitive), 0008 (call element status), 0036 (Python,
  `@remote`), 0002 (supervision owned once)
- `crates/envd/src/worker.rs`, `crates/envd/src/eval/process.rs`, `crates/agent/src/batch.rs`,
  `crates/agent/src/loop.rs`, `crates/agent/src/jobs.rs`, `crates/e2e/tests/p2_cancel_matrix.rs`
- Prior art named by the post: JavaScript `AbortSignal`, Go `context.Context`
