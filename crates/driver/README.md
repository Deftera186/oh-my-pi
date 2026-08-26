# omp-driver

`omp-driver` is OMP's headless coding-agent harness and reusable composition
boundary. It assembles durable agent sessions, execution modes, discovery,
settings, inference services, environment-host bridges, and orchestration
without depending on CLI, TUI, or desktop presentation crates.

The crate sits above `omp-envd`, `omp-env`, and `omp-serve` and below
`omp-app`. Driver code composes the session; app code selects a command or
presentation adapter.

## Structure

- `chat` owns durable project-chat composition shared across frontends:
  journals and session projections, tool selection, agent state, and
  higher-level control bindings.
- `headless` constructs the production non-interactive session used by print,
  RPC, and ACP adapters. It owns the joined lifetime of the agent, inference
  registry, environment composition, and environment client.
- `modes`, `subagent`, `collab`, `plan`, and related modules own reusable
  agent-runtime orchestration rather than UI behavior.
- `discovery`, `skills`, `settings`, and `rulebook` resolve the headless
  runtime's configuration and authored inputs.
- `registry` assembles inference and service registries.
- `bridges` implements driver-owned capabilities injected into `omp-envd`
  through `RegistryBridges`, including inference-backed search, active
  content, goal control, and telemetry integration.

`omp-driver` may construct `omp_envd::ProjectEnvironment` and supply the
higher-layer bridges it needs, but the filesystem/process/document/tool host
and Python extension-host/worker implementation remain in `omp-envd`.
Environment requests use `omp-env` clients. Neither boundary is reimplemented
in the driver.

## Philosophy

There is one headless production composition that every presentation reuses.
CLI parsing, terminal interaction, display policy, and presentation-protocol
adaptation stay in `omp-app`; reusable session state and authority wiring stay
here. This keeps print, RPC, ACP, and TUI modes from growing separate agent
stacks and
prevents presentation code from acquiring environment-host internals.

## Development

Run `just setup-python` once before commands that link embedded Python. Use
`just check-pkg omp-driver` and `just test-pkg omp-driver`. For joined session
behavior, use `just e2e` or an exact narrower E2E recipe from `just --list`.
Local model engines are opt-in through `local-all` or the individual
`local-*` features.
