## Unreleased (2026-10-15, assistant request from stdin, #769)

- `pohunek assistant --request-stdin` reads the free-form request from stdin,
  so a caller such as a terminal UI keeps it out of the process argv. The text
  is bounded UTF-8 under the `session new --input-stdin` rules, with
  surrounding whitespace trimmed. It lands in the opening prompt where
  positional words go, so both forms print the same `--json` result. An empty
  or invalid request fails with `cli_usage` before any daemon is started or
  dialed. Request words and the flag are mutually exclusive.
- A remote launch with a stdin request needs `--yes`
  (`confirmation_required`), since stdin cannot also answer the prompt. A
  launch that needs `--yes` (`--json` or a stdin request) is now refused
  before the CLI starts or dials any daemon; `--print-prompt` still needs no
  confirmation.
- `pohunek assistant --request-timeout-ms` and `--meta key=value` work as on
  `session new`: the deadline applies to the creation request only, and the
  metadata is written with the session.
- `docs/public-api.md` documents the creation outcome. The CLI sends one
  `session.new` and never resends it. Only the CLI's own refusals before that
  request prove that nothing was created. `request_timeout`, transport errors
  and typed daemon errors leave the outcome unknown. A caller-chosen `--meta`
  key with `session list --meta-eq` finds the session, or the duplicates of a
  retry, since `session.new` has no idempotency key.
