---
kind: changed
---
The Rust client checks `session_new_context_pin_supported` on the connection that carries `session.new`, like `session_new_secret_env_supported` and `session_new_request_id_supported`. A `pohunek assistant` launch whose daemon was replaced by an older release after the launch read its capabilities now fails with `session_new_context_pin_unsupported` before the request is sent, instead of starting a session whose context files are not pinned. ([#2113](https://github.com/zajca/pohunek/issues/2113))
