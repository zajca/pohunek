# CLI guide

Reference for the `pohunek` command line: every command, multihost targeting,
shell completion, automation and bounded observation, notifications, the
assistant, and the Hermes Agent runtime. `pohunek agent-skill` prints the
agent-facing operating guide for the same commands.

## Commands

Every command accepts `--host <name>` (default `local`), and nearly all of
them `--json` for machine-readable output.

The commands without `--json` are `attach`, `completions`, `daemon start`, `prompt render`, `relay login`, `relay logout`, `relay resume-rotation`, `relay rotate` and `service lock`.

Session targets are `<session-id>` or
`<host>/<session-id>`. A discovered route uses
`<overlay>:<canonical-identity>@<port>`. Canonical identities are typed
`peer~<base64url>` or `fqdn~<base64url>` selectors without padding, so provider
keys containing `/`, `+`, `=`, or the route separator `@` remain safe inside
session targets and URLs. The selector is decoded and re-resolved through
current provider state before every new connection while the discovered daemon
port is retained.

A host name resolves in this order:

1. `local` or an empty name: this machine's daemon, with no overlay lookup.
2. This machine's own overlay identity, which the overlay client reports in its
   own status (for `NetBird`, the root `fqdn` and `netbirdIp`; never the OS
   hostname). It matches by exact fqdn, own overlay IP, or short name (first
   DNS label), and `<overlay>:<name>` matches on that overlay only. Such a name
   selects the local daemon exactly like `local`: the same socket, no mesh
   dial, and local behaviour in every command, so one host name works on the
   machine itself and from its peers. A short name that this machine and a
   peer both carry is `overlay_host_ambiguous`, never a silent choice; its
   message lists the matching hosts (`this machine (<fqdn>)`, `peer <fqdn>`) and
   its hint the selector that names each (`<overlay>:<selector>`, or `local` for this machine). A
   `@<port>` route and a typed `peer~` or `fqdn~` selector always name a peer.
3. A peer of a configured overlay, dialed over its route.
4. Anything else: `host_unknown`. When the overlay status cannot be read, steps
   2 and 3 are skipped and the error is the one the connection reports.

Only the hosts a command selects are resolved: each target's host, a
positional host such as `host inspect <host>`, and `--host` where the command
reads it. A `--host` that the command ignores, or that an explicit
`<host>/<session-id>` overrides, is never looked up, so it cannot fail the
command.

`pohunek host list` and `host discover` show this machine first, named by its
own overlay name, with route `local`. In `--json` it is one record per overlay
that reports the machine's identity, marked `"local": true`, with `peer_id`
`null`; peer records carry no `local` key. Its classification comes from the
same health probe the peers get, sent to the machine's own overlay address and
daemon port: a daemon that does not answer there reads `unreachable` with the
probe error in the record's `reason` (also logged at WARN by the daemon, and shown
in the VERSION column as `probe failed: <reason>`), and an overlay that reports no address leaves a `candidate`.
The status column shows `local` for a reachable daemon and `local:<status>`
otherwise. When the overlay does not report the machine's identity within the 5 s
connect timeout, no local record is listed and the peers still print. `--all-hosts`
fan-out skips the local record, because the explicit `local` target already
covers this machine.

A remote host is resolved once per operation when the operation opens more than
one connection: `attach` (including its automatic re-attach, the live status
line and the menu actions), `session output`, `session wait` and `assistant`.
Every connection of the operation then looks the peer up by its stable identity
(`<overlay>:peer~<id>@<port>`, or `fqdn~` when the provider reports no peer id),
so a short name that starts selecting another peer midway is never followed; the
address may still change. A peer that reports neither a peer id nor an FQDN
cannot be pinned: the operation fails before its first connection with
`host_identity_unpinnable`. Errors and output keep the name you typed.

| Command | What it does |
|---|---|
| `pohunek doctor` | Environment health: binaries, socket, state dirs, NetBird, agents. |
| `pohunek service install [--from <dir>] [--prefix <dir>] [--json]` | Install the daemon as a native login service (systemd user unit or launchd agent) from staged binaries into a versioned `libexec` directory and write `service.toml`. |
| `pohunek service upgrade [--from <dir>] [--accept-runtime-loss] [--preflight-budget <SECONDS>] [--json]` | Switch to the staged version, restarting only the daemon; live workers keep their PID and PTY, and unreferenced old versions are removed. First runs a read-only preflight with the new binaries and refuses (`service_upgrade_sessions_at_risk`) while a live or stopped session would lose recovery or not be adopted, counting live and stopped risks separately and labelling each row with its kind and reason code; `--accept-runtime-loss` accepts that loss. Accepting records each verdict the daemon binds to an acceptance scope in `service-accepted-risks.json` once the upgrade completes, so the identical risk (same session, list, verdict, code and scope) is reported as previously accepted and no longer blocks the next upgrade; it changes no session, and a new session, a changed scope or a verdict without a scope still needs the flag. An owner-requested stop or removal is not itself a runtime-adoption loss, even while the runtime is alive; retained native recovery of a stop is still judged and identity checks remain conservative. A store the new daemon would refuse is never overridable (`service_upgrade_store_unusable`), nor is a preflight that cannot run (`service_upgrade_preflight_failed`), nor a service manager (systemd user manager or launchd) whose `PATH` cannot be read (`service_manager_environment_unavailable`). A service manager whose variables cannot be read for a job that records its `PATH` refuses an upgrade whose preflight judged a stopped session (`service_manager_variables_unread`) unless `--accept-runtime-loss` is given. The official runtime step adds five codes, none overridden by `--accept-runtime-loss`: `service_upgrade_runtimes_refused` (the release's plan cannot be applied; nothing was stopped), `service_upgrade_runtimes_failed` (the release's runtime step did not produce a finished report), `service_runtime_backup_invalid` (a registry, catalog state or host profile cannot be backed up exactly), `service_staged_runtime_invalid` (the release's `runtime/` tree is not the layout the release assembles, or a sealed release lists a `runtime/` file it does not carry), and `service_upgrade_runtimes_unconfirmed` (the step's daemon run could not be ended and its exit confirmed, so the upgrade is kept and not rolled back). Cancelling an upgrade at or after daemon registration checks whether the previous release can read the current store before changing the service, stops the new daemon, then checks again before starting the previous one. A refused rollback keeps its transaction (`service_rollback_store_unusable`, `service_rollback_sessions_at_risk`, or `service_rollback_preflight_failed`); store refusal is never overridable, while `--accept-runtime-loss` can accept unverified live or stopped-session recovery. The runtime plan repins host profiles pinned to an older version of the release's official package (`repin`, applied as `repinned`), lists the pinned profiles it keeps with a reason (`kept`), and says when it re-selects the release's package over a newer selected version. A stopped session that a newer official package version adopts is judged `adoptable_by_newer_package`. `service check` previews the refusal. `--json` carries the rollback preflight and `accepted_runtime_loss` in `rolled_back` (and at the top level when rollback is the only operation). |
| `pohunek service uninstall [--stop-sessions] [--purge] [--json]` | Remove the service; refuses while sessions are live unless `--stop-sessions`, and keeps durable metadata unless `--purge`. With `--purge`, recognized store-write temps and imported legacy archives are removed too; unrecognized data entries are retained and reported as byte-escaped display strings in `kept_purge_entries`. It also refuses (`workflow_units_installed`) while a workflow plugin still has jobs or services recorded or the reaction dispatcher unit is recorded: disable the plugins it names first. |
| `pohunek service status [--json]` | Daemon job, namespace, installed versions, worker jobs per generation, and any interrupted or running install transaction. |
| `pohunek service check [--prefix <dir>] [--accept-runtime-loss] [--preflight-budget <SECONDS>] [--json]` | Run every check the install (or, for an existing service, the upgrade) of this version makes before its first effect — `HOME` and XDG roots, the prefix, every directory it writes, a pending transaction, the recorded installation, and for an upgrade the live and stopped-session preflight, with earlier acceptances grouped as previously accepted — without changing anything, the ledger included; fails with the error code that command would. |
| `pohunek service lock -- <command> [args...]` | Run a command while holding the service transaction lock; `pohunek service` commands it runs reuse the lock with the holder token in `POHUNEK_SERVICE_LOCK_TOKEN`, every other one is refused until it exits; the lock ends with this process, never with a process the command left behind. Exits with the command's status. If execution fails, the error also reports terminal ownership before/after recovery and the raw errno of each attempted recovery operation. |
| `pohunek plugin list / inspect / doctor` | List installed runtime packages, show one (record, health, declared runtime), or verify every package root; `doctor` exits 1 when findings exist. Local daemon only; `--host` is rejected. |
| `pohunek plugin install <archive> (--sha256 <digest> \| --catalog <file>)` | Validate a package archive with a dry run and print what it declares (program, arguments, resume/fork support); installs only after repeating the command with `--yes`. A third-party archive is trusted solely by the digest you supply; `--catalog` authorizes official packages. `--no-enable` installs it disabled. Install and update wait up to 30 s for one daemon response (a slower host or a large archive can exceed the 5 s default other commands use); the budget is the CLI's tolerance for a slower install, not a daemon install deadline, and a `request_timeout` leaves the outcome unknown, so check `plugin list` before repeating the command — there is no retry. |
| `pohunek plugin link <dir>` | Copy a developer package directory into storage; installed disabled and unselected, with the same `--yes` consent. |
| `pohunek plugin update <package> <archive> (--sha256 <digest> \| --catalog <file>)` | Install a new version of an installed package next to the old one, enabled and selected; the old digest stays installed for `select` rollback. Needs `--yes`. An official archive (by its catalog, or a digest the registry records as official) whose version is lower than the selected official version is refused before anything is written (`plugin_update_downgrade`, naming both versions). Versions compare by semantic version precedence, ignoring build metadata. A version that is not a semantic version cannot be ordered and is refused too (`plugin_update_version_unordered`). Roll back on purpose with `plugin install` (another version stays selected), then `plugin select`. The guard is in the CLI only: `package.install` and `package.select` (SDK, daemon) keep selecting what they are told to. `update` waits like `install`: up to 30 s for one daemon response — the CLI's tolerance for a slower install, not a daemon install deadline. |
| `pohunek plugin select / enable / disable <package> [--digest <d>] [--version <v>]` | Pick the version bare requests resolve to, or allow or block fresh launches (live sessions keep running). An ambiguous selection is an error, never a guess. |
| `pohunek plugin uninstall <package> [--digest <d>] [--version <v>] [--remove-modified] --yes` | Remove an installed package no session or profile pins; `--remove-modified` is only for a root that fails verification. |
| `pohunek plugin profile list [--json]` | List host agent profiles (`agents/*.toml`) with base runtime, pinned package and digest, and a state: `builtin` (the base is the built-in `shell`), `pinned`, `needs_migration` (an installed package serves the base but the profile pins none), `package_not_installed` (the base is not built in and no installed package serves it, so the profile cannot launch; the detail names the package install, the official one for `codex`, `claude` and `hermes`, then the migration), `package_unavailable` (the pinned digest is installed but cannot be loaded, or no installed package serves the base but one that may serve it cannot be loaded; the detail names `pohunek plugin doctor`, which reports the fault and its repair, and for an unpinned profile the migration after it), `pin_not_installed`, or `unreadable`. Reports parse errors by line only; never prints file content. |
| `pohunek plugin profile migrate <name> [--digest <d>] [--yes]` | Pin a profile to an installed package; the daemon rewrites only its `package` and `digest` keys (every other byte, including `[env]`, is preserved) under the package lifecycle authority with an atomic rename, so the profile never goes missing. Without `--digest` it uses the selected, enabled package serving the base runtime. Previews first and needs `--yes`; a profile never changes digest through `plugin update` or `select`. `service upgrade` repins a profile pinned to an older version of the release's official package and keeps every other pin. |
| `pohunek workflow install <archive> --sha256 <digest> [--no-enable] [--yes]` | Verify a workflow plugin archive against the digest you supply, validate its `workflow.toml`, and print its id, version, digest and every action's, job's, service's and reaction's argv with resolved absolute paths; installs into this user's workflow store only after repeating the command with `--yes` (`consent_required` until then). A plugin with jobs, services or reactions is enabled like `workflow enable` does; without a `pohunek service` installation of this pohunek version an enabling install fails with `workflow_service_not_installed` before any effect, every refusal of an enabling install (including `workflow_store_busy` while another unit command holds `units.lock`) comes before the plugin is recorded, and when reaching the service manager, marking the plugin enabled (for example `workflow_store_busy` while another workflow command holds the store lock) or installing a unit fails afterwards, the command fails with `workflow_installed_disabled` (the failure's code as `cause`): the plugin stays installed disabled with no units (fix the cause, run `workflow enable`), or, when undoing the failed step failed too (a unit install, or clearing a flag that may have been written), the message says it may remain enabled or keep units (run `workflow disable`, then `workflow enable`). Installing a recorded archive again never touches its units. Never contacts a daemon; see the [workflow plugins guide](knowledge/guides/workflow-plugins.md). |
| `pohunek workflow list / inspect <id>` | List installed workflow plugins, or show one plugin's actions, jobs, services, reactions, files, state and config directories after re-verifying its files (`workflow_root_invalid` when they changed). |
| `pohunek workflow status [<id>] [--json]` | Show each recorded job and service of the installed plugins (every plugin with units, or the named one): its native state from the service manager (`running`, `stopped`, `failed`, or `unknown` with the reason), its last run record, its consecutive failures and, for a service, the restart delay its next start waits. Local and read-only; see the [workflow plugins guide](knowledge/guides/workflow-plugins.md#status). |
| `pohunek workflow enable / disable <id>` | Allow or block the plugin's actions. For a plugin with jobs or services, `enable` also installs, enables for login and starts one user service manager unit per job and service (systemd user units or launchd agents of the `pohunek service` installation, which it requires and which must record this pohunek version: `workflow_service_not_installed`), after refusing a job outside the `[jobs]` knobs (`workflow_unit_refused`); a unit that cannot be installed removes every unit and disables the plugin again (`workflow_units_failed`). `disable` marks the plugin disabled first, then stops and removes its units, also when the installed files fail verification, since their names are kept in the plugin's unit record. Both hold `units.lock` and are safe to repeat. Each unit runs the hidden `pohunek workflow __exec`, which refuses residue units (`workflow_unit_stale`, `workflow_unit_unknown`), skips a run while the previous one holds its lock (`overlap_skipped`), runs a job only once its wall-clock `every_s` has passed, and applies a service's restart backoff; see the [workflow plugins guide](knowledge/guides/workflow-plugins.md#jobs-and-services). |
| `pohunek workflow uninstall <id> [--purge] [--remove-modified]` | After the uninstall's own refusals (which change nothing), mark the plugin disabled, stop and remove its job and service units (also those of a plugin whose files fail verification) (a unit that cannot be removed fails the uninstall with `workflow_units_failed`, leaving the plugin installed disabled), then remove the plugin; its state and config directories are kept unless `--purge`, which also removes the directories an earlier uninstall kept and reports each directory's outcome (`workflow_purge_incomplete` names where remaining data is). `--remove-modified` is only for installed files that fail verification. Every changing `workflow` command holds the store lock (`workflow_store_busy` while another runs), and every `workflow` command is local-only: a remote `--host` fails with `workflow_local_only` before any I/O. |
| `pohunek workflow run [--json] <id> <action> [<args>...]` | Run one declared action of an installed, enabled plugin after re-verifying its files: no shell, the plugin's `files/` directory as cwd, stdin from `/dev/null`, its own process group, and an environment of exactly the plugin variables, `POHUNEK_CLI`, the set `HOME`/XDG roots and the complete `POHUNEK_SESSION_ID`/`POHUNEK_DAEMON_ID` pair (one marker alone refuses the run with `workflow_origin_incomplete`). Every argument after `<action>` (including `--json`, `--`, `-h` and empty strings) goes verbatim to an action declaring `args = "passthrough"`, so pohunek's own `--json` and `--host` go before `<id>`. Stdout and stderr stream here up to their caps; with `--json` stdout is captured instead and printed as `ok` byte for byte when the action exits 0 with exactly one JSON document, otherwise one error envelope with no output bytes: `workflow_action_failed` (with `exit_code`, and `signal` when a signal ended the action) for a failed action, `workflow_output_invalid` or `workflow_output_incomplete` (exit 1) for unusable stdout. Exits with the action's status, 124 at the deadline (`workflow_action_timeout`), 125 over an output cap (`workflow_output_limit`), or 128 plus the signal number when the CLI was signalled (`workflow_action_interrupted`); the group is stopped with `SIGTERM`, a grace, then `SIGKILL`. Limits come from `<config>/pohunek/workflow.toml` (`workflow_config_invalid` when it is unsafe or invalid), and every run of an installed plugin, refused or not, appends a content-free record to the plugin's `logs/runs.jsonl` (except runs that fail before the run log is opened, such as invalid knobs or an unknown plugin). |
| `pohunek workflow` event reactions | A plugin's `[reactions.<name>]` tables run for daemon events through one dispatcher unit that the CLI installs while an enabled plugin declares reactions; `workflow enable`, `disable` and `uninstall` keep it in step. The hidden `__dispatch` entry point is not typed by hand. See the guide's [Event reactions](knowledge/guides/workflow-plugins.md#event-reactions). |
| `pohunek daemon start [--detach] [--dev-subprocess]` | Run the installed daemon by hand (needs `service.toml`), or with `--dev-subprocess` a development daemon with plain subprocess workers. |
| `pohunek health [--json]` | Daemon liveness, build, and protocol version. |
| `pohunek status [--json]` | Alias for `health`. |
| `pohunek subscribe [--json]` | Stream daemon events as newline-delimited JSON. The stream is JSON with or without `--json`; the flag keeps error rendering in JSON. |
| `pohunek session new` | Start a session: `--agent`, `--name`, `--project`/`--repo`, `--branch`, `--base-branch`, `--cwd`, `--input`, `--request-timeout-ms`, `--meta k=v`. An existing local `--branch` is checked out as it is; an explicit `--base-branch` it ignores at another commit is reported as a `base_branch_fallback` warning ([Worktrees](knowledge/concepts/worktrees.md)). A `request_timeout` leaves the outcome unknown ([CLI Process API](public-api.md#cli-process-api)). |
| `pohunek session list` | List sessions, including a `running/recent` subagent count and a `BACKGROUND` column of compact per-kind counts of the latest background snapshot (`a` subagent, `s` shell, `m` monitor, `w` workflow, `t` teammate, `o` every other kind; zero kinds omitted; `-` when no snapshot has been reported, `0` when the latest snapshot lists no task). Example row: `2a 2s 1m`. Repeat `--filter state=running`, `--meta-eq key=value`, and `--meta-has key` to combine predicates with AND. Metadata values split at the first `=` and may be empty; keys are opaque. `-q` prints ids only. |
| `pohunek session inspect <target>` | Full logical session record: agent state, current/recent subagents, runtime state and generation, cwd, project, branch, worktree, recovery binding. Adds `background_items` (`<none>` without a snapshot, `0` for an empty one) and `background_observed` (age of the snapshot, `<none>` without one), then one line per subagent with its background label and age, and one `background [<kind>]: <status> <program> <label> first seen >= <age>` line per other background item. An absent program or label prints `-`. Labels and programs are escaped for the terminal. |
| `pohunek attach <target>` | Attach the current terminal; `Ctrl-]` detaches. |
| `pohunek session input <target> <text>` | Inject a prompt with agent-correct framing; use `--stdin` for non-argv input. `--until <activity>` (repeatable) and `--timeout <ms>` wait for the agent's reaction in the same call, for zero-delay profiles only. |
| `pohunek session screen <target>` | Read the current rendered terminal; `--json` preserves runtime identity, watermark, geometry, cursor, and visible lines. |
| `pohunek session read <target> [--source <source>] [--lines <n>] [--format <format>] [--json]` | Read a bounded terminal capture; the source defaults to the current visible screen. |
| `pohunek session resize <target> --cols <n> --rows <n> [--json]` | Resize a managed session terminal. |
| `pohunek session metadata <target> [--set k=v] [--clear k] [--json]` | Merge or clear owner-controlled session metadata (both flags repeat). |
| `pohunek session runtime-inventory [--json]` | List the durable worker runtime endpoints of the host. |
| `pohunek session policy get [--json]` | Show the session retention policy. |
| `pohunek session policy set [--enabled \| --disabled] [--sweep-interval-secs <n>] [--terminal-ttl-secs <n>] [--lost-ttl-secs <n>] [--max-removals-per-sweep <n>] [--json]` | Update the given retention policy fields; unspecified fields keep their value, and the daemon applies the policy on its next sweep. |
| `pohunek session retention sweep [--dry-run \| --apply] [--limit <n>] [--json]` | Run one retention sweep now with the current policy TTLs: `--dry-run` reports the selected sessions, `--apply` removes them with their owned worktrees, never beyond the policy's per-sweep cap. |
| `pohunek session detection <target>` | Preview the active detection manifest regions; `--json` also lists every supported region kind. |
| `pohunek session output <target>` | Read a newest retained tail or continue with `--worker-instance-id` (the session's `worker_instance_id`), `--runtime-generation`, and `--after-offset`; `--wait-ms` performs a bounded wait. |
| `pohunek session wait <target>...` | Wait for the next turn event (`turn_completed`, `agent_blocked`, `approval_required`, `exited`) of one or more sessions on one host: `--until`, `--after <cursor>`, `--timeout <duration>`; this is the mode with no flags. Any legacy flag (`--state`, `--activity`, the `--after-*` cursors, the runtime identity, `--timeout-ms`) selects a single-target long-poll of up to 8000 ms for explicit predicates. |
| `pohunek session fork <target>` | Fork an agent conversation into a new session when that session advertises fork capability (Claude Code, and runtime packages that declare a fork such as `pi`). |
| `pohunek session resume <target>` | Relaunch a stopped or lost session from its captured native reference. A session launched from a host profile relaunches only while the profile still has the revision frozen into the session; an edited or legacy profile fails with `agent_profile_changed` and a deleted one with `agent_profile_missing`. An adopted built-in session resumes or forks without the flag when the profile differs from the frozen one only by its `package` and `digest` keys, the pin `plugin profile migrate` writes. `--accept-profile-change` relaunches under the current profile and freezes its revision (also on `session fork`); the local daemon honors it, a remote host refuses it with `agent_profile_change_local_only`. A resume of a lost runtime proves the previous runtime ended first (job retirement and a marker sweep), so the CLI waits for the daemon's own result instead of the 5 s default and returns as soon as the daemon answers; after 5 s of waiting, human output prints a notice on stderr. The deadline is a bound, not a delay: it is the longest the daemon's validated settings allow, 110 minutes at the maximum 10 minutes per step and about 2 minutes with the installed defaults (30 s job exit, 5 s sweep grace, 10 s worker connect, 45 s worker initialization). A refusal for unreadable-marker processes lists them. If the deadline still elapses, the error is `session_resume_outcome_unknown`, not `request_timeout`: the resume may still complete, so run `session inspect` before retrying. |
| `pohunek session diff <target> [--base <ref>]` | Unified diff of the session's worktree vs its base. |
| `pohunek session rename / stop / rm` | Rename, stop, or evict a session. |
| `pohunek session rm <target> --accept-unconfirmed-cleanup` | Evict a session that `rm` refused with `runtime_supervision_ambiguous` only because same-user processes with unreadable environments may belong to its runtime (the refusal lists them). On macOS 27 the kernel withholds the environment of Apple platform binaries; the sweep reaps those the lost worker created (a `/bin/zsh` session root and its helpers) without this flag when the worker journaled its creation number in the current boot, and dismisses those created before the worker. When the worker also journaled its kernel coalitions (`worker_coalitions`, same boot), it dismisses every hidden process the kernel puts in other coalitions than the worker's, such as a launchd job, helper or another session's agent started after the worker. An orphan that re-executed after its creator died and a process whose creator exited after the worker started remain candidates once they have outlived the sweep grace (`sweep.grace_ms`, during which the sweep keeps re-reading the table for a process that is only passing; one removal waits at most one grace in total across all its runtimes, and on Linux a candidate that has already run longer than the grace and whose environment stays permission-denied is not waited for, while one whose environment reads as unobservable, such as a process replacing its executable, is waited for the whole grace; a removal left pending by such candidates is retried without rescanning until one of them exits), so the flag may still be needed after a lost runtime; the refusal lists each by pid and command name for you to judge. The same holds for every hidden process when the worker journaled no creation number or coalitions (a worker started by the previous release). Those candidates are never signalled and, if one carries the runtime marker, keep running unsupervised after the worktree, logs, and record are deleted. The consent covers one call, is never automatic, and without it the removal stays refused; inspect and end the listed processes, then retry, as the safe path. The result lists the accepted processes; a daemon without the method answers `method_not_found`. |
| `pohunek project add / list / show / rename / rm` | Manage git-repo-aware project records. |
| `pohunek project actions / action / prompt` | Resolve per-project launch recipes and prompt templates. |
| `pohunek host discover / list / inspect` | Find NetBird peers running daemons (standalone cache; `--refresh`) and query live capabilities. |
| `pohunek host governance inspect <host>` | Read the safe stable host identity and local governance state; `--json` preserves explicit absence and canonical revisions. It does not mutate enrollment or ownership. |
| `pohunek completions <bash\|zsh\|fish>` | Print static shell completion; add `--dynamic` for bounded host/session, package and workflow plugin candidates. |
| `pohunek notifications list / watch` | Inspect or stream the durable inbox; `--all-hosts` fans out. |
| `pohunek notifications read / ack / archive / delete` | Drive one record's lifecycle (`host/id` targets a specific host). |
| `pohunek notifications policy get [--all-hosts] [--json]` | Show the notification policy. |
| `pohunek notifications policy / retention` | Per-kind policy for the default flags or any runtime id (`--provider default\|<runtime-id>`; `default` is always the base flags, even if a runtime is named `default`), retention pruning (`--dry-run` / `--apply`). |
| `pohunek integration install` | Install Codex/Claude hooks into the runtime's own config home, one host profile's home (`--profile NAME`) or every distinct home (`--all-profiles`), or install a selected Hermes profile's managed plugin from the installed Hermes package with explicit access mode and host allowlist. |
| `pohunek integration status` | Inspect daemon-managed Codex/Claude hooks on the effective `--host` (`--profile` and `--all-profiles` select profile config homes and are local-only), or one explicitly selected local Hermes target. |
| `pohunek integration doctor / uninstall` | Diagnose or remove daemon-managed Codex/Claude hooks (`doctor` follows `--host`; `uninstall` targets the local daemon; both accept `--profile NAME` and `--all-profiles`, local-only), or, with `--agent hermes`, one explicitly selected local Hermes plugin target. |
| `pohunek integration update --agent hermes` | Atomically refresh one explicitly selected local Hermes plugin target. |
| `pohunek setup [--yes \| --dry-run] [--json]` | The one host setup: installs the service from this release, applies the official runtime packages, profile pins and hooks for the runtimes this host has, and writes the missing config files. It previews every change and asks once; `--yes` applies without asking, and without a terminal or with `--json` it needs `--yes`. `--dry-run` prints the plan and changes nothing. It never upgrades an installed service. |
| `pohunek setup config [--force] [--json]` | Install the default `attach.conf` and prompt templates; existing files are kept unless `--force`. |
| `pohunek setup completions <bash\|zsh\|fish>` | Install completion in the shell's conventional user directory; add `--dynamic` to opt in to runtime candidates. |
| `pohunek assistant project / update [request…]` | Steer the assistant toward project configuration or toward reconciling an update; the flags are those of `pohunek assistant`. |
| `pohunek assistant [intent] [request…]` | Launch the self-help assistant with knowledge bundle + live snapshot; `--request-stdin` reads the request from stdin, `--request-timeout-ms` and `--meta k=v` as on `session new`. |
| `pohunek agent-skill` | Print the complete bundled agent skill; `--json` wraps the skill text and its `content_sha256` in the process envelope. Fully local — `--host` is accepted and ignored. |
| `pohunek migration preflight [--accept-runtime-loss] [--socket <path>] [--json]` | Snapshot legacy sessions and refuse replacement while live PTYs exist; the daemon installer runs it at the one-time daemon-owned to worker-owned PTY boundary, with `--socket` naming the legacy socket moved aside. `--accept-runtime-loss` records consent to import live legacy sessions as runtime-lost. |
| `pohunek relay login --origin <origin> [--ca-file <file>]` | Sign in to an optional team relay with the OIDC device flow and store the credential in the OS keyring. `--origin` is the exact HTTPS origin; `--ca-file` adds a public CA certificate for a privately operated relay. |
| `pohunek relay status --origin <origin> [--ca-file <file>] [--json]` | Show the account that the stored credential of that origin authenticates. |
| `pohunek relay logout --origin <origin> [--ca-file <file>]` | Revoke the stored credential, then delete it from the keyring. |
| `pohunek relay rotate --origin <origin> [--ca-file <file>] [--expires-in-seconds <n>] [--overlap-seconds <n>]` | Replace the stored credential with explicit expiry and overlap limits. |
| `pohunek relay resume-rotation --origin <origin> [--ca-file <file>]` | Reconcile an interrupted rotation using its original policy and request. |
| `pohunek prompt render / link` | Render provider prompt templates and work-item link metadata (called by external launchers). |

`--preflight-budget <SECONDS>` (1 to 300, default 20) sets the probe and reference-check budget of this release's upgrade preflight only. A rollback judges with the previous release's daemon, which does not take the flag, so the rollback's preflight always uses the default budget.

### Working across hosts

```bash
pohunek host discover                          # which NetBird peers run a daemon?
pohunek host inspect buildbox --json           # agents/worktree capabilities, live

pohunek session new --host buildbox --project myapp --agent codex \
  --branch feat/parser --input "Fix the parser fuzz failures."

pohunek session list --host buildbox
pohunek attach buildbox/s-01J00000000000000000000000 # raw PTY over the mesh

pohunek notifications watch --all-hosts        # one triage stream for every machine
```

Remote session starts ask for confirmation (skip with `--yes`); project
references resolve on the *target* host, so no filesystem path ever crosses
the wire.

The human `host inspect` table adds `config_home_id=...` to every runtime that
declares a config home. Entries that show the same value launch against the same
config directory (one account); the value is an opaque keyed digest and never a
path.

### Host setup

Bare `pohunek setup` is the command to run on a host once, and again after an
upgrade of the release files. It runs in this order:

1. Service: without an installed service it installs the release binaries next
   to the running `pohunek`. When those binaries are an extracted release
   archive (a `MANIFEST` or a `packaging/` directory beside them), setup
   first verifies the archive with its own `packaging/verify-archive`, as
   `packaging/install-daemon.sh` does, and stops before any change if the
   archive is refused or incomplete; nothing from the archive runs before
   that. The check covers the archive's integrity and structure against its
   `MANIFEST`; whether the archive is an authentic release is what
   `gh attestation verify` of the download establishes. The install itself applies the runtime packages
   and profile pins that the host's profiles, sessions and integrations need;
   it reports them. An installed service of the same version is kept.
   An installed service of another version is a gap: run
   `pohunek service upgrade` first, because setup never upgrades.
2. Plan: the release's planner reads the runtime catalog and reports every
   runtime whose package program is on the service `PATH`, with the gaps that
   remain (the agent is not on the `PATH`, its config home is missing, or the
   platform has no package). The plan only reads.
3. Preview and consent: every effect and gap is printed before anything is
   written, including the catalog trust state (`record` means the install or
   the apply records it; on an installed service with no official package to
   install, nothing records it and the preview says so, to be recorded with the
   next official package install or `pohunek service upgrade`) and every Hermes
   home the apply acts on. The owner
   confirms once. A plan the planner refuses stops the run before its first
   change. An interrupted `pohunek service install` or `upgrade` that is still
   pending stops the run before any change too; the stop names the command that
   resumes it, or `pohunek service uninstall` to roll it back.
4. Apply: after a fresh install, setup reads the plan again and checks that it
   matches the preview. It allows only the transitions the install engine
   reports: a previewed package the engine installed is now present, and a
   previewed profile the engine pinned has left the plan. Setup then installs
   the remaining packages, enables and selects a package that the install left
   disabled or unselected, pins the remaining profiles, and installs the hooks in
   the homes that exist, except a hook the daemon already reports as current. A runtime already served by another package is left
   untouched.

   The Hermes homes applied are exactly the homes the preview listed. A home
   with the pohunek marker is updated, unless the preview lists it as
   `current`: its plugin already is the selected package's, unmodified and
   enabled, the `hermes` is a release the selected package supports, and the run selects no other
   Hermes package, so it is kept
   without a write. Setup runs the `hermes` executable on the daemon job's
   `PATH`, the one the plan detects Hermes with. A home without the marker
   is a gap: setup
   does not choose a Hermes access policy for you, so it names the Hermes
   install command for such a home (`pohunek integration install --agent
   hermes` with the home and an explicit access policy) and writes
   nothing into it. A home that appears after the preview is not touched; a
   listed home that has gone is reported. A profiles directory that cannot be
   read is reported as a failure. A gap does not fail the run.
5. Config: the missing config files are written and existing files are kept.
6. Verdict: the plan is read again. Every runtime the preview planned must be
   present in it, it must carry no refusal, and it must not list a runtime the
   preview did not show. Every profile pin the preview showed must be done. No
   hook may have failed. Then the daemon's health and `doctor` run. Hook gaps
   are listed, not failed.

If the plan read after the service install differs from the preview in any way
the install engine does not account for, setup stops there. The output names the
installed version, the prefix and the changes the install made; setup itself
changed nothing else. Fix the cause the output names and run `pohunek setup`
again: it plans from the installed service and converges without reinstalling
it.

On a host without the service, a gap that the install's own runtime step would
refuse stops setup before any change: a runtime a profile, session or
integration uses that has no package for this platform. Remove or repin that
profile, then run `pohunek setup` again. On a host with the service, setup reads
the plan again after you confirm; if the host changed while you read the
preview, for example a profile was pinned meanwhile, it stops before any change
and asks for a new run.

The run is refused when it cannot ask: without a terminal and without `--yes`,
or with `--json` and no `--yes`. The refusal changes nothing, and its hint
names `pohunek setup --dry-run --json` to read the plan and `--yes` to apply
it.

`--dry-run` reads the plan and prints it, then exits: in a terminal as the
preview a run shows before it asks, and with `--json` as one document whose
`ok` is that preview, the same object a `--json --yes` run carries in
`preview`. It asks nothing and changes nothing: no service, package, profile,
hook or config file, and it takes no lock a run would wait for. Like a run's
plan, it runs the agents' read-only queries: the version probes of the
release daemon's detection and, for a marked Hermes home, `hermes --version`
and `hermes plugins list --json`, which tell whether it is a Hermes release the
selected package supports and whether the plugin is enabled. A plan that
blocks the run, such as an installed service of another version or a release
archive that fails its verification, is the same error the run returns.
`--dry-run` with `--yes`, or with a setup subcommand, is a usage error.

`--json --yes` applies the plan without asking and prints one result document.
It carries the full preview in `preview` (service, packages, profiles,
integrations, gaps, catalog, every Hermes home and the config files), then the
`applied` lines, the hook outcomes, the config result and the verdict. It
prints no separate preview, because it cannot ask: to read the plan first, run
`pohunek setup --dry-run --json`.

A failure after the first change prints an error instead of a result, and its
message lists the changes made (`Changes made: ...`); `--json` carries the same
text in `msg`. The finished steps are kept, including the config files created
so far. Run `pohunek setup` again to complete them. `--host` is ignored.

### Shell completion

Print a static script for manual loading, or install it in the shell's
conventional per-user directory:

```bash
pohunek completions bash > pohunek.bash
pohunek setup completions zsh
pohunek setup completions fish --dynamic
```

Static completion performs no I/O beyond script generation. Dynamic completion
is opt-in: it reads the existing owner-private host-discovery cache and makes a
live, deadline-bounded `session.list` call for session targets. Every remote
candidate has a provider-qualified `<overlay>:<address>` form; a short host name
is offered only when it identifies one reachable route. A qualified `host/id`
target overrides `--host`; otherwise an explicit `--host` selects the session
source and the default is local. Missing daemons, unavailable overlays, name
collisions, and timeouts produce no shell diagnostics or unsafe fallback
candidates. The setup command does not edit shell startup files; for Zsh it
prints the `fpath` step required before `compinit`.

Dynamic completion also completes workflow plugins from the local store,
without the daemon: installed plugin ids for `workflow inspect`, `enable`,
`disable`, `uninstall` and `run` (`run` offers only enabled plugins), and for
`workflow run <id>` the actions that plugin's installed `workflow.toml`
declares. Actions come only from a plugin whose installed files verify, as
`run` requires. These lookups read the store's registry and, for actions,
verify the selected plugin's installed files (the same check `run` makes)
within the same deadline; they create, lock and write nothing, and a
missing, corrupt or unsafe store completes nothing silently. A line with a
remote `--host` completes no workflow plugin, since the command refuses it with
`workflow_local_only`. Static scripts never read the store.

### Automation and bounded observation

Use stdin for prompts that must not appear in the process list. The creation
forms `--input` and `--input-stdin` (alias `--stdin`) are mutually exclusive;
`session input` likewise accepts either positional text or `--stdin`:

```bash
printf '%s' 'Review the failing test and propose a fix.' \
  | pohunek session new --agent codex --input-stdin --json

printf '%s' 'Run the focused tests.' \
  | pohunek session input s-01J00000000000000000000000 --stdin --json
```

Stdin holds the input, so a remote `session new --input-stdin` cannot prompt for
confirmation. Without `--yes` it fails with `confirmation_required` before it
reads stdin or contacts the daemon, as `pohunek assistant --request-stdin`
does.

`session input` can also wait for the agent's reaction in the same call:
`--until` names the activities that end the wait (repeatable, default `idle`
and `blocked`) and `--timeout` bounds it to `1..8000` ms (default 8000). The
whole wait contract is validated before any text is delivered. The waited form
works only for agent profiles whose submit framing has no delay, such as
`shell`: Codex, Claude Code, and Hermes submit with a delay and fail with
`session_input_wait_unsupported` before any bytes are written. For them, send
the input and then wait with `session wait`:

```bash
pohunek session input s-01J00000000000000000000000 'make test' \
  --until idle --timeout 5000 --json          # a zero-delay shell profile

pohunek session input s-01J00000000000000000000000 'Continue.' --json
pohunek session wait s-01J00000000000000000000000 \
  --activity idle --activity blocked --timeout-ms 8000 --json
```

A waited input refuses a blocked agent with `session_agent_blocked`: an approval
is pending and belongs to the operator. `session_input_timeout` means delivery
or the target activity did not arrive before the deadline; the text may already
have been delivered, so inspect the session instead of resending it.
`session_input_stalled` means the agent stopped reading its terminal: the PTY
accepted no byte for `input.write_stall_timeout_ms` (service.toml, 3000 ms by
default), and part of the text may already have reached the agent. See
[sessions](knowledge/concepts/sessions.md) for the full wait contract.

Every `--json` success is one document shaped as
`{cli_version, protocol: {minimum, maximum}, ok}`; failures use the same prefix
with `err` instead of `ok` and exit non-zero. Human diagnostics remain on
stderr. Long counters such as `runtime_generation`, offsets, and watermarks are
decimal JSON strings.

Start observation with a screen or newest output tail, then carry the returned
runtime identity and cursor into later calls:

```bash
pohunek session screen s-01J00000000000000000000000 --json
pohunek session detection s-01J00000000000000000000000 --json
pohunek session output s-01J00000000000000000000000 --max-bytes 65536 --json
pohunek session output s-01J00000000000000000000000 \
  --worker-instance-id runtime-1 --runtime-generation 3 --after-offset 4096 \
  --max-bytes 65536 --wait-ms 5000 --json
pohunek session wait s-01J00000000000000000000000 \
  --worker-instance-id runtime-1 --runtime-generation 3 --after-output-offset 4096 \
  --timeout-ms 8000 --json
```

Detection manifests support `osc_title`, `osc_progress`, `whole_recent`,
`bottom_lines(N)`, `bottom_non_empty_lines(N)`, `top_non_empty_lines(N)`,
`last_non_empty_above_prompt_box`, `after_last_prompt_marker`,
`prompt_box_body`, and `after_last_horizontal_rule`. The detection diagnostic
shows the current matcher text for only the active manifest's required regions;
screen previews preserve the same wide-glyph and soft-wrap behavior as live
matching.

Waiting output and `session wait` use dedicated connections. Re-issue short
waits as needed; a killed client does not promise immediate daemon-side waiter
cancellation, so the requested timeout is the release bound.

If `session output` returns a structured `gap`, retained history no longer
contains the requested range: discard that cursor and restart from a current
screen or newest tail. If it reports `session_runtime_changed`, discard the old
runtime identity and cursor before retrying. A `session wait` result with
`reason: "timeout"` is a bounded no-change outcome, not proof of idle or
health; a wake reports the changed runtime/session snapshot and watermark.

TypeScript clients running inside a managed session configure the atomic origin
pair explicitly; the SDK copies it to ordinary, subscription, and dedicated
observation connections and never reads `process.env`:

```ts
const client = await connectLocal(socketPath, {
  origin: { sessionId: "s-origin", daemonId: "daemon-origin" },
});
```

### Following agent turns

A manager agent that starts sessions and follows them waits for turn events
instead of polling. A turn event is a finished turn (`turn_completed`), an agent
blocked on the owner (`agent_blocked`), a pending approval
(`approval_required`), or the session entering `stopped`, `done`, or `failed`
(`exited`); a session whose runtime was lost with an observed end enters
`failed`, so it records `exited` and ends the wait. Events are recorded whatever the notification policy keeps in the
inbox. `turn_completed`, `agent_blocked`, and `approval_required` are recorded
when the attention debounce window (`attention_debounce_secs`, 5 seconds by
default) after the agent's report ends, so a stop followed at once by resumed
work is not a finished turn; `exited` is recorded at once. Every result carries
the next cursor; pass it as `--after` so the next wait returns only events
recorded after it and nothing recorded between two waits is lost.

The cursor orders recording, not the agent's timeline: the stop of the previous
turn can be recorded after a cursor taken shortly after that stop. If the turn
ended at T, a cursor is taken at T+1s, input is sent at T+4.5s, and the agent is
seen working only at T+5.2s, the old `turn_completed` is recorded at T+5s and a
wait `--after` that cursor returns it as the end of the new prompt. Chain the
next prompt from the cursor of the result that returned the previous turn
event, or take a fresh cursor only once at least `attention_debounce_secs` have
passed since the previous turn ended. A short wait returns the current cursor,
usually with `reason: timeout`, but it can also return an `event` (handle it) or
`targets_ended`:

```bash
# a cursor taken at least attention_debounce_secs after the previous turn ended
pohunek session wait s-01J00000000000000000000000 --timeout 1ms --json
pohunek session input s-01J00000000000000000000000 --stdin --json < prompt.txt
pohunek session wait s-01J00000000000000000000000 s-01J00000000000000000000001 \
  --until turn_completed,agent_blocked,approval_required,exited \
  --after <cursor> --timeout 5m --json
# the cursor of that result covers the finished turn: chain the next prompt from it
```

`session wait <target>...` takes up to 32 targets; every target must be on one
host, or the command fails as a usage error before any request (run one wait
per host). Targets may be ids, `host/id`, or display names. `--until` takes a
comma list or a repeated flag and defaults to all four kinds. `--timeout` needs a
unit (`ms`, `s`, `m`, or `h`, for example `90s` or `5m`); without it the daemon
default applies (5 minutes unless `service.toml` `[waits]` sets another), and a
value above the daemon ceiling (30 minutes by default) fails with
`session_wait_limit_exceeded`. With no flags at all, `session wait
<target>` is a turn wait with every kind and the daemon default.

`--json` prints the daemon result as one document: `reason` (`event`,
`timeout`, `cursor_reset`, or `targets_ended`), the `event` (`session_id`,
`kind`, `at`, and `notification_id` when the inbox kept a record), the next
`cursor`, and `progress` for every target. The human form prints the event as
`<session> <kind> <at>` or the bare reason, then one `progress:` line per target
(`<session> <state> <activity> output_advanced=<bool> last_output_at=<time>
turn_started_at=<time>`, `-` for a time the daemon has not observed), then
`cursor: <cursor>` as the last line, so a shell loop can chain waits:

```bash
cursor=$(pohunek session wait s-01J00000000000000000000000 --timeout 1ms | sed -n 's/^cursor: //p')
pohunek session wait s-01J00000000000000000000000 --after "$cursor" --timeout 5m
```

The command exits 0 for every reason, a timeout included, as the legacy wait
does; branch on the reason. A `timeout` with `output_advanced=true` and activity
`working` means the agent is still working, so wait again with the new cursor; a
long-quiet `last_output_at` means it is stuck or waiting at an empty prompt, so
inspect it. `cursor_reset` means the cursor came from another daemon instance
(for example before a restart) or is older than the retained events: events in
between are not reported, so take the fresh cursor and inspect the sessions.
`targets_ended` means every target is terminal and nothing newer is pending.
To stream instead of wait, use `notifications watch --session <id> --kind
<kind> --unread`, which follows only the inbox records the policy keeps.

The legacy single-target predicate wait stays available unchanged: any of
`--state`, `--activity`, `--after-updated-at`, `--after-terminal-watermark`,
`--after-output-offset`, `--worker-instance-id`, `--runtime-generation`, or
`--timeout-ms` selects it, it requires `--timeout-ms` (1 to 8000), and mixing
it with `--until`, `--after`, or `--timeout` is a usage error.

### Notifications triage

```bash
pohunek notifications list --unread
pohunek notifications ack buildbox/n-42
pohunek notifications policy set --provider default --kind turn_completed --enabled
pohunek notifications policy set --provider pi --kind agent_blocked --disabled
pohunek notifications retention prune --status archived --before 2026-06-01T00:00:00Z --apply

# follow new finished turns of one session
pohunek notifications watch --session s-01J00000000000000000000000 --kind turn_completed --unread --json
```

`notifications watch` filters with `--session`, `--kind`, and `--status`
(`--unread` is `--status unread`) apply client-side to the record each create
or update event carries, so an update that marks an earlier finish
`acknowledged` does not pass `--unread`. While any filter is set, delete events
are dropped, because they carry only the notification id; for the same reason
`--status deleted` is refused.

### Assistant

```bash
pohunek assistant "why does attach fail on my laptop?"
pohunek assistant setup                # steer toward host setup
pohunek assistant debug --host buildbox --no-snapshot
pohunek assistant --agent hermes "Explain the current session runtime."
```

The assistant is an ordinary agent session — the same PTY, attach, and
notification machinery — launched with a materialized offline knowledge
bundle and a redacted snapshot of live state. No secrets enter the prompt. Its
automatic preference order is `pohunek-assistant`, `codex`, `claude`, then
`hermes`; explicit Hermes selection still requires the supported runtime on the
selected host. Codex, Claude Code and Hermes are served only by their official
packages: on a host with none of them (and no capable host profile) the
assistant answers `no_capable_agent`, whose hint names the package install.

Keep a request out of the process list with `--request-stdin`. The CLI reads
bounded UTF-8 text from stdin and trims surrounding whitespace. An empty
request fails with `cli_usage` before any daemon is started or dialed. The flag
cannot be combined with request words, and a remote launch with it needs
`--yes`, since stdin cannot also answer the confirmation prompt.
`--request-timeout-ms` and `--meta key=value` work as on `session new`:

```bash
printf '%s' 'Why does attach fail on my laptop?' \
  | pohunek assistant --request-stdin --meta launch=7f3a9c --json
pohunek session list --meta-eq launch=7f3a9c --json   # after request_timeout
```

A launch sends one `session.new` and never resends it. Only the CLI's own
refusals before that request prove that nothing was created; after any other
failure, look up the caller's `--meta` key before launching again. The key name
is the caller's choice. `session.new` has no idempotency key, so a retry can
still create a second session, which the same key lists. See "Assistant request
and creation outcome" in [public-api.md](public-api.md).

### Hermes Agent and operator plugin

Pohunek manages the local interactive Hermes terminal as `--agent hermes`. The
daemon compiles no Hermes in: only the official `pohunek.runtime.hermes`
package serves `hermes`, so install it from the extracted daemon archive first.
Without it `session new --agent hermes` fails with `agent_profile_not_found`, whose hint
names the install. Before launching, inspect the target host: the `hermes`
runtime must be `available` and report `supported=true`: its `version` lies in
the range the Hermes package declares (`[0.20.0, 1.0.0)`).

```bash
pohunek plugin install runtime/packages/hermes.tar.zst --catalog runtime/runtime-catalog.json --yes
pohunek host inspect local --json
pohunek session new --agent hermes --name "investigate-login-bug"
```

Pohunek launches exactly `hermes chat`. A valid reported native Hermes reference
is resumed only as `hermes chat --resume <reference>`; it never uses
`--continue` or `--pass-session-id`. Hermes has no supported native fork, so a
fork request returns typed `agent_fork_unsupported` data before a worktree or
child session is created. Pohunek never reads Hermes `state.db`.

Install the operator plugin only into a target you name explicitly. The default
profile is valid only when stated as `--hermes-profile default`; named profiles
and a custom absolute home are isolated alternatives. The installer creates a
Pohunek-owned owner-private policy outside the plugin checksum set, and binds
its exact absolute path into the managed plugin asset.

The operator plugin comes from the installed, enabled and selected official
Hermes package (`pohunek plugin install runtime/packages/hermes.tar.zst
--catalog runtime/runtime-catalog.json` from a Linux daemon archive). Without
it, `integration install` and `update` fail with
`hermes_package_not_installed` and change nothing, while `status`, `doctor`
and `uninstall` keep working on an existing install.

```bash
# Observation plus constrained peer-session management in one named profile.
pohunek integration install --agent hermes --hermes-profile work \
  --access-mode manage --allow-host local --json

# A default profile must still be selected explicitly.
pohunek integration status --agent hermes --hermes-profile default --json
pohunek integration doctor --agent hermes --hermes-profile work --json

# A relocated profile must be an explicit absolute, owner-private target.
pohunek integration install --agent hermes \
  --hermes-home /absolute/private/hermes-home \
  --access-mode read_only --allow-host local --json
```

`read_only` registers observation tools; `manage` adds constrained session
management; `full` alone registers stop and remove. Remote host access is
restricted by the explicit allowlist and goes directly to that daemon over
NetBird, never through SSH. `*` needs `--confirm-wildcard`. Use
`integration update` for a version/policy refresh and `integration uninstall`
to remove only managed assets; add `--confirm-modified` when the ownership
check reports changed assets. `status`, `doctor`, `update`, and `uninstall` are
local for Hermes. Codex and Claude expose daemon-backed `integration status`
and `integration doctor` on the effective `--host`, and a local `integration
uninstall`; `update` remains Hermes-only and returns a typed unsupported-action
error for those agents. A
remote status recovery hint names the daemon host where the local-only installer
must run; `--host` never turns `integration install` into a remote mutation.

### Codex and Claude config homes

`--agent` offers `hermes`, which selects the CLI-run Hermes operator plugin
lifecycle above; any other value is a runtime id, and `codex` and `claude` are
runtime ids: the daemon compiles neither in, so
`integration install --agent codex` or `--agent claude` needs the official
package of that runtime installed and answers `runtime_not_installed` (with the
package install as the hint) until it is. `status`, `doctor` and `uninstall`
with `--agent codex` or `--agent claude` work without the package: they address
the config home of the earlier built-in (`CODEX_HOME`, else `~/.codex`;
`CLAUDE_CONFIG_DIR`, else `~/.claude`), so hooks pohunek installed there can be
reported and removed; `--profile` and `--all-profiles` reach the homes of host
profiles over that runtime through each profile's own `[env]`, although such a
profile does not resolve without the package. Without `--agent`, `status` and
`doctor` cover only the runtimes the daemon serves, and a home that several
served runtimes reach through one handler (a third-party package declaring the
Claude handler and `~/.claude`, say) is attributed to none of them: each
reports `integration_home_ambiguous_owner` with its hint (the doctor as a
`config_root_invalid` finding whose remediation names `--agent`), and a bare
`install` fails with it before writing any home. Name the runtime with
`--agent` to act on such a home. On a host where no served runtime has
daemon-managed hooks, a bare `install` fails with
`integration_no_managed_runtime`, whose hint names the official package install
and then `pohunek integration install --agent <runtime>`. `--all-profiles` also
covers host profiles over the runtime that do not resolve (an unpinned profile
over a package-served `claude`, say): status, doctor and uninstall reach the
home their own `[env]` names, and install lists them as failing.

A Codex or Claude agent reads its settings and hook registration from a config
home: the runtime's declared variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) as the
launched agent sees it, else `~/.claude` / `~/.codex`. A host profile can point
that variable somewhere else in its `[env]`, which is how one machine keeps
separate subscriptions. The hooks must be installed into each home an agent can
run with:

```bash
# The runtime's own home (no profile).
pohunek integration install --agent claude

# The config home a host profile launches with; the profile must extend the runtime.
pohunek integration install --agent claude --profile work
pohunek integration status --profile work --json
pohunek integration doctor --agent claude --profile work

# Every distinct home of the runtime: its own and each host profile's.
pohunek integration install --agent claude --all-profiles
pohunek integration uninstall --agent claude --all-profiles
```

`--profile` and `--all-profiles` are mutually exclusive, are refused together
with the Hermes selectors (`--agent hermes`, `--hermes-profile`,
`--hermes-home`), and are served by the local daemon only (`status` and
`doctor` with a remote `--host` get `local_only_method`), because the reports
name directories derived from the profile's environment. Without `--agent`,
`--profile NAME` selects the profile's own runtime. A profile value that is not
an absolute path (`~/x`) is refused, never expanded.

Before an install or uninstall that carries a selector the CLI sends a
read-only `integration.status` with the same selector and proceeds only when the
daemon answers `home_selectors: true`. A daemon that predates the selectors would
ignore them and change the default home, so the CLI refuses with
`integration_home_selectors_unsupported` (update and restart the daemon) and
sends nothing mutating; `status` and `doctor` apply the same check to their own
reply. Recovery hints name the `--profile` the daemon verified for the home.

The directory is resolved from the environment a launched agent sees, not from
the daemon's own process environment: a `CLAUDE_CONFIG_DIR` or `CODEX_HOME` set
only in the daemon's service environment does not steer `integration install`
or `status`: they use `~/.claude` / `~/.codex`, and hooks installed in the
directory that variable names are not reported. Add the variable to the daemon's environment
allowlist so launched agents see it too, or move it into a host profile's
environment and use `--profile`. A package runtime that names a daemon-run
integration handler needs a `[config_home]` descriptor table; without one it is
left out of bare `install`/`status`/`doctor` and `--agent` answers
`agent_config_home_undeclared`.

`--all-profiles` runs one transaction per distinct directory (profiles that
resolve to the same canonical directory share one), skips homes whose directory
does not exist and fails when none exists, labels every result with the
profile(s) it stands for, and exits non-zero when any home failed. There is no
atomicity across homes: a home that fails rolls back to its own prior tree and
the others keep what they committed. A profile file that does not resolve is
skipped, and the daemon logs a warning that names it.

The plugin is a delegated-tool guardrail, not a sandbox against a same-user
Hermes process with shell or file-write access. It repeats the daemon's exact
origin-session denial for `session.stop`, `session.resume`, `session.remove`,
`session.fork`, `session.resize`, `session.set_metadata`, `session.rename`, and
`session.input` (the daemon also denies `session.remove_accepting_unconfirmed`, which the plugin never offers). Only `session.report_agent`, `session.release_agent`, and
`session.report_native_id` remain lifecycle-report exceptions. Hooks use
bounded local reporting, never a subprocess, network connection, or Hermes
database; if they fail, turns remain usable and daemon process/screen detection
is the fallback.

Hermes programmatic input preserves multiline prompts with bracketed paste and
a separate submit. It accepts LF and tab, rejects other terminal control
characters without rewriting them, and refuses input while Hermes is visibly
waiting for owner approval.
