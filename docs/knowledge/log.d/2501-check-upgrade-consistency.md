## Unreleased (2026-11-26, service check and assistant upgrade consistency, #2501)

- `pohunek service check` refuses a pending `refresh-path` record over a `service.toml` of
  an earlier schema with `service_refresh_config_schema` and the hint `service upgrade`
  gives, instead of planning a clean roll back. Both read the file through
  `refresh::load_installed_config`; the check does so last in `finish_check`, after the
  adoption preflight, as the upgrade does.
- `setup_assistant::decide` is the one decision about the listed assistant package for
  `pohunek setup` and `pohunek service upgrade`. A listed digest the daemon holds revoked
  is refused by both with the same text (the upgrade reports `failed`), a faulted one is
  installed again by both, and an enabled but unselected one is held with `plugin select`
  only; a disabled one names `plugin enable` and `plugin select`. A `package.list` error
  is a gap in setup, as it is a `failed` result in the upgrade step.
- The release upgrade harness asserts the `assistant` object of the real
  `service upgrade --from --no-assistant-package` run. The test-only `upgrade_assistant`
  library module stays public: the harness stages neither an older selected package, nor a
  revoked or faulted digest, nor a stopped daemon, nor a loopback index.
