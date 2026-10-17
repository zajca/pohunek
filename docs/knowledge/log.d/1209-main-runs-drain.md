## Unreleased (2026-10-17, pushes to main drain instead of cancelling, #1209)

- The `ci.yml` concurrency group for pushes to `main` no longer cancels the
  run in progress. Each started main run completes, and GitHub keeps only the
  newest push pending behind it, replacing an older pending push. `main` gets a
  verdict at most one merge behind its tip.
- A new push to a pull request still cancels that PR's run in progress.
  Schedule and dispatch runs keep their own groups.
- This revises #477 decision 4 ("a newer merge cancels the post-merge run in
  progress"). The coordinator no longer pauses merges twice a day for a
  `main` checkpoint.
