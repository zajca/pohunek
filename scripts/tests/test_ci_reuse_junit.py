"""Scenarios for the per-job JUnit download of `scripts/ci-reuse-junit` (#2204).

Each scenario writes a `ci-reuse-<tree>` record naming the source run of
each reused job, serves the source runs' artifacts from a fake `gh` on
PATH, and runs the script as CI does. The script must download each
reused job's reports from the run its own record entry names (never by
run age), must never touch the jobs the record does not name (this run's
own reports are downloaded afterwards and win), and must retry a failing
`gh` a bounded number of times while failing on an exhausted retry. The
workflow test below pins the order of the `test-duplicates` steps: reused
reports first, then this run's own reports through the pinned
`actions/download-artifact`.
"""

import base64
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "ci-reuse-junit"
CI = (ROOT / ".github" / "workflows" / "ci.yml").read_text()
REPO = "example/pohunek"


FAKE_GH = r'''#!/usr/bin/env python3
import base64
import json
import os
import sys
from pathlib import Path

fixture_path = Path(os.environ["GH_FIXTURE"])
fixture = json.loads(fixture_path.read_text())
fixture.setdefault("calls", [])
fixture["calls"].append(sys.argv[1:])
fixture_path.write_text(json.dumps(fixture))

args = sys.argv[1:]
if args[:2] == ["api", "--method"]:
    endpoint = next((arg for arg in args[1:] if arg.startswith("repos/")), "")
    if "/actions/runs/" in endpoint and endpoint.endswith("/artifacts"):
        run_id = str(endpoint.split("/")[-2])
        failures = fixture.get("fail_listings", {}).get(run_id, 0)
        seen = fixture.setdefault("seen_listings", {}).get(run_id, 0)
        fixture.setdefault("seen_listings", {})[run_id] = seen + 1
        fixture_path.write_text(json.dumps(fixture))
        if seen < failures:
            print("gh: Server Error (HTTP 500)", file=sys.stderr)
            raise SystemExit(1)
        print(json.dumps([{"artifacts": fixture.get("artifacts", {}).get(run_id, [])}]))
    elif "/actions/artifacts/" in endpoint and endpoint.endswith("/zip"):
        artifact_id = str(endpoint.split("/")[-2])
        zips = {str(key): value for key, value in fixture.get("artifact_zips", {}).items()}
        if artifact_id not in zips:
            print("gh: Not Found (HTTP 404)", file=sys.stderr)
            raise SystemExit(1)
        sys.stdout.buffer.write(base64.b64decode(zips[artifact_id]))
    else:
        raise SystemExit("unexpected Actions API endpoint: " + repr(endpoint))
else:
    raise SystemExit("unexpected gh invocation: " + repr(args))
'''


def report_zip(marker: str) -> str:
    """A base64 zip holding one JUnit report whose bytes name their source."""
    body = (
        '<?xml version="1.0"?><testsuites name="nextest-run">'
        f'<testsuite name="suite" tests="1">'
        f'<testcase name="the_test" classname="suite" time="0.1">{marker}</testcase>'
        "</testsuite></testsuites>"
    )
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("junit.xml", body)
    return base64.b64encode(buffer.getvalue()).decode()


class ReuseJunitDownloadTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        bin_dir = self.directory / "bin"
        bin_dir.mkdir()
        gh = bin_dir / "gh"
        gh.write_text(FAKE_GH)
        gh.chmod(0o755)
        self.fixture = self.directory / "fixture.json"
        self.dest = self.directory / "junit"
        self.dest.mkdir()
        self.record = self.directory / "ci-reuse-tree.json"
        self.saved = {
            key: os.environ.get(key)
            for key in ("PATH", "GH_FIXTURE", "GITHUB_REPOSITORY")
        }
        self.addCleanup(self.restore_env)
        os.environ["PATH"] = f"{bin_dir}{os.pathsep}{os.environ['PATH']}"
        os.environ["GH_FIXTURE"] = str(self.fixture)
        os.environ["GITHUB_REPOSITORY"] = REPO

    def restore_env(self):
        for key, value in self.saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value

    def write_fixture(self, fixture):
        self.fixture.write_text(json.dumps(fixture))

    def write_record(self, jobs):
        self.record.write_text(json.dumps({
            "schema_version": 1,
            "tree": "t" * 40,
            "source_run_id": 777,
            "source_runs": sorted({entry["run_id"] for entry in jobs.values()}),
            "jobs": jobs,
        }))

    def run_script(self):
        return subprocess.run(
            [sys.executable, str(SCRIPT),
             "--record", str(self.record),
             "--dest", str(self.dest),
             "--repo", REPO],
            capture_output=True, text=True,
        )

    def test_each_reused_job_comes_from_the_run_its_record_entry_names(self):
        # Two source runs hold the same artifact name with different bytes.
        # The record takes `fast-tests` from the older run 666 and
        # `platform-contracts` from the newer run 777: each job's report
        # must carry its own run's bytes, not the newest run's.
        self.write_record({
            "fast-tests": {"run_id": 666, "names": ["tests (core, fast)"]},
            "platform-contracts": {"run_id": 777, "names": ["platform contracts (arm64)"]},
        })
        self.write_fixture({
            "artifacts": {
                "666": [{"name": "nextest-junit-core", "id": 11}],
                "777": [{"name": "nextest-junit-core", "id": 12},
                        {"name": "macos-lane-junit", "id": 13}],
            },
            "artifact_zips": {
                "11": report_zip("from-666"),
                "12": report_zip("from-777"),
                "13": report_zip("lane-from-777"),
            },
        })
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "from-666",
            (self.dest / "nextest-junit-core" / "junit.xml").read_text(),
        )
        self.assertIn(
            "lane-from-777",
            (self.dest / "macos-lane-junit" / "junit.xml").read_text(),
        )

    def test_jobs_the_record_does_not_name_are_never_touched(self):
        # The record names only `fast-tests`; the untouched `macos-lane-junit`
        # directory holds this run's own report, which the later
        # download-artifact step must still win.
        own = self.dest / "macos-lane-junit"
        own.mkdir()
        (own / "junit.xml").write_text("own-report")
        self.write_record({
            "fast-tests": {"run_id": 666, "names": ["tests (core, fast)"]},
        })
        self.write_fixture({
            "artifacts": {
                "666": [{"name": "nextest-junit-core", "id": 11},
                        {"name": "macos-lane-junit", "id": 12}],
            },
            "artifact_zips": {
                "11": report_zip("from-666"),
                "12": report_zip("must-never-land"),
            },
        })
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "from-666",
            (self.dest / "nextest-junit-core" / "junit.xml").read_text(),
        )
        self.assertEqual((own / "junit.xml").read_text(), "own-report")

    def test_a_failing_listing_is_retried_then_fails_loudly(self):
        # The listing of run 666 fails twice, then succeeds: the retry
        # lands the report, and the fixture saw exactly three calls.
        self.write_record({
            "fast-tests": {"run_id": 666, "names": ["tests (core, fast)"]},
        })
        self.write_fixture({
            "fail_listings": {"666": 2},
            "artifacts": {"666": [{"name": "nextest-junit-core", "id": 11}]},
            "artifact_zips": {"11": report_zip("from-666")},
        })
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "from-666",
            (self.dest / "nextest-junit-core" / "junit.xml").read_text(),
        )
        calls = json.loads(self.fixture.read_text())["calls"]
        listings = [call for call in calls if "/actions/runs/666/artifacts" in " ".join(call)]
        self.assertEqual(len(listings), 3)
        # An always-failing listing exhausts the bound and fails the step,
        # so `test-duplicates` never checks a silently partial set.
        self.write_fixture({
            "fail_listings": {"666": 99},
            "artifacts": {"666": [{"name": "nextest-junit-core", "id": 11}]},
            "artifact_zips": {"11": report_zip("from-666")},
        })
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("giving up after 3 attempts", result.stderr)
        calls = json.loads(self.fixture.read_text())["calls"]
        listings = [call for call in calls if "/actions/runs/666/artifacts" in " ".join(call)]
        self.assertEqual(len(listings), 3)

    def test_a_missing_record_is_not_an_error(self):
        # Reuse skipped (or the kill switch off): no record, no reports,
        # and the step still succeeds for the run's own download.
        self.record.unlink(missing_ok=True)
        self.write_fixture({})
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("nothing reused", result.stdout + result.stderr)


def workflow_jobs():
    jobs_text = CI.split("jobs:\n", 1)[1]
    matches = list(re.finditer(r"(?m)^  ([a-z][a-z0-9-]*):\n", jobs_text))
    return {
        match.group(1): jobs_text[
            match.end(): matches[index + 1].start()
            if index + 1 < len(matches)
            else len(jobs_text)
        ]
        for index, match in enumerate(matches)
    }


class ReuseJunitWiringTests(unittest.TestCase):
    def test_reused_reports_come_from_the_record_before_the_own_download(self):
        body = workflow_jobs()["test-duplicates"]
        self.assertIn("needs.reuse.outputs.tree", body)
        self.assertIn("ci-reuse-${{ needs.reuse.outputs.tree }}", body)
        self.assertIn("scripts/ci-reuse-junit", body)
        self.assertIn("actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093", body)
        reused = body.index("scripts/ci-reuse-junit")
        own = body.index("Download the run's JUnit reports")
        self.assertLess(
            reused, own,
            "the per-job record download must precede this run's own download",
        )


if __name__ == "__main__":
    unittest.main()
