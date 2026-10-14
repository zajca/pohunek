## Unreleased (2026-10-14, Hermes macOS CI on Hermes inputs only, #1050)

- On a pull request, the CI job `hermes-compatibility-macos` ("pinned Hermes
  compatibility (macOS arm64, model-free)") runs only when its `hermes_macos`
  input filter matches. A runtime package selection that picks Hermes, which
  every core change does, no longer starts it. Pushes to `main`, the nightly
  schedule and `main`/`nightly` dispatch still run it, and the Linux
  `hermes-package` job keeps following the selection.
