## Unreleased (2026-10-22, session metadata edits survive a restart, #1469)

`session.set_metadata` writes the edited metadata to the session's durable record for every session, also one without a native id, before the session shows it. A daemon restart therefore adopts the session with the edited metadata, and a write the store refuses fails the call with `session_store_failed` and leaves the metadata unchanged. See [sessions](concepts/sessions.md) and `docs/public-api.md`.
