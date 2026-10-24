---
type: Guide
id: guide/remote-hosts
title: Remote hosts
description: Use host discovery and host-qualified targets to inspect and operate Pohunek across configured overlay peers.
source_kind: manual
intents: [setup, project, debug, help]
---

# Remote Hosts

This guide documents the current protocol-v4 owner path over configured
overlays. The [optional team relay](../concepts/team-relay.md) has an
implemented authentication and credential foundation, including native `pohunek
relay` commands, but it provides no remote-host enrollment, routing, attach, or
team client. Those commands do not alter the owner path described here.

## Overlay configuration

A host works over exactly one overlay provider. Without configuration that is
NetBird on TCP port 18722. The optional owner-private file
`~/.config/pohunek/overlays.toml` (`$XDG_CONFIG_HOME/pohunek/overlays.toml`)
sets the provider and the port for the CLI, the daemon and Rust clients alike:

```toml
providers = ["netbird"]   # or "tailscale"
port = 18722
```

- `providers` lists exactly one id, `netbird` or `tailscale`. Several
  entries are refused until one host can run several overlays (#1113).
- `port` is optional and must be in `1..=65535`; it is the port the daemon
  listens on and the CLI dials, on every host that reads the file.
- When nothing exists at `overlays.toml`, the defaults apply and the directory
  is not inspected (a symlinked or group-writable `~/.config` is fine): there
  is no configuration to trust, and a writer in an unsafe directory can only
  make a file appear, which is then refused, or delete it, which falls back to
  the NetBird default. When anything exists at that path (file, symlink, FIFO),
  every component of the directory path (`~/.config` included) must be a real
  directory, not a symlink, owned by you or root and not writable by other
  accounts. The file must be a regular file owned by you with mode
  `0600`, UTF-8, at most 64 KiB, with no unknown key.
- Any violation is `overlay_configuration_invalid`; the message names the file
  and a line number, never a value from it. `pohunekd` exits with the same code
  before it takes a lock or touches its store.
- The daemon reads the file at start, so restart it after a change (restart
  the service unit with the service manager; live workers keep running).
- `POHUNEK_REMOTE_PORT` overrides `port` in the process that sees it, for
  development and tests. A service-started daemon does not receive it; put a
  port both sides must use in the file.
- `pohunek doctor` checks the configured overlay and compares the file with the
  running daemon; see [Diagnostics](#diagnostics).
- A daemon of an older release ignores the file and binds the environment value
  or 18722, and so does a client of an earlier release (a CLI on another host,
  a Rust client pinned to an earlier tag such as the `pohunek-work` gateway).
  Upgrade every client and restart every daemon before you set `port`, or those
  clients dial a port nothing listens on; a pinned client adopts the file when
  it bumps its pin.

### Diagnostics

- `pohunek doctor` runs one `<overlay>_cli` check for each configured overlay
  (`netbird_cli` with the default). A provider CLI that is missing or untrusted
  is a `fail` when `overlays.toml` lists the provider, with the fix in the
  detail (install it, or remove the provider from the file and restart the
  daemon), and a `warn` for the NetBird default of an absent file.
- The daemon adds `overlay_<overlay>_listener`: the port it was configured with
  and the listener address the provider reports now. It is the provider's state,
  not proof that the daemon has bound the address; the daemon log says
  `serving control protocol over overlay` once it has.
- `overlay_config` compares the file with the running daemon. A change to
  `overlays.toml` applies after a daemon restart, so a difference reads
  "restart the daemon to apply". A `POHUNEK_REMOTE_PORT` in your shell changes
  the port the CLI would use and is named in the warning.
- A refused or timed-out connection to a resolved peer reports
  `host_unreachable` with the overlay, the address and the port that were
  dialled, for example `overlay netbird at 100.64.0.8:18722`. When the overlay is
  known the hint adds that the remote host's `overlays.toml` must list it: a host
  whose daemon works over another overlay does not listen on this one.

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
  visible as candidates instead of being discarded. This machine is listed first
  as a record with `"local": true` for each overlay that reports its identity; a
  consumer that fans out to all hosts adds the explicit local target through its
  Unix socket and skips that record, so this machine is contacted once. A remote
  `--host` is resolved once per operation that opens several connections
  (`attach`, `assistant`, `session output`, `session wait`, and every
  `session <action> <name>` command that looks the session name up first) and
  pinned to the peer identity it selected, so a renamed short name is never followed midway; a
  peer with no peer id and no FQDN fails with `host_identity_unpinnable`.
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
notification create, update, and delete events as they arrive. Its `--session`,
`--kind`, and `--status` (or `--unread`) filters run client-side on every host's
stream.

A remote daemon outage does not imply that its sessions stopped. Per-session
workers continue on that host, but clients cannot attach until the replacement
daemon completes reconciliation and becomes ready. After reconnection, inspect
`runtime.state`, `worker_id`, and `worker_instance_id`; `live` with the same runtime id
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

The TypeScript SDK resolves a host route by asking the local daemon:
`host.resolve` applies the same selector rules as the CLI and returns this
machine (`local`, the empty route or its own overlay name) or one peer with an
identity-pinned route, so the SDK holds no copy of the rules. The method is
served on the local Unix socket only. `resolveHost`, `connectHost` and the remote
form of `attachRaw` use it (`docs/public-api.md`, TypeScript SDK).

## macOS hosts

A Mac is a first-class host on the same direct overlay path; there is no
macOS-specific routing, bridging, or relay mode.

- **Finding the `netbird` CLI.** The daemon (a launchd job) and the CLI (a shell) each locate `netbird` in their own
  process: first the process `PATH`, then the trusted install directories
  (`/opt/homebrew/bin`, `/usr/local/bin` among them). An executable another
  account could replace is never used. `pohunek doctor` reports `netbird_cli`
  through the same lookup. Its warning has several causes: read the detail. A
  "not installed or not trusted" detail means no trusted `netbird` was located;
  install NetBird or fix the ownership and permissions of the directory holding
  it. A "local netbird state is unavailable" or "no netbird address resolved"
  detail means the CLI was found but is not logged in, its daemon is down or it
  did not answer in time. When the daemon answers, the entry reads
  `local: ...; daemon: ...` and the worse status wins: read each side, because
  a warning from one process does not prove the other cannot find the CLI.
  On nix-darwin, `/run/current-system/sw/bin/netbird` is accepted when the
  `current-system` link and its macOS system directory have trusted ownership;
  Pohunek then runs the verified canonical target.
- **Tailscale.** With `providers = ["tailscale"]` the host works over a
  tailnet. Pohunek runs `tailscale status --json` (found like the `netbird` CLI
  above, so the Homebrew and package-installer directories work on macOS); it
  never enables Tailscale because its CLI is installed. The daemon listens on the
  node's IPv4 address and only while Tailscale is `Running` on a kernel
  interface: userspace networking (`tailscaled --tun=userspace-networking`) has
  no interface to bind. Discovery and host resolution report it as
  `tailscale_state_unavailable`; the listener has no address
  (`tailscale_listener_address_missing`), and `pohunek doctor` names userspace
  networking as the reason in `tailscale_cli` and `overlay_tailscale_listener`.
  See the [Tailscale guide](tailscale.md). Peers
  that are expired, not in the network map or flagged `ShareeNode` (nodes of
  users you shared a machine to) are not selectable; nodes other users share
  into your tailnet are; tagged peers are. Name a peer by its short MagicDNS name, its
  full name, `tailscale:<name>` or its node ID. The remote listener has no
  authentication beyond reachability, so tailnet ACLs decide who can reach it.
  Allow TCP on the configured port between your hosts.
- **The listener.** The daemon binds its overlay listener only to the address
  the overlay reports for this host and re-binds when that address changes. The
  Unix socket stays available while the VPN is down, starting, or
  reconnecting; local clients are unaffected.
- **Sleep and wake.** Nothing runs while the Mac is asleep; its sessions
  continue after wake. Remote clients see the connection drop, retry with a
  capped exponential backoff, and resynchronize from a fresh snapshot. A
  request that failed while the host was unreachable is never replayed on the
  new connection, so a session is not created twice and committed input is not
  sent again. After wake, compare `worker_instance_id` before and after: the same id
  is continuity.
- **Troubleshooting.** Run `netbird status` (or `tailscale status`) in the same
  shell. Without an `overlays.toml` the NetBird default is not retried: if the
  daemon logged `overlay CLI missing; listener disabled`, fix the CLI lookup
  above and restart the daemon. For an overlay listed in `overlays.toml` the
  daemon logs `overlay CLI missing; serving local-only and retrying` and binds as
  soon as the CLI is found. For a VPN that was not ready, wait one retry
  interval and look for `serving control protocol over overlay`.

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
