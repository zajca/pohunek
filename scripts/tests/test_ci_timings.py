"""Regression checks for the CI timing/measurement helper (stdlib only)."""

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
        "doctests + release build\tRun Swatinem/rust-cache@v2"
        "\t2026-09-21T05:18:03Z Cache restored successfully",
        "doctests + release build\tPost Enable sccache"
        "\t2026-09-21T05:22:56Z [command]/opt/sccache --show-stats --stats-format=json",
        "doctests + release build\tPost Enable sccache"
        f"\t2026-09-21T05:22:56Z {SCCACHE_JSON}",
        "tests (unit, fast)\tRun Swatinem/rust-cache@v2"
        "\t2026-09-21T05:17:40Z Cache not found for keys: Linux-x64-gnu",
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

    def test_summarize_run_wall_clock(self):
        summary = ci_timings.summarize_run(RUN)
        self.assertEqual(summary["id"], 1)
        self.assertEqual(summary["wall_seconds"], 570.0)
        self.assertEqual(summary["branch"], "topic")

    def test_summarize_run_requires_timestamps(self):
        with self.assertRaisesRegex(ValueError, "no timestamps"):
            ci_timings.summarize_run({"databaseId": 7})

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

    def test_render_junit_markdown_separates_compile_time(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "junit.xml"
            path.write_text(JUNIT)
            summary = ci_timings.parse_junit([path])
        rendered = ci_timings.render_junit_markdown(
            summary, "tests (unit, fast)", job_seconds=226.0
        )
        self.assertIn("tests (unit, fast): 3 test(s)", rendered)
        self.assertIn("test execution is 6 % of it", rendered)
        self.assertIn("| cli::parse | 1 | 11.0 |", rendered)


class CacheTests(unittest.TestCase):
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
        self.assertIn("| doctests + release build | 1015 | 660 | 286 | 70 % | hit |", rendered)
        self.assertIn("| tests (unit, fast) | n/a | n/a | n/a | n/a | miss |", rendered)


class SnapshotTests(unittest.TestCase):
    def test_snapshot_roundtrip_and_rejection(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "snapshot.json"
            ci_timings.write_snapshot(path, [RUN])
            self.assertEqual(ci_timings.read_snapshot(path), [RUN])
            path.write_text('{"runs": []}')
            with self.assertRaisesRegex(ValueError, "JSON list"):
                ci_timings.read_snapshot(path)

