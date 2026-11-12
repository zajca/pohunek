"""Scenarios for the delta-reuse decision of `scripts/ci-reuse` (issue #2203).

The decision itself is a pure function of the config, the tree, the listed
runs and the jobs those runs report, so the scenarios drive `decide` with
injected data. Only the failure path runs `main`, against a fake `gh` that
refuses every API call.
"""

import datetime
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "ci-reuse"


def load_reuse_module():
    from importlib.machinery import SourceFileLoader

    loader = SourceFileLoader("ci_reuse_under_test", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


ci_reuse = load_reuse_module()
CONFIG = json.loads((ROOT / "scripts" / "ci-reuse.json").read_text())

SHA = "a" * 40
TREE = "c" * 40
REPO = "example/pohunek"
NOW = datetime.datetime(2026, 11, 12, 12, 0, 0, tzinfo=datetime.timezone.utc)
FRESH = "2026-11-12T11:00:00Z"
STALE = "2026-11-10T11:00:00Z"


def run(run_id=777, event="pull_request", head_repo=REPO):
    return {
        "id": run_id,
        "run_number": run_id,
        "run_attempt": 1,
        "head_sha": SHA,
        "event": event,
        "repository": {"full_name": REPO},
        "head_repository": {"full_name": head_repo},
        "html_url": f"https://github.com/example/pohunek/actions/runs/{run_id}",
    }


def job(name, conclusion="success", completed_at=FRESH):
    return {
        "id": hash((name, conclusion)) % 100000,
        "name": name,
        "run_number": 1,
        "run_attempt": 1,
        "status": "completed",
        "conclusion": conclusion,
        "completed_at": completed_at,
    }


def green_source(names):
    """Jobs where every allowlisted display name is green and fresh."""
    return [job(name) for names in CONFIG["jobs"].values() for name in names["names"]]


def decide(runs, jobs, narrowed=False, now=NOW):
    return ci_reuse.decide(
        CONFIG,
        TREE,
        runs,
        now,
        SHA,
        REPO,
        jobs_of=lambda repo, run_id: jobs,
        names_of=lambda repo, run_id: [f"tested-tree-{TREE}"],
        narrowed_of=lambda repo, run_id: narrowed,
    )


class CiReuseDecideTests(unittest.TestCase):
    def test_a_full_green_pull_request_run_skips_every_reusable_job(self):
        skip, source, reason = decide([run()], green_source(CONFIG["jobs"]))
        self.assertEqual(source["id"], 777)
        self.assertEqual(reason, "")
        # The closure keeps every producer whose consumers all run: a skipped
        # job's same-run producers are skipped with it.
        for job_id in skip:
            for consumer in CONFIG["jobs"][job_id]["consumers"]:
                self.assertIn(consumer, skip, job_id)
        # build-bins stays out: its consumer sdk is not reusable, so no
        # running job would download the same-run artifact it did not upload.
        self.assertNotIn("build-bins", skip)
        # A job with no consumers is always reused from a full green source.
        self.assertIn("test", skip)

    def test_a_narrowed_source_excludes_the_selection_dependent_jobs(self):
        skip, source, _ = decide([run()], green_source(CONFIG["jobs"]), narrowed=True)
        self.assertEqual(source["id"], 777)
        dependent = [
            job_id
            for job_id, entry in CONFIG["jobs"].items()
            if entry["selection_dependent"]
        ]
        self.assertTrue(dependent)
        for job_id in dependent:
            self.assertNotIn(job_id, skip)
        self.assertIn("test", skip)

    def test_no_same_tree_run_gives_an_empty_skip(self):
        other_tree = dict(run())
        skip, source, reason = ci_reuse.decide(
            CONFIG,
            TREE,
            [other_tree],
            NOW,
            SHA,
            REPO,
            jobs_of=lambda repo, run_id: green_source(CONFIG["jobs"]),
            names_of=lambda repo, run_id: [f"tested-tree-{'d' * 40}"],
            narrowed_of=lambda repo, run_id: False,
        )
        self.assertEqual(skip, [])
        self.assertIsNone(source)

    def test_a_fork_run_never_supplies_the_source(self):
        skip, source, _ = decide([run(head_repo="someone/pohunek")], [])
        self.assertEqual(skip, [])
        self.assertIsNone(source)

    def test_a_red_leg_is_not_reused(self):
        jobs = green_source(CONFIG["jobs"])
        for entry in jobs:
            if entry["name"] == "fmt + clippy":
                entry["conclusion"] = "failure"
        skip, source, _ = decide([run()], jobs)
        self.assertIsNotNone(source)
        # Only the job with the red leg stays out; the rest is still reused.
        self.assertNotIn("test", skip)
        self.assertIn("darwin-check", skip)

    def test_a_source_older_than_a_day_gives_an_empty_skip(self):
        jobs = [dict(entry, completed_at=STALE) for entry in green_source(CONFIG["jobs"])]
        skip, source, _ = decide([run()], jobs)
        self.assertEqual(skip, [])
        self.assertIsNone(source)

    def test_a_missing_selection_counts_as_narrowed(self):
        original_artifacts = ci_reuse.run_artifacts
        original_download = ci_reuse.download_artifact
        try:
            ci_reuse.run_artifacts = lambda repo, run_id: []
            ci_reuse.download_artifact = lambda repo, run_id, name, path: False
            self.assertTrue(ci_reuse.source_selection_narrowed(REPO, 777))
            ci_reuse.run_artifacts = lambda repo, run_id: ["test-selection"]
            self.assertIsNone(ci_reuse.source_selection_narrowed(REPO, 777))
        finally:
            ci_reuse.run_artifacts = original_artifacts
            ci_reuse.download_artifact = original_download


class CiReuseMainTests(unittest.TestCase):
    def test_an_api_error_gives_an_empty_skip_with_a_warning_and_exit_zero(self):
        temporary = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", str(temporary)], check=False))
        gh = temporary / "gh"
        gh.write_text("#!/bin/sh\necho refused >&2\nexit 1\n")
        gh.chmod(0o755)
        output = temporary / "output"
        output.write_text("")
        saved = {key: os.environ.get(key) for key in ("GITHUB_REPOSITORY", "GITHUB_SHA", "GITHUB_OUTPUT", "RUNNER_TEMP", "PATH")}
        original_tree = ci_reuse.commit_tree
        try:
            ci_reuse.commit_tree = lambda sha: TREE
            os.environ["GITHUB_REPOSITORY"] = REPO
            os.environ["GITHUB_SHA"] = SHA
            os.environ["GITHUB_OUTPUT"] = str(output)
            os.environ["RUNNER_TEMP"] = str(temporary)
            os.environ["PATH"] = f"{temporary}{os.pathsep}{saved['PATH']}"
            rc = ci_reuse.main()
        finally:
            ci_reuse.commit_tree = original_tree
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
        self.assertEqual(rc, 0)
        self.assertIn("skip=[]", output.read_text())


if __name__ == "__main__":
    unittest.main()
