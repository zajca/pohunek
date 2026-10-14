## Unreleased (2026-10-14, network-isolated archive smoke on macOS, #929)

- The `smoke` job in `.github/workflows/release-evidence.yml` becomes a matrix
  with two runners: `ubuntu-latest` (every Linux daemon archive in a network
  namespace via unprivileged user namespaces or `sudo -n unshare`) and `macos-15`
  (the aarch64-apple-darwin daemon archive under `sandbox-exec` after
  `packaging/macos/verify-signed --adhoc`). The macOS smoke validates that the
  Apple Silicon daemon archive and every official runtime package it carries
  (`claude`, `codex`, `hermes`, `pi`) can be installed offline and serve agents
  on the target platform. Leftover processes are checked with a weaker detection
  on macOS (#1010).
- Documented in `guides/hermes-operator`, `runbooks/install-on-macos`.
