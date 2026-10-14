## Unreleased (2026-10-13, shell completion of workflow plugins, #325)

- Dynamic shell completion completes installed workflow plugin ids for
  `workflow inspect`, `enable`, `disable`, `uninstall` and `run` (`run` offers
  only enabled plugins), and the actions of the typed plugin for
  `workflow run <id>`, read from its installed `workflow.toml` after its files
  verify. The lookups read the local store only, without the daemon, within the
  completion deadline; they create, lock and write nothing, and a missing,
  corrupt or unsafe store completes nothing silently. A line with a remote
  `--host` completes no workflow plugin, matching `workflow_local_only`. Static
  scripts never read the store.
- `docs/cli.md`, `docs/features.md` and the `setup` and `workflow-plugins`
  guides describe it.
