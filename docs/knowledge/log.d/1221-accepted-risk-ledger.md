## Unreleased (2026-10-19, durable acceptance of upgrade risk, #1221)

`pohunek service upgrade --accept-runtime-loss` now records what it accepted.
The new daemon's preflight binds each at-risk verdict of a loaded record to an
`acceptance_scope` (`v1:` and a digest of the session, generation, worker
instance, launch pin, native reference and capabilities), and the completed
upgrade writes the scoped verdicts of its final gate to
`<state>/pohunek/service-accepted-risks.json`. At the next upgrade the identical
session, list, verdict, code and scope is reported as previously accepted and no
longer blocks; a new session, a changed scope or a verdict without a scope still
needs the flag. Accepting never changes a session or retires its recovery.
`service check` reads the file and writes nothing; a ledger for another version,
namespace, store, scheme or schema is ignored with a warning, and an unsafe or
corrupt one fails with `service_accepted_risks_invalid`. The install transaction
record is schema 5 and schema 4 records still load. See
[the update runbook](runbooks/update-after-release.md#upgrade-preflight) and
`docs/public-api.md`.
