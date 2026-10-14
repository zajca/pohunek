## Unreleased (2026-10-12, Hermes lock shape, #147)

- `compat/hermes/compatibility-lock.json` has the official-package lock shape:
  `schema`, `package` (`pohunek.runtime.hermes`), `runtime` (`hermes`), an
  `upstream` object of kind `provisioner` that holds the release and its source
  provenance, `supported` (`[0.20.0, 0.20.1)`, equal to the package descriptor's
  version probe range) and `verified_with`; the launch contract is `launch`.
  `cargo xtask compat attest` and `compat verify` read it like the npm locks
  (`compat matrix-check` once `runtime-packages/hermes/` exists), and
  `compat stage-upstream` refuses it for having no `upstream.npm`.
  Documented in `concepts/upstream-staging` and `compat/hermes/README.md`.
