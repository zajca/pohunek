"""Regression checks for the CI timing/measurement helper (stdlib only)."""

import argparse
import contextlib
import io
import importlib.machinery
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "ci-timings"
ROOT = Path(__file__).resolve().parents[2]
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


def _selection(**overrides):
    """Selection args carrying every field the diagnosis reads."""
    defaults = {
        "input": None, "conclusion": None, "event": None, "branch": None,
        "attempts": "first",
    }
    return argparse.Namespace(**{**defaults, **overrides})


class RunTimingTests(unittest.TestCase):
    def test_format_seconds_scales_negative_values_too(self):
        # A delta is a duration: an improvement must read like the
        # regression it mirrors, not fall back to raw seconds.
        self.assertEqual(ci_timings.format_seconds(-196), "-3m16s")
        self.assertEqual(ci_timings.format_seconds(196), "3m16s")
        self.assertEqual(ci_timings.format_seconds(-4000), "-1h06m")
        self.assertEqual(ci_timings.format_seconds(-9), "-9s")
        self.assertEqual(ci_timings.format_seconds(0), "0s")

    def test_format_seconds_scales(self):
        self.assertEqual(ci_timings.format_seconds(9.4), "9s")
        self.assertEqual(ci_timings.format_seconds(83), "1m23s")
        self.assertEqual(ci_timings.format_seconds(3725), "1h02m")

    def test_job_seconds_skips_unfinished_jobs(self):
        seconds = ci_timings.job_seconds(RUN)
        self.assertEqual(seconds["fmt + clippy"], 199.0)
        self.assertNotIn("cargo-udeps (unused dependencies)", seconds)

    def test_job_seconds_skips_skipped_jobs_with_real_timestamps(self):
        # A conditionally skipped job can still carry usable timestamps, so
        # the conclusion is what disqualifies it -- counting it would add a
        # run that never ran.
        skipped = {
            "name": "tests (relay DB, PostgreSQL)",
            "startedAt": "2026-09-20T10:00:00Z",
            "completedAt": "2026-09-20T10:04:00Z",
            "conclusion": "skipped",
        }
        self.assertEqual(ci_timings.job_seconds({"jobs": [skipped]}), {})

    def test_job_seconds_skips_a_half_finished_job(self):
        # A job still running when the run was read has a start but no
        # completion; `gh` serializes that as null rather than the Go zero
        # time, and it carries no duration to measure.
        unfinished = {
            "name": "tests (heavy, PTY + Hermes)",
            "startedAt": "2026-09-20T10:00:00Z",
            "completedAt": None,
            "conclusion": "success",
        }
        self.assertEqual(ci_timings.job_seconds({"jobs": [unfinished]}), {})

    def test_job_seconds_skips_the_go_zero_timestamp(self):
        # `gh` serializes a missing time as Go's zero `time.Time`, not null.
        # Paired with a real completion that parses into a ~2000-year
        # duration, which would dominate every median it entered.
        zero_start = {
            "name": "TS binding drift (xs check)",
            "startedAt": "0001-01-01T00:00:00Z",
            "completedAt": "2026-09-20T10:04:00Z",
            "conclusion": "success",
        }
        self.assertEqual(ci_timings.job_seconds({"jobs": [zero_start]}), {})

    def test_job_seconds_skips_zero_length_jobs(self):
        instant = {
            "name": "fmt + clippy",
            "startedAt": "2026-09-20T10:00:00Z",
            "completedAt": "2026-09-20T10:00:00Z",
            "conclusion": "success",
        }
        self.assertEqual(ci_timings.job_seconds({"jobs": [instant]}), {})

    def test_runs_in_window_spans_the_whole_last_day(self):
        # The window is inclusive of its end date, so a run late on that day
        # belongs to it; the diagnosis and filter_runs must agree on this or
        # they report different populations for the same request.
        window = ci_timings.parse_window("2026-09-14..2026-09-16")
        last_day = _summary(
            createdAt="2026-09-16T23:59:59Z", startedAt="2026-09-16T23:59:59Z"
        )
        next_day = _summary(
            createdAt="2026-09-17T00:00:00Z", startedAt="2026-09-17T00:00:00Z"
        )
        first_day = _summary(
            createdAt="2026-09-14T00:00:00Z", startedAt="2026-09-14T00:00:00Z"
        )
        self.assertEqual(
            ci_timings.runs_in_window([last_day, next_day, first_day], window),
            [last_day, first_day],
        )
        # The same boundary, through the real selection path.
        self.assertEqual(
            [run["created_at"] for run in ci_timings.filter_runs(
                [last_day], window=window
            )],
            [last_day["created_at"]],
        )
        self.assertEqual(
            ci_timings.filter_runs([next_day], window=window), []
        )

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

    def test_filter_runs_keeps_run_whose_rerun_failed(self):
        # The scenario a server-side `gh run list --status success` would
        # lose: `gh run list` reports only attempt 2's failure, so the run
        # would never be fetched, yet attempt 1 is a genuine success sample.
        succeeded = _summary(
            databaseId=27, attempt=1, conclusion="success",
            startedAt="2026-09-20T10:00:00Z",
        )
        rerun = _summary(
            databaseId=27, attempt=2, conclusion="failure",
            startedAt="2026-09-20T15:00:00Z",
        )
        selected = ci_timings.filter_runs(
            [succeeded, rerun], conclusion="success"
        )
        self.assertEqual(
            [(run["id"], run["attempt"]) for run in selected], [(27, 1)]
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

    def test_list_arguments_keeps_conclusion_client_side(self):
        # `gh run list` reports the latest attempt's conclusion, so a
        # server-side --status would drop a run whose first attempt succeeded
        # and whose rerun failed before an attempt is even selected.
        arguments = ci_timings.list_arguments(
            argparse.Namespace(limit=40, event=None, branch=None,
                               conclusion="success"),
            None,
        )
        self.assertNotIn("--status", arguments)
        self.assertNotIn("success", arguments)

    def test_list_arguments_passes_window_and_branch(self):
        arguments = ci_timings.list_arguments(
            argparse.Namespace(limit=40, event=None, branch="main",
                               conclusion=None),
            ci_timings.parse_window("2026-09-14..2026-09-16"),
        )
        self.assertIn("--branch", arguments)
        self.assertIn("main", arguments)
        self.assertIn("--created", arguments)
        self.assertIn("2026-09-14..2026-09-16", arguments)

    def test_wanted_attempts_covers_every_rerun(self):
        # `all` must reach intermediate attempts that `gh run list` never
        # exposes; `first` needs attempt 1 plus the latest one it described.
        self.assertEqual(ci_timings.wanted_attempts(3, "all"), [1, 2, 3])
        self.assertEqual(ci_timings.wanted_attempts(3, "first"), [1, 3])
        self.assertEqual(ci_timings.wanted_attempts(1, "all"), [1])
        self.assertEqual(ci_timings.wanted_attempts(1, "first"), [1])
        self.assertEqual(ci_timings.wanted_attempts(None, "first"), [1])

    def test_empty_window_diagnosis_names_the_filter_that_excluded_runs(self):
        # The case that makes guessing unsafe: the window IS covered, so
        # telling the user to widen the fetch or drop --input would send them
        # away from the filter actually responsible.
        window = ci_timings.parse_window("2026-09-14..2026-09-16")
        covered = _summary(
            conclusion="failure",
            createdAt="2026-09-15T10:00:00Z",
            startedAt="2026-09-15T10:00:00Z",
        )
        for source in (None, "snapshot.json"):
            message = ci_timings.empty_window_diagnosis(
                _selection(input=source, conclusion="success", event=None, branch=None),
                [covered],
                window,
            ).message
            self.assertIn("1 run(s) fall inside it", message)
            self.assertIn("--conclusion success", message)
            self.assertNotIn("--limit", message)
            self.assertNotIn("drop --input", message)

    def test_empty_window_diagnosis_names_only_blocking_filters(self):
        # The fixture run is a successful `pull_request` on `topic`, so only
        # --conclusion rejects it; naming the matching --event would send the
        # user after a flag that excluded nothing.
        window = ci_timings.parse_window("2026-09-14..2026-09-16")
        covered = _summary(
            createdAt="2026-09-15T10:00:00Z", startedAt="2026-09-15T10:00:00Z"
        )
        message = ci_timings.empty_window_diagnosis(
            _selection(input=None, conclusion="failure", event="pull_request", branch="topic"),
            [covered],
            window,
        ).message
        self.assertIn("--conclusion failure", message)
        self.assertNotIn("--event", message)
        self.assertNotIn("--branch", message)

    def test_empty_window_diagnosis_lists_every_blocking_filter(self):
        window = ci_timings.parse_window("2026-09-14..2026-09-16")
        covered = _summary(
            createdAt="2026-09-15T10:00:00Z", startedAt="2026-09-15T10:00:00Z"
        )
        message = ci_timings.empty_window_diagnosis(
            _selection(input=None, conclusion="failure", event="push", branch="main"),
            [covered],
            window,
        ).message
        for expected in ("--conclusion failure", "--event push", "--branch main"):
            self.assertIn(expected, message)

    def test_empty_window_diagnosis_judges_filters_against_candidates(self):
        """A sibling attempt must not be blamed for emptying the window.

        `wanted_attempts("first")` always loads attempt 1 beside the latest,
        and reruns share their run's creation time, so both siblings sit in
        the window. Only attempt 1 is a candidate under `--attempts first`;
        counting attempt 2's mismatch would misreport what happened.
        """
        window = ci_timings.parse_window("2026-09-14..2026-09-16")
        def attempt(number, conclusion):
            return _summary(
                databaseId=60, attempt=number, conclusion=conclusion,
                createdAt="2026-09-15T10:00:00Z",
                startedAt="2026-09-15T10:00:00Z",
            )
        loaded = [attempt(1, "failure"), attempt(2, "success")]
        message = ci_timings.empty_window_diagnosis(
            _selection(conclusion="success"), loaded, window
        ).message
        # One candidate, not two: attempt 2 never reaches the filter stage.
        self.assertIn("1 run(s) fall inside it", message)
        self.assertIn("--conclusion success", message)
        # The narrower fix the user actually wants is named too.
        self.assertIn("--attempts all", message)
        self.assertEqual(
            [(run["id"], run["attempt"]) for run in ci_timings.filter_runs(
                loaded, window=window, conclusion="success", attempts="all"
            )],
            [(60, 2)],
        )

    def test_empty_window_diagnosis_ignores_a_non_candidate_mismatch(self):
        # Attempt 2 ran on another branch, but it is not a candidate under
        # --attempts first, so its --branch mismatch did not empty the
        # window and naming --branch would send the user after the wrong flag.
        window = ci_timings.parse_window("2026-09-14..2026-09-16")
        loaded = [
            _summary(databaseId=63, attempt=1, conclusion="failure",
                     headBranch="main", createdAt="2026-09-15T10:00:00Z",
                     startedAt="2026-09-15T10:00:00Z"),
            _summary(databaseId=63, attempt=2, conclusion="failure",
                     headBranch="other", createdAt="2026-09-15T10:00:00Z",
                     startedAt="2026-09-15T10:00:00Z"),
        ]
        message = ci_timings.empty_window_diagnosis(
            _selection(conclusion="success", branch="main"), loaded, window
        ).message
        self.assertIn("--conclusion success", message)
        self.assertNotIn("--branch", message)

    def test_empty_window_diagnosis_blames_filters_failed_by_any_candidate(self):
        """Two candidates, each failing a different filter, must name both.

        A filter blocks when it rejects *any* candidate, not only when it
        rejects every one. With one candidate the two readings agree, so
        this needs a heterogeneous set: judging "every" here would report no
        blocking filter at all and fall through to blaming attempt grouping,
        which has nothing to do with it -- there are no reruns in sight.
        """
        window = ci_timings.parse_window("2026-09-14..2026-09-16")

        def run(**overrides):
            return _summary(
                createdAt="2026-09-15T10:00:00Z",
                startedAt="2026-09-15T10:00:00Z",
                **overrides,
            )

        loaded = [
            run(databaseId=80, conclusion="success", event="push"),
            run(databaseId=81, conclusion="failure", event="pull_request"),
        ]
        args = _selection(conclusion="success", event="pull_request")
        # Neither run passes both filters, so the window is genuinely empty.
        self.assertEqual(
            ci_timings.filter_runs(
                loaded, window=window, conclusion="success",
                event="pull_request",
            ),
            [],
        )
        message = ci_timings.empty_window_diagnosis(args, loaded, window).message
        self.assertIn("--conclusion success", message)
        self.assertIn("--event pull_request", message)
        self.assertNotIn("--attempts", message)
        self.assertIn("2 run(s) fall inside it", message)

    def test_empty_window_diagnosis_omits_rescue_note_without_a_rescuer(self):
        # Both attempts failed, so --attempts all would not help and must
        # not be suggested.
        window = ci_timings.parse_window("2026-09-14..2026-09-16")
        def attempt(number):
            return _summary(
                databaseId=61, attempt=number, conclusion="failure",
                createdAt="2026-09-15T10:00:00Z",
                startedAt="2026-09-15T10:00:00Z",
            )
        message = ci_timings.empty_window_diagnosis(
            _selection(conclusion="success"),
            [attempt(1), attempt(2)],
            window,
        ).message
        self.assertIn("--conclusion success", message)
        self.assertNotIn("--attempts all", message)

    def test_select_attempts_backs_both_filtering_and_diagnosis(self):
        # One helper, so the diagnosis can never reason about a candidate set
        # that differs from the one filter_runs uses.
        def attempt(number):
            return _summary(databaseId=62, attempt=number,
                            startedAt="2026-09-15T10:00:00Z")
        runs = [attempt(3), attempt(1), attempt(2)]
        self.assertEqual(
            [run["attempt"] for run in ci_timings.select_attempts(runs, "first")],
            [1],
        )
        self.assertEqual(
            sorted(run["attempt"]
                   for run in ci_timings.select_attempts(runs, "all")),
            [1, 2, 3],
        )

    def test_empty_window_diagnosis_rejects_an_impossible_state(self):
        """Every candidate passing every filter contradicts an empty selection.

        `candidates` is `filter_runs`' own post-window pool, so this state
        means the two have drifted apart. That is a bug in the tool, not a
        user error, and must not be dressed up as advice.
        """
        window = ci_timings.parse_window("2026-09-14..2026-09-16")
        covered = _summary(
            createdAt="2026-09-15T10:00:00Z", startedAt="2026-09-15T10:00:00Z"
        )
        # The real pipeline selects this run, so no diagnosis is ever asked for.
        self.assertTrue(ci_timings.filter_runs([covered], window=window))
        with self.assertRaisesRegex(RuntimeError, "invariant violated"):
            ci_timings.empty_window_diagnosis(_selection(), [covered], window)

    def test_empty_window_diagnosis_explains_every_reachable_empty_window(self):
        """Tie the diagnosis to the pipeline and to the branch it reports.

        Testing the diagnosis in isolation is what let a branch survive on a
        premise the real pipeline contradicts, so every case asserts that
        `filter_runs` really is empty before asking for a diagnosis. The
        branch each case claims is then compared against the tag the
        function itself returns, so a case cannot look covered while
        exercising a different branch: a wrong label fails its own case, and
        the closing set comparison catches a branch no case reaches at all.
        """
        window = ci_timings.parse_window("2026-09-14..2026-09-16")

        def run(**overrides):
            return _summary(
                createdAt="2026-09-15T10:00:00Z",
                startedAt="2026-09-15T10:00:00Z",
                **overrides,
            )

        outside = _summary(
            createdAt="2026-09-21T10:00:00Z", startedAt="2026-09-21T10:00:00Z"
        )
        cases = {
            "uncovered, nothing in range": (
                _selection(), [outside], "no run was fetched for it",
                "uncovered-fetch",
            ),
            "uncovered, nothing loaded at all": (
                _selection(), [], "no run was fetched for it",
                "uncovered-fetch",
            ),
            "uncovered, snapshot": (
                _selection(input="snapshot.json"), [outside],
                "the snapshot given to --input holds no run in it",
                "uncovered-snapshot",
            ),
            "conclusion excludes": (
                _selection(conclusion="failure"), [run()],
                "none passed --conclusion failure", "blocking",
            ),
            "branch excludes": (
                _selection(branch="other"), [run()],
                "none passed --branch other", "blocking",
            ),
            "event excludes": (
                _selection(event="push"), [run()],
                "none passed --event push", "blocking",
            ),
            # Both attempts share createdAt, so both are in the window and
            # attempt 1 is a candidate: this reaches the blocking branch with
            # attempt 2 as the rescuable sibling.
            "rerun blocked with a rescuable sibling": (
                _selection(conclusion="success"),
                [run(databaseId=70, attempt=1, conclusion="failure"),
                 run(databaseId=70, attempt=2, conclusion="success")],
                "so --attempts all would match", "blocking-with-rescue",
            ),
            # A stitched-together snapshot can hold attempts with different
            # creation times; then --attempts first picks a candidate that
            # falls outside the window and hides the sibling inside it.
            "attempt selection misses the window": (
                _selection(),
                [_summary(databaseId=71, attempt=1,
                          createdAt="2026-09-21T10:00:00Z",
                          startedAt="2026-09-21T10:00:00Z"),
                 run(databaseId=71, attempt=2)],
                "kept no attempt of theirs inside it; pass --attempts all "
                "to count the attempts that do",
                "no-candidates",
            ),
        }
        reached = set()
        for label, (args, loaded, expected, branch) in cases.items():
            with self.subTest(case=label):
                selected = ci_timings.filter_runs(
                    loaded, window=window, event=args.event,
                    conclusion=args.conclusion, branch=args.branch,
                    attempts=args.attempts,
                )
                self.assertEqual(selected, [], "case must be genuinely empty")
                diagnosis = ci_timings.empty_window_diagnosis(
                    args, loaded, window
                )
                self.assertIn(expected, diagnosis.message)
                # The tag comes from the function's own control flow, so a
                # mislabelled case fails here instead of passing quietly.
                self.assertEqual(diagnosis.branch, branch)
                reached.add(diagnosis.branch)
        # Every branch reachable with a non-empty selection is represented;
        # the invariant branch is unreachable here by construction and has
        # its own test.
        self.assertEqual(
            reached,
            {"uncovered-fetch", "uncovered-snapshot", "blocking",
             "blocking-with-rescue", "no-candidates"},
        )

    def test_selection_coverage_warning_reports_uncovered_windows(self):
        window = ci_timings.parse_window("2026-09-14..2026-09-16")
        args = _selection(input=None, conclusion=None, event=None, branch=None)
        message = ci_timings.selection_coverage_warning(args, window, [], [])
        self.assertIn("2026-09-14..2026-09-16", message)
        covered = _summary(
            createdAt="2026-09-15T10:00:00Z", startedAt="2026-09-15T10:00:00Z"
        )
        self.assertIsNone(
            ci_timings.selection_coverage_warning(
                args, window, [covered], [covered]
            )
        )
        self.assertIsNone(
            ci_timings.selection_coverage_warning(args, None, [], [])
        )

    def test_parse_window_rejects_malformed_input(self):
        with self.assertRaisesRegex(ValueError, "START..END"):
            ci_timings.parse_window("2026-09-14")

    def test_window_summary_reports_p90_beside_the_median(self):
        # The report quotes a p90, so the tool has to be able to print one.
        runs = [
            _summary(databaseId=i, startedAt="2026-09-20T10:00:00Z",
                     updatedAt=f"2026-09-20T10:0{i}:00Z")
            for i in range(1, 6)
        ]
        summary = ci_timings.window_summary(runs)
        walls = sorted(run["wall_seconds"] for run in runs)
        self.assertEqual(summary["wall_p90"], ci_timings.percentile(walls, 0.90))
        # Nearest-rank: the value is an observed run, not an interpolation.
        self.assertIn(summary["wall_p90"], walls)
        self.assertEqual(summary["wall_p90"], walls[-1])
        rendered = ci_timings.render_runs_markdown(runs)
        self.assertIn("p90", rendered)
        self.assertIn(ci_timings.format_seconds(summary["wall_p90"]), rendered)

    def test_window_summary_reports_p90_per_job(self):
        # Job durations must spread, or p50 and p90 coincide and a wrong
        # percentile would be invisible.
        runs = []
        for index, minutes in enumerate((1, 2, 3, 4, 9), start=1):
            runs.append(ci_timings.summarize_run({
                "databaseId": index,
                "conclusion": "success", "event": "pull_request",
                "headBranch": "topic", "displayTitle": "CI",
                "createdAt": "2026-09-20T10:00:00Z",
                "startedAt": "2026-09-20T10:00:00Z",
                "updatedAt": "2026-09-20T10:10:00Z",
                "jobs": [{
                    "name": "fmt + clippy",
                    "startedAt": "2026-09-20T10:00:00Z",
                    "completedAt": f"2026-09-20T10:{minutes:02d}:00Z",
                    "conclusion": "success",
                }],
            }))
        job = ci_timings.window_summary(runs)["jobs"]["fmt + clippy"]
        self.assertEqual(job["p50"], 180.0)
        self.assertEqual(job["p90"], 540.0)
        self.assertIn("9m00s", ci_timings.render_runs_markdown(runs))

    def test_render_compare_markdown_includes_a_p90_row(self):
        baseline = {"runs": 2, "wall_seconds": 584.0, "wall_p90": 697.0,
                    "jobs": {}}
        current = {"runs": 2, "wall_seconds": 388.0, "wall_p90": 699.0,
                   "jobs": {}}
        rendered = ci_timings.render_compare_markdown(baseline, current, [])
        self.assertIn("**workflow (p90)**", rendered)
        self.assertIn("**11m37s**", rendered)
        self.assertIn("**11m39s**", rendered)
        # The median row's improvement scales like a duration.
        self.assertIn("**-3m16s**", rendered)

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



class ErrorReportingTests(unittest.TestCase):
    def test_describe_error_keeps_the_gh_diagnosis(self):
        # Without stderr the user sees only "non-zero exit status", which
        # says nothing about expired auth, a missing scope, or a rate limit.
        error = subprocess.CalledProcessError(
            1, ["gh", "run", "list"],
            stderr="gh: Requires authentication\nTry `gh auth login`\n",
        )
        message = ci_timings.describe_error(error)
        self.assertIn("Requires authentication", message)
        self.assertIn("exit status 1", message)

    def test_describe_error_falls_back_without_stderr(self):
        error = subprocess.CalledProcessError(1, ["gh", "run", "list"])
        self.assertEqual(ci_timings.describe_error(error), str(error))
        blank = subprocess.CalledProcessError(
            1, ["gh", "run", "list"], stderr="   \n"
        )
        self.assertEqual(ci_timings.describe_error(blank), str(blank))
        self.assertEqual(
            ci_timings.describe_error(ValueError("plain")), "plain"
        )

    def test_describe_error_redacts_credentials(self):
        """A secret printed to a terminal or CI log can only be rotated.

        Each pattern is checked separately so a regression in one cannot
        hide behind another still matching.
        """
        secrets = (
            "ghp_" + "A" * 36,
            "github_pat_" + "B" * 30,
            # The scheme word sits between the key and the value here, and
            # a pattern that stops at the first token redacts only "Bearer".
            "Authorization: Bearer " + "C" * 40,
            "https://api.github.com/x?access_token=" + "D" * 40,
            "Bearer " + "E" * 40,
            "token=" + "F" * 40,
        )
        runs = ["A" * 36, "B" * 30, "C" * 40, "D" * 40, "E" * 40, "F" * 40]
        for secret in secrets:
            with self.subTest(secret=secret[:12]):
                error = subprocess.CalledProcessError(
                    1, ["gh"], stderr=f"failed with {secret}"
                )
                message = ci_timings.describe_error(error)
                self.assertIn("[redacted]", message)
                for run in runs:
                    self.assertNotIn(run, message)

    def test_describe_error_keeps_credential_free_diagnostics(self):
        # Redaction must not eat the reason: these words appear in ordinary
        # gh messages that carry no secret.
        error = subprocess.CalledProcessError(
            1, ["gh"], stderr="gh: token expired, run gh auth login\n"
        )
        self.assertIn("token expired", ci_timings.describe_error(error))

    def test_main_reports_the_underlying_gh_failure(self):
        def failing(arguments):
            raise subprocess.CalledProcessError(
                1, ["gh", *arguments], stderr="gh: API rate limit exceeded\n"
            )

        original = ci_timings.gh_json
        ci_timings.gh_json = failing
        stderr = io.StringIO()
        try:
            with contextlib.redirect_stderr(stderr):
                code = ci_timings.main(
                    ["runs", "--window", "2026-09-14..2026-09-16"]
                )
        finally:
            ci_timings.gh_json = original
        self.assertEqual(code, 1)
        self.assertIn("API rate limit exceeded", stderr.getvalue())


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

    def test_select_junit_job_prefers_label_over_ambiguity(self):
        # Several test-bearing jobs are fine as long as --label names one of
        # them: --job is only required when nothing matches.
        document = {
            "jobs": [
                {"name": "tests (unit, fast)", "steps": [
                    {"name": "Run fast shard",
                     "startedAt": "2026-09-20T10:00:00Z",
                     "completedAt": "2026-09-20T10:04:18Z"},
                ]},
                {"name": "tests (cli, fast)", "steps": [
                    {"name": "Run fast shard",
                     "startedAt": "2026-09-20T10:00:00Z",
                     "completedAt": "2026-09-20T10:03:00Z"},
                ]},
            ]
        }
        name, _, _ = ci_timings.select_junit_job(
            document, label="tests (unit, fast)"
        )
        self.assertEqual(name, "tests (unit, fast)")

    def test_select_junit_job_refuses_to_guess_between_jobs(self):
        # The default label is "JUnit", which matches no job: guessing would
        # pair the artifact with an unrelated shard and report false numbers.
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
        with self.assertRaisesRegex(ValueError, "pass --job"):
            ci_timings.select_junit_job(document, label="JUnit")

    def test_select_junit_job_auto_selects_single_candidate(self):
        document = {
            "jobs": [
                {"name": "tests (unit, fast)", "steps": [
                    {"name": "Run fast shard",
                     "startedAt": "2026-09-20T10:00:00Z",
                     "completedAt": "2026-09-20T10:04:18Z"},
                ]},
                {"name": "fmt + clippy", "steps": [
                    {"name": "Clippy",
                     "startedAt": "2026-09-20T10:00:00Z",
                     "completedAt": "2026-09-20T10:03:00Z"},
                ]},
            ]
        }
        name, _, step = ci_timings.select_junit_job(document, label="JUnit")
        self.assertEqual((name, step), ("tests (unit, fast)", 258.0))


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

    def test_parse_cache_log_separates_restore_errors_from_misses(self):
        # "Failed to restore" is a backend/extraction failure, not evidence
        # that the entry was absent: reporting it as a miss would make the
        # cache look ineffective when it is the restore that broke.
        log = "\n".join(
            [
                "tests (heavy)\tCache cargo build"
                "\t2026-09-21T05:17:40Z Failed to restore: archive extraction failed",
                "tests (cli, fast)\tCache cargo build"
                "\t2026-09-21T05:17:40Z Cache not found for keys: Linux-x64-gnu",
            ]
        )
        records = {record["job"]: record for record in ci_timings.parse_cache_log(log)}
        self.assertEqual(records["tests (heavy)"]["rust_cache"], "error")
        self.assertEqual(records["tests (cli, fast)"]["rust_cache"], "miss")

    def test_parse_cache_log_accepts_unnamed_rust_cache_step(self):
        # A workflow step left unnamed is logged under its action reference;
        # its hit evidence must not be discarded.
        log = (
            "tests (unit, fast)\tRun Swatinem/rust-cache@v2"
            "\t2026-09-21T05:17:40Z Cache restored successfully"
        )
        records = ci_timings.parse_cache_log(log)
        self.assertEqual(
            [(record["job"], record["rust_cache"]) for record in records],
            [("tests (unit, fast)", "hit")],
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


class FetchTests(unittest.TestCase):
    def _fetch(self, listed, attempts, cache_contents=None, limit=40):
        """Run `list_documents` against a stubbed `gh`, returning the calls."""
        calls = []

        def fake_gh_json(arguments):
            calls.append(arguments)
            if arguments[1] == "list":
                return listed
            run_id = int(arguments[2])
            attempt = int(arguments[arguments.index("--attempt") + 1])
            return {
                "databaseId": run_id,
                "attempt": attempt,
                "workflowName": "CI",
                "conclusion": "success",
                "createdAt": "2026-09-20T10:00:00Z",
                "startedAt": "2026-09-20T10:00:00Z",
                "updatedAt": "2026-09-20T10:09:30Z",
                "jobs": [],
            }

        original = ci_timings.gh_json
        ci_timings.gh_json = fake_gh_json
        try:
            with tempfile.TemporaryDirectory() as directory:
                cache = Path(directory) / "ci-runs.json"
                if cache_contents is not None:
                    ci_timings.write_snapshot(cache, cache_contents)
                args = argparse.Namespace(
                    cache=str(cache), limit=limit, event=None, branch=None,
                    conclusion="success", attempts=attempts,
                )
                fetch = ci_timings.list_documents(args)
                stored = ci_timings.read_snapshot(cache) if cache.exists() else []
            return fetch.documents, stored, calls, fetch.truncated
        finally:
            ci_timings.gh_json = original

    def test_fetch_all_attempts_reaches_intermediate_reruns(self):
        # `gh run list` only ever describes attempt 3; attempt 2 exists only
        # if it is requested by number.
        listed = [{"databaseId": 77, "attempt": 3, "conclusion": "failure"}]
        documents, stored, calls, _ = self._fetch(listed, "all")
        self.assertEqual(
            sorted(document["attempt"] for document in documents), [1, 2, 3]
        )
        self.assertEqual(len(stored), 3)
        self.assertEqual(
            [call[call.index("--attempt") + 1] for call in calls if call[1] == "view"],
            ["1", "2", "3"],
        )

    def test_fetch_first_attempt_keeps_the_original_run(self):
        listed = [{"databaseId": 77, "attempt": 2, "conclusion": "success"}]
        documents, _, _, _ = self._fetch(listed, "first")
        self.assertEqual(
            sorted(document["attempt"] for document in documents), [1, 2]
        )

    def test_fetch_skips_attempts_already_cached(self):
        cached = {
            "databaseId": 77,
            "attempt": 1,
            "workflowName": "CI",
            "conclusion": "failure",
            "createdAt": "2026-09-20T10:00:00Z",
            "startedAt": "2026-09-20T10:00:00Z",
            "updatedAt": "2026-09-20T10:09:30Z",
            "jobs": [],
        }
        listed = [{"databaseId": 77, "attempt": 2, "conclusion": "success"}]
        _, stored, calls, _ = self._fetch(listed, "first", cache_contents=[cached])
        self.assertEqual(len(stored), 2)
        self.assertEqual(
            [call[call.index("--attempt") + 1] for call in calls if call[1] == "view"],
            ["2"],
        )

    def test_fetch_then_filter_recovers_a_run_whose_rerun_failed(self):
        """The whole finding-3 pipeline: listing shows only the failed rerun.

        `gh run list` reports attempt 2's `failure`, so a server-side
        `--status success` would drop the run outright. Fetching both
        attempts and applying the conclusion locally keeps attempt 1.
        """
        conclusions = {1: "success", 2: "failure"}
        calls = []

        def fake_gh_json(arguments):
            calls.append(arguments)
            if arguments[1] == "list":
                # `gh run list` describes the latest attempt only, and honors
                # --status against it. Modelling that is what makes this test
                # fail if --conclusion is ever pushed server-side again.
                listed = {"databaseId": 27, "attempt": 2, "conclusion": "failure"}
                if "--status" in arguments:
                    wanted = arguments[arguments.index("--status") + 1]
                    if listed["conclusion"] != wanted:
                        return []
                return [listed]
            attempt = int(arguments[arguments.index("--attempt") + 1])
            return {
                "databaseId": 27,
                "attempt": attempt,
                "workflowName": "CI",
                "conclusion": conclusions[attempt],
                "createdAt": "2026-09-20T10:00:00Z",
                "startedAt": "2026-09-20T10:00:00Z",
                "updatedAt": "2026-09-20T10:09:30Z",
                "jobs": [
                    {
                        "name": "tests (unit, fast)",
                        "startedAt": "2026-09-20T10:00:06Z",
                        "completedAt": "2026-09-20T10:03:52Z",
                        "conclusion": conclusions[attempt],
                    },
                ],
            }

        original = ci_timings.gh_json
        ci_timings.gh_json = fake_gh_json
        try:
            with tempfile.TemporaryDirectory() as directory:
                args = argparse.Namespace(
                    input=None, cache=str(Path(directory) / "ci-runs.json"),
                    limit=40, event=None, branch=None, conclusion="success",
                    attempts="first",
                )
                summaries = ci_timings.load_run_summaries(args).runs
                selected = ci_timings.select_runs(summaries, args)
        finally:
            ci_timings.gh_json = original

        listing = [call for call in calls if call[1] == "list"][0]
        self.assertNotIn("--status", listing)
        self.assertEqual(
            [(run["id"], run["attempt"], run["conclusion"]) for run in selected],
            [(27, 1, "success")],
        )

    def test_fetch_ignores_unrelated_cached_runs(self):
        """The sample follows the query, not the local cache's history.

        A snapshot accumulates every run ever fetched. Returning all of it
        would make the same command report different medians depending on
        what someone fetched earlier on that machine.
        """
        stale = {
            "databaseId": 99, "attempt": 1, "workflowName": "CI",
            "conclusion": "success", "createdAt": "2026-01-01T10:00:00Z",
            "startedAt": "2026-01-01T10:00:00Z",
            "updatedAt": "2026-01-01T10:09:30Z", "jobs": [],
        }
        listed = [{"databaseId": 77, "attempt": 1, "conclusion": "success"}]
        documents, stored, _, _ = self._fetch(
            listed, "first", cache_contents=[stale]
        )
        self.assertEqual([d["databaseId"] for d in documents], [77])
        # The unrelated run stays on disk for a later query that wants it.
        self.assertEqual(
            sorted(d["databaseId"] for d in stored), [77, 99]
        )

    def test_fetch_reuses_a_cached_document_without_refetching(self):
        cached = {
            "databaseId": 77, "attempt": 1, "workflowName": "CI",
            "conclusion": "success", "createdAt": "2026-09-20T10:00:00Z",
            "startedAt": "2026-09-20T10:00:00Z",
            "updatedAt": "2026-09-20T10:09:30Z", "jobs": [],
        }
        listed = [{"databaseId": 77, "attempt": 1, "conclusion": "success"}]
        documents, _, calls, _ = self._fetch(
            listed, "first", cache_contents=[cached]
        )
        self.assertEqual([d["databaseId"] for d in documents], [77])
        self.assertEqual([call for call in calls if call[1] == "view"], [])

    def test_fetch_reports_a_listing_that_filled_the_limit(self):
        # A full listing means `gh` may have had more, so the sample is a
        # truncation rather than the whole window.
        listed = [
            {"databaseId": i, "attempt": 1, "conclusion": "success"}
            for i in range(1, 4)
        ]
        self.assertTrue(self._fetch(listed, "first", limit=3)[3])
        self.assertFalse(self._fetch(listed, "first", limit=4)[3])

    def test_fetch_skips_unfinished_runs(self):
        listed = [{"databaseId": 77, "attempt": 1, "conclusion": None}]
        documents, _, calls, _ = self._fetch(listed, "first")
        self.assertEqual(documents, [])
        self.assertEqual([call for call in calls if call[1] == "view"], [])


class LogCacheTests(unittest.TestCase):
    def test_log_cache_path_carries_the_attempt(self):
        """A rerun keeps its run id, so the id alone is not a cache key.

        Without the attempt, `cache --run` keeps reporting the superseded
        attempt's hit/miss evidence after every rerun.
        """
        seen = []

        def fake_run(command, **kwargs):
            seen.append(command)
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

        original = ci_timings.subprocess.run
        ci_timings.subprocess.run = fake_run
        try:
            with tempfile.TemporaryDirectory() as directory:
                for attempt in (1, 2):
                    path = Path(directory) / f"run-55-attempt-{attempt}.log"
                    ci_timings.load_run_log(55, path, attempt)
                    self.assertTrue(path.exists())
        finally:
            ci_timings.subprocess.run = original
        self.assertEqual(
            [command[command.index("--attempt") + 1] for command in seen],
            ["1", "2"],
        )

    def test_log_cache_is_reused_without_calling_gh(self):
        def fail(command, **kwargs):
            raise AssertionError("gh must not be called for a cached log")

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "run-55-attempt-1.log"
            path.write_text("cached log")
            original = ci_timings.subprocess.run
            ci_timings.subprocess.run = fail
            try:
                self.assertEqual(
                    ci_timings.load_run_log(55, path, 1), "cached log"
                )
            finally:
                ci_timings.subprocess.run = original

    def test_cache_command_default_path_separates_attempts(self):
        """The default log path must distinguish a rerun from its original.

        `command_cache` builds this path itself, so testing `load_run_log`
        alone leaves the naming unguarded.
        """
        paths = []

        def fake_load(run_id, log_cache, attempt=None):
            paths.append((Path(log_cache).name, attempt))
            return ""

        originals = (ci_timings.gh_json, ci_timings.load_run_log)
        ci_timings.load_run_log = fake_load
        try:
            for attempt in (1, 2):
                ci_timings.gh_json = lambda arguments, attempt=attempt: {
                    "attempt": attempt, "conclusion": "success",
                }
                args = argparse.Namespace(
                    input=None, run=55, log_cache=None, json=True
                )
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(ci_timings.command_cache(args), 0)
        finally:
            ci_timings.gh_json, ci_timings.load_run_log = originals
        self.assertEqual(
            paths,
            [("run-55-attempt-1.log", 1), ("run-55-attempt-2.log", 2)],
        )

    def test_cache_command_refuses_an_unfinished_run(self):
        # An in-progress run's log is partial, and a partial log written to
        # the cache would never repair itself.
        original = ci_timings.gh_json
        ci_timings.gh_json = lambda arguments: {
            "attempt": 1, "conclusion": None,
        }
        try:
            args = argparse.Namespace(
                input=None, run=55, log_cache=None, json=False
            )
            with self.assertRaisesRegex(ValueError, "has not finished"):
                ci_timings.command_cache(args)
        finally:
            ci_timings.gh_json = original


class TruncationTests(unittest.TestCase):
    def test_truncation_warning_names_the_window_and_limit(self):
        window = ci_timings.parse_window("2026-09-14..2026-09-16")
        message = ci_timings.truncation_warning("baseline", window, 40)
        self.assertIn("baseline", message)
        self.assertIn("2026-09-14..2026-09-16", message)
        self.assertIn("--limit 40", message)
        self.assertIn("missing older runs", message)


class SnapshotTests(unittest.TestCase):
    def test_snapshot_roundtrip_and_rejection(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "snapshot.json"
            ci_timings.write_snapshot(path, [RUN])
            self.assertEqual(ci_timings.read_snapshot(path), [RUN])
            path.write_text('{"runs": []}')
            with self.assertRaisesRegex(ValueError, "JSON list"):
                ci_timings.read_snapshot(path)
