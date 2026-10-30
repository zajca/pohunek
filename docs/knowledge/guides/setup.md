---
type: Guide
id: guide/setup
title: Local setup
description: Configure a local Pohunek host enough to run daemon-backed sessions.
source_kind: manual
intents: [setup, help]
---

# Local Setup

On a new host, run `pohunek setup` from the extracted daemon archive (or after
`brew install`). It installs the login service, the official runtime packages
and hooks for the agents on the service `PATH`, and the missing config files,
and reports each gap with its next step; `docs/cli.md`, "Host setup", lists
every step.
Run it again after you install an agent; on a ready host it changes nothing.

To inspect a host, start with structured inspection:

1. Run `pohunek doctor --json` to check local binaries, socket paths, and
   writable state directories. The check list depends on the platform; see
   [Doctor checks by platform](#doctor-checks-by-platform).
2. Run `pohunek service status --json` to see whether the daemon is installed
   as a login service. If `installed` is false, run `pohunek setup`, or install
   only the service with `pohunek service install` (see below). For development only,
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
The owner-private `service.toml` (schema 7) records `[input]`
`initial_startup_grace_ms = 5000`, `submit_delay_ms = 150` and
`write_stall_timeout_ms = 3000` on a fresh
install. These host-wide values control the maximum wait for an input-ready
agent at `session new`, the delay between the prompt body and its separate
submit write, and the longest time one `session.input` or initial input write
may wait without the agent reading a byte (attached terminal input is not
bounded). The submit delay names no runtime: it applies exactly
to the runtimes whose own runtime descriptor marks that delay configurable
(Claude opts in; its descriptor value 150 is replaced; the others keep their
descriptor value). All three keys are whole milliseconds in `1..=600000`; a
missing key, a value outside the range, or an unknown key makes the daemon
refuse startup with the named configuration error. The startup grace ends
early when the agent signals an editable prompt or enables bracketed paste; it
only delays silent startups. For
a runtime that declares `ready = "screen_idle"` (Claude, Pi, Hermes) only the
rendered prompt ends it, and a session still unready at the end of the grace fails
with `session_input_not_ready`; Codex declares the equally strict
`ready = "manifest_idle"`, where the manifest's winning idle evidence from the
screen or the terminal title ends it.
The write stall bound restarts after every byte the PTY accepts, so it bounds a
stalled reader, not a long input. A write that exceeds it fails with
`session_input_stalled`, possibly after part of the input was written; a
`session new` with initial input then removes the session. The 3000 ms default
stays below the client's 5 s request deadline, so a fire-and-forget caller gets
the typed error instead of `request_timeout`; a reading agent drains its input
in microseconds, so 3 s without one accepted byte means the reader is stuck.
The daemon reads edits when it starts, so restart the service to apply them.
The file also records `[waits] turn_default_ms = 300000` and `turn_max_ms =
1800000` on a fresh install: the timeout of a `session.wait_turn` call that
names none (5 min) and the ceiling a requested timeout may not exceed (30 min);
a request above the ceiling is an error, never clamped. Both keys are whole
milliseconds in `5000..=3600000`, and the default must not exceed the
ceiling; a violation makes the daemon refuse startup with the named
configuration error. A daemon started without `service.toml` uses the same
5 min and 30 min.
Three optional `[waits]` keys set the TCP keepalive of every connection the
daemon accepts on an overlay (NetBird or Tailscale) address: `keepalive_idle_ms = 30000`,
`keepalive_interval_ms = 10000` and `keepalive_count = 3`. A client host that
vanishes without closing its connection (network loss, suspend) then frees the
waiter slot of its in-flight `session.wait_turn` within
`idle + interval x count`, 60 s by default, instead of at the wait's deadline.
The two times are whole seconds written in milliseconds, each in
`1000..=600000`, and the count is in `1..=10`; a violation makes the daemon
refuse startup with the named configuration error. The installer omits a key
that equals its default, and the same defaults apply to a daemon started
without `service.toml`. The Unix control socket is unaffected.
The `[hooks] stderr_tail_bytes = 4096` key is the number of bytes of a failing
`pre-create` or `post-create` hook's stderr kept for the `session.new` warning.
It is a whole number in `0..=32768`; `0` disables the capture, and a value
above the ceiling makes the daemon refuse startup with the named configuration
error.
The optional `[hooks] required = ["post-create"]` key lists the hook events whose
failure aborts the operation instead of becoming a warning. Only `post-create`
can be required in this release; another known event, an unknown name or a
repeated entry makes the configuration load fail with the named key, and
`pohunek service check` reports it. The installer omits the key while it is
empty, so a previous release still reads the file. A previous release that reads
a file with the key reports it as an unknown key and does not enforce it. See
the hooks guide in [worktrees](../concepts/worktrees.md).
The `[events]` table bounds the event journal `<data>/events/events.jsonl`:
`max_file_bytes = 33554432` (one file, `1048576..=1073741824`),
`max_files = 8` (files kept including the active one, `2..=64`) and
`max_age_days = 14` (`1..=3650`). A file rotates to `events.jsonl.1` (and so on)
before a write would cross `max_file_bytes`; the oldest file is deleted beyond
`max_files`; a rotated file whose last write is older than `max_age_days` is
deleted when the daemon starts and after each rotation, and an `events.jsonl`
last written longer ago than `max_age_days` is removed when the daemon starts. The journal therefore
never exceeds `max_file_bytes * max_files`. The journal does not record
`agent_state` events that restate a session's last journaled activity and
source. The first start with these bounds removes, without reading it, an
`events.jsonl` above `max_file_bytes` (an earlier release let it grow without
limit) and logs one WARN with its path and size to `pohunekd.jsonl` (the same
WARN covers an age-expired active file); the
history it held is not kept. A daemon started without `service.toml` uses the
values above.
The optional `[inventory]` table bounds the version-probe cache behind
`host.inspect`: `probe_cache_ttl_ms = 120000` (how long a successful probe
stays valid, `1..=600000`), `probe_failure_ttl_ms = 10000` (how long a failed
probe is remembered, `1..=600000`), `probe_cache_max_entries = 256`
(`1..=4096`) and `probe_parallelism = 32` (most probes one inventory runs at
once, `1..=256`). Every key is optional, a value outside its range makes the
daemon refuse startup with the named configuration error, and the writer omits
the table while every key equals its default, so a previous release still reads
the file unless the operator set a key. Concurrent inventories probe each
program once. The daemon reads edits when it starts.
The optional `[worker_journals]` table holds `max_age_days = 7` (`1..=365`): how
long the journal directory `<state>/workers/<session>/` of a session that no
longer exists must be untouched before the daemon's startup collection may
delete it, once every worker its journals name is gone. The writer omits the
table while the key equals its default, so a previous release still reads the
file; a previous release that reads the table reports it as an unknown table
and keeps the journals. Removing a session deletes its journal directory right
after its record.
`<ns>` is a 12-hex-digit namespace derived from the user ID and the state and
runtime roots, so separate installations never touch each other's jobs.
They never share a prefix either: the first install claims it in
`<prefix>/libexec/pohunek/installation_owner`, and an install from another
namespace (different XDG state or runtime roots) into the same prefix fails
with `service_prefix_owned`; pick another `--prefix`, or uninstall the owning
installation first, which releases the prefix. An existing version directory
is reused only when it and its binaries are exactly what the installer creates
(owned by you, mode `0755`, no symbolic links, no extra hard links); otherwise
the install fails with `service_version_untrusted`. The same check runs again
before any command executes or copies content from an installed version directory
(the source of an install or upgrade, including `service check`, the rollback and
runtime steps, and `pohunek setup` on an installed host). It covers the binaries,
the catalog trust anchor and the `runtime/` tree, so a directory that became
group-writable after the install is refused instead of run. On macOS the install also
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
running any archive binary, with nothing changed. The verifier runs `id` from
`/usr/bin`, `/bin` or, on NixOS, `/run/current-system/sw/bin`, never from `PATH`,
and trusts a directory only when the directory it resolves to is closed to group
and others and owned by root or the installing user; a host with none is refused
with `no trusted id found in <list>`. The install is ready only
when the daemon job's own main process answers `daemon.health` on the socket;
an install that fails from the registration step on keeps its record and fails
with `service_install_incomplete`, and rerunning `pohunek service install` with
the same version and prefix finishes it (`pohunek service uninstall` removes it
after checking for live sessions instead, even when its `service.toml` is
already gone). Upgrades use
`pohunek service upgrade`; `pohunek service status` shows the daemon job,
versions, and workers. When the installed `service.toml` is a previous
release's schema (schema 2, which predates the `[input]` and `[waits]`
tables, schema 3, which predates `[waits]`, schema 4, which predates
`input.write_stall_timeout_ms`, or schema 5, which predates `[hooks]`), the upgrade
is a configuration migration: the upgrade first saves the exact pre-upgrade
bytes owner-private next to its transaction record and journals their
digest, stops the daemon job before it rewrites `service.toml` (a previous
daemon's strict reader would refuse a restart against the new schema), then
registers the new daemon; the migration assigns the documented
defaults of the missing tables explicitly, keeps every recorded value, and
the `--json` report names the source schema with `config_schema_migration`. A crash resumes from the record after re-verifying
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
schema-2, schema-3, schema-4 or schema-5 installation it names the migration it would perform); `pohunek service lock -- <command>` runs a command that no other
service transaction can interleave with (the archive installer uses both). For
upgrades, removal, and runtime diagnosis, see
[update after release](../runbooks/update-after-release.md) and
[durable session workers](../runbooks/debug-session-runtime.md).

The installer refuses a unit or `LaunchAgents` directory that is group- or
world-writable and names the fix (`chmod go-w <path>`); it never changes
permissions itself.

`pohunek setup` is the host setup. Without a subcommand it previews the service
install or the kept service, the official runtime packages, profile pins, hooks
and gaps of this host, asks once (`--yes` applies unattended; without a
terminal and without `--yes`, or with `--json` and no `--yes`, it refuses;
`--dry-run` prints the plan and changes nothing), and then installs the service and
the packages, pins the profiles, installs the hooks of the homes that exist, and
writes the missing config files. It never upgrades an installed service: a
different installed version is a gap, and `pohunek service upgrade` runs first.
Every Hermes home the run acts on is listed in the preview. A marked home
whose plugin is already current is kept as it is, and setup runs the
`hermes` found on the daemon job's `PATH`. A home without the
pohunek marker is a gap that names the
Hermes install command (`pohunek integration install --agent hermes`); setup never chooses its access
policy and never writes into it. If the plan read after the service install
differs from the preview in a way the install engine does not account for,
setup stops with the service installed, names the install's changes, and
changes nothing else; a re-run converges. A pending interrupted install stops
setup before any change, and so does a gap the install's own runtime step
would refuse (a used runtime without a package for this platform). On an
installed host the plan is read again after the consent, and a host that
changed while the preview was shown stops the run before any change.
The subcommands are:

- `pohunek setup config` writes a default `attach.conf` (the `pohunek attach`
  reconnect settings, every key commented at its default) and the
  `prompts/issue.tmpl` and `prompts/pr.tmpl` templates the daemon resolves for
  project actions. Existing files are never overwritten unless `--force` is
  given.
- `pohunek setup completions <bash|zsh|fish>` installs shell completion.

`pohunek setup config` returns the `created` and `skipped` file lists with
`--json`; the bare `pohunek setup` reports them among its results. Desktop
launchers (rofi, sway) and the issue/PR pickers are not part of core; they live in `zajca/pohunek-work`
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
log directory writability checks, one `<overlay>_cli` check per configured
overlay (`netbird_cli` for the default NetBird overlay), and `schema_version`.
The provider and port the remote listener uses come from the optional
`overlays.toml`; see [Remote hosts](remote-hosts.md#overlay-configuration).
A provider CLI that is missing or untrusted fails its check when
`overlays.toml` lists the provider and only warns for the NetBird default of an
absent file. `pohunek doctor` adds `overlay_config`, which warns "restart the
daemon to apply" when the running daemon serves another overlay or port than the
file configures, and the daemon adds `overlay_<overlay>_listener` with its port
and the listener address its provider reports.
Executables count only when they are regular files the effective user can execute according to the kernel (`faccessat` with `X_OK`); a file it cannot execute is skipped and the `PATH` search continues. The writability probes create a randomly named file exclusively (never following a planted symlink) and remove it.

macOS adds the following checks. The daemon report also includes redacted
Codex and Claude hook integration readiness. A local request checks each
distinct configured profile home; a remote request checks only runtime homes.
Current hooks are `ok`, missing or outdated hooks are `warn`, and malformed or
unsafe provider state is `fail`. A status that a newer daemon adds reads as
`unknown` in `pohunek doctor`, ranks like `warn` and never makes a merged check
`ok`; the daemon's other checks stay listed. Configured Hermes profiles are explicitly
reported as unverified because the daemon has no inspection policy for their
CLI-managed hooks. No provider home or profile environment value is returned.
For detailed hook findings, including Python interpreter readiness, run
`pohunek integration doctor` on the daemon host:

| Check | Failure status | Meaning and remediation |
| --- | --- | --- |
| `runtime_dir_private` | `fail` | The runtime root (default `/private/tmp/pohunek-<uid>`) fails the same owner-private validation the daemon applies at startup: it must be a real directory you own with mode exactly `0700`, no ACL beyond the mode, and no symlinked path component. Remove it or point `XDG_RUNTIME_DIR` at a valid directory. An absent directory is `ok`. A directory that fails this check is never written to: `socket_dir_writable` reports `fail` without probing it. |
| `socket_dir_writable`, `state_dir_writable`, `log_dir_writable` | `fail` | On macOS the doctor never creates a directory (a default-umask `0755` directory would be refused at startup). An existing runtime or data directory must pass the daemon's owner-private validation and is then write-probed; the log directory needs a real directory owned by you below a valid state root (startup resets its mode to 0700, so a mode such as 0500 is `ok`); a missing directory is `ok` when startup could create it below a trusted, writable ancestor. |
| `worker_runtime_root`, `worker_state_root`, `launchd_definitions_dir`, `launchd_log_dir` | `fail` | `<runtime>/workers`, `<state>/workers`, `<state>/launchd` and `<logs>/launchd` are the directories startup and the launchd worker supervisor open owner-private (`0700`). Same rule as the runtime root: real directory, yours, mode `0700`, no symlinked component, or absent below a trusted writable ancestor. |
| `launchd_agents_dir` | `warn` | `~/Library/LaunchAgents`, opened by `pohunek service install`: an existing directory must be a real directory you own that group and others cannot write (`chmod go-w`); a missing one is created. |
| `socket_path_length` | `fail` | The daemon socket, or the longest worker socket including staged bind names, exceeds Darwin's 103-byte `sockaddr_un` limit. Set a shorter `XDG_RUNTIME_DIR`. |
| `filesystem_access` | `fail` | A required directory is denied. The probe runs in the privacy context of the process that runs it and says so (`readable by this process`): the CLI covers the config and data roots plus its current directory in the terminal's context, the daemon covers only its config and data roots in the launchd context. Neither proves that the other context can open your project directories. `EPERM`, or any denial below Documents, Desktop, Downloads, iCloud Drive, `Library/CloudStorage` or `/Volumes`, is a Privacy & Security (TCC) denial: grant the app that started the process (your terminal app, or the `pohunekd`/`pohunek-sessiond` executables when launchd runs them) access under System Settings > Privacy & Security > Files and Folders, or keep projects outside protected folders. Full Disk Access is not required and not recommended by default. Other denials point at ownership and mode. |
| `service_config_unknown_keys` | `warn` | CLI doctor only, with an installed and loadable `service.toml`: it holds keys or tables this version does not know (a typo, or a setting of a newer release). They are ignored and kept when the installer rewrites the file; the detail lists key paths and lines with a "did you mean" hint, never values. `ok` when there are none. An unknown key in `[namespace]` or `[environment]` is not a warning: the file does not load. |
| `service_config_shutdown_budget` | `warn` | CLI doctor only, with an installed and loadable `service.toml`: the daemon shutdown phase bounds (`create_drain_ms`, `driver_shutdown_ms`, `registry_shutdown_ms`, `event_log_flush_ms`) plus a 2000 ms margin exceed `daemon_exit_timeout_ms`. The file stays valid: the daemon warns at start and shortens the bounds in proportion. The detail names the least exit timeout that fits. Raise `daemon_exit_timeout_ms` or lower the phase bounds. `ok` when they fit. |
| `service_search_path_agents` | `warn` | CLI doctor only, macOS, with an installed `service.toml` that records a search path: `claude` or `codex` resolves through the search path your login shell reports now but not through the one recorded in `service.toml`, so the daemon, which runs with the recorded path, does not find it (an agent profile with an absolute `program` still starts). The detail names the program and where the login shell finds it; compare with `pohunek service refresh-path --dry-run`, then record the path with `pohunek service refresh-path`. The probe runs the same bounded login-shell discovery as `refresh-path`, takes no transaction lock and reports nothing when the shell fails or hangs, on a host that records no path, without an installation that loads and verifies (for example a runtime root that does not exist yet), or while a transaction record is pending (`pohunek service status` names it). `ok` when every program the login shell resolves also resolves through the recorded path. |
| `working_directory` | `fail` | CLI doctor only: the current directory cannot be determined (removed or unreadable), so `filesystem_access` cannot probe it. Run doctor from an existing readable directory. |
| `worker_executable` | `fail` (or `warn` for an older worker without capability reporting) | The daemon probes the worker executable used by its active supervision with a bounded, scrubbed `--protocol-info` call. Missing or unsafe executables, malformed capability data, unreadable journal schemas, and non-overlapping private protocol ranges fail with fixed redacted remediation. A worker that exits unsuccessfully for `--protocol-info` but returns a valid `pohunek-sessiond <version>` from bounded `--version` is reported as unverified (`warn`); malformed successful capability output is an error. `pohunek doctor` prefers the daemon's result. Without a reachable daemon the CLI derives the candidate like `pohunek daemon start`: installed `service.toml`, then `POHUNEK_WORKER_BIN` (any present value is used, an empty one included, and rejected unless absolute), then next to the located `pohunekd` (made absolute). |
| launchd checks (`launchd_domain`, `launchd_definitions_dir`, `launchd_log_dir`, `launchd_agents_dir`, `launchd_job`) | see below | Required only for a native launchd service (`--service-config`). A `--dev-subprocess` daemon (for example over SSH, with no `gui/<uid>` domain) omits them. When a daemon answers, `pohunek doctor` takes the launchd checks from its own report: a subprocess daemon reports none, so the CLI drops its local copies even when a `service.toml` is installed, and a native daemon's are merged with the local ones. Only when no daemon answers does the CLI infer the mode from `service.toml` (installed means native, otherwise unknown); an unknown mode reports them but a failure is at most a `warn`. |
| `launchd_domain` | `fail` when absent, `warn` when inconclusive | `launchctl print gui/<uid>` (fixed `/bin/launchctl`, argv only, 10 s deadline) reports whether the graphical domain exists. `pohunek doctor` calls `daemon.doctor` with a request timeout derived from that deadline plus reply headroom, so a wedged `launchctl` is reported by the daemon instead of the client giving up first. A bare SSH session without a console login has none. |
| `launchd_job` | `fail` for a failed job, or a loaded job without a process while no daemon answers; else `warn` | CLI doctor only: the installed daemon job's state from `pohunek service status`. launchd reports a loaded job as `running` or `unknown` (no process) and records no exit, so an `unknown` job is fatal only when the doctor also cannot reach the daemon. Not installed is a `warn`; a manually started daemon is valid. A pending install or upgrade is reported alongside the job state, never instead of it. |
| `bin:codex`, `bin:claude` | `warn` | Optional agents. A daemon started by launchd does not read shell startup files, so use an absolute agent profile `program` or fix the service PATH. |
| `login_shell` | `warn` | `$SHELL` must be an absolute executable listed in `/etc/shells`. |
| `desktop_notifications` | `warn` only when `osascript` is missing | Reports `/usr/bin/osascript`; delivery and user denial cannot be confirmed for an unbundled binary. If banners do not appear, allow notifications for the sending app in System Settings > Notifications. |
| `keychain` | `warn` | Presence of `/usr/bin/security` and the login keychain file only. No secret is read and lock state is not probed; a locked keychain is reported when a provider credential is first requested. |

Completion installation is idempotent and does not edit shell startup files.
The default script is static and performs no runtime lookup. Add `--dynamic`
only when host, session-target, package and workflow plugin candidates are
wanted; those lookups are deadline-bounded, do not start a daemon, and fail
silently. Zsh installation
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
