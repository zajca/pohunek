## Unreleased (2026-11-02, keyed create recovery text and store scan, #1877)

- The outcome-unknown recovery of `pohunek session new` names the caller's `--request-id` when one was passed and otherwise offers the `--meta` lookup. The assistant launch keeps an error's own recovery and appends the request-id text; on a remote host a repeat answers `session_request_conflict`, whose `details.session_id` names the created session. The daemon's durable check for a key reads only the matching records (`Store::sessions_holding_request`).
