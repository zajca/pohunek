---
kind: added
---
`session.new` accepts an optional `fetch_ref` (`source_ref` plus `commit`) that fetches one extra ref from `origin` into a worktree session's repository without checking it out, pinned under `refs/pohunek/sessions/<session_id>/fetched` until the worktree binding is dropped. The first response reports it as `fetched_ref`; clients refuse to send it to a daemon that predates the `session_new_fetch_ref_supported` capability instead of silently dropping it. ([#2216](https://github.com/zajca/pohunek/issues/2216))
