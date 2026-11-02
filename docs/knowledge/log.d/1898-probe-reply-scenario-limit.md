## Unreleased (2026-11-02, limit of the attached probe-reply scenario, #1898)

- The Claude package guide states that the real-Claude scenario `an_attached_clients_probe_reply_never_reaches_a_real_claude` cannot detect an unstripped client reply: it passes with the worker's stripping disabled on Claude 2.1.309 and 2.1.310, and the guard is the session-worker test `an_attached_terminal_reply_to_an_answered_probe_is_dropped`. See [claude-package.md](guides/claude-package.md).
