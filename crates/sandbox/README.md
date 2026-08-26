# omp-sandbox

`omp-sandbox` provides the fail-closed process-confinement boundary used by sandboxed extension and environment-host launches. Policies contain canonical read/write filesystem grants and an explicit network policy.

`SandboxPolicy::prepare` returns a native `SandboxLaunch` only after proving that the platform backend can install confinement. macOS uses Seatbelt and Linux uses bubblewrap. Missing, unsupported, or unusable backends return an error; callers must never retry a sandboxed launch without the returned launcher and argument prefix. Trusted execution remains a separate, explicit caller policy.
