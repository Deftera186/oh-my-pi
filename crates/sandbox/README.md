# omp-sandbox

`omp-sandbox` compiles backend-independent capability specifications into inspectable confinement plans. Supported backends are Seatbelt, Bubblewrap, gVisor, Docker, Docker with runsc, and Windows AppContainer.

Compilation is pure and secret-free. Preparation resolves filtered environments, private files, filesystem views, and cleanup ownership. Callers may retain `PreparedSandbox` beside a normal child command or use `Runner::run`; `Runner::native_command` selects only Seatbelt or Bubblewrap for inherited descriptors and caller `pre_exec` hooks.

Strict specifications fail when the selected backend cannot enforce every requested capability. Caveated plans expose limitations structurally, and `Plan::enforced` is authoritative.
