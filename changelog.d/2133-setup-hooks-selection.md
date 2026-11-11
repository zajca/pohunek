---
kind: added
---
`pohunek setup --hooks none|all|<agent[:profile]>,...` installs hooks only into the agent homes you select, so a client that asks per home can use one `setup --yes`. The default stays `all`. The preview, the result and `--dry-run --json` list only the selected homes, `--hooks none` leaves every agent home untouched, and an agent or profile the host has no home for fails with the usage exit status and the code `setup_hooks_unknown` before any change. The README of the runtime packages now states the Codex floor from `runtime.toml`, 0.164.0.
