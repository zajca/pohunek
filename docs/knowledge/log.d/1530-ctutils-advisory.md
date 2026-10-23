## Unreleased (2026-10-23, ctutils advisory RUSTSEC-2026-0385, #1530)

- `Cargo.lock` moves `ctutils` from 0.4.2 to 0.4.5, the patched release for
  RUSTSEC-2026-0385. The crate is reached only through `digest` from the
  `sqlx-postgres` stack used by `pohunek-relay`. No manifest, protocol, store or
  report change.
