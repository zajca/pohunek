---
type: Runbook
id: runbook/install-on-macos
title: Install, upgrade, and remove Pohunek on macOS
description: Install the daemon on Apple Silicon through Homebrew or a release archive, verify provenance, upgrade without losing sessions, roll back, uninstall, and handle Gatekeeper and Keychain prompts.
source_kind: manual
intents: [setup, update, debug, help]
since: 0.31.6
---

# Install, upgrade, and remove Pohunek on macOS

Native macOS archives target Apple Silicon and macOS 14 or newer
(`aarch64-apple-darwin`); Intel Macs and Rosetta shells are refused. macOS is not
yet a published platform: public support is declared only when the final native
acceptance gate passes. Release archives are ad-hoc signed, not notarized, and
carry no Developer ID; every release asset has a GitHub build-provenance
attestation. Everything runs as the logged-in owner: no `sudo`, no root service.

## What a release provides

| Archive | Contents | Installed by |
|---------|----------|--------------|
| `pohunek-cli-<v>-aarch64-apple-darwin` | `pohunek`, completions, offline docs | copy the binary onto `PATH` |
| `pohunek-daemon-<v>-aarch64-apple-darwin` | `pohunek`, `pohunekd`, `pohunek-sessiond`, `packaging/install-daemon.sh` | `packaging/install-daemon.sh` |

Every archive carries a `MANIFEST` (component, version, target, signing state
`signing adhoc`, SHA-256 of each member) next to a `.sha256` of the archive
itself. The installers verify the manifest, the host, and member permissions
before they run or change anything. A build made with `packaging/macos/package --development` is unsigned,
named `...-unsigned-development`, and not a release.

## Install with Homebrew

```bash
brew install zajca/pohunek/pohunek
pohunek service install
```

The tap is `zajca/homebrew-pohunek` and the formula is `pohunek`. It is a
formula, not a cask, so Homebrew sets no quarantine attribute. It places
`pohunek`, `pohunekd`, and `pohunek-sessiond` side by side and does no launchd
work; `pohunek service install` registers the login agent.

- After every `brew upgrade pohunek`, run `pohunek service upgrade`.
- Before `brew uninstall pohunek`, run `pohunek service uninstall`.

## Verify a download

```bash
shasum -a 256 -c pohunek-daemon-<v>-aarch64-apple-darwin.tar.gz.sha256
gh attestation verify pohunek-daemon-<v>-aarch64-apple-darwin.tar.gz --repo zajca/pohunek
```

After extracting, `codesign --verify --strict <binary>` succeeds and
`codesign -dvv <binary>` shows `Signature=adhoc` with identifier
`io.github.zajca.pohunek.<name>`. There is no Developer ID authority and no
team identifier.

## Install from the archive

Download with `curl -fLO` (curl sets no quarantine attribute), verify the
download as above, and extract it.

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

## Upgrade

With Homebrew, run `brew upgrade pohunek` and then `pohunek service upgrade`.
From an archive, extract the new daemon archive and run its installer again. The installer runs
`pohunek service upgrade`, which stages the new versioned directory, probes every
binary, and then restarts only the daemon agent. Live workers keep their process,
PTY, and child, and keep running from the version directory they started in; that
directory stays installed until no worker references it. Components that talk to each other must cross a
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
store, event logs, worker journals, and host identity: it is never implicit.

## Logs

- Daemon and worker launchd output: `~/.local/state/pohunek/logs/launchd/`.
- Daemon structured logs: `~/.local/state/pohunek/logs/`.
- Worker definitions: `~/.local/state/pohunek/launchd/` (owner-private).

## Logout, reboot, sleep

Locking the screen and closing a terminal do not stop workers.
Logging out ends the launchd login domain and every worker with it; after the next
login the daemon agent starts again and marks each of those sessions `lost` with
reason `runtime_lost`. A reboot does the same. Sessions with a native resume
reference can be recovered explicitly with `pohunek session resume <id>`. A sleeping
Mac executes nothing; sessions continue when it wakes.

## Troubleshooting

### Gatekeeper blocks a download

Archives are not notarized, so Gatekeeper blocks a file that carries the
`com.apple.quarantine` attribute, which browsers set. Homebrew and `curl -fLO`
downloads carry no quarantine attribute. Do not disable Gatekeeper and do not
remove quarantine from a whole directory. Verify the checksum and the
attestation first (see "Verify a download"), and only then remove the attribute
from that one file:

```bash
xattr -d com.apple.quarantine <file>
```

### Keychain asks for access again

An ad-hoc signature has no stable designated requirement, so after an upgrade
macOS may ask again whether `pohunek` may access its `pohunek-relay` Keychain
items. Choosing "Always Allow" applies to that build only; expect the prompt
again after the next upgrade.
