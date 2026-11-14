---
kind: added
---
Host agent profiles accept two optional presentation-only top-level keys: `display_name` (trimmed free text, at most 128 bytes, no hidden or separator characters) and `audience` (`owner`, `agent` or `hidden`; absent means `owner`). Neither key enters the profile revision, so label edits never trigger `agent_profile_changed`. `host.inspect` reports them on each resolvable profile's `runtimes` entry, `session list`/`session inspect` carry the live label as `agent_display_name`, and `plugin profile list` shows only `owner` profiles by default (`--all` reveals the rest). ([#2150](https://github.com/zajca/pohunek/issues/2150))
