---
kind: added
---
`pohunek session new` accepts `--fetch-ref <ref>` with `--fetch-commit <sha>`: it fetches one extra ref from `origin` into a worktree session's repository without checking it out, reported as `fetched_ref` in `--json` output (a `fetched: <source> -> <ref> at <commit>` line for humans). A host whose daemon does not advertise `session_new_fetch_ref_supported` is refused with `session_new_fetch_ref_unsupported` before the session is created. ([#2216](https://github.com/zajca/pohunek/issues/2216))
