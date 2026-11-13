# Claude OSC 7501 Canary

The canary records and diffs the OSC 7501 status records of the official
Claude runtime. Issue [#2137](https://github.com/zajca/pohunek/issues/2137)
defines it; the checked-in expected table is
`compat/claude/canary/expected.json`.

## What the canary is

Claude emits a canonical `app=claude:state=<state>[:kind=<kind>]` record for
every state change. The canary checks that each new Claude version still
emits the records the blocked-screen, attention and lifecycle logic reads.
A record is the canonical form only: `msg`, `title` and `progress` are never
stored or printed.

## When to run it

Run the comparison after every Claude version the owner or manager installs,
and before raising a floor that would make a new Claude the only supported
one. The record of a real capture is the evidence in the commit that
changes an expectation.

## Producing a capture

The capture command is tracked in issue #2137 and is not part of this
document. Do not run `claude` or `capture` by hand in CI: the capture
drives the real installed Claude binary on the owner's account (scenarios 1,
2, 3 and 5) and uses an isolated empty `CLAUDE_CONFIG_DIR` for scenario 4.

The capture schema, written by the capture command and read by `compare`:

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
list of unread attention kinds from `notifications list`
(`approval_required`, `agent_blocked`). `activity` is the `session inspect`
activity.

## Comparing

```text
cargo xtask claude canary compare --expected compat/claude/canary/expected.json --capture <capture.json>
```

`compare` prints `scenario | expected | observed | verdict` rows and, for
each mismatch, one `-`/`+` line pair per differing leaf, keyed by its JSON
pointer path in a stable sorted-key order:

```text
- <scenario><pointer>: <expected JSON value>
+ <scenario><pointer>: <observed JSON value>
```

A key present on only one side prints only its `-` or `+` line. Arrays
compare by index.

Exit codes:

| code | meaning |
| --- | --- |
| 0 | every row matches |
| 1 | at least one mismatch (diff printed) |
| 2 | usage error or unreadable input |
| 3 | at least one row is UNVERIFIED (expected value `null`) and none mismatch |

A row whose expected value is `null` is UNVERIFIED by definition and never a
pass. `compare` never writes the expected file: there is no `--update`
flag, and an expectation changes only by a reviewed hand edit with its
evidence in the commit message.

## Expected table

Every non-null value is taken from the cited evidence in the `source` field
of each row, never from a capture run. A usage-limit scenario is not part of
the table (#2101: passive journaling only).

## Special scenarios

- `login_screen` starts with an isolated empty `CLAUDE_CONFIG_DIR` and no
  credentials. Its first real run is the evidence that verifies its row
  (#2101 login-screen comment).
- `mcp_elicitation`: a project-local MCP server whose tool sends
  `elicitation/create`, with the tool pre-allowed. The record is read while
  the dialog is visible.

## How a difference is handled

Never update the table from a run. Open or update an issue with the new
Claude version, the capture, the diff of `compare` and the `claude
--version` line. An expectation changes only by a reviewed hand edit with
its evidence in the commit message.

## Side effects of a capture

The capture drives the real Claude binary on the owner's account and costs
tokens. The scratch-folder trust is accepted once through the first input in
that scratch directory; it writes a trust entry in the owner's `~/.claude.json`.
The scratch directory is created under the isolated root, never under the
owner's home.
