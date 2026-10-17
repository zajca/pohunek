## Unreleased (2026-10-17, the macOS lane follows the pull request's test selection, #1225)

- On a pull request, the macOS lane runs only the lane tests the change can
  affect, and the knowledge bundle under `docs/knowledge/` no longer starts or
  selects the lane: its readers all run on Linux, where the selection still
  closes over them. A pull request whose lane selection holds no lane package
  starts no macOS lane job. Any dependency input, a failed selection, and
  every push, nightly and dispatch run still run the whole lane.
