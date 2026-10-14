## Unreleased (2026-10-14, worktree owner in project show, #950)

- `project.show` adds the optional `owner_session_id` to each worktree that
  pohunek owns: the session the worktree was created for. It stays after that
  session stops, while `session_id` still names only a live session running in
  the worktree. Both are omitted, never `null`, when they do not apply. The
  human `project show` output flags the owner as `owner=<id>`.
