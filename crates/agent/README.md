# omp-agent

`omp-agent` provides the transport-neutral foundations for OMP's durable,
interruptible agent loop. It keeps the canonical `omp.thread.v1.Item` as the
only conversation shape and composes the live turn protocol with immutable
configuration, deterministic prompt heads, ordered interrupts, event fan-out,
journal projection, supervised tool batches, and detached-job settlement.

`AgentState` publishes immutable snapshots through a watch value. Each logical
turn re-reads the latest options, enabled tools, live revisioned registry,
workspace bytes, prompt source, interrupt policy, deadline, and bounded retry
policy without sharing mutable configuration. Registry identity is hashed
separately from prompt content so revision swaps invalidate held arbiter
context and re-project durable history through the current lift chains. Prompt
sources are synchronous and receive an immutable
workspace capture; every render is repeated and compared before canonical
system items and their stable SHA-256 hash are accepted.

The transcript journal is durable truth. Arbiter context is only a working
copy: projection rebuilds canonical threads, applies amendments and rewinds,
and lets the live tool registry lift historical results. One flume mailbox
orders immediate, turn-boundary, and idle inputs. Tool calls execute only
through `omp-env`; event subscribers observe shared payloads without feeding
back into the loop, and detached jobs settle through the same mailbox.

Cross-cutting policy lives in regimes, never in the loop. The `Arbiter`
resolves every fixed decision point (`CONTEXT` … `IDLE`) by folding durable
activations and the always-on core lanes (stream rules, empty-output retry,
checkpoint notice, provider failover) through one draft vocabulary: facts in
(`PointCx` — turn identity, stream part and delta, `empty_output`,
`trailing_aborts`, `hidden`), effects and one control out (inject items,
scoped settings, durable notes; retry/cancel/fail). The loop consumes only
the resolved control: a STREAM cancel becomes a generic
abort-and-continue transition carrying the resolution's injects, SETTLE
retry decisions open the follow-up turn with the staged reminder, and
post-settlement BATCH / IDLE injects are delivered at the next safe
boundary. Retry state is never mirrored: the empty-output lane reads the
journal's recoverable-abort projection directly.

The crate contains no provider, application, UI, docserver, or shell-engine
dependencies. Its `Agent` policy loop composes these foundations with a
`TurnClient`; RPC and in-process sessions retain the same generated protocol
contract and stable `TurnId` replay behavior.
