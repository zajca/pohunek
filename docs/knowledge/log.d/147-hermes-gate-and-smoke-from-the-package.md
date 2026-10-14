## Unreleased (2026-10-12, Hermes gate and smoke from the package, #147)

- Every `pohunek integration` result for Hermes reports `provenance`: the
  source the installed plugin's ownership marker records, as
  `{"source":"package","package_id":"pohunek.runtime.hermes","package_digest":"sha256:<hex>"}`
  or `{"source":"embedded"}`, and `null` when no managed plugin is installed
  (also after `uninstall`). The human form prints a `source:` line. Documented
  in the `hermes-operator` guide (regenerated Hermes skill).
- `cargo xtask hermes compatibility` takes `--bin-dir` (the directory of
  `pohunek`, `pohunekd`, and `pohunek-sessiond`) and `--package` (the Hermes
  runtime package archive, built by `scripts/build-hermes-package`) in place of
  `--pohunek-bin`. It copies the binaries into a private prefix with a
  throwaway trust anchor, signs a catalog for exactly that archive, starts the
  copied daemon, installs the package with `pohunek plugin install --catalog`,
  and fails unless install, status, and doctor report the archive digest as
  the provenance (`source=embedded` fails). `cargo xtask hermes
  throwaway-catalog` writes such a catalog and its anchor for the release
  smoke. `packaging/smoke-hermes-plugin-release` ships in the daemon archive
  and takes the extracted daemon archive, the package archive, the catalog, the
  anchor, and the Hermes executable. Documented in the source map.
