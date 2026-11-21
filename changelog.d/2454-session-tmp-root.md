---
kind: added
---
`service.toml` has a required `namespace.session_tmp_root` (schema 8): the directory below which each session's private `TMPDIR` is placed, so session scratch can live on another disk. `pohunek service install` and `pohunek service upgrade` of a schema-7 file record `<cache>/pohunek/session-tmp`, today's location, so nothing moves until you edit the key. The daemon never creates a configured root: while it is missing or unsafe a session launch fails with `session_tmpdir_failed` or `session_tmpdir_unsafe` naming it, the daemon keeps serving, and it logs one `session.tmp_root_unusable` warning at start. ([#2454](https://github.com/zajca/pohunek/issues/2454))
