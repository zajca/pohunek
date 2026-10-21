## Unreleased (2026-10-21, tolerant service.toml, #1401)

`service.toml` accepts keys and tables this version does not know instead of
failing the load, so an additive setting no longer forces a schema bump and a
daemon older than the file still starts. The daemon logs one
`service_config.unknown_key` warning per key (path, line and file, never the
value); `pohunek service status`, `service check`, `service upgrade` and the new
`service_config_unknown_keys` doctor check report them with a "did you mean"
hint. The installer keeps them when it rewrites the file. Unknown keys in
`[namespace]` and `[environment]` stay hard errors, `schema_version` stays 6,
and the schema 2 to 5 upgrade sources stay strict.
An interrupted schema 2 to 5 migration resumes when the rewritten target differs from the expected one only in its header comment; a different body is still refused with `service_config_backup_invalid`.
