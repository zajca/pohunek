## Unreleased (2026-10-13, workflow plugin store and consent install, #148)

- New `pohunek workflow install|list|inspect|enable|disable|uninstall`. A
  workflow plugin is a package archive with a strict `workflow.toml` manifest
  (schema 1, `[plugin]` and `[actions.<name>]` with argv arrays), trusted by the
  digest the owner supplies with `--sha256` and installed only with `--yes`
  after a review of every action's argv.
- The CLI keeps workflow plugins in its own registry under
  `<state>/pohunek/workflow-plugins/` with per-plugin state and config
  directories; the daemon never reads them, and no protocol method, event or
  SDK type was added. Every workflow command is local-only.
- New error codes `workflow_local_only`, `workflow_archive_invalid`,
  `workflow_digest_mismatch`, `workflow_descriptor_invalid`,
  `workflow_plugin_unknown`, `workflow_plugin_installed`,
  `workflow_root_invalid`, `workflow_root_intact`, `workflow_store_busy`,
  `workflow_purge_incomplete`, `workflow_dirs_overlap` and
  `workflow_store_failed`; install without consent returns `consent_required`.
- Changing commands hold `store.lock` in the store root. `uninstall --purge`
  also purges the kept directories of an id without a record, and
  `uninstall --remove-modified` removes installed files that fail
  verification. Config directories live under
  `<config>/pohunek/workflow-plugin-config/`.
- A committed uninstall whose files could not all be deleted, an install whose
  durability is unconfirmed, and a stuck leftover of an earlier purge are
  reported as `warnings` on success; they never decide the result. A purge
  leftover of the plugin being purged keeps the purge failing
  (`workflow_purge_incomplete`). An install whose outcome cannot be read back
  fails with `workflow_install_uncertain` and keeps the plugin directories.
- Running actions, reactions, jobs and services are not yet available.
- Documented in the new `workflow-plugins` guide, the `runtime-packages` guide,
  `safety/trust-model`, the index and the source map, and in
  `docs/architecture.md`, `docs/cli.md`, `docs/public-api.md` and `README.md`.
