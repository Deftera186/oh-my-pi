# omp-envd

`omp-envd` is OMP's live project-environment host. It assembles and serves the
environment daemon and owns project-scoped filesystem and document access,
process execution, workspace search, blob storage, tool dispatch, policy, and
extension runtime resources exposed through the environment protocol.

This is the crate to change for host behavior. `omp-env` is only the typed
client and framing boundary; it does not contain an alternate host.

## Structure

- `server` owns environment-service dispatch, project state, client
  connections, and the `EnvServer`/`EnvdError` server boundary.
- `workspace`, `docs`, `document_cache`, `search_backend`, and `tool_search`
  provide workspace, search, and document operations.
- `exec`, `process_store`, `process_log`, `shell_profile`, and `direnv` manage
  commands, named processes, logs, and shell environment setup.
- `tools` and the `tool_*` modules implement daemon-backed tool operations.
- `exthost` owns extension manifests, lifecycle, CONTROL routing, quotas,
  service routing, cancellation, and the extension-host child entry point.
  `worker` supervises the same-binary free-threaded Python extension hosts and
  Python tool workers; `worker_pool` owns named-worker routing and
  generation-fenced DATA transport.
- `policy`, `admission`, `http_egress`, `vault`, and `recovery` enforce access
  decisions and manage durable runtime state.
- `run` starts the platform transport. `ProjectEnvironment::attach` joins the
  build-keyed detached daemon and composes session-only tools locally.

The `omp` executable recognizes the hidden eval, extension-host, and Python
worker child arguments because those children re-enter the same binary.
Their entry functions and runtime implementations remain owned by
`omp-envd`; `omp-app` only performs process-level dispatch.

## Philosophy

Each project and executable generation has one detached environment daemon.
Environment-locus tools and filesystem, process, document, browser, debugger,
and memory effects execute there. Session-locus tools, extension workers, MCP,
presenters, and agent controls stay in the attaching process behind the same
partitioned `EnvClient`. An embedded full host is used only as a loud spawn
fallback or by explicitly isolated compositions.

The document socket is build-stable while environment sockets are build-keyed.
`DocumentHost` reconnects after a server restart, and a surviving current-build
environment may rehost the document authority without invalidating its clones.
A stale-build daemon drains without rehosting and releases authority as soon as
its last client disconnects.

The crate is deliberately below the headless driver and application layers.
Capabilities that require regime state, inference composition,
application-authored content, host RPC resources, or telemetry delivery enter
through `RegistryBridges`. `omp-driver` constructs those bridges and the
session composition; `omp-envd` does not import app presentation policy.

## Development

Run `just setup-python` once before commands that link embedded Python. Then
use the workspace recipes:

- `just check-pkg omp-envd`
- `just test-pkg omp-envd`

Run joined behavior separately with `just e2e` or the exact narrower E2E
recipe shown by `just --list`.
