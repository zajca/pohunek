## Unreleased (2026-10-29, tolerant runtime state, #1336)

- A client of an older build reads a runtime state that a newer daemon adds as `unknown`. The session may own a live runtime: `may_own_runtime` is true, `can_stop` and `can_remove` are false, `service uninstall` counts it as live (with `--stop-sessions` it waits until the stop timeout), and `session stop` and `session rm` refuse it locally. The daemon never holds `unknown`; its store decode, params and daemon-side `match` arms stay conservative.
