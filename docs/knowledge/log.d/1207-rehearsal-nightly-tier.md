## Unreleased (2026-10-16, the release rehearsal runs in the nightly tier, not on push to main, #1207)

- The release rehearsal (`release-rehearsal-build` and `release-rehearsal` in
  `ci.yml`) runs on the daily schedule, on a `tier=nightly` dispatch, and on a
  pull request that changes a `release_rehearsal` filter input. Push to `main`
  and a `tier=main` dispatch no longer run it, which takes about 30 jobs, 7-8 of
  them on macOS, out of every main run.
- `scripts/ci-evidence.json` requires the rehearsal jobs only in the nightly
  tier. A release still carries them: `scripts/release` requires both main and
  nightly evidence on the release commit.
- The `release_rehearsal` filter also lists `scripts/provision-hermes-compat`,
  `scripts/smoke-hermes-plugin-release`, `scripts/tests/test_smoke_archive.py`,
  `.cargo/**`, `package.json` and `bun.lock`. `scripts/tests/test_ci_tiers.py`
  checks that every path the rehearsal workflows and their scripts read matches
  the filter or is a named exception: the product crates, `docs/knowledge/**`,
  the documents copied into every archive, and the SDK package sources. A
  rehearsal-only failure in those surfaces in the nightly tier.
