## Unreleased (2026-10-26, contributor path, #1587)

- `scripts/dev-bootstrap` reads the Hermes test interpreter pins from the `hermes-test-python` composite action that `ci.yml` calls. Unreadable pins fail only the `hermes-test-python` row (`config-error`) and never abort the other rows or paths. A new `docs` path covers a documentation-only change (`cargo xtask docs check`).
- The contributor docs list the required CI checks. The review gate is an `opus-pr-review:v1` marker comment authored by `zajca` at the exact head; an outside contribution also needs the maintainer's explicit approval.
