---
kind: added
---
`scripts/red-on-old --base <rev> --scratch-root <dir> --rows R1,R2 --scenario R1=<filterset>` runs each regression scenario on the old code (the changed non-test files restored to the merge base, in a scratch worktree; modules that a `#[cfg(test)] mod` declaration names count as test code) and on the new code, and prints the `| row | scenario | old code | new code |` table for the pull request body; a scenario that passes on both, a missing row and a build failure are rejected. ([#2178](https://github.com/zajca/pohunek/issues/2178))
