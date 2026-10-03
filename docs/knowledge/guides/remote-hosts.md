---
type: Guide
id: guide/remote-hosts
title: Remote hosts
description: Use host discovery and host-qualified targets to inspect and operate Pohunek across configured overlay peers.
source_kind: manual
intents: [setup, project, debug, help]
---

# Remote Hosts

This guide documents the current protocol-v3 owner path over configured
overlays. The [optional team relay](../concepts/team-relay.md) has an
implemented authentication and credential foundation, including native `pohunek
relay` commands, but it provides no remote-host enrollment, routing, attach, or
team client. Those commands do not alter the owner path described here.

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
  peers only; browser and `--all-hosts` consumers add the explicit local target
  through its Unix socket.
- `pohunek host list --json` to list known live peers. These commands need the
  local overlay CLI/state, but do not connect to local `pohunekd`; a short
  owner-private cache avoids repeated probing, and `--refresh` bypasses it.
  Status loading and peer probes are bounded by a complete discovery deadline.
- `pohunek host inspect <host> --json` to inspect one host's daemon
  capabilities.
- `pohunek host governance inspect <host> --json` to read the selected daemon's
  safe stable host identity and local governance state. The route selector is
  not the returned `HostId`; preserve each for its separate purpose. This call
  is read-only and does not imply that the remote host is enrolled with a relay.

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
keep the overlay-qualified stable peer identity and discovered port for display,
caching, reconnects, and external attach. Each new connection re-resolves that
identity through current provider state; only control and raw attach opened by
one SDK client reuse its exact selected socket endpoint. The explicit
`<overlay>:<selector>@<port>` form carries the discovered port without trusting
a cached IP. Generated selectors encode typed identities as unpadded base64url:
`peer~<base64url>` for provider peer IDs and `fqdn~<base64url>` for the fallback.
Providers must resolve the requested identity field explicitly, so an FQDN can
never fall through to a colliding peer ID or short name. This keeps raw `/`, `+`,
`=`, and `@` characters out of target and exact-port grammar.
A socket-address literal cannot bypass current overlay membership. NetBird uses
`publicKey` or legacy `pubKey` as `peer_id`; when absent, `peer_id` stays null
and clients fall back to FQDN. A client that tunnels to remote peers should force a
new local-daemon discovery before each remote tunnel upgrade and refuse an
identity that no longer owns the cached address. A bare IPv6 literal such as
`fd00::2` remains an unqualified selector; only an explicit configured-overlay
prefix such as `netbird:fd00::2` qualifies it. A failure in one configured
overlay does not hide healthy peers from another overlay; discovery reports an
error only when every provider fails.

## macOS hosts

A Mac is a first-class host on the same direct overlay path; there is no
macOS-specific routing, bridging, or relay mode.

- **Finding the `netbird` CLI.** The daemon (a launchd job) and the CLI (a shell) each locate `netbird` in their own
  process: first the process `PATH`, then the trusted install directories
  (`/opt/homebrew/bin`, `/usr/local/bin` among them). An executable another
  account could replace is never used. `pohunek doctor` reports `netbird_cli`
  through the same lookup. Its warning has several causes: read the detail. A
  "not found" detail means no trusted `netbird` was located; install NetBird or
  fix the ownership and permissions of the directory holding it. A "local state
  is unavailable" or "no NetBird IP" detail means the CLI was found but is not
  logged in or its daemon is down. When the daemon answers, the entry reads
  `local: ...; daemon: ...` and the worse status wins: read each side, because
  a warning from one process does not prove the other cannot find the CLI.
- **The listener.** The daemon binds its overlay listener only to the address
  the overlay reports for this host and re-binds when that address changes. The
  Unix socket stays available while the VPN is down, starting, or
  reconnecting; local clients are unaffected.
- **Sleep and wake.** Nothing runs while the Mac is asleep; its sessions
  continue after wake. Remote clients see the connection drop, retry with a
  capped exponential backoff, and resynchronize from a fresh snapshot. A
  request that failed while the host was unreachable is never replayed on the
  new connection, so a session is not created twice and committed input is not
  sent again. After wake, compare `runtime_id` before and after: the same id
  is continuity.
- **Troubleshooting.** Run `netbird status` in the same shell. If the daemon
  logged `overlay CLI missing; listener disabled`, fix the CLI lookup above and
  restart the daemon; for a VPN that was not ready, wait one retry interval and
  look for `serving control protocol over overlay`.

The future public team relay does not replace this direct overlay model and does
not require NetBird. After host enrollment and transport work land, an enrolled
`pohunekd` will initiate its own userspace WireGuard link and all control and
attach streams to one relay. Independently
approved `HostShare` records can expose bounded project, profile, operation, and
resource capacity to multiple teams. Local and direct-overlay sessions remain
owner-only and never appear through the relay; only sessions created through a
share are eligible. The implemented foundation already provides generic OIDC
device flow and browser Authorization Code with PKCE for its credential
lifecycle, with no loopback fallback. It does not authorize host enrollment or
relay-created sessions. Until the host-link, routing, and team-client issues
linked from the [team-relay concept](../concepts/team-relay.md) land, use only
the owner commands documented above for remote hosts.
