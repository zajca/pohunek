## Unreleased (2026-11-09, notification lifecycle cut, #2046, #2043, #2058)

- Protocol 5 is the only served version (announced break): the notification update, delete, policy and retention methods are gone, `pohunek notifications` has `list` and `watch` only, and sessions launched before the upgrade must be relaunched because their hooks announce protocol 4.
- The inbox holds `agent_blocked` and `approval_required` only. Wait for a turn end with `pohunek session wait` (`turn_completed`, or the new `turn_error`); see [agent skill](guides/agent-skill.md) and [sessions](concepts/sessions.md).
- Resolved attention records expire after the `[notifications]` `record_ttl_secs` of `service.toml` (default 14 days); the store is `notifications.jsonl` format 3.
- A terminal approval notice (`OSC 9`, `OSC 777 notify`, `OSC 99`) is a fallback attention source when neither a program-status record nor a hook speaks for the session.
