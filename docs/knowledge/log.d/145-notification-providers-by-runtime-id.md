## Unreleased (2026-10-12, notification providers by runtime id, #145/#146)

- Notification source ranking follows the hook schemas: a provider ranks as a
  provider hook exactly when a registered hook schema lists it as an identity
  provider (today `shell`, `codex`, `claude`, and `hermes`, matched
  case-sensitively). Hook admission on `notification.create` checks those
  providers against the session's hook schema and additionally admits a
  session's own runtime, which does not change its rank. Hermes and shell hook
  reports therefore rank above user/external reports: inside the debounce
  window a pending Hermes attention report is kept over a later user report
  for the same session, and inside the dedupe window it upgrades a projector
  record and dedupes against an earlier provider-backed record.
- The default notification policy has no per-provider entries; every provider
  uses the base `enabled` flags until an override is set. A policy file the
  daemon already persisted keeps the entries it holds, so on such a host a
  `--provider default` toggle does not reach `codex`, `claude`, or `hermes`
  until their own entries are changed too (`policy set --provider <runtime-id>`).
  The SDK testkit fixture daemon serves the same empty default.
- `notification.create` judges a session hook claim by the provider name the
  record is stored and ranked under, and refuses with `hook_not_admitted` a
  raw provider that only normalizes to a hook provider (for example
  `"hermes\u0007"` or `" codex"`), so such a spelling cannot bypass the
  session's hook schema.
- `pohunek notifications policy set --provider` accepts `default` or any valid
  runtime id, installed or not (for example `pi`); a new override starts from
  the base flags. Documented in `concepts/sessions`, the `debug-daemon`
  runbook, `docs/cli.md`, and
  `docs/public-api.md`.
