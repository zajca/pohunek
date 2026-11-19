---
kind: fixed
---
`pohunek-plugin-kit lint` lints context packages through the same
install-time descriptor loader a runtime package lints through: a context
package passes with `plugin lint ok: <package_id> <version> context`
(`--json` reports `"kind": "context"` with no `runtime_id`). The kind is
the `[context]` export the descriptor carries, never merely the absence of
a runtime: a commands-only package refuses with a named reason (the kit
does not support commands-only packages), and a package with neither a
runtime nor a context export refuses with a named reason.
The reusable `.github/workflows/plugin-ci.yml` reads the kind from the kit
(`--json`, never a caller input): a context package lints, double-builds,
verifies and version-bump checks, while the smoke and the detect-check are
skipped and setting any runtime-only input (`smoke-script`,
`upstream-install`, `corpus-dir`) fails the run. The reusable
`.github/workflows/plugin-release.yml` declares `environment: release` on
its publishing job, so the caller repository's environment protection
applies to every plugin release.
([#2412](https://github.com/zajca/pohunek/issues/2412))
