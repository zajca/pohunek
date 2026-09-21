"""Regression checks for the CI timing/measurement helper (stdlib only)."""

import argparse
import importlib.machinery
import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "ci-timings"
LOADER = importlib.machinery.SourceFileLoader("ci_timings", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
ci_timings = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(ci_timings)

RUN = {
    "databaseId": 1,
    "displayTitle": "CI",
    "event": "pull_request",
    "conclusion": "success",
    "createdAt": "2026-09-20T10:00:00Z",
    "updatedAt": "2026-09-20T10:09:30Z",
    "headBranch": "topic",
    "jobs": [
        {
            "name": "fmt + clippy",
            "startedAt": "2026-09-20T10:00:05Z",
            "completedAt": "2026-09-20T10:03:24Z",
            "conclusion": "success",
        },
        {
            "name": "tests (unit, fast)",
            "startedAt": "2026-09-20T10:00:06Z",
            "completedAt": "2026-09-20T10:03:52Z",
            "conclusion": "success",
        },
        # A skipped job has no timestamps and contributes no duration.
        {
            "name": "cargo-udeps (unused dependencies)",
            "startedAt": None,
            "completedAt": None,
            "conclusion": "skipped",
        },
    ],
}

JUNIT = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="3" failures="0" errors="0" time="12.5">
  <testsuite name="gui-core::state" tests="2" failures="0" errors="0" time="1.5">
    <testcase name="test_alpha" classname="gui-core::state" time="1.0"/>
    <testcase name="test_beta" classname="gui-core::state" time="0.5"/>
  </testsuite>
  <testsuite name="cli::parse" tests="1" failures="1" errors="0" time="11.0">
    <testcase name="test_gamma" classname="cli::parse" time="11.0"/>
  </testsuite>
</testsuites>
"""

SCCACHE_JSON = (
    '{"stats":{"compile_requests":1207,"requests_executed":1015,'
    '"cache_hits":{"counts":{"Rust":660}},"cache_misses":{"counts":{"Rust":286}},'
    '"cache_write_errors":272,"compile_fails":8},'
    '"cache_location":"ghac, name: fe6676c9, prefix: /sccache/"'
    "}"
)

CACHE_LOG = "\n".join(
    [
        # Step names mirror the CI workflow: "Cache cargo build" is the
        # Swatinem/rust-cache step, "Post Enable sccache" prints the sccache
        # JSON. `actions/cache` steps ("Cache Bun packages", ...) must never
        # count as rust-cache evidence.
        "doctests + release build\tCache cargo build"
        "\t2026-09-21T05:18:03Z Cache restored successfully",
        "doctests + release build\tPost Enable sccache"
        "\t2026-09-21T05:22:56Z [command]/opt/sccache --show-stats --stats-format=json",
        "doctests + release build\tPost Enable sccache"
        f"\t2026-09-21T05:22:56Z {SCCACHE_JSON}",
        "tests (unit, fast)\tCache cargo build"
        "\t2026-09-21T05:17:40Z Cache not found for keys: Linux-x64-gnu",
        "web SDK + control center\tCache Bun packages"
        "\t2026-09-21T05:17:40Z Cache restored successfully",
    ]
)


def _summary(**overrides):
    """Build a run summary from the fixture with per-test overrides."""
    return ci_timings.summarize_run(dict(RUN, **overrides))


class RunTimingTests(unittest.TestCase):
    def test_format_seconds_scales(self):
        self.assertEqual(ci_timings.format_seconds(9.4), "9s")
        self.assertEqual(ci_timings.format_seconds(83), "1m23s")
        self.assertEqual(ci_timings.format_seconds(3725), "1h02m")

    def test_job_seconds_skips_unfinished_jobs(self):
        seconds = ci_timings.job_seconds(RUN)
        self.assertEqual(seconds["fmt + clippy"], 199.0)
        self.assertNotIn("cargo-udeps (unused dependencies)", seconds)

    def test_job_seconds_skips_skipped_and_zero_time_jobs(self):
        # Conditionally skipped jobs must not count as 0 s runs: `gh`
        # serializes their missing timestamps as null or the Go zero time,
        # which would drag medians down while inflating run counts.
        zero = {
            "name": "tests (relay DB, PostgreSQL)",
            "startedAt": "0001-01-01T00:00:00Z",
            "completedAt": "0001-01-01T00:00:00Z",
            "conclusion": "skipped",
        }
        skewed = {
            "name": "TS binding drift (xs check)",
            "startedAt": "2026-09-20T10:00:00Z",
            "completedAt": "2026-09-20T10:00:00Z",
            "conclusion": "success",
        }
        self.assertEqual(ci_timings.job_seconds({"jobs": [zero, skewed]}), {})

    def test_summarize_run_wall_clock(self):
        summary = ci_timings.summarize_run(RUN)
        self.assertEqual(summary["id"], 1)
        self.assertEqual(summary["wall_seconds"], 570.0)
        self.assertEqual(summary["branch"], "topic")

    def test_summarize_run_requires_timestamps(self):
        with self.assertRaisesRegex(ValueError, "no timestamps"):
            ci_timings.summarize_run({"databaseId": 7})

    def test_summarize_run_separates_created_and_started(self):
        # A rerun keeps createdAt at the original attempt; wall clock must
        # measure the actual execution from startedAt, not the gap between
        # attempts, while windows keep matching on creation time (the
        # server-side `--created` semantics).
        summary = _summary(
            createdAt="2026-09-20T09:00:00Z",
            startedAt="2026-09-20T10:00:00Z",
            updatedAt="2026-09-20T10:09:30Z",
        )
        self.assertEqual(summary["wall_seconds"], 570.0)
        self.assertEqual(summary["created_at"], ci_timings.parse_timestamp(
            "2026-09-20T09:00:00Z"
        ))
        self.assertEqual(summary["started_at"], ci_timings.parse_timestamp(
            "2026-09-20T10:00:00Z"
        ))
        self.assertEqual(summary["attempt"], 1)

    def test_filter_runs_windows_follow_created_not_started(self):
        queued = _summary(
            databaseId=25,
            createdAt="2026-09-19T23:50:00Z",
            startedAt="2026-09-20T00:10:00Z",
        )
        window = ci_timings.parse_window("2026-09-20..2026-09-21")
        self.assertEqual(ci_timings.filter_runs([queued], window=window), [])

    def test_filter_runs_conclusion_applies_to_grouped_first_attempt(self):
        # A failed first attempt must stay excluded even when its rerun
        # succeeded: grouping precedes the conclusion filter.
        failed = _summary(
            databaseId=26, attempt=1, conclusion="failure",
            startedAt="2026-09-20T10:00:00Z",
        )
        rerun = _summary(
            databaseId=26, attempt=2, conclusion="success",
            startedAt="2026-09-20T15:00:00Z",
        )
        selected = ci_timings.filter_runs(
            [failed, rerun], conclusion="success"
        )
        self.assertEqual(selected, [])
        everything = ci_timings.filter_runs(
            [failed, rerun], conclusion="success", attempts="all"
        )
        self.assertEqual(
            [(run["id"], run["attempt"]) for run in everything], [(26, 2)]
        )

    def test_filter_runs_default_keeps_only_first_attempts(self):
        first = _summary(databaseId=20, startedAt="2026-09-20T10:00:00Z")
        rerun = _summary(
            databaseId=20,
            startedAt="2026-09-20T15:00:00Z",
            attempt=2,
            conclusion="success",
        )
        other = _summary(databaseId=21, startedAt="2026-09-20T16:00:00Z")
        selected = ci_timings.filter_runs([rerun, other, first])
        self.assertEqual([run["id"] for run in selected], [20, 21])
        everything = ci_timings.filter_runs([rerun, other, first], attempts="all")
        self.assertEqual([run["id"] for run in everything], [20, 20, 21])

    def test_filter_runs_window_is_inclusive_and_sorted(self):
        older = _summary(databaseId=2, createdAt="2026-09-14T09:00:00Z")
        newer = _summary(databaseId=3, createdAt="2026-09-21T00:00:00Z")
        outside = _summary(databaseId=4, createdAt="2026-09-22T00:00:00Z")
        window = ci_timings.parse_window("2026-09-14..2026-09-21")
        selected = ci_timings.filter_runs(
            [outside, newer, older], window=window, event="pull_request"
        )
        self.assertEqual([run["id"] for run in selected], [2, 3])

    def test_filter_runs_metadata_filters(self):
        failed = _summary(databaseId=5, conclusion="failure")
        schedule = _summary(databaseId=6, event="schedule")
        branch = _summary(databaseId=7, headBranch="other")
        selected = ci_timings.filter_runs(
            [_summary(), failed, schedule, branch],
            event="pull_request",
            conclusion="success",
            branch="topic",
        )
        self.assertEqual([run["id"] for run in selected], [1])

    def test_filter_runs_first_attempt_is_by_attempt_number(self):
        # Attempts may arrive in either order (snapshot accumulation); the
        # choice must follow the attempt value, not input position.
        first = _summary(databaseId=22, startedAt="2026-09-20T10:00:00Z", attempt=1)
        rerun = _summary(databaseId=22, startedAt="2026-09-20T15:00:00Z", attempt=2)
        forward = ci_timings.filter_runs([first, rerun])
        backward = ci_timings.filter_runs([rerun, first])
        self.assertEqual([run["attempt"] for run in forward], [1])
        self.assertEqual([run["attempt"] for run in backward], [1])

    def test_document_key_and_workflow_filter(self):
        self.assertEqual(
            ci_timings.document_key({"databaseId": 23, "attempt": 2}), (23, 2)
        )
        self.assertEqual(
            ci_timings.document_key({"databaseId": 23}), (23, 1)
        )
        release = {"databaseId": 23, "workflowName": "Release"}
        ci_run = {"databaseId": 24, "workflowName": "CI"}
        legacy = {"databaseId": 25}
        kept = [
            document for document in (release, ci_run, legacy)
            if document.get("workflowName") in (None, "CI")
        ]
        self.assertEqual(
            [document["databaseId"] for document in kept], [24, 25]
        )

    def test_list_arguments_scopes_to_ci_workflow(self):
        arguments = ci_timings.list_arguments(
            argparse.Namespace(limit=40, event="pull_request", branch=None,
                               conclusion=None),
            None,
        )
        self.assertIn("--workflow", arguments)
        self.assertIn("ci.yml", arguments)

    def test_list_arguments_passes_conclusion_as_status(self):
        arguments = ci_timings.list_arguments(
            argparse.Namespace(limit=40, event=None, branch=None,
                               conclusion="success"),
            None,
        )
        self.assertIn("--status", arguments)
        self.assertIn("success", arguments)

    def test_parse_window_rejects_malformed_input(self):
        with self.assertRaisesRegex(ValueError, "START..END"):
            ci_timings.parse_window("2026-09-14")

    def test_window_summary_medians_across_runs(self):
        first = _summary(databaseId=10)
        second = _summary(
            databaseId=11,
            jobs=[
                {
                    "name": "fmt + clippy",
                    "startedAt": "2026-09-20T10:00:05Z",
                    "completedAt": "2026-09-20T10:04:05Z",
                    "conclusion": "success",
                }
            ],
        )
        summary = ci_timings.window_summary([first, second])
        self.assertEqual(summary["runs"], 2)
        self.assertEqual(summary["wall_seconds"], 570.0)
        self.assertEqual(summary["jobs"]["fmt + clippy"]["p50"], 219.5)
        self.assertEqual(summary["jobs"]["fmt + clippy"]["runs"], 2)
        self.assertEqual(summary["jobs"]["tests (unit, fast)"]["runs"], 1)

    def test_compare_windows_reports_deltas_and_missing_jobs(self):
        baseline = ci_timings.window_summary([_summary()])
        current = ci_timings.window_summary(
            [
                _summary(
                    databaseId=12,
                    jobs=[
                        {
                            "name": "fmt + clippy",
                            "startedAt": "2026-09-20T10:00:05Z",
                            "completedAt": "2026-09-20T10:01:05Z",
                            "conclusion": "success",
                        }
                    ],
                )
            ]
        )
        rows = ci_timings.compare_windows(baseline, current)
        clippy = next(row for row in rows if row["job"] == "fmt + clippy")
        self.assertEqual(clippy["before"], 199.0)
        self.assertEqual(clippy["after"], 60.0)
        self.assertEqual(clippy["delta"], -139.0)
        self.assertAlmostEqual(clippy["percent"], -69.8, places=1)
        unit = next(row for row in rows if row["job"] == "tests (unit, fast)")
        self.assertIsNone(unit["after"])
        self.assertIsNone(unit["percent"])

    def test_compare_windows_includes_current_only_jobs(self):
        baseline = ci_timings.window_summary([_summary()])
        current = ci_timings.window_summary(
            [
                _summary(
                    databaseId=13,
                    jobs=[
                        {
                            "name": "tests (new-shard, fast)",
                            "startedAt": "2026-09-20T10:00:05Z",
                            "completedAt": "2026-09-20T10:02:05Z",
                            "conclusion": "success",
                        }
                    ],
                )
            ]
        )
        rows = ci_timings.compare_windows(baseline, current)
        added = next(row for row in rows if row["job"] == "tests (new-shard, fast)")
        self.assertIsNone(added["before"])
        self.assertEqual(added["after"], 120.0)
        self.assertEqual(added["after_runs"], 1)
        self.assertIsNone(added["delta"])
        self.assertIsNone(added["percent"])
        # Rendered output shows the new job with `n/a` on the baseline side.
        rendered = ci_timings.render_compare_markdown(baseline, current, rows)
        self.assertIn("| tests (new-shard, fast) | n/a | 2m00s | n/a | n/a | 0/1 |", rendered)

    def test_render_compare_markdown_includes_workflow_row(self):
        baseline = ci_timings.window_summary([_summary()])
        current = ci_timings.window_summary([_summary(databaseId=13)])
        rows = ci_timings.compare_windows(baseline, current)
        rendered = ci_timings.render_compare_markdown(baseline, current, rows)
        self.assertIn(
            "| **workflow** | **9m30s** | **9m30s** | **0s** | **+0 %** | **1/1** |",
            rendered,
        )
        self.assertIn("| fmt + clippy | 3m19s | 3m19s | 0s | +0 % | 1/1 |", rendered)



class JunitTests(unittest.TestCase):
    def test_parse_junit_counts_and_times(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "junit.xml"
            path.write_text(JUNIT)
            summary = ci_timings.parse_junit([path], top=2)
        self.assertEqual(summary["cases"], 3)
        self.assertEqual(summary["failures"], 1)
        self.assertEqual(summary["seconds"], 12.5)
        self.assertEqual(summary["p50"], 1.0)
        self.assertEqual(summary["p95"], 11.0)
        self.assertEqual(summary["slowest"][0], ("test_gamma", 11.0))
        self.assertEqual(len(summary["suites"]), 2)

    def test_percentile_uses_nearest_rank(self):
        self.assertIsNone(ci_timings.percentile([], 0.95))
        self.assertEqual(ci_timings.percentile([5.0], 0.95), 5.0)
        self.assertEqual(ci_timings.percentile([1.0, 2.0, 3.0, 4.0], 0.5), 2.0)

    def test_percentile_nearest_rank_ceil_boundary(self):
        # Nearest rank: p95 of 11 values is the 11th value (ceil(0.95*11)=11),
        # not the 10th, which the previous round()-based index returned.
        values = [float(value) for value in range(1, 12)]
        self.assertEqual(ci_timings.percentile(values, 0.95), 11.0)
        self.assertEqual(ci_timings.percentile([1.0, 2.0], 0.5), 1.0)

    def test_render_junit_markdown_separates_compile_time(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "junit.xml"
            path.write_text(JUNIT)
            summary = ci_timings.parse_junit([path])
        rendered = ci_timings.render_junit_markdown(
            summary, "tests (unit, fast)", job_seconds=300.0,
            step_seconds=258.0,
        )
        self.assertIn("tests (unit, fast): 3 test(s)", rendered)
        self.assertIn("Test step elapsed 4m18s (86 % of the job wall clock)", rendered)
        self.assertIn("their summed time is not a wall-clock share", rendered)
        self.assertIn("| cli::parse | 1 | 11.0 |", rendered)

    def test_render_junit_markdown_handles_empty_artifact(self):
        # A valid JUnit artifact without test cases yields p50/p95 of None;
        # the renderer must print `n/a` instead of raising TypeError.
        rendered = ci_timings.render_junit_markdown(
            {"suites": [], "cases": 0, "failures": 0, "errors": 0, "skipped": 0,
             "seconds": 0.0, "p50": None, "p95": None, "slowest": []},
            "tests (empty)",
        )
        self.assertIn("tests (empty): 0 test(s)", rendered)
        self.assertIn("p50 n/a, p95 n/a", rendered)

    def test_select_junit_job_prefers_label_then_explicit(self):
        document = {
            "jobs": [
                {"name": "doctests + release build", "steps": [
                    {"name": "Documentation tests",
                     "startedAt": "2026-09-20T10:00:00Z",
                     "completedAt": "2026-09-20T10:02:00Z"},
                ]},
                {"name": "tests (unit, fast)", "steps": [
                    {"name": "Run fast shard",
                     "startedAt": "2026-09-20T10:00:00Z",
                     "completedAt": "2026-09-20T10:04:18Z"},
                ]},
            ]
        }
        name, wall, step = ci_timings.select_junit_job(
            document, label="tests (unit, fast)"
        )
        self.assertEqual((name, step), ("tests (unit, fast)", 258.0))
        self.assertIsNone(wall)  # job wall needs job start/end; absent → None
        name, _, _ = ci_timings.select_junit_job(
            document, label="tests (unit, fast)", requested="doctests + release build"
        )
        self.assertEqual(name, "doctests + release build")
        with self.assertRaisesRegex(ValueError, "not found"):
            ci_timings.select_junit_job(document, requested="nope")


class CacheTests(unittest.TestCase):
    def test_junit_step_seconds_matches_exact_step_names(self):
        # Only the test-execution step counts; a step whose name merely
        # contains "nextest" (e.g. "Install cargo-nextest") must not match.
        document = {
            "jobs": [
                {
                    "name": "tests (unit, fast)",
                    "steps": [
                        {
                            "name": "Install cargo-nextest",
                            "startedAt": "2026-09-20T10:00:00Z",
                            "completedAt": "2026-09-20T10:00:10Z",
                        },
                        {
                            "name": "Run fast shard",
                            "startedAt": "2026-09-20T10:01:00Z",
                            "completedAt": "2026-09-20T10:05:18Z",
                        },
                    ],
                },
                {"name": "docs check", "steps": []},
            ]
        }
        durations = ci_timings.junit_step_seconds(document)
        self.assertIsNone(durations["docs check"])
        self.assertEqual(durations["tests (unit, fast)"], 4.0 * 60 + 18)

    def test_parse_cache_log_extracts_sccache_json(self):
        records = {record["job"]: record for record in ci_timings.parse_cache_log(CACHE_LOG)}
        release = records["doctests + release build"]
        self.assertEqual(release["sccache"]["executed"], 1015)
        self.assertEqual(release["sccache"]["hits"], 660)
        self.assertEqual(release["sccache"]["misses"], 286)
        self.assertEqual(release["sccache"]["write_errors"], 272)
        self.assertIn("ghac", release["sccache"]["location"])
        self.assertEqual(release["rust_cache"], "hit")

    def test_parse_cache_log_records_miss_without_sccache(self):
        records = {record["job"]: record for record in ci_timings.parse_cache_log(CACHE_LOG)}
        unit = records["tests (unit, fast)"]
        self.assertIsNone(unit["sccache"])
        self.assertEqual(unit["rust_cache"], "miss")

    def test_render_cache_markdown_hit_ratio(self):
        rendered = ci_timings.render_cache_markdown(
            ci_timings.parse_cache_log(CACHE_LOG)
        )
        self.assertIn(
            "| doctests + release build | 1015 | 660 | 286 | 0 | 70 % | hit |",
            rendered,
        )
        self.assertIn(
            "| tests (unit, fast) | n/a | n/a | n/a | n/a | n/a | miss |",
            rendered,
        )

    def test_parse_cache_log_counts_cache_errors(self):
        # Backend lookup errors dilute the hit ratio: the same blob with
        # half its lookups failing must not read as highly cached.
        errored = CACHE_LOG.replace(
            '"cache_misses":{"counts":{"Rust":286}}',
            '"cache_misses":{"counts":{"Rust":286}},'
            '"cache_errors":{"counts":{"Timeout":946}}',
        )
        records = {
            record["job"]: record for record in ci_timings.parse_cache_log(errored)
        }
        release = records["doctests + release build"]
        self.assertEqual(release["sccache"]["errors"], 946)
        self.assertAlmostEqual(release["sccache"]["hit_ratio"], 660 / 1892)
        rendered = ci_timings.render_cache_markdown(
            ci_timings.parse_cache_log(errored)
        )
        self.assertIn(
            "| doctests + release build | 1015 | 660 | 286 | 946 | 35 % | hit |",
            rendered,
        )

    def test_parse_cache_log_ignores_unrelated_cache_steps(self):
        # "Cache restored successfully" from an `actions/cache` step (Bun,
        # Playwright) must not fabricate a rust-cache record; nor may an
        # sccache-looking line outside the post-step.
        records = {record["job"]: record for record in ci_timings.parse_cache_log(CACHE_LOG)}
        self.assertNotIn("web SDK + control center", records)
        unrelated = "\n".join(
            [
                "tests (cli, fast)\tRun fast shard"
                f"\t2026-09-21T05:20:00Z some output {{\"stats\": 1}}",
                "tests (cli, fast)\tCache Bun packages"
                "\t2026-09-21T05:17:40Z Cache restored successfully",
            ]
        )
        self.assertEqual(ci_timings.parse_cache_log(unrelated), [])


class SnapshotTests(unittest.TestCase):
    def test_snapshot_roundtrip_and_rejection(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "snapshot.json"
            ci_timings.write_snapshot(path, [RUN])
            self.assertEqual(ci_timings.read_snapshot(path), [RUN])
            path.write_text('{"runs": []}')
            with self.assertRaisesRegex(ValueError, "JSON list"):
                ci_timings.read_snapshot(path)

