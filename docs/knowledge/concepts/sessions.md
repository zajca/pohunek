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

A session can carry an optional owner-set display name. Set it at creation with
`pohunek session new --name <NAME>`, and change or clear it later with
`pohunek session rename <target> <NAME>` (or `--clear`). The name is cosmetic:
it shows in `pohunek session list`, `session inspect`, and the GUI, but never
affects targeting or resume — a session is still addressed by its id. The daemon
trims the name and rejects a control character or an over-long one. The name is
captured in the resume binding, so it survives a daemon restart.

The assistant feature reuses this session lifecycle. Its opening prompt is just
initial input to a normal session, so session warnings and applied-input status
remain the source of truth for whether the agent received that prompt.

For project-aware work, prefer a registered project or repository target over an
ad hoc directory. See [projects](projects.md) and [worktrees](worktrees.md).
