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

The capture command is tracked in issue #2137.

The capture schema, the schema the planned capture command (#2137) will
write and `compare` reads:

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
pass. `compare` never writes the expected file: there is no `--update`
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
