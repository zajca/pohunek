## Unreleased (2026-11-21, configurable session temporary root, #2454)

- `service.toml` is schema 8 with a required `namespace.session_tmp_root`, the
  root of every session's private `TMPDIR` (`<root>/<store key>/<session id>`). An
  upgrade from schema 7 and a fresh install record `<cache_dir>/session-tmp`; the
  installer creates only that default root, the daemon never creates a configured
  one. A missing root fails the launch with `session_tmpdir_failed`, an unsafe one
  with `session_tmpdir_unsafe`, both naming the root; the daemon keeps serving and
  logs one `session.tmp_root_unusable` warning at start. See
  [Temporary directory](concepts/sessions.md#temporary-directory) and
  [Setup](guides/setup.md).
