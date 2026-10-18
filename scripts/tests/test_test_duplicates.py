"""CLI scenarios for the same-OS test duplicate check (#1226).

Each scenario writes a run's downloaded JUnit artifacts (one directory per
artifact, nextest's report shape) and a config, then runs the script as CI
does. The runtime package scenarios of the repository config instead pass
`scripts/test-duplicates.json` itself.
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


class RepositoryRuntimePackageTests(DuplicateScenario):
    """Scenarios that run the check against the repository's own config.

    #1228 builds the integration tests of `pohunek-cli` as one binary, so a
    consolidated report names the binary id `pohunek-cli::all` and qualifies
    the module in every test name. The setUp config is unused: the scenarios
    run the script with the repository's `scripts/test-duplicates.json`.
    """

    BINARY = "pohunek-cli::all"

    # One always-running test of each runtime package module of `all`
    # (crates/cli/tests/<module>.rs): its shard and its package job both
    # execute it, once in-process and once under the real runtime. Its
    # ignored real-runtime test beside them runs in the package job alone,
    # so the shard reports it, skipped.
    MODULE_CASES = {
        "pi_package": "the_supported_range_equals_the_compatibility_lock",
        "claude_package": "the_supported_range_equals_the_compatibility_lock",
        "codex_package": "the_supported_range_equals_the_compatibility_lock",
        "hermes_package": "the_signed_hermes_package_is_selected_and_failed_installs_preserve_it",
    }
    IGNORED_CASES = {
        "pi_package": "a_real_pi_session_launches_is_detected_resumes_and_forks",
        "claude_package": "a_real_claude_session_launches_and_is_detected_through_package_facts",
        "codex_package": "a_real_codex_session_launches_and_is_detected_through_package_facts",
        "hermes_package": "a_real_hermes_turn_uses_the_selected_package_and_public_session_screen",
    }

    def setUp(self):
        super().setUp()
        self.config = CONFIG

    def module_pairs(self):
        """One (allowed entry, shard source, package source, module) per entry."""
        config = json.loads(CONFIG.read_text())
        pairs = []
        for entry in config["allowed"]:
            [package] = [source for source in entry["sources"] if source.startswith("package-junit-")]
            [shard] = [source for source in entry["sources"] if not source.startswith("package-junit-")]
            module = f"{package.removeprefix('package-junit-')}_package"
            pairs.append((entry, shard, package, module))
        return pairs

    def test_the_repository_config_admits_every_runtime_package_overlap(self):
        pairs = self.module_pairs()
        self.assertEqual(sorted(pair[3] for pair in pairs),
                         ["claude_package", "codex_package", "hermes_package", "pi_package"])
        shard_cases, package_cases = {}, {}
        for entry, shard, package, module in pairs:
            # The shard skipped the ignored real-runtime test; the package job ran it.
            overlap = f"{module}::{self.MODULE_CASES[module]}"
            ignored = f"{module}::{self.IGNORED_CASES[module]}"
            shard_cases.setdefault(shard, []).append((self.BINARY, overlap, 1.0))
            shard_cases[shard].append((self.BINARY, ignored, 0.0, True))
            package_cases.setdefault(package, []).append((self.BINARY, overlap, 4.0))
            package_cases[package].append((self.BINARY, ignored, 30.0))
        for artifact, members in {**shard_cases, **package_cases}.items():
            self.artifact(artifact, members)
        result = self.run_check("--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        reasons = {frozenset(entry["sources"]): entry["reason"] for entry, *_ in pairs}
        self.assertEqual(len(payload["duplicates"]), len(reasons))
        for row in payload["duplicates"]:
            with self.subTest(sources=row["sources"]):
                self.assertEqual(row["os"], "linux")
                self.assertEqual(row["binary"], self.BINARY, row)
                self.assertEqual(row["cases"], 1)
                self.assertTrue(row["allowed"])
                self.assertEqual(row["reason"], reasons[frozenset(row["sources"])])
        self.assertEqual(payload["unused_allowed"], [])

    def test_a_test_of_the_consolidated_binary_in_two_shards_stays_rejected(self):
        # No entry allows two shard sources: a heavy test leaked into the
        # fast shard is a partition bug, not the runtime package overlap.
        name = f"hermes_package::{self.MODULE_CASES['hermes_package']}"
        self.artifact("nextest-junit-cli", [(self.BINARY, name, 1.0)])
        self.artifact("nextest-junit-heavy", [(self.BINARY, name, 2.0)])
        result = self.run_check()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(
            "1 test(s) of pohunek-cli::all ran on linux in nextest-junit-cli and nextest-junit-heavy",
            result.stderr,
        )

    def test_a_test_of_the_consolidated_binary_in_two_package_jobs_stays_rejected(self):
        # No entry allows two package sources: a package job whose filter
        # matched another module's tests is a selection bug.
        name = f"pi_package::{self.MODULE_CASES['pi_package']}"
        self.artifact("package-junit-pi", [(self.BINARY, name, 4.0)])
        self.artifact("package-junit-claude", [(self.BINARY, name, 4.0)])
        result = self.run_check()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(
            "1 test(s) of pohunek-cli::all ran on linux in package-junit-claude and package-junit-pi",
            result.stderr,
        )

    def test_a_macos_duplicate_of_the_consolidated_binary_stays_rejected(self):
        # The entries are Linux-only: when the macOS lane stops skipping the
        # test the Hermes macOS job owns, no allowance may cover the pair.
        name = f"hermes_package::{self.MODULE_CASES['hermes_package']}"
        self.artifact("macos-lane-junit", [(self.BINARY, name, 1.0)])
        self.artifact("hermes-macos-junit", [(self.BINARY, name, 3.0)], file="package")
        result = self.run_check()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(
            "1 test(s) of pohunek-cli::all ran on macos in hermes-macos-junit/package and macos-lane-junit",
            result.stderr,
        )


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
