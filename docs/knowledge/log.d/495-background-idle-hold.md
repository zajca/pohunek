## Unreleased (2026-11-02, background idle hold, #495)

- Idle stop no longer stops an opted-in session while its `background` snapshot lists live work of kind `subagent`, `workflow`, `shell`, `monitor` or `mcp_task`. `activity` keeps its turn meaning; teammates, provider-internal kinds and bare `running` subagent rows (Codex) do not hold. See [Idle stop](concepts/sessions.md#idle-stop).
- When the last live item disappears the idle clock restarts from that moment, so the stop comes a full limit later. The hold has no age limit.
- No protocol, store or worker change; clients render `background` next to `activity`.
