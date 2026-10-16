## Unreleased (2026-10-16, CI evidence by tested tree, #1215)

- Every `ci.yml` run records the git tree its jobs check out as a
  `tested-tree-<tree>` artifact. `scripts/ci-evidence`, and with it
  `scripts/release`, accept a `workflow_dispatch` run of this repository as
  main or nightly evidence for another commit with the same tree, when the
  recorded tree matches and every required job passed. A completed run on the
  commit itself still decides, and pull-request, push, and schedule runs never
  match by tree. The report says whether the evidence matched by `sha` or by
  `tree`.
