## Unreleased (2026-10-17, ci-evidence reads its config from the checked commit, #1243)

- `scripts/ci-evidence --sha S` takes its required jobs and steps from
  `scripts/ci-evidence.json` in the tree of S, read from the local checkout when
  it holds S and otherwise from the contents API, instead of the copy next to
  the script. A check run from a branch whose config differs from S's now
  applies S's own requirements in both directions. A commit whose config cannot
  be read refuses the evidence; `--config` still names a file to use instead.
