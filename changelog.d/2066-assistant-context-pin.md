---
kind: changed
---
A session started by `pohunek assistant` now keeps the assistant context package it reads: `plugin uninstall` of that package digest is refused with `package_referenced` while the session is live, lost or resumable, also after a newer release was installed and after a daemon restart, and removing the session or a retention sweep releases it. `session.new` takes the digest as `context_package_digest`, and `HostCapabilities` advertises `session_new_context_pin_supported`; against a daemon without the flag the launch stays unpinned. The metadata store schema is now 18, so a rollback to the previous release needs the `metadata.jsonl.pre-schema-17` copy the migration writes. ([#2066](https://github.com/zajca/pohunek/issues/2066))
