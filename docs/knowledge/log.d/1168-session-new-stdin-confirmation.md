## Unreleased (2026-10-20, session new stdin confirmation, #1168)

`pohunek session new --input-stdin` on a remote `--host` without `--yes` now
fails with `confirmation_required` before it reads stdin or contacts the
daemon. Stdin already holds the input, so the confirmation prompt used to read
EOF and report `confirmation_declined`. The rule matches
`pohunek assistant --request-stdin`. See [sessions](concepts/sessions.md).
