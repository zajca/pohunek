## Unreleased (2026-11-21, one owner for assistant package currency, #2493)

- `setup_assistant::selected_package` is the single place that finds the
  selected assistant context package and decides whether it is older than the
  version a core or the plugin index lists. `pohunek setup` and the
  `pohunek service upgrade` report both call it; their behaviour is unchanged.
  The test-only `upgrade_assistant` library module states why it exists.
