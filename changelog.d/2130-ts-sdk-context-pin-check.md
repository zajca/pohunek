---
kind: changed
---
The TypeScript SDK checks `session_new_context_pin_supported` on the connection that carries `session.new` with a `context_package_digest`, like `session_new_secret_env_supported` and `session_new_request_id_supported`. Against a daemon without the flag the call now fails with `session_new_context_pin_unsupported` (`ClientError.kind` `sessionNewContextPinUnsupported`) before the request is sent, instead of starting a session whose context files are not pinned. ([#2130](https://github.com/zajca/pohunek/issues/2130))
