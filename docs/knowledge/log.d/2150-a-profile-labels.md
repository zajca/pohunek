## Unreleased (2026-11-13, profile display name and audience, #2150)

- Host agent profiles accept two optional presentation-only top-level keys:
  `display_name` (trimmed free text, at most 128 bytes, no hidden or separator
  characters) and `audience` (`owner`, `agent` or `hidden`; absent means
  `owner`). An invalid value is `invalid_profile` at launch. Neither key enters
  the profile revision, so label edits never trigger `agent_profile_changed`.
  `host.inspect` reports them on each resolvable profile's `runtimes` entry and
  `session list`/`session inspect` carry the live label as `agent_display_name`;
  the human `host inspect` table, `session list` (as `<agent> (<display_name>)`)
  and `inspect`, and `plugin profile list` (owner profiles by default, `--all`
  reveals the rest; new `DISPLAY NAME`/`AUDIENCE` columns) show them. See
  [Agent profiles](concepts/agent-profiles.md#presentation-labels).
