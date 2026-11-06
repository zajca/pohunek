## Unreleased (2026-11-06, checkout trust refusals, #1985)

- The [runtime packages guide](guides/runtime-packages.md#trusted-checkout-arguments) and `docs/public-api.md` list the on-disk checks that withhold trusted checkout arguments (case-folded entries, a declared path opened under another spelling) and state that the git calls and the on-disk walk share one 30 second deadline.
