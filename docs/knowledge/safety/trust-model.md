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
- Treat worker control sockets and journals as owner-private runtime authority.
  Never proxy a worker endpoint over NetBird, unlink a failed socket without
  proving unit inactivity and exact identity, or edit worker/runtime ids by
  hand.
- Never copy worker journal or structured-log diagnostics into shared reports
  without review. Journals intentionally omit prompt, input, terminal, and
  environment bytes; preserve that boundary when adding diagnostics.
- Preserve paired `origin_session_id`/`origin_daemon_id` request markers on
  ordinary, subscription, and dedicated SDK connections. The daemon uses them
  to deny exactly `session.stop`, `session.resume`, `session.remove`,
  `session.fork`, `session.resize`, `session.set_metadata`, `session.rename`, and
  `session.input` when they target the session hosting the caller. Do not strip
  or forge them to bypass `plugin_self_target_denied`. This is a narrow
  confused-deputy guard within the owner trust boundary, not per-session
  authentication or a general mutation policy. The lifecycle reports
  `session.report_agent`, `session.release_agent`, and
  `session.report_native_id` are deliberately allowed to target their own
  session; hooks require that path, and the public native-id report is the
  necessary local fallback when the owner-private worker claim cannot be
  delivered.
- Treat `notification.create` like every other control method: it is guarded by
  the owner-only daemon socket, not by per-session authentication. Any same-user
  process that can reach the socket can create notifications and influence
  attention dedupe within the single-operator trust boundary. A supplied
  `session_id` is shape-validated so it is bounded and contains no control
  characters, but it is not cryptographically authenticated to a session.
- For Hermes, never read, copy, or modify `HERMES_HOME` or `state.db` to infer
  a resumable session. Use only a valid reported native reference. Programmatic
  Hermes input is restricted to bounded text with LF/tab as the only controls
  and is denied while owner approval is visible; do not bypass that guard with
  raw attach bytes.

The assistant may write host or repo configuration when that is the requested
task, but it must stay inside the user's requested scope and respect the
boundaries in [secrets](secrets.md) and [repo `.pohunek/`](repo-pohunek.md).
