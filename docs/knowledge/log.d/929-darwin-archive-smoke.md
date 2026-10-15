## Unreleased (2026-10-14, network-isolated archive smoke on macOS, #929)

- The `smoke` job in `.github/workflows/release-evidence.yml` becomes a matrix
  with two runners: `ubuntu-latest` (every Linux daemon archive in a network
  namespace via unprivileged user namespaces or `sudo -n unshare`) and `macos-15`
  (the aarch64-apple-darwin daemon archive under `sandbox-exec` after
  `packaging/macos/verify-signed --adhoc`). On macOS, the consumer runs under
  `sandbox-exec` as a throwaway local user created per run and deleted afterwards.
  That requires passwordless `sudo -n`; without it, the smoke fails closed after
  archive and stage validation. Unix sockets are allowed only under the run's own
  roots and the throwaway user's Codex socket directory. The macOS smoke validates
  that the Apple Silicon daemon archive and every official runtime package it
  carries (`claude`, `codex`, `hermes`, `pi`) can be installed offline and serve
  agents on the target platform. Leftover processes are checked with a weaker
  detection on macOS (#1010).
- The macOS scenarios of `scripts/tests/test_smoke_archive` that require the
  isolated run are skipped on machines without passwordless sudo, and the
  macos-15 `smoke` job runs them with `POHUNEK_SMOKE_TESTS_REQUIRE_ISOLATED_RUN=1`.
  The host probe uses a permission-protected unix socket (mode 0755) and a
  world-connectable one (mode 0777) to verify that the sandbox profile itself
  denies unix sockets, independently of file permissions.
- Documented in `guides/hermes-operator`, `runbooks/install-on-macos`.
