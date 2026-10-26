## Unreleased (2026-10-26, 0.38 boundary rulings, #1626)

- The architecture records that plugins live outside core, that herd orchestration stays outside the daemon (core keeps only the `spawned_by` attribution and durable session metadata), and the full list of crates a Rust client links (`netbird`, `overlay` and `overlay-tailscale` included).
- The unknown-enum policy gains the same-release rule: a release adds values only to enums the previous release already decodes tolerantly in both the Rust client and the TypeScript SDK; the other enums need a protocol bump and an N-1 adapter, and the bump-only enums get no new value. The rule is in "Unknown enum values from a newer daemon" of `docs/architecture.md`.
