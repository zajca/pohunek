## Unreleased (2026-11-08, OSC 9;4 progress parser, #2040)

- The detector reads `OSC 9 ; 4 ; <state> [; <percent>]` as typed progress (states 0 to 4, percent 0 to 100) and exposes it to manifests as the `osc_progress` region text `4;<state>`. A plain `OSC 9 ; <text>` notification and a malformed `9;4` sequence are no longer progress evidence, so approval notices no longer flip a session to blocked or working. The generic shell manifest maps `4;0` to idle and `4;3` to working; states 1, 2 and 4 map to no activity. See [public API](../public-api.md) (`state_source`).
