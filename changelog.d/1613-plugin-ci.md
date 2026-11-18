---
kind: added
---
The reusable `.github/workflows/plugin-ci.yml` gives plugin repositories the
required CI: one `contents: read` job per row and platform that lints,
double-builds with a byte compare, verifies, detect-checks (when `corpus-dir`
is set), smokes and version-bump checks the package. Rows pin the core the kit
is built from: `release` (the kit from `core-tag`, smoked against that tag's
attested daemon archive), `previous-minor` (the kit from the newest release of
the previous minor) and `nightly` (the kit from `main`); the optional
`smoke-script` runs inside the generic smoke on the same live isolated
root, with `POHUNEK_SOCKET_PATH` and `POHUNEK_SMOKE_SESSION` exported.
([#1613](https://github.com/zajca/pohunek/issues/1613))
