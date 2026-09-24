---
type: Guide
id: guide/setup
title: Local setup
description: Configure a local Pohunek host enough to run daemon-backed sessions and launcher integration.
source_kind: manual
intents: [setup, help]
---

# Local Setup

Start with structured inspection:

1. Run `pohunek doctor --json` to check local binaries, socket paths, and
   writable state directories.
2. Run `pohunek service status --json` to see whether the daemon is installed
   as a login service. If `installed` is false, install it with
   `pohunek service install` (see below). For development only,
   `pohunek daemon start --dev-subprocess --detach` runs a daemon with plain
   subprocess workers and no service configuration.
3. Run `pohunek health --json` or `pohunek status --json` to confirm the daemon
   responds.
4. Run `pohunek host inspect local --json` to inspect local capabilities and
   available runtimes.
5. Run `pohunek host governance inspect local --json` to inspect the stable
   host ID and safe local governance state. A fresh host is valid with explicit
   null enrollment, owner, owner revision, and quarantine fields; it still has
   a public approval-key reference.

Pohunek resolves one owner-private application runtime root for the daemon and
worker sockets. A valid absolute `XDG_RUNTIME_DIR` selects its `pohunek` child on
Linux and macOS. Linux requires that variable. On macOS, when it is absent, the
root is `/private/tmp/pohunek-<effective-uid>`; `TMPDIR` is not consulted. An
explicit empty or relative value is a configuration error, not a request for the
macOS default. Do not repair a rejected root with broad `chmod` or recursive
deletion: Pohunek fails closed on symlinks, foreign ownership, wrong types, and
incorrect private modes.

Only live runtime sockets and locks use that root. Config, data, state, logs,
worker journals, the stable HostId, approval key, and governance records retain
their XDG or home-relative durable locations. Runtime cleanup must never remove
those durable records.

`pohunek service install [--from DIR] [--prefix DIR]` installs the daemon as a
native login service. It copies `pohunek`, `pohunekd`, and `pohunek-sessiond`
into `<prefix>/libexec/pohunek/<version>/` (prefix default `$HOME/.local`),
installs `<prefix>/bin/pohunek`, writes `<config>/pohunek/service.toml`, and
registers one daemon job: the systemd user unit
`pohunek-<ns>-daemon.service` plus the slice `pohunek-<ns>-sessions.slice` on
Linux, or the launchd agent
`~/Library/LaunchAgents/io.github.zajca.pohunek.<ns>.daemon.plist` on macOS.
`<ns>` is a 12-hex-digit namespace derived from the user ID and the state and
runtime roots, so separate installations never touch each other's jobs.
Session workers are not installed: the daemon starts one systemd transient unit
or one launchd job per worker generation, and the daemon and workers are
siblings, so restarting the daemon never stops a worker. The release archive's
`packaging/install-daemon.sh` wraps this command. Upgrades use
`pohunek service upgrade`; `pohunek service status` shows the daemon job,
versions, and workers. For upgrades, removal, and runtime diagnosis, see
[update after release](../runbooks/update-after-release.md) and
[durable session workers](../runbooks/debug-session-runtime.md).

The installer refuses a unit or `LaunchAgents` directory that is group- or
world-writable and names the fix (`chmod go-w <path>`); it never changes
permissions itself.

Setup assets are installed through `pohunek setup`. The subcommands split the
work into launcher scripts, config templates, sway integration, and shell
completion:

- `pohunek setup scripts`
- `pohunek setup config`
- `pohunek setup sway`
- `pohunek setup completions <bash|zsh|fish>`

Completion installation is idempotent and does not edit shell startup files.
The default script is static and performs no runtime lookup. Add `--dynamic`
only when host and session-target candidates are wanted; those queries are
deadline-bounded, do not start a daemon, and fail silently. Zsh installation
prints the `fpath` line that must appear before `compinit`.

Do not overwrite existing user config unless the user asks for that behavior and
the command supports it. For launcher details, see [launcher](launcher.md). For
profile and secret boundaries, see [agent profiles](../concepts/agent-profiles.md)
and [secrets](../safety/secrets.md). For the governance storage and lifecycle
boundary, see [host identity and local governance](../concepts/host-governance.md).

For the separately managed Hermes operator integration, use its typed install,
status, doctor, update, and uninstall commands. Do not edit Hermes YAML, a
database, credentials, or a real profile by hand; the install uses an isolated
profile or custom absolute home. See [Hermes operator](hermes-operator.md).
