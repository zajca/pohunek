---
type: Guide
id: guide/workflow-plugins
title: Workflow plugins
description: What a workflow plugin is, the workflow.toml manifest, installing a plugin pinned by its digest with pohunek workflow, running its actions with pohunek workflow run (verbatim arguments, --json results, environment, context, limits, run log, owner knobs), its periodic jobs and long-running services as user service manager units, and the same-UID owner trust boundary.
source_kind: manual
intents: [setup, update, debug, help]
---

# Workflow plugins

A workflow plugin is a bundle of owner tooling, such as a work-overview tool
that joins an issue tracker, a code host and pohunek sessions into one table.
It declares named **actions** as argv arrays, and optionally periodic
**jobs** and long-running **services** that your user service manager runs
while the plugin is enabled. It acts on pohunek only through the public CLI or
SDK, like any script the owner runs. It is not a runtime
package: the daemon never loads, lists or runs it, and it cannot extend the
daemon, the session worker, protocol methods, overlay transports or any user
interface.

`pohunek workflow` manages the plugins of the current user on this machine:

```bash
pohunek workflow install work.tar.zst --sha256 <digest>        # review, nothing installed
pohunek workflow install work.tar.zst --sha256 <digest> --yes  # install, enabled
pohunek workflow list [--json]
pohunek workflow inspect acme.work [--json]
pohunek workflow disable acme.work
pohunek workflow enable acme.work
pohunek workflow uninstall acme.work [--purge] [--remove-modified]
pohunek workflow run [--json] acme.work overview [<args>...]
```

Event reactions are not yet available; the manifest refuses the reaction
table until it ships (#148).

With dynamic shell completion (`pohunek setup completions <shell> --dynamic`),
Tab completes installed plugin ids for `inspect`, `enable`, `disable`,
`uninstall` and `run` (`run` offers only enabled plugins), and after
`workflow run <id>` the actions of that plugin. A plugin whose installed files
fail verification offers no actions. Completion only reads the store, within
its short deadline, and stays silent when the store is missing or unusable.

## The archive and its manifest

A workflow plugin ships as the same canonical `tar.zst` archive as a runtime
package (see the [runtime package archive](../concepts/runtime-package-archive.md)),
with `workflow.toml` at the archive root. An archive that also carries
`runtime.toml` is refused: one archive is never both kinds. In a pohunek
source checkout, `cargo xtask package build <dir> --output <file>` builds the
archive from a directory; executable files keep their executable bit.

```toml
schema = 1

[plugin]
id = "acme.work"            # package id grammar: [a-z0-9._-], no leading . or -, no ..
version = "1.2.0"           # [A-Za-z0-9.+_-], at most 64 bytes
description = "Work overview"

[actions.overview]          # action name: [a-z][a-z0-9_]{0,31}
argv = ["bin/work", "overview"]
args = "passthrough"        # or "none" (the default): extra arguments refused
timeout_ms = 30000          # optional
max_output_bytes = 1048576  # optional

[jobs.poll]                 # job name: same grammar as an action name
argv = ["bin/work", "poll"]
every_s = 300               # wall-clock seconds from one run's start to the next
timeout_ms = 120000         # optional

[services.notifier]         # service name: same grammar
argv = ["bin/work", "notify"]
```

Rules, each checked at install time:

- The manifest is at most 64 KiB, checked before it is parsed. Every table
  refuses unknown keys, so `[reactions]` is refused today. At least one
  action, job or service is required. The description is 1 to 1024 bytes.
- `argv` has 1 to 64 entries of at most 4096 bytes each, with no NUL byte. No
  shell ever interprets it.
- `argv[0]` is either a package path to an executable file of the archive
  (`bin/work`, resolved to the installed copy of that file) or an absolute
  host path such as `/usr/bin/git` (an absolute path must name a file, so `/`
  is refused). A bare name (`work`, `git`) is refused
  even when the archive has a file of that name, because no `PATH` is
  searched. A path with `.`, `..` or empty segments is refused, and so is a
  package path that is missing from the archive or lacks the executable bit.
- The argv rules apply to jobs and services too. A job's `every_s` is 1 to
  604800 (one week).
- `timeout_ms` and `max_output_bytes`, when present, must be positive. They
  are checked against the owner limits when the action runs (see
  [Owner knobs](#owner-knobs)), not at install time. A job's `every_s` and
  `timeout_ms` are checked against the `[jobs]` knobs when the plugin is
  enabled and again when the job runs.

A refused manifest fails with `workflow_descriptor_invalid`. The message names
the broken rule and, where it applies, a line number, the action name (which
already matched the name grammar) or an unsupported schema number. It never
quotes an argv entry, a description or any other manifest string, so an argv
entry cannot leak through an error.

## Install, trust and consent

Trust is the archive digest you supply with `--sha256` (`sha256:<64 hex>` or
the bare hex `sha256sum` prints). There is no catalog for workflow plugins.
Install reads the archive, checks its digest (`workflow_digest_mismatch`
otherwise) and validates the manifest, then prints the id, version, digest,
and every action's, job's and service's argv with `argv[0]` resolved to its
absolute path, with each job's interval and requested timeout. Nothing
is created until you repeat the command with `--yes`; until then it fails with
`consent_required`. `--no-enable` installs the plugin disabled. A plugin with
jobs or services is enabled as `pohunek workflow enable` enables it (see
[Jobs and services](#jobs-and-services)); without a `pohunek service`
installation of this pohunek version an enabling install is refused before
anything is created (`workflow_service_not_installed`), and `--no-enable` still
installs it. An
enabling install of a new plugin with units checks the service installation
and its environment and the `[jobs]` bounds, and takes the unit lifecycle
lock (`workflow_store_busy` while another enable, disable or uninstall holds
it), all before it records the plugin; for an archive already recorded it
decides under that lock whether the plugin is still recorded. Three steps
come after the record is written: reaching the service manager, marking the
plugin enabled (which needs the store lock, so another workflow command
holding it gives `workflow_store_busy`), and installing the units. When one
fails, the command fails with `workflow_installed_disabled`, whose `cause`
is the failure's own code. Normally everything is undone: the plugin stays
installed disabled with no units, and after fixing the cause
`pohunek workflow enable <id>` finishes the job; installing the archive again
does not enable it. When undoing the failed step fails too (a unit install, or
clearing a flag that may have been written), the message says the plugin may
remain enabled or keep units: run `pohunek workflow disable <id>` until it
succeeds, then enable it. Installing
a recorded archive again never touches its units: running jobs and services
are neither stopped nor restarted.

The store keeps one version per plugin id. Installing the recorded archive
again changes nothing (`already_installed`), unless its installed files are
missing: the review says so, and `--yes` extracts them again
(`root_restored`). Another archive of an installed id is refused with
`workflow_plugin_installed` until you uninstall the first. When the store
recorded an install but could not confirm the change is durable, install
succeeds with a warning; when it cannot even read the outcome back, it fails
with `workflow_install_uncertain`, keeps the plugin's directories, and asks you
to check `pohunek workflow list` (installing the same archive again is safe). Every command that
changes the store holds the store lock `<state>/pohunek/workflow-plugins/store.lock`
for its whole run, so two installs never record two versions of one id; a
command that finds the lock held fails with `workflow_store_busy` and changes
nothing.

Where things live, all owner-private (`0700`):

- `<state>/pohunek/workflow-plugins/` holds the plugin registry and the
  verified, content-addressed plugin files. It is separate from the runtime
  package store `<state>/pohunek/plugins/`.
- `<state>/pohunek/workflow-plugin-state/<id>/` is the plugin's state directory.
- `<config>/pohunek/workflow-plugin-config/<id>/` is the plugin's config
  directory.

`<state>` is `$XDG_STATE_HOME` (default `~/.local/state`) and `<config>` is
`$XDG_CONFIG_HOME` (default `~/.config`). The three locations never contain
each other; a configuration that makes one lie inside another is refused with
`workflow_dirs_overlap`. Install requires both parent directories to be
exactly `0700` and fails closed otherwise (`workflow_store_failed`), removing
any plugin directory it created.

The state and config directories are keyed by id, so they survive an
uninstall. `uninstall --purge` deletes them, also for an id an earlier
uninstall already removed. A purge always attempts both directories. When it
cannot finish, for example because a subdirectory is read-only, it fails with
`workflow_purge_incomplete`, says whether the plugin itself was uninstalled,
and names where the remaining data is (a `.purge-<id>+<hex>` directory beside
the plugin's); fix the cause and run the purge again. Every purge first
retries the leftovers in both parents. A leftover of the plugin being purged
is its own data: while it cannot be removed, the purge keeps failing with
`workflow_purge_incomplete` and names where it is. A leftover of another
plugin is retried best effort: one that still cannot be removed is reported in
the result's `warnings` with its location and never decides the outcome.

`inspect` and `enable` re-verify the installed files first and fail with
`workflow_root_invalid` when they are missing or changed since install. The
recovery hint depends on the cause:

- Missing files: install the same archive again, which restores them, or
  remove the plugin with a plain `uninstall`.
- Changed files are left as found: a plain `uninstall` refuses them too, and
  `uninstall --remove-modified` removes them.
- Files that cannot be read: nothing is judged or removed; fix the store's
  ownership or permissions and retry. When reading was refused for lack of
  permission, `uninstall --remove-modified` also removes the files.

`--remove-modified` refuses files that verify or are missing
(`workflow_root_intact`). When an uninstall removed the record but some
installed files could not be deleted, the command succeeds with a warning that
names the `.collect-*` residue below `<state>/pohunek/workflow-plugins/packages/`;
later store changes retry removing it, best effort.

## Running an action

`pohunek workflow run [--json] <plugin> <action> [<args>...]` runs one
declared action and exits with the action's own status (128 plus the signal
number when a signal ended it). Every argument after `<action>` belongs to the
action and is appended to argv verbatim, including `--json`, `--`, `-h`,
`--host` and empty strings, when the action declares `args = "passthrough"`;
with `args = "none"` any extra argument is refused with
`workflow_action_args_refused`. pohunek's own options, `--json` and the
global `--host`, therefore go before `<plugin>`
(`pohunek workflow run --json acme.work overview --since 1d`).

Before anything starts, the plugin's installed files are verified again, and
the run is refused when the plugin is unknown (`workflow_plugin_unknown`),
disabled (`workflow_plugin_disabled`), tampered with
(`workflow_root_invalid`), the action is not declared
(`workflow_action_unknown`), exactly one origin marker is set
(`workflow_origin_incomplete`, see below), the action declares a `timeout_ms`
above the owner ceiling (`workflow_timeout_refused`), or the context is
larger than its cap (`workflow_context_too_large`). A refused run starts
nothing.

### `--json` results

With `--json`, stdout is captured instead of shown, up to the same cap, and
stderr still streams to the terminal. The context's `output` is `json`, so the
action can choose its format. The command prints exactly one CLI process
envelope on stdout:

- The action exited 0 and its stdout is exactly one JSON document, with JSON
  whitespace allowed around it: `ok` is that document byte for byte (key
  order, number spelling and inner whitespace are kept), and the exit status
  is 0.
- The action exited non-zero or was ended by a signal the CLI did not send:
  `workflow_action_failed` with `exit_code` (the action's code, or 128 plus
  the signal number) and `signal` when a signal ended it. The CLI exits with
  `exit_code`, the same status as in text mode, and prints none of the
  action's stdout.
- The action exited 0 but its stdout is empty, is not JSON, ends inside its
  document, or has data after it: `workflow_output_invalid`, exit status 1.
  Arrays and objects nested deeper than 126 levels are reported as not JSON:
  the envelope adds one level, and common JSON parsers, serde_json's default
  among them, refuse more than 127.
- The action exited 0 but a process that left its group still held stdout
  open (see below): `workflow_output_incomplete`, exit status 1. The capture
  is never judged, because that process could still add to it.
- Every other error (refusals, `workflow_output_limit`,
  `workflow_action_timeout`, `workflow_action_interrupted`, …) is an error
  envelope with the exit status the table below gives.

No error envelope holds any byte of the action's output.

The action runs without a shell. argv is the installed `files/<argv0>` (or
the absolute host path) followed by the manifest arguments and any passed
arguments. The child runs in the plugin's installed `files/` directory, with
stdin from `/dev/null`, in a process group of its own, with every inherited
descriptor above stderr closed.

### Environment

The child environment is built from an empty one. It holds exactly:

- `POHUNEK_PLUGIN_ID`, `POHUNEK_PLUGIN_VERSION`, `POHUNEK_PLUGIN_DIGEST`.
- `POHUNEK_PLUGIN_CONFIG_DIR` and `POHUNEK_PLUGIN_STATE_DIR`, the plugin's
  own directories.
- `POHUNEK_PLUGIN_CONTEXT`, the context JSON below.
- `POHUNEK_CLI`, the canonical path of the `pohunek` that runs the action.
  Call it for every pohunek operation; no `PATH` is set.
- `HOME`, `XDG_RUNTIME_DIR`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME`,
  `XDG_DATA_HOME` and `XDG_CACHE_HOME`, each only when it is set for the CLI.
  These non-secret roots let `$POHUNEK_CLI` find the same daemon socket and
  the same stores as the CLI that started the action.
- `POHUNEK_SESSION_ID` and `POHUNEK_DAEMON_ID`, only when both are set for the
  CLI. When `workflow run` runs inside a managed session, the action inherits
  the session's origin, so the daemon still refuses the action's attempts to
  stop, input to, resize or otherwise act on that same session
  (`plugin_self_target_denied`). When exactly one of the two is set, the run is
  refused with `workflow_origin_incomplete` before anything starts.

Nothing else is passed: no `PATH`, locale, `TERM`, tokens,
`POHUNEK_SOCKET_PATH` or `POHUNEK_WORKER_*` variables.

### Context

`POHUNEK_PLUGIN_CONTEXT` is one compact JSON object:

```json
{"schema":1,
 "plugin":{"id":"acme.work","version":"1.2.0","digest":"sha256:…"},
 "invocation":{"kind":"action","name":"overview","run_id":"01K…"},
 "output":"text|json",
 "cli":{"version":"0.34.2","protocol":{"minimum":3,"maximum":4}}}
```

`output` is `json` for `workflow run --json` and `text` otherwise.
`run_id` is a ULID, also written to the run log. `cli.protocol` is the public
protocol range the running CLI speaks. A context larger than
`limits.max_context_bytes` refuses the run (`workflow_context_too_large`); it
is never truncated.

### Deadlines, output caps and stopping

The deadline is the action's `timeout_ms`, or `limits.action_timeout_default_ms`
when it declares none. Stdout and stderr stream to the terminal, each up to its
cap: `limits.max_stdout_bytes` and `limits.max_stderr_bytes`, or the action's
`max_output_bytes` when that is lower.

The CLI stops the action's process group when the deadline passes, when an
output stream passes its cap (the bytes past the cap are never shown, and the
CLI stops reading the stream), or when the CLI itself receives `SIGINT`,
`SIGTERM` or `SIGHUP` (the action's group does not receive the terminal's
`Ctrl-C` directly). Stopping sends `SIGTERM` to the group, waits until the
action's process exits or `limits.kill_grace_ms` elapses, then sends `SIGKILL`
to the group, so a process that ignores `SIGTERM` and descendants left in the
group still end. The grace ends early when the action's own process exits,
and the rest of the group is then killed at once, as it is after the action
exits by itself. A wrapper script should therefore `exec` the real program,
so the program itself receives `SIGTERM` and the whole grace. A descendant
that leaves the group (`setsid`) escapes this, as any same-user process can.

The action's output is delivered up to its cap however slowly your terminal,
pager or pipe reads it: the CLI waits for its own stdout and stderr before it
ends the run, unless it receives `SIGINT`, `SIGTERM` or `SIGHUP` meanwhile,
which ends the run as `interrupted`. Once the group is gone, an output pipe
that is still open is held by such an escaped descendant: after one second
not spent waiting for your consumer, whether the descendant writes or not,
the CLI stops waiting for the stream, keeps the run's outcome, and prints
`warning: the stdout of action … was cut` (or `stderr`) without any of the
stream's bytes. With `--json` a cut stdout instead fails a run whose action
exited 0 with `workflow_output_incomplete`.

When several apply, the outcome is decided in this order: an output stream
over its cap, then a signal the CLI received, then the deadline, then a
failure to observe the action, then the action's own exit status; with
`--json` a zero status is then followed by a cut stdout, then stdout that is
not exactly one JSON document.

| Result | Error code | Exit status |
|---|---|---|
| The action exited | none | the action's status |
| `--json`: the action exited non-zero or by a signal | `workflow_action_failed` | the action's status |
| `--json`: stdout was cut | `workflow_output_incomplete` | 1 |
| `--json`: stdout is not exactly one JSON document | `workflow_output_invalid` | 1 |
| Output over its cap | `workflow_output_limit` | 125 |
| The CLI received a signal | `workflow_action_interrupted` | 128 plus the signal number |
| The deadline passed | `workflow_action_timeout` | 124 |
| The action could not be started | `workflow_spawn_failed` | 1 |
| The CLI could not observe the action | `workflow_run_failed` | 1 |
| Refused before anything started | the refusal code | 1 |

A missing or non-executable program is reported as `workflow_spawn_failed`
before anything starts.

### Run log

Every run of an installed plugin, refused or not, appends one JSON line to
its run log, except a run that fails before the log is opened: for example
`workflow_local_only`, `workflow_dirs_overlap`, a missing `HOME`/XDG root,
`workflow_config_invalid` (the knobs set the log's retention),
`workflow_plugin_unknown`, a `workflow_store_failed` reading the store, or
`workflow_run_log_failed` itself. The log is
`<state>/pohunek/workflow-plugin-state/<id>/logs/runs.jsonl`, a bounded,
rotated, owner-private family (`runs.jsonl`, `runs.jsonl.1`, …):

```json
{"ts":"2026-10-13T12:00:00Z","run_id":"01K…","kind":"action","name":"overview",
 "version":"1.2.0","digest":"sha256:…","outcome":"ok","exit_code":0,
 "duration_ms":412,"stdout_bytes":1830,"stderr_bytes":0,"arg_count":1}
```

`kind` is `action` for `workflow run`, and `job` or `service` for a run of a
unit (see [Jobs and services](#jobs-and-services)). `outcome` is `ok`,
`exit_nonzero`, `timeout`, `output_limit`, `interrupted`, `spawn_failed`,
`failed`, `refused`, `overlap_skipped` (a unit run skipped because the
previous run of the same unit was still active), or, with `--json` after a
zero exit, `output_incomplete` or `output_invalid`. A service run also has
`backoff_ms`, the restart delay it waited before it started. `error_code` is present when the run
ended with an error and holds that error's code. `exit_code` or `signal`
describes how the action's process ended. `name` is present only when it is an
action the verified manifest declares. Byte counts are the bytes shown (with
`--json`, the bytes captured), at most the cap. A record never holds an argv value, an environment value, the
context or any output byte, and neither do the CLI's error messages. A run
whose log cannot be opened is refused with `workflow_run_log_failed`; a
record that cannot be written after a run is reported as a warning on stderr.

### Owner knobs

`<config>/pohunek/workflow.toml` holds the owner limits of workflow runs. It is
optional: without it every knob has its default. Its directory
`<config>/pohunek/` must not be writable by other accounts even when the file
is absent, because another account could otherwise plant one; a group- or
world-writable directory fails every run with `workflow_config_invalid`. When
the file exists it must be a regular file owned by you with mode `0600`, and
it is strict: an unknown table or key, a wrong type or an
out-of-range value fails every run with `workflow_config_invalid`, naming the
line or the key and its range, before the store is read.

```toml
[limits]
action_timeout_default_ms = 60000     # 1..=action_timeout_max_ms
action_timeout_max_ms = 600000        # 1..=86400000; a larger timeout_ms is refused
kill_grace_ms = 5000                  # 0..=60000; 0 sends SIGKILL at once
max_stdout_bytes = 16777216           # 1..=1073741824
max_stderr_bytes = 1048576            # 1..=1073741824
max_context_bytes = 65536             # 1..=98304 (96 KiB hard maximum)

[logs]
run_log_file_bytes = 1048576          # 4096..=67108864
run_log_files = 4                     # 1..=64, the active file included

[jobs]
timeout_default_ms = 60000            # 1..=timeout_max_ms; a job without timeout_ms
timeout_max_ms = 300000               # 1..=510000; a larger timeout_ms is refused
min_interval_s = 60                   # 1..=604800; a shorter every_s is refused
check_period_s = 60                   # 1..=3600; how often a job's timer checks

[services]
restart_initial_ms = 1000             # 1..=restart_max_ms
restart_max_ms = 300000               # 1..=86400000
stable_after_ms = 60000               # 1..=86400000
```

The values shown are the defaults. The context cap's hard maximum of 96 KiB
keeps `POHUNEK_PLUGIN_CONTEXT` below the length Linux allows for one
environment string. `jobs.timeout_max_ms` is at most 510000 ms: a unit's
service manager start timeout is ten minutes, and a job run keeps 60 s for the
longest kill grace and 30 s for its own checks below it, so the run is always
recorded as `timeout` before the service manager would kill it. Every knob is
read again at each run except `jobs.check_period_s`, which is written into a
job's timer when the plugin is enabled: run `pohunek workflow enable` again
after changing it. The reactions table arrives with that feature and is
refused until then.

## Jobs and services

A plugin's jobs and services run as units of your user service manager:
systemd user units on Linux, launchd agents on macOS. They need the
`pohunek service` installation: every unit runs its `<prefix>/bin/pohunek`,
in the same namespace and unit directory as the daemon unit, so
`pohunek workflow enable` of a plugin with jobs or services fails with
`workflow_service_not_installed` without one. Plugins that declare only
actions never touch the service manager.

- `enable` verifies the plugin's files, refuses a job whose `every_s` is
  below `jobs.min_interval_s` or whose `timeout_ms` is above
  `jobs.timeout_max_ms` (`workflow_unit_refused`, nothing changed), and
  refuses an installation recorded for another pohunek version
  (`workflow_service_not_installed`, nothing changed; its message names both
  versions). The units run this CLI's trampoline, so `pohunek service install`
  from this version, or an upgrade of the installation to it, is the
  recovery. It then marks the plugin enabled, records the names of its units,
  then installs, enables for login and starts every unit. If a unit cannot be
  installed, every unit is removed again, the recorded names are forgotten,
  the plugin is disabled again, and the command fails with
  `workflow_units_failed`.
  Enabling an enabled plugin replaces its units, which also applies a changed
  `jobs.check_period_s`.
- `disable` marks the plugin disabled first, so no further run starts, then
  stops every unit (a running job or service is stopped as described in
  [Deadlines, output caps and stopping](#deadlines-output-caps-and-stopping))
  and removes it. The units are the ones the plugin's unit record names and,
  when its installed files verify, the ones its manifest declares. So a
  plugin whose installed files fail verification still has its running units
  stopped and removed, and its record is forgotten once every unit is gone.
  Only a plugin whose files fail verification and that has no unit record
  gets a warning instead: no unit can be named, and a unit left behind
  refuses every run (the trampoline verifies the archive first).
- `uninstall` first applies its own refusals (`--remove-modified` on files
  that verify fails with `workflow_root_intact`, and so on), which change
  nothing; it then marks the plugin disabled and removes every unit before it
  removes the plugin. `--remove-modified` therefore stops the units of a
  plugin whose files were modified, as `disable` does. While a unit cannot be
  removed, for example because the service manager is unreachable, uninstall
  fails with `workflow_units_failed`
  (or `workflow_service_not_installed` when the recorded installation is
  unusable) and the plugin stays installed, disabled; repeat it once the
  cause is fixed.
- `pohunek service uninstall` refuses with `workflow_units_installed` while
  any plugin's unit record names a unit. The check runs before the service
  manager is contacted and changes nothing. It takes `units.lock` (a unit
  command running at the same time makes it fail with `workflow_store_busy`)
  and keeps it until the uninstall ends (creating the workflow store's lock
  file when no store exists yet), so a `workflow enable` or enabling install
  meanwhile fails with `workflow_store_busy` instead of recording units. Its
  message names each such plugin with the `pohunek workflow disable <id>` that
  stops it. A unit record that cannot be read refuses it with
  `workflow_store_failed`. The operator order is: `pohunek workflow disable <id>`
  for each named plugin (or `pohunek workflow uninstall <id>`), then
  `pohunek service uninstall`.
- Without a `pohunek service` installation, `disable` and `uninstall` still
  look for the plugin's units, in the namespace and unit directory a
  `pohunek service install` would record for this user and these roots, and
  remove them. They accept an installation of any version: removing a unit
  does not run the trampoline. When that service manager cannot be reached, or
  has no domain for this user (launchd over SSH), while the unit record names
  units, they fail with `workflow_units_failed` and keep the record: those
  units may be running, and marking the plugin disabled does not stop a
  running service. With no recorded units they say so in a warning instead.

All three hold `<state>/pohunek/workflow-plugins/units.lock` for their whole
run and are safe to repeat; a command that finds it held fails with
`workflow_store_busy`. The names of a plugin's units are kept in
`<state>/pohunek/workflow-plugins/units/<id>.json` from before the first unit
is installed until every one is removed; the file is deleted when none
remain. A record that cannot be read or is invalid fails the command with
`workflow_store_failed`, and never reads as "no units". A unit's name is
`pohunek-<ns>-wf-<h12>-<j|s>-<name>.service` (plus a `.timer` for a job) on
systemd and `io.github.zajca.pohunek.<ns>.wf.<h12>.<j|s>.<name>` on launchd,
where `<h12>` is the first 12 hex characters of SHA-256 over the plugin id,
so a job and a service may share a name.

### Status

`pohunek workflow status [<id>] [--json]` shows the recorded jobs and services
of the installed plugins: every plugin that has units when no id is given, or
the named plugin, with an empty `units` list when it has none. It is local and
read-only: it asks the service manager for each unit's state and reads the
plugin's run log, and it never contacts the daemon.

Each unit reports:

- `state`: `running`, `stopped`, `failed` or `unknown`, from the service
  manager. A job is `running` while its schedule is armed and `failed` while
  its last run failed; launchd never reports `failed`. The state is `unknown`,
  with a `state_reason`, when no `pohunek service` installation is recorded,
  the manager cannot be reached, the manager has no unit of that name, or it
  reports a transitional state.
- `last_run`: the newest run record of the unit of the installed archive, with
  `ts`, `outcome`, and `exit_code`, `signal` or `error_code` where recorded;
  `null` when there is none.
- `consecutive_failures`: for a job, the number of newest run records of the
  unit whose outcome is not `ok`, counted up to the first `ok`. An
  `overlap_skipped` record started no run, so it neither counts nor ends the
  count. For a service, it is the stored count of short runs in its backoff
  file minus one, the value a stable run leaves there. After a fresh start the
  first short run stores one and so reads zero; `last_run` and the run log are
  the exact history. `null` when the count cannot be determined.
- `backoff_ms` (services only): the delay the unit's next start waits, computed
  by the same function the trampoline waits with. `backoff_error` is present
  when the backoff file cannot be read or parsed; the delay then is the longest
  one, as the trampoline's own start.

The counts cover the retained run log only: the active file and its rotations,
at most `logs.run_log_files` files of `logs.run_log_file_bytes` bytes each. A
run log that cannot be read is reported as `run_log_error`; its units then have
no `last_run`, and a job has a `null` `consecutive_failures`.

### Each run

Every unit runs the hidden
`pohunek workflow __exec <id> <digest> job|service <name>`, which decides
each run the same way on both platforms:

1. A unit of a plugin that is no longer installed is refused with a message
   on stderr; there is no run log to record it in. A unit naming another
   archive than the installed one (`workflow_unit_stale`) or a disabled
   plugin is refused and recorded.
2. A non-blocking lock on `locks/<kind>-<name>.lock` in the plugin's state
   directory is taken. When an earlier run of the same unit still holds it,
   the run is recorded as `overlap_skipped` and nothing starts. The service
   managers never start an active job twice either.
3. A job's timer fires every `jobs.check_period_s` (or every `every_s` when
   that is shorter). The job keeps the wall-clock start of its last run in
   `schedule/job-<name>.json` and runs only when `every_s` has passed since
   then (within one second), so a long interval follows the wall clock
   across suspend and logins, and a due job starts at most about one check
   period late. A check that is not due exits without a record.
4. A service keeps its count of short runs in `backoff/service-<name>.json`.
   Before each run it waits `restart_initial_ms`, doubled for every further
   short run, at most `restart_max_ms`; the first start waits nothing. A run
   of at least `stable_after_ms` starts the count over. Any exit counts,
   including status 0.
5. The plugin's files are verified again, and the job or service must be one
   the installed manifest declares (`workflow_unit_unknown` otherwise). The
   run starts as an action does, with these differences: its deadline is the
   job's `timeout_ms` or `jobs.timeout_default_ms`, and a service has none;
   its stdout and stderr are discarded without a cap and only counted in the
   run record; and the context's `invocation.kind` is `job` or `service`,
   with no `output` field.

The environment is the one described under [Environment](#environment),
built from an empty one, with two differences: the session origin pair is
never passed, because a service manager's environment is not a session's;
and `POHUNEK_CLI` is the installed `<prefix>/bin/pohunek`. Whatever the
service manager adds to its own environment (`systemctl --user
set-environment`, `launchctl setenv`) and any inherited `POHUNEK_SOCKET_PATH`,
`POHUNEK_CONFIG_DIR` or `POHUNEK_ENV` never reach the job or service.

On macOS, an `XDG_RUNTIME_DIR` that `launchctl setenv` puts into the agent's
environment is ignored by `__exec` when the installation uses the default
runtime root (`/private/tmp/pohunek-<uid>`), because the units do not pin
that variable, and the job or service does not get it either, so its
`POHUNEK_CLI` reaches the same installation. Any other installation resolves the variable as set, and a
runtime root that does not match the recorded one refuses every run.

A stop request (`SIGTERM`, `SIGINT` or `SIGHUP`) to `__exec` stops the run's
process group within `limits.kill_grace_ms` and is recorded as
`interrupted`; one that arrives while a service waits its backoff ends the
wait at once. Both service managers stop a unit by sending `SIGTERM` to
`__exec` alone (systemd units use `KillMode=mixed`), so `__exec` stops the
run's process group itself and records the stop as `interrupted`. A unit's
native stop timeout is 90 s, above the longest kill grace; on systemd the
final `SIGKILL` then reaches every process still left in the unit.
A refused service waits `restart_max_ms` before it exits, so the service
manager's restart does not spin. The exit status is the run's own (124 for
`timeout`); 0 for `overlap_skipped`, a job that is not due, and a run stopped
by `SIGTERM`, so a unit its service manager stopped reads as stopped, not
failed; 128 plus the signal number for a stop by `SIGINT` or `SIGHUP`; and 1
for a refusal. launchd keeps no failed state for a unit; on systemd a stopped
job or service is `inactive`.

That `SIGKILL` case differs on macOS. A supervised child runs in its own
process group there. If `__exec` itself is killed with `SIGKILL` (not the
`SIGTERM` launchd sends to stop a unit), launchd's cleanup covers only the
trampoline's own group, so the run's process group can survive the unit. A
later start of the unit can then run alongside it, and `disable` and
`uninstall` do not find that process. Stop such a process yourself. On systemd
the unit's final `SIGKILL` reaches every process of the unit.

## Local only

Every `pohunek workflow` command works on this user's files and never contacts
a daemon; `enable`, `disable` and `uninstall` of a plugin with jobs or
services also use this user's service manager, and `status` only reads the
state of those units from it. A remote `--host` fails with
`workflow_local_only` before any file is read or written.

## Trust boundary

A workflow plugin runs with your account's full authority. The digest proves
which bytes you approved; it does not restrict what they do. Plugin code can
read your files and use every public owner API, including the CLI and the SDK,
as any same-user process can. That is the same owner trust boundary as a
third-party runtime package, not a sandbox. The minimal environment,
deadlines and caps of `workflow run` bound what the CLI forwards and how long
it waits; they do not confine the plugin. An enabled plugin's jobs and
services run without you, at every login, until you disable it. Install only plugins whose source
you trust, and review the argv the consent step prints.

Error codes: `workflow_local_only`, `workflow_archive_invalid`,
`workflow_digest_mismatch`, `workflow_descriptor_invalid`, `consent_required`,
`workflow_plugin_unknown`, `workflow_plugin_installed`,
`workflow_root_invalid`, `workflow_root_intact`, `workflow_install_uncertain`,
`workflow_store_busy`,
`workflow_purge_incomplete`, `workflow_dirs_overlap`, `workflow_store_failed`,
`workflow_config_invalid`, `workflow_plugin_disabled`,
`workflow_action_unknown`, `workflow_action_args_refused`,
`workflow_origin_incomplete`, `workflow_timeout_refused`,
`workflow_context_too_large`, `workflow_run_log_failed`,
`workflow_spawn_failed`, `workflow_run_failed`, `workflow_action_timeout`,
`workflow_output_limit`, `workflow_action_interrupted`,
`workflow_service_not_installed`, `workflow_unit_refused`,
`workflow_units_failed`, `workflow_unit_stale`, `workflow_unit_unknown`,
`workflow_installed_disabled`.
