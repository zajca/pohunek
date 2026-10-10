---
type: Guide
id: guide/setup
title: Local setup
description: Configure a local Pohunek host enough to run daemon-backed sessions.
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
native login service. It copies `pohunek`, `pohunekd`, and `pohunek-sessiond`,
plus the build's `runtime-catalog-anchor.json` when it ships one (an invalid
anchor is refused with `service_staged_anchor_invalid`; re-installing an
existing version with a different or missing anchor fails with
`service_version_conflict`), into `<prefix>/libexec/pohunek/<version>/` (prefix
default `$HOME/.local`),
installs `<prefix>/bin/pohunek`, writes `<config>/pohunek/service.toml`, and
registers one daemon job: the systemd user unit
`pohunek-<ns>-daemon.service` plus the slice `pohunek-<ns>-sessions.slice` on
Linux, or the launchd agent
`~/Library/LaunchAgents/io.github.zajca.pohunek.<ns>.daemon.plist` on macOS.
The owner-private `service.toml` records `[input] initial_startup_grace_ms =
5000` and `submit_delay_ms = 150` on a fresh install. These host-wide
values control the maximum wait for an input-ready agent at `session new` and
the delay between the prompt body and its separate submit write. Neither names
a runtime: the submit delay applies exactly to the runtimes whose own runtime
descriptor marks that delay configurable (Claude opts in; its descriptor value
150 is replaced; the others keep their descriptor value). Both keys are whole
milliseconds in `1..=600000`; a missing key, a value outside the
range, or an unknown key makes the daemon refuse startup with the named
configuration error. The startup grace ends early when the agent signals an
editable prompt or enables bracketed paste; it only delays silent startups.
The daemon reads edits when it starts, so restart the service to apply them.
`<ns>` is a 12-hex-digit namespace derived from the user ID and the state and
runtime roots, so separate installations never touch each other's jobs.
They never share a prefix either: the first install claims it in
`<prefix>/libexec/pohunek/installation_owner`, and an install from another
namespace (different XDG state or runtime roots) into the same prefix fails
with `service_prefix_owned`; pick another `--prefix`, or uninstall the owning
installation first, which releases the prefix. An existing version directory
is reused only when it and its binaries are exactly what the installer creates
(owned by you, mode `0755`, no symbolic links, no extra hard links); otherwise
the install fails with `service_version_untrusted`. On macOS the install also
resolves the daemon's `PATH` (login-shell discovery plus a trusted fallback
directory list) and prints a `path` line, with a `warning:` line for a failed
login shell or a refused directory; see
[environment resolution](environment-resolution.md).
Session workers are not installed: the daemon starts one systemd transient unit
or one launchd job per worker generation, and the daemon and workers are
siblings, so restarting the daemon never stops a worker. The release archive's
`packaging/install-daemon.sh` wraps this command and first verifies the archive:
the `MANIFEST` must describe a daemon archive built for this host (Linux x86_64
or native macOS arm64, never an Intel Mac or a Rosetta shell), the host must meet
the archive's minimum macOS version, and every listed member must be present,
unmodified, and not writable by another account; otherwise it exits before
running any archive binary, with nothing changed. The install is ready only
when the daemon job's own main process answers `daemon.health` on the socket;
an install that fails from the registration step on keeps its record and fails
with `service_install_incomplete`, and rerunning `pohunek service install` with
the same version and prefix finishes it (`pohunek service uninstall` removes it
after checking for live sessions instead, even when its `service.toml` is
already gone). Upgrades use
`pohunek service upgrade`; `pohunek service status` shows the daemon job,
versions, and workers. When the installed `service.toml` is a previous
release's schema (schema 2, which predates the `[input]` table), the upgrade
is a configuration migration: the upgrade first saves the exact pre-upgrade
bytes owner-private next to its transaction record and journals their
digest, stops the daemon job before it rewrites `service.toml` (a previous
daemon's strict reader would refuse a restart against the new schema), then
registers the new daemon; the migration assigns the documented
`[input]` defaults explicitly and the `--json` report names it with
`config_schema_migration`. A crash resumes from the record after re-verifying
the backup and the file on disk against that digest while the transaction has
not rewritten it yet; a failing step
rolls the migration back by restoring the exact pre-upgrade bytes before
starting the previous daemon. A missing or altered backup refuses the resume
and the rollback closed with `service_config_backup_invalid` — nothing is
written, no daemon restarts, and the record stays for a repaired retry.
`pohunek service check [--prefix DIR]` runs every check
the install or upgrade would make before changing anything, and changes
nothing (an upgrade also runs a read-only live-session preflight that
`--accept-runtime-loss` can override, except for an unusable store; on a
schema-2 installation it names the migration it would perform); `pohunek service lock -- <command>` runs a command that no other
service transaction can interleave with (the archive installer uses both). For
upgrades, removal, and runtime diagnosis, see
[update after release](../runbooks/update-after-release.md) and
[durable session workers](../runbooks/debug-session-runtime.md).

The installer refuses a unit or `LaunchAgents` directory that is group- or
world-writable and names the fix (`chmod go-w <path>`); it never changes
permissions itself.

Local configuration is installed through `pohunek setup`. The subcommands are:

- `pohunek setup config` writes a default `attach.conf` (the `pohunek attach`
  reconnect settings, every key commented at its default) and the
  `prompts/issue.tmpl` and `prompts/pr.tmpl` templates the daemon resolves for
  project actions. Existing files are never overwritten unless `--force` is
  given.
- `pohunek setup completions <bash|zsh|fish>` installs shell completion.

A bare `pohunek setup` is `pohunek setup config` without `--force`; `--json`
returns the `created` and `skipped` file lists. Desktop launchers (rofi, sway)
and the issue/PR pickers are not part of core; they live in `zajca/pohunek-work`
and call the public `pohunek` CLI. Upgrading from a release that installed
launchers: remove `<data_dir>/bin/{lib.sh,pohunek-rofi,pohunek-rofi-issue,pohunek-launch-issue,pohunek-launch-pr}`
and `<config_home>/sway/config.d/pohunek.conf` as described in the
[update-after-release runbook](../runbooks/update-after-release.md), or let the
`pohunek-work` setup own them.

## Doctor checks by platform

`pohunek doctor` and the `daemon.doctor` RPC share one probe list (crate
`hostcheck`). Each check has a stable `name` (the code) and a `detail` that
carries the remediation. When `pohunek doctor` reaches the daemon, a check both report is merged into one entry: the worse status wins and the detail reads `local: ...; daemon: ...` unless identical, because a terminal-launched CLI and a launchd-launched daemon can differ in `PATH` and privacy grants. Overall status is `fail` only when a required check
fails; optional capabilities are at most `warn`.

Linux: `bin:git` (required), `bin:codex`, `bin:claude`, the socket, state and
log directory writability checks, `netbird_cli`, and `schema_version`.
Executables count only when they are regular files the effective user can execute according to the kernel (`faccessat` with `X_OK`); a file it cannot execute is skipped and the `PATH` search continues. The writability probes create a randomly named file exclusively (never following a planted symlink) and remove it.

macOS adds the following checks. Hook interpreter readiness (`python3`) is
reported by `pohunek integration doctor`, not by `pohunek doctor`:

| Check | Failure status | Meaning and remediation |
| --- | --- | --- |
| `runtime_dir_private` | `fail` | The runtime root (default `/private/tmp/pohunek-<uid>`) fails the same owner-private validation the daemon applies at startup: it must be a real directory you own with mode exactly `0700`, no ACL beyond the mode, and no symlinked path component. Remove it or point `XDG_RUNTIME_DIR` at a valid directory. An absent directory is `ok`. A directory that fails this check is never written to: `socket_dir_writable` reports `fail` without probing it. |
| `socket_dir_writable`, `state_dir_writable`, `log_dir_writable` | `fail` | On macOS the doctor never creates a directory (a default-umask `0755` directory would be refused at startup). An existing runtime or data directory must pass the daemon's owner-private validation and is then write-probed; the log directory needs a real directory owned by you below a valid state root (startup resets its mode to 0700, so a mode such as 0500 is `ok`); a missing directory is `ok` when startup could create it below a trusted, writable ancestor. |
| `worker_runtime_root`, `worker_state_root`, `launchd_definitions_dir`, `launchd_log_dir` | `fail` | `<runtime>/workers`, `<state>/workers`, `<state>/launchd` and `<logs>/launchd` are the directories startup and the launchd worker supervisor open owner-private (`0700`). Same rule as the runtime root: real directory, yours, mode `0700`, no symlinked component, or absent below a trusted writable ancestor. |
| `launchd_agents_dir` | `warn` | `~/Library/LaunchAgents`, opened by `pohunek service install`: an existing directory must be a real directory you own that group and others cannot write (`chmod go-w`); a missing one is created. |
| `socket_path_length` | `fail` | The daemon socket, or the longest worker socket including staged bind names, exceeds Darwin's 103-byte `sockaddr_un` limit. Set a shorter `XDG_RUNTIME_DIR`. |
| `filesystem_access` | `fail` | A required directory is denied. The probe runs in the privacy context of the process that runs it and says so (`readable by this process`): the CLI covers the config and data roots plus its current directory in the terminal's context, the daemon covers only its config and data roots in the launchd context. Neither proves that the other context can open your project directories. `EPERM`, or any denial below Documents, Desktop, Downloads, iCloud Drive, `Library/CloudStorage` or `/Volumes`, is a Privacy & Security (TCC) denial: grant the app that started the process (your terminal app, or the `pohunekd`/`pohunek-sessiond` executables when launchd runs them) access under System Settings > Privacy & Security > Files and Folders, or keep projects outside protected folders. Full Disk Access is not required and not recommended by default. Other denials point at ownership and mode. |
| `working_directory` | `fail` | CLI doctor only: the current directory cannot be determined (removed or unreadable), so `filesystem_access` cannot probe it. Run doctor from an existing readable directory. |
| `worker_executable` | `fail` | `pohunek-sessiond` is missing, not absolute, or not executable. `daemon.doctor` reports the worker of the daemon's active supervision (`--service-config` or `--dev-subprocess`), and `pohunek doctor` prefers that result. Without a reachable daemon the CLI derives it like `pohunek daemon start`: installed `service.toml`, then `POHUNEK_WORKER_BIN` (any present value is used, an empty one included, and rejected unless absolute), then next to the located `pohunekd` (made absolute). The detail names the source. |
| launchd checks (`launchd_domain`, `launchd_definitions_dir`, `launchd_log_dir`, `launchd_agents_dir`, `launchd_job`) | see below | Required only for a native launchd service (`--service-config`). A `--dev-subprocess` daemon (for example over SSH, with no `gui/<uid>` domain) omits them. When a daemon answers, `pohunek doctor` takes the launchd checks from its own report: a subprocess daemon reports none, so the CLI drops its local copies even when a `service.toml` is installed, and a native daemon's are merged with the local ones. Only when no daemon answers does the CLI infer the mode from `service.toml` (installed means native, otherwise unknown); an unknown mode reports them but a failure is at most a `warn`. |
| `launchd_domain` | `fail` when absent, `warn` when inconclusive | `launchctl print gui/<uid>` (fixed `/bin/launchctl`, argv only, 10 s deadline) reports whether the graphical domain exists. `pohunek doctor` calls `daemon.doctor` with a request timeout derived from that deadline plus reply headroom, so a wedged `launchctl` is reported by the daemon instead of the client giving up first. A bare SSH session without a console login has none. |
| `launchd_job` | `fail` for a failed job, or a loaded job without a process while no daemon answers; else `warn` | CLI doctor only: the installed daemon job's state from `pohunek service status`. launchd reports a loaded job as `running` or `unknown` (no process) and records no exit, so an `unknown` job is fatal only when the doctor also cannot reach the daemon. Not installed is a `warn`; a manually started daemon is valid. A pending install or upgrade is reported alongside the job state, never instead of it. |
| `bin:codex`, `bin:claude` | `warn` | Optional agents. A daemon started by launchd does not read shell startup files, so use an absolute agent profile `program` or fix the service PATH. |
| `login_shell` | `warn` | `$SHELL` must be an absolute executable listed in `/etc/shells`. |
| `desktop_notifications` | `warn` only when `osascript` is missing | Reports `/usr/bin/osascript`; delivery and user denial cannot be confirmed for an unbundled binary. If banners do not appear, allow notifications for the sending app in System Settings > Notifications. |
| `keychain` | `warn` | Presence of `/usr/bin/security` and the login keychain file only. No secret is read and lock state is not probed; a locked keychain is reported when a provider credential is first requested. |

Completion installation is idempotent and does not edit shell startup files.
The default script is static and performs no runtime lookup. Add `--dynamic`
only when host and session-target candidates are wanted; those queries are
deadline-bounded, do not start a daemon, and fail silently. Zsh installation
prints the `fpath` line that must appear before `compinit`.

Do not overwrite existing user config unless the user asks for that behavior and
the command supports it. For
profile and secret boundaries, see [agent profiles](../concepts/agent-profiles.md)
and [secrets](../safety/secrets.md). For the governance storage and lifecycle
boundary, see [host identity and local governance](../concepts/host-governance.md).

For the separately managed Hermes operator integration, use its typed install,
status, doctor, update, and uninstall commands. Do not edit Hermes YAML, a
database, credentials, or a real profile by hand; the install uses an isolated
profile or custom absolute home. See [Hermes operator](hermes-operator.md).
