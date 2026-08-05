---
type: Concept
id: concept/architecture
title: Universal assistant architecture
description: The assistant is one ordinary agent session guided by a materialized knowledge bundle, a redacted snapshot, and a small navigational prompt.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Universal Assistant Architecture

The Universal Pohunek Assistant is designed as one capable coding-agent session,
not as a separate runtime or a set of specialized agents. The launch command
materializes a version-matched knowledge bundle, writes a redacted live snapshot,
builds a short navigational prompt, and starts a normal PTY-backed session with
initial input. Like every managed live session, that PTY belongs to its isolated
`pohunek-sessiond` worker; restarting the public daemon reconnects to the same
assistant runtime rather than relaunching it.

Knowledge delivery is pull-by-file. The prompt points the agent at this bundle,
the snapshot file, and the [source map](../assistant/source-map.md); it does not
inline the whole corpus. That keeps prompt size bounded and lets the same
Markdown serve humans and agents.

The planned assistant command surface uses one implementation with intent
filters: setup, project, update, debug, and help. Intent changes the initial
table of contents and first-step steering, but the concepts and safety model are
shared.

Local launches can bootstrap or verify the daemon before starting a session.
Remote launches preserve the existing remote session safety model and require a
knowledge bundle materialized on the host that runs the agent.

All clients use the same public protocol v2. Each request advertises an
inclusive `minimum`/`maximum` version range; the first response selects the
highest overlap for that connection. The old integer-v1 request envelope is not
accepted. Waiting observation calls open dedicated connections so they do not
block the caller's ordinary control connection.

Related concepts:

- [Sessions](sessions.md)
- [Projects](projects.md)
- [Worktrees](worktrees.md)
- [Agent profiles](agent-profiles.md)
- [Trust model](../safety/trust-model.md)
