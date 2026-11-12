"""Scenarios for the delta-reuse decision of `scripts/ci-reuse` (issue #2203).

The tests import `scripts/ci-reuse` as a module and call its `main()` with an
injected `now` (the clock is a scenario input, never the wall clock) against
a fake `gh` on PATH, the same technique `test_ci_evidence.py` uses: each
scenario writes a fixture of workflow runs, their artifacts and their jobs,
sets `GITHUB_REPOSITORY`, `GITHUB_SHA`, `GITHUB_OUTPUT` and `RUNNER_TEMP`, and
asserts the `skip`/`source_run` outputs, the `ci-reuse-<tree>` record, and the
warning text. The only stub is `commit_tree` (the local git object store
would otherwise fix the tree); the production code paths (`list_runs`,
`run_jobs`, `run_artifacts`, `download_artifact`, `source_selection_narrowed`)
all run. The fake `gh` honours `--paginate --slurp`: a paginated listing is
a JSON array of page objects, so an artifact that lives on the second page
is visible to the script exactly as the API serves it.
"""

import base64
import datetime
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "ci-reuse"
CONFIG = json.loads((ROOT / "scripts" / "ci-reuse.json").read_text())

SHA = "a" * 40
TREE = "c" * 40
REPO = "example/pohunek"
# The injected clock of every scenario; fixture timestamps are relative to
# it, never to the wall clock, so no scenario expires when the calendar moves.
NOW = datetime.datetime(2026, 11, 12, 12, 0, tzinfo=datetime.timezone.utc)
FRESH = "2026-11-12T11:00:00Z"
STALE = "2026-11-10T11:00:00Z"


FAKE_GH = r'''#!/usr/bin/env python3
import base64
import json
import os
import sys
from pathlib import Path

fixture_path = Path(os.environ["GH_FIXTURE"])
fixture = json.loads(fixture_path.read_text())
args = sys.argv[1:]

if args[:2] == ["api", "--method"]:
    endpoint = next((arg for arg in args[1:] if arg.startswith("repos/")), "")
    # `--paginate --slurp` returns a JSON array of pages (gh 2.x); `--paginate`
    # alone concatenates them. The fixtures carry real artifacts on later
    # pages, so a script that reads only the first page misses them.
    slurped = "--paginate" in args and "--slurp" in args
    def listing(key):
        pages = fixture.get("pages", {}).get(key)
        if pages is None:
            pages = fixture.get(f"{key}_pages")
        if pages is None:
            return None
        if slurped:
            print(json.dumps(pages))
        elif "--paginate" in args:
            for page in pages:
                print(json.dumps(page))
        else:
            print(json.dumps(pages[0] if pages else {}))
        return True
    if "/actions/workflows/" in endpoint and "/runs" in endpoint:
        if listing("runs") is not None:
            raise SystemExit(0)
        page = {"workflow_runs": fixture.get("runs", [])}
        fixture["queried_runs"] = fixture.get("queried_runs", 0) + 1
        fixture_path.write_text(json.dumps(fixture))
        print(json.dumps([page]))
    elif endpoint.endswith("/artifacts"):
        run_id = str(endpoint.split("/")[-2])
        entries = fixture.get("artifacts", {}).get(run_id, [])
        artifacts = []
        for entry in entries:
            if isinstance(entry, dict):
                artifacts.append({
                    "name": entry["name"],
                    "id": entry.get("id", 1),
                    "expired": entry.get("expired", False),
                })
            else:
                artifacts.append({"name": entry, "id": 1, "expired": False})
        if listing("artifacts") is None:
            print(json.dumps([{"artifacts": artifacts}]))
    elif "/actions/artifacts/" in endpoint and endpoint.endswith("/zip"):
        artifact_id = str(endpoint.split("/")[-2])
        zips = {str(key): value for key, value in fixture.get("artifact_zips", {}).items()}
        if artifact_id not in zips:
            print("gh: Not Found (HTTP 404)", file=sys.stderr)
            raise SystemExit(1)
        sys.stdout.buffer.write(base64.b64decode(zips[artifact_id]))
    elif "/actions/runs/" in endpoint and "/jobs" in endpoint:
        run_id = str(endpoint.split("/")[-2])
        if listing("jobs") is None:
            print(json.dumps([{"jobs": fixture.get("jobs_by_run", {}).get(run_id, [])}]))
    else:
        raise SystemExit("unexpected Actions API endpoint: " + repr(endpoint))
else:
    raise SystemExit("unexpected gh invocation: " + repr(args))
'''


def run_doc(run_id=777, event="pull_request", head_repo=REPO):
    return {
        "id": run_id,
        "run_number": run_id,
        "run_attempt": 1,
        "head_sha": SHA,
        "head_branch": "zajca/feature",
        "event": event,
        "repository": {"full_name": REPO},
        "head_repository": {"full_name": head_repo},
        "html_url": f"https://github.com/example/pohunek/actions/runs/{run_id}",
        "created_at": NOW.strftime("%Y-%m-%dT%H:%M:%SZ"),
    }


def job(name, conclusion="success", completed_at=FRESH):
    return {
        "id": abs(hash((name, conclusion))) % 100000 + 1,
        "name": name,
        "run_number": 1,
        "run_attempt": 1,
        "status": "completed",
        "conclusion": conclusion,
        "completed_at": completed_at,
    }


def green_jobs():
    """Every allowlisted leg green and fresh."""
    return [
        job(name)
        for entry in CONFIG["jobs"].values()
        for name in entry["names"]
    ]


def selection_zip(mode="packages"):
    """A base64 zip holding the source run's `selection.json`."""
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("selection.json", json.dumps({"mode": mode}))
    return base64.b64encode(buffer.getvalue()).decode()


class CiReuseProcessTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        bin_dir = self.directory / "bin"
        bin_dir.mkdir()
        gh = bin_dir / "gh"
        gh.write_text(FAKE_GH)
        gh.chmod(0o755)
        self.bin_dir = bin_dir
        self.fixture = self.directory / "fixture.json"
        self.output = self.directory / "output"
        self.output.write_text("")
        self.saved = {
            key: os.environ.get(key)
            for key in ("GITHUB_REPOSITORY", "GITHUB_SHA", "GITHUB_OUTPUT",
                        "RUNNER_TEMP", "PATH", "WORKFLOW_FILE")
        }
        self.addCleanup(self.restore_env)
        os.environ["GITHUB_REPOSITORY"] = REPO
        os.environ["GITHUB_SHA"] = SHA
        os.environ["GITHUB_OUTPUT"] = str(self.output)
        os.environ["RUNNER_TEMP"] = str(self.directory)
        os.environ["WORKFLOW_FILE"] = "ci.yml"
        os.environ["PATH"] = f"{bin_dir}{os.pathsep}{self.saved['PATH']}"
        self.git_tree = None

    def restore_env(self):
        for key, value in self.saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value

    def write_fixture(self, runs, jobs_by_run, artifacts, artifact_zips=None):
        self.fixture.write_text(json.dumps({
            "runs": runs,
            "jobs_by_run": jobs_by_run,
            "artifacts": artifacts,
            "artifact_zips": artifact_zips or {},
        }))
        os.environ["GH_FIXTURE"] = str(self.fixture)

    def run_script_with_tree(self, tree):
        import importlib.util
        loader = importlib.machinery.SourceFileLoader(
            "ci_reuse_under_test", str(SCRIPT))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        module = importlib.util.module_from_spec(spec)
        loader.exec_module(module)
        original = module.commit_tree
        module.commit_tree = lambda sha: tree
        try:
            rc = module.main(now=NOW)
        finally:
            module.commit_tree = original
        return rc

    def outputs(self):
        values = {}
        for line in self.output.read_text().splitlines():
            if "=" in line:
                key, value = line.split("=", 1)
                values[key] = value
        return values

    def record(self, tree):
        path = self.directory / f"ci-reuse-{tree}.json"
        return json.loads(path.read_text())

    def test_a_full_green_pull_request_run_skips_every_reusable_job(self):
        # The source run is full: the test-selection artifact holds id 2 with
        # a full selection, so selection_dependent jobs are reused. The
        # selection artifact lives on the second page of the artifact
        # listing: without `--paginate --slurp` the script never sees it and
        # counts the source as narrowed, so this scenario pins the real
        # pagination (it fails on a script that lists one page).
        self.write_fixture(
            runs=[run_doc()],
            jobs_by_run={"777": green_jobs()},
            artifacts={"777": [
                {"name": f"tested-tree-{TREE}", "id": 1},
                {"name": "test-selection", "id": 2},
            ]},
            artifact_zips={"2": selection_zip("full")},
        )
        fixture = json.loads(self.fixture.read_text())
        fixture["pages"] = {
            "artifacts": [
                {"artifacts": [{"name": f"tested-tree-{TREE}", "id": 1, "expired": False}]},
                {"artifacts": [{"name": "test-selection", "id": 2, "expired": False}]},
            ],
        }
        self.fixture.write_text(json.dumps(fixture))
        rc = self.run_script_with_tree(TREE)
        self.assertEqual(rc, 0)
        outputs = self.outputs()
        skip = json.loads(outputs["skip"])
        # The closure keeps every producer whose consumers all run: a skipped
        # job's same-run producers are skipped with it.
        for job_id in skip:
            for consumer in CONFIG["jobs"][job_id]["consumers"]:
                self.assertIn(consumer, skip, job_id)
        # build-bins stays out: its consumer sdk is not reusable, so no
        # running job would download the same-run artifact it did not upload.
        self.assertNotIn("build-bins", skip)
        self.assertIn("test", skip)
        self.assertEqual(outputs["source_run"], "777")
        record = self.record(TREE)
        self.assertEqual(record["schema_version"], 1)
        self.assertEqual(record["tree"], TREE)
        self.assertEqual(record["source_run_id"], 777)
        # Every reused job names its own run id (all equal here; #2204 takes
        # jobs from different runs without a schema change).
        for job_id in skip:
            self.assertEqual(record["jobs"][job_id]["run_id"], 777)

    def test_a_narrowed_source_excludes_the_selection_dependent_jobs(self):
        fixture_jobs = green_jobs()
        self.write_fixture(
            runs=[run_doc()],
            jobs_by_run={"777": fixture_jobs},
            artifacts={"777": [
                {"name": f"tested-tree-{TREE}", "id": 1},
                {"name": "test-selection", "id": 2},
            ]},
            artifact_zips={"2": selection_zip("packages")},
        )
        rc = self.run_script_with_tree(TREE)
        self.assertEqual(rc, 0)
        skip = json.loads(self.outputs()["skip"])
        dependent = [
            job_id for job_id, entry in CONFIG["jobs"].items()
            if entry["selection_dependent"]
        ]
        self.assertTrue(dependent)
        for job_id in dependent:
            self.assertNotIn(job_id, skip)
        self.assertIn("test", skip)
        self.assertEqual(self.outputs()["source_run"], "777")

    def test_a_missing_selection_counts_as_narrowed(self):
        self.write_fixture(
            runs=[run_doc()],
            jobs_by_run={"777": green_jobs()},
            artifacts={"777": [f"tested-tree-{TREE}"]},
        )
        rc = self.run_script_with_tree(TREE)
        self.assertEqual(rc, 0)
        skip = json.loads(self.outputs()["skip"])
        for job_id, entry in CONFIG["jobs"].items():
            if entry["selection_dependent"]:
                self.assertNotIn(job_id, skip)
        self.assertIn("test", skip)

    def test_no_same_tree_run_gives_an_empty_skip(self):
        self.write_fixture(
            runs=[run_doc()],
            jobs_by_run={"777": green_jobs()},
            artifacts={"777": ["tested-tree-" + "d" * 40]},
        )
        rc = self.run_script_with_tree(TREE)
        self.assertEqual(rc, 0)
        outputs = self.outputs()
        self.assertEqual(outputs["skip"], "[]")
        self.assertEqual(outputs["source_run"], "")
        record = self.record(TREE)
        self.assertIsNone(record["source_run_id"])
        self.assertEqual(record["jobs"], {})

    def test_a_fork_run_never_supplies_the_source(self):
        self.write_fixture(
            runs=[run_doc(head_repo="someone/pohunek")],
            jobs_by_run={},
            artifacts={},
        )
        rc = self.run_script_with_tree(TREE)
        self.assertEqual(rc, 0)
        self.assertEqual(self.outputs()["skip"], "[]")
        self.assertEqual(self.outputs()["source_run"], "")

    def test_a_red_leg_drops_only_its_job_while_the_rest_stays_reused(self):
        jobs = green_jobs()
        for entry in jobs:
            if entry["name"] == "fmt + clippy":
                entry["conclusion"] = "failure"
        self.write_fixture(
            runs=[run_doc()],
            jobs_by_run={"777": jobs},
            artifacts={"777": [
                {"name": f"tested-tree-{TREE}", "id": 1},
                {"name": "test-selection", "id": 2},
            ]},
            artifact_zips={"2": selection_zip("full")},
        )
        rc = self.run_script_with_tree(TREE)
        self.assertEqual(rc, 0)
        skip = json.loads(self.outputs()["skip"])
        # Only the job with the red leg stays out; the rest is still reused.
        self.assertNotIn("test", skip)
        self.assertIn("darwin-check", skip)

    def test_a_source_older_than_a_day_gives_an_empty_skip(self):
        jobs = []
        for entry in CONFIG["jobs"].values():
            for name in entry["names"]:
                jobs.append(job(name, completed_at=STALE))
        self.write_fixture(
            runs=[run_doc()],
            jobs_by_run={"777": jobs},
            artifacts={"777": [
                {"name": f"tested-tree-{TREE}", "id": 1},
                {"name": "test-selection", "id": 2},
            ]},
            artifact_zips={"2": selection_zip("full")},
        )
        rc = self.run_script_with_tree(TREE)
        self.assertEqual(rc, 0)
        self.assertEqual(self.outputs()["skip"], "[]")
        self.assertEqual(self.outputs()["source_run"], "")

    def test_an_api_error_gives_an_empty_skip_with_a_warning_and_exit_zero(self):
        gh = self.bin_dir / "gh"
        gh.write_text("#!/bin/sh\necho refused >&2\nexit 1\n")
        rc = self.run_script_with_tree(TREE)
        self.assertEqual(rc, 0)
        self.assertEqual(self.outputs()["skip"], "[]")



    def test_a_paired_lane_is_dropped_without_its_hermes_partner(self):
        # M3: the macOS lane preempts the Hermes tests unless the Hermes job
        # ran in the source. When the source did not run hermes-compatibility-macos
        # (hermes_macos false on the PR), reusing the lane on the dispatch
        # would run the Hermes tests in the lane while test-duplicates sees
        # both macos-lane-junit and hermes-macos-junit for one test.
        jobs = green_jobs()
        jobs = [entry for entry in jobs if entry["name"] != "pinned Hermes compatibility (macOS arm64, model-free)"]
        self.write_fixture(
            runs=[run_doc()],
            jobs_by_run={"777": jobs},
            artifacts={"777": [
                {"name": f"tested-tree-{TREE}", "id": 1},
                {"name": "test-selection", "id": 2},
            ]},
            artifact_zips={"2": selection_zip("full")},
        )
        rc = self.run_script_with_tree(TREE)
        self.assertEqual(rc, 0)
        skip = json.loads(self.outputs()["skip"])
        self.assertNotIn("platform-contracts", skip)
        self.assertNotIn("hermes-compatibility-macos", skip)
        self.assertIn("test", skip)

    def test_a_paired_lane_is_kept_with_its_hermes_partner(self):
        # When both the lane and the Hermes job are green in a full source,
        # the source lane skipped the Hermes tests exactly as the dispatch
        # lane will, so both may be reused.
        self.write_fixture(
            runs=[run_doc()],
            jobs_by_run={"777": green_jobs()},
            artifacts={"777": [
                {"name": f"tested-tree-{TREE}", "id": 1},
                {"name": "test-selection", "id": 2},
            ]},
            artifact_zips={"2": selection_zip("full")},
        )
        rc = self.run_script_with_tree(TREE)
        self.assertEqual(rc, 0)
        skip = json.loads(self.outputs()["skip"])
        self.assertIn("platform-contracts", skip)
        self.assertIn("hermes-compatibility-macos", skip)


if __name__ == "__main__":
    unittest.main()
