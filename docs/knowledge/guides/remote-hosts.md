---
type: Guide
id: guide/remote-hosts
title: Remote hosts
description: Use host discovery and host-qualified targets to inspect and operate Pohunek across configured overlay peers.
source_kind: manual
intents: [setup, project, debug, help]
---

# Remote Hosts

Remote behavior is host-aware. The CLI uses `--host <host>` for commands that
target a host, and session targets can use `<host>/<session-id>`.

Opt-in dynamic shell completion (`pohunek setup completions <shell> --dynamic`)
uses the same model. Host candidates come from the owner-private discovery
cache. Every reachable remote has a provider-qualified
`<overlay>:<address>` candidate; a short name is offered only when exactly one
reachable route owns it. Completion never resolves a collision by choosing the
first cache record. For a session target, an explicit `host/id` prefix wins,
then `--host`, then `local`; completion performs a bounded live `session.list`
query and emits no diagnostics when discovery or a daemon is unavailable.
Static completion is the default and performs no discovery or daemon I/O.

Use these commands for orientation:

- `pohunek host discover --json` to enumerate configured overlay peers and probe
  daemons. Each record carries its overlay, optional provider peer identity,
  address, and that overlay's effective daemon port. Address-less peers remain
  visible as candidates instead of being discarded. Discovery emits remote
  peers only; GUI, web, and `--all-hosts` consumers add the explicit local target
  through its Unix socket.
- `pohunek host list --json` to list known live peers. These commands need the
  local overlay CLI/state, but do not connect to local `pohunekd`; a short
  owner-private cache avoids repeated probing, and `--refresh` bypasses it.
  Status loading and peer probes are bounded by a complete discovery deadline.
- `pohunek host inspect <host> --json` to inspect one host's daemon
  capabilities.

Remote session creation should use a registered project or an explicit
repository path valid on the remote host. Non-local starts preserve the existing
confirmation model; non-interactive remote starts require `--yes`.

Durable notifications keep the same host-authoritative model. Each daemon owns
only its local notification store, and cross-host notification views are
client-side fan-out. `pohunek notifications list --all-hosts` queries the local
daemon plus reachable daemon peers discovered directly from local overlay state, then
renders per-host successes and structured per-host errors. The matching watch
command with `--all-hosts` opens one subscription per reachable host and streams
notification create, update, and delete events as they arrive.

A remote daemon outage does not imply that its sessions stopped. Per-session
workers continue on that host, but clients cannot attach until the replacement
daemon completes reconciliation and becomes ready. After reconnection, inspect
`runtime.state`, `worker_id`, and `runtime_id`; `live` with the same runtime id
is continuity, while `lost` means the remote PTY generation is gone.

Policy and retention commands can also fan out with `--all-hosts`:
`pohunek notifications policy get --all-hosts`, policy set with `--all-hosts`,
and retention prune with `--all-hosts`. Single-record actions use the target
host: `host/id` overrides `--host`, while a bare id targets the selected
`--host` or local daemon.

The assistant design keeps the same boundary. A remote assistant must use a
knowledge bundle materialized on the remote host, version-matched to the remote
binary, and readable by the selected remote agent profile.

The Hermes operator uses the same direct overlay path but never performs host
discovery on a model's behalf. Its policy allows only explicitly listed hosts;
a wildcard requires explicit install-time confirmation. See
[Hermes operator](hermes-operator.md#access-policy-and-targets).

Unqualified names that resolve in more than one overlay fail closed. Clients
keep the overlay-qualified peer identity for display and caching, and reuse the
exact discovered socket route for control calls and the separate raw attach
connection through the explicit trusted-route API. A socket-address literal
cannot bypass current overlay membership and configured-port policy. A bare
IPv6 literal such as `fd00::2` remains an unqualified selector; only an explicit
configured-overlay prefix such as `netbird:fd00::2` qualifies it. A failure in
one configured overlay does not hide healthy peers from another overlay;
discovery reports an error only when every provider fails.
