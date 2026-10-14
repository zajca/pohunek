## Unreleased (2026-10-12, Hermes package in runtime-packages, #147)

- `runtime-packages/hermes/` is the official Hermes runtime package:
  `runtime.toml`, `detect.toml`, the closed operator sidecar `operator.toml`
  and its payload in `operator/`. `compat/hermes/` holds the compatibility
  lock, the PTY goldens and the evidence notes. `cargo xtask hermes
  generate-skill` writes the operator skill and its checksum into the package.
- Releases build `pohunek-runtime-hermes-<version>.tar.zst` like every other
  official package. `compat/matrix.json` has Hermes rows for
  `x86_64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`, so the signed
  catalog authorizes the reserved `hermes` id on those targets and every Linux
  daemon archive ships `runtime/packages/hermes.tar.zst`. The macOS daemon
  archive carries no runtime package (#929). The release-evidence `rows` and
  `smoke` jobs install Python and uv for a provisioner lock, and the archive
  smoke restores every staged runtime from an untouched copy and verifies it
  before each Linux daemon archive.
- The built-in Hermes runtime and the CLI's compiled operator payload stay as
  the source for a host without an installed, enabled and selected official
  package.
- The regenerated operator skill changes the operator payload digest, so an
  existing operator install reports `outdated=true` in `integration status`
  until `pohunek integration update --agent hermes` for its Hermes profile
  rewrites it.
- The Linux CI job `hermes-package` follows the runtime package selection,
  builds its own binaries and the archive with `cargo xtask package build
  --evidence-dir`, matches a verified release baseline digest, runs the
  compatibility gate and the real-Hermes package test, and uploads
  `package-evidence-hermes` and `package-junit-hermes`. The macOS Hermes job
  also runs when the selection did not succeed or selects Hermes.
- Documented in the new `hermes-package` guide, the `hermes-operator` guide
  (regenerated Hermes skill), the `runtime-packages` guide,
  `concepts/release-bundle`, the index and the source map, and in
  `runtime-packages/README.md`, `compat/hermes/README.md`, `docs/install.md`,
  `docs/architecture.md`, `docs/development.md`,
  `docs/runbooks/hermes-operator-plugin.md` and
  `.agents/instructions/verification.md`.
