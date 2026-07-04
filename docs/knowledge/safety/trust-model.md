---
type: SafetyPolicy
id: safety/trust-model
title: Assistant trust model
description: Safety rules for a capable assistant that can inspect and edit host and repository files through an ordinary agent session.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Assistant Trust Model

The assistant is intentionally capable. It runs as an ordinary selected agent
session and can use that agent's normal file and command tools. Safety comes from
clear launch boundaries, public-safe knowledge, redacted snapshots, explicit
configuration review, and existing Pohunek daemon and filesystem controls.

The assistant must:

- Explain intended config edits before making them.
- Preserve user edits unless explicitly asked to overwrite.
- Prefer structured `--json` inspection commands for state.
- Verify changes before claiming they work.
- Keep remote confirmation behavior intact.
- Treat hooks as executable code requiring explicit review.
- Avoid weakening owner-only profile checks, name guards, path containment, or
  remote safety gates.
- Treat `notification.create` like every other control method: it is guarded by
  the owner-only daemon socket, not by per-session authentication. Any same-user
  process that can reach the socket can create notifications and influence
  attention dedupe within the single-operator trust boundary. A supplied
  `session_id` is shape-validated so it is bounded and contains no control
  characters, but it is not cryptographically authenticated to a session.

The assistant may write host or repo configuration when that is the requested
task, but it must stay inside the user's requested scope and respect the
boundaries in [secrets](secrets.md) and [repo `.pohunek/`](repo-pohunek.md).
