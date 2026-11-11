## Unreleased (2026-11-11, Context pin capability check on the session.new connection, #2113)

- The Rust client checks `session_new_context_pin_supported` on the connection that carries `session.new` when the request has a `context_package_digest`, and fails with `daemon/session_new_context_pin_unsupported` without sending it. This covers an assistant launch whose daemon was replaced by an older release after the launch read its capabilities. Updated [runtime-packages](guides/runtime-packages.md).
