"""CLI scenarios for the same-OS test duplicate check (#1226).

Each scenario writes a run's downloaded JUnit artifacts (one directory per
artifact, nextest's report shape) and a config, then runs the script as CI
does.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "test-duplicates"
CONFIG = Path(__file__).resolve().parents[1] / "test-duplicates.json"


def report(cases):
    """A nextest JUnit report of `cases`: (binary id, test name, seconds[, skipped])."""
    suites = {}
    for case in cases:
        suites.setdefault(case[0], []).append(case)
    body = []
    for binary, members in suites.items():
        body.append(f'<testsuite name="{binary}" tests="{len(members)}">')
        for case in members:
            skipped = "<skipped/>" if len(case) > 3 and case[3] else ""
            body.append(f'<testcase name="{case[1]}" classname="{binary}" time="{case[2]}">{skipped}</testcase>')
        body.append("</testsuite>")
    return f'<?xml version="1.0"?><testsuites name="nextest-run">{"".join(body)}</testsuites>'


class DuplicateScenario(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.reports = self.root / "junit"
        self.reports.mkdir()
        self.config = self.root / "config.json"
        self.write_config({
            "macos-lane-junit": "macos", "macos-tier-junit": "macos", "hermes-macos-junit": "macos",
            "nextest-junit-cli": "linux", "package-junit-pi": "linux",
        })

    def write_config(self, artifacts, allowed=()):
        self.config.write_text(json.dumps({"artifacts": artifacts, "allowed": list(allowed)}))

    def artifact(self, name, cases, file="junit"):
        directory = self.reports / name
        directory.mkdir(exist_ok=True)
        (directory / f"{file}.xml").write_text(report(cases))

    def run_check(self, *arguments, summary=None):
        environment = os.environ.copy()
        environment.pop("GITHUB_STEP_SUMMARY", None)
        if summary is not None:
            environment["GITHUB_STEP_SUMMARY"] = str(summary)
        return subprocess.run(
            [sys.executable, str(SCRIPT), str(self.reports), "--config", str(self.config), *arguments],
            capture_output=True, text=True, check=False, env=environment,
        )


class SameOsDuplicateTests(DuplicateScenario):
    def test_a_test_two_macos_jobs_ran_fails_the_run(self):
        self.artifact("macos-tier-junit", [("xtask", "hermes::tests::a", 1.5), ("xtask", "other", 0.1)])
        self.artifact("hermes-macos-junit", [("xtask", "hermes::tests::a", 2.0)], file="harness")
        summary = self.root / "summary.md"
        result = self.run_check(summary=summary)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(
            "1 test(s) of xtask ran on macos in hermes-macos-junit/harness and macos-tier-junit",
            result.stderr,
        )
        self.assertIn("| macos | `xtask` | hermes-macos-junit/harness + macos-tier-junit | 1 | 1.5 | **no** |",
                      summary.read_text())

    def test_the_same_test_on_two_operating_systems_is_platform_coverage(self):
        self.artifact("macos-lane-junit", [("pohunek-cli::pi_package", "a", 1.0)])
        self.artifact("nextest-junit-cli", [("pohunek-cli::pi_package", "a", 1.0)])
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("No test ran twice on the same OS.", result.stdout)

    def test_a_skipped_case_did_not_run(self):
        self.artifact("macos-lane-junit", [("pohunek-cli::hermes_package", "a", 0.0, True)])
        self.artifact("hermes-macos-junit", [("pohunek-cli::hermes_package", "a", 3.0)], file="package")
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_an_allowed_duplicate_passes_with_its_reason_and_an_unused_one_is_reported(self):
        self.write_config(
            {"nextest-junit-cli": "linux", "package-junit-pi": "linux", "macos-lane-junit": "macos"},
            allowed=[
                {"binaries": ["pohunek-cli::pi_package"], "sources": ["nextest-junit-cli", "package-junit-pi"],
                 "reason": "the package job runs it under the real runtime"},
                {"binaries": ["pohunek-cli::claude_package"], "sources": ["nextest-junit-cli", "package-junit-pi"],
                 "reason": "unused here"},
            ],
        )
        self.artifact("nextest-junit-cli", [("pohunek-cli::pi_package", "a", 1.0), ("pohunek-cli::pi_package", "b", 2.0)])
        self.artifact("package-junit-pi", [("pohunek-cli::pi_package", "a", 4.0), ("pohunek-cli::pi_package", "b", 1.0)])
        result = self.run_check("--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        [row] = payload["duplicates"]
        self.assertTrue(row["allowed"])
        self.assertEqual(row["cases"], 2)
        # Extra time is every run but the longest: 1.0 of `a` and 1.0 of `b`.
        self.assertEqual(row["extra_seconds"], 2.0)
        self.assertEqual([entry["binaries"] for entry in payload["unused_allowed"]], [["pohunek-cli::claude_package"]])

    def test_an_allowed_entry_covers_only_its_binary_and_its_sources(self):
        self.write_config(
            {"nextest-junit-cli": "linux", "package-junit-pi": "linux", "macos-lane-junit": "macos"},
            allowed=[{"binaries": ["pohunek-cli::pi_package"], "sources": ["nextest-junit-cli", "package-junit-pi"],
                      "reason": "the package job runs it under the real runtime"}],
        )
        self.artifact("nextest-junit-cli", [("pohunek-cli::other", "a", 1.0)])
        self.artifact("package-junit-pi", [("pohunek-cli::other", "a", 1.0)])
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("pohunek-cli::other", result.stderr)

    def test_an_artifact_the_config_does_not_name_fails_closed(self):
        self.artifact("nextest-junit-new-shard", [("pohunek-cli", "a", 1.0)])
        result = self.run_check()
        self.assertEqual(result.returncode, 2)
        self.assertIn("artifact nextest-junit-new-shard is not in the config", result.stderr)

    def test_no_reports_and_an_empty_artifact_fail_closed(self):
        result = self.run_check()
        self.assertEqual(result.returncode, 2)
        self.assertIn("no artifact directories", result.stderr)
        (self.reports / "macos-lane-junit").mkdir()
        result = self.run_check()
        self.assertEqual(result.returncode, 2)
        self.assertIn("artifact macos-lane-junit holds no JUnit report", result.stderr)

    def test_an_allowed_entry_without_a_reason_is_refused(self):
        self.write_config(
            {"nextest-junit-cli": "linux", "package-junit-pi": "linux"},
            allowed=[{"binaries": ["x"], "sources": ["nextest-junit-cli", "package-junit-pi"], "reason": " "}],
        )
        self.artifact("nextest-junit-cli", [("x", "a", 1.0)])
        result = self.run_check()
        self.assertEqual(result.returncode, 2)
        self.assertIn("needs a `reason`", result.stderr)


class RepositoryConfigTests(unittest.TestCase):
    def test_every_allowed_duplicate_is_on_linux_and_names_known_sources(self):
        config = json.loads(CONFIG.read_text())
        for entry in config["allowed"]:
            with self.subTest(entry=entry["binaries"]):
                operating_systems = {config["artifacts"][source.split("/", 1)[0]] for source in entry["sources"]}
                # Both macOS duplicates are removed by --skip-owned-by; none is allowed.
                self.assertEqual(operating_systems, {"linux"})


if __name__ == "__main__":
    unittest.main()
