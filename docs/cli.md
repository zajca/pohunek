# CLI guide

Reference for the `pohunek` command line: every command, multihost targeting,
shell completion, automation and bounded observation, notifications, the
assistant, and the Hermes Agent runtime. `pohunek agent-skill` prints the
agent-facing operating guide for the same commands.

## Commands

Every command accepts `--host <name>` (default `local`), and nearly all of
them `--json` for machine-readable output (the exceptions are `attach`,
`daemon start`, and `prompt render`). Session targets are `<session-id>` or
`<host>/<session-id>`. A discovered route uses
`<overlay>:<canonical-identity>@<port>`. Canonical identities are typed
`peer~<base64url>` or `fqdn~<base64url>` selectors without padding, so provider
keys containing `/`, `+`, `=`, or the route separator `@` remain safe inside
session targets and URLs. The selector is decoded and re-resolved through
current provider state before every new connection while the discovered daemon
port is retained.

| Command | What it does |
|---|---|
| `pohunek doctor` | Environment health: binaries, socket, state dirs, NetBird, agents. |
| `pohunek service install [--from <dir>] [--prefix <dir>] [--json]` | Install the daemon as a native login service (systemd user unit or launchd agent) from staged binaries into a versioned `libexec` directory and write `service.toml`. |
| `pohunek service upgrade [--from <dir>] [--accept-runtime-loss] [--json]` | Switch to the staged version, restarting only the daemon; live workers keep their PID and PTY, and unreferenced old versions are removed. First runs a read-only preflight with the new binaries and refuses (`service_upgrade_sessions_at_risk`) while a live session would lose recovery or not be adopted, listing each with its reason code; `--accept-runtime-loss` accepts that loss. A store the new daemon would refuse is never overridable (`service_upgrade_store_unusable`), nor is a preflight that cannot run (`service_upgrade_preflight_failed`). Cancelling an upgrade at or after daemon registration checks whether the previous release can read the current store before changing the service, stops the new daemon, then checks again before starting the previous one. A refused rollback keeps its transaction (`service_rollback_store_unusable`, `service_rollback_sessions_at_risk`, or `service_rollback_preflight_failed`); store refusal is never overridable, while `--accept-runtime-loss` can accept unverified live-session recovery. `service check` previews the refusal. `--json` carries the rollback preflight and `accepted_runtime_loss` in `rolled_back` (and at the top level when rollback is the only operation). |
| `pohunek service uninstall [--stop-sessions] [--purge] [--json]` | Remove the service; refuses while sessions are live unless `--stop-sessions`, and keeps durable metadata unless `--purge`. |
| `pohunek service status [--json]` | Daemon job, namespace, installed versions, worker jobs per generation, and any interrupted or running install transaction. |
| `pohunek service check [--prefix <dir>] [--accept-runtime-loss] [--json]` | Run every check the install (or, for an existing service, the upgrade) of this version makes before its first effect — `HOME` and XDG roots, the prefix, every directory it writes, a pending transaction, the recorded installation, and for an upgrade the live-session preflight — without changing anything; fails with the error code that command would. |
| `pohunek service lock -- <command> [args...]` | Run a command while holding the service transaction lock; `pohunek service` commands it runs reuse the lock with the holder token in `POHUNEK_SERVICE_LOCK_TOKEN`, every other one is refused until it exits; the lock ends with this process, never with a process the command left behind. Exits with the command's status. |
| `pohunek plugin list / inspect / doctor` | List installed runtime packages, show one (record, health, declared runtime), or verify every package root; `doctor` exits 1 when findings exist. Local daemon only; `--host` is rejected. |
| `pohunek plugin install <archive> (--sha256 <digest> \| --catalog <file>)` | Validate a package archive with a dry run and print what it declares (program, arguments, resume/fork support); installs only after repeating the command with `--yes`. A third-party archive is trusted solely by the digest you supply; `--catalog` authorizes official packages. `--no-enable` installs it disabled. |
| `pohunek plugin link <dir>` | Copy a developer package directory into storage; installed disabled and unselected, with the same `--yes` consent. |
| `pohunek plugin update <package> <archive> (--sha256 <digest> \| --catalog <file>)` | Install a new version of an installed package next to the old one, enabled and selected; the old digest stays installed for `select` rollback. Needs `--yes`. |
| `pohunek plugin select / enable / disable <package> [--digest <d>] [--version <v>]` | Pick the version bare requests resolve to, or allow or block fresh launches (live sessions keep running). An ambiguous selection is an error, never a guess. |
| `pohunek plugin uninstall <package> [--digest <d>] [--version <v>] [--remove-modified] --yes` | Remove an installed package no session or profile pins; `--remove-modified` is only for a root that fails verification. |
| `pohunek plugin profile list [--json]` | List host agent profiles (`agents/*.toml`) with base runtime, pinned package and digest, and a state: `builtin` (the base is the built-in `shell`), `pinned`, `needs_migration` (an installed package serves the base but the profile pins none), `package_not_installed` (the base is not built in and no installed package serves it, so the profile cannot launch; the detail names the package install, the official one for `codex`, `claude` and `hermes`, then the migration), `package_unavailable` (no installed package serves the base, but one that may serve it cannot be loaded; the detail names `pohunek plugin doctor`, which reports the fault and its repair), `pin_not_installed`, or `unreadable`. Reports parse errors by line only; never prints file content. |
| `pohunek plugin profile migrate <name> [--digest <d>] [--yes]` | Pin a profile to an installed package; the daemon rewrites only its `package` and `digest` keys (every other byte, including `[env]`, is preserved) under the package lifecycle authority with an atomic rename, so the profile never goes missing. Without `--digest` it uses the selected, enabled package serving the base runtime. Previews first and needs `--yes`; a profile never changes digest through `plugin update` or `select`. |
| `pohunek daemon start [--detach] [--dev-subprocess]` | Run the installed daemon by hand (needs `service.toml`), or with `--dev-subprocess` a development daemon with plain subprocess workers. |
| `pohunek health` / `status` | Daemon liveness, build, and protocol version. |
| `pohunek session new` | Start a session: `--agent`, `--name`, `--project`/`--repo`, `--branch`, `--base-branch`, `--cwd`, `--input`, `--request-timeout-ms`, `--meta k=v`. |
| `pohunek session list` | List sessions, including a `running/recent` subagent count and a `BACKGROUND` column of compact per-kind counts of the latest background snapshot (`a` subagent, `s` shell, `m` monitor, `w` workflow, `t` teammate, `o` every other kind; zero kinds omitted; `-` when no snapshot has been reported, `0` when the latest snapshot lists no task). Example row: `2a 2s 1m`. Repeat `--filter state=running`, `--meta-eq key=value`, and `--meta-has key` to combine predicates with AND. Metadata values split at the first `=` and may be empty; keys are opaque. `-q` prints ids only. |
| `pohunek session inspect <target>` | Full logical session record: agent state, current/recent subagents, runtime state and generation, cwd, project, branch, worktree, recovery binding. Adds `background_items` (`<none>` without a snapshot, `0` for an empty one) and `background_observed` (age of the snapshot, `<none>` without one), then one line per subagent with its background label and age, and one `background [<kind>]: <status> <program> <label> first seen >= <age>` line per other background item. An absent program or label prints `-`. Labels and programs are escaped for the terminal. |
| `pohunek attach <target>` | Attach the current terminal; `Ctrl-]` detaches. |
| `pohunek session input <target> <text>` | Inject a prompt with agent-correct framing; use `--stdin` for non-argv input. `--until <activity>` (repeatable) and `--timeout <ms>` wait for the agent's reaction in the same call, for zero-delay profiles only. |
| `pohunek session screen <target>` | Read the current rendered terminal; `--json` preserves runtime identity, watermark, geometry, cursor, and visible lines. |
| `pohunek session detection <target>` | Preview the active detection manifest regions; `--json` also lists every supported region kind. |
| `pohunek session output <target>` | Read a newest retained tail or continue with `--worker-instance-id` (the session's `worker_instance_id`), `--runtime-generation`, and `--after-offset`; `--wait-ms` performs a bounded wait. |
| `pohunek session wait <target>...` | Wait for the next turn event (`turn_completed`, `agent_blocked`, `approval_required`, `exited`) of one or more sessions on one host: `--until`, `--after <cursor>`, `--timeout <duration>`; this is the mode with no flags. Any legacy flag (`--state`, `--activity`, the `--after-*` cursors, the runtime identity, `--timeout-ms`) selects a single-target long-poll of up to 8000 ms for explicit predicates. |
| `pohunek session fork <target>` | Fork an agent conversation into a new session when that session advertises fork capability (Claude Code, and runtime packages that declare a fork such as `pi`). |
| `pohunek session resume <target>` | Relaunch a stopped or lost session from its captured native reference. A session launched from a host profile relaunches only while the profile still has the revision frozen into the session; an edited or legacy profile fails with `agent_profile_changed` and a deleted one with `agent_profile_missing`. `--accept-profile-change` relaunches under the current profile and freezes its revision (also on `session fork`); the local daemon honors it, a remote host refuses it with `agent_profile_change_local_only`. |
| `pohunek session diff <target> [--base <ref>]` | Unified diff of the session's worktree vs its base. |
| `pohunek session rename / stop / rm` | Rename, stop, or evict a session. |
| `pohunek session rm <target> --accept-unconfirmed-cleanup` | Evict a session that `rm` refused with `runtime_supervision_ambiguous` only because same-user processes with unreadable environments may belong to its runtime (the refusal lists them). On macOS 27 the kernel withholds the environment of Apple platform binaries; the sweep reaps those the lost worker created (a `/bin/zsh` session root and its helpers) without this flag when the worker journaled its creation number in the current boot, and dismisses those created before the worker. When the worker also journaled its kernel coalitions (`worker_coalitions`, same boot), it dismisses every hidden process the kernel puts in other coalitions than the worker's, such as a launchd job, helper or another session's agent started after the worker. An orphan that re-executed after its creator died and a process whose creator exited after the worker started remain candidates once they have outlived the sweep grace (`sweep.grace_ms`, during which the sweep keeps re-reading the table for a process that is only passing; one removal waits at most one grace in total across all its runtimes), so the flag may still be needed after a lost runtime; the refusal lists each by pid and command name for you to judge. The same holds for every hidden process when the worker journaled no creation number or coalitions (a worker started by the previous release). Those candidates are never signalled and, if one carries the runtime marker, keep running unsupervised after the worktree, logs, and record are deleted. The consent covers one call, is never automatic, and without it the removal stays refused; inspect and end the listed processes, then retry, as the safe path. The result lists the accepted processes; a daemon without the method answers `method_not_found`. |
| `pohunek project add / list / show / rename / rm` | Manage git-repo-aware project records. |
| `pohunek project actions / action / prompt` | Resolve per-project launch recipes and prompt templates. |
| `pohunek host discover / list / inspect` | Find NetBird peers running daemons (standalone cache; `--refresh`) and query live capabilities. |
| `pohunek host governance inspect <host>` | Read the safe stable host identity and local governance state; `--json` preserves explicit absence and canonical revisions. It does not mutate enrollment or ownership. |
| `pohunek completions <bash\|zsh\|fish>` | Print static shell completion; add `--dynamic` for bounded host/session candidates. |
| `pohunek notifications list / watch` | Inspect or stream the durable inbox; `--all-hosts` fans out. |
| `pohunek notifications read / ack / archive / delete` | Drive one record's lifecycle (`host/id` targets a specific host). |
| `pohunek notifications policy / retention` | Per-kind policy for the default flags or any runtime id (`--provider default\|<runtime-id>`; `default` is always the base flags, even if a runtime is named `default`), retention pruning (`--dry-run` / `--apply`). |
| `pohunek integration install` | Install Codex/Claude hooks into the runtime's own config home, one host profile's home (`--profile NAME`) or every distinct home (`--all-profiles`), or install a selected Hermes profile's managed plugin from the installed Hermes package with explicit access mode and host allowlist. |
| `pohunek integration status` | Inspect daemon-managed Codex/Claude hooks on the effective `--host` (`--profile` and `--all-profiles` select profile config homes and are local-only), or one explicitly selected local Hermes target. |
| `pohunek integration doctor / uninstall` | Diagnose or remove daemon-managed Codex/Claude hooks (`doctor` follows `--host`; `uninstall` targets the local daemon; both accept `--profile NAME` and `--all-profiles`, local-only), or, with `--agent hermes`, one explicitly selected local Hermes plugin target. |
| `pohunek integration update --agent hermes` | Atomically refresh one explicitly selected local Hermes plugin target. |
| `pohunek setup [config]` | Install the default `attach.conf` and prompt templates (a bare `setup` is `setup config`). |
| `pohunek setup completions <bash\|zsh\|fish>` | Install completion in the shell's conventional user directory; add `--dynamic` to opt in to runtime candidates. |
| `pohunek assistant [intent] [request…]` | Launch the self-help assistant with knowledge bundle + live snapshot. |
| `pohunek agent-skill` | Print the complete bundled agent skill; `--json` wraps the skill text and its `content_sha256` in the process envelope. Fully local — `--host` is accepted and ignored. |
| `pohunek prompt render / link` | Render provider prompt templates and work-item link metadata (called by external launchers). |

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
have been delivered, so inspect the session instead of resending it. See
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
(`exited`). Events are recorded whatever the notification policy keeps in the
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

### Hermes Agent and operator plugin

Pohunek manages the local interactive Hermes terminal as `--agent hermes`. The
daemon compiles no Hermes in: only the official `pohunek.runtime.hermes`
package serves `hermes`, so install it from the extracted Linux daemon archive
first (the macOS archive carries no runtime package yet, #929). Without it
`session new --agent hermes` fails with `agent_profile_not_found`, whose hint
names the install. Before launching, inspect the target host: the `hermes`
runtime must be `available` and report `version=0.20.0` with `supported=true`.

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
