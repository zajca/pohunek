## Unreleased (2026-11-06, session placement, #1947)

- Runtime descriptors declare how a session stays out of a shared host daemon with `[placement]` (runtime feature `placement-v1`); see [Runtime packages](guides/runtime-packages.md#session-placement) and the `SessionInfo.placement` field in the public API. `session.inspect` of a live session and `daemon.doctor` (`placement_escape`) report a process of the generation that left the PTY's POSIX session as a daemon; the check never signals a process. The official Claude Code, Codex, Pi and Hermes packages declare `in_process` placement.
