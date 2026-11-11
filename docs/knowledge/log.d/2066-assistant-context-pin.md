## Unreleased (2026-11-11, Assistant context package pin, #2066)

- A session launched by `pohunek assistant` pins the digest of the assistant context package it reads by path: `session.new` takes `context_package_digest` (advertised by `session_new_context_pin_supported`), and `plugin uninstall` of that digest is refused with `package_referenced` while the session is live, lost or resumable, also after a newer release was selected and after a daemon restart. `session rm` or retention releases the pin. Updated [runtime-packages](guides/runtime-packages.md).
