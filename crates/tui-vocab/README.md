# `omp-tui-vocab`

Canonical callback macros that own the shared markup vocabulary for the TUI layer.

This crate intentionally contains only two macros:

- `for_each_prop!` emits every well-known property row used by `omp_tui::Prop`.
- `for_each_component!` emits every typed component tag row used by `dom!`.

Both macros are shared by `omp-tui` and `omp-macros` so property/tag vocabularies
never drift between enum/type definitions and parser lowering.
