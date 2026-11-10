---
kind: changed
---
The attention record of a Claude session comes from the OSC 7501 `blocked` record instead of the `Notification` hook: `agent_state` events carry an optional `blocked_kind` (`permission`, `question`, `auth`), a permission prompt is one `approval_required` record, and an AskUserQuestion or an MCP elicitation is now an `agent_blocked` record with `reason=question` (it was `approval_required`). A `blocked` record followed by `idle` or `done` (Esc, a denial, an automatic denial) resolves the attention record. The Claude integration (version 17) registers no `Notification` hook, and `pohunek integration install` removes the ones an earlier release wrote; run it, or `pohunek service upgrade`, to refresh the hooks.
