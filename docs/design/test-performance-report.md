# Test Performance Report: CI Before/After

Date: 2026-09-21 · Scope: epic #115 / issue #127 · Target: −30–65 % workflow
wall-clock median vs the pre-optimization baseline.

All numbers are reproducible with `scripts/ci-timings` (see `AGENTS.md`
"Fast loops"); the commands are given per section. `target/ci-timings/
ci-runs.json` is the local run snapshot the commands default to; pass
`--input` to reuse it elsewhere.

## Baseline and current windows

- **Baseline**: successful `pull_request` runs, 2026-09-14 .. 2026-09-16
  (17 runs). This is the last full span before the optimization work merged:
  the fast-loop/test-performance milestone (issues #125–#128) landed on
  2026-09-17, so no clean baseline window exists after that date — the
  baseline is necessarily only 3 days wide.
- **Current**: successful `pull_request` runs, 2026-09-20 .. 2026-09-21
  (13 runs), on the milestone's merged output.

```bash
scripts/ci-timings compare --baseline 2026-09-14..2026-09-16 \
    --current 2026-09-20..2026-09-21 --event pull_request --conclusion success
```

## Workflow wall clock

The command above prints both figures per window, and its table carries a
`workflow (p90)` row beside the median one. Percentiles are nearest-rank, so
each is an observed run's wall clock rather than an interpolation.

| Window | Runs | p50 | p90 |
| --- | --- | --- | --- |
| Baseline (09-14..16) | 17 | **9m44s** | 11m37s |
| Current (09-20..21) | 13 | **6m28s** | 11m39s |

**Median workflow wall clock: −196 s (−34 %).** The current p90 (11m39s) is a
PR-only sample property, not a runner accident: by step timestamps, the two
longest current-window PR runs waited on queued jobs, not slow execution —
run `35496971392` (24m28s wall) started its last job (`tests (heavy, PTY +
Hermes)`) 19m49s after the run began, and run `35516132376` (11m39s wall)
started `tests (relay DB, PostgreSQL)` 6m40s in. GitHub schedules these jobs
late on busy runners; excluding queue-heavy tail runs, the remaining 11 PR
runs sit at 5m17s–7m18s wall.

## Job-level medians

Two regimes are compared below; the CI matrix itself changed in the
milestone, so most jobs do not have a baseline counterpart.

Jobs only in the current matrix (fast shards + release build; 13/13 runs each):

| Job | p50 |
| --- | --- |
| doctests + release build | 5m27s |
| tests (heavy, PTY + Hermes) | 5m29s |
| tests (relay DB, PostgreSQL) | 5m22s |
| tests (daemon, fast) | 4m19s |
| tests (relay, fast) | 4m26s |
| tests (cli, fast) | 3m52s |
| tests (unit, fast) | 4m07s |
| fmt + clippy | 1m18s |
| build bins (pohunekd + pohunek-sessiond + pohunek) | 1m41s |
| TS binding drift (xs check) | 2m07s |
| docs check (schema + drift + source-map + runbooks + secrets + release) | 32s |

Jobs present in both windows (p50, before → after):

| Job | Baseline | Current | Delta |
| --- | --- | --- | --- |
| cargo-audit (RustSec advisories) | 3m08s | 3m06s | −1 % |
| cargo-udeps (unused dependencies) | 2m33s | 2m33s | +0 % |
| web SDK + control center | 2m20s | 1m41s | −28 % |
| pinned Hermes compatibility (model-free) | 1m41s | 2m14s | +33 % |
| cargo-hack (feature powerset) | 2m02s | 2m31s | +24 % |
| platform contracts (arm64) | 22s | 1m10s | +218 % |

The retired monolithic `fmt + clippy + test + build` job (baseline p50
9m39s, the pipeline's critical path) no longer exists; it is replaced by the
parallel shards above plus `fmt + clippy` (1m18s), which is where the
workflow-level gain comes from. The Hermes gate and cargo-hack slowdowns are
known additive costs of later milestone work (pinned plugin surface,
feature-powerset over the new crates), not regressions of this change; arm64
contract timing reflects the new native-macos matrix leg with only 11 of 13
runs, so its p50 is noisy.

## JUnit evidence (run 35534741514, push main, 2026-09-20)

CI already uploads per-shard JUnit artifacts; nothing aggregates them, so
`scripts/ci-timings junit` parses downloaded artifacts locally:

```bash
gh run download 35534741514 -n nextest-junit-unit -D /tmp/junit-unit
scripts/ci-timings junit --label "tests (unit, fast)" --run 35534741514 \
    /tmp/junit-unit/junit.xml
```

| Shard | Cases | Failures | Σ test time | Test step elapsed | Job wall |
| --- | --- | --- | --- | --- | --- |
| tests (unit, fast) | 867 | 0 | 13s | 4m18s | 5m00s |
| tests (cli, fast) | 445 | 0 | 7s | 4m27s | 5m06s |
| tests (daemon, fast) | 673 | 0 | 32s | 4m41s | 5m18s |
| tests (relay, fast) | 126 | 0 | 46s | 4m24s | 4m58s |
| tests (relay DB, PostgreSQL) | 133 | 0 | 4m05s | 5m33s | 6m28s |
| tests (heavy, PTY + Hermes) | 685 | 0 | 4m02s | 1m14s | 5m56s |

**Σ test time is not a share of the job wall.** nextest runs test cases in
parallel, so the sum of testcase times is neither a wall-clock quantity nor
decomposable against the job clock — dividing it by the job wall can even
exceed 100 %. The comparable measure is the **elapsed time of the CI test
step itself**, from the step's own start/end timestamps
(`scripts/ci-timings junit --run <id>`), which is the "Test step elapsed"
column. Σ test time stays in the table because it bounds test wall time from
above: parallelism can only shrink it.

Reading the two columns together:

- **The four fast shards are compile-dominated by a wide margin.** Their
  "Run fast shard" step wraps `cargo nextest run`, so it includes
  compilation, and test wall time is at most the Σ — 7–46 s inside
  4.3–4.7-minute steps. Further fast-shard wins require build-time work
  (sccache/mold, shard packaging), not test pruning.
- **Heavy is not test-dominated either.** Its test step is 1m14s of a 5m56s
  job wall (Σ 4m02s across bounded 4-way concurrency), so setup and
  compilation dominate; the tests themselves are heavily parallel, not long.
- **Relay DB is indeterminate.** Σ 4m05s inside a 5m33s step bounds test wall
  time between Σ/4 ≈ 1m01s and 4m05s, so it may or may not be test-dominated;
  PostgreSQL-bound tests are the plausible slow tail either way.

Structurally: *every shard's job wall is setup/compile-dominated; only
relay-db has a plausibly test-dominated step, and proving that needs per-test
wall decomposition.* This does not affect the workflow-level verdict
(−34 % median).

## Cache evidence (same run, step-level timing via the GitHub API)

```bash
scripts/ci-timings cache --run 35534741514
```

- `doctests + release build`: rust-cache **hit** (the "Cache restored
  successfully" line from a `Swatinem/rust-cache` step — the parser accepts
  only that action's steps, by the workflow's "Cache cargo build" name or by
  the action reference an unnamed step logs under, because `actions/cache`
  steps such as "Cache Bun packages" and "Cache Playwright browsers" print
  the same marker); steps: Documentation tests 2m14s + Release build 4m34s.
  The sccache post-step prints the full JSON stats blob; on this run the
  job completed before the post step, so exact hit/miss counts were not
  captured here — the `cache` subcommand extracts
  `cache_hits.counts`/`cache_misses.counts`/`cache_errors.counts` whenever
  the blob is present (tested shape: 1248 requests / 1207 hits / 41 misses /
  0 errors ≈ 97 % of lookups; backend errors dilute the ratio by design).
- Fast shards and heavy: rust-cache **hit** on all inspected jobs.
- A restore that starts and then fails ("Failed to restore") is reported as
  `error`, not `miss`: it says the cache backend or the extraction broke, not
  that the entry was absent, and folding it into `miss` would understate how
  effective the cache is.

Log-derived cache statistics are best-effort: the sccache JSON only appears
in the post-step log, and run-log download requires repository admin, so the
report treats step timings + rust-cache markers as the durable evidence and
sccache hit ratios as opportunistic.

## Verdict

**Target met at the workflow level: −34 % median (target −30–65 %), with the
old critical-path job (9m39s monolithic build+test) eliminated.** The
improvement sits inside the target band but at its shallow end; the deep end
of the band would require attacking shared compile time (every shard's job
wall is setup/compile-dominated; the fast shards' test steps add up to
seconds of test work inside 4–5 minutes of compilation).

## Limitations

1. **Baseline window is 3 days (17 runs)**, not a full week — the
   optimization milestone itself merged on 2026-09-17, so no clean
   pre/post-week pair exists. Job-level medians for the 6 surviving jobs
   rest on those 17 runs.
2. **Warm local timings are excluded by construction** (script reads `gh`
   run data only), but the two-minute/90 %-of-changes CI target is *not*
   re-verified here; retaining per-shard JUnit evidence in CI remains a
   separate concern.
3. **JUnit Σ test time excludes compilation** by definition; and because
   nextest runs cases in parallel, that sum is *not* a wall-clock share of
   the job — the correct test-share measure is the CI test step's elapsed
   time (`scripts/ci-timings junit --run <id>`), see the methodology note
   in the JUnit section.
4. **Cache statistics are log-derived**, not first-class: sccache JSON lives
   in a post-step, and `gh run view --log` needs admin rights on this
   repository, so hit-ratio capture is opportunistic (the parsing itself is
   unit-tested against recorded shapes). The parser attributes outcomes only
   to their responsible steps — rust-cache markers to `Swatinem/rust-cache`
   steps, the sccache JSON to the `Post Enable sccache` post-step — so
   `actions/cache` steps (Bun packages, Playwright browsers) are never
   misreported as rust-cache.
5. **Runner queueing tails the p90**: the current p90 (11m39s, PR-only) comes
   from late-scheduled jobs (see the table note), not slow tests — the job
   walls themselves stay in the 4–7 minute band.
