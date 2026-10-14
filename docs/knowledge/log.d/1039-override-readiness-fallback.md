## Unreleased (2026-10-14, override readiness fallback, #1039)

- A host profile over a `screen_idle` base whose detection-manifest override
  has no idle rule on a screen region loads instead of failing with
  `invalid_profile`. Its sessions use `bracketed_paste` readiness, and
  `pohunek doctor` warns with the fix.
- The runtime packages guide, the Claude package guide and the update runbook
  describe the rule.
