# Claude OSC 7501 Canary

The canary diffs the OSC 7501 status records of the official Claude runtime.
Issue [#2137](https://github.com/zajca/pohunek/issues/2137) defines it; the
checked-in expected table is `compat/claude/canary/expected.json`.

## What the canary is

Claude emits a canonical `state=<state>[:kind=<kind>]` record for every state
change. `final_record` stores that string without the `app` segment, whose
claude name is `claude-code` (for example `state=done`). The canary checks
that each new Claude version still emits the records the blocked-screen,
attention and lifecycle logic reads. A record is the canonical form only:
`msg`, `title` and `progress` are never stored or printed.

## When to run it

Run the comparison after every Claude version the owner or manager installs,
and before raising a floor that would make a new Claude the only supported
one. The record of a real capture is the evidence in the commit that
changes an expectation.

## Producing a capture

`cargo xtask claude canary capture` starts an isolated daemon from the
release binaries, installs the official Claude runtime package, writes one
host profile per launch shape and drives each scenario through the daemon's
CLI. It writes `capture.json` and `daemon.log` into `--out` for `compare` to
diff. Whatever the records say, a completed run exits 0; comparing is
`compare`'s job.

The isolated daemon runs with `pohunekd --local-only` (#2368): it serves its
own Unix socket only and never starts the remote control listener, so it
never asks an overlay (NetBird, Tailscale) for an address or binds the host's
overlay port. The capture requires the daemon log line `remote control
listener disabled by --local-only` before it starts a session, so the release
binaries must be recent enough to accept the flag.

Prerequisites:

- A release tree: `cargo xtask release test-runtime --output <dir> --runtime
  claude`, then copy the head `pohunek`, `pohunekd` and `pohunek-sessiond`
  binaries into `<dir>` (the shape `scripts/upgrade-test` builds).
- The `claude` executable to test. The sessions start it with the isolated
  `PATH` (`/usr/bin:/bin`), so it must not need another interpreter on
  `PATH`.
- The config directory (`CLAUDE_CONFIG_DIR`) of a logged-in account, for
  every scenario but `login_screen`.
- A git work directory that account already trusts. The capture never
  answers Claude's folder-trust dialog: trust the directory once by starting
  `claude` there with `CLAUDE_CONFIG_DIR` set to that config and accepting
  the dialog.
- `python3` on `PATH` for `mcp_elicitation`, which runs the canary MCP
  server (`compat/claude/canary/elicit_server.py`).

```text
cargo xtask claude canary capture \
  --release-dir <dir> \
  --claude /absolute/path/to/claude \
  --claude-config-dir /absolute/path/to/claude-config \
  --workdir /absolute/path/to/git-workdir \
  --out <empty-or-new-dir> \
  [--scratch-parent /short/absolute/dir] \
  [--step-timeout-secs <seconds>] \
  [--scenario idle_after_turn|permission_prompt|mcp_elicitation|login_screen|running_turn]...
```

- `--release-dir`, `--claude`, `--workdir`, `--out`: required. Every path
  flag must be absolute. `--out` must not exist or must be empty, so a
  capture never mixes with an older one.
- `--claude-config-dir`: the account the scenarios run as. Without it only
  `login_screen` may be selected; an account scenario without it exits 2
  naming the flag.
- `--scenario`: repeatable; default is all five in table order.
- `--scratch-parent`: the directory the isolation root is created in and
  removed from at the end; `TMPDIR` by default. The deepest socket below it
  is a session worker's staged bind, 84 bytes below the parent with the
  longest session ID, and every socket path must fit the platform limit (107
  bytes on Linux, 103 on macOS). The parent may therefore be at most 23 bytes
  long on Linux and 19 on macOS (`/private/tmp` fits; the default macOS
  `TMPDIR` does not). The capture checks this the way the daemon does before
  it starts anything; a parent that is too long exits 2 naming the path
  length.
- `--step-timeout-secs`: the bound of every wait the driver polls (the
  first screen, a record and the first notification); 120 by default. The
  CLI's own wait for the agent to take a prompt (`--until working` of the
  turn scenarios) is bounded by the protocol's 8 s maximum instead.

Before every account scenario the command prints one warning line to
stderr: the scenario runs as the account of `--claude-config-dir` and may
spend its quota.

The scenarios:

| scenario | profile arguments | prompt | read point |
| --- | --- | --- | --- |
| `idle_after_turn` | none | `Reply with the single word OK.` | a `working` record and then `done` or `idle` |
| `permission_prompt` | `--permission-mode=default` | `Run the shell command touch canary-permission with the Bash tool.` | the first `blocked` record, then the first notification |
| `mcp_elicitation` | `--mcp-config=<scratch>/mcp.json --allowedTools=mcp__canary__ask --strict-mcp-config` | `Call the ask tool of the canary MCP server.` | the first `blocked` record, then the first notification |
| `login_screen` | none; an empty config home | none; Enter on the first-run theme picker | the first record after the theme picker, or 10 s after the login method list is on screen; `final_record` is the last record after the picker, `null` when none arrived |
| `running_turn` | none | `Write a 400 word story about a lighthouse.` | the first `working` record, before the turn ends |

Each session starts in `--workdir`. Before the prompt the capture waits for
the first screen: the prompt, the login method list (the package's
`login_method_list` rule) or the folder-trust dialog (its
`folder_trust_dialog` rule). The trust dialog stops the run with exit 4 and
the message to trust the workdir in Claude for that config first; no prompt
is sent while the dialog is on screen. The login method list in an account
scenario exits 2: log in to that account first.

`permission_prompt` asks for a command that writes a file, because Claude
runs a read-only command such as `true` without asking (observed with
2.1.322 in `default` mode). The capture never approves it, so the file is
not created.

An empty config home first shows Claude's first-run theme picker (the
package's `theme_picker` rule), then the login method list. The OSC 7501
contract has no record for either first-run dialog, so the `login_screen`
row reads at the login method list: the capture answers the theme picker
with Enter (its default) and then waits for the login screen. It checks the
screen again right before the key and stops with exit 4 if the folder-trust
dialog is showing, since Enter there would trust the directory. A record
written before the picker stays in the row's `records` and never becomes
its `final_record`. The contract defines no
record for the picker, so such a record carries no information about the login
screen. The boundary is the output read after the screen showed the picker,
right before the key; the worker retains output before its screen shows it,
so that read holds every record written before the picker. For the login
scenario with the theme picker answered, an `idle` record is taken for
Claude's prompt only from a record read after the picker records; a startup
`idle` written before the picker is ignored. For the login scenario without
the theme picker answered, in an empty config home where Claude goes straight
to its prompt, the capture exits 2 with "Claude opened its prompt without a
login in an empty config home", and only after `--step-timeout-secs` has
elapsed, because the prompt box is not recognized while Claude holds its
`idle` record. The theme picker in an account scenario exits 2: finish Claude's
first run for that config first.

The records of a row are those read up to its read point, and the reads
that depend on the held record (the activity and the stale-record check)
finish within 10 s of the record's first arrival: the 30 s staleness window
of #2138 then cannot replace the record with the screen. A later read stops
the run (exit 4), and a row the detector already replaced exits 2 so the
capture can be retried. `attention` is read with them; a blocked row with no
notification yet waits for the first one up to `--step-timeout-secs`. After each scenario, and on
every failure, the session is stopped before the daemon stops.

Side effects: every account scenario runs Claude as that account and may
spend its quota. The scenarios approve no tool call, so nothing is written
to `--workdir`; the theme picker's Enter goes to the empty login config home,
which is removed with the isolation root. The isolation root holds the daemon, its sockets, the
profiles, an empty login config home, and the scratch `mcp.json` and MCP
server; it is removed at the end of the run. `daemon.log` holds the
isolated daemon's own log.

The `mcp_elicitation` and `login_screen` rows, and the Claude flag
spellings of the profile arguments (from the Claude Code CLI reference),
were verified at the first real run with Claude 2.1.322 (#2101): the login
method list emits no record (`final_record: "none"`) and raises
`agent_blocked` through its screen rule.

Exit codes:

| code | meaning |
| --- | --- |
| 0 | every selected scenario was captured and `capture.json` was written |
| 2 | usage or setup error; stderr names its cause |
| 4 | a driver step timed out, a row's activity was not read within 10 s of its record, or the workdir is not trusted for the account; the partial `capture.json` holds the scenarios captured before and stderr names the step |

The capture schema, the schema the capture command writes and `compare`
reads:

```json
{
  "schema": 1,
  "runtime": "claude",
  "claude_version": "<line printed by claude --version>",
  "pohunek_version": "<pohunek --version line>",
  "scenarios": {
    "<name>": {
      "records": ["state=done", "state=blocked:kind=permission"],
      "final_record": "state=done",
      "activity": "idle",
      "attention": []
    }
  }
}
```

`records` are the canonical 7501 records in order. `attention` is the sorted
list of unread attention kinds from `notifications list`: one of the six
notification kinds `agent_blocked`, `approval_required`, `turn_completed`,
`session_finished`, `error`, `system`. `activity` is the `session inspect`
activity: one of `working`, `blocked`, `idle`. Every `records` entry and
every `final_record` must match the canonical grammar
`[app=..:]state=<state>[:kind=<kind>]` and nothing else; a `msg=` segment or
an unknown segment is refused.

## Comparing

```text
cargo xtask claude canary compare --expected compat/claude/canary/expected.json --capture <capture.json>
```

`compare` prints the capture's `claude_version` and `scenario | expected |
observed | verdict` rows. The expected and observed columns render the
`final_record` and `attention` of the row (for example `state=done []`);
`null` marks an UNVERIFIED row and `-` a MISSING row. For each mismatch it
prints one `-`/`+` line pair per differing leaf, keyed by its JSON pointer
path in a stable sorted-key order:

```text
- <scenario><pointer>: <expected JSON value>
+ <scenario><pointer>: <observed JSON value>
```

A key present on only one side prints only its `-` or `+` line. Arrays
compare by index. A capture that stores a message (`msg`, `title` or
`progress`, or a raw base64 message tail inside a record) is refused.
The closed allowlist of the schema is enforced as well: the capture may
carry only the top-level keys `schema`, `runtime`, `claude_version`,
`pohunek_version` and `scenarios`, and each row may carry only `records`,
`final_record`, `activity` and `attention`. A scenario name outside the
five, any unknown key at either level, a wrong shape or any other unknown
string exits 2. The version lines are the only exception: `claude_version`
is the line printed by `claude --version` and is free-form, while
`pohunek_version` must match exactly `^pohunek [0-9]+\.[0-9]+\.[0-9]+$`
(the release version line): `pohunek `, a space, three decimal numbers
separated by dots, nothing else.

Exit codes:

| code | meaning |
| --- | --- |
| 0 | every row matches |
| 1 | at least one mismatch (diff printed) |
| 2 | usage error or unreadable input |
| 3 | at least one row is UNVERIFIED (expected value `null`) and none mismatch |

A row whose expected value is `null` is UNVERIFIED by definition and never a
pass: `null` means the expectation is unknown. An expected `final_record` of
`"none"` states that the scenario emits no record: it is VERIFIED when the
captured `final_record` is `null` and a MISMATCH when a record arrived
(`- <scenario>/final_record: none`, `+ ...: "<record>"`). `compare` never writes the expected file: there is no `--update`
flag, and an expectation changes only by a reviewed hand edit with its
evidence in the commit message.

## Expected table

Every non-null value is taken from the cited evidence in the `source` field
of each row, never from a capture run. A usage-limit scenario is not part of
the table (#2101: passive journaling only).

## How a difference is handled

Never update the table from a run. Open or update an issue with the new
Claude version, the capture, the diff of `compare` and the `claude
--version` line. An expectation changes only by a reviewed hand edit with
its evidence in the commit message.
