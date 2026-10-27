## Unreleased (2026-10-27, per-launch input seam, #1648)

- An integration handler can contribute per-launch input (environment pairs, root-position arguments, private files) that core validates, places in the session's worker state directory as `launch-input.<name>`, and applies to every create, resume and fork without storing it in the resume snapshot. A refused contribution fails the launch with `runtime/launch_input_invalid` or `runtime/launch_input_unavailable`. No handler contributes yet. See [Per-launch input](guides/runtime-packages.md#per-launch-input).
