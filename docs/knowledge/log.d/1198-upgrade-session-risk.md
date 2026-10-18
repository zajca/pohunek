## Unreleased (2026-10-18, upgrade preflight session risk, #1198)

Upgrade and rollback refusals count live and stopped at-risk sessions separately
and label each listed session. The upgrade and check help describe both groups.
An owner-requested stop or removal is not itself an adoption loss, whether the
runtime is alive or dead (#1221 D1+D2); retained native recovery of a stop is
still judged, and process identity and foreign-runtime protections remain
conservative. See [the upgrade preflight runbook](runbooks/update-after-release.md#upgrade-preflight).
