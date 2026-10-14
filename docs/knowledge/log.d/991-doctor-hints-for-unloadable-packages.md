## Unreleased (2026-10-13, doctor hints for unloadable packages, #991)

- `pohunek plugin profile list` reports a pinned profile whose pinned digest is
  installed but cannot be loaded as `package_unavailable`, when the pin names
  that digest's package. The detail names `pohunek plugin doctor` only; the
  migration is not offered because the pin already names the digest. A
  loadable package that serves another runtime, or a pin naming another
  package than the digest's, is still `needs_migration`.
- An unpinned profile over an official alias (`codex`, `claude`, `hermes`)
  whose official package (`pohunek.runtime.<alias>`, matched by package id) is
  installed but was left out of the runtime registry after a rebuild is refused with `runtime_incompatible` (it was
  `runtime_not_installed`). Its recovery names `pohunek plugin doctor` and the
  migration after the repair, not the install, which the package's root or
  identity check refuses. The refusal is decided from the package report the
  registry rebuild records; no protocol code was added.
- The runtime packages guide, the runtime package recovery runbook and
  `docs/public-api.md` list the new states and refusal.
