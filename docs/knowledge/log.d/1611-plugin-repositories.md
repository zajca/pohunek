## Unreleased (2026-10-26, plugins live outside core, #1611)

- New plugins (runtime packages and workflow plugins) are never added to this repository: an official plugin lives in `pohunekd/plugin-<name>`, a third-party plugin in its author's account, and a plugin becomes installable by id through a pull request to `pohunekd/pohunek-plugin-index`. The rule, naming, required CI, compatibility declaration, index admission, release snapshot and documentation split are in the [plugin repositories guide](guides/plugin-repositories.md). The kit, template and index are available from a later release.
- `cargo xtask docs check` fails a new top-level directory under `runtime-packages/` or `compat/` (`plugin-directories`); the four official runtimes pass until they move.
