"""Regression checks for the CI timing/measurement helper (stdlib only)."""

import json
import os
import sys
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "ci-timings"
ROOT = Path(__file__).resolve().parents[2]  # TODO: unused, consider removing

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
  <testsuite name="daemon::state" tests="2" failures="0" errors="0" time="1.5">
    <testcase name="test_alpha" classname="daemon::state" time="1.0"/>
    <testcase name="test_beta" classname="daemon::state" time="0.5"/>
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
        "SDK workspace\tCache Bun packages"
        "\t2026-09-21T05:17:40Z Cache restored successfully",
    ]
)


class CliHarness(unittest.TestCase):
    """Shared scenario fixture for the CLI tests below.

    Every subclass starts the real script as a subprocess and asserts on
    the stdout/stderr/exit code an operator observes. Run documents are
    `gh run view --json` fixtures written into a disposable directory, so
    no case touches `gh`, the network, writes outside a temp root, or any
    host state.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)

    # -- fixture helpers --------------------------------------------------

    def _document(self, databaseId, *, created, updated, jobs=None,
                  started=None, attempt=1, conclusion="success",
                  event="pull_request", branch="topic"):
        """A `gh run view --json` document shaped like the CLI reads it."""
        if started is None:
            started = created
        return {
            "databaseId": databaseId,
            "name": "ci.yml",
            "displayTitle": "CI",
            "event": event,
            "conclusion": conclusion,
            "createdAt": created,
            "startedAt": started,
            "updatedAt": updated,
            "attempt": attempt,
            "headBranch": branch,
            "workflowName": "CI",
            "jobs": list(jobs) if jobs is not None else [],
        }

    @staticmethod
    def _job(name, start, end, conclusion="success"):
        """One job entry with the timestamp pair `gh` reports."""
        return {
            "name": name,
            "startedAt": start,
            "completedAt": end,
            "conclusion": conclusion,
        }

    def write_snapshot(self, name, documents):
        """Write a run snapshot the way an accumulated `--input` holds it."""
        path = self.dir / name
        path.write_text(json.dumps(list(documents), indent=1) + "\n")
        return str(path)

    def run_cli(self, *arguments, env=None):
        """Start the script exactly as an operator does."""
        return subprocess.run(
            [sys.executable, str(SCRIPT), *arguments],
            capture_output=True,
            text=True,
            check=False,
            cwd=self.dir,
            env=env,
        )

    def run_json(self, *arguments):
        """A successful `--json` invocation, parsed."""
        result = self.run_cli(*arguments, "--json")
        self.assertEqual(
            result.returncode, 0,
            f"unexpected failure: {result.stderr}",
        )
        return json.loads(result.stdout)

    def snapshot_one(self):
        """One successful run fixture, as `RUN`'s timestamps describe it."""
        return self.write_snapshot(
            "snapshot.json",
            [self._document(1, created="2026-09-20T10:00:00Z",
                            updated="2026-09-20T10:09:30Z")],
        )


class RunTimingTests(CliHarness):
    """Command-line scenarios for `scripts/ci-timings`.

    Every case starts the real script as a subprocess and asserts on the
    stdout/stderr/exit code an operator observes; selection fixtures come
    from `--input` snapshots and the fetch-path contract (which selection
    flags go where) uses a stub `gh` on `PATH` that records its arguments.

    A window is inclusive of both named dates. Attempt grouping happens
    before the metadata filters: with the default `--attempts first`, a
    failed first attempt is never displaced by its own successful rerun,
    and a first attempt that succeeded before its rerun failed is never
    lost -- which a server-side conclusion filter would do.
    """

    # -- jobs and per-run timing -----------------------------------------

    def test_runs_reports_job_wall_clock_and_skips_jobs_with_no_duration(self):
        # Skipped jobs (even with usable timestamps), half-finished jobs
        # (`gh` serializes a running job's end as null, not the Go zero
        # time), Go zero timestamps, and zero-length jobs all carry no
        # duration to measure; each must stay out of the tables instead of
        # dragging the medians or inflating run counts.
        jobs = [
            self._job("fmt + clippy",
                      "2026-09-20T10:00:05Z", "2026-09-20T10:03:24Z"),
            self._job("tests (relay DB, PostgreSQL)",
                      "2026-09-20T10:00:00Z", "2026-09-20T10:04:00Z",
                      conclusion="skipped"),
            self._job("tests (heavy, PTY + Hermes)",
                      "2026-09-20T10:00:00Z", None),
            self._job("TS binding drift (xs check)",
                      "0001-01-01T00:00:00Z", "2026-09-20T10:04:00Z"),
            self._job("instant job",
                      "2026-09-20T10:00:00Z", "2026-09-20T10:00:00Z"),
        ]
        snapshot = self.write_snapshot(
            "snapshot.json",
            [self._document(1, created="2026-09-20T10:00:00Z",
                            updated="2026-09-20T10:09:30Z", jobs=jobs)],
        )
        payload = self.run_json(
            "runs", "--input", snapshot, "--window", "2026-09-19..2026-09-21"
        )
        self.assertEqual(payload["runs"][0]["jobs"], {"fmt + clippy": 199.0})
        self.assertEqual(len(payload["summary"]["jobs"]), 1)
        rendered = self.run_cli(
            "runs", "--input", snapshot, "--window", "2026-09-19..2026-09-21"
        )
        self.assertEqual(rendered.returncode, 0)
        self.assertEqual(rendered.stderr, "")
        # One per-run row with the billable runner minutes (199 s ceils to
        # 4 min) and a wall clock measured from startedAt to updatedAt.
        self.assertIn(
            "| 1 | 2026-09-20 | pull_request | topic | success | "
            "9m30s | 4 |",
            rendered.stdout,
        )
        self.assertIn("| fmt + clippy | 3m19s | 3m19s | 1 |", rendered.stdout)
        for absent in ("relay DB", "heavy", "TS binding drift", "instant"):
            self.assertNotIn(absent, rendered.stdout)

    def test_runs_fails_loudly_without_timestamps_and_on_a_bad_snapshot(self):
        # A run without any timestamp cannot be measured; the tool must say
        # which run it stopped on rather than emit a fabricated sample.
        snapshot = self.write_snapshot("snapshot.json", [{"databaseId": 7}])
        result = self.run_cli("runs", "--input", snapshot)
        self.assertEqual(result.returncode, 1)
        self.assertIn("run 7 has no timestamps", result.stderr)
        missing = self.run_cli(
            "runs", "--input", str(self.dir / "absent.json")
        )
        self.assertEqual(missing.returncode, 1)
        self.assertTrue(missing.stderr.startswith("ci-timings: "))

    # -- window and selection --------------------------------------------

    def test_runs_window_is_inclusive_of_its_end_date(self):
        # The window is inclusive of its end date, so a run late on that
        # day belongs to it while the next day's first instant does not.
        when_to_id = {
            "2026-09-14T00:00:00Z": 20,
            "2026-09-16T23:59:59Z": 21,
            "2026-09-17T00:00:00Z": 22,
        }
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(id, created=when, updated=when)
                for when, id in when_to_id.items()
            ],
        )
        within = self.run_json(
            "runs", "--input", snapshot, "--window", "2026-09-14..2026-09-16"
        )
        self.assertEqual([run["id"] for run in within["runs"]], [20, 21])
        beyond = self.run_json(
            "runs", "--input", snapshot, "--window", "2026-09-17..2026-09-17"
        )
        self.assertEqual([run["id"] for run in beyond["runs"]], [22])

    def test_runs_window_selects_by_created_not_started(self):
        # A rerun keeps createdAt at the original attempt, and a run can
        # start on the day after its creation; windows keep matching on
        # creation time (the server-side `--created` semantics), never on
        # the gap between attempts or the execution window.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [self._document(
                25,
                created="2026-09-19T23:50:00Z",
                started="2026-09-20T00:10:00Z",
                updated="2026-09-20T00:19:30Z",
            )],
        )
        after = self.run_json(
            "runs", "--input", snapshot, "--window", "2026-09-20..2026-09-21"
        )
        self.assertEqual(after["runs"], [])
        before = self.run_json(
            "runs", "--input", snapshot, "--window", "2026-09-19..2026-09-19"
        )
        self.assertEqual([run["id"] for run in before["runs"]], [25])

    def test_runs_keeps_first_attempts_unless_attempts_all_is_passed(self):
        documents = [
            self._document(20, created="2026-09-20T10:00:00Z",
                           updated="2026-09-20T10:09:30Z"),
            self._document(20, attempt=2, created="2026-09-20T10:00:00Z",
                           updated="2026-09-20T15:09:30Z"),
            self._document(21, created="2026-09-20T16:00:00Z",
                           updated="2026-09-20T16:09:30Z"),
        ]
        window = ("--window", "2026-09-19..2026-09-21")
        for order in (documents, list(reversed(documents))):
            # Attempts may arrive in either order (snapshot accumulation);
            # the choice must follow the attempt value, not input position.
            snapshot = self.write_snapshot("snapshot.json", order)
            default = self.run_json("runs", "--input", snapshot, *window)
            self.assertEqual(
                [(run["id"], run["attempt"]) for run in default["runs"]],
                [(20, 1), (21, 1)],
            )
            every = self.run_json(
                "runs", "--input", snapshot, *window, "--attempts", "all"
            )
            self.assertEqual(
                [(run["id"], run["attempt"]) for run in every["runs"]],
                [(20, 1), (20, 2), (21, 1)],
            )
            rendered = self.run_cli(
                "runs", "--input", snapshot, *window, "--attempts", "all"
            )
            self.assertIn("(attempt 2)", rendered.stdout)

    def test_conclusion_filter_applies_after_attempt_grouping(self):
        # A failed first attempt stays excluded even when its rerun
        # succeeded (grouping precedes the filter), while the mirror case a
        # server-side `gh --status success` would lose -- a first attempt
        # that succeeded before its rerun failed -- stays a success sample.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(26, attempt=1, conclusion="failure",
                               created="2026-09-20T10:00:00Z",
                               updated="2026-09-20T10:09:30Z"),
                self._document(26, attempt=2, conclusion="success",
                               created="2026-09-20T10:00:00Z",
                               updated="2026-09-20T15:09:30Z"),
                self._document(27, attempt=1, conclusion="success",
                               created="2026-09-20T10:00:00Z",
                               updated="2026-09-20T10:09:30Z"),
                self._document(27, attempt=2, conclusion="failure",
                               created="2026-09-20T10:00:00Z",
                               updated="2026-09-20T15:09:30Z"),
            ],
        )
        window = ("--window", "2026-09-19..2026-09-21")
        first = self.run_json(
            "runs", "--input", snapshot, *window, "--conclusion", "success"
        )
        self.assertEqual(
            [(run["id"], run["attempt"]) for run in first["runs"]],
            [(27, 1)],
        )
        every = self.run_json(
            "runs", "--input", snapshot, *window, "--attempts", "all",
            "--conclusion", "success",
        )
        self.assertEqual(
            [(run["id"], run["attempt"]) for run in every["runs"]],
            [(26, 2), (27, 1)],
        )

    def test_runs_metadata_filters(self):
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(40, created="2026-09-20T10:00:00Z",
                               updated="2026-09-20T10:09:30Z"),
                self._document(41, conclusion="failure",
                               created="2026-09-20T11:00:00Z",
                               updated="2026-09-20T11:09:30Z"),
                self._document(42, event="schedule",
                               created="2026-09-20T12:00:00Z",
                               updated="2026-09-20T12:09:30Z"),
                self._document(43, branch="other",
                               created="2026-09-20T13:00:00Z",
                               updated="2026-09-20T13:09:30Z"),
            ],
        )
        payload = self.run_json(
            "runs", "--input", snapshot, "--window", "2026-09-19..2026-09-21",
            "--event", "pull_request", "--conclusion", "success",
            "--branch", "topic",
        )
        self.assertEqual([run["id"] for run in payload["runs"]], [40])

    # -- empty-window diagnosis ------------------------------------------

    def test_runs_empty_window_names_only_blocking_filters(self):
        # The case that makes guessing unsafe: the window IS covered, so
        # telling the user to widen the fetch or drop --input would send
        # them away from the filter actually responsible. The fixture run
        # is a failed `pull_request` on `topic`, so only --conclusion
        # rejects it; naming the matching --event or --branch would send
        # the user after a flag that excluded nothing.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [self._document(1, conclusion="failure",
                            created="2026-09-15T10:00:00Z",
                            updated="2026-09-15T10:09:30Z")],
        )
        result = self.run_cli(
            "runs", "--input", snapshot, "--window", "2026-09-14..2026-09-16",
            "--conclusion", "success",
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("--conclusion success", result.stderr)
        self.assertIn("1 run(s) fall inside it", result.stderr)
        for absent in ("--event", "--branch", "--limit", "drop --input"):
            self.assertNotIn(absent, result.stderr)
        # The empty selection still renders, with the diagnosis beside it.
        self.assertIn("| Run | Date | Event | Branch | Conclusion", result.stdout)

    def test_runs_empty_window_advises_the_snapshot_side(self):
        # A --input snapshot the window is not covered by gets its own
        # advice: re-request a window the snapshot holds, not one to fetch.
        snapshot = self.snapshot_one()
        result = self.run_cli(
            "runs", "--input", snapshot, "--window", "2026-09-02..2026-09-03"
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("no run matched the requested window", result.stderr)
        self.assertIn("the snapshot given to --input holds no run in it",
                      result.stderr)
        self.assertIn("drop --input", result.stderr)

    def test_runs_empty_window_names_both_filters_two_candidates_failed(self):
        """A filter blocks when it rejects any candidate, not only all.

        With one candidate the two readings agree, so this needs a
        heterogeneous set: judging "every" here would report no blocking
        filter at all and fall through to blaming attempt grouping, which
        has nothing to do with it -- there are no reruns in sight.
        """
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(80, event="push",
                               created="2026-09-15T10:00:00Z",
                               updated="2026-09-15T10:09:30Z"),
                self._document(81, conclusion="failure",
                               created="2026-09-15T10:00:00Z",
                               updated="2026-09-15T10:09:30Z"),
            ],
        )
        result = self.run_cli(
            "runs", "--input", snapshot, "--window", "2026-09-14..2026-09-16",
            "--conclusion", "success", "--event", "pull_request",
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("--conclusion success", result.stderr)
        self.assertIn("--event pull_request", result.stderr)
        self.assertNotIn("--attempts", result.stderr)
        self.assertIn("2 run(s) fall inside it", result.stderr)

    def test_runs_empty_window_suggests_attempts_all_when_a_rerun_would_pass(self):
        # `--attempts first` always loads attempt 1 beside the latest, and
        # reruns share their run's creation time, so both siblings sit in
        # the window. Only attempt 1 is a candidate here; counting attempt
        # 2's mismatch would misreport what happened, and the narrower fix
        # the user actually wants is named too.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(60, attempt=1, conclusion="failure",
                               created="2026-09-15T10:00:00Z",
                               updated="2026-09-15T10:09:30Z"),
                self._document(60, attempt=2, conclusion="success",
                               created="2026-09-15T10:00:00Z",
                               updated="2026-09-15T15:09:30Z"),
            ],
        )
        result = self.run_cli(
            "runs", "--input", snapshot, "--window", "2026-09-14..2026-09-16",
            "--conclusion", "success",
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("--conclusion success", result.stderr)
        self.assertIn("so --attempts all would match", result.stderr)
        self.assertIn("1 run(s) fall inside it", result.stderr)

    def test_runs_empty_window_omits_attempts_all_when_both_attempts_failed(self):
        # Both attempts failed, so --attempts all would not help and must
        # not be suggested.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(61, attempt=1, conclusion="failure",
                               created="2026-09-15T10:00:00Z",
                               updated="2026-09-15T10:09:30Z"),
                self._document(61, attempt=2, conclusion="failure",
                               created="2026-09-15T10:00:00Z",
                               updated="2026-09-15T15:09:30Z"),
            ],
        )
        result = self.run_cli(
            "runs", "--input", snapshot, "--window", "2026-09-14..2026-09-16",
            "--conclusion", "success",
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("--conclusion success", result.stderr)
        self.assertNotIn("--attempts all", result.stderr)

    def test_runs_empty_window_ignores_a_non_candidate_attempt_mismatch(self):
        # Attempt 2 ran on another branch, but it is not a candidate under
        # --attempts first, so its --branch mismatch did not empty the
        # window and naming --branch would send the user after the wrong
        # flag.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(63, attempt=1, conclusion="failure",
                               branch="main",
                               created="2026-09-15T10:00:00Z",
                               updated="2026-09-15T10:09:30Z"),
                self._document(63, attempt=2, conclusion="failure",
                               branch="other",
                               created="2026-09-15T10:00:00Z",
                               updated="2026-09-15T15:09:30Z"),
            ],
        )
        result = self.run_cli(
            "runs", "--input", snapshot, "--window", "2026-09-14..2026-09-16",
            "--conclusion", "success", "--branch", "main",
        )
        self.assertEqual(result.returncode, 0)
        self.assertIn("--conclusion success", result.stderr)
        self.assertNotIn("--branch", result.stderr)

    def test_runs_fetch_arguments_scope_to_the_ci_workflow(self):
        # The fetch must scope itself to `ci.yml` and pass the window and
        # branch server-side, and forbid a server-side `--status`: `gh run
        # list` reports only the latest attempt's conclusion, so filtering
        # there would drop a run whose first attempt succeeded and whose
        # rerun failed before attempt selection ever happens.
        shim_dir = self.dir / "bin"
        shim_dir.mkdir()
        shim = shim_dir / "gh"
        shim.write_text(
            "#!/bin/sh\n"
            'printf \'%s\\n\' "$@" >> "$GH_LOG"\n'
            "printf '[]\\n'\n"
        )
        shim.chmod(0o755)
        log = self.dir / "gh-argv.log"
        env = dict(os.environ)
        env["PATH"] = f"{shim_dir}:{env.get('PATH', '')}"
        env["GH_LOG"] = str(log)
        result = self.run_cli(
            "runs", "--window", "2026-09-14..2026-09-16", "--branch", "main",
            "--conclusion", "success", "--limit", "40", env=env,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        arguments = log.read_text()
        self.assertIn("--workflow", arguments)
        self.assertIn("ci.yml", arguments)
        self.assertIn("--created", arguments)
        self.assertIn("2026-09-14..2026-09-16", arguments)
        self.assertIn("--branch", arguments)
        self.assertIn("main", arguments)
        self.assertIn("--limit", arguments)
        self.assertIn("40", arguments)
        self.assertNotIn("--status", arguments)

    # -- percentiles and aggregates --------------------------------------

    def test_runs_reports_p90_beside_the_median(self):
        # The report quotes a p90 beside the median, both as observed runs
        # (nearest-rank), so a misreported percentile cannot hide behind an
        # interpolated value.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(
                    50 + index,
                    created="2026-09-20T10:00:00Z",
                    updated=f"2026-09-20T10:{index:02d}:00Z",
                )
                for index in range(1, 6)
            ],
        )
        payload = self.run_json(
            "runs", "--input", snapshot, "--window", "2026-09-19..2026-09-21"
        )
        walls = sorted(run["wall_seconds"] for run in payload["runs"])
        self.assertEqual(payload["summary"]["wall_seconds"], 180.0)
        self.assertIn(payload["summary"]["wall_p90"], walls)
        self.assertEqual(payload["summary"]["wall_p90"], 300.0)
        rendered = self.run_cli(
            "runs", "--input", snapshot, "--window", "2026-09-19..2026-09-21"
        )
        self.assertIn("p50 3m00s, p90 5m00s", rendered.stdout)

    def test_runs_per_job_percentiles_spread_across_runs(self):
        # Job durations must spread, or p50 and p90 coincide and a wrong
        # percentile would be invisible in the rendered table.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(
                    50 + index,
                    created="2026-09-20T10:00:00Z",
                    updated="2026-09-20T10:10:00Z",
                    jobs=[self._job(
                        "fmt + clippy",
                        "2026-09-20T10:00:00Z",
                        f"2026-09-20T10:{minutes:02d}:00Z",
                    )],
                )
                for index, minutes in enumerate((1, 2, 3, 4, 9), start=1)
            ],
        )
        payload = self.run_json(
            "runs", "--input", snapshot, "--window", "2026-09-19..2026-09-21"
        )
        job = payload["summary"]["jobs"]["fmt + clippy"]
        self.assertEqual(job["p50"], 180.0)
        self.assertEqual(job["p90"], 540.0)
        rendered = self.run_cli(
            "runs", "--input", snapshot, "--window", "2026-09-19..2026-09-21"
        )
        self.assertIn("| fmt + clippy | 3m00s | 9m00s | 5 |", rendered.stdout)

    def test_runs_job_medians_span_windows_of_runs(self):
        # Two run docs sharing a window: the workflow medians come from all
        # selected runs, and each job's median only counts the runs where
        # it actually ran.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(
                    60,
                    created="2026-09-20T10:00:00Z",
                    updated="2026-09-20T10:09:30Z",
                    jobs=[
                        self._job("fmt + clippy",
                                  "2026-09-20T10:00:05Z",
                                  "2026-09-20T10:03:24Z"),
                        self._job("tests (unit, fast)",
                                  "2026-09-20T10:00:06Z",
                                  "2026-09-20T10:03:52Z"),
                    ],
                ),
                self._document(
                    61,
                    created="2026-09-20T11:00:00Z",
                    updated="2026-09-20T11:09:30Z",
                    jobs=[self._job("fmt + clippy",
                                    "2026-09-20T11:00:05Z",
                                    "2026-09-20T11:04:05Z")],
                ),
            ],
        )
        payload = self.run_json(
            "runs", "--input", snapshot, "--window", "2026-09-19..2026-09-21"
        )
        summary = payload["summary"]
        self.assertEqual(summary["runs"], 2)
        self.assertEqual(summary["wall_seconds"], 570.0)
        self.assertEqual(summary["jobs"]["fmt + clippy"]["p50"], 219.5)
        self.assertEqual(summary["jobs"]["fmt + clippy"]["runs"], 2)
        self.assertEqual(summary["jobs"]["tests (unit, fast)"]["runs"], 1)

    # -- comparison ------------------------------------------------------

    def test_compare_reports_deltas_and_jobs_missing_from_one_window(self):
        # Jobs that exist in only one window still get a row with `n/a` on
        # the missing side, so added or removed shards stay visible both in
        # the JSON and in the rendered table a report quotes.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                self._document(
                    70,
                    created="2026-09-14T10:00:00Z",
                    updated="2026-09-14T10:09:30Z",
                    jobs=[
                        self._job("fmt + clippy",
                                  "2026-09-14T10:00:05Z",
                                  "2026-09-14T10:03:24Z"),
                        self._job("tests (unit, fast)",
                                  "2026-09-14T10:00:06Z",
                                  "2026-09-14T10:03:52Z"),
                    ],
                ),
                self._document(
                    71,
                    created="2026-09-20T10:00:00Z",
                    updated="2026-09-20T10:01:30Z",
                    jobs=[self._job("fmt + clippy",
                                    "2026-09-20T10:00:05Z",
                                    "2026-09-20T10:01:05Z")],
                ),
                self._document(
                    72,
                    created="2026-09-20T10:10:00Z",
                    updated="2026-09-20T10:12:10Z",
                    jobs=[self._job("tests (new-shard, fast)",
                                    "2026-09-20T10:10:05Z",
                                    "2026-09-20T10:12:05Z")],
                ),
            ],
        )
        arguments = ("compare", "--input", snapshot,
                     "--baseline", "2026-09-14..2026-09-14",
                     "--current", "2026-09-20..2026-09-20")
        payload = self.run_json(*arguments)
        rows = {row["job"]: row for row in payload["rows"]}
        clippy = rows["fmt + clippy"]
        self.assertEqual(clippy["before"], 199.0)
        self.assertEqual(clippy["after"], 60.0)
        self.assertEqual(clippy["delta"], -139.0)
        self.assertAlmostEqual(clippy["percent"], -69.8, places=1)
        self.assertIsNone(rows["tests (unit, fast)"]["after"])
        self.assertIsNone(rows["tests (unit, fast)"]["percent"])
        rendered = self.run_cli(*arguments)
        self.assertIn(
            "| tests (new-shard, fast) | n/a | 2m00s | n/a | n/a | 0/1 |",
            rendered.stdout,
        )

    def test_compare_renders_deltas_and_p90_rows_as_scaled_durations(self):
        # A delta is a duration: an improvement must read like the
        # regression it mirrors (sign first, magnitude scaled), and the
        # workflow p90 is quoted in its own row.
        snapshot = self.write_snapshot(
            "snapshot.json",
            [
                # Improvement of 196 s at p50 and p90 together.
                self._document(80, created="2026-09-14T10:00:00Z",
                               updated="2026-09-14T10:09:30Z"),
                self._document(81, created="2026-09-15T10:00:00Z",
                               updated="2026-09-15T10:06:14Z"),
                # An hour-scale improvement (4000 s) needs a second pair
                # of windows.
                self._document(82, created="2026-09-21T10:00:00Z",
                               updated="2026-09-21T12:00:00Z"),
                self._document(83, created="2026-09-22T10:00:00Z",
                               updated="2026-09-22T10:53:20Z"),
            ],
        )
        minutes = self.run_cli(
            "compare", "--input", snapshot,
            "--baseline", "2026-09-14..2026-09-14",
            "--current", "2026-09-15..2026-09-15",
        )
        self.assertEqual(minutes.returncode, 0, minutes.stderr)
        self.assertIn("**workflow (p90)**", minutes.stdout)
        self.assertIn("**-3m16s**", minutes.stdout)
        hours = self.run_cli(
            "compare", "--input", snapshot,
            "--baseline", "2026-09-21..2026-09-21",
            "--current", "2026-09-22..2026-09-22",
        )
        self.assertEqual(hours.returncode, 0, hours.stderr)
        self.assertIn("**-1h06m**", hours.stdout)

    # -- window validation -----------------------------------------------

    def test_runs_window_requires_start_and_end_dates(self):
        snapshot = self.snapshot_one()
        result = self.run_cli(
            "runs", "--input", snapshot, "--window", "2026-09-14"
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("window must be START..END", result.stderr)
        self.assertIn("2026-09-14", result.stderr)


    def test_runs_refuses_a_snapshot_that_is_not_a_list(self):
        snapshot = self.dir / "invalid-snapshot.json"
        snapshot.write_text('{"runs": []}')
        result = self.run_cli("runs", "--input", str(snapshot))
        self.assertEqual(result.returncode, 1)
        self.assertIn("must hold a JSON list", result.stderr)

    def test_runs_snapshot_excludes_release_runs_and_keeps_legacy_ci_runs(self):
        release = dict(RUN, databaseId=91, workflowName="Release")
        ci_run = dict(RUN, databaseId=92, workflowName="CI")
        legacy = dict(RUN, databaseId=93)
        snapshot = self.write_snapshot("ci-runs.json", [release, ci_run, legacy])
        payload = self.run_json("runs", "--input", snapshot)
        self.assertEqual(sorted(run["id"] for run in payload["runs"]), [92, 93])

    def test_runs_warns_when_fetch_limit_may_hide_older_runs(self):
        shim_dir = self.dir / "bin"
        shim_dir.mkdir()
        shim = shim_dir / "gh"
        shim.write_text(
            "#!/usr/bin/env python3\n"
            "import os, sys\n"
            "if sys.argv[1:3] == ['run', 'list']:\n"
            "    print(os.environ['GH_LIST'])\n"
            "elif sys.argv[1:3] == ['run', 'view']:\n"
            "    print(os.environ['GH_VIEW'])\n"
            "else:\n"
            "    raise SystemExit('unexpected gh request')\n"
        )
        shim.chmod(0o755)
        document = self._document(
            1, created="2026-09-20T10:00:00Z", updated="2026-09-20T10:09:30Z"
        )
        environment = {
            **os.environ,
            "PATH": str(shim_dir) + os.pathsep + os.environ["PATH"],
            "GH_LIST": json.dumps([{
                "databaseId": 1, "attempt": 1, "conclusion": "success"
            }]),
            "GH_VIEW": json.dumps(document),
        }
        result = self.run_cli(
            "runs", "--window", "2026-09-20..2026-09-20", "--limit", "1",
            "--cache", str(self.dir / "fetched.json"), env=environment,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("requested window 2026-09-20..2026-09-20", result.stderr)
        self.assertIn("--limit 1", result.stderr)
        self.assertIn("missing older runs", result.stderr)


class GhFailureCommandTests(unittest.TestCase):
    """The real command must print useful `gh` errors without leaking tokens."""

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        gh = bin_dir / "gh"
        gh.write_text(
            "#!/usr/bin/env python3\n"
            "import os, sys\n"
            "sys.stderr.write(os.environ.get('GH_ERROR', ''))\n"
            "raise SystemExit(1)\n"
        )
        gh.chmod(0o755)
        self.environment = {
            **os.environ,
            "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"],
        }

    def run_failed_fetch(self, message):
        environment = {**self.environment, "GH_ERROR": message}
        return subprocess.run(
            [sys.executable, str(SCRIPT), "runs", "--window", "2026-09-14..2026-09-16",
             "--cache", str(self.root / "runs.json")],
            cwd=self.root, env=environment, capture_output=True,
            text=True, check=False,
        )

    def test_failed_fetch_preserves_a_diagnostic_and_handles_blank_stderr(self):
        detailed = self.run_failed_fetch("gh: API rate limit exceeded\n")
        self.assertEqual(detailed.returncode, 1)
        self.assertIn("API rate limit exceeded", detailed.stderr)
        for blank in ("", "   \n"):
            result = self.run_failed_fetch(blank)
            self.assertEqual(result.returncode, 1)
            self.assertIn("returned non-zero exit status 1", result.stderr)

    def test_failed_fetch_redacts_each_credential_shape(self):
        secrets = (
            "ghp_" + "A" * 36,
            "github_pat_" + "B" * 30,
            "Authorization: Bearer " + "C" * 40,
            "https://api.github.com/x?access_token=" + "D" * 40,
            "Bearer " + "E" * 40,
            "token=" + "F" * 40,
        )
        runs = ["A" * 36, "B" * 30, "C" * 40, "D" * 40, "E" * 40, "F" * 40]
        for secret in secrets:
            with self.subTest(secret=secret[:12]):
                result = self.run_failed_fetch(f"rate limit exceeded: {secret}\n")
                self.assertEqual(result.returncode, 1)
                self.assertIn("rate limit exceeded", result.stderr)
                self.assertIn("[redacted]", result.stderr)
                for run in runs:
                    self.assertNotIn(run, result.stderr)

    def test_credential_free_auth_diagnostic_stays_actionable(self):
        result = self.run_failed_fetch("gh: token expired, run gh auth login\n")
        self.assertEqual(result.returncode, 1)
        self.assertIn("token expired", result.stderr)


class JunitCommandTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.report = self.root / "junit.xml"
        self.report.write_text(JUNIT)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        gh = self.bin / "gh"
        gh.write_text(
            "#!/usr/bin/env python3\n"
            "import os, pathlib, sys\n"
            "if sys.argv[1:4] != ['run', 'view', '7']:\n"
            "    raise SystemExit('unexpected gh request: ' + repr(sys.argv))\n"
            "print(pathlib.Path(os.environ['CI_TIMINGS_GH_FIXTURE']).read_text())\n"
        )
        gh.chmod(0o755)
        self.gh_fixture = self.root / "gh-run.json"

    def run_command(self, *args, jobs=None):
        environment = {**os.environ, "PATH": str(self.bin) + os.pathsep + os.environ["PATH"]}
        if jobs is not None:
            self.gh_fixture.write_text(json.dumps({"jobs": jobs}))
            environment["CI_TIMINGS_GH_FIXTURE"] = str(self.gh_fixture)
        return subprocess.run(
            [sys.executable, str(SCRIPT), "junit", str(self.report), *args],
            env=environment, capture_output=True, text=True, check=False,
        )

    def test_junit_json_counts_cases_and_slowest_test(self):
        result = self.run_command("--json", "--top", "2")
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = json.loads(result.stdout)
        self.assertEqual(summary["cases"], 3)
        self.assertEqual(summary["failures"], 1)
        self.assertEqual(summary["seconds"], 12.5)
        self.assertEqual(summary["p50"], 1.0)
        self.assertEqual(summary["p95"], 11.0)
        self.assertEqual(summary["slowest"][0], ["test_gamma", 11.0])
        self.assertEqual(len(summary["suites"]), 2)

    def test_retried_cases_count_once_in_a_real_nextest_artifact(self):
        self.report.write_bytes(
            (Path(__file__).resolve().parent / "fixtures/nextest-junit/flaky-result-fail.xml").read_bytes()
        )
        result = self.run_command("--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = json.loads(result.stdout)
        self.assertEqual((summary["cases"], summary["failures"], summary["errors"]), (3, 2, 0))
        self.assertEqual(len(summary["suites"]), 1)

    def test_percentiles_use_nearest_rank_for_real_junit_cases(self):
        cases = "".join(
            f'<testcase name="case-{n}" classname="suite" time="{n}"/>'
            for n in range(1, 12)
        )
        self.report.write_text(f'<testsuites><testsuite name="suite">{cases}</testsuite></testsuites>')
        result = self.run_command("--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = json.loads(result.stdout)
        self.assertEqual((summary["p50"], summary["p95"]), (6.0, 11.0))

    def test_markdown_separates_test_step_from_compile_time(self):
        jobs = [self.job("tests (unit, fast)", "Run fast shard", "10:04:18", "10:05:00")]
        result = self.run_command("--run", "7", "--label", "tests (unit, fast)", jobs=jobs)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tests (unit, fast): 3 test(s)", result.stdout)
        self.assertIn("Test step elapsed 4m18s (86 % of the job wall clock)", result.stdout)
        self.assertIn("their summed time is not a wall-clock share", result.stdout)
        self.assertIn("| cli::parse | 1 | 11.0 |", result.stdout)

    def test_empty_junit_artifact_has_readable_percentiles(self):
        self.report.write_text('<testsuites name="empty" tests="0"></testsuites>')
        result = self.run_command("--label", "tests (empty)")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tests (empty): 0 test(s)", result.stdout)
        self.assertIn("p50 n/a, p95 n/a", result.stdout)

    @staticmethod
    def job(name, step, step_end, job_end=None):
        document = {
            "name": name,
            "steps": [{
                "name": step,
                "startedAt": "2026-09-20T10:00:00Z",
                "completedAt": f"2026-09-20T{step_end}Z",
            }],
        }
        if job_end is not None:
            document["startedAt"] = "2026-09-20T10:00:00Z"
            document["completedAt"] = f"2026-09-20T{job_end}Z"
        return document

    def test_job_selection_prefers_label_and_allows_explicit_override(self):
        jobs = [
            self.job("doctests + release build", "Documentation tests", "10:02:00"),
            self.job("tests (unit, fast)", "Run fast shard", "10:04:18"),
        ]
        matched = self.run_command("--run", "7", "--label", "tests (unit, fast)", "--json", jobs=jobs)
        self.assertEqual(matched.returncode, 0, matched.stderr)
        self.assertEqual(json.loads(matched.stdout)["job"], {
            "run": 7, "name": "tests (unit, fast)",
            "wall_seconds": None, "step_seconds": 258.0,
        })
        explicit = self.run_command(
            "--run", "7", "--label", "tests (unit, fast)",
            "--job", "doctests + release build", "--json", jobs=jobs,
        )
        self.assertEqual(explicit.returncode, 0, explicit.stderr)
        self.assertEqual(json.loads(explicit.stdout)["job"]["name"], "doctests + release build")
        unknown = self.run_command("--run", "7", "--job", "nope", jobs=jobs)
        self.assertNotEqual(unknown.returncode, 0)
        self.assertIn("not found", unknown.stderr)

    def test_ambiguous_job_selection_requires_an_explicit_job(self):
        jobs = [
            self.job("doctests + release build", "Documentation tests", "10:02:00"),
            self.job("tests (unit, fast)", "Run fast shard", "10:04:18"),
        ]
        result = self.run_command("--run", "7", jobs=jobs)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pass --job", result.stderr)

    def test_single_test_job_is_selected_without_a_matching_label(self):
        jobs = [
            self.job("tests (unit, fast)", "Run fast shard", "10:04:18"),
            self.job("fmt + clippy", "Clippy", "10:03:00"),
        ]
        result = self.run_command("--run", "7", "--json", jobs=jobs)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["job"]["name"], "tests (unit, fast)")
        self.assertEqual(json.loads(result.stdout)["job"]["step_seconds"], 258.0)


    def test_junit_job_step_ignores_nextest_installation(self):
        jobs = [{
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
        }]
        result = self.run_command(
            "--run", "7", "--label", "tests (unit, fast)", "--json", jobs=jobs
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["job"]["step_seconds"], 258.0)




class FetchTests(unittest.TestCase):
    """Fetch-path scenarios through the real `runs` command and a fake `gh`.

    Every case starts `scripts/ci-timings runs` as a subprocess with a
    private fake `gh` on `PATH` that records every call and serves a
    scripted `gh run list` / `gh run view --json --attempt` answer from a
    state file. Assertions cover what an operator meets: the rendered or
    JSON output, the snapshot persisted into a disposable `--cache` file
    under the temp root, and the argv sequence the fetch actually sent to
    `gh`. No case touches the real `gh`, the network, or writes outside
    its temp root; a view request the fake `gh` has no scripted answer for
    fails the invocation loudly instead of faking an empty result.
    """

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        gh = bin_dir / "gh"
        gh.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "with Path(os.environ['GH_CALLS']).open('a') as calls:\n"
            "    calls.write(json.dumps(args) + '\\n')\n"
            "if args[:2] == ['run', 'list']:\n"
            "    listed = json.loads(Path(os.environ['GH_LIST']).read_text())\n"
            "    # `gh run list` honors --status against the latest attempt\n"
            "    # only; mirroring that is what proves the real command never\n"
            "    # relies on it for the local conclusion filter.\n"
            "    if '--status' in args:\n"
            "        wanted = args[args.index('--status') + 1]\n"
            "        listed = [entry for entry in listed\n"
            "                  if entry.get('conclusion') == wanted]\n"
            "    print(json.dumps(listed))\n"
            "elif args[:2] == ['run', 'view'] and '--attempt' in args:\n"
            "    views = json.loads(Path(os.environ['GH_VIEWS']).read_text())\n"
            "    key = str(args[2]) + ':' + args[args.index('--attempt') + 1]\n"
            "    if key not in views:\n"
            "        raise SystemExit('fake gh has no scripted view: ' + key)\n"
            "    print(json.dumps(views[key]))\n"
            "else:\n"
            "    raise SystemExit('unexpected gh request: ' + repr(args))\n"
        )
        gh.chmod(0o755)
        self.list = self.root / "gh-list.json"
        self.views = self.root / "gh-views.json"
        self.calls = self.root / "gh-calls.jsonl"
        self.cache = self.root / "cache" / "ci-runs.json"
        self.environment = {
            **os.environ,
            "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"],
            "GH_LIST": str(self.list),
            "GH_VIEWS": str(self.views),
            "GH_CALLS": str(self.calls),
        }

    # -- fixture helpers ---------------------------------------------------

    def set_listing(self, entries):
        """Script the next `gh run list` answer."""
        self.list.write_text(json.dumps(entries))

    def set_views(self, documents):
        """Script `gh run view` answers keyed by `<runId>:<attempt>`."""
        self.views.write_text(json.dumps(documents))

    def view_document(self, run_id, *, attempt, conclusion="success",
                      created="2026-09-20T10:00:00Z", updated=None):
        """A full `gh run view --json` document the fake `gh` can serve."""
        return {
            "databaseId": run_id,
            "name": "ci.yml",
            "displayTitle": "CI",
            "event": "pull_request",
            "conclusion": conclusion,
            "createdAt": created,
            "startedAt": created,
            "updatedAt": updated or "2026-09-20T10:09:30Z",
            "attempt": attempt,
            "headBranch": "topic",
            "workflowName": "CI",
            "jobs": [],
        }

    def seed_cache(self, documents):
        """Place a run snapshot on disk as a previous fetch left it."""
        self.cache.parent.mkdir(parents=True, exist_ok=True)
        self.cache.write_text(json.dumps(list(documents)))

    def gh_calls(self):
        """Every argv the fetch has handed to `gh`, in order."""
        return [
            json.loads(line) for line in self.calls.read_text().splitlines()
        ]

    def view_attempts(self):
        """The `--attempt` values of the view calls made, in order."""
        return [
            call[call.index("--attempt") + 1]
            for call in self.gh_calls() if call[1] == "view"
        ]

    def run_cli(self, *arguments):
        """Run `ci-timings ...` with the disposable `--cache` always set.

        Without it the script would fall back to the repository's real
        `target/ci-timings` cache and both leak host state into the sample
        and let it answer the fetch instead of `gh`.
        """
        return subprocess.run(
            [sys.executable, str(SCRIPT), *arguments,
             "--cache", str(self.cache)],
            cwd=self.root, env=self.environment,
            capture_output=True, text=True, check=False,
        )

    def run_json(self, *arguments):
        result = self.run_cli(*arguments, "--json")
        self.assertEqual(
            result.returncode, 0,
            f"unexpected failure: {result.stderr}",
        )
        return json.loads(result.stdout)

    # -- attempt selection -------------------------------------------------

    def test_fetch_all_attempts_reaches_intermediate_reruns(self):
        # `gh run list` only ever describes attempt 3; attempts 1 and 2
        # exist only when requested by number, and `--attempts all` must
        # request them all -- a silent drop would shrink `--attempts all`
        # aggregates to whatever the listing happened to describe.
        self.set_listing([{"databaseId": 77, "attempt": 3, "conclusion": "success"}])
        self.set_views({
            f"77:{number}": self.view_document(77, attempt=number)
            for number in (1, 2, 3)
        })
        payload = self.run_json(
            "runs", "--window", "2026-09-19..2026-09-21",
            "--attempts", "all",
        )
        self.assertEqual(
            sorted(run["attempt"] for run in payload["runs"]), [1, 2, 3]
        )
        self.assertEqual(self.view_attempts(), ["1", "2", "3"])
        stored = json.loads(self.cache.read_text())
        self.assertEqual(sorted(item["attempt"] for item in stored), [1, 2, 3])

    def test_fetch_first_attempt_keeps_the_original_run(self):
        # The listing describes attempt 2, but the `first` default still
        # fetches attempt 1 by number and keeps it as the run's sample:
        # a rerun displacing the original would re-time the whole window.
        self.set_listing([{"databaseId": 77, "attempt": 2, "conclusion": "success"}])
        self.set_views({
            "77:1": self.view_document(77, attempt=1),
            "77:2": self.view_document(77, attempt=2),
        })
        payload = self.run_json(
            "runs", "--window", "2026-09-19..2026-09-21"
        )
        self.assertEqual([run["attempt"] for run in payload["runs"]], [1])
        self.assertEqual(self.view_attempts(), ["1", "2"])

    # -- cache reuse -------------------------------------------------------

    def test_fetch_skips_attempts_already_cached(self):
        # Attempt 1 sits in the cache from an earlier fetch; only the
        # attempt the cache cannot answer is requested from `gh`, and the
        # cache then holds every attempt of the run.
        self.seed_cache([self.view_document(77, attempt=1,
                                            conclusion="failure")])
        self.set_listing([{"databaseId": 77, "attempt": 2, "conclusion": "success"}])
        self.set_views({"77:2": self.view_document(77, attempt=2)})
        payload = self.run_json(
            "runs", "--window", "2026-09-19..2026-09-21", "--attempts", "all",
        )
        self.assertEqual(
            sorted(run["attempt"] for run in payload["runs"]), [1, 2]
        )
        # The cached attempt 1 answered itself; only attempt 2 asked `gh`.
        self.assertEqual(self.view_attempts(), ["2"])
        stored = json.loads(self.cache.read_text())
        self.assertEqual(sorted(item["attempt"] for item in stored), [1, 2])

    def test_fetch_reuses_a_cached_document_without_refetching(self):
        # A complete cache serves the query with no `gh run view` call at
        # all, so a re-measurement costs no fetch.
        cached = self.view_document(77, attempt=1)
        self.seed_cache([cached])
        self.set_listing([{"databaseId": 77, "attempt": 1, "conclusion": "success"}])
        self.set_views({})  # Any view request would fail the invocation.
        first = self.run_json(
            "runs", "--window", "2026-09-19..2026-09-21"
        )
        self.assertEqual([run["id"] for run in first["runs"]], [77])
        self.assertEqual(self.view_attempts(), [])
        self.calls.write_text("")
        second = self.run_json(
            "runs", "--window", "2026-09-19..2026-09-21"
        )
        self.assertEqual([run["id"] for run in second["runs"]], [77])
        self.assertEqual(
            [call for call in self.gh_calls() if call[1] == "view"], []
        )

    # -- conclusion filter after fetching ----------------------------------

    def test_fetch_then_filter_recovers_a_run_whose_rerun_failed(self):
        """The fetch-before-filter pipeline: listing shows only the rerun.

        `gh run list` reports attempt 2's `failure`, so a server-side
        `--status success` would drop the run outright -- the fake `gh`
        honors `--status` the way the real one does exactly for this
        purpose. Fetching both attempts and applying the conclusion
        locally keeps attempt 1's success in the report.
        """
        self.set_listing([{"databaseId": 27, "attempt": 2, "conclusion": "failure"}])
        self.set_views({
            "27:1": self.view_document(27, attempt=1, conclusion="success"),
            "27:2": self.view_document(27, attempt=2, conclusion="failure"),
        })
        payload = self.run_json(
            "runs", "--window", "2026-09-19..2026-09-21",
            "--conclusion", "success",
        )
        listing = [call for call in self.gh_calls() if call[1] == "list"][0]
        self.assertNotIn("--status", listing)
        self.assertEqual(
            [(run["id"], run["attempt"], run["conclusion"])
             for run in payload["runs"]],
            [(27, 1, "success")],
        )

    # -- cache scope -------------------------------------------------------

    def test_fetch_ignores_unrelated_cached_runs(self):
        """The sample follows the query, not the local cache's history.

        A snapshot accumulates every run ever fetched. Returning all of it
        would make the same command report different medians depending on
        what someone fetched earlier on that machine; the stale run must
        stay on disk for a later query that actually names it.
        """
        self.seed_cache([self.view_document(
            99, attempt=1,
            created="2026-01-01T10:00:00Z",
            updated="2026-01-01T10:09:30Z",
        )])
        self.set_listing([{"databaseId": 77, "attempt": 1, "conclusion": "success"}])
        self.set_views({"77:1": self.view_document(77, attempt=1)})
        payload = self.run_json(
            "runs", "--window", "2026-09-19..2026-09-21"
        )
        self.assertEqual([run["id"] for run in payload["runs"]], [77])
        stored = json.loads(self.cache.read_text())
        self.assertEqual(sorted(item["databaseId"] for item in stored), [77, 99])

    # -- truncation --------------------------------------------------------

    def test_fetch_reports_a_listing_that_filled_the_limit(self):
        # A listing exactly as long as --limit means `gh` may have had
        # more: the sample is a truncation, and the report must say so
        # instead of passing a partial median for a complete one.
        self.set_listing([
            {"databaseId": run_id, "attempt": 1, "conclusion": "success"}
            for run_id in (1, 2, 3)
        ])
        self.set_views({
            f"{run_id}:1": self.view_document(run_id, attempt=1)
            for run_id in (1, 2, 3)
        })
        window = ("--window", "2026-09-19..2026-09-21")
        filled = self.run_cli("runs", *window, "--limit", "3")
        self.assertEqual(filled.returncode, 0, filled.stderr)
        self.assertIn("filled --limit 3", filled.stderr)
        self.assertIn("missing older runs", filled.stderr)
        headroom = self.run_cli("runs", *window, "--limit", "4")
        self.assertEqual(headroom.returncode, 0, headroom.stderr)
        self.assertEqual(headroom.stderr, "")

    # -- unfinished runs ---------------------------------------------------

    def test_fetch_skips_unfinished_runs(self):
        # An unfinished run has no conclusion and no completed job
        # timestamps: it is skipped without a view request, kept out of
        # the sample, and nothing half-fetched may land in the cache.
        self.set_listing([{"databaseId": 77, "attempt": 1, "conclusion": None}])
        self.set_views({})  # Any view request would fail the invocation.
        payload = self.run_json(
            "runs", "--window", "2026-09-19..2026-09-21"
        )
        self.assertEqual(payload["runs"], [])
        self.assertEqual(self.view_attempts(), [])
        self.assertEqual([call[1] for call in self.gh_calls()], ["list"])
        self.assertFalse(self.cache.exists())


class LogCacheCommandTests(unittest.TestCase):
    """Run the real cache command with a private root and a fixture `gh`."""

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        self.script = scripts / "ci-timings"
        self.script.write_bytes(SCRIPT.read_bytes())
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        gh = bin_dir / "gh"
        gh.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "with Path(os.environ['GH_CALLS']).open('a') as calls:\n"
            "    calls.write(json.dumps(args) + '\\n')\n"
            "state = json.loads(Path(os.environ['GH_STATE']).read_text())\n"
            "if args == ['run', 'view', '55', '--json', 'attempt,conclusion']:\n"
            "    print(json.dumps({'attempt': state['attempt'], 'conclusion': state['conclusion']}))\n"
            "elif args[:4] == ['run', 'view', '55', '--log']:\n"
            "    if state.get('fail_log'):\n"
            "        raise SystemExit('cached log must be reused')\n"
            "    print(Path(state['log']).read_text())\n"
            "else:\n"
            "    raise SystemExit('unexpected gh call: ' + repr(args))\n"
        )
        gh.chmod(0o755)
        self.state = self.root / "gh-state.json"
        self.calls = self.root / "gh-calls.jsonl"
        self.log = self.root / "fixture.log"
        self.log.write_text(CACHE_LOG)
        self.environment = {
            **os.environ,
            "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"],
            "GH_STATE": str(self.state),
            "GH_CALLS": str(self.calls),
        }

    def run_cache(self, attempt, conclusion="success", fail_log=False):
        self.state.write_text(json.dumps({
            "attempt": attempt,
            "conclusion": conclusion,
            "log": str(self.log),
            "fail_log": fail_log,
        }))
        return subprocess.run(
            [sys.executable, str(self.script), "cache", "--run", "55", "--json"],
            cwd=self.root, env=self.environment, capture_output=True,
            text=True, check=False,
        )

    def gh_calls(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def run_input(self, log, *arguments):
        self.log.write_text(log)
        return subprocess.run(
            [sys.executable, str(self.script), "cache", "--input", str(self.log), *arguments],
            cwd=self.root, env=self.environment, capture_output=True,
            text=True, check=False,
        )

    def test_input_log_reports_sccache_and_rust_cache_in_json_and_markdown(self):
        result = self.run_input(CACHE_LOG, "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        records = {record["job"]: record for record in json.loads(result.stdout)}
        release = records["doctests + release build"]
        self.assertEqual(release["sccache"]["executed"], 1015)
        self.assertEqual(release["sccache"]["hits"], 660)
        self.assertEqual(release["sccache"]["misses"], 286)
        self.assertEqual(release["sccache"]["write_errors"], 272)
        self.assertIn("ghac", release["sccache"]["location"])
        self.assertEqual(release["rust_cache"], "hit")
        self.assertNotIn("SDK workspace", records)
        markdown = self.run_input(CACHE_LOG)
        self.assertEqual(markdown.returncode, 0, markdown.stderr)
        self.assertIn(
            "| doctests + release build | 1015 | 660 | 286 | 0 | 70 % | hit |",
            markdown.stdout,
        )
        self.assertIn(
            "| tests (unit, fast) | n/a | n/a | n/a | n/a | n/a | miss |",
            markdown.stdout,
        )

    def test_input_log_counts_backend_errors_in_the_reported_hit_ratio(self):
        errored = CACHE_LOG.replace(
            '"cache_misses":{"counts":{"Rust":286}}',
            '"cache_misses":{"counts":{"Rust":286}},'
            '"cache_errors":{"counts":{"Timeout":946}}',
        )
        result = self.run_input(errored, "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        records = {record["job"]: record for record in json.loads(result.stdout)}
        release = records["doctests + release build"]
        self.assertEqual(release["sccache"]["errors"], 946)
        self.assertAlmostEqual(release["sccache"]["hit_ratio"], 660 / 1892)
        markdown = self.run_input(errored)
        self.assertEqual(markdown.returncode, 0, markdown.stderr)
        self.assertIn(
            "| doctests + release build | 1015 | 660 | 286 | 946 | 35 % | hit |",
            markdown.stdout,
        )

    def test_input_log_distinguishes_restore_errors_from_cache_misses(self):
        log = "\n".join([
            "tests (heavy)\tCache cargo build\t2026-09-21T05:17:40Z Failed to restore: archive extraction failed",
            "tests (cli, fast)\tCache cargo build\t2026-09-21T05:17:40Z Cache not found for keys: Linux-x64-gnu",
        ])
        result = self.run_input(log, "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        records = {record["job"]: record for record in json.loads(result.stdout)}
        self.assertEqual(records["tests (heavy)"]["rust_cache"], "error")
        self.assertEqual(records["tests (cli, fast)"]["rust_cache"], "miss")

    def test_input_log_accepts_unnamed_rust_cache_and_ignores_other_steps(self):
        unnamed = (
            "tests (unit, fast)\tRun Swatinem/rust-cache@v2"
            "\t2026-09-21T05:17:40Z Cache restored successfully"
        )
        result = self.run_input(unnamed, "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            [(record["job"], record["rust_cache"]) for record in json.loads(result.stdout)],
            [("tests (unit, fast)", "hit")],
        )
        unrelated = "\n".join([
            "tests (cli, fast)\tRun fast shard\t2026-09-21T05:20:00Z some output {\"stats\": 1}",
            "tests (cli, fast)\tCache Bun packages\t2026-09-21T05:17:40Z Cache restored successfully",
        ])
        ignored = self.run_input(unrelated, "--json")
        self.assertEqual(ignored.returncode, 0, ignored.stderr)
        self.assertEqual(json.loads(ignored.stdout), [])

    def test_rerun_logs_use_distinct_default_paths_and_cached_logs_are_reused(self):
        first = self.run_cache(1)
        second = self.run_cache(2)
        cached = self.run_cache(1, fail_log=True)
        for result in (first, second, cached):
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("sccache", result.stdout)
        cache = self.root / "target/ci-timings"
        self.assertEqual((cache / "run-55-attempt-1.log").read_text().strip(), CACHE_LOG)
        self.assertEqual((cache / "run-55-attempt-2.log").read_text().strip(), CACHE_LOG)
        log_calls = [call for call in self.gh_calls() if "--log" in call]
        self.assertEqual(log_calls, [
            ["run", "view", "55", "--log", "--attempt", "1"],
            ["run", "view", "55", "--log", "--attempt", "2"],
        ])

    def test_unfinished_run_is_refused_before_download_or_cache_write(self):
        result = self.run_cache(1, conclusion=None)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("has not finished", result.stderr)
        self.assertEqual(self.gh_calls(), [
            ["run", "view", "55", "--json", "attempt,conclusion"],
        ])
        self.assertFalse((self.root / "target/ci-timings").exists())


class RunnerMinuteTests(CliHarness):
    """Runner-minute scenarios driven through the real `runs` command.

    GitHub bills each job's wall clock up to a whole minute, the per-run
    total sums the per-job ceilings, and the report quotes the window's
    runner-minute median and sum beside the wall-clock medians
    (`runner_minutes` and `runner_minutes_total` in `window_summary`).
    Every case asserts those numbers through the CLI's own `--json` and
    Markdown output over an `--input` snapshot, as an operator reads them.
    """

    def _run_document(self, database_id, jobs, *, created="2026-09-20T10:00:00Z",
                      updated="2026-09-20T10:09:30Z"):
        return self._document(
            database_id, created=created, started=created, updated=updated,
            jobs=jobs,
        )

    def test_runner_minutes_round_each_job_up(self):
        # GitHub bills each job up to a whole minute: 59 s and 60 s bill
        # 1, 61 s bills 2, and the total sums per job -- it is never the
        # ceil of the summed wall clock. A run with no billable job at all
        # contributes 0.
        jobs = [
            self._job("a 59s", "2026-09-20T10:00:00Z", "2026-09-20T10:00:59Z"),
            self._job("b 60s", "2026-09-20T10:01:00Z", "2026-09-20T10:02:00Z"),
            self._job("c 61s", "2026-09-20T10:03:00Z", "2026-09-20T10:04:01Z"),
            self._job("d 199s", "2026-09-20T10:05:00Z", "2026-09-20T10:08:19Z"),
            self._job("e 226s", "2026-09-20T10:09:00Z", "2026-09-20T10:12:46Z"),
        ]
        snapshot = self.write_snapshot("snapshot.json", [
            self._run_document(1, jobs, updated="2026-09-20T10:20:00Z"),
            self._run_document(2, [], updated="2026-09-20T10:30:00Z"),
        ])
        payload = self.run_json("runs", "--input", snapshot)
        runs = {run["id"]: run for run in payload["runs"]}
        self.assertEqual(runs[1]["jobs"], {
            "a 59s": 59.0, "b 60s": 60.0, "c 61s": 61.0,
            "d 199s": 199.0, "e 226s": 226.0,
        })
        # 1 + 1 + 2 + 4 + 4, one ceiling per job.
        self.assertEqual(runs[1]["runner_minutes"], 12)
        self.assertEqual(runs[2]["jobs"], {})
        self.assertEqual(runs[2]["runner_minutes"], 0)
        rendered = self.run_cli("runs", "--input", snapshot)
        self.assertEqual(rendered.returncode, 0)
        self.assertEqual(rendered.stderr, "")
        self.assertIn("Runner min", rendered.stdout)
        self.assertIn(
            "| 1 | 2026-09-20 | pull_request | topic | success | "
            "20m00s | 12 |",
            rendered.stdout,
        )
        self.assertIn(
            "| 2 | 2026-09-20 | pull_request | topic | success | "
            "30m00s | 0 |",
            rendered.stdout,
        )
        # Median p50 of [12, 0] per run and the summed total.
        self.assertIn("runner minutes p50 6 min, summed 12 min", rendered.stdout)

    def test_window_summary_medians_and_sums_runner_minutes(self):
        # The window's runner-minute median samples per run (1, 2, 4) and
        # the total sums all of them (7), independently of wall clock.
        runs = [
            self._run_document(
                run_id,
                [self._job("fmt + clippy",
                           "2026-09-20T10:00:00Z",
                           f"2026-09-20T10:{end_minute:02d}:00Z")],
                created=f"2026-09-20T{hour:02d}:00:00Z",
                updated=f"2026-09-20T{hour:02d}:10:00Z",
            )
            for run_id, hour, end_minute in ((1, 10, 1), (2, 11, 2), (3, 12, 4))
        ]
        snapshot = self.write_snapshot("snapshot.json", runs)
        payload = self.run_json("runs", "--input", snapshot)
        summary = payload["summary"]
        runs = {run["id"]: run for run in payload["runs"]}
        self.assertEqual(
            [runs[run_id]["runner_minutes"] for run_id in (1, 2, 3)], [1, 2, 4]
        )
        self.assertEqual(summary["runner_minutes"], 2.0)
        self.assertEqual(summary["runner_minutes_total"], 7)
        rendered = self.run_cli("runs", "--input", snapshot)
        self.assertEqual(rendered.returncode, 0)
        self.assertIn("runner minutes p50 2 min, summed 7 min", rendered.stdout)

    def test_window_summary_without_runs_has_no_runner_minutes(self):
        # An empty window renders instead of failing, with no median to
        # quote and a summed total of 0.
        snapshot = self.snapshot_one()
        payload = self.run_json(
            "runs", "--input", snapshot, "--window", "2026-09-02..2026-09-03"
        )
        summary = payload["summary"]
        self.assertEqual(summary["runs"], 0)
        self.assertIsNone(summary["runner_minutes"])
        self.assertEqual(summary["runner_minutes_total"], 0)
        self.assertEqual(summary["jobs"], {})

    def test_render_runs_markdown_prints_runner_minutes(self):
        runs = [
            self._run_document(1, [
                self._job("fmt + clippy",
                          "2026-09-20T10:00:05Z",
                          "2026-09-20T10:03:24Z"),
                self._job("tests (unit, fast)",
                          "2026-09-20T10:00:06Z",
                          "2026-09-20T10:03:52Z"),
            ]),
        ]
        snapshot = self.write_snapshot("snapshot.json", runs)
        rendered = self.run_cli("runs", "--input", snapshot)
        self.assertEqual(rendered.returncode, 0)
        self.assertEqual(rendered.stderr, "")
        self.assertIn("Runner min", rendered.stdout)
        # 199 s and 226 s bill 4 minutes each inside the 9m30s wall clock.
        self.assertIn("| 1 | 2026-09-20 | pull_request | topic | success | "
                      "9m30s | 8 |", rendered.stdout)
        self.assertIn("runner minutes p50 8 min, summed 8 min", rendered.stdout)


class StepTimingTests(CliHarness):
    """Per-step timing scenarios driven through the real `steps` command.

    `step_durations` (which steps count at all), `step_summary` (the
    per-job, per-step p50/p90 aggregates), and `--job` filtering all run
    here through their supported boundary: a `steps --input snapshot`
    invocation whose summaries, run count, and rendered table an operator
    observes.
    """

    def _steps_run(self, run_id, jobs, *, hour=10):
        """Wrap step-bearing jobs in a full snapshot document at `hour`."""
        stamp = f"2026-09-20T{hour:02d}:00:00Z"
        updated = f"2026-09-20T{hour:02d}:10:00Z"
        return self._document(
            run_id, created=stamp, started=stamp, updated=updated, jobs=jobs,
        )

    def _steps_document(self):
        """A `gh run view` document with counted and skipped steps."""
        return {
            "jobs": [
                {
                    "name": "tests (unit, fast)",
                    "conclusion": "success",
                    "steps": [
                        {"name": "Install cargo-nextest", "number": 2,
                         "startedAt": "2026-09-20T10:00:00Z",
                         "completedAt": "2026-09-20T10:00:10Z",
                         "conclusion": "success"},
                        {"name": "Run fast shard", "number": 3,
                         "startedAt": "2026-09-20T10:01:00Z",
                         "completedAt": "2026-09-20T10:04:00Z",
                         "conclusion": "success"},
                        # A conditionally skipped step carries timestamps
                        # but never ran.
                        {"name": "Upload artifact", "number": 4,
                         "startedAt": "2026-09-20T10:04:00Z",
                         "completedAt": "2026-09-20T10:04:30Z",
                         "conclusion": "skipped"},
                        # The Go zero start is a half-finished step.
                        {"name": "Wait for shards", "number": 5,
                         "startedAt": "0001-01-01T00:00:00Z",
                         "completedAt": "2026-09-20T10:04:30Z",
                         "conclusion": "success"},
                    ],
                },
                # A skipped job contributes nothing, even with steps.
                {"name": "platform contracts (arm64)", "conclusion": "skipped",
                 "steps": [{"name": "Build", "number": 1,
                            "startedAt": "2026-09-20T10:00:00Z",
                            "completedAt": "2026-09-20T10:01:00Z"}]},
            ]
        }

    def test_steps_counts_only_measured_steps(self):
        # Skipped steps (even with usable timestamps), half-finished steps
        # (`gh` serializes a running step's start as the Go zero time),
        # and skipped jobs all contribute nothing: only the measured
        # "tests (unit, fast)" steps appear in the summary.
        document = self._steps_document()
        snapshot = self.write_snapshot(
            "snapshot.json",
            [self._steps_run(51, document["jobs"])],
        )
        payload = self.run_json("steps", "--input", snapshot, "--json")
        self.assertEqual(payload["runs"], 1)
        summary = payload["summary"]
        # A skipped job drops out entirely, like job_seconds drops it.
        self.assertEqual(sorted(summary), ["tests (unit, fast)"])
        fast = summary["tests (unit, fast)"]
        self.assertEqual(
            sorted(fast), ["Install cargo-nextest", "Run fast shard"]
        )
        self.assertEqual(
            fast["Run fast shard"], {"p50": 180.0, "p90": 180.0, "runs": 1}
        )
        self.assertEqual(fast["Install cargo-nextest"]["p50"], 10.0)
        # A snapshot holding only the skipped job aggregates nothing.
        skipped_snapshot = self.write_snapshot(
            "skipped.json",
            [self._steps_run(52, [document["jobs"][1]])],
        )
        skipped = self.run_json("steps", "--input", skipped_snapshot, "--json")
        self.assertEqual(skipped["summary"], {})
        rendered = self.run_cli(
            "steps", "--input", snapshot, "--window", "2026-09-19..2026-09-21"
        )
        self.assertEqual(rendered.returncode, 0)
        self.assertEqual(rendered.stderr, "")
        self.assertIn(
            "| tests (unit, fast) | Run fast shard | 3m00s | 3m00s | 1 |",
            rendered.stdout,
        )
        for absent in ("Upload artifact", "Wait for shards",
                       "platform contracts (arm64)"):
            self.assertNotIn(absent, rendered.stdout)

    def test_steps_keeps_two_steps_sharing_a_name(self):
        # The same action can run twice in one job; the 1-based step
        # number disambiguates the collision, and neither duration is lost.
        jobs = [{
            "name": "tests (unit, fast)", "conclusion": "success",
            "steps": [
                {"name": "Run", "number": 1,
                 "startedAt": "2026-09-20T10:00:00Z",
                 "completedAt": "2026-09-20T10:00:30Z",
                 "conclusion": "success"},
                {"name": "Run", "number": 2,
                 "startedAt": "2026-09-20T10:00:30Z",
                 "completedAt": "2026-09-20T10:01:00Z",
                 "conclusion": "success"},
            ],
        }]
        snapshot = self.write_snapshot("snapshot.json", [self._steps_run(53, jobs)])
        payload = self.run_json("steps", "--input", snapshot, "--json")
        steps = payload["summary"]["tests (unit, fast)"]
        self.assertEqual(sorted(steps), ["Run", "Run (#2)"])
        self.assertEqual(steps["Run"]["p50"], 30.0)
        self.assertEqual(steps["Run (#2)"]["p50"], 30.0)

    def _snapshot_with_two_runs(self, name):
        """Two runs with the same steps, one hour apart, in one snapshot.

        Run 41 measures `Run fast shard` at 180 s, run 42 at 240 s, so the
        aggregate's p50 and p90 are distinct observed samples.
        """
        first = self._steps_document()["jobs"][0]
        second = {
            "name": "tests (unit, fast)", "conclusion": "success",
            "steps": [
                {"name": "Install cargo-nextest", "number": 2,
                 "startedAt": "2026-09-20T11:00:00Z",
                 "completedAt": "2026-09-20T11:00:20Z",
                 "conclusion": "success"},
                {"name": "Run fast shard", "number": 3,
                 "startedAt": "2026-09-20T11:01:00Z",
                 "completedAt": "2026-09-20T11:05:00Z",
                 "conclusion": "success"},
            ],
        }
        return self.write_snapshot(name, [
            self._steps_run(41, [first], hour=10),
            self._steps_run(42, [second], hour=11),
        ])

    def test_steps_aggregates_with_p50_and_p90_across_runs(self):
        # Nearest-rank p90 over two samples: the later value. Both
        # medians are rendered as scaled durations, and the step samples
        # spread across runs stay counted.
        snapshot = self._snapshot_with_two_runs("snapshot.json")
        payload = self.run_json("steps", "--input", snapshot, "--json")
        self.assertEqual(payload["runs"], 2)
        step = payload["summary"]["tests (unit, fast)"]["Run fast shard"]
        self.assertEqual((step["p50"], step["p90"]), (210.0, 240.0))
        self.assertEqual(step["runs"], 2)
        rendered = self.run_cli("steps", "--input", snapshot)
        self.assertEqual(rendered.returncode, 0)
        self.assertIn("Steps over 2 run(s)", rendered.stdout)
        self.assertIn(
            "| tests (unit, fast) | Run fast shard | 3m30s | 4m00s | 2 |",
            rendered.stdout,
        )

    def test_steps_filter_limits_to_the_named_jobs(self):
        # A repeated `--job` keeps each requested name; a job no run
        # carries aggregates nothing at all, in JSON and in the table.
        snapshot = self._snapshot_with_two_runs("snapshot.json")
        matched = self.run_json(
            "steps", "--input", snapshot, "--job", "tests (unit, fast)", "--json"
        )
        self.assertEqual(
            sorted(matched["summary"]), ["tests (unit, fast)"]
        )
        unmatched = self.run_json(
            "steps", "--input", snapshot, "--job", "no-such-job", "--json"
        )
        self.assertEqual(unmatched["summary"], {})
        rendered = self.run_cli(
            "steps", "--input", snapshot, "--job", "no-such-job"
        )
        self.assertEqual(rendered.returncode, 0)
        self.assertIn("Steps over 2 run(s)", rendered.stdout)
        self.assertIn("| (none) | (none) | n/a | n/a | 0 |", rendered.stdout)


class CompareRunnerMinuteTests(CliHarness):
    """Runner-minute scenarios for `compare`, driven end to end.

    The window summaries `compare` renders come from the same CLI
    invocation an operator runs: runner-minute medians and totals for
    both windows are asserted through its real `--json` output, and the
    report rows the rendered Markdown quotes.
    """

    def test_compare_reports_runner_minutes_median_and_total(self):
        # 90 s bills 2 minutes after; 199 s and 226 s bill 4 + 4 before.
        snapshot = self.write_snapshot("snapshot.json", [
            self._document(
                1,
                created="2026-09-14T10:00:00Z",
                updated="2026-09-14T10:09:30Z",
                jobs=[
                    self._job("fmt + clippy",
                              "2026-09-14T10:00:05Z",
                              "2026-09-14T10:03:24Z"),
                    self._job("tests (unit, fast)",
                              "2026-09-14T10:00:06Z",
                              "2026-09-14T10:03:52Z"),
                ],
            ),
            self._document(
                13,
                created="2026-09-20T10:00:00Z",
                updated="2026-09-20T10:01:30Z",
                jobs=[self._job("fmt + clippy",
                                "2026-09-20T10:00:00Z",
                                "2026-09-20T10:01:30Z")],
            ),
        ])
        arguments = (
            "compare", "--input", snapshot,
            "--baseline", "2026-09-14..2026-09-14",
            "--current", "2026-09-20..2026-09-20",
        )
        payload = self.run_json(*arguments)
        self.assertEqual(payload["baseline"]["summary"]["runner_minutes"], 8.0)
        self.assertEqual(payload["baseline"]["summary"]["runner_minutes_total"], 8)
        self.assertEqual(payload["current"]["summary"]["runner_minutes"], 2.0)
        self.assertEqual(payload["current"]["summary"]["runner_minutes_total"], 2)
        rendered = self.run_cli(*arguments)
        self.assertEqual(rendered.returncode, 0)
        self.assertEqual(rendered.stderr, "")
        self.assertIn(
            "| **runner minutes (median)** | **8 min** | **2 min** | "
            "**-6 min** | **-75 %** | **1/1** |",
            rendered.stdout,
        )
        self.assertIn(
            "| **runner minutes (total)** | **8 min** | **2 min** | "
            "**-6 min** | **-75 %** | **1/1** |",
            rendered.stdout,
        )

