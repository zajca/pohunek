---
type: Concept
id: concept/team-relay
title: Optional team relay
description: The team relay lives in its own repository; this page states what Pohunek core keeps for it and how a host behaves with no relay configured.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Optional Team Relay

The optional team relay is developed in its own private repository,
`pohunekd/pohunek-relay`. It is not part of a Pohunek release: core ships no
relay server (`pohunek-relayd`), no relay client library, and no relay CLI
command group: invoking `relay` as a `pohunek` subcommand is an
unknown-command usage error.

## What a host needs from the relay

Nothing. Current Pohunek releases use public protocol v4 through an owner-only
Unix socket or a direct configured overlay such as NetBird or Tailscale. A host
with no relay configured behaves identically: `pohunek host list`,
`pohunek session list`, and `pohunek doctor` do not depend on a relay. A
browser transport that bridges WebSockets to a daemon is an external client
concern, not the team relay.

## What core keeps

- Stable host identity, one exact principal-or-team owner, checked revisions,
  and host-local governance (approval keys, quarantine, signed transfers). See
  [Host governance](host-governance.md).
- The `RelayId`, relay principal, and team identifier types in the protocol
  crate, because host-local governance records name them.
- The `LaunchSource::RelayProfile` seam in the daemon: a relay-selected launch
  resolves only a profile the owner approved locally, and a profile that
  declares `secret_env` slots is refused with `secret_env_missing`.

## Where the rest lives

The relay RFCs, the relay issues, and the relay code with its history are in
the relay repository. Closed relay issues remain in the core repository and are
listed in the relay repository README.
