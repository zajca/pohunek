## Unreleased (2026-11-11, setup hook selection, #2133)

- `pohunek setup --hooks none|all|<agent[:profile]>,...` limits the hooks setup installs to the selected agent homes (default `all`). The preview, the `--json` result and `--dry-run --json` list only the selected homes; an unknown agent or profile is a usage error with the code `setup_hooks_unknown`. See "Host setup" in `docs/cli.md`.
