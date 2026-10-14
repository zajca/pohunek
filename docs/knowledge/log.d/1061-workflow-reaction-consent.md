## Unreleased (2026-10-14, workflow reactions in the consent review, #1061)

- The `pohunek workflow install` consent review and `pohunek workflow inspect`
  show every event reaction: its name, its events, its argv with `argv[0]`
  resolved, and its requested timeout (`reactions[]` in `--json`). A plugin
  that declares only reactions now shows the commands it will run before
  `--yes`.
- `pohunek service uninstall`, refused while the reaction dispatcher unit is
  recorded, names the plugins to disable (the enabled plugins with reactions,
  or every installed plugin when none keeps the dispatcher) instead of a
  `pohunek workflow disable the reaction dispatcher` command that does not
  exist.
- Under a service installation of another pohunek version, disabling or
  uninstalling a plugin with reactions succeeds: the last one removes the
  dispatcher unit, as for a plugin's own units, and while another enabled
  plugin reacts the installed dispatcher is kept unchanged (it refuses the
  runs of a disabled plugin). Only installing the dispatcher needs this
  version.
