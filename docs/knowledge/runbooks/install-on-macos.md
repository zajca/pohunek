---
type: Runbook
id: runbook/install-on-macos
title: Install, upgrade, and remove Pohunek on macOS
description: Install the daemon, GUI, and owner web backend on Apple Silicon from release archives, upgrade without losing sessions, roll back, uninstall, and handle Gatekeeper.
source_kind: manual
intents: [setup, update, debug, help]
since: 0.31.6
---

# Install, upgrade, and remove Pohunek on macOS

Native macOS archives target Apple Silicon and macOS 14 or newer
(`aarch64-apple-darwin`); Intel Macs and Rosetta shells are refused. macOS is not
yet a published platform: public support is declared only when the final native
acceptance gate passes. The release workflow builds the archives, and publishes
them only when the repository's signing and notarization credentials exist, so a
signed archive is the only macOS archive that is ever attached to a release.
Everything runs as the logged-in owner: no `sudo`, no root service.

## What a release provides

| Archive | Contents | Installed by |
|---------|----------|--------------|
| `pohunek-cli-<v>-aarch64-apple-darwin` | `pohunek`, completions, offline docs | copy the binary onto `PATH` |
| `pohunek-daemon-<v>-aarch64-apple-darwin` | `pohunek`, `pohunekd`, `pohunek-sessiond`, `packaging/install-daemon.sh` | `packaging/install-daemon.sh` |
| `pohunek-gui-<v>-aarch64-apple-darwin` | `Pohunek.app` | copy to `~/Applications` |
| `pohunek-web-<v>-aarch64-apple-darwin` | owner web backend, SPA, `install.sh` | `./install.sh` |

Every archive carries a `MANIFEST` (component, version, target, signing state,
SHA-256 of each member) next to a `.sha256` of the archive itself. The installers
verify the manifest, the host, and member permissions before they run or change
anything. A build made with `packaging/macos/package --development` is unsigned,
named `...-unsigned-development`, and not a release.

## Verify a download

```bash
shasum -a 256 -c pohunek-daemon-<v>-aarch64-apple-darwin.tar.gz.sha256
```

After extracting, `codesign --verify --strict <binary>` and
`codesign -dvv <binary>` show the Developer ID Application authority, the hardened
runtime flag, and the team identifier. For the app bundle,
`spctl --assess --type execute --verbose=4 Pohunek.app` and
`xcrun stapler validate Pohunek.app` confirm Gatekeeper acceptance and the
stapled notarization ticket.

## Install the daemon

Extract the daemon archive anywhere owner-controlled (not a group- or
world-writable directory) and run its installer:

```bash
./packaging/install-daemon.sh
```

The installer installs into `~/.local` (`POHUNEK_INSTALL_PREFIX` selects another
absolute prefix): `<prefix>/libexec/pohunek/<version>/` holds `pohunek`,
`pohunekd`, and `pohunek-sessiond`; `<prefix>/bin/pohunek` is the CLI. It writes
`~/.config/pohunek/service.toml` (mode `0600`), registers the launchd login agent
`~/Library/LaunchAgents/io.github.zajca.pohunek.<ns>.daemon.plist`, and waits
until the daemon answers on its socket. Put `<prefix>/bin` on `PATH`. Concurrent
installers are serialized; an interrupted install resumes or rolls back when the
installer is run again.

```bash
pohunek service status
pohunek doctor
```

`pohunek doctor` lists anything the host still lacks (login shell, Terminal,
notifications, keychain, Privacy & Security access to project folders).

## Start, status, stop

launchd starts the agent at every login and restarts it after a failure. It never
starts session workers at login: each worker is its own job that the daemon
registers for a live session.

```bash
pohunek service status --json
pohunek health --json
```

Restart only the daemon (live workers keep running):

```bash
launchctl kickstart -k gui/$(id -u)/io.github.zajca.pohunek.<ns>.daemon
```

Stop only the daemon agent with `launchctl bootout gui/$(id -u)/io.github.zajca.pohunek.<ns>.daemon`;
workers keep running, and `pohunek service status` shows them. Never boot out the
whole `gui/$(id -u)` domain: that ends every worker.

## GUI

Copy `Pohunek.app` to `~/Applications` and open it from Finder. It finds the
installed `pohunek` through `pohunek_bin` in `~/.config/pohunek/gui.toml`, the
login-shell `PATH`, or `~/.local/bin`; install the daemon first. See the
[GUI guide](../guides/gui.md).

## Owner web backend (optional)

From the web archive, with the daemon installed:

```bash
./install.sh
```

The first run creates `~/.config/pohunek/backend.env`. Set the NetBird bind
address and a port, run `./install.sh` again, and the installer registers the
agent `io.github.zajca.pohunek.<ns>.backend`. It is a separate client service:
installing, updating, or removing it never touches the daemon or any session.
Re-run it after each edit of `backend.env`. Remove it with `./install.sh --uninstall`,
which keeps `backend.env` and the logs. See the
[web control center guide](../guides/web-control-center.md).

## Upgrade

Extract the new daemon archive and run its installer again. It runs
`pohunek service upgrade`, which stages the new versioned directory, probes every
binary, and then restarts only the daemon agent. Live workers keep their process,
PTY, and child, and keep running from the version directory they started in; that
directory stays installed until no worker references it. Update the web backend and
GUI from their own archives. Components that talk to each other must cross a
protocol boundary together; see
[update after release](update-after-release.md).

Verify afterwards:

```bash
pohunek service status --json
pohunek session list --json
```

Each live session keeps its worker and child PID. If the upgrade is interrupted,
`pohunek service status --json` shows a `pending_transaction`; running the
installer again resumes or rolls it back. Do not delete a version directory or a
worker definition by hand.

## Roll back

Rolling back is an upgrade to the previous archive: extract it and run its
`packaging/install-daemon.sh`. Only do this when the older version understands the
state the newer one wrote; the protocol and persisted-state boundaries in
[update after release](update-after-release.md) are one-way, so after crossing one
restore the complete newer component set instead.

## Uninstall

```bash
pohunek service uninstall
```

refuses while sessions are live and names them; end them yourself, or accept the
destructive path explicitly with `pohunek service uninstall --stop-sessions`. The
default keeps the durable host identity, governance keys, session history, and
worktrees. `pohunek service uninstall --purge` additionally removes the session
store, event logs, worker journals, and host identity: it is never implicit. Delete
`Pohunek.app` yourself, and remove the web backend with `./install.sh --uninstall`.

## Logs

- Daemon and worker launchd output: `~/.local/state/pohunek/logs/launchd/`.
- Daemon structured logs: `~/.local/state/pohunek/logs/`.
- Web backend: `~/.local/state/pohunek/web-logs/pohunek-backend.jsonl` (rotating,
  owner-private), `launchd.stderr` in the same directory for a startup failure.
- Worker definitions: `~/.local/state/pohunek/launchd/` (owner-private).

## Logout, reboot, sleep

Locking the screen, closing a terminal, and quitting the GUI do not stop workers.
Logging out ends the launchd login domain and every worker with it; after the next
login the daemon agent starts again and marks each of those sessions `lost` with
reason `runtime_lost`. A reboot does the same. Sessions with a native resume
reference can be recovered explicitly with `pohunek session resume <id>`. A sleeping
Mac executes nothing; sessions continue when it wakes.

## Gatekeeper

Release archives are signed and notarized, so Gatekeeper accepts them. If macOS
still refuses a download (an incomplete notarization, a quarantined archive from a
browser, a tampered file), do not disable Gatekeeper and do not remove quarantine
from a whole directory. Re-download, run the verification commands above, and only
when they prove a valid Developer ID signature of the expected team, allow that
one app in System Settings > Privacy & Security ("Open Anyway"). Downloading with
`curl` creates no quarantine attribute at all.
