---
type: Guide
id: guide/tailscale
title: Tailscale overlay
description: Run Pohunek hosts over a tailnet instead of NetBird, with the Tailscale client on Linux or macOS, the selectors that name a peer, and the error codes that explain a failure.
source_kind: manual
intents: [setup, debug, help]
---

# Tailscale Overlay

NetBird is the default overlay. Tailscale is a supported opt-in overlay with the
same direct owner path: `pohunekd` listens on the node's tailnet address, the
CLI and the SDKs dial it, and no relay or SSH bridge is involved. This guide
covers what differs from NetBird. [Remote hosts](remote-hosts.md) covers
everything the two overlays share (discovery, `--host` routes, diagnostics).

## Turn it on

A host works over exactly one overlay. Write the owner-private
`~/.config/pohunek/overlays.toml` (`$XDG_CONFIG_HOME/pohunek/overlays.toml`),
mode `0600`, in a directory only you can write:

```toml
providers = ["tailscale"]
port = 18722              # optional, 1..=65535
```

The daemon reads the file at start. Restart it after a change: restart the
service unit, and live workers keep running. Set the file on every host the
CLI reaches and on the host that runs the CLI: a client and a daemon that
configure different overlays or ports do not find each other. A host that still
runs the NetBird default does not listen on the tailnet.

Pohunek never starts, stops or logs in Tailscale. It only runs
`tailscale status --json` and reads the node's own state and its peers.

## Install the Tailscale client

- **Linux.** Install the distribution package, start `tailscaled` and join the
  tailnet with `tailscale up`. Reading status needs no `--operator` setting, so
  the account that runs `pohunekd` needs no extra Tailscale permission. The
  `tailscale` binary must be on the `PATH` of the process that runs `pohunekd`;
  Linux has no fallback directories. A service-started daemon uses the `PATH`
  of its service manager: when the client lives in `/usr/local/bin`, make sure
  that directory is on it, and check the `tailscale_cli` entry of `pohunek
  doctor`, which reports what the daemon itself finds.
- **macOS.** Install the client with Homebrew (`brew install tailscale`). Pohunek
  finds `/opt/homebrew/bin/tailscale` after the process `PATH` (the trusted
  install directories include `/opt/homebrew/bin` and `/usr/local/bin`), and refuses a binary or directory another account can write. The
  Homebrew client needs a running `tailscaled` with a kernel interface: starting
  it as root (for example `sudo brew services start tailscale`) is your choice,
  and Pohunek does not do it. The App Store and standalone builds are not
  supported: their command-line tool lives inside an application bundle that the
  trusted lookup does not accept.
- **Headscale and other control servers.** Pohunek reads only the local client's
  status, so a self-hosted control server needs no Pohunek setting. Point the
  client at it (`tailscale up --login-server=<url>`) and keep the rest of this
  guide.

## What the host must look like

The daemon binds only the node's own tailnet IPv4 address (`100.64.0.0/10`,
never the ChromeOS block or the service addresses), and only while Tailscale is
`Running` on a kernel interface. In every other state the Unix socket keeps
working, nothing is bound on the overlay and the daemon retries.

- **Userspace networking is not supported.** `tailscaled
  --tun=userspace-networking` has no interface, and a dial to a `100.64.0.0/10`
  address would leave through the default route, where that range can belong to
  an unrelated network. Pohunek treats the node as having no address. Run
  `tailscaled` with a TUN device.
- **The node must be logged in and running.** `NeedsLogin`, `NeedsMachineAuth`,
  `Stopped`, `Starting` and any state a newer client adds all leave the
  listener without an address; the daemon binds as soon as the state changes.
- **The node needs an IPv4 address.** An IPv6-only node has no listener.

## Name a peer

All forms below work for `--host`, and for the SDK's host routes:

| Selector | Meaning |
|---|---|
| `build-2` | the short MagicDNS name (first label) |
| `build-2.tail1234.ts.net` | the full MagicDNS name, without the trailing dot |
| `tailscale:build-2` | the same, restricted to this overlay |
| `nABCDEF1CNTRL` | the stable node ID; it survives re-keying |
| `100.64.0.8` | a node address, accepted only when the current status lists it for exactly that peer |

A peer is selectable when it is in the network map, its key has not expired,
and Tailscale assigned it an address from its range. A tagged peer is
selectable. `HostName` is never used, because the OS hostname is not unique and
not authenticated. An ambiguous name is refused with the candidates listed.

`ShareeNode` marks a node that belongs to a user this tailnet shared a machine
to. Those nodes are not selectable. A machine another tailnet shared into this
one carries no such flag and is selectable like any other peer.

An offline peer is listed without an address and can still be named
explicitly, so the failure names the peer instead of "unknown host".

## Access control

The listener authenticates nothing beyond overlay reachability. The tailnet
policy is the access control: allow TCP on the configured port only between the
devices you own, and nothing else reaches the daemon.

```json
{
  "acls": [
    {
      "action": "accept",
      "src": ["autogroup:member"],
      "dst": ["autogroup:self:18722"]
    }
  ]
}
```

Replace `18722` with the `port` of `overlays.toml`. A tailnet with the default
allow-all policy already reaches the port from every device, including devices
of other members and shared machines: tighten it before you rely on it. Headscale
policy files use a different dialect; consult its documentation for the same
rule.

## One overlay per host

`providers` lists one id. A host that must be reachable over both NetBird and
Tailscale is not supported until [#1113](https://github.com/zajca/pohunek/issues/1113);
several entries are refused with `overlay_configuration_invalid`.

## Troubleshooting

Start with `pohunek doctor`: it reports `tailscale_cli`, `overlay_tailscale_listener`
and `overlay_config`, and names the reason a node has no usable address.

| Code or check | Cause | Fix |
|---|---|---|
| `tailscale_cli_missing` | no trusted `tailscale` on the search path | install the client; on Linux put it on the daemon's `PATH`; on macOS use Homebrew or fix the ownership of the directory |
| `tailscale_state_unavailable` | `tailscale status` failed, printed something unreadable, or the node is not running on a kernel interface | read the message: start `tailscaled`, log in, or stop using userspace networking |
| `tailscale_listener_address_missing` | the node has no IPv4 address Pohunek may bind | the same causes; the message names the reason |
| `host_unknown` | the peer is expired, not in the network map or a `ShareeNode` | check `tailscale status` on this host |
| `overlay_peer_collision` | two peers match the name | use the node ID or the full name |
| `host_unreachable` | the peer was resolved but its daemon port did not answer | start the daemon on the peer, check the peer's `overlays.toml` port and the tailnet policy |
| `overlay_configuration_invalid` | `overlays.toml` is unsafe or malformed | the message names the file and the line |
| `overlay_config` warning | the running daemon uses another overlay or port than the file | restart the daemon |

`pohunek host list --refresh --json` shows exactly what the CLI selected and
the address it would dial.
