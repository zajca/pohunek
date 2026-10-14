## Unreleased (2026-10-14, client creates daemon directories owner-private, #962)

- `pohunek host discover` creates every missing ancestor of its cache
  directory with mode 0700, so `XDG_CACHE_HOME/pohunek` no longer appears as
  0755 and the daemon no longer refuses worker starts with
  `trusted entry has mode 0o755, expected 0o700`.
- The knowledge bundle materializer (`pohunek assistant` locally, and the
  daemon's `assistant.materialize`), the assistant's runtime snapshot
  directory under `XDG_RUNTIME_DIR/pohunek`, and the worker migration manifest
  writer create their missing ancestors the same way.
- A directory an earlier release already created as 0755 is not repaired:
  the daemon still refuses it, naming the path and the expected mode.
