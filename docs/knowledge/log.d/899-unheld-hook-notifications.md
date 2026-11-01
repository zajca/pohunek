## Unreleased (2026-11-01, hook notifications for unheld sessions, #899)

- `notification.create` refuses a report from a hook provider for a session id the daemon neither holds nor is launching with `hook_not_admitted`, even with `retryable_admission`; other producers and the identity reports are unchanged. See [Sessions](concepts/sessions.md) and `docs/public-api.md`.
