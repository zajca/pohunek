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
   writable state directories. The check list depends on the platform; see
   [Doctor checks by platform](#doctor-checks-by-platform).
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
They never share a prefix either: the first install claims it in
`<prefix>/libexec/pohunek/installation_owner`, and an install from another
namespace (different XDG state or runtime roots) into the same prefix fails
with `service_prefix_owned`; pick another `--prefix`, or uninstall the owning
installation first, which releases the prefix. An existing version directory
is reused only when it and its binaries are exactly what the installer creates
(owned by you, mode `0755`, no symbolic links, no extra hard links); otherwise
the install fails with `service_version_untrusted`.
Session workers are not installed: the daemon starts one systemd transient unit
or one launchd job per worker generation, and the daemon and workers are
siblings, so restarting the daemon never stops a worker. The release archive's
`packaging/install-daemon.sh` wraps this command. The install is ready only
when the daemon job's own main process answers `daemon.health` on the socket;
an install that fails from the registration step on keeps its record and fails
with `service_install_incomplete`, and rerunning `pohunek service install` with
the same version and prefix finishes it (`pohunek service uninstall` removes it
after checking for live sessions instead, even when its `service.toml` is
already gone). Upgrades use
`pohunek service upgrade`; `pohunek service status` shows the daemon job,
versions, and workers. `pohunek service check [--prefix DIR]` runs every check
the install or upgrade would make before changing anything, and changes
nothing; `pohunek service lock -- <command>` runs a command that no other
service transaction can interleave with (the archive installer uses both). For
upgrades, removal, and runtime diagnosis, see
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

sway and rofi are optional Linux capabilities. On macOS a bare `pohunek setup`
writes only the platform-neutral `launcher.conf` and prompt templates, reports
the launcher scripts and the sway drop-in as skipped (`skipped` array in
`--json`, `skipped <step>: <reason>` lines in human output), and prints macOS
next steps (`pohunek service install`, `pohunek doctor`). An explicit
`pohunek setup sway` on macOS exits successfully without writing anything and
returns `{"skipped": true, "step": "sway", "reason": ...}` with `--json`.
`pohunek setup scripts` still installs the scripts when asked. On Linux the
output is unchanged.

## Doctor checks by platform

`pohunek doctor` and the `daemon.doctor` RPC share one probe list (crate
`hostcheck`). Each check has a stable `name` (the code) and a `detail` that
carries the remediation. When `pohunek doctor` reaches the daemon, a check both report is merged into one entry: the worse status wins and the detail reads `local: ...; daemon: ...` unless identical, because a terminal-launched CLI and a launchd-launched daemon can differ in `PATH` and privacy grants. Overall status is `fail` only when a required check
fails; optional capabilities are at most `warn`.

Linux: `bin:git` (required), `bin:codex`, `bin:claude`, the socket, state and
log directory writability checks, `netbird_cli`, `schema_version`, and the
optional launcher probes `bin:rofi`, `bin:swaymsg`, `bin:python3`,
`bin:timeout`, `terminal` (`$TERMINAL`), `launcher_scripts` and `sway_include`.
Executables count only when they are regular files the effective user can execute according to the kernel (`faccessat` with `X_OK`); a file it cannot execute is skipped and the `PATH` search continues. The writability probes create a randomly named file exclusively (never following a planted symlink) and remove it.

macOS omits the Linux-only launcher probes (rofi, swaymsg, `timeout`,
`$TERMINAL`, launcher scripts, sway include) and `bin:python3` (hook
interpreter readiness is reported by `pohunek integration doctor`), and adds:

| Check | Failure status | Meaning and remediation |
| --- | --- | --- |
| `runtime_dir_private` | `fail` | The runtime root (default `/private/tmp/pohunek-<uid>`) fails the same owner-private validation the daemon applies at startup: it must be a real directory you own with mode exactly `0700`, no ACL beyond the mode, and no symlinked path component. Remove it or point `XDG_RUNTIME_DIR` at a valid directory. An absent directory is `ok`. A directory that fails this check is never written to: `socket_dir_writable` reports `fail` without probing it. |
| `socket_path_length` | `fail` | The daemon socket, or the longest worker socket including staged bind names, exceeds Darwin's 103-byte `sockaddr_un` limit. Set a shorter `XDG_RUNTIME_DIR`. |
| `filesystem_access` | `fail` | A required directory (config, data, and the CLI's current directory) is denied. `EPERM`, or any denial below Documents, Desktop, Downloads, iCloud Drive, `Library/CloudStorage` or `/Volumes`, is a Privacy & Security (TCC) denial: grant the app that started the process (your terminal app, or the `pohunekd`/`pohunek-sessiond` executables when launchd runs them) access under System Settings > Privacy & Security > Files and Folders, or keep projects outside protected folders. Full Disk Access is not required and not recommended by default. Other denials point at ownership and mode. |
| `working_directory` | `fail` | CLI doctor only: the current directory cannot be determined (removed or unreadable), so `filesystem_access` cannot probe it. Run doctor from an existing readable directory. |
| `worker_executable` | `fail` | `pohunek-sessiond` is missing, not absolute, or not executable. `daemon.doctor` reports the worker of the daemon's active supervision (`--service-config` or `--dev-subprocess`), and `pohunek doctor` prefers that result. Without a reachable daemon the CLI derives it like `pohunek daemon start`: installed `service.toml`, then `POHUNEK_WORKER_BIN`, then next to the located `pohunekd`. The detail names the source. |
| `launchd_domain` | `fail` when absent, `warn` when inconclusive | `launchctl print gui/<uid>` (fixed `/bin/launchctl`, argv only, 10 s deadline) reports whether the graphical domain exists. A bare SSH session without a console login has none. |
| `launchd_job` | `fail` only for a failed job, else `warn` | CLI doctor only: the installed daemon job's state from `pohunek service status`. Not installed is a `warn`; a manually started daemon is valid. |
| `bin:codex`, `bin:claude` | `warn` | Optional agents. A daemon started by launchd does not read shell startup files, so use an absolute agent profile `program` or fix the service PATH. |
| `login_shell` | `warn` | `$SHELL` must be an absolute executable listed in `/etc/shells`. |
| `terminal` | `warn` | The stock `/System/Applications/Utilities/Terminal.app`, plus the optional `terminal=` key in `launcher.conf`, read like the launcher does (last assignment wins, an empty value means unset). The launcher runs the whole value as one executable name, so a value with arguments such as `kitty -e` is reported as unresolvable; use a wrapper script. |
| `desktop_notifications` | `warn` only when `osascript` is missing | Reports `/usr/bin/osascript`; delivery and user denial cannot be confirmed for an unbundled binary. If banners do not appear, allow notifications for the sending app in System Settings > Notifications. |
| `keychain` | `warn` | Presence of `/usr/bin/security` and the login keychain file only. No secret is read and lock state is not probed; a locked keychain is reported when a provider credential is first requested. |

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
