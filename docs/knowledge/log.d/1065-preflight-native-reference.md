## Unreleased (2026-10-15, upgrade preflight native reference, #1065)

- `pohunekd upgrade-preflight` runs the native reference check resume runs
  before it relaunches a stopped session, and reports a refusal with resume's
  own code and message.
- A stopped session whose conversation is absent from its declared store
  (`agent_native_reference_missing`), or whose stored reference is malformed, is
  `already_not_resumable` when the upgrade keeps the check. A reported reference
  keeps its check only when it is pinned to a package digest, or when a 0.34.0 to
  0.34.2 store (schema 5) is adopted into the official package whose check equals
  the v0.34.2 golden (`V034_GOLDENS`, guarded by a test). Other reported references
  are `would_lose_recovery` with the reason. `service
  upgrade` then does not require `--accept-runtime-loss` for it, and `service
  check` still lists it.
- A relocated store (`agent_native_store_relocated`) or a store-condition
  refusal is `would_lose_recovery` with resume's code: the running release may
  detect those differently, so the preflight cannot prove it refuses too.
- A session the upgrade adopts from the built-in runtime stays
  `would_lose_recovery` when its reference check fails, unless the v0.34 identity
  above proves the check.
- The preflight budget bounds the native reference checks as well as the version
  probes. A stopped session whose reference is not checked before the budget is
  spent is `would_lose_recovery` with `reference_unverified_within_budget`; the
  detail gives the count of such sessions. `service upgrade` and `service check`
  take `--preflight-budget <SECONDS>` (1 to 300, default 20). The CLI's preflight
  deadline grows with it. The flag reaches only this release's upgrade preflight, not
  a rollback's. An installer user runs `<archive dir>/pohunek service check
  --preflight-budget <SECONDS>` and then `<archive dir>/pohunek service upgrade --from
  <archive dir> --preflight-budget <SECONDS>`; the installer script cannot pass it.
- The upgrade preflight judges with the allowlisted variables the service manager
  gives the daemon job (the systemd user manager's environment on Linux, the
  launchd domain environment on macOS), so a store variable such as
  `CLAUDE_CONFIG_DIR` is read as the job reads it. Variable values never appear in
  the report or in errors.
- A job that records its `PATH` no longer fails when its systemd user manager cannot be
  reached: the check runs without the manager's variables and reports a `warning`. A
  manager that answers unusably, or a job that takes its `PATH` from the manager, still
  fails closed.
- A runtime whose native reference is not checked (`check = "none"`) is not held by the
  preflight budget, since it reads nothing.
- Only an absent user bus (no address, or nothing listening at the socket) counts as an
  unreachable service manager. A bus that refuses the client stays an error. A degraded
  judgement's warning reaches every upgrade, rollback and check output, and a refusal that
  the judgement produced carries it too.
- A malformed session bus address is a configuration error and fails closed; only an
  absent socket (not found, or refused) means no service manager is reachable.
- Every refusal of an upgrade, rollback or check gate that a degraded judgement produced
  carries that judgement's warning.
- An upgrade or rollback whose judgement ran without the service manager's variables and
  covered a stopped session is refused with `service_manager_variables_unread` unless
  `--accept-runtime-loss` is given, since that session's reference was checked at the default
  store location. Without stopped sessions no verdict depends on those variables, so the
  gate passes with the warning. `service check` only warns.
  On macOS a missing launchd `gui/<uid>` domain (no GUI session) degrades the same way.
