## Unreleased (2026-10-26, runtime feature registry, #1612)

- A runtime descriptor can list core capabilities in a top-level `requires` array. An id the core does not implement, or a gated key or compiled id used without its feature, is refused with `runtime_feature_unsupported`. The core keeps an append-only table of features; the table starts empty and each Muse core change adds its own row. See [Runtime features](guides/runtime-packages.md#runtime-features).
