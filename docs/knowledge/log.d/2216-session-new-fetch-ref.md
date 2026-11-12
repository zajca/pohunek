## Unreleased (2026-11-12, session new fetch_ref, #2216)

- `session.new` with `branch` accepts an optional `fetch_ref` (`source_ref` plus
  `commit`): the daemon fetches one extra ref from `origin` into the worktree's
  repository without checking it out, before any hook runs, and keeps it as
  `refs/pohunek/sessions/<session_id>/fetched` for as long as the worktree
  binding lives (a failed or refused request leaves nothing behind; `FETCH_HEAD`
  gains no entry from it). A GitHub pull-request head is
  `refs/pull/<n>/head`; the daemon has no forge-specific knowledge. The commit
  is a full lowercase hex id the fetched ref must peel to. The first response
  reports `fetched_ref` (`ref`, `source_ref`, `commit`); a replay and the
  stored session never carry it. Clients check
  `HostCapabilities.session_new_fetch_ref_supported` first (the Rust client and
  the TypeScript SDK refuse with `session_new_fetch_ref_unsupported` without
  sending). See [Worktrees](concepts/worktrees.md#fetching-one-extra-ref).
