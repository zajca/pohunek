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

All shipped clients use the same public protocol v3. Each request advertises an
inclusive `minimum`/`maximum` version range; the first response selects the
highest overlap for that connection. The old integer-v1 request envelope is not
accepted. Waiting observation calls open dedicated connections so they do not
block the caller's ordinary control connection.

## Owner paths and the accepted relay direction

Current protocol-v3 operation is owner-only. Local clients connect to the Unix
socket, direct remote clients use a configured overlay such as NetBird, and the
shipped Bun browser backend transparently maps one WebSocket to one daemon
connection. Each host daemon remains authoritative for its sessions and each
worker remains authoritative for one live PTY. This owner WebUI remains a
supported local/direct-overlay path after the team relay ships.

Pohunek also has an [accepted optional team-relay design](team-relay.md), but it
is not implemented. The future relay is additive: standalone and direct
NetBird modes remain independent. Hosts initiate every userspace WireGuard,
control, and attach connection to a public Rust relay. The relay owns teams,
principals, roles, session ACLs, routing, and aggregation; the daemon recognizes
only the enrolled relay and enforces locally approved `HostShare` and immutable
session-origin limits. Local and direct-overlay sessions never enter relay
state. The relay has no local mode; its team WebUI and the retained owner WebUI
use separate explicit API adapters, credentials, state, and origins.

Protocol v4 and the typed relay API will arrive only through their linked
implementation issues. Assistants must not infer future commands or fields from
the RFC; verify currently available behavior in `docs/public-api.md` through
the [source map](../assistant/source-map.md).

Related concepts:

- [Sessions](sessions.md)
- [Projects](projects.md)
- [Worktrees](worktrees.md)
- [Agent profiles](agent-profiles.md)
- [Optional team relay](team-relay.md)
- [Trust model](../safety/trust-model.md)
