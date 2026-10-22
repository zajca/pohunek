## Unreleased (2026-10-22, projector epoch seed retry, #1446)

- When the notification projector cannot read a session's stored records on
  its first input, it keeps that input and the ones after it in order and
  projects them after a retry (every second, and once more when the daemon
  shuts down), instead of dropping the derived notification. A session that
  keeps failing holds at most 256 inputs. See [sessions](concepts/sessions.md).
