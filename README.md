<p align="center">
  <img src="assets/hero.png" alt="omp">
</p>

<p align="center">
  <strong>A coding agent with the IDE wired in — rewritten in Rust.</strong><br>
  <strong><a href="https://omp.sh">omp.sh</a></strong>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/stencil-hq/omp?style=flat&colorA=222222&colorB=58A6FF" alt="License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-DEA584?style=flat&colorA=222222&logo=rust&logoColor=white" alt="Rust"></a>
</p>

Pre-release: the workspace is being built up subsystem by subsystem; expect
renames and breaking changes without notice.

## Workspace layout

All crates live under `crates/*` (virtual workspace, resolver 3). Package
names are `omp-` prefixed; directory names are not.

### Core primitives

| Crate      | What it is                                                                                                   |
| ---------- | ------------------------------------------------------------------------------------------------------------ |
| `core`     | Compact strings/bytes (`Str`, `CowBytes`), sparse collections, binary↔text encodings, shared data structures |
| `ar`       | Bounded lazy ZIP/TAR/TAR.GZ reading, deterministic archive writing                                           |
| `walker`   | Filesystem traversal, filtering, file-candidate discovery                                                    |
| `slopjson` | Tolerant JSON for malformed, partial, and streaming documents                                                |
| `hashline` | Disk-free hashline patch parsing/application over immutable byte snapshots                                   |
| `ast`      | Tree-sitter source analysis, structural search, AST-aware editing                                            |
| `grep`     | Synchronous regex and PCRE2 search over memory and workspace files                                           |

### Inference

| Crate           | What it is                                                                                                   |
| --------------- | ------------------------------------------------------------------------------------------------------------ |
| `catalog`   | Typed offline provider/route/model/capability catalog (embedded snapshot, no runtime heuristics)             |
| `inference` | Typed request/response contracts and `Client` over the Tower service stack (routing, auth, retries, budgets) |

### Services

| Crate       | What it is                                                                    |
| ----------- | ----------------------------------------------------------------------------- |
| `proto`     | Generated Protobuf messages and gRPC bindings for the wire protocols          |
| `rpc`       | gRPC transport, handshake, health, TLS, Unix-socket plumbing                  |
| `storage`   | Append-only session transcripts and content-addressed blob storage            |
| `docserver` | Local document authority: filesystem, revisions, transactions, watch, LSP ops |
| `telemetry` | OpenTelemetry instrumentation, metrics, export, redaction                     |
| `env`       | Typed client boundary for environment services                                |
| `envd`      | Project environment daemon: filesystem, process, document, tool, and extension-host authority |
| `serve`     | Tonic gRPC projections for inference, authentication, and content-addressed blob services |
| `oauth`     | Provider-independent bounded OAuth discovery, PKCE authorization, callback, registration, and token primitives |
| `collab`    | Versioned, bounded collaboration substrate: room cryptography, Protobuf framing, replication, relay transport |
| `memory`    | Durable default-off Mnemopi memory banks, recall, retention, and isolated embeddings |

### Agent

| Crate            | What it is                                                                           |
| ---------------- | ------------------------------------------------------------------------------------ |
| `tool` / `tools` | Typed revisioned tool contracts/registry, and the resource-owning built-in executors |
| `agent`          | Durable, interruptible agent-loop foundations                                        |
| `driver`         | Headless session composition, execution modes, orchestration, discovery, and settings                       |
| `app`            | Production CLI application and daemon                                                |
| `e2e`            | Executable cross-crate acceptance proofs                                             |
| `ext`            | Extension configuration, dependency resolution, lockfiles, index metadata, and local trust state |
| `sdk`            | Stable native embedding facade for OMP sessions, callbacks, discovery, and tools |
| `snapcompact`    | Pure-Rust bitmap archive rendering and provider-aware framing for context compaction |

### Shell

| Crate            | What it is                                               |
| ---------------- | -------------------------------------------------------- |
| `shell-engine`   | Standalone Bash parser and execution engine              |
| `shell-builtins` | In-process coreutils and process builtins (no fork/exec) |
| `shell`          | Facade combining engine and builtins                     |

### Interface

| Crate          | What it is                                                                    |
| -------------- | ----------------------------------------------------------------------------- |
| `tui`          | Retained-mode terminal UI: components, rendering, input, terminal integration |
| `chat-ui`      | Host-agnostic immediate-mode chat scene and overlays shared by omp frontends |
| `macros`       | Procedural macros for declarative TUI markup and per-thread function caching |
| `gui`          | GPU-accelerated native window host for omp-tui apps                           |
| `desktop`      | Actor-owned native desktop capture, input, and accessibility automation |
| `webview`      | Pluggable embedded-browser surfaces using system webviews or installed Chromium/Firefox |
| `py`           | Embedded free-threaded CPython runtime with frozen stdlib                     |
| `voice`        | Cross-platform audio capture, playback, metering, and ownership coordination for OMP voice features |
| `voice-kokoro` | Kokoro-82M text-to-speech on candle with Metal acceleration                   |

### Infrastructure

| Crate      | What it is |
| ---------- | ---------- |
| `settings` | Typed reflected settings schemas and immutable revisioned snapshots |
| `secrets`  | Secret-rule validation, reversible keyed placeholders, and provider-bound text redaction |
| `sandbox`  | Deferred isolation boundary for OMP process confinement |
| `http`     | Process-wide outbound HTTP connection pools and TLS policy |

### Top level

| Path                  | What it is                                            |
| --------------------- | ----------------------------------------------------- |
| `PLAN.md`             | Authoritative plan: decisions, ledger, eight parts    |
| `.plan/feature-map/`  | Feature map and milestone roadmap                    |
| `.plan/quirks/`       | Catalog and inference notes                          |
| `fixtures/llm-oracle` | Recorded inference fixtures                           |
| `npm/pi-coding-agent` | npm package shim (`scripts/gen-npm-packages.py`)      |
| `vendor/python`       | Gitignored embedded-Python build inputs (see below)   |

## Building

Pinned nightly toolchain via `rust-toolchain.toml`; edition 2024, hard-tab
formatting (`cargo fmt`), workspace lint policy in the root `Cargo.toml`.

```sh
cargo build            # or: cargo check
just test              # nextest + doctests, workspace minus e2e
```

Tests run under [cargo-nextest](https://nexte.st) (`just test`, `just test-pkg
<crate>`, `just e2e`). nextest gives each test its own process and a real
parallel scheduler, but it **does not run doctests** — every recipe therefore
pairs `cargo nextest run` with a `cargo test --doc` pass. Invoke `cargo test`
directly only for doctests; otherwise use the recipes so both halves run.
Profiles beyond the defaults:

| Profile | Use |
| --- | --- |
| `dev` | Default. Line tables for workspace crates, no debuginfo for deps. |
| `release` | Shipping build: `opt-level = 2`, thin LTO, 1 codegen unit, stripped. |
| `release-dev` | Same codegen as `release` across 16 units, so a one-crate edit does not re-optimize everything. |
| `release-profiling` | `release` with symbols kept, for `perf`/`samply`/Instruments. |

```sh
cargo build --profile release-dev
```

`.cargo/config.toml` also sets `embed-metadata = false`, which keeps crate
metadata in `.rmeta` rather than duplicating it into every rlib — measured
196 MB → 130 MB of `target/` on a reqwest-sized graph at identical build
times. It needs the pinned nightly, and its accepted spelling is coupled to
the toolchain version.

The embedded-Python crate (`crates/py`) needs a one-time fetch before it
builds:

```sh
crates/py/scripts/fetch-python.sh
```

## Conventions

Dependency, allocation, async, and TUI-rendering rules are mandatory and
live in [`AGENTS.md`](AGENTS.md). Read it before touching anything.

## License

`omp` is released under the [MIT License](LICENSE).

Third-party material is excluded from these blanket license grants and remains
subject to its own license terms. See [third-party notices](THIRD-PARTY-NOTICES.txt)
for attribution and applicable terms.
