## Unreleased (2026-11-20, push reuse wording, #2321)

- Correction to the push-reuse entry of #2204: the `ci-reuse` record is written
  on every path, a fail-open run included (it then names no job), and it names
  only legs that were green in their source run. A delta dispatch reads its
  sources from pull request, push and dispatch runs, one source run per reused
  job, not from a single pull request run.
