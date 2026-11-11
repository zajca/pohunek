---
kind: added
---
The signed plugin index gains schema 2 (`official-index-v2.json`), whose entries have a `kind`: `runtime`, as before, or `context` for a context package such as `pohunek.context.assistant`. The kinds never authorize each other, and `pohunek.context.*` ids are valid only for `context` entries. The schema 1 document is unchanged, so every shipped host keeps resolving runtime plugins. `pohunek plugin install pohunek.context.<name>` resolves from the schema 2 index, and a daemon that reads it reports `package_install_index_context_supported`. `cargo xtask index build` and `index check-entry` handle `context` entries, `plugin prove` proves runtime entries only. The `gaps` of the release daemon's report name the profile of a profile config-home gap in `profile`.
