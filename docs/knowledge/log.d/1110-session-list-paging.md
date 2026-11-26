## Unreleased (2026-11-26, session.list paging, #1110)

- `session.list` accepts `limit` and `cursor` (the id of the last session of the
  previous page) and orders sessions by id. A result that does not fit one control
  line answers `runtime/session_list_too_large` instead of a frame the client
  refuses; both parameters and the code are additive and need no protocol bump.
- The Rust SDK `Client::session_list` and `pohunek session list` page through the whole set,
  200 sessions at a time, and halve the page when the daemon reports it too large.
  The TypeScript SDK does not page yet (tracked in #2570).
  See [Sessions](concepts/sessions.md).
