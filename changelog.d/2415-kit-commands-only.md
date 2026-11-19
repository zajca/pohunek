---
kind: added
---
The plugin kit and the reusable `plugin-ci.yml` support commands-only packages
(`package-commands-v1`, `[commands]` without `[runtime]` or `[context]`).
`pohunek-plugin-kit lint` loads them through the install-time loader and
reports `commands` as the kind, and `smoke` installs the archive on an
isolated daemon and runs every exported command once. `plugin-ci.yml` runs the
checks that apply to the kind and fails a caller that sets `corpus-dir`,
`upstream-install` or `smoke-script` for a package without a runtime, with a
named reason.
([#2415](https://github.com/zajca/pohunek/issues/2415))
