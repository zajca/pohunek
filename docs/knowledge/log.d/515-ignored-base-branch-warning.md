## Unreleased (2026-10-15, an ignored base branch is reported, #515)

- `session new --branch H --base-branch B` checks out an existing local `H` as
  it is, so `B` did not decide the worktree's commit, and nothing said so. When
  `--base-branch` is explicit and the existing branch is at another commit than
  the base (for a remote base, its fetched commit), the result now carries a
  `base_branch_fallback` warning whose `detail` names both commits. The kind is
  the existing one, so released clients still read the session.
- A branch a worktree creates from a base gets no upstream, as before; this is
  now documented, with `git push -u origin <branch>` as the way to set one.
