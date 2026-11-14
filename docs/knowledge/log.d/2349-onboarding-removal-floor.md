## Unreleased (2026-11-14, onboarding removal floor, #2349)

- The [Onboarding](guides/onboarding.md) floor names
  `pohunek session retention sweep` with `--apply` and
  `pohunek plugin profile rm`. The Claude deny table has
  `Bash(pohunek session retention sweep *--apply*)` and
  `Bash(pohunek plugin profile rm *)` (always). Like the other `Bash` deny
  rules, they refuse the usual spellings of these commands and are not a
  security boundary.
- `cargo xtask docs check` fails (`onboarding-floor`) in three cases:
  - a `pohunek` command of the floor has no always-on deny rule;
  - a `--flag` the floor names for a command has no always-on rule carrying
    that flag;
  - a `Bash(pohunek ...)` deny rule names no floor command and is not one of
    the listed gap rules.
