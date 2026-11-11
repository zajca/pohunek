## Unreleased (2026-11-11, version probe open range and deadline, #1963)

- `version_probe` may omit `below` (an open-ended range `[min, ..)`) when the descriptor requires `version-probe-open-range-v1`; a release below `min` is refused naming the minimum. A probe that runs to its 2 second deadline is `agent_runtime_unsupported` with its own message and `recover` hint (same code); the probe environment is the launch `PATH` plus the runtime's `[launch_env]`, nothing else. See [Runtime packages](guides/runtime-packages.md#version-probe-environment-and-deadline).
