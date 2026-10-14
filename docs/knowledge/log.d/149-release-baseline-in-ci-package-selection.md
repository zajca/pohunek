## Unreleased (2026-10-12, release baseline in CI package selection, #149)

- The `package-selection` CI job downloads release `v<workspace version>`'s
  runtime catalog, attestations and package archives with `gh release
  download` and runs `cargo xtask package baseline --json`. A pull request
  skips a package job only when the package and the core are unchanged and the
  signed release verifies the package; a failed download makes every package
  missing and runs every job, and only an unreadable checkout fails the
  selector. Each package job requires its rebuilt archive to hash to a verified
  baseline digest; a difference fails the job as a non-reproducible build.
  Release workflows never consult a baseline. Documented in
  `concepts/runtime-package-archive` ("Release baseline") and
  `docs/development.md`.
