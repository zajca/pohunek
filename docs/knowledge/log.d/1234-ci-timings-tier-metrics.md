## Unreleased (2026-10-16, ci-timings tiers measures completion, push vs dispatch and runner platforms, #1234)

- `scripts/ci-timings tiers` reports five tiers:
  - `pr`;
  - `main_push` and `main_dispatch` (`tier=main`);
  - `nightly_schedule` and `nightly_dispatch` (`tier=nightly`).
- A run counts as complete only when it reached a verdict: `success`,
  `failure`, `timed_out` or `startup_failure`. Cancelled runs no longer count.
  Each tier reports its conclusions and its completion share among finished
  runs.
- Each tier also reports, from the runner labels of the REST jobs stored in
  the snapshot:
  - started jobs and macOS jobs per complete run;
  - job queue p50/p90 per runner platform.

  An all-runs job queue covers every selected run.
- `--window` also takes UTC timestamps, for example
  `2026-10-15T06:39Z..2026-10-16T13:42Z`, and the end covers its whole minute.
- `gh` server errors and secondary rate limits are retried
  (`--gh-attempts`, `--gh-retry-delay`).
- A fetch that fails part way keeps what it fetched in the snapshot, so the
  next run resumes there. A failed REST job lookup is reported, and the jobs
  it concerns count as `unknown` platform.
