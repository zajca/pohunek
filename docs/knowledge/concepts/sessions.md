---
type: Concept
id: concept/sessions
title: Sessions
description: Pohunek sessions are daemon-owned PTY processes controlled by the CLI and addressed locally or through a host-qualified target.
source_kind: manual
intents: [debug, help, project]
---

# Sessions

A session is a daemon-owned PTY process running an agent in a working directory.
The CLI controls sessions through the daemon: start with `pohunek session new`,
inspect with `pohunek session inspect`, list with `pohunek session list`, send
input with `pohunek session input`, stop with `pohunek session stop`, and attach
with `pohunek attach`.

Session targets are host-aware. A bare session id targets the local host; a
`<host>/<session-id>` target names a specific host. Remote session creation keeps
the existing confirmation behavior: non-local starts require explicit approval,
and JSON/non-interactive remote starts require `--yes`.

The assistant feature reuses this session lifecycle. Its opening prompt is just
initial input to a normal session, so session warnings and applied-input status
remain the source of truth for whether the agent received that prompt.

For project-aware work, prefer a registered project or repository target over an
ad hoc directory. See [projects](projects.md) and [worktrees](worktrees.md).
