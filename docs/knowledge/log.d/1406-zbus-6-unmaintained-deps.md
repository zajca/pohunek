## Unreleased (2026-10-23, zbus 6 and unmaintained dependencies, #1406)

- A session bus address with the `unixexec:` transport (for example in
  `DBUS_SESSION_BUS_ADDRESS`) is now rejected as an unusable address: the
  systemd supervisor is built on zbus 6, which compiles that transport only
  behind a feature the build leaves off. Before, the address ran the named
  program. `pohunek service` reports the condition without quoting the address.
- The embedded agent `SKILL.md` is regenerated: its frontmatter `description`
  is double-quoted instead of single-quoted (same value), so the embedded bytes
  and the `content_sha256` that `agent-skill` reports change.
- `.cargo/audit.toml` carries four dated (2026-10-23) ignores for
  unmaintained-crate advisories: RUSTSEC-2026-0370, -0371 and -0372 (zvariant,
  zbus_names, zvariant_derive; only through `keyring` 3 and its `secret-service`
  4, tracked in #1504) and RUSTSEC-2026-0249 (smartstring; only through the
  optional `ts` bindings feature, tracked in #1505).
