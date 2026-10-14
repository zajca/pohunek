## Unreleased (2026-10-12, Hermes release consumer driver, #147)

- The release consumer suite has a `hermes` driver. It installs the Hermes
  package from the catalog, runs `integration install --agent hermes` into a
  private Hermes home and requires the install and status results to report
  `provenance` `source=package` with the archive's package id and digest, then
  drives a real pinned Hermes through one turn against a loopback provider,
  stops it and resumes it by the conversation id the plugin hook reported. The
  Linux Hermes package CI job (`hermes-package`) runs it in row mode on the
  archive built from `runtime-packages/hermes`, after restoring and verifying a
  pristine stage. Documented
  in `docs/development.md` ("Release consumer suite") and the source map.
