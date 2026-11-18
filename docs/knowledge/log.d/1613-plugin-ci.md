## Unreleased (2026-11-18, reusable plugin CI, #1613)

- The reusable `.github/workflows/plugin-ci.yml` runs the plugin kit per row
  and platform in one `contents: read` job (Linux on `ubuntu-24.04`, macOS on
  the pinned `macos-15` label): lint, a double
  build with a byte compare, verify, detect-check when `corpus-dir` is set,
  smoke, and a version-bump check. The `release` row builds the kit from
  `zajca/pohunek` at `core-tag` and smokes against that tag's attested daemon
  archive, `nightly` builds from `main`, and `previous-minor` builds from the
  newest release of the previous minor (usable from core 0.45); the optional
  `smoke-script` runs inside
  the generic smoke on the same live isolated root after every generic check
  passed, with bare `pohunek` routed through the isolated `XDG_RUNTIME_DIR`
  and `POHUNEK_SMOKE_SESSION` exporting the live session id.
  The "Required CI" section of the plugin repository guide lists every input,
  every row and the tag scheme ([Plugin
  repositories](guides/plugin-repositories.md#required-ci)).
