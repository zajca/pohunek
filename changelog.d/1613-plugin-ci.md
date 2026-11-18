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
root after every generic check passed: bare `pohunek` reaches the isolated
daemon through the isolated `XDG_RUNTIME_DIR` (the isolated prefix is first
on the script's `PATH`), with `POHUNEK_SMOKE_SESSION` exporting the live
session id (`POHUNEK_SOCKET_PATH` is only information for hook-style
clients).
([#1613](https://github.com/zajca/pohunek/issues/1613))
